//! Snapshots of scans ("what grew"): the sizes of the larger folders, saved after every scan and compared
//! with an earlier one — "+12 GB in AppData/Local/Temp over the last week".
//!
//! A snapshot keeps only folders of at least [`MIN_DIR`] bytes, as a small text file (one folder per line),
//! so even a drive with millions of files takes a few megabytes.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use crate::tree::{Kind, NodeId, ROOT, Tree};

/// Smaller folders are not recorded.
pub const MIN_DIR: u64 = 1 << 20;
const MAX_DIRS: usize = 300_000;
/// Snapshots kept per scanned location.
pub const KEEP: usize = 40;
/// A scan this soon after the previous one replaces it instead of piling up (F5 after a cleanup).
const REPLACE_WITHIN_SECS: u64 = 3600;
const MAGIC: &str = "SPACETREE-SNAPSHOT 1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// The scanned location, as given to the scan.
    pub root: String,
    /// Unix seconds.
    pub taken: u64,
    /// Total size of everything under `root`.
    pub total: u64,
    /// `(path relative to root with '/' separators ("" = root itself), size)`, sorted by path.
    pub dirs: Vec<(String, u64)>,
}

impl Snapshot {
    pub fn from_tree(tree: &Tree, taken: u64) -> Snapshot {
        let mut dirs = Vec::new();
        let mut stack: Vec<(NodeId, String)> = vec![(ROOT, String::new())];
        while let Some((id, rel)) = stack.pop() {
            for &c in tree.children(id) {
                let nd = tree.node(c);
                // children are sorted by size: the rest is smaller still
                if nd.size < MIN_DIR {
                    break;
                }
                let name = tree.name(c);
                if nd.kind == Kind::Dir && !name.contains(['\t', '\n', '\r', '/']) {
                    let path = if rel.is_empty() {
                        name.to_owned()
                    } else {
                        format!("{rel}/{name}")
                    };
                    stack.push((c, path));
                }
            }
            if tree.node(id).size >= MIN_DIR {
                dirs.push((rel, tree.node(id).size));
            }
        }
        if dirs.len() > MAX_DIRS {
            dirs.sort_unstable_by_key(|d| std::cmp::Reverse(d.1));
            dirs.truncate(MAX_DIRS);
        }
        dirs.sort_unstable();
        Snapshot {
            root: tree.name(ROOT).to_owned(),
            taken,
            total: tree.root_stats().0,
            dirs,
        }
    }

    pub fn write(&self, mut w: impl Write) -> io::Result<()> {
        writeln!(
            w,
            "{MAGIC}\nroot\t{}\ntaken\t{}\ntotal\t{}",
            self.root, self.taken, self.total
        )?;
        for (p, s) in &self.dirs {
            writeln!(w, "{s}\t{p}")?;
        }
        Ok(())
    }

    pub fn read(r: impl BufRead) -> io::Result<Snapshot> {
        let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, format!("bad snapshot: {what}"));
        let mut lines = r.lines();
        let mut next = || lines.next().unwrap_or_else(|| Err(bad("truncated")));
        if next()? != MAGIC {
            return Err(bad("header"));
        }
        let mut field = |key: &str| -> io::Result<String> {
            let l = next()?;
            l.strip_prefix(key)
                .and_then(|r| r.strip_prefix('\t'))
                .map(str::to_owned)
                .ok_or_else(|| bad(key))
        };
        let root = field("root")?;
        let taken = field("taken")?.parse().map_err(|_| bad("taken"))?;
        let total = field("total")?.parse().map_err(|_| bad("total"))?;
        let mut dirs = Vec::new();
        for l in lines {
            let l = l?;
            let (s, p) = l.split_once('\t').ok_or_else(|| bad("line"))?;
            // `diff` works in i64; no real folder comes near 2^63 bytes, so larger values mean a corrupt file.
            let size: u64 = s
                .parse()
                .ok()
                .filter(|&v| v <= i64::MAX as u64)
                .ok_or_else(|| bad("size"))?;
            dirs.push((p.to_owned(), size));
        }
        dirs.sort_unstable();
        Ok(Snapshot {
            root,
            taken,
            total,
            dirs,
        })
    }
}

/// A stored snapshot file.
#[derive(Clone, Debug)]
pub struct Entry {
    pub taken: u64,
    pub path: PathBuf,
}

/// Folder of the snapshots of one scanned location, e.g. `C:\` → `<base>/c__`.
fn series_dir(base: &Path, root: &str) -> PathBuf {
    let name: String = root
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    base.join(if name.is_empty() { "_".into() } else { name })
}

/// The `root` line of a stored snapshot (only the header is read).
fn stored_root(path: &Path) -> Option<String> {
    let mut lines = io::BufReader::new(std::fs::File::open(path).ok()?).lines();
    if lines.next()?.ok()? != MAGIC {
        return None;
    }
    lines.next()?.ok()?.strip_prefix("root\t").map(str::to_owned)
}

