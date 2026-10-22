//! Parallel directory walker (rayon). Never follows symlinks, junctions or mount points.
//!
//! Windows: directories are listed with `GetFileInformationByHandleEx(FileFullDirectoryInfo)`, which returns
//! names, sizes, real allocation sizes and timestamps of up to 64 KiB of entries per syscall; paths are kept as
//! UTF-16 with a `\\?\` prefix, so there is no MAX_PATH limit and no per-file conversion.
//! Unix: `std::fs::read_dir` + `lstat`; size on disk is `st_blocks * 512`, hard links are counted once.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::time::Instant;

use rayon::prelude::*;

use crate::error::ScanError;
use crate::progress::{PHASE_BUILDING, Progress};
use crate::scan::Timings;
use crate::tree::{Kind, NO_PARENT, Node, Tree};

const MAX_ERROR_SAMPLES: usize = 200;

/// Per-thread output. Each listed directory appends one block of nodes whose ids are consecutive.
#[derive(Default)]
struct Sink {
    nodes: Vec<Node>,
    /// `(first id, index of the block's first node in nodes)`.
    blocks: Vec<(u32, u32)>,
    names: String,
}

struct Ctx<'a> {
    progress: &'a Progress,
    next_id: AtomicU32,
    sinks: Vec<Mutex<Sink>>,
    errors: Mutex<Vec<String>>,
    #[cfg(unix)]
    seen_links: Mutex<std::collections::HashSet<(u64, u64)>>,
}

impl Ctx<'_> {
    fn error(&self, path: &str, e: &std::io::Error) {
        self.progress.errors.fetch_add(1, Relaxed);
        let mut v = self.errors.lock().unwrap();
        if v.len() < MAX_ERROR_SAMPLES {
            v.push(format!("{path}: {e}"));
        }
    }
}

/// Entries of one directory; names are packed into `names`.
#[derive(Default)]
struct Listing {
    names: String,
    entries: Vec<Entry>,
}

struct Entry {
    name_off: u32,
    name_len: u16,
    kind: Kind,
    size: u64,
    alloc: u64,
    mtime: u32,
}

impl Listing {
    fn push(&mut self, name: &str, kind: Kind, size: u64, alloc: u64, mtime: u32) {
        let name_off = self.names.len() as u32;
        self.names.push_str(name);
        self.entries.push(Entry {
            name_off,
            name_len: name.len() as u16,
            kind,
            size,
            alloc,
            mtime,
        });
    }

    fn name(&self, e: &Entry) -> &str {
        &self.names[e.name_off as usize..e.name_off as usize + e.name_len as usize]
    }
}

struct Block {
    first: u32,
    sink: u32,
    start: u32,
    name_base: u32,
}

pub struct WalkOutput {
    pub tree: Tree,
    pub errors: u64,
    pub error_samples: Vec<String>,
    pub timings: Timings,
}

/// Worker threads for the walk: a spinning disk gets few (parallel listings just make the head seek back
/// and forth), everything else — including drives whose type is unknown — gets the full set.
fn thread_count(root: &Path) -> usize {
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let full = (cores * 2).clamp(4, 32);
    #[cfg(windows)]
    if crate::winapi::incurs_seek_penalty(root) == Some(true) {
        return 2;
    }
    let _ = root;
    full
}

