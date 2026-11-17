//! Application state and key handling. Side effects are queued as [`Action`]s for main to run.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::TableState;
use throbber_widgets_tui::ThrobberState;
use tui_tree_widget::TreeState;

use crate::files::{FileKey, FindEnd, OpenFile, OpenFiles, Target};
use crate::model::{ClientStat, Model, RawSample};
use crate::workers::{FindMsg, FindReq};

pub const INTERVALS: [u64; 8] = [1, 2, 3, 5, 10, 15, 30, 60];
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// A failed reverse lookup is tried again after this long.
pub const DNS_RETRY: Duration = Duration::from_secs(120);
/// A resolved name is looked up again after this long.
pub const DNS_REFRESH: Duration = Duration::from_secs(3600);
const WHEEL_LINES: usize = 3;

/// Clickable regions recorded by the UI during the last draw.
#[derive(Debug, Default, Clone)]
pub struct HitMap {
    /// Client table header cells that sort.
    pub sort_headers: Vec<(Rect, SortKey)>,
    /// Where client table rows are drawn (excluding header).
    pub table_body: Rect,
    /// Detail view "back" button.
    pub back: Rect,
    /// Detail view open-files tree.
    pub tree: Rect,
    /// Footer hints and other buttons: clicking acts like pressing the key.
    pub buttons: Vec<(Rect, KeyCode)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Ip,
    Read,
    Write,
    Total,
    /// nfsd threads held (in-flight RPCs).
    Threads,
    ReadTot,
    WriteTot,
    Conns,
    Host,
}

impl SortKey {
    const ORDER: [SortKey; 9] = [
        SortKey::Total,
        SortKey::Read,
        SortKey::Write,
        SortKey::Threads,
        SortKey::ReadTot,
        SortKey::WriteTot,
        SortKey::Conns,
        SortKey::Host,
        SortKey::Ip,
    ];

    pub fn next(self) -> Self {
        let i = Self::ORDER.iter().position(|k| *k == self).unwrap_or(0);
        Self::ORDER[(i + 1) % Self::ORDER.len()]
    }

