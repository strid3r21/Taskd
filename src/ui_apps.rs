//! Apps tab: running applications (grouped processes), with per-app totals.

use std::collections::HashSet;

use eframe::egui::{self, Align, Color32, Key, Layout, RichText, Sense};
use egui_extras::{Column, TableBuilder};

use crate::actions::{self, ActionError, Signal};
use crate::app::Ctx;
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
}

pub struct AppsState {
    filter: String,
    selected: Option<String>,
    expanded: HashSet<String>,
    sort: Sort,
    desc: bool,
}

impl Default for AppsState {
    fn default() -> Self {
        AppsState { filter: String::new(), selected: None, expanded: HashSet::new(), sort: Sort::Cpu, desc: true }
    }
}

enum Act {
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
            .filter(|g| g.kind == Kind::App && g.mine)
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
        ]
        .into_iter()
        .filter(|(_, s)| show_gpu || *s != Sort::Gpu)
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
            None => {}
        }
    }
}

/// Signal a set of processes; anything denied is offered for a retry through pkexec.
fn signal_many(c: &mut Ctx, what: &str, pids: &[u32], sig: Signal) {
    let mut denied = Vec::new();
    let mut other = None;
    for &pid in pids {
        match actions::send_signal(pid, sig) {
            Ok(()) => {}
            Err(ActionError::Permission) => denied.push(pid),
            Err(ActionError::Other(e)) => other = Some(e),
        }
    }
    if !denied.is_empty() {
        let mut args = vec!["kill".to_string(), "-s".into(), sig.name().into()];
        args.extend(denied.iter().map(|p| p.to_string()));
        *c.elevate = Some((format!("{} {what}", sig.verb().to_lowercase()), args));
    } else if let Some(e) = other.filter(|_| pids.len() == 1) {
        c.error(format!("Couldn't {} {what}: {e}", sig.verb().to_lowercase()));
    } else {
        c.info(format!("{} {what}: sent SIG{} to {} process(es)", sig.verb(), sig.name(), pids.len()));
    }
}
