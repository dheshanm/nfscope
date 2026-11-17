//! Per-connection TCP stats ([`Sock`]) and their text form: the output of
//! `ss -tniH state established '( sport = :2049 )'`.
//!
//! Live sampling reads the kernel directly (`crate::sockdiag`, Linux only); this text form is
//! the fixture format (`scripts/capture-fixture.sh` saves `ss` output, `--dump-sockets` prints
//! nfscope's own reading the same way).
//!
//! Each socket is a header line (`Recv-Q Send-Q Local:Port Peer:Port`) followed by
//! one or more indented lines of `key:value` TCP info.

use std::net::IpAddr;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sock {
    /// Client (peer) address.
    pub ip: Option<IpAddr>,
    /// Client (peer) port.
    pub port: u16,
    /// Server (local) address the client connected to. A server with several addresses (VIPs,
    /// bonds, NFSv4.1 trunking) can see the same client port on two of them.
    pub local: Option<IpAddr>,
    /// Bytes the client has acknowledged receiving (server → client, i.e. client READ).
    pub bytes_acked: u64,
    /// Bytes the server has received (client → server, i.e. client WRITE).
    pub bytes_received: u64,
    pub rtt_ms: Option<f64>,
    pub retrans: u64,
    pub cwnd: Option<u64>,
    pub last_snd_ms: Option<u64>,
    pub last_rcv_ms: Option<u64>,
    /// Bytes received but not yet read by nfsd (header `Recv-Q`).
    pub recv_q: u64,
    /// Bytes queued to the client but not yet acknowledged (header `Send-Q`).
    pub send_q: u64,
    /// Segments sent but not yet acknowledged.
    pub unacked: u64,
    /// Segments currently considered lost (`ss` only prints this when nonzero).
    pub lost: u64,
    /// Cumulative time the connection had data in flight, and how much of it was limited by
    /// the client's receive window or the server's send buffer (all ms since connect).
    pub busy_ms: Option<u64>,
    pub rwnd_limited_ms: Option<u64>,
    pub sndbuf_limited_ms: Option<u64>,
}

/// Identifies one connection within a sample: client address and port, server address.
pub type ConnKey = (IpAddr, u16, Option<IpAddr>);

impl Sock {
    /// None for a socket whose client address could not be parsed.
    pub fn key(&self) -> Option<ConnKey> {
        Some((self.ip?, self.port, self.local))
    }
}

/// Split `addr:port`, handling `[v6]:port`, bare `::ffff:1.2.3.4:port` and `%iface` suffixes.
/// IPv4-mapped IPv6 addresses are canonicalised to IPv4.
pub fn parse_endpoint(s: &str) -> Option<(IpAddr, u16)> {
    let (host, port) = if let Some(rest) = s.strip_prefix('[') {
        let (h, p) = rest.split_once(']')?;
        (h, p.strip_prefix(':')?)
    } else {
        s.rsplit_once(':')?
    };
    let host = host.split('%').next()?;
    let ip: IpAddr = host.parse().ok()?;
    Some((ip.to_canonical(), port.parse().ok()?))
}

fn looks_like_endpoint(s: &str) -> bool {
    s.contains(':') && parse_endpoint(s).is_some()
}

/// `18ms` or `18ms(0.4%)` -> 18.
fn millis(v: &str) -> Option<u64> {
    v.split("ms").next()?.parse().ok()
}

