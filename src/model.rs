//! Turns raw counter snapshots into per-client rates, totals and history.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::IpAddr;
use std::time::{Duration, Instant};

use crate::parse::nfsd::ClientInfo;
use crate::parse::rpc::{NfsdStats, ThreadStates};
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
    /// Server-wide nfsd counters (`/proc/net/rpc/nfsd` plus pool stats).
    pub nfsd: Result<NfsdStats, String>,
}

/// Server-wide nfsd rates over the last interval.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServerRates {
    pub threads: Option<u32>,
    /// RPC calls per second (one NFSv4 COMPOUND counts once).
    pub calls_ps: f64,
    pub bad_ps: f64,
    /// What the nfsd threads were doing at this sample.
    pub thread_states: Option<ThreadStates>,
    /// Share of nfsd threads handling a request (on CPU or waiting on storage), 0–100.
    pub busy_pct: Option<f64>,
    pub arrived_ps: Option<f64>,
    /// Share of arriving requests that had to wait for a thread. Only on kernels where
    /// `sockets-enqueued` counts exactly that: newer ones (and RHEL 9 backports) count every
    /// transport wake-up, so `sockets-enqueued` exceeds `packets-arrived` and this is None.
    pub queued_pct: Option<f64>,
    pub enqueued_ps: Option<f64>,
    pub timedout_ps: Option<f64>,
    /// nfsd payload bytes/s read from / written to the exported filesystems.
    pub io_read_ps: f64,
    pub io_write_ps: f64,
    pub rc_hit_pct: Option<f64>,
    /// Workload ops per second, busiest first (plumbing ops left out).
    pub ops: Vec<(String, f64)>,
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
    prev_nfsd: Option<(Instant, NfsdStats)>,
    /// Latest server-wide rates (None until two nfsd snapshots have been seen).
    pub server: Option<ServerRates>,
    /// Thread count from the latest snapshot, available before rates are.
    pub server_threads: Option<u32>,
    /// Why server stats are unavailable, if they are.
    pub server_error: Option<String>,
    /// History of `ServerRates::busy_pct`.
    pub server_busy: VecDeque<f64>,
}

