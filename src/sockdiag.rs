//! Per-connection TCP stats straight from the kernel over `NETLINK_SOCK_DIAG`, the interface
//! `ss` itself is built on: one dump per address family of established TCP sockets, each with
//! its `tcp_info`. Message types come from `netlink-packet-sock-diag`, the socket from
//! `netlink-sys`.
//!
//! The kernel only ever appends fields to `struct tcp_info`, so an older kernel sends a shorter
//! one than the crate's struct (which ends at `tcpi_snd_wnd`, Linux 5.4). The crate's own
//! response parser rejects that, so replies are framed here and `tcp_info` is zero-padded
//! before the crate decodes it. Fields the kernel did not send read as 0. That matches `ss`,
//! which leaves zero-valued fields out, and so the `None`s the text parser used to produce.

use std::io;
use std::mem::size_of;

use netlink_packet_core::{
    DoneBuffer, ErrorBuffer, NLM_F_DUMP, NLM_F_REQUEST, NLMSG_DONE, NLMSG_ERROR, NetlinkBuffer, NetlinkHeader,
    NetlinkMessage, NlasIterator, Parseable,
};
use netlink_packet_sock_diag::inet::nlas::{TcpInfo, TcpInfoBuffer};
use netlink_packet_sock_diag::inet::{
    ExtensionFlags, InetRequest, InetResponseBuffer, InetResponseHeader, SocketId, StateFlags,
};
use netlink_packet_sock_diag::{AF_INET, AF_INET6, INET_DIAG_INFO, IPPROTO_TCP, SOCK_DIAG_BY_FAMILY, SockDiagMessage};
use netlink_sys::{Socket, SocketAddr, protocols::NETLINK_SOCK_DIAG};

use crate::parse::ss::Sock;

const RESPONSE_HEADER_LEN: usize = size_of::<InetResponseBuffer>();

/// `tcp_info` decoded by the crate, after zero-padding a short one from an older kernel.
fn tcp_info(raw: &[u8]) -> Option<TcpInfo> {
    let mut b = [0u8; size_of::<TcpInfoBuffer>()];
    let n = raw.len().min(b.len());
    b[..n].copy_from_slice(&raw[..n]);
    TcpInfo::parse(&b).ok()
}

/// One `SOCK_DIAG_BY_FAMILY` reply payload as a [`Sock`], if it is a connection to local
/// port `port`. Values follow what `ss -tni` prints: times in ms, zero meaning "not reported".
pub fn decode(payload: &[u8], port: u16) -> Option<Sock> {
    let h = InetResponseHeader::parse(payload.get(..RESPONSE_HEADER_LEN)?).ok()?;
    let id = &h.socket_id;
    if id.source_port != port {
        return None;
    }
    let mut s = Sock {
        ip: Some(id.destination_address.to_canonical()),
        port: id.destination_port,
        local: Some(id.source_address.to_canonical()),
        recv_q: u64::from(h.recv_queue),
        send_q: u64::from(h.send_queue),
        ..Default::default()
    };
    let info = NlasIterator::new(&payload[RESPONSE_HEADER_LEN..])
        .filter_map(Result::ok)
        .find(|a| a.kind() == INET_DIAG_INFO)
        .and_then(|a| tcp_info(a.value()));
    if let Some(t) = info {
        let nz = |v: u32| (v != 0).then_some(u64::from(v));
        let ms = |us: u64| (us != 0 && t.busy_time != 0).then_some(us / 1000);
        s.bytes_acked = t.bytes_acked;
        s.bytes_received = t.bytes_received;
        s.rtt_ms = (t.rtt != 0).then(|| f64::from(t.rtt) / 1000.0);
        s.retrans = u64::from(t.total_retrans);
        s.cwnd = nz(t.snd_cwnd);
        s.last_snd_ms = nz(t.last_data_sent);
        s.last_rcv_ms = nz(t.last_data_recv);
        s.unacked = u64::from(t.unacked);
        s.lost = u64::from(t.lost);
        s.busy_ms = (t.busy_time != 0).then_some(t.busy_time / 1000);
        s.rwnd_limited_ms = ms(t.rwnd_limited);
        s.sndbuf_limited_ms = ms(t.sndbuf_limited);
    }
    Some(s)
}

/// A `NETLINK_SOCK_DIAG` socket, kept open between samples.
pub struct SockDiag {
    sock: Socket,
    seq: u32,
    buf: Vec<u8>,
}

