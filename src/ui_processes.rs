//! Processes tab: virtualized, sortable table with tree view, filtering and process actions.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use eframe::egui::{self, Align, Color32, Key, Layout, RichText, Sense};
use egui_extras::{Column, TableBuilder};

use crate::actions::{self, ActionError, Details, Signal};
use crate::app::{Ctx, JobResult};
use crate::sampler::{ProcInfo, Snapshot};
use crate::util::{fmt_bytes, fmt_duration, fmt_rate};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Col {
    Name,
    Pid,
    User,
    Cpu,
    Spark,
    Mem,
    Gpu,
    Read,
    Write,
    Threads,
    Nice,
    State,
    Started,
    CpuTime,
    Command,
}

impl Col {
    pub const ALL: [Col; 15] = [
        Col::Name,
        Col::Pid,
        Col::User,
        Col::Cpu,
        Col::Spark,
        Col::Mem,
        Col::Gpu,
        Col::Read,
        Col::Write,
        Col::Threads,
        Col::Nice,
        Col::State,
        Col::Started,
        Col::CpuTime,
        Col::Command,
    ];
    fn key(self) -> &'static str {
        match self {
            Col::Name => "name",
            Col::Pid => "pid",
            Col::User => "user",
            Col::Cpu => "cpu",
            Col::Spark => "spark",
            Col::CpuTime => "cputime",
            Col::Mem => "mem",
            Col::Gpu => "gpu",
            Col::Read => "read",
            Col::Write => "write",
            Col::Threads => "threads",
            Col::Nice => "nice",
            Col::State => "state",
            Col::Started => "started",
            Col::Command => "command",
        }
    }
    fn title(self) -> &'static str {
        match self {
            Col::Name => "Name",
            Col::Pid => "PID",
            Col::User => "User",
            Col::Cpu => "CPU",
            Col::Spark => "CPU history",
            Col::CpuTime => "CPU time",
            Col::Mem => "Memory",
            Col::Gpu => "GPU",
            Col::Read => "Disk read",
            Col::Write => "Disk write",
            Col::Threads => "Threads",
            Col::Nice => "Nice",
            Col::State => "State",
            Col::Started => "Running for",
            Col::Command => "Command line",
        }
    }
    fn from_key(k: &str) -> Option<Col> {
        Col::ALL.into_iter().find(|c| c.key() == k)
    }
    fn numeric(self) -> bool {
        !matches!(self, Col::Name | Col::User | Col::State | Col::Command)
    }
    fn column(self) -> Column {
        match self {
            Col::Name => Column::initial(230.0).at_least(80.0).clip(true),
            Col::Command => Column::remainder().at_least(100.0).clip(true),
            Col::User => Column::initial(90.0).at_least(40.0).clip(true),
            Col::Pid | Col::Threads | Col::Nice => Column::initial(64.0).at_least(40.0),
            Col::Spark => Column::initial(90.0).at_least(40.0),
            Col::State => Column::initial(80.0).at_least(40.0).clip(true),
            _ => Column::initial(86.0).at_least(50.0),
        }
    }
    fn compare(self, a: &ProcInfo, b: &ProcInfo) -> Ordering {
        match self {
            Col::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            Col::Pid => a.pid.cmp(&b.pid),
            Col::User => a.user.cmp(&b.user),
            Col::Cpu => a.cpu.total_cmp(&b.cpu),
            // Sort the sparkline by recent average load.
            Col::Spark => spark_avg(a).total_cmp(&spark_avg(b)),
            Col::CpuTime => a.cpu_time.total_cmp(&b.cpu_time),
            Col::Mem => a.mem.cmp(&b.mem),
            Col::Gpu => a.gpu.total_cmp(&b.gpu),
            Col::Read => a.read_rate.total_cmp(&b.read_rate),
            Col::Write => a.write_rate.total_cmp(&b.write_rate),
            Col::Threads => a.threads.cmp(&b.threads),
            Col::Nice => a.nice.cmp(&b.nice),
            Col::State => a.state.cmp(&b.state),
            // Earlier start = longer running.
            Col::Started => b.start.total_cmp(&a.start),
            Col::Command => a.cmd.cmp(&b.cmd),
        }
        .then(a.pid.cmp(&b.pid))
    }
}

