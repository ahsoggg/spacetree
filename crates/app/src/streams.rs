//! The "hidden streams" tab: NTFS alternate data streams (`file.txt:secret.bin`) that Explorer does not count
//! in the file's size, found by the MFT scan.

use std::collections::HashSet;

use eframe::egui::{self, Align2, Color32, Id, Key, RichText, Ui, pos2, vec2};
use spacetree_core::NodeId;
use spacetree_core::stats::human_size;

use crate::views::{ROW_H, cell, font, row, row_bg};
use crate::{Action, App, ToastKind, settings, theme};

/// Highlight color of files with hidden streams and of the folders that hold them.
pub const ADS_COLOR: Color32 = Color32::from_rgb(214, 58, 187);

#[derive(Default)]
pub struct StreamsState {
    /// Streams deleted from here (hidden from the list until the next scan).
    removed: HashSet<(NodeId, String)>,
    pending: Option<(NodeId, String, u64, u64)>,
}

/// Stream names come from the disk: one that could point somewhere else (`x:y`, `..\z`) is not touched.
fn safe_stream_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['\\', '/', ':', '\0']) && name != "$DATA"
}

/// Deletes the alternate stream `name` of the file `path`; the file's main content is not affected.
fn delete_stream(path: &str, name: &str) -> std::io::Result<()> {
    if !safe_stream_name(name) {
        return Err(std::io::Error::other("недопустимое имя потока"));
    }
    std::fs::remove_file(format!("{}:{name}", spacetree_core::fs_path(path).display()))
}

