//! Performance tab: device list with sparklines on the left, detail graphs on the right.

use eframe::egui::{self, Color32, RichText, Sense, Vec2};

use std::collections::HashSet;

use crate::actions::Signal;
use crate::app::{Ctx, Tab};
use crate::graph::{self, CPU, DISK, GPU, MEM, NET, Series, graph};
use crate::groups::Group;
use crate::sampler::{History, Snapshot};
use crate::ui_apps::signal_many;
use crate::ui_processes::ProcState;
use crate::util::{fmt_bits, fmt_bytes, fmt_duration, fmt_rate};

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Device {
    #[default]
    Cpu,
    Mem,
    Disk(usize),
    Net(usize),
    Gpu(usize),
}

#[derive(Default)]
pub struct PerfState {
    sel: Device,
    /// Key of the sidebar item being dragged to a new position.
    drag: Option<String>,
}

/// Round a throughput ceiling up to a "nice" value so graphs don't jitter.
fn nice_max(v: f32) -> f32 {
    let v = v.max(1024.0);
    let p = 10f32.powf(v.log10().floor());
    for m in [1.0, 2.0, 5.0, 10.0] {
        if v <= m * p {
            return m * p;
        }
    }
    10.0 * p
}

fn pct(a: u64, b: u64) -> f32 {
    if b == 0 { 0.0 } else { 100.0 * a as f32 / b as f32 }
}

/// Stable key for a device, used to remember the sidebar order across restarts.
fn device_key(d: Device, s: &Snapshot) -> String {
    match d {
        Device::Cpu => "cpu".into(),
        Device::Mem => "mem".into(),
        Device::Disk(i) => format!("disk:{}", s.disks[i].name),
        Device::Net(i) => format!("net:{}", s.nets[i].name),
        Device::Gpu(i) => format!("gpu:{i}"),
    }
}

/// All devices in the user's order; ones not in the saved order keep their default place at the end.
fn ordered_devices(s: &Snapshot, order: &str) -> Vec<(Device, String)> {
    let mut v: Vec<Device> = vec![Device::Cpu, Device::Mem];
    v.extend((0..s.disks.len()).map(Device::Disk));
    v.extend((0..s.nets.len()).map(Device::Net));
    v.extend((0..s.gpus.len()).map(Device::Gpu));
    let pos: Vec<&str> = order.split(',').filter(|k| !k.is_empty()).collect();
    let mut keyed: Vec<(Device, String)> = v.into_iter().map(|d| (d, device_key(d, s))).collect();
    keyed.sort_by_key(|(_, k)| pos.iter().position(|p| p == k).unwrap_or(usize::MAX));
    keyed
}

/// Move `keys[from]` so it lands before the item that was at `target` (or last if `target == len`).
fn move_item(mut keys: Vec<String>, from: usize, target: usize) -> Vec<String> {
    let item = keys.remove(from);
    let to = if target > from { target - 1 } else { target };
    keys.insert(to.min(keys.len()), item);
    keys
}

impl PerfState {
    pub fn ui(&mut self, ui: &mut egui::Ui, c: &mut Ctx, procs: &mut ProcState) {
        let s = c.snap.clone();
        // Clamp selection if a device disappeared.
        self.sel = match self.sel {
            Device::Disk(i) if i >= s.disks.len() => Device::Cpu,
            Device::Net(i) if i >= s.nets.len() => Device::Cpu,
            Device::Gpu(i) if i >= s.gpus.len() => Device::Cpu,
            d => d,
        };
        let panel = egui::Panel::left("perf_list")
            .resizable(true)
            .default_size(c.cfg.perf_sidebar)
            .size_range(170.0..=420.0)
            .show(ui, |ui| {
                // Don't shrink to the content, or the panel collapses to it and ignores its size.
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.sidebar(ui, &s, &mut c.cfg.perf_order));
            });
        // Remember the width once the user lets go (not on every frame of the drag).
        let w = panel.response.rect.width().round();
        if !ui.input(|i| i.pointer.any_down()) && (w - c.cfg.perf_sidebar).abs() >= 1.0 {
            c.cfg.perf_sidebar = w;
        }

