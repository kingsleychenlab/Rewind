# Rewind

**Time-travel debugging for any local Git repository — in your terminal.**

Rewind is an installable terminal application. Inside any Git repository, run
`rewind` and you get a full-screen interface with an embedded interactive shell,
a project timeline, content-addressed snapshots, test history, snapshot diffs,
and deterministic failure investigation. When a test that used to pass starts
failing, Rewind compares the last-good snapshot with the failing one and ranks
the likely culprits — then lets you safely restore.

It is a single self-contained binary. No Electron, no web server, no cloud, no
accounts, no AI. Your project data never leaves your machine.

```
┌ Rewind ─ repository ─ recording ─ last test: failed ┐
│ Timeline │ Embedded Shell              │ Investigation │
│          │                             │               │
│          │                             │               │
├──────────┴─────────────────────────────┴───────────────┤
│ Snapshot / test status / selected event                │
└────────────────────────────────────────────────────────┘
```

---

## Install

### From source (any platform with Rust ≥ 1.82)

```bash
cargo install --path .        # from a clone
# or, once published:
cargo install rewind
```

### Prebuilt binaries

Each [GitHub Release](https://github.com/rewind-dev/rewind/releases) attaches
`tar.gz` archives with SHA-256 checksums for:

| OS    | x86-64                        | ARM64                          |
|-------|-------------------------------|--------------------------------|
| Linux | `x86_64-unknown-linux-gnu`    | `aarch64-unknown-linux-gnu`    |
| macOS | `x86_64-apple-darwin`         | `aarch64-apple-darwin`         |

Each archive contains the `rewind` binary, a man page (`rewind.1`), and shell
completions.

```bash
tar xzf rewind-v0.1.0-aarch64-apple-darwin.tar.gz
sudo install rewind-v0.1.0-aarch64-apple-darwin/rewind /usr/local/bin/
```

### Homebrew

A formula template lives at [`packaging/homebrew/rewind.rb`](packaging/homebrew/rewind.rb)
for use in a tap.

---

## Usage

```bash
cd any-git-repository
rewind                    # open the interactive interface
```

Non-interactive commands (safe for scripts and CI):

```bash
rewind init                     # create/update .rewind.toml (autodetects a test command)
rewind run -- <command>         # run a command with before/after snapshots
rewind test                     # run the configured test command with snapshots
rewind checkpoint [name]        # create a named checkpoint snapshot
rewind diff [snap-a] [snap-b]   # show changes between snapshots (or vs. the working tree)
rewind restore [snapshot]       # restore files from a snapshot (dry-run + confirm)
rewind undo                     # undo the most recent restore
rewind doctor                   # environment, storage, and configuration diagnostics
rewind ci -- <test-command>     # non-interactive run that writes .rewind-report/
rewind completions <shell>      # generate shell completions
rewind man                      # generate the man page
```

`rewind run`, `rewind test`, and `rewind ci` set their process exit code from
the command's exit code, so they compose with other tooling.

### Quick start

```bash
cd my-project
rewind init --test-command "npm test"   # or cargo test, pytest, go test ./..., …
rewind test                              # records a snapshot before and after
# ...edit code, it breaks...
rewind test                              # fails — Rewind prints ranked likely causes
rewind restore 4                         # dry-run, confirm, then restore the good snapshot
rewind test                              # green again
```

---

## Keyboard controls (interactive interface)

`Ctrl+G` toggles between **Shell** mode (keystrokes go to the embedded shell, so
your normal shell shortcuts keep working) and **Navigation** mode.

| Key            | Action                                             |
|----------------|----------------------------------------------------|
| `Ctrl+G`       | Toggle Shell / Navigation modes                    |
| `j` / `↓`      | Move selection down (or scroll detail)             |
| `k` / `↑`      | Move selection up (or scroll detail)               |
| `g` / `G`      | Jump to first / last event                         |
| `Tab`          | Switch focus between timeline and detail           |
| `Enter`        | Open the selected event's detail                   |
| `t`            | Run the configured test command                    |
| `c`            | Create a checkpoint snapshot                        |
| `r`            | Restore the selected snapshot (with confirmation)  |
| `u`            | Undo the last restore                              |
| `/`            | Search the timeline                                |
| `?`            | Toggle the help overlay                            |
| `q`            | Quit                                               |

The embedded shell is the primary view: it is a real PTY running your `$SHELL`
in the repository root, rendered faithfully (colors, cursor, resize).

---

## Configuration — `.rewind.toml`

Optional, committed to the repository:

```toml
test_command = "npm test"       # used by `rewind test`
max_file_size = 1048576         # bytes; larger files are not snapshotted
test_timeout_secs = 0           # 0 = no timeout
track_secrets = false           # opt in to tracking secret-looking files (warned)
ignore = ["coverage/", "generated/"]   # extra ignore patterns (gitignore syntax)
```

Run `rewind init` to create one; it autodetects a test command from common
project files (`Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`, …).

---

## What Rewind captures (and what it does not)

Rewind **only** records commands executed:

- inside Rewind's embedded shell (via shell integration for bash/zsh),
- through `rewind run`,
- through `rewind test`,
- through `rewind ci`.

It does **not** capture activity from other terminal windows, editors, or the
operating system at large. File *changes* made by any tool are still detected by
the filesystem watcher and reconciled at startup, but Rewind never claims to
know a command it did not run.

---

## Privacy & safety

- **Local only.** Snapshots and metadata are stored under your OS application-data
  directory, keyed by a stable hash of the repository's canonical path:

  ```
  <app-data>/rewind/<repository-hash>/
    rewind.db        # SQLite metadata (migrated in place)
    objects/         # content-addressed file versions (BLAKE3, deduplicated)
    logs/            # structured logs and per-session command logs
  ```

  Nothing is ever uploaded. Snapshot data is **never** written inside your
  repository. On Unix the storage directory is created with `0700` permissions.

- **Secrets excluded by default.** `.env`, `.env.*`, `*.pem`, `*.key`, key files,
  and common credential files are never snapshotted unless you explicitly set
  `track_secrets = true` (which prints a warning).

- **Git is read-only.** Rewind reads Git state with standard commands and never
  modifies your commits, branches, index, or working tree — except when *you*
  perform a restore.

- **Restores are safe.** Every restore first creates a safety snapshot, shows a
  dry-run of exactly which files will be created, overwritten, and deleted, and
  requires confirmation. Writes are atomic (temp file + rename), strictly
  confined to the repository root, and never touch `.git`. `rewind undo` reverts
  the last restore from its safety snapshot. Dependencies are never installed
  automatically.

---

## Failure investigation

When a passing test run is followed by a failing one, Rewind compares the last
passing post-test snapshot with the failing post-test snapshot and ranks changed
files by a **relevance score** — a transparent, deterministic number, *not* a
confidence or probability. Results are always presented as **likely causes**,
never confirmed diagnoses.

| Signal                                   | Δ score |
|------------------------------------------|:-------:|
| Appears in a stack-trace line            |   +5    |
| Dependency version changed               |   +4    |
| Changed between pass and failure         |   +3    |
| Path appears in the failing output       |   +3    |
| Changed immediately before the failure   |   +2    |
| Generated or unrelated file              |   −3    |

For each likely cause Rewind shows the file path, score, the evidence behind
each signal, the changed lines, the related command, relevant lines of failing
output, and any dependency version changes.

---

## Continuous integration

`rewind ci -- <test-command>` runs your tests with no TUI and writes:

```
.rewind-report/
  report.json     # structured: result, command, duration, changed files, deps, likely causes
  summary.md      # human-readable summary
  test.log        # full combined output
  changes.patch   # unified diff of the relevant changes
```

It exits non-zero when the tests fail. A drop-in GitHub Actions template is in
[`docs/rewind-ci.yml`](docs/rewind-ci.yml); it uploads `.rewind-report/` as a
workflow artifact and requires no credentials.

---

## Architecture

Rewind is a Rust library (`src/lib.rs`) with a thin binary front-end
(`src/main.rs`), so tests exercise the same code paths as the CLI and TUI.

| Module        | Responsibility |
|---------------|----------------|
| `paths`       | Storage layout, repo hashing, restrictive permissions |
| `config`      | `.rewind.toml`, default ignores, secret patterns |
| `repo`        | Git repository detection and read-only state capture |
| `db`          | SQLite schema, migrations (`user_version`), typed models |
| `objects`     | Content-addressed store (BLAKE3, atomic writes, dedup) |
| `tracking`    | Ignore rules, reconciliation scan, debounced watcher |
| `snapshot`    | Manifest creation and content dedup |
| `diff`        | Manifest diffs (add/modify/delete/rename) and text diffs |
| `exec`        | Command execution: streaming, capture, timeout, cancel |
| `investigate` | Deterministic relevance scoring |
| `restore`     | Dry-run planning, path validation, atomic apply, undo |
| `session`     | The `Engine`: high-level operations shared by CLI and TUI |
| `ci`          | Non-interactive report generation |
| `cli`         | `clap` command definitions and dispatch |
| `tui`         | Embedded PTY shell, vt100 rendering, ratatui interface |

The interface is built with `ratatui` + `crossterm`; the embedded shell uses
`portable-pty` with `vt100` parsing; the filesystem watcher uses `notify`;
storage uses `rusqlite` (bundled SQLite) and `blake3`; ignore handling uses
`ignore`. The terminal is restored correctly on normal exit, errors, signals,
and panics.

---

## Limitations

- Embedded-shell command capture uses shell hooks for **bash and zsh**. Other
  shells still work as shells; Rewind captures file *changes* via the watcher
  but not the command text.
- Very large files (over `max_file_size`) and binary files are not snapshotted.
- Rewind reasons about tracked files only — not database state or external
  services.
- The `t` shortcut runs the test synchronously; the interface pauses redraws
  while it runs (the shell keeps running in the background). Long test suites are
  best run in the embedded shell.

---

## Development

```bash
cargo build
cargo test            # unit + integration tests (uses temporary Git repos)
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

CI runs on macOS and Linux (`.github/workflows/ci.yml`). Tagging `vX.Y.Z` builds
release binaries and checksums for all four targets and attaches them to the
GitHub Release (`.github/workflows/release.yml`).

### Cutting a release

1. Bump `version` in `Cargo.toml` (and the Homebrew formula template).
2. Update the changelog.
3. `git tag vX.Y.Z && git push origin vX.Y.Z`.
4. The release workflow builds, tests, packages, checksums, and uploads.
5. Fill the SHA-256 sums into the Homebrew formula and push to the tap.

---

## License

Dual-licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE)
at your option.
