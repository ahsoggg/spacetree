#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod chrome;
mod compress;
mod delete;
mod demo;
mod diag;
mod growth;
mod installer;
mod live;
mod programs;
mod recover;
mod release_sig;
mod settings;
mod streams;
mod theme;
mod treemap;
mod update;
mod views;
mod winint;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Key, Rect, Ui};
use spacetree_core::cleanup::{self, Group};
use spacetree_core::dupes::{self, DupProgress, DupSet};
use spacetree_core::orphans::{self, Installed};
use spacetree_core::snapshot::{self, Entry, Snapshot};
use spacetree_core::stats::{TypeStat, top_files, type_stats};
use spacetree_core::{Backend, NodeId, Progress, ROOT, ScanError, ScanOutput, Tree, Used, export};

use settings::Settings;

pub const TOP_N: usize = 1000;
const SEARCH_LIMIT: usize = 5000;
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(200);

/// Everything computed in the background after a scan.
pub struct Loaded {
    pub out: ScanOutput,
    pub top: Vec<NodeId>,
    pub types: Vec<TypeStat>,
    pub cleanup: Vec<Group>,
    pub cleanup_total: u64,
    /// Files worth hashing for content duplicates.
    pub dup_candidates: Vec<NodeId>,
    /// Result of the duplicate search (`None` until it was run).
    pub dup_sets: Option<Vec<DupSet>>,
    /// For each offered duplicate, the copy that is kept.
    pub dup_keep: std::collections::HashMap<NodeId, NodeId>,
    /// This scan as a snapshot, and the earlier snapshots of the same location.
    pub snapshot: Option<Snapshot>,
    pub history: Vec<Entry>,
    /// `(total, free)` of the scanned volume.
    pub disk: Option<(u64, u64)>,
    pub analysis: Duration,
    pub root_path: PathBuf,
    /// Programs installed on this computer (for the "orphaned data" check).
    pub installed: Arc<Installed>,
    /// Files with a large hidden alternate data stream -> hidden bytes in total.
    pub ads: HashMap<NodeId, u64>,
    /// Folders that contain such files (for the treemap highlight).
    pub ads_dirs: HashSet<NodeId>,
}

impl Loaded {
    /// Recomputes the derived lists after the tree changed (deletions).
    pub fn reanalyze(&mut self) {
        let tree = &self.out.tree;
        let ((top, types), (mut groups, candidates)) = rayon::join(
            || rayon::join(|| top_files(tree, TOP_N), || type_stats(tree, ROOT)),
            || cleanup::find_with_dup_candidates(tree, now_unix()),
        );
        let claimed: Vec<NodeId> = groups.iter().flat_map(|g| g.items.iter().copied()).collect();
        groups.extend(orphans::find(tree, &self.installed, &claimed));
        cleanup::sort_groups(&mut groups);
        self.cleanup = groups;
        self.dup_candidates = candidates;
        self.top = top;
        self.types = types;
        self.disk = spacetree_core::disk_space(&self.root_path);
        self.merge_dups();
    }

    /// Fills `ads` / `ads_dirs` from the scan's alternate data streams.
    pub fn index_streams(&mut self) {
        let tree = &self.out.tree;
        self.ads.clear();
        self.ads_dirs.clear();
        for s in &self.out.streams {
            *self.ads.entry(s.node).or_default() += s.size.max(s.alloc);
        }
        for &n in self.ads.keys() {
            let mut cur = tree.parent(n);
            while let Some(p) = cur {
                if !self.ads_dirs.insert(p) {
                    break; // the rest of the way up is already marked
                }
                cur = tree.parent(p);
            }
        }
    }

    /// (Re)builds the "duplicates" group from `dup_sets` and refreshes the total.
    pub fn merge_dups(&mut self) {
        self.cleanup.retain(|g| g.category != cleanup::Category::Duplicates);
        self.dup_keep.clear();
        if let Some(sets) = &self.dup_sets
            && let Some((group, keep)) = cleanup::duplicates_group(&self.out.tree, sets)
        {
            self.cleanup.push(group);
            self.dup_keep = keep;
        }
        cleanup::sort_groups(&mut self.cleanup);
        self.cleanup_total = self.cleanup.iter().map(|g| g.size).sum();
    }

    /// Space that deleting the redundant copies of the verified duplicates would free.
    pub fn dup_wasted(&self) -> u64 {
        self.cleanup
            .iter()
            .find(|g| g.category == cleanup::Category::Duplicates)
            .map_or(0, |g| g.size)
    }
}

struct DupJob {
    progress: Arc<DupProgress>,
    rx: Receiver<Option<Vec<DupSet>>>,
    started: Instant,
}