/// Open files and connections, as collapsible sections of the details window.
fn files_section(ui: &mut egui::Ui, d: &Details) {
    let f = match &d.files {
        Ok(f) => f,
        Err(e) => {
            ui.label(RichText::new(format!("Open files: {e}")).weak());
            return;
        }
    };
    let more = if f.truncated { "+" } else { "" };
    egui::CollapsingHeader::new(format!("Network connections ({})", f.conns.len())).id_salt(("conns", d.pid)).show(
        ui,
        |ui| {
            egui::Grid::new(("cg", d.pid)).num_columns(4).striped(true).spacing([12.0, 3.0]).show(ui, |ui| {
                for c in &f.conns {
                    ui.label(c.proto);
                    ui.label(&c.local);
                    ui.label(if c.remote.is_empty() { "—" } else { &c.remote });
                    ui.label(RichText::new(c.state).weak());
                    ui.end_row();
                }
            });
        },
    );
    egui::CollapsingHeader::new(format!("Open files ({}{more})", f.files.len())).id_salt(("files", d.pid)).show(
        ui,
        |ui| {
            egui::Grid::new(("fg", d.pid)).num_columns(3).striped(true).spacing([12.0, 3.0]).show(ui, |ui| {
                for o in &f.files {
                    ui.label(RichText::new(o.fd.to_string()).weak());
                    ui.label(o.kind);
                    ui.add(egui::Label::new(&o.target).wrap());
                    ui.end_row();
                }
            });
        },
    );
}

fn spark_avg(p: &ProcInfo) -> f32 {
    let n = p.spark_len as usize;
    if n == 0 { 0.0 } else { p.spark[crate::sampler::SPARK - n..].iter().map(|&v| v as f32).sum::<f32>() / n as f32 }
}

/// Tiny CPU-history graph (% of one core, 0..100) drawn straight into the table cell.
fn sparkline(ui: &mut egui::Ui, p: &ProcInfo) {
    let rect = ui.max_rect().shrink2(egui::vec2(3.0, 3.0));
    let n = p.spark_len as usize;
    if n < 2 || !ui.is_rect_visible(rect) {
        return;
    }
    let vals = &p.spark[crate::sampler::SPARK - n..];
    let step = rect.width() / (crate::sampler::SPARK - 1) as f32;
    let x0 = rect.right() - step * (n - 1) as f32;
    let pts: Vec<egui::Pos2> = vals
        .iter()
        .enumerate()
        .map(|(i, &v)| egui::pos2(x0 + step * i as f32, rect.bottom() - rect.height() * v as f32 / 100.0))
        .collect();
    let color = crate::graph::CPU;
    ui.painter().add(egui::Shape::line(pts, egui::Stroke::new(1.2, color)));
}

/// CPU time as h:mm:ss (or m:ss.s for short-lived work).
fn fmt_cpu_time(secs: f64) -> String {
    let s = secs as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:04.1}", s / 60, secs % 60.0)
    }
}

fn state_name(s: u8) -> &'static str {
    match s {
        b'R' => "Running",
        b'S' => "Sleeping",
        b'D' => "Disk wait",
        b'Z' => "Zombie",
        b'T' => "Stopped",
        b't' => "Traced",
        b'I' => "Idle",
        b'X' => "Dead",
        _ => "?",
    }
}

enum Action {
    Select(u32),
    Signal(u32, Signal),
    Nice(u32, i32),
    Details(u32),
    Copy(String),
    Toggle(u32),
    FilterUser(String),
}

#[derive(Default, PartialEq, Clone)]
struct OrderKey {
    generation: u64,
    sort: String,
    desc: bool,
    filter: String,
    tree: bool,
    kernel: bool,
    mine: bool,
    collapsed: usize,
    scope: u64,
}

/// The "Find open file" window.
struct FindState {
    query: String,
    /// Matches and how many processes couldn't be inspected; `None` until a search finishes.
    results: Option<(Vec<crate::files::Holder>, usize)>,
    searching: bool,
}

/// A filter applied from another tab ("Show in Processes", double-click a user).
/// It overrides "Only mine" / "Kernel threads" so the picked processes are always visible.
enum Scope {
    /// An app or service: the processes it had when picked.
    Group(String, HashSet<u32>),
    /// Every process of a user, including ones started later.
    User(String),
}

