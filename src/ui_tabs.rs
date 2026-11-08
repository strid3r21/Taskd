//! Users, Services and Startup tabs.

use std::collections::HashMap;

use eframe::egui::{self, Align, Color32, Layout, RichText, Sense};
use egui_extras::{Column, TableBuilder};

use crate::app::{Ctx, JobResult, Tab};
use crate::services::{self, Service};
use crate::startup::{self, StartupEntry};
use crate::ui_processes::ProcState;
use crate::util::{fmt_bytes, fmt_rate};

fn right(ui: &mut egui::Ui, s: String) {
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.label(s));
}

// ---------------------------------------------------------------- Users

#[derive(Default)]
pub struct UsersState {
    selected: Option<String>,
}

struct UserRow {
    name: String,
    procs: usize,
    cpu: f32,
    mem: u64,
    read: f64,
    write: f64,
}

impl UsersState {
    pub fn ui(&mut self, ui: &mut egui::Ui, c: &mut Ctx, procs: &mut ProcState) {
        let mut by: HashMap<&str, UserRow> = HashMap::new();
        for p in c.snap.procs.iter().filter(|p| !p.kernel) {
            let r = by.entry(&p.user).or_insert_with(|| UserRow {
                name: p.user.to_string(),
                procs: 0,
                cpu: 0.0,
                mem: 0,
                read: 0.0,
                write: 0.0,
            });
            r.procs += 1;
            r.cpu += p.cpu;
            r.mem += p.mem;
            r.read += p.read_rate;
            r.write += p.write_rate;
        }
        let mut rows: Vec<UserRow> = by.into_values().collect();
        rows.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(b.mem.cmp(&a.mem)));

        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} users with processes", rows.len())).weak());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add_enabled(self.selected.is_some(), egui::Button::new("Show processes")).clicked()
                    && let Some(u) = &self.selected
                {
                    procs.show_user(u);
                    *c.switch_to = Some(Tab::Processes);
                }
            });
        });
        let mut clicked = None;
        let mut open = None;
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .auto_shrink([false, false])
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::initial(200.0).at_least(80.0))
            .columns(Column::initial(105.0).at_least(60.0), 5)
            .header(24.0, |mut h| {
                h.col(|ui| {
                    ui.strong("User");
                });
                for t in ["Processes", "CPU", "Memory", "Disk read", "Disk write"] {
                    h.col(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.strong(t));
                    });
                }
            })
            .body(|body| {
                body.rows(22.0, rows.len(), |mut row| {
                    let r = &rows[row.index()];
                    row.set_selected(self.selected.as_deref() == Some(&r.name));
                    row.col(|ui| {
                        ui.label(RichText::new(&r.name).strong());
                    });
                    row.col(|ui| right(ui, r.procs.to_string()));
                    row.col(|ui| right(ui, format!("{:.1}%", r.cpu)));
                    row.col(|ui| right(ui, fmt_bytes(r.mem)));
                    row.col(|ui| right(ui, fmt_rate(r.read)));
                    row.col(|ui| right(ui, fmt_rate(r.write)));
                    let resp = row.response();
                    if resp.clicked() {
                        clicked = Some(r.name.clone());
                    }
                    if resp.double_clicked() {
                        open = Some(r.name.clone());
                    }
                });
            });
        if clicked.is_some() {
            self.selected = clicked;
        }
        if let Some(u) = open {
            procs.show_user(&u);
            *c.switch_to = Some(Tab::Processes);
        }
    }
}

// ---------------------------------------------------------------- Services

#[derive(Default)]
pub struct ServicesState {
    list: Vec<Service>,
    error: Option<String>,
    loading: bool,
    loaded_for: Option<bool>,
    user: bool,
    filter: String,
    selected: Option<String>,
    only_running: bool,
    reload: bool,
    /// Log window: (unit, text or error, still loading).
    logs: Option<(String, Result<String, String>, bool)>,
}

/// Right-click actions on a service: (systemctl verb, menu label, hover help).
const SERVICE_ACTIONS: &[(&str, &str, &str)] = &[
    ("start", "Start", "Start the service"),
    ("stop", "Stop", "Stop the service"),
    ("restart", "Restart", "Stop and start it again"),
    ("reload", "Reload", "Ask it to reload its configuration (not every service supports this)"),
    ("freeze", "Pause", "Suspend all of the service's processes (systemctl freeze)"),
    ("thaw", "Resume", "Resume a paused service (systemctl thaw)"),
    ("enable", "Enable at boot", "Start automatically at boot/login"),
    ("disable", "Disable at boot", "Don't start automatically"),
];

impl ServicesState {
    pub fn logs_loaded(&mut self, unit: String, res: Result<String, String>) {
        if self.logs.as_ref().is_some_and(|(u, ..)| *u == unit) {
            self.logs = Some((unit, res, false));
        }
    }

