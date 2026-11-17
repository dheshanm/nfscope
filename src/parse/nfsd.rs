//! Parsers for `/proc/fs/nfsd/clients/<id>/{info,states}` and `/proc/self/mountinfo`.

use std::collections::HashMap;
use std::net::IpAddr;

use super::ss::parse_endpoint;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClientInfo {
    /// Directory name under /proc/fs/nfsd/clients.
    pub id: String,
    pub ip: Option<IpAddr>,
    pub name: Option<String>,
    pub status: Option<String>,
    pub minor_version: Option<u32>,
    /// All `key: value` fields in file order (quotes stripped), for the detail view.
    pub fields: Vec<(String, String)>,
}

impl ClientInfo {
    pub fn field(&self, key: &str) -> Option<&str> {
        self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
    pub fn seconds_since_renew(&self) -> Option<u64> {
        self.field("seconds from last renew")?.parse().ok()
    }
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    match v.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
        Some(inner) => unescape(inner),
        None => v.to_string(),
    }
}

/// Undo kernel `seq_escape`-style quoting (`\xHH`, `\ooo`, `\\`, `\"`).
pub fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            let n = b[i + 1];
            let num = |range: std::ops::Range<usize>, radix| {
                b.get(range).and_then(|d| std::str::from_utf8(d).ok()).and_then(|d| u8::from_str_radix(d, radix).ok())
            };
            if n == b'x' {
                if let Some(v) = num(i + 2..i + 4, 16) {
                    out.push(v);
                    i += 4;
                    continue;
                }
            } else if (b'0'..=b'7').contains(&n) {
                if let Some(v) = num(i + 1..i + 4, 8) {
                    out.push(v);
                    i += 4;
                    continue;
                }
            } else {
                out.push(n);
                i += 2;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn parse_info(id: &str, text: &str) -> ClientInfo {
    let mut ci = ClientInfo { id: id.to_string(), ..Default::default() };
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let k = k.trim().to_string();
        let v = unquote(v);
        match k.as_str() {
            "address" => ci.ip = parse_endpoint(&v).map(|(ip, _)| ip),
            "name" => ci.name = Some(v.clone()),
            "status" => ci.status = Some(v.clone()),
            "minor version" => ci.minor_version = v.parse().ok(),
            _ => {}
        }
        ci.fields.push((k, v));
    }
    ci
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct StateEntry {
    /// open | lock | deleg | layout | ...
    pub kind: String,
    pub access: Option<String>,
    pub deny: Option<String>,
    /// (major, minor) device numbers, decimal.
    pub dev: Option<(u32, u32)>,
    pub ino: Option<u64>,
    /// Last two path components (`%pD2`), e.g. `dir/file`.
    pub filename: Option<String>,
}

/// Parse the `{ key: value, key: "quoted", ... }` body of a states line.
fn parse_kv_body(body: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut chars = body.char_indices().peekable();
    let bytes = body.as_bytes();
    loop {
        // skip separators
        while let Some(&(_, c)) = chars.peek() {
            if c == ',' || c.is_whitespace() || c == '{' {
                chars.next();
            } else {
                break;
            }
        }
        let Some(&(start, c)) = chars.peek() else { break };
        if c == '}' {
            break;
        }
        // key
        let mut kend = start;
        while let Some(&(i, c)) = chars.peek() {
            if c == ':' {
                kend = i;
                chars.next();
                break;
            }
            kend = i + c.len_utf8();
            chars.next();
        }
        let key = body[start..kend].trim().to_string();
        while matches!(chars.peek(), Some(&(_, ' '))) {
            chars.next();
        }
        // value
        let val = match chars.peek() {
            Some(&(vs, '"')) => {
                chars.next();
                let mut end = bytes.len();
                let mut esc = false;
                for (i, c) in chars.by_ref() {
                    if esc {
                        esc = false;
                    } else if c == '\\' {
                        esc = true;
                    } else if c == '"' {
                        end = i;
                        break;
                    }
                }
                unescape(&body[vs + 1..end])
            }
            Some(&(vs, _)) => {
                let mut end = bytes.len();
                while let Some(&(i, c)) = chars.peek() {
                    if c == ',' || c == '}' {
                        end = i;
                        break;
                    }
                    chars.next();
                }
                body[vs..end].trim().to_string()
            }
            None => String::new(),
        };
        if key.is_empty() {
            break;
        }
        out.push((key, val));
    }
    out
}

pub fn parse_states(text: &str) -> Vec<StateEntry> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(open) = line.find('{') else { continue };
        let mut e = StateEntry::default();
        for (k, v) in parse_kv_body(&line[open..]) {
            match k.as_str() {
                "type" => e.kind = v,
                "access" => e.access = Some(v),
                "deny" => e.deny = Some(v),
                "superblock" => {
                    // "maj:min:ino": the kernel prints `%02x:%02x:%ld`, so the device is hex but
                    // the inode is decimal (what `stat` / `ls -i` show).
                    let mut it = v.split(':');
                    let maj = it.next().and_then(|x| u32::from_str_radix(x, 16).ok());
                    let min = it.next().and_then(|x| u32::from_str_radix(x, 16).ok());
                    e.ino = it.next().and_then(|x| x.parse::<u64>().ok());
                    if let (Some(a), Some(b)) = (maj, min) {
                        e.dev = Some((a, b));
                    }
                }
                "filename" => e.filename = Some(v),
                _ => {}
            }
        }
        if !e.kind.is_empty() || e.filename.is_some() {
            out.push(e);
        }
    }
    out
}

