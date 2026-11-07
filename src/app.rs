//! Application shell: window, tab bar, status bar, background jobs.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Key, Layout, RichText};

use crate::actions;
use crate::config::Config;
use crate::sampler::{self, Shared, Snapshot};
use crate::services::Service;
use crate::startup::StartupEntry;
use crate::ui_performance::PerfState;
use crate::ui_processes::ProcState;
use crate::ui_tabs::{ServicesState, StartupState, UsersState};
use crate::util::{fmt_bytes, fmt_duration};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Processes,
    Performance,
    Users,
    Services,
    Startup,
}

impl Tab {
    const ALL: [Tab; 5] = [Tab::Processes, Tab::Performance, Tab::Users, Tab::Services, Tab::Startup];
    fn label(self) -> &'static str {
        match self {
            Tab::Processes => "Processes",
            Tab::Performance => "Performance",
            Tab::Users => "Users",
            Tab::Services => "Services",
            Tab::Startup => "Startup",
        }
    }
    fn key(self) -> &'static str {
        match self {
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
}

pub struct Ctx<'a> {
    pub snap: &'a Arc<Snapshot>,
    pub cfg: &'a mut Config,
    pub jobs: &'a Jobs,
    pub status: &'a mut Option<(String, bool, Instant)>,
    pub elevate: &'a mut Option<(String, Vec<String>)>,
    pub my_uid: u32,
    pub switch_to: &'a mut Option<Tab>,
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
    perf: PerfState,
    users: UsersState,
    services: ServicesState,
    startup: StartupState,
}

pub fn run() -> eframe::Result {
    let cfg = Config::load();
    let viewport = egui::ViewportBuilder::default()
        .with_title("Taskd")
        .with_app_id("taskd")
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
        App {
            shared,
            snap,
            tab: Tab::from_key(&cfg.tab),
            saved: cfg.clone(),
            procs: ProcState::default(),
            perf: PerfState::default(),
            users: UsersState::default(),
            services: ServicesState::default(),
            startup: StartupState::default(),
            cfg,
            jobs: Jobs { tx, ctx: cc.egui_ctx.clone() },
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
            }
        }
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
                for (n, k) in [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5].into_iter().enumerate() {
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
        });
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));

        let mut switch_to = None;
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
            };
            match self.tab {
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
