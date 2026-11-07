//! Performance tab: device list with sparklines on the left, detail graphs on the right.

use eframe::egui::{self, Color32, RichText, Sense, Vec2};

use crate::app::Ctx;
use crate::graph::{self, CPU, DISK, GPU, MEM, NET, Series, graph};
use crate::groups::Group;
use crate::sampler::{History, Snapshot};
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

impl PerfState {
    pub fn ui(&mut self, ui: &mut egui::Ui, c: &mut Ctx) {
        let s = c.snap.clone();
        // Clamp selection if a device disappeared.
        self.sel = match self.sel {
            Device::Disk(i) if i >= s.disks.len() => Device::Cpu,
            Device::Net(i) if i >= s.nets.len() => Device::Cpu,
            Device::Gpu(i) if i >= s.gpus.len() => Device::Cpu,
            d => d,
        };
        egui::Panel::left("perf_list").resizable(false).exact_size(230.0).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.sidebar(ui, &s));
        });
        let window = window_label(c.cfg.interval_ms);
        let groups = c.groups;
        egui::CentralPanel::default_margins().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                match self.sel {
                    Device::Cpu => cpu_view(ui, &s, c, &window),
                    Device::Mem => mem_view(ui, &s, &window),
                    Device::Disk(i) => disk_view(ui, &s, i, &window),
                    Device::Net(i) => net_view(ui, &s, i, &window),
                    Device::Gpu(i) => gpu_view(ui, &s, i, &window),
                }
                match self.sel {
                    Device::Cpu => top_list(ui, groups, Metric::Cpu, CPU),
                    Device::Mem => top_list(ui, groups, Metric::Mem, MEM),
                    Device::Disk(_) => top_list(ui, groups, Metric::Disk, DISK),
                    Device::Gpu(_) => top_list(ui, groups, Metric::Gpu, GPU),
                    Device::Net(_) => {
                        ui.add_space(14.0);
                        ui.label(
                            RichText::new(
                                "Per-app network usage isn't shown: Linux only exposes it per process \
                                 to root (via eBPF/netlink).",
                            )
                            .weak(),
                        );
                    }
                }
            });
        });
    }

    fn sidebar(&mut self, ui: &mut egui::Ui, s: &Snapshot) {
        let mut item =
            |ui: &mut egui::Ui, dev: Device, title: &str, sub: String, h: &History, max: f32, col: Color32| {
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
                            graph(ui, Vec2::new(64.0, 40.0), &[Series::new(h, col)], max, false);
                            ui.vertical(|ui| {
                                ui.label(RichText::new(title).strong());
                                ui.label(RichText::new(sub).small());
                            });
                        });
                    })
                    .response
                    .interact(Sense::click());
                if r.clicked() {
                    self.sel = dev;
                }
            };
        let cpu = &s.cpu;
        let mut sub = format!("{:.0}%", cpu.usage);
        if let Some(f) = cpu.freq_mhz {
            sub += &format!("  {:.2} GHz", f / 1000.0);
        }
        if let Some(t) = cpu.temp_c {
            sub += &format!("  {t:.0}°C");
        }
        item(ui, Device::Cpu, "CPU", sub, &cpu.hist, 100.0, CPU);
        let m = &s.mem;
        item(
            ui,
            Device::Mem,
            "Memory",
            format!("{}/{} ({:.0}%)", fmt_bytes(m.used), fmt_bytes(m.total), pct(m.used, m.total)),
            &m.hist,
            100.0,
            MEM,
        );
        for (i, d) in s.disks.iter().enumerate() {
            item(
                ui,
                Device::Disk(i),
                &format!("Disk {i} ({})", d.name),
                format!("{}  {:.0}%", if d.rotational { "HDD" } else { "SSD" }, d.busy),
                &d.busy_hist,
                100.0,
                DISK,
            );
        }
        for (i, n) in s.nets.iter().enumerate() {
            let max = nice_max(n.rx_hist.max().max(n.tx_hist.max()));
            item(
                ui,
                Device::Net(i),
                &n.name,
                format!("R {}  S {}", fmt_bits(n.rx_rate), fmt_bits(n.tx_rate)),
                &n.rx_hist,
                max,
                NET,
            );
        }
        for (i, g) in s.gpus.iter().enumerate() {
            let mut sub = g.util.map_or("—".into(), |u| format!("{u:.0}%"));
            if let Some(t) = g.temp_c {
                sub += &format!("  {t:.0}°C");
            }
            item(ui, Device::Gpu(i), &format!("GPU {i}"), sub, &g.hist, 100.0, GPU);
        }
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
fn top_list(ui: &mut egui::Ui, groups: &[Group], m: Metric, color: Color32) {
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
    ui.label(RichText::new(title).strong().size(15.0));
    ui.add_space(4.0);
    if top.is_empty() {
        ui.label(RichText::new("Nothing is using this right now.").weak());
        return;
    }
    let max = val(top[0]).max(f64::MIN_POSITIVE);
    let bar_w = (ui.available_width() - 420.0).clamp(80.0, 320.0);
    egui::Grid::new(("top", title)).num_columns(4).spacing([12.0, 5.0]).striped(true).show(ui, |ui| {
        for (i, g) in top.iter().enumerate() {
            ui.label(RichText::new(format!("{}", i + 1)).weak());
            ui.horizontal(|ui| {
                ui.set_min_width(240.0);
                let r = ui.add(egui::Label::new(RichText::new(&g.name).strong()).truncate());
                r.on_hover_text(format!("{} · {} process(es)", g.kind.label(), g.members.len()));
                ui.label(RichText::new(g.kind.label()).small().weak());
            });
            let (rect, _) = ui.allocate_exact_size(Vec2::new(bar_w, 10.0), Sense::hover());
            let p = ui.painter();
            p.rect_filled(rect, 2.0, color.gamma_multiply(0.12));
            let w = rect.width() * (val(g) / max).clamp(0.0, 1.0) as f32;
            p.rect_filled(egui::Rect::from_min_size(rect.min, Vec2::new(w, rect.height())), 2.0, color);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.set_min_width(80.0);
                ui.label(fmt(val(g)));
            });
            ui.end_row();
        }
    });
    if matches!(m, Metric::Disk) {
        ui.label(
            RichText::new(
                "Disk I/O of other users' processes (most system services) is only visible when running as root.",
            )
            .small()
            .weak(),
        );
    }
}
