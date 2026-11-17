//! Background threads: periodic sampler, on-demand open-state loader, full-path finder, reverse DNS.

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

use crate::files::{self, FindResult, OpenFiles, Target};
use crate::inflight::InflightAcc;
use crate::model::RawSample;
use crate::parse::nfsd::parse_info;
use crate::parse::rpc::{ThreadAcc, parse_pool_stats, parse_rpc_nfsd, parse_thread_states};
use crate::source::{Source, SourceSpec};

pub enum SamplerCmd {
    SetInterval(Duration),
}

pub fn now_hms() -> String {
    jiff::ZonedDateTime::now().strftime("%H:%M:%S").to_string()
}

/// Snapshots the sampler takes between two samples (~10 per interval).
#[derive(Debug, Clone, Default)]
pub struct IntervalAcc {
    /// nfsd thread scheduler states.
    pub threads: ThreadAcc,
    /// RPCs in flight.
    pub inflight: InflightAcc,
}

/// One sample with a single snapshot of the nfsd threads (tests, `peek`).
pub fn collect(src: &mut dyn Source, port: u16) -> RawSample {
    collect_with(src, port, &mut IntervalAcc::default())
}

/// Take one snapshot of the nfsd threads' states and of the RPCs they are working on.
fn snapshot(src: &mut dyn Source, acc: &mut IntervalAcc) {
    if let Some(t) = src.nfsd_thread_states().ok().and_then(|t| parse_thread_states(&t)) {
        acc.threads.add(t);
    }
    match src.rpc_status() {
        Ok(rpcs) => acc.inflight.add(&rpcs),
        Err(e) => acc.inflight.fail(format!("{e:#}")),
    }
}

/// One sample whose thread load and in-flight RPCs average `acc` (snapshots taken since the
/// last sample) plus a snapshot taken now. `acc` is reset for the next interval.
/// Single-threaded, for tests and `peek`; the live sampler hands the snapshots to [`Probe`].
pub fn collect_with(src: &mut dyn Source, port: u16, acc: &mut IntervalAcc) -> RawSample {
    let mono = Instant::now();
    let (socks, clients) = sockets_and_clients(src, port);
    snapshot(src, acc);
    let pool = src.pool_stats().ok();
    assemble(src, mono, socks, clients, Ok((std::mem::take(acc), pool)))
}

type Socks = Result<Vec<crate::parse::ss::Sock>, String>;
type Clients = Result<Vec<crate::parse::nfsd::ClientInfo>, String>;

fn sockets_and_clients(src: &mut dyn Source, port: u16) -> (Socks, Clients) {
    let socks = src.sockets(port).map_err(|e| format!("{e:#}"));
    let clients =
        src.client_infos().map(|v| v.iter().map(|(id, t)| parse_info(id, t)).collect()).map_err(|e| format!("{e:#}"));
    (socks, clients)
}

/// The interval's nfsd snapshots and pool stats, or for how long the probe has not answered.
type ProbeResult = Result<(IntervalAcc, Option<String>), Duration>;

fn assemble(src: &mut dyn Source, mono: Instant, socks: Socks, clients: Clients, probe: ProbeResult) -> RawSample {
    let nfsd_stalled = probe.as_ref().err().copied();
    let (acc, pool) = match probe {
        Ok((acc, pool)) => (Some(acc), pool),
        Err(_) => (None, None),
    };
    let nfsd = src.nfsd_stats().map_err(|e| format!("{e:#}")).map(|t| {
        let mut n = parse_rpc_nfsd(&t);
        n.pool = pool.as_deref().and_then(parse_pool_stats);
        n.thread_load = acc.as_ref().and_then(|a| a.threads.finish());
        n
    });
    let inflight = match (acc, nfsd_stalled) {
        (Some(a), _) => a.inflight.finish(),
        (None, Some(d)) => Err(format!("nfsd has not answered for {}", crate::fmt::duration(d.as_secs_f64()))),
        (None, None) => Err("no snapshots".into()),
    };
    if let Err(e) = &inflight {
        tracing::debug!("in-flight RPCs unavailable: {e}");
    }
    if let Err(e) = &nfsd {
        tracing::debug!("nfsd server stats unavailable: {e}");
    }
    if let Err(e) = &socks {
        tracing::warn!("socket sample failed: {e}");
    }
    if let Err(e) = &clients {
        tracing::debug!("nfsd client info unavailable: {e}");
    }
    RawSample { mono, wall: now_hms(), socks, clients, nfsd, inflight, nfsd_stalled }
}

