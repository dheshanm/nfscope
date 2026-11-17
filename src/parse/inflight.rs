//! In-flight nfsd RPCs, as reported by the nfsd generic-netlink family's `rpc-status-get` dump
//! (Linux 6.7+): decoding the netlink attributes, naming the operation, and a one-line text
//! form used by fixtures (`DIR/inflight/*.txt`) and `nfscope --dump-inflight`.
//!
//! The kernel lists every nfsd thread that is between decoding an NFS request's arguments and
//! sending the reply. Its `service-time` is when the thread took the request off the
//! connection, on `CLOCK_MONOTONIC`, so the age is how long that thread has been on it.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use super::rpc::{PLUMBING, V3_OPS, V4_OPS};

pub const NFS_PROGRAM: u32 = 100003;
pub const NFSACL_PROGRAM: u32 = 100227;

/// `NFSD_CMD_RPC_STATUS_GET`: dump the requests nfsd threads are working on.
pub const RPC_STATUS_GET: u8 = 1;

// `rpc-status` attribute numbers (include/uapi/linux/nfsd_netlink.h).
pub const A_XID: u16 = 1;
pub const A_PROG: u16 = 3;
pub const A_VERSION: u16 = 4;
pub const A_PROC: u16 = 5;
pub const A_SERVICE_TIME: u16 = 6;
pub const A_SADDR4: u16 = 8;
pub const A_SADDR6: u16 = 10;
pub const A_SPORT: u16 = 12;
pub const A_COMPOUND_OPS: u16 = 14;

/// One request an nfsd thread is working on right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InflightRpc {
    /// The client's address and port (None if the transport has no IP address).
    pub client: Option<SocketAddr>,
    pub xid: u32,
    pub prog: u32,
    pub vers: u8,
    pub proc_: u32,
    /// How long the thread has been working on it.
    pub age_us: u64,
    /// NFSv4 COMPOUND operation numbers (the kernel reports at most 16).
    pub ops: Vec<u32>,
}

fn arr<const N: usize>(v: &[u8]) -> Option<[u8; N]> {
    v.get(..N)?.try_into().ok()
}

impl InflightRpc {
    /// Decode one `rpc-status-get` reply's attributes. `now_us` is `CLOCK_MONOTONIC` read at
    /// dump time, to turn the start stamp into an age. None if the reply carries no XID.
    pub fn from_attrs<A: AsRef<[u8]>>(attrs: impl IntoIterator<Item = (u16, A)>, now_us: i64) -> Option<Self> {
        let (mut xid, mut prog, mut vers, mut proc_, mut start) = (None, 0, 0, 0, None);
        let (mut ip, mut port, mut ops) = (None, 0, Vec::new());
        for (ty, v) in attrs {
            let v = v.as_ref();
            match ty {
                A_XID => xid = arr(v).map(u32::from_be_bytes),
                A_PROG => prog = arr(v).map_or(0, u32::from_ne_bytes),
                A_VERSION => vers = v.first().copied().unwrap_or(0),
                A_PROC => proc_ = arr(v).map_or(0, u32::from_ne_bytes),
                A_SERVICE_TIME => start = arr(v).map(i64::from_ne_bytes),
                A_SADDR4 => ip = arr::<4>(v).map(|b| IpAddr::V4(Ipv4Addr::from(b))),
                A_SADDR6 => ip = arr::<16>(v).map(|b| IpAddr::V6(Ipv6Addr::from(b)).to_canonical()),
                A_SPORT => port = arr(v).map_or(0, u16::from_be_bytes),
                A_COMPOUND_OPS => ops.extend(arr(v).map(u32::from_ne_bytes)),
                _ => {}
            }
        }
        Some(Self {
            client: ip.map(|ip| SocketAddr::new(ip, port)),
            xid: xid?,
            prog,
            vers,
            proc_,
            age_us: start.map_or(0, |s| now_us.saturating_sub(s).max(0) as u64),
            ops,
        })
    }

    pub fn ip(&self) -> Option<IpAddr> {
        self.client.map(|c| c.ip())
    }

