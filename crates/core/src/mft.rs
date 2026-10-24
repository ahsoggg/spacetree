//! NTFS backend: reads `$MFT` straight from the raw volume (read-only) and rebuilds the tree
//! from parent references. Requires administrator rights.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{channel, sync_channel};
use std::time::Instant;

use rayon::prelude::*;

use crate::error::ScanError;
use crate::ntfs::{self, ATTR_ATTRIBUTE_LIST, ATTR_BITMAP, ATTR_DATA, ChunkOut, Run};
use crate::progress::{PHASE_BUILDING, Progress};
use crate::scan::Timings;
use crate::tree::Tree;
use crate::winapi;

const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_INVALID_FUNCTION: i32 = 1;
const SHARE_ALL: u32 = 0x1 | 0x2 | 0x4; // FILE_SHARE_READ | WRITE | DELETE
const CHUNK: u64 = 8 * 1024 * 1024;
/// Small tasks keep every core busy on each 8 MiB chunk (16 tasks per chunk); sized in bytes so that
/// volumes with 4 KiB records get as many tasks as those with 1 KiB records.
const TASK_BYTES: usize = 512 * 1024;
/// Buffers in flight: the reader fills up to this many chunks ahead of the parser.
const POOL_BUFFERS: usize = 4;
/// Runs of free records at least this long are not read; shorter ones are read through to stay sequential.
/// A spinning disk pays a seek for every skip, so it gets a long threshold; an SSD a short one.
fn skip_gap_bytes(drive: char) -> u64 {
    match crate::winapi::incurs_seek_penalty(std::path::Path::new(&format!("{drive}:\\"))) {
        Some(true) => 1024 * 1024,
        Some(false) => 128 * 1024,
        None => 256 * 1024,
    }
}
const MAX_ERROR_SAMPLES: usize = 200;

pub struct MftOutput {
    pub tree: Tree,
    pub errors: u64,
    pub error_samples: Vec<String>,
    pub extra_links: u64,
    pub timings: Timings,
    pub notes: Vec<String>,
    pub streams: Vec<ntfs::StreamInfo>,
    pub record_nodes: Vec<u32>,
    /// Journal position when the scan began: the live monitor replays the changes made since.
    pub usn: Option<ntfs::UsnPos>,
}

pub struct VolumeInfo {
    pub(crate) bytes_per_cluster: u64,
    pub(crate) record_size: usize,
    pub(crate) total_clusters: u64,
    mft_start_lcn: u64,
}

/// Opens `\\.\X:` read-only and checks that it is NTFS.
pub fn probe(drive: char) -> Result<(File, VolumeInfo), ScanError> {
    let path = format!(r"\\.\{drive}:");
    let file = OpenOptions::new()
        .read(true)
        .share_mode(SHARE_ALL)
        .open(&path)
        .map_err(|e| {
            if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) {
                ScanError::Unsupported(format!("нет прав администратора для чтения тома {drive}: ({e})"))
            } else {
                ScanError::Io(format!("не удалось открыть том {drive}: ({e})"))
            }
        })?;
    let v = winapi::ntfs_volume_data(&file).map_err(|e| {
        if e.raw_os_error() == Some(ERROR_INVALID_FUNCTION) {
            ScanError::Unsupported(format!("том {drive}: не NTFS"))
        } else {
            ScanError::Io(format!("FSCTL_GET_NTFS_VOLUME_DATA для {drive}: ({e})"))
        }
    })?;
    let (bpc, rs, lcn) = (
        v.BytesPerCluster as u64,
        v.BytesPerFileRecordSegment as usize,
        v.MftStartLcn,
    );
    if bpc == 0 || rs < 512 || !rs.is_power_of_two() || lcn <= 0 {
        return Err(ScanError::Ntfs("некорректные параметры тома".into()));
    }
    Ok((
        file,
        VolumeInfo {
            bytes_per_cluster: bpc,
            record_size: rs,
            total_clusters: v.TotalClusters.max(0) as u64,
            mft_start_lcn: lcn as u64,
        },
    ))
}

