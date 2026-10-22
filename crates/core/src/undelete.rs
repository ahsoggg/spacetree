//! Recovery of deleted files on NTFS.
//!
//! Deleting a file only clears the "in use" flag of its `$MFT` record and frees its clusters in `$Bitmap`; the
//! record (name, size, data runs) and the data stay where they were until something reuses them. [`scan`] reads
//! every free record, rebuilds names and paths, and checks the clusters of each file against the current
//! cluster bitmap to estimate the chance of getting the content back. [`Volume::restore`] copies the clusters
//! out to another drive.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{channel, sync_channel};
use std::time::{Duration, UNIX_EPOCH};

use rayon::prelude::*;

use crate::error::ScanError;
use crate::mft::{self, VolumeInfo};
use crate::ntfs::{self, ATTR_DATA, ATTR_FILE_NAME, ATTR_STANDARD_INFORMATION, Run, u16_at, u32_at, u64_at};
use crate::progress::Progress;

const FLAG_IN_USE: u16 = 0x01;
const FLAG_DIR: u16 = 0x02;
const AF_COMPRESSED: u16 = 0x0001;
const AF_ENCRYPTED: u16 = 0x4000;
const ATTR_END: u32 = 0xFFFF_FFFF;
const FRN_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
const ROOT_RECORD: u64 = 5;
/// Records below this number are NTFS metadata files (`$MFT`, `$LogFile`, ...).
const FIRST_USER_RECORD: u64 = 24;
const CHUNK: u64 = 8 * 1024 * 1024;
const TASK_BYTES: usize = 512 * 1024;
const POOL_BUFFERS: usize = 4;
const COPY_BUF: u64 = 4 * 1024 * 1024;
const MAX_CLUSTER_BITMAP: u64 = 1024 * 1024 * 1024;
const UNKNOWN_DIR: &str = "[папка не найдена]";

/// How likely the content of a deleted file can still be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Chance {
    /// Every cluster is still free: the data should be intact (unless the drive trimmed it).
    Good,
    /// Some clusters were reused by other files: the result will be damaged.
    Partial,
    /// Found by a signature scan of free space: content is plausible, name and date are lost.
    Carved,
    /// All clusters were reused (or the record lost its data runs).
    Lost,
    /// Encrypted (EFS) streams cannot be restored without the key.
    Unsupported,
}

#[derive(Clone, Debug)]
pub struct DeletedFile {
    pub rec: u64,
    pub name: String,
    /// Folder the file was in, relative to the drive root (`""` for the root).
    pub dir: String,
    pub size: u64,
    /// Last modification, Unix seconds (0 = unknown).
    pub mtime: u32,
    pub chance: Chance,
    /// Share of the clusters that are still free, 0..=100.
    pub intact: u8,
    resident: Option<Vec<u8>>,
    runs: Vec<Run>,
    /// log2 of the compression unit in clusters (0 = not compressed).
    unit: u8,
    free: u64,
    total: u64,
    /// Bytes the data runs cover.
    covered: u64,
}

impl DeletedFile {
    /// A file found by the signature scan: `size` bytes starting at cluster `lcn`.
    pub(crate) fn carved(lcn: u64, size: u64, ext: &str, bpc: u64) -> Self {
        let len = size.div_ceil(bpc);
        DeletedFile {
            rec: lcn,
            name: format!("carved_{lcn}.{ext}"),
            dir: "[глубокий поиск]".into(),
            size,
            mtime: 0,
            chance: Chance::Carved,
            intact: 100,
            resident: None,
            runs: vec![Run {
                vcn: 0,
                len,
                lcn: Some(lcn),
            }],
            unit: 0,
            free: len,
            total: len,
            covered: len * bpc,
        }
    }

    /// First cluster of the data (used to match carved files against MFT results).
    pub fn first_lcn(&self) -> Option<u64> {
        self.runs.iter().find_map(|r| r.lcn)
    }

    pub fn path(&self) -> String {
        if self.dir.is_empty() {
            self.name.clone()
        } else {
            format!("{}\\{}", self.dir, self.name)
        }
    }

    pub fn ext(&self) -> Option<&str> {
        self.name
            .rsplit_once('.')
            .map(|(_, e)| e)
            .filter(|e| !e.is_empty() && e.len() <= 8)
    }
}

#[derive(Clone)]
pub struct UndeleteScan {
    pub drive: char,
    pub files: Vec<DeletedFile>,
    pub records: u64,
    /// Free records whose name or data could not be used.
    pub skipped: u64,
    /// `Some(true)` for an SSD (deleted data is usually trimmed), `Some(false)` for a spinning disk.
    pub ssd: Option<bool>,
}

impl UndeleteScan {
    /// First clusters of every file known from the MFT.
    pub fn start_lcns(&self) -> std::collections::HashSet<u64> {
        self.files.iter().filter_map(DeletedFile::first_lcn).collect()
    }
}

// ---------------------------------------------------------------- record parsing

struct DirRec {
    seq: u16,
    parent: u64,
    parent_seq: u16,
    name: String,
    in_use: bool,
}

struct Data {
    size: u64,
    resident: Option<Vec<u8>>,
    start_vcn: u64,
    flags: u16,
    unit: u8,
    runs: Vec<Run>,
    bad_runs: bool,
}

struct RawFile {
    rec: u64,
    parent: u64,
    parent_seq: u16,
    name: String,
    mtime: u32,
    data: Option<Data>,
}

struct Ext {
    base: u64,
    data: Data,
}

#[derive(Default)]
struct Chunk {
    dirs: Vec<(u64, DirRec)>,
    files: Vec<RawFile>,
    exts: Vec<Ext>,
    skipped: u64,
}

