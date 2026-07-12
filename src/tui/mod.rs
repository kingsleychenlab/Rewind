//! The interactive terminal application.
//!
//! Sets up the terminal (with guaranteed restoration on exit, error, and
//! panic), spawns the embedded shell, wires the filesystem watcher and shell
//! command capture, and runs the draw/input loop. Keyboard input is routed to
//! the PTY in Shell mode and to Rewind navigation in Nav mode; `Ctrl+G` toggles.

mod app;
mod pty;
mod ui;

use std::io::{BufRead, IsTerminal};
use std::path::PathBuf;
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{cursor, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;

use crate::error::{Result, RewindError};
use crate::exec::{resolve_shell, CancelToken};
use crate::session::Engine;
use crate::tracking::watcher::{Watcher, DEFAULT_DEBOUNCE};

use app::{App, Focus, Mode, Overlay};
use pty::PtyShell;

/// Restore the terminal to a sane state. Safe to call more than once.
fn restore_terminal() {
    let mut out = std::io::stdout();
    let _ = execute!(
        out,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        cursor::Show
    );
    let _ = disable_raw_mode();
}

/// Launch the interactive interface for `engine`.
pub fn run(mut engine: Engine) -> Result<()> {
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        return Err(RewindError::other(
            "rewind's interactive interface needs a terminal; use `rewind run`, `rewind test`, \
             or `rewind ci` in scripts and CI",
        ));
    }

    // Ensure the terminal is restored even on panic, before the default hook
    // prints the message.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        original_hook(info);
    }));

    enable_raw_mode()?;
    execute!(
        std::io::stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste
    )?;

    let result = run_loop(&mut engine);

    restore_terminal();
    // Best-effort session close (also done by Engine::drop).
    let _ = engine.end_session();
    result
}

fn run_loop(engine: &mut Engine) -> Result<()> {
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    engine.start_session()?;

    // Spawn the embedded shell sized to the shell pane.
    let size = terminal.size()?;
    let panes = ui::layout(Rect::new(0, 0, size.width, size.height));
    let shell_path = resolve_shell();
    let mut shell = PtyShell::spawn(
        &shell_path,
        &engine.repo.root,
        panes.shell_inner.height,
        panes.shell_inner.width,
        Some(&engine.storage.logs),
    )?;

    // Filesystem watcher (best-effort — Rewind still reconciles at startup).
    // `_watcher` is held for the whole session; dropping it stops watching.
    let (_watcher, watch_rx): (Option<Watcher>, Receiver<Vec<String>>) =
        match Watcher::start(&engine.repo.root, engine.rules.clone(), DEFAULT_DEBOUNCE) {
            Ok((w, rx)) => (Some(w), rx),
            Err(_) => (None, unbounded().1),
        };

    // Command capture tailer.
    let (cmd_tx, cmd_rx) = unbounded::<String>();
    if let Some(log) = shell.command_log.clone() {
        std::thread::Builder::new()
            .name("rewind-cmd-tailer".into())
            .spawn(move || tail_commands(log, cmd_tx))
            .map_err(RewindError::Io)?;
    }

    let mut app = App::new(engine.repo.name());
    app.refresh(engine)?;

    loop {
        // Keep the PTY grid matched to the shell pane.
        let size = terminal.size()?;
        let panes = ui::layout(Rect::new(0, 0, size.width, size.height));
        shell.resize(panes.shell_inner.height, panes.shell_inner.width)?;

        terminal.draw(|f| ui::render(f, &app, shell.parser()))?;

        if event::poll(Duration::from_millis(30))? {
            match event::read()? {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Release {
                        handle_key(&mut app, engine, &mut shell, &mut terminal, key)?;
                    }
                }
                Event::Paste(text) => {
                    if app.mode == Mode::Shell {
                        shell.write_input(text.as_bytes())?;
                    } else if let Overlay::Search = app.overlay {
                        app.search_input.push_str(&text);
                        app.run_search(engine)?;
                    }
                }
                Event::Resize(_, _) => { /* handled at top of next iteration */ }
                _ => {}
            }
        }

        // Drain filesystem change batches.
        while let Ok(batch) = watch_rx.try_recv() {
            for p in batch {
                app.dirty.insert(p);
            }
        }
        // Drain captured shell commands into the timeline.
        while let Ok(cmd) = cmd_rx.try_recv() {
            app.capture_shell_command(engine, &cmd)?;
        }

        app.shell_alive = shell.is_alive();
        if app.should_quit {
            break;
        }
    }
    Ok(())
}

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

