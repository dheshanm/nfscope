//! In-flight RPC snapshots averaged over one sampling interval: which clients and operations
//! are holding nfsd threads, and the longest-running requests.
//!
//! The sampler snapshots the kernel's in-flight list ~10x per interval (alongside the thread
//! states). An RPC counts once per snapshot it appears in, so a client's average is the number
//! of nfsd threads it kept busy over the interval (by Little's law: its request rate times how
//! long its requests take).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};

use crate::parse::inflight::InflightRpc;

/// Longest-running requests kept per interval.
pub const SLOWEST: usize = 50;

/// One client's share of the nfsd threads over the interval.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClientLoad {
    /// None for requests on a transport without an IP address.
    pub ip: Option<IpAddr>,
    /// RPCs in flight, averaged over the snapshots.
    pub avg: f64,
    /// Most RPCs in flight in any one snapshot.
    pub peak: u32,
    /// Age of the oldest request seen.
    pub oldest_us: u64,
    /// This client's requests by operation, busiest first. Their averages add up to `avg`.
    pub ops: Vec<OpLoad>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OpLoad {
    pub name: String,
    pub avg: f64,
    pub oldest_us: u64,
}

/// A distinct request seen during the interval.
#[derive(Debug, Clone, PartialEq)]
pub struct SlowRpc {
    pub client: Option<IpAddr>,
    /// `v4 READ`, …
    pub label: String,
    /// Oldest age it was seen at.
    pub age_us: u64,
    /// Still in flight at the last snapshot.
    pub running: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct InflightLoad {
    pub snapshots: u32,
    /// RPCs in flight, averaged over the snapshots: nfsd threads busy with a request.
    pub avg: f64,
    pub peak: u32,
    /// In flight at the last snapshot.
    pub now: u32,
    /// Busiest first.
    pub clients: Vec<ClientLoad>,
    /// Busiest first.
    pub ops: Vec<OpLoad>,
    /// Longest-running first, at most [`SLOWEST`].
    pub slowest: Vec<SlowRpc>,
}

impl InflightLoad {
    pub fn client(&self, ip: IpAddr) -> Option<&ClientLoad> {
        self.clients.iter().find(|c| c.ip == Some(ip))
    }
}

#[derive(Debug, Clone, Default)]
struct Sum {
    n: u64,
    peak: u32,
    oldest: u64,
}

/// Collects in-flight snapshots between two samples.
#[derive(Debug, Clone, Default)]
pub struct InflightAcc {
    snaps: u32,
    total: u64,
    peak: u32,
    now: u32,
    clients: HashMap<Option<IpAddr>, Sum>,
    ops: HashMap<String, Sum>,
    client_ops: HashMap<(Option<IpAddr>, String), Sum>,
    /// By (client address and port, XID): one entry per distinct request.
    seen: HashMap<(Option<SocketAddr>, u32), SlowRpc>,
    /// Why the last snapshot failed, if it did.
    error: Option<String>,
}

impl InflightAcc {
    pub fn add(&mut self, rpcs: &[InflightRpc]) {
        let n = rpcs.len() as u32;
        self.snaps += 1;
        self.total += n as u64;
        self.peak = self.peak.max(n);
        self.now = n;
        self.error = None;
        for s in self.seen.values_mut() {
            s.running = false;
        }
        let mut per_client: HashMap<Option<IpAddr>, u32> = HashMap::new();
        for r in rpcs {
            let ip = r.ip();
            *per_client.entry(ip).or_default() += 1;
            let c = self.clients.entry(ip).or_default();
            c.oldest = c.oldest.max(r.age_us);
            let op = r.op_name();
            for o in [self.client_ops.entry((ip, op.clone())).or_default(), self.ops.entry(op).or_default()] {
                o.n += 1;
                o.oldest = o.oldest.max(r.age_us);
            }
            let s = self.seen.entry((r.client, r.xid)).or_insert_with(|| SlowRpc {
                client: ip,
                label: r.label(),
                age_us: 0,
                running: true,
            });
            s.age_us = s.age_us.max(r.age_us);
            s.running = true;
        }
        for (ip, k) in per_client {
            let c = self.clients.entry(ip).or_default();
            c.n += k as u64;
            c.peak = c.peak.max(k);
        }
    }

    pub fn fail(&mut self, e: String) {
        self.error = Some(e);
    }

