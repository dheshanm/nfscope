//! Snapshot tests: replay the fixtures through the real model/app and render to a TestBackend.

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nfscope::app::{Action, App, SortKey, View};
use nfscope::files;
use nfscope::source::{FixtureSource, Source};
use nfscope::workers::{FindMsg, collect};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Run queued actions synchronously against the fixture source.
fn run_actions(app: &mut App, src: &mut FixtureSource) {
    for a in std::mem::take(&mut app.actions) {
        match a {
            Action::Resolve(ip) => {
                let n = src.resolve(ip);
                app.on_dns(ip, n);
            }
            Action::LoadStates(ip) => {
                let r = files::load(src, ip);
                app.on_states(ip, r);
            }
            Action::SetInterval(_) | Action::CancelFind => {}
            Action::FindPaths(req) => {
                let res = src.find_paths(&req.export, &req.targets, &mut |_| true).map_err(|e| e.to_string());
                app.on_find(FindMsg::Done { id: req.id, res, secs: 0.5 });
            }
        }
    }
}

thread_local! {
    /// When this test's `fixture_app` started (each test runs on its own thread).
    static T0: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}

/// `secs` after `fixture_app`'s first sample. Later samples must be placed with this, not as
/// `Instant::now() + secs`: otherwise graph spans include the test's own run time, and a slow
/// machine renders `-13s` where the snapshot says `-12s`.
fn at(secs: u64) -> Instant {
    T0.with(|t| t.get()).expect("fixture_app() first") + Duration::from_secs(secs)
}

fn fixture_app() -> (App, FixtureSource) {
    let mut src = FixtureSource::new(fixture_dir());
    let mut app = App::new("testserver".into(), Duration::from_secs(2), true);
    let t0 = Instant::now();
    T0.with(|t| t.set(Some(t0)));
    for i in 0..2u64 {
        let mut s = collect(&mut src, 2049);
        s.mono = t0 + Duration::from_secs(2 * i);
        s.wall = format!("12:00:0{}", 2 * i);
        app.on_sample(s);
    }
    run_actions(&mut app, &mut src);
    (app, src)
}

fn render(app: &mut App, w: u16, h: u16) -> TestBackend {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| nfscope::ui::draw(f, app)).unwrap();
    t.backend().clone()
}

fn key(app: &mut App, c: KeyCode) {
    app.handle_key(KeyEvent::new(c, KeyModifiers::NONE));
}

#[test]
fn top_view() {
    let (mut app, _) = fixture_app();
    insta::assert_snapshot!(render(&mut app, 150, 20));
}

#[test]
fn top_rates_match_fixture() {
    let (mut app, _) = fixture_app();
    let rows = app.rows();
    // Default sort is TOTAL desc; client-01 has two nconnect sockets at 60 + 51.1 MiB/s.
    assert_eq!(rows[0].ip, "10.20.1.11".parse::<IpAddr>().unwrap());
    assert_eq!(rows[0].conns.len(), 2);
    assert!((rows[0].read_bps / 1048576.0 - 111.1).abs() < 0.01);
    assert_eq!(rows[1].ip, "10.20.1.12".parse::<IpAddr>().unwrap());
    // IPv4-mapped peer is folded into its IPv4 address and matched to its NFSv4 record.
    let mapped = rows.iter().find(|c| c.ip == "10.20.3.30".parse::<IpAddr>().unwrap()).unwrap();
    assert_eq!(mapped.nfs_name(), Some("Linux NFSv4.2 mapped-01.example.org"));
}

#[test]
fn sort_cycle_and_filter() {
    let (mut app, _) = fixture_app();
    key(&mut app, KeyCode::Char('s'));
    assert_eq!(app.sort, SortKey::Read);
    key(&mut app, KeyCode::Char('/'));
    for c in "gpu".chars() {
        key(&mut app, KeyCode::Char(c));
    }
    key(&mut app, KeyCode::Enter);
    let ips: Vec<String> = app.rows().iter().map(|c| c.ip.to_string()).collect();
    assert_eq!(ips, ["10.20.2.21", "10.20.2.22"]);
    insta::assert_snapshot!(render(&mut app, 150, 12));
}

#[test]
fn detail_view() {
    let (mut app, mut src) = fixture_app();
    key(&mut app, KeyCode::Enter); // top row = 10.20.1.11
    assert!(app.actions.contains(&Action::LoadStates("10.20.1.11".parse().unwrap())));
    run_actions(&mut app, &mut src);
    insta::assert_snapshot!(render(&mut app, 130, 40));
}

#[test]
fn detail_view_loading_and_nfsv3() {
    let (mut app, mut src) = fixture_app();
    app.open_detail("10.20.4.50".parse().unwrap());
    let loading = render(&mut app, 110, 24);
    assert!(format!("{loading}").contains("reading open state"));
    run_actions(&mut app, &mut src);
    insta::assert_snapshot!(render(&mut app, 110, 24));
}

#[test]
fn detail_filter_files() {
    let (mut app, mut src) = fixture_app();
    app.open_detail("10.20.1.11".parse().unwrap());
    run_actions(&mut app, &mut src);
    key(&mut app, KeyCode::Char('/'));
    for c in "log".chars() {
        key(&mut app, KeyCode::Char(c));
    }
    let out = format!("{}", render(&mut app, 130, 40));
    assert!(out.contains("run.log"));
    assert!(!out.contains("scan_0000.dcm"));
    key(&mut app, KeyCode::Esc); // clears filter + stops editing
    key(&mut app, KeyCode::Esc); // back to top
    assert!(matches!(app.view, nfscope::app::View::Top));
}

