//! Application shell: window, tab bar, status bar, background jobs.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Key, Layout, RichText};

use crate::actions;
use crate::config::Config;
use crate::groups::{Group, Grouper};
use crate::sampler::{self, Shared, Snapshot};
use crate::services::Service;
use crate::startup::StartupEntry;
use crate::ui_apps::AppsState;
use crate::ui_performance::PerfState;
use crate::ui_processes::ProcState;
use crate::ui_tabs::{ServicesState, StartupState, UsersState};
use crate::updater;
use crate::util::{fmt_bytes, fmt_duration};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Apps,
    Processes,
    Performance,
    Users,
    Services,
    Startup,
}

impl Tab {
    const ALL: [Tab; 6] = [Tab::Apps, Tab::Processes, Tab::Performance, Tab::Users, Tab::Services, Tab::Startup];
    fn label(self) -> &'static str {
        match self {
            Tab::Apps => "Apps",
            Tab::Processes => "Processes",
            Tab::Performance => "Performance",
            Tab::Users => "Users",
            Tab::Services => "Services",
            Tab::Startup => "Startup",
        }
    }
    fn key(self) -> &'static str {
        match self {
            Tab::Apps => "apps",
            Tab::Processes => "processes",
            Tab::Performance => "performance",
            Tab::Users => "users",
            Tab::Services => "services",
            Tab::Startup => "startup",
        }
    }
    fn from_key(k: &str) -> Tab {
        Tab::ALL.into_iter().find(|t| t.key() == k).unwrap_or(Tab::Processes)
    }
}

/// Results of work done off the UI thread.
pub enum JobResult {
    Status(String, bool),
    Services(bool, Result<Vec<Service>, String>),
    Startup(Vec<StartupEntry>),
    /// Result of an update check; `bool` = started by the user (report "up to date"/errors).
    UpdateCheck(Result<Option<updater::Release>, String>, bool),
    UpdateInstalled(Result<std::path::PathBuf, String>),
}

#[derive(Default)]
struct UpdateState {
    kind: Option<updater::InstallKind>,
    available: Option<updater::Release>,
    installing: bool,
    dismissed: bool,
    last_error: Option<String>,
}

pub struct Ctx<'a> {
    pub snap: &'a Arc<Snapshot>,
    pub cfg: &'a mut Config,
    pub jobs: &'a Jobs,
    pub status: &'a mut Option<(String, bool, Instant)>,
    pub elevate: &'a mut Option<(String, Vec<String>)>,
    pub my_uid: u32,
    pub switch_to: &'a mut Option<Tab>,
    /// Apps/services grouping; only computed while a tab that uses it is visible.
    pub groups: &'a [Group],
}

impl Ctx<'_> {
    pub fn info(&mut self, msg: impl Into<String>) {
        *self.status = Some((msg.into(), false, Instant::now()));
    }
    pub fn error(&mut self, msg: impl Into<String>) {
        *self.status = Some((msg.into(), true, Instant::now()));
    }
}

#[derive(Clone)]
pub struct Jobs {
    tx: Sender<JobResult>,
    ctx: egui::Context,
}

impl Jobs {
    pub fn spawn(&self, f: impl FnOnce() -> JobResult + Send + 'static) {
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(f());
            ctx.request_repaint();
        });
    }
}

pub struct App {
    shared: Arc<Shared>,
    snap: Arc<Snapshot>,
    cfg: Config,
    saved: Config,
    tab: Tab,
    jobs: Jobs,
    rx: Receiver<JobResult>,
    status: Option<(String, bool, Instant)>,
    elevate: Option<(String, Vec<String>)>,
    my_uid: u32,
    procs: ProcState,
    apps: AppsState,
    grouper: Grouper,
    perf: PerfState,
    users: UsersState,
    services: ServicesState,
    startup: StartupState,
    update: UpdateState,
}

pub fn run() -> eframe::Result {
    let cfg = Config::load();
    let viewport = egui::ViewportBuilder::default()
        .with_title("Taskd")
        .with_app_id("taskd-monitor")
        .with_inner_size([1040.0, 700.0])
        .with_min_inner_size([620.0, 400.0])
        .with_icon(Arc::new(icon()));
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "Taskd",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            // A zero-delay request_repaint makes egui paint two frames; any non-zero delay paints
            // exactly one. One frame per sample is all we need.
            let shared = sampler::spawn(cfg.interval_ms, move || ctx.request_repaint_after(Duration::from_millis(1)));
            Ok(Box::new(App::new(cc, shared, cfg)))
        }),
    )
}

