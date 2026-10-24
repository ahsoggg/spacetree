//! Live monitoring through the NTFS change journal (`$UsnJrnl`): which files were created, deleted, renamed or
//! changed size since the scan, without rescanning. Read-only; needs the same administrator rights as the MFT scan.
//!
//! The monitor thread polls the journal, merges the records of a poll per file (a log file that is appended to
//! a thousand times is one event) and asks the file system for the file's current size, so the consumer gets
//! ready-made [`Change`]s and never touches the disk itself.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use crate::winapi;

pub use crate::ntfs::UsnPos;

pub const REASON_DATA_OVERWRITE: u32 = 0x0000_0001;
pub const REASON_DATA_EXTEND: u32 = 0x0000_0002;
pub const REASON_DATA_TRUNCATION: u32 = 0x0000_0004;
pub const REASON_FILE_CREATE: u32 = 0x0000_0100;
pub const REASON_FILE_DELETE: u32 = 0x0000_0200;
pub const REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;

/// Only these records are requested from the journal.
const REASON_MASK: u32 = REASON_DATA_OVERWRITE
    | REASON_DATA_EXTEND
    | REASON_DATA_TRUNCATION
    | REASON_FILE_CREATE
    | REASON_FILE_DELETE
    | REASON_RENAME_NEW_NAME;

const ATTR_DIRECTORY: u32 = 0x10;
const FRN_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
const POLL: Duration = Duration::from_millis(400);
/// While the journal is busy (a build, an unpacked archive) its records are merged per file for at most this
/// long and then handed over, so the consumer gets steady batches instead of one huge one at the end.
const FLUSH_EVERY: Duration = Duration::from_millis(250);
/// Distinct files merged per poll; more activity than this is counted as dropped, not buffered without bound.
const MAX_BATCH: usize = 20_000;
const READ_BUF: usize = 256 * 1024;
const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_INVALID_PARAMETER: i32 = 87;

/// The part of a USN_RECORD_V2 that matters here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// Full file reference number (record number in the low 48 bits, sequence number above).
    pub frn: u64,
    pub parent_frn: u64,
    pub reason: u32,
    pub attrs: u32,
    pub name: String,
}