struct Job {
    progress: Arc<Progress>,
    rx: Receiver<Result<Loaded, ScanError>>,
    started: Instant,
    path: String,
}

#[derive(PartialEq, Clone, Copy)]
pub enum Tab {
    Overview,
    Types,
    Top,
    Growth,
    Cleanup,
    /// NTFS alternate data streams (MFT scans only).
    Streams,
}

#[derive(PartialEq, Clone, Copy)]
pub enum SortKey {
    Size,
    Name,
    Files,
    Modified,
}

/// Sort order of flat lists (largest files, search results).
#[derive(PartialEq, Clone, Copy)]
pub enum ListSort {
    Size,
    Modified,
    Name,
}

#[derive(PartialEq, Clone, Copy)]
pub enum TypeSort {
    Size,
    Count,
    Name,
}

pub struct TmCache {
    pub root: NodeId,
    pub size: egui::Vec2,
    pub by_age: bool,
    /// Bumped by deletions so that the layout is recomputed.
    pub generation: u64,
    pub rects: Vec<treemap::TmRect>,
    /// Per rect: fill color and lower-cased extension ("" for folders).
    pub colors: Vec<Color32>,
    pub exts: Vec<String>,
}

/// Live update of the treemap: tiles glide from where they were to where the new layout puts them.
pub struct TmMorph {
    pub start: Instant,
    pub from: HashMap<NodeId, Rect>,
}

/// Zoom transition of the treemap: the new layout grows from `from` (local coordinates) to the full area.
pub struct TmAnim {
    pub from: Rect,
    pub start: Instant,
}

pub struct DriveInfo {
    pub path: String,
    pub space: Option<(u64, u64)>,
}

/// Something the user asked for from a row, a tile or a context menu; applied after drawing.
#[derive(Clone, Copy)]
pub enum Action {
    Select(NodeId),
    /// Select and show in the overview tab (tree + treemap focused on its folder).
    Show(NodeId),
    Toggle(NodeId),
    Reveal(NodeId),
    Open(NodeId),
    CopyPath(NodeId),
    Delete(NodeId, bool),
    /// Compress (`false`) or decompress (`true`) with LZX.
    Compress(NodeId, bool),
}

#[derive(Clone, Copy, PartialEq)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    pub born: Instant,
    /// Shown with a "restart" button and kept until dismissed.
    pub restart: bool,
}

pub enum Mode {
    App,
    Install(installer::Install),
    Uninstall(installer::Uninstall),
}

pub struct App {
    pub mode: Mode,
    pub settings: Settings,
    pub update: update::Shared,
    update_reported: bool,
    pub toasts: Vec<Toast>,
    pub show_settings: bool,
    pub icon: Option<egui::TextureHandle>,

    pub path_text: String,
    pub drives: Vec<DriveInfo>,
    pub backend: Backend,
    /// Raw MFT reading works (Windows + administrator): scans of whole drives are much faster.
    pub mft_ready: bool,
    auto_scan: bool,
    job: Option<Job>,
    pub data: Option<Arc<Loaded>>,
    /// Bumped whenever the tree changes after a scan.
    pub generation: u64,
    /// Bumped whenever a new tree is installed; deletions remember it so that node ids of one tree are never
    /// applied to another.
    pub scans: u64,
    pub message: String,
    pub status: Arc<Mutex<Option<(String, ToastKind)>>>,
    pub tab: Tab,

    pub selected: Option<NodeId>,
    pub tm_root: NodeId,
    pub expanded: HashSet<NodeId>,
    pub rows: Vec<(NodeId, u16)>,
    pub rows_dirty: bool,
    pub scroll_to: Option<usize>,
    pub tm: Option<TmCache>,
    pub tm_anim: Option<TmAnim>,
    pub tm_morph: Option<TmMorph>,
    pub tm_menu: Option<NodeId>,
    pub highlight_ext: Option<String>,
    pub sort: SortKey,
    pub sort_desc: bool,
    pub top_sort: (ListSort, bool),
    pub top_order: Vec<NodeId>,
    pub top_order_key: Option<(ListSort, bool, u64)>,
    pub types_sort: (TypeSort, bool),

    pub search: String,
    search_done: String,
    search_edited: Option<Instant>,
    pub search_hits: Vec<NodeId>,
    pub search_truncated: bool,
    pub search_sort: (ListSort, bool),

