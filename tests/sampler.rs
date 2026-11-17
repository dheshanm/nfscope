//! The live sampler thread, run against fixture data whose nfsd reads can be made to block.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use color_eyre::eyre::Result;
use crossbeam_channel::unbounded;
use nfscope::files::{FindResult, Target};
use nfscope::parse::inflight::InflightRpc;
use nfscope::source::{FixtureSource, Source};
use nfscope::workers::{SamplerCmd, spawn_sampler};

/// Fixture data, except that the reads which take nfsd's mutex in the kernel (thread states,
/// pool stats, rpc-status) hang while `blocked` is set, like during a stuck `rpc.nfsd` restart.
struct Blocking {
    inner: FixtureSource,
    blocked: Arc<AtomicBool>,
}

impl Blocking {
    fn wait(&self) {
        while self.blocked.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Source for Blocking {
    fn sockets(&mut self, port: u16) -> Result<String> {
        self.inner.sockets(port)
    }
    fn client_infos(&mut self) -> Result<Vec<(String, String)>> {
        self.inner.client_infos()
    }
    fn states(&mut self, id: &str) -> Result<String> {
        self.inner.states(id)
    }
    fn mountinfo(&mut self) -> Result<String> {
        self.inner.mountinfo()
    }
    fn resolve(&mut self, ip: IpAddr) -> Option<String> {
        self.inner.resolve(ip)
    }
    fn nfsd_stats(&mut self) -> Result<String> {
        self.inner.nfsd_stats()
    }
    fn pool_stats(&mut self) -> Result<String> {
        self.wait();
        self.inner.pool_stats()
    }
    fn nfsd_thread_states(&mut self) -> Result<String> {
        self.wait();
        self.inner.nfsd_thread_states()
    }
    fn rpc_status(&mut self) -> Result<Vec<InflightRpc>> {
        self.wait();
        self.inner.rpc_status()
    }
    fn find_paths(&mut self, root: &str, t: &[Target], p: &mut dyn FnMut(u64) -> bool) -> Result<FindResult> {
        self.inner.find_paths(root, t, p)
    }
}

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

#[test]
fn blocked_nfsd_reads_do_not_stop_the_rates() {
    let blocked = Arc::new(AtomicBool::new(false));
    let b = blocked.clone();
    let build = move || -> Box<dyn Source> {
        Box::new(Blocking { inner: FixtureSource::new(fixture_dir()), blocked: b.clone() })
    };
    let (tx, rx) = unbounded();
    let (_cmd_tx, cmd_rx) = unbounded();
    spawn_sampler(build, 2049, Duration::from_millis(200), tx, cmd_rx);
    let next = || rx.recv_timeout(Duration::from_secs(5)).expect("a sample");

    let s = next();
    assert!(s.nfsd_stalled.is_none());
    assert!(s.nfsd.as_ref().unwrap().thread_load.is_some());

    blocked.store(true, Ordering::Relaxed);
    // The probe is stuck from its next snapshot on; within a few samples the sampler says so,
    // and keeps delivering socket rates and the nfsd counters meanwhile.
    let stalled = (0..10).map(|_| next()).find(|s| s.nfsd_stalled.is_some()).expect("stall reported");
    assert!(stalled.socks.as_ref().is_ok_and(|v| !v.is_empty()));
    assert!(stalled.nfsd.as_ref().is_ok_and(|n| n.thread_load.is_none() && n.pool.is_none()));
    assert!(stalled.inflight.as_ref().unwrap_err().contains("not answered"), "{:?}", stalled.inflight);
    let later = next();
    assert!(later.nfsd_stalled > stalled.nfsd_stalled, "the stall keeps counting up");

    blocked.store(false, Ordering::Relaxed);
    let back = (0..10).map(|_| next()).find(|s| s.nfsd_stalled.is_none()).expect("probe answers again");
    assert!(back.nfsd.as_ref().unwrap().thread_load.is_some());
}

#[test]
fn changing_the_interval_does_not_sample_early() {
    let build = || -> Box<dyn Source> { Box::new(FixtureSource::new(fixture_dir())) };
    let (tx, rx) = unbounded();
    let (cmd_tx, cmd_rx) = unbounded();
    spawn_sampler(build, 2049, Duration::from_millis(400), tx, cmd_rx);
    let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    // Like pressing `-` right after a sample: the next one still comes a full new interval
    // after the last, not milliseconds later.
    cmd_tx.send(SamplerCmd::SetInterval(Duration::from_millis(300))).unwrap();
    let t = Instant::now();
    let second = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let gap = second.mono - first.mono;
    assert!(gap >= Duration::from_millis(290), "{gap:?}");
    assert!(t.elapsed() < Duration::from_millis(400), "the new, shorter interval applies to this wait");
}

#[test]
fn a_search_cancelled_before_it_starts_still_reports_back() {
    use nfscope::app::{App, View};
    use nfscope::source::SourceSpec;
    use nfscope::workers::{FindMsg, FindReq, spawn_find_worker};
    use std::sync::atomic::AtomicU64;

    let current = Arc::new(AtomicU64::new(0));
    let (req_tx, req_rx) = unbounded();
    let (tx, rx) = unbounded();
    spawn_find_worker(SourceSpec::Fixture(fixture_dir()), req_rx, tx, current.clone());
    // Search 7 was queued, then Esc (current = 0) before the worker got to it.
    let ip: IpAddr = "10.20.1.11".parse().unwrap();
    req_tx.send(FindReq { id: 7, ip, export: "/export/data".into(), targets: vec![] }).unwrap();
    let FindMsg::Done { id, res, .. } = rx.recv_timeout(Duration::from_secs(5)).unwrap() else { panic!() };
    assert_eq!(id, 7);
    assert_eq!(res.unwrap().end, nfscope::files::FindEnd::Cancelled);

    // The drill-down waiting on it stops its spinner, so the next Esc leaves.
    let mut app = App::new("t".into(), Duration::from_secs(2), false);
    app.open_detail(ip);
    let View::Detail(d) = &mut app.view else { panic!() };
    d.find = Some(nfscope::app::FindJob { id: 7, export: "/export/data".into(), files: 1, dirs: 0 });
    app.on_find(FindMsg::Done {
        id: 7,
        res: Ok(nfscope::files::FindResult {
            found: Default::default(),
            dirs: 0,
            end: nfscope::files::FindEnd::Cancelled,
        }),
        secs: 0.0,
    });
    let View::Detail(d) = &app.view else { panic!() };
    assert!(d.find.is_none());
}
