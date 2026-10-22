//! Recovery of deleted files: window with the scan of a drive's `$MFT`, filters, and restoring to another drive.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};

use eframe::egui::{self, Align2, Color32, RichText, Ui, vec2};
use spacetree_core::stats::{format_date, human_size};
use spacetree_core::{Progress, ScanError};

use crate::{App, ToastKind, relaunch_elevated, theme};

#[cfg(windows)]
use spacetree_core::undelete::{self, Chance, DeletedFile, UndeleteScan};

/// Without the Windows-only core module the window has nothing to show.
#[cfg(not(windows))]
mod undelete {
    use std::collections::HashSet;

    #[derive(Clone)]
    pub struct UndeleteScan {
        pub files: Vec<DeletedFile>,
        pub records: u64,
        pub skipped: u64,
        pub ssd: Option<bool>,
    }
    impl UndeleteScan {
        pub fn start_lcns(&self) -> HashSet<u64> {
            HashSet::new()
        }
    }
    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    pub enum Chance {
        Good,
        Partial,
        Carved,
        Lost,
        Unsupported,
    }
    #[derive(Clone)]
    pub struct DeletedFile {
        pub name: String,
        pub dir: String,
        pub size: u64,
        pub mtime: u32,
        pub chance: Chance,
        pub intact: u8,
    }
    pub fn scan(_: char, _: &spacetree_core::Progress) -> Result<UndeleteScan, spacetree_core::ScanError> {
        Err(spacetree_core::ScanError::Unsupported("только Windows".into()))
    }
}
#[cfg(not(windows))]
use undelete::{Chance, DeletedFile, UndeleteScan};

#[cfg(windows)]
use spacetree_core::carve;
#[cfg(not(windows))]
mod carve {
    pub fn carve(
        _: char,
        _: &std::collections::HashSet<u64>,
        _: &spacetree_core::Progress,
    ) -> Result<Vec<super::DeletedFile>, spacetree_core::ScanError> {
        Err(spacetree_core::ScanError::Unsupported("только Windows".into()))
    }
}

const ROW_H: f32 = 22.0;

#[derive(PartialEq, Clone, Copy)]
enum Sort {
    Size,
    Date,
    Name,
    Chance,
}

struct ScanJob {
    progress: Arc<Progress>,
    rx: Receiver<Result<UndeleteScan, ScanError>>,
    drive: char,
}

struct RestoreJob {
    progress: Arc<Progress>,
    rx: Receiver<RestoreReport>,
    done: Arc<AtomicUsize>,
    total: usize,
    current: Arc<Mutex<String>>,
    dest: PathBuf,
}

struct RestoreReport {
    ok: usize,
    failed: Vec<String>,
    zeroed: usize,
    damaged: usize,
}

struct DeepJob {
    progress: Arc<Progress>,
    rx: Receiver<Result<Vec<DeletedFile>, ScanError>>,
}

pub struct Recover {
    pub open: bool,
    deep: Option<DeepJob>,
    selected: Option<usize>,
    preview: String,
    drive: char,
    scan: Option<ScanJob>,
    result: Option<Arc<UndeleteScan>>,
    show_lost: bool,
    query: String,
    min_size_mb: u32,
    sort: (Sort, bool),
    view: Vec<usize>,
    dirty: bool,
    marked: HashSet<usize>,
    dest: String,
    keep_dirs: bool,
    restore: Option<RestoreJob>,
    note: String,
}

impl Default for Recover {
    fn default() -> Self {
        Recover {
            open: false,
            deep: None,
            selected: None,
            preview: String::new(),
            drive: 'C',
            scan: None,
            result: None,
            show_lost: false,
            query: String::new(),
            min_size_mb: 0,
            sort: (Sort::Size, true),
            view: Vec::new(),
            dirty: true,
            marked: HashSet::new(),
            dest: String::new(),
            keep_dirs: true,
            restore: None,
            note: String::new(),
        }
    }
}

fn shown_by_default(c: Chance) -> bool {
    matches!(c, Chance::Good | Chance::Partial | Chance::Carved)
}

