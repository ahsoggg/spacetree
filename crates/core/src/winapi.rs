//! Thin WinAPI wrappers. All `unsafe` of the crate lives here.

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileFullDirectoryInfo, GetDiskFreeSpaceExW, GetFileInformationByHandleEx, GetVolumeInformationByHandleW,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DEVICE_SEEK_PENALTY_DESCRIPTOR, FSCTL_GET_NTFS_VOLUME_DATA, IOCTL_STORAGE_QUERY_PROPERTY, NTFS_VOLUME_DATA_BUFFER,
    PropertyStandardQuery, STORAGE_PROPERTY_QUERY, StorageDeviceSeekPenaltyProperty,
};

#[repr(C)]
struct VolumeDataOut {
    base: NTFS_VOLUME_DATA_BUFFER,
    // NTFS_EXTENDED_VOLUME_DATA may follow the base structure; reserve room for it.
    extended: [u8; 64],
}

/// FSCTL_GET_NTFS_VOLUME_DATA; fails with `ERROR_INVALID_FUNCTION` on non-NTFS volumes.
pub fn ntfs_volume_data(volume: &File) -> io::Result<NTFS_VOLUME_DATA_BUFFER> {
    // SAFETY: `VolumeDataOut` is plain-old-data, all-zero is a valid value.
    let mut out: VolumeDataOut = unsafe { std::mem::zeroed() };
    let mut returned = 0u32;
    // SAFETY: the handle is valid for the lifetime of `volume`; the output pointer/size describe
    // `out`, which outlives the call; no input buffer is used (null, 0); no OVERLAPPED is passed
    // and the handle was opened synchronously.
    let ok = unsafe {
        DeviceIoControl(
            volume.as_raw_handle(),
            FSCTL_GET_NTFS_VOLUME_DATA,
            std::ptr::null(),
            0,
            &mut out as *mut VolumeDataOut as *mut c_void,
            std::mem::size_of::<VolumeDataOut>() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(out.base)
}

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// `(total, free)` bytes of the volume containing `path`.
pub fn disk_space(path: &Path) -> Option<(u64, u64)> {
    let src = wide(path);
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: `src` is NUL-terminated; out-pointers reference live locals.
    let ok = unsafe { GetDiskFreeSpaceExW(src.as_ptr(), &mut avail, &mut total, &mut free) };
    (ok != 0 && total > 0).then_some((total, free))
}

/// Serial number of the volume that really holds the existing file or folder `path`: the open follows
/// junctions, symbolic links, mount points and `subst` drives. `None` when it cannot be opened or queried.
pub fn volume_serial(path: &Path) -> Option<u32> {
    let p = wide(path);
    // SAFETY: `p` is NUL-terminated; zero access with backup semantics opens files and folders alike.
    let h = unsafe {
        CreateFileW(
            p.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return None;
    }
    let h = Handle(h);
    let mut serial = 0u32;
    // SAFETY: valid handle; only the serial out-pointer is passed, the optional buffers are null with size 0.
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            h.0,
            std::ptr::null_mut(),
            0,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    (ok != 0).then_some(serial)
}

/// Whether the drive holding `path` is a spinning disk (`Some(true)`), an SSD (`Some(false)`), or unknown
/// (`None`: network path, no drive letter, or the device does not answer). Needs no administrator rights:
/// the volume is opened with zero access, which is enough for a property query.
pub fn incurs_seek_penalty(path: &Path) -> Option<bool> {
    let s = path.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    let mut it = s.chars();
    let letter = it.next().filter(char::is_ascii_alphabetic)?;
    if it.next() != Some(':') {
        return None;
    }
    let vol: Vec<u16> = format!(r"\\.\{}:", letter.to_ascii_uppercase())
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `vol` is NUL-terminated; no security attributes or template handle are passed.
    let h = unsafe {
        CreateFileW(
            vol.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return None;
    }
    let h = Handle(h);
    // SAFETY: both structures are plain-old-data; all-zero is a valid value (PropertyStandardQuery == 0).
    let mut query: STORAGE_PROPERTY_QUERY = unsafe { std::mem::zeroed() };
    query.PropertyId = StorageDeviceSeekPenaltyProperty;
    query.QueryType = PropertyStandardQuery;
    let mut out: DEVICE_SEEK_PENALTY_DESCRIPTOR = unsafe { std::mem::zeroed() };
    let mut returned = 0u32;
    // SAFETY: the pointers/sizes describe `query` and `out`, which outlive the call; the handle is valid and
    // synchronous, so no OVERLAPPED is needed.
    let ok = unsafe {
        DeviceIoControl(
            h.0,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &query as *const STORAGE_PROPERTY_QUERY as *const c_void,
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            &mut out as *mut DEVICE_SEEK_PENALTY_DESCRIPTOR as *mut c_void,
            std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    (ok != 0 && returned as usize >= std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>())
        .then_some(out.IncursSeekPenalty)
}

/// One entry of a directory listing; `name` is UTF-16 without terminator.
pub struct RawEntry<'a> {
    pub name: &'a [u16],
    pub attributes: u32,
    /// Reparse tag when `attributes` has `FILE_ATTRIBUTE_REPARSE_POINT`.
    pub reparse_tag: u32,
    pub size: u64,
    pub alloc: u64,
    /// FILETIME (100 ns ticks since 1601).
    pub mtime: i64,
}

/// 64 KiB, 8-byte aligned, reused by each thread.
const LIST_BUF_WORDS: usize = 8192;

thread_local! {
    static LIST_BUF: std::cell::RefCell<Vec<u64>> = std::cell::RefCell::new(vec![0u64; LIST_BUF_WORDS]);
}

struct Handle(windows_sys::Win32::Foundation::HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful CreateFileW and is closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

/// Lists a directory with `GetFileInformationByHandleEx(FileFullDirectoryInfo)`: one syscall returns
/// as many entries as fit into 64 KiB, with sizes, allocation size and timestamps — no per-file calls.
/// `path` must be NUL-terminated (a `\\?\` prefix lifts the MAX_PATH limit). `.` and `..` are skipped.
pub fn list_dir(path: &[u16], mut f: impl FnMut(RawEntry)) -> io::Result<()> {
    debug_assert_eq!(path.last(), Some(&0));
    // SAFETY: `path` is NUL-terminated; no security attributes or template handle are passed.
    let h = unsafe {
        CreateFileW(
            path.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    query_entries(&Handle(h), &mut f)
}

fn query_entries(h: &Handle, f: &mut impl FnMut(RawEntry)) -> io::Result<()> {
    LIST_BUF.with_borrow_mut(|buf| {
        let cap = buf.len() * 8;
        loop {
            // SAFETY: `buf` is a writable, 8-byte aligned buffer of `cap` bytes that outlives the call;
            // the handle is a valid synchronous directory handle.
            let ok = unsafe {
                GetFileInformationByHandleEx(h.0, FileFullDirectoryInfo, buf.as_mut_ptr() as *mut c_void, cap as u32)
            };
            if ok == 0 {
                let e = io::Error::last_os_error();
                return if e.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                    Ok(())
                } else {
                    Err(e)
                };
            }
            // SAFETY: the buffer is initialised memory of `cap` bytes; u8 has no alignment requirement.
            let b: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, cap) };
            parse_full_dir_info(b, f);
        }
    })
}

/// Walks the FILE_FULL_DIR_INFO chain in `b` (layout: learn.microsoft.com, "FILE_FULL_DIR_INFO structure").
fn parse_full_dir_info(b: &[u8], f: &mut impl FnMut(RawEntry)) {
    const NAME: usize = 0x44;
    let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let i64_at = |o: usize| i64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let mut name_buf: Vec<u16> = Vec::new();
    let mut off = 0usize;
    loop {
        if off + NAME > b.len() {
            return;
        }
        let next = u32_at(off) as usize;
        let name_bytes = u32_at(off + 0x3C) as usize;
        let start = off + NAME;
        if start + name_bytes > b.len() {
            return;
        }
        name_buf.clear();
        name_buf.extend(
            b[start..start + name_bytes]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|p| u16::from_le_bytes(*p)),
        );
        let dot = u16::from(b'.');
        let is_dot = matches!(name_buf.as_slice(), [d] | [d, _] if *d == dot) && name_buf.iter().all(|&c| c == dot);
        if !is_dot {
            f(RawEntry {
                name: &name_buf,
                attributes: u32_at(off + 0x38),
                reparse_tag: u32_at(off + 0x40),
                size: i64_at(off + 0x28).max(0) as u64,
                alloc: i64_at(off + 0x30).max(0) as u64,
                mtime: i64_at(off + 0x18),
            });
        }
        if next == 0 {
            return;
        }
        off += next;
    }
}