#[derive(Default)]
pub struct ProcState {
    pub filter: String,
    selected: Option<u32>,
    /// Start time of the selected process, so a reused pid never inherits the selection.
    selected_start: Option<f64>,
    /// (index into snapshot.procs, tree depth, has children)
    order: Vec<(u32, u16, bool)>,
    key: OrderKey,
    collapsed: HashSet<u32>,
    details: Vec<(Details, u64, bool)>,
    find: Option<FindState>,
    focus_search: bool,
    scroll_to_selected: bool,
    pending_sort: Option<Col>,
    clipboard: Option<String>,
    /// "Show in Processes" from another tab: only these pids, labelled with the app/service name.
    scope: Option<Scope>,
    scope_gen: u64,
}

impl ProcState {
    fn hidden_cols(cfg: &crate::config::Config) -> HashSet<Col> {
        cfg.hidden_cols.split(',').filter_map(Col::from_key).collect()
    }

    fn rebuild_order(&mut self, snap: &Snapshot, c: &Ctx) {
        let key = OrderKey {
            generation: snap.generation,
            sort: c.cfg.sort_col.clone(),
            desc: c.cfg.sort_desc,
            filter: self.filter.clone(),
            tree: c.cfg.tree,
            kernel: c.cfg.show_kernel,
            mine: c.cfg.only_mine,
            collapsed: self.collapsed.len(),
            scope: self.scope_gen,
        };
        if key == self.key {
            return;
        }
        let sort = Col::from_key(&key.sort).unwrap_or(Col::Cpu);
        let desc = key.desc;
        let cmp = |a: &ProcInfo, b: &ProcInfo| {
            let o = sort.compare(a, b);
            if desc { o.reverse() } else { o }
        };
        let needle = key.filter.trim().to_lowercase();
        let procs = &snap.procs;
        let visible: Vec<u32> = (0..procs.len() as u32)
            .filter(|&i| {
                let p = &procs[i as usize];
                let shown = match &self.scope {
                    Some(Scope::Group(_, pids)) => pids.contains(&p.pid),
                    Some(Scope::User(u)) => *p.user == **u && (key.kernel || !p.kernel),
                    None => (key.kernel || !p.kernel) && (!key.mine || p.uid == c.my_uid),
                };
                shown
                    && (needle.is_empty()
                        || p.name.to_lowercase().contains(&needle)
                        || p.cmd.to_lowercase().contains(&needle)
                        || p.user.to_lowercase() == needle
                        || p.pid.to_string() == needle)
            })
            .collect();

        // Forget collapsed state of processes that are gone (their pid may be reused).
        if self.collapsed.len() > 64 || snap.generation % 30 == 0 {
            let live: HashSet<u32> = procs.iter().map(|p| p.pid).collect();
            self.collapsed.retain(|pid| live.contains(pid));
        }
        self.order.clear();
        if key.tree && needle.is_empty() {
            let set: HashMap<u32, u32> = visible.iter().map(|&i| (procs[i as usize].pid, i)).collect();
            let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
            let mut roots = Vec::new();
            for &i in &visible {
                let p = &procs[i as usize];
                if p.ppid != p.pid && set.contains_key(&p.ppid) {
                    children.entry(p.ppid).or_default().push(i);
                } else {
                    roots.push(i);
                }
            }
            let by = |v: &mut Vec<u32>| v.sort_by(|&a, &b| cmp(&procs[a as usize], &procs[b as usize]));
            by(&mut roots);
            for v in children.values_mut() {
                by(v);
            }
            let mut stack: Vec<(u32, u16)> = roots.into_iter().rev().map(|i| (i, 0)).collect();
            while let Some((i, d)) = stack.pop() {
                let pid = procs[i as usize].pid;
                let kids = children.get(&pid);
                self.order.push((i, d, kids.is_some()));
                if let Some(kids) = kids
                    && !self.collapsed.contains(&pid)
                {
                    stack.extend(kids.iter().rev().map(|&k| (k, d + 1)));
                }
            }
        } else {
            let mut v = visible;
            v.sort_by(|&a, &b| cmp(&procs[a as usize], &procs[b as usize]));
            self.order.extend(v.into_iter().map(|i| (i, 0, false)));
        }
        self.key = key;
    }