/// Short description of file content for the preview: a type guess and a text excerpt or a hex dump.
fn describe(head: &[u8]) -> String {
    if head.is_empty() {
        return "не удалось прочитать начало файла".into();
    }
    let kind = [
        (&b"\x89PNG"[..], "PNG"),
        (&[0xFF, 0xD8, 0xFF][..], "JPEG"),
        (&b"%PDF"[..], "PDF"),
        (&b"PK\x03\x04"[..], "ZIP/Office"),
        (&b"GIF8"[..], "GIF"),
        (&b"SQLite format 3"[..], "SQLite"),
        (&[0x37, 0x7A, 0xBC, 0xAF][..], "7z"),
        (&b"Rar!"[..], "RAR"),
        (&b"MZ"[..], "EXE/DLL"),
        (&[0xD0, 0xCF, 0x11, 0xE0][..], "Office 97-2003"),
    ]
    .iter()
    .find(|(m, _)| head.starts_with(m))
    .map(|(_, n)| *n)
    .or_else(|| (head.len() > 8 && &head[4..8] == b"ftyp").then_some("MP4/MOV"));
    if head.iter().all(|&b| b == 0) {
        return "начало файла — нули (данные стёрты или TRIM)".into();
    }
    let text = std::str::from_utf8(head).ok().or_else(|| {
        // a multi-byte character may be cut at the end of the excerpt
        std::str::from_utf8(&head[..head.len().saturating_sub(3)]).ok()
    });
    if let Some(t) = text.filter(|t| kind.is_none() && t.chars().all(|c| !c.is_control() || c.is_whitespace())) {
        let excerpt: String = t.chars().take(300).collect();
        return format!("текст: {excerpt}");
    }
    let hex: Vec<String> = head.iter().take(48).map(|b| format!("{b:02X}")).collect();
    format!("{}: {}", kind.unwrap_or("двоичные данные"), hex.join(" "))
}

fn chance_style(c: Chance) -> (&'static str, Color32) {
    match c {
        Chance::Good => ("целый", theme::SAFE),
        Chance::Partial => ("частично", theme::LIKELY),
        Chance::Carved => ("по сигнатуре", theme::ACCENT),
        Chance::Lost => ("перезаписан", theme::REVIEW),
        Chance::Unsupported => ("сжат/зашифрован", Color32::from_gray(120)),
    }
}

impl Recover {
    /// Preselects the drive unless a scan or its result is on screen.
    pub fn set_drive(&mut self, drive: char) {
        if self.scan.is_none() && self.result.is_none() {
            self.drive = drive;
        }
    }

    fn rebuild(&mut self) {
        let Some(res) = &self.result else {
            self.view.clear();
            return;
        };
        let q = self.query.to_lowercase();
        let min = u64::from(self.min_size_mb) * 1024 * 1024;
        let show_lost = self.show_lost;
        let mut v: Vec<usize> = res
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| {
                (show_lost || shown_by_default(f.chance))
                    && f.size >= min
                    && (q.is_empty() || f.name.to_lowercase().contains(&q) || f.dir.to_lowercase().contains(&q))
            })
            .map(|(i, _)| i)
            .collect();
        let (key, desc) = self.sort;
        let files = &res.files;
        match key {
            Sort::Size => v.sort_by_key(|&i| std::cmp::Reverse(files[i].size)),
            Sort::Date => v.sort_by_key(|&i| std::cmp::Reverse(files[i].mtime)),
            Sort::Name => v.sort_by_cached_key(|&i| files[i].name.to_lowercase()),
            Sort::Chance => v.sort_by_key(|&i| {
                (
                    files[i].chance,
                    std::cmp::Reverse(files[i].intact),
                    std::cmp::Reverse(files[i].size),
                )
            }),
        }
        if (key != Sort::Name && key != Sort::Chance) != desc {
            v.reverse();
        }
        self.view = v;
        self.dirty = false;
    }
}

impl App {
    pub fn recover_window(&mut self, ctx: &egui::Context) {
        self.poll_recover(ctx);
        if !self.recover.open {
            return;
        }
        let mut open = true;
        egui::Window::new("Восстановление удалённых файлов")
            .open(&mut open)
            .collapsible(false)
            .default_size(vec2(980.0, 640.0))
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .show(ctx, |ui| self.recover_ui(ui));
        self.recover.open = open;
    }