/// Parses the output of FSCTL_READ_USN_JOURNAL: `(next usn, records)`. Records of other versions and malformed
/// ones are skipped; nothing in `buf` can make this read out of bounds.
pub fn parse_records(buf: &[u8]) -> (i64, Vec<Record>) {
    let Some(next) = buf.first_chunk::<8>().map(|b| i64::from_le_bytes(*b)) else {
        return (0, Vec::new());
    };
    let mut out = Vec::new();
    let mut off = 8usize;
    while let Some(head) = buf.get(off..off + 4) {
        let len = u32::from_le_bytes(head.try_into().unwrap()) as usize;
        if len < 60 || !len.is_multiple_of(8) || off + len > buf.len() {
            break; // a broken length cannot be stepped over
        }
        let rec = &buf[off..off + len];
        off += len;
        if u16::from_le_bytes([rec[4], rec[5]]) != 2 {
            continue;
        }
        let u32_at = |o: usize| u32::from_le_bytes(rec[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(rec[o..o + 8].try_into().unwrap());
        let (nlen, noff) = (
            u16::from_le_bytes([rec[56], rec[57]]) as usize,
            u16::from_le_bytes([rec[58], rec[59]]) as usize,
        );
        let Some(raw) = rec.get(noff..noff + nlen).filter(|_| nlen.is_multiple_of(2)) else {
            continue;
        };
        let units = raw.as_chunks::<2>().0.iter().map(|p| u16::from_le_bytes(*p));
        out.push(Record {
            frn: u64_at(8),
            parent_frn: u64_at(16),
            reason: u32_at(40),
            attrs: u32_at(52),
            name: char::decode_utf16(units)
                .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
                .collect(),
        });
    }
    (next, out)
}

/// What happened to one file (all its journal records of a poll merged).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    pub parent_frn: u64,
    pub name: String,
    pub attrs: u32,
    pub reasons: u32,
}

/// Merges the records of a poll per file: reasons add up, the latest name and parent win.
#[derive(Default)]
pub struct Batch {
    files: HashMap<u64, Pending>,
    pub dropped: u64,
}

impl Batch {
    pub fn add(&mut self, r: Record) {
        if self.files.len() >= MAX_BATCH && !self.files.contains_key(&r.frn) {
            self.dropped += 1;
            return;
        }
        let e = self.files.entry(r.frn).or_insert(Pending {
            parent_frn: r.parent_frn,
            name: String::new(),
            attrs: 0,
            reasons: 0,
        });
        e.reasons |= r.reason;
        e.attrs = r.attrs;
        e.parent_frn = r.parent_frn;
        e.name = r.name;
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn drain(&mut self) -> Vec<(u64, Pending)> {
        self.files.drain().collect()
    }
}

/// MFT record number of a file reference number.
pub fn record_of(frn: u64) -> u64 {
    frn & FRN_MASK
}

/// A change to apply to the tree. File reference numbers are full ones; use [`record_of`] to look nodes up.
#[derive(Clone, Debug)]
pub enum Change {
    /// A file changed size (`name` is its current name, to catch a reused record number).
    Resized {
        frn: u64,
        name: String,
        size: u64,
        alloc: u64,
    },
    Created {
        frn: u64,
        parent_frn: u64,
        name: String,
        is_dir: bool,
        size: u64,
        alloc: u64,
    },
    Renamed {
        frn: u64,
        parent_frn: u64,
        name: String,
        is_dir: bool,
        size: u64,
        alloc: u64,
    },
    Deleted {
        frn: u64,
        name: String,
    },
    /// Monitoring stopped (journal overwritten, deleted, volume gone): the tree can no longer be kept current.
    Lost(String),
}

#[derive(Default)]
pub struct Stats {
    /// Distinct files seen changing since monitoring started (per poll).
    pub events: AtomicU64,
    /// Files whose changes were not reported because a poll had more than `MAX_BATCH` of them.
    pub dropped: AtomicU64,
}

pub struct Monitor {
    pub rx: Receiver<Change>,
    pub stats: Arc<Stats>,
    stop: Arc<AtomicBool>,
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed); // the thread notices within one poll interval and exits
    }
}

/// Turns the pending changes of one file into a [`Change`] by looking at the file as it is now; `None` when it
/// cannot be told (no access) and is better ignored than guessed.
fn classify(volume: &std::fs::File, frn: u64, p: Pending) -> Option<Change> {
    if p.reasons & REASON_FILE_DELETE != 0 {
        return Some(Change::Deleted { frn, name: p.name });
    }
    let info = match winapi::file_by_id(volume, frn) {
        Ok(i) if i.delete_pending => return Some(Change::Deleted { frn, name: p.name }),
        Ok(i) => i,
        Err(e) if matches!(e.raw_os_error(), Some(ERROR_FILE_NOT_FOUND | ERROR_INVALID_PARAMETER)) => {
            return Some(Change::Deleted { frn, name: p.name });
        }
        Err(_) => return None,
    };
    let is_dir = info.is_dir || p.attrs & ATTR_DIRECTORY != 0;
    let (size, alloc) = if is_dir { (0, 0) } else { (info.size, info.alloc) };
    Some(if p.reasons & REASON_FILE_CREATE != 0 {
        Change::Created {
            frn,
            parent_frn: p.parent_frn,
            name: p.name,
            is_dir,
            size,
            alloc,
        }
    } else if p.reasons & REASON_RENAME_NEW_NAME != 0 {
        Change::Renamed {
            frn,
            parent_frn: p.parent_frn,
            name: p.name,
            is_dir,
            size,
            alloc,
        }
    } else if is_dir {
        return None; // folders only change through their contents
    } else {
        Change::Resized {
            frn,
            name: p.name,
            size,
            alloc,
        }
    })
}

