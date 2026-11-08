//! Battery and power from /sys/class/power_supply (laptops; absent on most desktops).

use std::path::{Path, PathBuf};

use crate::sampler::History;

#[derive(Clone, Default)]
pub struct Battery {
    pub name: String,
    pub model: String,
    pub percent: f32,
    /// "Charging", "Discharging", "Full", "Not charging", …
    pub status: String,
    /// Current draw (discharging) or charge rate, watts.
    pub power_w: Option<f32>,
    /// Hours until empty (discharging) or full (charging).
    pub hours_left: Option<f32>,
    /// Full capacity now vs. when new, percent.
    pub health: Option<f32>,
    pub cycles: Option<u32>,
    pub on_ac: bool,
    pub hist: History,
    pub power_hist: History,
}

impl Battery {
    pub fn discharging(&self) -> bool {
        self.status == "Discharging"
    }
}

fn read(dir: &Path, f: &str) -> Option<String> {
    let s = std::fs::read_to_string(dir.join(f)).ok()?;
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn num(dir: &Path, f: &str) -> Option<f64> {
    read(dir, f)?.parse().ok()
}

pub struct PowerReader {
    root: PathBuf,
    batteries: Vec<(PathBuf, Battery)>,
    ac: Vec<PathBuf>,
}

impl PowerReader {
    pub fn new() -> Self {
        Self::with_root("/sys/class/power_supply")
    }

    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let mut r = PowerReader { root, batteries: Vec::new(), ac: Vec::new() };
        r.rescan();
        r
    }

    pub fn rescan(&mut self) {
        let Ok(rd) = std::fs::read_dir(&self.root) else { return };
        let mut bats = Vec::new();
        self.ac.clear();
        for e in rd.flatten() {
            let dir = e.path();
            match read(&dir, "type").as_deref() {
                // Peripheral batteries (mice, headsets) report scope=Device: skip them.
                Some("Battery") if read(&dir, "scope").as_deref() != Some("Device") => {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let old = self.batteries.iter().find(|(d, _)| *d == dir).map(|(_, b)| b.clone());
                    let mut b = old.unwrap_or_default();
                    b.name = name;
                    b.model = read(&dir, "model_name").unwrap_or_default();
                    bats.push((dir, b));
                }
                Some("Mains") | Some("USB") => self.ac.push(dir),
                _ => {}
            }
        }
        bats.sort_by(|a, b| a.1.name.cmp(&b.1.name));
        self.batteries = bats;
    }

    pub fn read(&mut self) -> Vec<Battery> {
        let on_ac = self.ac.iter().any(|d| num(d, "online") == Some(1.0));
        for (dir, b) in &mut self.batteries {
            b.on_ac = on_ac;
            b.status = read(dir, "status").unwrap_or_default();
            let volts = num(dir, "voltage_now").map(|v| v / 1e6);
            // Energy (µWh) or charge (µAh × V) counters, whichever the firmware provides.
            let (now, full, design) = match (num(dir, "energy_now"), num(dir, "energy_full")) {
                (Some(n), Some(f)) => (Some(n / 1e6), Some(f / 1e6), num(dir, "energy_full_design").map(|d| d / 1e6)),
                _ => match (num(dir, "charge_now"), num(dir, "charge_full"), volts) {
                    (Some(n), Some(f), Some(v)) => {
                        (Some(n / 1e6 * v), Some(f / 1e6 * v), num(dir, "charge_full_design").map(|d| d / 1e6 * v))
                    }
                    _ => (None, None, None),
                },
            };
            b.percent =
                num(dir, "capacity").or_else(|| Some(now? / full? * 100.0)).unwrap_or(0.0).clamp(0.0, 100.0) as f32;
            let watts = num(dir, "power_now")
                .map(|p| p / 1e6)
                .or_else(|| Some(num(dir, "current_now")? / 1e6 * volts?))
                .filter(|w| *w > 0.0);
            b.power_w = watts.map(|w| w as f32);
            b.hours_left = match (watts, now, full) {
                (Some(w), Some(n), _) if b.status == "Discharging" => Some((n / w) as f32),
                (Some(w), Some(n), Some(f)) if b.status == "Charging" => Some(((f - n).max(0.0) / w) as f32),
                _ => None,
            };
            b.health = match (full, design) {
                (Some(f), Some(d)) if d > 0.0 => Some((f / d * 100.0).min(100.0) as f32),
                _ => None,
            };
            b.cycles = num(dir, "cycle_count").map(|c| c as u32).filter(|c| *c > 0);
            b.hist.push(b.percent);
            b.power_hist.push(b.power_w.unwrap_or(0.0));
        }
        self.batteries.iter().map(|(_, b)| b.clone()).collect()
    }
}

impl Default for PowerReader {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, f: &str, v: &str) {
        std::fs::write(dir.join(f), v).unwrap();
    }

    #[test]
    fn reads_fake_battery() {
        let root = std::env::temp_dir().join(format!("taskd-power-{}", std::process::id()));
        let bat = root.join("BAT0");
        let ac = root.join("AC");
        let mouse = root.join("hidpp_battery_0");
        for d in [&bat, &ac, &mouse] {
            std::fs::create_dir_all(d).unwrap();
        }
        write(&bat, "type", "Battery");
        write(&bat, "status", "Discharging");
        write(&bat, "capacity", "80");
        write(&bat, "energy_now", "40000000"); // 40 Wh
        write(&bat, "energy_full", "50000000");
        write(&bat, "energy_full_design", "60000000");
        write(&bat, "power_now", "10000000"); // 10 W
        write(&bat, "cycle_count", "123");
        write(&ac, "type", "Mains");
        write(&ac, "online", "0");
        write(&mouse, "type", "Battery");
        write(&mouse, "scope", "Device");

        let mut r = PowerReader::with_root(&root);
        let b = r.read();
        assert_eq!(b.len(), 1, "peripheral batteries are skipped");
        let b = &b[0];
        assert_eq!(b.percent, 80.0);
        assert!(b.discharging() && !b.on_ac);
        assert_eq!(b.power_w, Some(10.0));
        assert!((b.hours_left.unwrap() - 4.0).abs() < 0.01);
        assert!((b.health.unwrap() - 83.33).abs() < 0.1);
        assert_eq!(b.cycles, Some(123));
        std::fs::remove_dir_all(&root).ok();
    }
}
