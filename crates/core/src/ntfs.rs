//! Platform-independent NTFS $MFT record parser and tree builder.
//!
//! Format references (offsets verified against these pages):
//! - FILE record header / flags: <https://flatcap.github.io/linux-ntfs/ntfs/concepts/file_record.html>
//! - Fixup (update sequence): <https://flatcap.github.io/linux-ntfs/ntfs/concepts/fixup.html>
//! - Attribute header (resident / non-resident): <https://flatcap.github.io/linux-ntfs/ntfs/concepts/attribute_header.html>
//! - $STANDARD_INFORMATION: <https://flatcap.github.io/linux-ntfs/ntfs/attributes/standard_information.html>
//! - $FILE_NAME: <https://flatcap.github.io/linux-ntfs/ntfs/attributes/file_name.html>
//! - Namespaces: <https://flatcap.github.io/linux-ntfs/ntfs/concepts/filename_namespace.html>
//! - Data runs: <https://flatcap.github.io/linux-ntfs/ntfs/concepts/data_runs.html>
//! - $ATTRIBUTE_LIST: <https://flatcap.github.io/linux-ntfs/ntfs/attributes/attribute_list.html>

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use rayon::prelude::*;

use crate::error::ScanError;
use crate::tree::{Kind, NO_PARENT, Node, ROOT, Tree};

pub const ATTR_STANDARD_INFORMATION: u32 = 0x10;
pub const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
pub const ATTR_FILE_NAME: u32 = 0x30;
pub const ATTR_DATA: u32 = 0x80;
pub const ATTR_BITMAP: u32 = 0xB0;
const ATTR_END: u32 = 0xFFFF_FFFF;

const FLAG_IN_USE: u16 = 0x01;
const FLAG_DIR: u16 = 0x02;
/// Attribute flags: compressed / sparse (attribute header 0x0C).
const ATTR_FLAG_COMPRESSED: u16 = 0x0001;
const ATTR_FLAG_SPARSE: u16 = 0x8000;

const NS_DOS: u8 = 2;
const FRN_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
/// MFT record number of the root directory (`.`).
pub const ROOT_RECORD: u32 = 5;
/// Fixup stride is always 512 bytes regardless of the sector size.
const FIXUP_STRIDE: usize = 512;

pub const B_DIR: u8 = 1;
pub const B_HAS_NAME: u8 = 2;
pub const B_HAS_DATA: u8 = 4;

pub(crate) fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
pub(crate) fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
pub(crate) fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Undoes the multi-sector transfer protection. Returns false for torn/corrupt records.
pub fn apply_fixups(rec: &mut [u8]) -> bool {
    if rec.len() < 0x30 || &rec[0..4] != b"FILE" {
        return false;
    }
    let usa_off = u16_at(rec, 4) as usize;
    let usa_cnt = u16_at(rec, 6) as usize;
    if usa_cnt == 0 || usa_off + usa_cnt * 2 > rec.len() || (usa_cnt - 1) * FIXUP_STRIDE > rec.len() {
        return false;
    }
    let usn = [rec[usa_off], rec[usa_off + 1]];
    for i in 1..usa_cnt {
        let end = i * FIXUP_STRIDE;
        if rec[end - 2..end] != usn {
            return false;
        }
        rec[end - 2] = rec[usa_off + 2 * i];
        rec[end - 1] = rec[usa_off + 2 * i + 1];
    }
    true
}

/// Base record (or merged base) info needed for the tree.
#[derive(Clone, Copy, Debug)]
pub struct BaseRec {
    pub rec: u32,
    pub parent: u32,
    pub name_off: u32,
    pub name_len: u16,
    pub flags: u8,
    /// Additional non-DOS names (hard links) that are not counted.
    pub extra_links: u8,
    pub size: u64,
    pub alloc: u64,
    /// Last modification from $STANDARD_INFORMATION, Unix seconds.
    pub mtime: u32,
}

/// Attributes found in an extension record that belong to `base`.
#[derive(Clone, Copy, Debug)]
pub struct ExtRec {
    pub base: u32,
    pub parent: u32,
    pub name_off: u32,
    pub name_len: u16,
    pub flags: u8,
    pub size: u64,
    pub alloc: u64,
}

