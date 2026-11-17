//! Turns raw counter snapshots into per-client rates, totals and history.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::IpAddr;
use std::time::{Duration, Instant};

use crate::parse::nfsd::ClientInfo;
use crate::parse::ss::Sock;

/// Number of rate samples kept per client for sparklines/charts.
pub const HISTORY: usize = 300;

/// One raw observation of the system, produced by the sampler thread.
#[derive(Debug, Clone)]
pub struct RawSample {
    pub mono: Instant,
    /// Wall clock time, already formatted for display (HH:MM:SS).
    pub wall: String,
    pub socks: Result<Vec<Sock>, String>,
    pub clients: Result<Vec<ClientInfo>, String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Point {
    /// Seconds since the model's first sample.
    pub t: f64,
    pub read: f64,
    pub write: f64,
}

impl Point {
    pub fn total(&self) -> f64 {
        self.read + self.write
    }
}

#[derive(Debug, Clone)]
pub struct ClientStat {
    pub ip: IpAddr,
    /// Server → client bytes/s (client reading).
    pub read_bps: f64,
    /// Client → server bytes/s (client writing).
    pub write_bps: f64,
    /// Sum of counters over the currently open sockets (resets on reconnect).
    pub read_total: u64,
    pub write_total: u64,
    pub conns: Vec<Sock>,
    /// NFSv4 client records whose address is this IP (empty for NFSv3).
    pub infos: Vec<ClientInfo>,
    pub history: VecDeque<Point>,
}

impl ClientStat {
    fn new(ip: IpAddr) -> Self {
        Self {
            ip,
            read_bps: 0.0,
            write_bps: 0.0,
            read_total: 0,
            write_total: 0,
            conns: Vec::new(),
            infos: Vec::new(),
            history: VecDeque::new(),
        }
    }

    pub fn total_bps(&self) -> f64 {
        self.read_bps + self.write_bps
    }

    /// The most relevant NFSv4 record: confirmed first, then most recently renewed.
    pub fn best_info(&self) -> Option<&ClientInfo> {
        self.infos
            .iter()
            .min_by_key(|c| (c.status.as_deref() != Some("confirmed"), c.seconds_since_renew().unwrap_or(u64::MAX)))
    }

    pub fn nfs_name(&self) -> Option<&str> {
        self.best_info().and_then(|c| c.name.as_deref())
    }
}

#[derive(Debug, Default)]
pub struct Model {
    start: Option<Instant>,
    prev: HashMap<(IpAddr, u16), (u64, u64)>,
    prev_at: Option<Instant>,
    pub clients: BTreeMap<IpAddr, ClientStat>,
    /// Server-wide totals over time.
    pub total: VecDeque<Point>,
    /// Number of samples that produced rates (0 until two snapshots have been seen).
    pub rate_samples: u64,
    pub last_dt: Option<Duration>,
    pub last_wall: Option<String>,
    pub sock_error: Option<String>,
    pub nfsd_error: Option<String>,
}

fn push_capped(h: &mut VecDeque<Point>, p: Point) {
    if h.len() == HISTORY {
        h.pop_front();
    }
    h.push_back(p);
}

impl Model {
    pub fn has_rates(&self) -> bool {
        self.rate_samples > 0
    }

    pub fn total_rates(&self) -> (f64, f64) {
        self.clients.values().fold((0.0, 0.0), |(r, w), c| (r + c.read_bps, w + c.write_bps))
    }

    fn update_infos(&mut self, s: &RawSample) -> HashMap<IpAddr, Vec<ClientInfo>> {
        let mut by_ip: HashMap<IpAddr, Vec<ClientInfo>> = HashMap::new();
        match &s.clients {
            Ok(list) => {
                self.nfsd_error = None;
                for c in list {
                    if let Some(ip) = c.ip {
                        by_ip.entry(ip).or_default().push(c.clone());
                    }
                }
            }
            Err(e) => {
                self.nfsd_error = Some(e.clone());
                // Keep what we knew before.
                for (ip, c) in &self.clients {
                    by_ip.insert(*ip, c.infos.clone());
                }
            }
        }
        by_ip
    }

