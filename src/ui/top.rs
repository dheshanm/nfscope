use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table};
use throbber_widgets_tui::Throbber;

use crossterm::event::KeyCode;
use ratatui::layout::Flex;

use super::{DIM, READ, READ_TOT, SLOW_RPC_US, WARN, WRITE, WRITE_TOT, footer as draw_footer, legend, status_lines};
use crate::app::{App, SortKey};
use crate::fmt;

const TREND_W: usize = 12;

pub fn draw(f: &mut Frame, area: Rect, app: &mut App) {
    let status = status_lines(app);
    let [header, banner, table, footer] = Layout::vertical([
        Constraint::Length(6),
        Constraint::Length(status.len() as u16),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_header(f, header, app);
    f.render_widget(Paragraph::new(status), banner);
    draw_table(f, table, app);

    if app.filter_editing {
        let line = Line::from(vec![Span::raw(" filter: ").yellow(), Span::raw(format!("{}█", app.filter))]);
        f.render_widget(Paragraph::new(line), footer);
    } else {
        use KeyCode::Char;
        draw_footer(
            f,
            footer,
            &[
                ("q", "quit", Some(Char('q'))),
                ("↵", "details", Some(KeyCode::Enter)),
                ("s/S", "sort", Some(Char('s'))),
                ("/", "filter", Some(Char('/'))),
                ("p", "pause", Some(Char('p'))),
                ("+", "slower", Some(Char('+'))),
                ("-", "faster", Some(Char('-'))),
                ("l", "log", Some(Char('l'))),
                ("o", "nfsd", Some(Char('o'))),
                ("i", "in-flight", Some(Char('i'))),
                ("?", "help", Some(Char('?'))),
            ],
            &mut app.hit,
        );
    }
}

/// Average nfsd threads the client held over the interval: dim `·` when none, bold from one
/// whole thread, orange when one of its requests has been running for a second or more.
fn threads_cell(c: &crate::model::ClientStat) -> Cell<'static> {
    let Some(l) = &c.inflight else { return Cell::from("") };
    let style = match l {
        l if l.oldest_us >= SLOW_RPC_US => Style::new().fg(WARN).bold(),
        l if l.avg >= 1.0 => Style::new().bold(),
        _ => Style::new(),
    };
    let text = if l.avg <= 0.0 { Line::from("·").fg(DIM) } else { Line::from(fmt::avg(l.avg)).style(style) };
    Cell::from(text.alignment(Alignment::Right))
}

/// TCP health flags for a client (`Q`, `L`, …), or a dim `·` when there are none.
pub fn flags_line(c: &crate::model::ClientStat) -> Line<'static> {
    let flags = c.flags();
    if flags.is_empty() {
        return Line::from(" ·").fg(DIM);
    }
    let mut spans = vec![Span::raw(" ")];
    spans.extend(flags.into_iter().map(|f| Span::raw(f.letter().to_string()).fg(super::flag_color(f)).bold()));
    Line::from(spans)
}

/// `nfsd 14/128 busy ↑41  4.1k op/s`: server-wide health at a glance (average busy threads over
/// the interval, ↑ the peak snapshot).
fn server_line(app: &App) -> Line<'static> {
    let m = &app.model;
    if m.server_error.is_some() {
        // Without root the nfsd banner already explains what is missing.
        return if m.nfsd_error.is_some() { Line::default() } else { Line::from("nfsd stats n/a").fg(DIM) };
    }
    let mut spans = vec![Span::raw("nfsd ").bold()];
    match (
        m.server.as_ref().and_then(|r| r.thread_load),
        m.server.as_ref().and_then(|r| r.threads).or(m.server_threads),
    ) {
        (Some(t), _) => {
            let color = super::load_color(&t);
            spans.push(Span::raw(format!("{}/{} busy", t.avg_busy.round() as u64, t.total)).fg(color));
            if t.samples > 1 {
                spans.push(Span::raw(format!(" ↑{}", t.peak_busy)).fg(color));
            }
            spans.push(Span::raw("  "));
        }
        (None, Some(t)) => spans.push(Span::raw(format!("{t} thr  "))),
        (None, None) => {}
    }
    match &m.server {
        Some(r) => spans.push(Span::raw(format!("{} op/s", fmt::count(r.calls_ps)))),
        None => spans.push(Span::raw("…").fg(DIM)),
    }
    Line::from(spans)
}

