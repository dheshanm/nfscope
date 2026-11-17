use std::collections::BTreeMap;

use nucleo_matcher::Utf32Str;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, Cell, Chart, Dataset, GraphType, Paragraph, Row, Table, Wrap};
use throbber_widgets_tui::Throbber;
use tui_tree_widget::{Tree, TreeItem};

use crossterm::event::KeyCode;

use super::{DIM, READ, READ_TOT, WARN, WRITE, WRITE_TOT, footer as draw_footer, status_lines};
use crate::app::{App, Detail, PathState, View};
use crate::files::{ANON_DIR, OpenFile, OpenFiles, dir_label};
use crate::fmt;
use crate::model::{ClientStat, FLAG_LIMITED_PCT, FLAG_RECV_Q, FLAG_RETRANS_PS, FLAG_SEND_Q, Flag};

const MIB: f64 = 1024.0 * 1024.0;

pub fn draw(f: &mut Frame, area: Rect, app: &mut App) {
    let status = status_lines(app);
    let View::Detail(d) = &mut app.view else { return };
    let live = app.model.clients.get(&d.ip);
    let stat = live.or(d.last.as_ref());

    let n_conns = stat.map_or(0, |c| c.conns.len()).clamp(1, 4) as u16;
    let n_flags = stat.map_or(0, |c| c.flags().len()) as u16;
    // One more row for the `nfsd` (in-flight) line when the kernel reports it.
    let info_h = 9 + u16::from(stat.is_some_and(|c| c.inflight.is_some()));
    let ops_h = stat.map_or(0, ops_height);
    let [title, banner, stats, ops, conns, files, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(status.len() as u16),
        Constraint::Length(info_h),
        Constraint::Length(ops_h),
        Constraint::Length(n_conns + n_flags + 3),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .areas(area);

    // Title
    let host = match app.dns.get(&d.ip) {
        Some(crate::app::Dns::Name(n)) => n.clone(),
        _ => String::new(),
    };
    const BACK: &str = " ◀ back ";
    app.hit.back = Rect::new(title.x, title.y, (BACK.chars().count() as u16).min(title.width), title.height.min(1));
    let mut t = vec![
        Span::raw(BACK).black().on_gray(),
        Span::raw(" "),
        Span::raw(d.ip.to_string()).bold(),
        Span::raw(format!("  {host}")),
    ];
    if let Some(n) = stat.and_then(ClientStat::nfs_name) {
        t.push(Span::raw(format!("  [{n}]")).fg(DIM));
    }
    if live.is_none() {
        t.push(Span::raw("  (disconnected: showing last known values)").red());
    }
    if app.paused {
        t.push(Span::raw("  "));
        t.push(" PAUSED ".black().on_yellow());
    }
    f.render_widget(Paragraph::new(Line::from(t)), title);
    f.render_widget(Paragraph::new(status), banner);

    // Stats + chart
    let [info, chart] = Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(stats);
    draw_info(f, info, stat, app.model.has_rates());
    draw_chart(f, chart, stat);
    if let Some(c) = stat.filter(|_| ops_h > 0) {
        draw_ops(f, ops, c);
    }
    draw_conns(f, conns, stat);
    draw_files(f, files, d, &mut app.matcher, &app.throbber);
    app.hit.tree = files;

    if d.filter_editing {
        let line = Line::from(vec![Span::raw(" filter files: ").yellow(), Span::raw(format!("{}█", d.filter))]);
        f.render_widget(Paragraph::new(line), footer);
    } else {
        use KeyCode::Char;
        draw_footer(
            f,
            footer,
            &[
                ("Esc", "back", Some(KeyCode::Esc)),
                ("↑↓", "move", None),
                ("←→/↵", "fold", Some(KeyCode::Enter)),
                ("e", "expand all", Some(Char('e'))),
                ("c", "collapse all", Some(Char('c'))),
                ("d", if d.hide_deleg { "show delegs" } else { "hide delegs" }, Some(Char('d'))),
                ("/", "filter", Some(Char('/'))),
                ("f", "full path", Some(Char('f'))),
                ("r", "reload", Some(Char('r'))),
                ("p", "pause", Some(Char('p'))),
                ("?", "help", Some(Char('?'))),
            ],
            &mut app.hit,
        );
    }
}

fn kv<'a>(k: &'a str, v: impl Into<Span<'a>>) -> Line<'a> {
    Line::from(vec![Span::raw(format!("{k:<9}")).fg(DIM), v.into()])
}

fn draw_info(f: &mut Frame, area: Rect, stat: Option<&ClientStat>, has_rates: bool) {
    let block = Block::bordered().title(" Client ");
    let Some(c) = stat else {
        f.render_widget(Paragraph::new("no data").block(block), area);
        return;
    };
    let rate = |v: f64| if has_rates { fmt::rate(v) } else { "…".into() };
    let mut lines = vec![
        Line::from(vec![
            Span::raw(format!("{:<9}", "READ")).fg(READ).bold(),
            Span::raw(format!("{:>12}", rate(c.read_bps))),
            Span::raw("   total ").fg(DIM),
            Span::raw(fmt::bytes(c.read_total)).fg(READ_TOT),
        ]),
        Line::from(vec![
            Span::raw(format!("{:<9}", "WRITE")).fg(WRITE).bold(),
            Span::raw(format!("{:>12}", rate(c.write_bps))),
            Span::raw("   total ").fg(DIM),
            Span::raw(fmt::bytes(c.write_total)).fg(WRITE_TOT),
        ]),
    ];
    let rtts: Vec<f64> = c.conns.iter().filter_map(|k| k.rtt_ms).collect();
    let avg_rtt = if rtts.is_empty() { 0.0 } else { rtts.iter().sum::<f64>() / rtts.len() as f64 };
    // Queue sizes are per connection in the table below; this line must stay one row.
    let tcp =
        format!("{} conn{} · avg rtt {avg_rtt:.2} ms · ", c.conns.len(), if c.conns.len() == 1 { "" } else { "s" },);
    let retr = Span::raw(format!("{} retr/s", c.health.retrans_ps.map_or("…".into(), |r| format!("{r:.1}"))));
    lines.push(Line::from(vec![
        Span::raw(format!("{:<9}", "TCP")).fg(DIM),
        Span::raw(tcp),
        retrans_style(retr, c.health.retrans_ps),
    ]));
    // nfsd threads this client kept busy (in-flight RPCs; absent on kernels without them).
    if let Some(l) = &c.inflight {
        let mut v = vec![Span::raw(format!("{:<9}", "nfsd")).fg(DIM)];
        if l.peak == 0 {
            v.push(Span::raw("no requests in flight this interval").fg(DIM));
        } else {
            let oldest = Span::raw(fmt::latency(l.oldest_us));
            v.extend([
                Span::raw(format!("{} thread{} avg", fmt::avg(l.avg), if fmt::avg(l.avg) == "1.0" { "" } else { "s" })),
                Span::raw(format!(" · peak {} · oldest ", l.peak)).fg(DIM),
                if l.oldest_us >= super::SLOW_RPC_US { oldest.fg(WARN).bold() } else { oldest },
            ]);
        }
        lines.push(Line::from(v));
    }
    match c.best_info() {
        Some(ci) => {
            let mut nfs = format!("v4.{}", ci.minor_version.unwrap_or(0));
            if let Some(s) = &ci.status {
                nfs += &format!(" · {s}");
            }
            if let Some(r) = ci.seconds_since_renew() {
                nfs += &format!(" · renewed {r}s ago");
            }
            if let Some(cb) = ci.field("callback state") {
                nfs += &format!(" · callback {cb}");
            }
            lines.push(kv("NFS", nfs));
            if let Some(i) = ci.field("Implementation name") {
                lines.push(kv("Impl", i.to_string()));
            }
            let ids: Vec<String> =
                c.infos.iter().map(|i| format!("{} ({})", i.field("clientid").unwrap_or("?"), i.id)).collect();
            lines.push(kv("Client", ids.join(", ")));
        }
        None => lines.push(kv("NFS", Span::raw("no NFSv4 client record (NFSv3, or nfsd info unreadable)").italic())),
    }
    f.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: false }), area);
}

