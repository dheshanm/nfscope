//! Where raw data comes from: the live system (`ss` + `/proc`) or a fixture directory.

use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::Command;

use color_eyre::eyre::{Result, WrapErr, eyre};

use crate::files::{FindEnd, FindResult, Target, find_paths};
use crate::parse::inflight::{InflightRpc, parse_snapshot};
use crate::parse::ss::{Sock, parse_ss, render_ss};

/// How long a full-path search may walk an export before giving up.
pub const FIND_BUDGET: std::time::Duration = std::time::Duration::from_secs(600);

pub trait Source: Send {
    /// Raw `ss -tniH` text for established sockets with local port `port`.
    fn sockets(&mut self, port: u16) -> Result<String>;
    /// `(client dir id, info file text)` for every NFSv4 client.
    fn client_infos(&mut self) -> Result<Vec<(String, String)>>;
    /// Text of `clients/<id>/states`.
    fn states(&mut self, id: &str) -> Result<String>;
    /// Text of `/proc/self/mountinfo`.
    fn mountinfo(&mut self) -> Result<String>;
    /// Reverse lookup for `ip`.
    fn resolve(&mut self, ip: IpAddr) -> Option<String>;
    /// Text of `/proc/net/rpc/nfsd`.
    fn nfsd_stats(&mut self) -> Result<String>;
    /// Text of `/proc/fs/nfsd/pool_stats`.
    fn pool_stats(&mut self) -> Result<String>;
    /// Current scheduler state of the nfsd threads, as `<state letter> <count>` lines.
    fn nfsd_thread_states(&mut self) -> Result<String>;
    /// The RPCs nfsd threads are working on right now (nfsd netlink `rpc-status-get`).
    fn rpc_status(&mut self) -> Result<Vec<InflightRpc>>;
    /// Full paths of `targets` on the filesystem mounted at `root` (see [`find_paths`]).
    fn find_paths(
        &mut self,
        root: &str,
        targets: &[Target],
        progress: &mut dyn FnMut(u64) -> bool,
    ) -> Result<FindResult>;
}

/// Continue a series of counter snapshots past its end: every number in `last` that is not
/// below the matching number in `prev` becomes `last + k * (last - prev)`. Everything else
/// (labels, counters that went down, lines that don't line up) is kept as in `last`.
pub fn extrapolate_text(prev: &str, last: &str, k: u64) -> String {
    let mut out = String::with_capacity(last.len());
    let mut prev_lines = prev.lines();
    for line in last.lines() {
        let p: Vec<&str> = prev_lines.next().unwrap_or("").split_whitespace().collect();
        let toks: Vec<String> = line
            .split_whitespace()
            .enumerate()
            .map(|(i, t)| match (t.parse::<u64>(), p.get(i).and_then(|x| x.parse::<u64>().ok())) {
                (Ok(l), Some(pv)) if l >= pv => (l + k * (l - pv)).to_string(),
                _ => t.to_string(),
            })
            .collect();
        out.push_str(&toks.join(" "));
        out.push('\n');
    }
    out
}

#[derive(Debug, Clone)]
pub enum SourceSpec {
    Live { nfsd_dir: PathBuf },
    Fixture(PathBuf),
}

impl SourceSpec {
    pub fn build(&self) -> Box<dyn Source> {
        match self {
            SourceSpec::Live { nfsd_dir } => Box::new(LiveSource::new(nfsd_dir.clone())),
            SourceSpec::Fixture(dir) => Box::new(FixtureSource::new(dir.clone())),
        }
    }
}

fn read_client_dirs(dir: &Path) -> Result<Vec<(String, String)>> {
    let rd = fs::read_dir(dir).wrap_err_with(|| format!("reading {}", dir.display()))?;
    let mut out = Vec::new();
    for ent in rd.flatten() {
        let id = ent.file_name().to_string_lossy().into_owned();
        match fs::read_to_string(ent.path().join("info")) {
            Ok(t) => out.push((id, t)),
            // Client went away between readdir and read.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).wrap_err_with(|| format!("reading {}/info", ent.path().display())),
        }
    }
    out.sort();
    Ok(out)
}

// ---------------------------------------------------------------------------------------------