    pub cleanup_open: HashSet<usize>,
    pub cleanup_show: [bool; 3],
    pub cleanup_by_size: bool,
    /// Items ticked for deletion in the cleanup tab.
    pub marked: HashSet<NodeId>,
    pub delete: delete::DeleteState,
    pub compress: compress::CompressState,
    pub live: live::Live,
    /// Start live monitoring as soon as the next scan is shown (it needs the egui context).
    live_autostart: bool,
    pub streams: streams::StreamsState,
    pub growth: growth::Growth,
    pub recover: recover::Recover,
    dup_job: Option<DupJob>,
    /// Search result waiting for the tree to be free (a deletion may hold it).
    dup_pending: Option<Vec<DupSet>>,
    pub demo: Option<demo::Demo>,
}

pub fn list_drives() -> Vec<DriveInfo> {
    let paths: Vec<String> = if cfg!(windows) {
        ('A'..='Z')
            .map(|c| format!("{c}:\\"))
            .filter(|p| std::path::Path::new(p).exists())
            .collect()
    } else {
        vec!["/".into()]
    };
    paths
        .into_iter()
        .map(|path| DriveInfo {
            space: spacetree_core::disk_space(std::path::Path::new(&path)),
            path,
        })
        .collect()
}

/// Saves the snapshot (if enabled) and returns the earlier snapshots of the same location, oldest first.
fn store_snapshot(snap: &Snapshot, save: bool) -> Vec<Entry> {
    let Some(base) = settings::dir()
        .map(|d| d.join("snapshots"))
        .filter(|_| !settings::is_demo())
    else {
        return Vec::new();
    };
    if save && let Err(e) = snapshot::save(&base, snap) {
        diag::warn(format!("cannot save snapshot: {e}"));
    }
    let mut history = snapshot::list(&base, &snap.root);
    history.retain(|e| e.taken < snap.taken);
    history
}

pub fn now_unix() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().min(u32::MAX as u64) as u32)
}

/// Case-insensitive substring test; `needle` must already be lower-case.
fn contains_ci(hay: &str, needle: &str) -> bool {
    if needle.is_ascii() {
        let n = needle.as_bytes();
        hay.len() >= n.len() && hay.as_bytes().windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
    } else {
        hay.to_lowercase().contains(needle)
    }
}

/// The folder to scan, from the command line (Explorer passes it as `"%1"` / `"%V"`). A drive root comes
/// as `"C:\"`, which Windows argument parsing turns into `C:"` — repair that.
fn path_arg() -> Option<String> {
    if settings::is_demo() {
        return None; // `--screenshots <dir>`: the argument is an output folder, not something to scan
    }
    std::env::args()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .map(|a| a.trim_matches('"').trim().to_owned())
        .filter(|a| !a.is_empty())
}

pub fn sort_list(tree: &Tree, ids: &mut [NodeId], (key, desc): (ListSort, bool)) {
    match key {
        ListSort::Size => ids.sort_by_key(|&i| std::cmp::Reverse(tree.node(i).size)),
        ListSort::Modified => ids.sort_by_key(|&i| std::cmp::Reverse(tree.node(i).mtime)),
        ListSort::Name => ids.sort_by_cached_key(|&i| tree.name(i).to_lowercase()),
    }
    let natural_desc = key != ListSort::Name;
    if natural_desc != desc {
        ids.reverse();
    }
}