pub fn scan(root: &Path, progress: &Progress) -> Result<WalkOutput, ScanError> {
    let root = std::path::absolute(root)?;
    let meta = std::fs::metadata(&root).map_err(|e| ScanError::Io(format!("{}: {e}", root.display())))?;
    if !meta.is_dir() {
        return Err(ScanError::Io(format!("{}: не папка", root.display())));
    }

    let t_start = Instant::now();
    let threads = thread_count(&root);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .map_err(|e| ScanError::Io(e.to_string()))?;

    let ctx = Ctx {
        progress,
        next_id: AtomicU32::new(1),
        sinks: (0..=threads).map(|_| Mutex::new(Sink::default())).collect(),
        errors: Mutex::new(Vec::new()),
        #[cfg(unix)]
        seen_links: Mutex::new(Default::default()),
    };

    let start = plat::root_path(&root);
    pool.install(|| {
        rayon::scope(|s| scan_dir(&ctx, s, start, 0));
    });
    if progress.is_cancelled() {
        return Err(ScanError::Cancelled);
    }

    let t_walk = t_start.elapsed();
    progress.phase.store(PHASE_BUILDING, Relaxed);
    let t_build = Instant::now();
    let total = ctx.next_id.load(Relaxed) as usize;
    let root_name = root.to_string_lossy().into_owned();
    let sinks: Vec<Sink> = ctx.sinks.into_iter().map(|s| s.into_inner().unwrap()).collect();
    let mut names = String::with_capacity(root_name.len() + sinks.iter().map(|s| s.names.len()).sum::<usize>());
    names.push_str(&root_name);
    // Blocks tile the ids 1..total; sorted by first id, the block holding an id is found by binary search.
    let mut blocks: Vec<Block> = Vec::with_capacity(sinks.iter().map(|s| s.blocks.len()).sum());
    for (si, sink) in sinks.iter().enumerate() {
        let name_base = names.len() as u32;
        names.push_str(&sink.names);
        blocks.extend(sink.blocks.iter().map(|&(first, start)| Block {
            first,
            sink: si as u32,
            start,
            name_base,
        }));
    }
    blocks.par_sort_unstable_by_key(|b| b.first);
    let root_node = Node::new(NO_PARENT, 0, root_name.len() as u16, Kind::Dir, 0, 0);
    let nodes: Vec<Node> = (0..total as u32)
        .into_par_iter()
        .with_min_len(4096)
        .map(|id| {
            if id == 0 {
                return root_node;
            }
            let b = &blocks[blocks.partition_point(|b| b.first <= id) - 1];
            let mut n = sinks[b.sink as usize].nodes[(b.start + (id - b.first)) as usize];
            n.name_off += b.name_base;
            n
        })
        .collect();
    let errors = progress.errors.load(Relaxed);
    let error_samples = ctx.errors.into_inner().unwrap();
    Ok(WalkOutput {
        tree: Tree::from_parts(nodes, names),
        errors,
        error_samples,
        timings: vec![("обход папок", t_walk), ("построение дерева", t_build.elapsed())],
    })
}

fn scan_dir<'s>(ctx: &'s Ctx<'s>, scope: &rayon::Scope<'s>, path: plat::DirPath, id: u32) {
    if ctx.progress.is_cancelled() {
        return;
    }
    let mut list = Listing::default();
    if let Err(e) = plat::list(ctx, &path, &mut list) {
        ctx.error(&plat::display(&path), &e);
        return;
    }
    if list.entries.is_empty() {
        return;
    }

    let first = ctx.next_id.fetch_add(list.entries.len() as u32, Relaxed);
    let (mut files, mut bytes) = (0u64, 0u64);
    {
        let idx = rayon::current_thread_index().map_or(0, |i| i + 1);
        let mut sink = ctx.sinks[idx.min(ctx.sinks.len() - 1)].lock().unwrap();
        let base = sink.names.len() as u32;
        sink.names.push_str(&list.names);
        let start = sink.nodes.len() as u32;
        sink.blocks.push((first, start));
        for e in &list.entries {
            let mut node = Node::new(id, base + e.name_off, e.name_len, e.kind, e.size, e.alloc);
            node.mtime = e.mtime;
            sink.nodes.push(node);
            if e.kind == Kind::File {
                files += 1;
                bytes += e.size;
            }
        }
    }
    ctx.progress.files.fetch_add(files, Relaxed);
    ctx.progress.bytes.fetch_add(bytes, Relaxed);

    for (i, e) in list.entries.iter().enumerate() {
        if e.kind == Kind::Dir {
            let child = plat::join(&path, list.name(e));
            let child_id = first + i as u32;
            scope.spawn(move |s| scan_dir(ctx, s, child, child_id));
        }
    }
}

/// FILETIME (100 ns since 1601-01-01) -> Unix seconds, clamped to `u32`.
#[cfg_attr(not(windows), allow(dead_code))]
fn filetime_to_unix(ft: i64) -> u32 {
    (ft / 10_000_000 - 11_644_473_600).clamp(0, u32::MAX as i64) as u32
}

#[cfg(windows)]
mod plat {
    use std::path::Path;

    use super::{Ctx, Listing, filetime_to_unix};
    use crate::tree::Kind;
    use crate::winapi;

    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

    /// UTF-16 path with a `\\?\` prefix, no terminator.
    pub type DirPath = Vec<u16>;