/// Map `(major, minor)` → mount point from `/proc/self/mountinfo`.
/// When a device is mounted several times, prefer the mount of its root (`/`), then the shortest path.
pub fn parse_mountinfo(text: &str) -> HashMap<(u32, u32), String> {
    let mut best: HashMap<(u32, u32), (bool, String)> = HashMap::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 5 {
            continue;
        }
        let Some((a, b)) = f[2].split_once(':') else { continue };
        let (Ok(a), Ok(b)) = (a.parse::<u32>(), b.parse::<u32>()) else { continue };
        let is_root = f[3] == "/";
        let mp = unescape(f[4]);
        let better = match best.get(&(a, b)) {
            None => true,
            Some((r, cur)) => (is_root && !r) || (is_root == *r && mp.len() < cur.len()),
        };
        if better {
            best.insert((a, b), (is_root, mp));
        }
    }
    best.into_iter().map(|(k, (_, v))| (k, v)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO: &str = r#"clientid: 0x6d0f67a65f1c8e9e
address: "10.20.7.211:707"
status: confirmed
seconds from last renew: 23
name: "Linux NFSv4.2 client-01.example.org"
minor version: 2
Implementation domain: "kernel.org"
Implementation name: "Linux 6.8.0-45-generic #45-Ubuntu SMP x86_64"
Implementation time: [0, 0]
callback state: UP
callback address: 10.20.7.211:0
"#;

    #[test]
    fn info() {
        let c = parse_info("42", INFO);
        assert_eq!(c.ip, Some("10.20.7.211".parse().unwrap()));
        assert_eq!(c.name.as_deref(), Some("Linux NFSv4.2 client-01.example.org"));
        assert_eq!(c.minor_version, Some(2));
        assert_eq!(c.status.as_deref(), Some("confirmed"));
        assert_eq!(c.seconds_since_renew(), Some(23));
        assert_eq!(c.field("Implementation name"), Some("Linux 6.8.0-45-generic #45-Ubuntu SMP x86_64"));
    }

    #[test]
    fn info_v6() {
        let c = parse_info("1", "address: \"[2001:db8::21]:900\"\n");
        assert_eq!(c.ip, Some("2001:db8::21".parse().unwrap()));
    }

    #[test]
    fn states() {
        let t = r#"- 0x00000001c5e1a6650b48c9f3: { type: open, access: rw, deny: --, superblock: "fd:10:3541", filename: "study_a/scan, \"001\".dcm", owner: "open id:\x00\x00\x00>" }
- 0x00000002c5e1a6650b48c9f3: { type: deleg, access: r, superblock: "fd:10:3542", filename: "study_a/scan_002.dcm" }
- 0x00000003c5e1a6650b48c9f3: { type: lock, superblock: "08:01:3543", filename: "logs/run.log", owner: "lock id:\x01" }
"#;
        let s = parse_states(t);
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].kind, "open");
        assert_eq!(s[0].access.as_deref(), Some("rw"));
        assert_eq!(s[0].dev, Some((253, 16)));
        assert_eq!(s[0].ino, Some(3541));
        assert_eq!(s[0].filename.as_deref(), Some("study_a/scan, \"001\".dcm"));
        assert_eq!(s[1].kind, "deleg");
        assert_eq!(s[2].kind, "lock");
        assert_eq!(s[2].dev, Some((8, 1)));
        assert_eq!(s[2].access, None);
    }

    #[test]
    fn mountinfo() {
        let t = "\
22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
40 22 253:16 / /export/data rw,relatime shared:2 - xfs /dev/mapper/vg-data rw
41 22 253:16 /sub /srv/bind rw,relatime shared:2 - xfs /dev/mapper/vg-data rw
42 22 253:17 / /export/with\\040space rw - xfs /dev/x rw
";
        let m = parse_mountinfo(t);
        assert_eq!(m[&(253, 16)], "/export/data");
        assert_eq!(m[&(8, 1)], "/");
        assert_eq!(m[&(253, 17)], "/export/with space");
    }

    #[test]
    fn unescape_forms() {
        assert_eq!(unescape(r"a\x41b"), "aAb");
        assert_eq!(unescape(r"a\040b"), "a b");
        assert_eq!(unescape(r#"a\"b\\c"#), "a\"b\\c");
        assert_eq!(unescape(r"trail\"), "trail\\");
    }

    #[test]
    fn superblock_device_is_hex_inode_is_decimal() {
        // Captured on a live server; `ls -i` on the file gives 595568 and mountinfo has 0:52.
        let s = parse_states(
            r#"- 0x1: { type: deleg, access: r, superblock: "00:34:595568", filename: "inf_invivo/epoch_4.pklv4" }"#,
        );
        assert_eq!((s[0].dev, s[0].ino), (Some((0, 52)), Some(595568)));
    }
}