pub struct LiveSource {
    pub nfsd_dir: PathBuf,
    /// `/proc/<pid>/stat` of each nfsd thread, so thread-state snapshots (taken ~10x per
    /// interval) read ~threads small files instead of scanning all of /proc.
    nfsd_stats: Vec<PathBuf>,
    /// `/proc/fs/nfsd/threads` when `nfsd_stats` was built; a change triggers a rescan.
    nfsd_threads_at_scan: Option<String>,
    /// When /proc was last scanned for nfsd threads.
    last_rescan: Option<std::time::Instant>,
    rpc_status: RpcStatus,
}

/// While nfsd has no threads (stopped), how often /proc is scanned for them again.
const IDLE_RESCAN: std::time::Duration = std::time::Duration::from_secs(5);

impl LiveSource {
    pub fn new(nfsd_dir: PathBuf) -> Self {
        Self {
            nfsd_dir,
            nfsd_stats: Vec::new(),
            nfsd_threads_at_scan: None,
            last_rescan: None,
            rpc_status: RpcStatus::default(),
        }
    }

    fn configured_threads(&self) -> Option<String> {
        let p = self.nfsd_dir.parent()?.join("threads");
        fs::read_to_string(p).ok().map(|t| t.trim().to_string())
    }

    /// Find every nfsd kernel thread (one /proc/<pid> each: `pid (nfsd) S ...`).
    fn rescan_nfsd(&mut self) -> Result<()> {
        self.nfsd_threads_at_scan = self.configured_threads();
        self.nfsd_stats.clear();
        for ent in fs::read_dir("/proc")?.flatten() {
            if !ent.file_name().to_str().is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit())) {
                continue;
            }
            let p = ent.path().join("stat");
            if fs::read_to_string(&p).is_ok_and(|s| nfsd_state(&s).is_some()) {
                self.nfsd_stats.push(p);
            }
        }
        Ok(())
    }

    /// `<state> <count>` lines from the cached threads, or None if the cache is stale.
    fn cached_thread_states(&self) -> Option<String> {
        if self.nfsd_stats.is_empty() || self.configured_threads() != self.nfsd_threads_at_scan {
            return None;
        }
        let mut counts: std::collections::BTreeMap<char, u32> = Default::default();
        for p in &self.nfsd_stats {
            // A thread that exited (or a pid reused by something else) means rescan.
            let state = nfsd_state(&fs::read_to_string(p).ok()?)?;
            *counts.entry(state).or_default() += 1;
        }
        Some(counts.iter().map(|(s, n)| format!("{s} {n}\n")).collect())
    }
}

/// After a failed in-flight dump, how long to keep reporting that error before trying again.
/// The sampler asks ~10x per interval, and most failures (old kernel, nfsd stopped) persist.
const RPC_STATUS_RETRY: std::time::Duration = std::time::Duration::from_secs(10);

/// The nfsd generic-netlink connection used for in-flight RPCs, opened on first use.
#[derive(Default)]
struct RpcStatus {
    /// Socket and the nfsd family id.
    #[cfg(target_os = "linux")]
    conn: Option<(crate::netlink::GenlSocket, u16)>,
    failed: Option<(std::time::Instant, String)>,
}

