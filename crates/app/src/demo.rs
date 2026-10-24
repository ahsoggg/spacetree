//! Demo mode: a made-up computer instead of the real disk.
//!
//! `SpaceTree.exe --demo` opens the app on invented data (nothing is scanned, written or deleted), and
//! `SpaceTree.exe --screenshots <dir>` additionally walks through the tabs and saves a PNG of each — that is
//! how the screenshots in the README are made, without a single real file name in them.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Event, ViewportCommand};
use spacetree_core::cleanup::Category;
use spacetree_core::dupes::DupSet;
use spacetree_core::orphans::Installed;
use spacetree_core::snapshot::{Change, Entry, Snapshot};
use spacetree_core::stats::{top_files, type_stats};
use spacetree_core::tree::{Builder, Kind, NO_PARENT};
use spacetree_core::{NO_NODE, NodeId, ROOT, ScanOutput, Selection, StreamInfo, Tree, Used};

use crate::growth::Growth;
use crate::{App, DriveInfo, Loaded, TOP_N, Tab, now_unix};

const GB: u64 = 1 << 30;
const MB: u64 = 1 << 20;
const DAY: u32 = 86_400;
/// Time to let animations finish before a screenshot.
const SETTLE: Duration = Duration::from_millis(1600);

pub fn drives() -> Vec<DriveInfo> {
    vec![
        DriveInfo {
            path: "C:\\".into(),
            space: Some((512 * GB, 131 * GB)),
        },
        DriveInfo {
            path: "D:\\".into(),
            space: Some((2048 * GB, 1210 * GB)),
        },
    ]
}

/// What the screenshot tour does, in order.
const STEPS: [&str; 9] = [
    "welcome",
    "overview",
    "types",
    "growth",
    "cleanup",
    "duplicates",
    "orphans",
    "streams",
    "largest",
];

/// Programs "installed" on the invented computer (the folder of the removed one is left in AppData).
const INSTALLED: [&str; 7] = [
    "Google Chrome",
    "Discord",
    "Adobe Inc.",
    "JetBrains",
    "Docker Desktop",
    "Git",
    "Microsoft Office",
];

pub struct Demo {
    dir: Option<PathBuf>,
    step: usize,
    prepared: bool,
    asked: bool,
    since: Instant,
}

impl Demo {
    pub fn from_args() -> Demo {
        let mut args = std::env::args().skip_while(|a| a != "--screenshots");
        let dir = args.nth(1).map(PathBuf::from);
        Demo {
            dir,
            step: 0,
            prepared: false,
            asked: false,
            since: Instant::now(),
        }
    }
}

struct Maker {
    b: Builder,
    now: u32,
    rng: u64,
}