fn si_mtime(attr: &[u8]) -> u32 {
    if attr.len() < 0x18 {
        return 0;
    }
    let coff = u16_at(attr, 0x14) as usize;
    if u32_at(attr, 0x10) < 0x10 || coff + 0x10 > attr.len() {
        return 0;
    }
    let ft = u64_at(attr, coff + 0x08) as i64;
    (ft / 10_000_000 - 11_644_473_600).clamp(0, u32::MAX as i64) as u32
}

/// `(rank, parent, parent sequence, name)` of a resident `$FILE_NAME`; DOS-only names rank lowest.
fn file_name(attr: &[u8]) -> Option<(u8, u64, u16, String)> {
    if attr.len() < 0x18 {
        return None;
    }
    let coff = u16_at(attr, 0x14) as usize;
    let c = attr.get(coff..)?;
    if c.len() < 0x42 {
        return None;
    }
    let (nchars, ns) = (c[0x40] as usize, c[0x41]);
    if nchars == 0 || 0x42 + nchars * 2 > c.len() {
        return None;
    }
    let units = c[0x42..0x42 + nchars * 2]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| u16::from_le_bytes(*p));
    let name: String = char::decode_utf16(units)
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect();
    let parent_ref = u64_at(c, 0);
    let rank = match ns {
        2 => 1,
        0 => 2,
        _ => 3,
    };
    Some((rank, parent_ref & FRN_MASK, (parent_ref >> 48) as u16, name))
}

/// The unnamed `$DATA` attribute of a record.
fn data_attr(attr: &[u8]) -> Option<Data> {
    if attr[8] == 0 {
        let (clen, coff) = (u32_at(attr, 0x10) as usize, u16_at(attr, 0x14) as usize);
        let content = attr.get(coff..coff.checked_add(clen)?)?;
        return Some(Data {
            size: clen as u64,
            resident: Some(content.to_vec()),
            start_vcn: 0,
            flags: 0,
            unit: 0,
            runs: Vec::new(),
            bad_runs: false,
        });
    }
    if attr.len() < 0x40 {
        return None;
    }
    let start_vcn = u64_at(attr, 0x10);
    let roff = u16_at(attr, 0x20) as usize;
    let (mut runs, mut bad_runs) = match ntfs::parse_runs(attr.get(roff..).unwrap_or(&[])) {
        Ok(r) => (r, false),
        Err(_) => (Vec::new(), true),
    };
    for r in &mut runs {
        match r.vcn.checked_add(start_vcn).filter(|v| v.checked_add(r.len).is_some()) {
            Some(v) => r.vcn = v,
            None => bad_runs = true,
        }
    }
    if bad_runs {
        runs.clear();
    }
    Some(Data {
        // Sizes are only meaningful in the first extent.
        size: if start_vcn == 0 { u64_at(attr, 0x30) } else { 0 },
        resident: None,
        start_vcn,
        flags: u16_at(attr, 0x0C),
        unit: if u16_at(attr, 0x0C) & AF_COMPRESSED != 0 {
            // NTFS only writes 2^4-cluster units; larger values come from stale bytes and would only make
            // `read_unit` allocate up to `bpc << 16` per unit.
            u16_at(attr, 0x22).min(4) as u8
        } else {
            0
        },
        runs,
        bad_runs,
    })
}

/// Parses one `$MFT` record. Only directories (any state) and deleted files are kept.
fn parse_free(buf: &mut [u8], rec_no: u64, out: &mut Chunk) {
    if buf.len() < 0x30 || &buf[0..4] != b"FILE" || rec_no < FIRST_USER_RECORD {
        return;
    }
    let flags = u16_at(buf, 0x16);
    let (in_use, is_dir) = (flags & FLAG_IN_USE != 0, flags & FLAG_DIR != 0);
    if in_use && !is_dir {
        return; // the common case: a live file
    }
    if !ntfs::apply_fixups(buf) {
        return;
    }
    let seq = u16_at(buf, 0x10);
    let base_ref = u64_at(buf, 0x20) & FRN_MASK;
    let used = (u32_at(buf, 0x18) as usize).min(buf.len());
    let mut off = u16_at(buf, 0x14) as usize;

    let mut name: Option<(u8, u64, u16, String)> = None;
    let mut mtime = 0;
    let mut data: Option<Data> = None;
    while off + 8 <= used {
        let ty = u32_at(buf, off);
        if ty == ATTR_END || ty > ATTR_DATA {
            break;
        }
        let len = u32_at(buf, off + 4) as usize;
        if len < 0x10 || !len.is_multiple_of(8) || off + len > buf.len() {
            break;
        }
        let attr = &buf[off..off + len];
        match ty {
            ATTR_STANDARD_INFORMATION if attr[8] == 0 => mtime = si_mtime(attr),
            ATTR_FILE_NAME if attr[8] == 0 => {
                if let Some(n) = file_name(attr)
                    && name.as_ref().is_none_or(|cur| n.0 > cur.0)
                {
                    name = Some(n);
                }
            }
            ATTR_DATA if attr[9] == 0 && data.is_none() => data = data_attr(attr),
            _ => {}
        }
        off += len;
    }

    if is_dir {
        if base_ref == 0
            && let Some((_, parent, parent_seq, name)) = name
        {
            out.dirs.push((
                rec_no,
                DirRec {
                    seq,
                    parent,
                    parent_seq,
                    name,
                    in_use,
                },
            ));
        }
        return;
    }
    if base_ref != 0 {
        if let Some(data) = data {
            out.exts.push(Ext { base: base_ref, data });
        }
        return;
    }
    let Some((_, parent, parent_seq, name)) = name else {
        out.skipped += 1;
        return;
    };
    out.files.push(RawFile {
        rec: rec_no,
        parent,
        parent_seq,
        name,
        mtime,
        data,
    });
}