impl RpcStatus {
    fn get(&mut self) -> Result<Vec<InflightRpc>> {
        if let Some((at, e)) = &self.failed
            && at.elapsed() < RPC_STATUS_RETRY
        {
            return Err(eyre!("{e}"));
        }
        match self.dump() {
            Ok(v) => {
                self.failed = None;
                Ok(v)
            }
            Err(e) => {
                #[cfg(target_os = "linux")]
                {
                    self.conn = None;
                }
                if self.failed.is_none() {
                    tracing::info!("in-flight RPCs unavailable: {e}");
                }
                self.failed = Some((std::time::Instant::now(), e.clone()));
                Err(eyre!(e))
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn dump(&mut self) -> std::result::Result<Vec<InflightRpc>, String> {
        use crate::netlink::{GenlSocket, attrs, monotonic_us};
        use crate::parse::inflight::RPC_STATUS_GET;
        let why = |e: std::io::Error| match e.raw_os_error() {
            Some(libc::ENODEV) => "nfsd is not running".to_string(),
            Some(libc::EOPNOTSUPP) => "this kernel's nfsd netlink has no rpc-status (needs Linux 6.7+)".to_string(),
            Some(libc::EPERM | libc::EACCES) => format!("nfsd netlink: {e} (run as root)"),
            _ => format!("nfsd netlink: {e}"),
        };
        if self.conn.is_none() {
            let mut s = GenlSocket::open().map_err(why)?;
            let id = s
                .family_id("nfsd")
                .map_err(why)?
                .ok_or("no nfsd netlink family (needs Linux 6.7+ with nfsd loaded)")?;
            self.conn = Some((s, id));
        }
        let (s, id) = self.conn.as_mut().expect("opened above");
        let mut raw: Vec<Vec<u8>> = Vec::new();
        s.dump(*id, RPC_STATUS_GET, 1, |a| raw.push(a.to_vec())).map_err(why)?;
        // Read the clock after the dump: every start stamp is then in the past.
        let now = monotonic_us();
        Ok(raw.iter().filter_map(|a| InflightRpc::from_attrs(attrs(a), now)).collect())
    }

    #[cfg(not(target_os = "linux"))]
    fn dump(&mut self) -> std::result::Result<Vec<InflightRpc>, String> {
        Err("in-flight RPCs need Linux (nfsd netlink)".into())
    }
}

/// Scheduler state letter from a `/proc/<pid>/stat` line (`pid (comm) state ppid ...`), if the
/// task is an nfsd kernel thread: comm exactly `nfsd` and parent kthreadd (pid 2), so a user
/// process that happens to be called `nfsd` is not counted.
fn nfsd_state(stat: &str) -> Option<char> {
    let (head, rest) = stat.rsplit_once(") ")?;
    let (pid, comm) = head.split_once(" (")?;
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let ppid = fields.next()?;
    (pid.bytes().all(|b| b.is_ascii_digit()) && comm == "nfsd" && ppid == "2").then_some(state)
}

impl Source for LiveSource {
    fn sockets(&mut self, port: u16) -> Result<String> {
        let out = Command::new("ss")
            .args(["-tniH", "state", "established", &format!("( sport = :{port} )")])
            // Newer ss sizes its columns to $COLUMNS when not on a terminal; keep its layout fixed.
            .env_remove("COLUMNS")
            .output()
            .wrap_err("running `ss` (is iproute2 installed?)")?;
        if !out.status.success() {
            return Err(eyre!("ss failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn client_infos(&mut self) -> Result<Vec<(String, String)>> {
        read_client_dirs(&self.nfsd_dir)
    }

    fn states(&mut self, id: &str) -> Result<String> {
        let p = self.nfsd_dir.join(id).join("states");
        fs::read_to_string(&p).wrap_err_with(|| format!("reading {}", p.display()))
    }

    fn mountinfo(&mut self) -> Result<String> {
        Ok(fs::read_to_string("/proc/self/mountinfo")?)
    }

    fn resolve(&mut self, ip: IpAddr) -> Option<String> {
        dns_lookup::lookup_addr(&ip).ok()
    }

    fn nfsd_stats(&mut self) -> Result<String> {
        let p = "/proc/net/rpc/nfsd";
        let mut t = fs::read_to_string(p).wrap_err_with(|| format!("reading {p}"))?;
        // Some kernels leave the `th` line out; the thread count is also in the nfsd fs.
        if !t.lines().any(|l| l.starts_with("th ")) {
            let threads = self.nfsd_dir.parent().map(|d| d.join("threads"));
            if let Some(n) = threads.and_then(|p| fs::read_to_string(p).ok()) {
                t.push_str(&format!("th {}\n", n.trim()));
            }
        }
        Ok(t)
    }

    fn pool_stats(&mut self) -> Result<String> {
        let p = self.nfsd_dir.parent().ok_or_else(|| eyre!("no nfsd dir"))?.join("pool_stats");
        fs::read_to_string(&p).wrap_err_with(|| format!("reading {}", p.display()))
    }

    fn nfsd_thread_states(&mut self) -> Result<String> {
        if let Some(t) = self.cached_thread_states() {
            return Ok(t);
        }
        // The last scan found none (nfsd stopped): don't read all of /proc ~10x a second.
        if self.nfsd_stats.is_empty() && self.last_rescan.is_some_and(|t| t.elapsed() < IDLE_RESCAN) {
            return Err(eyre!("no nfsd threads running"));
        }
        self.last_rescan = Some(std::time::Instant::now());
        self.rescan_nfsd()?;
        self.cached_thread_states().ok_or_else(|| eyre!("no nfsd threads running"))
    }

    fn rpc_status(&mut self) -> Result<Vec<InflightRpc>> {
        self.rpc_status.get()
    }

    fn find_paths(
        &mut self,
        root: &str,
        targets: &[Target],
        progress: &mut dyn FnMut(u64) -> bool,
    ) -> Result<FindResult> {
        find_paths(Path::new(root), targets, FIND_BUDGET, progress).wrap_err_with(|| format!("searching {root}"))
    }
}

// ---------------------------------------------------------------------------------------------

/// Replays captured data from a directory:
///
/// ```text
/// DIR/ss/*.txt              ss snapshots, replayed in name order
/// DIR/clients/<id>/info     NFSv4 client info
/// DIR/clients/<id>/states   open state
/// DIR/mountinfo             (optional) /proc/self/mountinfo
/// DIR/hosts                 (optional) "ip name" lines used for reverse DNS
/// DIR/rpc/*.txt             (optional) /proc/net/rpc/nfsd snapshots, replayed in name order
/// DIR/pool/*.txt            (optional) /proc/fs/nfsd/pool_stats snapshots, likewise
/// DIR/threads/*.txt         (optional) nfsd thread states (`R 1` / `D 8` / `I 55` lines), cycled
/// DIR/inflight/*.txt        (optional) in-flight RPCs (`nfscope --dump-inflight` lines), cycled
/// DIR/paths                 (optional) "<inode, decimal> <full path>" lines answering find_paths
/// ```
///
/// After the captured snapshots run out, it extrapolates from the last two with a
/// deterministic wobble so the display keeps moving.
/// Current counters plus the fixed per-socket (read, write) delta used for extrapolation.
type Synth = (Vec<Sock>, Vec<(u64, u64)>);

pub struct FixtureSource {
    dir: PathBuf,
    tick: usize,
    rpc_tick: u64,
    pool_tick: u64,
    threads_tick: usize,
    inflight_tick: usize,
    /// (current counters, per-socket base delta) once replay is exhausted.
    synth: Option<Synth>,
}

impl FixtureSource {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, tick: 0, rpc_tick: 0, pool_tick: 0, threads_tick: 0, inflight_tick: 0, synth: None }
    }

    fn snapshots(&self) -> Result<Vec<PathBuf>> {
        self.series("ss")
    }

    /// `DIR/<sub>/*.txt` in name order.
    fn series(&self, sub: &str) -> Result<Vec<PathBuf>> {
        let mut v: Vec<PathBuf> = fs::read_dir(self.dir.join(sub))
            .wrap_err_with(|| format!("fixture dir has no {sub}/ subdirectory"))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "txt"))
            .collect();
        v.sort();
        Ok(v)
    }
}

/// Snapshot `t` of a counter series, extrapolated linearly from the last two once it runs out.
fn replay(files: &[PathBuf], t: u64) -> Result<String> {
    let n = files.len() as u64;
    if n == 0 {
        return Err(eyre!("empty series"));
    }
    if t < n {
        return Ok(fs::read_to_string(&files[t as usize])?);
    }
    let last = fs::read_to_string(&files[n as usize - 1])?;
    let prev = if n > 1 { fs::read_to_string(&files[n as usize - 2])? } else { last.clone() };
    Ok(extrapolate_text(&prev, &last, t - n + 1))
}

impl Source for FixtureSource {
    fn sockets(&mut self, _port: u16) -> Result<String> {
        let snaps = self.snapshots()?;
        if snaps.is_empty() {
            return Ok(String::new());
        }
        let t = self.tick;
        self.tick += 1;
        if t < snaps.len() {
            return Ok(fs::read_to_string(&snaps[t])?);
        }
        if self.synth.is_none() {
            let last = parse_ss(&fs::read_to_string(&snaps[snaps.len() - 1])?);
            let prev =
                if snaps.len() > 1 { parse_ss(&fs::read_to_string(&snaps[snaps.len() - 2])?) } else { Vec::new() };
            let base = last
                .iter()
                .map(|s| {
                    prev.iter().find(|p| p.key() == s.key()).map_or((0, 0), |p| {
                        (s.bytes_acked.saturating_sub(p.bytes_acked), s.bytes_received.saturating_sub(p.bytes_received))
                    })
                })
                .collect();
            self.synth = Some((last, base));
        }
        let (cur, base) = self.synth.as_mut().unwrap();
        for (i, (s, (dr, dw))) in cur.iter_mut().zip(base.iter()).enumerate() {
            let phase = (t as f64) * 0.45 + i as f64 * 1.3;
            let f_r = (0.6 + 0.6 * phase.sin()).max(0.0);
            let f_w = (0.6 + 0.6 * (phase * 0.7 + 2.0).cos()).max(0.0);
            s.bytes_acked += (*dr as f64 * f_r) as u64;
            s.bytes_received += (*dw as f64 * f_w) as u64;
        }
        Ok(render_ss(cur, "10.0.0.1:2049"))
    }

    fn client_infos(&mut self) -> Result<Vec<(String, String)>> {
        read_client_dirs(&self.dir.join("clients"))
    }

    fn states(&mut self, id: &str) -> Result<String> {
        Ok(fs::read_to_string(self.dir.join("clients").join(id).join("states"))?)
    }

    fn mountinfo(&mut self) -> Result<String> {
        Ok(fs::read_to_string(self.dir.join("mountinfo")).unwrap_or_default())
    }

    fn resolve(&mut self, ip: IpAddr) -> Option<String> {
        let hosts = fs::read_to_string(self.dir.join("hosts")).ok()?;
        hosts.lines().find_map(|l| {
            let mut it = l.split_whitespace();
            let a: IpAddr = it.next()?.parse().ok()?;
            (a == ip).then(|| it.next().map(str::to_string)).flatten()
        })
    }

    fn nfsd_stats(&mut self) -> Result<String> {
        let files = self.series("rpc")?;
        self.rpc_tick += 1;
        replay(&files, self.rpc_tick - 1)
    }

    fn pool_stats(&mut self) -> Result<String> {
        let files = self.series("pool")?;
        self.pool_tick += 1;
        replay(&files, self.pool_tick - 1)
    }

    fn nfsd_thread_states(&mut self) -> Result<String> {
        // A snapshot, not a counter: cycle through the captures instead of extrapolating.
        let files = self.series("threads")?;
        if files.is_empty() {
            return Err(eyre!("empty series"));
        }
        self.threads_tick += 1;
        Ok(fs::read_to_string(&files[(self.threads_tick - 1) % files.len()])?)
    }

    fn rpc_status(&mut self) -> Result<Vec<InflightRpc>> {
        // Snapshots too: cycle through them.
        let files = self.series("inflight")?;
        if files.is_empty() {
            return Err(eyre!("empty series"));
        }
        self.inflight_tick += 1;
        Ok(parse_snapshot(&fs::read_to_string(&files[(self.inflight_tick - 1) % files.len()])?))
    }

    fn find_paths(
        &mut self,
        root: &str,
        targets: &[Target],
        _progress: &mut dyn FnMut(u64) -> bool,
    ) -> Result<FindResult> {
        let text = fs::read_to_string(self.dir.join("paths")).unwrap_or_default();
        let mut found = std::collections::HashMap::new();
        for l in text.lines() {
            let Some((ino, path)) = l.split_once(' ') else { continue };
            let Ok(ino) = ino.parse::<u64>() else { continue };
            let p = PathBuf::from(path);
            if p.starts_with(root)
                && targets.iter().any(|t| t.ino == ino && p.file_name().is_some_and(|n| *n == *t.name))
            {
                found.insert(ino, p);
            }
        }
        let end = if found.len() == targets.len() { FindEnd::AllFound } else { FindEnd::Exhausted };
        Ok(FindResult { found, dirs: text.lines().count() as u64, end })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_nfsd_threads_in_proc_stat() {
        assert_eq!(nfsd_state("1234 (nfsd) I 2 0 0 0 -1 ..."), Some('I'));
        assert_eq!(nfsd_state("1234 (nfsd) D 2 0 0"), Some('D'));
        assert_eq!(nfsd_state("99 (lockd) S 2 0 0"), None);
        assert_eq!(nfsd_state("77 (not (nfsd) R 1 0"), None, "comm with parentheses");
        assert_eq!(nfsd_state("4321 (nfsd) S 1000 0 0"), None, "user process named nfsd");
        assert_eq!(nfsd_state("garbage"), None);
    }

    #[test]
    fn extrapolates_counters_only() {
        let prev = "# pool packets-arrived\n0 100 5\nrpc 10 3\n";
        let last = "# pool packets-arrived\n0 150 4\nrpc 30 3\n";
        assert_eq!(extrapolate_text(prev, last, 2), "# pool packets-arrived\n0 250 4\nrpc 70 3\n");
    }
}
