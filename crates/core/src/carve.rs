//! Signature carving ("deep scan"): finds files in free clusters by their headers when the `$MFT` record of a
//! deleted file is gone. Names and dates are lost; only contiguous files can be found.
//!
//! Phase 1 reads every free extent and looks at the first bytes of each cluster for a known signature. Phase 2
//! measures each hit (a header length field, or a footer search bounded by the next hit) and builds a
//! [`DeletedFile`] for it.

use std::collections::HashSet;
use std::fs::File;
use std::sync::atomic::Ordering::Relaxed;

use rayon::prelude::*;

use crate::error::ScanError;
use crate::mft;
use crate::progress::{PHASE_BUILDING, Progress};
use crate::undelete::{ClusterMap, DeletedFile, read_cluster_map};

const SCAN_BLOCK: u64 = 8 * 1024 * 1024;
const SEGMENT: u64 = 64 * 1024 * 1024;
const SEARCH_BLOCK: u64 = 4 * 1024 * 1024;
const SIG_LEN: usize = 32;
const MIN_SIZE: u64 = 1024;
const MAX_FOOTERS: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Jpg,
    Png,
    Pdf,
    Zip,
    Bmp,
    Riff,
    Mp4,
    Sqlite,
    SevenZ,
    Mkv,
}

fn signature(b: &[u8]) -> Option<Kind> {
    if b.len() < SIG_LEN {
        return None;
    }
    let be32 = |o: usize| u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let le32 = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    if b[..3] == [0xFF, 0xD8, 0xFF] && matches!(b[3], 0xE0..=0xEF | 0xDB | 0xFE | 0xC0 | 0xC4) {
        Some(Kind::Jpg)
    } else if b[..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] && &b[12..16] == b"IHDR" {
        Some(Kind::Png)
    } else if &b[..5] == b"%PDF-" && b[5].is_ascii_digit() {
        Some(Kind::Pdf)
    } else if b[..4] == [b'P', b'K', 3, 4] {
        Some(Kind::Zip)
    } else if &b[..2] == b"BM"
        && b[6..10] == [0; 4]
        && matches!(le32(14), 12 | 40 | 52 | 56 | 64 | 108 | 124)
        && le32(2) >= 26
    {
        Some(Kind::Bmp)
    } else if &b[..4] == b"RIFF" && matches!(&b[8..12], b"WAVE" | b"AVI " | b"WEBP") {
        Some(Kind::Riff)
    } else if &b[4..8] == b"ftyp" && (16..=64).contains(&be32(0)) {
        Some(Kind::Mp4)
    } else if &b[..16] == b"SQLite format 3\0" {
        Some(Kind::Sqlite)
    } else if b[..6] == [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C] {
        Some(Kind::SevenZ)
    } else if b[..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        Some(Kind::Mkv)
    } else {
        None
    }
}

impl Kind {
    /// Longest file of this kind worth looking for.
    fn max_bytes(self) -> u64 {
        match self {
            Kind::Jpg | Kind::Png => 256 << 20,
            Kind::Pdf => 1 << 30,
            Kind::Zip => 4 << 30,
            // The length comes from the header, so only the extent limits it (videos can be 100 GB).
            _ => 1 << 40,
        }
    }

    /// The header gives the exact length: the file may run past other signatures found inside it.
    fn sized(self) -> bool {
        !matches!(self, Kind::Jpg | Kind::Png | Kind::Pdf | Kind::Zip)
    }
}

/// Where the bytes come from; the volume in production, memory in tests.
trait Source: Sync {
    fn bpc(&self) -> u64;
    /// `n` bytes of the data starting `off` bytes after cluster `lcn` (`off` is a multiple of the cluster size).
    /// May return fewer bytes at the end of the volume; `None` on a read error.
    fn read(&self, lcn: u64, off: u64, n: u64) -> Option<Vec<u8>>;

