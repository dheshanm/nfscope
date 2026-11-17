//! Aggregation of a client's NFSv4 open state into a file list for the drill-down view, and
//! on-demand lookup of a file's full path (the kernel only reports `parent/name`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::parse::nfsd::{ClientInfo, StateEntry, parse_info, parse_mountinfo, parse_states};
use crate::source::Source;

#[derive(Debug, Clone, PartialEq)]
pub struct OpenFile {
    /// Mount point of the filesystem holding the file (or `dev MAJ:MIN`).
    pub export: String,
    /// Parent directory *name* only: the kernel reports the last two path components, so every
    /// directory called `IMAGES` lands in the same group. `""` means the filesystem root.
    pub dir: String,
    pub name: String,
    /// `(major, minor)` of the filesystem and the inode: what identifies the file.
    pub dev: Option<(u32, u32)>,
    pub ino: Option<u64>,
    /// open / lock / deleg / layout ...
    pub kinds: BTreeSet<String>,
    pub read: bool,
    pub write: bool,
    /// Number of state entries referring to this file.
    pub count: usize,
    /// Tree id among its group: the name, or `name#inode` when the group has two files of
    /// that name (e.g. `IMAGES/0001.dcm` from two different `IMAGES` directories).
    pub leaf: String,
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

    /// A file the client merely holds a cached delegation for (not currently opened).
    pub fn deleg_only(&self) -> bool {
        self.kinds.len() == 1 && self.kinds.contains("deleg")
    }

    /// Key for the resolved-path cache: the export plus the inode.
    pub fn key(&self) -> Option<FileKey> {
        Some((self.export.clone(), self.ino?))
    }
}

pub type FileKey = (String, u64);

/// [`OpenFile::dir`] of files the kernel prints as `/`: nfsd reached them by file handle and
/// holds a *disconnected* dentry, one with no name or parent (an export with
/// `no_subtree_check` never reconnects regular files). Only the inode is known. A real
/// directory name can't contain `/`, so this can't clash with one.
pub const ANON_DIR: &str = "//anon";

/// How a directory group is labelled: the kernel only knows the parent's own name, so it is
/// shown as `…/name/`; files directly in the filesystem root (kernel path `/name`) as `./`.
pub fn dir_label(dir: &str) -> String {
    match dir {
        ANON_DIR => "(name unknown to the kernel)".into(),
        "" => "./".into(),
        "." => "?/".into(),
        d => format!("…/{d}/"),
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
        // Same inode = same file; without an inode, fall back to the (ambiguous) name.
        #[derive(PartialEq, Eq, Hash)]
        enum Id {
            Ino((u32, u32), u64),
            Name(String, String, String),
        }
        let mut index: HashMap<Id, usize> = HashMap::new();
        let mut files: Vec<OpenFile> = Vec::new();
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
                // Disconnected dentry: no name at all, just the inode.
                Some(("", "")) => {
                    (ANON_DIR.to_string(), e.ino.map_or_else(|| "<unknown>".into(), |i| format!("inode {i}")))
                }
                Some((d, n)) => (d.to_string(), n.to_string()),
                None => (".".to_string(), fname),
            };
            let id = match (e.dev, e.ino) {
                (Some(d), Some(i)) => Id::Ino(d, i),
                _ => Id::Name(export.clone(), dir.clone(), name.clone()),
            };
            let i = *index.entry(id).or_insert_with(|| {
                files.push(OpenFile {
                    export,
                    dir,
                    name,
                    dev: e.dev,
                    ino: e.ino,
                    kinds: BTreeSet::new(),
                    read: false,
                    write: false,
                    count: 0,
                    leaf: String::new(),
                });
                files.len() - 1
            });
            let f = &mut files[i];
            f.kinds.insert(e.kind.clone());
            f.count += 1;
            if let Some(a) = &e.access {
                f.read |= a.contains('r');
                f.write |= a.contains('w');
            }
        }
        files.sort_by(|a, b| (&a.export, &a.dir, &a.name, a.ino).cmp(&(&b.export, &b.dir, &b.name, b.ino)));
        // Sorted, so same-named files in a group are neighbours.
        for i in 0..files.len() {
            let same = |j: usize| {
                files.get(j).is_some_and(|o: &OpenFile| {
                    (&o.export, &o.dir, &o.name) == (&files[i].export, &files[i].dir, &files[i].name)
                })
            };
            let dup = (i > 0 && same(i - 1)) || same(i + 1);
            let f = &files[i];
            let leaf = match (dup, f.ino) {
                (true, Some(ino)) => format!("{}#{ino}", f.name),
                _ => f.name.clone(),
            };
            files[i].leaf = leaf;
        }
        OpenFiles { clients, files, entries: entries.len(), kind_counts, errors: vec![] }
    }

    /// Distinct parent directory *names* (not directories: see [`OpenFile::dir`]).
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