/// Header: summary on the left, server-wide READ / WRITE history on the right.
///
/// The two sparklines share one scale so their heights compare directly; newest sample is at
/// the right edge, and the border shows the time window and the scale.
fn draw_header(f: &mut Frame, area: Rect, app: &mut App) {
    let block = Block::bordered()
        .title(Line::from(vec![" nfscope ".bold(), format!("· {} ", app.server_name).into()]))
        .title(Line::from(legend()).right_aligned());
    let inner = block.inner(area);
    let [text, graphs] = Layout::horizontal([Constraint::Length(34), Constraint::Fill(1)]).spacing(2).areas(inner);

    // ---- left: summary
    let active = app.model.clients.values().filter(|c| c.total_bps() > 0.0).count();
    let mut l2 = vec![Span::raw(format!("Interval {}s  ", app.interval.as_secs()))];
    if app.paused {
        l2.push(" PAUSED ".black().on_yellow());
    } else if let Some(w) = &app.model.last_wall {
        l2.push(Span::raw(format!("updated {w}")).fg(DIM));
    }
    let total = if app.model.has_rates() {
        let (r, w) = app.model.total_rates();
        Line::from(vec![Span::raw("TOTAL ").bold(), Span::raw(fmt::rate(r + w)).bold()])
    } else {
        let t = Throbber::default();
        Line::from(vec![t.to_symbol_span(&app.throbber), Span::raw(" collecting first interval…").fg(DIM)])
    };
    let lines = vec![
        Line::from(vec![
            Span::raw(format!("Clients {} ", app.model.clients.len())),
            Span::raw(format!("({active} active)")).fg(DIM),
        ]),
        Line::from(l2),
        server_line(app),
        total,
    ];
    // The nfsd health line opens the server overlay when clicked.
    let health = Rect::new(text.x, text.y + 2, lines[2].width() as u16, 1).intersection(text);
    if !health.is_empty() {
        app.hit.buttons.push((health, KeyCode::Char('o')));
    }
    f.render_widget(Paragraph::new(lines), text);

    // ---- right: READ above WRITE, each 2 rows tall, label + current rate + sparkline
    const LABEL_W: u16 = 19;
    let [read_row, write_row] = Layout::vertical([Constraint::Length(2), Constraint::Length(2)]).areas(graphs);
    let spark_w = graphs.width.saturating_sub(LABEL_W) as usize;
    let hist = &app.model.total;
    let visible: Vec<_> = hist.iter().rev().take(spark_w).collect(); // newest first
    let scale = visible.iter().map(|p| p.read.max(p.write)).fold(0.0, f64::max);
    let (cur_r, cur_w) = app.model.total_rates();
    for (row, name, color, cur, pick) in [
        (read_row, "READ", READ, cur_r, (|p: &crate::model::Point| p.read) as fn(&crate::model::Point) -> f64),
        (write_row, "WRITE", WRITE, cur_w, |p: &crate::model::Point| p.write),
    ] {
        let [label, spark] = Layout::horizontal([Constraint::Length(LABEL_W), Constraint::Fill(1)]).areas(row);
        let rate = if app.model.has_rates() { fmt::rate(cur) } else { "…".into() };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(format!("{name:<6}")).fg(color).bold(),
                Span::raw(format!("{rate:>12}")).fg(color),
            ])),
            label,
        );
        let data: Vec<f64> = visible.iter().map(|p| pick(p)).collect();
        super::sparkline(f.buffer_mut(), spark, &data, scale, color);
    }

    // ---- border caption: what the graph covers
    let mut block = block;
    if let (Some(newest), Some(oldest)) = (visible.first(), visible.last()) {
        let window = if visible.len() >= 2 {
            format!("last {}", fmt::duration(newest.t - oldest.t))
        } else {
            "newest at right".to_string()
        };
        block = block.title_bottom(
            Line::from(vec![
                Span::raw(format!(" ◀ {window} · ")).fg(DIM),
                Span::raw(format!("full height = {} ", fmt::rate(scale))).fg(DIM),
            ])
            .right_aligned(),
        );
    }
    f.render_widget(block, area);
}

