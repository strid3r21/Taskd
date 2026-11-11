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

/// Which per-app number in a recorded `top` entry belongs to a graph.
/// Entry: [name, kind, cpu %, memory MiB, gpu %, disk I/O B/s, network B/s].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Res {
    Cpu,
    Mem,
    Gpu,
    Io,
    Net,
}

impl Res {
    const ALL: [Res; 5] = [Res::Cpu, Res::Mem, Res::Gpu, Res::Io, Res::Net];
    fn of(src: &Source) -> Res {
        match src {
            // No per-app power figure exists; CPU is what drains a battery.
            Source::Cpu | Source::Battery => Res::Cpu,
            Source::Mem => Res::Mem,
            Source::Gpu(_) => Res::Gpu,
            Source::Disk(_) => Res::Io,
            Source::Net(_) => Res::Net,
        }
    }
    fn idx(self) -> usize {
        match self {
            Res::Cpu => 2,
            Res::Mem => 3,
            Res::Gpu => 4,
            Res::Io => 5,
            Res::Net => 6,
        }
    }
    /// This resource's value in a recorded entry (None in records from before it was saved).
    fn get(self, e: &Value) -> Option<f64> {
        e.get(self.idx()).and_then(Value::as_f64)
    }
    fn fmt(self, v: f64) -> String {
        match self {
            Res::Cpu => format!("{v:.0}% CPU"),
            Res::Mem => fmt_bytes((v * 1_048_576.0) as u64),
            Res::Gpu => format!("{v:.0}% GPU"),
            Res::Io => format!("{} disk", fmt_rate(v)),
            Res::Net => format!("{} network", fmt_rate(v)),
        }
    }
    fn column(self) -> &'static str {
        match self {
            Res::Cpu => "Avg CPU",
            Res::Mem => "Peak memory",
            Res::Gpu => "Avg GPU",
            Res::Io => "Avg disk I/O",
            Res::Net => "Avg network",
        }
    }
    fn cell(self, v: f64) -> String {
        match self {
            Res::Cpu => format!("{v:.1}%"),
            Res::Mem => fmt_bytes((v * 1_048_576.0) as u64),
            Res::Gpu => format!("{v:.0}%"),
            Res::Io | Res::Net => fmt_rate(v),
        }
    }
    /// Shown when no app has a value for this resource in a sample.
    fn none_text(self, recorded: bool) -> &'static str {
        match self {
            _ if !recorded => "Per-app use of this wasn't recorded at this time (older Taskd version).",
            Res::Net => "No per-app network use recorded. It needs the helper: ⚙ → Per-app network.",
            _ => "No app was using this noticeably.",
        }
    }
}