impl App {
    fn new(cc: &eframe::CreationContext, mode: Mode) -> Self {
        theme::install(&cc.egui_ctx);
        let icon = image_from_rgba(include_bytes!("../assets/icon64.rgba"), 64)
            .map(|img| cc.egui_ctx.load_texture("icon", img, egui::TextureOptions::LINEAR));
        let settings = if settings::is_demo() {
            Settings::default()
        } else {
            Settings::load()
        };
        let in_app = matches!(mode, Mode::App);
        let drives = match (in_app, settings::is_demo()) {
            (false, _) => Vec::new(),
            (true, true) => demo::drives(),
            (true, false) => list_drives(),
        };
        let arg = if in_app { path_arg() } else { None };
        let mft_ready = in_app
            && !settings::is_demo()
            && cfg!(windows)
            && drives.first().is_some_and(|d| {
                spacetree_core::select_backend(std::path::Path::new(&d.path), Backend::Auto)
                    .is_ok_and(|s| s.used == Used::Mft)
            });
        let mut app = App {
            mode,
            update: Arc::new(Mutex::new(update::State::Idle)),
            update_reported: false,
            toasts: Vec::new(),
            show_settings: false,
            icon,
            path_text: arg
                .clone()
                .or_else(|| settings.recent.first().cloned())
                .or_else(|| drives.first().map(|d| d.path.clone()))
                .unwrap_or_default(),
            settings,
            auto_scan: arg.is_some(),
            drives,
            backend: Backend::Auto,
            mft_ready,
            job: None,
            data: None,
            generation: 0,
            scans: 0,
            message: String::new(),
            status: Arc::default(),
            tab: Tab::Overview,
            selected: None,
            tm_root: ROOT,
            expanded: HashSet::new(),
            rows: Vec::new(),
            rows_dirty: false,
            scroll_to: None,
            tm: None,
            tm_anim: None,
            tm_morph: None,
            tm_menu: None,
            highlight_ext: None,
            sort: SortKey::Size,
            sort_desc: true,
            top_sort: (ListSort::Size, true),
            top_order: Vec::new(),
            top_order_key: None,
            types_sort: (TypeSort::Size, true),
            search: String::new(),
            search_done: String::new(),
            search_edited: None,
            search_hits: Vec::new(),
            search_truncated: false,
            search_sort: (ListSort::Size, true),
            cleanup_open: HashSet::new(),
            cleanup_show: [true; 3],
            cleanup_by_size: false,
            marked: HashSet::new(),
            delete: delete::DeleteState::default(),
            compress: compress::CompressState::default(),
            live: live::Live::default(),
            live_autostart: false,
            streams: streams::StreamsState::default(),
            growth: growth::Growth::default(),
            recover: recover::Recover::default(),
            dup_job: None,
            dup_pending: None,
            demo: None,
        };
        if in_app && settings::is_demo() {
            app.demo = Some(demo::Demo::from_args());
        }
        if in_app
            && !settings::is_demo()
            && let Some(report) = diag::take_unseen_crash()
        {
            app.toast(
                format!(
                    "Прошлый запуск завершился сбоем. Отчёт сохранён: {} — пришлите его разработчику.",
                    report.display()
                ),
                ToastKind::Error,
            );
        }
        if in_app {
            update::cleanup_old();
            if winint::running_installed() {
                winint::refresh_installed_version();
            }
            if !settings::is_demo() {
                app.maybe_check_updates(&cc.egui_ctx, false);
            }
        }
        app
    }

    /// Release builds check GitHub for a new version at most every few hours (`force`: now).
    pub fn maybe_check_updates(&mut self, ctx: &egui::Context, force: bool) {
        let enabled = cfg!(not(debug_assertions)) || std::env::var_os("SPACETREE_UPDATE_DEBUG").is_some();
        if !enabled && !force {
            return;
        }
        let now = now_unix() as u64;
        if !force && now.saturating_sub(self.settings.last_update_check) < update::CHECK_INTERVAL_SECS {
            return;
        }
        self.settings.last_update_check = now;
        self.settings.save();
        self.update_reported = false;
        let ctx = ctx.clone();
        update::spawn(self.update.clone(), self.settings.auto_update, move || {
            ctx.request_repaint()
        });
    }

    pub fn toast(&mut self, text: impl Into<String>, kind: ToastKind) {
        self.toasts.push(Toast {
            text: text.into(),
            kind,
            born: Instant::now(),
            restart: false,
        });
    }

    fn poll_update(&mut self) {
        if self.update_reported {
            return;
        }
        let state = self.update.lock().unwrap().clone();
        match state {
            update::State::Ready { version } => {
                self.update_reported = true;
                self.toasts.push(Toast {
                    text: format!(
                        "Обновление {version} установлено. Перезапустите SpaceTree, чтобы начать им пользоваться."
                    ),
                    kind: ToastKind::Success,
                    born: Instant::now(),
                    restart: true,
                });
            }
            update::State::Available { version, .. } => {
                self.update_reported = true;
                self.toast(
                    format!("Доступна новая версия {version} — откройте настройки ⚙"),
                    ToastKind::Info,
                );
            }
            update::State::Failed(e) => {
                self.update_reported = true;
                // a background check failing (offline etc.) is not worth a popup; settings show it
                diag::warn(format!("update check failed: {e}"));
            }
            _ => {}
        }
    }

