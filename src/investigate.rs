//! Deterministic failure investigation.
//!
//! When a passing test run is followed by a failing one, Rewind compares the
//! last passing post-test snapshot with the failing post-test snapshot and
//! ranks changed files by a **relevance score** — never a confidence or
//! probability. Results are *likely causes*, not confirmed diagnoses.
//!
//! Scoring (all additive, fully deterministic):
//!
//! | signal                                   | delta |
//! |------------------------------------------|-------|
//! | appears in a stack-trace line            |  +5   |
//! | dependency version changed               |  +4   |
//! | changed between pass and failure         |  +3   |
//! | path appears in failing output           |  +3   |
//! | changed immediately before the failure   |  +2   |
//! | generated or unrelated file              |  -3   |

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::db::models::{self, TestRun};
use crate::db::Db;
use crate::diff::{self, ChangeStatus, FileChange};
use crate::error::Result;
use crate::exec::{is_dependency_manifest, DependencyChange};
use crate::objects::ObjectStore;

/// Score weights, exposed for documentation and testing.
pub mod weight {
    pub const STACK_TRACE: i32 = 5;
    pub const DEPENDENCY: i32 = 4;
    pub const CHANGED_PASS_TO_FAIL: i32 = 3;
    pub const IN_OUTPUT: i32 = 3;
    pub const CHANGED_BEFORE: i32 = 2;
    pub const GENERATED: i32 = -3;
}

/// The signals gathered for one changed file, independent of how they were
/// detected. [`score`] turns this into a number.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CauseSignals {
    pub changed_between_pass_and_fail: bool,
    pub changed_before_failure: bool,
    pub in_stack_trace: bool,
    pub in_failing_output: bool,
    pub dependency_changed: bool,
    pub generated_or_unrelated: bool,
}

/// Pure, deterministic relevance score for a set of signals.
pub fn score(s: &CauseSignals) -> i32 {
    let mut total = 0;
    if s.in_stack_trace {
        total += weight::STACK_TRACE;
    }
    if s.dependency_changed {
        total += weight::DEPENDENCY;
    }
    if s.changed_between_pass_and_fail {
        total += weight::CHANGED_PASS_TO_FAIL;
    }
    if s.in_failing_output {
        total += weight::IN_OUTPUT;
    }
    if s.changed_before_failure {
        total += weight::CHANGED_BEFORE;
    }
    if s.generated_or_unrelated {
        total += weight::GENERATED;
    }
    total
}

/// Human-readable evidence lines corresponding to the signals that fired.
pub fn evidence(s: &CauseSignals) -> Vec<String> {
    let mut ev = Vec::new();
    if s.in_stack_trace {
        ev.push(format!(
            "appears in a stack-trace line (+{})",
            weight::STACK_TRACE
        ));
    }
    if s.dependency_changed {
        ev.push(format!(
            "dependency version changed (+{})",
            weight::DEPENDENCY
        ));
    }
    if s.changed_between_pass_and_fail {
        ev.push(format!(
            "changed between the last pass and the failure (+{})",
            weight::CHANGED_PASS_TO_FAIL
        ));
    }
    if s.in_failing_output {
        ev.push(format!(
            "path appears in failing output (+{})",
            weight::IN_OUTPUT
        ));
    }
    if s.changed_before_failure {
        ev.push(format!(
            "changed immediately before the failing run (+{})",
            weight::CHANGED_BEFORE
        ));
    }
    if s.generated_or_unrelated {
        ev.push(format!(
            "looks generated or unrelated ({})",
            weight::GENERATED
        ));
    }
    ev
}

/// A ranked likely cause. `score` is a relevance score, not a probability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LikelyCause {
    pub path: String,
    pub score: i32,
    #[serde(with = "change_status_serde")]
    pub status: ChangeStatus,
    pub evidence: Vec<String>,
    pub changed_lines: Vec<usize>,
    pub relevant_output: Vec<String>,
    pub dependency_changes: Vec<DependencyChange>,
}