#[derive(Default)]
pub struct ChunkOut {
    pub bases: Vec<BaseRec>,
    pub exts: Vec<ExtRec>,
    pub names: String,
}

struct Found {
    parent: u32,
    name_off: u32,
    name_len: u16,
    extra_links: u8,
    has_name: bool,
    size: u64,
    alloc: u64,
    has_data: bool,
    mtime: u32,
}

/// Parses one FILE record (fixups are applied in place). `rec_no` is the record's index in $MFT.
pub fn parse_record(buf: &mut [u8], rec_no: u32, out: &mut ChunkOut) {
    // Free records are the majority on many volumes; the flags word lies before the first sector tail,
    // so it can be checked before (and instead of) applying the fixups.
    if buf.len() < 0x30 || &buf[0..4] != b"FILE" || u16_at(buf, 0x16) & FLAG_IN_USE == 0 {
        return;
    }
    if !apply_fixups(buf) {
        return;
    }
    let flags = u16_at(buf, 0x16);
    let base_ref = u64_at(buf, 0x20) & FRN_MASK;
    let used = (u32_at(buf, 0x18) as usize).min(buf.len());
    let mut off = u16_at(buf, 0x14) as usize;

    let mut f = Found {
        parent: 0,
        name_off: 0,
        name_len: 0,
        extra_links: 0,
        has_name: false,
        size: 0,
        alloc: 0,
        has_data: false,
        mtime: 0,
    };

    while off + 8 <= used {
        let ty = u32_at(buf, off);
        // Attributes are stored in ascending type order; nothing after $DATA is needed.
        if ty == ATTR_END || ty > ATTR_DATA {
            break;
        }
        let len = u32_at(buf, off + 4) as usize;
        if len < 0x10 || !len.is_multiple_of(8) || off + len > buf.len() {
            break;
        }
        let attr = &buf[off..off + len];
        match ty {
            ATTR_STANDARD_INFORMATION if attr[8] == 0 => parse_standard_info(attr, &mut f),
            ATTR_FILE_NAME if attr[8] == 0 => parse_file_name(attr, out, &mut f),
            ATTR_DATA if attr[9] == 0 => parse_data(attr, &mut f),
            _ => {}
        }
        off += len;
    }

    let mut bflags = 0;
    if flags & FLAG_DIR != 0 {
        bflags |= B_DIR;
    }
    if f.has_name {
        bflags |= B_HAS_NAME;
    }
    if f.has_data {
        bflags |= B_HAS_DATA;
    }
    if base_ref == 0 {
        out.bases.push(BaseRec {
            rec: rec_no,
            parent: f.parent,
            name_off: f.name_off,
            name_len: f.name_len,
            flags: bflags,
            extra_links: f.extra_links,
            size: f.size,
            alloc: f.alloc,
            mtime: f.mtime,
        });
    } else if (f.has_name || f.has_data) && base_ref <= u32::MAX as u64 {
        out.exts.push(ExtRec {
            base: base_ref as u32,
            parent: f.parent,
            name_off: f.name_off,
            name_len: f.name_len,
            flags: bflags,
            size: f.size,
            alloc: f.alloc,
        });
    }
}

/// `attr` is a resident $STANDARD_INFORMATION attribute; takes the "file altered" time (content offset 0x08).
fn parse_standard_info(attr: &[u8], f: &mut Found) {
    if attr.len() < 0x18 {
        return;
    }
    let coff = u16_at(attr, 0x14) as usize;
    if u32_at(attr, 0x10) < 0x10 || coff + 0x10 > attr.len() {
        return;
    }
    let ft = u64_at(attr, coff + 0x08) as i64;
    f.mtime = (ft / 10_000_000 - 11_644_473_600).clamp(0, u32::MAX as i64) as u32;
}