/// Stored snapshots of `root`, oldest first. Different roots can map to the same series folder
/// (`E:\a b`, `E:\a_b`), so each file's recorded root is checked (case-insensitively, as Windows paths are).
pub fn list(base: &Path, root: &str) -> Vec<Entry> {
    let Ok(rd) = std::fs::read_dir(series_dir(base, root)) else {
        return Vec::new();
    };
    let want = root.to_lowercase();
    let mut v: Vec<Entry> = rd
        .filter_map(Result::ok)
        .filter_map(|e| {
            let p = e.path();
            let taken = p.file_stem()?.to_str()?.parse().ok()?;
            (p.extension()? == "tsv").then_some(Entry { taken, path: p })
        })
        .filter(|e| stored_root(&e.path).is_some_and(|r| r.to_lowercase() == want))
        .collect();
    v.sort_by_key(|e| e.taken);
    v
}

pub fn load(entry: &Entry) -> io::Result<Snapshot> {
    Snapshot::read(io::BufReader::new(std::fs::File::open(&entry.path)?))
}

/// Stores `snap`, replacing the previous one if it is very recent, and drops the oldest beyond [`KEEP`].
pub fn save(base: &Path, snap: &Snapshot) -> io::Result<Entry> {
    let dir = series_dir(base, &snap.root);
    std::fs::create_dir_all(&dir)?;
    let existing = list(base, &snap.root);
    if let Some(last) = existing.last()
        && snap.taken.saturating_sub(last.taken) < REPLACE_WITHIN_SECS
    {
        let _ = std::fs::remove_file(&last.path);
    }
    let path = dir.join(format!("{}.tsv", snap.taken));
    let tmp = dir.join(format!("{}.tmp", snap.taken));
    let mut w = io::BufWriter::new(std::fs::File::create(&tmp)?);
    snap.write(&mut w)?;
    w.flush()?;
    drop(w);
    std::fs::rename(&tmp, &path)?;
    let all = list(base, &snap.root);
    for old in all.iter().take(all.len().saturating_sub(KEEP)) {
        let _ = std::fs::remove_file(&old.path);
    }
    Ok(Entry {
        taken: snap.taken,
        path,
    })
}

/// The snapshot to compare with by default: the newest one that is at least `age` seconds older than
/// `now`, or else the oldest. `entries` are oldest first and must not contain the current scan.
pub fn pick_baseline(entries: &[Entry], now: u64, age: u64) -> Option<&Entry> {
    entries
        .iter()
        .rev()
        .find(|e| now.saturating_sub(e.taken) >= age)
        .or(entries.first())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    /// Relative path with '/' separators.
    pub path: String,
    pub old: Option<u64>,
    pub new: Option<u64>,
    pub delta: i64,
}

