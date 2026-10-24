//! "Compress with LZX (Windows Compact)": confirmation, background run with progress, result.
//!
//! The work itself is `spacetree_core::compress`; this is the dialogs around it.

use std::io;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Instant;

use eframe::egui::{self, Color32, Id, Key, RichText};
use spacetree_core::compress::{CompressProgress, Report};
use spacetree_core::stats::human_size;
use spacetree_core::{Kind, NodeId, ROOT};

use crate::{App, ToastKind, settings, theme};

pub struct Pending {
    path: String,
    size: u64,
    undo: bool,
    /// `App::scans` when asked: the node id is only valid for that tree.
    scan: u64,
    /// Inside Program Files / ProgramData: allowed, but with a warning.
    system: bool,
}

struct Running {
    rx: Receiver<io::Result<Report>>,
    progress: Arc<CompressProgress>,
    started: Instant,
    undo: bool,
    name: String,
}

#[derive(Default)]
pub struct CompressState {
    pending: Option<Pending>,
    running: Option<Running>,
}

impl CompressState {
    pub fn idle(&self) -> bool {
        self.pending.is_none() && self.running.is_none()
    }

    pub fn running(&self) -> bool {
        self.running.is_some()
    }

    /// A new tree was installed: a confirmation on screen refers to a node of the old one.
    pub fn tree_replaced(&mut self) {
        self.pending = None;
    }
}

