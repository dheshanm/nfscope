//! Parsers for server-wide nfsd counters: `/proc/net/rpc/nfsd` and `/proc/fs/nfsd/pool_stats`.

use std::collections::BTreeMap;

/// NFSv3 procedure names, indexed by procedure number (`proc3` line).
pub const V3_OPS: [&str; 22] = [
    "NULL",
    "GETATTR",
    "SETATTR",
    "LOOKUP",
    "ACCESS",
    "READLINK",
    "READ",
    "WRITE",
    "CREATE",
    "MKDIR",
    "SYMLINK",
    "MKNOD",
    "REMOVE",
    "RMDIR",
    "RENAME",
    "LINK",
    "READDIR",
    "READDIRPLUS",
    "FSSTAT",
    "FSINFO",
    "PATHCONF",
    "COMMIT",
];

/// NFSv4 operation names, indexed by operation number (`proc4ops` line). 0–2 are unused.
pub const V4_OPS: [&str; 76] = [
    "",
    "",
    "",
    "ACCESS",
    "CLOSE",
    "COMMIT",
    "CREATE",
    "DELEGPURGE",
    "DELEGRETURN",
    "GETATTR",
    "GETFH",
    "LINK",
    "LOCK",
    "LOCKT",
    "LOCKU",
    "LOOKUP",
    "LOOKUPP",
    "NVERIFY",
    "OPEN",
    "OPENATTR",
    "OPEN_CONFIRM",
    "OPEN_DOWNGRADE",
    "PUTFH",
    "PUTPUBFH",
    "PUTROOTFH",
    "READ",
    "READDIR",
    "READLINK",
    "REMOVE",
    "RENAME",
    "RENEW",
    "RESTOREFH",
    "SAVEFH",
    "SECINFO",
    "SETATTR",
    "SETCLIENTID",
    "SETCLIENTID_CONFIRM",
    "VERIFY",
    "WRITE",
    "RELEASE_LOCKOWNER",
    "BACKCHANNEL_CTL",
    "BIND_CONN_TO_SESSION",
    "EXCHANGE_ID",
    "CREATE_SESSION",
    "DESTROY_SESSION",
    "FREE_STATEID",
    "GET_DIR_DELEGATION",
    "GETDEVICEINFO",
    "GETDEVICELIST",
    "LAYOUTCOMMIT",
    "LAYOUTGET",
    "LAYOUTRETURN",
    "SECINFO_NO_NAME",
    "SEQUENCE",
    "SET_SSV",
    "TEST_STATEID",
    "WANT_DELEGATION",
    "DESTROY_CLIENTID",
    "RECLAIM_COMPLETE",
    "ALLOCATE",
    "COPY",
    "COPY_NOTIFY",
    "DEALLOCATE",
    "IO_ADVISE",
    "LAYOUTERROR",
    "LAYOUTSTATS",
    "OFFLOAD_CANCEL",
    "OFFLOAD_STATUS",
    "READ_PLUS",
    "SEEK",
    "WRITE_SAME",
    "CLONE",
    "GETXATTR",
    "SETXATTR",
    "LISTXATTRS",
    "REMOVEXATTR",
];

/// Ops that ride along in (nearly) every request and say nothing about the workload.
pub const PLUMBING: [&str; 7] = ["NULL", "PUTFH", "PUTROOTFH", "GETFH", "SAVEFH", "RESTOREFH", "SEQUENCE"];

/// Thread-pool counters from `/proc/fs/nfsd/pool_stats`, summed over all pools.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PoolStats {
    pub arrived: u64,
    /// Requests that found no idle thread and had to queue.
    pub enqueued: u64,
    pub woken: u64,
    /// Not reported by every kernel.
    pub timedout: Option<u64>,
}

/// What the nfsd threads are doing right now (a snapshot, not a counter).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ThreadStates {
    /// On a CPU (`R`).
    pub running: u32,
    /// In uninterruptible sleep (`D`): almost always waiting on the exported filesystem.
    pub blocked: u32,
    /// Waiting for a request (`I`, or `S` on older kernels).
    pub idle: u32,
}

impl ThreadStates {
    pub fn busy(&self) -> u32 {
        self.running + self.blocked
    }

    pub fn total(&self) -> u32 {
        self.busy() + self.idle
    }

    /// Busy threads as a share of all threads (0–100).
    pub fn busy_pct(&self) -> Option<f64> {
        (self.total() > 0).then(|| self.busy() as f64 / self.total() as f64 * 100.0)
    }
}

/// Thread states averaged over the snapshots taken during one sampling interval.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ThreadLoad {
    pub samples: u32,
    /// Thread count at the last snapshot.
    pub total: u32,
    pub avg_busy: f64,
    pub avg_blocked: f64,
    pub avg_running: f64,
    pub peak_busy: u32,
    /// Share of snapshots in which no thread was idle (0–100): the "need more threads" signal.
    pub all_busy_pct: f64,
}

impl ThreadLoad {
    /// Average busy threads as a share of all threads (0–100).
    pub fn busy_pct(&self) -> Option<f64> {
        (self.total > 0).then(|| (self.avg_busy / self.total as f64 * 100.0).min(100.0))
    }
}