#[test]
fn fixture_source_keeps_moving_after_replay() {
    let mut src = FixtureSource::new(fixture_dir());
    let mut prev = src.sockets(2049).unwrap();
    for _ in 0..5 {
        let cur = src.sockets(2049).unwrap();
        assert_eq!(cur.len(), prev.len());
        for (a, b) in prev.iter().zip(&cur) {
            assert!(b.bytes_acked >= a.bytes_acked && b.bytes_received >= a.bytes_received);
        }
        prev = cur;
    }
}

/// Sanity-check the parsers against a real capture in `tests/fixtures/local/` if one exists
/// (gitignored; see README for how to capture one).
#[test]
fn local_capture_parses() {
    use nfscope::parse::nfsd::{parse_info, parse_mountinfo, parse_states};
    let dir = fixture_dir().join("local");
    if !dir.join("ss").exists() {
        eprintln!("no local capture; skipping");
        return;
    }
    let mut src = FixtureSource::new(dir.clone());
    let socks = src.sockets(2049).unwrap();
    assert!(!socks.is_empty());
    let raw_headers = std::fs::read_to_string(dir.join("ss/001.txt"))
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with(char::is_whitespace) && !l.trim().is_empty())
        .count();
    assert_eq!(socks.len(), raw_headers, "every socket header parsed");
    assert!(socks.iter().all(|s| s.rtt_ms.is_some() && s.cwnd.is_some()));

    let infos: Vec<_> = src.client_infos().unwrap().iter().map(|(id, t)| parse_info(id, t)).collect();
    assert!(infos.iter().all(|c| c.ip.is_some() && c.name.is_some()), "{infos:#?}");

    let mounts = parse_mountinfo(&src.mountinfo().unwrap());
    let mut unresolved = std::collections::BTreeSet::new();
    let mut n = 0;
    for c in &infos {
        for e in parse_states(&src.states(&c.id).unwrap()) {
            n += 1;
            assert!(e.filename.is_some(), "{e:?}");
            let dev = e.dev.expect("superblock parsed");
            if !mounts.contains_key(&dev) {
                unresolved.insert(dev);
            }
        }
    }
    eprintln!("{} sockets, {} clients, {n} states, unresolved devs: {unresolved:?}", socks.len(), infos.len());
    assert!(unresolved.is_empty());

    // And the full pipeline renders without panicking.
    let mut app = App::new("local".into(), Duration::from_secs(2), true);
    let t0 = Instant::now();
    for i in 0..3u64 {
        let mut s = collect(&mut src, 2049);
        s.mono = t0 + Duration::from_secs(2 * i);
        app.on_sample(s);
    }
    run_actions(&mut app, &mut src);
    let ip = app.rows()[0].ip;
    app.open_detail(ip);
    run_actions(&mut app, &mut src);
    let _ = render(&mut app, 160, 50);
}

#[test]
fn detail_hide_delegations() {
    let (mut app, mut src) = fixture_app();
    app.open_detail("10.20.1.11".parse().unwrap());
    run_actions(&mut app, &mut src);
    // Hidden by default: the three cached_*.nii.gz files are held only as delegations.
    let before = format!("{}", render(&mut app, 130, 40));
    assert!(before.contains("Open files: 33 under 5 dir names"));
    assert!(before.contains("delegation-only hidden (30 shown)"), "{before}");
    assert!(!before.contains("sub_020"));
    key(&mut app, KeyCode::Char('d'));
    let after = format!("{}", render(&mut app, 130, 40));
    assert!(!after.contains("delegation-only hidden"), "{after}");
    assert!(after.contains("sub_020"), "{after}");
    // The choice carries over to the next client opened.
    key(&mut app, KeyCode::Esc);
    app.open_detail("10.20.1.12".parse().unwrap());
    let View::Detail(d) = &app.view else { panic!("in the drill-down") };
    assert!(!d.hide_deleg);
}

#[test]
fn find_full_paths_on_demand() {
    let (mut app, mut src) = fixture_app();
    app.hide_deleg = false; // search the whole tree, cached delegations included
    app.open_detail("10.20.1.11".parse().unwrap());
    run_actions(&mut app, &mut src);
    let select = |app: &mut App, id: &[&str]| {
        let View::Detail(d) = &mut app.view else { panic!() };
        d.tree_state.open(id[..id.len().min(2)].iter().map(|s| s.to_string()).collect());
        d.tree_state.select(id.iter().map(|s| s.to_string()).collect());
    };
    // One file: its real directory shows up next to it.
    select(&mut app, &["/export/data", "sub_001", "scan_0000.dcm"]);
    key(&mut app, KeyCode::Char('f'));
    let Some(Action::FindPaths(req)) = app.actions.first().cloned() else { panic!("{:?}", app.actions) };
    assert_eq!((req.export.as_str(), req.targets.len(), req.targets[0].ino), ("/export/data", 1, 0x1001));
    run_actions(&mut app, &mut src);
    let out = format!("{}", render(&mut app, 130, 40));
    assert!(out.contains("scan_0000.dcm  in study/sub_001/"), "{out}");
    assert!(out.contains("found 1/1 full path"), "{out}");
    // The whole group: only the one not yet known is searched for; the rest are gone.
    select(&mut app, &["/export/data", "sub_001"]);
    key(&mut app, KeyCode::Char('f'));
    let Some(Action::FindPaths(req)) = app.actions.first().cloned() else { panic!() };
    assert_eq!(req.targets.len(), 13);
    run_actions(&mut app, &mut src);
    let out = format!("{}", render(&mut app, 130, 40));
    assert!(out.contains("scan_0001.dcm  in study/sub_001/"), "{out}");
    assert!(out.contains("scan_0002.dcm  not found"), "{out}");
    assert!(out.contains("found 1/13 full paths"), "{out}");
    // The filter matches resolved paths.
    key(&mut app, KeyCode::Char('/'));
    for c in "study".chars() {
        key(&mut app, KeyCode::Char(c));
    }
    let out = format!("{}", render(&mut app, 130, 40));
    assert!(out.contains("(2 match)"), "{out}");
}

