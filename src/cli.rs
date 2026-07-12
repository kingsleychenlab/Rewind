//! Command-line interface and dispatch.
//!
//! `rewind` with no subcommand launches the interactive TUI. The subcommands
//! (`init`, `run`, `test`, `checkpoint`, `diff`, `restore`, `undo`, `doctor`,
//! `ci`, `completions`, `man`) drive the same [`Engine`] operations
//! non-interactively for scripts and CI.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, CommandFactory, Parser, Subcommand};

use crate::config::Config;
use crate::error::RewindError;
use crate::exec::{CancelToken, OutputChunk};
use crate::repo::Repo;
use crate::restore::Selection;
use crate::session::Engine;

/// Time-travel debugging for any local Git repository.
#[derive(Debug, Parser)]
#[command(
    name = "rewind",
    version,
    about = "Time-travel debugging for any local Git repository",
    long_about = "Rewind records snapshots, test history, and command output for a Git \
repository, and gives you an interactive terminal to browse the timeline, diff \
snapshots, investigate failures, and safely restore files.\n\nRun `rewind` with no \
arguments inside a repository to open the interactive interface."
)]
pub struct Cli {
    /// Operate as if started in DIR instead of the current directory.
    #[arg(short = 'C', long = "dir", global = true, value_name = "DIR")]
    dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Create or update `.rewind.toml` (and a Git repository if needed).
    Init(InitArgs),
    /// Run a command with before/after snapshots, streaming its output.
    Run(RunArgs),
    /// Run the configured test command with before/after snapshots.
    Test(TestArgs),
    /// Create a named checkpoint snapshot.
    Checkpoint(CheckpointArgs),
    /// Show the changes between two snapshots (or a snapshot and the working tree).
    Diff(DiffArgs),
    /// Restore files from a snapshot (dry-run + confirmation first).
    Restore(RestoreArgs),
    /// Undo the most recent restore using its safety snapshot.
    Undo,
    /// Print environment, storage, and configuration diagnostics.
    Doctor,
    /// Run a test command with no TUI and write `.rewind-report/`.
    Ci(CiArgs),
    /// Generate shell completions.
    Completions(CompletionsArgs),
    /// Generate the man page to stdout.
    Man,
}

#[derive(Debug, Args)]
struct InitArgs {
    /// Test command to record (autodetected when omitted).
    #[arg(long, value_name = "COMMAND")]
    test_command: Option<String>,
    /// Initialize a Git repository if the directory is not one already.
    #[arg(long)]
    git: bool,
}

#[derive(Debug, Args)]
struct RunArgs {
    /// The command to run, after `--`.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "COMMAND"
    )]
    command: Vec<String>,
}

#[derive(Debug, Args)]
struct TestArgs {
    /// Override the configured test command for this run.
    #[arg(long, value_name = "COMMAND")]
    command: Option<String>,
}

#[derive(Debug, Args)]
struct CheckpointArgs {
    /// Optional checkpoint name.
    name: Option<String>,
}

#[derive(Debug, Args)]
struct DiffArgs {
    /// First snapshot id (older side).
    snapshot_a: Option<i64>,
    /// Second snapshot id (newer side).
    snapshot_b: Option<i64>,
    /// Show the full unified patch, not just the file list.
    #[arg(long)]
    patch: bool,
}

#[derive(Debug, Args)]
struct RestoreArgs {
    /// Snapshot id to restore from.
    snapshot: Option<i64>,
    /// Restore only these paths (relative to the repository root).
    #[arg(long = "file", value_name = "PATH")]
    files: Vec<String>,
    /// Apply without the interactive confirmation prompt.
    #[arg(short = 'y', long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct CiArgs {
    /// Directory for the report (default: `.rewind-report`).
    #[arg(long, value_name = "DIR")]
    out: Option<PathBuf>,
    /// The test command to run, after `--`.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "COMMAND"
    )]
    command: Vec<String>,
}

#[derive(Debug, Args)]
struct CompletionsArgs {
    /// Shell to generate completions for.
    #[arg(value_enum)]
    shell: clap_complete::Shell,
}

/// Parse arguments and dispatch. Returns a process exit code.
pub fn run() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("rewind: {e}");
            ExitCode::from(1)
        }
    }
}