/// Folders whose size changed by at least `min_delta` bytes, largest change first.
///
/// A folder whose change is almost entirely (80 %+) explained by one of its subfolders is left out in favor
/// of that subfolder, so that the list points at the culprit and not at every parent on the way to it.
/// A folder missing from a snapshot was smaller than [`MIN_DIR`] then (or did not exist).
pub fn diff(old: &Snapshot, new: &Snapshot, min_delta: u64) -> Vec<Change> {
    let old_map: HashMap<&str, u64> = old.dirs.iter().map(|(p, s)| (p.as_str(), *s)).collect();
    let new_map: HashMap<&str, u64> = new.dirs.iter().map(|(p, s)| (p.as_str(), *s)).collect();
    let mut changes: Vec<Change> = old_map
        .keys()
        .chain(new_map.keys())
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|p| !p.is_empty())
        .filter_map(|p| {
            let (o, n) = (old_map.get(p).copied(), new_map.get(p).copied());
            let delta = n.unwrap_or(0) as i64 - o.unwrap_or(0) as i64;
            (delta.unsigned_abs() >= min_delta).then(|| Change {
                path: p.to_owned(),
                old: o,
                new: n,
                delta,
            })
        })
        .collect();

    // strongest same-direction change among the direct children of each folder
    let mut child_max: HashMap<(&str, bool), i64> = HashMap::new();
    for c in &changes {
        let parent = c.path.rsplit_once('/').map_or("", |(p, _)| p);
        let e = child_max.entry((parent, c.delta > 0)).or_insert(0);
        *e = (*e).max(c.delta.abs());
    }
    let explained: Vec<bool> = changes
        .iter()
        .map(|c| {
            let m = child_max.get(&(c.path.as_str(), c.delta > 0)).copied().unwrap_or(0);
            m * 5 >= c.delta.abs() * 4
        })
        .collect();
    let mut i = 0;
    changes.retain(|_| {
        i += 1;
        !explained[i - 1]
    });
    changes.sort_by(|a, b| b.delta.abs().cmp(&a.delta.abs()).then_with(|| a.path.cmp(&b.path)));
    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Builder, NO_PARENT};

    const M: u64 = 1 << 20;

    fn snap(taken: u64, dirs: &[(&str, u64)]) -> Snapshot {
        let mut dirs: Vec<(String, u64)> = dirs.iter().map(|(p, s)| (p.to_string(), s * M)).collect();
        dirs.sort();
        Snapshot {
            root: "C:\\".into(),
            taken,
            total: 0,
            dirs,
        }
    }

    #[test]
    fn from_tree_keeps_big_dirs_and_roundtrips() {
        let mut b = Builder::default();
        let root = b.add(NO_PARENT, "C:\\", Kind::Dir, 0, 0);
        let a = b.add(root, "a", Kind::Dir, 0, 0);
        let sub = b.add(a, "sub dir", Kind::Dir, 0, 0);
        b.add(sub, "f", Kind::File, 5 * M, 5 * M);
        let tiny = b.add(root, "tiny", Kind::Dir, 0, 0);
        b.add(tiny, "f", Kind::File, 10, 10);
        let t = b.finish();
        let s = Snapshot::from_tree(&t, 42);
        assert_eq!(
            s.dirs,
            vec![
                ("".into(), 5 * M + 10),
                ("a".into(), 5 * M),
                ("a/sub dir".into(), 5 * M)
            ]
        );
        let mut buf = Vec::new();
        s.write(&mut buf).unwrap();
        assert_eq!(Snapshot::read(&buf[..]).unwrap(), s);
        assert!(Snapshot::read(&b"junk"[..]).is_err());
        let huge = format!("{MAGIC}\nroot\tC:\\\ntaken\t1\ntotal\t1\n{}\ta\n", 1u64 << 63);
        assert!(
            Snapshot::read(huge.as_bytes()).is_err(),
            "sizes beyond i64 would overflow diff"
        );
    }

    #[test]
    fn diff_points_at_the_culprit_folder() {
        let old = snap(1, &[("", 100), ("a", 60), ("a/tmp", 10), ("b", 40), ("gone", 50)]);
        let new = snap(2, &[("", 190), ("a", 130), ("a/tmp", 80), ("b", 41), ("fresh", 30)]);
        let d = diff(&old, &new, 5 * M);
        let got: Vec<(&str, i64)> = d.iter().map(|c| (c.path.as_str(), c.delta / M as i64)).collect();
        // a (+70) is explained by a/tmp (+70); b changed too little
        assert_eq!(got, vec![("a/tmp", 70), ("gone", -50), ("fresh", 30)]);
        assert_eq!(d[1].new, None);
        assert_eq!(d[2].old, None);
    }

    #[test]
    fn store_replaces_recent_prunes_old_and_picks_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let day = 86_400;
        let mut s = snap(1_000_000, &[("", 1)]);
        save(dir.path(), &s).unwrap();
        s.taken += 60; // within an hour: replaces
        save(dir.path(), &s).unwrap();
        assert_eq!(list(dir.path(), "C:\\").len(), 1);
        for i in 1..=(KEEP as u64 + 5) {
            s.taken = 1_000_000 + i * day;
            save(dir.path(), &s).unwrap();
        }
        let all = list(dir.path(), "C:\\");
        assert_eq!(all.len(), KEEP);
        assert_eq!(load(&all[0]).unwrap().taken, all[0].taken);
        let now = all.last().unwrap().taken + day;
        // a week back from `now`
        assert_eq!(pick_baseline(&all, now, 7 * day).unwrap().taken, now - 7 * day);
        assert_eq!(pick_baseline(&all, now, 1000 * day).unwrap().taken, all[0].taken);
        assert!(pick_baseline(&[], now, day).is_none());
        assert!(list(dir.path(), "D:\\").is_empty());
    }

    /// `E:\a b` and `E:\a_b` share a series folder name; their histories must not mix or delete each other.
    #[test]
    fn roots_with_the_same_series_folder_keep_separate_histories() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = snap(1_000_000, &[("", 1)]);
        a.root = r"E:\a b".into();
        let mut b = snap(1_000_060, &[("", 2)]);
        b.root = r"E:\a_b".into();
        save(dir.path(), &a).unwrap();
        save(dir.path(), &b).unwrap();
        let la = list(dir.path(), r"E:\a b");
        let lb = list(dir.path(), r"E:\a_b");
        assert_eq!(la.len(), 1, "saving E:\\a_b must not replace the snapshot of E:\\a b");
        assert_eq!(lb.len(), 1);
        assert_eq!(load(&la[0]).unwrap().root, r"E:\a b");
        assert_eq!(load(&lb[0]).unwrap().root, r"E:\a_b");
        assert_eq!(list(dir.path(), r"e:\A B").len(), 1, "same folder, other letter case");
    }
}
