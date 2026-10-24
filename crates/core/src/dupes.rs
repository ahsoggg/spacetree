//! Duplicate files by content: same size, then same hash of head+middle+tail samples, then same hash of the whole file.
//!
//! Every stage only keeps the files that still have a possible twin, so most files are never read at all
//! and the rest are read only as far as needed to tell them apart.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering::Relaxed};

use rayon::prelude::*;

use crate::tree::{NodeId, Tree};

/// Bytes hashed at each of the three sample points in the cheap middle stage.
const SAMPLE_LEN: usize = 16 << 10;
const BUF_LEN: usize = 1 << 20;
/// Parallel readers: enough to keep an SSD busy, few enough not to make a spinning disk seek itself to death.
const READERS: usize = 4;

pub const STAGE_SIZES: u8 = 0;
pub const STAGE_PARTIAL: u8 = 1;
pub const STAGE_FULL: u8 = 2;

/// Files with identical content.
#[derive(Clone, Debug)]
pub struct DupSet {
    pub size: u64,
    /// Newest first (by modification time): `items[0]` is the copy to keep, the rest are redundant.
    pub items: Vec<NodeId>,
}

impl DupSet {
    pub fn wasted(&self) -> u64 {
        self.size * (self.items.len() as u64 - 1)
    }
}

/// Shared between the searching thread and the observer.
#[derive(Default)]
pub struct DupProgress {
    cancel: AtomicBool,
    pub stage: AtomicU8,
    /// Files to look at / looked at in the current stage.
    pub total: AtomicU64,
    pub done: AtomicU64,
    /// Bytes read so far.
    pub bytes: AtomicU64,
}

impl DupProgress {
    pub fn cancel(&self) {
        self.cancel.store(true, Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Relaxed)
    }

    fn start(&self, stage: u8, total: usize) {
        self.stage.store(stage, Relaxed);
        self.total.store(total as u64, Relaxed);
        self.done.store(0, Relaxed);
    }
}

/// What part of a file is hashed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    /// Head, middle and tail: containers (video, ISO, PDF) often share an identical header.
    Sampled,
    Whole,
}

/// OneDrive/iCloud-style placeholders: reading them would download the file.
#[cfg(windows)]
fn is_cloud_only(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const OFFLINE: u32 = 0x1000;
    const RECALL_ON_OPEN: u32 = 0x4_0000;
    const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
    meta.file_attributes() & (OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS) != 0
}

#[cfg(not(windows))]
fn is_cloud_only(_: &std::fs::Metadata) -> bool {
    false
}

/// Feeds `len` bytes starting at `offset` into `hasher`; `None` on a read error or cancellation.
fn feed(f: &mut File, offset: u64, len: u64, hasher: &mut blake3::Hasher, progress: &DupProgress) -> Option<()> {
    f.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = vec![0u8; (len.min(BUF_LEN as u64)) as usize];
    let mut left = len;
    while left > 0 {
        if progress.is_cancelled() {
            return None;
        }
        let want = left.min(buf.len() as u64) as usize;
        let n = f.read(&mut buf[..want]).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        progress.bytes.fetch_add(n as u64, Relaxed);
        left -= n as u64;
    }
    Some(())
}

/// Hashes the requested part of a file; `None` if the file cannot be read, is not `size` bytes long any more,
/// is a cloud placeholder, or the search was cancelled.
fn hash_file(path: &Path, size: u64, part: Part, progress: &DupProgress) -> Option<blake3::Hash> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() != size || is_cloud_only(&meta) {
        return None;
    }
    let mut f = File::open(path).ok()?;
    let mut hasher = blake3::Hasher::new();
    let sample = SAMPLE_LEN as u64;
    if part == Part::Whole || size <= 3 * sample {
        feed(&mut f, 0, size, &mut hasher, progress)?;
    } else {
        for offset in [0, size / 2 - sample / 2, size - sample] {
            feed(&mut f, offset, sample, &mut hasher, progress)?;
        }
    }
    Some(hasher.finalize())
}

/// Byte-by-byte comparison; stops at the first difference.
pub fn same_content(a: &Path, b: &Path) -> io::Result<bool> {
    same_handles(&mut File::open(a)?, &mut File::open(b)?)
}