    /// Text columns sort ascending by default, numbers descending.
    fn default_desc(self) -> bool {
        !matches!(self, SortKey::Ip | SortKey::Host)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Dns {
    Pending,
    Name(String),
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Resolve(IpAddr),
    LoadStates(IpAddr),
    SetInterval(Duration),
    /// Start a full-path search (replacing any running one).
    FindPaths(FindReq),
    /// Stop the running full-path search.
    CancelFind,
}

/// What we know about an open file's full path.
#[derive(Debug, Clone, PartialEq)]
pub enum PathState {
    Pending,
    Found(String),
    /// The whole export was searched without finding it (closed, renamed or deleted since).
    Missing,
}

/// A running full-path search.
#[derive(Debug, Clone, PartialEq)]
pub struct FindJob {
    pub id: u64,
    pub export: String,
    pub files: usize,
    pub dirs: u64,
}

pub struct Detail {
    pub ip: IpAddr,
    /// `None` while loading.
    pub files: Option<Result<OpenFiles, String>>,
    pub tree_state: TreeState<String>,
    pub filter: String,
    pub filter_editing: bool,
    /// Hide files the client only holds a cached delegation for.
    pub hide_deleg: bool,
    /// Last-known stats, so the view survives the client disconnecting.
    pub last: Option<ClientStat>,
    /// Full paths looked up with `f`, by (export, inode). Kept across reloads.
    pub paths: HashMap<FileKey, PathState>,
    pub find: Option<FindJob>,
    /// Outcome of the last search, shown under the tree.
    pub find_note: Option<String>,
}

impl Detail {
    pub fn path_of(&self, f: &OpenFile) -> Option<&PathState> {
        self.paths.get(&f.key()?)
    }
}

pub enum View {
    Top,
    Detail(Box<Detail>),
}

pub struct App {
    pub model: Model,
    pub server_name: String,
    pub interval: Duration,
    pub sort: SortKey,
    pub sort_desc: bool,
    pub selected: Option<IpAddr>,
    pub table_state: TableState,
    pub filter: String,
    pub filter_editing: bool,
    pub view: View,
    pub paused: bool,
    pub show_log: bool,
    pub show_help: bool,
    /// nfsd server health overlay.
    pub show_ops: bool,
    /// In-flight RPCs overlay: which clients and operations hold nfsd threads.
    pub show_inflight: bool,
    /// Drill-down hides files held only as cached delegations (`d` toggles). On by default and
    /// kept across clients, so a choice made in one drill-down carries over to the next.
    pub hide_deleg: bool,
    pub dns_enabled: bool,
    pub dns: HashMap<IpAddr, Dns>,
    /// When each address was last sent for a lookup.
    dns_asked: HashMap<IpAddr, Instant>,
    pub throbber: ThrobberState,
    pub quit: bool,
    pub actions: Vec<Action>,
    pub matcher: Matcher,
    pub hit: HitMap,
    /// When the last sample arrived (wall time on this machine), to notice a stuck sampler.
    pub last_sample_at: Option<Instant>,
    last_click: Option<(Instant, Position)>,
    next_find_id: u64,
}

impl App {
    pub fn new(server_name: String, interval: Duration, dns_enabled: bool) -> Self {
        Self {
            model: Model::default(),
            server_name,
            interval,
            sort: SortKey::Total,
            sort_desc: true,
            selected: None,
            table_state: TableState::default(),
            filter: String::new(),
            filter_editing: false,
            view: View::Top,
            paused: false,
            show_log: false,
            show_help: false,
            show_ops: false,
            show_inflight: false,
            hide_deleg: true,
            dns_enabled,
            dns: HashMap::new(),
            dns_asked: HashMap::new(),
            throbber: ThrobberState::default(),
            quit: false,
            actions: Vec::new(),
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            hit: HitMap::default(),
            last_sample_at: None,
            last_click: None,
            next_find_id: 0,
        }
    }

    // ---------------------------------------------------------------- data in

    pub fn on_sample(&mut self, s: RawSample) {
        self.last_sample_at = Some(Instant::now());
        if self.paused {
            self.model.rebaseline(&s);
            return;
        }
        self.model.ingest(s);
        if self.dns_enabled {
            self.request_dns(Instant::now());
        }
        if let View::Detail(d) = &mut self.view
            && let Some(c) = self.model.clients.get(&d.ip)
        {
            d.last = Some(c.clone());
        }
    }

    /// How long since the last sample, when that is well past the interval: the sampler is
    /// stuck and everything on screen is old.
    pub fn stale_for(&self) -> Option<Duration> {
        let age = self.last_sample_at?.elapsed();
        (age > self.interval * 2 + Duration::from_secs(1)).then_some(age)
    }

    /// Queue reverse lookups: new clients, failed lookups again after [`DNS_RETRY`], and names
    /// again after [`DNS_REFRESH`] (addresses get reassigned). Clients gone that long are forgotten.
    pub fn request_dns(&mut self, now: Instant) {
        fn age(asked: &HashMap<IpAddr, Instant>, ip: &IpAddr, now: Instant) -> Duration {
            asked.get(ip).map_or(Duration::MAX, |t| now.saturating_duration_since(*t))
        }
        let clients = &self.model.clients;
        self.dns.retain(|ip, _| clients.contains_key(ip) || age(&self.dns_asked, ip, now) < DNS_REFRESH);
        self.dns_asked.retain(|ip, _| self.dns.contains_key(ip));
        for ip in clients.keys() {
            let due = match self.dns.get(ip) {
                None => true,
                Some(Dns::Pending) => false,
                Some(Dns::Failed) => age(&self.dns_asked, ip, now) >= DNS_RETRY,
                Some(Dns::Name(_)) => age(&self.dns_asked, ip, now) >= DNS_REFRESH,
            };
            if due {
                // A known name stays on screen while it is looked up again.
                self.dns.entry(*ip).or_insert(Dns::Pending);
                self.dns_asked.insert(*ip, now);
                self.actions.push(Action::Resolve(*ip));
            }
        }
    }

    pub fn on_dns(&mut self, ip: IpAddr, name: Option<String>) {
        match (name, self.dns.get(&ip)) {
            // A failed refresh keeps the name we had.
            (None, Some(Dns::Name(_))) => {}
            (name, _) => {
                self.dns.insert(ip, name.map_or(Dns::Failed, Dns::Name));
            }
        }
    }

    pub fn on_states(&mut self, ip: IpAddr, res: Result<OpenFiles, String>) {
        if let View::Detail(d) = &mut self.view
            && d.ip == ip
        {
            d.tree_state = TreeState::default();
            if let Ok(of) = &res {
                default_open(&mut d.tree_state, of);
            }
            d.files = Some(res);
        }
    }

    pub fn on_find(&mut self, msg: FindMsg) {
        let View::Detail(d) = &mut self.view else { return };
        let Some(job) = &mut d.find else { return };
        match msg {
            FindMsg::Progress { id, dirs } if id == job.id => job.dirs = dirs,
            FindMsg::Done { id, res, secs } if id == job.id => {
                let job = d.find.take().expect("checked above");
                let note = match res {
                    Ok(r) => {
                        let complete = matches!(r.end, FindEnd::AllFound | FindEnd::Exhausted);
                        for ((export, ino), st) in d.paths.iter_mut() {
                            if *st == PathState::Pending && *export == job.export {
                                if let Some(p) = r.found.get(ino) {
                                    *st = PathState::Found(p.display().to_string());
                                } else if complete {
                                    *st = PathState::Missing;
                                }
                            }
                        }
                        let why = match r.end {
                            FindEnd::AllFound => "",
                            FindEnd::Exhausted => " · the rest are gone (closed, renamed or deleted)",
                            FindEnd::TimedOut => " · gave up: time limit",
                            FindEnd::Cancelled => " · stopped",
                        };
                        format!(
                            "found {}/{} full path{} · searched {} dirs in {secs:.1}s{why}",
                            r.found.len(),
                            job.files,
                            if job.files == 1 { "" } else { "s" },
                            r.dirs
                        )
                    }
                    Err(e) => format!("path search failed: {e}"),
                };
                d.paths.retain(|_, s| *s != PathState::Pending);
                d.find_note = Some(note);
            }
            _ => {}
        }
    }

    /// `f` in the drill-down: look up the full paths of the selected file, or of every file
    /// under the selected directory group / export.
    fn find_selected(&mut self) {
        let View::Detail(d) = &mut self.view else { return };
        let Some(Ok(of)) = &d.files else { return };
        let sel = d.tree_state.selected().to_vec();
        let Some(export) = sel.first().cloned() else { return };
        if !export.starts_with('/') {
            d.find_note = Some(format!("can't search {export}: no mount point for that device"));
            return;
        }
        let targets: Vec<Target> = of
            .files
            .iter()
            .filter(|f| f.export == export)
            .filter(|f| sel.get(1).is_none_or(|x| *x == f.dir))
            .filter(|f| sel.get(2).is_none_or(|x| *x == f.leaf))
            .filter(|f| !(d.hide_deleg && f.deleg_only()))
            .filter(|f| !matches!(d.path_of(f), Some(PathState::Found(_))))
            .filter_map(Target::of)
            .collect();
        if targets.is_empty() {
            d.find_note = Some("full path already known (or the kernel gave no inode)".into());
            return;
        }
        // One search at a time: a new one replaces the old.
        if d.find.take().is_some() {
            d.paths.retain(|_, s| *s != PathState::Pending);
        }
        self.next_find_id += 1;
        let id = self.next_find_id;
        for t in &targets {
            d.paths.insert((export.clone(), t.ino), PathState::Pending);
        }
        d.find = Some(FindJob { id, export: export.clone(), files: targets.len(), dirs: 0 });
        d.find_note = None;
        self.actions.push(Action::FindPaths(FindReq { id, ip: d.ip, export, targets }));
    }

    /// Back to the client list, stopping any path search the drill-down started.
    fn leave_detail(&mut self) {
        if let View::Detail(d) = &self.view
            && d.find.is_some()
        {
            self.actions.push(Action::CancelFind);
        }
        self.view = View::Top;
    }

    pub fn tick(&mut self) {
        self.throbber.calc_next();
    }

    // ---------------------------------------------------------------- derived

    pub fn host_label(&self, ip: &IpAddr) -> &str {
        match self.dns.get(ip) {
            Some(Dns::Name(n)) => n,
            Some(Dns::Pending) => "…",
            _ if !self.dns_enabled => "",
            _ => "?",
        }
    }

    fn sort_name(&self, c: &ClientStat) -> String {
        match self.dns.get(&c.ip) {
            Some(Dns::Name(n)) => n.clone(),
            _ => c.nfs_name().map(str::to_string).unwrap_or_else(|| "~".repeat(4) + &c.ip.to_string()),
        }
    }

    /// Clients after filtering and sorting.
    pub fn rows(&mut self) -> Vec<ClientStat> {
        let pattern =
            (!self.filter.is_empty()).then(|| Pattern::parse(&self.filter, CaseMatching::Ignore, Normalization::Smart));
        let mut buf = Vec::new();
        let mut rows: Vec<ClientStat> = Vec::new();
        for c in self.model.clients.values() {
            if let Some(p) = &pattern {
                let hay = format!("{} {} {}", c.ip, self.host_label(&c.ip), c.nfs_name().unwrap_or(""));
                if p.score(Utf32Str::new(&hay, &mut buf), &mut self.matcher).is_none() {
                    continue;
                }
            }
            rows.push(c.clone());
        }
        let key = self.sort;
        let names: HashMap<IpAddr, String> = if key == SortKey::Host {
            rows.iter().map(|c| (c.ip, self.sort_name(c))).collect()
        } else {
            HashMap::new()
        };
        rows.sort_by(|a, b| {
            let o = match key {
                SortKey::Ip => a.ip.cmp(&b.ip),
                SortKey::Read => a.read_bps.total_cmp(&b.read_bps),
                SortKey::Write => a.write_bps.total_cmp(&b.write_bps),
                SortKey::Total => a.total_bps().total_cmp(&b.total_bps()),
                SortKey::Threads => a.threads().total_cmp(&b.threads()),
                SortKey::ReadTot => a.read_total.cmp(&b.read_total),
                SortKey::WriteTot => a.write_total.cmp(&b.write_total),
                SortKey::Conns => a.conns.len().cmp(&b.conns.len()),
                SortKey::Host => names[&a.ip].cmp(&names[&b.ip]),
            };
            let o = if self.sort_desc { o.reverse() } else { o };
            // Stable tiebreak so equal rows don't jump around between samples.
            o.then_with(|| (a.read_total + a.write_total).cmp(&(b.read_total + b.write_total)).reverse())
                .then_with(|| a.ip.cmp(&b.ip))
        });
        rows
    }

    /// Resolve the selected IP to an index in `rows`, falling back to the first row.
    pub fn sync_selection(&mut self, rows: &[ClientStat]) {
        let idx = self.selected.and_then(|ip| rows.iter().position(|c| c.ip == ip));
        let idx = idx.or(if rows.is_empty() { None } else { Some(0) });
        self.selected = idx.map(|i| rows[i].ip);
        self.table_state.select(idx);
    }

    // ---------------------------------------------------------------- input

    pub fn handle_key(&mut self, k: KeyEvent) {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.overlay_open() {
            // `o` and `i` switch between the two server overlays; any other key closes.
            let (ops, inflight) = (self.show_ops, self.show_inflight);
            self.close_overlay();
            match k.code {
                KeyCode::Char('o') if !ops => self.show_ops = true,
                KeyCode::Char('i') if !inflight => self.show_inflight = true,
                _ => {}
            }
            return;
        }
        if self.editing_filter() {
            self.handle_filter_key(k);
            return;
        }
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char('o') => self.show_ops = true,
            KeyCode::Char('i') => self.show_inflight = true,
            KeyCode::Char('p') => self.paused = !self.paused,
            KeyCode::Char('l') => self.show_log = !self.show_log,
            KeyCode::Char('+') | KeyCode::Char('=') => self.step_interval(1),
            KeyCode::Char('-') => self.step_interval(-1),
            _ => match self.view {
                View::Top => self.handle_top_key(k),
                View::Detail(_) => self.handle_detail_key(k),
            },
        }
    }

    /// A modal (help, server health or in-flight RPCs) is on top; the next key or click closes it.
    pub fn overlay_open(&self) -> bool {
        self.show_help || self.show_ops || self.show_inflight
    }

    fn close_overlay(&mut self) {
        self.show_help = false;
        self.show_ops = false;
        self.show_inflight = false;
    }

    fn editing_filter(&self) -> bool {
        match &self.view {
            View::Top => self.filter_editing,
            View::Detail(d) => d.filter_editing,
        }
    }

    fn handle_filter_key(&mut self, k: KeyEvent) {
        let (text, editing) = match &mut self.view {
            View::Top => (&mut self.filter, &mut self.filter_editing),
            View::Detail(d) => (&mut d.filter, &mut d.filter_editing),
        };
        match k.code {
            KeyCode::Esc => {
                text.clear();
                *editing = false;
            }
            KeyCode::Enter => *editing = false,
            KeyCode::Backspace => {
                text.pop();
            }
            // Ctrl/Alt combinations are commands, not text (Shift is just a capital).
            KeyCode::Char(_) if k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {}
            KeyCode::Char(c) => text.push(c),
            KeyCode::Up | KeyCode::Down => {
                *editing = false;
                self.handle_key(k);
            }
            _ => {}
        }
        if let View::Detail(d) = &mut self.view
            && let Some(Ok(of)) = &d.files
        {
            // Re-open things so matches are visible.
            if d.filter.is_empty() {
                default_open(&mut d.tree_state, of);
            } else {
                open_all(&mut d.tree_state, of);
            }
        }
    }

    fn step_interval(&mut self, dir: i32) {
        let cur = self.interval.as_secs();
        let i = INTERVALS.iter().position(|&s| s >= cur).unwrap_or(INTERVALS.len() - 1) as i32;
        let n = (i + dir).clamp(0, INTERVALS.len() as i32 - 1) as usize;
        let d = Duration::from_secs(INTERVALS[n]);
        if d != self.interval {
            self.interval = d;
            self.actions.push(Action::SetInterval(d));
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let rows = self.rows();
        if rows.is_empty() {
            return;
        }
        let cur = self.selected.and_then(|ip| rows.iter().position(|c| c.ip == ip)).unwrap_or(0) as isize;
        let n = (cur + delta).clamp(0, rows.len() as isize - 1) as usize;
        self.selected = Some(rows[n].ip);
    }

    fn handle_top_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::Home | KeyCode::Char('g') => self.move_selection(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_selection(isize::MAX / 2),
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                // The THREADS column only exists while in-flight RPCs are available.
                if self.sort == SortKey::Threads && self.model.inflight.is_none() {
                    self.sort = self.sort.next();
                }
                self.sort_desc = self.sort.default_desc();
            }
            KeyCode::Char('S') => self.sort_desc = !self.sort_desc,
            KeyCode::Char('/') => self.filter_editing = true,
            KeyCode::Esc => self.filter.clear(),
            KeyCode::Enter | KeyCode::Right => {
                let rows = self.rows();
                self.sync_selection(&rows);
                if let Some(ip) = self.selected {
                    self.open_detail(ip);
                }
            }
            _ => {}
        }
    }

