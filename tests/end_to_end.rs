//! End-to-end integration tests over temporary Git repositories.
//!
//! These drive the same [`rewind::session::Engine`] the CLI and TUI use, so
//! they exercise real snapshots, real command execution, real investigation,
//! and real restores. File-based test commands are used deliberately to avoid
//! interpreter bytecode caches interfering with pass/fail transitions.

use std::path::Path;
use std::process::Command;

use rewind::config::Config;
use rewind::exec::CancelToken;
use rewind::paths::StoragePaths;
use rewind::repo::Repo;
use rewind::restore::Selection;
use rewind::session::Engine;

/// Build an engine over a fresh temp git repo with isolated storage.
fn setup(test_command: &str) -> (tempfile::TempDir, tempfile::TempDir, Engine) {
    let work = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    git(work.path(), &["init"]);
    git(work.path(), &["config", "user.email", "t@example.com"]);
    git(work.path(), &["config", "user.name", "Test"]);

    let repo = Repo::discover(work.path()).unwrap();
    let config = Config {
        test_command: Some(test_command.to_string()),
        ..Default::default()
    };
    let storage = StoragePaths::for_repo_in(data.path(), &repo.root).unwrap();
    let engine = Engine::open_with_storage(repo, config, storage).unwrap();
    (work, data, engine)
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?} failed");
}

fn write(root: &Path, rel: &str, contents: &str) {
    std::fs::write(root.join(rel), contents).unwrap();
}

#[test]
fn pass_edit_fail_investigate_restore_pass() {
    // A deterministic, cache-free test command: compare two files.
    let (work, _data, mut engine) = setup("diff answer.txt expected.txt");
    let root = engine.repo.root.clone();
    write(&root, "answer.txt", "EXPECTED\n");
    write(&root, "expected.txt", "EXPECTED\n");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-m", "init", "-q"]);

    // 1. Passing run.
    let r1 = engine.run_test(&CancelToken::new(), |_| {}).unwrap();
    assert!(r1.passed, "initial test should pass");
    assert!(r1.investigation.is_none(), "no investigation on a pass");
    let passing_post = engine.get_test_run_post(r1.test_run_id).unwrap().unwrap();

    // 2. Edit to introduce a failure.
    write(&root, "answer.txt", "WRONG\n");

    // 3. Failing run with automatic investigation.
    let r2 = engine.run_test(&CancelToken::new(), |_| {}).unwrap();
    assert!(!r2.passed, "test should now fail");
    let inv = r2.investigation.expect("failure should be investigated");
    assert_eq!(inv.passing_test_run_id, Some(r1.test_run_id));
    let top = &inv.causes[0];
    assert_eq!(
        top.path, "answer.txt",
        "top cause should be the edited file"
    );
    assert!(top.score >= 5, "changed + in-output should score highly");

    // 4. Restore the passing post-test snapshot.
    let plan = engine.plan_restore(passing_post, &Selection::All).unwrap();
    assert_eq!(plan.overwrite.len(), 1);
    let outcome = engine
        .execute_restore(passing_post, &Selection::All, &plan)
        .unwrap();
    assert_eq!(outcome.stats.written, 1);
    assert_eq!(
        std::fs::read_to_string(root.join("answer.txt")).unwrap(),
        "EXPECTED\n"
    );

    // 5. Passing again after the restore.
    let r3 = engine.run_test(&CancelToken::new(), |_| {}).unwrap();
    assert!(r3.passed, "test should pass after restore");

    // 6. Undo the restore, returning to the broken content.
    let stats = engine.undo_restore(outcome.restore_id).unwrap();
    assert_eq!(stats.written, 1);
    assert_eq!(
        std::fs::read_to_string(root.join("answer.txt")).unwrap(),
        "WRONG\n"
    );

    drop(work);
}

#[test]
fn snapshots_dedup_unchanged_content() {
    let (_work, _data, mut engine) = setup("true");
    let root = engine.repo.root.clone();
    write(&root, "a.txt", "shared\n");
    write(&root, "b.txt", "shared\n");
    let s1 = engine
        .snapshot(rewind::db::models::snapshot_kind::MANUAL, "one")
        .unwrap();
    // Same content again in a second snapshot should not create new objects.
    let objects_before = count_objects(&engine.storage.objects);
    write(&root, "c.txt", "shared\n");
    let _s2 = engine
        .snapshot(rewind::db::models::snapshot_kind::MANUAL, "two")
        .unwrap();
    let objects_after = count_objects(&engine.storage.objects);
    assert_eq!(
        objects_before, objects_after,
        "identical content must not add objects"
    );
    assert!(s1.file_count >= 2);
}

fn count_objects(dir: &Path) -> usize {
    let mut n = 0;
    if let Ok(shards) = std::fs::read_dir(dir) {
        for shard in shards.flatten() {
            if let Ok(files) = std::fs::read_dir(shard.path()) {
                n += files.flatten().count();
            }
        }
    }
    n
}

#[test]
fn restore_refuses_path_escapes() {
    // The public API only ever produces safe plans, but the validator itself is
    // the last line of defense against a tampered manifest.
    let root = Path::new("/repo");
    assert!(rewind::restore::validate_rel_path(root, "src/main.rs").is_ok());
    assert!(rewind::restore::validate_rel_path(root, "../../etc/passwd").is_err());
    assert!(rewind::restore::validate_rel_path(root, ".git/hooks/pre-commit").is_err());
}

#[test]
fn secrets_are_excluded_from_snapshots() {
    let (_work, _data, mut engine) = setup("true");
    let root = engine.repo.root.clone();
    write(&root, "src.rs", "fn main() {}\n");
    write(&root, ".env", "API_KEY=supersecret\n");
    std::fs::create_dir_all(root.join("certs")).unwrap();
    write(&root, "certs/server.pem", "-----BEGIN-----\n");

    let snap = engine
        .snapshot(rewind::db::models::snapshot_kind::MANUAL, "s")
        .unwrap();
    let manifest = rewind::snapshot::manifest_map(&engine.db, snap.id).unwrap();
    assert!(manifest.contains_key("src.rs"));
    assert!(
        !manifest.contains_key(".env"),
        "secret .env must be excluded"
    );
    assert!(
        !manifest.contains_key("certs/server.pem"),
        "*.pem must be excluded"
    );
}
