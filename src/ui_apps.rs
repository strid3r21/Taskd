//! Apps tab: running applications (grouped processes), with per-app totals.

use std::collections::HashSet;

use eframe::egui::{self, Align, Color32, Key, Layout, RichText, Sense};
use egui_extras::{Column, TableBuilder};

use crate::actions::{self, ActionError, Signal};
use crate::app::{Ctx, JobResult};
use crate::groups::{Group, Kind};
use crate::util::{fmt_bytes, fmt_rate};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sort {
    Name,
    Procs,
    Cpu,
    Mem,
    Gpu,
    Read,
    Write,
    Net,
}

pub struct AppsState {
    filter: String,
    selected: Option<String>,
    expanded: HashSet<String>,
    sort: Sort,
    desc: bool,
    limit: Option<crate::ui_limits::LimitDialog>,
}

impl Default for AppsState {
    fn default() -> Self {
        AppsState {
            filter: String::new(),
            selected: None,
            expanded: HashSet::new(),
            sort: Sort::Cpu,
            desc: true,
            limit: None,
        }
    }
}

enum Act {
    /// `docker`/`podman` <verb> <container id>.
    Container(String, &'static str),
    Limit(String),
    Select(String),
    Toggle(String),
    SignalApp(String, Signal),
    SignalPid(u32, Signal),
}

pub fn heat(ui: &egui::Ui, v: f32) -> Color32 {
    let a = (v.clamp(0.0, 1.0) * 0.85 + if v > 0.0 { 0.06 } else { 0.0 }).min(0.9);
    let scale = if ui.visuals().dark_mode { 120.0 } else { 90.0 };
    Color32::from_rgba_unmultiplied(0x3b, 0x8e, 0xea, (a * scale) as u8)
}

impl AppsState {
    pub fn ui(&mut self, ui: &mut egui::Ui, c: &mut Ctx) {
        let snap = c.snap.clone();
        let needle = self.filter.trim().to_lowercase();
        let mut apps: Vec<&Group> = c
            .groups
            .iter()
            // Your apps, plus containers and VMs (which usually run as root).
            .filter(|g| (g.kind == Kind::App && g.mine) || matches!(g.kind, Kind::Container | Kind::Vm))
            .filter(|g| needle.is_empty() || g.name.to_lowercase().contains(&needle))
            .collect();
        let (sort, desc) = (self.sort, self.desc);
        apps.sort_by(|a, b| {
            let o = match sort {
                Sort::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                Sort::Procs => a.members.len().cmp(&b.members.len()),
                Sort::Cpu => a.cpu.total_cmp(&b.cpu),
                Sort::Mem => a.mem.cmp(&b.mem),
                Sort::Gpu => a.gpu.total_cmp(&b.gpu),
                Sort::Read => a.read.total_cmp(&b.read),
                Sort::Write => a.write.total_cmp(&b.write),
                Sort::Net => a.net.total_cmp(&b.net),
            };
            (if desc { o.reverse() } else { o }).then_with(|| a.name.cmp(&b.name))
        });

        let selected = self.selected.as_ref().and_then(|k| apps.iter().find(|g| &g.key == k).copied());
        let mut act: Option<Act> = None;

        // Keyboard: Delete ends the selected app, Shift+Delete kills it.
        if !ui.ctx().egui_wants_keyboard_input()
            && let Some(g) = selected
        {
            let (del, shift) = ui.input(|i| (i.key_pressed(Key::Delete), i.modifiers.shift));
            if del {
                act = Some(Act::SignalApp(g.key.clone(), if shift { Signal::Kill } else { Signal::Term }));
            }
        }

        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("🔍 Filter apps").desired_width(240.0));
            ui.label(RichText::new(format!("{} apps running", apps.len())).weak());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_enabled_ui(selected.is_some(), |ui| {
                    if ui.button("End task").on_hover_text("End every process of the app (Delete)").clicked()
                        && let Some(g) = selected
                    {
                        act = Some(Act::SignalApp(g.key.clone(), Signal::Term));
                    }
                });
            });
        });
        ui.add_space(2.0);

        // Flatten into rows: each app, then its processes when expanded.
        let rows: Vec<(usize, Option<u32>)> = apps
            .iter()
            .enumerate()
            .flat_map(|(gi, g)| {
                let open = self.expanded.contains(&g.key);
                std::iter::once((gi, None)).chain(g.members.iter().filter(move |_| open).map(move |&m| (gi, Some(m))))
            })
            .collect();

        let show_gpu = !snap.gpus.is_empty();
        let show_net = snap.helper.is_some();
        let total_mem = snap.mem.total.max(1) as f32;
        let mut sort_click = None;
        let headers: Vec<(&str, Sort)> = [
            ("Name", Sort::Name),
            ("Processes", Sort::Procs),
            ("CPU", Sort::Cpu),
            ("Memory", Sort::Mem),
            ("GPU", Sort::Gpu),
            ("Disk read", Sort::Read),
            ("Disk write", Sort::Write),
            ("Network", Sort::Net),
        ]
        .into_iter()
        .filter(|(_, s)| (show_gpu || *s != Sort::Gpu) && (show_net || *s != Sort::Net))
        .collect();

        let mut tb = TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .auto_shrink([false, false])
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::initial(300.0).at_least(120.0).clip(true));
        for _ in 1..headers.len() {
            tb = tb.column(Column::initial(95.0).at_least(55.0));
        }
        tb.header(24.0, |mut h| {
            for (title, s) in &headers {
                h.col(|ui| {
                    let arrow = if *s == sort { if desc { " ⏷" } else { " ⏶" } } else { "" };
                    let layout = if *s == Sort::Name {
                        Layout::left_to_right(Align::Center)
                    } else {
                        Layout::right_to_left(Align::Center)
                    };
                    ui.with_layout(layout, |ui| {
                        let l = egui::Label::new(RichText::new(format!("{title}{arrow}")).strong())
                            .sense(Sense::click())
                            .selectable(false);
                        if ui.add(l).clicked() {
                            sort_click = Some(*s);
                        }
                    });
                });
            }
        })
        .body(|body| {
            body.rows(21.0, rows.len(), |mut row| {
                let (gi, member) = rows[row.index()];
                let g = apps[gi];
                let num = |ui: &mut egui::Ui, s: String| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.label(s));
                };
                match member {
                    None => {
                        row.set_selected(self.selected.as_deref() == Some(&g.key));
                        row.col(|ui| {
                            let open = self.expanded.contains(&g.key);
                            let arrow =
                                egui::Label::new(if open { "⏷" } else { "⏵" }).sense(Sense::click()).selectable(false);
                            if ui.add(arrow).clicked() {
                                act = Some(Act::Toggle(g.key.clone()));
                            }
                            ui.add(egui::Label::new(RichText::new(&g.name).strong()).selectable(false).truncate());
                            if g.kind != Kind::App {
                                ui.label(RichText::new(g.kind.label()).small().weak());
                            }
                        });
                        row.col(|ui| num(ui, g.members.len().to_string()));
                        row.col(|ui| {
                            ui.painter().rect_filled(ui.max_rect(), 0.0, heat(ui, g.cpu / 25.0));
                            num(ui, format!("{:.1}%", g.cpu));
                        });
                        row.col(|ui| {
                            ui.painter().rect_filled(ui.max_rect(), 0.0, heat(ui, g.mem as f32 / total_mem * 8.0));
                            num(ui, fmt_bytes(g.mem));
                        });
                        if show_gpu {
                            row.col(|ui| {
                                ui.painter().rect_filled(ui.max_rect(), 0.0, heat(ui, g.gpu / 50.0));
                                num(ui, format!("{:.0}%", g.gpu));
                            });
                        }
                        row.col(|ui| num(ui, fmt_rate(g.read)));
                        row.col(|ui| num(ui, fmt_rate(g.write)));
                        if show_net {
                            row.col(|ui| num(ui, fmt_rate(g.net)));
                        }
                        let r = row.response();
                        if r.clicked() {
                            act = Some(Act::Select(g.key.clone()));
                        }
                        if r.double_clicked() {
                            act = Some(Act::Toggle(g.key.clone()));
                        }
                        r.context_menu(|ui| {
                            ui.label(RichText::new(&g.name).strong());
                            ui.separator();
                            for (label, sig) in [
                                ("End task", Signal::Term),
                                ("Kill", Signal::Kill),
                                ("Suspend", Signal::Stop),
                                ("Resume", Signal::Cont),
                            ] {
                                if ui.button(label).clicked() {
                                    act = Some(Act::SignalApp(g.key.clone(), sig));
                                    ui.close();
                                }
                            }
                            ui.separator();
                            if ui.button("Limit resources…").clicked() {
                                act = Some(Act::Limit(g.key.clone()));
                                ui.close();
                            }
                            if let Some(id) = g.key.strip_prefix("ctr:") {
                                ui.separator();
                                for (label, verb) in [("Stop container", "stop"), ("Restart container", "restart")] {
                                    if ui.button(label).on_hover_text("Cleanly, through docker/podman").clicked() {
                                        act = Some(Act::Container(id.to_string(), verb));
                                        ui.close();
                                    }
                                }
                            }
                        });
                    }
                    Some(m) => {
                        let p = &snap.procs[m as usize];
                        row.col(|ui| {
                            ui.add_space(28.0);
                            ui.add(egui::Label::new(&*p.name).selectable(false).truncate());
                            ui.label(RichText::new(p.pid.to_string()).weak().small());
                        });
                        row.col(|ui| num(ui, String::new()));
                        row.col(|ui| num(ui, format!("{:.1}%", p.cpu)));
                        row.col(|ui| num(ui, fmt_bytes(p.mem)));
                        if show_gpu {
                            row.col(|ui| num(ui, format!("{:.0}%", p.gpu)));
                        }
                        row.col(|ui| num(ui, fmt_rate(p.read_rate)));
                        row.col(|ui| num(ui, fmt_rate(p.write_rate)));
                        if show_net {
                            row.col(|ui| num(ui, fmt_rate(p.net_rx + p.net_tx)));
                        }
                        row.response().context_menu(|ui| {
                            ui.label(RichText::new(format!("{} ({})", p.name, p.pid)).strong());
                            ui.separator();
                            for (label, sig) in [("End process", Signal::Term), ("Kill process", Signal::Kill)] {
                                if ui.button(label).clicked() {
                                    act = Some(Act::SignalPid(p.pid, sig));
                                    ui.close();
                                }
                            }
                        });
                    }
                }
            });
        });

        if let Some(s) = sort_click {
            if self.sort == s {
                self.desc = !self.desc;
            } else {
                self.sort = s;
                self.desc = s != Sort::Name;
            }
        }
        match act {
            Some(Act::Select(k)) => self.selected = Some(k),
            Some(Act::Toggle(k)) => {
                if !self.expanded.remove(&k) {
                    self.expanded.insert(k);
                }
            }
            Some(Act::SignalApp(k, sig)) => {
                if let Some(g) = apps.iter().find(|g| g.key == k) {
                    let pids: Vec<u32> = g.members.iter().map(|&m| snap.procs[m as usize].pid).collect();
                    signal_many(c, &g.name, &pids, sig);
                }
            }
            Some(Act::SignalPid(pid, sig)) => signal_many(c, &pid.to_string(), &[pid], sig),
            Some(Act::Limit(key)) => {
                if let Some(g) = apps.iter().find(|g| g.key == key) {
                    let unit = g.members.iter().find_map(|&m| snap.procs[m as usize].unit.as_deref());
                    // Only app scopes / containers can be limited live; session-launched apps can't.
                    let unit = unit.filter(|u| !u.starts_with("session-"));
                    let target = crate::limits::target(unit, key.strip_prefix("ctr:"));
                    let exec = c.exec_for(&key).map(|e| crate::limits::exec_argv(&e)).unwrap_or_default();
                    self.limit = Some(crate::ui_limits::LimitDialog::new(&g.name, target, exec));
                }
            }
            Some(Act::Container(id, verb)) => {
                c.info(format!("{verb} container {}…", &id[..id.len().min(12)]));
                c.jobs.spawn(move || {
                    // Whichever runtime owns it: docker first, then podman.
                    let mut last = String::new();
                    for tool in ["docker", "podman"] {
                        match std::process::Command::new(actions::sys_bin(tool)).args([verb, "--", &id]).output() {
                            Ok(o) if o.status.success() => {
                                return JobResult::Status(format!("{verb} container: done"), false);
                            }
                            Ok(o) => last = String::from_utf8_lossy(&o.stderr).trim().to_string(),
                            Err(_) => {}
                        }
                    }
                    JobResult::Status(format!("{verb} container failed: {last}"), true)
                });
            }
            None => {}
        }
        if let Some(d) = &mut self.limit
            && !d.show(ui.ctx(), c, snap.cpu.logical)
        {
            self.limit = None;
        }
    }
}

/// Signal a set of processes; anything denied is offered for a retry through pkexec.
pub fn signal_many(c: &mut Ctx, what: &str, pids: &[u32], sig: Signal) {
    let mut denied = Vec::new();
    let mut other = None;
    let mut sent = 0;
    for &pid in pids {
        match actions::send_signal(pid, sig) {
            Ok(()) => sent += 1,
            Err(ActionError::Permission) => denied.push(pid),
            Err(ActionError::Other(e)) => other = Some(e),
        }
    }
    if !denied.is_empty() {
        let mut args = vec![actions::sys_bin("kill"), "-s".into(), sig.name().into()];
        args.extend(denied.iter().map(|p| p.to_string()));
        *c.elevate = Some((format!("{} {what}", sig.verb().to_lowercase()), args));
    } else if sent == 0 {
        let why = other.unwrap_or_else(|| "no processes left".into());
        c.error(format!("Couldn't {} {what}: {why}", sig.verb().to_lowercase()));
    } else {
        c.info(format!("{} {what}: sent SIG{} to {sent} process(es)", sig.verb(), sig.name()));
    }
}
