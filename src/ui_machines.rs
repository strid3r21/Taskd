//! Machines tab: other Linux computers (e.g. a home server) watched over SSH.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use eframe::egui::{self, Color32, RichText, Vec2};
use serde_json::Value;

use crate::app::{Ctx, JobResult};
use crate::graph::{CPU, MEM, graph_series};
use crate::remote::{self, Remote, Status};
use crate::util::{fmt_bytes, fmt_duration, fmt_rate};

#[derive(Default)]
pub struct MachinesState {
    remotes: Vec<Remote>,
    new_host: String,
    /// Milliseconds (since `epoch`) when the tab was last drawn: updates only repaint while visible.
    drawn: Arc<AtomicU64>,
    installing: Option<String>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

pub fn hosts(cfg: &str) -> Vec<String> {
    cfg.split(',').map(|h| h.trim().to_string()).filter(|h| remote::valid_host(h)).collect()
}

impl MachinesState {
    pub fn installed(&mut self) {
        self.installing = None;
    }

    /// Keep one connection per configured host.
    fn sync(&mut self, ctx: &egui::Context, list: &[String]) {
        self.remotes.retain(|r| list.contains(&r.host));
        for h in list {
            if !self.remotes.iter().any(|r| r.host == *h) {
                let (ctx, drawn) = (ctx.clone(), self.drawn.clone());
                self.remotes.push(Remote::ssh(h, move || {
                    if now_ms().saturating_sub(drawn.load(Ordering::Relaxed)) < 1500 {
                        ctx.request_repaint();
                    }
                }));
            }
        }
        self.remotes.sort_by_key(|r| list.iter().position(|h| *h == r.host));
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, c: &mut Ctx) {
        self.drawn.store(now_ms(), Ordering::Relaxed);
        let list = hosts(&c.cfg.remote_hosts);
        self.sync(ui.ctx(), &list);

        ui.horizontal(|ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.new_host)
                    .hint_text("user@host or an ~/.ssh/config name")
                    .desired_width(260.0),
            );
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let h = self.new_host.trim().to_string();
            let ok = remote::valid_host(&h) && !list.contains(&h);
            if (ui.add_enabled(ok, egui::Button::new("Add machine")).clicked() || (enter && ok)) && ok {
                let mut l = list.clone();
                l.push(h);
                c.cfg.remote_hosts = l.join(",");
                self.new_host.clear();
            }
        });
        ui.label(
            RichText::new(
                "Watches other Linux machines over SSH using key-based login (set it up once with ssh-copy-id). \
                 Taskd must be installed on the other machine; nothing else runs there and no port is opened.",
            )
            .small()
            .weak(),
        );
        ui.separator();
        if self.remotes.is_empty() {
            ui.add_space(12.0);
            ui.label(RichText::new("No machines yet. Add one above, e.g. root@tower for an Unraid server.").weak());
            return;
        }
        let mut remove = None;
        let mut install = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for r in &self.remotes {
                let st = r.state.lock().unwrap();
                egui::Frame::group(ui.style()).inner_margin(10.0).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let shown = st.last.as_ref().and_then(|v| v["host"].as_str()).filter(|h| !h.is_empty());
                        ui.label(RichText::new(shown.unwrap_or(&r.host)).size(18.0).strong());
                        if shown.is_some_and(|h| h != r.host) {
                            ui.label(RichText::new(&r.host).weak());
                        }
                        let (dot, text) = match &st.status {
                            Status::Connected => (
                                Color32::from_rgb(0x3c, 0xb3, 0x71),
                                format!("Taskd {}", st.last.as_ref().and_then(|v| v["version"].as_str()).unwrap_or("")),
                            ),
                            Status::Connecting => (Color32::from_rgb(0xe0, 0xa8, 0x2e), "Connecting…".into()),
                            Status::Error(_) => (ui.visuals().error_fg_color, "Not connected".into()),
                        };
                        ui.colored_label(dot, "●");
                        ui.label(RichText::new(text).small());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("✖").on_hover_text("Stop watching this machine").clicked() {
                                remove = Some(r.host.clone());
                            }
                        });
                    });
                    if let Status::Error(e) = &st.status {
                        ui.colored_label(ui.visuals().warn_fg_color, e.replace("HOST", &r.host));
                        if e.contains("isn't installed") {
                            let busy = self.installing.as_deref() == Some(r.host.as_str());
                            if ui
                                .add_enabled(
                                    !busy,
                                    egui::Button::new(if busy { "Installing…" } else { "Install there" }),
                                )
                                .on_hover_text(format!("Runs the one-line installer on {} over SSH", r.host))
                                .clicked()
                            {
                                install = Some(r.host.clone());
                            }
                        }
                    }
                    if let Some(v) = &st.last {
                        let stale = st.at.is_some_and(|t| t.elapsed().as_secs() > 15);
                        if stale {
                            ui.label(RichText::new("(last known values)").small().weak());
                        }
                        machine_card(ui, v, &st.cpu_hist, &st.mem_hist);
                    }
                });
                ui.add_space(6.0);
            }
        });
        if let Some(h) = remove {
            c.cfg.remote_hosts = list.into_iter().filter(|x| *x != h).collect::<Vec<_>>().join(",");
        }
        if let Some(h) = install {
            self.installing = Some(h.clone());
            c.jobs.spawn(move || match remote::install_over_ssh(&h) {
                Ok(_) => JobResult::Installed(format!("Taskd installed on {h}")),
                Err(e) => JobResult::Installed(format!("Couldn't install on {h}: {e}")),
            });
        }
    }
}

