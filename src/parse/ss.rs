//! Parser for `ss -tniH state established '( sport = :2049 )'` output.
//!
//! Each socket is a header line (`Recv-Q Send-Q Local:Port Peer:Port`) followed by
//! one or more indented lines of `key:value` TCP info.

use std::net::IpAddr;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sock {
    pub ip: Option<IpAddr>,
    pub port: u16,
    /// Bytes the client has acknowledged receiving (server → client, i.e. client READ).
    pub bytes_acked: u64,
    /// Bytes the server has received (client → server, i.e. client WRITE).
    pub bytes_received: u64,
    pub rtt_ms: Option<f64>,
    pub retrans: u64,
    pub cwnd: Option<u64>,
    pub last_snd_ms: Option<u64>,
    pub last_rcv_ms: Option<u64>,
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
            let peer = toks
                .windows(2)
                .find(|w| looks_like_endpoint(w[0]) && looks_like_endpoint(w[1]))
                .and_then(|w| parse_endpoint(w[1]));
            let (ip, port) = match peer {
                Some((ip, port)) => (Some(ip), port),
                None => (None, 0),
            };
            out.push(Sock { ip, port, ..Default::default() });
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
                _ => {}
            }
        }
    }
    out.retain(|s| s.ip.is_some());
    out
}

/// Render sockets back into `ss`-like text (used by the fixture source to synthesise samples).
pub fn render_ss(socks: &[Sock], local: &str) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    for k in socks {
        let Some(ip) = k.ip else { continue };
        let peer = match ip {
            IpAddr::V4(v4) => format!("{v4}:{}", k.port),
            IpAddr::V6(v6) => format!("[{v6}]:{}", k.port),
        };
        let _ = writeln!(s, "0      0      {local}      {peer}");
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
        let _ = writeln!(s, " retrans:0/{}", k.retrans);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
0      0        10.0.0.1:2049        10.156.77.211:707
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
        assert_eq!(s[0].ip, Some("10.156.77.211".parse().unwrap()));
        assert_eq!(s[0].port, 707);
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
    fn tolerates_state_column_and_roundtrips() {
        let t = "ESTAB 0 0 10.0.0.1:2049 10.0.0.9:1000\n\t bytes_acked:9 bytes_received:8\n";
        let s = parse_ss(t);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].port, 1000);
        let again = parse_ss(&render_ss(&s, "10.0.0.1:2049"));
        assert_eq!(again, s);
    }
}