/// Collects thread-state snapshots between two samples.
#[derive(Debug, Clone, Default)]
pub struct ThreadAcc {
    n: u32,
    busy: u64,
    blocked: u64,
    running: u64,
    peak: u32,
    all_busy: u32,
    total: u32,
}

impl ThreadAcc {
    pub fn add(&mut self, t: ThreadStates) {
        self.n += 1;
        self.busy += t.busy() as u64;
        self.blocked += t.blocked as u64;
        self.running += t.running as u64;
        self.peak = self.peak.max(t.busy());
        self.all_busy += u32::from(t.idle == 0 && t.total() > 0);
        self.total = t.total();
    }

    /// The averages so far, or None if nothing was added.
    pub fn finish(&self) -> Option<ThreadLoad> {
        let n = self.n as f64;
        (self.n > 0).then(|| ThreadLoad {
            samples: self.n,
            total: self.total,
            avg_busy: self.busy as f64 / n,
            avg_blocked: self.blocked as f64 / n,
            avg_running: self.running as f64 / n,
            peak_busy: self.peak,
            all_busy_pct: self.all_busy as f64 / n * 100.0,
        })
    }
}

/// Parse `<state letter> <count>` lines (what `Source::nfsd_thread_states` produces).
pub fn parse_thread_states(text: &str) -> Option<ThreadStates> {
    let mut t = ThreadStates::default();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(state), Some(n)) = (it.next(), it.next().and_then(|n| n.parse::<u32>().ok())) else { continue };
        match state {
            "R" => t.running += n,
            "D" => t.blocked += n,
            _ => t.idle += n,
        }
    }
    (t.total() > 0).then_some(t)
}

/// Server-wide counters from `/proc/net/rpc/nfsd` (all cumulative since nfsd start).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NfsdStats {
    pub threads: Option<u32>,
    pub rpc_calls: u64,
    pub rpc_bad: u64,
    /// nfsd payload bytes read from / written to disk.
    pub io_read: u64,
    pub io_write: u64,
    pub rc_hits: u64,
    pub rc_misses: u64,
    pub proc3: Vec<u64>,
    pub proc4ops: Vec<u64>,
    pub pool: Option<PoolStats>,
    /// nfsd thread activity over the interval that ends at this sample.
    pub thread_load: Option<ThreadLoad>,
}

fn nums<'a>(it: impl Iterator<Item = &'a str>) -> Vec<u64> {
    it.map(|t| t.parse().unwrap_or(0)).collect()
}

/// `procN <count> v0 v1 ...`: the first number is how many values follow.
fn counted(rest: &[u64]) -> Vec<u64> {
    match rest.split_first() {
        Some((&n, vals)) => vals.iter().copied().take(n as usize).collect(),
        None => Vec::new(),
    }
}

pub fn parse_rpc_nfsd(text: &str) -> NfsdStats {
    let mut s = NfsdStats::default();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let Some(key) = it.next() else { continue };
        let v = nums(it);
        let at = |i: usize| v.get(i).copied().unwrap_or(0);
        match key {
            "th" => s.threads = v.first().map(|&n| n as u32),
            "rpc" => (s.rpc_calls, s.rpc_bad) = (at(0), at(1)),
            "io" => (s.io_read, s.io_write) = (at(0), at(1)),
            "rc" => (s.rc_hits, s.rc_misses) = (at(0), at(1)),
            "proc3" => s.proc3 = counted(&v),
            "proc4ops" => s.proc4ops = counted(&v),
            _ => {}
        }
    }
    s
}

/// Parse `pool_stats`, mapping columns by the `# pool ...` header (default order if absent).
pub fn parse_pool_stats(text: &str) -> Option<PoolStats> {
    let mut cols: Vec<String> =
        ["pool", "packets-arrived", "sockets-enqueued", "threads-woken", "threads-timedout"].map(String::from).into();
    let mut out: Option<PoolStats> = None;
    for line in text.lines() {
        if let Some(h) = line.strip_prefix('#') {
            cols = h.split_whitespace().map(String::from).collect();
            continue;
        }
        let v: Vec<&str> = line.split_whitespace().collect();
        if v.is_empty() {
            continue;
        }
        let get =
            |name: &str| -> Option<u64> { cols.iter().position(|c| c == name).and_then(|i| v.get(i)?.parse().ok()) };
        let p = out.get_or_insert_with(PoolStats::default);
        p.arrived += get("packets-arrived").unwrap_or(0);
        p.enqueued += get("sockets-enqueued").unwrap_or(0);
        p.woken += get("threads-woken").unwrap_or(0);
        if let Some(t) = get("threads-timedout") {
            *p.timedout.get_or_insert(0) += t;
        }
    }
    out
}