impl Maker {
    fn rnd(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    fn dir(&mut self, parent: NodeId, name: &str) -> NodeId {
        self.b.add(parent, name, Kind::Dir, 0, 0)
    }

    fn file(&mut self, parent: NodeId, name: &str, size: u64, age_days: u32) -> NodeId {
        let alloc = size.div_ceil(4096) * 4096;
        let id = self.b.add(parent, name, Kind::File, size, alloc);
        self.b.nodes[id as usize].mtime = self.now - age_days * DAY;
        id
    }

    /// `count` files `<prefix> <n>.<ext>` of random sizes adding up to about `total`, modified within `ages`.
    fn spread(&mut self, parent: NodeId, prefix: &str, ext: &str, count: u64, total: u64, ages: (u32, u32)) {
        let weights: Vec<u64> = (0..count).map(|_| 20 + self.rnd() % 100).collect();
        let sum: u64 = weights.iter().sum();
        for (i, w) in weights.into_iter().enumerate() {
            let age = ages.0 + (self.rnd() % u64::from(ages.1 - ages.0 + 1)) as u32;
            self.file(parent, &format!("{prefix} {:03}.{ext}", i + 1), total / sum * w, age);
        }
    }

    /// Nested folders from a `/`-separated path under `parent`.
    fn path(&mut self, mut parent: NodeId, path: &str) -> NodeId {
        for part in path.split('/') {
            parent = self.dir(parent, part);
        }
        parent
    }
}

struct Fixture {
    tree: Tree,
    dup_sets: Vec<DupSet>,
    streams: Vec<StreamInfo>,
    selected: NodeId,
}

fn build() -> Fixture {
    let now = now_unix();
    let mut m = Maker {
        b: Builder::default(),
        now,
        rng: 0x9E37_79B9_7F4A_7C15,
    };
    let root = m.b.add(NO_PARENT, "C:\\", Kind::Dir, 0, 0);

    // Windows and programs
    let win = m.dir(root, "Windows");
    let sys32 = m.dir(win, "System32");
    m.spread(sys32, "driver", "dll", 40, 7 * GB, (20, 400));
    let winsxs = m.dir(win, "WinSxS");
    m.spread(winsxs, "component", "dll", 60, 11 * GB, (30, 500));
    let wtemp = m.dir(win, "Temp");
    m.spread(wtemp, "install", "log", 12, 900 * MB, (1, 30));
    let swd = m.path(win, "SoftwareDistribution/Download");
    m.spread(swd, "update", "cab", 9, 2 * GB + 200 * MB, (2, 40));
    let pf = m.dir(root, "Program Files");
    for (name, gb) in [
        ("Adobe", 11u64),
        ("Microsoft Office", 5),
        ("JetBrains", 3),
        ("Docker", 4),
        ("Git", 1),
    ] {
        let d = m.dir(pf, name);
        m.spread(d, "module", "dll", 14, gb * GB, (40, 600));
    }
    let pd = m.dir(root, "ProgramData");
    m.spread(pd, "package", "bin", 20, 8 * GB, (10, 300));

    // Games on the same drive
    let games = m.dir(root, "Games");
    let g1 = m.dir(games, "Starfall Odyssey");
    m.spread(g1, "assets", "pak", 24, 62 * GB, (3, 90));
    let g2 = m.dir(games, "Dungeon Keeper Remake");
    m.spread(g2, "data", "pak", 12, 28 * GB, (60, 400));
    let g3 = m.dir(games, "Neon Drift");
    m.spread(g3, "level", "dat", 16, 17 * GB, (100, 500));

    // The user
    let users = m.dir(root, "Users");
    let me = m.dir(users, "demo");
    let appdata = m.dir(me, "AppData");
    let local = m.dir(appdata, "Local");
    let temp = m.dir(local, "Temp");
    m.spread(temp, "setup", "tmp", 26, 12 * GB + 400 * MB, (0, 25));
    let chrome = m.path(local, "Google/Chrome/User Data/Default");
    let cache = m.dir(chrome, "Cache");
    m.spread(cache, "f", "cache", 30, 3 * GB + 800 * MB, (0, 20));
    m.file(chrome, "Bookmarks", 3 * MB, 5);
    let npm = m.dir(local, "npm-cache");
    m.spread(npm, "pkg", "tgz", 20, 2 * GB + 300 * MB, (10, 200));
    let dumps = m.dir(local, "CrashDumps");
    m.spread(dumps, "app", "dmp", 4, 1500 * MB, (20, 120));
    let roaming = m.dir(appdata, "Roaming");
    let discord = m.path(roaming, "discord/Cache");
    m.spread(discord, "d", "cache", 10, 1100 * MB, (0, 15));
    let leftover = m.dir(roaming, "ProSketch Studio");
    m.spread(leftover, "brushcache", "db", 8, 3 * GB + 400 * MB, (200, 700));

    let downloads = m.dir(me, "Downloads");
    m.file(downloads, "VisualStudioSetup.exe", 1700 * MB, 140);
    m.file(downloads, "node-installer.msi", 900 * MB, 95);
    m.file(downloads, "dataset-backup.zip", 3 * GB + 100 * MB, 210);
    m.file(downloads, "ubuntu-24.04.iso", 5 * GB, 160);
    m.file(downloads, "photos-from-trip.zip", 2 * GB, 60);
    let dl_holiday = m.file(downloads, "Holiday 2024 (1).mp4", 4 * GB + 200 * MB, 40);
    m.file(downloads, "invoice.pdf", 2 * MB, 3);
    let readme = m.file(downloads, "readme.txt", 1024, 200);

    let videos = m.dir(me, "Videos");
    let holiday = m.file(videos, "Holiday 2024.mp4", 4 * GB + 200 * MB, 12);
    m.file(videos, "Conference talk.mkv", 3 * GB, 80);
    m.file(videos, "Wedding film (archive).mkv", 6 * GB, 700);
    let rec = m.dir(videos, "Screen recordings");
    m.spread(rec, "capture", "mp4", 14, 9 * GB + 100 * MB, (0, 30));

    let pics = m.dir(me, "Pictures");
    let trip = m.dir(pics, "Trip 2023");
    m.spread(trip, "IMG", "jpg", 60, 3 * GB, (300, 600));
    let trip_copy = m.path(pics, "Trip 2023 (copy)");
    m.spread(trip_copy, "IMG", "jpg", 6, 600 * MB, (200, 250));
    let raw = m.dir(pics, "RAW");
    m.spread(raw, "DSC", "nef", 40, 6 * GB, (30, 400));
    let scans = m.dir(pics, "Scans");
    let scan_a = m.file(scans, "contract-scan.pdf", 38 * MB, 20);
    let docs = m.dir(me, "Documents");
    let notes = m.file(docs, "meeting-notes.txt", 0, 30);
    let backup = m.dir(docs, "Backup");
    let scan_b = m.file(backup, "contract-scan.pdf", 38 * MB, 90);
    let backup_holiday = m.file(backup, "Holiday 2024.mp4", 4 * GB + 200 * MB, 70);
    m.file(docs, "thesis.docx.bak", 48 * MB, 50);
    m.spread(docs, "report", "docx", 30, 400 * MB, (10, 500));
    let music = m.dir(me, "Music");
    m.spread(music, "track", "flac", 120, 9 * GB, (200, 900));

    let projects = m.dir(me, "Projects");
    let shop = m.dir(projects, "webshop");
    m.file(shop, "package.json", 2048, 2);
    let nm = m.dir(shop, "node_modules");
    m.spread(nm, "mod", "js", 40, GB + 500 * MB, (1, 30));
    m.spread(shop, "src", "ts", 20, 15 * MB, (0, 30));
    let eng = m.dir(projects, "rust-engine");
    m.file(eng, "Cargo.toml", 1024, 1);
    let target = m.dir(eng, "target");
    m.spread(target, "build", "rlib", 30, 5 * GB + 600 * MB, (0, 10));
    m.spread(eng, "src", "rs", 25, 8 * MB, (0, 40));

    let bin = m.dir(root, "$Recycle.Bin");
    m.spread(bin, "deleted", "mkv", 3, 3 * GB + 400 * MB, (3, 60));
    m.file(root, "hiberfil.sys", 6 * GB + 400 * MB, 2);
    m.file(root, "pagefile.sys", 4 * GB, 2);
    m.file(root, "swapfile.sys", 256 * MB, 2);

    let tree = m.b.finish();
    let dup_sets = vec![
        DupSet {
            size: 4 * GB + 200 * MB,
            items: vec![holiday, dl_holiday, backup_holiday],
        },
        DupSet {
            size: 38 * MB,
            items: vec![scan_a, scan_b],
        },
    ];
    let streams = vec![
        StreamInfo {
            node: notes,
            name: "backup.vhd".into(),
            size: 6 * GB,
            alloc: 6 * GB,
        },
        StreamInfo {
            node: readme,
            name: "payload.dat".into(),
            size: GB + 200 * MB,
            alloc: GB + 200 * MB,
        },
    ];
    let selected = find(&tree, "Users/demo/Videos").unwrap_or(ROOT);
    Fixture {
        tree,
        dup_sets,
        streams,
        selected,
    }
}

/// Node at the `/`-separated path under the root.
fn find(tree: &Tree, rel: &str) -> Option<NodeId> {
    let mut cur = ROOT;
    for part in rel.split('/') {
        cur = tree
            .children(cur)
            .iter()
            .copied()
            .find(|&c| tree.name(c).eq_ignore_ascii_case(part))?;
    }
    Some(cur)
}

/// Changes over the last week: (path, delta in GB; negative = shrank or gone).
const CHANGES: [(&str, f64); 10] = [
    ("Games/Starfall Odyssey", 14.0),
    ("Users/demo/AppData/Local/Temp", 12.0),
    ("Users/demo/Videos/Screen recordings", 6.1),
    ("Users/demo/Documents/Old projects", -5.4),
    ("Users/demo/Downloads", -3.5),
    ("Windows/SoftwareDistribution/Download", 2.2),
    ("Users/demo/AppData/Local/Google/Chrome/User Data/Default/Cache", 1.9),
    ("Users/demo/Projects/rust-engine/target", 1.6),
    ("Users/demo/AppData/Roaming/discord/Cache", 0.8),
    ("$Recycle.Bin", -1.2),
];

fn growth(tree: &Tree, snapshot: &Snapshot) -> Growth {
    let now = u64::from(now_unix());
    let day = u64::from(DAY);
    let history = [30, 14, 7]
        .iter()
        .map(|d| Entry {
            taken: now - d * day,
            path: PathBuf::from("demo"),
        })
        .collect();
    let mut changes: Vec<Change> = CHANGES
        .iter()
        .map(|&(path, gb)| {
            let delta = (gb * GB as f64) as i64;
            let new = find(tree, path).map(|id| tree.node(id).size);
            let old = new.and_then(|n| u64::try_from(n as i64 - delta).ok().filter(|&o| o > 0));
            Change {
                path: path.into(),
                old: if new.is_none() { Some(delta.unsigned_abs()) } else { old },
                new,
                delta,
            }
        })
        .collect();
    changes.sort_by_key(|c| std::cmp::Reverse(c.delta.abs()));
    let total_delta: i64 = changes.iter().map(|c| c.delta).sum();
    Growth {
        chosen: Some(2),
        base: Some((now - 7 * day, snapshot.total.saturating_sub(total_delta.max(0) as u64))),
        history,
        changes,
        total: snapshot.total,
        total_delta,
        error: None,
    }
}

impl App {
    /// Replaces the data with the invented computer (also used instead of a real scan in `--demo`).
    pub fn load_demo(&mut self) {
        let fx = build();
        let snapshot = Snapshot::from_tree(&fx.tree, u64::from(now_unix()));
        let growth = growth(&fx.tree, &snapshot);
        let (top, types) = (top_files(&fx.tree, TOP_N), type_stats(&fx.tree, ROOT));
        let mut loaded = Loaded {
            out: ScanOutput {
                tree: fx.tree,
                selection: Selection {
                    used: Used::Mft,
                    reason: "demo".into(),
                },
                errors: 0,
                error_samples: Vec::new(),
                elapsed: Duration::from_millis(4200),
                extra_links: 0,
                timings: Vec::new(),
                notes: Vec::new(),
                streams: fx.streams,
                // non-empty: the demo behaves like an MFT scan (shows the hidden-streams tab)
                record_nodes: vec![NO_NODE],
                usn: None,
            },
            top,
            types,
            cleanup: Vec::new(),
            cleanup_total: 0,
            dup_candidates: Vec::new(),
            installed: std::sync::Arc::new(Installed::new(INSTALLED.map(String::from))),
            ads: Default::default(),
            ads_dirs: Default::default(),
            dup_sets: Some(fx.dup_sets),
            dup_keep: Default::default(),
            snapshot: Some(snapshot),
            history: growth.history.clone(),
            disk: None,
            analysis: Duration::from_millis(300),
            root_path: PathBuf::from("C:\\"),
        };
        loaded.index_streams();
        loaded.reanalyze();
        // reanalyze looked at the real disk: show a plausible one for the invented computer instead
        loaded.disk = Some((512 * GB, 512 * GB - loaded.out.tree.root_stats().0 - 60 * GB));
        self.install(loaded, "C:\\");
        self.growth = growth;
        self.path_text = "C:\\".into();
        self.mft_ready = true;
        self.select_keep_view(fx.selected);
        for id in ["Users", "Users/demo"].iter().filter_map(|p| {
            let d = self.data.as_ref()?;
            find(&d.out.tree, p)
        }) {
            self.expanded.insert(id);
        }
        self.rows_dirty = true;
    }