fn handle_key(
    app: &mut App,
    engine: &mut Engine,
    shell: &mut PtyShell,
    terminal: &mut Term,
    key: KeyEvent,
) -> Result<()> {
    // Ctrl+G always toggles between Shell and Nav modes.
    if key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.mode = match app.mode {
            Mode::Shell => Mode::Nav,
            Mode::Nav => Mode::Shell,
        };
        app.status = match app.mode {
            Mode::Shell => "Shell mode — keys go to the shell (Ctrl+G for navigation)".into(),
            Mode::Nav => "Navigation mode — j/k move, ? for help (Ctrl+G back to shell)".into(),
        };
        return Ok(());
    }

    match app.mode {
        Mode::Shell => {
            if let Some(bytes) = key_to_bytes(&key) {
                shell.write_input(&bytes)?;
            }
            Ok(())
        }
        Mode::Nav => handle_nav_key(app, engine, terminal, shell, key),
    }
}

fn handle_nav_key(
    app: &mut App,
    engine: &mut Engine,
    terminal: &mut Term,
    shell: &mut PtyShell,
    key: KeyEvent,
) -> Result<()> {
    match std::mem::replace(&mut app.overlay, Overlay::None) {
        Overlay::Help => {
            // Any of ?/Esc/q closes; otherwise reopen.
            if !matches!(
                key.code,
                KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q')
            ) {
                app.overlay = Overlay::Help;
            }
            Ok(())
        }
        Overlay::Message(_) => Ok(()), // any key dismisses
        Overlay::ConfirmRestore => {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => app.confirm_restore(engine)?,
                _ => {
                    app.pending_restore = None;
                    app.status = "Restore cancelled.".into();
                }
            }
            Ok(())
        }
        Overlay::Search => {
            handle_search_key(app, engine, key)?;
            Ok(())
        }
        Overlay::None => handle_nav_main(app, engine, terminal, shell, key),
    }
}

fn handle_search_key(app: &mut App, engine: &mut Engine, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => {
            app.search_input.clear();
            app.search_results.clear();
        }
        KeyCode::Enter => {
            app.run_search(engine)?;
            app.overlay = Overlay::Search;
        }
        KeyCode::Backspace => {
            app.search_input.pop();
            app.run_search(engine)?;
            app.overlay = Overlay::Search;
        }
        KeyCode::Char(c) => {
            app.search_input.push(c);
            app.run_search(engine)?;
            app.overlay = Overlay::Search;
        }
        _ => app.overlay = Overlay::Search,
    }
    Ok(())
}

fn handle_nav_main(
    app: &mut App,
    engine: &mut Engine,
    terminal: &mut Term,
    shell: &mut PtyShell,
    key: KeyEvent,
) -> Result<()> {
    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('?') => app.overlay = Overlay::Help,
        KeyCode::Char('/') => {
            app.search_input.clear();
            app.search_results.clear();
            app.overlay = Overlay::Search;
        }
        KeyCode::Tab => {
            app.focus = match app.focus {
                Focus::Timeline => Focus::Detail,
                Focus::Detail => Focus::Timeline,
            };
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if app.focus == Focus::Detail {
                app.detail.scroll = app.detail.scroll.saturating_add(1);
            } else {
                app.select_next();
                app.recompute_detail(engine)?;
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if app.focus == Focus::Detail {
                app.detail.scroll = app.detail.scroll.saturating_sub(1);
            } else {
                app.select_prev();
                app.recompute_detail(engine)?;
            }
        }
        KeyCode::Char('g') => {
            app.select_first();
            app.recompute_detail(engine)?;
        }
        KeyCode::Char('G') => {
            app.select_last();
            app.recompute_detail(engine)?;
        }
        KeyCode::Enter => {
            app.focus = Focus::Detail;
            app.recompute_detail(engine)?;
        }
        KeyCode::Char('c') => app.checkpoint(engine)?,
        KeyCode::Char('r') => {
            app.begin_restore_selected(engine)?;
        }
        KeyCode::Char('u') => app.undo_last_restore(engine)?,
        KeyCode::Char('t') => run_test_interactive(app, engine, terminal, shell)?,
        KeyCode::Esc => {}
        _ => {}
    }
    Ok(())
}