/// A file to locate: the kernel's `parent/name` for it plus its inode.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub dir: String,
    pub name: String,
    pub ino: u64,
}

impl Target {
    pub fn of(f: &OpenFile) -> Option<Self> {
        Some(Target { dir: f.dir.clone(), name: f.name.clone(), ino: f.ino? })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindEnd {
    /// Every target was found.
    AllFound,
    /// The whole filesystem was searched; the rest are gone (closed, renamed or unlinked).
    Exhausted,
    TimedOut,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FindResult {
    /// inode → full path.
    pub found: HashMap<u64, PathBuf>,
    pub dirs: u64,
    pub end: FindEnd,
}

/// Walk the filesystem mounted at `root` (without crossing into other mounts) looking for each
/// target as `<a directory named target.dir>/<target.name>` with the target's inode. Reads
/// directories only, and stops as soon as every target is found. Targets in [`ANON_DIR`] have
/// no name, so they are matched on the inode of every directory entry (`d_ino` from readdir:
/// no extra syscalls).
///
/// The walk is breadth-first: working directories tend to be a few levels down, and big
/// archives are mostly deep trees (a depth-first walk of one took 7 minutes to reach a
/// depth-5 directory that breadth-first gets to in seconds). Once [`FIND_QUEUE_CAP`]
/// directories are queued, new ones go to the front instead, finishing subtrees depth-first so
/// memory stays bounded. `progress(dirs_scanned)` is
/// called every so often; returning `false` cancels.
pub const FIND_QUEUE_CAP: usize = 250_000;

pub fn find_paths(
    root: &Path,
    targets: &[Target],
    budget: Duration,
    progress: &mut dyn FnMut(u64) -> bool,
) -> std::io::Result<FindResult> {
    use std::os::unix::fs::MetadataExt;
    let started = Instant::now();
    let dev = std::fs::symlink_metadata(root)?.dev();
    let mut by_parent: HashMap<&str, Vec<&Target>> = HashMap::new();
    for t in targets {
        by_parent.entry(t.dir.as_str()).or_default().push(t);
    }
    let want: BTreeSet<u64> = targets.iter().map(|t| t.ino).collect();
    let anon: BTreeSet<u64> = targets.iter().filter(|t| t.dir == ANON_DIR).map(|t| t.ino).collect();
    let mut found: HashMap<u64, PathBuf> = HashMap::new();
    // Look for `dir/<name>` of every target whose parent is called like `dir`.
    let check = |dir: &Path, parent: &str, found: &mut HashMap<u64, PathBuf>| {
        for t in by_parent.get(parent).into_iter().flatten() {
            if found.contains_key(&t.ino) {
                continue;
            }
            let p = dir.join(&t.name);
            if std::fs::symlink_metadata(&p).is_ok_and(|m| m.ino() == t.ino && m.dev() == dev) {
                found.insert(t.ino, p);
            }
        }
    };
    // Kernel path `/name`: the file sits directly in the filesystem root.
    check(root, "", &mut found);
    if let Some(n) = root.file_name() {
        check(root, &n.to_string_lossy(), &mut found);
    }
    let mut queue = std::collections::VecDeque::from([root.to_path_buf()]);
    let mut dirs = 0u64;
    let mut last_report = Instant::now();
    let end = loop {
        if want.iter().all(|i| found.contains_key(i)) {
            break FindEnd::AllFound;
        }
        let Some(d) = queue.pop_front() else { break FindEnd::Exhausted };
        dirs += 1;
        if last_report.elapsed() >= Duration::from_millis(200) {
            last_report = Instant::now();
            if !progress(dirs) {
                break FindEnd::Cancelled;
            }
            if started.elapsed() > budget {
                break FindEnd::TimedOut;
            }
        }
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for ent in rd.flatten() {
            if !anon.is_empty() {
                use std::os::unix::fs::DirEntryExt;
                let ino = ent.ino();
                if anon.contains(&ino)
                    && !found.contains_key(&ino)
                    && ent.metadata().is_ok_and(|m| m.ino() == ino && m.dev() == dev)
                {
                    found.insert(ino, ent.path());
                }
            }
            // d_type first (no syscall), so only directories get an lstat; symlinks aren't
            // followed. The lstat is for the device: don't wander into other mounts.
            if !ent.file_type().is_ok_and(|t| t.is_dir()) || !ent.metadata().is_ok_and(|m| m.dev() == dev) {
                continue;
            }
            let p = ent.path();
            check(&p, &ent.file_name().to_string_lossy(), &mut found);
            if queue.len() < FIND_QUEUE_CAP {
                queue.push_back(p);
            } else {
                queue.push_front(p);
            }
        }
    };
    found.retain(|i, _| want.contains(i));
    Ok(FindResult { found, dirs, end })
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

    #[test]
    fn same_dir_name_different_dirs_stay_apart() {
        let s = parse_states(
            r#"- 0x1: { type: open, access: r, deny: --, superblock: "0:39:10", filename: "IMAGES/0001.dcm" }
- 0x2: { type: open, access: r, deny: --, superblock: "0:39:11", filename: "IMAGES/0001.dcm" }
- 0x3: { type: deleg, access: r, superblock: "0:39:10", filename: "IMAGES/0001.dcm" }
- 0x4: { type: open, access: r, deny: --, superblock: "0:39:12", filename: "/top.txt" }
- 0x5: { type: open, access: r-, deny: --, superblock: "00:39:81833716", filename: "/" }
- 0x6: { type: deleg, access: r, superblock: "00:39:81833716", filename: "/" }
"#,
        );
        let mounts = HashMap::from([((0, 57), "/pool0/data".to_string())]);
        let of = OpenFiles::build(vec![], &s, &mounts);
        assert_eq!(of.files.len(), 4, "two different 0001.dcm, top.txt and a nameless one");
        assert_eq!(of.files[0].dir, "");
        assert_eq!(dir_label(&of.files[0].dir), "./");
        assert_eq!((of.files[2].ino, of.files[2].count), (Some(10), 2));
        assert_eq!((of.files[3].ino, of.files[3].count), (Some(11), 1));
        assert_eq!((of.files[2].leaf.as_str(), of.files[3].leaf.as_str()), ("0001.dcm#10", "0001.dcm#11"));
        assert_eq!(of.files[0].leaf, "top.txt");
        assert_eq!(dir_label("IMAGES"), "…/IMAGES/");
        let a = of.files.iter().find(|f| f.ino == Some(81833716)).unwrap();
        assert_eq!((a.dir.as_str(), a.name.as_str(), a.count), (ANON_DIR, "inode 81833716", 2));
        assert_eq!(dir_label(&a.dir), "(name unknown to the kernel)");
        assert_eq!(of.dir_count(), 3);
    }

    #[test]
    fn finds_full_paths_by_inode() {
        use std::os::unix::fs::MetadataExt;
        let root = std::env::temp_dir().join(format!("nfscope-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["a/s1/IMAGES", "b/s2/IMAGES", "c/code"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        for f in ["a/s1/IMAGES/x.dcm", "b/s2/IMAGES/x.dcm", "c/code/run.py", "top.txt"] {
            std::fs::write(root.join(f), "").unwrap();
        }
        let ino = |p: &str| std::fs::metadata(root.join(p)).unwrap().ino();
        let t = |dir: &str, name: &str, p: &str| Target { dir: dir.into(), name: name.into(), ino: ino(p) };
        let targets = [
            t("IMAGES", "x.dcm", "b/s2/IMAGES/x.dcm"),
            t("code", "run.py", "c/code/run.py"),
            t("", "top.txt", "top.txt"),
        ];
        let r = find_paths(&root, &targets, Duration::from_secs(30), &mut |_| true).unwrap();
        assert_eq!(r.end, FindEnd::AllFound);
        assert_eq!(r.found[&targets[0].ino], root.join("b/s2/IMAGES/x.dcm"));
        assert_eq!(r.found[&targets[1].ino], root.join("c/code/run.py"));
        assert_eq!(r.found[&targets[2].ino], root.join("top.txt"));

        // Name unknown (kernel printed "/"): found by inode alone.
        let anon = [Target { dir: ANON_DIR.into(), name: "inode".into(), ino: ino("a/s1/IMAGES/x.dcm") }];
        let r = find_paths(&root, &anon, Duration::from_secs(30), &mut |_| true).unwrap();
        assert_eq!(r.found[&anon[0].ino], root.join("a/s1/IMAGES/x.dcm"));

        // A file that no longer exists: whole tree searched, nothing found.
        let gone = [Target { dir: "IMAGES".into(), name: "gone.dcm".into(), ino: u64::MAX }];
        let r = find_paths(&root, &gone, Duration::from_secs(30), &mut |_| true).unwrap();
        assert_eq!((r.end, r.found.len()), (FindEnd::Exhausted, 0));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Breadth-first: a shallow target is found without walking the deep trees beside it; a
    /// target 20 levels down is still found; an exhaustive search lists each directory once.
    #[test]
    fn find_goes_shallow_first() {
        let root = std::env::temp_dir().join(format!("nfscope-passes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // 4 deep archives, each a chain 20 levels deep with 3 side directories per level.
        let mut total = 0;
        for a in 0..4 {
            let mut d = root.join(format!("archive{a}"));
            total += 1;
            for lvl in 0..20 {
                for side in 0..3 {
                    std::fs::create_dir_all(d.join(format!("side{side}"))).unwrap();
                    total += 1;
                }
                d = d.join(format!("l{lvl}"));
                std::fs::create_dir_all(&d).unwrap();
                total += 1;
            }
            if a == 3 {
                std::fs::write(d.join("deep.dat"), "").unwrap();
            }
        }
        std::fs::create_dir_all(root.join("proj/code")).unwrap();
        total += 2;
        std::fs::write(root.join("proj/code/run.py"), "").unwrap();
        let ino = |p: PathBuf| std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(p).unwrap());
        let deep_path = (0..20).fold(root.join("archive3"), |d, l| d.join(format!("l{l}"))).join("deep.dat");

        let shallow = [Target { dir: "code".into(), name: "run.py".into(), ino: ino(root.join("proj/code/run.py")) }];
        let r = find_paths(&root, &shallow, Duration::from_secs(30), &mut |_| true).unwrap();
        assert_eq!(r.end, FindEnd::AllFound);
        assert_eq!(r.found[&shallow[0].ino], root.join("proj/code/run.py"));
        assert!(r.dirs <= 6, "root and the top level only, listed {}", r.dirs);

        let deep = [Target { dir: "l19".into(), name: "deep.dat".into(), ino: ino(deep_path.clone()) }];
        let r = find_paths(&root, &deep, Duration::from_secs(30), &mut |_| true).unwrap();
        assert_eq!((r.end, &r.found[&deep[0].ino]), (FindEnd::AllFound, &deep_path));

        let anon = [Target { dir: ANON_DIR.into(), name: "inode".into(), ino: ino(deep_path.clone()) }];
        let r = find_paths(&root, &anon, Duration::from_secs(30), &mut |_| true).unwrap();
        assert_eq!(r.found[&anon[0].ino], deep_path);

        let gone = [Target { dir: "code".into(), name: "gone.py".into(), ino: u64::MAX }];
        let r = find_paths(&root, &gone, Duration::from_secs(30), &mut |_| true).unwrap();
        assert_eq!((r.end, r.dirs), (FindEnd::Exhausted, total + 1), "every directory (and the root) listed once");

        let r = find_paths(&root, &gone, Duration::from_secs(30), &mut |_| false).unwrap();
        assert!(matches!(r.end, FindEnd::Cancelled | FindEnd::Exhausted), "{:?}", r.end);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
