//! Timeline tab: crashes, out-of-memory kills and failed services from the journal.

use std::time::Instant;

use eframe::egui::{self, Color32, RichText};

use crate::app::{Ctx, JobResult};
use crate::events::{Event, Kind};
use crate::ui_history::fmt_time;

const RANGES: [(&str, u64); 4] =
    [("24 h", 86_400), ("7 days", 7 * 86_400), ("30 days", 30 * 86_400), ("90 days", 90 * 86_400)];

#[derive(Default)]
pub struct EventsState {
    range: usize,
    events: Vec<Event>,
    error: Option<String>,
    loaded: Option<(usize, Instant)>,
    loading: bool,
    filter: String,
    hide_services: bool,
}

pub fn kind_color(k: Kind) -> Color32 {
    match k {
        Kind::Crash => Color32::from_rgb(0xdd, 0x4b, 0x4b),
        Kind::OomKill | Kind::Oomd => Color32::from_rgb(0xa3, 0x5c, 0xe0),
        Kind::LimitKill => Color32::from_rgb(0xc0, 0x7c, 0xe8),
        Kind::ServiceFailed => Color32::from_rgb(0xe0, 0xa8, 0x2e),
    }
}

impl EventsState {
    pub fn loaded(&mut self, range: usize, res: Result<Vec<Event>, String>) {
        self.loading = false;
        if range != self.range {
            return;
        }
        match res {
            Ok(v) => {
                self.events = v;
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
        self.loaded = Some((range, Instant::now()));
    }

    pub fn request_reload(&mut self) {
        self.loaded = None;
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, c: &mut Ctx) {
        let stale = !self.loaded.is_some_and(|(r, t)| r == self.range && t.elapsed().as_secs() < 60);
        if stale && !self.loading {
            self.loading = true;
            let (range, secs) = (self.range, RANGES[self.range].1);
            c.jobs.spawn(move || {
                JobResult::Events(range, crate::events::load(crate::recorder::epoch().saturating_sub(secs)))
            });
        }

        ui.horizontal(|ui| {
            for (i, (label, _)) in RANGES.iter().enumerate() {
                if ui.selectable_label(self.range == i, *label).clicked() {
                    self.range = i;
                }
            }
            ui.separator();
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Filter…").desired_width(180.0));
            ui.checkbox(&mut self.hide_services, "Hide failed services");
            if self.loading {
                ui.spinner();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("⟳ Refresh").clicked() {
                    self.loaded = None;
                }
            });
        });

        let count = |k: &[Kind]| self.events.iter().filter(|e| k.contains(&e.kind)).count();
        let (crashes, ooms, failed) = (
            count(&[Kind::Crash]),
            count(&[Kind::OomKill, Kind::LimitKill, Kind::Oomd]),
            count(&[Kind::ServiceFailed]),
        );
        ui.label(
            RichText::new(format!(
                "In the last {}: {crashes} crash{}, {ooms} out-of-memory kill{}, {failed} failed service{}",
                RANGES[self.range].0,
                if crashes == 1 { "" } else { "es" },
                if ooms == 1 { "" } else { "s" },
                if failed == 1 { "" } else { "s" },
            ))
            .strong(),
        );
        if let Some(e) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        ui.label(
            RichText::new(
                "From the system journal. If you're not in the adm group, only your own apps and services are shown.",
            )
            .small()
            .weak(),
        );
        ui.separator();

        let f = self.filter.to_lowercase();
        let shown: Vec<&Event> = self
            .events
            .iter()
            .rev()
            .filter(|e| !(self.hide_services && e.kind == Kind::ServiceFailed))
            .filter(|e| f.is_empty() || e.title.to_lowercase().contains(&f) || e.detail.to_lowercase().contains(&f))
            .collect();
        if shown.is_empty() && !self.loading {
            ui.add_space(12.0);
            ui.label(RichText::new("Nothing crashed or was killed in this period.").weak());
            return;
        }
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        let mut set_filter = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, row_h, shown.len(), |ui, rows| {
            for e in &shown[rows] {
                let r = ui
                    .horizontal(|ui| {
                        ui.add_sized([96.0, row_h], egui::Label::new(RichText::new(fmt_time(e.t, true)).weak()));
                        ui.add_sized(
                            [116.0, row_h],
                            egui::Label::new(RichText::new(e.kind.label()).color(kind_color(e.kind)).strong()),
                        );
                        ui.add(egui::Label::new(RichText::new(&e.title).strong()).truncate());
                        ui.add(egui::Label::new(RichText::new(&e.detail).weak()).truncate());
                    })
                    .response
                    .interact(egui::Sense::click());
                r.on_hover_text(format!("{}\n{}", e.title, e.detail)).context_menu(|ui| {
                    if ui.button("Copy").clicked() {
                        ui.ctx().copy_text(format!(
                            "{}  {}  {}  {}",
                            fmt_time(e.t, true),
                            e.kind.label(),
                            e.title,
                            e.detail
                        ));
                        ui.close();
                    }
                    if ui.button(format!("Only show {}", e.name)).clicked() {
                        set_filter = Some(e.name.clone());
                        ui.close();
                    }
                });
            }
        });
        if let Some(n) = set_filter {
            self.filter = n;
        }
    }
}