        let window = window_label(c.cfg.interval_ms);
        let groups = c.groups;
        let mut act = None;
        egui::CentralPanel::default_margins().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                match self.sel {
                    Device::Cpu => cpu_view(ui, &s, c, &window),
                    Device::Mem => mem_view(ui, &s, &window),
                    Device::Disk(i) => disk_view(ui, &s, i, &window),
                    Device::Net(i) => net_view(ui, &s, i, &window),
                    Device::Gpu(i) => gpu_view(ui, &s, i, &window),
                }
                act = match self.sel {
                    Device::Cpu => top_list(ui, groups, &s, Metric::Cpu, CPU),
                    Device::Mem => top_list(ui, groups, &s, Metric::Mem, MEM),
                    Device::Disk(_) => top_list(ui, groups, &s, Metric::Disk, DISK),
                    Device::Gpu(_) => top_list(ui, groups, &s, Metric::Gpu, GPU),
                    Device::Net(_) => {
                        ui.add_space(14.0);
                        ui.label(
                            RichText::new(
                                "Per-app network usage isn't shown: Linux only exposes it per process \
                                 to root (via eBPF/netlink).",
                            )
                            .weak(),
                        );
                        None
                    }
                };
            });
        });
        match act {
            Some(TopAction::Show(name, pids)) => {
                procs.show_group(name, pids);
                *c.switch_to = Some(Tab::Processes);
            }
            Some(TopAction::Signal(name, pids, sig)) => signal_many(c, &name, &pids, sig),
            None => {}
        }
    }

    fn sidebar(&mut self, ui: &mut egui::Ui, s: &Snapshot, order: &mut String) {
        let devices = ordered_devices(s, order);
        let mut rects = Vec::with_capacity(devices.len());
        let mut reset = false;
        for (dev, key) in &devices {
            let r = self.item(ui, *dev, s);
            rects.push(r.rect);
            if r.clicked() {
                self.sel = *dev;
            }
            if r.drag_started() {
                self.drag = Some(key.clone());
            }
            r.on_hover_text("Drag to reorder").context_menu(|ui| {
                if ui.button("Reset order").clicked() {
                    reset = true;
                    ui.close();
                }
            });
        }
        if reset {
            order.clear();
        }

        // Drag-to-reorder: draw where the item would land, move it on release.
        let Some(dragged) = self.drag.clone() else { return };
        let Some(from) = devices.iter().position(|(_, k)| *k == dragged) else {
            self.drag = None;
            return;
        };
        let y = ui.input(|i| i.pointer.interact_pos()).map_or(0.0, |p| p.y);
        let target = rects.iter().filter(|r| r.center().y < y).count();
        let line_y =
            if target < rects.len() { rects[target].top() - 1.0 } else { rects[rects.len() - 1].bottom() + 1.0 };
        let x = rects[0].x_range();
        ui.painter().hline(x, line_y, egui::Stroke::new(2.0, ui.visuals().selection.stroke.color));
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        if ui.input(|i| i.pointer.any_released()) {
            let keys: Vec<String> = devices.into_iter().map(|(_, k)| k).collect();
            *order = move_item(keys, from, target).join(",");
            self.drag = None;
        }
    }

    /// One sidebar entry: sparkline + name + current value. Clickable and draggable.
    fn item(&self, ui: &mut egui::Ui, dev: Device, s: &Snapshot) -> egui::Response {
        let (title, sub, hist, max, col): (String, String, &History, f32, Color32) = match dev {
            Device::Cpu => {
                let cpu = &s.cpu;
                let mut sub = format!("{:.0}%", cpu.usage);
                if let Some(f) = cpu.freq_mhz {
                    sub += &format!("  {:.2} GHz", f / 1000.0);
                }
                if let Some(t) = cpu.temp_c {
                    sub += &format!("  {t:.0}°C");
                }
                ("CPU".into(), sub, &cpu.hist, 100.0, CPU)
            }
            Device::Mem => {
                let m = &s.mem;
                let sub = format!("{}/{} ({:.0}%)", fmt_bytes(m.used), fmt_bytes(m.total), pct(m.used, m.total));
                ("Memory".into(), sub, &m.hist, 100.0, MEM)
            }
            Device::Disk(i) => {
                let d = &s.disks[i];
                let sub = format!("{}  {:.0}%", if d.rotational { "HDD" } else { "SSD" }, d.busy);
                (format!("Disk {i} ({})", d.name), sub, &d.busy_hist, 100.0, DISK)
            }
            Device::Net(i) => {
                let n = &s.nets[i];
                let max = nice_max(n.rx_hist.max().max(n.tx_hist.max()));
                (n.name.clone(), format!("R {}  S {}", fmt_bits(n.rx_rate), fmt_bits(n.tx_rate)), &n.rx_hist, max, NET)
            }
            Device::Gpu(i) => {
                let g = &s.gpus[i];
                let mut sub = g.util.map_or("—".into(), |u| format!("{u:.0}%"));
                if let Some(t) = g.temp_c {
                    sub += &format!("  {t:.0}°C");
                }
                (format!("GPU {i}"), sub, &g.hist, 100.0, GPU)
            }
        };
        let selected = self.sel == dev;
        let frame = egui::Frame::new().inner_margin(6.0).corner_radius(4.0).fill(if selected {
            col.gamma_multiply(0.15)
        } else {
            Color32::TRANSPARENT
        });
        let r = frame
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    graph(ui, Vec2::new(64.0, 40.0), &[Series::new(hist, col)], max, false);
                    ui.vertical(|ui| {
                        ui.add(egui::Label::new(RichText::new(title).strong()).truncate());
                        ui.add(egui::Label::new(RichText::new(sub).small()).truncate());
                    });
                });
            })
            .response;
        r.interact(Sense::click_and_drag())
    }
}

