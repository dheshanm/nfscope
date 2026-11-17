//! Render a fixture directory once to stdout (debug aid): `[TICKS=n] cargo run --example peek -- DIR [ip]`
use std::time::{Duration, Instant};

use nfscope::app::{Action, App};
use nfscope::source::{FixtureSource, Source};
use ratatui::{Terminal, backend::TestBackend};

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("usage: peek DIR [ip]");
    let ip = args.next();
    let mut src = FixtureSource::new(dir.into());
    let mut app = App::new("peek".into(), Duration::from_secs(2), true);
    let t0 = Instant::now();
    let ticks: u64 = std::env::var("TICKS").ok().and_then(|t| t.parse().ok()).unwrap_or(3);
    for i in 0..ticks {
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
    let mut t = Terminal::new(TestBackend::new(170, 45)).unwrap();
    t.draw(|f| nfscope::ui::draw(f, &mut app)).unwrap();
    for y in 0..45 {
        let line: String = (0..170).map(|x| t.backend().buffer()[(x, y)].symbol().to_string()).collect();
        println!("{}", line.trim_end());
    }
}