    /// Any byte range, not necessarily aligned.
    fn read_at(&self, lcn: u64, byte: u64, n: u64) -> Option<Vec<u8>> {
        let bpc = self.bpc();
        let start = byte / bpc * bpc;
        let data = self.read(lcn, start, (byte - start + n).next_multiple_of(bpc))?;
        let skip = (byte - start) as usize;
        Some(
            data.get(skip..)
                .unwrap_or(&[])
                .iter()
                .take(n as usize)
                .copied()
                .collect(),
        )
    }
}

struct VolumeSource<'a> {
    file: &'a File,
    bpc: u64,
    volume_bytes: u64,
}

impl Source for VolumeSource<'_> {
    fn bpc(&self) -> u64 {
        self.bpc
    }

    fn read(&self, lcn: u64, off: u64, n: u64) -> Option<Vec<u8>> {
        let abs = lcn * self.bpc + off;
        let len = n.next_multiple_of(self.bpc).min(self.volume_bytes.saturating_sub(abs));
        if len == 0 {
            return Some(Vec::new());
        }
        let mut buf = vec![0u8; len as usize];
        mft::read_exact_at(self.file, &mut buf, abs).ok()?;
        buf.truncate(n as usize);
        Some(buf)
    }
}

/// Runs of consecutive free clusters as `(first, length)`.
fn free_extents(map: &ClusterMap, total: u64) -> Vec<(u64, u64)> {
    let limit = total.min(map.0.len() as u64 * 8);
    let (mut out, mut start, mut i) = (Vec::new(), None::<u64>, 0u64);
    while i < limit {
        if i % 8 == 0 && i + 8 <= limit {
            match map.0[(i / 8) as usize] {
                0x00 => {
                    start.get_or_insert(i);
                    i += 8;
                    continue;
                }
                0xFF => {
                    if let Some(s) = start.take() {
                        out.push((s, i - s));
                    }
                    i += 8;
                    continue;
                }
                _ => {}
            }
        }
        if map.is_free(i) {
            start.get_or_insert(i);
        } else if let Some(s) = start.take() {
            out.push((s, i - s));
        }
        i += 1;
    }
    if let Some(s) = start {
        out.push((s, limit - s));
    }
    out
}

/// Occurrences of `pat` in the first `bound` bytes of the file at `lcn`, up to `limit` of them.
fn find_all(src: &dyn Source, lcn: u64, bound: u64, pat: &[u8], limit: usize) -> Vec<u64> {
    let bpc = src.bpc();
    let block = (SEARCH_BLOCK / bpc).max(1) * bpc;
    let (mut found, mut carry, mut off) = (Vec::new(), Vec::<u8>::new(), 0u64);
    while off < bound {
        let Some(data) = src.read(lcn, off, block.min(bound - off)) else {
            break;
        };
        if data.is_empty() {
            break;
        }
        let base = off - carry.len() as u64;
        let mut buf = std::mem::take(&mut carry);
        buf.extend_from_slice(&data);
        let mut i = 0;
        while let Some(p) = buf[i..].iter().position(|&b| b == pat[0]) {
            let at = i + p;
            if buf.len() - at >= pat.len() && buf[at..at + pat.len()] == *pat {
                found.push(base + at as u64);
                if found.len() >= limit {
                    return found;
                }
            }
            i = at + 1;
        }
        carry = buf[buf.len().saturating_sub(pat.len() - 1)..].to_vec();
        off += data.len() as u64;
    }
    found
}

/// Picks the end of a file among footer hits: the first one followed by zeros up to the end of its cluster
/// (a thumbnail inside an EXIF block is followed by more data), else the last hit.
fn pick_end(src: &dyn Source, lcn: u64, hits: &[(u64, u64)]) -> Option<u64> {
    let bpc = src.bpc();
    for &(_, end) in hits {
        let rem = end.next_multiple_of(bpc) - end;
        if rem == 0 || src.read_at(lcn, end, rem).is_some_and(|t| t.iter().all(|&b| b == 0)) {
            return Some(end);
        }
    }
    hits.last().map(|h| h.1)
}

