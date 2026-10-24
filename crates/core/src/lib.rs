//! spacetree core: arena tree model, scanners (parallel walk, NTFS MFT), aggregation, CSV export.

pub mod cleanup;
pub mod dupes;
pub mod error;
pub mod export;
pub mod ntfs;
pub mod orphans;
pub mod progress;
pub mod query;
pub mod scan;
pub mod snapshot;
pub mod stats;
pub mod tree;
pub mod walk;

#[cfg(windows)]
pub mod carve;
#[cfg(windows)]
pub mod compress;
#[cfg(windows)]
pub mod mft;
#[cfg(windows)]
pub mod undelete;
#[cfg(windows)]
pub mod usn;
#[cfg(windows)]
mod winapi;

pub use error::ScanError;
pub use ntfs::{NO_NODE, StreamInfo, UsnPos};
pub use progress::Progress;
pub use scan::{Backend, ScanOutput, Selection, Timings, Used, drive_of_root, scan, select_backend};
pub use tree::{Kind, NodeId, ROOT, Tree};

/// `(total, free)` bytes of the volume that contains `path`; `None` when unknown (or not on Windows).
pub fn disk_space(path: &std::path::Path) -> Option<(u64, u64)> {
    #[cfg(windows)]
    {
        winapi::disk_space(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

/// The path to hand to file-system calls for a path built by [`Tree::path`]. On Windows an absolute path gets
/// the `\\?\` prefix: Win32 normalisation would otherwise strip trailing dots and spaces from every component
/// (`data.` -> `data`), so a call meant for one file would hit its neighbour. Tree names never are `.` or `..`.
pub fn fs_path(path: &str) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        if path.starts_with(r"\\?\") || path.starts_with(r"\\.\") {
            return path.into();
        }
        if let Some(unc) = path.strip_prefix(r"\\") {
            return format!(r"\\?\UNC\{}", unc.replace('/', "\\")).into();
        }
        let b = path.as_bytes();
        if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/') {
            return format!(r"\\?\{}", path.replace('/', "\\")).into();
        }
    }
    path.into()
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn fs_path_makes_absolute_paths_verbatim() {
        let s = |p: &str| super::fs_path(p).to_string_lossy().into_owned();
        assert_eq!(s(r"C:\a\b."), r"\\?\C:\a\b.");
        assert_eq!(s("D:/x/y "), r"\\?\D:\x\y ");
        assert_eq!(s(r"\\srv\share\f."), r"\\?\UNC\srv\share\f.");
        assert_eq!(s(r"\\?\C:\a"), r"\\?\C:\a");
        assert_eq!(s("relative"), "relative");
    }
}