fn header(ui: &mut egui::Ui, title: &str, subtitle: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).size(24.0).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add(egui::Label::new(RichText::new(subtitle).size(14.0)).truncate());
        });
    });
    ui.add_space(4.0);
}

fn caption(ui: &mut egui::Ui, left: &str, right: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(left).small().weak());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(right).small().weak());
        });
    });
}

fn stats(ui: &mut egui::Ui, id: &str, items: &[(&str, String)]) {
    ui.add_space(8.0);
    const CELL: f32 = 180.0;
    let gap = 20.0;
    let cols = (((ui.available_width() + gap) / (CELL + gap)).floor() as usize).clamp(1, 5);
    egui::Grid::new(id).num_columns(cols).min_col_width(CELL).spacing([gap, 10.0]).show(ui, |ui| {
        for (i, (k, v)) in items.iter().enumerate() {
            ui.vertical(|ui| {
                ui.set_min_width(CELL);
                ui.add(egui::Label::new(RichText::new(*k).small().weak()).extend());
                ui.add(egui::Label::new(RichText::new(v).size(17.0)).extend());
            });
            if (i + 1) % cols == 0 {
                ui.end_row();
            }
        }
    });
}

fn window_label(interval_ms: u64) -> String {
    let secs = interval_ms * crate::sampler::HISTORY as u64 / 1000;
    if secs >= 120 && secs % 60 == 0 { format!("{} minutes", secs / 60) } else { format!("{secs} seconds") }
}

fn cpu_view(ui: &mut egui::Ui, s: &Snapshot, c: &mut Ctx, window: &str) {
    let cpu = &s.cpu;
    header(ui, "CPU", &cpu.model);
    ui.horizontal(|ui| {
        ui.label("Graph:");
        ui.selectable_value(&mut c.cfg.cpu_per_core_graphs, false, "Overall");
        ui.selectable_value(&mut c.cfg.cpu_per_core_graphs, true, "Logical processors");
    });
    caption(ui, &format!("% Utilization over {}", window), "100%");
    let w = ui.available_width();
    if c.cfg.cpu_per_core_graphs && !cpu.core_hist.is_empty() {
        let n = cpu.core_hist.len();
        let cols = ((n as f32).sqrt() * 1.5).ceil().clamp(1.0, 16.0) as usize;
        let rows = n.div_ceil(cols);
        let gap = 4.0;
        let cw = (w - gap * (cols - 1) as f32) / cols as f32;
        let ch = (cw * 0.6).clamp(36.0, 120.0);
        for r in 0..rows {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for i in r * cols..((r + 1) * cols).min(n) {
                    graph(ui, Vec2::new(cw, ch), &[Series::new(&cpu.core_hist[i], CPU)], 100.0, true)
                        .on_hover_text(format!("CPU {i}: {:.0}%", cpu.per_core[i]));
                }
            });
        }
    } else {
        let h = (w * 0.42).clamp(160.0, 360.0);
        graph(ui, Vec2::new(w, h), &[Series::new(&cpu.hist, CPU)], 100.0, true);
    }
    caption(ui, "", "0");
    let mut items = vec![
        ("Utilization", format!("{:.0}%", cpu.usage)),
        ("User / System", format!("{:.0}% / {:.0}%", cpu.user, cpu.system)),
        ("I/O wait", format!("{:.1}%", cpu.iowait)),
    ];
    if let Some(f) = cpu.freq_mhz {
        items.push(("Speed (avg)", format!("{:.2} GHz", f / 1000.0)));
    }
    if let Some(t) = cpu.temp_c {
        items.push(("Temperature", format!("{t:.1} °C")));
    }
    items.extend([
        ("Processes", s.procs.len().to_string()),
        ("Threads", s.threads.to_string()),
        ("Running", s.running.to_string()),
        ("Load average", format!("{:.2}  {:.2}  {:.2}", s.load[0], s.load[1], s.load[2])),
        ("Context switches", format!("{:.0}/s", cpu.ctxt_rate)),
        ("Cores / Logical", format!("{} / {}", cpu.physical, cpu.logical)),
        ("Up time", fmt_duration(s.uptime as u64)),
    ]);
    stats(ui, "cpu_stats", &items);
}