/// Rates between two nfsd snapshots, or None if a counter went backwards (nfsd restarted).
fn server_rates(prev: &NfsdStats, cur: &NfsdStats, dt: f64) -> Option<ServerRates> {
    let d = |a: u64, b: u64| b.checked_sub(a);
    let calls = d(prev.rpc_calls, cur.rpc_calls)?;
    let bad = d(prev.rpc_bad, cur.rpc_bad)?;
    let io_r = d(prev.io_read, cur.io_read)?;
    let io_w = d(prev.io_write, cur.io_write)?;
    let hits = d(prev.rc_hits, cur.rc_hits)?;
    let misses = d(prev.rc_misses, cur.rc_misses)?;
    let (mut arrived_ps, mut enqueued_ps, mut timedout_ps, mut queued_pct) = (None, None, None, None);
    if let (Some(p), Some(c)) = (&prev.pool, &cur.pool) {
        let arrived = d(p.arrived, c.arrived)?;
        let enqueued = d(p.enqueued, c.enqueued)?;
        arrived_ps = Some(arrived as f64 / dt);
        enqueued_ps = Some(enqueued as f64 / dt);
        if let (Some(a), Some(b)) = (p.timedout, c.timedout) {
            timedout_ps = Some(d(a, b)? as f64 / dt);
        }
        if enqueued <= arrived {
            queued_pct = Some(if arrived == 0 { 0.0 } else { enqueued as f64 / arrived as f64 * 100.0 });
        }
    }
    let prev_ops = prev.named_ops();
    let mut ops: Vec<(String, f64)> = cur
        .named_ops()
        .into_iter()
        .filter_map(|(name, n)| {
            let delta = n.checked_sub(prev_ops.get(&name).copied().unwrap_or(0))?;
            (delta > 0).then(|| (name, delta as f64 / dt))
        })
        .collect();
    ops.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Some(ServerRates {
        threads: cur.threads,
        calls_ps: calls as f64 / dt,
        bad_ps: bad as f64 / dt,
        thread_states: cur.thread_states,
        busy_pct: cur.thread_states.and_then(|t| t.busy_pct()),
        arrived_ps,
        queued_pct,
        enqueued_ps,
        timedout_ps,
        io_read_ps: io_r as f64 / dt,
        io_write_ps: io_w as f64 / dt,
        rc_hit_pct: (hits + misses > 0).then(|| hits as f64 / (hits + misses) as f64 * 100.0),
        ops,
    })
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
        if let Ok(n) = &s.nfsd {
            self.prev_nfsd = Some((s.mono, n.clone()));
        }
    }

    fn ingest_server(&mut self, s: &RawSample) {
        let cur = match &s.nfsd {
            Ok(n) => n,
            Err(e) => {
                self.server_error = Some(e.clone());
                self.server = None;
                self.prev_nfsd = None;
                return;
            }
        };
        self.server_error = None;
        self.server_threads = cur.threads;
        if let Some((at, prev)) = &self.prev_nfsd {
            let dt = s.mono.saturating_duration_since(*at).as_secs_f64();
            if dt > 0.0 {
                // A counter going backwards (nfsd restart) just re-baselines.
                self.server = server_rates(prev, cur, dt);
                if let Some(b) = self.server.as_ref().and_then(|r| r.busy_pct) {
                    if self.server_busy.len() == HISTORY {
                        self.server_busy.pop_front();
                    }
                    self.server_busy.push_back(b);
                }
            }
        }
        self.prev_nfsd = Some((s.mono, cur.clone()));
    }

    pub fn ingest(&mut self, s: RawSample) {
        let start = *self.start.get_or_insert(s.mono);
        self.last_wall = Some(s.wall.clone());
        self.ingest_server(&s);
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
        RawSample { mono: at, wall: "00:00:00".into(), socks: Ok(socks), clients: Ok(vec![]), nfsd: Err("none".into()) }
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

    fn nfsd(calls: u64, arrived: u64, enqueued: u64, v4_read: u64, v4_getattr: u64) -> NfsdStats {
        use crate::parse::rpc::PoolStats;
        let mut proc4ops = vec![0; 76];
        proc4ops[9] = v4_getattr;
        proc4ops[25] = v4_read;
        proc4ops[22] = calls; // PUTFH: plumbing, must not show up
        NfsdStats {
            threads: Some(16),
            rpc_calls: calls,
            io_read: calls * 1000,
            rc_misses: calls,
            proc4ops,
            pool: Some(PoolStats { arrived, enqueued, woken: arrived, timedout: Some(0) }),
            thread_states: Some(ThreadStates { running: 1, blocked: 3, idle: 12 }),
            ..Default::default()
        }
    }

    fn with_nfsd(at: Instant, n: NfsdStats) -> RawSample {
        RawSample { nfsd: Ok(n), ..sample(at, vec![sock("10.0.0.1", 1, 0, 0)]) }
    }

    #[test]
    fn server_rates_and_busy() {
        let t0 = Instant::now();
        let mut m = Model::default();
        m.ingest(with_nfsd(t0, nfsd(1000, 1000, 0, 100, 100)));
        assert_eq!(m.server, None);
        assert_eq!(m.server_threads, Some(16));
        m.ingest(with_nfsd(t0 + Duration::from_secs(2), nfsd(3000, 3000, 100, 300, 1100)));
        let r = m.server.as_ref().expect("rates after two samples");
        assert_eq!(r.calls_ps, 1000.0);
        assert_eq!(r.io_read_ps, 1_000_000.0);
        assert_eq!(r.busy_pct, Some(25.0));
        assert_eq!(r.queued_pct, Some(5.0));
        assert_eq!(r.enqueued_ps, Some(50.0));
        assert_eq!(r.rc_hit_pct, Some(0.0));
        assert_eq!(r.ops, vec![("GETATTR".to_string(), 500.0), ("READ".to_string(), 100.0)]);
        assert_eq!(m.server_busy.len(), 1);
    }

    #[test]
    fn queued_share_hidden_when_enqueued_exceeds_arrived() {
        // Seen on RHEL 9.8 (5.14 with newer sunrpc): ~9 enqueues per arriving request.
        let t0 = Instant::now();
        let mut m = Model::default();
        m.ingest(with_nfsd(t0, nfsd(0, 0, 0, 0, 0)));
        m.ingest(with_nfsd(t0 + Duration::from_secs(5), nfsd(1000, 13_915, 129_650, 0, 0)));
        let r = m.server.as_ref().unwrap();
        assert_eq!(r.queued_pct, None);
        assert_eq!(r.busy_pct, Some(25.0), "busy comes from thread states, not the pool ratio");
    }

    #[test]
    fn server_counter_reset_skips_a_tick() {
        let t0 = Instant::now();
        let mut m = Model::default();
        m.ingest(with_nfsd(t0, nfsd(1000, 1000, 0, 0, 0)));
        m.ingest(with_nfsd(t0 + Duration::from_secs(1), nfsd(2000, 2000, 0, 0, 0)));
        assert!(m.server.is_some());
        // nfsd restarted: counters start over.
        m.ingest(with_nfsd(t0 + Duration::from_secs(2), nfsd(10, 10, 0, 0, 0)));
        assert_eq!(m.server, None);
        m.ingest(with_nfsd(t0 + Duration::from_secs(3), nfsd(110, 110, 0, 0, 0)));
        assert_eq!(m.server.as_ref().unwrap().calls_ps, 100.0);
    }

    #[test]
    fn server_rebaseline_and_errors() {
        let t0 = Instant::now();
        let mut m = Model::default();
        m.ingest(with_nfsd(t0, nfsd(0, 0, 0, 0, 0)));
        m.rebaseline(&with_nfsd(t0 + Duration::from_secs(60), nfsd(60_000, 0, 0, 0, 0)));
        m.ingest(with_nfsd(t0 + Duration::from_secs(62), nfsd(60_200, 0, 0, 0, 0)));
        assert_eq!(m.server.as_ref().unwrap().calls_ps, 100.0);
        m.ingest(sample(t0 + Duration::from_secs(64), vec![]));
        assert_eq!(m.server, None);
        assert_eq!(m.server_error.as_deref(), Some("none"));
    }
}