    /// What the request does, named like the server ops table: the NFSv3 procedure, or for
    /// an NFSv4 COMPOUND its first workload op (`READ`, not `SEQUENCE PUTFH READ`).
    pub fn op_name(&self) -> String {
        let name = |table: &[&str], i: u32| match table.get(i as usize) {
            Some(n) if !n.is_empty() => (*n).to_string(),
            _ => format!("op#{i}"),
        };
        match (self.prog, self.vers, self.proc_) {
            (NFS_PROGRAM, 3, p) => name(&V3_OPS, p),
            (NFS_PROGRAM, 4, 0) => "NULL".into(),
            (NFS_PROGRAM, 4, _) => {
                let names: Vec<String> = self.ops.iter().map(|&o| name(&V4_OPS, o)).collect();
                names
                    .iter()
                    .find(|n| !PLUMBING.contains(&n.as_str()))
                    .or(names.last())
                    .cloned()
                    .unwrap_or_else(|| "COMPOUND".into())
            }
            (NFSACL_PROGRAM, _, 1) => "GETACL".into(),
            (NFSACL_PROGRAM, _, 2) => "SETACL".into(),
            (prog, vers, p) => format!("prog {prog} v{vers} proc {p}"),
        }
    }

    /// `v3 READ`, `v4 READ`, … for lists that mix protocol versions.
    pub fn label(&self) -> String {
        match self.prog {
            NFS_PROGRAM => format!("v{} {}", self.vers, self.op_name()),
            NFSACL_PROGRAM => format!("v{} ACL {}", self.vers, self.op_name()),
            _ => self.op_name(),
        }
    }

    /// Parse the text form written by `Display`. Unknown `key=value` fields are ignored.
    pub fn parse_line(line: &str) -> Option<Self> {
        let mut it = line.split_whitespace();
        let client = match it.next()? {
            "-" => None,
            c => Some(c.parse::<SocketAddr>().ok()?),
        };
        let mut r = Self { client, xid: 0, prog: NFS_PROGRAM, vers: 4, proc_: 1, age_us: 0, ops: Vec::new() };
        let mut has_xid = false;
        for kv in it {
            let Some((k, v)) = kv.split_once('=') else { continue };
            match k {
                "xid" => {
                    r.xid = u32::from_str_radix(v.trim_start_matches("0x"), 16).ok()?;
                    has_xid = true;
                }
                "prog" => r.prog = v.parse().ok()?,
                "vers" => r.vers = v.parse().ok()?,
                "proc" => r.proc_ = v.parse().ok()?,
                "age_us" => r.age_us = v.parse().ok()?,
                "ops" => {
                    r.ops = v.split(',').filter(|s| !s.is_empty()).map(str::parse).collect::<Result<_, _>>().ok()?
                }
                _ => {}
            }
        }
        has_xid.then_some(r)
    }
}

/// `10.20.1.11:875 xid=0x1a2b3c4d prog=100003 vers=4 proc=1 age_us=1520 ops=53,22,25`
impl fmt::Display for InflightRpc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.client {
            Some(c) => write!(f, "{c}")?,
            None => write!(f, "-")?,
        }
        write!(
            f,
            " xid={:#010x} prog={} vers={} proc={} age_us={}",
            self.xid, self.prog, self.vers, self.proc_, self.age_us
        )?;
        if !self.ops.is_empty() {
            let ops: Vec<String> = self.ops.iter().map(u32::to_string).collect();
            write!(f, " ops={}", ops.join(","))?;
        }
        Ok(())
    }
}