    /// Sort by `key`; choosing the current sort column again reverses it.
    pub fn set_sort(&mut self, key: SortKey) {
        if self.sort == key {
            self.sort_desc = !self.sort_desc;
        } else {
            self.sort = key;
            self.sort_desc = key.default_desc();
        }
    }

    pub fn handle_mouse(&mut self, m: MouseEvent) {
        self.handle_mouse_at(m, Instant::now());
    }

    /// Mouse handling with an explicit clock (for double-click detection in tests).
    pub fn handle_mouse_at(&mut self, m: MouseEvent, now: Instant) {
        let pos = Position::new(m.column, m.row);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let double =
                    self.last_click.is_some_and(|(t, p)| p == pos && now.saturating_duration_since(t) <= DOUBLE_CLICK);
                self.last_click = if double { None } else { Some((now, pos)) };
                self.click(pos, double);
            }
            MouseEventKind::Down(MouseButton::Right) if !self.overlay_open() => {
                if matches!(self.view, View::Detail(_)) {
                    self.leave_detail();
                }
            }
            MouseEventKind::ScrollUp => self.wheel(pos, -1),
            MouseEventKind::ScrollDown => self.wheel(pos, 1),
            _ => {}
        }
    }

    fn click(&mut self, pos: Position, double: bool) {
        if self.overlay_open() {
            self.close_overlay();
            return;
        }
        // Any click finishes filter editing (keeping the filter text), before a button's key
        // is pressed: otherwise that key would be typed into the filter.
        match &mut self.view {
            View::Top => self.filter_editing = false,
            View::Detail(d) => d.filter_editing = false,
        }
        if let Some(&(_, code)) = self.hit.buttons.iter().find(|(r, _)| r.contains(pos)) {
            self.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
            return;
        }
        match &mut self.view {
            View::Top => {
                if let Some(&(_, key)) = self.hit.sort_headers.iter().find(|(r, _)| r.contains(pos)) {
                    self.set_sort(key);
                } else if self.hit.table_body.contains(pos) {
                    let idx = self.table_state.offset() + (pos.y - self.hit.table_body.y) as usize;
                    if let Some(ip) = self.rows().get(idx).map(|c| c.ip) {
                        self.selected = Some(ip);
                        if double {
                            self.open_detail(ip);
                        }
                    }
                }
            }
            View::Detail(d) => {
                if self.hit.back.contains(pos) {
                    self.leave_detail();
                } else if let Some(id) = d.tree_state.rendered_at(pos).map(<[String]>::to_vec) {
                    // export / dir nodes fold on a single click; files just get selected.
                    let is_node = id.len() < 3;
                    d.tree_state.select(id.clone());
                    if is_node {
                        d.tree_state.toggle(id);
                    }
                }
            }
        }
    }

    fn wheel(&mut self, pos: Position, dir: isize) {
        if self.overlay_open() {
            return;
        }
        match &mut self.view {
            View::Top => self.move_selection(dir),
            View::Detail(d) => {
                if self.hit.tree.contains(pos) {
                    if dir < 0 {
                        d.tree_state.scroll_up(WHEEL_LINES);
                    } else {
                        d.tree_state.scroll_down(WHEEL_LINES);
                    }
                }
            }
        }
    }

    pub fn open_detail(&mut self, ip: IpAddr) {
        self.view = View::Detail(Box::new(Detail {
            ip,
            files: None,
            tree_state: TreeState::default(),
            filter: String::new(),
            filter_editing: false,
            hide_deleg: self.hide_deleg,
            last: self.model.clients.get(&ip).cloned(),
            paths: HashMap::new(),
            find: None,
            find_note: None,
        }));
        self.actions.push(Action::LoadStates(ip));
    }

    fn handle_detail_key(&mut self, k: KeyEvent) {
        let View::Detail(d) = &mut self.view else { return };
        let ts = &mut d.tree_state;
        match k.code {
            KeyCode::Esc if d.find.is_some() => {
                // The worker reports back as `Cancelled`; on_find tidies up.
                self.actions.push(Action::CancelFind);
            }
            KeyCode::Esc | KeyCode::Backspace => {
                if !d.filter.is_empty() && k.code == KeyCode::Esc {
                    d.filter.clear();
                } else {
                    self.leave_detail();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                ts.key_up();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                ts.key_down();
            }
            KeyCode::Left => {
                ts.key_left();
            }
            KeyCode::Right => {
                ts.key_right();
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                ts.toggle_selected();
            }
            KeyCode::PageUp => {
                ts.select_relative(|c| c.map_or(0, |c| c.saturating_sub(20)));
            }
            KeyCode::PageDown => {
                ts.select_relative(|c| c.map_or(0, |c| c.saturating_add(20)));
            }
            KeyCode::Home | KeyCode::Char('g') => {
                ts.select_first();
            }
            KeyCode::End | KeyCode::Char('G') => {
                ts.select_last();
            }
            KeyCode::Char('e') => {
                if let Some(Ok(of)) = &d.files {
                    open_all(ts, of);
                }
            }
            KeyCode::Char('c') => {
                ts.close_all();
            }
            KeyCode::Char('/') => d.filter_editing = true,
            KeyCode::Char('d') => {
                d.hide_deleg = !d.hide_deleg;
                self.hide_deleg = d.hide_deleg;
                ts.select_first();
            }
            KeyCode::Char('f') => self.find_selected(),
            KeyCode::Char('r') => {
                d.files = None;
                let ip = d.ip;
                self.actions.push(Action::LoadStates(ip));
            }
            _ => {}
        }
    }
}

/// Exports open; directories open too when there are only a few.
pub fn default_open(ts: &mut TreeState<String>, of: &OpenFiles) {
    ts.close_all();
    let few = of.dir_count() <= 4;
    for f in &of.files {
        ts.open(vec![f.export.clone()]);
        if few {
            ts.open(vec![f.export.clone(), f.dir.clone()]);
        }
    }
}

pub fn open_all(ts: &mut TreeState<String>, of: &OpenFiles) {
    for f in &of.files {
        ts.open(vec![f.export.clone()]);
        ts.open(vec![f.export.clone(), f.dir.clone()]);
    }
}