/// Run the configured test command from the TUI. The interface pauses redraws
/// while the test runs (the shell keeps rendering in the background thread).
fn run_test_interactive(
    app: &mut App,
    engine: &mut Engine,
    terminal: &mut Term,
    shell: &mut PtyShell,
) -> Result<()> {
    if engine.test_command().is_none() {
        app.overlay = Overlay::Message(
            "No test command configured.\nAdd `test_command = \"...\"` to .rewind.toml, \
             then press `t` again."
                .into(),
        );
        return Ok(());
    }
    app.status = "Running test…".into();
    terminal.draw(|f| ui::render(f, app, shell.parser()))?;

    let cancel = CancelToken::new();
    let result = engine.run_test(&cancel, |_| {})?;
    app.status = format!(
        "test {} in {}",
        if result.passed { "passed" } else { "failed" },
        crate::util::format_duration_ms(result.outcome.duration_ms)
    );
    app.refresh(engine)?;
    app.select_first();
    app.recompute_detail(engine)?;
    Ok(())
}

/// Tail the shell command log, forwarding each captured command line.
fn tail_commands(path: PathBuf, tx: crossbeam_channel::Sender<String>) {
    // Wait for the file to exist.
    let mut waited = 0;
    while !path.exists() && waited < 100 {
        std::thread::sleep(Duration::from_millis(50));
        waited += 1;
    }
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                // EOF: wait for more to be appended.
                std::thread::sleep(Duration::from_millis(120));
            }
            Ok(_) => {
                let cmd = line.trim_end_matches(['\n', '\r']).trim().to_string();
                if !cmd.is_empty() && tx.send(cmd).is_err() {
                    return; // consumer gone
                }
            }
            Err(_) => return,
        }
    }
}

/// Translate a crossterm key event into the bytes a terminal would send to the
/// PTY. Covers the keys needed for interactive shell use.
fn key_to_bytes(key: &KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    let mut out: Vec<u8> = match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                // Control code: Ctrl-A..Ctrl-Z and a few punctuation.
                let b = ctrl_byte(c)?;
                vec![b]
            } else {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf).as_bytes().to_vec()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => vec![0x1b, b'[', b'Z'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::F(n) => function_key(n)?,
        _ => return None,
    };

    // Alt/Meta prefixes the sequence with ESC (for printable/control input).
    if alt {
        let mut prefixed = Vec::with_capacity(out.len() + 1);
        prefixed.push(0x1b);
        prefixed.append(&mut out);
        return Some(prefixed);
    }
    Some(out)
}

fn ctrl_byte(c: char) -> Option<u8> {
    let lc = c.to_ascii_lowercase();
    match lc {
        'a'..='z' => Some((lc as u8 - b'a') + 1), // Ctrl-A = 1 .. Ctrl-Z = 26
        ' ' | '@' => Some(0),
        '[' => Some(27),
        '\\' => Some(28),
        ']' => Some(29),
        '^' => Some(30),
        '_' => Some(31),
        _ => None,
    }
}

fn function_key(n: u8) -> Option<Vec<u8>> {
    let seq: &[u8] = match n {
        1 => b"\x1bOP",
        2 => b"\x1bOQ",
        3 => b"\x1bOR",
        4 => b"\x1bOS",
        5 => b"\x1b[15~",
        6 => b"\x1b[17~",
        7 => b"\x1b[18~",
        8 => b"\x1b[19~",
        9 => b"\x1b[20~",
        10 => b"\x1b[21~",
        11 => b"\x1b[23~",
        12 => b"\x1b[24~",
        _ => return None,
    };
    Some(seq.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn plain_char_maps_to_utf8() {
        assert_eq!(
            key_to_bytes(&key(KeyCode::Char('a'), KeyModifiers::NONE)),
            Some(vec![b'a'])
        );
    }

    #[test]
    fn ctrl_c_maps_to_etx() {
        assert_eq!(
            key_to_bytes(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(vec![3])
        );
    }

    #[test]
    fn arrows_map_to_csi() {
        assert_eq!(
            key_to_bytes(&key(KeyCode::Up, KeyModifiers::NONE)),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            key_to_bytes(&key(KeyCode::Left, KeyModifiers::NONE)),
            Some(b"\x1b[D".to_vec())
        );
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(
            key_to_bytes(&key(KeyCode::Char('b'), KeyModifiers::ALT)),
            Some(vec![0x1b, b'b'])
        );
    }

    #[test]
    fn enter_and_backspace() {
        assert_eq!(
            key_to_bytes(&key(KeyCode::Enter, KeyModifiers::NONE)),
            Some(vec![b'\r'])
        );
        assert_eq!(
            key_to_bytes(&key(KeyCode::Backspace, KeyModifiers::NONE)),
            Some(vec![0x7f])
        );
    }
}