#[test]
fn esc_stops_a_running_search_before_leaving() {
    let (mut app, mut src) = fixture_app();
    app.hide_deleg = false; // search the whole tree, cached delegations included
    app.open_detail("10.20.1.11".parse().unwrap());
    run_actions(&mut app, &mut src);
    {
        let View::Detail(d) = &mut app.view else { panic!() };
        d.tree_state.select(vec!["/export/data".into()]);
    }
    key(&mut app, KeyCode::Char('f'));
    app.actions.clear(); // leave the search running
    let out = format!("{}", render(&mut app, 130, 40));
    assert!(out.contains("locating 31 files under /export/data"), "{out}");
    key(&mut app, KeyCode::Esc);
    assert_eq!(app.actions, [Action::CancelFind]);
    assert!(matches!(app.view, View::Detail(_)), "first Esc only stops the search");
}

// ------------------------------------------------------------------ mouse

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

/// Screen position of the first occurrence of `needle` in a rendered frame.
fn find(b: &TestBackend, needle: &str) -> (u16, u16) {
    let buf = b.buffer();
    for y in 0..buf.area.height {
        let line: Vec<String> = (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect();
        let joined: String = line.concat();
        if let Some(byte_idx) = joined.find(needle) {
            // Convert byte offset to column index (cells may hold multi-byte symbols).
            let mut acc = 0;
            for (x, sym) in line.iter().enumerate() {
                if acc == byte_idx {
                    return (x as u16, y);
                }
                acc += sym.len();
            }
        }
    }
    panic!("{needle:?} not on screen:\n{b}");
}

fn click_at(app: &mut App, (x, y): (u16, u16), at: Instant) {
    app.handle_mouse_at(
        MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y, modifiers: KeyModifiers::NONE },
        at,
    );
}

fn mouse(app: &mut App, kind: MouseEventKind, (x, y): (u16, u16)) {
    app.handle_mouse(MouseEvent { kind, column: x, row: y, modifiers: KeyModifiers::NONE });
}

#[test]
fn mouse_header_click_sorts_and_reverses() {
    let (mut app, _) = fixture_app();
    let t = Instant::now();
    let b = render(&mut app, 150, 20);
    click_at(&mut app, find(&b, "WRITE/s"), t);
    assert_eq!((app.sort, app.sort_desc), (SortKey::Write, true));
    let b = render(&mut app, 150, 20);
    assert!(format!("{b}").contains("WRITE/s ▼"));
    // Clicking the same header again reverses (and isn't mistaken for a double-click on a row).
    click_at(&mut app, find(&b, "WRITE/s"), t + Duration::from_millis(100));
    assert_eq!((app.sort, app.sort_desc), (SortKey::Write, false));
    let b = render(&mut app, 150, 20);
    assert!(format!("{b}").contains("WRITE/s ▲"));
    // Text columns start ascending.
    click_at(&mut app, find(&b, "HOST"), t + Duration::from_secs(5));
    assert_eq!((app.sort, app.sort_desc), (SortKey::Host, false));
    // TREND isn't sortable.
    let b = render(&mut app, 150, 20);
    click_at(&mut app, find(&b, "TREND"), t + Duration::from_secs(6));
    assert_eq!(app.sort, SortKey::Host);
}

#[test]
fn mouse_row_select_double_click_opens_and_back() {
    let (mut app, mut src) = fixture_app();
    let t = Instant::now();
    let b = render(&mut app, 150, 20);
    let row = find(&b, "10.20.1.13");
    click_at(&mut app, row, t);
    assert_eq!(app.selected, Some("10.20.1.13".parse().unwrap()));
    assert!(matches!(app.view, nfscope::app::View::Top), "single click only selects");
    click_at(&mut app, row, t + Duration::from_millis(200));
    assert!(matches!(&app.view, nfscope::app::View::Detail(d) if d.ip == "10.20.1.13".parse::<IpAddr>().unwrap()));
    run_actions(&mut app, &mut src);
    let b = render(&mut app, 130, 40);
    click_at(&mut app, find(&b, "◀ back"), t + Duration::from_secs(2));
    assert!(matches!(app.view, nfscope::app::View::Top));
}

#[test]
fn mouse_slow_clicks_are_not_double() {
    let (mut app, _) = fixture_app();
    let t = Instant::now();
    let b = render(&mut app, 150, 20);
    let row = find(&b, "10.20.1.13");
    click_at(&mut app, row, t);
    click_at(&mut app, row, t + Duration::from_secs(1));
    assert!(matches!(app.view, nfscope::app::View::Top));
}