/// `attr` is a resident $FILE_NAME attribute.
fn parse_file_name(attr: &[u8], out: &mut ChunkOut, f: &mut Found) {
    if attr.len() < 0x18 {
        return;
    }
    let clen = u32_at(attr, 0x10) as usize;
    let coff = u16_at(attr, 0x14) as usize;
    if coff + 0x42 > attr.len() || clen < 0x42 {
        return;
    }
    let c = &attr[coff..];
    let nchars = c[0x40] as usize;
    let ns = c[0x41];
    if ns == NS_DOS || 0x42 + nchars * 2 > c.len() || nchars == 0 {
        return;
    }
    if f.has_name {
        f.extra_links = f.extra_links.saturating_add(1);
        return;
    }
    let before = out.names.len();
    let units = c[0x42..0x42 + nchars * 2]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| u16::from_le_bytes(*p));
    out.names
        .extend(char::decode_utf16(units).map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER)));
    f.parent = (u64_at(c, 0) & FRN_MASK).min(u32::MAX as u64) as u32;
    f.name_off = before as u32;
    f.name_len = (out.names.len() - before) as u16;
    f.has_name = true;
}

/// `attr` is an unnamed $DATA attribute.
fn parse_data(attr: &[u8], f: &mut Found) {
    if attr[8] == 0 {
        if attr.len() >= 0x18 && !f.has_data {
            f.size = u32_at(attr, 0x10) as u64;
            f.alloc = 0; // resident: stored inside the MFT record
            f.has_data = true;
        }
        return;
    }
    // Non-resident: sizes are only valid in the first extent (starting VCN 0).
    if attr.len() < 0x40 || u64_at(attr, 0x10) != 0 || f.has_data {
        return;
    }
    let aflags = u16_at(attr, 0x0C);
    f.size = u64_at(attr, 0x30);
    f.alloc = u64_at(attr, 0x28);
    // Compressed/sparse streams carry the on-disk size at 0x40 (linux-ntfs ATTR_RECORD.compressed_size).
    if aflags & (ATTR_FLAG_COMPRESSED | ATTR_FLAG_SPARSE) != 0 && u16_at(attr, 0x22) != 0 && attr.len() >= 0x48 {
        f.alloc = u64_at(attr, 0x40);
    }
    f.has_data = true;
}

/// One extent of a non-resident attribute. `lcn == None` is a sparse run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub vcn: u64,
    pub len: u64,
    pub lcn: Option<u64>,
}

/// Decodes a data-run list (bytes starting at the "data runs offset").
pub fn parse_runs(mut b: &[u8]) -> Result<Vec<Run>, ScanError> {
    let bad = || ScanError::Ntfs("повреждённый список data runs".into());
    let (mut runs, mut vcn, mut lcn) = (Vec::new(), 0u64, 0i64);
    while let Some(&hdr) = b.first() {
        if hdr == 0 {
            break;
        }
        let (ls, os) = ((hdr & 0x0F) as usize, (hdr >> 4) as usize);
        if ls == 0 || ls > 8 || os > 8 || b.len() < 1 + ls + os {
            return Err(bad());
        }
        let mut len = 0u64;
        for (i, &x) in b[1..1 + ls].iter().enumerate() {
            len |= (x as u64) << (8 * i);
        }
        let run_lcn = if os == 0 {
            None
        } else {
            let o = &b[1 + ls..1 + ls + os];
            let mut v = 0i64;
            for (i, &x) in o.iter().enumerate() {
                v |= (x as i64) << (8 * i);
            }
            if o[os - 1] & 0x80 != 0 && os < 8 {
                v |= -1i64 << (8 * os); // sign-extend
            }
            lcn = lcn.checked_add(v).ok_or_else(bad)?;
            if lcn < 0 {
                return Err(bad());
            }
            Some(lcn as u64)
        };
        runs.push(Run { vcn, len, lcn: run_lcn });
        // Runs end at or before u64::MAX, so `vcn + len` never overflows for any returned run.
        vcn = vcn.checked_add(len).ok_or_else(bad)?;
        b = &b[1 + ls + os..];
    }
    Ok(runs)
}

/// Entries of a (resident or already loaded) $ATTRIBUTE_LIST: `(attribute type, starting VCN, base record number)`.
pub fn parse_attribute_list(mut b: &[u8]) -> Vec<(u32, u64, u64)> {
    let mut v = Vec::new();
    while b.len() >= 0x1A {
        let elen = u16_at(b, 4) as usize;
        if elen < 0x1A || elen > b.len() {
            break;
        }
        v.push((u32_at(b, 0), u64_at(b, 8), u64_at(b, 0x10) & FRN_MASK));
        b = &b[elen..];
    }
    v
}

