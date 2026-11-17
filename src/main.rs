use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use clap::Parser;
use color_eyre::eyre::Result;
use crossbeam_channel::{select, tick, unbounded};
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use crossterm::execute;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use nfscope::app::{Action, App};
use nfscope::source::SourceSpec;
use nfscope::workers::{SamplerCmd, spawn_dns_worker, spawn_find_worker, spawn_sampler, spawn_states_worker};

/// Live top-like view of NFS client I/O, run on the NFS server itself.
///
/// READ = server→client (the client is reading), WRITE = client→server (the client is writing).
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Sampling interval in seconds (1-60; `+`/`-` change it while running).
    #[arg(short, long, default_value_t = 2, value_parser = clap::value_parser!(u64).range(1..=60))]
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

    /// Log file (stdout belongs to the TUI), created readable by its owner only. Filter with
    /// RUST_LOG. The `l` pane shows the same log.
    #[arg(long, default_value = "/var/log/nfscope.log")]
    log: PathBuf,

    /// Don't reverse-resolve client addresses.
    #[arg(long)]
    no_dns: bool,

    /// Don't capture the mouse (keeps the terminal's native text selection).
    #[arg(long)]
    no_mouse: bool,

    /// Debug the full-path search without the TUI: `--find MOUNT PARENT/NAME INODE` walks MOUNT
    /// the way the `f` key does, prints what it finds and exits. PARENT/NAME `/` = name unknown.
    #[arg(long, num_args = 3, value_names = ["MOUNT", "PARENT/NAME", "INODE"], hide = true)]
    find: Option<Vec<String>>,

    /// Print the RPCs nfsd is working on right now, one per line, and exit. The output is the
    /// fixture format: save a few runs as `DIR/inflight/001.txt`, … to replay them.
    #[arg(long, hide = true)]
    dump_inflight: bool,
}

fn init_logging(path: &std::path::Path) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Owner-only (it names clients), and never through a symlink someone planted.
        opts.mode(0o600);
        #[cfg(target_os = "linux")]
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let opened = opts.open(path);
    let open_error = opened.as_ref().err().map(|e| e.to_string());
    let file_layer = opened.ok().map(|f| {
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
    if let Some(e) = open_error {
        tracing::warn!("not logging to {}: {e} (use --log PATH)", path.display());
    }
    guard
}

fn main() -> Result<()> {
    color_eyre::install()?;
    let args = Args::parse();
    if let Some(f) = &args.find {
        return find_cli(&f[0], &f[1], &f[2]);
    }
    let spec = match &args.fixture {
        Some(dir) => SourceSpec::Fixture(dir.clone()),
        None => SourceSpec::Live { nfsd_dir: args.nfsd_dir.clone() },
    };
    if args.dump_inflight {
        for rpc in spec.build().rpc_status()? {
            println!("{rpc}");
        }
        return Ok(());
    }
    let _guard = init_logging(&args.log);

    tracing::info!("starting nfscope: {spec:?}, interval {}s, port {}", args.interval, args.port);

    let interval = Duration::from_secs(args.interval);
    let (sample_tx, sample_rx) = unbounded();
    let (cmd_tx, cmd_rx) = unbounded();
    let (states_req_tx, states_req_rx) = unbounded();
    let (states_tx, states_rx) = unbounded();
    let (dns_req_tx, dns_req_rx) = unbounded();
    let (dns_tx, dns_rx) = unbounded();
    let sampler_spec = spec.clone();
    spawn_sampler(move || sampler_spec.build(), args.port, interval, sample_tx, cmd_rx);
    spawn_states_worker(spec.clone(), states_req_rx, states_tx);
    let (find_req_tx, find_req_rx) = unbounded();
    let (find_tx, find_rx) = unbounded();
    let find_current = Arc::new(AtomicU64::new(0));
    spawn_find_worker(spec.clone(), find_req_rx, find_tx, find_current.clone());
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
    if !args.no_mouse {
        execute!(std::io::stdout(), EnableMouseCapture)?;
        // ratatui's panic hook restores the screen; also release the mouse so the shell isn't left
        // receiving mouse escape sequences.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = execute!(std::io::stdout(), DisableMouseCapture);
            prev(info);
        }));
    }
    let res: Result<()> = (|| {
        loop {
            terminal.draw(|f| nfscope::ui::draw(f, &mut app))?;
            select! {
                recv(key_rx) -> ev => match ev? {
                    Event::Key(k) if k.kind != KeyEventKind::Release => app.handle_key(k),
                    Event::Mouse(m) => app.handle_mouse(m),
                    _ => {}
                },
                recv(sample_rx) -> s => app.on_sample(s?),
                recv(states_rx) -> r => { let (ip, res) = r?; app.on_states(ip, res) },
                recv(find_rx) -> m => app.on_find(m?),
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
                    Action::FindPaths(req) => {
                        // Newer id = the running search (if any) stops at its next progress check.
                        find_current.store(req.id, Ordering::Relaxed);
                        let _ = find_req_tx.send(req);
                    }
                    Action::CancelFind => find_current.store(0, Ordering::Relaxed),
                }
            }
            if app.quit {
                return Ok(());
            }
        }
    })();
    if !args.no_mouse {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
    }
    ratatui::restore();
    res
}

fn find_cli(mount: &str, kernel_path: &str, ino: &str) -> Result<()> {
    let (dir, name) = match kernel_path.rsplit_once('/') {
        Some(("", "")) => (nfscope::files::ANON_DIR, ""),
        Some(dn) => dn,
        None => ("", kernel_path),
    };
    let target = nfscope::files::Target { dir: dir.into(), name: name.into(), ino: ino.parse()? };
    let t = std::time::Instant::now();
    let mut progress = |dirs| {
        eprint!("\r{dirs} dirs searched");
        true
    };
    let r = nfscope::files::find_paths(
        std::path::Path::new(mount),
        &[target],
        nfscope::source::FIND_BUDGET,
        &mut progress,
    )?;
    eprintln!("\r{} dirs searched in {:.1}s: {:?}", r.dirs, t.elapsed().as_secs_f64(), r.end);
    for p in r.found.values() {
        println!("{}", p.display());
    }
    Ok(())
}
