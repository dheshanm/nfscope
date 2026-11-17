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
/// Cumulative read bytes: a deeper shade of `READ`, so totals sit behind the live rates.
pub const READ_TOT: Color = Color::Indexed(32); // #0087d7
/// Cumulative write bytes: a deeper shade of `WRITE`.
pub const WRITE_TOT: Color = Color::Indexed(169); // #d75faf
/// Warnings (retransmits, slow requests, busy threads, missing data sources) are orange;
/// red is kept for server-side trouble.
pub const WARN: Color = Color::Indexed(214); // #ffaf00

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

/// Colour for an NFS operation: data ops share the READ / WRITE colours (COMMIT, which flushes
/// writes, the deeper write shade); metadata ops are a neutral grey.
pub fn op_color(name: &str) -> Color {
    match name {
        "READ" => READ,
        "WRITE" => WRITE,
        "COMMIT" => WRITE_TOT,
        _ => Color::Indexed(250),
    }
}

/// A request running this long (µs) is worth a look: highlighted in `WARN` orange.
pub const SLOW_RPC_US: u64 = 1_000_000;

/// Server-side trouble is red, path loss orange, client/tuning limits cyan.
pub fn flag_color(f: crate::model::Flag) -> Color {
    use crate::model::Flag;
    match f {
        Flag::RecvQ => Color::Red,
        Flag::Loss => WARN,
        Flag::SendQ | Flag::Rwnd | Flag::Sndbuf => Color::Cyan,
    }
}

/// Colour for an interval's thread load: by average busy share, but red whenever every
/// thread was busy in at least 10 % of the snapshots.
pub fn load_color(t: &crate::parse::rpc::ThreadLoad) -> Color {
    if t.all_busy_pct >= 10.0 { Color::Red } else { busy_color(t.busy_pct().unwrap_or(0.0)) }
}