fn dispatch(cli: Cli) -> anyhow::Result<ExitCode> {
    let start_dir = match &cli.dir {
        Some(d) => d.clone(),
        None => std::env::current_dir()?,
    };

    match cli.command {
        None => cmd_tui(&start_dir),
        Some(Commands::Init(a)) => cmd_init(&start_dir, a),
        Some(Commands::Run(a)) => cmd_run(&start_dir, a),
        Some(Commands::Test(a)) => cmd_test(&start_dir, a),
        Some(Commands::Checkpoint(a)) => cmd_checkpoint(&start_dir, a),
        Some(Commands::Diff(a)) => cmd_diff(&start_dir, a),
        Some(Commands::Restore(a)) => cmd_restore(&start_dir, a),
        Some(Commands::Undo) => cmd_undo(&start_dir),
        Some(Commands::Doctor) => cmd_doctor(&start_dir),
        Some(Commands::Ci(a)) => cmd_ci(&start_dir, a),
        Some(Commands::Completions(a)) => cmd_completions(a),
        Some(Commands::Man) => cmd_man(),
    }
}

/// Discover the repository, giving a friendly message when there isn't one.
fn open_engine(start_dir: &std::path::Path) -> anyhow::Result<Engine> {
    let repo = match Repo::discover(start_dir) {
        Ok(r) => r,
        Err(RewindError::NotAGitRepo) => {
            anyhow::bail!(
                "not inside a Git repository.\n  Run `rewind init --git` here to create one, \
                 or `cd` into an existing repository."
            );
        }
        Err(e) => return Err(e.into()),
    };
    Ok(Engine::open(repo)?)
}

fn cmd_tui(start_dir: &std::path::Path) -> anyhow::Result<ExitCode> {
    let engine = open_engine(start_dir)?;
    crate::tui::run(engine)?;
    Ok(ExitCode::SUCCESS)
}

fn cmd_init(start_dir: &std::path::Path, args: InitArgs) -> anyhow::Result<ExitCode> {
    let repo = match Repo::discover(start_dir) {
        Ok(r) => r,
        Err(RewindError::NotAGitRepo) => {
            if args.git {
                println!(
                    "Initializing a new Git repository in {}",
                    start_dir.display()
                );
                Repo::init(start_dir)?
            } else {
                anyhow::bail!(
                    "not a Git repository. Re-run with `rewind init --git` to create one."
                );
            }
        }
        Err(e) => return Err(e.into()),
    };

    let mut config = Config::load(&repo.root).unwrap_or_default();
    let existed = Config::exists(&repo.root);
    let detected = args
        .test_command
        .or_else(|| config.test_command.clone())
        .or_else(|| detect_test_command(&repo.root));
    config.test_command = detected;
    config.save(&repo.root)?;

    // Touch the engine so storage is initialized and the repo is recorded.
    let _engine = Engine::open_with_config(repo.clone(), config.clone())?;

    println!(
        "{} {}",
        if existed { "Updated" } else { "Created" },
        repo.root.join(".rewind.toml").display()
    );
    match &config.test_command {
        Some(cmd) => println!("  test_command = \"{cmd}\""),
        None => {
            println!("  no test command set — edit .rewind.toml to add `test_command = \"...\"`")
        }
    }
    println!("Run `rewind` to open the interactive interface.");
    Ok(ExitCode::SUCCESS)
}

fn cmd_run(start_dir: &std::path::Path, args: RunArgs) -> anyhow::Result<ExitCode> {
    if args.command.is_empty() {
        anyhow::bail!("no command given. Usage: rewind run -- <command>");
    }
    let command = args.command.join(" ");
    let mut engine = open_engine(start_dir)?;
    let cancel = install_cancel_handler();
    let mut stdout = std::io::stdout();
    let result = engine.run_command(&command, &cancel, |chunk: &OutputChunk| {
        let _ = stdout.write_all(&chunk.data);
        let _ = stdout.flush();
    })?;
    print_outcome_footer(&command, &result.outcome);
    Ok(exit_from_code(
        result.outcome.exit_code,
        result.outcome.passed(),
    ))
}