fn zip_ext(src: &dyn Source, lcn: u64, eocd: u64, zip_end: u64) -> &'static str {
    let head = src.read_at(lcn, eocd, 22).unwrap_or_default();
    if head.len() < 22 {
        return "zip";
    }
    let size = u64::from(u32::from_le_bytes(head[12..16].try_into().unwrap())).min(4 << 20);
    let off = u64::from(u32::from_le_bytes(head[16..20].try_into().unwrap()));
    if off + size > zip_end {
        return "zip";
    }
    let cd = src.read_at(lcn, off, size).unwrap_or_default();
    let has = |s: &[u8]| cd.windows(s.len()).any(|w| w == s);
    if has(b"word/") {
        "docx"
    } else if has(b"xl/") {
        "xlsx"
    } else if has(b"ppt/") {
        "pptx"
    } else if has(b"AndroidManifest.xml") {
        "apk"
    } else if has(b"META-INF/MANIFEST.MF") {
        "jar"
    } else {
        "zip"
    }
}

/// `(size in bytes, extension)` of the file of `kind` at `lcn`, which cannot be longer than `bound` bytes.
fn measure(src: &dyn Source, kind: Kind, lcn: u64, bound: u64) -> Option<(u64, &'static str)> {
    let bound = bound.min(kind.max_bytes());
    let head = src.read(lcn, 0, src.bpc().max(64))?;
    let le32 = |o: usize| {
        head.get(o..o + 4)
            .map(|b| u64::from(u32::from_le_bytes(b.try_into().unwrap())))
    };
    let be32 = |o: usize| {
        head.get(o..o + 4)
            .map(|b| u64::from(u32::from_be_bytes(b.try_into().unwrap())))
    };
    let fit = |n: u64| (MIN_SIZE..=bound).contains(&n).then_some(n);
    match kind {
        Kind::Bmp => fit(le32(2)?).map(|n| (n, "bmp")),
        Kind::Riff => {
            let ext = match head.get(8..12)? {
                b"WAVE" => "wav",
                b"AVI " => "avi",
                _ => "webp",
            };
            fit(le32(4)? + 8).map(|n| (n, ext))
        }
        Kind::Sqlite => {
            let page = match u64::from(u16::from_be_bytes([*head.get(16)?, *head.get(17)?])) {
                1 => 65536,
                p => p,
            };
            fit(page * be32(28)?).map(|n| (n, "sqlite"))
        }
        Kind::Mkv => {
            // EBML header element, then the Segment element whose size covers the rest of the file.
            let vint = |at: usize| -> Option<(usize, Option<u64>)> {
                let first = *head.get(at)?;
                let len = first.leading_zeros() as usize + 1;
                if len > 8 {
                    return None;
                }
                let mut v = u64::from(first) & (0xFF >> len);
                let mut all_ones = v == (0xFF >> len);
                for b in head.get(at + 1..at + len)? {
                    v = v << 8 | u64::from(*b);
                    all_ones &= *b == 0xFF;
                }
                Some((len, (!all_ones).then_some(v)))
            };
            let (l, size) = vint(4)?;
            let seg = 4 + l + usize::try_from(size?).ok()?;
            if head.get(seg..seg + 4)? != [0x18, 0x53, 0x80, 0x67] {
                return None;
            }
            let (l2, size2) = vint(seg + 4)?; // None = unknown size (live recording): cannot be measured
            let ext = if head[..seg].windows(4).any(|w| w == b"webm") {
                "webm"
            } else {
                "mkv"
            };
            fit(seg as u64 + 4 + l2 as u64 + size2?).map(|n| (n, ext))
        }
        Kind::SevenZ => {
            let next = u64::from_le_bytes(head.get(12..20)?.try_into().ok()?);
            let len = u64::from_le_bytes(head.get(20..28)?.try_into().ok()?);
            fit(32u64.checked_add(next)?.checked_add(len)?).map(|n| (n, "7z"))
        }
        Kind::Mp4 => {
            let (mut off, mut core) = (0u64, false);
            // Saturating: a corrupt 64-bit box size must end the walk, not wrap the offset back to the start.
            while off.saturating_add(16) <= bound {
                let b = src.read_at(lcn, off, 16)?;
                let mut size = u64::from(u32::from_be_bytes(b[..4].try_into().unwrap()));
                let ty = &b[4..8];
                if !ty.iter().all(|c| c.is_ascii_alphanumeric() || *c == b' ' || *c == 0xA9) {
                    break;
                }
                core |= matches!(ty, b"moov" | b"mdat");
                if size == 1 {
                    size = u64::from_be_bytes(b[8..16].try_into().unwrap());
                } else if size == 0 {
                    off = bound;
                    break;
                }
                if size < 8 {
                    break;
                }
                off = off.saturating_add(size);
            }
            (core).then(|| off.min(bound)).and_then(fit).map(|n| (n, "mp4"))
        }
        Kind::Png => {
            let hits: Vec<(u64, u64)> = find_all(src, lcn, bound, b"IEND", 1)
                .into_iter()
                .map(|p| (p, p + 8))
                .collect();
            fit(hits.first()?.1).map(|n| (n, "png"))
        }
        Kind::Jpg => {
            let hits: Vec<(u64, u64)> = find_all(src, lcn, bound, &[0xFF, 0xD9], MAX_FOOTERS)
                .into_iter()
                .map(|p| (p, p + 2))
                .collect();
            fit(pick_end(src, lcn, &hits)?).map(|n| (n, "jpg"))
        }
        Kind::Pdf => {
            let hits: Vec<(u64, u64)> = find_all(src, lcn, bound, b"%%EOF", MAX_FOOTERS)
                .into_iter()
                .map(|p| (p, p + 5))
                .collect();
            fit(pick_end(src, lcn, &hits)?).map(|n| (n, "pdf"))
        }
        Kind::Zip => {
            let mut hits = Vec::new();
            for p in find_all(src, lcn, bound, &[b'P', b'K', 5, 6], MAX_FOOTERS) {
                let tail = src.read_at(lcn, p, 22)?;
                if tail.len() == 22 {
                    let comment = u64::from(u16::from_le_bytes([tail[20], tail[21]]));
                    hits.push((p, p + 22 + comment));
                }
            }
            let end = pick_end(src, lcn, &hits)?;
            let eocd = hits.iter().find(|h| h.1 == end)?.0;
            fit(end).map(|n| (n, zip_ext(src, lcn, eocd, end)))
        }
    }
}