#[test]
fn mouse_tree_browsing() {
    let (mut app, mut src) = fixture_app();
    app.open_detail("10.20.1.11".parse().unwrap());
    run_actions(&mut app, &mut src);
    let t = Instant::now();
    let b = render(&mut app, 130, 40);
    // 5 dirs > 4, so dirs start collapsed. Click one to expand it.
    assert!(!format!("{b}").contains("scan_0000.dcm"));
    click_at(&mut app, find(&b, "sub_017/"), t);
    let b = render(&mut app, 130, 40);
    assert!(format!("{b}").contains("scan_0004.dcm"), "{b}");
    // Click a file: selects it, doesn't toggle anything.
    click_at(&mut app, find(&b, "scan_0004.dcm"), t + Duration::from_secs(1));
    let View::Detail(d) = &app.view else { panic!() };
    assert_eq!(d.tree_state.selected(), ["/export/data", "sub_017", "scan_0004.dcm"]);
    // Click the dir again: collapses.
    let b = render(&mut app, 130, 40);
    click_at(&mut app, find(&b, "sub_017/"), t + Duration::from_secs(2));
    let b = render(&mut app, 130, 40);
    assert!(!format!("{b}").contains("scan_0004.dcm"));
    // Wheel over the tree scrolls it; right-click goes back.
    mouse(&mut app, MouseEventKind::ScrollDown, find(&b, "/export/data"));
    mouse(&mut app, MouseEventKind::Down(MouseButton::Right), (5, 5));
    assert!(matches!(app.view, nfscope::app::View::Top));
}

#[test]
fn mouse_footer_buttons() {
    let (mut app, mut src) = fixture_app();
    let t = Instant::now();
    let b = render(&mut app, 150, 20);
    click_at(&mut app, find(&b, "p pause"), t);
    assert!(app.paused);
    let b = render(&mut app, 150, 20);
    click_at(&mut app, find(&b, "+ slower"), t + Duration::from_secs(1));
    assert_eq!(app.interval, Duration::from_secs(3));
    let b = render(&mut app, 150, 20);
    click_at(&mut app, find(&b, "? help"), t + Duration::from_secs(2));
    assert!(app.show_help);
    click_at(&mut app, (1, 1), t + Duration::from_secs(3)); // any click closes help
    assert!(!app.show_help);

    app.open_detail("10.20.1.11".parse().unwrap());
    run_actions(&mut app, &mut src);
    let b = render(&mut app, 130, 40);
    click_at(&mut app, find(&b, "d show delegs"), t + Duration::from_secs(4));
    let b = render(&mut app, 130, 40);
    assert!(format!("{b}").contains("d hide delegs"));
    click_at(&mut app, find(&b, "Esc back"), t + Duration::from_secs(5));
    assert!(matches!(app.view, nfscope::app::View::Top));
    let b = render(&mut app, 150, 20);
    click_at(&mut app, find(&b, "q quit"), t + Duration::from_secs(6));
    assert!(app.quit);
}

// ------------------------------------------------------------------ colours

/// Blue = read, pink = write, everywhere they appear (totals in a deeper shade).
#[test]
fn read_write_colours_are_consistent() {
    use nfscope::ui::{READ, READ_TOT, WRITE, WRITE_TOT};
    let fg = |b: &TestBackend, pos: (u16, u16)| b.buffer()[pos].fg;

    let (mut app, mut src) = fixture_app();
    let b = render(&mut app, 150, 22);
    // Header labels and their graphs.
    let (x, y) = find(&b, "READ ");
    assert_eq!(fg(&b, (x, y)), READ);
    let (wx, wy) = find(&b, "WRITE  ");
    assert_eq!(fg(&b, (wx, wy)), WRITE);
    let right_edge = b.buffer().area.width - 2;
    assert_eq!(fg(&b, (right_edge, y + 1)), READ, "newest READ bar at the right edge");
    assert_eq!(fg(&b, (right_edge, wy + 1)), WRITE, "newest WRITE bar at the right edge");
    // Table column headers and rate cells.
    let (hx, hy) = find(&b, "READ/s");
    assert_eq!(fg(&b, (hx, hy)), READ);
    assert_eq!(fg(&b, find(&b, "WRITE/s")), WRITE);
    // Cumulative totals are a deeper shade of the same hue.
    assert_eq!(fg(&b, find(&b, "READ TOT")), READ_TOT);
    assert_eq!(fg(&b, find(&b, "WRITE TOT")), WRITE_TOT);
    assert_eq!(fg(&b, find(&b, "111.1 MiB/s")), READ);
    assert_eq!(fg(&b, find(&b, "43.5 MiB/s")), WRITE);

    // Drill-down: access flags in the tree.
    app.open_detail("10.20.1.12".parse().unwrap());
    run_actions(&mut app, &mut src);
    let b = render(&mut app, 130, 40);
    let (ax, ay) = find(&b, "rw open");
    assert_eq!(fg(&b, (ax, ay)), READ);
    assert_eq!(fg(&b, (ax + 1, ay)), WRITE);
    assert_eq!(fg(&b, find(&b, "3 writable")), WRITE);
    // Connection table: totals in the deeper shades, retransmits in warning orange.
    assert_eq!(fg(&b, find(&b, "READ TOT")), READ_TOT);
    assert_eq!(fg(&b, find(&b, "WRITE TOT")), WRITE_TOT);
    assert_eq!(fg(&b, find(&b, "RETR/s")), nfscope::ui::WARN);
}

// ------------------------------------------------------------------ nfsd server health

#[test]
fn server_overlay() {
    let (mut app, _) = fixture_app();
    let b = render(&mut app, 150, 20);
    assert!(format!("{b}").contains("nfsd 12/64 busy  4.1k op/s"), "{b}");
    key(&mut app, KeyCode::Char('o'));
    assert!(app.show_ops);
    insta::assert_snapshot!(render(&mut app, 100, 30));
    key(&mut app, KeyCode::Esc);
    assert!(!app.show_ops);
}

