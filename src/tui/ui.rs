//! Rendering: the fixed layout, the embedded-terminal grid, and overlays.
//!
//! The design is deliberately restrained — borders, a timeline list, the shell,
//! a detail panel, and a status line. No charts, cards, or animation.

use std::sync::{Arc, Mutex};

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use super::app::{App, Focus, Mode, Overlay};
use crate::db::models::event_kind;

/// Panel rectangles for one frame.
pub struct Panes {
    pub header: Rect,
    pub timeline: Rect,
    pub shell: Rect,
    pub shell_inner: Rect,
    pub detail: Rect,
    pub status: Rect,
}

/// Compute the layout for a given terminal area.
pub fn layout(area: Rect) -> Panes {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Min(3),    // body
            Constraint::Length(1), // status
        ])
        .split(area);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(30), // timeline
            Constraint::Min(30),    // shell
            Constraint::Length(42), // detail
        ])
        .split(rows[1]);

    let shell = cols[1];
    let shell_inner = inner(shell);
    Panes {
        header: rows[0],
        timeline: cols[0],
        shell,
        shell_inner,
        detail: cols[2],
        status: rows[2],
    }
}

/// The area inside a single-cell border.
fn inner(r: Rect) -> Rect {
    Rect {
        x: r.x.saturating_add(1),
        y: r.y.saturating_add(1),
        width: r.width.saturating_sub(2),
        height: r.height.saturating_sub(2),
    }
}

/// Render a full frame.
pub fn render(f: &mut Frame, app: &App, parser: &Arc<Mutex<vt100::Parser>>) {
    let panes = layout(f.area());

    render_header(f, app, panes.header);
    render_timeline(f, app, panes.timeline);
    render_shell(f, app, parser, panes.shell, panes.shell_inner);
    render_detail(f, app, panes.detail);
    render_status(f, app, panes.status);

    match &app.overlay {
        Overlay::Help => render_help(f, f.area()),
        Overlay::Search => render_search(f, app, f.area()),
        Overlay::ConfirmRestore => render_confirm(f, app, f.area()),
        Overlay::Message(m) => render_message(f, m, f.area()),
        Overlay::None => {}
    }

    // Place the cursor in the shell when typing there.
    if app.mode == Mode::Shell && app.overlay == Overlay::None {
        if let Ok(p) = parser.lock() {
            let screen = p.screen();
            if !screen.hide_cursor() {
                let (cr, cc) = screen.cursor_position();
                let x = panes.shell_inner.x.saturating_add(cc);
                let y = panes.shell_inner.y.saturating_add(cr);
                if x < panes.shell_inner.right() && y < panes.shell_inner.bottom() {
                    f.set_cursor_position((x, y));
                }
            }
        }
    }
}

fn render_header(f: &mut Frame, app: &App, area: Rect) {
    let test_style = match app.last_test_status.as_str() {
        "passed" => Style::default().fg(Color::Green),
        "failed" => Style::default().fg(Color::Red),
        _ => Style::default().fg(Color::DarkGray),
    };
    let mut spans = vec![
        Span::styled(" Rewind ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("─ "),
        Span::styled(app.repo_name.clone(), Style::default().fg(Color::Cyan)),
        Span::raw(" ─ "),
        Span::styled(
            if app.recording { "recording" } else { "paused" },
            Style::default().fg(if app.recording {
                Color::Green
            } else {
                Color::Yellow
            }),
        ),
        Span::raw(" ─ last test: "),
        Span::styled(app.last_test_status.clone(), test_style),
    ];
    if !app.dirty.is_empty() {
        spans.push(Span::raw(" ─ "));
        spans.push(Span::styled(
            format!("{} changed", app.dirty.len()),
            Style::default().fg(Color::Yellow),
        ));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Reset)),
        area,
    );
}

fn render_timeline(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.mode == Mode::Nav && app.focus == Focus::Timeline;
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(" Timeline ");

    let items: Vec<ListItem> = app
        .events
        .iter()
        .map(|e| {
            let glyph = glyph_for(&e.kind);
            let time = crate::util::format_clock(e.created_at);
            let text = format!("{time} {glyph} {}", crate::util::truncate(&e.summary, 22));
            ListItem::new(Line::from(text)).style(style_for(&e.kind))
        })
        .collect();

    let mut state = ListState::default();
    if !app.events.is_empty() {
        state.select(Some(app.selected));
    }
    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .add_modifier(Modifier::REVERSED)
            .add_modifier(Modifier::BOLD),
    );
    f.render_stateful_widget(list, area, &mut state);
}

fn render_shell(
    f: &mut Frame,
    app: &App,
    parser: &Arc<Mutex<vt100::Parser>>,
    outer: Rect,
    inner: Rect,
) {
    let focused = app.mode == Mode::Shell;
    let border_style = if focused {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let title = if app.shell_alive {
        " Embedded Shell "
    } else {
        " Embedded Shell (exited) "
    };
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(title),
        outer,
    );

    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let guard = match parser.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let screen = guard.screen();
    let (grows, gcols) = screen.size();
    let buf = f.buffer_mut();
    for row in 0..inner.height {
        for col in 0..inner.width {
            let x = inner.x + col;
            let y = inner.y + row;
            if row >= grows || col >= gcols {
                buf[(x, y)].set_symbol(" ").set_style(Style::default());
                continue;
            }
            match screen.cell(row, col) {
                Some(cell) => {
                    let contents = cell.contents();
                    let symbol = if contents.is_empty() { " " } else { &contents };
                    let style = cell_style(cell);
                    buf[(x, y)].set_symbol(symbol).set_style(style);
                }
                None => {
                    buf[(x, y)].set_symbol(" ");
                }
            }
        }
    }
}