    /// Show every process of `user` (live: new processes appear too).
    pub fn show_user(&mut self, user: &str) {
        self.filter.clear();
        self.scope = Some(Scope::User(user.to_string()));
        self.scope_gen += 1;
    }

    /// Show only `pids` (an app or service picked on another tab) and select its first process.
    pub fn show_group(&mut self, label: String, pids: HashSet<u32>) {
        self.selected = pids.iter().min().copied();
        self.selected_start = None;
        self.scroll_to_selected = true;
        self.filter.clear();
        self.scope = Some(Scope::Group(label, pids));
        self.scope_gen += 1;
    }

    fn clear_scope(&mut self) {
        if self.scope.take().is_some() {
            self.scope_gen += 1;
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, c: &mut Ctx) {
        let snap = c.snap.clone();
        self.keyboard(ui, &snap, c);
        self.toolbar(ui, &snap, c);
        ui.add_space(2.0);
        self.rebuild_order(&snap, c);
        let act = self.table(ui, &snap, c);
        self.find_window(ui, &snap, c);
        if let Some(col) = self.pending_sort.take() {
            if c.cfg.sort_col == col.key() {
                c.cfg.sort_desc = !c.cfg.sort_desc;
            } else {
                c.cfg.sort_col = col.key().into();
                // Numbers default to descending (biggest first), text ascending.
                c.cfg.sort_desc = col.numeric() && col != Col::Pid;
            }
        }
        if let Some(a) = act {
            self.apply(a, &snap, c);
        }
    }

    pub fn holders_loaded(&mut self, found: Vec<crate::files::Holder>, denied: usize) {
        if let Some(f) = &mut self.find {
            f.results = Some((found, denied));
            f.searching = false;
        }
    }

    fn find_window(&mut self, ui: &egui::Ui, snap: &Snapshot, c: &mut Ctx) {
        let Some(FindState { query, results: res, searching }) = &mut self.find else { return };
        let (mut open, mut search, mut pick) = (true, false, None);
        egui::Window::new("Find open file").open(&mut open).default_size([620.0, 380.0]).show(ui.ctx(), |ui| {
            ui.horizontal(|ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(query)
                        .hint_text("Path or part of it, e.g. /media/usb or .lock")
                        .desired_width(360.0),
                );
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                if (ui.add_enabled(!query.trim().is_empty() && !*searching, egui::Button::new("Search")).clicked()
                    || enter)
                    && !query.trim().is_empty()
                {
                    search = true;
                }
                if *searching {
                    ui.spinner();
                }
            });
            if let Some((found, denied)) = res {
                ui.label(RichText::new(format!("{} match(es)", found.len())).strong());
                if *denied > 0 {
                    ui.label(
                        RichText::new(format!(
                            "{denied} processes of other users couldn't be inspected (run as root to include them)."
                        ))
                        .small()
                        .weak(),
                    );
                }
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    egui::Grid::new("holders").num_columns(4).striped(true).spacing([12.0, 4.0]).show(ui, |ui| {
                        for h in found.iter() {
                            if ui
                                .link(format!("{} ({})", h.name, h.pid))
                                .on_hover_text("Show in the process list")
                                .clicked()
                            {
                                pick = Some(h.pid);
                            }
                            ui.label(RichText::new(format!("fd {}", h.fd)).weak());
                            ui.label(&h.path);
                            ui.end_row();
                        }
                    });
                });
            }
        });
        if search {
            *searching = true;
            *res = None;
            let q = query.trim().to_string();
            c.jobs.spawn(move || {
                let (found, denied) = crate::files::find_holders(&q);
                JobResult::FileHolders(found, denied)
            });
        }
        if let Some(pid) = pick {
            self.clear_scope();
            self.filter.clear();
            self.select(pid, snap);
            self.scroll_to_selected = true;
        }
        if !open {
            self.find = None;
        }
    }

    fn select(&mut self, pid: u32, snap: &Snapshot) {
        self.selected = Some(pid);
        self.selected_start = snap.procs.iter().find(|p| p.pid == pid).map(|p| p.start);
    }

    /// The selected process, but only if it is still the same process *and* visible:
    /// Delete / End task must never hit a row the user can't see.
    fn selected_proc<'a>(&self, snap: &'a Snapshot) -> Option<&'a ProcInfo> {
        let pid = self.selected?;
        let p = snap.procs.iter().find(|p| p.pid == pid)?;
        if self.selected_start.is_some_and(|s| (s - p.start).abs() > 0.02) {
            return None;
        }
        self.order.iter().any(|&(i, ..)| snap.procs[i as usize].pid == pid).then_some(p)
    }

    fn keyboard(&mut self, ui: &egui::Ui, snap: &Snapshot, c: &mut Ctx) {
        let typing = ui.ctx().egui_wants_keyboard_input();
        let (find, del, shift, up, down, enter) = ui.input(|i| {
            (
                i.modifiers.command && i.key_pressed(Key::F),
                i.key_pressed(Key::Delete),
                i.modifiers.shift,
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::ArrowDown),
                i.key_pressed(Key::Enter),
            )
        });
        if find {
            self.focus_search = true;
        }
        if typing {
            return;
        }
        if del && let Some(p) = self.selected_proc(snap) {
            let sig = if shift { Signal::Kill } else { Signal::Term };
            self.apply(Action::Signal(p.pid, sig), snap, c);
        }
        if enter && let Some(pid) = self.selected_proc(snap).map(|p| p.pid) {
            self.apply(Action::Details(pid), snap, c);
        }
        if up || down {
            let pos =
                self.selected.and_then(|pid| self.order.iter().position(|&(i, ..)| snap.procs[i as usize].pid == pid));
            let next = match (pos, up) {
                (None, _) => 0,
                (Some(p), true) => p.saturating_sub(1),
                (Some(p), false) => (p + 1).min(self.order.len().saturating_sub(1)),
            };
            if let Some(&(i, ..)) = self.order.get(next) {
                self.select(snap.procs[i as usize].pid, snap);
                self.scroll_to_selected = true;
            }
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui, snap: &Snapshot, c: &mut Ctx) {
        ui.horizontal(|ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("🔍 Filter by name, command, user or PID  (Ctrl+F)")
                    .desired_width(320.0),
            );
            if self.focus_search {
                r.request_focus();
                self.focus_search = false;
            }
            if r.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                self.filter.clear();
            }
            if !self.filter.is_empty() && ui.small_button("✖").clicked() {
                self.filter.clear();
            }
            if let Some(scope) = &self.scope {
                let chip = match scope {
                    Scope::Group(label, pids) => format!("Showing: {label} ({}) ✖", pids.len()),
                    Scope::User(u) => format!("Showing: user {u} ✖"),
                };
                if ui.button(RichText::new(chip).strong()).on_hover_text("Show all processes again").clicked() {
                    self.clear_scope();
                }
            }
            ui.checkbox(&mut c.cfg.tree, "Tree");
            ui.checkbox(&mut c.cfg.only_mine, "Only mine");
            ui.checkbox(&mut c.cfg.show_kernel, "Kernel threads");

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let sel = self.selected_proc(snap).cloned();
                ui.add_enabled_ui(sel.is_some(), |ui| {
                    if ui
                        .button("End task")
                        .on_hover_text("Send SIGTERM (Delete)\nShift+Delete sends SIGKILL")
                        .clicked()
                        && let Some(p) = &sel
                    {
                        self.apply(Action::Signal(p.pid, Signal::Term), snap, c);
                    }
                });
                if ui
                    .button("Find open file…")
                    .on_hover_text("Which process has a file, device or socket open?")
                    .clicked()
                {
                    self.find = Some(FindState { query: String::new(), results: None, searching: false });
                }
                ui.menu_button("Columns", |ui| {
                    let mut hidden = Self::hidden_cols(c.cfg);
                    for col in Col::ALL.into_iter().skip(1) {
                        let mut shown = !hidden.contains(&col);
                        if ui.checkbox(&mut shown, col.title()).changed() {
                            if shown {
                                hidden.remove(&col);
                            } else {
                                hidden.insert(col);
                            }
                            c.cfg.hidden_cols = Col::ALL
                                .iter()
                                .filter(|x| hidden.contains(x))
                                .map(|x| x.key())
                                .collect::<Vec<_>>()
                                .join(",");
                        }
                    }
                    ui.separator();
                    ui.checkbox(&mut c.cfg.per_core_percent, "CPU % per core (top-style)")
                        .on_hover_text("Off: % of the whole machine. On: 100% = one full core.");
                });
                ui.label(RichText::new(format!("{} shown", self.order.len())).weak());
            });
        });
    }

    fn table(&mut self, ui: &mut egui::Ui, snap: &Snapshot, c: &Ctx) -> Option<Action> {
        let hidden = Self::hidden_cols(c.cfg);
        let no_gpu = snap.gpus.is_empty();
        let cols: Vec<Col> =
            Col::ALL.into_iter().filter(|c| !hidden.contains(c) && !(no_gpu && *c == Col::Gpu)).collect();
        let sort = Col::from_key(&c.cfg.sort_col).unwrap_or(Col::Cpu);
        let desc = c.cfg.sort_desc;
        let mut act: Option<Action> = None;
        let mut sort_click: Option<Col> = None;
        let ncpu = snap.cpu.logical.max(1) as f32;
        let cpu_mult = if c.cfg.per_core_percent { ncpu } else { 1.0 };
        let total_mem = snap.mem.total.max(1) as f32;
        // Process start times are on the boot clock: compare with uptime, not wall time.
        let now_since_boot = snap.uptime;
        let dark = ui.visuals().dark_mode;
        let heat = |v: f32| -> Color32 {
            let a = (v.clamp(0.0, 1.0) * 0.85 + if v > 0.0 { 0.06 } else { 0.0 }).min(0.9);
            if dark {
                Color32::from_rgba_unmultiplied(0x3b, 0x8e, 0xea, (a * 120.0) as u8)
            } else {
                Color32::from_rgba_unmultiplied(0x3b, 0x8e, 0xea, (a * 90.0) as u8)
            }
        };
        let scroll_to = if self.scroll_to_selected {
            self.scroll_to_selected = false;
            self.selected.and_then(|pid| self.order.iter().position(|&(i, ..)| snap.procs[i as usize].pid == pid))
        } else {
            None
        };
        let row_h = 20.0;

        let mut tb = TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .cell_layout(Layout::left_to_right(Align::Center))
            .min_scrolled_height(0.0)
            .auto_shrink([false, false]);
        if let Some(r) = scroll_to {
            tb = tb.scroll_to_row(r, None);
        }
        for col in &cols {
            tb = tb.column(col.column());
        }
        tb.header(24.0, |mut h| {
            for &col in &cols {
                h.col(|ui| {
                    let arrow = if col == sort { if desc { " ⏷" } else { " ⏶" } } else { "" };
                    let text = RichText::new(format!("{}{arrow}", col.title())).strong();
                    let layout = if col.numeric() {
                        Layout::right_to_left(Align::Center)
                    } else {
                        Layout::left_to_right(Align::Center)
                    };
                    ui.with_layout(layout, |ui| {
                        if ui.add(egui::Label::new(text).sense(Sense::click()).selectable(false)).clicked() {
                            sort_click = Some(col);
                        }
                    });
                });
            }
        })
        .body(|body| {
            body.rows(row_h, self.order.len(), |mut row| {
                let (idx, depth, has_kids) = self.order[row.index()];
                let p = &snap.procs[idx as usize];
                row.set_selected(
                    self.selected == Some(p.pid) && self.selected_start.is_none_or(|s| (s - p.start).abs() <= 0.02),
                );
                for &col in &cols {
                    row.col(|ui| {
                        let num = |ui: &mut egui::Ui, s: String| {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.label(s));
                        };
                        match col {
                            Col::Name => {
                                if depth > 0 {
                                    ui.add_space(depth as f32 * 14.0);
                                }
                                if c.cfg.tree {
                                    if has_kids {
                                        let open = !self.collapsed.contains(&p.pid);
                                        let r = ui.add(
                                            egui::Label::new(if open { "⏷" } else { "⏵" })
                                                .sense(Sense::click())
                                                .selectable(false),
                                        );
                                        if r.clicked() {
                                            act = Some(Action::Toggle(p.pid));
                                        }
                                    } else {
                                        ui.add_space(14.0);
                                    }
                                }
                                let t = if p.state == b'T' {
                                    RichText::new(format!("{} (suspended)", p.name)).italics()
                                } else if p.state == b'Z' {
                                    RichText::new(format!("{} (zombie)", p.name)).weak()
                                } else {
                                    RichText::new(&*p.name)
                                };
                                ui.add(egui::Label::new(t).selectable(false).truncate());
                            }
                            Col::Pid => num(ui, p.pid.to_string()),
                            Col::User => {
                                ui.add(egui::Label::new(&*p.user).selectable(false).truncate());
                            }
                            Col::Cpu => {
                                ui.painter().rect_filled(ui.max_rect(), 0.0, heat(p.cpu / 25.0));
                                num(ui, format!("{:.1}%", p.cpu * cpu_mult));
                            }
                            Col::Mem => {
                                ui.painter().rect_filled(ui.max_rect(), 0.0, heat(p.mem as f32 / total_mem * 8.0));
                                num(ui, fmt_bytes(p.mem));
                            }
                            Col::Gpu => {
                                ui.painter().rect_filled(ui.max_rect(), 0.0, heat(p.gpu / 50.0));
                                num(ui, format!("{:.0}%", p.gpu));
                            }
                            Col::Spark => sparkline(ui, p),
                            Col::CpuTime => num(ui, fmt_cpu_time(p.cpu_time)),
                            Col::Read => num(ui, fmt_rate(p.read_rate)),
                            Col::Write => num(ui, fmt_rate(p.write_rate)),
                            Col::Threads => num(ui, p.threads.to_string()),
                            Col::Nice => num(ui, p.nice.to_string()),
                            Col::State => {
                                ui.add(egui::Label::new(state_name(p.state)).selectable(false));
                            }
                            Col::Started => num(ui, fmt_duration((now_since_boot - p.start).max(0.0) as u64)),
                            Col::Command => {
                                let t = if p.cmd.is_empty() { format!("[{}]", p.name) } else { p.cmd.to_string() };
                                ui.add(egui::Label::new(RichText::new(t).weak()).selectable(false).truncate());
                            }
                        }
                    });
                }
                let r = row.response();
                if r.clicked() {
                    act = Some(Action::Select(p.pid));
                }
                if r.double_clicked() {
                    act = Some(Action::Details(p.pid));
                }
                r.context_menu(|ui| {
                    if let Some(a) = context_menu(ui, p) {
                        act = Some(a);
                    }
                });
            });
        });

        self.pending_sort = sort_click;
        act
    }

    fn apply(&mut self, a: Action, snap: &Snapshot, c: &mut Ctx) {
        match a {
            Action::Select(pid) => self.select(pid, snap),
            Action::Toggle(pid) => {
                if !self.collapsed.remove(&pid) {
                    self.collapsed.insert(pid);
                }
            }
            Action::Details(pid) => {
                if !self.details.iter().any(|(d, ..)| d.pid == pid) {
                    self.details.push((actions::read_details(pid), snap.generation, true));
                }
            }
            Action::Copy(s) => {
                c.info("Copied to clipboard");
                self.clipboard = Some(s);
            }
            Action::FilterUser(u) => self.show_user(&u),
            Action::Signal(pid, sig) => {
                let name = snap.procs.iter().find(|p| p.pid == pid).map_or_else(|| "?".into(), |p| p.name.clone());
                match actions::send_signal(pid, sig) {
                    Ok(()) => c.info(format!("{} {name} ({pid}): sent SIG{}", sig.verb(), sig.name())),
                    Err(ActionError::Permission) => {
                        *c.elevate = Some((
                            format!("{} {name} ({pid})", sig.verb().to_lowercase()),
                            actions::elevated_signal_args(pid, sig),
                        ));
                    }
                    Err(ActionError::Other(e)) => {
                        c.error(format!("Couldn't {} {name}: {e}", sig.verb().to_lowercase()))
                    }
                }
            }
            Action::Nice(pid, n) => match actions::set_nice(pid, n) {
                Ok(()) => c.info(format!("Priority of {pid} set to nice {n}")),
                Err(ActionError::Permission) => {
                    *c.elevate =
                        Some((format!("change the priority of {pid} to {n}"), actions::elevated_nice_args(pid, n)));
                }
                Err(ActionError::Other(e)) => c.error(format!("Couldn't change priority: {e}")),
            },
        }
    }

    /// Process detail windows (several may be open at once). Refreshed once per snapshot.
    pub fn detail_windows(&mut self, ctx: &egui::Context, snap: &Arc<Snapshot>) {
        if let Some(s) = self.clipboard.take() {
            ctx.copy_text(s);
        }
        for (d, generation, open) in &mut self.details {
            if *generation != snap.generation {
                *d = actions::read_details(d.pid);
                *generation = snap.generation;
            }
            let live = snap.procs.iter().find(|p| p.pid == d.pid);
            let title = match live {
                Some(p) => format!("{} ({})", p.name, d.pid),
                None => format!("PID {} (exited)", d.pid),
            };
            egui::Window::new(title)
                .id(egui::Id::new(("details", d.pid)))
                .open(open)
                .default_size([460.0, 420.0])
                .resizable(true)
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        egui::Grid::new(("dg", d.pid)).num_columns(2).striped(true).spacing([12.0, 4.0]).show(
                            ui,
                            |ui| {
                                let mut row = |k: &str, v: &str| {
                                    ui.label(RichText::new(k).strong());
                                    ui.add(egui::Label::new(v).wrap());
                                    ui.end_row();
                                };
                                if let Some(p) = live {
                                    row("CPU", &format!("{:.1}%", p.cpu));
                                    row("Memory (private)", &fmt_bytes(p.mem));
                                    row("Resident (RSS)", &fmt_bytes(p.rss));
                                    row("GPU", &format!("{:.0}%", p.gpu));
                                    row(
                                        "Disk read / write",
                                        &format!("{} / {}", fmt_rate(p.read_rate), fmt_rate(p.write_rate)),
                                    );
                                    row("User", &p.user);
                                    row("Nice", &p.nice.to_string());
                                }
                                row("Executable", &d.exe);
                                row("Working dir", &d.cwd);
                                row("Command line", &d.cmdline);
                                if let Some(n) = d.open_fds {
                                    row("Open files", &n.to_string());
                                }
                                if let Some(l) = &d.limits_open_files {
                                    row("Open file limit", l);
                                }
                                if let Some(n) = d.env_count {
                                    row("Environment vars", &n.to_string());
                                }
                                row("Cgroup", &d.cgroup);
                                for (k, v) in &d.status {
                                    row(k, v);
                                }
                            },
                        );
                        files_section(ui, d);
                    });
                });
        }
        self.details.retain(|(_, _, open)| *open);
    }
}

