//! Aggregation of a client's NFSv4 open state into a file list for the drill-down view.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::IpAddr;

use crate::parse::nfsd::{ClientInfo, StateEntry, parse_info, parse_mountinfo, parse_states};
use crate::source::Source;

#[derive(Debug, Clone, PartialEq)]
pub struct OpenFile {
    /// Mount point of the filesystem holding the file (or `dev MAJ:MIN`).
    pub export: String,
    /// Parent directory name (the kernel only reports the last two path components).
    pub dir: String,
    pub name: String,
    /// open / lock / deleg / layout ...
    pub kinds: BTreeSet<String>,
    pub read: bool,
    pub write: bool,
    /// Number of state entries referring to this file.
    pub count: usize,
}

impl OpenFile {
    pub fn access(&self) -> &'static str {
        match (self.read, self.write) {
            (true, true) => "rw",
            (true, false) => "r-",
            (false, true) => "-w",
            (false, false) => "--",
        }
    }

    pub fn path(&self) -> String {
        format!("{}/{}", self.dir, self.name)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OpenFiles {
    /// NFSv4 client records matched for this IP.
    pub clients: Vec<ClientInfo>,
    /// Sorted by export, dir, name.
    pub files: Vec<OpenFile>,
    pub entries: usize,
    pub kind_counts: BTreeMap<String, usize>,
    /// Non-fatal errors (e.g. one client's states file unreadable).
    pub errors: Vec<String>,
}

impl OpenFiles {
    pub fn build(clients: Vec<ClientInfo>, entries: &[StateEntry], mounts: &HashMap<(u32, u32), String>) -> Self {
        let mut map: BTreeMap<(String, String, String), OpenFile> = BTreeMap::new();
        let mut kind_counts = BTreeMap::new();
        for e in entries {
            *kind_counts.entry(e.kind.clone()).or_insert(0) += 1;
            let export = match e.dev {
                Some(d) => mounts.get(&d).cloned().unwrap_or_else(|| format!("dev {}:{}", d.0, d.1)),
                None => "?".to_string(),
            };
            let fname = e.filename.clone().unwrap_or_else(|| match e.ino {
                Some(i) => format!("<inode {i}>"),
                None => "<unknown>".into(),
            });
            let (dir, name) = match fname.rsplit_once('/') {
                Some((d, n)) => (d.to_string(), n.to_string()),
                None => (".".to_string(), fname),
            };
            let f = map.entry((export.clone(), dir.clone(), name.clone())).or_insert_with(|| OpenFile {
                export,
                dir,
                name,
                kinds: BTreeSet::new(),
                read: false,
                write: false,
                count: 0,
            });
            f.kinds.insert(e.kind.clone());
            f.count += 1;
            if let Some(a) = &e.access {
                f.read |= a.contains('r');
                f.write |= a.contains('w');
            }
        }
        OpenFiles { clients, files: map.into_values().collect(), entries: entries.len(), kind_counts, errors: vec![] }
    }

    pub fn dir_count(&self) -> usize {
        self.files.iter().map(|f| (&f.export, &f.dir)).collect::<BTreeSet<_>>().len()
    }

    pub fn writers(&self) -> usize {
        self.files.iter().filter(|f| f.write).count()
    }
}

/// Load the open state for every NFSv4 client record with address `ip`.
pub fn load(src: &mut dyn Source, ip: IpAddr) -> Result<OpenFiles, String> {
    let infos = src.client_infos().map_err(|e| format!("{e:#}"))?;
    let clients: Vec<ClientInfo> = infos.iter().map(|(id, t)| parse_info(id, t)).filter(|c| c.ip == Some(ip)).collect();
    let mounts = parse_mountinfo(&src.mountinfo().unwrap_or_default());
    let mut entries = Vec::new();
    let mut errors = Vec::new();
    for c in &clients {
        match src.states(&c.id) {
            Ok(t) => entries.extend(parse_states(&t)),
            Err(e) => errors.push(format!("client {}: {e:#}", c.id)),
        }
    }
    let mut of = OpenFiles::build(clients, &entries, &mounts);
    of.errors = errors;
    Ok(of)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_open_and_deleg() {
        let s = parse_states(
            r#"- 0x1: { type: open, access: r, deny: --, superblock: "fd:10:1", filename: "a/x.dcm" }
- 0x2: { type: deleg, access: r, superblock: "fd:10:1", filename: "a/x.dcm" }
- 0x3: { type: open, access: rw, deny: --, superblock: "fd:10:2", filename: "b/y.log" }
- 0x4: { type: open, access: r, deny: --, superblock: "8:1:3", filename: "top" }
"#,
        );
        let mounts = HashMap::from([((253, 16), "/export/data".to_string())]);
        let of = OpenFiles::build(vec![], &s, &mounts);
        assert_eq!(of.entries, 4);
        assert_eq!(of.files.len(), 3);
        let x = &of.files[0];
        assert_eq!((x.export.as_str(), x.dir.as_str(), x.name.as_str()), ("/export/data", "a", "x.dcm"));
        assert_eq!(x.count, 2);
        assert_eq!(x.kinds.iter().cloned().collect::<Vec<_>>(), ["deleg", "open"]);
        assert_eq!(x.access(), "r-");
        assert_eq!(of.files[1].access(), "rw");
        assert_eq!(of.files[2].export, "dev 8:1");
        assert_eq!(of.files[2].dir, ".");
        assert_eq!(of.kind_counts["open"], 3);
        assert_eq!(of.dir_count(), 3);
        assert_eq!(of.writers(), 1);
    }
}
