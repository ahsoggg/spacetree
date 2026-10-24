//! The "what grew" tab: the current scan against an earlier snapshot of the same location.

use eframe::egui::{self, Align2, RichText, Ui, pos2, vec2};
use spacetree_core::snapshot::{self, Change, Entry, Snapshot};
use spacetree_core::stats::{format_date, human_size};
use spacetree_core::{NodeId, ROOT, Tree};

use crate::theme;
use crate::views::{ROW_H, cell, font, row, row_bg};
use crate::{Action, App, now_unix};

const DAY: u64 = 86_400;
/// Compare with a snapshot at least this old by default.
const DEFAULT_AGE: u64 = 7 * DAY;
const MAX_ROWS: usize = 400;

#[derive(Default)]
pub struct Growth {
    /// Earlier snapshots of this location, oldest first.
    pub history: Vec<Entry>,
    /// Index into `history` of the snapshot being compared with.
    pub chosen: Option<usize>,
    /// `(taken, total size)` of the chosen snapshot.
    pub base: Option<(u64, u64)>,
    pub changes: Vec<Change>,
    /// Total size now, and the change of the total since the baseline.
    pub total: u64,
    pub total_delta: i64,
    pub error: Option<String>,
}

impl Growth {
    /// Compares `current` with the default baseline of `history`.
    pub fn new(history: Vec<Entry>, current: &Snapshot) -> Growth {
        let mut g = Growth {
            chosen: snapshot::pick_baseline(&history, current.taken, DEFAULT_AGE)
                .and_then(|e| history.iter().position(|h| h.path == e.path)),
            history,
            total: current.total,
            ..Growth::default()
        };
        g.compute(current);
        g
    }

    pub fn choose(&mut self, i: usize, current: &Snapshot) {
        self.chosen = Some(i);
        self.compute(current);
    }

    fn compute(&mut self, current: &Snapshot) {
        self.changes.clear();
        self.base = None;
        self.error = None;
        let Some(entry) = self.chosen.and_then(|i| self.history.get(i)) else {
            return;
        };
        match snapshot::load(entry) {
            Ok(old) => {
                // ignore changes below ~0.2 % of the scanned size (but between 1 MB and 256 MB)
                let min = (current.total / 500).clamp(1 << 20, 256 << 20);
                self.changes = snapshot::diff(&old, current, min);
                self.total_delta = current.total as i64 - old.total as i64;
                self.base = Some((old.taken, old.total));
            }
            Err(e) => {
                crate::diag::warn(format!("cannot read snapshot {}: {e}", entry.path.display()));
                self.error = Some(format!("Не удалось прочитать снимок: {e}"));
            }
        }
    }

    /// `+12,3 ГБ`-style label for the tab.
    pub fn tab_hint(&self) -> Option<String> {
        self.base.map(|_| signed(self.total_delta))
    }
}

pub fn signed(delta: i64) -> String {
    let sign = if delta >= 0 { '+' } else { '−' };
    format!("{sign}{}", human_size(delta.unsigned_abs()))
}

fn days_ago(taken: u64, now: u64) -> String {
    match now.saturating_sub(taken) / DAY {
        0 => "сегодня".into(),
        1 => "вчера".into(),
        n => format!("{n} дн. назад"),
    }
}

fn entry_label(e: &Entry, now: u64) -> String {
    format!("{} ({})", format_date(e.taken as u32), days_ago(e.taken, now))
}

/// Node at the relative path `rel` ('/'-separated), if it is still in the tree.
fn find_path(tree: &Tree, rel: &str) -> Option<NodeId> {
    let mut cur = ROOT;
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        cur = tree
            .children(cur)
            .iter()
            .copied()
            .find(|&c| tree.name(c).eq_ignore_ascii_case(part))?;
    }
    Some(cur)
}