fn same_handles(fa: &mut File, fb: &mut File) -> io::Result<bool> {
    if fa.metadata()?.len() != fb.metadata()?.len() {
        return Ok(false);
    }
    let (mut ba, mut bb) = (vec![0u8; BUF_LEN], vec![0u8; BUF_LEN]);
    loop {
        let n = fa.read(&mut ba)?;
        if n == 0 {
            return Ok(true);
        }
        fb.read_exact(&mut bb[..n])?;
        if ba[..n] != bb[..n] {
            return Ok(false);
        }
    }
}

/// Extensions of files that are written to in place. Two names of one hard-linked file are one file: a program
/// that saves its database, log or settings through its own name changes the other program's copy too.
/// Only formats that nobody edits in place are linked (see [`hardlink_allowed`]).
const IMMUTABLE_EXT: &[&str] = &[
    // code and packages
    "dll", "exe", "sys", "so", "dylib", "lib", "pdb", "obj", "rlib", "jar", "whl", "nupkg", "msi", "cab", "mui",
    // game and application assets
    "pak", "ucas", "utoc", "bsa", "ba2", "vpk", "wad", "pck", "assets", "bundle", "resource", "ress",
    // media
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "svg", "ttf", "otf", "woff", "woff2", "mp3", "flac", "ogg",
    "wav", "mp4", "mkv", "avi", "mov", "webm", "pdf", "iso", // archives
    "zip", "7z", "rar", "gz", "xz", "bz2", "zst", "tar", "tgz",
];

/// Whether `path` may be turned into a hard link: its extension is on the immutable list.
pub fn hardlink_allowed(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMMUTABLE_EXT.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// Opens `path` for reading so that nobody can write to or delete it while the handle lives.
fn open_locked(path: &Path) -> io::Result<File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 1;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(path)
    }
    #[cfg(not(windows))]
    {
        File::open(path)
    }
}

fn stamp(f: &File) -> io::Result<(u64, Option<std::time::SystemTime>)> {
    let m = f.metadata()?;
    Ok((m.len(), m.modified().ok()))
}

/// Replaces `dup` by a hard link to `keep`: both names stay, one copy of the data remains on disk.
/// Refused for formats that programs rewrite in place (databases, logs, settings), for a file somebody has
/// open for writing, and for content that changed since the search. Both files are held open without write
/// sharing while they are compared; the link is created beside `dup` under a temporary name, `dup` is checked
/// once more (unchanged size and time) and the link is renamed over it, so `dup` is never missing. Same NTFS
/// volume only.
pub fn replace_with_hardlink(keep: &Path, dup: &Path) -> io::Result<()> {
    if !hardlink_allowed(keep) || !hardlink_allowed(dup) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "этот тип файла программы изменяют на месте — общая копия испортила бы чужие данные",
        ));
    }
    let seen = {
        let (mut fk, mut fd) = (open_locked(keep)?, open_locked(dup)?);
        let before = stamp(&fd)?;
        if !same_handles(&mut fk, &mut fd)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "содержимое файлов изменилось",
            ));
        }
        before
    };
    let mut tmp = dup.as_os_str().to_owned();
    tmp.push(".spacetree-link");
    let tmp = PathBuf::from(tmp);
    std::fs::hard_link(keep, &tmp)?;
    // The handles are closed here (a rename over an open file fails); a writer that sneaked in changed the
    // size or the modification time.
    let unchanged = open_locked(dup).and_then(|f| stamp(&f)).is_ok_and(|now| now == seen);
    if !unchanged {
        let _ = std::fs::remove_file(&tmp);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "файл изменился во время замены",
        ));
    }
    std::fs::rename(&tmp, dup).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Keeps the groups of `keyed` (sorted by key) that have at least two members.
fn twins<K: PartialEq + Ord + Copy>(mut keyed: Vec<(K, NodeId)>) -> Vec<(K, Vec<NodeId>)> {
    keyed.sort_unstable();
    keyed
        .chunk_by(|a, b| a.0 == b.0)
        .filter(|g| g.len() > 1)
        .map(|g| (g[0].0, g.iter().map(|x| x.1).collect()))
        .collect()
}

