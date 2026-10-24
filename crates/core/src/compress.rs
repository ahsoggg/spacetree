//! NTFS system compression ("Windows Compact", LZX) of a folder or a file, and its removal.
//!
//! This is the mechanism behind `compact /C /EXE:LZX`: the data stays in the file, which keeps its name,
//! attributes and contents; Windows decompresses transparently when a program reads it. Saves roughly 30–50 %
//! on binaries and game assets, costs some CPU on reads and makes writes slower, so it suits files that are
//! read often and rarely changed.
//!
//! The walk never enters or touches links (junctions, symlinks, mount points, cloud placeholders): the
//! system tool `compact /S` follows junctions, which could compress files elsewhere on the disk.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

use rayon::prelude::*;

use crate::winapi;

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
/// Smaller files save nothing (cluster rounding) and cost a call each.
pub const MIN_SIZE: u64 = 8 * 1024;
/// Already-compressed formats: LZX cannot shrink them, so compressing is wasted time.
const INCOMPRESSIBLE: &[&str] = &[
    "zip", "7z", "rar", "gz", "xz", "bz2", "zst", "br", "lz4", "cab", "jar", "apk", "msi", "jpg", "jpeg", "png", "gif",
    "webp", "heic", "avif", "mp3", "aac", "ogg", "opus", "flac", "m4a", "mp4", "mkv", "avi", "mov", "webm", "m4v",
    "wmv", "docx", "xlsx", "pptx", "odt", "pdf",
];
const ERROR_NOT_SUPPORTED: i32 = 50;
const ERROR_INVALID_FUNCTION: i32 = 1;
const ERROR_COMPRESSION_NOT_BENEFICIAL: i32 = 344;
const ERROR_OBJECT_NOT_EXTERNALLY_BACKED: i32 = 342;
const ERROR_NOT_A_REPARSE_POINT: i32 = 4390;
const ERROR_FILE_NOT_EXTERNALLY_BACKED: i32 = 4393;
const ERROR_REPARSE_TAG_MISMATCH: i32 = 4394;
const ERROR_CLOUD_FILE_PROVIDER_NOT_RUNNING: i32 = 362;

#[derive(Default)]
pub struct CompressProgress {
    /// Files to process (known once the walk is done).
    pub total: AtomicU64,
    pub done: AtomicU64,
    cancelled: AtomicBool,
    pub current: Mutex<String>,
}

impl CompressProgress {
    pub fn cancel(&self) {
        self.cancelled.store(true, Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Relaxed)
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// Files looked at.
    pub files: u64,
    /// Files that were compressed (or, when undoing, restored).
    pub changed: u64,
    /// Files left alone on purpose or because they cannot be compressed (small, already packed, in use by others...).
    pub skipped: u64,
    /// Files that failed for another reason; see `first_error`.
    pub failed: u64,
    /// Size on disk of the changed files before / after.
    pub before: u64,
    pub after: u64,
    pub first_error: Option<String>,
    pub cancelled: bool,
}

impl Report {
    /// Disk space gained (negative when undoing).
    pub fn saved(&self) -> i64 {
        self.before as i64 - self.after as i64
    }
}

fn is_link(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Regular files under `root` (or `root` itself) with their sizes, without following or listing links.
fn collect(root: &Path, progress: &CompressProgress) -> io::Result<Vec<(PathBuf, u64)>> {
    let meta = std::fs::symlink_metadata(root)?;
    if is_link(&meta) {
        return Err(io::Error::other(
            "это ссылка (junction или symlink): сжимать нужно сами папки",
        ));
    }
    if !meta.is_dir() {
        return Ok(vec![(root.to_owned(), meta.len())]);
    }
    let mut files = Vec::new();
    let mut stack = vec![root.to_owned()];
    while let Some(dir) = stack.pop() {
        if progress.is_cancelled() {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let Ok(m) = e.metadata() else { continue }; // does not follow links
            if is_link(&m) {
                continue;
            }
            if m.is_dir() {
                stack.push(e.path());
            } else if m.is_file() {
                files.push((e.path(), m.len()));
            }
        }
    }
    Ok(files)
}

fn skipped_by_error(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(
            ERROR_NOT_SUPPORTED
                | ERROR_INVALID_FUNCTION
                | ERROR_COMPRESSION_NOT_BENEFICIAL
                | ERROR_OBJECT_NOT_EXTERNALLY_BACKED
                | ERROR_NOT_A_REPARSE_POINT
                | ERROR_FILE_NOT_EXTERNALLY_BACKED
                | ERROR_REPARSE_TAG_MISMATCH
                | ERROR_CLOUD_FILE_PROVIDER_NOT_RUNNING
        )
    )
}

fn worth_compressing(path: &Path, size: u64) -> bool {
    size >= MIN_SIZE
        && !path
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| INCOMPRESSIBLE.iter().any(|i| x.eq_ignore_ascii_case(i)))
}

