//! "Automatic rules" window: per-app priority / memory / CPU rules and game mode.

use eframe::egui::{self, RichText};

use crate::config::Config;
use crate::rules::{self, Action, Rule};

#[derive(Default)]
pub struct RulesWindow {
    pub open: bool,
    /// Rules being edited (rows without a name yet live only here).
    draft: Vec<Rule>,
}

impl RulesWindow {
    /// Open the window, optionally with a new rule for `app`.
    pub fn open_for(&mut self, cfg: &Config, app: Option<&str>) {
        if !self.open {
            self.draft = rules::parse(&cfg.rules);
        }
        self.open = true;
        if let Some(a) = app {
            self.draft.push(Rule { app: a.to_string(), action: Action::Priority(true) });
        }
    }

    /// `running`: names of apps running now, offered as suggestions.
    pub fn show(&mut self, ctx: &egui::Context, cfg: &mut Config, running: &[String]) {
        if !self.open {
            return;
        }
        let mut open = true;
        egui::Window::new("Automatic rules").open(&mut open).default_width(560.0).show(ctx, |ui| {
            ui.label(
                RichText::new(
                    "Applied by the background recorder whenever the app is running (within a few seconds of it \
                     starting). Rules change your own apps and containers; system services aren't touched.",
                )
                .weak(),
            );
            if !cfg.record {
                ui.colored_label(ui.visuals().warn_fg_color, "Background recording is off, so rules won't run.");
            }
            ui.add_space(6.0);
            let mut remove = None;
            egui::Grid::new("rules").num_columns(4).spacing([10.0, 6.0]).show(ui, |ui| {
                for (i, r) in self.draft.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut r.app).hint_text("App name").desired_width(190.0));
                        ui.menu_button("⏷", |ui| {
                            ui.label(RichText::new("Running now").weak());
                            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                                for n in running {
                                    if ui.button(n).clicked() {
                                        r.app = n.clone();
                                        ui.close();
                                    }
                                }
                            });
                        });
                    });
                    let kind = match r.action {
                        Action::Priority(_) => 0,
                        Action::Memory(_) => 1,
                        Action::Cpu(_) => 2,
                    };
                    let labels = ["Priority", "Memory cap", "CPU cap"];
                    egui::ComboBox::from_id_salt(("rule_kind", i)).selected_text(labels[kind]).show_ui(ui, |ui| {
                        if ui.selectable_label(kind == 0, labels[0]).clicked() && kind != 0 {
                            r.action = Action::Priority(true);
                        }
                        if ui.selectable_label(kind == 1, labels[1]).clicked() && kind != 1 {
                            r.action = Action::Memory(4.0);
                        }
                        if ui.selectable_label(kind == 2, labels[2]).clicked() && kind != 2 {
                            r.action = Action::Cpu(100.0);
                        }
                    });
                    match &mut r.action {
                        Action::Priority(low) => {
                            ui.horizontal(|ui| {
                                ui.selectable_value(low, true, "Low");
                                ui.selectable_value(low, false, "High");
                            });
                        }
                        Action::Memory(gb) => {
                            ui.add(egui::DragValue::new(gb).range(0.1..=1024.0).speed(0.1).suffix(" GB"));
                        }
                        Action::Cpu(p) => {
                            ui.add(egui::DragValue::new(p).range(5.0..=6400.0).speed(5.0).suffix(" %"))
                                .on_hover_text("Percent of one CPU core: 200% = two cores");
                        }
                    }
                    if ui.small_button("✖").clicked() {
                        remove = Some(i);
                    }
                    ui.end_row();
                }
            });
            if let Some(i) = remove {
                self.draft.remove(i);
            }
            if ui.button("+ Add rule").clicked() {
                self.draft.push(Rule { app: String::new(), action: Action::Priority(true) });
            }
            ui.label(
                RichText::new(
                    "Priority and caps need the app in its own systemd scope (true for apps started from the menu on \
                     most desktops). Otherwise only Low priority works (via nice).",
                )
                .small()
                .weak(),
            );
            ui.separator();
            ui.checkbox(&mut cfg.game_mode, RichText::new("Game mode").strong());
            ui.label(
                RichText::new(
                    "While one of these is running, your other apps get low priority so the game gets the CPU.",
                )
                .weak(),
            );
            ui.add_enabled(
                cfg.game_mode,
                egui::TextEdit::singleline(&mut cfg.game_apps)
                    .hint_text("Steam, Heroic Games Launcher, Lutris")
                    .desired_width(380.0),
            );
        });
        cfg.rules = rules::encode(&self.draft);
        if !open {
            self.open = false;
            self.draft.retain(|r| !r.app.trim().is_empty());
        }
    }
}