/// Colour for the share of busy nfsd threads: plain when low, orange from 50 %, red from 80 %.
pub fn busy_color(pct: f64) -> Color {
    match pct {
        p if p >= 80.0 => Color::Red,
        p if p >= 50.0 => WARN,
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
    if app.show_inflight {
        inflight_panel(f, area, app);
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
            lines.push(Line::from(format!("nfsd server stats unavailable: {e}")).fg(WARN));
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
            let mut l = vec![label("In flight")];
            match (&m.inflight, &m.inflight_error) {
                (Some(t), _) => {
                    l.extend([num(t.now.to_string()), dim(" now · avg "), num(fmt::avg(t.avg))]);
                    if let Some(c) = t.clients.first().filter(|c| c.avg > 0.0) {
                        l.extend([
                            dim(" · busiest "),
                            num(client_name(app, c.ip)),
                            dim(&format!(" {}", fmt::avg(c.avg))),
                        ]);
                    }
                    l.push(dim(" · i: details"));
                }
                (None, Some(e)) => l.push(dim(&format!("unavailable: {e}"))),
                (None, None) => l.push(dim("…")),
            }
            lines.push(Line::from(l));
            lines.push(Line::default());
            lines.push(Line::from("busy = threads handling a request. Often saturated = add threads;").fg(DIM));
            lines.push(Line::from("many on storage = the exported filesystem is the bottleneck.").fg(DIM));
            lines.push(Line::default());
            lines.push(Line::from(format!("{:<20}{:>8}{:>8}", "OPERATION", "OPS/S", "SHARE")).bold());
            ops = &r.ops;
        }
    }

    // Fixed size whatever the state or number of ops, so the panel never moves or resizes
    // under the reader's eyes: 11 summary/heading lines, the op rows, and the border.
    const MAX_OPS: usize = 12;
    const HEIGHT: u16 = 11 + MAX_OPS as u16 + 2;
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

/// A client's host name if resolved, else its address (`?` for requests without one).
fn client_name(app: &App, ip: Option<std::net::IpAddr>) -> String {
    match ip {
        Some(ip) => match app.dns.get(&ip) {
            Some(crate::app::Dns::Name(n)) => n.clone(),
            _ => ip.to_string(),
        },
        None => "?".into(),
    }
}

/// `s` cut to `w` columns (ending in `…` when cut), padded to exactly `w`.
pub(super) fn fit(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w {
        format!("{s:<w$}")
    } else {
        let mut t: String = s.chars().take(w.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// In-flight overlay: which clients and operations hold the nfsd threads, and the requests
/// that have been running longest (nfsd netlink `rpc-status-get`, Linux 6.7+).
fn inflight_panel(f: &mut Frame, area: Rect, app: &App) {
    use crate::fmt;
    // Fixed size, like the server panel: 3 summary lines, the client and op tables side by
    // side, the longest-running list, and the border.
    const ROWS: usize = 8;
    const SLOW_ROWS: usize = 8;
    const HEIGHT: u16 = 3 + 1 + ROWS as u16 + 1 + 2 + SLOW_ROWS as u16 + 2;
    let inner = modal(f, area, 100, HEIGHT, " nfsd requests in flight (any key to close · o: server) ");
    let dim = |s: &str| Span::raw(s.to_string()).fg(DIM);
    let m = &app.model;
    let Some(l) = &m.inflight else {
        let lines = match &m.inflight_error {
            Some(e) => vec![
                Line::from(format!("In-flight requests unavailable: {e}")).fg(WARN),
                Line::default(),
                Line::from("They come from the nfsd netlink interface (rpc-status-get), which needs").fg(DIM),
                Line::from("Linux 6.7 or newer with nfsd running. Busy-thread counts (o) work without it.").fg(DIM),
            ],
            None => vec![Line::from("Collecting the first interval…").fg(DIM)],
        };
        f.render_widget(Paragraph::new(lines), inner);
        return;
    };
    let age = |us: u64| {
        let s = Span::raw(format!("{:>9}", fmt::latency(us)));
        if us >= SLOW_RPC_US { s.fg(WARN).bold() } else { s }
    };
    let head = Style::new().bold().bg(Color::Indexed(236));
    let mut lines = vec![
        Line::from(vec![
            Span::raw(format!("{} in flight now", l.now)).bold(),
            dim(" · avg "),
            Span::raw(fmt::avg(l.avg)),
            dim(" · peak "),
            Span::raw(l.peak.to_string()),
            dim(&format!(" · {} snapshot{} this interval", l.snapshots, if l.snapshots == 1 { "" } else { "s" })),
        ]),
        Line::from("THREADS = nfsd threads busy with those requests, averaged over the interval.").fg(DIM),
        Line::default(),
    ];

    // Clients (left) and operations (right), side by side.
    let (cw, ow) = (inner.width.saturating_sub(2) as usize * 3 / 5, inner.width.saturating_sub(2) as usize * 2 / 5);
    let name_w = cw.saturating_sub(8 + 5 + 9 + 1);
    let op_w = ow.saturating_sub(8 + 9 + 1);
    let mut row = vec![
        Span::styled(format!("{}{:>8}{:>5}{:>9} ", fit("CLIENT", name_w), "THREADS", "PEAK", "OLDEST"), head),
        Span::raw("  "),
        Span::styled(format!("{}{:>8}{:>9} ", fit("OPERATION", op_w), "THREADS", "OLDEST"), head),
    ];
    lines.push(Line::from(std::mem::take(&mut row)));
    for i in 0..ROWS {
        let mut spans = Vec::new();
        match l.clients.get(i) {
            Some(c) => spans.extend([
                Span::raw(fit(&client_name(app, c.ip), name_w)),
                Span::raw(format!("{:>8}{:>5}", fmt::avg(c.avg), c.peak)),
                age(c.oldest_us),
                Span::raw(" "),
            ]),
            None if i == 0 => spans.push(dim(&fit("no requests in flight this interval", cw))),
            None => spans.push(Span::raw(" ".repeat(cw))),
        }
        spans.push(Span::raw("  "));
        if let Some(o) = l.ops.get(i) {
            spans.extend([
                Span::raw(fit(&o.name, op_w)),
                Span::raw(format!("{:>8}", fmt::avg(o.avg))),
                age(o.oldest_us),
            ]);
        }
        lines.push(Line::from(spans));
    }
    let more = l.clients.len().saturating_sub(ROWS);
    lines.push(if more > 0 { Line::from(format!("… and {more} more clients")).fg(DIM) } else { Line::default() });

    lines.push(Line::from("Longest-running requests this interval").bold());
    let slow_name_w = cw.saturating_sub(9 + 2);
    lines.push(Line::from(Span::styled(
        format!("{:>9}  {}{}", "AGE", fit("CLIENT", slow_name_w), fit("OPERATION", ow)),
        head,
    )));
    for s in l.slowest.iter().take(SLOW_ROWS) {
        lines.push(Line::from(vec![
            age(s.age_us),
            Span::raw("  "),
            Span::raw(fit(&client_name(app, s.client), slow_name_w)),
            Span::raw(fit(&s.label, op_w)),
            if s.running { Span::raw(" running").cyan() } else { dim(" done") },
        ]));
    }
    if l.slowest.is_empty() {
        lines.push(dim("none").into());
    }
    f.render_widget(Paragraph::new(lines), inner);
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
            .fg(WARN),
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
        ("f", "detail: find full path of file / every file in group (Esc stops)"),
        ("…/IMAGES/", "kernel gives only parent/name: groups every dir with that name"),
        ("d", "detail: hide files held only as cached delegations"),
        ("o", "nfsd server health: busy threads, ops/s"),
        ("i", "requests in flight: which clients / ops hold nfsd threads"),
        ("THREADS", "avg nfsd threads busy with a client's requests (Linux 6.7+)"),
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
