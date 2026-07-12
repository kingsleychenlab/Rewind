//! Non-interactive CI mode.
//!
//! `rewind ci -- <test-command>` runs the command with pre/post snapshots (no
//! TUI), then writes a machine- and human-readable report to
//! `.rewind-report/`:
//!
//! ```text
//! .rewind-report/
//!   report.json     structured result
//!   summary.md      human-readable summary
//!   test.log        full combined output
//!   changes.patch   unified diff of the relevant changes
//! ```

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::diff::{self, ChangeStatus, FileChange};
use crate::error::Result;
use crate::exec::{CancelToken, DependencyChange, OutputChunk};
use crate::investigate::LikelyCause;
use crate::session::Engine;
use crate::util::{format_duration_ms, format_timestamp, now_millis};

/// Default report directory name.
pub const REPORT_DIR: &str = ".rewind-report";

/// A per-file change entry in the report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub status: String,
    pub added: usize,
    pub removed: usize,
}

/// The full CI report, serialized to `report.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CiReport {
    pub rewind_version: String,
    pub generated_at: String,
    pub command: String,
    pub passed: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub duration_ms: u64,
    pub changed_files: Vec<ChangedFile>,
    pub dependency_changes: Vec<DependencyChange>,
    pub likely_causes: Vec<LikelyCause>,
    pub investigated: bool,
    pub summary: String,
}

impl CiReport {
    /// The process exit code CI should propagate.
    pub fn exit_code_for_process(&self) -> i32 {
        if self.passed {
            0
        } else {
            self.exit_code.unwrap_or(1)
        }
    }
}

/// Run the test command non-interactively and produce a report under `out_dir`.
///
/// When `command_override` is provided it replaces the configured test command
/// for this run. Output is streamed to stdout when `live` is true.
pub fn run(
    engine: &mut Engine,
    command_override: Option<&str>,
    out_dir: &Path,
    cancel: &CancelToken,
    live: bool,
) -> Result<CiReport> {
    if let Some(cmd) = command_override {
        engine.config.test_command = Some(cmd.to_string());
    }

    let mut stdout = std::io::stdout();
    let on_chunk = |chunk: &OutputChunk| {
        if live {
            let _ = stdout.write_all(&chunk.data);
            let _ = stdout.flush();
        }
    };
    let result = engine.run_test(cancel, on_chunk)?;

    // Choose the diff base: the last passing run when an investigation exists,
    // otherwise this run's own pre-test snapshot.
    let base_snapshot = result
        .investigation
        .as_ref()
        .and_then(|inv| inv.passing_test_run_id)
        .and_then(|pid| engine.get_test_run_post(pid).ok().flatten())
        .unwrap_or(result.pre_snapshot);

    let changes = diff::diff_snapshots(&engine.db, base_snapshot, result.post_snapshot)?;
    let changed_files: Vec<ChangedFile> = changes
        .iter()
        .map(|c| {
            let stat = diff::line_stat(&engine.store, c.old_hash.as_deref(), c.new_hash.as_deref())
                .unwrap_or_default();
            ChangedFile {
                path: c.path.clone(),
                status: status_word(&c.status).to_string(),
                added: stat.added,
                removed: stat.removed,
            }
        })
        .collect();

    let command = engine.test_command().unwrap_or("<none>").to_string();

    let (dependency_changes, likely_causes, investigated) = match &result.investigation {
        Some(inv) => (inv.dependency_changes.clone(), inv.causes.clone(), true),
        None => (Vec::new(), Vec::new(), false),
    };

    let summary = summary_line(&result, &changed_files);
    let report = CiReport {
        rewind_version: crate::VERSION.to_string(),
        generated_at: format_timestamp(now_millis()),
        command,
        passed: result.passed,
        exit_code: result.outcome.exit_code,
        timed_out: result.outcome.timed_out,
        cancelled: result.outcome.cancelled,
        duration_ms: result.outcome.duration_ms,
        changed_files,
        dependency_changes,
        likely_causes,
        investigated,
        summary,
    };

    write_report(&report, &result.outcome.combined, &changes, engine, out_dir)?;
    Ok(report)
}

