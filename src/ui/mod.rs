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

/// Server-side trouble is red, path loss yellow, client/tuning limits cyan.
pub fn flag_color(f: crate::model::Flag) -> Color {
    use crate::model::Flag;
    match f {
        Flag::RecvQ => Color::Red,
        Flag::Loss => Color::Yellow,
        Flag::SendQ | Flag::Rwnd | Flag::Sndbuf => Color::Cyan,
    }
}

/// Colour for an interval's thread load: by average busy share, but red whenever every
/// thread was busy in at least 10 % of the snapshots.
pub fn load_color(t: &crate::parse::rpc::ThreadLoad) -> Color {
    if t.all_busy_pct >= 10.0 { Color::Red } else { busy_color(t.busy_pct().unwrap_or(0.0)) }
}

/// Colour for the share of busy nfsd threads: plain when low, yellow from 50 %, red from 80 %.
pub fn busy_color(pct: f64) -> Color {
    match pct {
        p if p >= 80.0 => Color::Red,
        p if p >= 50.0 => Color::Yellow,
        _ => Color::Reset,
    }
}

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
    if app.show_ops {
        ops_panel(f, area, app);
    }
    if app.show_help {
        help(f, area);
    }
}

/// Centered modal of `w` x `h` (clamped to `area`), cleared and bordered with `title`.
/// Everything behind it is dimmed so the modal reads as the top layer.
fn modal(f: &mut Frame, area: Rect, w: u16, h: u16, title: &str) -> Rect {
    f.buffer_mut().set_style(
        area,
        Style::new()
            .fg(DIM)
            .bg(Color::Reset)
            .add_modifier(Modifier::DIM)
            .remove_modifier(Modifier::BOLD | Modifier::REVERSED),
    );
    let [r] = Layout::horizontal([Constraint::Length(w.min(area.width))]).flex(Flex::Center).areas(area);
    let [r] = Layout::vertical([Constraint::Length(h.min(area.height))]).flex(Flex::Center).areas(r);
    f.render_widget(Clear, r);
    let block = Block::bordered().title(title.to_string());
    let inner = block.inner(r);
    f.render_widget(block, r);
    inner
}