    pub fn root_path(p: &Path) -> DirPath {
        let s = p.to_string_lossy();
        let full = if s.starts_with(r"\\?\") {
            s.into_owned()
        } else if let Some(unc) = s.strip_prefix(r"\\") {
            format!(r"\\?\UNC\{unc}")
        } else {
            format!(r"\\?\{s}")
        };
        full.encode_utf16().collect()
    }

    pub fn join(parent: &DirPath, name: &str) -> DirPath {
        let mut p = Vec::with_capacity(parent.len() + name.len() + 2);
        p.extend_from_slice(parent);
        if p.last() != Some(&u16::from(b'\\')) {
            p.push(u16::from(b'\\'));
        }
        p.extend(name.encode_utf16());
        p
    }

    pub fn display(p: &DirPath) -> String {
        let s = String::from_utf16_lossy(p);
        match s.strip_prefix(r"\\?\UNC\") {
            Some(unc) => format!(r"\\{unc}"),
            None => s.strip_prefix(r"\\?\").map(str::to_owned).unwrap_or(s),
        }
    }

    /// Cloud-file placeholders (OneDrive etc.) are reparse points too, but they are real folders.
    fn is_cloud_tag(tag: u32) -> bool {
        tag & 0xFFFF_0FFF == 0x9000_001A
    }

    pub fn list(_: &Ctx, path: &DirPath, out: &mut Listing) -> std::io::Result<()> {
        let mut z = Vec::with_capacity(path.len() + 1);
        z.extend_from_slice(path);
        z.push(0);
        let mut name = String::new();
        winapi::list_dir(&z, |e| {
            name.clear();
            name.extend(char::decode_utf16(e.name.iter().copied()).map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER)));
            let reparse = e.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
            let mtime = filetime_to_unix(e.mtime);
            if e.attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                let kind = if reparse && !is_cloud_tag(e.reparse_tag) {
                    Kind::Link
                } else {
                    Kind::Dir
                };
                out.push(&name, kind, 0, 0, mtime);
            } else if reparse && e.reparse_tag == IO_REPARSE_TAG_SYMLINK {
                out.push(&name, Kind::Link, 0, 0, mtime);
            } else {
                out.push(&name, Kind::File, e.size, e.alloc, mtime);
            }
        })
    }
}

#[cfg(not(windows))]
mod plat {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{Ctx, Listing};
    use crate::tree::Kind;

    pub type DirPath = PathBuf;

    pub fn root_path(p: &Path) -> DirPath {
        p.to_path_buf()
    }

    pub fn join(parent: &DirPath, name: &str) -> DirPath {
        parent.join(name)
    }

    pub fn display(p: &DirPath) -> String {
        p.display().to_string()
    }

    pub fn list(ctx: &Ctx, path: &DirPath, out: &mut Listing) -> std::io::Result<()> {
        for ent in fs::read_dir(path)? {
            let ent = match ent {
                Ok(e) => e,
                Err(e) => {
                    ctx.error(&display(path), &e);
                    continue;
                }
            };
            let name = ent.file_name().to_string_lossy().into_owned();
            let meta = match ent.metadata() {
                Ok(m) => m,
                Err(e) => {
                    ctx.error(&display(&ent.path()), &e);
                    continue;
                }
            };
            let mtime = mtime_of(&meta);
            let ft = meta.file_type();
            if ft.is_symlink() {
                out.push(&name, Kind::Link, 0, 0, mtime);
            } else if ft.is_dir() {
                out.push(&name, Kind::Dir, 0, 0, mtime);
            } else if let Some((size, alloc)) = file_sizes(ctx, &meta) {
                out.push(&name, Kind::File, size, alloc, mtime);
            }
        }
        Ok(())
    }

    fn mtime_of(meta: &fs::Metadata) -> u32 {
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs().min(u32::MAX as u64) as u32)
    }

    /// Returns `None` when the file is a hard link already counted elsewhere.
    #[cfg(unix)]
    fn file_sizes(ctx: &Ctx, meta: &fs::Metadata) -> Option<(u64, u64)> {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() > 1 && !ctx.seen_links.lock().unwrap().insert((meta.dev(), meta.ino())) {
            return None;
        }
        Some((meta.len(), meta.blocks() * 512))
    }

    #[cfg(not(unix))]
    fn file_sizes(_: &Ctx, meta: &fs::Metadata) -> Option<(u64, u64)> {
        Some((meta.len(), meta.len()))
    }
}