fn cmd_test(start_dir: &std::path::Path, args: TestArgs) -> anyhow::Result<ExitCode> {
    let mut engine = open_engine(start_dir)?;
    if let Some(cmd) = args.command {
        engine.config.test_command = Some(cmd);
    }
    if engine.test_command().is_none() {
        anyhow::bail!(
            "no test command configured. Set it with `rewind init --test-command \"...\"` \
             or in .rewind.toml, or pass `--command`."
        );
    }
    let cancel = install_cancel_handler();
    let mut stdout = std::io::stdout();
    let result = engine.run_test(&cancel, |chunk: &OutputChunk| {
        let _ = stdout.write_all(&chunk.data);
        let _ = stdout.flush();
    })?;
    println!();
    print_outcome_footer(engine.test_command().unwrap_or("test"), &result.outcome);
    if let Some(inv) = &result.investigation {
        println!("\nLikely causes (relevance score — likely, not confirmed):");
        print_causes(inv);
    }
    Ok(exit_from_code(result.outcome.exit_code, result.passed))
}

fn cmd_checkpoint(start_dir: &std::path::Path, args: CheckpointArgs) -> anyhow::Result<ExitCode> {
    let mut engine = open_engine(start_dir)?;
    let snap = engine.checkpoint(args.name.as_deref())?;
    println!(
        "Created checkpoint #{} ({} files, {})",
        snap.id,
        snap.file_count,
        crate::util::format_timestamp(snap.created_at)
    );
    Ok(ExitCode::SUCCESS)
}