    pub fn start_scan(&mut self, ctx: &egui::Context) {
        if self.job.is_some() {
            return;
        }
        if !self.delete.idle() || !self.compress.idle() {
            self.toast("Сначала дождитесь окончания удаления или сжатия", ToastKind::Info);
            return;
        }
        if settings::is_demo() {
            self.load_demo();
            return;
        }
        let mut text = self.path_text.trim().trim_matches('"').to_owned();
        if cfg!(windows) && text.len() == 2 && text.ends_with(':') {
            text.push('\\'); // `C:` alone means "current directory on C:"
        }
        self.path_text = text.clone();
        let path = PathBuf::from(&text);
        let backend = self.backend;
        let save_snapshots = self.settings.save_snapshots;
        let progress = Arc::new(Progress::default());
        let (tx, rx) = channel();
        let (p, ctx2) = (progress.clone(), ctx.clone());
        std::thread::spawn(move || {
            let installed = Arc::new(Installed::new(programs::installed_names()));
            let res = spacetree_core::scan(&path, backend, &p).map(|out| {
                let t = Instant::now();
                let snapshot = Snapshot::from_tree(&out.tree, now_unix() as u64);
                let history = store_snapshot(&snapshot, save_snapshots);
                let mut l = Loaded {
                    out,
                    top: Vec::new(),
                    types: Vec::new(),
                    cleanup: Vec::new(),
                    cleanup_total: 0,
                    dup_candidates: Vec::new(),
                    dup_sets: None,
                    dup_keep: Default::default(),
                    snapshot: Some(snapshot),
                    history,
                    disk: None,
                    analysis: Duration::ZERO,
                    root_path: path,
                    installed,
                    ads: HashMap::new(),
                    ads_dirs: HashSet::new(),
                };
                l.index_streams();
                l.reanalyze();
                l.analysis = t.elapsed();
                l
            });
            let _ = tx.send(res);
            ctx2.request_repaint();
        });
        self.job = Some(Job {
            progress,
            rx,
            started: Instant::now(),
            path: text,
        });
        self.message.clear();
    }

    fn poll_job(&mut self) {
        let Some(job) = &self.job else { return };
        match job.rx.try_recv() {
            Ok(Ok(loaded)) => {
                let path = job.path.clone();
                let o = &loaded.out;
                diag::info(format!(
                    "scan finished: backend={} files={} dirs={} errors={} scan={:.1}s analysis={:.1}s",
                    o.selection.used.as_str(),
                    o.tree.root_stats().2,
                    o.tree.dir_count(),
                    o.errors,
                    o.elapsed.as_secs_f32(),
                    loaded.analysis.as_secs_f32(),
                ));
                let phases: Vec<String> = o
                    .timings
                    .iter()
                    .map(|(n, d)| format!("{}={:.2}s", n.trim(), d.as_secs_f32()))
                    .collect();
                if !phases.is_empty() {
                    diag::info(format!("scan phases: {}", phases.join(", ")));
                }
                for note in &o.notes {
                    diag::info(format!("scan: {note}"));
                }
                for sample in &o.error_samples {
                    diag::warn(format!("scan: {sample}"));
                }
                self.job = None;
                self.install(loaded, &path);
            }
            Ok(Err(e)) => {
                diag::error(format!("scan of {} failed: {e}", job.path));
                self.message = e.to_string();
                self.job = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                diag::error("scan thread terminated abnormally");
                self.message = "поток сканирования завершился аварийно".into();
                self.job = None;
            }
        }
    }

    /// Makes a finished scan the current data and resets everything that referred to the previous one.
    pub fn install(&mut self, loaded: Loaded, path: &str) {
        self.growth = match &loaded.snapshot {
            Some(cur) => growth::Growth::new(loaded.history.clone(), cur),
            None => growth::Growth::default(),
        };
        self.dup_job = None;
        self.dup_pending = None;
        self.data = Some(Arc::new(loaded));
        self.generation += 1;
        self.scans += 1;
        self.delete.tree_replaced();
        self.compress.tree_replaced();
        self.stop_live();
        self.tree_replaced_streams();
        self.live_autostart = self.settings.live_monitor;
        self.tm_morph = None;
        self.selected = None;
        self.tm_root = ROOT;
        self.tm = None;
        self.tm_anim = None;
        self.expanded = HashSet::from([ROOT]);
        self.rows_dirty = true;
        self.search_done.clear();
        self.search_hits.clear();
        self.cleanup_open.clear();
        self.marked.clear();
        self.highlight_ext = None;
        self.top_order_key = None;
        if !settings::is_demo() {
            self.drives = list_drives();
            self.settings.push_recent(path);
            self.settings.save();
        }
    }

    pub fn dup_running(&self) -> bool {
        self.dup_job.is_some()
    }

    /// Progress of the duplicate search: `(stage 0..=2, done, total, bytes read, elapsed)`.
    pub fn dup_progress(&self) -> Option<(u8, u64, u64, u64, Duration)> {
        use std::sync::atomic::Ordering::Relaxed;
        let j = self.dup_job.as_ref()?;
        let p = &j.progress;
        Some((
            p.stage.load(Relaxed),
            p.done.load(Relaxed),
            p.total.load(Relaxed),
            p.bytes.load(Relaxed),
            j.started.elapsed(),
        ))
    }

