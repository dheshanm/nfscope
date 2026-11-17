mod detail;
mod top;

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crossterm::event::KeyCode;

use crate::app::{App, HitMap, View};

pub const READ: Color = Color::Cyan;
pub const WRITE: Color = Color::Magenta;
pub const DIM: Color = Color::DarkGray;

pub fn draw(f: &mut Frame, app: &mut App) {
    app.hit = HitMap::default();
    let area = f.area();
    let [main, log] = if app.show_log {
        Layout::vertical([Constraint::Min(5), Constraint::Length(10)]).areas(area)
    } else {
        [area, Rect::default()]
    };
    match app.view {
        View::Top => top::draw(f, main, app),
        View::Detail(_) => detail::draw(f, main, app),
    }
    if app.show_log {
        f.render_widget(
            tui_logger::TuiLoggerWidget::default()
                .block(Block::bordered().title(" log (l to hide) "))
                .style_error(Style::new().red())
                .style_warn(Style::new().yellow())
                .style_info(Style::new().green()),
            log,
        );
    }
    if app.show_help {
        help(f, area);
    }
}

/// Render a `key desc  key desc ...` footer. Hints with a key code become clickable buttons.
pub fn footer(f: &mut Frame, area: Rect, pairs: &[(&str, &str, Option<KeyCode>)], hits: &mut HitMap) {
    let mut spans = Vec::new();
    let mut x = area.x;
    for (k, d, code) in pairs {
        let key = Span::styled(k.to_string(), Style::new().add_modifier(Modifier::BOLD).fg(Color::Yellow));
        let desc = Span::raw(format!(" {d}"));
        let w = (key.width() + desc.width()) as u16;
        if let Some(code) = code {
            let r = Rect::new(x, area.y, w, 1).intersection(area);
            if !r.is_empty() {
                hits.buttons.push((r, *code));
            }
        }
        x = x.saturating_add(w + 2);
        spans.extend([key, desc, Span::raw("  ")]);
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

pub fn status_lines(app: &App) -> Vec<Line<'static>> {
    let mut v = Vec::new();
    if let Some(e) = &app.model.sock_error {
        v.push(Line::from(format!(" ⚠ socket sampling failed: {e}")).red());
    }
    if let Some(e) = &app.model.nfsd_error {
        v.push(
            Line::from(format!(
                " ⚠ NFSv4 client info unavailable ({e}). Run as root on the NFS server; rates still work."
            ))
            .yellow(),
        );
    }
    v
}

fn help(f: &mut Frame, area: Rect) {
    let rows: &[(&str, &str)] = &[
        ("↑↓ j k PgUp PgDn", "move"),
        ("Enter", "drill into client / toggle tree node"),
        ("Esc / Backspace", "back (or clear filter)"),
        ("s / S", "cycle sort column / reverse"),
        ("/", "fuzzy filter (hosts, or files in detail)"),
        ("p", "pause (counters keep being sampled)"),
        ("+ / -", "sampling interval"),
        ("← →  e / c", "detail: collapse/expand, expand/collapse all"),
        ("r", "detail: reload open files"),
        ("d", "detail: hide files held only as cached delegations"),
        ("l", "toggle log pane"),
        ("mouse", "click header: sort (again: reverse) · double-click row: open"),
        ("", "click tree node: fold · wheel: scroll · right-click: back"),
        ("q / Ctrl-C", "quit"),
    ];
    let mut lines = vec![
        Line::from("READ = server→client (client reading), WRITE = client→server (client writing)").fg(DIM),
        Line::from("Totals are per currently-open TCP connection and reset when a client reconnects.").fg(DIM),
        Line::default(),
    ];
    for (k, d) in rows {
        lines.push(Line::from(vec![Span::styled(format!("{k:>18}  "), Style::new().yellow().bold()), Span::raw(*d)]));
    }
    let w = 86.min(area.width);
    let h = (lines.len() as u16 + 2).min(area.height);
    let [r] = Layout::horizontal([Constraint::Length(w)]).flex(Flex::Center).areas(area);
    let [r] = Layout::vertical([Constraint::Length(h)]).flex(Flex::Center).areas(r);
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(lines).block(Block::bordered().title(" help (any key to close) ")), r);
}