fn mem_view(ui: &mut egui::Ui, s: &Snapshot, window: &str) {
    let m = &s.mem;
    header(ui, "Memory", &format!("{} total", fmt_bytes(m.total)));
    caption(ui, &format!("Memory usage over {}", window), &fmt_bytes(m.total));
    let w = ui.available_width();
    let h = (w * 0.34).clamp(140.0, 300.0);
    graph(ui, Vec2::new(w, h), &[Series::new(&m.hist, MEM)], 100.0, true);
    ui.add_space(8.0);
    caption(ui, "Memory composition", "");
    let free = m.total.saturating_sub(m.used + m.cached + m.buffers);
    graph::bar(
        ui,
        w,
        &[(m.used as f32, MEM.gamma_multiply(0.85)), ((m.cached + m.buffers) as f32, MEM.gamma_multiply(0.35))],
        m.total as f32,
        MEM,
    );
    caption(ui, "■ In use   ■ Cache / buffers   □ Free", &fmt_bytes(free));
    if m.swap_total > 0 {
        ui.add_space(8.0);
        caption(ui, "Swap", &fmt_bytes(m.swap_total));
        graph(ui, Vec2::new(w, 70.0), &[Series::new(&m.swap_hist, MEM)], 100.0, true);
    }
    stats(
        ui,
        "mem_stats",
        &[
            ("In use", format!("{} ({:.0}%)", fmt_bytes(m.used), pct(m.used, m.total))),
            ("Available", fmt_bytes(m.available)),
            ("Cached", fmt_bytes(m.cached)),
            ("Buffers", fmt_bytes(m.buffers)),
            ("Shared", fmt_bytes(m.shmem)),
            ("Dirty", fmt_bytes(m.dirty)),
            ("Swap used", format!("{} / {}", fmt_bytes(m.swap_used), fmt_bytes(m.swap_total))),
        ],
    );
}

fn disk_view(ui: &mut egui::Ui, s: &Snapshot, i: usize, window: &str) {
    let d = &s.disks[i];
    header(ui, &format!("Disk {i} ({})", d.name), &d.model);
    let w = ui.available_width();
    caption(ui, &format!("Active time over {}", window), "100%");
    graph(ui, Vec2::new(w, (w * 0.25).clamp(110.0, 220.0)), &[Series::new(&d.busy_hist, DISK)], 100.0, true);
    ui.add_space(8.0);
    let max = nice_max(d.read_hist.max().max(d.write_hist.max()));
    caption(ui, "Transfer rate (solid: read, dashed: write)", &fmt_rate(max as f64));
    graph(
        ui,
        Vec2::new(w, (w * 0.18).clamp(80.0, 160.0)),
        &[Series::new(&d.read_hist, DISK), Series::new(&d.write_hist, DISK.gamma_multiply(0.9)).dashed()],
        max,
        true,
    );
    stats(
        ui,
        "disk_stats",
        &[
            ("Active time", format!("{:.0}%", d.busy)),
            ("Read speed", fmt_rate(d.read_rate)),
            ("Write speed", fmt_rate(d.write_rate)),
            ("Capacity", fmt_bytes(d.size)),
            ("Type", if d.rotational { "HDD".into() } else { "SSD".into() }),
            ("Device", format!("/dev/{}", d.name)),
        ],
    );
}