    pub fn start_dups(&mut self, ctx: &egui::Context) {
        let Some(data) = self.data.clone() else { return };
        if self.dup_job.is_some() {
            return;
        }
        let progress = Arc::new(DupProgress::default());
        let (tx, rx) = channel();
        let (p, ctx2) = (progress.clone(), ctx.clone());
        diag::info(format!(
            "duplicate search started: {} candidates",
            data.dup_candidates.len()
        ));
        std::thread::spawn(move || {
            let sets = dupes::find(&data.out.tree, &data.dup_candidates, &p);
            let _ = tx.send(sets);
            ctx2.request_repaint();
        });
        self.dup_job = Some(DupJob {
            progress,
            rx,
            started: Instant::now(),
        });
    }

    pub fn cancel_dups(&self) {
        if let Some(j) = &self.dup_job {
            j.progress.cancel();
        }
    }

    fn poll_dups(&mut self) {
        if let Some(job) = &self.dup_job {
            match job.rx.try_recv() {
                Ok(Some(sets)) => {
                    diag::info(format!(
                        "duplicate search finished: {} sets in {:.1}s",
                        sets.len(),
                        job.started.elapsed().as_secs_f32()
                    ));
                    self.dup_pending = Some(sets);
                    self.dup_job = None;
                }
                Ok(None) => {
                    diag::info("duplicate search cancelled");
                    self.dup_job = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    diag::error("duplicate search thread terminated abnormally");
                    self.dup_job = None;
                    self.toast("Поиск дубликатов прервался из-за внутренней ошибки", ToastKind::Error);
                }
            }
        }
        if self.dup_job.is_some() {
            return;
        }
        let Some(sets) = self.dup_pending.take() else { return };
        let Some(data) = self.data.as_mut().and_then(Arc::get_mut) else {
            self.dup_pending = Some(sets); // the tree is still borrowed elsewhere; retry next frame
            return;
        };
        data.dup_sets = Some(sets);
        data.merge_dups();
        self.cleanup_open.clear();
    }

    /// Called after nodes were removed from the tree.
    pub fn tree_changed(&mut self) {
        self.generation += 1;
        let Some(data) = self.data.clone() else { return };
        let tree = &data.out.tree;
        if self.selected.is_some_and(|s| tree.is_removed(s)) {
            self.selected = None;
        }
        while tree.is_removed(self.tm_root) {
            self.tm_root = tree.parent(self.tm_root).unwrap_or(ROOT);
        }
        self.marked.retain(|&m| !tree.is_removed(m));
        self.expanded.retain(|&e| !tree.is_removed(e));
        self.tm = None;
        self.tm_anim = None;
        self.rows_dirty = true;
        self.top_order_key = None;
        self.search_done.clear(); // re-run the query on the changed tree
    }

    pub fn rebuild_rows(&mut self) {
        self.rows.clear();
        self.rows_dirty = false;
        let Some(data) = self.data.clone() else { return };
        let tree = &data.out.tree;
        let mut stack = vec![(ROOT, 0u16)];
        while let Some((id, depth)) = stack.pop() {
            self.rows.push((id, depth));
            if tree.is_dir(id) && self.expanded.contains(&id) {
                let mut ch = tree.children(id).to_vec(); // size, descending
                match self.sort {
                    SortKey::Size => {}
                    SortKey::Name => ch.sort_by_cached_key(|&c| tree.name(c).to_lowercase()),
                    SortKey::Files => ch.sort_by_key(|&c| std::cmp::Reverse(tree.node(c).files)),
                    SortKey::Modified => ch.sort_by_key(|&c| std::cmp::Reverse(tree.node(c).mtime)),
                }
                let natural_desc = self.sort != SortKey::Name;
                if natural_desc != self.sort_desc {
                    ch.reverse();
                }
                // pushed in reverse so that the first child is popped (shown) first
                stack.extend(ch.into_iter().rev().map(|c| (c, depth + 1)));
            }
        }
    }

    /// Selects a node everywhere: expands its ancestors, points the treemap at its folder, scrolls the tree.
    pub fn select(&mut self, id: NodeId) {
        self.select_keep_view(id);
        let Some(data) = &self.data else { return };
        let tree = &data.out.tree;
        self.tm_root = if tree.is_dir(id) {
            id
        } else {
            tree.parent(id).unwrap_or(ROOT)
        };
    }

    /// Like `select`, but leaves the treemap where it is.
    pub fn select_keep_view(&mut self, id: NodeId) {
        let Some(data) = self.data.clone() else { return };
        for a in data.out.tree.ancestors(id) {
            if a != id {
                self.expanded.insert(a);
            }
        }
        self.selected = Some(id);
        self.rebuild_rows();
        self.scroll_to = self.rows.iter().position(|&(r, _)| r == id);
    }

    pub fn tm_up(&mut self) {
        let Some(data) = &self.data else { return };
        if self.tm_root != ROOT {
            self.tm_root = data.out.tree.parent(self.tm_root).unwrap_or(ROOT);
        }
    }

