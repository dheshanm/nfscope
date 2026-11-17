use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Sparkline, Table};
use throbber_widgets_tui::Throbber;

use crossterm::event::KeyCode;
use ratatui::layout::Flex;

use super::{DIM, READ, WRITE, footer as draw_footer, status_lines};
use crate::app::{App, SortKey};
use crate::fmt;

const TREND_W: usize = 12;

pub fn draw(f: &mut Frame, area: Rect, app: &mut App) {
    let status = status_lines(app);
    let [header, banner, table, footer] = Layout::vertical([
        Constraint::Length(4),
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
                ("?", "help", Some(Char('?'))),
            ],
            &mut app.hit,
        );
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &mut App) {
    let block = Block::bordered()
        .title(Line::from(vec![" nfscope ".bold(), format!("· {} ", app.server_name).into()]))
        .title(Line::from(" READ = server→client · WRITE = client→server ").fg(DIM).right_aligned());
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [text, spark] = Layout::horizontal([Constraint::Min(50), Constraint::Percentage(40)]).areas(inner);

    let active = app.model.clients.values().filter(|c| c.total_bps() > 0.0).count();
    let mut l1 = vec![
        Span::raw(format!("Clients {} ", app.model.clients.len())),
        Span::raw(format!("({active} active)   ")).fg(DIM),
        Span::raw(format!("Interval {}s   ", app.interval.as_secs())),
    ];
    if app.paused {
        l1.push(" PAUSED ".black().on_yellow());
        l1.push(Span::raw(" "));
    }
    if let Some(w) = &app.model.last_wall {
        l1.push(Span::raw(format!("updated {w}")).fg(DIM));
    }
    let l2 = if app.model.has_rates() {
        let (r, w) = app.model.total_rates();
        Line::from(vec![
            Span::raw("READ ").fg(READ).bold(),
            Span::raw(format!("{:<14}", fmt::rate(r))),
            Span::raw("WRITE ").fg(WRITE).bold(),
            Span::raw(format!("{:<14}", fmt::rate(w))),
            Span::raw("TOTAL ").bold(),
            Span::raw(fmt::rate(r + w)),
        ])
    } else {
        let t = Throbber::default().label("collecting first interval…");
        Line::from(vec![t.to_symbol_span(&app.throbber), Span::raw(" collecting first interval…").fg(DIM)])
    };
    f.render_widget(Paragraph::new(vec![Line::from(l1), l2]), text);

    let n = spark.width as usize;
    let data: Vec<u64> =
        app.model.total.iter().skip(app.model.total.len().saturating_sub(n)).map(|p| p.total() as u64).collect();
    f.render_widget(Sparkline::default().data(&data).style(Style::new().fg(Color::Green)), spark);
}

fn draw_table(f: &mut Frame, area: Rect, app: &mut App) {
    let rows = app.rows();
    app.sync_selection(&rows);
    let has_rates = app.model.has_rates();

    let ip_w = rows.iter().map(|c| c.ip.to_string().len()).max().unwrap_or(0).clamp(9, 39) as u16;
    let cols: [(&str, Option<SortKey>, u16); 9] = [
        ("CLIENT", Some(SortKey::Ip), ip_w),
        ("READ/s", Some(SortKey::Read), 12),
        ("WRITE/s", Some(SortKey::Write), 12),
        ("TOTAL/s", Some(SortKey::Total), 12),
        ("TREND", None, TREND_W as u16),
        ("READ TOT", Some(SortKey::ReadTot), 10),
        ("WRITE TOT", Some(SortKey::WriteTot), 10),
        ("CONN", Some(SortKey::Conns), 5),
        ("HOST  [NFSv4 client name]", Some(SortKey::Host), 0),
    ];
    let header = Row::new(cols.iter().enumerate().map(|(i, (name, key, _))| {
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
        let style =
            if key.is_some_and(|k| k == app.sort) { Style::new().bold().underlined() } else { Style::new().bold() };
        let line = Line::from(text).style(style);
        Cell::from(if (1..8).contains(&i) && i != 4 { line.alignment(Alignment::Right) } else { line })
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
            Row::new(vec![
                Cell::from(c.ip.to_string()),
                num(c.read_bps, READ),
                num(c.write_bps, WRITE),
                if has_rates { right(fmt::rate(c.total_bps()), total_style) } else { right("…".into(), total_style) },
                Cell::from(fmt::trend(&c.history, TREND_W)).style(Style::new().fg(Color::Green)),
                right(fmt::bytes(c.read_total), Style::new()),
                right(fmt::bytes(c.write_total), Style::new()),
                right(c.conns.len().to_string(), Style::new()),
                Cell::from(Line::from(host)),
            ])
        })
        .collect();

    let widths: Vec<Constraint> =
        cols.iter().map(|(_, _, w)| if *w == 0 { Constraint::Fill(1) } else { Constraint::Length(*w) }).collect();

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
    for ((_, key, _), r) in cols.iter().zip(col_rects.iter()) {
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