fn cmd_diff(start_dir: &std::path::Path, args: DiffArgs) -> anyhow::Result<ExitCode> {
    let mut engine = open_engine(start_dir)?;
    let snaps = engine.list_snapshots()?;
    if snaps.is_empty() {
        println!("No snapshots yet. Run `rewind checkpoint` or `rewind test` first.");
        return Ok(ExitCode::SUCCESS);
    }

    // Resolve the two sides.
    let (old_id, new_id) = match (args.snapshot_a, args.snapshot_b) {
        (Some(a), Some(b)) => (a, b),
        (Some(a), None) => {
            // Compare snapshot a to a fresh snapshot of the working tree.
            let head = engine.snapshot(
                crate::db::models::snapshot_kind::MANUAL,
                "diff working tree",
            )?;
            (a, head.id)
        }
        (None, _) => {
            // Last two snapshots.
            if snaps.len() < 2 {
                println!("Only one snapshot exists; nothing to compare.");
                return Ok(ExitCode::SUCCESS);
            }
            (snaps[1].id, snaps[0].id)
        }
    };

    let changes = crate::diff::diff_snapshots(&engine.db, old_id, new_id)?;
    println!(
        "Diff snapshot #{old_id} → #{new_id}: {} change(s)",
        changes.len()
    );
    for c in &changes {
        let stat =
            crate::diff::line_stat(&engine.store, c.old_hash.as_deref(), c.new_hash.as_deref())
                .unwrap_or_default();
        println!(
            "  {} {}  (+{} -{})",
            c.letter(),
            c.path,
            stat.added,
            stat.removed
        );
        if args.patch {
            if let Some(patch) = crate::diff::unified_diff(
                &engine.store,
                &c.path,
                c.old_hash.as_deref(),
                c.new_hash.as_deref(),
            ) {
                for line in patch.lines() {
                    println!("    {line}");
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_restore(start_dir: &std::path::Path, args: RestoreArgs) -> anyhow::Result<ExitCode> {
    let mut engine = open_engine(start_dir)?;
    let snapshot_id = match args.snapshot {
        Some(id) => id,
        None => {
            print_recent_snapshots(&engine)?;
            anyhow::bail!("specify a snapshot id to restore, e.g. `rewind restore 12`");
        }
    };
    if engine.get_snapshot(snapshot_id)?.is_none() {
        anyhow::bail!("no snapshot #{snapshot_id}");
    }

    let selection = Selection::from_paths(args.files.clone());
    let plan = engine.plan_restore(snapshot_id, &selection)?;

    if plan.is_empty() {
        println!("Nothing to restore — the working tree already matches snapshot #{snapshot_id}.");
        return Ok(ExitCode::SUCCESS);
    }

    // Dry-run display.
    println!("Restore plan (snapshot #{snapshot_id}):");
    for e in &plan.create {
        println!("  create    {}", e.path);
    }
    for e in &plan.overwrite {
        println!("  overwrite {}", e.path);
    }
    for p in &plan.delete {
        println!("  delete    {p}");
    }
    println!(
        "  {} file(s) to write, {} to delete",
        plan.create.len() + plan.overwrite.len(),
        plan.delete.len()
    );

    if !args.yes {
        if !std::io::stdin().is_terminal() {
            anyhow::bail!("refusing to restore without confirmation; re-run with `--yes`");
        }
        if !confirm("Proceed with restore? A safety snapshot will be created first. [y/N] ")? {
            println!("Aborted.");
            return Ok(ExitCode::SUCCESS);
        }
    }

    let outcome = engine.execute_restore(snapshot_id, &selection, &plan)?;
    println!(
        "Restored: {} written, {} deleted. Safety snapshot #{}. Undo with `rewind undo`.",
        outcome.stats.written, outcome.stats.deleted, outcome.safety_snapshot_id
    );
    Ok(ExitCode::SUCCESS)
}

fn cmd_undo(start_dir: &std::path::Path) -> anyhow::Result<ExitCode> {
    let mut engine = open_engine(start_dir)?;
    let last = crate::db::models::last_undoable_restore(engine.db.conn(), engine.repo_id)?;
    let Some(record) = last else {
        println!("No restore to undo.");
        return Ok(ExitCode::SUCCESS);
    };
    let stats = engine.undo_restore(record.id)?;
    println!(
        "Undid restore #{}: {} written, {} deleted (from safety snapshot #{}).",
        record.id, stats.written, stats.deleted, record.safety_snapshot_id
    );
    Ok(ExitCode::SUCCESS)
}

fn cmd_doctor(start_dir: &std::path::Path) -> anyhow::Result<ExitCode> {
    println!("Rewind v{}", crate::VERSION);
    match Repo::discover(start_dir) {
        Ok(repo) => {
            println!("  repository:   {}", repo.root.display());
            let state = repo.state();
            println!(
                "  git:          branch {}, head {}{}",
                state.branch.as_deref().unwrap_or("(none)"),
                state
                    .head
                    .as_deref()
                    .map(|h| &h[..h.len().min(8)])
                    .unwrap_or("(none)"),
                if state.dirty { ", dirty" } else { "" }
            );
            let engine = Engine::open(repo)?;
            let sp = &engine.storage;
            println!("  storage:      {}", sp.root.display());
            println!("  database:     {}", sp.db.display());
            println!("  objects:      {}", sp.objects.display());
            println!("  repo hash:    {}", sp.repo_hash);
            println!(
                "  config:       {}",
                if Config::exists(&engine.repo.root) {
                    ".rewind.toml present"
                } else {
                    "using defaults (no .rewind.toml)"
                }
            );
            println!(
                "  test command: {}",
                engine.test_command().unwrap_or("(none configured)")
            );
            let snaps = engine.list_snapshots()?;
            let tests = engine.list_test_runs(1)?;
            println!("  snapshots:    {}", snaps.len());
            println!(
                "  last test:    {}",
                match tests.first() {
                    Some(t) => match t.passed {
                        Some(true) => "passed",
                        Some(false) => "failed",
                        None => "incomplete",
                    },
                    None => "none yet",
                }
            );
        }
        Err(RewindError::NotAGitRepo) => {
            println!(
                "  repository:   NOT a Git repository ({})",
                start_dir.display()
            );
            println!("  hint:         run `rewind init --git` to create one");
        }
        Err(e) => println!("  repository:   error: {e}"),
    }
    println!("  shell:        {}", crate::exec::resolve_shell());
    println!(
        "  data dir env: {}",
        std::env::var(crate::paths::DATA_DIR_ENV).unwrap_or_else(|_| "(unset)".into())
    );
    println!("  secrets excluded by default: .env, .env.*, *.pem, *.key, credentials, …");
    Ok(ExitCode::SUCCESS)
}

fn cmd_ci(start_dir: &std::path::Path, args: CiArgs) -> anyhow::Result<ExitCode> {
    let mut engine = open_engine(start_dir)?;
    let override_cmd = if args.command.is_empty() {
        None
    } else {
        Some(args.command.join(" "))
    };
    if override_cmd.is_none() && engine.test_command().is_none() {
        anyhow::bail!("no test command. Usage: rewind ci -- <test-command>");
    }
    let out_dir = crate::ci::resolve_out_dir(&engine.repo.root, args.out.as_deref());
    let cancel = install_cancel_handler();
    let report = crate::ci::run(
        &mut engine,
        override_cmd.as_deref(),
        &out_dir,
        &cancel,
        true,
    )?;
    println!("\n─────────────────────────────────────────────");
    println!("{}", report.summary);
    println!("Report written to {}", out_dir.display());
    Ok(ExitCode::from(
        report.exit_code_for_process().clamp(0, 255) as u8
    ))
}

fn cmd_completions(args: CompletionsArgs) -> anyhow::Result<ExitCode> {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    clap_complete::generate(args.shell, &mut cmd, name, &mut std::io::stdout());
    Ok(ExitCode::SUCCESS)
}

fn cmd_man() -> anyhow::Result<ExitCode> {
    let cmd = Cli::command();
    let man = clap_mangen::Man::new(cmd);
    man.render(&mut std::io::stdout())?;
    Ok(ExitCode::SUCCESS)
}

// -- helpers ---------------------------------------------------------------

/// A cancellation token for a non-interactive command.
///
/// The interactive TUI drives cancellation from its own key handling. For the
/// non-interactive subcommands there is no separate UI thread to watch for
/// input, so Ctrl-C is left to terminate the process via the default SIGINT
/// disposition; the child shares the process group and is signalled too. The
/// token is still threaded through so the same `Engine` API is used everywhere.
fn install_cancel_handler() -> CancelToken {
    CancelToken::new()
}

fn confirm(prompt: &str) -> anyhow::Result<bool> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let a = line.trim().to_ascii_lowercase();
    Ok(a == "y" || a == "yes")
}

fn print_outcome_footer(command: &str, outcome: &crate::exec::CommandOutcome) {
    let status = if outcome.cancelled {
        "cancelled".to_string()
    } else if outcome.timed_out {
        "timed out".to_string()
    } else if outcome.passed() {
        "ok".to_string()
    } else {
        match outcome.exit_code {
            Some(c) => format!("exit {c}"),
            None => "killed".to_string(),
        }
    };
    eprintln!(
        "[rewind] `{command}` {status} in {}",
        crate::util::format_duration_ms(outcome.duration_ms)
    );
}

fn print_causes(inv: &crate::investigate::Investigation) {
    if inv.causes.is_empty() {
        println!("  (no changed files to rank)");
        return;
    }
    for (i, c) in inv.causes.iter().take(8).enumerate() {
        println!("  {}. {} — relevance {}", i + 1, c.path, c.score);
        for ev in &c.evidence {
            println!("       - {ev}");
        }
    }
}

fn print_recent_snapshots(engine: &Engine) -> anyhow::Result<()> {
    let snaps = engine.list_snapshots()?;
    println!("Recent snapshots:");
    for s in snaps.iter().take(15) {
        println!(
            "  #{:<4} {:<14} {}  ({} files)",
            s.id,
            s.kind,
            crate::util::format_timestamp(s.created_at),
            s.file_count
        );
    }
    Ok(())
}

fn detect_test_command(root: &std::path::Path) -> Option<String> {
    let has = |f: &str| root.join(f).exists();
    if has("Cargo.toml") {
        Some("cargo test".into())
    } else if has("package.json") {
        Some("npm test".into())
    } else if has("pyproject.toml") || has("pytest.ini") || has("tox.ini") {
        Some("pytest".into())
    } else if has("go.mod") {
        Some("go test ./...".into())
    } else if has("Gemfile") {
        Some("bundle exec rake test".into())
    } else if has("Makefile") {
        Some("make test".into())
    } else {
        None
    }
}

fn exit_from_code(code: Option<i32>, passed: bool) -> ExitCode {
    if passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(code.unwrap_or(1).clamp(0, 255) as u8)
    }
}
