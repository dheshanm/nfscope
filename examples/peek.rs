//! Render a fixture directory once to stdout (debug aid, and the source of the README
//! screenshots: see `scripts/screenshots.sh`).
//!
//! `[TICKS=n] [KEYS=keys] [COLS=170] [ROWS=45] [NAME=peek] [FORMAT=text|html] cargo run --example peek -- DIR [ip]`
//!
//! KEYS are pressed before the frame is drawn, e.g. `KEYS=i` for the in-flight overlay or
//! `KEYS=sss` to sort by THREADS. `FORMAT=html` prints a standalone page with the frame's
//! colours, for a headless browser to screenshot.
use std::fmt::Write;
use std::time::{Duration, Instant};

use nfscope::app::{Action, App};
use nfscope::source::{FixtureSource, Source};
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use ratatui::{Terminal, backend::TestBackend};

fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("usage: peek DIR [ip]");
    let ip = args.next();
    let (cols, rows): (u16, u16) = (env("COLS", 170), env("ROWS", 45));
    let mut src = FixtureSource::new(dir.into());
    let mut app = App::new(env("NAME", "peek".to_string()), Duration::from_secs(2), true);
    let t0 = Instant::now();
    for i in 0..env("TICKS", 3u64) {
        let mut s = nfscope::workers::collect(&mut src, 2049);
        s.mono = t0 + Duration::from_secs(2 * i);
        app.on_sample(s);
    }
    if let Some(ip) = ip {
        app.open_detail(ip.parse().unwrap());
    }
    for a in std::mem::take(&mut app.actions) {
        match a {
            Action::Resolve(ip) => {
                let n = src.resolve(ip);
                app.on_dns(ip, n)
            }
            Action::LoadStates(ip) => {
                let r = nfscope::files::load(&mut src, ip);
                app.on_states(ip, r)
            }
            _ => {}
        }
    }
    for c in std::env::var("KEYS").unwrap_or_default().chars() {
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(c),
            crossterm::event::KeyModifiers::NONE,
        ));
    }
    let mut t = Terminal::new(TestBackend::new(cols, rows)).unwrap();
    t.draw(|f| nfscope::ui::draw(f, &mut app)).unwrap();
    let buf = t.backend().buffer();
    if std::env::var("FORMAT").is_ok_and(|f| f == "html") {
        print!("{}", html(buf));
    } else {
        for y in 0..rows {
            let line: String = (0..cols).map(|x| buf[(x, y)].symbol().to_string()).collect();
            println!("{}", line.trim_end());
        }
    }
}

const BG: &str = "#14161b";
const FG: &str = "#d4d7dd";

/// A dark terminal theme for the 16 named colours.
const NAMED: [&str; 16] = [
    "#1d1f24", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#abb2bf", // normal
    "#5c6370", "#ff7b86", "#b5e890", "#ffd88a", "#8cc8ff", "#e0a0ff", "#7fdbe6", "#ffffff", // bright
];

/// CSS colour for a ratatui colour; `None` = the terminal default.
fn css(c: Color) -> Option<String> {
    let named = |i: usize| Some(NAMED[i].to_string());
    match c {
        Color::Reset => None,
        Color::Black => named(0),
        Color::Red => named(1),
        Color::Green => named(2),
        Color::Yellow => named(3),
        Color::Blue => named(4),
        Color::Magenta => named(5),
        Color::Cyan => named(6),
        Color::Gray => named(7),
        Color::DarkGray => named(8),
        Color::LightRed => named(9),
        Color::LightGreen => named(10),
        Color::LightYellow => named(11),
        Color::LightBlue => named(12),
        Color::LightMagenta => named(13),
        Color::LightCyan => named(14),
        Color::White => named(15),
        Color::Rgb(r, g, b) => Some(format!("#{r:02x}{g:02x}{b:02x}")),
        // The xterm 256-colour table: 16 named, a 6x6x6 cube, then 24 greys.
        Color::Indexed(i @ 0..=15) => named(usize::from(i)),
        Color::Indexed(i @ 16..=231) => {
            let lvl = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
            let i = i - 16;
            Some(format!("#{:02x}{:02x}{:02x}", lvl(i / 36), lvl(i / 6 % 6), lvl(i % 6)))
        }
        Color::Indexed(i) => {
            let v = 8 + 10 * (i - 232);
            Some(format!("#{v:02x}{v:02x}{v:02x}"))
        }
    }
}

/// The frame as a standalone HTML page: one `<span>` per run of identically styled cells.
fn html(buf: &Buffer) -> String {
    let area = buf.area;
    let mut body = String::new();
    for y in 0..area.height {
        let mut run_style = String::new();
        let mut run = String::new();
        for x in 0..area.width {
            let cell = &buf[(x, y)];
            let (mut fg, mut bg) = (css(cell.fg).unwrap_or(FG.into()), css(cell.bg).unwrap_or(BG.into()));
            let m = cell.modifier;
            if m.contains(Modifier::REVERSED) {
                std::mem::swap(&mut fg, &mut bg);
            }
            let mut style = format!("color:{fg};background:{bg}");
            if m.contains(Modifier::BOLD) {
                style.push_str(";font-weight:bold");
            }
            if m.contains(Modifier::DIM) {
                style.push_str(";opacity:.6");
            }
            if style != run_style && !run.is_empty() {
                let _ = write!(body, "<span style=\"{run_style}\">{run}</span>");
                run.clear();
            }
            run_style = style;
            for ch in cell.symbol().chars() {
                match ch {
                    '&' => run.push_str("&amp;"),
                    '<' => run.push_str("&lt;"),
                    '>' => run.push_str("&gt;"),
                    // Pin non-ASCII glyphs to one cell: a font without them (braille in Menlo)
                    // falls back to one whose advance differs, which would skew the row.
                    c if !c.is_ascii() => {
                        run.push_str("<i>");
                        run.push(c);
                        run.push_str("</i>");
                    }
                    c => run.push(c),
                }
            }
        }
        let _ = writeln!(body, "<span style=\"{run_style}\">{run}</span>");
    }
    format!(
        "<!doctype html><meta charset=utf-8><style>\
         html,body{{margin:0;background:{BG}}}\
         pre{{margin:0;padding:14px 16px;font:13px/1.22 Menlo,'DejaVu Sans Mono',monospace;color:{FG};\
         display:inline-block}}\
         i{{font-style:normal;display:inline-block;width:1ch}}\
         </style><pre>{body}</pre>\n"
    )
}