// ---------------------------------------------------------------- paths

struct Paths {
    dirs: std::collections::HashMap<u64, DirRec>,
    memo: std::collections::HashMap<u64, Option<String>>,
}

impl Paths {
    /// Path of directory record `rec` (referenced with sequence number `seq`); `None` when the chain is broken.
    fn dir_path(&mut self, rec: u64, seq: u16, depth: u32) -> Option<String> {
        if rec == ROOT_RECORD {
            return Some(String::new());
        }
        if depth > 64 {
            return None;
        }
        if let Some(p) = self.memo.get(&rec) {
            return p.clone();
        }
        let d = self.dirs.get(&rec)?;
        // A freed record's sequence number was bumped on deletion, so accept both.
        let ok = if d.in_use {
            d.seq == seq
        } else {
            d.seq == seq || d.seq == seq.wrapping_add(1)
        };
        let (parent, parent_seq, name) = (d.parent, d.parent_seq, d.name.clone());
        let res = if ok {
            self.dir_path(parent, parent_seq, depth + 1).map(|mut base| {
                if !base.is_empty() {
                    base.push('\\');
                }
                base.push_str(&name);
                base
            })
        } else {
            None
        };
        self.memo.insert(rec, res.clone());
        res
    }
}

// ---------------------------------------------------------------- cluster bitmap

/// The volume's `$Bitmap`: one bit per cluster, set when allocated.
pub(crate) struct ClusterMap(pub(crate) Vec<u8>);

impl ClusterMap {
    pub(crate) fn is_free(&self, lcn: u64) -> bool {
        self.0.get((lcn / 8) as usize).is_some_and(|b| b >> (lcn % 8) & 1 == 0)
    }

    /// Number of free clusters in `lcn..lcn + len` (clusters past the bitmap count as used).
    pub(crate) fn free_in(&self, lcn: u64, len: u64) -> u64 {
        // Clusters past the bitmap count as used, so the walk can stop there (a garbage run length may be ~2^64).
        let bitmap_end = (self.0.len() as u64).saturating_mul(8);
        let (mut i, end) = (lcn, lcn.saturating_add(len).min(bitmap_end));
        let mut free = 0;
        while i < end {
            if i % 8 == 0 && i + 8 <= end {
                if let Some(b) = self.0.get((i / 8) as usize) {
                    free += u64::from(b.count_zeros());
                }
                i += 8;
            } else {
                free += u64::from(self.is_free(i));
                i += 1;
            }
        }
        free
    }
}

pub(crate) fn read_cluster_map(file: &File, mft_runs: &[Run], info: &VolumeInfo) -> Result<ClusterMap, ScanError> {
    let (bpc, rs) = (info.bytes_per_cluster, info.record_size);
    let mut rec = vec![0u8; rs];
    mft::read_virtual(file, mft_runs, bpc, 6 * rs as u64, &mut rec)?; // record 6 is $Bitmap
    if !ntfs::apply_fixups(&mut rec) {
        return Err(ScanError::Ntfs("запись 6 ($Bitmap) повреждена".into()));
    }
    let attr = ntfs::find_attr(&rec, ATTR_DATA).ok_or_else(|| ScanError::Ntfs("в $Bitmap нет $DATA".into()))?;
    let (runs, real) = mft::data_extents(attr)?;
    if real == 0 || real > MAX_CLUSTER_BITMAP {
        return Err(ScanError::Unsupported("размер $Bitmap вне допустимого".into()));
    }
    let mut buf = vec![0u8; real.next_multiple_of(bpc) as usize];
    mft::read_virtual(file, &runs, bpc, 0, &mut buf)?;
    buf.truncate(real as usize);
    Ok(ClusterMap(buf))
}

/// Chance and intact percentage from cluster counts; `covered` is how many bytes the data runs describe.
fn rate(free: u64, total: u64, covered: u64, size: u64) -> (Chance, u8) {
    let mut pct = (free * 100).checked_div(total).map_or(100, |v| v as u8);
    if covered < size {
        pct = (u64::from(pct) * covered / size.max(1)) as u8;
    }
    let chance = match pct {
        100 => Chance::Good,
        0 => Chance::Lost,
        _ => Chance::Partial,
    };
    (chance, pct)
}

/// `(chance, intact %, size, resident bytes, runs, free clusters, allocated clusters, covered bytes)`.
type Assessment = (Chance, u8, u64, Option<Vec<u8>>, Vec<Run>, u64, u64, u64);

fn assess(data: &Option<Data>, extra: &[Run], map: &ClusterMap, bpc: u64) -> Assessment {
    let Some(d) = data else {
        return (Chance::Lost, 0, 0, None, Vec::new(), 0, 0, 0);
    };
    if let Some(bytes) = &d.resident {
        return (Chance::Good, 100, d.size, Some(bytes.clone()), Vec::new(), 0, 0, d.size);
    }
    if d.flags & AF_ENCRYPTED != 0 {
        return (Chance::Unsupported, 0, d.size, None, Vec::new(), 0, 0, 0);
    }
    let mut runs = d.runs.clone();
    runs.extend_from_slice(extra);
    runs.sort_by_key(|r| r.vcn);
    runs.dedup_by_key(|r| r.vcn);
    if d.bad_runs || runs.is_empty() {
        return (Chance::Lost, 0, d.size, None, runs, 0, 0, 0);
    }
    let (mut total, mut free, mut covered) = (0u64, 0u64, 0u64);
    for r in &runs {
        covered += r.len;
        if let Some(lcn) = r.lcn {
            total += r.len;
            free += map.free_in(lcn, r.len);
        }
    }
    let covered = covered.saturating_mul(bpc);
    let (chance, pct) = rate(free, total, covered, d.size);
    (chance, pct, d.size, None, runs, free, total, covered)
}