fn draw_chart(f: &mut Frame, area: Rect, stat: Option<&ClientStat>) {
    let peak = stat.map_or(0.0, |c| c.history.iter().map(|p| p.total()).fold(0.0, f64::max));
    let block = Block::bordered().title(Line::from(vec![
        Span::raw(" Throughput MiB/s: "),
        Span::raw("read").fg(READ),
        Span::raw(" / "),
        Span::raw("write").fg(WRITE),
        Span::raw(format!(" · peak {} ", fmt::rate(peak))).fg(DIM),
    ]));
    let Some(c) = stat.filter(|c| !c.history.is_empty()) else {
        f.render_widget(Paragraph::new("waiting for samples…").fg(DIM).block(block), area);
        return;
    };
    let now = c.history.back().map_or(0.0, |p| p.t);
    let read: Vec<(f64, f64)> = c.history.iter().map(|p| (p.t - now, p.read / MIB)).collect();
    let write: Vec<(f64, f64)> = c.history.iter().map(|p| (p.t - now, p.write / MIB)).collect();
    let span = (now - c.history.front().map_or(now, |p| p.t)).max(1.0);
    let ymax = read.iter().chain(write.iter()).map(|p| p.1).fold(0.0, f64::max).max(0.1) * 1.1;
    let chart = Chart::new(vec![
        Dataset::default().marker(Marker::Braille).graph_type(GraphType::Line).style(Style::new().fg(READ)).data(&read),
        Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(WRITE))
            .data(&write),
    ])
    .block(block)
    .x_axis(
        Axis::default()
            .bounds([-span, 0.0])
            .labels([Line::from(format!("-{span:.0}s")), Line::from("now")])
            .style(Style::new().fg(DIM)),
    )
    .y_axis(
        Axis::default()
            .bounds([0.0, ymax])
            .labels([Line::from("0"), Line::from(format!("{ymax:.1}"))])
            .style(Style::new().fg(DIM)),
    );
    f.render_widget(chart, area);
}