fn net_view(ui: &mut egui::Ui, s: &Snapshot, i: usize, window: &str) {
    let n = &s.nets[i];
    let link = n.speed_mbps.map_or(String::new(), |m| format!("Link speed {m} Mbps"));
    header(ui, &n.name, &link);
    let w = ui.available_width();
    let max = nice_max(n.rx_hist.max().max(n.tx_hist.max()));
    caption(ui, &format!("Throughput over {} (solid: receive, dashed: send)", window), &fmt_bits(max as f64));
    graph(
        ui,
        Vec2::new(w, (w * 0.4).clamp(150.0, 340.0)),
        &[Series::new(&n.rx_hist, NET), Series::new(&n.tx_hist, NET).dashed()],
        max,
        true,
    );
    stats(
        ui,
        "net_stats",
        &[
            ("Receive", format!("{} ({})", fmt_bits(n.rx_rate), fmt_rate(n.rx_rate))),
            ("Send", format!("{} ({})", fmt_bits(n.tx_rate), fmt_rate(n.tx_rate))),
            ("Total received", fmt_bytes(n.rx_total)),
            ("Total sent", fmt_bytes(n.tx_total)),
        ],
    );
}

fn gpu_view(ui: &mut egui::Ui, s: &Snapshot, i: usize, window: &str) {
    let g = &s.gpus[i];
    header(ui, &format!("GPU {i}"), &g.name);
    let w = ui.available_width();
    caption(ui, &format!("Utilization over {}", window), "100%");
    graph(ui, Vec2::new(w, (w * 0.4).clamp(150.0, 340.0)), &[Series::new(&g.hist, GPU)], 100.0, true);
    let mut items = vec![("Utilization", g.util.map_or("—".into(), |u| format!("{u:.0}%")))];
    if let (Some(u), Some(t)) = (g.mem_used, g.mem_total) {
        items.push(("Dedicated memory", format!("{} / {}", fmt_bytes(u), fmt_bytes(t))));
    }
    if let Some(m) = g.mem_util {
        items.push(("Memory controller", format!("{m:.0}%")));
    }
    if let Some(t) = g.temp_c {
        items.push(("Temperature", format!("{t:.0} °C")));
    }
    if let Some(p) = g.power_w {
        items.push(("Power", format!("{p:.1} W")));
    }
    items.push(("Vendor", g.vendor.to_string()));
    stats(ui, "gpu_stats", &items);
}

#[derive(Clone, Copy)]
enum Metric {
    Cpu,
    Mem,
    Gpu,
    Disk,
}

/// "Top 10 apps & services" by one resource, below the device graphs.
/// What the user asked for on a "top" row.
pub enum TopAction {
    Show(String, HashSet<u32>),
    Signal(String, Vec<u32>, Signal),
}

