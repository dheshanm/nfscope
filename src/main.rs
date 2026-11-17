use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use color_eyre::eyre::Result;
use crossbeam_channel::{select, tick, unbounded};
use crossterm::event::{self, Event, KeyEventKind};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use nfscope::app::{Action, App};
use nfscope::source::SourceSpec;
use nfscope::workers::{SamplerCmd, spawn_dns_worker, spawn_sampler, spawn_states_worker};

/// Live top-like view of NFS client I/O, run on the NFS server itself.
///
/// READ = server→client (the client is reading), WRITE = client→server (the client is writing).
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Sampling interval in seconds.
    #[arg(short, long, default_value_t = 2)]
    interval: u64,

    /// NFS server TCP port.
    #[arg(long, default_value_t = 2049)]
    port: u16,

    /// Replay captured data from a directory instead of the live system (see tests/fixtures).
    #[arg(long, value_name = "DIR")]
    fixture: Option<PathBuf>,

    /// nfsd per-client directory.
    #[arg(long, default_value = "/proc/fs/nfsd/clients", hide = true)]
    nfsd_dir: PathBuf,

    /// Log file (stdout belongs to the TUI). Filter with RUST_LOG.
    #[arg(long, default_value = "/tmp/nfscope.log")]
    log: PathBuf,

    /// Don't reverse-resolve client addresses.
    #[arg(long)]
    no_dns: bool,
}

fn init_logging(path: &std::path::Path) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let file_layer = std::fs::OpenOptions::new().create(true).append(true).open(path).ok().map(|f| {
        let (w, guard) = tracing_appender::non_blocking(f);
        (tracing_subscriber::fmt::layer().with_writer(w).with_ansi(false).with_filter(filter()), guard)
    });
    let (file_layer, guard) = match file_layer {
        Some((l, g)) => (Some(l), Some(g)),
        None => (None, None),
    };
    tracing_subscriber::registry()
        .with(file_layer)
        .with(tui_logger::TuiTracingSubscriberLayer.with_filter(filter()))
        .init();
    let _ = tui_logger::init_logger(tui_logger::LevelFilter::Trace);
    guard
}

fn main() -> Result<()> {
    color_eyre::install()?;
    let args = Args::parse();
    let _guard = init_logging(&args.log);

    let spec = match &args.fixture {
        Some(dir) => SourceSpec::Fixture(dir.clone()),
        None => SourceSpec::Live { nfsd_dir: args.nfsd_dir.clone() },
    };
    tracing::info!("starting nfscope: {spec:?}, interval {}s, port {}", args.interval, args.port);

    let interval = Duration::from_secs(args.interval.max(1));
    let (sample_tx, sample_rx) = unbounded();
    let (cmd_tx, cmd_rx) = unbounded();
    let (states_req_tx, states_req_rx) = unbounded();
    let (states_tx, states_rx) = unbounded();
    let (dns_req_tx, dns_req_rx) = unbounded();
    let (dns_tx, dns_rx) = unbounded();
    spawn_sampler(spec.clone(), args.port, interval, sample_tx, cmd_rx);
    spawn_states_worker(spec.clone(), states_req_rx, states_tx);
    if !args.no_dns {
        spawn_dns_worker(spec.clone(), dns_req_rx, dns_tx);
    }

    let server_name = match &args.fixture {
        Some(d) => format!("fixture {}", d.display()),
        None => dns_lookup::get_hostname().unwrap_or_else(|_| "localhost".into()),
    };
    let mut app = App::new(server_name, interval, !args.no_dns);

    // Terminal input on its own thread so the main loop can `select!` over everything.
    let (key_tx, key_rx) = unbounded();
    std::thread::Builder::new().name("input".into()).spawn(move || {
        while let Ok(ev) = event::read() {
            if key_tx.send(ev).is_err() {
                break;
            }
        }
    })?;
    let anim = tick(Duration::from_millis(150));

    let mut terminal = ratatui::init();
    let res: Result<()> = (|| {
        loop {
            terminal.draw(|f| nfscope::ui::draw(f, &mut app))?;
            select! {
                recv(key_rx) -> ev => match ev? {
                    Event::Key(k) if k.kind != KeyEventKind::Release => app.handle_key(k),
                    _ => {}
                },
                recv(sample_rx) -> s => app.on_sample(s?),
                recv(states_rx) -> r => { let (ip, res) = r?; app.on_states(ip, res) },
                recv(dns_rx) -> r => { if let Ok((ip, name)) = r { app.on_dns(ip, name) } },
                recv(anim) -> _ => app.tick(),
            }
            for a in app.actions.drain(..) {
                match a {
                    Action::Resolve(ip) => {
                        let _ = dns_req_tx.send(ip);
                    }
                    Action::LoadStates(ip) => {
                        let _ = states_req_tx.send(ip);
                    }
                    Action::SetInterval(d) => {
                        let _ = cmd_tx.send(SamplerCmd::SetInterval(d));
                    }
                }
            }
            if app.quit {
                return Ok(());
            }
        }
    })();
    ratatui::restore();
    res
}