/// Most operations listed in the per-op panel; the rest are counted in its title.
const OP_ROWS: usize = 5;

/// Height of the per-op panel: none on kernels without in-flight RPCs, otherwise one row per
/// operation (at least one, to say none was seen) plus the border.
fn ops_height(c: &ClientStat) -> u16 {
    if c.inflight.is_none() && c.op_history.is_empty() {
        return 0;
    }
    c.op_history.len().clamp(1, OP_ROWS) as u16 + 2
}

/// nfsd threads this client's requests held, per operation: this interval's numbers on the left,
/// history on the right. All rows share one scale, so heights compare across operations.
///
/// These come from sampling the requests in flight ~10x per interval, so an op weighs in by how
/// many requests were running *and* how long each took: it shows where this client's share of
/// the server goes, not how many operations it sent. Fast ops (GETATTR, ACCESS, …) on a quiet
/// client may never be caught at all.
/// Smallest full height of the ops sparklines, in nfsd threads.
const MIN_OPS_SCALE: f64 = 1.0;

fn draw_ops(f: &mut Frame, area: Rect, c: &ClientStat) {
    const LABEL_W: u16 = 44;
    let mut block = Block::bordered().title(Line::from(vec![
        Span::raw(" nfsd threads by operation "),
        Span::raw("· time on the server, not op counts ").fg(DIM),
    ]));
    let inner = block.inner(area);
    let spark_w = inner.width.saturating_sub(LABEL_W + 1) as usize;
    let ops = c.busiest_ops(spark_w.max(1));
    if ops.len() > OP_ROWS {
        block = block.title(Line::from(format!(" +{} more ", ops.len() - OP_ROWS)).fg(DIM).right_aligned());
    }
    if ops.is_empty() {
        let msg = "no requests from this client caught in flight yet (fast or idle)";
        f.render_widget(Paragraph::new(msg).fg(DIM).block(block), area);
        return;
    }
    let ops = &ops[..ops.len().min(OP_ROWS)];
    let visible = |name: &str| -> Vec<f64> { c.op_history[name].iter().rev().take(spark_w).copied().collect() };
    // At least one thread tall, so a stray 0.1-thread sample on a quiet client stays a sliver
    // instead of a full bar; real load still fills the rows.
    let scale = ops.iter().flat_map(|(name, _)| visible(name)).fold(MIN_OPS_SCALE, f64::max);
    let now = c.inflight.as_ref();
    let total = now.map_or(0.0, |l| l.avg);
    let rows = Layout::vertical(vec![Constraint::Length(1); ops.len()]).split(inner);
    for ((name, _), row) in ops.iter().zip(rows.iter()) {
        let [label, _, spark] =
            Layout::horizontal([Constraint::Length(LABEL_W), Constraint::Length(1), Constraint::Fill(1)]).areas(*row);
        let color = super::op_color(name);
        let cur = now.and_then(|l| l.ops.iter().find(|o| o.name == *name));
        let avg = cur.map_or(0.0, |o| o.avg);
        let mut spans = vec![
            Span::raw(format!("{:<11}", super::fit(name, 10))).fg(color).bold(),
            Span::raw(format!("{:>5}", fmt::avg(avg))).fg(if avg > 0.0 { color } else { DIM }),
            Span::raw(" thr").fg(DIM),
            Span::raw(format!(
                "{:>6}",
                if avg > 0.0 && total > 0.0 { fmt::percent(avg / total * 100.0) } else { "-".into() }
            ))
            .fg(if avg > 0.0 { Color::Reset } else { DIM }),
        ];
        if let Some(o) = cur {
            let age = Span::raw(format!("{:>9}", fmt::latency(o.oldest_us)));
            spans.push(Span::raw("  oldest").fg(DIM));
            spans.push(if o.oldest_us >= super::SLOW_RPC_US { age.fg(WARN).bold() } else { age });
        }
        f.render_widget(Paragraph::new(Line::from(spans)), label);
        super::sparkline(f.buffer_mut(), spark, &visible(name), scale, color);
    }
    // Border caption: what the graphs cover (op samples line up with the rate history).
    let n = ops.iter().map(|(name, _)| c.op_history[*name].len().min(spark_w)).max().unwrap_or(0);
    let window = match (c.history.back(), c.history.len().checked_sub(n)) {
        (Some(newest), Some(i)) if n >= 2 => format!("last {}", fmt::duration(newest.t - c.history[i].t)),
        _ => "newest at right".into(),
    };
    block = block.title_bottom(
        Line::from(format!(" ◀ {window} · full height = {} threads ", fmt::avg(scale))).fg(DIM).right_aligned(),
    );
    f.render_widget(block, area);
}