/// Narrows `groups` down by hashing `part` of each file; returns groups that are still ambiguous.
fn refine(
    tree: &Tree,
    groups: Vec<(u64, Vec<NodeId>)>,
    part: Part,
    stage: u8,
    progress: &DupProgress,
    pool: &rayon::ThreadPool,
) -> Vec<(u64, Vec<NodeId>)> {
    progress.start(stage, groups.iter().map(|g| g.1.len()).sum());
    let hashed: Vec<(u64, [u8; 32], NodeId)> = pool.install(|| {
        groups
            .par_iter()
            .flat_map_iter(|(size, ids)| ids.iter().map(move |&id| (*size, id)))
            .filter_map(|(size, id)| {
                let h = hash_file(&crate::fs_path(&tree.path(id)), size, part, progress);
                progress.done.fetch_add(1, Relaxed);
                h.map(|h| (size, *h.as_bytes(), id))
            })
            .collect()
    });
    if progress.is_cancelled() {
        return Vec::new();
    }
    twins(hashed.into_iter().map(|(s, h, id)| ((s, h), id)).collect())
        .into_iter()
        .map(|((size, _), ids)| (size, ids))
        .collect()
}

/// Finds sets of files with identical content among `candidates` (regular files of `tree`).
/// Sets are ordered by wasted space, largest first. `None` when cancelled.
pub fn find(tree: &Tree, candidates: &[NodeId], progress: &DupProgress) -> Option<Vec<DupSet>> {
    progress.start(STAGE_SIZES, candidates.len());
    let by_size = twins(candidates.iter().map(|&id| (tree.node(id).size, id)).collect());
    progress.done.store(candidates.len() as u64, Relaxed);

    let pool = rayon::ThreadPoolBuilder::new().num_threads(READERS).build().ok()?;
    let partial = refine(tree, by_size, Part::Sampled, STAGE_PARTIAL, progress, &pool);
    let full = refine(tree, partial, Part::Whole, STAGE_FULL, progress, &pool);
    if progress.is_cancelled() {
        return None;
    }

    let mut sets: Vec<DupSet> = full
        .into_iter()
        .map(|(size, mut items)| {
            items.sort_by_key(|&i| (std::cmp::Reverse(tree.node(i).mtime), i));
            DupSet { size, items }
        })
        .collect();
    sets.sort_by_key(|s| std::cmp::Reverse(s.wasted()));
    Some(sets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Builder, Kind, NO_PARENT};

    fn write(dir: &std::path::Path, name: &str, data: &[u8]) {
        std::fs::write(dir.join(name), data).unwrap();
    }

    #[test]
    fn finds_only_files_with_identical_content() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let len = 200 << 10;
        let base = vec![7u8; len];
        let mut tail_differs = base.clone();
        *tail_differs.last_mut().unwrap() = 8; // same first 64 KiB: only the full hash tells them apart
        let mut head_differs = base.clone();
        head_differs[0] = 9;
        write(root, "a.bin", &base);
        write(root, "b.bin", &base);
        write(root, "c.bin", &tail_differs);
        write(root, "d.bin", &head_differs);
        write(root, "e.bin", &base[..len - 1]);
        write(root, "f.bin", &base);

        let mut b = Builder::default();
        let r = b.add(NO_PARENT, root.to_str().unwrap(), Kind::Dir, 0, 0);
        let ids: Vec<NodeId> = ["a.bin", "b.bin", "c.bin", "d.bin", "e.bin", "f.bin"]
            .iter()
            .map(|n| b.add(r, n, Kind::File, std::fs::metadata(root.join(n)).unwrap().len(), 0))
            .collect();
        // a.bin is the oldest, f.bin the newest
        for (i, &id) in ids.iter().enumerate() {
            b.nodes[id as usize].mtime = 100 + i as u32;
        }
        let tree = b.finish();

        let progress = DupProgress::default();
        let sets = find(&tree, &ids, &progress).unwrap();
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].items, vec![ids[5], ids[1], ids[0]], "newest first");
        assert_eq!(sets[0].wasted(), 2 * len as u64);
    }

    #[test]
    fn hardlink_replaces_copy_only_when_content_matches() {
        let dir = tempfile::tempdir().unwrap();
        let (keep, dup, other) = (
            dir.path().join("k.pak"),
            dir.path().join("d.pak"),
            dir.path().join("o.pak"),
        );
        std::fs::write(&keep, vec![1u8; 3 << 20]).unwrap();
        std::fs::write(&dup, vec![1u8; 3 << 20]).unwrap();
        std::fs::write(&other, vec![2u8; 3 << 20]).unwrap();

        assert!(replace_with_hardlink(&keep, &other).is_err());
        assert_eq!(std::fs::read(&other).unwrap()[0], 2, "mismatching file untouched");

        replace_with_hardlink(&keep, &dup).unwrap();
        std::fs::write(&keep, b"changed").unwrap();
        assert_eq!(
            std::fs::read(&dup).unwrap(),
            b"changed",
            "dup now shares the data of keep"
        );
        assert!(!dir.path().join("d.pak.spacetree-link").exists());
    }

    #[test]
    fn mutable_formats_are_never_linked() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "a.db", "a.sqlite", "a.log", "a.ini", "a.json", "a.cfg", "a.sav", "noext",
        ] {
            let (keep, dup) = (dir.path().join(name), dir.path().join(format!("copy-{name}")));
            std::fs::write(&keep, b"same").unwrap();
            std::fs::write(&dup, b"same").unwrap();
            assert!(!hardlink_allowed(&keep), "{name}");
            assert!(replace_with_hardlink(&keep, &dup).is_err(), "{name}");
            std::fs::write(&keep, b"changed").unwrap();
            assert_eq!(std::fs::read(&dup).unwrap(), b"same", "{name} must stay independent");
        }
        assert!(hardlink_allowed(Path::new("x.DLL")));
    }

    /// A file somebody has open for writing is not touched.
    #[cfg(windows)]
    #[test]
    fn a_file_open_for_writing_is_not_linked() {
        let dir = tempfile::tempdir().unwrap();
        let (keep, dup) = (dir.path().join("k.zip"), dir.path().join("d.zip"));
        std::fs::write(&keep, b"same").unwrap();
        std::fs::write(&dup, b"same").unwrap();
        let _writer = std::fs::OpenOptions::new().write(true).open(&dup).unwrap();
        assert!(replace_with_hardlink(&keep, &dup).is_err());
        assert!(!dir.path().join("d.zip.spacetree-link").exists());
    }

    /// `a.` must be hashed as itself: through a Win32 path it would read its neighbour `a`, and a unique file
    /// would be offered for deletion as a "duplicate".
    #[cfg(windows)]
    #[test]
    fn trailing_dot_name_is_read_as_itself() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        let verbatim = |n: &str| PathBuf::from(format!(r"\\?\{root}\{n}"));
        std::fs::write(verbatim("a"), b"same same same").unwrap();
        std::fs::write(verbatim("a."), b"unique content").unwrap();
        std::fs::write(verbatim("b"), b"same same same").unwrap();
        let mut b = Builder::default();
        let r = b.add(NO_PARENT, root, Kind::Dir, 0, 0);
        let ids: Vec<NodeId> = ["a.", "b"].iter().map(|n| b.add(r, n, Kind::File, 14, 0)).collect();
        let tree = b.finish();
        let sets = find(&tree, &ids, &DupProgress::default()).unwrap();
        assert!(sets.is_empty(), "a. and b differ");
        let _ = std::fs::remove_file(verbatim("a."));
    }

    #[test]
    fn unreadable_and_cancelled() {
        let mut b = Builder::default();
        let r = b.add(NO_PARENT, "/nonexistent-spacetree-test", Kind::Dir, 0, 0);
        let x = b.add(r, "x", Kind::File, 5, 0);
        let y = b.add(r, "y", Kind::File, 5, 0);
        let tree = b.finish();
        let p = DupProgress::default();
        assert!(find(&tree, &[x, y], &p).unwrap().is_empty());
        p.cancel();
        assert!(find(&tree, &[x, y], &p).is_none());
    }
}