#[test]
fn server_health_line_click_opens_overlay() {
    let (mut app, _) = fixture_app();
    let t = Instant::now();
    let b = render(&mut app, 150, 20);
    click_at(&mut app, find(&b, "12/64 busy"), t);
    assert!(app.show_ops);
    // Wheel and right-click are swallowed while the overlay is up; a click closes it.
    mouse(&mut app, MouseEventKind::ScrollDown, (5, 10));
    assert!(app.show_ops);
    click_at(&mut app, (1, 1), t + Duration::from_secs(1));
    assert!(!app.show_ops);
}

#[test]
fn server_stats_keep_moving_after_replay() {
    let (mut app, mut src) = fixture_app();
    let t0 = Instant::now();
    for i in 2..8u64 {
        let mut s = collect(&mut src, 2049);
        s.mono = t0 + Duration::from_secs(2 * i);
        app.on_sample(s);
        let r = app.model.server.as_ref().expect("server rates");
        assert!((4000.0..4200.0).contains(&r.calls_ps), "tick {i}: {}", r.calls_ps);
        assert_eq!(r.threads, Some(64));
    }
}

#[test]
fn server_stats_missing_from_fixture() {
    // A fixture without rpc/ and pool/ (an older capture) shows "n/a" instead of failing.
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("no-rpc-fixture");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("ss")).unwrap();
    std::fs::create_dir_all(dir.join("clients")).unwrap();
    std::fs::copy(fixture_dir().join("ss/001.txt"), dir.join("ss/001.txt")).unwrap();
    let mut src = FixtureSource::new(dir);
    let mut app = App::new("t".into(), Duration::from_secs(2), false);
    app.on_sample(collect(&mut src, 2049));
    assert!(app.model.server_error.is_some());
    let b = render(&mut app, 150, 20);
    assert!(format!("{b}").contains("nfsd stats n/a"), "{b}");
    key(&mut app, KeyCode::Char('o'));
    let b = render(&mut app, 100, 30);
    assert!(format!("{b}").contains("nfsd server stats unavailable"), "{b}");

    // No inflight/ either: no THREADS column, sorting skips it, `i` explains.
    key(&mut app, KeyCode::Esc);
    assert!(app.model.inflight.is_none());
    assert!(!format!("{}", render(&mut app, 150, 20)).contains("THREADS"));
    for _ in 0..3 {
        key(&mut app, KeyCode::Char('s'));
    }
    assert_eq!(app.sort, SortKey::ReadTot, "Total → Read → Write → (Threads skipped) → ReadTot");
    key(&mut app, KeyCode::Char('i'));
    let b = render(&mut app, 100, 30);
    assert!(format!("{b}").contains("In-flight requests unavailable"), "{b}");
}

#[test]
fn inflight_overlay() {
    let (mut app, _) = fixture_app();
    // fixture_app's second sample replays inflight/002.txt.
    assert_eq!(app.model.inflight.as_ref().expect("fixture has inflight/").now, 7);
    key(&mut app, KeyCode::Char('i'));
    assert!(app.show_inflight);
    insta::assert_snapshot!(render(&mut app, 100, 30));
    // `o` and `i` switch between the two server overlays; the same key again closes.
    key(&mut app, KeyCode::Char('o'));
    assert!(app.show_ops && !app.show_inflight);
    let text = format!("{}", render(&mut app, 100, 30));
    assert!(text.contains("In flight 7 now · avg 7.0 · busiest client-01.example.org 3.0 · i: details"), "{text}");
    key(&mut app, KeyCode::Char('i'));
    assert!(app.show_inflight && !app.show_ops);
    key(&mut app, KeyCode::Char('i'));
    assert!(!app.overlay_open());
}

#[test]
fn threads_column_averages_inflight_and_sorts() {
    use nfscope::parse::inflight::parse_snapshot;
    use nfscope::workers::IntervalAcc;
    let (mut app, mut src) = fixture_app();
    // Two earlier snapshots this interval caught gpu-01 with 4 requests in flight. collect_with
    // adds the fixture's next one (inflight/001.txt again: gpu-01's slow COMMIT only).
    let mut acc = IntervalAcc::default();
    let four = (1..=4).map(|x| format!("10.20.2.21:706 xid={x} age_us=5000\n")).collect::<String>();
    acc.inflight.add(&parse_snapshot(&four));
    acc.inflight.add(&parse_snapshot(&four));
    let mut s = nfscope::workers::collect_with(&mut src, 2049, &mut acc);
    s.mono = at(10);
    app.on_sample(s);
    let gpu: IpAddr = "10.20.2.21".parse().unwrap();
    let l = app.model.clients[&gpu].inflight.clone().unwrap();
    assert_eq!((l.avg, l.peak, l.oldest_us), (3.0, 4, 1_340_000));

    for _ in 0..3 {
        key(&mut app, KeyCode::Char('s'));
    }
    assert_eq!(app.sort, SortKey::Threads);
    assert_eq!(app.rows()[0].ip, gpu);
    let b = render(&mut app, 150, 20);
    let (x, y) = find(&b, "THREADS ▼");
    // Its oldest request ran past a second: the cell is warning orange.
    let row = (y + 1..b.buffer().area.height)
        .find(|&r| format!("{b}").lines().nth(r as usize).is_some_and(|l| l.contains("10.20.2.21")))
        .unwrap();
    assert_eq!(b.buffer()[(x + 8, row)].fg, nfscope::ui::WARN, "{b}");

    app.open_detail(gpu);
    run_actions(&mut app, &mut src);
    let text = format!("{}", render(&mut app, 130, 40));
    assert!(text.contains("nfsd     3.0 threads avg · peak 4 · oldest 1.34 s"), "{text}");
}