fn draw_conns(f: &mut Frame, area: Rect, stat: Option<&ClientStat>) {
    let mut conns: Vec<&crate::parse::ss::Sock> = stat.map(|c| c.conns.iter().collect()).unwrap_or_default();
    conns.sort_by_key(|k| std::cmp::Reverse(k.bytes_acked + k.bytes_received));
    // Active TCP health flags, explained above the table.
    let flags = stat.map(ClientStat::flags).unwrap_or_default();
    let block = Block::bordered();
    let inner = block.inner(area);
    let [legend, table_area] =
        Layout::vertical([Constraint::Length(flags.len() as u16), Constraint::Fill(1)]).areas(inner);
    let legend_lines: Vec<Line> = flags
        .iter()
        .map(|f| {
            Line::from(vec![
                Span::raw(format!(" {} ", f.letter())).fg(super::flag_color(*f)).bold(),
                Span::raw(f.meaning()),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(legend_lines), legend);

    let health = |port: u16| stat.and_then(|c| c.conn_health.get(&port)).copied().unwrap_or_default();
    let fits = table_area.height.saturating_sub(1) as usize;
    let hdr = ["PORT", "READ TOT", "WRITE TOT", "RTT", "RETR/s", "RECV-Q", "SEND-Q", "RWND%", "CWND", "IDLE"];
    let right = |s: String| Cell::from(Line::from(s).alignment(Alignment::Right));
    let rows: Vec<Row> = conns
        .iter()
        .map(|k| {
            let idle = match (k.last_snd_ms, k.last_rcv_ms) {
                (Some(a), Some(b)) => format!("{:.1}s", a.min(b) as f64 / 1000.0),
                _ => "-".into(),
            };
            let h = health(k.port);
            let q = |v: u64| if v == 0 { "0".into() } else { fmt::bytes(v) };
            // Health columns: dim when quiet, coloured like their flag once past its threshold.
            let gauge = |text: String, quiet: bool, over: bool, flag: Flag| {
                let c = right(text);
                if over {
                    c.fg(super::flag_color(flag)).bold()
                } else if quiet {
                    c.fg(DIM)
                } else {
                    c
                }
            };
            let retr = h.retrans_ps.map_or("-".into(), |r| format!("{r:.1}"));
            Row::new(vec![
                right(k.port.to_string()),
                right(fmt::bytes(k.bytes_acked)).fg(READ_TOT),
                right(fmt::bytes(k.bytes_received)).fg(WRITE_TOT),
                right(k.rtt_ms.map_or("-".into(), |r| format!("{r:.2} ms"))),
                Cell::from(retrans_style(Line::from(retr).alignment(Alignment::Right), h.retrans_ps)),
                gauge(q(k.recv_q), k.recv_q == 0, k.recv_q >= FLAG_RECV_Q, Flag::RecvQ),
                gauge(q(k.send_q), k.send_q == 0, k.send_q >= FLAG_SEND_Q, Flag::SendQ),
                gauge(
                    h.rwnd_pct.map_or("-".into(), |p| format!("{p:.0}%")),
                    h.rwnd_pct.is_none_or(|p| p < 1.0),
                    h.rwnd_pct.is_some_and(|p| p >= FLAG_LIMITED_PCT),
                    Flag::Rwnd,
                ),
                right(k.cwnd.map_or("-".into(), |c| c.to_string())),
                right(idle),
            ])
        })
        .collect();
    let widths = [6, 11, 11, 10, 7, 10, 10, 6, 6, 8].map(Constraint::Length);
    let table = Table::new(rows, widths).header(
        Row::new(hdr.map(|h| {
            let color = match h {
                "READ TOT" => READ_TOT,
                "WRITE TOT" => WRITE_TOT,
                "RETR/s" => WARN,
                _ => Color::Reset,
            };
            Cell::from(Line::from(h).alignment(Alignment::Right).fg(color))
        }))
        .style(Style::new().bold().bg(Color::Indexed(236))),
    );
    let title = if conns.len() > fits {
        format!(" TCP connections ({}, busiest {fits} shown) ", conns.len())
    } else {
        format!(" TCP connections ({}) ", conns.len())
    };
    f.render_widget(block.title(title), area);
    f.render_widget(table, table_area);
}

fn file_line(of: &OpenFile, path: Option<&PathState>) -> Line<'static> {
    let kinds: Vec<&str> = of.kinds.iter().map(String::as_str).collect();
    let name =
        if deleg_only(of) { Span::raw(of.name.clone()).fg(Color::Gray) } else { Span::raw(of.name.clone()).bold() };
    let mut spans = vec![
        if of.read { Span::raw("r").fg(READ).bold() } else { Span::raw("-").fg(DIM) },
        if of.write { Span::raw("w").fg(WRITE).bold() } else { Span::raw("-").fg(DIM) },
        Span::raw(format!(" {:<12} ", kinds.join(","))).fg(DIM),
        name,
    ];
    match path {
        // The kernel had no name for it: show what it is called.
        Some(PathState::Found(p)) if of.dir == ANON_DIR => {
            let rel = std::path::Path::new(p).strip_prefix(&of.export).map_or(p.clone(), |r| r.display().to_string());
            spans.push(Span::raw(format!("  = {rel}")).cyan())
        }
        Some(PathState::Found(p)) => spans.push(Span::raw(format!("  in {}", rel_dir(&of.export, p))).cyan()),
        Some(PathState::Pending) => spans.push(Span::raw("  locating…").yellow()),
        Some(PathState::Missing) => spans.push(Span::raw("  not found (closed, renamed or deleted?)").red()),
        None => {}
    }
    Line::from(spans)
}

/// Directory of `path` relative to the export it is under (`./` for the export root).
fn rel_dir(export: &str, path: &str) -> String {
    let p = std::path::Path::new(path);
    let dir = p.parent().unwrap_or(p);
    match dir.strip_prefix(export) {
        Ok(r) if r.as_os_str().is_empty() => "./".into(),
        Ok(r) => format!("{}/", r.display()),
        Err(_) => format!("{}/", dir.display()),
    }
}

/// A file the client merely holds a cached delegation for (not currently opened).
pub fn deleg_only(f: &OpenFile) -> bool {
    f.deleg_only()
}

pub fn build_tree(
    of: &OpenFiles,
    d: &Detail,
    matcher: &mut nucleo_matcher::Matcher,
) -> (Vec<TreeItem<'static, String>>, usize) {
    let (filter, hide_deleg) = (d.filter.as_str(), d.hide_deleg);
    let pattern = (!filter.is_empty()).then(|| Pattern::parse(filter, CaseMatching::Ignore, Normalization::Smart));
    let mut buf = Vec::new();
    // export -> dir -> files
    let mut groups: BTreeMap<&str, BTreeMap<&str, Vec<&OpenFile>>> = BTreeMap::new();
    let mut shown = 0;
    for file in &of.files {
        if hide_deleg && deleg_only(file) {
            continue;
        }
        if let Some(p) = &pattern {
            let hay = match d.path_of(file) {
                Some(PathState::Found(p)) => p.clone(),
                _ => format!("{}/{}", file.export, file.path()),
            };
            if p.score(Utf32Str::new(&hay, &mut buf), matcher).is_none() {
                continue;
            }
        }
        shown += 1;
        groups.entry(&file.export).or_default().entry(&file.dir).or_default().push(file);
    }
    let items = groups
        .into_iter()
        .map(|(export, dirs)| {
            let n_files: usize = dirs.values().map(Vec::len).sum();
            let mut dirs: Vec<(&str, Vec<&OpenFile>)> = dirs.into_iter().collect();
            dirs.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));
            let children = dirs
                .into_iter()
                .map(|(dir, files)| {
                    let writers = files.iter().filter(|f| f.write).count();
                    let mut label = vec![
                        Span::raw(dir_label(dir)).bold(),
                        Span::raw(format!("  {} file{}", files.len(), if files.len() == 1 { "" } else { "s" })).fg(DIM),
                    ];
                    if writers > 0 {
                        label.push(Span::raw(format!(" · {writers} writable")).fg(WRITE));
                    }
                    let leaves =
                        files.iter().map(|f| TreeItem::new_leaf(f.leaf.clone(), file_line(f, d.path_of(f)))).collect();
                    TreeItem::new(dir.to_string(), Line::from(label), leaves).expect("unique leaf ids per dir")
                })
                .collect();
            let label =
                Line::from(vec![Span::raw(export.to_string()).bold(), Span::raw(format!("  {n_files} files")).fg(DIM)]);
            TreeItem::new(export.to_string(), label, children).expect("unique dirs per export")
        })
        .collect();
    (items, shown)
}

