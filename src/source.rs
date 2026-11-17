//! Where raw data comes from: the live system (`ss` + `/proc`) or a fixture directory.

use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::Command;

use color_eyre::eyre::{Result, WrapErr, eyre};

use crate::parse::ss::{Sock, parse_ss, render_ss};

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
}

#[derive(Debug, Clone)]
pub enum SourceSpec {
    Live { nfsd_dir: PathBuf },
    Fixture(PathBuf),
}

impl SourceSpec {
    pub fn build(&self) -> Box<dyn Source> {
        match self {
            SourceSpec::Live { nfsd_dir } => Box::new(LiveSource { nfsd_dir: nfsd_dir.clone() }),
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
}

impl Source for LiveSource {
    fn sockets(&mut self, port: u16) -> Result<String> {
        let out = Command::new("ss")
            .args(["-tniH", "state", "established", &format!("( sport = :{port} )")])
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
/// ```
///
/// After the captured snapshots run out, it extrapolates from the last two with a
/// deterministic wobble so the display keeps moving.
/// Current counters plus the fixed per-socket (read, write) delta used for extrapolation.
type Synth = (Vec<Sock>, Vec<(u64, u64)>);

pub struct FixtureSource {
    dir: PathBuf,
    tick: usize,
    /// (current counters, per-socket base delta) once replay is exhausted.
    synth: Option<Synth>,
}

impl FixtureSource {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, tick: 0, synth: None }
    }

    fn snapshots(&self) -> Result<Vec<PathBuf>> {
        let mut v: Vec<PathBuf> = fs::read_dir(self.dir.join("ss"))
            .wrap_err("fixture dir has no ss/ subdirectory")?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "txt"))
            .collect();
        v.sort();
        Ok(v)
    }
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
                    prev.iter().find(|p| p.ip == s.ip && p.port == s.port).map_or((0, 0), |p| {
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
}