/// Starts watching the journal of `drive`. With `resume` (the position noted when the scan began) the changes
/// since then are replayed first; it is an error when the journal no longer reaches back that far. `wake` is
/// called whenever changes are queued.
pub fn start(drive: char, resume: Option<UsnPos>, wake: impl Fn() + Send + 'static) -> Result<Monitor, String> {
    let volume = OpenOptions::new()
        .read(true)
        .share_mode(0x1 | 0x2 | 0x4)
        .open(format!(r"\\.\{drive}:"))
        .map_err(|e| format!("не удалось открыть том {drive}: ({e})"))?;
    let now = winapi::usn_query(&volume).map_err(|e| match e.raw_os_error() {
        Some(1179) => format!("на томе {drive}: не включён журнал USN"),
        _ => format!("журнал USN тома {drive}: ({e})"),
    })?;
    let mut next = match resume {
        Some(p) if p.id == now.id && p.next_usn >= now.lowest_valid_usn => p.next_usn,
        Some(_) => return Err("журнал USN изменился с момента сканирования — пересканируйте диск (F5)".into()),
        None => now.next_usn,
    };
    let journal_id = now.id;
    let stats = Arc::new(Stats::default());
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = channel();
    let (st, sp) = (stats.clone(), stop.clone());
    std::thread::spawn(move || watch(volume, journal_id, &mut next, &tx, &st, &sp, &wake));
    Ok(Monitor { rx, stats, stop })
}

fn watch(
    volume: std::fs::File,
    journal_id: u64,
    next: &mut i64,
    tx: &Sender<Change>,
    stats: &Stats,
    stop: &AtomicBool,
    wake: &dyn Fn(),
) {
    let mut buf = vec![0u8; READ_BUF];
    while !stop.load(Relaxed) {
        let mut batch = Batch::default();
        let started = Instant::now();
        // Whether the journal was read up to its end; a busy one is flushed part by part and read on at once,
        // the kernel's ring buffer must not wrap around while this thread is classifying files.
        let caught_up = loop {
            let n = match winapi::usn_read(&volume, journal_id, *next, REASON_MASK, &mut buf) {
                Ok(n) => n,
                Err(e) => {
                    let _ = tx.send(Change::Lost(lost_reason(&e)));
                    wake();
                    return;
                }
            };
            let (nx, records) = parse_records(&buf[..n]);
            if nx <= *next {
                break true;
            }
            *next = nx;
            for r in records {
                batch.add(r);
            }
            if stop.load(Relaxed) {
                return;
            }
            if started.elapsed() >= FLUSH_EVERY || batch.len() >= MAX_BATCH / 2 {
                break false;
            }
        };
        stats.dropped.fetch_add(batch.dropped, Relaxed);
        stats.events.fetch_add(batch.len() as u64, Relaxed);
        let mut sent = false;
        for (frn, pending) in batch.drain() {
            if let Some(change) = classify(&volume, frn, pending) {
                if tx.send(change).is_err() {
                    return; // the consumer is gone
                }
                sent = true;
            }
        }
        if sent {
            wake();
        }
        if !caught_up {
            continue;
        }
        let mut waited = Duration::ZERO;
        while waited < POLL && !stop.load(Relaxed) {
            std::thread::sleep(Duration::from_millis(50));
            waited += Duration::from_millis(50);
        }
    }
}