/// Two deleted files that claim the same clusters cannot both be intact: the newer one owns them, the older
/// one loses the shared clusters.
fn apply_overlaps(files: &mut [DeletedFile]) {
    let mut ev: Vec<(u64, u64, usize)> = Vec::new(); // (first cluster, end, file)
    for (i, f) in files.iter().enumerate() {
        ev.extend(f.runs.iter().filter_map(|r| r.lcn.map(|l| (l, l + r.len, i))));
    }
    ev.sort_unstable();
    let mut lost = vec![0u64; files.len()];
    let mut holder: Option<(u64, usize)> = None; // the run reaching furthest so far
    for (s, e, i) in ev {
        if let Some((he, hi)) = holder
            && s < he
            && hi != i
        {
            let (a, b) = (&files[i], &files[hi]);
            let loser = if (a.mtime, a.rec) < (b.mtime, b.rec) { i } else { hi };
            lost[loser] += e.min(he) - s;
        }
        if holder.is_none_or(|(he, _)| e > he) {
            holder = Some((e, i));
        }
    }
    for (f, lost) in files.iter_mut().zip(lost) {
        if lost > 0 {
            f.free = f.free.saturating_sub(lost);
            (f.chance, f.intact) = rate(f.free, f.total, f.covered, f.size);
        }
    }
}

// ---------------------------------------------------------------- scan

/// Reads the whole `$MFT` of `drive` and returns every deleted file whose record can still be read.
pub fn scan(drive: char, progress: &Progress) -> Result<UndeleteScan, ScanError> {
    let (file, info) = mft::probe(drive)?;
    let (runs, real_size, _) = mft::mft_extents(&file, &info)?;
    let (bpc, rs) = (info.bytes_per_cluster, info.record_size);
    let total_records = real_size / rs as u64;
    progress.total_units.store(total_records, Relaxed);
    let map = read_cluster_map(&file, &runs, &info)?;

    let per_task = (TASK_BYTES / rs).max(1);
    let per_chunk = (CHUNK / rs as u64).max(1);
    let (tx, rx) = sync_channel::<(u64, usize, Vec<u8>)>(POOL_BUFFERS);
    let (pool_tx, pool_rx) = channel::<Vec<u8>>();
    for _ in 0..POOL_BUFFERS {
        let _ = pool_tx.send(vec![0u8; per_chunk as usize * rs]);
    }
    let mut chunks: Vec<Chunk> = Vec::new();
    std::thread::scope(|s| {
        let (file, runs) = (&file, &runs);
        s.spawn(move || {
            let mut pos = 0;
            while pos < total_records {
                if progress.is_cancelled() {
                    break;
                }
                let Ok(mut buf) = pool_rx.recv() else { break };
                let n = per_chunk.min(total_records - pos);
                let slice = &mut buf[..n as usize * rs];
                if mft::read_virtual(file, runs, bpc, pos * rs as u64, slice).is_err() {
                    progress.errors.fetch_add(1, Relaxed);
                    slice.fill(0);
                }
                if tx.send((pos, n as usize, buf)).is_err() {
                    break;
                }
                pos += n;
            }
        });
        for (first, n, buf) in rx {
            if progress.is_cancelled() {
                break;
            }
            let mut buf = buf;
            let parsed: Vec<Chunk> = buf[..n * rs]
                .par_chunks_mut(per_task * rs)
                .enumerate()
                .map(|(i, part)| {
                    let mut out = Chunk::default();
                    let base = first + (i * per_task) as u64;
                    for (j, rec) in part.chunks_exact_mut(rs).enumerate() {
                        parse_free(rec, base + j as u64, &mut out);
                    }
                    out
                })
                .collect();
            progress
                .files
                .fetch_add(parsed.iter().map(|c| c.files.len() as u64).sum(), Relaxed);
            progress.done_units.fetch_add(n as u64, Relaxed);
            chunks.extend(parsed);
            let _ = pool_tx.send(buf);
        }
        drop(pool_tx);
    });
    if progress.is_cancelled() {
        return Err(ScanError::Cancelled);
    }

    let mut paths = Paths {
        dirs: std::collections::HashMap::new(),
        memo: std::collections::HashMap::new(),
    };
    let mut raw: Vec<RawFile> = Vec::new();
    let mut exts: std::collections::HashMap<u64, Vec<Data>> = std::collections::HashMap::new();
    let mut skipped = 0;
    for c in chunks {
        paths.dirs.extend(c.dirs);
        raw.extend(c.files);
        for e in c.exts {
            exts.entry(e.base).or_default().push(e.data);
        }
        skipped += c.skipped;
    }

    let mut files = Vec::with_capacity(raw.len());
    for f in raw {
        let more = exts.remove(&f.rec).unwrap_or_default();
        let mut extra: Vec<Run> = Vec::new();
        let mut data = f.data;
        for e in more {
            if data.is_none() && e.start_vcn == 0 {
                data = Some(e);
            } else {
                extra.extend(e.runs);
            }
        }
        let unit = data.as_ref().map_or(0, |d| d.unit);
        let (chance, intact, size, resident, runs, free, total, covered) = assess(&data, &extra, &map, bpc);
        if size == 0 {
            skipped += 1;
            continue;
        }
        let dir = paths
            .dir_path(f.parent, f.parent_seq, 0)
            .unwrap_or_else(|| UNKNOWN_DIR.to_owned());
        files.push(DeletedFile {
            rec: f.rec,
            name: f.name,
            dir,
            size,
            mtime: f.mtime,
            chance,
            intact,
            resident,
            runs,
            unit,
            free,
            total,
            covered,
        });
    }
    apply_overlaps(&mut files);
    let ssd = crate::winapi::incurs_seek_penalty(Path::new(&format!("{drive}:\\"))).map(|penalty| !penalty);
    Ok(UndeleteScan {
        drive,
        files,
        records: total_records,
        skipped,
        ssd,
    })
}