// ChangeStatus already derives Serialize; this wrapper keeps LikelyCause's
// field flattened cleanly.
mod change_status_serde {
    use super::ChangeStatus;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    pub fn serialize<S: Serializer>(v: &ChangeStatus, s: S) -> Result<S::Ok, S::Error> {
        v.serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<ChangeStatus, D::Error> {
        ChangeStatus::deserialize(d)
    }
}

/// The complete outcome of an investigation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Investigation {
    pub failing_test_run_id: i64,
    pub passing_test_run_id: Option<i64>,
    pub summary: String,
    pub causes: Vec<LikelyCause>,
    pub dependency_changes: Vec<DependencyChange>,
    pub failing_output_excerpt: String,
    pub related_command: Option<String>,
}

/// Investigate a failing test run against the most recent passing run before
/// it. Returns `None` when there is no prior passing run to compare against.
pub fn investigate(
    db: &Db,
    store: &ObjectStore,
    repo_id: i64,
    failing: &TestRun,
) -> Result<Option<Investigation>> {
    if failing.passed != Some(false) {
        return Ok(None);
    }
    let passing = models::last_passing_before(db.conn(), repo_id, failing.started_at)?;
    let (Some(passing), Some(failing_post)) = (passing.as_ref(), failing.post_snapshot_id) else {
        return Ok(None);
    };
    let Some(passing_post) = passing.post_snapshot_id else {
        return Ok(None);
    };

    let main_changes = diff::diff_snapshots(db, passing_post, failing_post)?;
    let before: BTreeSet<String> = match failing.pre_snapshot_id {
        Some(pre) => diff::diff_snapshots(db, passing_post, pre)?
            .into_iter()
            .map(|c| c.path)
            .collect(),
        None => BTreeSet::new(),
    };

    let output = failing
        .log_object
        .as_deref()
        .map(|h| {
            store
                .read(h)
                .map(|b| String::from_utf8_lossy(&b).into_owned())
        })
        .transpose()?
        .unwrap_or_default();
    let stack_lines: Vec<&str> = output.lines().filter(|l| is_stack_frame_line(l)).collect();

    let mut all_dep_changes: Vec<DependencyChange> = Vec::new();
    let mut causes: Vec<LikelyCause> = Vec::new();

    for change in &main_changes {
        let dep_changes = if is_dependency_manifest(&change.path) {
            dependency_diff(store, change)
        } else {
            Vec::new()
        };
        all_dep_changes.extend(dep_changes.iter().cloned());

        let in_output = output_mentions(&output, &change.path);
        let in_stack = stack_lines.iter().any(|l| line_mentions(l, &change.path));
        let signals = CauseSignals {
            changed_between_pass_and_fail: true,
            changed_before_failure: before.contains(&change.path),
            in_stack_trace: in_stack,
            in_failing_output: in_output,
            dependency_changed: !dep_changes.is_empty(),
            generated_or_unrelated: is_generated_or_unrelated(&change.path),
        };
        let changed_lines = match &change.status {
            ChangeStatus::Deleted => Vec::new(),
            _ => diff::changed_new_lines(
                store,
                change.old_hash.as_deref(),
                change.new_hash.as_deref(),
            ),
        };
        let relevant_output = relevant_output_lines(&output, &change.path, 4);

        causes.push(LikelyCause {
            path: change.path.clone(),
            score: score(&signals),
            status: change.status.clone(),
            evidence: evidence(&signals),
            changed_lines,
            relevant_output,
            dependency_changes: dep_changes,
        });
    }

    // Rank: highest score first, then path for a stable order.
    causes.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));

    let summary = build_summary(&causes, main_changes.len());
    Ok(Some(Investigation {
        failing_test_run_id: failing.id,
        passing_test_run_id: Some(passing.id),
        summary,
        causes,
        dependency_changes: all_dep_changes,
        failing_output_excerpt: tail_excerpt(&output, 60),
        related_command: Some(failing.command.clone()),
    }))
}

fn build_summary(causes: &[LikelyCause], total_changes: usize) -> String {
    if causes.is_empty() {
        return "No tracked file changes between the last passing and failing runs.".to_string();
    }
    let top = &causes[0];
    format!(
        "{total_changes} changed file(s); most relevant: {} (relevance {})",
        top.path, top.score
    )
}

