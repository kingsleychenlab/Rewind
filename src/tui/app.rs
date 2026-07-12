//! Interactive application state and the operations the key handler invokes.
//!
//! [`App`] holds only UI state; the [`Engine`] is owned by the run loop and
//! passed into the methods that need it. This keeps rendering pure and the
//! side effects explicit.

use std::collections::BTreeSet;

use crate::db::models::{self, event_kind, snapshot_kind, Event};
use crate::error::Result;
use crate::restore::{RestorePlan, Selection};
use crate::session::Engine;

/// Whether keystrokes go to the shell or drive Rewind navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Shell,
    Nav,
}

/// Which navigable panel currently has focus (in [`Mode::Nav`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Timeline,
    Detail,
}

/// A modal overlay on top of the main layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    None,
    Help,
    Search,
    ConfirmRestore,
    Message(String),
}

/// The right-hand detail panel's rendered content.
#[derive(Debug, Clone, Default)]
pub struct Detail {
    pub title: String,
    pub lines: Vec<String>,
    pub scroll: u16,
}

/// All interactive UI state.
pub struct App {
    pub mode: Mode,
    pub focus: Focus,
    pub overlay: Overlay,
    pub should_quit: bool,

    pub repo_name: String,
    pub recording: bool,

    pub events: Vec<Event>,
    pub selected: usize,
    pub detail: Detail,

    pub status: String,
    pub dirty: BTreeSet<String>,
    pub shell_alive: bool,
    pub last_test_status: String,

    pub search_input: String,
    pub search_results: Vec<Event>,

    /// Pending restore awaiting confirmation: (snapshot id, plan).
    pub pending_restore: Option<(i64, RestorePlan)>,
}

impl App {
    pub fn new(repo_name: String) -> App {
        App {
            mode: Mode::Shell,
            focus: Focus::Timeline,
            overlay: Overlay::None,
            should_quit: false,
            repo_name,
            recording: true,
            events: Vec::new(),
            selected: 0,
            detail: Detail::default(),
            status: "Ctrl+G: navigation · type to use the shell".into(),
            dirty: BTreeSet::new(),
            shell_alive: true,
            last_test_status: "none".into(),
            search_input: String::new(),
            search_results: Vec::new(),
            pending_restore: None,
        }
    }

    /// Reload the timeline and derived status from the database.
    pub fn refresh(&mut self, engine: &Engine) -> Result<()> {
        self.events = engine.list_events(300)?;
        if self.selected >= self.events.len() {
            self.selected = self.events.len().saturating_sub(1);
        }
        self.last_test_status = match engine.last_test_run()? {
            Some(t) => match (t.passed, t.cancelled) {
                (_, true) => "cancelled".into(),
                (Some(true), _) => "passed".into(),
                (Some(false), _) => "failed".into(),
                (None, _) => "incomplete".into(),
            },
            None => "none".into(),
        };
        self.recompute_detail(engine)?;
        Ok(())
    }

    /// The currently selected event, if any.
    pub fn selected_event(&self) -> Option<&Event> {
        self.events.get(self.selected)
    }