    fn poll_recover(&mut self, ctx: &egui::Context) {
        if let Some(job) = &self.recover.scan {
            match job.rx.try_recv() {
                Ok(res) => {
                    let drive = job.drive;
                    self.recover.scan = None;
                    match res {
                        Ok(r) => {
                            crate::diag::info(format!("undelete scan {drive}: {} candidates", r.files.len()));
                            self.recover.result = Some(Arc::new(r));
                            self.recover.marked.clear();
                            self.recover.note.clear();
                            self.recover.dirty = true;
                        }
                        Err(ScanError::Cancelled) => self.recover.note = "Скан отменён".into(),
                        Err(e) => self.recover.note = e.to_string(),
                    }
                }
                Err(_) => ctx.request_repaint_after(std::time::Duration::from_millis(100)),
            }
        }
        if let Some(job) = &self.recover.deep {
            match job.rx.try_recv() {
                Ok(res) => {
                    self.recover.deep = None;
                    match res {
                        Ok(found) => {
                            let n = found.len();
                            if let Some(cur) = self.recover.result.take() {
                                let mut merged = (*cur).clone();
                                merged.files.extend(found);
                                self.recover.result = Some(Arc::new(merged));
                            }
                            self.recover.dirty = true;
                            self.recover.note = format!("Глубокий поиск: найдено файлов по сигнатурам: {n}");
                        }
                        Err(ScanError::Cancelled) => self.recover.note = "Глубокий поиск отменён".into(),
                        Err(e) => self.recover.note = e.to_string(),
                    }
                }
                Err(_) => ctx.request_repaint_after(std::time::Duration::from_millis(100)),
            }
        }
        if let Some(job) = &self.recover.restore {
            match job.rx.try_recv() {
                Ok(rep) => {
                    let dest = job.dest.clone();
                    self.recover.restore = None;
                    let mut msg = format!("Восстановлено файлов: {}", rep.ok);
                    if rep.damaged > 0 {
                        msg += &format!(", возможно повреждены: {}", rep.damaged);
                    }
                    if rep.zeroed > 0 {
                        msg += &format!(", пустые (данные затёрты/TRIM): {}", rep.zeroed);
                    }
                    if !rep.failed.is_empty() {
                        msg += &format!(", ошибок: {} ({})", rep.failed.len(), rep.failed[0]);
                    }
                    let kind = if rep.failed.is_empty() && rep.ok > 0 {
                        ToastKind::Success
                    } else {
                        ToastKind::Error
                    };
                    self.toast(msg.clone(), kind);
                    self.recover.note = format!("{msg}  →  {}", dest.display());
                }
                Err(_) => ctx.request_repaint_after(std::time::Duration::from_millis(100)),
            }
        }
    }