/// Whether a log line looks like a stack-trace frame.
pub fn is_stack_frame_line(line: &str) -> bool {
    let l = line.trim_start();
    if l.starts_with("at ")
        || l.starts_with("File \"")
        || l.starts_with("from ")
        || l.contains("panicked at")
        || l.contains("Traceback")
        || l.contains("stack backtrace")
    {
        return true;
    }
    // `path.ext:line` style references for common languages.
    const EXT_MARKERS: &[&str] = &[
        ".rs:", ".py:", ".py\"", ".js:", ".jsx:", ".ts:", ".tsx:", ".go:", ".rb:", ".java:",
        ".kt:", ".c:", ".cc:", ".cpp:", ".h:", ".ex:", ".exs:", ".php:", ".cs:", ".swift:",
    ];
    EXT_MARKERS.iter().any(|m| l.contains(m))
}

/// Whether the failing output mentions a path (by full relative path or by
/// basename).
pub fn output_mentions(output: &str, path: &str) -> bool {
    if output.contains(path) {
        return true;
    }
    let base = basename(path);
    base.len() >= 3 && output.contains(base)
}

fn line_mentions(line: &str, path: &str) -> bool {
    line.contains(path) || {
        let base = basename(path);
        base.len() >= 3 && line.contains(base)
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Up to `max` output lines that mention this path, for display as evidence.
fn relevant_output_lines(output: &str, path: &str, max: usize) -> Vec<String> {
    output
        .lines()
        .filter(|l| line_mentions(l, path))
        .take(max)
        .map(|l| l.trim_end().to_string())
        .collect()
}

/// Deterministic "generated or unrelated" detection based on path patterns.
pub fn is_generated_or_unrelated(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    let base = basename(&p);
    const GENERATED_SUBSTR: &[&str] = &[
        "/generated/",
        "generated/",
        ".generated.",
        "/__generated__/",
        "/.gen/",
        "/gen/",
        "/snapshots/",
        "__snapshots__",
    ];
    const GENERATED_SUFFIX: &[&str] = &[
        ".min.js",
        ".min.css",
        ".map",
        ".pb.go",
        "_pb2.py",
        ".g.dart",
        ".freezed.dart",
        ".d.ts",
    ];
    if GENERATED_SUBSTR.iter().any(|s| p.contains(s)) {
        return true;
    }
    if GENERATED_SUFFIX.iter().any(|s| base.ends_with(s)) {
        return true;
    }
    false
}

/// Parse a dependency manifest change into per-name version changes.
///
/// Best-effort and format-agnostic: it extracts `name = version`-style pairs
/// (TOML, `package.json`, `requirements.txt`, lockfiles) from the old and new
/// text and diffs the resulting maps.
pub fn dependency_diff(store: &ObjectStore, change: &FileChange) -> Vec<DependencyChange> {
    let old_text = change
        .old_hash
        .as_deref()
        .and_then(|h| store.read(h).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_default();
    let new_text = change
        .new_hash
        .as_deref()
        .and_then(|h| store.read(h).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_default();
    let old = parse_versions(&old_text);
    let new = parse_versions(&new_text);

    let mut out = Vec::new();
    let mut names: BTreeSet<&String> = BTreeSet::new();
    names.extend(old.keys());
    names.extend(new.keys());
    for name in names {
        let o = old.get(name).cloned();
        let n = new.get(name).cloned();
        if o != n {
            out.push(DependencyChange {
                file: change.path.clone(),
                name: name.clone(),
                old: o,
                new: n,
            });
        }
    }
    out
}

/// Extract `name -> version` pairs from a manifest's text using a few common
/// syntaxes. Intentionally conservative to avoid false positives.
fn parse_versions(text: &str) -> std::collections::BTreeMap<String, String> {
    use std::collections::BTreeMap;
    let mut map = BTreeMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
            continue;
        }
        // JSON: "name": "^1.2.3"
        if let Some((k, v)) = parse_json_pair(line) {
            map.insert(k, v);
            continue;
        }
        // TOML / generic: name = "1.2.3"
        if let Some((k, v)) = parse_eq_pair(line) {
            map.insert(k, v);
            continue;
        }
        // requirements.txt: name==1.2.3
        if let Some((k, v)) = line.split_once("==") {
            let k = k.trim();
            let v = v.trim().trim_matches('"');
            if is_ident(k) && !v.is_empty() {
                map.insert(k.to_string(), v.to_string());
            }
        }
    }
    map
}

fn parse_json_pair(line: &str) -> Option<(String, String)> {
    let line = line.trim_end_matches(',');
    let (k, v) = line.split_once(':')?;
    let k = k.trim().trim_matches('"');
    let v = v.trim().trim_matches('"');
    if k.is_empty() || v.is_empty() || !looks_like_version(v) {
        return None;
    }
    if !is_ident(k) {
        return None;
    }
    Some((k.to_string(), v.to_string()))
}

fn parse_eq_pair(line: &str) -> Option<(String, String)> {
    let (k, v) = line.split_once('=')?;
    // Avoid "==" (handled elsewhere) and comparison operators.
    if v.starts_with('=') {
        return None;
    }
    let k = k.trim();
    let v = v.trim().trim_matches('"').trim_matches('\'');
    if !is_ident(k) || v.is_empty() || !looks_like_version(v) {
        return None;
    }
    Some((k.to_string(), v.to_string()))
}

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '@'))
}