fn lost_reason(e: &io::Error) -> String {
    match e.raw_os_error() {
        Some(1181) => "журнал USN перезаписан — изменения потеряны, пересканируйте диск (F5)".into(),
        Some(1178 | 1179) => "журнал USN отключён".into(),
        _ => format!("чтение журнала USN прервалось: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A USN_RECORD_V2 as NTFS lays it out.
    fn record(frn: u64, parent: u64, reason: u32, attrs: u32, name: &str) -> Vec<u8> {
        let n: Vec<u8> = name.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let len = (60 + n.len()).next_multiple_of(8);
        let mut r = vec![0u8; len];
        r[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        r[4..6].copy_from_slice(&2u16.to_le_bytes());
        r[8..16].copy_from_slice(&frn.to_le_bytes());
        r[16..24].copy_from_slice(&parent.to_le_bytes());
        r[40..44].copy_from_slice(&reason.to_le_bytes());
        r[52..56].copy_from_slice(&attrs.to_le_bytes());
        r[56..58].copy_from_slice(&(n.len() as u16).to_le_bytes());
        r[58..60].copy_from_slice(&60u16.to_le_bytes());
        r[60..60 + n.len()].copy_from_slice(&n);
        r
    }

    fn output(next: i64, recs: &[Vec<u8>]) -> Vec<u8> {
        let mut b = next.to_le_bytes().to_vec();
        recs.iter().for_each(|r| b.extend_from_slice(r));
        b
    }

    #[test]
    fn parses_records_and_the_next_usn() {
        let buf = output(
            4096,
            &[
                record(7 | (3 << 48), 5, REASON_DATA_EXTEND, 0x20, "log.txt"),
                record(9, 5, REASON_FILE_CREATE, ATTR_DIRECTORY, "новая папка"),
            ],
        );
        let (next, recs) = parse_records(&buf);
        assert_eq!(next, 4096);
        assert_eq!(recs.len(), 2);
        assert_eq!(
            (recs[0].name.as_str(), recs[0].frn, recs[0].parent_frn),
            ("log.txt", 7 | (3 << 48), 5)
        );
        assert_eq!(record_of(recs[0].frn), 7);
        assert_eq!(recs[1].name, "новая папка");
        assert_eq!(recs[1].attrs & ATTR_DIRECTORY, ATTR_DIRECTORY);
    }

    #[test]
    fn malformed_buffers_are_survived() {
        assert_eq!(parse_records(&[]), (0, vec![]));
        assert_eq!(parse_records(&[1, 2, 3]).1.len(), 0);
        // header only
        assert_eq!(parse_records(&7i64.to_le_bytes()), (7, vec![]));
        // length pointing past the end / too short / not a multiple of 8
        for bad_len in [10_000u32, 8, 61, 0, u32::MAX] {
            let mut b = output(1, &[record(1, 2, 0, 0, "a")]);
            b[8..12].copy_from_slice(&bad_len.to_le_bytes());
            assert!(parse_records(&b).1.is_empty(), "{bad_len}");
        }
        // name outside the record, odd name length, unknown version
        let mut r = record(1, 2, 0, 0, "abc");
        r[58..60].copy_from_slice(&200u16.to_le_bytes());
        assert!(parse_records(&output(1, &[r])).1.is_empty());
        let mut r = record(1, 2, 0, 0, "abc");
        r[56..58].copy_from_slice(&5u16.to_le_bytes());
        assert!(parse_records(&output(1, &[r])).1.is_empty());
        let mut r = record(1, 2, 0, 0, "abc");
        r[4..6].copy_from_slice(&3u16.to_le_bytes());
        assert!(parse_records(&output(1, &[r])).1.is_empty());
        // a bad record does not hide the good one before it
        let mut b = output(1, &[record(1, 2, 0, 0, "ok")]);
        b.extend_from_slice(&[0xFF; 12]);
        assert_eq!(parse_records(&b).1.len(), 1);
    }

    fn rec(frn: u64, reason: u32, name: &str) -> Record {
        Record {
            frn,
            parent_frn: 5,
            reason,
            attrs: 0,
            name: name.into(),
        }
    }

    #[test]
    fn a_file_appended_to_many_times_is_one_event() {
        let mut b = Batch::default();
        for _ in 0..1000 {
            b.add(rec(7, REASON_DATA_EXTEND, "app.log"));
        }
        b.add(rec(8, REASON_FILE_CREATE, "a.tmp"));
        b.add(rec(8, REASON_FILE_DELETE, "a.tmp"));
        assert_eq!(b.len(), 2);
        let mut all = b.drain();
        all.sort_by_key(|(f, _)| *f);
        assert_eq!(all[0].1.reasons, REASON_DATA_EXTEND);
        assert_eq!(all[1].1.reasons, REASON_FILE_CREATE | REASON_FILE_DELETE);
    }

    #[test]
    fn a_flood_is_dropped_and_counted_not_buffered() {
        let mut b = Batch::default();
        for i in 0..(MAX_BATCH as u64 + 50) {
            b.add(rec(i, REASON_DATA_EXTEND, "x"));
        }
        assert_eq!(b.len(), MAX_BATCH);
        assert_eq!(b.dropped, 50);
        b.add(rec(0, REASON_FILE_DELETE, "x")); // known files keep merging
        assert_eq!(b.dropped, 50);
    }
}
