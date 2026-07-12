//! Command execution with live streaming, full capture, timeout, and
//! cancellation.
//!
//! This module is deliberately unopinionated about snapshots and the database;
//! it just runs a command through the user's shell and reports exactly what
//! happened. Pass/fail is derived from the exit code by callers — never from
//! output text.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, RecvTimeoutError};

use crate::error::{Result, RewindError};

/// Which stream a chunk of output came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Stdout,
    Stderr,
}

/// A chunk of live output.
#[derive(Debug, Clone)]
pub struct OutputChunk {
    pub kind: StreamKind,
    pub data: Vec<u8>,
}

/// What to run.
#[derive(Debug, Clone)]
pub struct RunSpec {
    /// Command line, interpreted by the shell (so pipes/operators work).
    pub command: String,
    /// Working directory (normally the repository root).
    pub cwd: PathBuf,
    /// Shell binary to invoke with `-c`.
    pub shell: String,
    /// Optional wall-clock timeout. `None` means no limit.
    pub timeout: Option<Duration>,
}

impl RunSpec {
    /// Build a spec running `command` in `cwd` using the resolved user shell.
    pub fn new(command: impl Into<String>, cwd: impl Into<PathBuf>) -> RunSpec {
        RunSpec {
            command: command.into(),
            cwd: cwd.into(),
            shell: resolve_shell(),
            timeout: None,
        }
    }

    /// Set an optional timeout (`0` seconds disables it).
    pub fn with_timeout_secs(mut self, secs: u64) -> RunSpec {
        self.timeout = if secs == 0 {
            None
        } else {
            Some(Duration::from_secs(secs))
        };
        self
    }
}

/// The full result of a run.
#[derive(Debug, Clone)]
pub struct CommandOutcome {
    /// Process exit code, or `None` if it was killed by a signal.
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub cancelled: bool,
    pub timed_out: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// stdout and stderr interleaved in arrival order — the "log".
    pub combined: Vec<u8>,
}

impl CommandOutcome {
    /// True only when the process exited with code 0 and was neither cancelled
    /// nor timed out.
    pub fn passed(&self) -> bool {
        !self.cancelled && !self.timed_out && self.exit_code == Some(0)
    }

    /// The combined log as a lossy UTF-8 string.
    pub fn log_string(&self) -> String {
        String::from_utf8_lossy(&self.combined).into_owned()
    }
}

/// A cooperative cancellation flag shared with a running command.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> CancelToken {
        CancelToken(Arc::new(AtomicBool::new(false)))
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Resolve the shell to run commands with: `$SHELL` on Unix (falling back to
/// `/bin/sh`), `cmd` on Windows.
pub fn resolve_shell() -> String {
    if cfg!(windows) {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd".into())
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
    }
}

fn build_command(spec: &RunSpec) -> Command {
    let mut cmd = Command::new(&spec.shell);
    if cfg!(windows) {
        cmd.arg("/C");
    } else {
        cmd.arg("-c");
    }
    cmd.arg(&spec.command);
    cmd.current_dir(&spec.cwd);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd
}

/// Run a command, forwarding output chunks to `on_chunk` as they arrive while
/// also capturing them. Honors cancellation and timeout by killing the child.
pub fn run_streaming<F>(
    spec: &RunSpec,
    cancel: &CancelToken,
    mut on_chunk: F,
) -> Result<CommandOutcome>
where
    F: FnMut(&OutputChunk),
{
    let start = Instant::now();
    let mut child = build_command(spec)
        .spawn()
        .map_err(|e| RewindError::other(format!("failed to launch `{}`: {e}", spec.command)))?;

    let (tx, rx) = unbounded::<OutputChunk>();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut readers = Vec::new();
    if let Some(out) = stdout {
        readers.push(spawn_reader(out, StreamKind::Stdout, tx.clone()));
    }
    if let Some(err) = stderr {
        readers.push(spawn_reader(err, StreamKind::Stderr, tx.clone()));
    }
    drop(tx); // channel closes once both readers finish

    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();
    let mut combined = Vec::new();
    let mut disconnected = false;
    let mut killed = false;
    let mut cancelled = false;
    let mut timed_out = false;

    let exit_code = loop {
        if !disconnected {
            match rx.recv_timeout(Duration::from_millis(40)) {
                Ok(chunk) => {
                    match chunk.kind {
                        StreamKind::Stdout => stdout_buf.extend_from_slice(&chunk.data),
                        StreamKind::Stderr => stderr_buf.extend_from_slice(&chunk.data),
                    }
                    combined.extend_from_slice(&chunk.data);
                    on_chunk(&chunk);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => disconnected = true,
            }
        } else {
            std::thread::sleep(Duration::from_millis(15));
        }

        if !killed {
            if cancel.is_cancelled() {
                let _ = child.kill();
                killed = true;
                cancelled = true;
            } else if let Some(limit) = spec.timeout {
                if start.elapsed() >= limit {
                    let _ = child.kill();
                    killed = true;
                    timed_out = true;
                }
            }
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                if disconnected {
                    break status.code();
                }
            }
            Ok(None) => {}
            Err(e) => return Err(e.into()),
        }
    };

    for r in readers {
        let _ = r.join();
    }

    Ok(CommandOutcome {
        exit_code,
        duration_ms: start.elapsed().as_millis() as u64,
        cancelled,
        timed_out,
        stdout: stdout_buf,
        stderr: stderr_buf,
        combined,
    })
}

/// Convenience wrapper that discards live output and just returns the outcome.
pub fn run_captured(spec: &RunSpec, cancel: &CancelToken) -> Result<CommandOutcome> {
    run_streaming(spec, cancel, |_| {})
}

fn spawn_reader<R: Read + Send + 'static>(
    mut reader: R,
    kind: StreamKind,
    tx: crossbeam_channel::Sender<OutputChunk>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx
                        .send(OutputChunk {
                            kind,
                            data: buf[..n].to_vec(),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    })
}