fn write_report(
    report: &CiReport,
    log: &[u8],
    changes: &[FileChange],
    engine: &Engine,
    out_dir: &Path,
) -> Result<()> {
    std::fs::create_dir_all(out_dir)?;

    // report.json
    let json = serde_json::to_string_pretty(report)?;
    std::fs::write(out_dir.join("report.json"), json)?;

    // test.log
    std::fs::write(out_dir.join("test.log"), log)?;

    // summary.md
    std::fs::write(out_dir.join("summary.md"), render_summary_md(report))?;

    // changes.patch
    let mut patch = String::new();
    for c in changes {
        match &c.status {
            ChangeStatus::Deleted => {
                patch.push_str(&format!("--- a/{}\n+++ /dev/null\n", c.path));
            }
            _ => {
                match diff::unified_diff(
                    &engine.store,
                    &c.path,
                    c.old_hash.as_deref(),
                    c.new_hash.as_deref(),
                ) {
                    Some(d) if !d.is_empty() => patch.push_str(&d),
                    Some(_) => {}
                    None => {
                        patch.push_str(&format!("Binary file {} changed\n", c.path));
                    }
                }
            }
        }
        if !patch.ends_with('\n') {
            patch.push('\n');
        }
    }
    std::fs::write(out_dir.join("changes.patch"), patch)?;
    Ok(())
}

fn render_summary_md(report: &CiReport) -> String {
    let mut md = String::new();
    let verdict = if report.passed {
        "✅ PASSED"
    } else {
        "❌ FAILED"
    };
    md.push_str(&format!("# Rewind CI Report — {verdict}\n\n"));
    md.push_str(&format!("- **Command:** `{}`\n", report.command));
    md.push_str(&format!(
        "- **Exit code:** {}\n",
        report
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "killed".into())
    ));
    md.push_str(&format!(
        "- **Duration:** {}\n",
        format_duration_ms(report.duration_ms)
    ));
    if report.timed_out {
        md.push_str("- **Timed out:** yes\n");
    }
    if report.cancelled {
        md.push_str("- **Cancelled:** yes\n");
    }
    md.push_str(&format!("- **Generated:** {}\n", report.generated_at));
    md.push_str(&format!("- **Rewind:** v{}\n\n", report.rewind_version));

    md.push_str(&format!(
        "## Changed files ({})\n\n",
        report.changed_files.len()
    ));
    if report.changed_files.is_empty() {
        md.push_str("_No tracked file changes._\n\n");
    } else {
        md.push_str("| Status | File | +/- |\n|---|---|---|\n");
        for f in &report.changed_files {
            md.push_str(&format!(
                "| {} | `{}` | +{} / -{} |\n",
                f.status, f.path, f.added, f.removed
            ));
        }
        md.push('\n');
    }

    if !report.dependency_changes.is_empty() {
        md.push_str("## Dependency changes\n\n");
        for d in &report.dependency_changes {
            md.push_str(&format!(
                "- `{}` in `{}`: {} → {}\n",
                d.name,
                d.file,
                d.old.as_deref().unwrap_or("—"),
                d.new.as_deref().unwrap_or("—"),
            ));
        }
        md.push('\n');
    }

    if report.investigated {
        md.push_str("## Likely causes\n\n");
        md.push_str(
            "_Ranked by relevance score. These are likely causes, not confirmed diagnoses._\n\n",
        );
        for (i, cause) in report.likely_causes.iter().take(10).enumerate() {
            md.push_str(&format!(
                "{}. `{}` — relevance {}\n",
                i + 1,
                cause.path,
                cause.score
            ));
            for ev in &cause.evidence {
                md.push_str(&format!("   - {ev}\n"));
            }
        }
        md.push('\n');
    } else if !report.passed {
        md.push_str("## Likely causes\n\n_No prior passing run to compare against, so no ranking was produced._\n\n");
    }

    md
}

fn summary_line(result: &crate::session::TestResult, changed: &[ChangedFile]) -> String {
    let verdict = if result.passed { "passed" } else { "failed" };
    format!(
        "test {verdict} in {}; {} changed file(s)",
        format_duration_ms(result.outcome.duration_ms),
        changed.len()
    )
}

fn status_word(s: &ChangeStatus) -> &'static str {
    match s {
        ChangeStatus::Added => "added",
        ChangeStatus::Modified => "modified",
        ChangeStatus::Deleted => "deleted",
        ChangeStatus::Renamed { .. } => "renamed",
    }
}

/// Resolve the report directory: `path` if given, else `<repo_root>/.rewind-report`.
pub fn resolve_out_dir(repo_root: &Path, path: Option<&Path>) -> PathBuf {
    match path {
        Some(p) if p.is_absolute() => p.to_path_buf(),
        Some(p) => repo_root.join(p),
        None => repo_root.join(REPORT_DIR),
    }
}
