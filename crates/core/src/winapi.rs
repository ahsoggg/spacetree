//! Thin WinAPI wrappers. All `unsafe` of the crate lives here.

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use crate::ntfs::UsnPos;
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_DESCRIPTOR, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileFullDirectoryInfo, FileIdType, FileStandardInfo,
    GetCompressedFileSizeW, GetDiskFreeSpaceExW, GetFileInformationByHandleEx, GetVolumeInformationByHandleW,
    OPEN_EXISTING, OpenFileById,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DEVICE_SEEK_PENALTY_DESCRIPTOR, FSCTL_GET_NTFS_VOLUME_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL,
    IOCTL_STORAGE_QUERY_PROPERTY, NTFS_VOLUME_DATA_BUFFER, PropertyStandardQuery, READ_USN_JOURNAL_DATA_V0,
    STORAGE_PROPERTY_QUERY, StorageDeviceSeekPenaltyProperty, USN_JOURNAL_DATA_V0,
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

/// FSCTL_QUERY_USN_JOURNAL; fails with `ERROR_JOURNAL_NOT_ACTIVE` when the volume keeps no journal.
pub fn usn_query(volume: &File) -> io::Result<UsnPos> {
    // SAFETY: plain-old-data, all-zero is valid.
    let mut out: USN_JOURNAL_DATA_V0 = unsafe { std::mem::zeroed() };
    let mut returned = 0u32;
    // SAFETY: valid volume handle; the output pointer/size describe `out`; no input buffer, no OVERLAPPED.
    let ok = unsafe {
        DeviceIoControl(
            volume.as_raw_handle(),
            FSCTL_QUERY_USN_JOURNAL,
            std::ptr::null(),
            0,
            &mut out as *mut USN_JOURNAL_DATA_V0 as *mut c_void,
            std::mem::size_of::<USN_JOURNAL_DATA_V0>() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(UsnPos {
        id: out.UsnJournalID,
        next_usn: out.NextUsn,
        lowest_valid_usn: out.LowestValidUsn,
    })
}

/// FSCTL_READ_USN_JOURNAL without waiting: fills `buf` with `[next usn: i64][USN_RECORD_V2...]` for the records
/// from `start_usn` on whose reason matches `reason_mask`, and returns the number of bytes written.
pub fn usn_read(volume: &File, journal_id: u64, start_usn: i64, reason_mask: u32, buf: &mut [u8]) -> io::Result<usize> {
    let input = READ_USN_JOURNAL_DATA_V0 {
        StartUsn: start_usn,
        ReasonMask: reason_mask,
        ReturnOnlyOnClose: 0,
        Timeout: 0,
        BytesToWaitFor: 0,
        UsnJournalID: journal_id,
    };
    let mut returned = 0u32;
    // SAFETY: valid volume handle; `input` and `buf` outlive the call and their sizes are passed exactly;
    // no OVERLAPPED is used and the handle was opened synchronously.
    let ok = unsafe {
        DeviceIoControl(
            volume.as_raw_handle(),
            FSCTL_READ_USN_JOURNAL,
            &input as *const READ_USN_JOURNAL_DATA_V0 as *const c_void,
            std::mem::size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
            buf.as_mut_ptr() as *mut c_void,
            buf.len().min(u32::MAX as usize) as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(returned as usize)
}

/// What a file reference number (as found in the journal) currently looks like on disk.
#[derive(Clone, Copy, Debug)]
pub struct FileById {
    pub size: u64,
    pub alloc: u64,
    pub is_dir: bool,
    pub delete_pending: bool,
}

/// Opens the file with the given reference number (via any handle `volume` of the same volume) and returns
/// its size, allocation size and kind. `ErrorKind::NotFound` means the file is gone.
pub fn file_by_id(volume: &File, frn: u64) -> io::Result<FileById> {
    // SAFETY: plain-old-data, all-zero is valid; the fields are then set below.
    let mut desc: FILE_ID_DESCRIPTOR = unsafe { std::mem::zeroed() };
    desc.dwSize = std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32;
    desc.Type = FileIdType;
    desc.Anonymous.FileId = frn as i64;
    // SAFETY: `desc` is a valid descriptor; zero access is enough to query attributes; backup semantics let it
    // open folders; no security attributes.
    let h = unsafe {
        OpenFileById(
            volume.as_raw_handle(),
            &desc,
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            FILE_FLAG_BACKUP_SEMANTICS,
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let h = Handle(h);
    // SAFETY: plain-old-data, all-zero is valid.
    let mut info: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: valid handle; the buffer is `info` and its exact size is passed.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            h.0,
            FileStandardInfo,
            &mut info as *mut FILE_STANDARD_INFO as *mut c_void,
            std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(FileById {
        size: info.EndOfFile.max(0) as u64,
        alloc: info.AllocationSize.max(0) as u64,
        is_dir: info.Directory,
        delete_pending: info.DeletePending,
    })
}

/// `FSCTL_SET_EXTERNAL_BACKING` / `FSCTL_DELETE_EXTERNAL_BACKING` (winioctl.h): CTL_CODE(FILE_DEVICE_FILE_SYSTEM,
/// 195 / 197, METHOD_BUFFERED, FILE_SPECIAL_ACCESS).
const FSCTL_SET_EXTERNAL_BACKING: u32 = 0x0009_030C;
const FSCTL_DELETE_EXTERNAL_BACKING: u32 = 0x0009_0314;
const WOF_CURRENT_VERSION: u32 = 1;
const WOF_PROVIDER_FILE: u32 = 2;
const FILE_PROVIDER_CURRENT_VERSION: u32 = 1;
/// `FILE_PROVIDER_COMPRESSION_LZX`: the strongest "system compression" (Windows Compact) algorithm.
const FILE_PROVIDER_COMPRESSION_LZX: u32 = 1;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

/// WOF_EXTERNAL_INFO followed by FILE_PROVIDER_EXTERNAL_INFO_V1.
#[repr(C)]
struct WofRequest {
    wof_version: u32,
    wof_provider: u32,
    provider_version: u32,
    algorithm: u32,
    flags: u32,
}

/// `through_wof`: open the logical file of an already compressed file (needed to remove the compression, since
/// such a file is itself a reparse point). Otherwise a link is never followed: the control code then acts on
/// (and fails for) the link itself.
fn open_for_wof(path: &Path, access: u32, share: u32, through_wof: bool) -> io::Result<Handle> {
    let p = wide(path);
    let flags = if through_wof { 0 } else { FILE_FLAG_OPEN_REPARSE_POINT };
    // SAFETY: `p` is NUL-terminated; no security attributes or template.
    let h = unsafe {
        CreateFileW(
            p.as_ptr(),
            access,
            share,
            std::ptr::null(),
            OPEN_EXISTING,
            flags,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(Handle(h))
}

/// `FSCTL_GET_EXTERNAL_BACKING`: CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 196, METHOD_BUFFERED, FILE_ANY_ACCESS).
const FSCTL_GET_EXTERNAL_BACKING: u32 = 0x0009_0310;

/// Whether the file is currently stored by the Windows Overlay Filter (system compression). The query is
/// answered on a handle that opens the reparse point itself, not the logical file.
fn is_externally_backed(path: &Path) -> bool {
    let Ok(h) = open_for_wof(path, 0, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, false) else {
        return false;
    };
    let mut out = [0u8; 64];
    let mut returned = 0u32;
    // SAFETY: valid handle; `out` is writable for its exact length; no input buffer, no OVERLAPPED.
    let ok = unsafe {
        DeviceIoControl(
            h.0,
            FSCTL_GET_EXTERNAL_BACKING,
            std::ptr::null(),
            0,
            out.as_mut_ptr() as *mut c_void,
            out.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    ok != 0
}

/// Compresses the file with LZX in place (what `compact /C /EXE:LZX` does) or, with `compress == false`,
/// restores it to ordinary storage. The file keeps its name, contents and attributes; programs read it as before.
/// Fails for files that cannot be compressed (open by others, too small, not on NTFS, ...); removing the
/// compression from a file that has none fails with `ERROR_OBJECT_NOT_EXTERNALLY_BACKED` (342).
pub fn set_lzx_compression(path: &Path, compress: bool) -> io::Result<()> {
    // Compressing: sharing is limited to readers, so a file somebody writes to is not touched.
    let share = if compress {
        FILE_SHARE_READ
    } else {
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
    };
    if !compress && !is_externally_backed(path) {
        return Err(io::Error::from_raw_os_error(342));
    }
    let h = open_for_wof(path, GENERIC_READ | GENERIC_WRITE, share, !compress)?;
    let req = WofRequest {
        wof_version: WOF_CURRENT_VERSION,
        wof_provider: WOF_PROVIDER_FILE,
        provider_version: FILE_PROVIDER_CURRENT_VERSION,
        algorithm: FILE_PROVIDER_COMPRESSION_LZX,
        flags: 0,
    };
    let (code, input, input_len) = if compress {
        (
            FSCTL_SET_EXTERNAL_BACKING,
            &req as *const WofRequest as *const c_void,
            std::mem::size_of::<WofRequest>() as u32,
        )
    } else {
        (FSCTL_DELETE_EXTERNAL_BACKING, std::ptr::null(), 0)
    };
    let mut returned = 0u32;
    // SAFETY: valid handle; `req` (or null with size 0) outlives the call; there is no output buffer and no
    // OVERLAPPED (the handle is synchronous).
    let ok = unsafe {
        DeviceIoControl(
            h.0,
            code,
            input,
            input_len,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok != 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    // Removing the compression reports ERROR_OBJECT_NOT_EXTERNALLY_BACKED although it worked: what counts is the
    // state of the file afterwards.
    if !compress && !is_externally_backed(path) {
        return Ok(());
    }
    Err(err)
}

/// Bytes the file occupies on disk (`GetCompressedFileSizeW`: reflects NTFS and system compression).
pub fn size_on_disk(path: &Path) -> io::Result<u64> {
    let p = wide(path);
    let mut high = 0u32;
    // SAFETY: `p` is NUL-terminated; `high` is a live out-parameter.
    let low = unsafe { GetCompressedFileSizeW(p.as_ptr(), &mut high) };
    if low == u32::MAX {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(0) {
            return Err(e);
        }
    }
    Ok(u64::from(high) << 32 | u64::from(low))
}