pub(crate) fn read_exact_at(file: &File, mut buf: &mut [u8], mut off: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let n = file.seek_read(buf, off)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf = &mut buf[n..];
        off += n as u64;
    }
    Ok(())
}

/// Reads `buf.len()` bytes of the $MFT data stream starting at virtual offset `voff`.
pub(crate) fn read_virtual(file: &File, runs: &[Run], bpc: u64, mut voff: u64, buf: &mut [u8]) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let vcn = voff / bpc;
        let run = runs
            .iter()
            .find(|r| vcn >= r.vcn && vcn < r.vcn + r.len)
            .ok_or_else(|| io::Error::other("смещение вне data runs $MFT"))?;
        let into = voff - run.vcn * bpc;
        let left_in_run = run.len.saturating_mul(bpc) - into;
        let n = left_in_run.min((buf.len() - done) as u64) as usize;
        let dst = &mut buf[done..done + n];
        match run.lcn {
            Some(l) => {
                let at = l
                    .checked_mul(bpc)
                    .and_then(|b| b.checked_add(into))
                    .ok_or_else(|| io::Error::other("смещение data run вне тома"))?;
                read_exact_at(file, dst, at)?
            }
            None => dst.fill(0),
        }
        done += n;
        voff += n as u64;
    }
    Ok(())
}

pub(crate) fn data_extents(attr: &[u8]) -> Result<(Vec<Run>, u64), ScanError> {
    if attr.len() < 0x40 || attr[8] == 0 {
        return Err(ScanError::Ntfs("$DATA файла $MFT резидентный".into()));
    }
    let start = u64::from_le_bytes(attr[0x10..0x18].try_into().unwrap());
    let real = u64::from_le_bytes(attr[0x30..0x38].try_into().unwrap());
    let roff = u16::from_le_bytes([attr[0x20], attr[0x21]]) as usize;
    let mut runs = ntfs::parse_runs(attr.get(roff..).unwrap_or(&[]))?;
    for r in &mut runs {
        r.vcn = r
            .vcn
            .checked_add(start)
            .filter(|v| v.checked_add(r.len).is_some())
            .ok_or_else(|| ScanError::Ntfs("повреждённый список data runs".into()))?;
    }
    Ok((runs, real))
}

/// Reads record 0 ($MFT itself) and returns the extents of its $DATA stream and its real size.
/// Extents of `$MFT`'s data, its real size, and the in-use bitmap when it could be read.
pub(crate) type MftLayout = (Vec<Run>, u64, Option<Vec<u8>>);

pub(crate) fn mft_extents(file: &File, info: &VolumeInfo) -> Result<MftLayout, ScanError> {
    let bpc = info.bytes_per_cluster;
    let rs = info.record_size;
    let mut block = vec![0u8; rs.max(4096)];
    let rec0_at = info
        .mft_start_lcn
        .checked_mul(bpc)
        .ok_or_else(|| ScanError::Ntfs("некорректные параметры тома".into()))?;
    read_exact_at(file, &mut block, rec0_at)?;
    if !ntfs::apply_fixups(&mut block[..rs]) {
        return Err(ScanError::Ntfs("запись 0 ($MFT) повреждена".into()));
    }
    let rec0 = &block[..rs];

    let first = ntfs::find_attr(rec0, ATTR_DATA).ok_or_else(|| ScanError::Ntfs("в $MFT нет атрибута $DATA".into()))?;
    let (mut runs, real_size) = data_extents(first)?;

    // A heavily fragmented $MFT spills its $DATA extents into extension records listed in $ATTRIBUTE_LIST.
    if let Some(list) = ntfs::find_attr(rec0, ATTR_ATTRIBUTE_LIST) {
        if list.len() < 0x18 {
            return Err(ScanError::Ntfs("битый $ATTRIBUTE_LIST".into()));
        }
        if list[8] != 0 {
            return Err(ScanError::Unsupported(
                "нерезидентный $ATTRIBUTE_LIST у $MFT не поддерживается".into(),
            ));
        }
        let coff = u16::from_le_bytes([list[0x14], list[0x15]]) as usize;
        let clen = u32::from_le_bytes(list[0x10..0x14].try_into().unwrap()) as usize;
        let content = list
            .get(coff..coff + clen)
            .ok_or_else(|| ScanError::Ntfs("битый $ATTRIBUTE_LIST".into()))?;
        let mut seen = vec![0u64];
        for (ty, _vcn, rec_no) in ntfs::parse_attribute_list(content) {
            if ty != ATTR_DATA || seen.contains(&rec_no) {
                continue;
            }
            seen.push(rec_no);
            runs.sort_by_key(|r| r.vcn);
            let mut ext = vec![0u8; rs];
            read_virtual(file, &runs, bpc, rec_no * rs as u64, &mut ext)?;
            if !ntfs::apply_fixups(&mut ext) {
                return Err(ScanError::Ntfs(format!("расширяющая запись {rec_no} $MFT повреждена")));
            }
            if let Some(a) = ntfs::find_attr(&ext, ATTR_DATA) {
                runs.extend(data_extents(a)?.0);
            }
        }
        runs.sort_by_key(|r| r.vcn);
    }
    runs.sort_by_key(|r| r.vcn);
    // The real size never exceeds what the runs allocate; a corrupt one would make the scan chase records that
    // do not exist (billions of chunk reads, each failing).
    let allocated = runs
        .iter()
        .map(|r| r.len.saturating_mul(bpc))
        .fold(0u64, u64::saturating_add);
    let bitmap = read_bitmap(file, rec0, bpc);
    Ok((runs, real_size.min(allocated), bitmap))
}