    pub fn select_prev(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn select_next(&mut self) {
        if self.selected + 1 < self.events.len() {
            self.selected += 1;
        }
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    pub fn select_last(&mut self) {
        self.selected = self.events.len().saturating_sub(1);
    }

    /// Recompute the detail panel for the current selection.
    pub fn recompute_detail(&mut self, engine: &Engine) -> Result<()> {
        self.detail.scroll = 0;
        let Some(ev) = self.selected_event().cloned() else {
            self.detail = Detail {
                title: "Details".into(),
                lines: vec!["No events yet. Use the shell or press `t` to run tests.".into()],
                scroll: 0,
            };
            return Ok(());
        };
        self.detail = match ev.kind.as_str() {
            event_kind::TEST => self.detail_for_test(engine, ev.ref_id)?,
            event_kind::INVESTIGATION => Detail {
                title: "Investigation".into(),
                lines: wrap_summary(&ev.summary),
                scroll: 0,
            },
            event_kind::SNAPSHOT | event_kind::CHECKPOINT => {
                self.detail_for_snapshot(engine, ev.ref_id)?
            }
            event_kind::COMMAND => self.detail_for_command(engine, ev.ref_id, &ev.summary)?,
            _ => Detail {
                title: title_for(&ev.kind),
                lines: vec![ev.summary.clone()],
                scroll: 0,
            },
        };
        Ok(())
    }

    fn detail_for_test(&self, engine: &Engine, ref_id: Option<i64>) -> Result<Detail> {
        let mut lines = Vec::new();
        let Some(id) = ref_id else {
            return Ok(Detail {
                title: "Test run".into(),
                lines: vec!["(no test run linked)".into()],
                scroll: 0,
            });
        };
        let Some(run) = models::get_test_run(engine.db.conn(), id)? else {
            return Ok(Detail {
                title: "Test run".into(),
                lines: vec!["(test run not found)".into()],
                scroll: 0,
            });
        };
        lines.push(format!("command: {}", run.command));
        lines.push(format!(
            "result:  {}",
            match run.passed {
                Some(true) => "passed",
                Some(false) => "failed",
                None => "incomplete",
            }
        ));
        if let Some(code) = run.exit_code {
            lines.push(format!("exit:    {code}"));
        }
        if let Some(d) = run.duration_ms {
            lines.push(format!(
                "time:    {}",
                crate::util::format_duration_ms(d as u64)
            ));
        }

        if run.passed == Some(false) {
            lines.push(String::new());
            lines.push("Likely causes (relevance score — likely, not confirmed):".into());
            match crate::investigate::investigate(&engine.db, &engine.store, engine.repo_id, &run)?
            {
                Some(inv) => {
                    if inv.causes.is_empty() {
                        lines.push(
                            "  no changed files between the last pass and this failure".into(),
                        );
                    }
                    for (i, c) in inv.causes.iter().take(12).enumerate() {
                        lines.push(format!("  {}. {} — relevance {}", i + 1, c.path, c.score));
                        for ev in &c.evidence {
                            lines.push(format!("       - {ev}"));
                        }
                        for l in c.changed_lines.iter().take(6) {
                            lines.push(format!("       changed line {l}"));
                        }
                    }
                    if !inv.failing_output_excerpt.is_empty() {
                        lines.push(String::new());
                        lines.push("Failing output (tail):".into());
                        for l in inv
                            .failing_output_excerpt
                            .lines()
                            .rev()
                            .take(15)
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                        {
                            lines.push(format!("  {l}"));
                        }
                    }
                }
                None => lines.push("  no prior passing run to compare against".into()),
            }
        }
        Ok(Detail {
            title: "Test run".into(),
            lines,
            scroll: 0,
        })
    }

    fn detail_for_snapshot(&self, engine: &Engine, ref_id: Option<i64>) -> Result<Detail> {
        let Some(id) = ref_id else {
            return Ok(Detail {
                title: "Snapshot".into(),
                lines: vec!["(no snapshot linked)".into()],
                scroll: 0,
            });
        };
        let Some(snap) = engine.get_snapshot(id)? else {
            return Ok(Detail {
                title: "Snapshot".into(),
                lines: vec!["(snapshot not found)".into()],
                scroll: 0,
            });
        };
        let mut lines = vec![
            format!("snapshot #{}  ({})", snap.id, snap.kind),
            format!("label:  {}", snap.label),
            format!("files:  {}", snap.file_count),
            format!("time:   {}", crate::util::format_timestamp(snap.created_at)),
        ];
        if let Some(b) = &snap.git_branch {
            lines.push(format!("branch: {b}"));
        }
        if let Some(parent) = snap.parent_id {
            lines.push(String::new());
            lines.push(format!("changes vs snapshot #{parent}:"));
            let changes = crate::diff::diff_snapshots(&engine.db, parent, snap.id)?;
            if changes.is_empty() {
                lines.push("  (no changes)".into());
            }
            for c in changes.iter().take(200) {
                let stat = crate::diff::line_stat(
                    &engine.store,
                    c.old_hash.as_deref(),
                    c.new_hash.as_deref(),
                )
                .unwrap_or_default();
                lines.push(format!(
                    "  {} {} (+{} -{})",
                    c.letter(),
                    c.path,
                    stat.added,
                    stat.removed
                ));
            }
        }
        lines.push(String::new());
        lines.push("Press `r` to restore this snapshot.".into());
        Ok(Detail {
            title: "Snapshot".into(),
            lines,
            scroll: 0,
        })
    }

    fn detail_for_command(
        &self,
        engine: &Engine,
        ref_id: Option<i64>,
        summary: &str,
    ) -> Result<Detail> {
        let mut lines = vec![summary.to_string(), String::new()];
        if let Some(id) = ref_id {
            let obj: Option<String> = engine
                .db
                .conn()
                .query_row(
                    "SELECT output_object FROM commands WHERE id = ?1",
                    [id],
                    |r| r.get(0),
                )
                .ok()
                .flatten();
            if let Some(hash) = obj {
                if let Ok(bytes) = engine.store.read(&hash) {
                    let text = String::from_utf8_lossy(&bytes);
                    lines.push("output:".into());
                    for l in text
                        .lines()
                        .rev()
                        .take(40)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                    {
                        lines.push(format!("  {l}"));
                    }
                }
            }
        }
        Ok(Detail {
            title: "Command".into(),
            lines,
            scroll: 0,
        })
    }

    // -- actions -----------------------------------------------------------

    pub fn checkpoint(&mut self, engine: &mut Engine) -> Result<()> {
        let snap = engine.checkpoint(None)?;
        self.status = format!(
            "Checkpoint #{} created ({} files)",
            snap.id, snap.file_count
        );
        self.refresh(engine)?;
        Ok(())
    }

    /// Prepare a restore of the selected snapshot, populating the confirm
    /// overlay. Returns true when a confirmation is now pending.
    pub fn begin_restore_selected(&mut self, engine: &mut Engine) -> Result<bool> {
        let Some(ev) = self.selected_event() else {
            return Ok(false);
        };
        if ev.kind != event_kind::SNAPSHOT && ev.kind != event_kind::CHECKPOINT {
            self.status = "Select a snapshot or checkpoint to restore.".into();
            return Ok(false);
        }
        let Some(snap_id) = ev.ref_id else {
            return Ok(false);
        };
        let plan = engine.plan_restore(snap_id, &Selection::All)?;
        if plan.is_empty() {
            self.status = "Working tree already matches that snapshot.".into();
            return Ok(false);
        }
        self.pending_restore = Some((snap_id, plan));
        self.overlay = Overlay::ConfirmRestore;
        Ok(true)
    }

    pub fn confirm_restore(&mut self, engine: &mut Engine) -> Result<()> {
        if let Some((snap_id, plan)) = self.pending_restore.take() {
            let outcome = engine.execute_restore(snap_id, &Selection::All, &plan)?;
            self.status = format!(
                "Restored snapshot #{snap_id}: {} written, {} deleted. `u`ndo available.",
                outcome.stats.written, outcome.stats.deleted
            );
            self.refresh(engine)?;
        }
        self.overlay = Overlay::None;
        Ok(())
    }

    pub fn undo_last_restore(&mut self, engine: &mut Engine) -> Result<()> {
        match models::last_undoable_restore(engine.db.conn(), engine.repo_id)? {
            Some(rec) => {
                let stats = engine.undo_restore(rec.id)?;
                self.status = format!(
                    "Undid restore #{}: {} written, {} deleted.",
                    rec.id, stats.written, stats.deleted
                );
                self.refresh(engine)?;
            }
            None => self.status = "No restore to undo.".into(),
        }
        Ok(())
    }

    pub fn run_search(&mut self, engine: &Engine) -> Result<()> {
        self.search_results = if self.search_input.trim().is_empty() {
            Vec::new()
        } else {
            engine.search_events(self.search_input.trim(), 100)?
        };
        Ok(())
    }

    /// Record a command captured from the embedded shell: snapshot the working
    /// tree and add a timeline entry.
    pub fn capture_shell_command(&mut self, engine: &mut Engine, command: &str) -> Result<()> {
        let command = command.trim();
        if command.is_empty() {
            return Ok(());
        }
        let snap = engine.snapshot(snapshot_kind::POST_COMMAND, &format!("after `{command}`"))?;
        let cmd_id = models::insert_command(
            engine.db.conn(),
            engine.repo_id,
            engine.session_id,
            "shell",
            command,
            Some(&engine.repo.root.to_string_lossy()),
            crate::util::now_millis(),
            None,
        )?;
        models::finish_command(
            engine.db.conn(),
            cmd_id,
            None,
            crate::util::now_millis(),
            0,
            Some(snap.id),
            None,
        )?;
        models::insert_event(
            engine.db.conn(),
            engine.repo_id,
            engine.session_id,
            event_kind::COMMAND,
            Some(cmd_id),
            &format!("$ {command}"),
        )?;
        self.dirty.clear();
        self.refresh(engine)?;
        Ok(())
    }
}

fn title_for(kind: &str) -> String {
    match kind {
        event_kind::SESSION_START => "Session start".into(),
        event_kind::SESSION_END => "Session end".into(),
        event_kind::RESTORE => "Restore".into(),
        other => other.to_string(),
    }
}

fn wrap_summary(s: &str) -> Vec<String> {
    s.split(';').map(|p| p.trim().to_string()).collect()
}
