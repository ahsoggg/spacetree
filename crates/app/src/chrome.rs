//! Window chrome: toolbar, tabs, status bar, welcome / scanning screens, settings window, toasts.

use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Id, Key, Layout, Margin, Order, Rect, RichText, Sense, Stroke,
    StrokeKind, Ui, pos2, vec2,
};
use spacetree_core::Backend;
use spacetree_core::Used;
use spacetree_core::stats::human_size;

use crate::{App, Tab, ToastKind, diag, relaunch_elevated, theme, update, winint};

const TOAST_LIFE: f32 = 4.5;

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

impl App {
    pub fn toolbar(&mut self, ui: &mut Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if let Some(icon) = &self.icon {
                ui.add(egui::Image::new(icon).fit_to_exact_size(vec2(20.0, 20.0)));
            }
            ui.label(RichText::new("SpaceTree").strong().size(16.0));
            ui.add_space(8.0);
            let scanning = self.job_running();
            ui.add_enabled_ui(!scanning, |ui| {
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.path_text)
                        .hint_text("Диск или папка")
                        .desired_width(260.0),
                );
                if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    let ctx = ui.ctx().clone();
                    self.start_scan(&ctx);
                }
                let mut pick = None;
                egui::ComboBox::from_id_salt("drives")
                    .selected_text("Диски")
                    .height(420.0)
                    .show_ui(ui, |ui| {
                        for d in &self.drives {
                            let label = match d.space {
                                Some((t, f)) => format!("{}   свободно {} из {}", d.path, human_size(f), human_size(t)),
                                None => d.path.clone(),
                            };
                            if ui.selectable_label(false, label).clicked() {
                                pick = Some(d.path.clone());
                            }
                        }
                        if !self.settings.recent.is_empty() {
                            ui.separator();
                            ui.label(RichText::new("Недавние").weak().small());
                            for r in &self.settings.recent {
                                if ui.selectable_label(false, r).clicked() {
                                    pick = Some(r.clone());
                                }
                            }
                        }
                    });
                if let Some(p) = pick {
                    self.path_text = p;
                }
                if ui.button("Папка…").on_hover_text("Выбрать папку").clicked()
                    && let Some(p) = rfd::FileDialog::new().pick_folder()
                {
                    self.path_text = p.to_string_lossy().into_owned();
                }
                egui::ComboBox::from_id_salt("backend")
                    .selected_text(match self.backend {
                        Backend::Auto => "Авто",
                        Backend::Walk => "Обход папок",
                        Backend::Mft => "MFT",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.backend, Backend::Auto, "Авто")
                            .on_hover_text("MFT для целого NTFS-диска при правах администратора, иначе обход папок");
                        ui.selectable_value(&mut self.backend, Backend::Walk, "Обход папок");
                        ui.selectable_value(&mut self.backend, Backend::Mft, "MFT (только корень NTFS-диска)");
                    })
                    .response
                    .on_hover_text("Способ сканирования");
            });
            if scanning {
                if ui.button("⏹ Отмена").clicked() {
                    self.cancel_scan();
                }
            } else if theme::primary_button(ui, "▶ Сканировать").clicked() {
                let ctx = ui.ctx().clone();
                self.start_scan(&ctx);
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("⚙").on_hover_text("Настройки").clicked() {
                    self.show_settings = !self.show_settings;
                }
                self.update_chip(ui);
                if ui
                    .button("♻ Восстановление")
                    .on_hover_text("Найти и восстановить удалённые файлы (NTFS)")
                    .clicked()
                {
                    self.recover.open = !self.recover.open;
                    if let Some(c) = self.path_text.chars().next().filter(|c| c.is_ascii_alphabetic()) {
                        self.recover.set_drive(c.to_ascii_uppercase());
                    }
                }
                if let Some(data) = self.data.clone() {
                    if ui.button("⬇ CSV").on_hover_text("Экспорт всего дерева в CSV").clicked() {
                        self.export_csv(data);
                    }
                    let id = Id::new("search");
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.search)
                            .id(id)
                            .hint_text("🔍 Поиск по имени (Ctrl+F)")
                            .desired_width(220.0),
                    );
                    if resp.changed() {
                        self.search_edited = Some(std::time::Instant::now());
                        self.tab = Tab::Overview;
                    }
                    if ui.input(|i| i.modifiers.command && i.key_pressed(Key::F)) {
                        resp.request_focus();
                    }
                    if resp.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                        self.clear_search();
                    }
                }
            });
        });
        ui.add_space(2.0);
    }

    /// Small update indicator in the toolbar.
    fn update_chip(&mut self, ui: &mut Ui) {
        let state = self.update.lock().unwrap().clone();
        match state {
            update::State::Downloading { version, done, total } => {
                let pct = (done * 100).checked_div(total).unwrap_or(0);
                ui.add(egui::Spinner::new().size(14.0));
                ui.label(RichText::new(format!("Обновление {version}: {pct}%")).weak());
            }
            update::State::Ready { version } => {
                if theme::primary_button(ui, &format!("⟳ Перезапустить ({version})")).clicked() {
                    self.restart_for_update(ui.ctx());
                }
            }
            update::State::Available { version, .. }
                if ui
                    .button(RichText::new(format!("Доступна {version}")).color(theme::ACCENT))
                    .clicked() =>
            {
                self.show_settings = true;
            }
            _ => {}
        }
    }

    pub fn restart_for_update(&mut self, ctx: &egui::Context) {
        match update::restart() {
            Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Err(e) => self.toast(format!("Не удалось перезапустить: {e}"), ToastKind::Error),
        }
    }

    /// Text tabs with an accent underline that slides to the active one.
    pub fn tabs(&mut self, ui: &mut Ui) {
        let Some(data) = self.data.clone() else { return };
        ui.add_space(4.0);
        let cleanup = format!("Можно удалить · {}", human_size(data.cleanup_total));
        let growth = match self.growth.tab_hint() {
            Some(h) => format!("Что выросло · {h}"),
            None => "Что выросло".to_owned(),
        };
        let streams = format!("Скрытые потоки · {}", self.streams_left());
        let mut tabs = vec![
            (Tab::Overview, "Обзор"),
            (Tab::Types, "Типы файлов"),
            (Tab::Top, "Крупнейшие файлы"),
            (Tab::Growth, growth.as_str()),
            (Tab::Cleanup, cleanup.as_str()),
        ];
        if !data.out.record_nodes.is_empty() {
            tabs.push((Tab::Streams, streams.as_str())); // only an MFT scan can see alternate data streams
        }
        let mut active_rect = None;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 18.0;
            for (tab, label) in tabs {
                let on = self.tab == tab;
                let font = FontId::proportional(14.0);
                let galley = ui.painter().layout_no_wrap(label.to_owned(), font, Color32::WHITE);
                let (rect, resp) = ui.allocate_exact_size(galley.size() + vec2(4.0, 10.0), Sense::click());
                let hover = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
                let v = ui.visuals();
                let color = if on {
                    v.strong_text_color()
                } else {
                    theme::lerp_color(v.weak_text_color(), v.text_color(), hover)
                };
                ui.painter()
                    .galley_with_override_text_color(rect.center() - galley.size() / 2.0, galley, color);
                if on {
                    active_rect = Some(rect);
                }
                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    self.tab = tab;
                }
            }
        });
        if let Some(r) = active_rect {
            let ctx = ui.ctx();
            let x = ctx.animate_value_with_time(Id::new("tab_x"), r.left(), 0.22);
            let w = ctx.animate_value_with_time(Id::new("tab_w"), r.width(), 0.22);
            let line = Rect::from_min_size(pos2(x, r.bottom() - 1.0), vec2(w, 3.0));
            ui.painter().rect_filled(line, 2.0, theme::ACCENT);
        }
    }

    pub fn status_bar(&mut self, ui: &mut Ui) {
        let mut toggle_live = false;
        ui.horizontal(|ui| {
            if let Some(d) = &self.data {
                let (size, alloc, files) = d.out.tree.root_stats();
                ui.label(format!(
                    "Файлов: {files} · папок: {} · размер: {} · на диске: {}",
                    d.out.tree.dir_count().saturating_sub(1),
                    human_size(size),
                    human_size(alloc),
                ));
                ui.separator();
                let used = match d.out.selection.used {
                    Used::Mft => "MFT",
                    Used::Walk => "обход папок",
                };
                ui.label(format!("{used} · {:.2} с", d.out.elapsed.as_secs_f64()))
                    .on_hover_text(format!(
                        "Способ: {}\nАнализ после сканирования: {:.2} с",
                        d.out.selection.reason,
                        d.analysis.as_secs_f64()
                    ));
                if d.out.errors > 0 {
                    ui.separator();
                    ui.label(RichText::new(format!("⚠ нет доступа: {}", d.out.errors)).color(theme::LIKELY))
                        .on_hover_text(
                            d.out
                                .error_samples
                                .iter()
                                .take(20)
                                .cloned()
                                .collect::<Vec<_>>()
                                .join("\n"),
                        );
                }
                if let Some(id) = self.selected
                    && !d.out.tree.is_removed(id)
                {
                    ui.separator();
                    let nd = d.out.tree.node(id);
                    ui.label(RichText::new(format!("{} · {}", d.out.tree.name(id), human_size(nd.size))).weak());
                }
            }
            if self.live_available() {
                ui.separator();
                let on = self.live.active();
                let text = if on {
                    RichText::new(format!("● Live · {}", self.live.applied)).color(theme::SAFE)
                } else {
                    RichText::new("○ Live").color(ui.visuals().weak_text_color())
                };
                let (events, dropped) = self.live.stats();
                let tip = if on {
                    format!(
                        "Карта следует за диском через журнал USN NTFS. Применено изменений: {} · файлов затронуто: {events}\nНе удалось разместить: {} · пропущено при перегрузке: {dropped}\nЕсли два последних числа растут, нажмите F5. Нажмите, чтобы выключить.",
                        self.live.applied, self.live.unplaced
                    )
                } else {
                    "Следить за изменениями на диске (журнал USN): карта будет перестраиваться, когда что-то пишет гигабайты. Нажмите, чтобы включить.".to_owned()
                };
                if ui.add(egui::Button::new(text).frame(false)).on_hover_text(tip).clicked() {
                    toggle_live = true;
                }
            }
            if !self.message.is_empty() {
                ui.colored_label(ui.visuals().error_fg_color, &self.message);
            }
            if let Some((total, free)) = self.data.as_ref().and_then(|d| d.disk) {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(100.0, 8.0), Sense::hover());
                    theme::bar(ui, r, 1.0 - free as f32 / total.max(1) as f32, theme::ACCENT);
                    ui.label(format!("Свободно {} из {}", human_size(free), human_size(total)));
                });
            }
        });
        if toggle_live {
            let ctx = ui.ctx().clone();
            if self.live.active() {
                self.stop_live();
                self.settings.live_monitor = false;
            } else {
                self.start_live(&ctx);
                self.settings.live_monitor = self.live.active();
            }
            self.settings.save();
        }
    }

    pub fn welcome(&mut self, ui: &mut Ui) {
        let appear = ui.ctx().animate_bool_with_time(Id::new("welcome_in"), true, 0.35);
        ui.multiply_opacity(appear);
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.10).max(16.0) + (1.0 - appear) * 20.0);
            if let Some(icon) = &self.icon {
                ui.add(egui::Image::new(icon).fit_to_exact_size(vec2(56.0, 56.0)));
                ui.add_space(6.0);
            }
            ui.label(RichText::new("Что занимает место?").size(26.0).strong());
            ui.label(
                RichText::new("Выберите диск или папку — SpaceTree покажет, куда ушло место и что можно удалить.")
                    .weak(),
            );
        });
        ui.add_space(20.0);

        // drive cards, centered, wrapping into rows
        let (card_w, card_h, gap) = (240.0, 84.0, 12.0);
        let avail = ui.available_width();
        let cols = (((avail + gap) / (card_w + gap)).floor() as usize).clamp(1, self.drives.len().max(1));
        let rows = self.drives.len().div_ceil(cols);
        let (area, _) = ui.allocate_exact_size(vec2(avail, rows as f32 * (card_h + gap)), Sense::hover());
        let mut chosen = None;
        for (i, d) in self.drives.iter().enumerate() {
            let (row, col) = (i / cols, i % cols);
            let in_row = (self.drives.len() - row * cols).min(cols) as f32;
            let left = area.left() + (avail - in_row * (card_w + gap) + gap) / 2.0;
            let base = Rect::from_min_size(
                pos2(
                    left + col as f32 * (card_w + gap),
                    area.top() + row as f32 * (card_h + gap),
                ),
                vec2(card_w, card_h),
            );
            let id = Id::new(("drive", i));
            let resp = ui
                .interact(base, id, Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text("Сканировать");
            let h = ui.ctx().animate_bool_with_time(id, resp.hovered(), 0.15);
            let rect = base.translate(vec2(0.0, -3.0 * h));
            let v = ui.visuals();
            let fill = theme::lerp_color(v.window_fill, v.widgets.hovered.weak_bg_fill, h);
            let stroke = Stroke::new(
                1.0,
                theme::lerp_color(v.widgets.noninteractive.bg_stroke.color, theme::ACCENT, h),
            );
            let (text, weak) = (v.text_color(), v.weak_text_color());
            let p = ui.painter();
            if h > 0.0 {
                p.rect_filled(
                    rect.translate(vec2(0.0, 4.0)).expand(1.0),
                    12.0,
                    Color32::from_black_alpha((50.0 * h) as u8),
                );
            }
            p.rect(rect, 10.0, fill, stroke, StrokeKind::Inside);
            let inner = rect.shrink(14.0);
            p.text(
                inner.left_top(),
                Align2::LEFT_TOP,
                &d.path,
                FontId::proportional(18.0),
                text,
            );
            if let Some((total, free)) = d.space {
                let frac = (total - free) as f32 / total.max(1) as f32;
                let bar = Rect::from_min_size(inner.left_top() + vec2(0.0, 28.0), vec2(inner.width(), 8.0));
                let shown = frac * ui.ctx().animate_bool_with_time(Id::new(("drive_bar", i)), true, 0.6);
                theme::bar(ui, bar, shown, if frac > 0.9 { theme::REVIEW } else { theme::ACCENT });
                ui.painter().text(
                    inner.left_bottom(),
                    Align2::LEFT_BOTTOM,
                    format!("свободно {} из {}", human_size(free), human_size(total)),
                    FontId::proportional(12.5),
                    weak,
                );
            }
            if resp.clicked() {
                chosen = Some(d.path.clone());
            }
        }

        ui.vertical_centered(|ui| {
            ui.add_space(6.0);
            if ui.button("Выбрать папку…").clicked()
                && let Some(p) = rfd::FileDialog::new().pick_folder()
            {
                chosen = Some(p.to_string_lossy().into_owned());
            }
            if !self.settings.recent.is_empty() {
                ui.add_space(14.0);
                ui.label(RichText::new("Недавние").weak());
                for r in self.settings.recent.iter().take(5) {
                    if ui.link(r).clicked() {
                        chosen = Some(r.clone());
                    }
                }
            }
            ui.add_space(20.0);
            if cfg!(windows) && !self.mft_ready {
                theme::card(ui).show(ui, |ui| {
                    ui.set_max_width(560.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new("⚡ Быстрый режим").strong().color(theme::LIKELY));
                        ui.label(
                            RichText::new(
                                "Без прав администратора диск сканируется обходом папок. С правами администратора \
                                 SpaceTree читает таблицу MFT напрямую — для целых дисков это обычно намного быстрее.",
                            )
                            .weak(),
                        );
                        ui.add_space(4.0);
                        if ui.button("Перезапустить от администратора").clicked() {
                            match relaunch_elevated(self.path_text.trim()) {
                                Ok(()) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
                                Err(e) => self.message = e,
                            }
                        }
                    });
                });
            }
            if cfg!(windows) && !winint::running_installed() {
                ui.add_space(10.0);
                if ui
                    .link("Установить SpaceTree: ярлык в «Пуске» и пункт «Сканировать» в меню папок")
                    .clicked()
                {
                    self.show_settings = true;
                }
            }
        });
        if let Some(p) = chosen {
            self.path_text = p;
            let ctx = ui.ctx().clone();
            self.start_scan(&ctx);
        }
    }

    pub fn scanning(&mut self, ui: &mut Ui) {
        let Some((path, files, bytes, errors, elapsed, fraction, building)) = self.job_progress() else {
            return;
        };
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.22).max(16.0));
            ui.add(egui::Spinner::new().size(42.0).color(theme::ACCENT));
            ui.add_space(14.0);
            ui.label(RichText::new(format!("Сканирование {path}")).size(20.0).strong());
            ui.add_space(12.0);
            match fraction {
                Some(f) if !building => {
                    let shown = ui.ctx().animate_value_with_time(Id::new("scan_frac"), f, 0.2);
                    ui.add(egui::ProgressBar::new(shown).desired_width(420.0).show_percentage());
                }
                _ => theme::marquee(ui, 420.0),
            }
            if building {
                ui.label(RichText::new("Построение дерева…").weak());
            }
            ui.add_space(12.0);
            // numbers roll smoothly instead of jumping every 100 ms
            let shown_files = ui
                .ctx()
                .animate_value_with_time(Id::new("scan_files"), files as f32, 0.15);
            let size = if bytes > 0 {
                format!(" · {}", human_size(bytes))
            } else {
                String::new()
            };
            ui.label(
                RichText::new(format!(
                    "{} файлов{size} · {:.1} с",
                    shown_files as u64,
                    elapsed.as_secs_f32()
                ))
                .size(15.0),
            );
            if errors > 0 {
                ui.label(RichText::new(format!("нет доступа: {errors}")).weak());
            }
            ui.add_space(16.0);
            if ui.button("⏹ Отмена").clicked() {
                self.cancel_scan();
            }
        });
        ui.ctx().request_repaint_after(Duration::from_millis(50));
    }

    pub fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let mut open = true;
        egui::Window::new("Настройки")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(460.0)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.heading("Обновления");
                let mut auto = self.settings.auto_update;
                if ui
                    .checkbox(&mut auto, "Автоматически скачивать и устанавливать обновления")
                    .changed()
                {
                    self.settings.auto_update = auto;
                    self.settings.save();
                }
                ui.horizontal(|ui| {
                    ui.label(format!("Версия {}", env!("CARGO_PKG_VERSION")));
                    let state = self.update.lock().unwrap().clone();
                    let busy = matches!(state, update::State::Checking | update::State::Downloading { .. });
                    if ui.add_enabled(!busy, egui::Button::new("Проверить сейчас")).clicked() {
                        self.maybe_check_updates(ctx, true);
                    }
                    match state {
                        update::State::Idle => {}
                        update::State::Checking => {
                            ui.add(egui::Spinner::new());
                        }
                        update::State::UpToDate => {
                            ui.label(RichText::new("✔ актуальная версия").color(theme::SAFE));
                        }
                        update::State::Available { version, url } => {
                            ui.label(format!("доступна {version}"));
                            if ui.button("Скачать и установить").clicked() {
                                self.settings.auto_update = true;
                                self.settings.save();
                                self.maybe_check_updates(ctx, true);
                            }
                            ui.hyperlink_to("страница релиза", url);
                        }
                        update::State::Downloading { done, total, .. } => {
                            ui.add(
                                egui::ProgressBar::new(done as f32 / total.max(1) as f32)
                                    .desired_width(120.0)
                                    .show_percentage(),
                            );
                        }
                        update::State::Ready { version } => {
                            if ui.button(format!("Перезапустить ({version})")).clicked() {
                                self.restart_for_update(ctx);
                            }
                        }
                        update::State::Failed(e) => {
                            ui.label(RichText::new(e).color(ui.visuals().error_fg_color));
                        }
                    }
                });

                ui.add_space(8.0);
                ui.heading("Интерфейс");
                let mut age = self.settings.color_by_age;
                if ui
                    .checkbox(&mut age, "Раскрашивать treemap по дате изменения")
                    .changed()
                {
                    self.settings.color_by_age = age;
                    self.settings.save();
                }
                let mut confirm = self.settings.confirm_delete;
                if ui
                    .checkbox(&mut confirm, "Спрашивать подтверждение перед удалением в корзину")
                    .changed()
                {
                    self.settings.confirm_delete = confirm;
                    self.settings.save();
                }

                let mut snaps = self.settings.save_snapshots;
                if ui
                    .checkbox(&mut snaps, "Сохранять снимок каждого сканирования («Что выросло»)")
                    .on_hover_text("Хранится только список крупных папок и их размеры — около мегабайта на диск")
                    .changed()
                {
                    self.settings.save_snapshots = snaps;
                    self.settings.save();
                }

                ui.add_space(8.0);
                ui.heading("Диагностика");
                ui.label(
                    RichText::new(
                        "Журнал ошибок и отчёты о сбоях лежат на этом компьютере и никуда не отправляются. Если что-то сломалось — пришлите их разработчику.",
                    )
                    .weak(),
                );
                ui.horizontal(|ui| {
                    if ui.button("Открыть папку журнала").clicked()
                        && let Some(d) = diag::logs_dir()
                    {
                        let _ = std::fs::create_dir_all(&d);
                        crate::open_path(&d.to_string_lossy());
                    }
                    if ui.button("Скопировать диагностику").clicked() {
                        ctx.copy_text(diag::report());
                        self.toast("Диагностика скопирована в буфер обмена", ToastKind::Info);
                    }
                });

                if cfg!(windows) {
                    ui.add_space(8.0);
                    ui.heading("Интеграция с Windows");
                    self.integration_settings(ui);
                }

                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(RichText::new("SpaceTree").strong());
                    ui.hyperlink_to("GitHub", winint::REPO_URL);
                });
            });
        if !open {
            self.show_settings = false;
        }
    }

    fn integration_settings(&mut self, ui: &mut Ui) {
        if winint::running_installed() {
            ui.label(
                RichText::new("✔ SpaceTree установлен: ярлык в меню «Пуск», поиск по слову «SpaceTree».")
                    .color(theme::SAFE),
            );
            let mut menu = winint::context_menu_enabled();
            if ui
                .checkbox(
                    &mut menu,
                    "Пункт «Сканировать в SpaceTree» в контекстном меню папок и дисков",
                )
                .on_hover_text("В Windows 11 — в разделе «Показать дополнительные параметры» (Shift+F10)")
                .changed()
                && let Ok(exe) = std::env::current_exe()
                && let Err(e) = winint::set_context_menu(menu, &exe)
            {
                self.toast(e, ToastKind::Error);
            }
        } else {
            ui.label(
                RichText::new(
                    "Сейчас запущена переносная копия. Установка добавит SpaceTree в меню «Пуск» (поиск по слову \
                     «SpaceTree»), пункт «Сканировать в SpaceTree» в меню папок и запись в «Приложения». \
                     Права администратора не нужны.",
                )
                .weak(),
            );
            if theme::primary_button(ui, "Установить SpaceTree").clicked() {
                let opts = winint::InstallOptions {
                    desktop_shortcut: false,
                    context_menu: true,
                };
                match winint::install(opts) {
                    Ok(exe) => {
                        self.toast(
                            "SpaceTree установлен. Найдите его в «Пуске» или через правый клик по папке.",
                            ToastKind::Success,
                        );
                        let _ = std::process::Command::new(exe).spawn();
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    Err(e) => self.toast(e, ToastKind::Error),
                }
            }
        }
    }

    /// Notifications in the bottom-right corner: slide in, stay a few seconds, fade out.
    pub fn draw_toasts(&mut self, ctx: &egui::Context) {
        let now = std::time::Instant::now();
        self.toasts
            .retain(|t| t.restart || now.duration_since(t.born).as_secs_f32() < TOAST_LIFE + 0.4);
        if self.toasts.is_empty() {
            return;
        }
        let screen = ctx.content_rect();
        let mut y = screen.bottom() - 44.0;
        let mut dismiss = None;
        let mut restart = false;
        for (i, t) in self.toasts.iter().enumerate().rev() {
            let age = now.duration_since(t.born).as_secs_f32();
            let appear = ease_out(age / 0.25);
            let fade = if t.restart {
                1.0
            } else {
                1.0 - ((age - TOAST_LIFE) / 0.4).clamp(0.0, 1.0)
            };
            let alpha = appear * fade;
            let accent = match t.kind {
                ToastKind::Info => theme::ACCENT,
                ToastKind::Success => theme::SAFE,
                ToastKind::Error => theme::REVIEW,
            };
            let resp = egui::Area::new(Id::new(("toast", t.born)))
                .order(Order::Foreground)
                .pivot(Align2::RIGHT_BOTTOM)
                .fixed_pos(pos2(screen.right() - 16.0 + (1.0 - appear) * 40.0, y))
                .interactable(true)
                .show(ctx, |ui| {
                    ui.multiply_opacity(alpha);
                    egui::Frame::new()
                        .fill(ui.visuals().window_fill)
                        .stroke(Stroke::new(1.0, accent.gamma_multiply(0.7)))
                        .corner_radius(CornerRadius::same(10))
                        .shadow(ui.visuals().popup_shadow)
                        .inner_margin(Margin::symmetric(14, 10))
                        .show(ui, |ui| {
                            ui.set_max_width(380.0);
                            ui.horizontal(|ui| {
                                let (r, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                                ui.painter().circle_filled(r.center(), 4.0, accent);
                                ui.label(&t.text);
                            });
                            if t.restart {
                                ui.horizontal(|ui| {
                                    if theme::primary_button(ui, "Перезапустить").clicked() {
                                        restart = true;
                                    }
                                    if ui.button("Позже").clicked() {
                                        dismiss = Some(i);
                                    }
                                });
                            }
                        });
                })
                .response;
            if !t.restart && resp.clicked() {
                dismiss = Some(i);
            }
            y -= resp.rect.height() + 8.0;
        }
        if let Some(i) = dismiss {
            self.toasts.remove(i);
        }
        if restart {
            self.restart_for_update(ctx);
        }
        ctx.request_repaint_after(Duration::from_millis(30));
    }
}

impl App {
    pub fn job_running(&self) -> bool {
        self.job.is_some()
    }

    pub fn cancel_scan(&self) {
        if let Some(j) = &self.job {
            j.progress.cancel();
        }
    }

    #[allow(clippy::type_complexity)]
    fn job_progress(&self) -> Option<(String, u64, u64, u64, Duration, Option<f32>, bool)> {
        let j = self.job.as_ref()?;
        let p = &j.progress;
        Some((
            j.path.clone(),
            p.files.load(Relaxed),
            p.bytes.load(Relaxed),
            p.errors.load(Relaxed),
            j.started.elapsed(),
            p.fraction(),
            p.phase.load(Relaxed) == spacetree_core::progress::PHASE_BUILDING,
        ))
    }
}