/// `$MFT`'s own `$BITMAP`: one bit per record, set when the record is in use. `None` when it cannot be read
/// (the scan then simply reads every record).
fn read_bitmap(file: &File, rec0: &[u8], bpc: u64) -> Option<Vec<u8>> {
    const MAX_BITMAP: u64 = 256 * 1024 * 1024;
    let attr = ntfs::find_attr(rec0, ATTR_BITMAP)?;
    if attr.len() < 0x18 {
        return None;
    }
    if attr[8] == 0 {
        let clen = u32::from_le_bytes(attr[0x10..0x14].try_into().unwrap()) as usize;
        let coff = u16::from_le_bytes([attr[0x14], attr[0x15]]) as usize;
        return attr.get(coff..coff.checked_add(clen)?).map(<[u8]>::to_vec);
    }
    let (runs, real) = data_extents(attr).ok()?;
    if real == 0 || real > MAX_BITMAP {
        return None;
    }
    // The volume is read unbuffered, so length and offset must be sector multiples: read whole clusters
    // (the runs cover whole clusters) and cut the tail off afterwards.
    let mut buf = vec![0u8; real.next_multiple_of(bpc) as usize];
    read_virtual(file, &runs, bpc, 0, &mut buf).ok()?;
    buf.truncate(real as usize);
    Some(buf)
}