fn draw_files(
    f: &mut Frame,
    area: Rect,
    d: &mut Detail,
    matcher: &mut nucleo_matcher::Matcher,
    throbber: &throbber_widgets_tui::ThrobberState,
) {
    let base = Block::bordered().title(" Open files ");
    match &d.files {
        None => {
            let inner = base.inner(area);
            f.render_widget(base, area);
            let t = Throbber::default().label("reading open state…").style(Style::new().fg(DIM));
            f.render_widget(Paragraph::new(t.to_line(throbber)), inner);
        }
        Some(Err(e)) => {
            f.render_widget(
                Paragraph::new(format!("Could not read open state: {e}")).red().wrap(Wrap { trim: false }).block(base),
                area,
            );
        }
        Some(Ok(of)) if of.clients.is_empty() => {
            f.render_widget(
                Paragraph::new(
                    "No NFSv4 client record for this address. NFSv3 clients don't expose per-client open state.",
                )
                .fg(DIM)
                .wrap(Wrap { trim: false })
                .block(base),
                area,
            );
        }
        Some(Ok(of)) => {
            let (items, shown) = build_tree(of, d, matcher);
            let mut title = vec![Span::raw(format!(
                " Open files: {} under {} dir names · {} writable · states:",
                of.files.len(),
                of.dir_count(),
                of.writers()
            ))];
            for (k, n) in &of.kind_counts {
                title.push(Span::raw(format!(" {k} {n}")));
            }
            title.push(Span::raw(" "));
            if d.hide_deleg {
                title.push(Span::raw(format!("· delegation-only hidden ({shown} shown) ")).cyan());
            }
            if !d.filter.is_empty() {
                title.push(Span::raw(format!("· filter: {} ({shown} match) ", d.filter)).yellow());
            }
            let mut block = Block::bordered().title(Line::from(title));
            if let Some(e) = of.errors.first() {
                block = block.title_bottom(Line::from(format!(" ⚠ {e} ")).red());
            }
            if let Some(job) = &d.find {
                let t = Throbber::default().label(format!(
                    "locating {} file{} under {}: {} dirs searched (Esc stops) ",
                    job.files,
                    if job.files == 1 { "" } else { "s" },
                    job.export,
                    job.dirs
                ));
                block = block.title_bottom(t.to_line(throbber).yellow().right_aligned());
            } else if let Some(n) = &d.find_note {
                block = block.title_bottom(Line::from(format!(" {n} ")).fg(DIM).right_aligned());
            }
            if items.is_empty() {
                let msg = if of.files.is_empty() {
                    "No open files."
                } else if !d.filter.is_empty() {
                    "Nothing matches the filter."
                } else {
                    "Only cached delegations; nothing currently opened (press d to show them)."
                };
                f.render_widget(Paragraph::new(msg).fg(DIM).block(block), area);
                return;
            }
            if d.tree_state.selected().is_empty() {
                d.tree_state.select_first();
            }
            let tree = Tree::new(&items)
                .expect("unique exports")
                .block(block)
                .highlight_style(Style::new().reversed())
                .node_closed_symbol("▸ ")
                .node_open_symbol("▾ ")
                .node_no_children_symbol("  ");
            f.render_stateful_widget(tree, area, &mut d.tree_state);
        }
    }
}

/// Retransmits: dim when none, orange when any, bold orange from the loss-flag threshold.
fn retrans_style<'a, T: Stylize<'a, T>>(t: T, ps: Option<f64>) -> T {
    match ps {
        Some(r) if r >= FLAG_RETRANS_PS => t.fg(WARN).bold(),
        Some(r) if r > 0.0 => t.fg(WARN),
        _ => t.fg(DIM),
    }
}