    pub fn apply(&mut self, ctx: &egui::Context, action: Action) {
        let Some(data) = self.data.clone() else { return };
        let tree = &data.out.tree;
        match action {
            Action::Select(id) => self.select_keep_view(id),
            Action::Show(id) => {
                self.tab = Tab::Overview;
                self.select(id);
            }
            Action::Toggle(id) => {
                if !self.expanded.remove(&id) {
                    self.expanded.insert(id);
                }
                self.rows_dirty = true;
            }
            Action::Reveal(id) => reveal(&tree.path(id)),
            Action::Open(id) => open_path(&tree.path(id)),
            Action::CopyPath(id) => {
                ctx.copy_text(tree.path(id));
                self.toast("Путь скопирован", ToastKind::Info);
            }
            Action::Delete(id, permanent) => self.request_delete(vec![id], permanent),
            Action::Compress(id, undo) => self.request_compress(id, undo),
        }
    }

    fn update_search(&mut self, ctx: &egui::Context) {
        let q = self.search.trim().to_lowercase();
        if q == self.search_done {
            return;
        }
        if let Some(t) = self.search_edited
            && t.elapsed() < SEARCH_DEBOUNCE
        {
            ctx.request_repaint_after(SEARCH_DEBOUNCE);
            return;
        }
        self.search_done = q.clone();
        self.search_hits.clear();
        self.search_truncated = false;
        let Some(data) = self.data.clone() else { return };
        if q.is_empty() {
            return;
        }
        use rayon::prelude::*;
        let tree = &data.out.tree;
        let mut hits: Vec<NodeId> = (1..tree.len() as NodeId)
            .into_par_iter()
            .filter(|&i| !tree.is_removed(i) && contains_ci(tree.name(i), &q))
            .collect();
        hits.par_sort_unstable_by_key(|&i| std::cmp::Reverse(tree.node(i).size));
        self.search_truncated = hits.len() > SEARCH_LIMIT;
        hits.truncate(SEARCH_LIMIT);
        sort_list(tree, &mut hits, self.search_sort);
        self.search_hits = hits;
    }

    pub fn resort_search(&mut self) {
        if let Some(data) = self.data.clone() {
            sort_list(&data.out.tree, &mut self.search_hits, self.search_sort);
        }
    }

    pub fn search_active(&self) -> bool {
        !self.search_done.is_empty()
    }

    pub fn clear_search(&mut self) {
        self.search.clear();
        self.search_done.clear();
        self.search_hits.clear();
    }

    pub fn export_csv(&mut self, data: Arc<Loaded>) {
        let Some(p) = rfd::FileDialog::new().set_file_name("spacetree.csv").save_file() else {
            return;
        };
        let status = self.status.clone();
        self.toast("Экспорт CSV…", ToastKind::Info);
        std::thread::spawn(move || {
            let res = std::fs::File::create(&p)
                .map(std::io::BufWriter::new)
                .and_then(|mut w| {
                    export::write_csv(&data.out.tree, &mut w)?;
                    std::io::Write::flush(&mut w)
                });
            *status.lock().unwrap() = Some(match res {
                Ok(()) => (format!("CSV сохранён: {}", p.display()), ToastKind::Success),
                Err(e) => (format!("Ошибка записи CSV: {e}"), ToastKind::Error),
            });
        });
    }

