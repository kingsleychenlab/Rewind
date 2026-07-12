# Changelog

All notable changes to Rewind are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-07-12

Initial release.

### Added

- **Interactive TUI** (`rewind`): full-screen interface with an embedded PTY
  shell (`portable-pty` + `vt100`), a project timeline, a snapshot/diff detail
  panel, deterministic failure investigation, event search, and a help overlay.
  `Ctrl+G` toggles between Shell and Navigation modes; the terminal is restored
  on exit, error, signals, and panic.
- **Content-addressed snapshots** with BLAKE3 hashing and automatic
  deduplication; manifests stored in SQLite with migrations.
- **Filesystem tracking** that respects `.gitignore` and Rewind's built-in
  exclusions (VCS/Rewind internals, dependency/build dirs, caches, binaries,
  oversized files, and secret patterns), with a startup reconciliation scan and
  a debounced `notify` watcher.
- **Command and test execution** with live streaming, full stdout/stderr/exit/
  duration capture, configurable timeouts, and cancellation. Pass/fail is
  derived from the exit code, never from output text.
- **Failure investigation** ranking changed files by a transparent, deterministic
  relevance score (stack-trace/dependency/changed/output/timing/generated
  signals). Results are labeled *likely causes*, never confirmed diagnoses.
- **Safe restore** with a pre-restore safety snapshot, a dry-run plan, strict
  path validation, atomic writes confined to the repository root, and `rewind
  undo`.
- **Non-interactive commands**: `init`, `run`, `test`, `checkpoint`, `diff`,
  `restore`, `undo`, `doctor`, `ci`, `completions`, and `man`.
- **CI mode** (`rewind ci`) writing `.rewind-report/` (`report.json`,
  `summary.md`, `test.log`, `changes.patch`) with a matching GitHub Actions
  template.
- **Packaging**: CI workflow (macOS + Linux), release workflow building
  `x86_64`/`aarch64` binaries for macOS and Linux with SHA-256 checksums, shell
  completions, a man page, and a Homebrew formula template.

[Unreleased]: https://github.com/rewind-dev/rewind/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/rewind-dev/rewind/releases/tag/v0.1.0