// ---------------------------------------------------------------- restore

/// An opened volume that deleted files can be copied from.
pub struct Volume {
    drive: char,
    file: File,
    bpc: u64,
}

#[derive(Debug)]
pub struct Restored {
    pub path: PathBuf,
    pub bytes: u64,
    /// Everything read was zeros (the drive trimmed the data, or it was never there).
    pub all_zero: bool,
    pub read_errors: u64,
}

/// Whether `dest` (which may not exist yet, may be relative, `\\?\`-prefixed, UNC, a `subst` drive or reached
/// through a junction) lies on the volume of `drive`. Compares volume serial numbers of the nearest existing
/// ancestor; falls back to the drive letter of the absolute path when a serial cannot be read.
fn same_volume(dest: &Path, drive: char) -> bool {
    let abs = std::path::absolute(dest).unwrap_or_else(|_| dest.to_path_buf());
    let existing = abs.ancestors().find(|a| a.exists());
    let source = crate::winapi::volume_serial(Path::new(&format!("{drive}:\\")));
    if let (Some(d), Some(s)) = (existing.and_then(crate::winapi::volume_serial), source) {
        return d == s;
    }
    let s = abs.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    s.chars().next().is_some_and(|c| c.eq_ignore_ascii_case(&drive)) && s[1..].starts_with(':')
}

fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if "<>:\"/\\|?*".contains(c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let s = s.trim_end_matches([' ', '.']);
    if s.is_empty() { "_".into() } else { s.to_owned() }
}