    fn demo_prepare(&mut self, ctx: &egui::Context, step: &str) {
        if step != "welcome" && self.data.is_none() {
            self.load_demo();
        }
        let cleanup_group = |app: &App, cat: Category| {
            app.data
                .as_ref()
                .and_then(|d| d.cleanup.iter().position(|g| g.category == cat))
        };
        match step {
            "overview" => self.tab = Tab::Overview,
            "types" => {
                self.tab = Tab::Types;
                self.highlight_ext = Some("mp4".into());
            }
            "growth" => self.tab = Tab::Growth,
            "streams" => self.tab = Tab::Streams,
            "orphans" => {
                self.tab = Tab::Cleanup;
                self.cleanup_show = [false, false, true];
                self.cleanup_open.clear();
                if let Some(i) = cleanup_group(self, Category::OrphanedData) {
                    self.cleanup_open.insert(i);
                }
            }
            "cleanup" => {
                self.tab = Tab::Cleanup;
                self.cleanup_show = [true; 3];
                self.cleanup_open.clear();
                for cat in [Category::Temp, Category::PackageCache] {
                    if let Some(i) = cleanup_group(self, cat) {
                        self.cleanup_open.insert(i);
                    }
                }
            }
            "duplicates" => {
                self.tab = Tab::Cleanup;
                self.cleanup_show = [false, false, true];
                self.cleanup_open.clear();
                if let Some(i) = cleanup_group(self, Category::Duplicates) {
                    self.cleanup_open.insert(i);
                    let items = self
                        .data
                        .as_ref()
                        .map(|d| d.cleanup[i].items.clone())
                        .unwrap_or_default();
                    self.marked = items.into_iter().collect();
                }
            }
            "largest" => {
                self.tab = Tab::Top;
                self.marked.clear();
            }
            _ => {}
        }
        ctx.request_repaint();
    }