impl SockDiag {
    pub fn open() -> io::Result<Self> {
        let mut sock = Socket::new(NETLINK_SOCK_DIAG)?;
        sock.bind_auto()?;
        sock.connect(&SocketAddr::new(0, 0))?;
        Ok(Self { sock, seq: 0, buf: Vec::with_capacity(256 * 1024) })
    }

    /// Every established TCP connection to local port `port`, IPv4 and IPv6.
    pub fn established(&mut self, port: u16) -> io::Result<Vec<Sock>> {
        let mut out = Vec::new();
        for family in [AF_INET, AF_INET6] {
            self.dump(family, |p| out.extend(decode(p, port)))?;
        }
        Ok(out)
    }

    /// Dump established TCP sockets of `family` with their `tcp_info`, handing each reply
    /// payload to `each`. The kernel can't filter by port without a bytecode filter, which the
    /// crate doesn't model, so [`decode`] filters; an NFS server has few other TCP sockets.
    fn dump(&mut self, family: u8, mut each: impl FnMut(&[u8])) -> io::Result<()> {
        self.seq = self.seq.wrapping_add(1);
        let seq = self.seq;
        let mut header = NetlinkHeader::default();
        header.flags = NLM_F_REQUEST | NLM_F_DUMP;
        header.sequence_number = seq;
        let socket_id = if family == AF_INET { SocketId::new_v4() } else { SocketId::new_v6() };
        let req = SockDiagMessage::InetRequest(InetRequest {
            family,
            protocol: IPPROTO_TCP,
            extensions: ExtensionFlags::INFO,
            states: StateFlags::ESTABLISHED,
            socket_id,
        });
        let mut msg = NetlinkMessage::new(header, req.into());
        msg.finalize();
        let mut bytes = vec![0; msg.buffer_len()];
        msg.serialize(&mut bytes);
        self.sock.send(&bytes, 0)?;

        let bad = |e: netlink_packet_core::DecodeError| io::Error::other(format!("sock_diag reply: {e}"));
        loop {
            self.buf.clear();
            // MSG_TRUNC makes recv report the real datagram size, so truncation is caught.
            let n = self.sock.recv(&mut self.buf, libc::MSG_TRUNC)?;
            if n > self.buf.len() {
                return Err(io::Error::other(format!("sock_diag reply truncated ({n} bytes)")));
            }
            let mut rest = &self.buf[..];
            while !rest.is_empty() {
                let m = NetlinkBuffer::new_checked(rest).map_err(bad)?;
                let len = (m.length() as usize).next_multiple_of(4).min(rest.len());
                if m.sequence_number() == seq {
                    match m.message_type() {
                        NLMSG_DONE => {
                            return match DoneBuffer::new_checked(m.payload()).map(|d| d.code()).unwrap_or(0) {
                                0 => Ok(()),
                                e => Err(io::Error::from_raw_os_error(-e)),
                            };
                        }
                        NLMSG_ERROR => {
                            return match ErrorBuffer::new_checked(m.payload()).map_err(bad)?.code() {
                                None => Ok(()),
                                Some(e) => Err(io::Error::from_raw_os_error(-e.get())),
                            };
                        }
                        SOCK_DIAG_BY_FAMILY => each(m.payload()),
                        _ => {}
                    }
                }
                rest = &rest[len..];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use netlink_packet_core::{Emitable, NlaBuffer};
    use netlink_packet_sock_diag::inet::nlas::Nla;
    use std::net::IpAddr;

    /// A reply payload as the kernel sends it: response header, then `tcp_info` truncated to
    /// `info_len` bytes (an older kernel's shorter struct).
    fn reply(peer: &str, local: &str, info: &TcpInfo, info_len: usize) -> Vec<u8> {
        let h = InetResponseHeader {
            family: AF_INET6,
            state: 1,
            timer: None,
            socket_id: SocketId {
                source_port: 2049,
                destination_port: 707,
                source_address: local.parse().unwrap(),
                destination_address: peer.parse().unwrap(),
                interface_id: 0,
                cookie: [0; 8],
            },
            recv_queue: 2048,
            send_queue: 4096,
            uid: 0,
            inode: 1,
        };
        let mut b = vec![0; h.buffer_len()];
        h.emit(&mut b);
        let nla = Nla::TcpInfo(info.clone());
        let mut a = vec![0; nla.buffer_len()];
        nla.emit(&mut a);
        // Shorten the attribute to `info_len` bytes of value, fixing its length field.
        a.truncate(4 + info_len);
        a[0..2].copy_from_slice(&((4 + info_len) as u16).to_ne_bytes());
        NlaBuffer::new_checked(&a[..]).unwrap();
        a.resize(a.len().next_multiple_of(4), 0);
        b.extend(a);
        b
    }

    fn info() -> TcpInfo {
        let mut t = tcp_info(&[]).unwrap();
        t.rtt = 317;
        t.snd_cwnd = 10;
        t.total_retrans = 7;
        t.last_data_sent = 12;
        t.last_data_recv = 34;
        t.unacked = 3;
        t.lost = 1;
        t.bytes_acked = 5000;
        t.bytes_received = 1234;
        t.busy_time = 52_340_000;
        t.rwnd_limited = 18_000;
        t.sndbuf_limited = 0;
        t
    }

    #[test]
    fn decodes_like_ss() {
        let full = size_of::<TcpInfoBuffer>();
        let s = decode(&reply("::ffff:10.0.2.30", "::ffff:10.0.0.1", &info(), full), 2049).unwrap();
        assert_eq!(s.ip, Some("10.0.2.30".parse().unwrap()), "v4-mapped peer folds to IPv4");
        assert_eq!(s.local, Some("10.0.0.1".parse().unwrap()));
        assert_eq!((s.port, s.recv_q, s.send_q), (707, 2048, 4096));
        assert_eq!((s.bytes_acked, s.bytes_received), (5000, 1234));
        assert_eq!(s.rtt_ms, Some(0.317));
        assert_eq!((s.cwnd, s.last_snd_ms, s.last_rcv_ms), (Some(10), Some(12), Some(34)));
        assert_eq!((s.retrans, s.unacked, s.lost), (7, 3, 1));
        assert_eq!((s.busy_ms, s.rwnd_limited_ms, s.sndbuf_limited_ms), (Some(52_340), Some(18), None));
        assert!(decode(&reply("10.0.2.30", "10.0.0.1", &info(), full), 22).is_none(), "other port");
    }

    #[test]
    fn short_tcp_info_from_old_kernel() {
        // Linux 4.6 ends tcp_info at tcpi_data_segs_out (bytes_acked/received are there, the
        // busy/limited times are not); 104 bytes is pre-4.1 (no byte counters at all).
        let s = decode(&reply("2001:db8::21", "2001:db8::1", &info(), 160), 2049).unwrap();
        assert_eq!(s.ip, Some("2001:db8::21".parse().unwrap()));
        assert_eq!((s.bytes_acked, s.bytes_received, s.cwnd), (5000, 1234, Some(10)));
        assert_eq!((s.busy_ms, s.rwnd_limited_ms), (None, None));
        let s = decode(&reply("10.0.2.30", "10.0.0.1", &info(), 104), 2049).unwrap();
        assert_eq!((s.bytes_acked, s.rtt_ms), (0, Some(0.317)));
    }

    #[test]
    fn longer_tcp_info_from_new_kernel() {
        let mut p = reply("10.0.2.30", "10.0.0.1", &info(), size_of::<TcpInfoBuffer>());
        // A newer kernel appends fields: grow the attribute by 16 bytes.
        let at = RESPONSE_HEADER_LEN;
        let len = u16::from_ne_bytes([p[at], p[at + 1]]) + 16;
        p[at..at + 2].copy_from_slice(&len.to_ne_bytes());
        p.resize(at + usize::from(len).next_multiple_of(4), 0xff);
        assert_eq!(decode(&p, 2049).unwrap().bytes_acked, 5000);
    }

    #[test]
    fn truncated_payload_is_skipped() {
        assert!(decode(&[0; 10], 2049).is_none());
    }

    /// A real dump from the kernel: a loopback connection shows up with the bytes sent on it.
    #[test]
    fn kernel_round_trip() {
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let (mut srv, peer) = l.accept().unwrap();
        c.write_all(&[7; 100_000]).unwrap();
        let mut got = vec![0; 100_000];
        srv.read_exact(&mut got).unwrap();
        srv.write_all(&[1; 5000]).unwrap();
        c.read_exact(&mut got[..5000]).unwrap();

        let mut d = SockDiag::open().unwrap();
        let socks = d.established(port).unwrap();
        let s = socks.iter().find(|s| s.port == peer.port()).expect("our connection");
        assert_eq!(s.ip, Some(IpAddr::from([127, 0, 0, 1])));
        assert_eq!(s.local, Some(IpAddr::from([127, 0, 0, 1])));
        assert_eq!(s.bytes_received, 100_000);
        assert_eq!(s.bytes_acked, 5000);
        assert!(s.rtt_ms.is_some() && s.cwnd.is_some());
        // The socket is reusable for the next sample.
        assert!(!d.established(port).unwrap().is_empty());
    }
}