    fn start_deep_scan(&mut self, ctx: &egui::Context) {
        let Some(res) = self.recover.result.clone() else { return };
        // The drive the result came from, not whatever the combo shows now.
        let drive = res.drive;
        let known = res.start_lcns();
        let progress = Arc::new(Progress::default());
        let (tx, rx) = channel();
        let (p, ctx2) = (progress.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(carve::carve(drive, &known, &p));
            ctx2.request_repaint();
        });
        self.recover.note.clear();
        self.recover.deep = Some(DeepJob { progress, rx });
    }

    /// Reads the beginning of file `i` for the preview (opens the volume for a moment).
    #[cfg(windows)]
    fn load_preview(&mut self, i: usize) {
        let Some(res) = self.recover.result.clone() else { return };
        self.recover.selected = Some(i);
        self.recover.preview = match undelete::Volume::open(res.drive) {
            Ok(v) => describe(&v.head(&res.files[i], 512)),
            Err(e) => e.to_string(),
        };
    }

    #[cfg(not(windows))]
    fn load_preview(&mut self, i: usize) {
        self.recover.selected = Some(i);
    }

    fn start_recover_scan(&mut self, ctx: &egui::Context) {
        let drive = self.recover.drive;
        let progress = Arc::new(Progress::default());
        let (tx, rx) = channel();
        let (p, ctx2) = (progress.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(undelete::scan(drive, &p));
            ctx2.request_repaint();
        });
        self.recover.result = None;
        self.recover.selected = None;
        self.recover.preview.clear();
        self.recover.view.clear();
        self.recover.note.clear();
        self.recover.scan = Some(ScanJob { progress, rx, drive });
    }

    #[cfg(windows)]
    fn start_restore(&mut self, ctx: &egui::Context) {
        let Some(res) = self.recover.result.clone() else { return };
        // Cluster numbers of the result only mean something on the volume that was scanned.
        let drive = res.drive;
        let mut ids: Vec<usize> = self.recover.marked.iter().copied().collect();
        ids.sort_unstable();
        let dest = PathBuf::from(self.recover.dest.trim());
        let keep_dirs = self.recover.keep_dirs;
        let progress = Arc::new(Progress::default());
        let done = Arc::new(AtomicUsize::new(0));
        let current = Arc::new(Mutex::new(String::new()));
        let (tx, rx) = channel();
        let (p, d2, c2, ctx2, dest2) = (
            progress.clone(),
            done.clone(),
            current.clone(),
            ctx.clone(),
            dest.clone(),
        );
        let total = ids.len();
        std::thread::spawn(move || {
            let mut rep = RestoreReport {
                ok: 0,
                failed: Vec::new(),
                zeroed: 0,
                damaged: 0,
            };
            match undelete::Volume::open(drive) {
                Err(e) => rep.failed.push(e.to_string()),
                Ok(vol) => {
                    for i in ids {
                        let f: &DeletedFile = &res.files[i];
                        *c2.lock().unwrap() = f.path();
                        match vol.restore(f, &dest2, keep_dirs, &p) {
                            Ok(r) => {
                                rep.ok += 1;
                                rep.zeroed += usize::from(r.all_zero);
                                rep.damaged += usize::from(f.chance != Chance::Good || r.read_errors > 0);
                            }
                            Err(ScanError::Cancelled) => break,
                            Err(e) => rep.failed.push(format!("{}: {e}", f.name)),
                        }
                        d2.fetch_add(1, Relaxed);
                    }
                }
            }
            let _ = tx.send(rep);
            ctx2.request_repaint();
        });
        self.recover.restore = Some(RestoreJob {
            progress,
            rx,
            done,
            total,
            current,
            dest,
        });
    }

    #[cfg(not(windows))]
    fn start_restore(&mut self, _: &egui::Context) {}

    fn recover_ui(&mut self, ui: &mut Ui) {
        let busy_scan = self.recover.scan.is_some() || self.recover.deep.is_some();
        let busy_restore = self.recover.restore.is_some();

        ui.horizontal(|ui| {
            ui.add_enabled_ui(!busy_scan && !busy_restore, |ui| {
                ui.label("Диск:");
                let mut drive = self.recover.drive;
                egui::ComboBox::from_id_salt("recover_drive")
                    .selected_text(format!("{drive}:"))
                    .show_ui(ui, |ui| {
                        for d in &self.drives {
                            if let Some(c) = d.path.chars().next() {
                                ui.selectable_value(&mut drive, c, d.path.clone());
                            }
                        }
                    });
                self.recover.drive = drive;
                if theme::primary_button(ui, "🔍 Сканировать").clicked() {
                    if self.mft_ready {
                        let ctx = ui.ctx().clone();
                        self.start_recover_scan(&ctx);
                    } else {
                        self.recover.note = "Нужны права администратора (чтение $MFT)".into();
                    }
                }
            });
            if busy_scan && ui.button("⏹ Отмена").clicked() {
                let job = self.recover.scan.as_ref().map(|j| &j.progress);
                job.or(self.recover.deep.as_ref().map(|j| &j.progress))
                    .inspect(|p| p.cancel());
            }
            if !self.mft_ready && ui.button("Перезапустить от администратора").clicked() {
                let path = format!("{}:\\", self.recover.drive);
                match relaunch_elevated(&path) {
                    Ok(()) => std::process::exit(0),
                    Err(e) => self.recover.note = e,
                }
            }
        });

        if !self.mft_ready {
            ui.colored_label(
                theme::LIKELY,
                "Для чтения служебных данных тома нужен запуск от администратора.",
            );
        }
        if !self.recover.note.is_empty() {
            ui.label(RichText::new(&self.recover.note).weak());
        }

        let running = self
            .recover
            .scan
            .as_ref()
            .map(|j| (&j.progress, "Чтение таблицы файлов"))
            .or(self
                .recover
                .deep
                .as_ref()
                .map(|j| (&j.progress, "Поиск по сигнатурам в свободном месте")));
        if let Some((progress, what)) = running {
            ui.add_space(8.0);
            let analysing = progress.phase.load(Relaxed) == spacetree_core::progress::PHASE_BUILDING;
            let frac = if analysing {
                1.0
            } else {
                progress.fraction().unwrap_or(0.0)
            };
            ui.add(egui::ProgressBar::new(frac).show_percentage());
            ui.label(
                RichText::new(format!(
                    "{what}{}. Найдено: {}",
                    if analysing {
                        " — измерение найденных файлов"
                    } else {
                        ""
                    },
                    progress.files.load(Relaxed)
                ))
                .weak()
                .small(),
            );
            return;
        }
        let Some(res) = self.recover.result.clone() else {
            ui.add_space(8.0);
            ui.label(
                RichText::new(
                    "Удалённые файлы остаются на диске, пока их место не займут другие. Сканирование читает \
                     таблицу файлов (MFT) и проверяет, не занято ли место удалённых файлов — это быстро.\n\
                     На SSD с TRIM данные часто стираются сразу. Восстанавливайте на другой диск.",
                )
                .weak(),
            );
            return;
        };

        if self.recover.dirty {
            self.recover.rebuild();
        }
        let (good, partial) = res.files.iter().fold((0, 0), |(g, p), f| match f.chance {
            Chance::Good => (g + 1, p),
            Chance::Partial => (g, p + 1),
            _ => (g, p),
        });
        let (lost, lost_max) = res
            .files
            .iter()
            .filter(|f| matches!(f.chance, Chance::Lost))
            .fold((0usize, 0u64), |(n, m), f| (n + 1, m.max(f.size)));
        if res.ssd == Some(true) {
            ui.colored_label(
                theme::LIKELY,
                "SSD: Windows обычно стирает данные удалённых файлов (TRIM) почти сразу, шансы ниже.",
            );
        }
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            let mut changed = false;
            ui.label("Поиск:");
            changed |= ui
                .add(egui::TextEdit::singleline(&mut self.recover.query).desired_width(180.0))
                .changed();
            ui.label("от");
            changed |= ui
                .add(
                    egui::DragValue::new(&mut self.recover.min_size_mb)
                        .range(0..=100_000)
                        .suffix(" МБ"),
                )
                .changed();
            changed |= ui
                .checkbox(&mut self.recover.show_lost, "показывать перезаписанные")
                .changed();
            ui.label(
                RichText::new(format!(
                    "целых {good}, частично {partial}, перезаписано {lost}{}, всего записей {} (показано {})",
                    if lost > 0 {
                        format!(" (самый большой {})", human_size(lost_max))
                    } else {
                        String::new()
                    },
                    res.files.len(),
                    self.recover.view.len()
                ))
                .weak(),
            );
            if ui
                .add_enabled(!busy_restore, egui::Button::new("🧬 Глубокий поиск"))
                .on_hover_text(
                    "Искать файлы по сигнатурам (JPG, PNG, PDF, ZIP/Office, MP4, …) в свободном месте. Долго.",
                )
                .clicked()
            {
                let ctx = ui.ctx().clone();
                self.start_deep_scan(&ctx);
            }
            if changed {
                self.recover.dirty = true;
            }
        });

        let mut sort = self.recover.sort;
        let view = self.recover.view.clone();
        ui.horizontal(|ui| {
            ui.add_space(26.0);
            for (k, label, w) in [
                (Sort::Chance, "Шанс", 110.0),
                (Sort::Size, "Размер", 80.0),
                (Sort::Date, "Изменён", 100.0),
                (Sort::Name, "Имя / папка", 300.0),
            ] {
                let arrow = if sort.0 == k {
                    if sort.1 { " ▼" } else { " ▲" }
                } else {
                    ""
                };
                if ui
                    .add_sized([w, 20.0], egui::Button::new(format!("{label}{arrow}")).frame(false))
                    .clicked()
                {
                    sort = if sort.0 == k {
                        (k, !sort.1)
                    } else {
                        (k, k != Sort::Name && k != Sort::Chance)
                    };
                }
            }
        });
        if sort != self.recover.sort {
            self.recover.sort = sort;
            self.recover.dirty = true;
        }

        let bottom_h = if self.recover.selected.is_some() { 118.0 } else { 74.0 };
        let list_h = (ui.available_height() - bottom_h).max(120.0);
        let weak = ui.visuals().weak_text_color();
        let mut toggles: Vec<(usize, bool)> = Vec::new();
        let mut clicked_row: Option<usize> = None;
        let selected = self.recover.selected;
        egui::ScrollArea::vertical()
            .max_height(list_h)
            .auto_shrink([false, false])
            .show_rows(ui, ROW_H, view.len(), |ui, range| {
                for r in range {
                    let i = view[r];
                    let f = &res.files[i];
                    let (label, color) = chance_style(f.chance);
                    ui.horizontal(|ui| {
                        let mut on = self.recover.marked.contains(&i);
                        if ui.checkbox(&mut on, "").changed() {
                            toggles.push((i, on));
                        }
                        let pct = if matches!(f.chance, Chance::Partial) {
                            format!("{label} {}%", f.intact)
                        } else {
                            label.to_owned()
                        };
                        ui.add_sized([110.0, ROW_H], egui::Label::new(RichText::new(pct).color(color)));
                        ui.add_sized([80.0, ROW_H], egui::Label::new(human_size(f.size)));
                        ui.add_sized(
                            [100.0, ROW_H],
                            egui::Label::new(RichText::new(format_date(f.mtime)).color(weak)),
                        );
                        if ui.selectable_label(selected == Some(i), &f.name).clicked() {
                            clicked_row = Some(i);
                        }
                        ui.label(RichText::new(&f.dir).color(weak).small());
                    });
                }
            });
        if let Some(i) = clicked_row {
            self.load_preview(i);
        }
        for (i, on) in toggles {
            if on {
                self.recover.marked.insert(i);
            } else {
                self.recover.marked.remove(&i);
            }
        }

        ui.separator();
        if let (Some(i), false) = (self.recover.selected, self.recover.preview.is_empty()) {
            let f = &res.files[i];
            ui.label(RichText::new(f.path()).strong());
            ui.label(RichText::new(&self.recover.preview).monospace().small());
        }
        if let Some(j) = &self.recover.restore {
            let (done, total) = (j.done.load(Relaxed), j.total);
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new());
                ui.label(format!("Восстановление {done}/{total}: {}", j.current.lock().unwrap()));
            });
            let cur = j.progress.total_units.load(Relaxed).max(1);
            ui.add(egui::ProgressBar::new(
                (j.progress.done_units.load(Relaxed) as f64 / cur as f64).min(1.0) as f32,
            ));
            if ui.button("⏹ Остановить").clicked() {
                j.progress.cancel();
            }
            return;
        }
        ui.horizontal(|ui| {
            if ui.button("Выбрать все показанные").clicked() {
                let cap = view.iter().filter(|&&i| shown_by_default(res.files[i].chance));
                self.recover.marked.extend(cap.copied());
            }
            if ui.button("Снять выбор").clicked() {
                self.recover.marked.clear();
            }
            let size: u64 = self.recover.marked.iter().map(|&i| res.files[i].size).sum();
            ui.label(format!("Выбрано {} ({})", self.recover.marked.len(), human_size(size)));
        });
        ui.horizontal(|ui| {
            ui.label("Куда:");
            ui.add(
                egui::TextEdit::singleline(&mut self.recover.dest)
                    .hint_text("папка на ДРУГОМ диске")
                    .desired_width(380.0),
            );
            if ui.button("Папка…").clicked()
                && let Some(p) = rfd::FileDialog::new().pick_folder()
            {
                self.recover.dest = p.to_string_lossy().into_owned();
            }
            ui.checkbox(&mut self.recover.keep_dirs, "сохранять папки");
            let ready = !self.recover.marked.is_empty() && !self.recover.dest.trim().is_empty();
            ui.add_enabled_ui(ready, |ui| {
                if theme::primary_button(ui, "♻ Восстановить").clicked() {
                    let ctx = ui.ctx().clone();
                    self.start_restore(&ctx);
                }
            });
        });
    }
}