/// `(first record, count)` ranges worth reading. Free stretches of at least `gap` records are skipped.
fn used_ranges(bitmap: Option<&[u8]>, total: u64, gap: u64) -> Vec<(u64, u64)> {
    let Some(bm) = bitmap else {
        return vec![(0, total)];
    };
    // Records past the end of the bitmap are treated as in use (read them rather than lose files).
    let used = |i: u64| bm.get((i / 8) as usize).is_none_or(|b| b >> (i % 8) & 1 != 0);
    let mut out: Vec<(u64, u64)> = Vec::new();
    let mut i = 0u64;
    while i < total {
        if !used(i) {
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i + 1; // one past the last used record seen
        i += 1;
        while i < total && i - end < gap {
            if used(i) {
                end = i + 1;
            }
            i += 1;
        }
        out.push((start, end - start));
    }
    out
}

pub fn scan(drive: char, progress: &Progress) -> Result<MftOutput, ScanError> {
    let t0 = Instant::now();
    let (file, info) = probe(drive)?;
    let usn = winapi::usn_query(&file).ok();
    let (runs, real_size, bitmap) = mft_extents(&file, &info)?;
    let (bpc, rs) = (info.bytes_per_cluster, info.record_size);
    let total_records = real_size / rs as u64;
    let ranges = used_ranges(bitmap.as_deref(), total_records, skip_gap_bytes(drive) / rs as u64);
    let to_read: u64 = ranges.iter().map(|r| r.1).sum();
    progress.total_units.store(to_read, Relaxed);
    let t_extents = t0.elapsed();

    let errors: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let note = |msg: String| {
        progress.errors.fetch_add(1, Relaxed);
        let mut v = errors.lock().unwrap();
        if v.len() < MAX_ERROR_SAMPLES {
            v.push(msg);
        }
    };

    let t1 = Instant::now();
    let (read_ns, parse_ns) = (AtomicU64::new(0), AtomicU64::new(0));
    let per_task = (TASK_BYTES / rs).max(1);
    let per_chunk = (CHUNK / rs as u64).max(1);
    // Chunk buffers circulate: the parser hands each one back to the reader once it is done with it.
    let (tx, rx) = sync_channel::<(u64, usize, Vec<u8>)>(POOL_BUFFERS);
    let (pool_tx, pool_rx) = channel::<Vec<u8>>();
    for _ in 0..POOL_BUFFERS {
        let _ = pool_tx.send(vec![0u8; (per_chunk as usize) * rs]);
    }
    let mut outs: Vec<ChunkOut> = Vec::new();
    std::thread::scope(|s| {
        let (file, runs, ranges, note, read_ns) = (&file, &runs, &ranges, &note, &read_ns);
        s.spawn(move || {
            'ranges: for &(start, count) in ranges {
                let (mut pos, end) = (start, start + count);
                while pos < end {
                    if progress.is_cancelled() {
                        break 'ranges;
                    }
                    let Ok(mut buf) = pool_rx.recv() else { break 'ranges };
                    let n = per_chunk.min(end - pos);
                    let slice = &mut buf[..n as usize * rs];
                    let t = Instant::now();
                    if let Err(e) = read_virtual(file, runs, bpc, pos * rs as u64, slice) {
                        note(format!("чтение $MFT, запись {pos}: {e}"));
                        slice.fill(0);
                    }
                    read_ns.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
                    if tx.send((pos, n as usize, buf)).is_err() {
                        break 'ranges;
                    }
                    pos += n;
                }
            }
            drop(tx); // closes the channel so the consumer loop ends
        });
        for (first, n, mut buf) in rx {
            if progress.is_cancelled() {
                break; // dropping `rx` stops the reader
            }
            let t = Instant::now();
            let parsed: Vec<ChunkOut> = buf[..n * rs]
                .par_chunks_mut(per_task * rs)
                .enumerate()
                .map(|(i, part)| {
                    let mut out = ChunkOut::default();
                    let base = first + (i * per_task) as u64;
                    for (j, rec) in part.chunks_exact_mut(rs).enumerate() {
                        ntfs::parse_record(rec, (base + j as u64) as u32, &mut out);
                    }
                    out
                })
                .collect();
            parse_ns.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            let named = parsed.iter().map(|c| c.bases.len() as u64).sum::<u64>();
            progress.files.fetch_add(named, Relaxed);
            progress.done_units.fetch_add(n as u64, Relaxed);
            outs.extend(parsed);
            let _ = pool_tx.send(buf);
        }
        drop(pool_tx); // unblocks the reader if the consumer stopped early
    });
    if progress.is_cancelled() {
        return Err(ScanError::Cancelled);
    }
    let t_pipeline = t1.elapsed();

    progress.phase.store(PHASE_BUILDING, Relaxed);
    let t2 = Instant::now();
    let built = ntfs::build_tree(outs, &format!("{drive}:\\"))?;
    let timings: Timings = vec![
        ("экстенты $MFT и битовая карта", t_extents),
        ("чтение и разбор (конвейер)", t_pipeline),
        (
            "  из них чтение (поток)",
            std::time::Duration::from_nanos(read_ns.load(Relaxed)),
        ),
        (
            "  из них разбор (поток)",
            std::time::Duration::from_nanos(parse_ns.load(Relaxed)),
        ),
        ("построение дерева", t2.elapsed()),
    ];
    let mb = |records: u64| (records * rs as u64) as f64 / 1e6;
    let read_s = read_ns.load(Relaxed) as f64 / 1e9;
    let notes = vec![format!(
        "битовая карта $MFT: {}; записей всего {total_records}, прочитано {to_read} ({:.0} МБ из {:.0} МБ), пропущено {}; чтение {:.0} МБ/с",
        if bitmap.is_some() {
            "прочитана"
        } else {
            "НЕ прочитана — читаются все записи"
        },
        mb(to_read),
        mb(total_records),
        total_records - to_read,
        if read_s > 0.0 { mb(to_read) / read_s } else { 0.0 },
    )];
    Ok(MftOutput {
        tree: built.tree,
        notes,
        errors: progress.errors.load(Relaxed),
        error_samples: errors.into_inner().unwrap(),
        extra_links: built.extra_links,
        timings,
        streams: built.streams,
        record_nodes: built.record_nodes,
        usn,
    })
}

#[cfg(test)]
mod volume_tests {
    use super::{VolumeInfo, mft_extents};
    use crate::ntfs::testutil::{nonresident_data, record};

    const BPC: u64 = 4096;

    /// A fake volume: record 0 of $MFT at cluster 1, nothing else.
    fn volume_with_record0(attrs: &[Vec<u8>]) -> (tempfile::NamedTempFile, VolumeInfo) {
        let mut img = vec![0u8; 4 * BPC as usize];
        let rec = record(0x01, 0, attrs);
        img[BPC as usize..BPC as usize + rec.len()].copy_from_slice(&rec);
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), &img).unwrap();
        let info = VolumeInfo {
            bytes_per_cluster: BPC,
            record_size: 1024,
            total_clusters: 4,
            mft_start_lcn: 1,
        };
        (f, info)
    }

    /// $DATA of $MFT: one cluster at LCN 1, claiming `real` bytes.
    fn mft_data(real: u64) -> Vec<u8> {
        let mut a = nonresident_data(real, BPC, 0, 0, None);
        a[0x48..0x4C].copy_from_slice(&[0x11, 0x01, 0x01, 0x00]);
        a
    }

    #[test]
    fn truncated_attribute_list_is_an_error_not_a_panic() {
        let mut list = vec![0u8; 0x10];
        list[0..4].copy_from_slice(&0x20u32.to_le_bytes());
        list[4..8].copy_from_slice(&0x10u32.to_le_bytes());
        let (f, info) = volume_with_record0(&[list, mft_data(BPC)]);
        let file = std::fs::File::open(f.path()).unwrap();
        assert!(mft_extents(&file, &info).is_err());
    }

    /// A corrupt real size must not make the scan walk 2^54 records that do not exist.
    #[test]
    fn real_size_is_capped_by_the_allocated_runs() {
        let (f, info) = volume_with_record0(&[mft_data(u64::MAX)]);
        let file = std::fs::File::open(f.path()).unwrap();
        let (runs, real, _) = mft_extents(&file, &info).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(real, BPC);
    }
}

#[cfg(test)]
mod tests {
    use super::used_ranges;

    #[test]
    fn no_bitmap_reads_everything() {
        assert_eq!(used_ranges(None, 100, 8), vec![(0, 100)]);
    }

    #[test]
    fn skips_long_gaps_and_bridges_short_ones() {
        // records: 0-1 used, 2-3 free (short gap), 4 used, 5..=20 free (long), 21 used
        let mut bm = vec![0u8; 4];
        for i in [0u32, 1, 4, 21] {
            bm[(i / 8) as usize] |= 1 << (i % 8);
        }
        assert_eq!(used_ranges(Some(&bm), 24, 8), vec![(0, 5), (21, 1)]);
    }

    #[test]
    fn records_past_the_bitmap_are_read() {
        let bm = [0b1u8];
        assert_eq!(used_ranges(Some(&bm), 20, 4), vec![(0, 1), (8, 12)]);
    }
}