/// Scans the free space of `drive`. `known` are first clusters of files already listed from the `$MFT`.
pub fn carve(drive: char, known: &HashSet<u64>, progress: &Progress) -> Result<Vec<DeletedFile>, ScanError> {
    let (file, info) = mft::probe(drive)?;
    let (runs, _, _) = mft::mft_extents(&file, &info)?;
    let map = read_cluster_map(&file, &runs, &info)?;
    let bpc = info.bytes_per_cluster;
    let src = VolumeSource {
        file: &file,
        bpc,
        volume_bytes: info.total_clusters * bpc,
    };
    let extents = free_extents(&map, info.total_clusters);
    progress.total_units.store(extents.iter().map(|e| e.1).sum(), Relaxed);
    progress.done_units.store(0, Relaxed);

    // (first cluster, clusters, end of the extent it belongs to)
    let segments: Vec<(u64, u64, u64)> = extents
        .iter()
        .flat_map(|&(s, n)| {
            let step = (SEGMENT / bpc).max(1);
            (0..n.div_ceil(step)).map(move |k| (s + k * step, step.min(n - k * step), s + n))
        })
        .collect();
    let spinning = crate::winapi::incurs_seek_penalty(std::path::Path::new(&format!("{drive}:\\"))) != Some(false);
    let scan_segment = |&(start, len, ext_end): &(u64, u64, u64)| -> Vec<(u64, Kind, u64)> {
        let mut hits = Vec::new();
        let per_block = (SCAN_BLOCK / bpc).max(1);
        let mut done = 0;
        while done < len && !progress.is_cancelled() {
            let n = per_block.min(len - done);
            if let Some(buf) = src.read(start + done, 0, n * bpc) {
                for (i, c) in buf.chunks(bpc as usize).enumerate() {
                    if let Some(k) = signature(c.get(..SIG_LEN).unwrap_or(c)) {
                        hits.push((start + done + i as u64, k, ext_end));
                    }
                }
            } else {
                progress.errors.fetch_add(1, Relaxed);
            }
            done += n;
            progress.done_units.fetch_add(n, Relaxed);
        }
        progress.files.fetch_add(hits.len() as u64, Relaxed);
        hits
    };
    let mut candidates: Vec<(u64, Kind, u64)> = if spinning {
        segments.iter().flat_map(scan_segment).collect()
    } else {
        segments.par_iter().flat_map_iter(scan_segment).collect()
    };
    if progress.is_cancelled() {
        return Err(ScanError::Cancelled);
    }
    candidates.sort_unstable_by_key(|c| c.0);
    candidates.dedup_by_key(|c| c.0);

    progress.phase.store(PHASE_BUILDING, Relaxed);
    let bounds: Vec<u64> = (0..candidates.len())
        .map(|i| {
            let (lcn, kind, ext_end) = candidates[i];
            let next = if kind.sized() {
                ext_end
            } else {
                candidates.get(i + 1).map_or(ext_end, |n| n.0.min(ext_end))
            };
            (next - lcn) * bpc
        })
        .collect();
    let mut found: Vec<DeletedFile> = candidates
        .par_iter()
        .zip(bounds.par_iter())
        .filter(|(c, _)| !known.contains(&c.0) && !progress.is_cancelled())
        .filter_map(|(&(lcn, kind, _), &bound)| {
            let (size, ext) = measure(&src, kind, lcn, bound)?;
            Some(DeletedFile::carved(lcn, size, ext, bpc))
        })
        .collect();
    if progress.is_cancelled() {
        return Err(ScanError::Cancelled);
    }
    found.sort_by_key(|f| f.rec);
    // A signature inside an already measured file (a cover picture in a video) is part of that file.
    let mut until = 0;
    found.retain(|f| {
        let keep = f.rec >= until;
        if keep {
            until = f.rec + f.size.div_ceil(bpc);
        }
        keep
    });
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mem(Vec<u8>);

    impl Source for Mem {
        fn bpc(&self) -> u64 {
            512
        }
        fn read(&self, lcn: u64, off: u64, n: u64) -> Option<Vec<u8>> {
            let start = (lcn * 512 + off) as usize;
            let end = (start + n.next_multiple_of(512) as usize).min(self.0.len());
            Some(
                self.0
                    .get(start..end)
                    .unwrap_or(&[])
                    .iter()
                    .take(n as usize)
                    .copied()
                    .collect(),
            )
        }
    }

    fn padded(mut v: Vec<u8>, to: usize) -> Vec<u8> {
        v.resize(to, 0);
        v
    }

    #[test]
    fn signatures_need_more_than_the_magic() {
        let mut jpg = vec![0xFF, 0xD8, 0xFF, 0xE0];
        jpg.resize(32, 0);
        assert_eq!(signature(&jpg), Some(Kind::Jpg));
        jpg[3] = 0x00;
        assert_eq!(signature(&jpg), None);
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
        png.extend(b"IHDR");
        png.resize(32, 0);
        assert_eq!(signature(&png), Some(Kind::Png));
        assert_eq!(signature(&[0u8; 32]), None);
    }

    /// A 64-bit box size near u64::MAX made the offset wrap around to 0: the deep scan looped forever.
    #[test]
    fn mp4_box_size_overflow_terminates() {
        let mut d = vec![0, 0, 0, 16];
        d.extend(b"ftypisom");
        d.extend([0u8; 4]);
        d.extend([0, 0, 0, 1]);
        d.extend(b"mdat");
        d.extend(0xFFFF_FFFF_FFFF_FFF0u64.to_be_bytes());
        let m = Mem(padded(d, 4096));
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(measure(&m, Kind::Mp4, 0, 4096));
        });
        let r = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("measure(Mp4) must not loop forever");
        assert!(r.is_none_or(|(n, _)| n <= 4096));
    }

    #[test]
    fn jpeg_end_skips_the_thumbnail_marker() {
        let mut d = vec![0xFF, 0xD8, 0xFF, 0xE1];
        d.extend(vec![7u8; 1500]);
        d.extend([0xFF, 0xD9, 5, 5, 5]); // thumbnail end, more data follows in the cluster
        d.extend(vec![9u8; 600]);
        let end = d.len() + 2;
        d.extend([0xFF, 0xD9]); // real end, then zero padding
        let m = Mem(padded(d, 4096));
        let (size, ext) = measure(&m, Kind::Jpg, 0, 4096).unwrap();
        assert_eq!((size, ext), (end as u64, "jpg"));
    }

    #[test]
    fn png_ends_after_the_iend_crc() {
        let mut d = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
        d.extend(b"IHDR");
        d.extend(vec![1u8; 1200]);
        d.extend([0, 0, 0, 0]);
        d.extend(b"IEND");
        d.extend([0xAE, 0x42, 0x60, 0x82]);
        let len = d.len() as u64;
        let m = Mem(padded(d, 4096));
        assert_eq!(measure(&m, Kind::Png, 0, 4096), Some((len, "png")));
    }

    #[test]
    fn header_length_formats_respect_the_bound() {
        let mut bmp = vec![b'B', b'M'];
        bmp.extend(3000u32.to_le_bytes());
        bmp.extend([0; 4]);
        bmp.extend(54u32.to_le_bytes());
        bmp.extend(40u32.to_le_bytes());
        let m = Mem(padded(bmp, 8192));
        assert_eq!(signature(&m.0[..32]), Some(Kind::Bmp));
        assert_eq!(measure(&m, Kind::Bmp, 0, 4096), Some((3000, "bmp")));
        assert_eq!(measure(&m, Kind::Bmp, 0, 2048), None);
    }

    #[test]
    fn zip_is_classified_by_its_central_directory() {
        let mut d = b"PK\x03\x04".to_vec();
        d.extend(vec![1u8; 1100]);
        let cd_off = d.len() as u32;
        let mut cd = b"PK\x01\x02".to_vec();
        cd.extend(vec![0u8; 42]);
        cd.extend(b"word/document.xml");
        d.extend(&cd);
        d.extend(b"PK\x05\x06");
        d.extend([0u8; 4]);
        d.extend([1, 0, 1, 0]);
        d.extend((cd.len() as u32).to_le_bytes());
        d.extend(cd_off.to_le_bytes());
        d.extend([0, 0]);
        let len = d.len() as u64;
        let m = Mem(padded(d, 4096));
        assert_eq!(measure(&m, Kind::Zip, 0, 4096), Some((len, "docx")));
    }

    #[test]
    fn mkv_length_comes_from_the_segment_size() {
        let mut d = vec![0x1A, 0x45, 0xDF, 0xA3, 0x84, b'w', b'e', b'b', b'm']; // EBML header, 4 bytes of content
        d.extend([0x18, 0x53, 0x80, 0x67, 0x01, 0, 0, 0, 0, 0, 0x10, 0x00]); // Segment, 8-byte size = 4096
        let total = d.len() as u64 + 4096;
        let m = Mem(padded(d, 8192));
        assert_eq!(signature(&m.0[..32]), Some(Kind::Mkv));
        assert_eq!(measure(&m, Kind::Mkv, 0, 1 << 20), Some((total, "webm")));
    }

    #[test]
    fn free_extents_merge_across_bytes() {
        let map = ClusterMap(vec![0b0000_0011, 0x00, 0xFF, 0b1111_1100]);
        assert_eq!(free_extents(&map, 32), vec![(2, 14), (24, 2)]);
        assert_eq!(free_extents(&map, 20), vec![(2, 14)]);
    }

    #[test]
    fn search_finds_patterns_across_block_edges() {
        let mut d = vec![0u8; 8 * 1024 * 1024];
        let at = SEARCH_BLOCK as usize - 1;
        d[at] = 0xFF;
        d[at + 1] = 0xD9;
        let m = Mem(d);
        assert_eq!(find_all(&m, 0, 8 << 20, &[0xFF, 0xD9], 4), vec![at as u64]);
    }
}