fn op_name(table: &[&'static str], i: usize) -> String {
    match table.get(i) {
        Some(n) if !n.is_empty() => (*n).to_string(),
        _ => format!("op#{i}"),
    }
}

impl NfsdStats {
    /// Workload op counts by name (v3 and v4 merged, plumbing ops left out).
    pub fn named_ops(&self) -> BTreeMap<String, u64> {
        let mut m = BTreeMap::new();
        for (table, vals) in [(&V3_OPS[..], &self.proc3), (&V4_OPS[..], &self.proc4ops)] {
            for (i, &n) in vals.iter().enumerate() {
                let name = op_name(table, i);
                if n > 0 && !PLUMBING.contains(&name.as_str()) {
                    *m.entry(name).or_insert(0) += n;
                }
            }
        }
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = "\
rc 0 2364 1094
fh 0 0 0 0 0
io 1245184 4505600
th 64 0 0.000 0.000 0.000 0.000 0.000 0.000 0.000 0.000 0.000 0.000
ra 32 0 0 0 0 0 0 0 0 0 0 0
net 3466 0 3466 2
rpc 3458 1 0 1 0
proc3 22 2 41 0 9 12 0 100 50 0 0 0 0 0 0 0 0 0 0 0 0 0 7
proc4 2 1 3456
{V4}
";

    /// A `proc4ops` line of `n` ops with the given (op number, count) set.
    fn v4line(n: usize, set: &[(usize, u64)]) -> String {
        let mut v = vec![0u64; n];
        for &(i, c) in set {
            v[i] = c;
        }
        format!("proc4ops {n} {}", v.iter().map(u64::to_string).collect::<Vec<_>>().join(" "))
    }

    #[test]
    fn old_kernel_full() {
        // ACCESS 5, GETATTR 400, PUTFH 900, READ 30, WRITE 60, SEQUENCE 900.
        let v4 = v4line(72, &[(3, 5), (9, 400), (22, 900), (25, 30), (38, 60), (53, 900)]);
        let s = parse_rpc_nfsd(&OLD.replace("{V4}", &v4));
        assert_eq!(s.threads, Some(64));
        assert_eq!((s.rpc_calls, s.rpc_bad), (3458, 1));
        assert_eq!((s.io_read, s.io_write), (1245184, 4505600));
        assert_eq!((s.rc_hits, s.rc_misses), (0, 2364));
        assert_eq!(s.proc3.len(), 22);
        assert_eq!(s.proc4ops.len(), 72);
        assert_eq!(s.proc4ops[25], 30); // READ
        let ops = s.named_ops();
        // v3 READ (100) + v4 READ (30); WRITE 50 + 60; GETATTR 41 + 400.
        assert_eq!(ops["READ"], 130);
        assert_eq!(ops["WRITE"], 110);
        assert_eq!(ops["GETATTR"], 441);
        assert_eq!(ops["COMMIT"], 7);
        assert!(!ops.contains_key("PUTFH"));
        assert!(!ops.contains_key("SEQUENCE"));
        assert!(!ops.contains_key("NULL"));
    }

    #[test]
    fn short_th_and_unknown_ops() {
        let s = parse_rpc_nfsd(&format!("th 8 0\nrpc 10 0 0 0 0\n{}\n", v4line(78, &[(3, 1), (77, 5)])));
        assert_eq!(s.threads, Some(8));
        assert_eq!(s.proc4ops.len(), 78);
        let ops = s.named_ops();
        assert_eq!(ops["ACCESS"], 1);
        assert_eq!(ops["op#77"], 5);
    }

    #[test]
    fn thread_states() {
        let t = parse_thread_states("I 117\nD 10\nR 1\nS 0\n").unwrap();
        assert_eq!(t, ThreadStates { running: 1, blocked: 10, idle: 117 });
        assert_eq!(t.busy(), 11);
        assert_eq!(t.busy_pct(), Some(11.0 / 128.0 * 100.0));
        assert_eq!(parse_thread_states(""), None);
    }

    #[test]
    fn thread_acc_averages() {
        let mut acc = ThreadAcc::default();
        assert_eq!(acc.finish(), None);
        let st = |r, d, i| ThreadStates { running: r, blocked: d, idle: i };
        acc.add(st(1, 3, 4)); // 4 busy
        acc.add(st(0, 0, 8)); // 0 busy
        acc.add(st(2, 6, 0)); // all 8 busy
        acc.add(st(1, 1, 6)); // 2 busy
        let l = acc.finish().unwrap();
        assert_eq!((l.samples, l.total, l.peak_busy), (4, 8, 8));
        assert_eq!((l.avg_busy, l.avg_blocked, l.avg_running), (3.5, 2.5, 1.0));
        assert_eq!(l.all_busy_pct, 25.0);
        assert_eq!(l.busy_pct(), Some(3.5 / 8.0 * 100.0));
    }

    #[test]
    fn pool_stats_header_mapping() {
        let p = parse_pool_stats(
            "# pool packets-arrived sockets-enqueued threads-woken threads-timedout\n0 100 5 95 2\n1 50 1 49 0\n",
        )
        .unwrap();
        assert_eq!(p, PoolStats { arrived: 150, enqueued: 6, woken: 144, timedout: Some(2) });

        let p = parse_pool_stats("# pool packets-arrived sockets-enqueued threads-woken\n0 10 1 9\n").unwrap();
        assert_eq!((p.arrived, p.enqueued, p.timedout), (10, 1, None));
        assert_eq!(parse_pool_stats(""), None);
    }
}