#[test]
fn server_overlay_keeps_its_place_and_dims_background() {
    let (mut app, _) = fixture_app();
    let corner = |b: &TestBackend| find(b, "┌ nfsd server");
    let b = render(&mut app, 120, 40);
    let header = find(&b, "Clients 10");
    assert_eq!(b.buffer()[header].fg, ratatui::style::Color::Reset, "not dimmed while closed");

    key(&mut app, KeyCode::Char('o'));
    let b = render(&mut app, 120, 40);
    let at = corner(&b);
    assert_eq!(b.buffer()[header].fg, nfscope::ui::DIM, "background dimmed");
    assert!(b.buffer()[header].modifier.contains(ratatui::style::Modifier::DIM));
    let (x, y) = find(&b, "OPERATION");
    assert_ne!(b.buffer()[(x, y)].fg, nfscope::ui::DIM, "panel itself not dimmed");

    // Fewer operations (or none yet) must not move or resize the panel.
    app.model.server.as_mut().unwrap().ops.truncate(2);
    let b = render(&mut app, 120, 40);
    assert_eq!(corner(&b), at);
    app.model.server = None;
    let b = render(&mut app, 120, 40);
    assert_eq!(corner(&b), at);
}

/// Full-height graph cells paint their background, so stacked rows join without a seam
/// even when the terminal's line height is taller than the `█` glyph.
#[test]
fn sparkline_full_cells_fill_background() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Color;
    let area = Rect::new(0, 0, 3, 2);
    let mut buf = Buffer::empty(area);
    // newest first: full (2 rows), 1.5 rows, 0.25 rows
    nfscope::ui::sparkline(&mut buf, area, &[100.0, 75.0, 12.5], 100.0, Color::Blue);
    let cell = |x, y| (buf[(x, y)].symbol().to_string(), buf[(x, y)].bg);
    // Rightmost column: both rows full, both background-filled.
    assert_eq!(cell(2, 0), ("█".into(), Color::Blue));
    assert_eq!(cell(2, 1), ("█".into(), Color::Blue));
    // Middle column: bottom full (filled), top half glyph on the default background.
    assert_eq!(cell(1, 1), ("█".into(), Color::Blue));
    assert_eq!(cell(1, 0), ("▄".into(), Color::Reset));
    // Left column: a quarter cell, no background.
    assert_eq!(cell(0, 1), ("▂".into(), Color::Reset));
    assert_eq!(cell(0, 0).0, " ");
}

// ------------------------------------------------------------------ TCP health flags

#[test]
fn flagged_client_explains_its_flags() {
    use nfscope::model::Flag;
    let (mut app, mut src) = fixture_app();
    let ip: IpAddr = "10.20.2.22".parse().unwrap();
    assert_eq!(app.model.clients[&ip].flags(), vec![Flag::RecvQ]);
    let b = render(&mut app, 150, 20);
    let (x, y) = find(&b, " Q ");
    assert_eq!(b.buffer()[(x + 1, y)].fg, nfscope::ui::flag_color(Flag::RecvQ));

    app.open_detail(ip);
    run_actions(&mut app, &mut src);
    let b = render(&mut app, 130, 40);
    let text = format!("{b}");
    assert!(text.contains("Q server is behind on this client's requests"), "{b}");
    assert!(text.contains("3.0 MiB"), "Recv-Q shown per connection:\n{b}");
}

#[test]
fn busy_threads_averaged_over_interval() {
    use nfscope::parse::rpc::{ThreadAcc, ThreadStates};
    use nfscope::workers::IntervalAcc;
    let (mut app, mut src) = fixture_app();
    // Snapshots the sampler took between ticks: one of them caught every thread busy.
    let mut acc = IntervalAcc::default();
    acc.threads.add(ThreadStates { running: 1, blocked: 8, idle: 55 });
    acc.threads.add(ThreadStates { running: 4, blocked: 60, idle: 0 });
    acc.threads.add(ThreadStates { running: 2, blocked: 10, idle: 52 });
    // collect_with adds the fixture's next snapshot (threads/001.txt: 9 busy) and resets acc.
    let mut s = nfscope::workers::collect_with(&mut src, 2049, &mut acc);
    s.mono = at(10); // after fixture_app's samples, which run 2 s ahead
    app.on_sample(s);
    let l = app.model.server.as_ref().unwrap().thread_load.unwrap();
    assert_eq!((l.samples, l.peak_busy, l.avg_busy, l.all_busy_pct), (4, 64, 23.5, 25.0));
    assert_eq!(ThreadAcc::default().finish(), None);

    let b = render(&mut app, 150, 20);
    let (x, y) = find(&b, "24/64 busy ↑64");
    assert_eq!(b.buffer()[(x, y)].fg, ratatui::style::Color::Red, "saturated in 25% of snapshots");
    key(&mut app, KeyCode::Char('o'));
    let text = format!("{}", render(&mut app, 100, 30));
    assert!(text.contains("avg 24 busy (22 on storage, 2.0 on CPU) · peak 64"), "{text}");
    assert!(text.contains("25% of the time every thread was busy (4 snapshots)"), "{text}");
}