fn draw_table(f: &mut Frame, area: Rect, app: &mut App) {
    let rows = app.rows();
    app.sync_selection(&rows);
    let has_rates = app.model.has_rates();

    // THREADS (nfsd threads held, from in-flight RPCs) only when the kernel reports them.
    let threads = app.model.inflight.is_some();
    let ip_w = rows.iter().map(|c| c.ip.to_string().len()).max().unwrap_or(0).clamp(9, 39) as u16;
    // (name, sort key, width (0 = fill), right-aligned)
    let mut cols: Vec<(&str, Option<SortKey>, u16, bool)> = vec![
        ("CLIENT", Some(SortKey::Ip), ip_w, false),
        ("READ/s", Some(SortKey::Read), 12, true),
        ("WRITE/s", Some(SortKey::Write), 12, true),
        ("TOTAL/s", Some(SortKey::Total), 12, true),
        ("TREND", None, TREND_W as u16, false),
        ("READ TOT", Some(SortKey::ReadTot), 10, true),
        ("WRITE TOT", Some(SortKey::WriteTot), 10, true),
        ("CONN", Some(SortKey::Conns), 5, true),
        ("FLAGS", None, 6, false),
        ("HOST  [NFSv4 client name]", Some(SortKey::Host), 0, false),
    ];
    const THREADS_AT: usize = 5;
    if threads {
        cols.insert(THREADS_AT, ("THREADS", Some(SortKey::Threads), 9, true));
    }
    let header = Row::new(cols.iter().map(|(name, key, _, right)| {
        let arrow = match key {
            Some(k) if *k == app.sort => {
                if app.sort_desc {
                    " ▼"
                } else {
                    " ▲"
                }
            }
            _ => "",
        };
        let text = format!("{name}{arrow}");
        let mut style =
            if key.is_some_and(|k| k == app.sort) { Style::new().bold().underlined() } else { Style::new().bold() };
        match key {
            Some(SortKey::Read) => style = style.fg(READ),
            Some(SortKey::Write) => style = style.fg(WRITE),
            Some(SortKey::ReadTot) => style = style.fg(READ_TOT),
            Some(SortKey::WriteTot) => style = style.fg(WRITE_TOT),
            _ => {}
        }
        let line = Line::from(text).style(style);
        Cell::from(if *right { line.alignment(Alignment::Right) } else { line })
    }))
    .style(Style::new().bg(Color::Indexed(236)));

    let right = |s: String, st: Style| Cell::from(Line::from(s).alignment(Alignment::Right).style(st));
    let body: Vec<Row> = rows
        .iter()
        .map(|c| {
            let idle = c.total_bps() <= 0.0;
            let num = |v: f64, color: Color| {
                if !has_rates {
                    right("…".into(), Style::new().fg(DIM))
                } else if v <= 0.0 {
                    right(fmt::rate(0.0), Style::new().fg(DIM))
                } else {
                    right(fmt::rate(v), Style::new().fg(color))
                }
            };
            let total_style = if idle { Style::new().fg(DIM) } else { Style::new().bold() };
            let mut host = vec![Span::raw(app.host_label(&c.ip).to_string())];
            if let Some(n) = c.nfs_name() {
                host.push(Span::raw(format!("  [{n}]")).fg(DIM));
            } else {
                host.push(Span::raw("  [no NFSv4 record]").fg(DIM).italic());
            }
            let mut cells = vec![
                Cell::from(c.ip.to_string()),
                num(c.read_bps, READ),
                num(c.write_bps, WRITE),
                if has_rates { right(fmt::rate(c.total_bps()), total_style) } else { right("…".into(), total_style) },
                Cell::from(fmt::trend(&c.history, TREND_W)).style(Style::new().fg(Color::Gray)),
                right(fmt::bytes(c.read_total), Style::new().fg(READ_TOT)),
                right(fmt::bytes(c.write_total), Style::new().fg(WRITE_TOT)),
                right(c.conns.len().to_string(), Style::new()),
                Cell::from(flags_line(c)),
                Cell::from(Line::from(host)),
            ];
            if threads {
                cells.insert(THREADS_AT, threads_cell(c));
            }
            Row::new(cells)
        })
        .collect();

    let widths: Vec<Constraint> =
        cols.iter().map(|(_, _, w, _)| if *w == 0 { Constraint::Fill(1) } else { Constraint::Length(*w) }).collect();

    let mut title = vec![Span::raw(format!(" Clients ({}) ", rows.len()))];
    if !app.filter.is_empty() {
        title.push(Span::raw(format!("filter: {} ", app.filter)).yellow());
    }
    // Record clickable header cells / body rows, laid out exactly as Table does it.
    let block = Block::bordered().title(Line::from(title));
    let inner = block.inner(area);
    let col_rects = Layout::horizontal(widths.clone()).flex(Flex::Start).spacing(1).split(Rect::new(
        inner.x,
        inner.y,
        inner.width,
        1.min(inner.height),
    ));
    for ((_, key, _, _), r) in cols.iter().zip(col_rects.iter()) {
        if let Some(k) = key {
            app.hit.sort_headers.push((*r, *k));
        }
    }
    app.hit.table_body = Rect::new(inner.x, inner.y + 1, inner.width, inner.height.saturating_sub(1));

    let table = Table::new(body, widths)
        .header(header)
        .block(block)
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .column_spacing(1);
    f.render_stateful_widget(table, area, &mut app.table_state);

    if rows.is_empty() {
        let msg = if app.model.clients.is_empty() {
            "No established NFS connections."
        } else {
            "No clients match the filter."
        };
        let [_, mid, _] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1), Constraint::Fill(1)]).areas(area);
        f.render_widget(Paragraph::new(msg).fg(DIM).alignment(Alignment::Center), mid);
    }
}
