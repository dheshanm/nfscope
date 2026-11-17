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

use super::{DIM, READ, WRITE, footer as draw_footer, status_lines};
use crate::app::{App, Detail, View};
use crate::files::{OpenFile, OpenFiles};
use crate::fmt;
use crate::model::ClientStat;

const MIB: f64 = 1024.0 * 1024.0;

pub fn draw(f: &mut Frame, area: Rect, app: &mut App) {
    let status = status_lines(app);
    let View::Detail(d) = &mut app.view else { return };
    let live = app.model.clients.get(&d.ip);
    let stat = live.or(d.last.as_ref());

    let n_conns = stat.map_or(0, |c| c.conns.len()).clamp(1, 4) as u16;
    let [title, banner, stats, conns, files, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(status.len() as u16),
        Constraint::Length(9),
        Constraint::Length(n_conns + 3),
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
            Span::raw(format!("   total {}", fmt::bytes(c.read_total))).fg(DIM),
        ]),
        Line::from(vec![
            Span::raw(format!("{:<9}", "WRITE")).fg(WRITE).bold(),
            Span::raw(format!("{:>12}", rate(c.write_bps))),
            Span::raw(format!("   total {}", fmt::bytes(c.write_total))).fg(DIM),
        ]),
    ];
    let rtts: Vec<f64> = c.conns.iter().filter_map(|k| k.rtt_ms).collect();
    let avg_rtt = if rtts.is_empty() { 0.0 } else { rtts.iter().sum::<f64>() / rtts.len() as f64 };
    let retrans: u64 = c.conns.iter().map(|k| k.retrans).sum();
    lines.push(kv(
        "TCP",
        format!(
            "{} conn{} · avg rtt {avg_rtt:.2} ms · {retrans} retrans",
            c.conns.len(),
            if c.conns.len() == 1 { "" } else { "s" }
        ),
    ));
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

fn draw_conns(f: &mut Frame, area: Rect, stat: Option<&ClientStat>) {
    let mut conns: Vec<&crate::parse::ss::Sock> = stat.map(|c| c.conns.iter().collect()).unwrap_or_default();
    conns.sort_by_key(|k| std::cmp::Reverse(k.bytes_acked + k.bytes_received));
    let fits = area.height.saturating_sub(3) as usize;
    let hdr = ["PORT", "READ TOT", "WRITE TOT", "RTT", "RETRANS", "CWND", "IDLE"];
    let right = |s: String| Cell::from(Line::from(s).alignment(Alignment::Right));
    let rows: Vec<Row> = conns
        .iter()
        .map(|k| {
            let idle = match (k.last_snd_ms, k.last_rcv_ms) {
                (Some(a), Some(b)) => format!("{:.1}s", a.min(b) as f64 / 1000.0),
                _ => "-".into(),
            };
            Row::new(vec![
                right(k.port.to_string()),
                right(fmt::bytes(k.bytes_acked)).fg(READ),
                right(fmt::bytes(k.bytes_received)).fg(WRITE),
                right(k.rtt_ms.map_or("-".into(), |r| format!("{r:.2} ms"))),
                right(k.retrans.to_string()),
                right(k.cwnd.map_or("-".into(), |c| c.to_string())),
                right(idle),
            ])
        })
        .collect();
    let widths = [6, 11, 11, 10, 8, 6, 8].map(Constraint::Length);
    let table = Table::new(rows, widths)
        .header(
            Row::new(hdr.map(|h| {
                let color = match h {
                    "READ TOT" => READ,
                    "WRITE TOT" => WRITE,
                    _ => Color::Reset,
                };
                Cell::from(Line::from(h).alignment(Alignment::Right).fg(color))
            }))
            .style(Style::new().bold().bg(Color::Indexed(236))),
        )
        .block(Block::bordered().title(if conns.len() > fits {
            format!(" TCP connections ({}, busiest {fits} shown) ", conns.len())
        } else {
            format!(" TCP connections ({}) ", conns.len())
        }));
    f.render_widget(table, area);
}

fn file_line(of: &OpenFile) -> Line<'static> {
    let kinds: Vec<&str> = of.kinds.iter().map(String::as_str).collect();
    let name =
        if deleg_only(of) { Span::raw(of.name.clone()).fg(Color::Gray) } else { Span::raw(of.name.clone()).bold() };
    Line::from(vec![
        if of.read { Span::raw("r").fg(READ).bold() } else { Span::raw("-").fg(DIM) },
        if of.write { Span::raw("w").fg(WRITE).bold() } else { Span::raw("-").fg(DIM) },
        Span::raw(format!(" {:<12} ", kinds.join(","))).fg(DIM),
        name,
    ])
}

/// A file the client merely holds a cached delegation for (not currently opened).
pub fn deleg_only(f: &OpenFile) -> bool {
    f.kinds.len() == 1 && f.kinds.contains("deleg")
}

pub fn build_tree(
    of: &OpenFiles,
    filter: &str,
    hide_deleg: bool,
    matcher: &mut nucleo_matcher::Matcher,
) -> (Vec<TreeItem<'static, String>>, usize) {
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
            let hay = format!("{}/{}", file.export, file.path());
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
                        Span::raw(format!("{dir}/")).bold(),
                        Span::raw(format!("  {} file{}", files.len(), if files.len() == 1 { "" } else { "s" })).fg(DIM),
                    ];
                    if writers > 0 {
                        label.push(Span::raw(format!(" · {writers} writable")).fg(WRITE));
                    }
                    let leaves = files.iter().map(|f| TreeItem::new_leaf(f.name.clone(), file_line(f))).collect();
                    TreeItem::new(dir.to_string(), Line::from(label), leaves).expect("unique file names per dir")
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
            let (items, shown) = build_tree(of, &d.filter, d.hide_deleg, matcher);
            let mut title = vec![Span::raw(format!(
                " Open files: {} in {} dirs · {} writable · states:",
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