#[test]
fn detail_ops_panel_breaks_threads_down_by_operation() {
    use nfscope::parse::inflight::parse_snapshot;
    use nfscope::workers::IntervalAcc;
    let (mut app, mut src) = fixture_app();
    // Six more intervals for gpu-01: WRITEs ramp up, GETATTRs come and go. collect_with adds
    // the fixture's own snapshot each time (gpu-01's long-running COMMIT, sometimes a WRITE).
    for i in 0..6u64 {
        let mut acc = IntervalAcc::default();
        let writes = (0..=i).map(|x| format!("10.20.2.21:706 xid={} ops=53,22,38 age_us=9000\n", 100 + x));
        let mut snap: String = writes.collect();
        if i % 2 == 0 {
            snap += "10.20.2.21:706 xid=999 ops=53,22,9 age_us=150\n";
        }
        acc.inflight.add(&parse_snapshot(&snap));
        acc.inflight.add(&parse_snapshot(&snap));
        let mut s = nfscope::workers::collect_with(&mut src, 2049, &mut acc);
        s.mono = at(4 + 2 * i);
        app.on_sample(s);
    }
    let gpu: IpAddr = "10.20.2.21".parse().unwrap();
    let c = &app.model.clients[&gpu];
    let names: Vec<&str> = c.busiest_ops(100).into_iter().map(|(n, _)| n).collect();
    // The long COMMIT holds a thread in every snapshot: it outweighs the occasional GETATTRs.
    assert_eq!(names, ["WRITE", "COMMIT", "GETATTR"]);
    let l = c.inflight.as_ref().unwrap();
    let sum: f64 = l.ops.iter().map(|o| o.avg).sum();
    assert!((sum - l.avg).abs() < 1e-9, "per-op threads add up to the client's");

    app.open_detail(gpu);
    run_actions(&mut app, &mut src);
    let b = render(&mut app, 130, 40);
    let (_, top) = find(&b, "nfsd threads by operation");
    let y = top + 1;
    let line = |y: u16| (0..b.buffer().area.width).map(|x| b.buffer()[(x, y)].symbol().to_string()).collect::<String>();
    assert!(line(y).starts_with("│WRITE        4.3 thr   93%"), "busiest op first: {b}");
    assert_eq!(b.buffer()[(1, y)].fg, nfscope::ui::WRITE);
    // The COMMIT has been running for over a second: its age is warning orange.
    let (_, cy) = find(&b, "COMMIT");
    let row = line(cy);
    let ax = row.find(" s ").map(|i| row[..i].chars().count() as u16).expect(&row);
    assert_eq!(b.buffer()[(ax, cy)].fg, nfscope::ui::WARN, "{row}");
    // WRITE's sparkline climbs: its newest column is full height.
    let right = b.buffer().area.width - 2;
    assert_eq!(b.buffer()[(right, y)].symbol(), "█");
    insta::assert_snapshot!(b);
}

#[test]
fn detail_ops_panel_scale_is_at_least_one_thread() {
    use nfscope::workers::IntervalAcc;
    let (mut app, mut src) = fixture_app();
    // A light client: nine empty snapshots per interval, plus the fixture's own, which catches
    // one of its requests. About 0.1 threads: a sliver, not a full-height bar. Enough intervals
    // to scroll fixture_app's single-snapshot samples (1.0 threads) out of the graph.
    for i in 0..90u64 {
        let mut acc = IntervalAcc::default();
        for _ in 0..9 {
            acc.inflight.add(&[]);
        }
        let mut s = nfscope::workers::collect_with(&mut src, 2049, &mut acc);
        s.mono = at(4 + 2 * i);
        app.on_sample(s);
    }
    let ip: IpAddr = "10.20.4.50".parse().unwrap();
    let l = app.model.clients[&ip].inflight.clone().unwrap();
    assert!((l.avg - 0.1).abs() < 1e-9, "{l:?}");

    app.open_detail(ip);
    run_actions(&mut app, &mut src);
    let b = render(&mut app, 130, 40);
    let text = format!("{b}");
    assert!(text.contains("full height = 1.0 threads"), "{text}");
    let (_, top) = find(&b, "nfsd threads by operation");
    let right = b.buffer().area.width - 2;
    assert_eq!(b.buffer()[(right, top + 1)].symbol(), "▁", "{text}");
}

#[test]
fn detail_lists_every_nconnect_connection_when_it_fits() {
    let (mut app, mut src) = fixture_app();
    let ip: IpAddr = "10.20.1.11".parse().unwrap();
    // nconnect=16: the fixture's two sockets plus fourteen more.
    let c = app.model.clients.get_mut(&ip).unwrap();
    let base = c.conns[0].clone();
    for i in 0..14u16 {
        c.conns.push(nfscope::parse::ss::Sock { port: 900 + i, bytes_acked: 1000 + u64::from(i), ..base.clone() });
    }
    app.open_detail(ip);
    run_actions(&mut app, &mut src);

    let text = format!("{}", render(&mut app, 130, 50));
    assert!(text.contains("TCP connections (16) "), "all 16 fit on a 50-row screen:\n{text}");
    for i in 0..14 {
        assert!(text.contains(&format!("│   {}", 900 + i)), "port {} listed:\n{text}", 900 + i);
    }
    assert!(text.contains("Open files"), "{text}");

    // A short screen keeps room for the open-files tree and says how many are shown.
    let text = format!("{}", render(&mut app, 130, 30));
    assert!(text.contains("TCP connections (16, busiest 4 shown)"), "{text}");
    assert!(text.contains("Open files"), "{text}");
}