    fn control(&mut self, c: &mut Ctx, unit: &str, verb: &'static str, label: &str) {
        let (unit, user) = (unit.to_string(), self.user);
        c.info(format!("{label} {unit}…"));
        c.jobs.spawn(move || match services::control(user, verb, &unit) {
            Ok(()) => JobResult::Status(format!("{verb} {unit}: done"), false),
            Err(e) => JobResult::Status(format!("{verb} {unit} failed: {e}"), true),
        });
        self.reload = true;
    }

    fn show_logs(&mut self, c: &Ctx, unit: &str) {
        let (u, user) = (unit.to_string(), self.user);
        self.logs = Some((u.clone(), Ok(String::new()), true));
        c.jobs.spawn(move || {
            let r = services::logs(user, &u, 300);
            JobResult::ServiceLogs(u, r)
        });
    }

    fn logs_window(&mut self, ctx: &egui::Context) {
        let Some((unit, res, loading)) = &self.logs else { return };
        let mut open = true;
        egui::Window::new(format!("Logs: {unit}"))
            .id(egui::Id::new("service_logs"))
            .open(&mut open)
            .default_size([760.0, 440.0])
            .resizable(true)
            .show(ctx, |ui| {
                if *loading {
                    ui.spinner();
                    return;
                }
                match res {
                    Ok(text) => {
                        egui::ScrollArea::both().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                            ui.add(egui::Label::new(RichText::new(text).monospace().size(12.0)).extend());
                        });
                    }
                    Err(e) => {
                        ui.colored_label(ui.visuals().error_fg_color, e);
                    }
                }
            });
        if !open {
            self.logs = None;
        }
    }

    pub fn request_reload(&mut self) {
        self.reload = true;
    }

    pub fn loaded(&mut self, user: bool, res: Result<Vec<Service>, String>) {
        self.loading = false;
        if user != self.user {
            return; // stale result from a scope we've switched away from
        }
        self.loaded_for = Some(user);
        match res {
            Ok(l) => {
                self.list = l;
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn load(&mut self, c: &Ctx) {
        self.loading = true;
        self.reload = false;
        let user = self.user;
        c.jobs.spawn(move || JobResult::Services(user, services::list(user)));
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, c: &mut Ctx, procs: &mut ProcState) {
        if !self.loading && (self.reload || self.loaded_for != Some(self.user)) {
            self.load(c);
        }
        let sel = self.selected.clone().and_then(|u| self.list.iter().find(|s| s.unit == u).cloned());
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("🔍 Filter services").desired_width(240.0));
            ui.selectable_value(&mut self.user, false, "System");
            ui.selectable_value(&mut self.user, true, "User");
            ui.checkbox(&mut self.only_running, "Running only");
            if ui.button("⟳ Refresh").on_hover_text("F5").clicked() {
                self.reload = true;
            }
            if self.loading {
                ui.spinner();
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let enabled = sel.is_some();
                for (verb, label) in [
                    ("disable", "Disable"),
                    ("enable", "Enable"),
                    ("restart", "Restart"),
                    ("stop", "Stop"),
                    ("start", "Start"),
                ] {
                    if ui
                        .add_enabled(enabled, egui::Button::new(label))
                        .on_hover_text("More actions: right-click a service")
                        .clicked()
                        && let Some(s) = &sel
                    {
                        self.control(c, &s.unit.clone(), verb, label);
                    }
                }
            });
        });
        if let Some(e) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        let needle = self.filter.to_lowercase();
        let rows: Vec<&Service> = self
            .list
            .iter()
            .filter(|s| !self.only_running || s.sub == "running")
            .filter(|s| {
                needle.is_empty()
                    || s.unit.to_lowercase().contains(&needle)
                    || s.description.to_lowercase().contains(&needle)
            })
            .collect();
        let mut clicked = None;
        enum Menu {
            Control(String, &'static str, &'static str),
            Processes(String),
            Logs(String),
        }
        let mut menu: Option<Menu> = None;
        let green =
            if ui.visuals().dark_mode { Color32::from_rgb(110, 200, 120) } else { Color32::from_rgb(30, 130, 50) };
        let red = ui.visuals().error_fg_color;
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .auto_shrink([false, false])
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::initial(260.0).at_least(100.0).clip(true))
            .column(Column::initial(110.0).at_least(60.0))
            .column(Column::initial(90.0).at_least(60.0))
            .column(Column::remainder().at_least(100.0).clip(true))
            .header(24.0, |mut h| {
                for t in ["Service", "Status", "Startup", "Description"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, rows.len(), |mut row| {
                    let s = rows[row.index()];
                    row.set_selected(self.selected.as_deref() == Some(&s.unit));
                    row.col(|ui| {
                        ui.add(egui::Label::new(s.unit.trim_end_matches(".service")).selectable(false).truncate());
                    });
                    row.col(|ui| {
                        let color = match s.active.as_str() {
                            "active" => green,
                            "failed" => red,
                            _ => ui.visuals().weak_text_color(),
                        };
                        let text = if s.load == "masked" || s.load == "not-found" { &s.load } else { &s.sub };
                        ui.label(RichText::new(text).color(color)).on_hover_text(format!("{} / {}", s.active, s.load));
                    });
                    row.col(|ui| {
                        ui.label(&s.enabled);
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(RichText::new(&s.description).weak()).selectable(false).truncate());
                    });
                    let r = row.response();
                    if r.clicked() || r.secondary_clicked() {
                        clicked = Some(s.unit.clone());
                    }
                    r.context_menu(|ui| {
                        ui.label(RichText::new(s.unit.trim_end_matches(".service")).strong());
                        ui.label(RichText::new(format!("{} · {}", s.sub, s.enabled)).small().weak());
                        ui.separator();
                        for (i, (verb, label, help)) in SERVICE_ACTIONS.iter().enumerate() {
                            if i == 4 || i == 6 {
                                ui.separator();
                            }
                            if ui.button(*label).on_hover_text(*help).clicked() {
                                menu = Some(Menu::Control(s.unit.clone(), verb, label));
                                ui.close();
                            }
                        }
                        ui.separator();
                        if ui.button("Show processes").clicked() {
                            menu = Some(Menu::Processes(s.unit.clone()));
                            ui.close();
                        }
                        if ui.button("Show logs").clicked() {
                            menu = Some(Menu::Logs(s.unit.clone()));
                            ui.close();
                        }
                    });
                });
            });
        if clicked.is_some() {
            self.selected = clicked;
        }
        match menu {
            Some(Menu::Control(unit, verb, label)) => self.control(c, &unit, verb, label),
            Some(Menu::Logs(unit)) => self.show_logs(c, &unit),
            Some(Menu::Processes(unit)) => {
                let pids: std::collections::HashSet<u32> =
                    c.snap.procs.iter().filter(|p| p.unit.as_deref() == Some(unit.as_str())).map(|p| p.pid).collect();
                if pids.is_empty() {
                    c.info(format!("{unit} has no running processes"));
                } else {
                    procs.show_group(unit.trim_end_matches(".service").to_string(), pids);
                    *c.switch_to = Some(Tab::Processes);
                }
            }
            None => {}
        }
        self.logs_window(ui.ctx());
    }
}

