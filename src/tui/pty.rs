//! The embedded shell: a real pseudo-terminal running the user's `$SHELL`.
//!
//! Output is parsed with [`vt100`] so Rewind can render a faithful terminal
//! grid inside a ratatui panel. Input and resize events are forwarded to the
//! PTY. Optionally, a shell-integration snippet is injected so commands typed
//! in the embedded shell are captured to a log Rewind tails (bash/zsh).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

use crate::error::{Result, RewindError};

/// Handle to the embedded shell and its parsed screen.
pub struct PtyShell {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    child: Box<dyn Child + Send + Sync>,
    reader_handle: Option<JoinHandle<()>>,
    rows: u16,
    cols: u16,
    /// Path the shell integration writes captured commands to (if enabled).
    pub command_log: Option<PathBuf>,
}

impl PtyShell {
    /// Spawn `shell` in `cwd` with a PTY of the given size. When `integration`
    /// is `Some`, a per-session command log is set up and its path returned via
    /// [`PtyShell::command_log`].
    pub fn spawn(
        shell: &str,
        cwd: &Path,
        rows: u16,
        cols: u16,
        integration: Option<&Path>,
    ) -> Result<PtyShell> {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| RewindError::other(format!("failed to open pty: {e}")))?;

        let mut cmd = CommandBuilder::new(shell);
        cmd.cwd(cwd);
        // A sensible terminal type for programs that inspect $TERM.
        cmd.env("TERM", "xterm-256color");
        cmd.env("REWIND_SESSION", "1");

        let command_log = match integration {
            Some(dir) => setup_integration(&mut cmd, shell, dir).ok().flatten(),
            None => None,
        };

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| RewindError::other(format!("failed to spawn shell `{shell}`: {e}")))?;
        // The slave handle is not needed once the child is spawned.
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| RewindError::other(format!("failed to clone pty reader: {e}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| RewindError::other(format!("failed to take pty writer: {e}")))?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 2000)));
        let parser_reader = Arc::clone(&parser);
        let reader_handle = std::thread::Builder::new()
            .name("rewind-pty-reader".into())
            .spawn(move || read_loop(reader, parser_reader))
            .map_err(RewindError::Io)?;

        Ok(PtyShell {
            master: pair.master,
            writer,
            parser,
            child,
            reader_handle: Some(reader_handle),
            rows,
            cols,
            command_log,
        })
    }

    /// Forward raw bytes to the shell's input.
    pub fn write_input(&mut self, data: &[u8]) -> Result<()> {
        self.writer.write_all(data)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Resize the PTY and the vt100 grid.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if rows == self.rows && cols == self.cols {
            return Ok(());
        }
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| RewindError::other(format!("failed to resize pty: {e}")))?;
        if let Ok(mut p) = self.parser.lock() {
            p.set_size(rows, cols);
        }
        self.rows = rows;
        self.cols = cols;
        Ok(())
    }

    /// Access the parser (holding the lock) for rendering.
    pub fn parser(&self) -> &Arc<Mutex<vt100::Parser>> {
        &self.parser
    }

    /// Whether the shell process is still running.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for PtyShell {
    fn drop(&mut self) {
        // Ask the shell to exit, then reap it so we don't leave a zombie.
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(h) = self.reader_handle.take() {
            let _ = h.join();
        }
    }
}

fn read_loop(mut reader: Box<dyn std::io::Read + Send>, parser: Arc<Mutex<vt100::Parser>>) {
    use std::io::Read;
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if let Ok(mut p) = parser.lock() {
                    p.process(&buf[..n]);
                }
            }
            Err(_) => break,
        }
    }
}

/// Configure shell-integration command capture for supported shells.
///
/// Returns the path of the per-session command log if integration was set up.
/// Supports bash (via a temp rcfile + DEBUG trap) and zsh (via `ZDOTDIR` +
/// `preexec`). Other shells fall back to no command-text capture (Rewind still
/// snapshots file changes via the watcher).
fn setup_integration(cmd: &mut CommandBuilder, shell: &str, dir: &Path) -> Result<Option<PathBuf>> {
    let base = Path::new(shell)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let log = dir.join(format!("commands-{}.log", std::process::id()));
    // Ensure the log exists and is empty for this session.
    std::fs::write(&log, b"")?;
    cmd.env("REWIND_CMD_LOG", &log);

    match base.as_str() {
        "bash" => {
            let rc = dir.join("rewind-init.bash");
            std::fs::write(
                &rc,
                r#"# Rewind bash integration
[ -f "$HOME/.bashrc" ] && source "$HOME/.bashrc"
__rewind_preexec() {
  [ -n "$COMP_LINE" ] && return
  case "$BASH_COMMAND" in
    __rewind_*|"$PROMPT_COMMAND") return;;
  esac
  printf '%s\n' "$BASH_COMMAND" >> "$REWIND_CMD_LOG" 2>/dev/null
}
trap '__rewind_preexec' DEBUG
"#,
            )?;
            cmd.args(["--rcfile", &rc.to_string_lossy(), "-i"]);
            Ok(Some(log))
        }
        "zsh" => {
            // Point ZDOTDIR at our dir; our .zshrc sources the real one.
            if let Some(real) = std::env::var_os("ZDOTDIR").or_else(|| std::env::var_os("HOME")) {
                cmd.env("REWIND_REAL_ZDOTDIR", real);
            }
            let zshrc = dir.join(".zshrc");
            std::fs::write(
                &zshrc,
                r#"# Rewind zsh integration
if [ -f "${REWIND_REAL_ZDOTDIR:-$HOME}/.zshrc" ]; then
  source "${REWIND_REAL_ZDOTDIR:-$HOME}/.zshrc"
fi
__rewind_preexec() { print -r -- "$1" >> "$REWIND_CMD_LOG" 2>/dev/null }
autoload -Uz add-zsh-hook 2>/dev/null && add-zsh-hook preexec __rewind_preexec
"#,
            )?;
            cmd.env("ZDOTDIR", dir);
            Ok(Some(log))
        }
        _ => Ok(Some(log)), // log created but no hook; capture will be empty
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn pty_runs_a_command_headless() {
        // A PTY can be opened without a controlling terminal, so this works in
        // CI. We run a shell that echoes and exits, then confirm the grid saw it.
        let tmp = tempfile::tempdir().unwrap();
        let shell = if cfg!(windows) {
            "cmd".to_string()
        } else {
            "/bin/sh".to_string()
        };
        let mut pty = PtyShell::spawn(&shell, tmp.path(), 24, 80, None).unwrap();
        pty.write_input(b"echo rewind-marker\n").unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = false;
        while Instant::now() < deadline {
            {
                let p = pty.parser().lock().unwrap();
                let text = screen_to_string(p.screen());
                if text.contains("rewind-marker") {
                    seen = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = pty.write_input(b"exit\n");
        assert!(seen, "expected the echoed marker to appear on the grid");
    }

    fn screen_to_string(screen: &vt100::Screen) -> String {
        let (rows, cols) = screen.size();
        let mut s = String::new();
        for r in 0..rows {
            for c in 0..cols {
                if let Some(cell) = screen.cell(r, c) {
                    s.push_str(&cell.contents());
                }
            }
            s.push('\n');
        }
        s
    }
}