/// How long the sampler waits for the probe's snapshots before sampling without them.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

enum ProbeMsg {
    /// Take a last snapshot, read the pool stats and send everything since the last collect.
    Collect(u64),
    /// Snapshot every this often.
    Step(Duration),
}

/// The nfsd thread-state, in-flight and pool-stats reads, on a thread of their own.
///
/// Reading `/proc/fs/nfsd/threads` or `pool_stats`, or dumping `rpc-status-get`, takes the
/// kernel's `nfsd_mutex`. While nfsd is being restarted or resized (`rpc.nfsd N`) that mutex
/// is held until every thread has stopped, which takes as long as the slowest request; with a
/// thread stuck on hung storage, forever. Doing these reads here keeps the socket rates live:
/// the sampler stops waiting after [`PROBE_TIMEOUT`] and says the nfsd figures are stalled.
pub struct Probe {
    tx: Sender<ProbeMsg>,
    rx: Receiver<(u64, IntervalAcc, Option<String>)>,
    seq: u64,
    /// When the probe first failed to answer, while it still hasn't.
    stalled_since: Option<Instant>,
}

impl Probe {
    pub fn spawn(mut src: Box<dyn Source>, step: Duration) -> Self {
        let (tx, reqs) = crossbeam_channel::unbounded();
        let (replies, rx) = crossbeam_channel::unbounded();
        thread::Builder::new()
            .name("nfsd-probe".into())
            .spawn(move || {
                let mut acc = IntervalAcc::default();
                let mut step = step;
                let mut next = Instant::now() + step;
                loop {
                    match reqs.recv_deadline(next) {
                        Ok(ProbeMsg::Collect(seq)) => {
                            snapshot(src.as_mut(), &mut acc);
                            let pool = src.pool_stats().ok();
                            if replies.send((seq, std::mem::take(&mut acc), pool)).is_err() {
                                return;
                            }
                        }
                        Ok(ProbeMsg::Step(d)) => {
                            step = d;
                            next = Instant::now() + step;
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            snapshot(src.as_mut(), &mut acc);
                            // After a long block, carry on from now rather than catching up.
                            next = (next + step).max(Instant::now());
                        }
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            })
            .expect("spawn nfsd probe thread");
        Self { tx, rx, seq: 0, stalled_since: None }
    }

    pub fn set_step(&self, step: Duration) {
        let _ = self.tx.send(ProbeMsg::Step(step));
    }

    /// Everything the probe gathered since the last collect, or for how long it has been stuck.
    fn collect(&mut self, timeout: Duration) -> ProbeResult {
        self.seq += 1;
        let asked = Instant::now();
        if self.tx.send(ProbeMsg::Collect(self.seq)).is_err() {
            return Err(self.stalled(asked));
        }
        let deadline = asked + timeout;
        // Answers to collects that timed out arrive late, once the probe is unstuck: skip them.
        while let Ok((seq, acc, pool)) = self.rx.recv_deadline(deadline) {
            if seq == self.seq {
                if let Some(since) = self.stalled_since.take() {
                    tracing::info!("nfsd answering again after {:?}", since.elapsed());
                }
                return Ok((acc, pool));
            }
        }
        Err(self.stalled(asked))
    }

    fn stalled(&mut self, asked: Instant) -> Duration {
        let since = *self.stalled_since.get_or_insert_with(|| {
            tracing::warn!("nfsd not answering (thread states / pool_stats / rpc-status blocked)");
            asked
        });
        since.elapsed()
    }
}

/// The nfsd threads are snapshotted ~10x per interval, so the busy and in-flight averages and
/// peaks cover the whole interval.
fn probe_step(interval: Duration) -> Duration {
    (interval / 10).max(Duration::from_millis(100))
}

/// Samples sockets, client records and nfsd counters every `interval`, with the nfsd thread
/// snapshots from a [`Probe`]. `build` makes the data source (one per thread).
pub fn spawn_sampler(
    build: impl Fn() -> Box<dyn Source> + Send + 'static,
    port: u16,
    mut interval: Duration,
    tx: Sender<RawSample>,
    cmds: Receiver<SamplerCmd>,
) {
    thread::Builder::new()
        .name("sampler".into())
        .spawn(move || {
            let mut src = build();
            let mut probe = Probe::spawn(build(), probe_step(interval));
            loop {
                let started = Instant::now();
                let (socks, clients) = sockets_and_clients(src.as_mut(), port);
                let snaps = probe.collect(PROBE_TIMEOUT);
                let sample = assemble(src.as_mut(), started, socks, clients, snaps);
                if tx.send(sample).is_err() {
                    return;
                }
                tracing::trace!("sample took {:?}", started.elapsed());
                // A new interval moves the next sample but doesn't take one early: a sample
                // milliseconds after the last would turn a single burst into a huge rate.
                let mut deadline = started + interval;
                loop {
                    match cmds.recv_deadline(deadline) {
                        Ok(SamplerCmd::SetInterval(d)) => {
                            interval = d;
                            deadline = started + interval;
                            probe.set_step(probe_step(interval));
                            tracing::info!("interval set to {d:?}");
                        }
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            }
        })
        .expect("spawn sampler thread");
}

pub type StatesResult = (IpAddr, Result<OpenFiles, String>);

pub fn spawn_states_worker(spec: SourceSpec, reqs: Receiver<IpAddr>, tx: Sender<StatesResult>) {
    thread::Builder::new()
        .name("states".into())
        .spawn(move || {
            let mut src = spec.build();
            for ip in reqs {
                let t = Instant::now();
                let res = files::load(src.as_mut(), ip);
                match &res {
                    Ok(of) => tracing::info!("{ip}: {} state entries in {:?}", of.entries, t.elapsed()),
                    Err(e) => tracing::warn!("{ip}: loading states failed: {e}"),
                }
                if tx.send((ip, res)).is_err() {
                    return;
                }
            }
        })
        .expect("spawn states thread");
}

/// A request to find the full paths of some open files on one export.
#[derive(Debug, Clone, PartialEq)]
pub struct FindReq {
    /// Increases with every request; the worker drops a search once a newer id is current.
    pub id: u64,
    pub ip: IpAddr,
    /// Mount point to search.
    pub export: String,
    pub targets: Vec<Target>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FindMsg {
    Progress { id: u64, dirs: u64 },
    Done { id: u64, res: Result<FindResult, String>, secs: f64 },
}

/// Runs one search at a time. `current` holds the id of the search that is still wanted (0 =
/// none): bumping it cancels the running search.
pub fn spawn_find_worker(spec: SourceSpec, reqs: Receiver<FindReq>, tx: Sender<FindMsg>, current: Arc<AtomicU64>) {
    thread::Builder::new()
        .name("find".into())
        .spawn(move || {
            let mut src = spec.build();
            for req in reqs {
                if current.load(Ordering::Relaxed) != req.id {
                    // Cancelled or replaced before it started. Still answer, or a drill-down
                    // that is waiting for this id keeps its spinner (and Esc) forever.
                    let stopped = FindResult { found: Default::default(), dirs: 0, end: files::FindEnd::Cancelled };
                    if tx.send(FindMsg::Done { id: req.id, res: Ok(stopped), secs: 0.0 }).is_err() {
                        return;
                    }
                    continue;
                }
                let t = Instant::now();
                let mut progress = |dirs| {
                    let _ = tx.send(FindMsg::Progress { id: req.id, dirs });
                    current.load(Ordering::Relaxed) == req.id
                };
                let res = src.find_paths(&req.export, &req.targets, &mut progress).map_err(|e| format!("{e:#}"));
                let secs = t.elapsed().as_secs_f64();
                match &res {
                    Ok(r) => tracing::info!(
                        "{}: found {}/{} paths under {} ({} dirs, {secs:.1}s, {:?})",
                        req.ip,
                        r.found.len(),
                        req.targets.len(),
                        req.export,
                        r.dirs,
                        r.end
                    ),
                    Err(e) => tracing::warn!("{}: path search failed: {e}", req.ip),
                }
                if tx.send(FindMsg::Done { id: req.id, res, secs }).is_err() {
                    return;
                }
            }
        })
        .expect("spawn find thread");
}

/// Reverse DNS on a small pool so one slow lookup does not hold up the rest.
pub fn spawn_dns_worker(spec: SourceSpec, reqs: Receiver<IpAddr>, tx: Sender<(IpAddr, Option<String>)>) {
    for i in 0..4 {
        let (spec, reqs, tx) = (spec.clone(), reqs.clone(), tx.clone());
        thread::Builder::new()
            .name(format!("dns-{i}"))
            .spawn(move || {
                let mut src = spec.build();
                for ip in reqs {
                    let name = src.resolve(ip);
                    tracing::debug!("resolved {ip} -> {name:?}");
                    if tx.send((ip, name)).is_err() {
                        return;
                    }
                }
            })
            .expect("spawn dns thread");
    }
}