/// A dependency-manifest change, surfaced by investigations and reports.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DependencyChange {
    pub file: String,
    pub name: String,
    pub old: Option<String>,
    pub new: Option<String>,
}

/// Files whose changes indicate dependency version churn.
pub const DEPENDENCY_MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "package.json",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "requirements.txt",
    "Pipfile",
    "Pipfile.lock",
    "poetry.lock",
    "pyproject.toml",
    "go.mod",
    "go.sum",
    "Gemfile",
    "Gemfile.lock",
    "composer.json",
    "composer.lock",
    "pom.xml",
    "build.gradle",
];

/// Whether a path is a recognized dependency manifest (matched on basename).
pub fn is_dependency_manifest(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    DEPENDENCY_MANIFESTS.contains(&base)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_spec(cmd: &str) -> (tempfile::TempDir, RunSpec) {
        let tmp = tempfile::tempdir().unwrap();
        let spec = RunSpec::new(cmd, tmp.path().to_path_buf());
        (tmp, spec)
    }

    #[test]
    fn captures_stdout_and_exit_zero() {
        let (_t, spec) = tmp_spec("echo hello");
        let out = run_captured(&spec, &CancelToken::new()).unwrap();
        assert!(out.passed());
        assert_eq!(out.exit_code, Some(0));
        assert!(String::from_utf8_lossy(&out.stdout).contains("hello"));
    }

    #[test]
    fn nonzero_exit_is_failure() {
        let (_t, spec) = tmp_spec("exit 3");
        let out = run_captured(&spec, &CancelToken::new()).unwrap();
        assert_eq!(out.exit_code, Some(3));
        assert!(!out.passed());
    }

    #[test]
    fn captures_stderr() {
        let (_t, spec) = tmp_spec("echo oops 1>&2");
        let out = run_captured(&spec, &CancelToken::new()).unwrap();
        assert!(String::from_utf8_lossy(&out.stderr).contains("oops"));
    }

    #[test]
    fn timeout_is_enforced() {
        let (_t, mut spec) = tmp_spec("sleep 10");
        spec.timeout = Some(Duration::from_millis(300));
        let out = run_captured(&spec, &CancelToken::new()).unwrap();
        assert!(out.timed_out);
        assert!(!out.passed());
        assert!(out.duration_ms < 5000, "should have been killed promptly");
    }

    #[test]
    fn cancellation_stops_the_command() {
        let (_t, spec) = tmp_spec("sleep 10");
        let cancel = CancelToken::new();
        let c2 = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            c2.cancel();
        });
        let out = run_captured(&spec, &cancel).unwrap();
        assert!(out.cancelled);
        assert!(out.duration_ms < 5000);
    }

    #[test]
    fn dependency_manifest_detection() {
        assert!(is_dependency_manifest("package.json"));
        assert!(is_dependency_manifest("sub/dir/Cargo.lock"));
        assert!(!is_dependency_manifest("src/main.rs"));
    }
}