    fn remember(&mut self, socks: &[Sock], at: Instant) {
        self.prev = socks.iter().filter_map(|k| Some(((k.ip?, k.port), (k.bytes_acked, k.bytes_received)))).collect();
        self.prev_at = Some(at);
    }

    /// Record counters as the new baseline without changing anything visible
    /// (used while paused, so that resuming yields a fresh interval rate).
    pub fn rebaseline(&mut self, s: &RawSample) {
        if let Ok(socks) = &s.socks {
            self.remember(socks, s.mono);
        }
    }

    pub fn ingest(&mut self, s: RawSample) {
        let start = *self.start.get_or_insert(s.mono);
        self.last_wall = Some(s.wall.clone());
        let mut infos = self.update_infos(&s);
        let socks = match &s.socks {
            Ok(v) => {
                self.sock_error = None;
                v
            }
            Err(e) => {
                self.sock_error = Some(e.clone());
                return;
            }
        };

        let dt = self.prev_at.map(|p| s.mono.saturating_duration_since(p)).filter(|d| !d.is_zero());
        let mut next: BTreeMap<IpAddr, ClientStat> = BTreeMap::new();
        let mut deltas: HashMap<IpAddr, (u64, u64)> = HashMap::new();

        for k in socks {
            let Some(ip) = k.ip else { continue };
            let cur = (k.bytes_acked, k.bytes_received);
            let d = match self.prev.get(&(ip, k.port)) {
                Some(&p) if cur.0 >= p.0 && cur.1 >= p.1 => (cur.0 - p.0, cur.1 - p.1),
                // Counter went backwards (same port reused by a new connection) or a socket
                // that did not exist at the previous sample: everything it carries is new.
                _ => cur,
            };
            let e = deltas.entry(ip).or_default();
            e.0 += d.0;
            e.1 += d.1;

            let c = next.entry(ip).or_insert_with(|| {
                let mut c = self.clients.remove(&ip).unwrap_or_else(|| ClientStat::new(ip));
                c.conns.clear();
                c.read_total = 0;
                c.write_total = 0;
                c
            });
            c.read_total += cur.0;
            c.write_total += cur.1;
            c.conns.push(k.clone());
        }

        let t = s.mono.saturating_duration_since(start).as_secs_f64();
        let (mut tr, mut tw) = (0.0, 0.0);
        for (ip, c) in next.iter_mut() {
            c.infos = infos.remove(ip).unwrap_or_default();
            c.conns.sort_by_key(|k| k.port);
            if let Some(dt) = dt {
                let (dr, dw) = deltas.get(ip).copied().unwrap_or_default();
                c.read_bps = dr as f64 / dt.as_secs_f64();
                c.write_bps = dw as f64 / dt.as_secs_f64();
                push_capped(&mut c.history, Point { t, read: c.read_bps, write: c.write_bps });
                tr += c.read_bps;
                tw += c.write_bps;
            }
        }
        if dt.is_some() {
            push_capped(&mut self.total, Point { t, read: tr, write: tw });
            self.rate_samples += 1;
        }
        self.last_dt = dt;
        self.clients = next;
        self.remember(socks, s.mono);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sock(ip: &str, port: u16, r: u64, w: u64) -> Sock {
        Sock { ip: Some(ip.parse().unwrap()), port, bytes_acked: r, bytes_received: w, ..Default::default() }
    }

    fn sample(at: Instant, socks: Vec<Sock>) -> RawSample {
        RawSample { mono: at, wall: "00:00:00".into(), socks: Ok(socks), clients: Ok(vec![]) }
    }

    const MB: u64 = 1 << 20;

    #[test]
    fn first_sample_has_no_rates() {
        let mut m = Model::default();
        m.ingest(sample(Instant::now(), vec![sock("10.0.0.1", 1, 5 * MB, 0)]));
        assert!(!m.has_rates());
        let c = &m.clients[&"10.0.0.1".parse::<IpAddr>().unwrap()];
        assert_eq!(c.read_bps, 0.0);
        assert_eq!(c.read_total, 5 * MB);
        assert!(c.history.is_empty());
    }

    #[test]
    fn rates_and_nconnect_aggregation() {
        let t0 = Instant::now();
        let mut m = Model::default();
        m.ingest(sample(t0, vec![sock("10.0.0.1", 1, 0, 0), sock("10.0.0.1", 2, 0, 0), sock("10.0.0.2", 3, 0, 0)]));
        m.ingest(sample(
            t0 + Duration::from_secs(2),
            vec![sock("10.0.0.1", 1, 4 * MB, 2 * MB), sock("10.0.0.1", 2, 2 * MB, 0), sock("10.0.0.2", 3, 0, MB)],
        ));
        assert!(m.has_rates());
        let c = &m.clients[&"10.0.0.1".parse::<IpAddr>().unwrap()];
        assert_eq!(c.read_bps, (3 * MB) as f64);
        assert_eq!(c.write_bps, MB as f64);
        assert_eq!(c.conns.len(), 2);
        assert_eq!(c.read_total, 6 * MB);
        let (r, w) = m.total_rates();
        assert_eq!(r, (3 * MB) as f64);
        assert_eq!(w, (MB + MB / 2) as f64);
        assert_eq!(m.total.len(), 1);
    }

    #[test]
    fn reconnect_and_new_socket_count_from_zero() {
        let t0 = Instant::now();
        let mut m = Model::default();
        m.ingest(sample(t0, vec![sock("10.0.0.1", 1, 100 * MB, 100 * MB)]));
        // Same port, counters reset (new connection), plus a brand-new socket.
        m.ingest(sample(t0 + Duration::from_secs(1), vec![sock("10.0.0.1", 1, MB, 0), sock("10.0.0.1", 9, MB, 0)]));
        let c = &m.clients[&"10.0.0.1".parse::<IpAddr>().unwrap()];
        assert_eq!(c.read_bps, (2 * MB) as f64);
        assert_eq!(c.write_bps, 0.0);
    }

    #[test]
    fn disconnected_clients_drop_out() {
        let t0 = Instant::now();
        let mut m = Model::default();
        m.ingest(sample(t0, vec![sock("10.0.0.1", 1, 0, 0), sock("10.0.0.2", 1, 0, 0)]));
        m.ingest(sample(t0 + Duration::from_secs(1), vec![sock("10.0.0.2", 1, 0, 0)]));
        assert_eq!(m.clients.len(), 1);
    }

    #[test]
    fn rebaseline_while_paused() {
        let t0 = Instant::now();
        let mut m = Model::default();
        m.ingest(sample(t0, vec![sock("10.0.0.1", 1, 0, 0)]));
        m.ingest(sample(t0 + Duration::from_secs(1), vec![sock("10.0.0.1", 1, MB, 0)]));
        m.rebaseline(&sample(t0 + Duration::from_secs(60), vec![sock("10.0.0.1", 1, 1000 * MB, 0)]));
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        assert_eq!(m.clients[&ip].read_bps, MB as f64, "visible state untouched while paused");
        m.ingest(sample(t0 + Duration::from_secs(62), vec![sock("10.0.0.1", 1, 1004 * MB, 0)]));
        assert_eq!(m.clients[&ip].read_bps, (2 * MB) as f64);
    }

    #[test]
    fn socket_error_keeps_previous_rates() {
        let t0 = Instant::now();
        let mut m = Model::default();
        m.ingest(sample(t0, vec![sock("10.0.0.1", 1, 0, 0)]));
        m.ingest(sample(t0 + Duration::from_secs(1), vec![sock("10.0.0.1", 1, MB, 0)]));
        m.ingest(RawSample { socks: Err("boom".into()), ..sample(t0 + Duration::from_secs(2), vec![]) });
        assert_eq!(m.sock_error.as_deref(), Some("boom"));
        assert_eq!(m.clients.len(), 1);
    }
}
