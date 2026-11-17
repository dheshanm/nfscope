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

use crate::files::OpenFiles;
use crate::model::{ClientStat, Model, RawSample};

pub const INTERVALS: [u64; 8] = [1, 2, 3, 5, 10, 15, 30, 60];
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
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
    ReadTot,
    WriteTot,
    Conns,
    Host,
}

impl SortKey {
    const ORDER: [SortKey; 8] = [
        SortKey::Total,
        SortKey::Read,
        SortKey::Write,
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
    pub dns_enabled: bool,
    pub dns: HashMap<IpAddr, Dns>,
    pub throbber: ThrobberState,
    pub quit: bool,
    pub actions: Vec<Action>,
    pub matcher: Matcher,
    pub hit: HitMap,
    last_click: Option<(Instant, Position)>,
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
            dns_enabled,
            dns: HashMap::new(),
            throbber: ThrobberState::default(),
            quit: false,
            actions: Vec::new(),
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            hit: HitMap::default(),
            last_click: None,
        }
    }

    // ---------------------------------------------------------------- data in

    pub fn on_sample(&mut self, s: RawSample) {
        if self.paused {
            self.model.rebaseline(&s);
            return;
        }
        self.model.ingest(s);
        if self.dns_enabled {
            let new: Vec<IpAddr> = self.model.clients.keys().filter(|ip| !self.dns.contains_key(ip)).copied().collect();
            for ip in new {
                self.dns.insert(ip, Dns::Pending);
                self.actions.push(Action::Resolve(ip));
            }
        }
        if let View::Detail(d) = &mut self.view
            && let Some(c) = self.model.clients.get(&d.ip)
        {
            d.last = Some(c.clone());
        }
    }

    pub fn on_dns(&mut self, ip: IpAddr, name: Option<String>) {
        self.dns.insert(ip, name.map_or(Dns::Failed, Dns::Name));
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
        if self.show_help {
            self.show_help = false;
            return;
        }
        if self.editing_filter() {
            self.handle_filter_key(k);
            return;
        }
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.show_help = true,
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
            MouseEventKind::Down(MouseButton::Right) if !self.show_help => {
                if matches!(self.view, View::Detail(_)) {
                    self.view = View::Top;
                }
            }
            MouseEventKind::ScrollUp => self.wheel(pos, -1),
            MouseEventKind::ScrollDown => self.wheel(pos, 1),
            _ => {}
        }
    }

    fn click(&mut self, pos: Position, double: bool) {
        if self.show_help {
            self.show_help = false;
            return;
        }
        if let Some(&(_, code)) = self.hit.buttons.iter().find(|(r, _)| r.contains(pos)) {
            self.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
            return;
        }
        // Clicking anywhere else finishes filter editing (keeping the filter text).
        match &mut self.view {
            View::Top => self.filter_editing = false,
            View::Detail(d) => d.filter_editing = false,
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
                    self.view = View::Top;
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
        if self.show_help {
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
            hide_deleg: false,
            last: self.model.clients.get(&ip).cloned(),
        }));
        self.actions.push(Action::LoadStates(ip));
    }

    fn handle_detail_key(&mut self, k: KeyEvent) {
        let View::Detail(d) = &mut self.view else { return };
        let ts = &mut d.tree_state;
        match k.code {
            KeyCode::Esc | KeyCode::Backspace => {
                if !d.filter.is_empty() && k.code == KeyCode::Esc {
                    d.filter.clear();
                } else {
                    self.view = View::Top;
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
                ts.select_first();
            }
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