impl App {
    pub fn streams_view(&mut self, ui: &mut Ui) {
        let Some(data) = self.data.clone() else { return };
        let tree = &data.out.tree;
        let shown: Vec<usize> = (0..data.out.streams.len())
            .filter(|&i| {
                let s = &data.out.streams[i];
                !tree.is_removed(s.node) && !self.streams.removed.contains(&(s.node, s.name.clone()))
            })
            .collect();
        let hidden: u64 = shown
            .iter()
            .map(|&i| data.out.streams[i].size.max(data.out.streams[i].alloc))
            .sum();
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("≈ {}", human_size(hidden)))
                        .size(26.0)
                        .strong()
                        .color(ADS_COLOR),
                );
                ui.label(RichText::new("спрятано в скрытых потоках").size(16.0).weak());
            });
            ui.label(
                RichText::new(
                    "В NTFS к файлу можно прицепить дополнительный поток (file.txt:имя). Проводник показывает только \
                     размер основного содержимого, поэтому файл в 0 байт может весить гигабайты. Здесь — потоки от \
                     1 МБ; их размера нет в размерах файлов и папок на других вкладках. Такие файлы и папки с ними \
                     подсвечены на карте розовым. Легальные примеры: данные облачных клиентов и резервных копий; \
                     вредоносные программы тоже прячут данные так.",
                )
                .weak(),
            );
        });
        ui.add_space(6.0);
        if data.out.record_nodes.is_empty() {
            ui.label("Скрытые потоки видны только при сканировании через MFT: нужны права администратора и скан диска целиком.");
            return;
        }
        if shown.is_empty() {
            ui.label("Скрытых потоков от 1 МБ не найдено.");
            return;
        }
        let (text_col, weak) = (ui.visuals().text_color(), ui.visuals().weak_text_color());
        let mut act = None;
        let mut ask = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, ROW_H, shown.len(), |ui, range| {
                for i in range {
                    let s = &data.out.streams[shown[i]];
                    let (rect, resp, hover) = row(ui, ROW_H, ("stream", s.node, &s.name));
                    row_bg(ui, rect, self.selected == Some(s.node), hover, i);
                    let p = ui.painter_at(rect);
                    let y = rect.center().y;
                    cell(&p, rect.left() + 100.0, y, human_size(s.size), ADS_COLOR);
                    cell(&p, rect.left() + 200.0, y, human_size(s.alloc), weak);
                    let clip = egui::Rect::from_min_max(rect.min, pos2(rect.right() - 200.0, rect.bottom()));
                    ui.painter_at(clip).text(
                        pos2(rect.left() + 212.0, y),
                        Align2::LEFT_CENTER,
                        format!("{}:{}", tree.path(s.node), s.name),
                        font(),
                        text_col,
                    );
                    let reveal = egui::Rect::from_center_size(pos2(rect.right() - 150.0, y), vec2(76.0, 20.0));
                    let delete = egui::Rect::from_center_size(pos2(rect.right() - 54.0, y), vec2(100.0, 20.0));
                    if ui
                        .put(
                            reveal,
                            egui::Button::new(RichText::new("В Проводнике").size(11.5)).frame(hover > 0.5),
                        )
                        .clicked()
                    {
                        act = Some(Action::Reveal(s.node));
                    }
                    if ui
                        .put(
                            delete,
                            egui::Button::new(RichText::new("Удалить поток…").size(11.5).color(theme::REVIEW))
                                .frame(hover > 0.5),
                        )
                        .on_hover_text("Удалить только этот скрытый поток; сам файл останется")
                        .clicked()
                    {
                        ask = Some((s.node, s.name.clone(), s.size, s.alloc));
                    }
                    if resp.clicked() {
                        act = Some(Action::Select(s.node));
                    } else if resp.double_clicked() {
                        act = Some(Action::Show(s.node));
                    }
                }
            });
        if ask.is_some() {
            self.streams.pending = ask;
        }
        if let Some(a) = act {
            self.apply(ui.ctx(), a);
        }
    }

    pub fn stream_dialogs(&mut self, ctx: &egui::Context) {
        let Some((node, name, size, _)) = self.streams.pending.clone() else {
            return;
        };
        let Some(data) = self.data.clone() else { return };
        let path = data.out.tree.path(node);
        let mut decision = None;
        egui::Modal::new(Id::new("confirm_stream")).show(ctx, |ui| {
            ui.set_width(500.0);
            ui.label(
                RichText::new(format!("Удалить скрытый поток «{name}»?"))
                    .size(17.0)
                    .strong(),
            );
            ui.label(RichText::new(&path).weak().small());
            ui.add_space(4.0);
            ui.label(format!(
                "Будет удалён только этот поток ({}); основное содержимое файла останется. Восстановить поток \
                 после этого нельзя.",
                human_size(size)
            ));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .add(
                        egui::Button::new(RichText::new("Удалить навсегда").color(Color32::WHITE).strong())
                            .fill(theme::REVIEW),
                    )
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
                self.streams.pending = None;
                if settings::is_demo() {
                    self.toast("В демо-режиме ничего не удаляется", ToastKind::Info);
                    return;
                }
                match delete_stream(&path, &name) {
                    Ok(()) => {
                        crate::diag::info(format!("deleted stream {path}:{name}"));
                        self.streams.removed.insert((node, name.clone()));
                        self.toast(
                            format!("Поток «{name}» удалён · освобождено около {}", human_size(size)),
                            ToastKind::Success,
                        );
                    }
                    Err(e) => self.toast(format!("Не удалось удалить поток: {e}"), ToastKind::Error),
                }
            }
            Some(false) => self.streams.pending = None,
            None => {}
        }
    }

    /// Streams still listed (not deleted from this tab).
    pub fn streams_left(&self) -> usize {
        self.data.as_ref().map_or(0, |d| {
            d.out
                .streams
                .iter()
                .filter(|s| !d.out.tree.is_removed(s.node) && !self.streams.removed.contains(&(s.node, s.name.clone())))
                .count()
        })
    }

    pub fn tree_replaced_streams(&mut self) {
        self.streams = StreamsState::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_names_that_could_escape_are_refused() {
        for bad in ["", "a:b", r"..\x", "a/b", "$DATA", "a\0b"] {
            assert!(!safe_stream_name(bad), "{bad:?}");
        }
        assert!(safe_stream_name("secret.bin"));
        assert!(safe_stream_name("Zone.Identifier"));
        assert!(delete_stream(r"C:\x\f.txt", "a:b").is_err());
    }

    /// Deleting a stream leaves the file and its other streams alone (NTFS only; on other file systems the stream
    /// cannot even be created and the test is skipped).
    #[test]
    fn deleting_a_stream_keeps_the_file() {
        let dir = std::env::temp_dir().join(format!("spacetree-ads-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("host.txt");
        std::fs::write(&file, b"visible").unwrap();
        let (a, b) = (
            format!("{}:hidden", file.display()),
            format!("{}:other", file.display()),
        );
        if std::fs::write(&a, vec![1u8; 4096]).is_err() {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        std::fs::write(&b, b"keep").unwrap();

        delete_stream(file.to_str().unwrap(), "hidden").unwrap();
        assert!(std::fs::read(&a).is_err(), "the stream is gone");
        assert_eq!(std::fs::read(&b).unwrap(), b"keep", "the other stream stays");
        assert_eq!(std::fs::read(&file).unwrap(), b"visible", "the file stays");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