impl App {
    pub fn growth_view(&mut self, ui: &mut Ui) {
        let Some(data) = self.data.clone() else { return };
        let tree = &data.out.tree;
        let now = now_unix() as u64;
        let Some(current) = data.snapshot.as_ref() else { return };
        let weak = ui.visuals().weak_text_color();
        let text_col = ui.visuals().text_color();

        let mut chosen = None;
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            let g = &self.growth;
            match (g.base, &g.error) {
                (Some((taken, total)), _) => {
                    let color = if g.total_delta > 0 { theme::REVIEW } else { theme::SAFE };
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(signed(g.total_delta)).size(26.0).strong().color(color));
                        ui.label(
                            RichText::new(format!(
                                "с {} · было {}, стало {}",
                                format_date(taken as u32),
                                human_size(total),
                                human_size(g.total)
                            ))
                            .size(15.0)
                            .weak(),
                        );
                    });
                }
                (None, Some(e)) => {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
                (None, None) => {
                    ui.label(RichText::new("Это первое сканирование этого места").size(18.0).strong());
                    ui.label(
                        RichText::new(
                            "Снимок сохранён. Отсканируйте это же место позже — например, через неделю — и здесь \
                             появится, какие папки выросли и насколько.",
                        )
                        .weak(),
                    );
                }
            }
            if !g.history.is_empty() {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("Сравнить с:");
                    let selected = g
                        .chosen
                        .and_then(|i| g.history.get(i))
                        .map_or_else(String::new, |e| entry_label(e, now));
                    egui::ComboBox::from_id_salt("growth_base")
                        .selected_text(selected)
                        .height(360.0)
                        .show_ui(ui, |ui| {
                            for (i, e) in g.history.iter().enumerate().rev() {
                                if ui.selectable_label(g.chosen == Some(i), entry_label(e, now)).clicked() {
                                    chosen = Some(i);
                                }
                            }
                        });
                });
            }
            if !self.settings.save_snapshots {
                ui.label(
                    RichText::new(
                        "Сохранение снимков выключено в настройках — новые сканирования не пополняют историю.",
                    )
                    .weak(),
                );
            }
        });
        if let Some(i) = chosen {
            self.growth.choose(i, current);
        }
        if self.settings.ai_enabled
            && !self.growth.changes.is_empty()
            && ui
                .add_enabled(!self.ai.busy(), egui::Button::new("✨ Объяснить, что выросло"))
                .on_hover_text("Локальная модель назовёт вероятных виновников по списку ниже")
                .clicked()
        {
            self.ai_explain_growth();
        }
        ui.add_space(8.0);

        let g = &self.growth;
        if g.base.is_some() && g.changes.is_empty() {
            ui.label("Заметных изменений нет.");
            return;
        }
        if g.changes.is_empty() {
            return;
        }
        let max = g.changes.iter().map(|c| c.delta.unsigned_abs()).max().unwrap_or(1) as f32;
        let width = ui.available_width();
        let (hdr, _) = ui.allocate_exact_size(vec2(width, ROW_H), egui::Sense::hover());
        let p = ui.painter_at(hdr);
        for (right, label) in [(110.0, "Изменение"), (210.0, "Было"), (310.0, "Стало")] {
            p.text(
                pos2(hdr.left() + right - 8.0, hdr.center().y),
                Align2::RIGHT_CENTER,
                label,
                font(),
                weak,
            );
        }
        p.text(
            pos2(hdr.left() + 410.0, hdr.center().y),
            Align2::LEFT_CENTER,
            "Папка",
            font(),
            weak,
        );

        let mut act = None;
        let shown = &g.changes[..g.changes.len().min(MAX_ROWS)];
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, ROW_H, shown.len(), |ui, range| {
                for i in range {
                    let c = &shown[i];
                    let node = find_path(tree, &c.path);
                    let (rect, resp, hover) = row(ui, ROW_H, ("growth", &c.path));
                    row_bg(ui, rect, false, hover, i);
                    let p = ui.painter_at(rect);
                    let y = rect.center().y;
                    let color = if c.delta > 0 { theme::REVIEW } else { theme::SAFE };
                    cell(&p, rect.left() + 110.0, y, signed(c.delta), color);
                    cell(&p, rect.left() + 210.0, y, c.old.map_or("—".into(), human_size), weak);
                    cell(
                        &p,
                        rect.left() + 310.0,
                        y,
                        c.new.map_or("—".into(), human_size),
                        text_col,
                    );
                    let bar = egui::Rect::from_min_size(pos2(rect.left() + 322.0, y - 4.0), vec2(70.0, 8.0));
                    theme::bar(ui, bar, c.delta.unsigned_abs() as f32 / max, color);
                    let shown_path = if cfg!(windows) {
                        c.path.replace('/', "\\")
                    } else {
                        c.path.clone()
                    };
                    let tag = match (c.old, c.new) {
                        (None, _) => "  новая",
                        (_, None) => "  удалена или уменьшилась",
                        _ => "",
                    };
                    p.text(
                        pos2(rect.left() + 410.0, y),
                        Align2::LEFT_CENTER,
                        format!("{shown_path}{tag}"),
                        font(),
                        if node.is_some() { text_col } else { weak },
                    );
                    if let Some(id) = node {
                        if resp.clicked() {
                            act = Some(Action::Show(id));
                        }
                        resp.on_hover_text("Нажмите, чтобы показать в обзоре");
                    }
                }
            });
        if let Some(a) = act {
            self.apply(ui.ctx(), a);
        }
    }
}