/// Compresses (`undo == false`) or decompresses everything under `root` with LZX. Files are processed on all
/// cores; `progress` can be watched and cancelled from another thread (already processed files stay processed).
pub fn run(root: &Path, undo: bool, progress: &CompressProgress) -> io::Result<Report> {
    let files = collect(root, progress)?;
    progress.total.store(files.len() as u64, Relaxed);
    let first_error: Mutex<Option<String>> = Mutex::new(None);
    let (changed, skipped, failed, before, after) = (
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
    );
    files.par_iter().for_each(|(path, size)| {
        if progress.is_cancelled() {
            return;
        }
        progress.done.fetch_add(1, Relaxed);
        if let Ok(mut c) = progress.current.try_lock() {
            *c = path.to_string_lossy().into_owned();
        }
        if !undo && !worth_compressing(path, *size) {
            skipped.fetch_add(1, Relaxed);
            return;
        }
        let was = winapi::size_on_disk(path).unwrap_or(*size);
        match winapi::set_lzx_compression(path, !undo) {
            Ok(()) => {
                changed.fetch_add(1, Relaxed);
                before.fetch_add(was, Relaxed);
                after.fetch_add(winapi::size_on_disk(path).unwrap_or(was), Relaxed);
            }
            Err(e) if skipped_by_error(&e) => {
                skipped.fetch_add(1, Relaxed);
            }
            Err(e) => {
                failed.fetch_add(1, Relaxed);
                first_error
                    .lock()
                    .unwrap()
                    .get_or_insert_with(|| format!("{}: {e}", path.display()));
            }
        }
    });
    Ok(Report {
        files: files.len() as u64,
        changed: changed.into_inner(),
        skipped: skipped.into_inner(),
        failed: failed.into_inner(),
        before: before.into_inner(),
        after: after.into_inner(),
        first_error: first_error.into_inner().unwrap(),
        cancelled: progress.is_cancelled(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("spacetree-compress-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn compresses_and_restores_a_compressible_file_in_place() {
        let dir = tmp("roundtrip");
        let big = dir.join("zeros.bin");
        let data = vec![0u8; 3 * 1024 * 1024];
        std::fs::write(&big, &data).unwrap();
        std::fs::write(dir.join("tiny.txt"), b"hello").unwrap();
        std::fs::write(dir.join("packed.zip"), vec![0u8; 100_000]).unwrap();

        let r = run(&dir, false, &CompressProgress::default()).unwrap();
        if r.changed == 0 {
            // not an NTFS volume (or no WOF): nothing can be compressed here, and nothing must have broken
            assert_eq!(r.failed, 0, "{r:?}");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        assert_eq!((r.files, r.changed, r.skipped, r.failed), (3, 1, 2, 0), "{r:?}");
        assert!(r.saved() > 2_000_000, "3 MB of zeros must shrink a lot: {r:?}");
        assert_eq!(std::fs::read(&big).unwrap(), data, "contents are unchanged");
        assert!(winapi::size_on_disk(&big).unwrap() < 100_000);

        let back = run(&dir, true, &CompressProgress::default()).unwrap();
        assert_eq!(back.changed, 1, "{back:?}");
        assert!(winapi::size_on_disk(&big).unwrap() >= 3 * 1024 * 1024);
        assert_eq!(std::fs::read(&big).unwrap(), data);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The reason this module does not shell out to `compact /S`: that follows junctions.
    #[test]
    fn junctions_are_not_followed() {
        let base = tmp("junction");
        let (inside, outside) = (base.join("inside"), base.join("outside"));
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("zeros.bin"), vec![0u8; 3 * 1024 * 1024]).unwrap();
        std::fs::write(inside.join("zeros.bin"), vec![0u8; 3 * 1024 * 1024]).unwrap();
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(inside.join("link"))
            .arg(&outside)
            .output()
            .unwrap();
        assert!(made.status.success());
        let before = winapi::size_on_disk(&outside.join("zeros.bin")).unwrap();

        let r = run(&inside, false, &CompressProgress::default()).unwrap();
        assert_eq!(r.files, 1, "only the file inside is seen: {r:?}");
        assert_eq!(
            winapi::size_on_disk(&outside.join("zeros.bin")).unwrap(),
            before,
            "the target is untouched"
        );

        // a link handed over as the root is refused
        assert!(run(&inside.join("link"), false, &CompressProgress::default()).is_err());
        let _ = std::fs::remove_dir(inside.join("link"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn cancelling_stops_before_the_work() {
        let dir = tmp("cancel");
        std::fs::write(dir.join("a.bin"), vec![0u8; 100_000]).unwrap();
        let p = CompressProgress::default();
        p.cancel();
        let r = run(&dir, false, &p).unwrap();
        assert!(r.cancelled);
        assert_eq!(r.changed, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_root_is_an_error() {
        assert!(
            run(
                Path::new(r"Z:\definitely\not\here"),
                false,
                &CompressProgress::default()
            )
            .is_err()
        );
    }
}
