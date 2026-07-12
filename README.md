# Rewind

**Time-travel debugging for any local Git repository — in your terminal.**

Rewind is a single-binary terminal app. Run `rewind` inside a Git repo and you
get a full-screen interface with a real embedded shell, a timeline of everything
that happened, content-addressed snapshots, test history, snapshot diffs, and
deterministic failure investigation. When a test that used to pass starts
failing, Rewind compares the last-good snapshot with the failing one, ranks the
likely culprits, and lets you safely restore.

No Electron, no web server, no cloud, no accounts, no AI. Your project data never
leaves your machine.

```
┌ Rewind ─ repository ─ recording ─ last test: failed ┐
│ Timeline │ Embedded Shell              │ Investigation │
│          │                             │               │
├──────────┴─────────────────────────────┴───────────────┤
│ Snapshot / test status / selected event                │
└────────────────────────────────────────────────────────┘
```

---

## What it's for

- **Find what broke a test.** Rewind snapshots your files before and after every
  test run. When a pass turns into a failure, it diffs the two states and ranks
  the changed files by a transparent relevance score.
- **Undo confidently.** Every restore shows a dry-run, creates a safety snapshot
  first, and can be reverted with `rewind undo`. Git is never touched.
- **Keep a local history of your session** — commands, checkpoints, and test
  results — without committing anything or sending data anywhere.
- **Run tests in CI** with `rewind ci`, which writes a report (result, changed
  files, dependency changes, likely causes) as an artifact.

---

## Install

Requires nothing at runtime; building needs Rust ≥ 1.82.

**From source**

```bash
git clone https://github.com/rewind-dev/rewind
cd rewind
cargo install --path .
```

**From crates.io** (once published)

```bash
cargo install rewind
```

**Prebuilt binaries** — download the archive for your platform from
[Releases](https://github.com/rewind-dev/rewind/releases) (macOS and Linux,
x86-64 and ARM64; each archive includes the binary, a man page, and shell
completions), then:

```bash
tar xzf rewind-v0.1.0-aarch64-apple-darwin.tar.gz
sudo install rewind-*/rewind /usr/local/bin/
```

**Homebrew** — a formula template is in
[`packaging/homebrew/rewind.rb`](packaging/homebrew/rewind.rb).

**Shell completions & man page**

```bash
rewind completions zsh  > ~/.zsh/completions/_rewind   # bash | zsh | fish | powershell | elvish
rewind man > /usr/local/share/man/man1/rewind.1
```

---

## Quick start

```bash
cd my-project
rewind init --test-command "npm test"   # autodetects cargo/pytest/go/etc. if omitted
rewind test                             # snapshots before & after; records pass/fail
# ...you edit code and it breaks...
rewind test                             # fails → prints ranked likely causes
rewind restore 4                        # dry-run, confirm, restore the good snapshot
rewind test                             # green again
```

Just run `rewind` with no arguments to open the interactive interface.

---

## Commands

| Command | What it does |
|---|---|
| `rewind` | Open the interactive terminal interface |
| `rewind init [--test-command CMD] [--git]` | Create/update `.rewind.toml` (autodetects a test command; `--git` inits a repo) |
| `rewind run -- <command>` | Run a command with before/after snapshots, streaming output |
| `rewind test [--command CMD]` | Run the configured test command with snapshots |
| `rewind checkpoint [name]` | Create a named checkpoint snapshot |
| `rewind diff [snap-a] [snap-b]` | Show changes between snapshots (or a snapshot vs. the working tree) |
| `rewind restore [snapshot] [--file PATH]... [-y]` | Restore files from a snapshot (dry-run + confirm) |
| `rewind undo` | Undo the most recent restore |
| `rewind doctor` | Print environment, storage, and configuration diagnostics |
| `rewind ci -- <test-command>` | Non-interactive run that writes `.rewind-report/` |
| `rewind completions <shell>` | Generate shell completions |
| `rewind man` | Generate the man page |

`run`, `test`, and `ci` set their exit code from the command's exit code, so they
compose with other tooling.

---

## Keyboard controls

The embedded shell is the primary view — a real PTY running your `$SHELL`.
`Ctrl+G` toggles between **Shell** mode (keystrokes go to the shell, so your
normal shortcuts keep working) and **Navigation** mode.

| Key | Action |
|---|---|
| `Ctrl+G` | Toggle Shell / Navigation modes |
| `j` / `k` (or `↓` / `↑`) | Move selection (or scroll the detail panel) |
| `g` / `G` | Jump to first / last event |
| `Tab` | Switch focus between timeline and detail |
| `Enter` | Open the selected event's detail |
| `t` | Run the configured test command |
| `c` | Create a checkpoint |
| `r` | Restore the selected snapshot (with confirmation) |
| `u` | Undo the last restore |
| `/` | Search the timeline |
| `?` | Toggle the help overlay |
| `q` | Quit |

---

## Configuration — `.rewind.toml` (optional, committed)

```toml
test_command = "npm test"              # used by `rewind test`
max_file_size = 1048576                # bytes; larger files aren't snapshotted
test_timeout_secs = 0                  # 0 = no timeout
track_secrets = false                  # opt in to snapshotting secret-looking files (warned)
ignore = ["coverage/", "generated/"]   # extra ignore patterns (gitignore syntax)
```

`rewind init` writes one for you, autodetecting a test command from common
project files.

---

## Privacy & safety

- **Local only.** Snapshots and metadata live under your OS application-data
  directory (`<app-data>/rewind/<repo-hash>/`), never inside your repo, never
  uploaded. On Unix the storage dir is created `0700`.
- **Secrets excluded by default:** `.env`, `.env.*`, `*.pem`, `*.key`, and common
  credential files are never snapshotted unless you set `track_secrets = true`.
- **Git is read-only.** Rewind never modifies your commits, branches, index, or
  working tree — except when *you* perform a restore.
- **Restores are safe:** safety snapshot first, dry-run preview, explicit
  confirmation, atomic writes confined to the repo root, never touching `.git`,
  and `rewind undo` to revert.

Rewind only records commands run **inside its embedded shell** (bash/zsh) or via
`rewind run` / `rewind test` / `rewind ci`. It does not watch other terminals,
editors, or the OS. File *changes* by any tool are still detected and reconciled.

---

## Failure investigation

When a passing run is followed by a failing one, changed files are ranked by a
**relevance score** — a transparent, deterministic number, not a confidence or
probability. Results are always **likely causes**, never confirmed diagnoses.

| Signal | Δ |
|---|:--:|
| Appears in a stack-trace line | +5 |
| Dependency version changed | +4 |
| Changed between pass and failure | +3 |
| Path appears in the failing output | +3 |
| Changed immediately before the failure | +2 |
| Generated or unrelated file | −3 |

---

## Development

```bash
cargo build
cargo test            # unit + integration tests over temporary Git repos
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

Architecture (library + thin binary, so tests exercise the same code paths as
the CLI and TUI): `paths` · `config` · `repo` · `db` · `objects` · `tracking` ·
`snapshot` · `diff` · `exec` · `investigate` · `restore` · `session` (the
`Engine`) · `ci` · `cli` · `tui`. Built with `ratatui`/`crossterm`,
`portable-pty`/`vt100`, `notify`, `rusqlite` (bundled SQLite), and `blake3`.

CI runs on macOS and Linux; tagging `vX.Y.Z` builds release binaries with
SHA-256 checksums for all four targets. See [CHANGELOG.md](CHANGELOG.md).

---

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your
option.