#[test]
fn detail_ops_rows_keep_a_fixed_order() {
    use nfscope::parse::inflight::parse_snapshot;
    use nfscope::workers::IntervalAcc;
    let (mut app, mut src) = fixture_app();
    // gpu-02 is busiest with GETATTR, then LOOKUP, then a single READ.
    let mut acc = IntervalAcc::default();
    let snap = "10.20.2.22:707 xid=1 ops=53,22,9 age_us=500\n\
                10.20.2.22:707 xid=2 ops=53,22,9 age_us=500\n\
                10.20.2.22:707 xid=3 ops=53,22,9 age_us=500\n\
                10.20.2.22:707 xid=4 ops=53,22,15 age_us=500\n\
                10.20.2.22:707 xid=5 ops=53,22,15 age_us=500\n\
                10.20.2.22:707 xid=6 ops=53,22,25 age_us=500\n";
    acc.inflight.add(&parse_snapshot(snap));
    let mut s = nfscope::workers::collect_with(&mut src, 2049, &mut acc);
    s.mono = at(4);
    app.on_sample(s);
    let ip: IpAddr = "10.20.2.22".parse().unwrap();
    let busiest: Vec<&str> = app.model.clients[&ip].busiest_ops(100).into_iter().map(|(n, _)| n).collect();
    assert_eq!(&busiest[..3], ["GETATTR", "LOOKUP", "READ"]);

    app.open_detail(ip);
    run_actions(&mut app, &mut src);
    let b = render(&mut app, 130, 40);
    // Data ops first, the rest alphabetically: READ above GETATTR whatever the load.
    let (_, r) = find(&b, "│READ ");
    let (_, g) = find(&b, "│GETATTR ");
    let (_, l) = find(&b, "│LOOKUP ");
    assert!(r < g && g < l, "{b}");
}

#[test]
fn detail_names_the_server_address_when_there_are_several() {
    let (mut app, mut src) = fixture_app();
    let ip: IpAddr = "10.20.1.11".parse().unwrap();
    let text = |app: &mut App| format!("{}", render(app, 130, 40));
    app.open_detail(ip);
    run_actions(&mut app, &mut src);
    assert!(!text(&mut app).contains("SERVER"), "one server address: no column");
    let c = app.model.clients.get_mut(&ip).unwrap();
    c.conns[1].local = Some("10.20.0.2".parse().unwrap());
    let out = text(&mut app);
    assert!(out.contains("SERVER") && out.contains("10.20.0.2") && out.contains("10.20.0.1"), "{out}");
}

#[test]
fn stuck_sampler_and_stalled_nfsd_are_called_out() {
    let (mut app, _) = fixture_app();
    let text = |app: &mut App| format!("{}", render(app, 170, 20));
    assert!(!text(&mut app).contains("⚠"));
    // Probe stuck: rates still come, the nfsd figures don't.
    app.model.nfsd_stalled = Some(Duration::from_secs(12));
    let out = text(&mut app);
    assert!(out.contains("nfsd not answering for 12s"), "{out}");
    // No sample at all for well past the interval (2 s here).
    app.last_sample_at = Some(Instant::now() - Duration::from_secs(9));
    let out = text(&mut app);
    assert!(out.contains("no new sample for 9s: the sampler is stuck"), "{out}");
    assert!(out.contains("Everything shown is from 12:00:02"), "{out}");
    app.last_sample_at = Some(Instant::now());
    assert!(!text(&mut app).contains("sampler is stuck"));
}

#[test]
fn dns_failures_are_retried_and_names_refreshed() {
    use nfscope::app::{DNS_REFRESH, DNS_RETRY, Dns};
    let (mut app, _) = fixture_app();
    let ip: IpAddr = "10.20.1.11".parse().unwrap();
    let resolves = |app: &mut App| std::mem::take(&mut app.actions).contains(&Action::Resolve(ip));
    let t0 = Instant::now();
    app.actions.clear();
    app.dns.insert(ip, Dns::Failed); // the lookup fixture_app sent just now failed
    // fixture_app asked just now: no retry until DNS_RETRY has passed.
    app.request_dns(t0);
    assert!(!resolves(&mut app));
    app.request_dns(t0 + DNS_RETRY + Duration::from_secs(1));
    assert!(resolves(&mut app));
    app.on_dns(ip, Some("new-name".into()));
    // A name is looked up again after DNS_REFRESH, and stays shown meanwhile.
    let later = t0 + DNS_RETRY + DNS_REFRESH + Duration::from_secs(2);
    app.request_dns(later);
    assert!(resolves(&mut app));
    assert_eq!(app.dns[&ip], Dns::Name("new-name".into()));
    // A failed refresh keeps it.
    app.on_dns(ip, None);
    assert_eq!(app.dns[&ip], Dns::Name("new-name".into()));
}

#[test]
fn clicks_and_shortcuts_while_typing_a_filter() {
    let (mut app, _) = fixture_app();
    key(&mut app, KeyCode::Char('/'));
    key(&mut app, KeyCode::Char('g'));
    app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT));
    app.handle_key(KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT));
    assert_eq!(app.filter, "gP", "Ctrl/Alt keys are not text");
    // Clicking the nfsd header line opens the panel and leaves the filter alone.
    let b = render(&mut app, 150, 20);
    click_at(&mut app, find(&b, "busy"), Instant::now());
    assert!(app.show_ops);
    assert_eq!(app.filter, "gP");
    assert!(!app.filter_editing);
}