    /// Drives the screenshot tour (no-op unless started with `--screenshots <dir>`).
    pub fn demo_step(&mut self, ctx: &egui::Context) {
        let Some(mut d) = self.demo.take() else { return };
        if let Some(dir) = d.dir.clone() {
            ctx.request_repaint();
            match STEPS.get(d.step) {
                None => ctx.send_viewport_cmd(ViewportCommand::Close),
                Some(&step) => {
                    if !d.prepared {
                        self.demo_prepare(ctx, step);
                        d.prepared = true;
                        d.since = Instant::now();
                    }
                    let shot = ctx.input(|i| {
                        i.events.iter().find_map(|e| match e {
                            Event::Screenshot { image, .. } => Some(image.clone()),
                            _ => None,
                        })
                    });
                    if let Some(img) = shot {
                        let file = dir.join(format!("{:02}-{step}.png", d.step + 1));
                        match save_png(&file, &img) {
                            Ok(()) => println!("saved {}", file.display()),
                            Err(e) => eprintln!("cannot save {}: {e}", file.display()),
                        }
                        d.step += 1;
                        d.prepared = false;
                        d.asked = false;
                    } else if !d.asked && d.since.elapsed() > SETTLE {
                        ctx.send_viewport_cmd(ViewportCommand::Screenshot(egui::UserData::default()));
                        d.asked = true;
                    }
                }
            }
        }
        self.demo = Some(d);
    }
}

fn save_png(path: &std::path::Path, img: &egui::ColorImage) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), img.width() as u32, img.height() as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().map_err(|e| e.to_string())?;
    w.write_image_data(img.as_raw()).map_err(|e| e.to_string())
}