fn looks_like_version(v: &str) -> bool {
    let v = v.trim_start_matches(['^', '~', '>', '<', '=', ' ']);
    v.chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
}

/// The last `max` lines of output, for a compact excerpt.
fn tail_excerpt(output: &str, max: usize) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let start = lines.len().saturating_sub(max);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoring_matches_spec_weights() {
        let all = CauseSignals {
            changed_between_pass_and_fail: true,
            changed_before_failure: true,
            in_stack_trace: true,
            in_failing_output: true,
            dependency_changed: true,
            generated_or_unrelated: true,
        };
        // 5 + 4 + 3 + 3 + 2 - 3 = 14
        assert_eq!(score(&all), 14);

        let just_changed = CauseSignals {
            changed_between_pass_and_fail: true,
            ..Default::default()
        };
        assert_eq!(score(&just_changed), 3);

        let generated_only = CauseSignals {
            generated_or_unrelated: true,
            ..Default::default()
        };
        assert_eq!(score(&generated_only), -3);
    }

    #[test]
    fn stack_frame_detection() {
        assert!(is_stack_frame_line("    at src/foo.js:12:5"));
        assert!(is_stack_frame_line("  File \"app/main.py\", line 3"));
        assert!(is_stack_frame_line(
            "thread 'main' panicked at src/lib.rs:9:1"
        ));
        assert!(is_stack_frame_line("Traceback (most recent call last):"));
        assert!(!is_stack_frame_line("all tests passed"));
    }

    #[test]
    fn output_mention_by_basename() {
        assert!(output_mentions(
            "error in src/calc.rs at line 4",
            "src/calc.rs"
        ));
        assert!(output_mentions("ReferenceError in calc.js", "lib/calc.js"));
        assert!(!output_mentions("nothing here", "src/calc.rs"));
    }

    #[test]
    fn generated_detection() {
        assert!(is_generated_or_unrelated("dist/app.min.js"));
        assert!(is_generated_or_unrelated("src/generated/api.rs"));
        assert!(is_generated_or_unrelated("proto/user_pb2.py"));
        assert!(!is_generated_or_unrelated("src/main.rs"));
    }

    #[test]
    fn parse_versions_across_formats() {
        let toml = "serde = \"1.0.2\"\nother = 2";
        let m = parse_versions(toml);
        assert_eq!(m.get("serde").map(String::as_str), Some("1.0.2"));

        let json = "  \"left-pad\": \"^1.3.0\",";
        let m = parse_versions(json);
        assert_eq!(m.get("left-pad").map(String::as_str), Some("^1.3.0"));

        let req = "requests==2.31.0";
        let m = parse_versions(req);
        assert_eq!(m.get("requests").map(String::as_str), Some("2.31.0"));
    }
}