/// Server health overlay: nfsd thread pool, RPC and payload rates, and the busiest operations.
fn ops_panel(f: &mut Frame, area: Rect, app: &App) {
    use crate::fmt;
    const LABEL: usize = 10;
    let m = &app.model;
    let label = |s: &str| Span::raw(format!("{s:<LABEL$}")).bold();
    let num = |s: String| Span::raw(s);
    let dim = |s: &str| Span::raw(s.to_string()).fg(DIM);

    let mut lines: Vec<Line> = Vec::new();
    let mut ops: &[(String, f64)] = &[];
    let busy_row;
    match (&m.server, &m.server_error) {
        (_, Some(e)) => {
            lines.push(Line::from(format!("nfsd server stats unavailable: {e}")).yellow());
            lines.push(Line::from("Run nfscope as root on the NFS server with nfsd running.").fg(DIM));
            busy_row = None;
        }
        (None, None) => {
            lines.push(Line::from("Collecting the first interval…").fg(DIM));
            busy_row = None;
        }
        (Some(r), None) => {
            let total = r.thread_load.map(|t| t.total).or(r.threads);
            let mut l = vec![label("Threads"), num(total.map_or("?".into(), |t| t.to_string()))];
            if let Some(t) = r.thread_load {
                l.extend([
                    dim(" · avg "),
                    Span::raw(format!("{} busy", fmt::avg(t.avg_busy))).fg(load_color(&t)).bold(),
                    dim(&format!(
                        " ({} on storage, {} on CPU) · peak ",
                        fmt::avg(t.avg_blocked),
                        fmt::avg(t.avg_running)
                    )),
                    num(t.peak_busy.to_string()),
                ]);
            }
            lines.push(Line::from(l));
            // Saturation: how often every thread was busy. The busy-share graph sits after it.
            match r.thread_load {
                Some(t) => {
                    let l = vec![
                        label("Saturated"),
                        Span::raw(fmt::percent(t.all_busy_pct)).fg(load_color(&t)).bold(),
                        dim(&format!(
                            " of the time every thread was busy ({} snapshot{})",
                            t.samples,
                            if t.samples == 1 { "" } else { "s" }
                        )),
                    ];
                    busy_row = Some(l.iter().map(|s| s.width()).sum::<usize>() as u16 + 2);
                    lines.push(Line::from(l));
                }
                None => {
                    busy_row = None;
                    lines.push(Line::from(vec![label("Saturated"), dim("thread states unavailable")]));
                }
            }
            if let Some(a) = r.arrived_ps {
                let mut l = vec![label("Requests"), num(fmt::count(a)), dim("/s arrived")];
                if let Some(q) = r.queued_pct {
                    l.extend([dim(" · "), num(fmt::percent(q)), dim(" waited for a thread")]);
                }
                if let Some(t) = r.timedout_ps {
                    l.extend([dim(" · "), num(format!("{t:.1}")), dim("/s idle timeouts")]);
                }
                lines.push(Line::from(l));
            } else {
                lines.push(Line::from(vec![label("Requests"), dim("pool_stats unavailable")]));
            }
            let mut l = vec![
                label("RPC"),
                num(fmt::count(r.calls_ps)),
                dim(" calls/s · "),
                num(fmt::count(r.bad_ps)),
                dim(" bad/s"),
            ];
            if let Some(h) = r.rc_hit_pct {
                l.extend([dim(" · reply cache "), num(fmt::percent(h)), dim(" hits")]);
            }
            lines.push(Line::from(l));
            let (tr, tw) = m.total_rates();
            lines.push(Line::from(vec![
                label("Payload"),
                Span::raw("READ ").fg(READ).bold(),
                Span::raw(fmt::rate(r.io_read_ps)).fg(READ),
                Span::raw("  WRITE ").fg(WRITE).bold(),
                Span::raw(fmt::rate(r.io_write_ps)).fg(WRITE),
                dim(&format!("   TCP {} / {}", fmt::rate(tr), fmt::rate(tw))),
            ]));
            lines.push(Line::default());
            lines.push(Line::from("busy = threads handling a request. Often saturated = add threads;").fg(DIM));
            lines.push(Line::from("many on storage = the exported filesystem is the bottleneck.").fg(DIM));
            lines.push(Line::default());
            lines.push(Line::from(format!("{:<20}{:>8}{:>8}", "OPERATION", "OPS/S", "SHARE")).bold());
            ops = &r.ops;
        }
    }

    // Fixed size whatever the state or number of ops, so the panel never moves or resizes
    // under the reader's eyes: 10 summary/heading lines, the op rows, and the border.
    const MAX_OPS: usize = 12;
    const HEIGHT: u16 = 10 + MAX_OPS as u16 + 2;
    let inner = modal(f, area, 80, HEIGHT, " nfsd server (any key to close) ");
    let room = (inner.height as usize).saturating_sub(lines.len());
    let total: f64 = ops.iter().map(|o| o.1).sum();
    let top = ops.first().map_or(0.0, |o| o.1);
    let bar_w = inner.width.saturating_sub(38) as f64;
    if ops.is_empty() && m.server.is_some() {
        lines.push(Line::from("no operations this interval").fg(DIM));
    }
    for (name, rate) in ops.iter().take(room.min(MAX_OPS)) {
        let share = if total > 0.0 { rate / total * 100.0 } else { 0.0 };
        let bar = "█".repeat(if top > 0.0 { (rate / top * bar_w).round().max(1.0) as usize } else { 0 });
        lines.push(Line::from(vec![
            Span::raw(format!("{name:<20}{:>8}{:>8}  ", fmt::count(*rate), fmt::percent(share))),
            Span::raw(bar).fg(Color::Cyan),
        ]));
    }
    f.render_widget(Paragraph::new(lines), inner);

    // Average-busy history to the right of the Saturated line, scaled so small values show.
    if let Some(x) = busy_row {
        let spark = Rect::new(inner.x + x, inner.y + 1, inner.width.saturating_sub(x), 1).intersection(inner);
        let hist: Vec<f64> = m.server_busy.iter().rev().take(spark.width as usize).copied().collect();
        let scale = hist.iter().copied().fold(10.0, f64::max);
        // The graph stays visible (green) while the share is low.
        let color = match busy_color(hist.first().copied().unwrap_or(0.0)) {
            Color::Reset => Color::Green,
            c => c,
        };
        sparkline(f.buffer_mut(), spark, &hist, scale, color);
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
            let cell = &mut buf[(x, area.bottom() - 1 - r)];
            match level {
                0 => {}
                // Full cell: paint the background too. A `█` glyph only covers the font's glyph
                // box, so with any line spacing a thin seam shows between stacked rows; the cell
                // background always covers the whole line, so stacked rows join seamlessly.
                8 => {
                    cell.set_symbol(BARS[8]).set_fg(color).set_bg(color);
                }
                _ => {
                    cell.set_symbol(BARS[level]).set_fg(color);
                }
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
        ("o", "nfsd server health: busy threads, ops/s"),
        ("l", "toggle log pane"),
        ("FLAGS", "Q server behind · S client slow to read · L loss"),
        ("", "W client window limit · B server send buffer limit"),
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
    let h = lines.len() as u16 + 2;
    let inner = modal(f, area, 86, h, " help (any key to close) ");
    f.render_widget(Paragraph::new(lines), inner);
}