// ---------------------------------------------------------------- Startup

#[derive(Default)]
pub struct StartupState {
    list: Vec<StartupEntry>,
    loaded: bool,
    loading: bool,
    show_other_desktops: bool,
}

impl StartupState {
    pub fn request_reload(&mut self) {
        self.loaded = false;
    }

    pub fn loaded(&mut self, list: Vec<StartupEntry>) {
        self.list = list;
        self.loaded = true;
        self.loading = false;
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, c: &mut Ctx) {
        if !self.loaded && !self.loading {
            self.loading = true;
            c.jobs.spawn(|| JobResult::Startup(startup::list()));
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new("Applications started when you log in").weak());
            ui.checkbox(&mut self.show_other_desktops, "Show entries for other desktops");
            if ui.button("⟳ Refresh").clicked() {
                self.loaded = false;
            }
        });
        let rows: Vec<&StartupEntry> = self.list.iter().filter(|e| self.show_other_desktops || !e.not_here).collect();
        let mut toggle: Option<(String, bool)> = None;
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .auto_shrink([false, false])
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::initial(90.0).at_least(80.0))
            .column(Column::initial(230.0).at_least(100.0).clip(true))
            .column(Column::initial(70.0).at_least(50.0))
            .column(Column::remainder().at_least(100.0).clip(true))
            .header(24.0, |mut h| {
                for t in ["Enabled", "Name", "Scope", "Command"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(22.0, rows.len(), |mut row| {
                    let e = rows[row.index()];
                    row.col(|ui| {
                        let mut on = e.enabled;
                        let label = if on { "On" } else { "Off" };
                        if ui.checkbox(&mut on, label).changed() {
                            toggle = Some((e.file.clone(), on));
                        }
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(RichText::new(&e.name).strong()).truncate()).on_hover_text(&e.comment);
                    });
                    row.col(|ui| {
                        ui.label(if e.user { "User" } else { "System" });
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(RichText::new(&e.exec).weak()).truncate());
                    });
                });
            });
        if let Some((file, on)) = toggle {
            match startup::set_enabled(&file, on) {
                Ok(()) => c.info(format!("{file} {}", if on { "enabled" } else { "disabled" })),
                Err(e) => c.error(format!("Couldn't update {file}: {e}")),
            }
            self.loaded = false;
        }
    }
}