/// Locates the attribute `ty` (unnamed) in an already fixed-up record; returns its bytes.
pub fn find_attr(rec: &[u8], ty: u32) -> Option<&[u8]> {
    let used = (u32_at(rec, 0x18) as usize).min(rec.len());
    let mut off = u16_at(rec, 0x14) as usize;
    while off + 8 <= used {
        let t = u32_at(rec, off);
        if t == ATTR_END {
            break;
        }
        let len = u32_at(rec, off + 4) as usize;
        if len < 0x10 || !len.is_multiple_of(8) || off + len > rec.len() {
            break;
        }
        if t == ty && rec[off + 9] == 0 {
            return Some(&rec[off..off + len]);
        }
        off += len;
    }
    None
}

/// Result of merging all chunks into a tree.
pub struct Built {
    pub tree: Tree,
    pub extra_links: u64,
}

const NONE: u32 = u32::MAX;
const ORPHANS: &str = "[без родителя]";

/// Merges parsed chunks into the final tree. `root_name` names the root directory (e.g. `C:\`).
pub fn build_tree(chunks: Vec<ChunkOut>, root_name: &str) -> Result<Built, ScanError> {
    // Sizes are known up front: the big vectors are allocated once instead of regrown (and copied) per chunk,
    // and the per-chunk name offsets are shifted in parallel.
    let mut chunks = chunks;
    let mut shifts = Vec::with_capacity(chunks.len());
    let (mut name_total, mut base_total, mut ext_total) = (0usize, 0usize, 0usize);
    for c in &chunks {
        shifts.push(name_total as u32);
        name_total += c.names.len();
        base_total += c.bases.len();
        ext_total += c.exts.len();
    }
    chunks.par_iter_mut().zip(shifts).for_each(|(c, shift)| {
        if shift != 0 {
            for b in &mut c.bases {
                b.name_off += shift;
            }
            for e in &mut c.exts {
                e.name_off += shift;
            }
        }
    });
    // room for the two names appended below (root and orphan folder)
    let mut names = String::with_capacity(name_total + 64);
    let mut bases: Vec<BaseRec> = Vec::with_capacity(base_total);
    let mut exts: Vec<ExtRec> = Vec::with_capacity(ext_total);
    for mut c in chunks {
        names.push_str(&c.names);
        bases.append(&mut c.bases);
        exts.append(&mut c.exts);
    }

    let max_rec = bases.iter().map(|b| b.rec).max().unwrap_or(0) as usize;
    let mut slot = vec![NONE; max_rec + 1];
    for (i, b) in bases.iter().enumerate() {
        slot[b.rec as usize] = i as u32;
    }
    for e in &exts {
        let Some(&i) = slot.get(e.base as usize).filter(|&&i| i != NONE) else {
            continue;
        };
        let b = &mut bases[i as usize];
        if e.flags & B_HAS_NAME != 0 && b.flags & B_HAS_NAME == 0 {
            (b.parent, b.name_off, b.name_len) = (e.parent, e.name_off, e.name_len);
            b.flags |= B_HAS_NAME;
        }
        if e.flags & B_HAS_DATA != 0 && b.flags & B_HAS_DATA == 0 {
            (b.size, b.alloc) = (e.size, e.alloc);
            b.flags |= B_HAS_DATA;
        }
    }

    let root_idx = slot.get(ROOT_RECORD as usize).copied().filter(|&i| i != NONE);
    let Some(root_idx) = root_idx.filter(|&i| bases[i as usize].flags & B_DIR != 0) else {
        return Err(ScanError::Ntfs("в $MFT не найден корневой каталог (запись 5)".into()));
    };

    // node id assignment: root = 0, then every named base in order.
    let mut node_of = vec![NONE; max_rec + 1];
    node_of[ROOT_RECORD as usize] = 0;
    let mut order: Vec<u32> = Vec::with_capacity(bases.len());
    order.push(root_idx);
    for (i, b) in bases.iter().enumerate() {
        if b.flags & B_HAS_NAME != 0 && b.rec != ROOT_RECORD {
            node_of[b.rec as usize] = order.len() as u32;
            order.push(i as u32);
        }
    }

    let root_off = names.len() as u32;
    names.push_str(root_name);
    let orphan_off = names.len() as u32;
    names.push_str(ORPHANS);

    let mut nodes: Vec<Node> = Vec::with_capacity(order.len() + 1);
    nodes.push(Node::new(NO_PARENT, root_off, root_name.len() as u16, Kind::Dir, 0, 0));
    let orphan_id = order.len() as u32;
    let has_orphans = AtomicBool::new(false);
    let extra_links: u64 = order[1..].par_iter().map(|&bi| bases[bi as usize].extra_links as u64).sum();
    nodes.par_extend(order[1..].par_iter().with_min_len(8192).map(|&bi| {
        let b = &bases[bi as usize];
        let is_dir = b.flags & B_DIR != 0;
        let parent = match node_of.get(b.parent as usize).copied() {
            Some(p) if p != NONE && b.parent != b.rec && bases[order[p as usize] as usize].flags & B_DIR != 0 => p,
            _ => {
                if !has_orphans.load(Relaxed) {
                    has_orphans.store(true, Relaxed);
                }
                orphan_id
            }
        };
        let (size, alloc) = if is_dir { (0, 0) } else { (b.size, b.alloc) };
        let kind = if is_dir { Kind::Dir } else { Kind::File };
        let mut node = Node::new(parent, b.name_off, b.name_len, kind, size, alloc);
        node.mtime = b.mtime;
        node
    }));
    let has_orphans = has_orphans.into_inner();
    // The orphan node is always created so that its id (== nodes.len()) is stable; it is dropped when unused.
    if has_orphans {
        nodes.push(Node::new(ROOT, orphan_off, ORPHANS.len() as u16, Kind::Dir, 0, 0));
    }
    debug_assert_eq!(ROOT, 0);
    Ok(Built {
        tree: Tree::from_parts(nodes, names),
        extra_links,
    })
}