/// Windows itself: boot files and system components must not be compressed behind the system's back.
fn in_windows_dir(path: &str) -> bool {
    let p = path.to_lowercase().replace('/', "\\");
    let p = p.strip_prefix(r"\\?\").unwrap_or(&p);
    p.contains(":\\windows\\") || p.ends_with(":\\windows")
}

fn report_text(r: &Report, undo: bool) -> String {
    let skipped = if r.skipped > 0 {
        format!(" · пропущено {} (мелкие, уже сжатые и занятые файлы)", r.skipped)
    } else {
        String::new()
    };
    if undo {
        format!(
            "Сжатие снято с файлов: {} · на диске заняли {} → {}{skipped}",
            r.changed,
            human_size(r.before),
            human_size(r.after)
        )
    } else {
        format!(
            "Сжато файлов: {} · {} → {} · освобождено {}{skipped}",
            r.changed,
            human_size(r.before),
            human_size(r.after),
            human_size(r.saved().max(0) as u64)
        )
    }
}

impl App {
    pub fn request_compress(&mut self, id: NodeId, undo: bool) {
        let Some(data) = self.data.clone() else { return };
        let tree = &data.out.tree;
        if settings::is_demo() {
            self.toast("В демо-режиме ничего не сжимается", ToastKind::Info);
            return;
        }
        if !self.compress.idle() || !self.delete.idle() {
            self.toast("Дождитесь окончания текущей операции", ToastKind::Info);
            return;
        }
        let path = tree.path(id);
        let root_text = tree.name(ROOT);
        let is_volume_root = id == ROOT && root_text.len() <= 3;
        if tree.is_removed(id) || tree.node(id).kind == Kind::Link {
            self.toast(
                "Ссылки (junction, symlink) не сжимаются — выберите саму папку",
                ToastKind::Error,
            );
            return;
        }
        if is_volume_root || in_windows_dir(&path) {
            self.toast(
                "Системные файлы Windows и корень диска не сжимаются: для этого есть «compact /compactos»",
                ToastKind::Error,
            );
            return;
        }
        self.compress.pending = Some(Pending {
            size: tree.node(id).alloc,
            system: super::delete::is_system(tree, id),
            path,
            undo,
            scan: self.scans,
        });
    }

    fn start_compress(&mut self, ctx: &egui::Context, p: Pending) {
        if p.scan != self.scans {
            self.toast(
                "Список изменился после пересканирования — выберите папку заново",
                ToastKind::Error,
            );
            return;
        }
        let progress = Arc::new(CompressProgress::default());
        let (tx, rx) = channel();
        let (pr, ctx2, path, undo) = (progress.clone(), ctx.clone(), p.path.clone(), p.undo);
        crate::diag::info(format!(
            "{} started: {path}",
            if undo { "LZX decompression" } else { "LZX compression" }
        ));
        std::thread::spawn(move || {
            let res = spacetree_core::compress::run(&spacetree_core::fs_path(&path), undo, &pr);
            let _ = tx.send(res);
            ctx2.request_repaint();
        });
        let name = std::path::Path::new(&p.path)
            .file_name()
            .map_or_else(|| p.path.clone(), |n| n.to_string_lossy().into_owned());
        self.compress.running = Some(Running {
            rx,
            progress,
            started: Instant::now(),
            undo,
            name,
        });
    }

    pub fn poll_compress(&mut self) {
        let Some(r) = &self.compress.running else { return };
        let (undo, name) = (r.undo, r.name.clone());
        match r.rx.try_recv() {
            Ok(Ok(rep)) => {
                crate::diag::info(format!("LZX run finished: {rep:?}"));
                self.compress.running = None;
                let text = report_text(&rep, undo);
                if rep.cancelled {
                    self.toast(format!("Остановлено. {text}"), ToastKind::Info);
                } else if rep.changed == 0 && rep.failed == 0 {
                    self.toast(
                        format!("«{name}»: нечего менять — {} файлов пропущено", rep.skipped),
                        ToastKind::Info,
                    );
                } else {
                    self.toast(
                        format!("{text}. Размеры в списке обновит повторное сканирование (F5)"),
                        ToastKind::Success,
                    );
                }
                if rep.failed > 0 {
                    self.toast(
                        format!(
                            "Не удалось обработать файлов: {} (первый: {})",
                            rep.failed,
                            rep.first_error.as_deref().unwrap_or("?")
                        ),
                        ToastKind::Error,
                    );
                }
            }
            Ok(Err(e)) => {
                crate::diag::warn(format!("LZX run failed: {e}"));
                self.compress.running = None;
                self.toast(format!("Не удалось: {e}"), ToastKind::Error);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.compress.running = None;
                self.toast("Операция прервалась из-за внутренней ошибки", ToastKind::Error);
            }
        }
    }

    pub fn compress_dialogs(&mut self, ctx: &egui::Context) {
        if let Some(r) = &self.compress.running {
            let (done, total) = (r.progress.done.load(Relaxed), r.progress.total.load(Relaxed));
            let current = r.progress.current.lock().map(|c| c.clone()).unwrap_or_default();
            let (undo, name, started) = (r.undo, r.name.clone(), r.started);
            let mut cancel = false;
            egui::Modal::new(Id::new("compressing")).show(ctx, |ui| {
                ui.set_width(440.0);
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new());
                    let what = if undo {
                        "Снятие сжатия"
                    } else {
                        "Сжатие LZX"
                    };
                    ui.label(RichText::new(format!("{what}: {name}")).strong());
                });
                if total == 0 {
                    ui.label(RichText::new("Просмотр папки…").weak());
                } else {
                    ui.add(egui::ProgressBar::new((done as f64 / total as f64).min(1.0) as f32).show_percentage());
                    ui.label(
                        RichText::new(format!("{done} из {total} файлов · {} с", started.elapsed().as_secs())).weak(),
                    );
                }
                ui.label(RichText::new(current).weak().small());
                if ui.button("Остановить").clicked() {
                    cancel = true;
                }
            });
            if cancel && let Some(r) = &self.compress.running {
                r.progress.cancel();
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            return;
        }
        let Some(p) = &self.compress.pending else { return };
        let mut decision = None;
        egui::Modal::new(Id::new("confirm_compress")).show(ctx, |ui| {
            ui.set_width(500.0);
            let verb = if p.undo { "Снять сжатие LZX с" } else { "Сжать алгоритмом LZX" };
            let name = std::path::Path::new(&p.path)
                .file_name()
                .map_or_else(|| p.path.clone(), |n| n.to_string_lossy().into_owned());
            ui.label(RichText::new(format!("{verb} «{name}»?")).size(17.0).strong());
            ui.label(RichText::new(&p.path).weak().small());
            ui.add_space(4.0);
            if p.undo {
                ui.label(format!(
                    "Файлы будут снова храниться без сжатия: займут больше места (сейчас на диске около {}).",
                    human_size(p.size)
                ));
            } else {
                ui.label(format!(
                    "Сжатие встроено в NTFS (Windows Compact): файлы остаются на тех же местах, программы запускаются \
                     и читают их как обычно. Обычно это экономит 30–50% на программах, играх и ассетах. Сейчас на \
                     диске около {}.",
                    human_size(p.size)
                ));
                ui.add_space(2.0);
                ui.label(
                    RichText::new(
                        "Цена — немного процессорного времени при чтении и более медленная запись, поэтому не сжимайте \
                         папки, в которые программы часто пишут (базы данных, логи, рабочие проекты). Уже сжатые форматы \
                         (zip, jpg, mp4…), мелкие и занятые файлы пропускаются. Отменить можно командой «Снять сжатие».",
                    )
                    .weak(),
                );
            }
            if p.system {
                ui.add_space(2.0);
                ui.label(
                    RichText::new("⚠ Это папка установленных программ: сжимайте её, только если понимаете, зачем.")
                        .color(theme::REVIEW),
                );
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let label = if p.undo { "Снять сжатие" } else { "Сжать" };
                if ui
                    .add(egui::Button::new(RichText::new(label).color(Color32::WHITE).strong()).fill(theme::ACCENT))
                    .clicked()
                    || ui.input(|i| i.key_pressed(Key::Enter))
                {
                    decision = Some(true);
                }
                if ui.button("Отмена").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    decision = Some(false);
                }
            });
        });
        match decision {
            Some(true) => {
                let p = self.compress.pending.take().unwrap();
                self.start_compress(ctx, p);
            }
            Some(false) => self.compress.pending = None,
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_directory_is_recognised() {
        assert!(in_windows_dir(r"C:\Windows"));
        assert!(in_windows_dir(r"c:\WINDOWS\System32\drivers"));
        assert!(in_windows_dir(r"\\?\C:\Windows\WinSxS"));
        assert!(!in_windows_dir(r"C:\Program Files\Windows Defender"));
        assert!(!in_windows_dir(r"D:\Games\Windowsill"));
    }

    #[test]
    fn report_wording() {
        let r = Report {
            files: 10,
            changed: 4,
            skipped: 6,
            before: 4000,
            after: 1000,
            ..Report::default()
        };
        let t = report_text(&r, false);
        assert!(t.contains("Сжато файлов: 4") && t.contains("пропущено 6"), "{t}");
        assert!(report_text(&r, true).starts_with("Сжатие снято"));
    }
}