pub fn parse_ss(text: &str) -> Vec<Sock> {
    let mut out: Vec<Sock> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if !line.starts_with(char::is_whitespace) {
            // Header line: locate the Local and Peer columns as the first pair of
            // adjacent endpoint-looking tokens (robust to an optional State/Netid column).
            let toks: Vec<&str> = line.split_whitespace().collect();
            let at = toks.windows(2).position(|w| looks_like_endpoint(w[0]) && looks_like_endpoint(w[1]));
            let (ip, port) = match at.and_then(|i| parse_endpoint(toks[i + 1])) {
                Some((ip, port)) => (Some(ip), port),
                None => (None, 0),
            };
            let local = at.and_then(|i| parse_endpoint(toks[i])).map(|(ip, _)| ip);
            // `[State] Recv-Q Send-Q Local Peer`: the queues are the two numbers before Local.
            let (recv_q, send_q) = match at {
                Some(i) if i >= 2 => (toks[i - 2].parse().unwrap_or(0), toks[i - 1].parse().unwrap_or(0)),
                _ => (0, 0),
            };
            out.push(Sock { ip, port, local, recv_q, send_q, ..Default::default() });
            continue;
        }
        let Some(sock) = out.last_mut() else { continue };
        for tok in line.split_whitespace() {
            let Some((k, v)) = tok.split_once(':') else { continue };
            match k {
                "bytes_acked" => sock.bytes_acked = v.parse().unwrap_or(0),
                "bytes_received" => sock.bytes_received = v.parse().unwrap_or(0),
                "rtt" => sock.rtt_ms = v.split('/').next().and_then(|x| x.parse().ok()),
                // `retrans:cur/total` (or just `retrans:N` on old versions).
                "retrans" => sock.retrans = v.rsplit('/').next().and_then(|x| x.parse().ok()).unwrap_or(0),
                "cwnd" => sock.cwnd = v.parse().ok(),
                "lastsnd" => sock.last_snd_ms = v.parse().ok(),
                "lastrcv" => sock.last_rcv_ms = v.parse().ok(),
                "unacked" => sock.unacked = v.parse().unwrap_or(0),
                "lost" => sock.lost = v.parse().unwrap_or(0),
                "busy" => sock.busy_ms = millis(v),
                "rwnd_limited" => sock.rwnd_limited_ms = millis(v),
                "sndbuf_limited" => sock.sndbuf_limited_ms = millis(v),
                _ => {}
            }
        }
    }
    out.retain(|s| s.ip.is_some());
    out
}

