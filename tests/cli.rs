//! Black-box tests of the `rewind` binary itself.

use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use predicates::prelude::*;

fn git(dir: &Path, args: &[&str]) {
    assert!(StdCommand::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap()
        .status
        .success());
}

/// A temp git repo plus an isolated data directory for `REWIND_DATA_DIR`.
fn repo() -> (tempfile::TempDir, tempfile::TempDir) {
    let work = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    git(work.path(), &["init"]);
    git(work.path(), &["config", "user.email", "t@example.com"]);
    git(work.path(), &["config", "user.name", "Test"]);
    (work, data)
}

fn rewind(work: &Path, data: &Path) -> Command {
    let mut cmd = Command::cargo_bin("rewind").unwrap();
    cmd.current_dir(work).env("REWIND_DATA_DIR", data);
    cmd
}

#[test]
fn version_prints() {
    Command::cargo_bin("rewind")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("rewind"));
}

#[test]
fn doctor_reports_repo() {
    let (work, data) = repo();
    rewind(work.path(), data.path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("Rewind v"))
        .stdout(predicate::str::contains("repository:"));
}

#[test]
fn init_creates_config() {
    let (work, data) = repo();
    rewind(work.path(), data.path())
        .args(["init", "--test-command", "true"])
        .assert()
        .success()
        .stdout(predicate::str::contains(".rewind.toml"));
    assert!(work.path().join(".rewind.toml").exists());
}

#[test]
fn run_streams_and_snapshots() {
    let (work, data) = repo();
    rewind(work.path(), data.path())
        .args(["run", "--", "echo", "hello-rewind"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello-rewind"));
}

#[test]
fn checkpoint_then_diff() {
    let (work, data) = repo();
    std::fs::write(work.path().join("f.txt"), "one\n").unwrap();
    rewind(work.path(), data.path())
        .args(["checkpoint", "base"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Created checkpoint"));
}

#[test]
fn test_command_pass_and_fail_exit_codes() {
    let (work, data) = repo();
    rewind(work.path(), data.path())
        .args(["init", "--test-command", "true"])
        .assert()
        .success();
    rewind(work.path(), data.path())
        .arg("test")
        .assert()
        .success();

    rewind(work.path(), data.path())
        .args(["test", "--command", "false"])
        .assert()
        .failure();
}

#[test]
fn outside_git_repo_is_friendly() {
    let data = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();
    Command::cargo_bin("rewind")
        .unwrap()
        .current_dir(empty.path())
        .env("REWIND_DATA_DIR", data.path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("repository:"));
}
