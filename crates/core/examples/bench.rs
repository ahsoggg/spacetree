//! Reproducible scan benchmarks (no extra dependencies).
//!
//! ```text
//! cargo run --release -p spacetree-core --example bench -- gen  <dir> [dirs] [files]   # synthetic tree
//! cargo run --release -p spacetree-core --example bench -- walk <dir> [runs]           # directory walk
//! cargo run --release -p spacetree-core --example bench -- mft  [records] [runs]       # MFT parse + tree build
//! ```

use std::path::Path;
use std::time::{Duration, Instant};

use spacetree_core::Progress;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize, d: u64| args.get(i).and_then(|s| s.parse().ok()).unwrap_or(d);
    match args.first().map(String::as_str) {
        Some("gen") => generate(Path::new(&args[1]), arg(2, 6000) as usize, arg(3, 60_000) as usize),
        Some("walk") => walk(Path::new(&args[1]), arg(2, 5) as usize),
        Some("mft") => mft(arg(1, 1_000_000) as usize, arg(2, 5) as usize),
        _ => eprintln!("usage: bench gen <dir> [dirs] [files] | walk <dir> [runs] | mft [records] [runs]"),
    }
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

/// Deterministic tree: `dirs` directories (random parent, so depth varies), `files` files spread over them;
/// 70 % of the files are empty, the rest are 1..=1500 bytes (mostly resident in the MFT), so the tree costs
/// almost no disk space.
fn generate(root: &Path, dirs: usize, files: usize) {
    let mut rng = Lcg(42);
    std::fs::create_dir_all(root).unwrap();
    let mut paths = vec![root.to_path_buf()];
    for i in 0..dirs {
        let parent = &paths[(rng.next() as usize) % paths.len()];
        let p = parent.join(format!("d{i:05}"));
        std::fs::create_dir(&p).unwrap();
        paths.push(p);
    }
    let mut total = 0u64;
    for i in 0..files {
        let dir = &paths[(rng.next() as usize) % paths.len()];
        let len = if rng.next() % 10 < 7 { 0 } else { (rng.next() % 1500 + 1) as usize };
        total += len as u64;
        std::fs::write(dir.join(format!("f{i:06}.bin")), vec![b'a'; len]).unwrap();
    }
    println!("generated {dirs} dirs, {files} files, {total} bytes under {}", root.display());
}

fn report(name: &str, mut times: Vec<Duration>) {
    times.sort();
    let med = times[times.len() / 2];
    println!(
        "{name}: min {:.3} s, median {:.3} s, max {:.3} s ({} runs)",
        times[0].as_secs_f64(),
        med.as_secs_f64(),
        times[times.len() - 1].as_secs_f64(),
        times.len()
    );
}

fn walk(dir: &Path, runs: usize) {
    let mut times = Vec::new();
    let mut phases: Vec<(&str, Vec<Duration>)> = Vec::new();
    for _ in 0..runs {
        let t = Instant::now();
        let out = spacetree_core::walk::scan(dir, &Progress::default()).unwrap();
        times.push(t.elapsed());
        for (i, (n, d)) in out.timings.iter().enumerate() {
            if phases.len() <= i {
                phases.push((n, Vec::new()));
            }
            phases[i].1.push(*d);
        }
        if times.len() == 1 {
            let (size, alloc, files) = out.tree.root_stats();
            println!(
                "nodes {}, files {files}, size {size}, alloc {alloc}, errors {}",
                out.tree.len(),
                out.errors
            );
        }
    }
    report("walk total", times);
    for (n, t) in phases {
        report(&format!("  {n}"), t);
    }
}

