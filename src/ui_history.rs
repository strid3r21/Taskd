//! Recorded history in the Performance tab: Live / 1 h / 6 h / 24 h / 7 d.

use std::collections::HashMap;
use std::time::Instant;

use eframe::egui::{self, Color32, RichText, Vec2};
use serde_json::Value;

use crate::app::{Ctx, JobResult};
use crate::graph::graph_series;
use crate::recorder;
use crate::util::{fmt_bytes, fmt_rate};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Range {
    #[default]
    Live,
    H1,
    H6,
    H24,
    D7,
}

impl Range {
    const ALL: [Range; 5] = [Range::Live, Range::H1, Range::H6, Range::H24, Range::D7];
    fn label(self) -> &'static str {
        match self {
            Range::Live => "Live",
            Range::H1 => "1 h",
            Range::H6 => "6 h",
            Range::H24 => "24 h",
            Range::D7 => "7 days",
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            Range::Live => "live",
            Range::H1 => "1h",
            Range::H6 => "6h",
            Range::H24 => "24h",
            Range::D7 => "7d",
        }
    }
    pub fn from_key(k: &str) -> Range {
        Range::ALL.into_iter().find(|r| r.key() == k).unwrap_or_default()
    }
    pub fn secs(self) -> u64 {
        match self {
            Range::Live => 0,
            Range::H1 => 3600,
            Range::H6 => 6 * 3600,
            Range::H24 => 86_400,
            Range::D7 => 7 * 86_400,
        }
    }
}