impl App {
    fn new(cc: &eframe::CreationContext, shared: Arc<Shared>, cfg: Config) -> Self {
        cc.egui_ctx.options_mut(|o| o.input_options.line_scroll_speed *= 1.5);
        cc.egui_ctx.global_style_mut(|s| {
            s.spacing.item_spacing.y = 4.0;
            s.interaction.tooltip_delay = 0.4;
        });
        let (tx, rx) = channel();
        let snap = shared.latest();
        let jobs = Jobs { tx, ctx: cc.egui_ctx.clone() };
        let update = UpdateState { kind: Some(updater::install_kind()), ..Default::default() };
        if cfg.auto_update {
            jobs.spawn(|| JobResult::UpdateCheck(updater::check(), false));
        }
        App {
            update,
            shared,
            snap,
            tab: Tab::from_key(&cfg.tab),
            saved: cfg.clone(),
            procs: ProcState::default(),
            apps: AppsState::default(),
            grouper: Grouper::default(),
            perf: PerfState::default(),
            users: UsersState::default(),
            services: ServicesState::default(),
            startup: StartupState::default(),
            cfg,
            jobs,
            rx,
            status: None,
            elevate: None,
            // SAFETY: getuid never fails.
            my_uid: unsafe { libc::getuid() },
        }
    }

    fn handle_jobs(&mut self) {
        while let Ok(r) = self.rx.try_recv() {
            match r {
                JobResult::Status(m, err) => self.status = Some((m, err, Instant::now())),
                JobResult::Services(user, res) => self.services.loaded(user, res),
                JobResult::Startup(list) => self.startup.loaded(list),
                JobResult::UpdateCheck(res, manual) => self.update_checked(res, manual),
                JobResult::UpdateInstalled(Ok(exe)) => {
                    // Settings are saved on every change, so it's safe to hand over right away.
                    updater::restart(&exe);
                }
                JobResult::UpdateInstalled(Err(e)) => {
                    self.update.installing = false;
                    self.status = Some((format!("Update failed: {e}"), true, Instant::now()));
                    self.update.last_error = Some(e);
                }
            }
        }
    }

    fn update_checked(&mut self, res: Result<Option<updater::Release>, String>, manual: bool) {
        match res {
            Ok(Some(rel)) => {
                self.update.last_error = None;
                self.update.dismissed = false;
                let silent = matches!(self.update.kind, Some(updater::InstallKind::Portable(_)));
                self.update.available = Some(rel);
                // Tarball installs need no password, so install right away when allowed.
                if silent && self.cfg.auto_update {
                    self.start_install();
                }
            }
            Ok(None) => {
                self.update.last_error = None;
                if manual {
                    self.status = Some(("Taskd is up to date".into(), false, Instant::now()));
                }
            }
            Err(e) => {
                if manual {
                    self.status = Some((format!("Couldn't check for updates: {e}"), true, Instant::now()));
                }
                self.update.last_error = Some(e);
            }
        }
    }

    fn start_install(&mut self) {
        let (Some(rel), Some(kind)) = (self.update.available.clone(), self.update.kind.clone()) else { return };
        if self.update.installing {
            return;
        }
        self.update.installing = true;
        self.jobs.spawn(move || JobResult::UpdateInstalled(updater::install(&rel, &kind)));
    }

