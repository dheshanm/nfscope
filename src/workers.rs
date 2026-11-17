//! Background threads: periodic sampler, on-demand open-state loader, reverse DNS.

use std::net::IpAddr;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

use crate::files::{self, OpenFiles};
use crate::model::RawSample;
use crate::parse::nfsd::parse_info;
use crate::parse::rpc::{parse_pool_stats, parse_rpc_nfsd, parse_thread_states};
use crate::parse::ss::parse_ss;
use crate::source::{Source, SourceSpec};

pub enum SamplerCmd {
    SetInterval(Duration),
}

pub fn now_hms() -> String {
    jiff::ZonedDateTime::now().strftime("%H:%M:%S").to_string()
}

pub fn collect(src: &mut dyn Source, port: u16) -> RawSample {
    let mono = Instant::now();
    let socks = src.sockets(port).map(|t| parse_ss(&t)).map_err(|e| format!("{e:#}"));
    let clients =
        src.client_infos().map(|v| v.iter().map(|(id, t)| parse_info(id, t)).collect()).map_err(|e| format!("{e:#}"));
    let nfsd = src.nfsd_stats().map_err(|e| format!("{e:#}")).map(|t| {
        let mut n = parse_rpc_nfsd(&t);
        n.pool = src.pool_stats().ok().and_then(|p| parse_pool_stats(&p));
        n.thread_states = src.nfsd_thread_states().ok().and_then(|t| parse_thread_states(&t));
        n
    });
    if let Err(e) = &nfsd {
        tracing::debug!("nfsd server stats unavailable: {e}");
    }
    if let Err(e) = &socks {
        tracing::warn!("socket sample failed: {e}");
    }
    if let Err(e) = &clients {
        tracing::debug!("nfsd client info unavailable: {e}");
    }
    RawSample { mono, wall: now_hms(), socks, clients, nfsd }
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
            loop {
                let started = Instant::now();
                if tx.send(collect(src.as_mut(), port)).is_err() {
                    return;
                }
                tracing::trace!("sample took {:?}", started.elapsed());
                // Sleep until the next tick, waking early if the interval changes.
                match cmds.recv_deadline(started + interval) {
                    Ok(SamplerCmd::SetInterval(d)) => {
                        interval = d;
                        tracing::info!("interval set to {d:?}");
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
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
