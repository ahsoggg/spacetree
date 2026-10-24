//! The four tabs: overview (tree / search results + treemap), file types, largest files, cleanup.

use std::time::Instant;

use eframe::egui::{
    self, Align2, Color32, FontId, Id, Key, Painter, Pos2, Rect, Response, RichText, Sense, Shape, Stroke, StrokeKind,
    Ui, pos2, vec2,
};
use spacetree_core::cleanup::{Category, Safety};
use spacetree_core::stats::{extension, format_date, human_size};
use spacetree_core::{Kind, NodeId, ROOT, Tree};

use crate::streams::ADS_COLOR;
use crate::theme::{self, ext_color, shade};
use crate::{Action, App, ListSort, SortKey, Tab, TmAnim, TmCache, TmMorph, TypeSort, now_unix, sort_list, treemap};

pub(crate) const ROW_H: f32 = 22.0;
const INDENT: f32 = 16.0;
const CLEANUP_ITEMS: usize = 300;
const ZOOM_SECS: f32 = 0.28;
/// How long tiles glide to their places after a live update.
const MORPH_SECS: f32 = 0.30;

pub(crate) fn font() -> FontId {
    FontId::proportional(13.0)
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

fn lerp_rect(a: Rect, b: Rect, t: f32) -> Rect {
    Rect::from_min_max(a.min.lerp(b.min, t), a.max.lerp(b.max, t))
}

/// Right-aligned cell text ending at `right`.
pub(crate) fn cell(p: &Painter, right: f32, y: f32, text: impl ToString, color: Color32) {
    p.text(
        pos2(right - 8.0, y),
        Align2::RIGHT_CENTER,
        text.to_string(),
        font(),
        color,
    );
}

/// Shared right-click menu for any node.
pub(crate) fn node_menu(ui: &mut Ui, tree: &Tree, id: NodeId, act: &mut Option<Action>) {
    ui.label(RichText::new(tree.name(id)).strong());
    ui.separator();
    let mut item = |ui: &mut Ui, text: &str, a: Action| {
        if ui.button(text).clicked() {
            *act = Some(a);
            ui.close();
        }
    };
    item(ui, "Показать в обзоре", Action::Show(id));
    item(ui, "Показать в Проводнике", Action::Reveal(id));
    item(ui, "Открыть", Action::Open(id));
    item(ui, "Копировать путь", Action::CopyPath(id));
    if crate::ai::enabled() && tree.is_dir(id) && id != ROOT {
        item(ui, "Что это за папка? (ИИ)", Action::AskAi(id));
    }
    if cfg!(windows) && id != ROOT && tree.node(id).kind != Kind::Link {
        ui.separator();
        item(
            ui,
            "Сжать алгоритмом LZX (Windows Compact)…",
            Action::Compress(id, false),
        );
        item(ui, "Снять сжатие LZX…", Action::Compress(id, true));
    }
    if id != ROOT {
        ui.separator();
        item(ui, "Удалить в корзину          Del", Action::Delete(id, false));
        item(ui, "Удалить безвозвратно…  Shift+Del", Action::Delete(id, true));
    }
}

/// A full-width list row with a stable id (so hover animations and context menus survive scrolling).
pub(crate) fn row(ui: &mut Ui, height: f32, key: impl std::hash::Hash + std::fmt::Debug) -> (Rect, Response, f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    let resp = ui.interact(rect, Id::new(key), Sense::click());
    let hover = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    (rect, resp, hover)
}

/// Row background: selection, animated hover, zebra.
pub(crate) fn row_bg(ui: &Ui, rect: Rect, selected: bool, hover: f32, i: usize) {
    let v = ui.visuals();
    let base = if i % 2 == 1 {
        v.faint_bg_color
    } else {
        Color32::TRANSPARENT
    };
    let fill = if selected {
        v.selection.bg_fill
    } else {
        theme::lerp_color(base, v.widgets.hovered.weak_bg_fill, hover)
    };
    if fill != Color32::TRANSPARENT {
        ui.painter().rect_filled(rect, 4.0, fill);
    }
}

/// Expand/collapse triangle (pointing right when closed, down when open).
fn disclosure(p: &Painter, c: Pos2, open: f32, color: Color32) {
    // rotate the "closed" triangle by up to 90°
    let a = open * std::f32::consts::FRAC_PI_2;
    let (s, co) = a.sin_cos();
    let rot = |v: egui::Vec2| c + vec2(v.x * co - v.y * s, v.x * s + v.y * co);
    let pts = vec![rot(vec2(-2.5, -4.0)), rot(vec2(3.0, 0.0)), rot(vec2(-2.5, 4.0))];
    p.add(Shape::convex_polygon(pts, color, Stroke::NONE));
}

/// Small folder glyph centered at `c` (the emoji font has no good one).
fn folder_icon(p: &Painter, c: Pos2, color: Color32) {
    let body = Rect::from_center_size(c + vec2(0.0, 1.0), vec2(13.0, 9.0));
    let tab = Rect::from_min_size(body.left_top() - vec2(0.0, 2.0), vec2(5.5, 3.0));
    p.rect_filled(tab, 1.0, shade(color, 0.8));
    p.rect_filled(body, 1.5, color);
}

/// Small up/down triangle centered at `c`.
fn sort_arrow(p: &Painter, c: Pos2, desc: bool, color: Color32) {
    let pts = if desc {
        vec![c + vec2(-4.0, -2.0), c + vec2(4.0, -2.0), c + vec2(0.0, 3.0)]
    } else {
        vec![c + vec2(-4.0, 2.0), c + vec2(4.0, 2.0), c + vec2(0.0, -3.0)]
    };
    p.add(Shape::convex_polygon(pts, color, Stroke::NONE));
}

/// Checkbox drawn inside a row (the row itself handles the click).
fn tick(p: &Painter, c: Pos2, on: bool, ui: &Ui) {
    let r = Rect::from_center_size(c, vec2(14.0, 14.0));
    if on {
        p.rect_filled(r, 3.0, theme::ACCENT);
        let pts = [c + vec2(-3.5, 0.0), c + vec2(-1.0, 3.0), c + vec2(4.0, -3.0)];
        p.line(pts.to_vec(), Stroke::new(2.0, Color32::WHITE));
    } else {
        p.rect_stroke(
            r,
            3.0,
            Stroke::new(1.2, ui.visuals().weak_text_color()),
            StrokeKind::Inside,
        );
    }
}

/// File-type marker: folder glyph, link glyph or a colored square.
fn kind_icon(p: &Painter, tree: &Tree, id: NodeId, at: Pos2, text: Color32) {
    match tree.node(id).kind {
        Kind::Dir => folder_icon(p, at + vec2(7.0, 0.0), theme::LIKELY),
        Kind::Link => {
            p.text(at, Align2::LEFT_CENTER, "↪", FontId::proportional(13.0), text);
        }
        Kind::File => {
            let r = Rect::from_center_size(at + vec2(6.0, 0.0), vec2(9.0, 9.0));
            p.rect_filled(r, 2.0, ext_color(&extension(tree.name(id))));
        }
    }
}

/// Clickable column header; returns true when clicked.
fn header(p: &Painter, resp: &Response, rect: Rect, label: &str, right: bool, sort: Option<bool>) -> bool {
    let color = if sort.is_some() {
        theme::ACCENT
    } else {
        Color32::from_gray(140)
    };
    let y = rect.center().y;
    let text = if right {
        p.text(pos2(rect.right() - 8.0, y), Align2::RIGHT_CENTER, label, font(), color)
    } else {
        p.text(pos2(rect.left() + 8.0, y), Align2::LEFT_CENTER, label, font(), color)
    };
    if let Some(desc) = sort {
        let x = if right { text.left() - 9.0 } else { text.right() + 9.0 };
        sort_arrow(p, pos2(x, y), desc, color);
    }
    resp.clicked() && resp.interact_pointer_pos().is_some_and(|pos| rect.contains(pos))
}

/// Toggle helper for `(key, descending)` sort state; names sort ascending first.
fn toggle_sort<K: PartialEq + Copy>(state: &mut (K, bool), key: K, name_key: K) -> bool {
    if state.0 == key {
        state.1 = !state.1;
    } else {
        *state = (key, key != name_key);
    }
    true
}

impl App {
    pub fn tree_view(&mut self, ui: &mut Ui) {
        let Some(data) = self.data.clone() else { return };
        let tree = &data.out.tree;
        if self.rows_dirty {
            self.rebuild_rows();
        }
        let width = ui.available_width();
        let (w_size, w_pct, w_files, w_date) = (88.0, 120.0, 72.0, 92.0);
        let cols_w = w_size * 2.0 + w_pct + w_files + w_date;
        let weak = ui.visuals().weak_text_color();
        let text_col = ui.visuals().text_color();

        // header
        let (hdr, hresp) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::click());
        let p = ui.painter_at(hdr);
        let mut x = hdr.right();
        let mut col = |w: f32| {
            let r = Rect::from_min_max(pos2(x - w, hdr.top()), pos2(x, hdr.bottom()));
            x -= w;
            r
        };
        let (r_date, r_files, r_pct, r_alloc, r_size) =
            (col(w_date), col(w_files), col(w_pct), col(w_size), col(w_size));
        let r_name = Rect::from_min_max(hdr.min, pos2(r_size.left(), hdr.bottom()));
        let mut st = (self.sort, self.sort_desc);
        let mut changed = None;
        for (r, k, label, right) in [
            (r_name, SortKey::Name, "Имя", false),
            (r_size, SortKey::Size, "Размер", true),
            (r_files, SortKey::Files, "Файлов", true),
            (r_date, SortKey::Modified, "Изменён", true),
        ] {
            if header(&p, &hresp, r, label, right, (st.0 == k).then_some(st.1)) {
                changed = Some(k);
            }
        }
        header(&p, &hresp, r_alloc, "На диске", true, None);
        header(&p, &hresp, r_pct, "% папки", true, None);
        if let Some(k) = changed {
            toggle_sort(&mut st, k, SortKey::Name);
            (self.sort, self.sort_desc) = st;
            self.rows_dirty = true;
        }
        ui.painter().hline(
            hdr.x_range(),
            hdr.bottom(),
            ui.visuals().widgets.noninteractive.bg_stroke,
        );

        let mut act: Option<Action> = None;
        self.tree_keys(ui, tree, &mut act);

        let mut sa = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if let Some(r) = self.scroll_to.take() {
            sa = sa.vertical_scroll_offset((r as f32 * (ROW_H + ui.spacing().item_spacing.y) - 80.0).max(0.0));
        }
        let selected = self.selected;
        let rows = &self.rows;
        let expanded = &self.expanded;
        sa.show_rows(ui, ROW_H, rows.len(), |ui, range| {
            for i in range {
                let (id, depth) = rows[i];
                let (rect, resp, hover) = row(ui, ROW_H, ("tree", id));
                row_bg(ui, rect, selected == Some(id), hover, i);
                let painter = ui.painter_at(rect);
                let nd = tree.node(id);
                let is_dir = nd.kind == Kind::Dir;
                let tx = rect.left() + 4.0 + depth as f32 * INDENT;
                if is_dir && !tree.children(id).is_empty() {
                    let open = ui
                        .ctx()
                        .animate_bool_with_time(Id::new(("open", id)), expanded.contains(&id), 0.12);
                    disclosure(&painter, pos2(tx + 6.0, rect.center().y), open, weak);
                }
                let name_clip = Rect::from_min_max(rect.min, pos2(rect.right() - cols_w, rect.bottom()));
                let np = ui.painter_at(name_clip);
                kind_icon(&np, tree, id, pos2(tx + 15.0, rect.center().y), text_col);
                np.text(
                    pos2(tx + 34.0, rect.center().y),
                    Align2::LEFT_CENTER,
                    tree.name(id),
                    font(),
                    if nd.kind == Kind::Link { weak } else { text_col },
                );
                let parent_size = tree.parent(id).map(|p| tree.node(p).size).unwrap_or(nd.size);
                let frac = if parent_size == 0 {
                    0.0
                } else {
                    nd.size as f64 / parent_size as f64
                };
                let y = rect.center().y;
                let mut x = rect.right();
                cell(&painter, x, y, format_date(nd.mtime), weak);
                x -= w_date;
                cell(
                    &painter,
                    x,
                    y,
                    if is_dir { nd.files.to_string() } else { String::new() },
                    weak,
                );
                x -= w_files;
                let bar = Rect::from_min_size(pos2(x - w_pct + 6.0, y - 4.0), vec2(56.0, 8.0));
                theme::bar(ui, bar, frac as f32, theme::ACCENT.gamma_multiply(0.85));
                cell(&painter, x, y, format!("{:.1}%", frac * 100.0), text_col);
                x -= w_pct;
                cell(&painter, x, y, human_size(nd.alloc), weak);
                x -= w_size;
                cell(&painter, x, y, human_size(nd.size), text_col);

                if resp.clicked() {
                    let on_arrow = resp.interact_pointer_pos().is_some_and(|p| p.x < tx + 14.0);
                    act = Some(if is_dir && on_arrow {
                        Action::Toggle(id)
                    } else {
                        Action::Show(id)
                    });
                }
                if resp.double_clicked() && is_dir {
                    act = Some(Action::Toggle(id));
                }
                resp.context_menu(|ui| node_menu(ui, tree, id, &mut act));
            }
        });
        if let Some(a) = act {
            self.apply(ui.ctx(), a);
        }
    }

    /// Arrow keys move through the tree: ↑/↓ select, › expand / enter, ← collapse / go to the parent.
    fn tree_keys(&mut self, ui: &Ui, tree: &Tree, act: &mut Option<Action>) {
        if ui.ctx().egui_wants_keyboard_input() {
            return;
        }
        let (up, down, left, right, enter) = ui.input(|i| {
            (
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::ArrowDown),
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::Enter),
            )
        });
        if !(up || down || left || right || enter) {
            return;
        }
        let pos = self.selected.and_then(|s| self.rows.iter().position(|&(r, _)| r == s));
        let Some(pos) = pos else {
            if let Some(&(first, _)) = self.rows.first() {
                *act = Some(Action::Show(first));
            }
            return;
        };
        let id = self.rows[pos].0;
        if up && pos > 0 {
            *act = Some(Action::Show(self.rows[pos - 1].0));
        } else if down && pos + 1 < self.rows.len() {
            *act = Some(Action::Show(self.rows[pos + 1].0));
        } else if right && tree.is_dir(id) {
            if !self.expanded.contains(&id) {
                *act = Some(Action::Toggle(id));
            } else if let Some(&c) = tree.children(id).first() {
                *act = Some(Action::Show(c));
            }
        } else if left {
            if tree.is_dir(id) && self.expanded.contains(&id) {
                *act = Some(Action::Toggle(id));
            } else if let Some(p) = tree.parent(id) {
                *act = Some(Action::Show(p));
            }
        } else if enter && tree.is_dir(id) {
            *act = Some(Action::Toggle(id));
        }
    }

    fn sort_chips(ui: &mut Ui, state: &mut (ListSort, bool)) -> bool {
        let mut changed = false;
        ui.label(RichText::new("Сортировка:").weak());
        for (k, label) in [
            (ListSort::Size, "размер"),
            (ListSort::Modified, "дата"),
            (ListSort::Name, "имя"),
        ] {
            let on = state.0 == k;
            let arrow = if !on {
                ""
            } else if state.1 {
                " ↓"
            } else {
                " ↑"
            };
            if ui
                .add(egui::Button::selectable(on, format!("{label}{arrow}")))
                .clicked()
            {
                changed = toggle_sort(state, k, ListSort::Name);
            }
        }
        changed
    }

    pub fn search_view(&mut self, ui: &mut Ui) {
        let Some(data) = self.data.clone() else { return };
        let tree = &data.out.tree;
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Найдено: {}", self.search_hits.len())).strong());
            if self.search_truncated {
                ui.label(RichText::new("(показаны крупнейшие)").weak());
            }
            if ui.small_button("× Сбросить").clicked() {
                self.clear_search();
            }
        });
        ui.horizontal(|ui| {
            if Self::sort_chips(ui, &mut self.search_sort) {
                self.resort_search();
            }
        });
        ui.separator();
        let weak = ui.visuals().weak_text_color();
        let text_col = ui.visuals().text_color();
        let mut act = None;
        let selected = self.selected;
        let hits = &self.search_hits;
        let mut retarget = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, ROW_H * 2.0, hits.len(), |ui, range| {
                for i in range {
                    let id = hits[i];
                    let (rect, resp, hover) = row(ui, ROW_H * 2.0, ("hit", id));
                    row_bg(ui, rect, selected == Some(id), hover, i);
                    let p = ui.painter_at(rect.shrink2(vec2(0.0, 1.0)));
                    let top = rect.top() + ROW_H * 0.55;
                    kind_icon(&p, tree, id, pos2(rect.left() + 8.0, top), text_col);
                    p.text(
                        pos2(rect.left() + 28.0, top),
                        Align2::LEFT_CENTER,
                        tree.name(id),
                        font(),
                        text_col,
                    );
                    let nd = tree.node(id);
                    cell(&p, rect.right(), top, human_size(nd.size), text_col);
                    cell(&p, rect.right() - 90.0, top, format_date(nd.mtime), weak);
                    let parent = tree.parent(id).map(|p| tree.path(p)).unwrap_or_default();
                    p.text(
                        pos2(rect.left() + 28.0, rect.top() + ROW_H * 1.45),
                        Align2::LEFT_CENTER,
                        parent,
                        FontId::proportional(11.5),
                        weak,
                    );
                    if resp.clicked() {
                        act = Some(Action::Select(id));
                        retarget = Some(if tree.is_dir(id) {
                            id
                        } else {
                            tree.parent(id).unwrap_or(ROOT)
                        });
                    }
                    resp.context_menu(|ui| node_menu(ui, tree, id, &mut act));
                }
            });
        if let Some(r) = retarget {
            self.tm_root = r;
        }
        if let Some(a) = act {
            self.apply(ui.ctx(), a);
        }
    }

    /// Starts the zoom transition from the previous treemap root to the new one.
    fn start_zoom(&mut self, tree: &Tree, old: &TmCache, size: egui::Vec2) {
        let full = Rect::from_min_size(Pos2::ZERO, size);
        let new_root = self.tm_root;
        let from = if tree.ancestors(new_root).contains(&old.root) {
            // zoom in: the new root grows out of its tile in the old layout
            tree.ancestors(new_root)
                .iter()
                .rev()
                .find_map(|a| old.rects.iter().find(|r| r.id == *a).map(|r| r.rect))
        } else if tree.ancestors(old.root).contains(&new_root) {
            // zoom out: start magnified so that the old root fills the view, then shrink to its tile
            let layout = treemap::layout(tree, new_root, full);
            tree.ancestors(old.root)
                .iter()
                .rev()
                .find_map(|a| layout.iter().find(|r| r.id == *a).map(|r| r.rect))
                .map(|r| {
                    let (sx, sy) = (full.width() / r.width().max(1.0), full.height() / r.height().max(1.0));
                    Rect::from_min_size(
                        pos2(-r.min.x * sx, -r.min.y * sy),
                        vec2(full.width() * sx, full.height() * sy),
                    )
                })
        } else {
            None
        };
        self.tm_anim = from.map(|from| TmAnim {
            from,
            start: Instant::now(),
        });
    }

    pub fn treemap_view(&mut self, ui: &mut Ui) {
        let Some(data) = self.data.clone() else { return };
        let tree = &data.out.tree;
        let by_age = self.settings.color_by_age;
        let mut goto: Option<NodeId> = None;
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(self.tm_root != ROOT, egui::Button::new("⬆"))
                .on_hover_text("На уровень выше (Backspace)")
                .clicked()
            {
                self.tm_up();
            }
            for (i, &c) in tree.ancestors(self.tm_root).iter().enumerate() {
                if i > 0 {
                    ui.label(RichText::new("›").weak());
                }
                let name = if c == ROOT {
                    tree.path(ROOT)
                } else {
                    tree.name(c).to_owned()
                };
                if ui.link(name).clicked() {
                    goto = Some(c);
                }
            }
            ui.label(RichText::new(human_size(tree.node(self.tm_root).size)).weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut age = by_age;
                ui.selectable_value(&mut age, true, "по дате");
                ui.selectable_value(&mut age, false, "по типу");
                ui.label(RichText::new("Цвет:").weak());
                if age != by_age {
                    self.settings.color_by_age = age;
                    self.settings.save();
                }
            });
        });
        if let Some(c) = goto {
            self.tm_root = c;
        }

        let legend_h = 26.0;
        let avail = ui.available_size();
        let (resp, painter) = ui.allocate_painter(vec2(avail.x, (avail.y - legend_h).max(50.0)), Sense::click());
        let area = resp.rect;
        painter.rect_filled(area, 6.0, ui.visuals().extreme_bg_color);
        let stale = self.tm.as_ref().is_none_or(|c| {
            c.root != self.tm_root || c.size != area.size() || c.by_age != by_age || c.generation != self.generation
        });
        if stale {
            if let Some(old) = self.tm.take() {
                if old.root != self.tm_root && old.generation == self.generation {
                    self.start_zoom(tree, &old, area.size());
                } else if old.root == self.tm_root
                    && old.size == area.size()
                    && old.by_age == by_age
                    && old.generation != self.generation
                    && self.live.active()
                {
                    // live update: glide from what is on screen now (a morph in progress counts) to the new layout
                    let prev = self.tm_morph.take();
                    let prev_t = prev
                        .as_ref()
                        .map(|m| ease_out(m.start.elapsed().as_secs_f32() / MORPH_SECS));
                    let from = old
                        .rects
                        .iter()
                        .map(|r| {
                            let shown = match (&prev, prev_t) {
                                (Some(m), Some(t)) if t < 1.0 => {
                                    m.from.get(&r.id).map_or(r.rect, |f| lerp_rect(*f, r.rect, t))
                                }
                                _ => r.rect,
                            };
                            (r.id, shown)
                        })
                        .collect();
                    self.tm_morph = Some(TmMorph {
                        start: Instant::now(),
                        from,
                    });
                }
            }
            let rects = treemap::layout(tree, self.tm_root, Rect::from_min_size(Pos2::ZERO, area.size()));
            let exts: Vec<String> = rects
                .iter()
                .map(|r| {
                    if tree.is_dir(r.id) {
                        String::new()
                    } else {
                        extension(tree.name(r.id))
                    }
                })
                .collect();
            let now = now_unix();
            let colors = rects
                .iter()
                .zip(&exts)
                .map(|(r, e)| {
                    if by_age {
                        theme::age_color(tree.node(r.id).mtime, now)
                    } else if tree.is_dir(r.id) {
                        theme::DIR_TILE
                    } else {
                        ext_color(e)
                    }
                })
                .collect();
            self.tm = Some(TmCache {
                root: self.tm_root,
                size: area.size(),
                by_age,
                generation: self.generation,
                rects,
                colors,
                exts,
            });
        }
        let cache = self.tm.take().unwrap();

        // zoom transition: map the layout into a rect that animates towards the full area
        let t = self
            .tm_anim
            .as_ref()
            .map(|a| ease_out(a.start.elapsed().as_secs_f32() / ZOOM_SECS))
            .unwrap_or(1.0);
        let full = Rect::from_min_size(Pos2::ZERO, area.size());
        let frame = match &self.tm_anim {
            Some(a) if t < 1.0 => Rect::from_min_max(a.from.min.lerp(full.min, t), a.from.max.lerp(full.max, t)),
            _ => {
                self.tm_anim = None;
                full
            }
        };
        let animating = self.tm_anim.is_some();
        let morphing = self.tm_morph.is_some();
        let (sx, sy) = (frame.width() / full.width(), frame.height() / full.height());
        let map = |r: Rect| {
            Rect::from_min_max(
                area.min + vec2(frame.min.x + r.min.x * sx, frame.min.y + r.min.y * sy),
                area.min + vec2(frame.min.x + r.max.x * sx, frame.min.y + r.max.y * sy),
            )
        };
        let painter = painter.with_clip_rect(area);
        let morph_t = self
            .tm_morph
            .as_ref()
            .map(|m| ease_out(m.start.elapsed().as_secs_f32() / MORPH_SECS))
            .filter(|&t| t < 1.0);
        if morph_t.is_none() {
            self.tm_morph = None;
        }
        let morph = self.tm_morph.as_ref().zip(morph_t);
        // where a tile is drawn right now: gliding from its previous place during a live update
        let shown = |r: &treemap::TmRect| -> Rect {
            match morph {
                Some((m, t)) => m.from.get(&r.id).map_or(r.rect, |f| lerp_rect(*f, r.rect, t)),
                None => r.rect,
            }
        };

        let off = area.min.to_vec2();
        let hover = if animating {
            None
        } else {
            resp.hover_pos()
                .and_then(|p| treemap::hit(&cache.rects, p - off))
                .map(|r| r.id)
        };

        // All leaf tiles go into one mesh with a light-to-dark diagonal gradient.
        let mut mesh = egui::Mesh::default();
        let mut labels: Vec<(Rect, NodeId, bool)> = Vec::new();
        let frame_col = Color32::from_gray(if ui.visuals().dark_mode { 70 } else { 150 });
        // outlines go over the tiles: pink for hidden streams, white fading out for what just changed on disk
        let mut outlines: Vec<(Rect, Stroke)> = Vec::new();
        for (k, r) in cache.rects.iter().enumerate() {
            let rect = map(shown(r));
            if !rect.intersects(area) {
                continue;
            }
            if r.nested {
                let stroke = if data.ads_dirs.contains(&r.id) {
                    Stroke::new(1.5, ADS_COLOR)
                } else {
                    Stroke::new(1.0, frame_col)
                };
                painter.rect_stroke(rect, 0.0, stroke, StrokeKind::Inside);
                if r.header {
                    labels.push((rect, r.id, true));
                }
                continue;
            }
            let dim = self.highlight_ext.as_ref().is_some_and(|h| *h != cache.exts[k]);
            let base = if dim {
                shade(cache.colors[k], 0.3)
            } else {
                cache.colors[k]
            };
            let tr = rect.shrink(0.5);
            let (c0, c1, c2) = (shade(base, 1.18), shade(base, 0.72), shade(base, 0.95));
            let i = mesh.vertices.len() as u32;
            mesh.colored_vertex(tr.left_top(), c0);
            mesh.colored_vertex(tr.right_top(), c2);
            mesh.colored_vertex(tr.right_bottom(), c1);
            mesh.colored_vertex(tr.left_bottom(), c2);
            mesh.add_triangle(i, i + 1, i + 2);
            mesh.add_triangle(i, i + 2, i + 3);
            if rect.width() > 60.0 && rect.height() > 16.0 && !dim {
                labels.push((rect, r.id, false));
            }
            if data.ads.contains_key(&r.id) {
                outlines.push((tr.shrink(0.5), Stroke::new(2.0, ADS_COLOR)));
            }
            if let Some(t) = self.live.recent.get(&r.id) {
                let fade = 1.0 - t.elapsed().as_secs_f32() / crate::live::FLASH.as_secs_f32();
                outlines.push((
                    tr,
                    Stroke::new(2.5, Color32::WHITE.gamma_multiply(fade.clamp(0.0, 1.0))),
                ));
            }
        }
        painter.add(Shape::mesh(mesh));
        for (rect, stroke) in outlines {
            painter.rect_stroke(rect, 0.0, stroke, StrokeKind::Inside);
        }
        let folder_text = if ui.visuals().dark_mode {
            Color32::from_gray(210)
        } else {
            Color32::from_gray(40)
        };
        let label_alpha = if animating { t * t } else { 1.0 };
        for (rect, id, folder) in labels {
            let (pos, color) = if folder {
                (rect.min + vec2(3.0, 1.0), folder_text)
            } else {
                (rect.min + vec2(3.0, 2.0), Color32::BLACK)
            };
            painter.with_clip_rect(rect.intersect(area)).text(
                pos,
                Align2::LEFT_TOP,
                tree.name(id),
                FontId::proportional(11.0),
                color.gamma_multiply(label_alpha),
            );
        }
        if !animating {
            for (id, c) in [(self.selected, Color32::WHITE), (hover, Color32::YELLOW)] {
                if let Some(id) = id
                    && let Some(r) = cache.rects.iter().find(|r| r.id == id)
                {
                    painter.rect_stroke(map(shown(r)), 0.0, Stroke::new(2.0, c), StrokeKind::Inside);
                }
            }
        } else {
            ui.ctx().request_repaint();
        }
        if morphing {
            ui.ctx().request_repaint();
        }
        if cache.rects.is_empty() {
            painter.text(
                area.center(),
                Align2::CENTER_CENTER,
                "Папка пуста",
                font(),
                ui.visuals().weak_text_color(),
            );
        }

        let root_size = tree.node(self.tm_root).size.max(1);
        if let Some(h) = hover {
            let nd = tree.node(h);
            resp.clone().on_hover_text(format!(
                "{}\n{} · на диске {} · {:.1}% от текущей папки{}",
                tree.path(h),
                human_size(nd.size),
                human_size(nd.alloc),
                nd.size as f64 * 100.0 / root_size as f64,
                if nd.mtime != 0 {
                    format!("\nизменён {}", format_date(nd.mtime))
                } else {
                    String::new()
                }
            ));
        }
        self.tm = Some(cache);

        let mut act = None;
        if resp.secondary_clicked() {
            self.tm_menu = hover;
        }
        if let Some(id) = self.tm_menu {
            resp.context_menu(|ui| node_menu(ui, tree, id, &mut act));
        }
        // click selects, double click drills into a folder; Backspace / mouse "back" goes up
        if let Some(h) = hover {
            if resp.double_clicked() && tree.is_dir(h) {
                self.select(h);
            } else if resp.clicked() {
                act = Some(Action::Select(h));
            }
        }
        let back =
            ui.input(|i| i.pointer.button_clicked(egui::PointerButton::Extra1) || i.key_pressed(egui::Key::Backspace));
        if back && resp.hovered() && !ui.ctx().egui_wants_keyboard_input() {
            self.tm_up();
        }
        if let Some(a) = act {
            self.apply(ui.ctx(), a);
        }

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            if !data.ads.is_empty() {
                let (r, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
                ui.painter()
                    .rect_stroke(r, 3.0, Stroke::new(2.0, ADS_COLOR), StrokeKind::Inside);
                ui.label("скрытые потоки").on_hover_text(
                    "Файлы со скрытыми NTFS-потоками и папки, где они лежат. Подробности — на вкладке «Скрытые потоки»",
                );
            }
            if by_age {
                let now = now_unix();
                for (label, days) in theme::AGE_LEGEND {
                    let (r, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
                    ui.painter()
                        .rect_filled(r, 3.0, theme::age_color(now - days * 86_400, now));
                    ui.label(label);
                }
                return;
            }
            for t in data.types.iter().take(10) {
                let on = self.highlight_ext.as_deref() == Some(t.ext.as_str());
                let name = if t.ext.is_empty() {
                    "без расширения".to_owned()
                } else {
                    format!(".{}", t.ext)
                };
                let (r, resp) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::click());
                ui.painter().rect_filled(r, 3.0, ext_color(&t.ext));
                let label = ui.add(
                    egui::Label::new(RichText::new(name).color(if on {
                        theme::ACCENT
                    } else {
                        ui.visuals().text_color()
                    }))
                    .sense(Sense::click()),
                );
                if resp.clicked() || label.clicked() {
                    self.highlight_ext = if on { None } else { Some(t.ext.clone()) };
                }
                label.on_hover_text(format!(
                    "{} · {} файлов. Нажмите, чтобы подсветить",
                    human_size(t.size),
                    t.count
                ));
            }
            if self.highlight_ext.is_some() && ui.small_button("×").on_hover_text("Снять подсветку").clicked()
            {
                self.highlight_ext = None;
            }
        });
    }

    pub fn types_view(&mut self, ui: &mut Ui) {
        let Some(data) = self.data.clone() else { return };
        let total = data.out.tree.root_stats().0.max(1) as f64;
        let max = data.types.iter().map(|t| t.size).max().unwrap_or(1).max(1) as f32;
        let mut order: Vec<usize> = (0..data.types.len()).collect();
        let (key, desc) = self.types_sort;
        match key {
            TypeSort::Size => {}
            TypeSort::Count => order.sort_by_key(|&i| std::cmp::Reverse(data.types[i].count)),
            TypeSort::Name => order.sort_by(|&a, &b| data.types[a].ext.cmp(&data.types[b].ext)),
        }
        if (key != TypeSort::Name) != desc {
            order.reverse();
        }
        ui.label(
            RichText::new("Нажмите на тип, чтобы подсветить его файлы в treemap; двойной щелчок — перейти к обзору.")
                .weak(),
        );
        ui.add_space(4.0);
        let width = ui.available_width();
        let weak = ui.visuals().weak_text_color();
        let text_col = ui.visuals().text_color();
        let (hdr, hresp) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::click());
        let p = ui.painter_at(hdr);
        let r = |x0: f32, x1: f32| Rect::from_min_max(pos2(x0, hdr.top()), pos2(x1, hdr.bottom()));
        let right = hdr.right();
        let mut st = self.types_sort;
        let mut clicked = None;
        for (rect, k, label, right_align) in [
            (r(right - 90.0, right), TypeSort::Count, "Файлов", true),
            (r(right - 360.0, right - 260.0), TypeSort::Size, "Размер", true),
            (
                r(hdr.left() + 22.0, hdr.left() + 200.0),
                TypeSort::Name,
                "Расширение",
                false,
            ),
        ] {
            if header(&p, &hresp, rect, label, right_align, (st.0 == k).then_some(st.1)) {
                clicked = Some(k);
            }
        }
        header(&p, &hresp, r(right - 160.0, right - 90.0), "%", true, None);
        header(&p, &hresp, r(right - 260.0, right - 160.0), "На диске", true, None);
        if let Some(k) = clicked {
            toggle_sort(&mut st, k, TypeSort::Name);
            self.types_sort = st;
        }
        let mut pick = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, ROW_H, order.len(), |ui, range| {
                for i in range {
                    let t = &data.types[order[i]];
                    let on = self.highlight_ext.as_deref() == Some(t.ext.as_str());
                    let (rect, resp, hover) = row(ui, ROW_H, ("type", &t.ext));
                    row_bg(ui, rect, on, hover, i);
                    let p = ui.painter_at(rect);
                    let y = rect.center().y;
                    p.rect_filled(
                        Rect::from_center_size(pos2(rect.left() + 14.0, y), vec2(11.0, 11.0)),
                        3.0,
                        ext_color(&t.ext),
                    );
                    let name = if t.ext.is_empty() {
                        "(без расширения)".to_owned()
                    } else {
                        format!(".{}", t.ext)
                    };
                    p.text(pos2(rect.left() + 30.0, y), Align2::LEFT_CENTER, name, font(), text_col);
                    let mut x = rect.right();
                    cell(&p, x, y, t.count, weak);
                    x -= 90.0;
                    cell(&p, x, y, format!("{:.1}%", t.size as f64 * 100.0 / total), text_col);
                    x -= 70.0;
                    cell(&p, x, y, human_size(t.alloc), weak);
                    x -= 100.0;
                    cell(&p, x, y, human_size(t.size), text_col);
                    x -= 100.0;
                    let bar_w = (x - rect.left() - 200.0).clamp(0.0, 260.0);
                    if bar_w > 20.0 {
                        let r = Rect::from_min_size(pos2(x - bar_w - 8.0, y - 4.0), vec2(bar_w, 8.0));
                        theme::bar(ui, r, t.size as f32 / max, ext_color(&t.ext));
                    }
                    if resp.double_clicked() {
                        pick = Some((Some(t.ext.clone()), true));
                    } else if resp.clicked() {
                        pick = Some((if on { None } else { Some(t.ext.clone()) }, false));
                    }
                }
            });
        if let Some((ext, go)) = pick {
            self.highlight_ext = ext;
            if go {
                self.tab = Tab::Overview;
            }
        }
    }

    pub fn top_view(&mut self, ui: &mut Ui) {
        let Some(data) = self.data.clone() else { return };
        let tree = &data.out.tree;
        let key = (self.top_sort.0, self.top_sort.1, self.generation);
        if self.top_order_key != Some(key) {
            self.top_order = data.top.clone();
            sort_list(tree, &mut self.top_order, self.top_sort);
            self.top_order_key = Some(key);
        }
        let width = ui.available_width();
        let weak = ui.visuals().weak_text_color();
        let text_col = ui.visuals().text_color();
        let (hdr, hresp) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::click());
        let p = ui.painter_at(hdr);
        let r = |x0: f32, x1: f32| {
            Rect::from_min_max(pos2(hdr.left() + x0, hdr.top()), pos2(hdr.left() + x1, hdr.bottom()))
        };
        let mut st = self.top_sort;
        let mut clicked = None;
        for (rect, k, label, right) in [
            (r(0.0, 100.0), ListSort::Size, "Размер", true),
            (r(100.0, 200.0), ListSort::Modified, "Изменён", true),
            (r(208.0, 420.0), ListSort::Name, "Имя / путь", false),
        ] {
            if header(&p, &hresp, rect, label, right, (st.0 == k).then_some(st.1)) {
                clicked = Some(k);
            }
        }
        if let Some(k) = clicked {
            toggle_sort(&mut st, k, ListSort::Name);
            self.top_sort = st;
        }
        let mut act = None;
        let selected = self.selected;
        let order = &self.top_order;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, ROW_H, order.len(), |ui, range| {
                for i in range {
                    let id = order[i];
                    let nd = tree.node(id);
                    let (rect, resp, hover) = row(ui, ROW_H, ("top", id));
                    row_bg(ui, rect, selected == Some(id), hover, i);
                    let p = ui.painter_at(rect);
                    let y = rect.center().y;
                    cell(&p, rect.left() + 100.0, y, human_size(nd.size), text_col);
                    cell(&p, rect.left() + 200.0, y, format_date(nd.mtime), weak);
                    kind_icon(&p, tree, id, pos2(rect.left() + 210.0, y), text_col);
                    p.text(
                        pos2(rect.left() + 230.0, y),
                        Align2::LEFT_CENTER,
                        tree.path(id),
                        font(),
                        text_col,
                    );
                    if resp.clicked() {
                        act = Some(Action::Show(id));
                    }
                    resp.on_hover_text("Нажмите, чтобы показать в обзоре; правая кнопка — меню")
                        .context_menu(|ui| node_menu(ui, tree, id, &mut act));
                }
            });
        if let Some(a) = act {
            self.apply(ui.ctx(), a);
        }
    }

    /// Bottom bar of the cleanup tab while items are ticked.
    pub fn cleanup_bar(&mut self, ui: &mut Ui) {
        let Some(data) = self.data.clone() else { return };
        let tree = &data.out.tree;
        let size: u64 = self.marked.iter().map(|&i| tree.node(i).size).sum();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Выбрано: {} · {}", self.marked.len(), human_size(size))).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let ids: Vec<NodeId> = self.marked.iter().copied().collect();
                if ids.iter().all(|i| data.dup_keep.contains_key(i))
                    && ui
                        .button("В жёсткие ссылки…")
                        .on_hover_text("Оставить имена на местах, но хранить одну копию данных (CreateHardLink)")
                        .clicked()
                {
                    self.request_hardlink(ids.clone());
                }
                if ui
                    .add(egui::Button::new(
                        RichText::new("Удалить безвозвратно…").color(theme::REVIEW),
                    ))
                    .clicked()
                {
                    self.request_delete(ids.clone(), true);
                }
                if theme::primary_button(ui, "Удалить в корзину").clicked() {
                    self.request_delete(ids, false);
                }
                if ui.button("Снять выделение").clicked() {
                    self.marked.clear();
                }
            });
        });
        ui.add_space(2.0);
    }

    pub fn cleanup_view(&mut self, ui: &mut Ui) {
        let Some(data) = self.data.clone() else { return };
        let tree = &data.out.tree;
        let levels = [Safety::Safe, Safety::Likely, Safety::Review];
        let sum = |s: Safety| -> u64 {
            data.cleanup
                .iter()
                .filter(|g| g.category.safety() == s)
                .map(|g| g.size)
                .sum()
        };

        let dup_progress = self.dup_progress();
        let mut dup_cmd = None; // Some(true) = start, Some(false) = cancel
        let mut hardlink_all = false;
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let shown = ui
                    .ctx()
                    .animate_value_with_time(Id::new("cleanup_total"), data.cleanup_total as f32, 0.5);
                ui.label(
                    RichText::new(format!("≈ {}", human_size(shown as u64)))
                        .size(26.0)
                        .strong(),
                );
                ui.label(RichText::new("можно освободить").size(16.0).weak());
            });
            ui.label(
                RichText::new(
                    "Это подсказки по именам и расположению файлов. Отметьте нужное галочками и удалите кнопкой внизу \
                     (по умолчанию — в корзину). Закройте программы, чей кэш чистите.",
                )
                .weak(),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Показывать:");
                for (k, s) in levels.iter().enumerate() {
                    let c = theme::safety_color(*s);
                    let text = RichText::new(format!("{} — {}", s.label(), human_size(sum(*s)))).color(
                        if self.cleanup_show[k] {
                            c
                        } else {
                            ui.visuals().weak_text_color()
                        },
                    );
                    if ui.add(egui::Button::selectable(self.cleanup_show[k], text)).clicked() {
                        self.cleanup_show[k] = !self.cleanup_show[k];
                    }
                }
                ui.separator();
                ui.label("Порядок:");
                ui.selectable_value(&mut self.cleanup_by_size, false, "по надёжности");
                ui.selectable_value(&mut self.cleanup_by_size, true, "по размеру");
            });
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Дубликаты по содержимому").strong());
                if let Some((stage, done, total, bytes, elapsed)) = dup_progress {
                    let step = match stage {
                        spacetree_core::dupes::STAGE_SIZES => "сравнение размеров",
                        spacetree_core::dupes::STAGE_PARTIAL => "хеш начала, середины и конца",
                        _ => "полный хеш",
                    };
                    ui.add(egui::Spinner::new());
                    ui.label(
                        RichText::new(format!(
                            "{step}: {done} из {total} · прочитано {} · {} с",
                            human_size(bytes),
                            elapsed.as_secs()
                        ))
                        .weak(),
                    );
                    if ui.button("Отмена").clicked() {
                        dup_cmd = Some(false);
                    }
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
                } else if let Some(sets) = &data.dup_sets {
                    let wasted = data.dup_wasted();
                    ui.label(
                        RichText::new(if sets.is_empty() {
                            "одинаковых файлов не найдено".to_owned()
                        } else {
                            format!("наборов: {} · лишние копии занимают {}", sets.len(), human_size(wasted))
                        })
                        .weak(),
                    );
                    if !sets.is_empty()
                        && theme::primary_button(ui, "Схлопнуть в жёсткие ссылки (Hard Links)…")
                            .on_hover_text(
                                "Все лишние копии останутся на своих местах (ни одна программа не потеряет файл), \
                                 но на диске будут храниться одним экземпляром: NTFS, один том. Освободится место \
                                 лишних копий. Новейшая копия каждого набора остаётся как есть.",
                            )
                            .clicked()
                    {
                        hardlink_all = true;
                    }
                    if ui.button("Искать заново").clicked() {
                        dup_cmd = Some(true);
                    }
                } else {
                    ui.label(
                        RichText::new(format!(
                            "файлов-кандидатов: {} (от 1 МБ, личные данные). Читаются только файлы одинакового размера.",
                            data.dup_candidates.len()
                        ))
                        .weak(),
                    );
                    if theme::primary_button(ui, "Найти дубликаты").clicked() {
                        dup_cmd = Some(true);
                    }
                }
            });
        });
        match dup_cmd {
            Some(true) => self.start_dups(ui.ctx()),
            Some(false) => self.cancel_dups(),
            None => {}
        }
        if hardlink_all {
            let mut ids: Vec<NodeId> = data.dup_keep.keys().copied().collect();
            ids.sort_by_key(|&i| std::cmp::Reverse(tree.node(i).size));
            self.request_hardlink(ids);
        }
        ui.add_space(8.0);
        if data.cleanup.is_empty() {
            ui.label("Ничего подходящего не найдено.");
            return;
        }

        let mut order: Vec<usize> = (0..data.cleanup.len()).collect();
        if self.cleanup_by_size {
            order.sort_by_key(|&i| std::cmp::Reverse(data.cleanup[i].size));
        }
        let mut act = None;
        let mut toggles: Vec<NodeId> = Vec::new();
        let mut group_toggle: Option<(usize, bool)> = None;
        let mut empty_bin = None;
        let weak = ui.visuals().weak_text_color();
        let text_col = ui.visuals().text_color();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for &gi in &order {
                let g = &data.cleanup[gi];
                let s = g.category.safety();
                if !self.cleanup_show[levels.iter().position(|l| *l == s).unwrap()] {
                    continue;
                }
                let open = self.cleanup_open.contains(&gi);
                let open_t = ui.ctx().animate_bool_with_time(Id::new(("cl_open", gi)), open, 0.18);
                let deletable = !matches!(g.category, Category::Hibernation | Category::VirtualDisks);
                let all_marked = deletable && g.items.iter().all(|i| self.marked.contains(i));
                let header = theme::card(ui)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let (r, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
                            disclosure(ui.painter(), r.center(), open_t, weak);
                            theme::badge(ui, s.label(), theme::safety_color(s));
                            ui.label(RichText::new(g.category.title()).strong().size(14.5));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(RichText::new(human_size(g.size)).strong().size(14.5));
                                ui.label(
                                    RichText::new(format!("{} объектов · {} файлов", g.items.len(), g.files)).weak(),
                                );
                                if g.category == Category::RecycleBin {
                                    if ui.small_button("Очистить корзину").clicked() {
                                        empty_bin = Some(g.items.clone());
                                    }
                                } else if deletable {
                                    let label = if all_marked {
                                        "Снять все"
                                    } else {
                                        "Выбрать все"
                                    };
                                    if ui.small_button(label).clicked() {
                                        group_toggle = Some((gi, !all_marked));
                                    }
                                }
                            });
                        });
                        ui.label(RichText::new(g.category.hint()).weak());
                        if open_t > 0.0 {
                            ui.add_space(4.0 * open_t);
                            ui.scope(|ui| {
                                ui.multiply_opacity(open_t);
                                for (i, &id) in g.items.iter().take(CLEANUP_ITEMS).enumerate() {
                                    let nd = tree.node(id);
                                    let (rect, resp, hover) = row(ui, ROW_H, ("cl_item", id));
                                    let resp = match data.dup_keep.get(&id) {
                                        Some(&keep) if g.category == Category::Duplicates => resp.on_hover_text(
                                            format!("Идентичная копия, которая останется:\n{}", tree.path(keep)),
                                        ),
                                        _ => resp,
                                    };
                                    let ticked = self.marked.contains(&id);
                                    row_bg(ui, rect, self.selected == Some(id) || ticked, hover, i);
                                    let p = ui.painter_at(rect);
                                    let y = rect.center().y;
                                    let mut x0 = rect.left();
                                    if deletable && g.category != Category::RecycleBin {
                                        tick(&p, pos2(x0 + 12.0, y), ticked, ui);
                                        x0 += 24.0;
                                    }
                                    cell(&p, x0 + 90.0, y, human_size(nd.size), text_col);
                                    cell(&p, x0 + 180.0, y, format_date(nd.mtime), weak);
                                    kind_icon(&p, tree, id, pos2(x0 + 190.0, y), text_col);
                                    let path_clip =
                                        Rect::from_min_max(rect.min, pos2(rect.right() - 84.0, rect.bottom()));
                                    ui.painter_at(path_clip).text(
                                        pos2(x0 + 210.0, y),
                                        Align2::LEFT_CENTER,
                                        tree.path(id),
                                        font(),
                                        text_col,
                                    );
                                    let btn = Rect::from_center_size(pos2(rect.right() - 40.0, y), vec2(76.0, 20.0));
                                    let b = ui
                                        .put(
                                            btn,
                                            egui::Button::new(RichText::new("В Проводнике").size(11.5))
                                                .frame(hover > 0.5),
                                        )
                                        .on_hover_text("Показать в Проводнике");
                                    if b.clicked() {
                                        act = Some(Action::Reveal(id));
                                    } else if resp.double_clicked() {
                                        act = Some(Action::Show(id));
                                    } else if resp.clicked() {
                                        if deletable && g.category != Category::RecycleBin {
                                            toggles.push(id);
                                        } else {
                                            act = Some(Action::Select(id));
                                        }
                                    }
                                    resp.context_menu(|ui| node_menu(ui, tree, id, &mut act));
                                }
                                if g.items.len() > CLEANUP_ITEMS {
                                    ui.label(
                                        RichText::new(format!(
                                            "…и ещё {} (показаны крупнейшие; «Выбрать все» отметит и их)",
                                            g.items.len() - CLEANUP_ITEMS
                                        ))
                                        .weak(),
                                    );
                                }
                            });
                        }
                    })
                    .response;
                // the header area (above the item list) toggles the group
                let head = Rect::from_min_size(header.rect.min, vec2((header.rect.width() - 330.0).max(80.0), 52.0));
                if ui.interact(head, Id::new(("cleanup", gi)), Sense::click()).clicked() {
                    if open {
                        self.cleanup_open.remove(&gi);
                    } else {
                        self.cleanup_open.insert(gi);
                    }
                }
                ui.add_space(6.0);
            }
        });
        for id in toggles {
            if !self.marked.remove(&id) {
                self.marked.insert(id);
            }
        }
        if let Some((gi, on)) = group_toggle {
            for &id in &data.cleanup[gi].items {
                if on {
                    self.marked.insert(id);
                } else {
                    self.marked.remove(&id);
                }
            }
        }
        if let Some(bins) = empty_bin {
            self.request_empty_bin(bins);
        }
        let del = ui.input(|i| (i.key_pressed(Key::Delete), i.modifiers.shift));
        if del.0 && !self.marked.is_empty() && self.delete.idle() && !ui.ctx().egui_wants_keyboard_input() {
            self.request_delete(self.marked.iter().copied().collect(), del.1);
        }
        if let Some(a) = act {
            self.apply(ui.ctx(), a);
        }
    }
}