fn unique(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (2..)
        .map(|n| path.with_file_name(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .unwrap()
}

impl Volume {
    pub fn open(drive: char) -> Result<Self, ScanError> {
        let (file, info) = mft::probe(drive)?;
        Ok(Volume {
            drive,
            file,
            bpc: info.bytes_per_cluster,
        })
    }

    /// Copies `f` into `dest` (a folder on another drive). With `keep_dirs` the original folder structure is
    /// recreated under `dest`. `progress.done_units` counts bytes of the current file.
    pub fn restore(
        &self,
        f: &DeletedFile,
        dest: &Path,
        keep_dirs: bool,
        progress: &Progress,
    ) -> Result<Restored, ScanError> {
        if same_volume(dest, self.drive) {
            return Err(ScanError::Unsupported(
                "восстанавливать нужно на другой диск: запись на тот же том может затереть удалённые данные".into(),
            ));
        }
        let mut dir = dest.to_path_buf();
        if keep_dirs {
            for part in f.dir.split('\\').filter(|p| !p.is_empty()) {
                dir.push(sanitize(part));
            }
        }
        std::fs::create_dir_all(&dir)?;
        let path = unique(dir.join(sanitize(&f.name)));
        progress.total_units.store(f.size, Relaxed);
        progress.done_units.store(0, Relaxed);
        let mut out = OpenOptions::new().write(true).create_new(true).open(&path)?;
        let res = self.copy_into(f, &mut out, progress);
        let (bytes, all_zero, read_errors) = match res {
            Ok(v) => v,
            Err(e) => {
                drop(out);
                let _ = std::fs::remove_file(&path);
                return Err(e);
            }
        };
        out.set_len(f.size)?;
        if f.mtime != 0 {
            let _ = out.set_modified(UNIX_EPOCH + Duration::from_secs(u64::from(f.mtime)));
        }
        Ok(Restored {
            path,
            bytes,
            all_zero: all_zero && bytes > 0,
            read_errors,
        })
    }

    fn copy_into(&self, f: &DeletedFile, out: &mut File, progress: &Progress) -> Result<(u64, bool, u64), ScanError> {
        if let Some(bytes) = &f.resident {
            out.write_all(bytes)?;
            return Ok((bytes.len() as u64, bytes.iter().all(|&b| b == 0), 0));
        }
        if f.unit > 0 {
            return self.copy_compressed(f, out, progress);
        }
        let (mut pos, mut all_zero, mut errors) = (0u64, true, 0u64);
        let mut buf = vec![0u8; COPY_BUF as usize];
        for run in &f.runs {
            if pos >= f.size {
                break;
            }
            let span = run.len.saturating_mul(self.bpc).min(f.size - pos);
            let Some(lcn) = run.lcn else {
                pos += span;
                out.seek(SeekFrom::Start(pos))?; // sparse: a hole of zeros
                progress.done_units.store(pos, Relaxed);
                continue;
            };
            let mut done = 0;
            while done < span {
                if progress.is_cancelled() {
                    return Err(ScanError::Cancelled);
                }
                // The volume is read unbuffered: whole clusters at cluster-aligned offsets.
                let want = (span - done).min(COPY_BUF);
                let n = want.next_multiple_of(self.bpc) as usize;
                let slice = &mut buf[..n];
                let at = lcn.checked_mul(self.bpc).and_then(|b| b.checked_add(done));
                if at.is_none_or(|at| mft::read_exact_at(&self.file, slice, at).is_err()) {
                    errors += 1;
                    slice.fill(0);
                }
                let part = &slice[..want as usize];
                all_zero &= part.iter().all(|&b| b == 0);
                out.write_all(part)?;
                done += want;
                pos += want;
                progress.done_units.store(pos, Relaxed);
            }
        }
        Ok((pos, all_zero, errors))
    }

    /// Decodes an NTFS-compressed stream unit by unit.
    fn copy_compressed(
        &self,
        f: &DeletedFile,
        out: &mut File,
        progress: &Progress,
    ) -> Result<(u64, bool, u64), ScanError> {
        let unit_bytes = (self.bpc << f.unit) as usize;
        let (mut pos, mut all_zero, mut errors) = (0u64, true, 0u64);
        let mut unit = 0u64;
        while pos < f.size {
            if progress.is_cancelled() {
                return Err(ScanError::Cancelled);
            }
            let want = (f.size - pos).min(unit_bytes as u64) as usize;
            let data = self.read_unit(f, unit, &mut errors);
            let part = &data[..want.min(data.len())];
            all_zero &= part.iter().all(|&b| b == 0);
            out.write_all(part)?;
            pos += part.len() as u64;
            unit += 1;
            progress.done_units.store(pos, Relaxed);
        }
        Ok((pos, all_zero, errors))
    }

    /// Uncompressed bytes of compression unit `unit` of `f` (always a whole unit).
    fn read_unit(&self, f: &DeletedFile, unit: u64, errors: &mut u64) -> Vec<u8> {
        let cu = 1u64 << f.unit;
        let unit_bytes = (cu * self.bpc) as usize;
        let segs = unit_segments(&f.runs, unit * cu, cu);
        let n: u64 = segs.iter().map(|s| s.1).sum();
        if n == 0 {
            return vec![0u8; unit_bytes];
        }
        let mut raw = Vec::with_capacity((n * self.bpc) as usize);
        for (lcn, len) in segs {
            let at = raw.len();
            raw.resize(at + (len * self.bpc) as usize, 0);
            if mft::read_exact_at(&self.file, &mut raw[at..], lcn * self.bpc).is_err() {
                *errors += 1;
            }
        }
        if n >= cu {
            return raw;
        }
        let mut out = Vec::with_capacity(unit_bytes);
        lznt1_decompress(&raw, &mut out);
        out.resize(unit_bytes, 0);
        out
    }

    /// The first `n` bytes of the file's content (for previews); empty when unreadable.
    pub fn head(&self, f: &DeletedFile, n: usize) -> Vec<u8> {
        let n = n.min(f.size as usize);
        if let Some(b) = &f.resident {
            return b[..n.min(b.len())].to_vec();
        }
        if f.unit > 0 {
            let mut errors = 0;
            let mut d = self.read_unit(f, 0, &mut errors);
            d.truncate(n);
            return d;
        }
        let Some(run) = f.runs.first() else { return Vec::new() };
        let Some(lcn) = run.lcn else { return vec![0u8; n] };
        // `max` last: a zero-length run (legal in a run list) would make `clamp(1, 0)` panic.
        let clusters = (n as u64).div_ceil(self.bpc).min(run.len).max(1);
        let Some(at) = lcn.checked_mul(self.bpc) else {
            return Vec::new();
        };
        let mut buf = vec![0u8; (clusters * self.bpc) as usize];
        if mft::read_exact_at(&self.file, &mut buf, at).is_err() {
            return Vec::new();
        }
        buf.truncate(n);
        buf
    }
}

/// `(first cluster, length)` pieces of the allocated clusters of VCNs `from..from + count`.
fn unit_segments(runs: &[Run], from: u64, count: u64) -> Vec<(u64, u64)> {
    let to = from + count;
    let first = runs.partition_point(|r| r.vcn + r.len <= from);
    let mut out = Vec::new();
    for r in &runs[first..] {
        if r.vcn >= to {
            break;
        }
        if let Some(lcn) = r.lcn {
            let (s, e) = (from.max(r.vcn), to.min(r.vcn + r.len));
            out.push((lcn + (s - r.vcn), e - s));
        }
    }
    out
}

/// LZNT1 (the NTFS compression format): chunks of up to 4096 output bytes, each either stored or coded as
/// literals and (offset, length) back references.
fn lznt1_decompress(src: &[u8], out: &mut Vec<u8>) {
    let mut p = 0;
    while p + 2 <= src.len() {
        let hdr = u16::from_le_bytes([src[p], src[p + 1]]);
        if hdr == 0 {
            break;
        }
        p += 2;
        let end = (p + (hdr & 0x0FFF) as usize + 1).min(src.len());
        if hdr & 0x8000 == 0 {
            out.extend_from_slice(&src[p..end]);
            p = end;
            continue;
        }
        let chunk_start = out.len();
        while p < end {
            let flags = src[p];
            p += 1;
            for bit in 0..8 {
                if p >= end {
                    break;
                }
                if flags >> bit & 1 == 0 {
                    out.push(src[p]);
                    p += 1;
                    continue;
                }
                if p + 2 > end {
                    return;
                }
                let t = u16::from_le_bytes([src[p], src[p + 1]]) as usize;
                p += 2;
                let pos = out.len() - chunk_start;
                let mut offset_bits = 4;
                while (1usize << offset_bits) < pos {
                    offset_bits += 1;
                }
                let len_bits = 16 - offset_bits;
                let len = (t & ((1 << len_bits) - 1)) + 3;
                let offset = (t >> len_bits) + 1;
                if offset > pos {
                    return;
                }
                for _ in 0..len {
                    out.push(out[out.len() - offset]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ntfs::testutil::*;

    /// A garbage run length from a stale record must not make the scan count clusters past the bitmap for hours.
    #[test]
    fn free_in_stops_at_the_end_of_the_bitmap() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(ClusterMap(vec![0; 8]).free_in(0, 1 << 50));
        });
        let free = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("free_in must not take forever");
        assert_eq!(free, 64);
        // clusters 5 and 7 of each byte are used: 3, 4, 6 and 8..=12 are free
        assert_eq!(ClusterMap(vec![0b1010_0000; 2]).free_in(3, 10), 8);
    }

    /// Recovered data must never be written onto the volume it is read from, however the folder is spelled.
    #[test]
    fn restore_target_on_the_source_volume_is_detected_in_any_spelling() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path().to_str().unwrap().to_owned();
        let drive = t.chars().next().unwrap();
        assert!(same_volume(tmp.path(), drive));
        assert!(same_volume(Path::new(&format!(r"{t}\not\yet\created")), drive));
        assert!(
            same_volume(Path::new(&format!(r"\\?\{t}\new")), drive),
            "verbatim prefix"
        );
        let unc = format!(r"\\localhost\{}$\{}", drive, &t[3..]);
        if Path::new(&unc).exists() {
            assert!(
                same_volume(Path::new(&unc), drive),
                "administrative share of the same volume"
            );
        }
        let cwd = std::env::current_dir().unwrap();
        let cwd_drive = cwd.to_str().unwrap().chars().next().unwrap();
        assert!(
            same_volume(Path::new("restored"), cwd_drive),
            "relative path lands on the current drive"
        );
        // a folder reached through a junction lives on the junction target's volume
        let link = cwd.join(format!("spacetree-junction-test-{}", std::process::id()));
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(tmp.path())
            .output()
            .is_ok_and(|o| o.status.success());
        if made {
            let r = same_volume(&link.join("sub"), drive);
            let _ = std::fs::remove_dir(&link);
            assert!(r, "junction to the source volume");
        }
    }

    /// Free records are full of stale bytes: a huge starting VCN must mark the runs bad, not overflow.
    #[test]
    fn garbage_start_vcn_and_compression_unit_are_contained() {
        let mut a = nonres(4096, &[0x11, 0x10, 0x05], AF_COMPRESSED);
        a[0x10..0x18].copy_from_slice(&(u64::MAX - 4).to_le_bytes());
        a[0x22..0x24].copy_from_slice(&15u16.to_le_bytes());
        let d = data_attr(&a).unwrap();
        assert!(d.bad_runs && d.runs.is_empty());
        assert!(
            d.unit <= 4,
            "NTFS compression units are 2^4 clusters; larger ones only allocate garbage"
        );
    }

    /// A run of zero clusters (allowed by the run-list format) made the preview panic on the GUI thread.
    #[test]
    fn preview_of_a_zero_length_run_does_not_panic() {
        let img = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(img.path(), vec![0x5Au8; 4096]).unwrap();
        let v = Volume {
            drive: 'Z',
            file: File::open(img.path()).unwrap(),
            bpc: 512,
        };
        let mut f = DeletedFile::carved(1, 0, "bin", 512);
        assert_eq!(f.runs[0].len, 0);
        assert!(v.head(&f, 512).is_empty());
        f.size = 100;
        assert_eq!(v.head(&f, 512), vec![0x5A; 100]);
    }

    fn nonres(real: u64, runs: &[u8], flags: u16) -> Vec<u8> {
        let mut a = nonresident_data(real, real, 0, flags, None);
        a.truncate(0x48);
        a.extend_from_slice(runs);
        a.push(0);
        while !a.len().is_multiple_of(8) {
            a.push(0);
        }
        let l = a.len() as u32;
        a[4..8].copy_from_slice(&l.to_le_bytes());
        a[0x20..0x22].copy_from_slice(&0x48u16.to_le_bytes());
        a
    }

    fn parse(rec: Vec<u8>, no: u64) -> Chunk {
        let mut out = Chunk::default();
        let mut r = rec;
        parse_free(&mut r, no, &mut out);
        out
    }

    #[test]
    fn live_files_are_ignored_and_deleted_ones_found() {
        let name = file_name_attr(5, "a.txt", 1);
        assert!(
            parse(record(1, 0, &[name.clone(), resident(0x80, b"hi")]), 30)
                .files
                .is_empty()
        );
        let c = parse(record(0, 0, &[name, resident(0x80, b"hi")]), 30);
        assert_eq!(c.files.len(), 1);
        assert_eq!(c.files[0].name, "a.txt");
        assert_eq!(c.files[0].data.as_ref().unwrap().resident.as_deref(), Some(&b"hi"[..]));
    }

    #[test]
    fn dos_name_loses_to_long_name_and_metadata_is_skipped() {
        let rec = record(
            0,
            0,
            &[
                file_name_attr(5, "LONGNA~1.TXT", 2),
                file_name_attr(5, "long name.txt", 1),
                resident(0x80, b"x"),
            ],
        );
        assert_eq!(parse(rec.clone(), 40).files[0].name, "long name.txt");
        assert!(parse(rec, 10).files.is_empty());
    }

    #[test]
    fn nonresident_runs_are_decoded_and_ranked_by_free_clusters() {
        // 4 clusters at LCN 16
        let rec = record(
            0,
            0,
            &[file_name_attr(5, "big.bin", 1), nonres(16384, &[0x11, 0x04, 0x10], 0)],
        );
        let f = parse(rec, 50).files.pop().unwrap();
        let d = f.data.unwrap();
        assert_eq!(
            d.runs,
            vec![Run {
                vcn: 0,
                len: 4,
                lcn: Some(16)
            }]
        );

        let mut bits = vec![0u8; 8]; // all free
        let map = ClusterMap(bits.clone());
        let (c, pct, size, ..) = assess(
            &Some(Data {
                runs: d.runs.clone(),
                ..d
            }),
            &[],
            &map,
            4096,
        );
        assert_eq!((c, pct, size), (Chance::Good, 100, 16384));

        bits[2] = 0b0000_0011; // LCN 16 and 17 reused
        let map = ClusterMap(bits.clone());
        let runs = vec![Run {
            vcn: 0,
            len: 4,
            lcn: Some(16),
        }];
        let data = Data {
            size: 16384,
            resident: None,
            start_vcn: 0,
            flags: 0,
            unit: 0,
            runs,
            bad_runs: false,
        };
        let (c, pct, ..) = assess(&Some(data), &[], &map, 4096);
        assert_eq!((c, pct), (Chance::Partial, 50));

        bits[2] = 0b0000_1111;
        let map = ClusterMap(bits);
        let runs = vec![Run {
            vcn: 0,
            len: 4,
            lcn: Some(16),
        }];
        let data = Data {
            size: 16384,
            resident: None,
            start_vcn: 0,
            flags: 0,
            unit: 0,
            runs,
            bad_runs: false,
        };
        assert_eq!(assess(&Some(data), &[], &map, 4096).0, Chance::Lost);
    }

    #[test]
    fn encrypted_is_unsupported_but_compressed_is_assessed() {
        let rec = record(
            0,
            0,
            &[
                file_name_attr(5, "c.bin", 1),
                nonres(100, &[0x11, 0x01, 0x10], AF_ENCRYPTED),
            ],
        );
        let f = parse(rec, 60).files.pop().unwrap();
        let (c, ..) = assess(&f.data, &[], &ClusterMap(vec![0; 8]), 4096);
        assert_eq!(c, Chance::Unsupported);

        let rec = record(
            0,
            0,
            &[
                file_name_attr(5, "z.bin", 1),
                nonres(100, &[0x11, 0x01, 0x10], AF_COMPRESSED),
            ],
        );
        let f = parse(rec, 61).files.pop().unwrap();
        let (c, ..) = assess(&f.data, &[], &ClusterMap(vec![0; 8]), 4096);
        assert_eq!(c, Chance::Good);
    }

    #[test]
    fn lznt1_literals_and_back_references() {
        // flags 0b10: literal 'a', then (offset 1, length 9); chunk header 0xB003 (4 bytes of data)
        let src = [0x03, 0xB0, 0x02, b'a', 0x06, 0x00];
        let mut out = Vec::new();
        lznt1_decompress(&src, &mut out);
        assert_eq!(out, b"aaaaaaaaaa");
        // stored chunk
        let mut out = Vec::new();
        lznt1_decompress(&[0x02, 0x30, b'x', b'y', b'z'], &mut out);
        assert_eq!(out, b"xyz");
    }

    #[test]
    fn unit_segments_skip_sparse_clusters() {
        let runs = [
            Run {
                vcn: 0,
                len: 5,
                lcn: Some(100),
            },
            Run {
                vcn: 5,
                len: 11,
                lcn: None,
            },
            Run {
                vcn: 16,
                len: 16,
                lcn: Some(300),
            },
        ];
        assert_eq!(unit_segments(&runs, 0, 16), vec![(100, 5)]);
        assert_eq!(unit_segments(&runs, 16, 16), vec![(300, 16)]);
        assert_eq!(unit_segments(&runs, 3, 4), vec![(103, 2)]);
    }

    fn deleted(rec: u64, mtime: u32, lcn: u64, len: u64) -> DeletedFile {
        let mut f = DeletedFile::carved(lcn, len * 4096, "bin", 4096);
        (f.rec, f.mtime, f.chance) = (rec, mtime, Chance::Good);
        f
    }

    #[test]
    fn overlapping_deleted_files_demote_the_older_one() {
        let mut files = vec![deleted(1, 100, 10, 8), deleted(2, 200, 14, 8), deleted(3, 50, 100, 4)];
        apply_overlaps(&mut files);
        assert_eq!((files[0].chance, files[0].intact), (Chance::Partial, 50)); // clusters 14..18 are taken
        assert_eq!(files[1].chance, Chance::Good);
        assert_eq!(files[2].chance, Chance::Good);
    }

    #[test]
    fn paths_follow_live_and_deleted_dirs_and_reject_reused_records() {
        let mut dirs = std::collections::HashMap::new();
        dirs.insert(
            30,
            DirRec {
                seq: 1,
                parent: 5,
                parent_seq: 5,
                name: "Users".into(),
                in_use: true,
            },
        );
        dirs.insert(
            31,
            DirRec {
                seq: 3,
                parent: 30,
                parent_seq: 1,
                name: "Old".into(),
                in_use: false,
            },
        );
        dirs.insert(
            32,
            DirRec {
                seq: 9,
                parent: 30,
                parent_seq: 1,
                name: "Reused".into(),
                in_use: true,
            },
        );
        let mut p = Paths {
            dirs,
            memo: Default::default(),
        };
        assert_eq!(p.dir_path(31, 2, 0).as_deref(), Some("Users\\Old"));
        assert_eq!(p.dir_path(5, 5, 0).as_deref(), Some(""));
        assert_eq!(p.dir_path(32, 1, 0), None); // the record now belongs to another directory
        assert_eq!(p.dir_path(99, 1, 0), None);
    }

    #[test]
    fn cluster_map_counts_free_clusters_across_byte_edges() {
        let map = ClusterMap(vec![0b1111_0000, 0x00, 0xFF]);
        assert_eq!(map.free_in(0, 4), 4);
        assert_eq!(map.free_in(2, 12), 2 + 6); // 2..4 free, 4..8 used, 8..14 free
        assert_eq!(map.free_in(16, 8), 0);
        assert_eq!(map.free_in(24, 8), 0); // past the bitmap counts as used
    }

    #[test]
    fn sanitize_and_unique_names() {
        assert_eq!(sanitize("a:b?.txt "), "a_b_.txt");
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.txt");
        std::fs::write(&p, b"1").unwrap();
        assert_eq!(unique(p).file_name().unwrap(), "x (2).txt");
    }
}