/// Apps in one record, heaviest first by `res`, with their value (zeros left out).
fn top_by(rec: &Value, res: Res) -> Vec<(&str, f64)> {
    let mut v: Vec<(&str, f64)> = rec["top"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| Some((e[0].as_str().unwrap_or("?"), res.get(e)?)))
        .filter(|(_, x)| *x > 0.0)
        .collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1));
    v
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
            let res = Res::of(&src);
            let apps = top_by(rec, res);
            let recorded = rec["top"].as_array().into_iter().flatten().any(|e| res.get(e).is_some());
            resp.on_hover_ui_at_pointer(|ui| {
                ui.label(RichText::new(format!("{}  ·  {value}", fmt_time(times[i], true))).strong());
                if apps.is_empty() {
                    ui.label(RichText::new(res.none_text(recorded)).weak());
                }
                egui::Grid::new("hist_hover").num_columns(2).spacing([16.0, 2.0]).show(ui, |ui| {
                    for (name, v) in apps.iter().take(6) {
                        ui.label(*name);
                        ui.label(res.fmt(*v));
                        ui.end_row();
                    }
                });
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

    /// The apps/services that were heaviest over the period (from each record's top list),
    /// ranked by the resource this graph shows.
    fn top_during(&self, ui: &mut egui::Ui, src: &Source) {
        let res = Res::of(src);
        let n = self.records.len().max(1) as f64;
        // Per name: kind, then per resource the sum (or, for memory, the peak).
        let mut acc: HashMap<&str, (&str, [f64; 5])> = HashMap::new();
        for r in &self.records {
            for e in r["top"].as_array().into_iter().flatten() {
                let a = acc.entry(e[0].as_str().unwrap_or("?")).or_insert((e[1].as_str().unwrap_or(""), [0.0; 5]));
                for (k, m) in Res::ALL.into_iter().enumerate() {
                    let x = m.get(e).unwrap_or(0.0);
                    a.1[k] = if m == Res::Mem { a.1[k].max(x) } else { a.1[k] + x };
                }
            }
        }
        let value = |vals: &[f64; 5], m: Res| {
            let k = Res::ALL.iter().position(|x| *x == m).unwrap_or(0);
            if m == Res::Mem { vals[k] } else { vals[k] / n }
        };
        let mut v: Vec<(&str, (&str, [f64; 5]))> = acc.into_iter().filter(|(_, (_, x))| value(x, res) > 0.0).collect();
        v.sort_by(|a, b| value(&b.1.1, res).total_cmp(&value(&a.1.1, res)));
        // The graph's own resource first, then CPU and memory for context.
        let mut cols = vec![res];
        cols.extend([Res::Cpu, Res::Mem].into_iter().filter(|m| *m != res));
        ui.add_space(14.0);
        ui.label(RichText::new("Heaviest during this period").strong().size(15.0));
        if v.is_empty() {
            let recorded = self
                .records
                .iter()
                .flat_map(|r| r["top"].as_array().into_iter().flatten())
                .any(|e| res.get(e).is_some());
            ui.label(RichText::new(res.none_text(recorded)).weak());
            return;
        }
        egui::Grid::new("hist_top").num_columns(cols.len() + 1).striped(true).spacing([16.0, 4.0]).show(ui, |ui| {
            ui.label(RichText::new("Name").weak());
            for m in &cols {
                ui.label(RichText::new(m.column()).weak());
            }
            ui.end_row();
            for (name, (kind, vals)) in v.iter().take(10) {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(*name).strong());
                    ui.label(RichText::new(*kind).small().weak());
                });
                for (j, m) in cols.iter().enumerate() {
                    let t = m.cell(value(vals, *m));
                    ui.label(if j == 0 { RichText::new(t).strong() } else { RichText::new(t) });
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hover_lists_the_graphs_own_resource() {
        // [name, kind, cpu %, memory MiB, gpu %, disk I/O B/s, network B/s]
        let rec = json!({"top": [
            ["Firefox", "app", 40.0, 2000, 5.0, 1000.0, 50000.0],
            ["Steam", "app", 2.0, 900, 80.0, 9000000.0, 0.0],
            ["Old", "app", 9.0, 100, 0.0],
        ]});
        let names = |r: Res| top_by(&rec, r).into_iter().map(|(n, _)| n).collect::<Vec<_>>();
        assert_eq!(names(Res::Cpu), ["Firefox", "Old", "Steam"]);
        assert_eq!(names(Res::Gpu), ["Steam", "Firefox"]);
        assert_eq!(names(Res::Io), ["Steam", "Firefox"]);
        assert_eq!(names(Res::Net), ["Firefox"]);
        assert_eq!(top_by(&rec, Res::Mem)[0], ("Firefox", 2000.0));
        assert_eq!(Res::of(&Source::Disk("sda")), Res::Io);
        assert_eq!(Res::Gpu.fmt(80.0), "80% GPU");
        // Records written before disk/network were saved don't pretend the apps used none.
        let old = json!({"top": [["Old", "app", 9.0, 100, 0.0]]});
        assert!(top_by(&old, Res::Io).is_empty() && Res::Io.get(&old["top"][0]).is_none());
    }
}