/// Which recorded series to show (mirrors the Performance devices).
pub enum Source<'a> {
    Cpu,
    Mem,
    Disk(&'a str),
    Net(&'a str),
    Gpu(usize),
    Battery,
}

#[derive(Default)]
pub struct HistoryView {
    pub range: Range,
    records: Vec<Value>,
    loaded: Option<(Range, Instant)>,
    loading: bool,
}

const MAX_POINTS: usize = 360;

impl HistoryView {
    pub fn loaded(&mut self, range: Range, records: Vec<Value>) {
        self.loading = false;
        if range == self.range {
            self.records = records;
            self.loaded = Some((range, Instant::now()));
        }
    }

    /// The Live / 1 h / … selector (remembered in the config). Returns true when showing history.
    pub fn selector(&mut self, ui: &mut egui::Ui, saved: &mut String) -> bool {
        self.range = Range::from_key(saved);
        ui.horizontal(|ui| {
            for r in Range::ALL {
                if ui.selectable_label(self.range == r, r.label()).clicked() {
                    self.range = r;
                    *saved = r.key().into();
                }
            }
        });
        self.range != Range::Live
    }

    fn ensure_loaded(&mut self, c: &Ctx) {
        let fresh = self.loaded.is_some_and(|(r, t)| r == self.range && t.elapsed().as_secs() < 60);
        if fresh || self.loading {
            return;
        }
        self.loading = true;
        let range = self.range;
        c.jobs.spawn(move || {
            let since = recorder::epoch().saturating_sub(range.secs());
            JobResult::History(range, recorder::load(since))
        });
    }

    pub fn show(&mut self, ui: &mut egui::Ui, c: &mut Ctx, src: Source, title: &str, color: Color32) {
        self.ensure_loaded(c);
        ui.horizontal(|ui| {
            ui.label(RichText::new(title).size(24.0).strong());
            if self.loading {
                ui.spinner();
            }
        });
        if self.records.is_empty() {
            ui.add_space(12.0);
            if self.loading {
                return;
            }
            ui.label(if c.cfg.record {
                "No history recorded for this period yet. The background recorder saves a sample every few seconds."
            } else {
                "History is recorded by the background recorder, which is turned off."
            });
            if !c.cfg.record && ui.button("Turn on background recording").clicked() {
                c.cfg.record = true;
            }
            return;
        }
        // Pull the series for this device out of the records, then bucket-average them.
        let (series, unit, dashed2): (Vec<Vec<f32>>, &str, bool) = match src {
            Source::Cpu => (vec![self.pick(|r| r["cpu"].as_f64())], "%", false),
            Source::Mem => (vec![self.pick(|r| r["mem"].as_f64())], "%", false),
            Source::Disk(name) => (vec![self.pick(|r| r["disk"][name][0].as_f64())], "%", false),
            Source::Net(name) => (
                vec![self.pick(|r| r["net"][name][0].as_f64()), self.pick(|r| r["net"][name][1].as_f64())],
                "B/s",
                true,
            ),
            Source::Gpu(i) => (vec![self.pick(|r| r["gpu"][i][0].as_f64())], "%", false),
            Source::Battery => (vec![self.pick(|r| r["bat"][0].as_f64())], "%", false),
        };
        let times: Vec<u64> = self.records.iter().map(|r| r["t"].as_u64().unwrap_or(0)).collect();
        let buckets = times.len().clamp(1, MAX_POINTS);
        let down = |v: &[f32]| -> Vec<f32> {
            (0..buckets)
                .map(|b| {
                    let (lo, hi) =
                        (b * v.len() / buckets, ((b + 1) * v.len() / buckets).max(b * v.len() / buckets + 1));
                    v[lo..hi.min(v.len())].iter().sum::<f32>() / (hi.min(v.len()) - lo).max(1) as f32
                })
                .collect()
        };
        let ds: Vec<Vec<f32>> = series.iter().map(|s| down(s)).collect();
        let max = if unit == "%" { 100.0 } else { ds.iter().flatten().copied().fold(1.0, f32::max) * 1.15 };
        let span = |t: u64| fmt_time(t, self.range.secs() > 86_400);
        ui.horizontal(|ui| {
            ui.label(RichText::new(span(*times.first().unwrap_or(&0))).small().weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let top = if unit == "%" { "100%".to_string() } else { fmt_rate(max as f64) };
                ui.label(RichText::new(format!("{top}   now")).small().weak());
            });
        });
        let w = ui.available_width();
        let sets: Vec<(&[f32], Color32, bool)> =
            ds.iter().enumerate().map(|(i, s)| (s.as_slice(), color, dashed2 && i == 1)).collect();
        let (resp, hover) = graph_series(ui, Vec2::new(w, (w * 0.4).clamp(160.0, 360.0)), &sets, max);
        if unit == "B/s" {
            ui.label(RichText::new("solid: receive · dashed: send").small().weak());
        }
        // Tooltip: time, value, and what was running then.
        if let Some(b) = hover {
            let i = (b * times.len() / buckets).min(times.len() - 1);
            let rec = &self.records[i];
            let value = ds.iter().map(|s| if unit == "%" { format!("{:.0}%", s[b]) } else { fmt_rate(s[b] as f64) });
            let value = value.collect::<Vec<_>>().join(" / ");
            resp.on_hover_ui_at_pointer(|ui| {
                ui.label(RichText::new(format!("{}  ·  {value}", fmt_time(times[i], true))).strong());
                for e in rec["top"].as_array().into_iter().flatten().take(6) {
                    ui.label(format!(
                        "{}  {:.0}% CPU  {}",
                        e[0].as_str().unwrap_or("?"),
                        e[2].as_f64().unwrap_or(0.0),
                        fmt_bytes(e[3].as_u64().unwrap_or(0) * 1_048_576)
                    ));
                }
            });
        }
        let s0 = &series[0];
        let (mn, mx) = s0.iter().fold((f32::MAX, f32::MIN), |(a, b), &x| (a.min(x), b.max(x)));
        let avg = s0.iter().sum::<f32>() / s0.len().max(1) as f32;
        let f = |v: f32| if unit == "%" { format!("{v:.0}%") } else { fmt_rate(v as f64) };
        ui.label(format!("Average {}  ·  peak {}  ·  low {}  ·  {} samples", f(avg), f(mx), f(mn), s0.len()));
        self.top_during(ui, &src);
    }

    fn pick(&self, f: impl Fn(&Value) -> Option<f64>) -> Vec<f32> {
        self.records.iter().map(|r| f(r).unwrap_or(0.0) as f32).collect()
    }

    /// The apps/services that were heaviest over the period (from each record's top list).
    fn top_during(&self, ui: &mut egui::Ui, src: &Source) {
        let by_mem = matches!(src, Source::Mem);
        let by_gpu = matches!(src, Source::Gpu(_));
        let n = self.records.len().max(1) as f64;
        let mut acc: HashMap<String, (String, f64, u64, f64)> = HashMap::new(); // kind, cpu sum, max mem, gpu sum
        for r in &self.records {
            for e in r["top"].as_array().into_iter().flatten() {
                let name = e[0].as_str().unwrap_or("?").to_string();
                let a = acc.entry(name).or_insert((e[1].as_str().unwrap_or("").to_string(), 0.0, 0, 0.0));
                a.1 += e[2].as_f64().unwrap_or(0.0);
                a.2 = a.2.max(e[3].as_u64().unwrap_or(0));
                a.3 += e[4].as_f64().unwrap_or(0.0);
            }
        }
        let mut v: Vec<(String, (String, f64, u64, f64))> = acc.into_iter().collect();
        if by_mem {
            v.sort_by_key(|x| std::cmp::Reverse(x.1.2));
        } else if by_gpu {
            v.sort_by(|a, b| b.1.3.total_cmp(&a.1.3));
        } else {
            v.sort_by(|a, b| b.1.1.total_cmp(&a.1.1));
        }
        ui.add_space(14.0);
        ui.label(RichText::new("Heaviest during this period").strong().size(15.0));
        egui::Grid::new("hist_top").num_columns(4).striped(true).spacing([16.0, 4.0]).show(ui, |ui| {
            ui.label(RichText::new("Name").weak());
            ui.label(RichText::new("Avg CPU").weak());
            ui.label(RichText::new("Peak memory").weak());
            ui.label(RichText::new("Avg GPU").weak());
            ui.end_row();
            for (name, (kind, cpu, mem, gpu)) in v.iter().take(10) {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(name).strong());
                    ui.label(RichText::new(kind).small().weak());
                });
                ui.label(format!("{:.1}%", cpu / n));
                ui.label(fmt_bytes(mem * 1_048_576));
                ui.label(format!("{:.0}%", gpu / n));
                ui.end_row();
            }
        });
    }
}

/// Local time, "14:05" or "Sat 14:05".
pub fn fmt_time(t: u64, with_day: bool) -> String {
    let tt = t as libc::time_t;
    // SAFETY: localtime_r writes into the zeroed struct we pass.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&tt, &mut tm);
        tm
    };
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    if with_day {
        format!("{} {:02}:{:02}", DAYS[tm.tm_wday.clamp(0, 6) as usize], tm.tm_hour, tm.tm_min)
    } else {
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    }
}