    fn settings_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("⚙", |ui| {
            let kind = self.update.kind.clone().unwrap_or(updater::InstallKind::Unmanaged);
            ui.label(RichText::new(format!("Taskd {}", updater::current_version())).strong());
            ui.label(RichText::new(updater::describe(&kind)).small().weak());
            ui.separator();
            ui.checkbox(&mut self.cfg.auto_update, "Check for updates at launch").on_hover_text(match kind {
                updater::InstallKind::Portable(_) => "Updates are downloaded, verified and installed automatically.",
                updater::InstallKind::Deb => "You'll be asked for your password to install an update.",
                updater::InstallKind::Unmanaged => "This build can't update itself; you'll only be notified.",
            });
            if ui.add_enabled(!self.update.installing, egui::Button::new("Check for updates now")).clicked() {
                self.jobs.spawn(|| JobResult::UpdateCheck(updater::check(), true));
                ui.close();
            }
            if let Some(e) = &self.update.last_error {
                ui.label(RichText::new(format!("Last update error: {e}")).small().color(ui.visuals().error_fg_color));
            }
        });
    }

    fn update_banner(&mut self, ui: &mut egui::Ui) {
        let Some(rel) = self.update.available.clone() else { return };
        if self.update.dismissed {
            return;
        }
        let kind = self.update.kind.clone().unwrap_or(updater::InstallKind::Unmanaged);
        egui::Frame::new()
            .fill(ui.visuals().selection.bg_fill.gamma_multiply(0.35))
            .inner_margin(egui::Margin::symmetric(8, 5))
            .corner_radius(4.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if self.update.installing {
                        ui.spinner();
                        ui.label(format!("Updating to Taskd {}…", rel.version));
                        return;
                    }
                    ui.label(RichText::new(format!("Taskd {} is available", rel.version)).strong());
                    if kind != updater::InstallKind::Unmanaged {
                        let hint = if kind == updater::InstallKind::Deb { "Asks for your password" } else { "" };
                        if ui.button("Update & restart").on_hover_text(hint).clicked() {
                            self.start_install();
                        }
                    }
                    if !rel.url.is_empty() && ui.button("What's new").clicked() {
                        let _ = std::process::Command::new("xdg-open").arg(&rel.url).spawn();
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("✖").on_hover_text("Dismiss").clicked() {
                            self.update.dismissed = true;
                        }
                    });
                });
            });
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            ui.label(RichText::new("Taskd").strong().size(16.0));
            ui.add_space(10.0);
            for (i, t) in Tab::ALL.into_iter().enumerate() {
                let r = ui.selectable_label(self.tab == t, t.label());
                if r.on_hover_text(format!("Ctrl+{}", i + 1)).clicked() {
                    self.tab = t;
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                self.settings_menu(ui);
                let paused = self.shared.paused.load(Ordering::Relaxed);
                let label = if paused { "▶ Resume" } else { "⏸ Pause" };
                if ui.button(label).on_hover_text("Pause/resume updates (Ctrl+P)").clicked() {
                    self.toggle_pause();
                }
                let mut ms = self.cfg.interval_ms;
                egui::ComboBox::from_id_salt("interval").width(70.0).selected_text(interval_label(ms)).show_ui(
                    ui,
                    |ui| {
                        for v in [250, 500, 1000, 2000, 5000, 10000] {
                            ui.selectable_value(&mut ms, v, interval_label(v));
                        }
                    },
                );
                if ms != self.cfg.interval_ms {
                    self.cfg.interval_ms = ms;
                    self.shared.interval_ms.store(ms, Ordering::Relaxed);
                    self.shared.poke();
                }
                ui.label("Update:");
            });
        });
    }

    fn toggle_pause(&mut self) {
        let p = !self.shared.paused.load(Ordering::Relaxed);
        self.shared.paused.store(p, Ordering::Relaxed);
        if !p {
            self.shared.poke();
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let s = &self.snap;
        ui.horizontal(|ui| {
            ui.label(format!("Processes: {}", s.procs.len()));
            ui.separator();
            ui.label(format!("Threads: {}", s.threads));
            ui.separator();
            ui.label(format!("CPU: {:.0}%", s.cpu.usage));
            ui.separator();
            let pct = if s.mem.total > 0 { 100.0 * s.mem.used as f64 / s.mem.total as f64 } else { 0.0 };
            ui.label(format!("Memory: {pct:.0}% ({})", fmt_bytes(s.mem.used)));
            ui.separator();
            ui.label(format!("Up: {}", fmt_duration(s.uptime as u64)));
            if let Some((msg, err, at)) = &self.status {
                if at.elapsed() < Duration::from_secs(8) {
                    ui.separator();
                    let c = if *err { ui.visuals().error_fg_color } else { ui.visuals().strong_text_color() };
                    ui.label(RichText::new(msg).color(c));
                    ui.ctx().request_repaint_after(Duration::from_secs(8));
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(format!("sample {:.2} ms", s.cost.as_secs_f64() * 1000.0)).weak())
                    .on_hover_text("Time Taskd spent collecting the last sample");
            });
        });
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let (tab, pause, refresh) = ctx.input(|i| {
            let mut tab = None;
            if i.modifiers.command {
                for (n, k) in [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6].into_iter().enumerate()
                {
                    if i.key_pressed(k) {
                        tab = Some(Tab::ALL[n]);
                    }
                }
            }
            (tab, i.modifiers.command && i.key_pressed(Key::P), i.key_pressed(Key::F5))
        });
        if let Some(t) = tab {
            self.tab = t;
        }
        if pause {
            self.toggle_pause();
        }
        if refresh {
            self.shared.poke();
            match self.tab {
                Tab::Services => self.services.request_reload(),
                Tab::Startup => self.startup.request_reload(),
                _ => {}
            }
        }
    }

    fn elevate_dialog(&mut self, ctx: &egui::Context) {
        let Some((desc, args)) = self.elevate.clone() else { return };
        let mut close = false;
        egui::Window::new("Administrator rights required")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(format!("You don't have permission to {desc}."));
                ui.label("Retry as administrator? (you'll be asked to authenticate)");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Retry as administrator").clicked() {
                        let d = desc.clone();
                        self.jobs.spawn(move || match actions::run_elevated(&args) {
                            Ok(()) => JobResult::Status(format!("Done: {d}"), false),
                            Err(e) => JobResult::Status(format!("Couldn't {d}: {e}"), true),
                        });
                        close = true;
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        close = true;
                    }
                });
            });
        if close {
            self.elevate = None;
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.snap = self.shared.latest();
        self.handle_jobs();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.shortcuts(&ctx);

        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(3.0);
            self.top_bar(ui);
            ui.add_space(1.0);
            self.update_banner(ui);
        });
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));

        let mut switch_to = None;
        if matches!(self.tab, Tab::Apps | Tab::Performance) {
            self.grouper.update(&self.snap, self.my_uid);
        }
        egui::CentralPanel::default_margins().show(ui, |ui| {
            let snap = self.snap.clone();
            let mut c = Ctx {
                snap: &snap,
                cfg: &mut self.cfg,
                jobs: &self.jobs,
                status: &mut self.status,
                elevate: &mut self.elevate,
                my_uid: self.my_uid,
                switch_to: &mut switch_to,
                groups: &self.grouper.groups,
            };
            match self.tab {
                Tab::Apps => self.apps.ui(ui, &mut c),
                Tab::Processes => self.procs.ui(ui, &mut c),
                Tab::Performance => self.perf.ui(ui, &mut c),
                Tab::Users => self.users.ui(ui, &mut c, &mut self.procs),
                Tab::Services => self.services.ui(ui, &mut c),
                Tab::Startup => self.startup.ui(ui, &mut c),
            }
        });
        if let Some(t) = switch_to {
            self.tab = t;
        }
        self.procs.detail_windows(&ctx, &self.snap);
        self.elevate_dialog(&ctx);

        self.cfg.tab = self.tab.key().into();
        if self.cfg != self.saved {
            self.cfg.save();
            self.saved = self.cfg.clone();
        }
    }
}

fn interval_label(ms: u64) -> String {
    if ms < 1000 { format!("{ms} ms") } else { format!("{} s", ms / 1000) }
}

/// A small procedurally drawn icon (bar chart on a rounded tile), so no asset files are needed.
fn icon() -> egui::IconData {
    const N: usize = 64;
    let mut rgba = vec![0u8; N * N * 4];
    let bars = [(10, 30), (22, 18), (34, 40), (46, 24)];
    for y in 0..N {
        for x in 0..N {
            let i = (y * N + x) * 4;
            let (dx, dy) = ((x as i32 - 31).abs(), (y as i32 - 31).abs());
            let corner = dx > 22 && dy > 22 && ((dx - 22).pow(2) + (dy - 22).pow(2)) > 81;
            if dx > 31 || dy > 31 || corner {
                continue;
            }
            let mut c = [0x1e, 0x29, 0x3b, 0xff];
            for (bx, h) in bars {
                if x >= bx && x < bx + 8 && y >= 54 - h && y < 54 {
                    c = [0x3b, 0x8e, 0xea, 0xff];
                }
            }
            rgba[i..i + 4].copy_from_slice(&c);
        }
    }
    egui::IconData { rgba, width: N as u32, height: N as u32 }
}