#[doc(hidden)]
pub mod testutil {
    //! Builder for synthetic FILE records (1024 bytes, 2 sectors, fixups applied); public only for the
    //! benchmark example and tests.

    pub fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
    }

    fn align8(v: &mut Vec<u8>) {
        while !v.len().is_multiple_of(8) {
            v.push(0);
        }
    }

    pub fn file_name_attr(parent: u64, name: &str, ns: u8) -> Vec<u8> {
        let mut c = vec![0u8; 0x42];
        c[0..8].copy_from_slice(&(parent | (1 << 48)).to_le_bytes());
        c[0x40] = name.encode_utf16().count() as u8;
        c[0x41] = ns;
        c.extend(utf16(name));
        resident(0x30, &c)
    }

    pub fn resident(ty: u32, content: &[u8]) -> Vec<u8> {
        let mut a = vec![0u8; 0x18];
        a[0..4].copy_from_slice(&ty.to_le_bytes());
        a[0x10..0x14].copy_from_slice(&(content.len() as u32).to_le_bytes());
        a[0x14..0x16].copy_from_slice(&0x18u16.to_le_bytes());
        a.extend_from_slice(content);
        align8(&mut a);
        let l = a.len() as u32;
        a[4..8].copy_from_slice(&l.to_le_bytes());
        a
    }

    pub fn nonresident_data(real: u64, alloc: u64, start_vcn: u64, flags: u16, compressed: Option<u64>) -> Vec<u8> {
        let mut a = vec![0u8; 0x48];
        a[0..4].copy_from_slice(&0x80u32.to_le_bytes());
        a[8] = 1;
        a[0x0C..0x0E].copy_from_slice(&flags.to_le_bytes());
        a[0x10..0x18].copy_from_slice(&start_vcn.to_le_bytes());
        a[0x20..0x22].copy_from_slice(&0x48u16.to_le_bytes());
        a[0x28..0x30].copy_from_slice(&alloc.to_le_bytes());
        a[0x30..0x38].copy_from_slice(&real.to_le_bytes());
        if let Some(c) = compressed {
            a[0x22..0x24].copy_from_slice(&4u16.to_le_bytes());
            a[0x40..0x48].copy_from_slice(&c.to_le_bytes());
        }
        a.push(0); // empty run list
        align8(&mut a);
        let l = a.len() as u32;
        a[4..8].copy_from_slice(&l.to_le_bytes());
        a
    }

    pub fn record(flags: u16, base: u64, attrs: &[Vec<u8>]) -> Vec<u8> {
        let mut r = vec![0u8; 1024];
        r[0..4].copy_from_slice(b"FILE");
        r[4..6].copy_from_slice(&0x30u16.to_le_bytes());
        r[6..8].copy_from_slice(&3u16.to_le_bytes());
        r[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes());
        r[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
        r[0x20..0x28].copy_from_slice(&base.to_le_bytes());
        let mut off = 0x38;
        for a in attrs {
            r[off..off + a.len()].copy_from_slice(a);
            off += a.len();
        }
        r[off..off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        off += 8;
        r[0x18..0x1C].copy_from_slice(&(off as u32).to_le_bytes());
        r[0x1C..0x20].copy_from_slice(&1024u32.to_le_bytes());
        // fixups: USN 0xBEEF, store the real sector tails, then stamp the USN.
        r[0x30..0x32].copy_from_slice(&0xBEEFu16.to_le_bytes());
        for i in 1..3 {
            let end = i * 512;
            let (a, b) = (r[end - 2], r[end - 1]);
            r[0x30 + 2 * i] = a;
            r[0x30 + 2 * i + 1] = b;
            r[end - 2..end].copy_from_slice(&0xBEEFu16.to_le_bytes());
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    fn parse_all(recs: Vec<Vec<u8>>) -> Vec<ChunkOut> {
        let mut out = ChunkOut::default();
        for (i, mut r) in recs.into_iter().enumerate() {
            parse_record(&mut r, i as u32, &mut out);
        }
        vec![out]
    }

    #[test]
    fn fixups_restore_sector_tails_and_reject_torn_records() {
        let mut r = record(1, 0, &[]);
        // original bytes at the sector ends were saved into the USA by `record`
        r[510] = 0x11;
        r[511] = 0x22; // not USN -> torn
        assert!(!apply_fixups(&mut r));

        let mut r = record(1, 0, &[]);
        assert!(apply_fixups(&mut r));
        assert_eq!(&r[510..512], &[0, 0]);
    }

    #[test]
    fn decodes_data_runs_with_negative_and_sparse() {
        // 11 30 60 | 21 10 00 01 -> wait: run2 len 0x10, off 0x0100; | 11 20 E0 -> off -0x20 | 01 05 sparse
        let b = [
            0x11, 0x30, 0x60, 0x21, 0x10, 0x00, 0x01, 0x11, 0x20, 0xE0, 0x01, 0x05, 0x00,
        ];
        let r = parse_runs(&b).unwrap();
        assert_eq!(
            r[0],
            Run {
                vcn: 0,
                len: 0x30,
                lcn: Some(0x60)
            }
        );
        assert_eq!(
            r[1],
            Run {
                vcn: 0x30,
                len: 0x10,
                lcn: Some(0x160)
            }
        );
        assert_eq!(
            r[2],
            Run {
                vcn: 0x40,
                len: 0x20,
                lcn: Some(0x140)
            }
        );
        assert_eq!(
            r[3],
            Run {
                vcn: 0x60,
                len: 5,
                lcn: None
            }
        );
    }

    /// Corrupt run lists (deleted records, damaged volumes) must be rejected, not overflow the VCN counter.
    #[test]
    fn run_list_with_overflowing_lengths_is_rejected() {
        // two sparse runs of 2^64-1 clusters each: the second one would start past u64::MAX
        let mut b = vec![0x08];
        b.extend([0xFF; 8]);
        b.push(0x08);
        b.extend([0xFF; 8]);
        b.push(0);
        assert!(parse_runs(&b).is_err());
        // a single run ending exactly at u64::MAX is still representable
        let mut one = vec![0x08];
        one.extend([0xFF; 8]);
        one.push(0);
        assert_eq!(parse_runs(&one).unwrap().len(), 1);
    }

    #[test]
    fn attribute_list_entries() {
        let mut e = vec![0u8; 0x20];
        e[0..4].copy_from_slice(&0x80u32.to_le_bytes());
        e[4..6].copy_from_slice(&0x20u16.to_le_bytes());
        e[8..16].copy_from_slice(&7u64.to_le_bytes());
        e[0x10..0x18].copy_from_slice(&(42u64 | (3 << 48)).to_le_bytes());
        assert_eq!(parse_attribute_list(&e), vec![(0x80, 7, 42)]);
    }

    #[test]
    fn builds_tree_with_dos_names_hardlinks_extensions_and_orphans() {
        let root_ref = 5u64;
        let mut recs: Vec<Vec<u8>> = (0..16).map(|_| vec![0u8; 1024]).collect();
        recs[5] = record(0x03, 0, &[file_name_attr(5, ".", 1)]);
        // 16: dir "docs", modified 2021-01-01T00:00:00Z
        let mut si = vec![0u8; 0x30];
        si[8..16].copy_from_slice(&((1_609_459_200u64 + 11_644_473_600) * 10_000_000).to_le_bytes());
        recs.push(record(
            0x03,
            0,
            &[resident(0x10, &si), file_name_attr(root_ref, "docs", 1)],
        ));
        // 17: file with DOS + Win32 names, resident data 10 bytes
        recs.push(record(
            0x01,
            0,
            &[
                file_name_attr(16, "LONGNA~1.TXT", 2),
                file_name_attr(16, "long name ü.txt", 1),
                resident(0x80, &[1u8; 10]),
            ],
        ));
        // 18: hard-linked file (two Win32 links), non-resident 5000 bytes / 8192 allocated
        recs.push(record(
            0x01,
            0,
            &[
                file_name_attr(16, "a.bin", 1),
                file_name_attr(root_ref, "a-link.bin", 1),
                nonresident_data(5000, 8192, 0, 0, None),
            ],
        ));
        // 19: file whose name is in extension record 20 and data in extension 21
        recs.push(record(0x01, 0, &[]));
        recs.push(record(
            0x01,
            19 | (1 << 48),
            &[file_name_attr(root_ref, "split.dat", 1)],
        ));
        recs.push(record(0x01, 19, &[nonresident_data(777, 4096, 0, 0, None)]));
        // 22: sparse/compressed: on-disk size from 0x40
        recs.push(record(
            0x01,
            0,
            &[
                file_name_attr(root_ref, "sparse.img", 1),
                nonresident_data(1_000_000, 1_000_000, 0, 0x8000, Some(65536)),
            ],
        ));
        // 23: orphan (parent record 999 does not exist)
        recs.push(record(
            0x01,
            0,
            &[file_name_attr(999, "lost.txt", 1), resident(0x80, &[0u8; 3])],
        ));
        // 24: not in use
        recs.push(record(0x00, 0, &[file_name_attr(root_ref, "deleted.txt", 1)]));

        let built = build_tree(parse_all(recs), "X:\\").unwrap();
        let t = &built.tree;
        let (size, alloc, files) = t.root_stats();
        assert_eq!(files, 5, "long name, a.bin, split.dat, sparse.img, lost.txt");
        assert_eq!(size, 10 + 5000 + 777 + 1_000_000 + 3);
        assert_eq!(alloc, 8192 + 4096 + 65536);
        assert_eq!(built.extra_links, 1);
        let docs = (0..t.len() as u32).find(|&i| t.name(i) == "docs").unwrap();
        assert_eq!(t.node(docs).mtime, 1_609_459_200);
        assert_eq!(
            t.node(0).mtime,
            1_609_459_200,
            "directories carry the latest mtime of their subtree"
        );
        let paths: Vec<String> = (0..t.len() as u32).map(|i| t.path(i)).collect();
        let sep = std::path::MAIN_SEPARATOR;
        assert!(paths.contains(&format!("X:\\docs{sep}long name ü.txt")));
        assert!(paths.contains(&format!("X:\\docs{sep}a.bin")));
        assert!(paths.contains(&"X:\\split.dat".to_string()));
        assert!(paths.contains(&format!("X:\\[без родителя]{sep}lost.txt")));
        assert!(
            !paths
                .iter()
                .any(|p| p.contains("LONGNA") || p.contains("deleted") || p.contains("a-link"))
        );
    }
}
