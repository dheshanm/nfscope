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
use crate::parse::ss::parse_ss;
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
pub fn collect_with(src: &mut dyn Source, port: u16, acc: &mut IntervalAcc) -> RawSample {
    let mono = Instant::now();
    let socks = src.sockets(port).map(|t| parse_ss(&t)).map_err(|e| format!("{e:#}"));
    let clients =
        src.client_infos().map(|v| v.iter().map(|(id, t)| parse_info(id, t)).collect()).map_err(|e| format!("{e:#}"));
    snapshot(src, acc);
    let acc = std::mem::take(acc);
    let nfsd = src.nfsd_stats().map_err(|e| format!("{e:#}")).map(|t| {
        let mut n = parse_rpc_nfsd(&t);
        n.pool = src.pool_stats().ok().and_then(|p| parse_pool_stats(&p));
        n.thread_load = acc.threads.finish();
        n
    });
    let inflight = acc.inflight.finish();
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
    RawSample { mono, wall: now_hms(), socks, clients, nfsd, inflight }
}

pub fn spawn_sampler(
    spec: SourceSpec,
    port: u16,
    mut interval: Duration,
    tx: Sender<RawSample>,
    cmds: Receiver<SamplerCmd>,
) {
    thread::Builder::new()
        .name("sampler".into())
        .spawn(move || {
            let mut src = spec.build();
            let mut acc = IntervalAcc::default();
            loop {
                let started = Instant::now();
                if tx.send(collect_with(src.as_mut(), port, &mut acc)).is_err() {
                    return;
                }
                tracing::trace!("sample took {:?}", started.elapsed());
                // Until the next tick, snapshot the nfsd threads ~10x so the busy and in-flight
                // averages and peaks cover the whole interval. A new interval wakes us early.
                let deadline = started + interval;
                let step = (interval / 10).max(Duration::from_millis(100));
                let mut next = started + step;
                loop {
                    match cmds.recv_deadline(next.min(deadline)) {
                        Ok(SamplerCmd::SetInterval(d)) => {
                            interval = d;
                            tracing::info!("interval set to {d:?}");
                            break;
                        }
                        Err(RecvTimeoutError::Timeout) if Instant::now() >= deadline => break,
                        Err(RecvTimeoutError::Timeout) => {
                            snapshot(src.as_mut(), &mut acc);
                            next += step;
                        }
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