    fn app_ui(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.poll_job();
        self.poll_delete();
        self.poll_compress();
        if std::mem::take(&mut self.live_autostart) {
            self.start_live(&ctx);
        }
        self.poll_live(&ctx);
        self.poll_dups();
        self.poll_update();
        self.demo_step(&ctx);
        if let Some((text, kind)) = self.status.lock().unwrap().take() {
            self.toasts.push(Toast {
                text,
                kind,
                born: Instant::now(),
                restart: false,
            });
        }
        if self.auto_scan {
            self.auto_scan = false;
            self.start_scan(&ctx);
        }
        let typing = ctx.egui_wants_keyboard_input();
        if ctx.input(|i| i.key_pressed(Key::F5)) && self.data.is_some() {
            self.start_scan(&ctx);
        }
        if !typing && self.data.is_some() && self.delete.idle() && self.tab != Tab::Cleanup {
            let (del, shift) = ctx.input(|i| (i.key_pressed(Key::Delete), i.modifiers.shift));
            if del && let Some(id) = self.selected {
                self.request_delete(vec![id], shift);
            }
        }
        self.update_search(&ctx);

        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        if self.job.is_none() && self.data.is_some() {
            egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        }
        if self.job.is_some() {
            egui::CentralPanel::default().show(ui, |ui| self.scanning(ui));
        } else if self.data.is_none() {
            egui::CentralPanel::default().show(ui, |ui| {
                if !self.message.is_empty() {
                    ui.colored_label(ui.visuals().error_fg_color, &self.message);
                }
                self.welcome(ui)
            });
        } else {
            egui::Panel::top("tabs").show(ui, |ui| self.tabs(ui));
            match self.tab {
                Tab::Overview => {
                    egui::Panel::left("tree")
                        .resizable(true)
                        .default_size(660.0)
                        .min_size(320.0)
                        .show(ui, |ui| {
                            if self.search_active() {
                                self.search_view(ui)
                            } else {
                                self.tree_view(ui)
                            }
                        });
                    egui::CentralPanel::default().show(ui, |ui| self.treemap_view(ui));
                }
                Tab::Types => {
                    egui::CentralPanel::default().show(ui, |ui| self.types_view(ui));
                }
                Tab::Top => {
                    egui::CentralPanel::default().show(ui, |ui| self.top_view(ui));
                }
                Tab::Growth => {
                    egui::CentralPanel::default().show(ui, |ui| self.growth_view(ui));
                }
                Tab::Cleanup => {
                    if !self.marked.is_empty() {
                        egui::Panel::bottom("cleanup_bar").show(ui, |ui| self.cleanup_bar(ui));
                    }
                    egui::CentralPanel::default().show(ui, |ui| self.cleanup_view(ui));
                }
                Tab::Streams => {
                    egui::CentralPanel::default().show(ui, |ui| self.streams_view(ui));
                }
            }
        }
        self.delete_dialogs(&ctx);
        self.compress_dialogs(&ctx);
        self.stream_dialogs(&ctx);
        self.recover_window(&ctx);
        self.settings_window(&ctx);
        self.draw_toasts(&ctx);
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        match &self.mode {
            Mode::App => self.app_ui(ui),
            Mode::Install(_) | Mode::Uninstall(_) => self.installer_ui(ui),
        }
    }
}

fn image_from_rgba(bytes: &[u8], side: usize) -> Option<egui::ColorImage> {
    (bytes.len() == side * side * 4).then(|| egui::ColorImage::from_rgba_unmultiplied([side, side], bytes))
}

pub fn reveal(path: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("explorer")
            .raw_arg(format!("/select,\"{path}\""))
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let dir = std::path::Path::new(path)
            .parent()
            .unwrap_or(std::path::Path::new(path));
        let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
    }
}

pub fn open_path(path: &str) {
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(not(windows))]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

/// Starts this executable again through the UAC prompt, asking it to scan `path`.
pub fn relaunch_elevated(path: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let w = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
        let (verb, file) = (w("runas"), w(&exe.to_string_lossy()));
        // A trailing backslash before the closing quote would escape it (`"C:\"`): double it.
        let arg = if path.ends_with('\\') {
            format!("{path}\\")
        } else {
            path.to_owned()
        };
        let params = w(&format!("\"{arg}\""));
        // SAFETY: all strings are NUL-terminated and outlive the call; no window handle or directory.
        let h = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                file.as_ptr(),
                params.as_ptr(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        if h as isize > 32 {
            Ok(())
        } else {
            Err("не удалось перезапуститься от администратора (запрос UAC отклонён?)".into())
        }
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err("доступно только в Windows".into())
    }
}

fn main() -> eframe::Result {
    diag::install_panic_hook();
    if std::env::args().any(|a| a == "--demo" || a == "--screenshots") {
        settings::set_demo();
    }
    let mode = if std::env::args().any(|a| a == "--uninstall") {
        Mode::Uninstall(installer::Uninstall::default())
    } else if winint::is_setup_launch() {
        Mode::Install(installer::Install::default())
    } else {
        Mode::App
    };
    let (size, min, title) = match mode {
        Mode::App => ([1360.0, 860.0], [900.0, 560.0], "SpaceTree — анализ места на диске"),
        Mode::Install(_) => ([560.0, 440.0], [560.0, 440.0], "Установка SpaceTree"),
        Mode::Uninstall(_) => ([520.0, 300.0], [520.0, 300.0], "Удаление SpaceTree"),
    };
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size(size)
        .with_min_inner_size(min)
        .with_title(title)
        .with_app_id("SpaceTree");
    if let Some(img) = image_from_rgba(include_bytes!("../assets/icon64.rgba"), 64) {
        viewport = viewport.with_icon(egui::IconData {
            rgba: img.as_raw().to_vec(),
            width: 64,
            height: 64,
        });
    }
    if !matches!(mode, Mode::App) {
        viewport = viewport.with_resizable(false).with_maximize_button(false);
    }
    let opts = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native("SpaceTree", opts, Box::new(|cc| Ok(Box::new(App::new(cc, mode)))))
}