/// Parse a text snapshot: one RPC per line; blank lines and `#` comments are skipped.
pub fn parse_snapshot(text: &str) -> Vec<InflightRpc> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(InflightRpc::parse_line)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Attributes in the order and encoding nfsd uses (nfsd_genl_rpc_status_compose_msg),
    /// including the PAD attribute nla_put_s64 may add.
    fn kernel_attrs(saddr: &[u8], ops: &[u32]) -> Vec<(u16, Vec<u8>)> {
        let mut a = vec![
            (A_XID, 0x1a2b3c4du32.to_be_bytes().to_vec()),
            (2, 0x21u32.to_ne_bytes().to_vec()),
            (A_PROG, NFS_PROGRAM.to_ne_bytes().to_vec()),
            (A_PROC, 1u32.to_ne_bytes().to_vec()),
            (A_VERSION, vec![4]),
            (7, vec![]),
            (A_SERVICE_TIME, 5_000_000i64.to_ne_bytes().to_vec()),
        ];
        if saddr.len() == 4 {
            a.push((A_SADDR4, saddr.to_vec()));
            a.push((9, vec![10, 0, 0, 1]));
        } else {
            a.push((A_SADDR6, saddr.to_vec()));
            a.push((11, vec![0; 16]));
        }
        a.push((A_SPORT, 875u16.to_be_bytes().to_vec()));
        a.push((13, 2049u16.to_be_bytes().to_vec()));
        a.extend(ops.iter().map(|o| (A_COMPOUND_OPS, o.to_ne_bytes().to_vec())));
        a
    }

    #[test]
    fn decodes_kernel_attributes() {
        let a = kernel_attrs(&[10, 20, 1, 11], &[53, 22, 25]);
        let r = InflightRpc::from_attrs(a, 5_001_520).unwrap();
        assert_eq!(r.client, Some("10.20.1.11:875".parse().unwrap()));
        assert_eq!((r.xid, r.prog, r.vers, r.proc_), (0x1a2b3c4d, NFS_PROGRAM, 4, 1));
        assert_eq!(r.age_us, 1520);
        assert_eq!(r.ops, vec![53, 22, 25]);
        assert_eq!(r.op_name(), "READ", "SEQUENCE and PUTFH are plumbing");
        assert_eq!(r.label(), "v4 READ");
    }

    #[test]
    fn mapped_ipv6_folds_to_ipv4_and_clock_skew_is_zero_age() {
        let mut v6 = [0u8; 16];
        v6[10..12].copy_from_slice(&[0xff, 0xff]);
        v6[12..].copy_from_slice(&[10, 20, 3, 30]);
        let r = InflightRpc::from_attrs(kernel_attrs(&v6, &[]), 4_000_000).unwrap();
        assert_eq!(r.ip(), Some("10.20.3.30".parse().unwrap()));
        assert_eq!(r.age_us, 0);
        assert_eq!(r.op_name(), "COMPOUND");
        assert!(InflightRpc::from_attrs(std::iter::empty::<(u16, Vec<u8>)>(), 0).is_none());
    }

    #[test]
    fn names_ops() {
        let rpc = |prog, vers, proc_, ops: &[u32]| InflightRpc {
            client: None,
            xid: 1,
            prog,
            vers,
            proc_,
            age_us: 0,
            ops: ops.to_vec(),
        };
        assert_eq!(rpc(NFS_PROGRAM, 3, 7, &[]).label(), "v3 WRITE");
        assert_eq!(rpc(NFS_PROGRAM, 4, 1, &[53]).op_name(), "SEQUENCE", "lease renewal");
        assert_eq!(rpc(NFS_PROGRAM, 4, 1, &[53, 22, 9]).op_name(), "GETATTR");
        assert_eq!(rpc(NFS_PROGRAM, 4, 1, &[22, 200]).op_name(), "op#200");
        assert_eq!(rpc(NFS_PROGRAM, 4, 0, &[]).op_name(), "NULL");
        assert_eq!(rpc(NFSACL_PROGRAM, 3, 1, &[]).label(), "v3 ACL GETACL");
        assert_eq!(rpc(100099, 1, 2, &[]).label(), "prog 100099 v1 proc 2");
    }

    #[test]
    fn text_round_trip() {
        let a = kernel_attrs(&[10, 20, 1, 11], &[53, 22, 38]);
        let r = InflightRpc::from_attrs(a, 5_250_000).unwrap();
        let line = r.to_string();
        assert_eq!(line, "10.20.1.11:875 xid=0x1a2b3c4d prog=100003 vers=4 proc=1 age_us=250000 ops=53,22,38");
        assert_eq!(InflightRpc::parse_line(&line), Some(r));
        let snap = parse_snapshot(
            "# comment\n\n[2001:db8::40]:709 xid=0x2 vers=3 proc=6 age_us=9\n- xid=0x3\nbad line\n10.0.0.1:1 prog=x xid=1\n",
        );
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].label(), "v3 READ");
        assert_eq!(snap[1].client, None);
    }
}