    /// The interval's load. An error if no snapshot succeeded.
    pub fn finish(&self) -> Result<InflightLoad, String> {
        if self.snaps == 0 {
            return Err(self.error.clone().unwrap_or_else(|| "not sampled yet".into()));
        }
        let snaps = self.snaps as f64;
        let op_load =
            |name: &str, s: &Sum| OpLoad { name: name.to_string(), avg: s.n as f64 / snaps, oldest_us: s.oldest };
        let by_load = |a: &OpLoad, b: &OpLoad| {
            b.avg.total_cmp(&a.avg).then(b.oldest_us.cmp(&a.oldest_us)).then(a.name.cmp(&b.name))
        };
        let mut per_client: HashMap<Option<IpAddr>, Vec<OpLoad>> = HashMap::new();
        for ((ip, name), s) in &self.client_ops {
            per_client.entry(*ip).or_default().push(op_load(name, s));
        }
        let mut clients: Vec<ClientLoad> = self
            .clients
            .iter()
            .map(|(ip, s)| {
                let mut ops = per_client.remove(ip).unwrap_or_default();
                ops.sort_by(by_load);
                ClientLoad { ip: *ip, avg: s.n as f64 / snaps, peak: s.peak, oldest_us: s.oldest, ops }
            })
            .collect();
        clients.sort_by(|a, b| b.avg.total_cmp(&a.avg).then(b.oldest_us.cmp(&a.oldest_us)).then(a.ip.cmp(&b.ip)));
        let mut ops: Vec<OpLoad> = self.ops.iter().map(|(name, s)| op_load(name, s)).collect();
        ops.sort_by(by_load);
        let mut slowest: Vec<SlowRpc> = self.seen.values().cloned().collect();
        slowest.sort_by(|a, b| b.age_us.cmp(&a.age_us).then(a.client.cmp(&b.client)).then(a.label.cmp(&b.label)));
        slowest.truncate(SLOWEST);
        Ok(InflightLoad {
            snapshots: self.snaps,
            avg: self.total as f64 / snaps,
            peak: self.peak,
            now: self.now,
            clients,
            ops,
            slowest,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::inflight::parse_snapshot;

    #[test]
    fn averages_and_tracks_long_runners() {
        let mut acc = InflightAcc::default();
        assert!(acc.finish().is_err());
        acc.fail("nope".into());
        assert_eq!(acc.finish(), Err("nope".into()));
        // A slow READ from .11 spans all three snapshots; .12 has a short WRITE in one.
        acc.add(&parse_snapshot(
            "10.0.0.11:700 xid=0x1 ops=53,22,25 age_us=100000\n\
             10.0.0.12:701 xid=0x9 ops=53,22,38 age_us=2000\n",
        ));
        acc.add(&parse_snapshot("10.0.0.11:700 xid=0x1 ops=53,22,25 age_us=300000\n"));
        acc.add(&parse_snapshot(
            "10.0.0.11:700 xid=0x1 ops=53,22,25 age_us=500000\n\
             10.0.0.11:700 xid=0x2 ops=53,22,25 age_us=1000\n",
        ));
        let l = acc.finish().unwrap();
        assert_eq!((l.snapshots, l.peak, l.now), (3, 2, 2));
        assert!((l.avg - 5.0 / 3.0).abs() < 1e-9);
        let ip11: IpAddr = "10.0.0.11".parse().unwrap();
        let c = l.client(ip11).unwrap();
        assert!((c.avg - 4.0 / 3.0).abs() < 1e-9);
        assert_eq!((c.peak, c.oldest_us), (2, 500000));
        assert_eq!(l.clients[0].ip, Some(ip11), "busiest first");
        assert_eq!(l.ops[0].name, "READ");
        assert_eq!(l.ops[1].name, "WRITE");
        // Per client, by op: .11's 4 sightings were all READs; .12 had only its WRITE.
        let names = |c: &ClientLoad| c.ops.iter().map(|o| (o.name.clone(), o.avg)).collect::<Vec<_>>();
        assert_eq!(names(c), vec![("READ".to_string(), 4.0 / 3.0)]);
        assert_eq!(c.ops[0].oldest_us, 500000);
        let c12 = l.client("10.0.0.12".parse().unwrap()).unwrap();
        assert_eq!(names(c12), vec![("WRITE".to_string(), 1.0 / 3.0)]);
        assert_eq!(c12.ops[0].oldest_us, 2000);
        assert_eq!(l.slowest.len(), 3, "one entry per distinct request");
        assert_eq!(
            l.slowest[0],
            SlowRpc { client: Some(ip11), label: "v4 READ".into(), age_us: 500000, running: true }
        );
        assert!(
            !l.slowest.iter().find(|s| s.label == "v4 WRITE").unwrap().running,
            "finished before the last snapshot"
        );
    }

    #[test]
    fn same_xid_from_different_connections_is_two_requests() {
        let mut acc = InflightAcc::default();
        acc.add(&parse_snapshot("10.0.0.1:700 xid=0x5 age_us=1\n10.0.0.1:701 xid=0x5 age_us=2\n"));
        assert_eq!(acc.finish().unwrap().slowest.len(), 2);
    }

    #[test]
    fn idle_snapshots_lower_the_average() {
        let mut acc = InflightAcc::default();
        acc.add(&parse_snapshot("10.0.0.1:700 xid=0x5 age_us=1\n"));
        acc.add(&[]);
        let l = acc.finish().unwrap();
        assert_eq!((l.avg, l.now), (0.5, 0));
        assert_eq!(l.clients[0].avg, 0.5);
    }
}