fn cell_style(cell: &vt100::Cell) -> Style {
    let mut style = Style::default()
        .fg(map_color(cell.fgcolor()))
        .bg(map_color(cell.bgcolor()));
    if cell.bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

fn map_color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn render_detail(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.mode == Mode::Nav && app.focus == Focus::Detail;
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let text: Vec<Line> = app
        .detail
        .lines
        .iter()
        .map(|l| Line::from(l.clone()))
        .collect();
    let title = format!(" {} ", app.detail.title);
    let p = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(title),
        )
        .scroll((app.detail.scroll, 0));
    f.render_widget(p, area);
}

fn render_status(f: &mut Frame, app: &App, area: Rect) {
    let mode = match app.mode {
        Mode::Shell => Span::styled(
            " SHELL ",
            Style::default().bg(Color::Green).fg(Color::Black),
        ),
        Mode::Nav => Span::styled(" NAV ", Style::default().bg(Color::Cyan).fg(Color::Black)),
    };
    let hint = match app.mode {
        Mode::Shell => "Ctrl+G nav · keys go to shell",
        Mode::Nav => "Ctrl+G shell · j/k move · Enter open · t test · c checkpoint · r restore · u undo · / search · ? help · q quit",
    };
    let line = Line::from(vec![
        mode,
        Span::raw(" "),
        Span::styled(
            crate::util::truncate(&app.status, area.width.saturating_sub(24) as usize),
            Style::default().fg(Color::White),
        ),
        Span::raw("  "),
        Span::styled(hint, Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn render_help(f: &mut Frame, area: Rect) {
    let r = centered(area, 64, 22);
    f.render_widget(Clear, r);
    let lines = vec![
        Line::from(Span::styled(
            "Rewind — keyboard help",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("Ctrl+G      toggle Shell / Navigation modes"),
        Line::from(""),
        Line::from(Span::styled(
            "Navigation mode:",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from("  j / ↓        move selection down"),
        Line::from("  k / ↑        move selection up"),
        Line::from("  g / G        jump to first / last event"),
        Line::from("  Tab          switch focus (timeline / detail)"),
        Line::from("  Enter        open the selected event's detail"),
        Line::from("  t            run the configured test command"),
        Line::from("  c            create a checkpoint snapshot"),
        Line::from("  r            restore the selected snapshot"),
        Line::from("  u            undo the last restore"),
        Line::from("  /            search events"),
        Line::from("  ?            toggle this help"),
        Line::from("  q            quit Rewind"),
        Line::from(""),
        Line::from(Span::styled(
            "Press ? or Esc to close.",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" Help "))
            .wrap(Wrap { trim: false }),
        r,
    );
}

fn render_search(f: &mut Frame, app: &App, area: Rect) {
    let r = centered(area, 70, 20);
    f.render_widget(Clear, r);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("search: ", Style::default().fg(Color::Cyan)),
            Span::raw(app.search_input.clone()),
            Span::styled("▏", Style::default().fg(Color::Cyan)),
        ]),
        Line::from(""),
    ];
    if app.search_results.is_empty() {
        lines.push(Line::from(Span::styled(
            "type to search the timeline; Enter to run, Esc to close",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for e in app.search_results.iter().take(14) {
            lines.push(Line::from(format!(
                "{} {} {}",
                crate::util::format_clock(e.created_at),
                glyph_for(&e.kind),
                crate::util::truncate(&e.summary, 56)
            )));
        }
    }
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Search events "),
        ),
        r,
    );
}

fn render_confirm(f: &mut Frame, app: &App, area: Rect) {
    let r = centered(area, 66, 18);
    f.render_widget(Clear, r);
    let mut lines = vec![
        Line::from(Span::styled(
            "Confirm restore",
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Yellow),
        )),
        Line::from(""),
    ];
    if let Some((snap_id, plan)) = &app.pending_restore {
        lines.push(Line::from(format!("Snapshot #{snap_id}")));
        lines.push(Line::from(format!(
            "  {} to write, {} to delete",
            plan.create.len() + plan.overwrite.len(),
            plan.delete.len()
        )));
        lines.push(Line::from(""));
        for e in plan.create.iter().take(4) {
            lines.push(Line::from(format!("  + {}", e.path)));
        }
        for e in plan.overwrite.iter().take(4) {
            lines.push(Line::from(format!("  ~ {}", e.path)));
        }
        for p in plan.delete.iter().take(4) {
            lines.push(Line::from(format!("  - {p}")));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "A safety snapshot is created first. Undo with `u`.",
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Press y to proceed, n or Esc to cancel.",
        Style::default().fg(Color::White),
    )));
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" Restore "))
            .wrap(Wrap { trim: false }),
        r,
    );
}

fn render_message(f: &mut Frame, msg: &str, area: Rect) {
    let r = centered(area, 60, 7);
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(msg.to_string())
            .block(Block::default().borders(Borders::ALL).title(" Rewind "))
            .wrap(Wrap { trim: false }),
        r,
    );
}

fn glyph_for(kind: &str) -> &'static str {
    match kind {
        event_kind::SESSION_START => "▶",
        event_kind::SESSION_END => "■",
        event_kind::COMMAND => "$",
        event_kind::TEST => "✓",
        event_kind::SNAPSHOT => "◆",
        event_kind::CHECKPOINT => "⚑",
        event_kind::RESTORE => "↺",
        event_kind::INVESTIGATION => "?",
        _ => "·",
    }
}

fn style_for(kind: &str) -> Style {
    match kind {
        event_kind::TEST => Style::default().fg(Color::Green),
        event_kind::INVESTIGATION => Style::default().fg(Color::Yellow),
        event_kind::RESTORE => Style::default().fg(Color::Magenta),
        event_kind::CHECKPOINT => Style::default().fg(Color::Cyan),
        _ => Style::default(),
    }
}