fn context_menu(ui: &mut egui::Ui, p: &ProcInfo) -> Option<Action> {
    let mut act = None;
    ui.label(RichText::new(format!("{} ({})", p.name, p.pid)).strong());
    ui.separator();
    if ui.button("End task (TERM)").clicked() {
        act = Some(Action::Signal(p.pid, Signal::Term));
    }
    if ui.button("Kill (KILL)").clicked() {
        act = Some(Action::Signal(p.pid, Signal::Kill));
    }
    if p.state == b'T' {
        if ui.button("Resume (CONT)").clicked() {
            act = Some(Action::Signal(p.pid, Signal::Cont));
        }
    } else if ui.button("Suspend (STOP)").clicked() {
        act = Some(Action::Signal(p.pid, Signal::Stop));
    }
    ui.menu_button("Other signals", |ui| {
        for s in [Signal::Hup, Signal::Int, Signal::Cont, Signal::Stop] {
            if ui.button(format!("SIG{}", s.name())).clicked() {
                act = Some(Action::Signal(p.pid, s));
            }
        }
    });
    ui.menu_button("Priority", |ui| {
        for (label, n) in [
            ("Very high (-15)", -15),
            ("High (-5)", -5),
            ("Normal (0)", 0),
            ("Low (5)", 5),
            ("Very low (15)", 15),
            ("Idle (19)", 19),
        ] {
            if ui.radio(p.nice == n, label).clicked() {
                act = Some(Action::Nice(p.pid, n));
            }
        }
    });
    ui.separator();
    if ui.button("Details…").clicked() {
        act = Some(Action::Details(p.pid));
    }
    if ui.button("Filter by this user").clicked() {
        act = Some(Action::FilterUser(p.user.to_string()));
    }
    ui.menu_button("Copy", |ui| {
        if ui.button("PID").clicked() {
            act = Some(Action::Copy(p.pid.to_string()));
        }
        if ui.button("Name").clicked() {
            act = Some(Action::Copy(p.name.to_string()));
        }
        if ui.button("Command line").clicked() {
            act = Some(Action::Copy(p.cmd.to_string()));
        }
    });
    if act.is_some() {
        ui.close();
    }
    act
}