/// Render sockets as `ss`-like text (`--dump-sockets`).
/// Each socket's own server address is used when known, else `local` (`addr:port`).
pub fn render_ss(socks: &[Sock], local: &str) -> String {
    use std::fmt::Write;
    let endpoint = |ip: IpAddr, port: u16| match ip {
        IpAddr::V4(v4) => format!("{v4}:{port}"),
        IpAddr::V6(v6) => format!("[{v6}]:{port}"),
    };
    let local_port = parse_endpoint(local).map_or(2049, |(_, p)| p);
    let mut s = String::new();
    for k in socks {
        let Some(ip) = k.ip else { continue };
        let peer = endpoint(ip, k.port);
        let here = k.local.map_or_else(|| local.to_string(), |l| endpoint(l, local_port));
        let _ = writeln!(s, "{}      {}      {here}      {peer}", k.recv_q, k.send_q);
        let _ = write!(s, "\t cubic wscale:7,7 rto:201");
        if let Some(r) = k.rtt_ms {
            let _ = write!(s, " rtt:{r:.3}/0.05");
        }
        if let Some(c) = k.cwnd {
            let _ = write!(s, " cwnd:{c}");
        }
        let _ = write!(s, " bytes_acked:{} bytes_received:{}", k.bytes_acked, k.bytes_received);
        if let Some(v) = k.last_snd_ms {
            let _ = write!(s, " lastsnd:{v}");
        }
        if let Some(v) = k.last_rcv_ms {
            let _ = write!(s, " lastrcv:{v}");
        }
        if let Some(v) = k.busy_ms {
            let _ = write!(s, " busy:{v}ms");
        }
        if let Some(v) = k.rwnd_limited_ms {
            let _ = write!(s, " rwnd_limited:{v}ms(0.0%)");
        }
        if let Some(v) = k.sndbuf_limited_ms {
            let _ = write!(s, " sndbuf_limited:{v}ms(0.0%)");
        }
        if k.unacked > 0 {
            let _ = write!(s, " unacked:{}", k.unacked);
        }
        if k.lost > 0 {
            let _ = write!(s, " lost:{}", k.lost);
        }
        let _ = writeln!(s, " retrans:0/{}", k.retrans);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
0      0        10.0.0.1:2049        10.20.7.211:707
\t cubic wscale:7,7 rto:201 rtt:0.317/0.05 ato:40 mss:8948 cwnd:10 bytes_sent:100 bytes_acked:5000 bytes_received:1234 segs_out:3 send 2.26Gbps lastsnd:12 lastrcv:34 lastack:12 retrans:0/7 rcv_space:1
0      0   [::ffff:10.0.0.1]:2049   [::ffff:10.0.2.30]:801
\t cubic rtt:1.5/0.1 bytes_acked:10 bytes_received:20
0      0   [2001:db8::1]:2049   [2001:db8::21]:900
\t cubic bytes_acked:1 bytes_received:2
";

    #[test]
    fn parses_v4_mapped_and_v6() {
        let s = parse_ss(SAMPLE);
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].ip, Some("10.20.7.211".parse().unwrap()));
        assert_eq!(s[0].port, 707);
        assert_eq!(s[0].local, Some("10.0.0.1".parse().unwrap()));
        assert_eq!(s[1].local, Some("10.0.0.1".parse().unwrap()), "IPv4-mapped server address");
        assert_eq!(s[0].bytes_acked, 5000);
        assert_eq!(s[0].bytes_received, 1234);
        assert_eq!(s[0].rtt_ms, Some(0.317));
        assert_eq!(s[0].retrans, 7);
        assert_eq!(s[0].cwnd, Some(10));
        assert_eq!(s[0].last_snd_ms, Some(12));
        assert_eq!(s[1].ip, Some("10.0.2.30".parse().unwrap()));
        assert_eq!(s[2].ip, Some("2001:db8::21".parse().unwrap()));
        assert_eq!(s[2].port, 900);
    }

    #[test]
    fn endpoint_forms() {
        assert_eq!(parse_endpoint("1.2.3.4:5"), Some(("1.2.3.4".parse().unwrap(), 5)));
        assert_eq!(parse_endpoint("::ffff:1.2.3.4:5"), Some(("1.2.3.4".parse().unwrap(), 5)));
        assert_eq!(parse_endpoint("[fe80::1%eth0]:5"), Some(("fe80::1".parse().unwrap(), 5)));
        assert_eq!(parse_endpoint("garbage"), None);
    }

    #[test]
    fn queues_and_limits() {
        let t = "\
2048 1048576 10.0.0.1:2049 10.0.0.9:1000
\t cubic busy:52340ms rwnd_limited:18ms(0.0%) sndbuf_limited:2600ms(5.0%) unacked:3 lost:1 retrans:0/9
";
        let s = &parse_ss(t)[0];
        assert_eq!((s.recv_q, s.send_q), (2048, 1048576));
        assert_eq!((s.busy_ms, s.rwnd_limited_ms, s.sndbuf_limited_ms), (Some(52340), Some(18), Some(2600)));
        assert_eq!((s.unacked, s.lost, s.retrans), (3, 1, 9));
    }

    #[test]
    fn tolerates_state_column_and_roundtrips() {
        let t = "ESTAB 7 9 10.0.0.1:2049 10.0.0.9:1000\n\t bytes_acked:9 bytes_received:8 busy:40ms rwnd_limited:4ms(10.0%) unacked:2\n";
        let s = parse_ss(t);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].port, 1000);
        assert_eq!((s[0].recv_q, s[0].send_q), (7, 9));
        let again = parse_ss(&render_ss(&s, "10.0.0.1:2049"));
        assert_eq!(again, s);
    }
}
