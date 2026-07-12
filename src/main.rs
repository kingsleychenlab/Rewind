//! Binary entry point.
//!
//! Initializes structured logging (to a file, so it never corrupts the TUI) and
//! dispatches to the CLI. The heavy lifting lives in the library so integration
//! tests exercise the same code paths.

use std::process::ExitCode;

fn main() -> ExitCode {
    init_tracing();
    rewind::cli::run()
}

/// Send `tracing` output to a log file when `REWIND_LOG` names a level, so it
/// never interferes with terminal rendering. Disabled by default.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = match std::env::var("REWIND_LOG") {
        Ok(v) if !v.is_empty() => EnvFilter::new(v),
        _ => return,
    };

    // Log to a file in the current directory's app-data if possible; fall back
    // to stderr only when explicitly requested via REWIND_LOG_STDERR.
    if std::env::var_os("REWIND_LOG_STDERR").is_some() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .try_init();
        return;
    }
    if let Some(dir) = dirs::data_dir() {
        let logs = dir.join("rewind").join("logs");
        if std::fs::create_dir_all(&logs).is_ok() {
            if let Ok(file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(logs.join("rewind.log"))
            {
                let _ = tracing_subscriber::fmt()
                    .with_env_filter(filter)
                    .with_ansi(false)
                    .with_writer(std::sync::Mutex::new(file))
                    .try_init();
            }
        }
    }
}
