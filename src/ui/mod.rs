mod detail;
mod top;

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crossterm::event::KeyCode;

use crate::app::{App, HitMap, View};

/// Read (server → client) is blue everywhere: numbers, labels, graphs, `r` access flags.
pub const READ: Color = Color::Indexed(75); // #5fafff
/// Write (client → server) is pink everywhere: numbers, labels, graphs, `w` access flags.
pub const WRITE: Color = Color::Indexed(212); // #ff87d7

/// `READ` / `WRITE` legend spans, colour-coded.
pub fn legend() -> Vec<Span<'static>> {
    vec![
        Span::raw(" "),
        Span::raw("READ").fg(READ).bold(),
        Span::raw(" server→client · ").fg(DIM),
        Span::raw("WRITE").fg(WRITE).bold(),
        Span::raw(" client→server ").fg(DIM),
    ]
}
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

/// Right-aligned multi-row sparkline: `newest_first[0]` is drawn in the rightmost column.
/// Columns with no sample yet, or a zero value, show a faint dotted baseline on the bottom row, so
/// the graph reads as a timeline filling in from the right.
pub fn sparkline(buf: &mut ratatui::buffer::Buffer, area: Rect, newest_first: &[f64], max: f64, color: Color) {
    const BARS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    if area.is_empty() {
        return;
    }
    let rows = area.height as f64;
    for i in 0..area.width {
        let x = area.right() - 1 - i;
        let v = newest_first.get(i as usize).copied().unwrap_or(0.0);
        // Height in eighths of a cell; any non-zero value gets at least one eighth.
        let eighths = if v <= 0.0 || max <= 0.0 { 0 } else { ((v / max) * rows * 8.0).round().max(1.0) as u16 };
        if eighths == 0 {
            buf[(x, area.bottom() - 1)].set_symbol("·").set_fg(DIM);
            continue;
        }
        for r in 0..area.height {
            let level = eighths.saturating_sub(r * 8).min(8) as usize;
            if level > 0 {
                buf[(x, area.bottom() - 1 - r)].set_symbol(BARS[level]).set_fg(color);
            }
        }
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
        Line::from(vec![
            Span::raw("READ").fg(READ).bold(),
            Span::raw(" = server→client (client reading), ").fg(DIM),
            Span::raw("WRITE").fg(WRITE).bold(),
            Span::raw(" = client→server (client writing)").fg(DIM),
        ]),
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
