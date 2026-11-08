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
    Assistant,
    Processes,
    Performance,
    Users,
    Services,
    Startup,
}

impl Tab {
    const ALL: [Tab; 7] =
        [Tab::Apps, Tab::Processes, Tab::Performance, Tab::Users, Tab::Services, Tab::Startup, Tab::Assistant];
    fn label(self) -> &'static str {
        match self {
            Tab::Apps => "Apps",
            Tab::Assistant => "Assistant",
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
            Tab::Assistant => "assistant",
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

    /// Tabs in the user's order; tabs missing from the saved order keep their default place at the end.
    fn ordered(order: &str) -> Vec<Tab> {
        let pos: Vec<&str> = order.split(',').collect();
        let mut v = Tab::ALL.to_vec();
        v.sort_by_key(|t| pos.iter().position(|p| *p == t.key()).unwrap_or(usize::MAX));
        v
    }
}

/// Results of work done off the UI thread.
pub enum JobResult {
    Status(String, bool),
    Services(bool, Result<Vec<Service>, String>),
    ServiceLogs(String, Result<String, String>),
    /// "Find open file" results: matches and how many processes couldn't be inspected.
    FileHolders(Vec<crate::files::Holder>, usize),
    History(crate::ui_history::Range, Vec<serde_json::Value>),
    /// Background recorder enabled/disabled (or the error).
    Recorder(Result<bool, String>),
    Ai(crate::ui_assistant::AiMsg),
    Startup(Vec<StartupEntry>),
    /// Result of an update check; `bool` = started by the user (report "up to date"/errors).
    UpdateCheck(Result<Option<updater::Release>, String>, bool),
    UpdateInstalled(Result<std::path::PathBuf, String>, String),
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
    pub grouper: &'a Grouper,
}

impl Ctx<'_> {
    pub fn info(&mut self, msg: impl Into<String>) {
        *self.status = Some((msg.into(), false, Instant::now()));
    }
    /// How to launch an app group (its desktop entry's Exec line), if known.
    pub fn exec_for(&self, key: &str) -> Option<String> {
        self.grouper.exec_for(key)
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
    /// Send a result from any thread (for workers that report progress before finishing).
    pub fn send(&self, r: JobResult) {
        let _ = self.tx.send(r);
        self.ctx.request_repaint();
    }

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
    assistant: crate::ui_assistant::AssistantState,
    grouper: Grouper,
    perf: PerfState,
    users: UsersState,
    services: ServicesState,
    startup: StartupState,
    update: UpdateState,
    tab_drag: Option<Tab>,
    tray: Option<crate::tray::Tray>,
    tray_tried: bool,
    tray_tx: Sender<crate::tray::TrayCmd>,
    tray_rx: Receiver<crate::tray::TrayCmd>,
    tray_updated: Option<Instant>,
    /// Set when the user really wants to quit (tray "Quit"), so close isn't intercepted.
    quitting: bool,
    /// Recorder state last applied to the systemd user service (None = not yet this session).
    recorder_applied: Option<bool>,
    alerts_open: bool,
    /// Window geometry waiting to be saved once it has been stable for a moment.
    geom_pending: Option<(String, Instant)>,
}

pub fn run() -> eframe::Result {
    let cfg = Config::load();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(format!("Taskd {}", env!("CARGO_PKG_VERSION")))
        .with_app_id("taskd-monitor")
        .with_inner_size([1040.0, 700.0])
        .with_min_inner_size([620.0, 400.0])
        .with_icon(Arc::new(icon()));
    // Restore the last window geometry (position is ignored on Wayland, which is fine).
    let g: Vec<f32> = cfg.window.split(',').filter_map(|v| v.parse().ok()).collect();
    if let [w, h, x, y, max] = g[..] {
        if (400.0..10000.0).contains(&w) && (300.0..10000.0).contains(&h) {
            viewport = viewport.with_inner_size([w, h]);
        }
        if (-200.0..20000.0).contains(&x) && (-200.0..20000.0).contains(&y) {
            viewport = viewport.with_position([x, y]);
        }
        viewport = viewport.with_maximized(max > 0.5);
    }
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
        cc.egui_ctx.set_theme(theme_pref(&cfg.theme));
        let (tray_tx, tray_rx) = channel();
        App {
            update,
            tab_drag: None,
            tray: None,
            tray_tried: false,
            tray_tx,
            tray_rx,
            tray_updated: None,
            quitting: false,
            recorder_applied: None,
            alerts_open: false,
            geom_pending: None,
            shared,
            snap,
            tab: Tab::from_key(&cfg.tab),
            saved: cfg.clone(),
            procs: ProcState::default(),
            apps: AppsState::default(),
            assistant: Default::default(),
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
                JobResult::ServiceLogs(unit, res) => self.services.logs_loaded(unit, res),
                JobResult::FileHolders(found, denied) => self.procs.holders_loaded(found, denied),
                JobResult::History(range, recs) => self.perf.history.loaded(range, recs),
                JobResult::Ai(m) => self.assistant.on_msg(m),
                JobResult::Recorder(Ok(on)) => self.recorder_applied = Some(on),
                JobResult::Recorder(Err(e)) => {
                    self.status = Some((format!("Background recorder: {e}"), true, Instant::now()));
                    self.recorder_applied = Some(!self.cfg.record); // don't retry in a loop
                }
                JobResult::Startup(list) => self.startup.loaded(list),
                JobResult::UpdateCheck(res, manual) => self.update_checked(res, manual),
                JobResult::UpdateInstalled(Ok(exe), version) => {
                    // Settings are saved on every change, so it's safe to hand over right away.
                    let err = updater::restart(&exe, &version);
                    self.update.installing = false;
                    self.status = Some((err, true, Instant::now()));
                }
                JobResult::UpdateInstalled(Err(e), _) => {
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
                // Tarball installs need no password, so install right away when allowed
                // (unless we just restarted into this very version: see JUST_UPDATED_ENV).
                let looping = self.update.available.as_ref().is_some_and(|r| updater::just_updated_to(&r.version));
                if silent && self.cfg.auto_update && !looping {
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
        self.jobs.spawn(move || JobResult::UpdateInstalled(updater::install(&rel, &kind), rel.version.clone()));
    }

    /// Remember size/position/maximized, saved once the window has stopped moving for a second
    /// (window drags would otherwise rewrite the config every frame).
    fn track_geometry(&mut self, ctx: &egui::Context) {
        let (inner, outer, max, min) = ctx.input(|i| {
            let v = i.viewport();
            (v.inner_rect, v.outer_rect, v.maximized.unwrap_or(false), v.minimized.unwrap_or(false))
        });
        let (Some(inner), false) = (inner, min) else { return };
        let old: Vec<f32> = self.cfg.window.split(',').filter_map(|v| v.parse().ok()).collect();
        let geom = if max {
            // Keep the restored size/position so un-maximizing goes back to it.
            match old[..] {
                [w, h, x, y, _] => format!("{w},{h},{x},{y},1"),
                _ => format!("{},{},0,0,1", inner.width().round(), inner.height().round()),
            }
        } else {
            let pos = outer.map_or(inner.min, |r| r.min);
            format!("{},{},{},{},0", inner.width().round(), inner.height().round(), pos.x.round(), pos.y.round())
        };
        if geom == self.cfg.window {
            self.geom_pending = None;
            return;
        }
        match &self.geom_pending {
            Some((g, since)) if *g == geom && since.elapsed() >= Duration::from_secs(1) => {
                self.cfg.window = geom;
                self.geom_pending = None;
            }
            Some((g, _)) if *g == geom => ctx.request_repaint_after(Duration::from_millis(1100)),
            _ => {
                self.geom_pending = Some((geom, Instant::now()));
                ctx.request_repaint_after(Duration::from_millis(1100));
            }
        }
    }

    /// Keep the systemd user service in line with the "record" setting. Only installed copies
    /// manage the service: a development build must not register itself to run at login.
    fn apply_recorder(&mut self) {
        let want = self.cfg.record;
        if self.recorder_applied == Some(want) {
            return;
        }
        let Some(updater::InstallKind::Portable(_) | updater::InstallKind::Deb) = self.update.kind else {
            self.recorder_applied = Some(want);
            return;
        };
        self.recorder_applied = Some(want); // in flight; the job reports the outcome
        self.jobs.spawn(move || {
            let r = if want {
                std::env::current_exe().map_err(|e| e.to_string()).and_then(|exe| crate::recorder::enable(&exe))
            } else {
                crate::recorder::disable()
            };
            JobResult::Recorder(r.map(|_| want))
        });
    }

    fn alerts_window(&mut self, ctx: &egui::Context) {
        if !self.alerts_open {
            return;
        }
        let mut open = true;
        let c = &mut self.cfg;
        egui::Window::new("Alerts").open(&mut open).resizable(false).default_width(420.0).show(ctx, |ui| {
            ui.label(RichText::new("Desktop notifications from the background recorder (0 = off).").weak());
            if !c.record {
                ui.colored_label(ui.visuals().warn_fg_color, "Background recording is off, so alerts won't fire.");
            }
            ui.add_space(4.0);
            egui::Grid::new("alerts").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                ui.label("Available memory below");
                ui.add(egui::DragValue::new(&mut c.alert_mem_pct).range(0.0..=90.0).suffix(" %"));
                ui.end_row();
                ui.label("CPU above");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut c.alert_cpu_pct).range(0.0..=100.0).suffix(" %"));
                    ui.label("for");
                    ui.add(egui::DragValue::new(&mut c.alert_cpu_mins).range(1.0..=60.0).suffix(" min"));
                });
                ui.end_row();
                ui.label("A disk busier than (5 min)");
                ui.add(egui::DragValue::new(&mut c.alert_disk_pct).range(0.0..=100.0).suffix(" %"));
                ui.end_row();
                ui.label("CPU temperature above");
                ui.add(egui::DragValue::new(&mut c.alert_cpu_temp).range(0.0..=120.0).suffix(" °C"));
                ui.end_row();
                ui.label("GPU temperature above");
                ui.add(egui::DragValue::new(&mut c.alert_gpu_temp).range(0.0..=120.0).suffix(" °C"));
                ui.end_row();
                ui.label("Battery below (on battery)");
                ui.add(egui::DragValue::new(&mut c.alert_battery_pct).range(0.0..=90.0).suffix(" %"));
                ui.end_row();
            });
            ui.checkbox(&mut c.alert_swap, "When the system starts swapping heavily");
            ui.checkbox(&mut c.alert_failed_services, "When a systemd service fails");
            ui.separator();
            ui.label(RichText::new("Per-app memory limits").strong());
            let mut limits: Vec<(String, String)> = c
                .alert_app_mem
                .split(';')
                .filter_map(|e| e.split_once('='))
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect();
            let mut remove = None;
            for (i, (name, gb)) in limits.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(name)
                            .hint_text("App name, e.g. Firefox Web Browser")
                            .desired_width(220.0),
                    );
                    ui.add(egui::TextEdit::singleline(gb).desired_width(40.0));
                    ui.label("GB");
                    if ui.small_button("✖").clicked() {
                        remove = Some(i);
                    }
                });
            }
            if let Some(i) = remove {
                limits.remove(i);
            }
            if ui.button("+ Add limit").clicked() {
                limits.push((String::new(), "4".into()));
            }
            c.alert_app_mem = limits
                .iter()
                .map(|(n, v)| format!("{}={}", n.replace([';', '='], " "), v.trim()))
                .collect::<Vec<_>>()
                .join(";");
            ui.separator();
            if ui.button("Send a test notification").clicked() {
                std::thread::spawn(|| crate::recorder::notify("Taskd", "Alerts are working.", false));
            }
        });
        self.alerts_open = open;
    }

    /// Per-app network & service I/O: the optional privileged helper.
    fn helper_controls(&mut self, ui: &mut egui::Ui, kind: &updater::InstallKind) {
        ui.label(RichText::new("Per-app network & service disk I/O").strong());
        let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let deb = *kind == updater::InstallKind::Deb;
        let run = |jobs: &Jobs, args: Vec<String>, done: &'static str| {
            jobs.spawn(move || match actions::run_elevated(&args) {
                Ok(()) => JobResult::Status(done.into(), false),
                Err(e) => JobResult::Status(format!("Helper: {e}"), true),
            });
        };
        let systemctl = actions::sys_bin("systemctl");
        match self.snap.helper.clone() {
            Some(v) => {
                ui.label(RichText::new(format!("✓ Helper running (v{v})")).small());
                if !deb && v != env!("CARGO_PKG_VERSION") && ui.button("Update helper").clicked() {
                    run(&self.jobs, vec![exe.clone(), "--install-helper".into()], "Helper updated");
                    ui.close();
                }
                if ui.button("Turn off").clicked() {
                    let args = if deb {
                        vec![systemctl, "disable".into(), "--now".into(), "taskd-helper.service".into()]
                    } else {
                        vec![exe, "--uninstall-helper".into()]
                    };
                    run(&self.jobs, args, "Helper turned off");
                    ui.close();
                }
            }
            None => {
                ui.label(
                    RichText::new("Needs a small privileged background service; Linux only shows this to root.")
                        .small()
                        .weak(),
                );
                if ui.button("Turn on (asks for your password)").clicked() {
                    let args = if deb {
                        vec![systemctl, "enable".into(), "--now".into(), "taskd-helper.service".into()]
                    } else {
                        vec![exe, "--install-helper".into()]
                    };
                    run(&self.jobs, args, "Helper turned on: per-app network appears within a few seconds");
                    ui.close();
                }
            }
        }
    }

    fn tray_logic(&mut self, ctx: &egui::Context) {
        use crate::tray::{Tray, TrayCmd};
        if self.cfg.tray && self.tray.is_none() && !self.tray_tried {
            self.tray_tried = true; // once per session: no tray host = no retries
            self.tray = Tray::start(ctx.clone(), self.tray_tx.clone());
        } else if !self.cfg.tray && self.tray.is_some() {
            self.tray = None;
            self.tray_tried = false;
        }
        while let Ok(cmd) = self.tray_rx.try_recv() {
            match cmd {
                TrayCmd::Show => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                TrayCmd::TogglePause => self.toggle_pause(),
                TrayCmd::Quit => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
        // Refresh the tray graph at most every 2 s.
        if let Some(t) = &self.tray
            && self.tray_updated.is_none_or(|u| u.elapsed() >= Duration::from_secs(2))
        {
            let m = &self.snap.mem;
            let mem = if m.total > 0 { 100.0 * m.used as f32 / m.total as f32 } else { 0.0 };
            t.update(self.snap.cpu.usage, mem, self.shared.paused.load(Ordering::Relaxed));
            self.tray_updated = Some(Instant::now());
        }
    }

    fn settings_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("⚙", |ui| {
            let kind = self.update.kind.clone().unwrap_or(updater::InstallKind::Unmanaged);
            ui.label(RichText::new(format!("Taskd {}", updater::current_version())).strong());
            ui.label(RichText::new(updater::describe(&kind)).small().weak());
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Theme:");
                for (key, label) in [("system", "System"), ("light", "Light"), ("dark", "Dark")] {
                    if ui.selectable_label(self.cfg.theme == key, label).clicked() {
                        self.cfg.theme = key.into();
                        ui.ctx().set_theme(theme_pref(key));
                    }
                }
            });
            if ui.button("Reset tab order").clicked() {
                self.cfg.tab_order.clear();
            }
            ui.checkbox(&mut self.cfg.tray, "Show tray icon");
            ui.add_enabled_ui(self.cfg.tray && self.tray.is_some(), |ui| {
                ui.checkbox(&mut self.cfg.close_to_tray, "Keep running in the tray when closed");
            });
            if self.cfg.tray && self.tray.is_none() && self.tray_tried {
                ui.label(RichText::new("No tray available on this desktop.").small().weak());
            }
            ui.separator();
            ui.checkbox(&mut self.cfg.record, "Record history & alerts in the background")
                .on_hover_text("A small user service (taskdm --record) keeps ~7 days of history and raises alerts, even with this window closed.");
            if ui.button("Alerts…").clicked() {
                self.alerts_open = true;
                ui.close();
            }
            ui.separator();
            self.helper_controls(ui, &kind);
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
            ui.label(RichText::new(format!("v{}", env!("CARGO_PKG_VERSION"))).small().weak());
            ui.add_space(10.0);
            let tabs = Tab::ordered(&self.cfg.tab_order);
            let mut rects = Vec::with_capacity(tabs.len());
            for (i, t) in tabs.iter().copied().enumerate() {
                let r = ui.selectable_label(self.tab == t, t.label()).interact(egui::Sense::click_and_drag());
                rects.push(r.rect);
                if r.drag_started() {
                    self.tab_drag = Some(t);
                }
                if r.on_hover_text(format!("Ctrl+{} · drag to reorder", i + 1)).clicked() {
                    self.tab = t;
                }
            }
            // Drag-to-reorder tabs: show where it would land, apply on release.
            if let Some(dragged) = self.tab_drag
                && let Some(from) = tabs.iter().position(|t| *t == dragged)
            {
                let x = ui.input(|i| i.pointer.interact_pos()).map_or(0.0, |p| p.x);
                let target = rects.iter().filter(|r| r.center().x < x).count();
                let line_x = if target < rects.len() {
                    rects[target].left() - 2.0
                } else {
                    rects[rects.len() - 1].right() + 2.0
                };
                ui.painter().vline(
                    line_x,
                    rects[0].y_range(),
                    egui::Stroke::new(2.0, ui.visuals().selection.stroke.color),
                );
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                if ui.input(|i| i.pointer.any_released()) {
                    let mut order = tabs.clone();
                    let t = order.remove(from);
                    let to = if target > from { target - 1 } else { target };
                    order.insert(to.min(order.len()), t);
                    self.cfg.tab_order = order.iter().map(|t| t.key()).collect::<Vec<_>>().join(",");
                    self.tab_drag = None;
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
                let order = Tab::ordered(&self.cfg.tab_order);
                let keys = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7];
                for (n, k) in keys.into_iter().enumerate() {
                    if i.key_pressed(k) {
                        tab = order.get(n).copied();
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
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.snap = self.shared.latest();
        self.handle_jobs();
        self.tray_logic(ctx); // also runs while the window is hidden in the tray
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Close to tray: hide instead of quitting (unless quitting from the tray menu).
        if ctx.input(|i| i.viewport().close_requested())
            && self.cfg.close_to_tray
            && self.tray.is_some()
            && !self.quitting
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
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
                grouper: &self.grouper,
            };
            match self.tab {
                Tab::Apps => self.apps.ui(ui, &mut c),
                Tab::Assistant => self.assistant.ui(ui, &mut c, &self.shared),
                Tab::Processes => self.procs.ui(ui, &mut c),
                Tab::Performance => self.perf.ui(ui, &mut c, &mut self.procs),
                Tab::Users => self.users.ui(ui, &mut c, &mut self.procs),
                Tab::Services => self.services.ui(ui, &mut c, &mut self.procs),
                Tab::Startup => self.startup.ui(ui, &mut c),
            }
        });
        if let Some(t) = switch_to {
            self.tab = t;
        }
        self.procs.detail_windows(&ctx, &self.snap);
        self.elevate_dialog(&ctx);

        self.cfg.tab = self.tab.key().into();
        self.track_geometry(&ctx);
        self.apply_recorder();
        self.alerts_window(&ctx);
        if self.cfg != self.saved {
            self.cfg.save();
            self.saved = self.cfg.clone();
        }
    }
}

fn theme_pref(key: &str) -> egui::ThemePreference {
    match key {
        "light" => egui::ThemePreference::Light,
        "dark" => egui::ThemePreference::Dark,
        _ => egui::ThemePreference::System,
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