/// Synthetic $MFT: `records` 1 KiB FILE records (10 % directories, 5 % free, 3 % with a DOS name, files placed in
/// any folder), parsed the
/// way `mft::scan` does it (parallel 512 KiB tasks over 8 MiB chunks) and merged into a tree.
fn mft(records: usize, runs: usize) {
    use spacetree_core::ntfs::{self, ChunkOut, testutil};
    const RS: usize = 1024;
    let mut rng = Lcg(7);
    let mut buf = Vec::with_capacity(records * RS);
    // As on a real volume, records are reused: a file's folder may have a higher record number than the file.
    let rolls: Vec<u64> = (0..records).map(|_| rng.next() % 100).collect();
    let all_dirs: Vec<u64> = std::iter::once(5)
        .chain((16..records).filter(|&i| (5..15).contains(&rolls[i])).map(|i| i as u64))
        .collect();
    let mut dirs: Vec<u64> = vec![5];
    for (i, &roll) in rolls.iter().enumerate() {
        let r = if i < 16 && i != 5 {
            vec![0u8; RS]
        } else if i == 5 {
            testutil::record(0x03, 0, &[testutil::file_name_attr(5, ".", 1)])
        } else {
            let parent = if roll < 15 {
                dirs[(rng.next() as usize) % dirs.len()]
            } else {
                all_dirs[(rng.next() as usize) % all_dirs.len()]
            };
            let mut si = vec![0u8; 0x30];
            si[8..16].copy_from_slice(&((1_700_000_000u64 + rng.next() % 10_000_000 + 11_644_473_600) * 10_000_000).to_le_bytes());
            if roll < 5 {
                testutil::record(0x00, 0, &[testutil::file_name_attr(parent, "deleted", 1)])
            } else if roll < 15 {
                dirs.push(i as u64);
                testutil::record(
                    0x03,
                    0,
                    &[testutil::resident(0x10, &si), testutil::file_name_attr(parent, &format!("dir{i}"), 1)],
                )
            } else {
                let name = format!("file name {i} with some length.dat");
                let mut attrs = vec![testutil::resident(0x10, &si)];
                if roll < 18 {
                    attrs.push(testutil::file_name_attr(parent, "FILENA~1.DAT", 2));
                }
                attrs.push(testutil::file_name_attr(parent, &name, 1));
                if roll < 60 {
                    attrs.push(testutil::nonresident_data(rng.next() % 10_000_000, 4096 * (rng.next() % 2500), 0, 0, None));
                } else {
                    attrs.push(testutil::resident(0x80, &[1u8; 100]));
                }
                testutil::record(0x01, 0, &attrs)
            }
        };
        buf.extend_from_slice(&r);
    }
    println!("synthetic $MFT: {records} records, {} MiB", buf.len() >> 20);

    use rayon::prelude::*;
    const CHUNK: usize = 8 * 1024 * 1024;
    const TASK: usize = 512 * 1024;
    let (mut t_parse, mut t_build) = (Vec::new(), Vec::new());
    for _ in 0..runs {
        let mut data = buf.clone();
        let t = Instant::now();
        let mut outs: Vec<ChunkOut> = Vec::new();
        for (ci, chunk) in data.chunks_mut(CHUNK).enumerate() {
            let first = ci * (CHUNK / RS);
            let parsed: Vec<ChunkOut> = chunk
                .par_chunks_mut(TASK)
                .enumerate()
                .map(|(i, part)| {
                    let mut out = ChunkOut::default();
                    let base = first + i * (TASK / RS);
                    for (j, rec) in part.as_chunks_mut::<RS>().0.iter_mut().enumerate() {
                        ntfs::parse_record(rec, (base + j) as u32, &mut out);
                    }
                    out
                })
                .collect();
            outs.extend(parsed);
        }
        t_parse.push(t.elapsed());
        let t = Instant::now();
        let built = ntfs::build_tree(outs, "X:\\").unwrap();
        t_build.push(t.elapsed());
        if t_parse.len() == 1 {
            let (size, alloc, files) = built.tree.root_stats();
            println!("nodes {}, files {files}, size {size}, alloc {alloc}", built.tree.len());
        }
    }
    report("mft parse", t_parse);
    report("mft build_tree", t_build);
}