fn top_list(ui: &mut egui::Ui, groups: &[Group], snap: &Snapshot, m: Metric, color: Color32) -> Option<TopAction> {
    let val = |g: &Group| -> f64 {
        match m {
            Metric::Cpu => g.cpu as f64,
            Metric::Mem => g.mem as f64,
            Metric::Gpu => g.gpu as f64,
            Metric::Disk => g.read + g.write,
        }
    };
    let fmt = |v: f64| -> String {
        match m {
            Metric::Cpu => format!("{v:.1}%"),
            Metric::Mem => fmt_bytes(v as u64),
            Metric::Gpu => format!("{v:.0}%"),
            Metric::Disk => fmt_rate(v),
        }
    };
    let title = match m {
        Metric::Cpu => "Top apps & services by CPU",
        Metric::Mem => "Top apps & services by memory",
        Metric::Gpu => "Top apps & services by GPU",
        Metric::Disk => "Top apps & services by disk I/O (all disks)",
    };
    let mut top: Vec<&Group> = groups.iter().filter(|g| val(g) > 0.0).collect();
    top.sort_by(|a, b| val(b).total_cmp(&val(a)));
    top.truncate(10);

    ui.add_space(16.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).strong().size(15.0));
        ui.label(RichText::new("double-click to show in Processes · right-click for actions").small().weak());
    });
    ui.add_space(4.0);
    if top.is_empty() {
        ui.label(RichText::new("Nothing is using this right now.").weak());
        return None;
    }
    let max = val(top[0]).max(f64::MIN_POSITIVE);
    let pids = |g: &Group| -> Vec<u32> { g.members.iter().map(|&i| snap.procs[i as usize].pid).collect() };
    let mut act = None;
    let (strong, weak) = (ui.visuals().strong_text_color(), ui.visuals().weak_text_color());
    let stripe = ui.visuals().faint_bg_color;
    let hover = ui.visuals().widgets.hovered.weak_bg_fill;
    for (i, g) in top.iter().enumerate() {
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), Sense::click());
        if !ui.is_rect_visible(rect) {
            continue;
        }
        let p = ui.painter();
        if resp.hovered() {
            p.rect_filled(rect, 3.0, hover);
        } else if i % 2 == 1 {
            p.rect_filled(rect, 0.0, stripe);
        }
        let cy = rect.center().y;
        let font = egui::FontId::proportional(14.0);
        p.text(egui::pos2(rect.left() + 4.0, cy), egui::Align2::LEFT_CENTER, format!("{}", i + 1), font.clone(), weak);
        // Name (clipped to its column) + kind tag.
        let name_w = (rect.width() * 0.38).clamp(160.0, 340.0);
        let name_rect = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 30.0, rect.top()),
            egui::pos2(rect.left() + 30.0 + name_w, rect.bottom()),
        );
        let clipped = p.with_clip_rect(name_rect);
        let r =
            clipped.text(egui::pos2(name_rect.left(), cy), egui::Align2::LEFT_CENTER, &g.name, font.clone(), strong);
        clipped.text(
            egui::pos2(r.right() + 6.0, cy + 1.0),
            egui::Align2::LEFT_CENTER,
            g.kind.label(),
            egui::FontId::proportional(10.5),
            weak,
        );
        // Bar + value.
        let value_w = 90.0;
        let bar_left = name_rect.right() + 10.0;
        let bar_right = (rect.right() - value_w - 10.0).max(bar_left + 40.0);
        let bar = egui::Rect::from_min_max(egui::pos2(bar_left, cy - 5.0), egui::pos2(bar_right, cy + 5.0));
        p.rect_filled(bar, 2.0, color.gamma_multiply(0.12));
        let w = bar.width() * (val(g) / max).clamp(0.0, 1.0) as f32;
        p.rect_filled(egui::Rect::from_min_size(bar.min, Vec2::new(w, bar.height())), 2.0, color);
        p.text(egui::pos2(rect.right() - 4.0, cy), egui::Align2::RIGHT_CENTER, fmt(val(g)), font, strong);

        let resp = resp.on_hover_text(format!("{} · {} process(es)", g.kind.label(), g.members.len()));
        if resp.double_clicked() {
            act = Some(TopAction::Show(g.name.clone(), pids(g).into_iter().collect()));
        }
        resp.context_menu(|ui| {
            ui.label(RichText::new(&g.name).strong());
            ui.label(RichText::new(format!("{} · {} process(es)", g.kind.label(), g.members.len())).small().weak());
            ui.separator();
            if ui.button("Show in Processes").clicked() {
                act = Some(TopAction::Show(g.name.clone(), pids(g).into_iter().collect()));
                ui.close();
            }
            if ui.button("End task").clicked() {
                act = Some(TopAction::Signal(g.name.clone(), pids(g), Signal::Term));
                ui.close();
            }
            if ui.button("Kill").clicked() {
                act = Some(TopAction::Signal(g.name.clone(), pids(g), Signal::Kill));
                ui.close();
            }
        });
    }
    if matches!(m, Metric::Disk) {
        ui.label(
            RichText::new(
                "Disk I/O of other users' processes (most system services) is only visible when running as root.",
            )
            .small()
            .weak(),
        );
    }
    act
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sampler::{DiskInfo, NetInfo};

    fn keys(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reorder() {
        let k = keys(&["cpu", "mem", "disk:a", "gpu:0"]);
        assert_eq!(move_item(k.clone(), 3, 0), keys(&["gpu:0", "cpu", "mem", "disk:a"]));
        assert_eq!(move_item(k.clone(), 0, 4), keys(&["mem", "disk:a", "gpu:0", "cpu"]));
        assert_eq!(move_item(k.clone(), 1, 3), keys(&["cpu", "disk:a", "mem", "gpu:0"]));
        assert_eq!(move_item(k.clone(), 2, 2), k);
        assert_eq!(move_item(k.clone(), 2, 3), k);
    }

    #[test]
    fn saved_order() {
        let mut disk = DiskInfo::default();
        disk.name = "nvme0n1".into();
        let mut net = NetInfo::default();
        net.name = "enp6s0".into();
        let s = Snapshot { disks: vec![disk], nets: vec![net], ..Default::default() };
        let order = |o: &str| ordered_devices(&s, o).into_iter().map(|(_, k)| k).collect::<Vec<_>>();
        assert_eq!(order(""), keys(&["cpu", "mem", "disk:nvme0n1", "net:enp6s0"]));
        // Saved keys first in saved order; unknown/stale keys ignored; unsaved devices keep default order after.
        assert_eq!(order("net:enp6s0,gone:x,cpu"), keys(&["net:enp6s0", "cpu", "mem", "disk:nvme0n1"]));
    }
}
