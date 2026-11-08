//! "Limit resources" dialog, shared by the Apps and Services tabs.

use eframe::egui::{self, RichText};

use crate::app::{Ctx, JobResult};
use crate::limits::{self, Limits, Target};

pub struct LimitDialog {
    name: String,
    target: Option<Target>,
    /// For apps not in their own scope: how to launch a new, limited instance.
    exec: Vec<String>,
    mem_gb: f64,
    cpu_pct: f64,
    persist: bool,
}

impl LimitDialog {
    pub fn new(name: &str, target: Option<Target>, exec: Vec<String>) -> Self {
        let cur = target.as_ref().map(limits::current).unwrap_or_default();
        LimitDialog {
            name: name.to_string(),
            mem_gb: cur.memory.map_or(0.0, |b| (b as f64 / 1_073_741_824.0 * 10.0).round() / 10.0),
            cpu_pct: cur.cpu_pct.unwrap_or(0.0),
            target,
            exec,
            persist: false,
        }
    }

    fn limits(&self) -> Limits {
        Limits {
            memory: (self.mem_gb > 0.0).then_some((self.mem_gb * 1_073_741_824.0) as u64),
            cpu_pct: (self.cpu_pct > 0.0).then_some(self.cpu_pct),
        }
    }

    /// Returns false once the dialog should close.
    pub fn show(&mut self, ctx: &egui::Context, c: &mut Ctx, ncpu: usize) -> bool {
        let mut open = true;
        let mut close = false;
        egui::Window::new(format!("Limit resources: {}", self.name))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                egui::Grid::new("limits").num_columns(2).spacing([14.0, 8.0]).show(ui, |ui| {
                    ui.label("Memory limit");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut self.mem_gb).range(0.0..=1024.0).speed(0.1).suffix(" GB"));
                        ui.label(RichText::new("0 = no limit").weak());
                    });
                    ui.end_row();
                    ui.label("CPU limit");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::DragValue::new(&mut self.cpu_pct)
                                .range(0.0..=(ncpu as f64 * 100.0))
                                .speed(5.0)
                                .suffix(" %"),
                        );
                        ui.label(RichText::new(format!("of one core (max {}%) · 0 = no limit", ncpu * 100)).weak());
                    });
                    ui.end_row();
                });
                ui.label(
                    RichText::new(
                        "Going over the memory limit makes the kernel reclaim memory and, if it can't, end the app.",
                    )
                    .small()
                    .weak(),
                );
                ui.separator();
                match &self.target {
                    Some(t) => {
                        ui.label(RichText::new(format!("Applies to {}.", t.describe())).small());
                        if t.can_persist() {
                            ui.checkbox(&mut self.persist, "Keep after restart/reboot");
                        } else {
                            ui.label(RichText::new("Lasts until the app is closed.").small().weak());
                        }
                        ui.horizontal(|ui| {
                            if ui.button("Apply").clicked() {
                                let (t, l, persist, name) = (t.clone(), self.limits(), self.persist, self.name.clone());
                                c.jobs.spawn(move || match limits::apply(&t, &l, persist) {
                                    Ok(()) => JobResult::Status(format!("Limits set for {name}"), false),
                                    Err(e) => JobResult::Status(format!("Couldn't limit {name}: {e}"), true),
                                });
                                close = true;
                            }
                            if ui.button("Remove limits").clicked() {
                                let (t, name) = (t.clone(), self.name.clone());
                                c.jobs.spawn(move || match limits::apply(&t, &Limits::default(), true) {
                                    Ok(()) => JobResult::Status(format!("Limits removed for {name}"), false),
                                    Err(e) => JobResult::Status(format!("Couldn't change {name}: {e}"), true),
                                });
                                close = true;
                            }
                        });
                    }
                    None => {
                        ui.label(
                            "This app was started by your desktop session rather than in its own systemd scope, \
                             so Linux can't limit it while it's running.",
                        );
                        if self.exec.is_empty() {
                            ui.label(
                                RichText::new("Taskd doesn't know how to start it, so it can't relaunch it limited.")
                                    .weak(),
                            );
                        } else if ui.button("Start a new instance with these limits").clicked() {
                            match limits::launch_limited(&self.exec, &self.name, &self.limits()) {
                                Ok(()) => c.info(format!("Started {} with limits", self.name)),
                                Err(e) => c.error(format!("Couldn't start {}: {e}", self.name)),
                            }
                            close = true;
                        }
                    }
                }
            });
        open && !close
    }
}