fn spark(ui: &mut egui::Ui, h: &std::collections::VecDeque<f32>, color: Color32) {
    let v: Vec<f32> = h.iter().copied().collect();
    graph_series(ui, Vec2::new(120.0, 34.0), &[(&v, color, false)], 100.0);
}

fn machine_card(
    ui: &mut egui::Ui,
    v: &Value,
    cpu_h: &std::collections::VecDeque<f32>,
    mem_h: &std::collections::VecDeque<f32>,
) {
    let f = |x: &Value| x.as_f64().unwrap_or(0.0);
    ui.horizontal_wrapped(|ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new(format!("CPU {:.0}%", f(&v["cpu"]["usage"]))).strong());
            spark(ui, cpu_h, CPU);
        });
        ui.add_space(8.0);
        ui.vertical(|ui| {
            let (used, total) = (v["mem"]["used"].as_u64().unwrap_or(0), v["mem"]["total"].as_u64().unwrap_or(0));
            ui.label(RichText::new(format!("Memory {} / {}", fmt_bytes(used), fmt_bytes(total))).strong());
            spark(ui, mem_h, MEM);
        });
        ui.add_space(8.0);
        ui.vertical(|ui| {
            let mut cpu = v["cpu"]["model"].as_str().unwrap_or("").to_string();
            if let Some(t) = v["cpu"]["temp"].as_f64() {
                cpu += &format!(" · {t:.0} °C");
            }
            if let Some(w) = v["cpu"]["power"].as_f64() {
                cpu += &format!(" · {w:.0} W");
            }
            ui.label(RichText::new(cpu).small());
            let l = &v["load"];
            ui.label(
                RichText::new(format!(
                    "Load {:.2} {:.2} {:.2} · up {}",
                    f(&l[0]),
                    f(&l[1]),
                    f(&l[2]),
                    fmt_duration(f(&v["uptime"]) as u64)
                ))
                .small(),
            );
            if let Some(p) = v["psi"]["verdict"].as_str() {
                ui.label(RichText::new(p).small().strong());
            }
            for g in v["gpus"].as_array().into_iter().flatten() {
                let util = g["util"].as_f64().map_or("—".into(), |u| format!("{u:.0}%"));
                let temp = g["temp"].as_f64().map_or(String::new(), |t| format!(" · {t:.0} °C"));
                ui.label(RichText::new(format!("{} {util}{temp}", g["name"].as_str().unwrap_or("GPU"))).small());
            }
        });
    });
    // Busy disks / network, and the fullest filesystems.
    let disks: Vec<String> = v["disks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|d| f(&d["busy"]) >= 1.0)
        .map(|d| format!("{} {:.0}% busy", d["name"].as_str().unwrap_or("?"), f(&d["busy"])))
        .collect();
    let nets: Vec<String> = v["nets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|n| f(&n["rx"]) + f(&n["tx"]) > 1024.0)
        .map(|n| format!("{} ↓{} ↑{}", n["name"].as_str().unwrap_or("?"), fmt_rate(f(&n["rx"])), fmt_rate(f(&n["tx"]))))
        .collect();
    if !disks.is_empty() || !nets.is_empty() {
        ui.label(RichText::new([disks, nets].concat().join("  ·  ")).small());
    }
    let mut space: Vec<(String, u64, u64)> = v["space"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| {
            (
                s["mount"].as_str().unwrap_or("?").to_string(),
                s["total"].as_u64().unwrap_or(0),
                s["avail"].as_u64().unwrap_or(0),
            )
        })
        .filter(|s| s.1 > 0)
        .collect();
    space.sort_by(|a, b| (a.2 as f64 / a.1 as f64).total_cmp(&(b.2 as f64 / b.1 as f64)));
    if !space.is_empty() {
        let txt: Vec<String> =
            space.iter().take(4).map(|(m, t, a)| format!("{m} {} free of {}", fmt_bytes(*a), fmt_bytes(*t))).collect();
        ui.label(RichText::new(format!("Space: {}", txt.join("  ·  "))).small());
    }
    let top = v["top"].as_array().cloned().unwrap_or_default();
    if !top.is_empty() {
        egui::CollapsingHeader::new(RichText::new("Heaviest apps").small())
            .id_salt(("top", v["host"].as_str().unwrap_or("")))
            .show(ui, |ui| {
                egui::Grid::new(("rtop", v["host"].as_str().unwrap_or(""))).num_columns(4).spacing([14.0, 2.0]).show(
                    ui,
                    |ui| {
                        for t in top {
                            ui.label(t[0].as_str().unwrap_or("?"));
                            ui.label(RichText::new(t[1].as_str().unwrap_or("")).small().weak());
                            ui.label(format!("{:.1}%", f(&t[2])));
                            ui.label(fmt_bytes(t[3].as_u64().unwrap_or(0)));
                            ui.end_row();
                        }
                    },
                );
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_a_card_from_a_real_summary() {
        let mut sampler = crate::sampler::Sampler::new();
        sampler.sample();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let s = sampler.sample();
        let mut g = crate::groups::Grouper::default();
        g.update(&s, 0);
        let v = remote::summary(&s, &g);
        let hist: std::collections::VecDeque<f32> = (0..90).map(|i| i as f32).collect();
        let ctx = egui::Context::default();
        for _ in 0..2 {
            let mut out = ctx.run_ui(Default::default(), |ui| machine_card(ui, &v, &hist, &hist));
            out.textures_delta.clear();
        }
        assert_eq!(hosts("root@tower, -bad ,nas"), vec!["root@tower", "nas"]);
    }
}
