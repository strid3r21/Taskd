//! Hardware sensors (hwmon temperatures, fans, power), CPU package power (RAPL) and
//! pressure stall information (PSI: how much time programs spend *waiting* for CPU, memory
//! or disk, the best single answer to "is the computer actually slow right now").

use crate::sampler::History;
use crate::util::CachedFile;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SensorKind {
    Temp,
    Fan,
    Power,
}

impl SensorKind {
    pub fn fmt(self, v: f32) -> String {
        match self {
            SensorKind::Temp => format!("{v:.0} °C"),
            SensorKind::Fan => format!("{v:.0} RPM"),
            SensorKind::Power => format!("{v:.1} W"),
        }
    }
}

#[derive(Clone)]
pub struct Sensor {
    /// Chip, e.g. "k10temp" or "nvme (Samsung SSD 980)".
    pub chip: String,
    pub label: String,
    pub kind: SensorKind,
    pub value: Option<f32>,
    pub min: f32,
    pub max: f32,
    pub hist: History,
}

struct Source {
    file: CachedFile,
    /// Raw value -> unit (millidegrees -> °C, microwatts -> W, RPM as is).
    scale: f32,
}

/// One RAPL energy counter (e.g. "package-0"), read directly or through the root helper.
struct Rapl {
    name: String,
    file: CachedFile,
    max_range: u64,
    prev: Option<u64>,
}

/// hwmon sensors. Some are slow to read (an NVMe temperature is an admin command to the
/// drive, ~5-10 ms; some Wi-Fi chips take 20 ms), so they're read on their own thread and
/// only while something shows them (see `want`), not on every sample.
#[derive(Default)]
struct Hwmon {
    sources: Vec<Source>,
    sensors: Vec<Sensor>,
}

/// When sensor values were last wanted (Unix seconds).
static WANTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Ask for sensor readings for the next few seconds (call while showing them).
pub fn want() {
    WANTED.store(crate::recorder::epoch(), std::sync::atomic::Ordering::Relaxed);
}

#[derive(Default)]
pub struct SensorReader {
    feed: std::sync::Arc<std::sync::Mutex<Vec<Sensor>>>,
    rapl: Vec<Rapl>,
    rapl_direct: bool,
    psi: [Option<CachedFile>; 3],
    pub pressure: Option<Pressure>,
}

#[derive(Clone, Default)]
pub struct Pressure {
    /// Percent of the last 10 s that at least one runnable task waited for a CPU.
    pub cpu: f32,
    /// ... waited for memory (reclaim, swap-in, thrashing).
    pub mem: f32,
    /// ... all tasks were stalled on memory at once.
    pub mem_full: f32,
    /// ... waited for disk I/O.
    pub io: f32,
    pub io_full: f32,
    /// Combined strain (see `score`), and each resource's wait.
    pub hist: History,
    pub cpu_hist: History,
    pub mem_hist: History,
    pub io_hist: History,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Ok,
    Slow,
    Struggling,
}

impl Pressure {
    /// Thresholds per resource (some avg10): CPU waits are normal under load, so it gets more slack.
    fn levels(&self) -> [(Level, &'static str); 3] {
        let lv = |v: f32, slow: f32, bad: f32| {
            if v >= bad {
                Level::Struggling
            } else if v >= slow {
                Level::Slow
            } else {
                Level::Ok
            }
        };
        [
            (lv(self.mem, 10.0, 30.0), "memory"),
            (lv(self.io, 15.0, 40.0), "the disk"),
            (lv(self.cpu, 30.0, 70.0), "the CPU"),
        ]
    }

    pub fn level(&self) -> Level {
        self.levels().iter().map(|l| l.0).max().unwrap_or(Level::Ok)
    }

    /// "Responsive" / "Slowing down: waiting for memory" / ...
    pub fn verdict(&self) -> String {
        let lv = self.levels();
        let worst = self.level();
        let why: Vec<&str> = lv.iter().filter(|l| l.0 == worst).map(|l| l.1).collect();
        match worst {
            Level::Ok => "Responsive".into(),
            Level::Slow => format!("Slowing down: waiting for {}", why.join(" and ")),
            Level::Struggling => format!("Struggling: waiting for {}", why.join(" and ")),
        }
    }

    /// One number for the sidebar graph: the worst wait, scaled so each resource's
    /// "struggling" threshold lands at the same height.
    pub fn score(&self) -> f32 {
        (self.mem / 30.0).max(self.io / 40.0).max(self.cpu / 70.0).min(1.5) * 66.7
    }
}

/// `some avg10=1.23 ...` / `full avg10=...` -> (some, full).
pub fn parse_psi(text: &[u8]) -> (f32, f32) {
    let mut out = (0.0, 0.0);
    for line in text.split(|&c| c == b'\n') {
        let s = String::from_utf8_lossy(line);
        let v = s.split_whitespace().find_map(|f| f.strip_prefix("avg10=")).and_then(|v| v.parse().ok());
        if let Some(v) = v {
            if s.starts_with("some") {
                out.0 = v;
            } else if s.starts_with("full") {
                out.1 = v;
            }
        }
    }
    out
}

fn read_trim(p: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

impl SensorReader {
    pub fn new() -> Self {
        let mut r = SensorReader {
            psi: ["cpu", "memory", "io"].map(|n| {
                let p = format!("/proc/pressure/{n}");
                std::path::Path::new(&p).exists().then(|| CachedFile::new(p, false))
            }),
            ..Default::default()
        };
        r.rescan();
        // The sensor thread ends when this reader is dropped.
        let weak = std::sync::Arc::downgrade(&r.feed);
        std::thread::spawn(move || {
            let mut hw = Hwmon::default();
            hw.rescan();
            let mut buf = Vec::new();
            let mut n = 0u32;
            loop {
                let Some(feed) = weak.upgrade() else { return };
                let active =
                    crate::recorder::epoch().saturating_sub(WANTED.load(std::sync::atomic::Ordering::Relaxed)) <= 10;
                if active {
                    hw.read(&mut buf);
                }
                *feed.lock().unwrap() = hw.sensors.clone();
                drop(feed);
                n += 1;
                if n % 30 == 0 {
                    hw.rescan(); // hotplug
                }
                std::thread::sleep(std::time::Duration::from_millis(if active { 2000 } else { 1000 }));
            }
        });
        r
    }

    /// Find RAPL counters (cheap).
    pub fn rescan(&mut self) {
        if self.rapl.is_empty() {
            self.rapl = rapl_domains()
                .into_iter()
                .map(|(name, path, max_range)| Rapl {
                    name,
                    file: CachedFile::new(format!("{path}/energy_uj"), true),
                    max_range,
                    prev: None,
                })
                .collect();
            let mut buf = Vec::new();
            self.rapl_direct = self.rapl.iter_mut().any(|r| r.file.read_u64(&mut buf).is_some());
        }
    }
}

impl Hwmon {
    /// Find hwmon sensors (cheap: directory listings, no sensor reads).
    fn rescan(&mut self) {
        let mut found: Vec<(Sensor, String, f32)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir("/sys/class/hwmon") {
            let mut dirs: Vec<_> = rd.flatten().map(|e| e.path()).collect();
            dirs.sort();
            for base in dirs {
                let Some(name) = read_trim(&base.join("name")) else { continue };
                let chip = match read_trim(&base.join("device/model")) {
                    Some(m) => format!("{name} ({m})"),
                    None => name,
                };
                let Ok(files) = std::fs::read_dir(&base) else { continue };
                let mut inputs: Vec<String> = files
                    .flatten()
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|f| f.ends_with("_input") || (f.starts_with("power") && f.ends_with("_average")))
                    .collect();
                inputs.sort_by_key(|f| {
                    (f.chars().filter(|c| c.is_ascii_digit()).collect::<String>().parse::<u32>().ok(), f.clone())
                });
                for f in inputs {
                    let (kind, scale) = if f.starts_with("temp") {
                        (SensorKind::Temp, 1000.0)
                    } else if f.starts_with("fan") {
                        (SensorKind::Fan, 1.0)
                    } else if f.starts_with("power") {
                        (SensorKind::Power, 1_000_000.0)
                    } else {
                        continue;
                    };
                    let stem = f.trim_end_matches("_input").trim_end_matches("_average");
                    if found.iter().any(|(s, p, _)| s.chip == chip && p.contains(&format!("/{stem}_"))) {
                        continue; // power1_input and power1_average: keep one
                    }
                    let label = read_trim(&base.join(format!("{stem}_label"))).unwrap_or_else(|| stem.to_string());
                    let path = base.join(&f).to_string_lossy().into_owned();
                    let s = Sensor {
                        chip: chip.clone(),
                        label,
                        kind,
                        value: None,
                        min: f32::MAX,
                        max: f32::MIN,
                        hist: History::default(),
                    };
                    found.push((s, path, scale));
                }
            }
        }
        // Keep history/min/max of sensors that are still there.
        let old = std::mem::take(&mut self.sensors);
        self.sources.clear();
        for (mut s, path, scale) in found {
            if let Some(o) = old.iter().find(|o| o.chip == s.chip && o.label == s.label && o.kind == s.kind) {
                s = o.clone();
            }
            self.sources.push(Source { file: CachedFile::new(path, true), scale });
            self.sensors.push(s);
        }
    }

    fn read(&mut self, buf: &mut Vec<u8>) {
        for (src, s) in self.sources.iter_mut().zip(self.sensors.iter_mut()) {
            // Some sensors refuse reads while their device sleeps (EIO/ENODATA): show "—".
            s.value = src.file.read_u64(buf).map(|v| v as f32 / src.scale).filter(|v| v.is_finite());
            if let Some(v) = s.value {
                s.min = s.min.min(v);
                s.max = s.max.max(v);
                s.hist.push(v);
            }
        }
    }
}

impl SensorReader {
    /// Read everything. `helper_rapl`: energy counters from the root helper (RAPL is root-only
    /// on current kernels). Returns the CPU package power in watts, if known.
    pub fn read(
        &mut self,
        buf: &mut Vec<u8>,
        elapsed: f64,
        helper_rapl: Option<&std::collections::HashMap<String, u64>>,
    ) -> Option<f32> {
        let mut watts = None::<f32>;
        for r in &mut self.rapl {
            let now =
                if self.rapl_direct { r.file.read_u64(buf) } else { helper_rapl.and_then(|h| h.get(&r.name).copied()) };
            if let (Some(now), Some(prev)) = (now, r.prev) {
                let delta = if now >= prev { now - prev } else { now + r.max_range.saturating_sub(prev) };
                *watts.get_or_insert(0.0) += (delta as f64 / 1e6 / elapsed) as f32;
            }
            r.prev = now;
        }

        let mut p = self.pressure.take().unwrap_or_default();
        let mut any = false;
        for (i, f) in self.psi.iter_mut().enumerate() {
            let Some(f) = f else { continue };
            if f.read(buf).is_err() {
                continue;
            }
            any = true;
            let (some, full) = parse_psi(buf);
            match i {
                0 => p.cpu = some,
                1 => (p.mem, p.mem_full) = (some, full),
                _ => (p.io, p.io_full) = (some, full),
            }
        }
        if any {
            p.hist.push(p.score());
            p.cpu_hist.push(p.cpu);
            p.mem_hist.push(p.mem);
            p.io_hist.push(p.io);
            self.pressure = Some(p);
        }
        watts
    }

    /// Latest readings from the sensor thread (values are None until wanted and read).
    pub fn sensors(&self) -> Vec<Sensor> {
        self.feed.lock().unwrap().clone()
    }

    pub fn has_rapl(&self) -> bool {
        !self.rapl.is_empty()
    }
}

/// Top-level RAPL domains (`intel-rapl:0`, also used on AMD): (name, dir, wrap-around range).
pub fn rapl_domains() -> Vec<(String, String, u64)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/sys/class/powercap") else { return out };
    let mut dirs: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    dirs.sort();
    for d in dirs {
        let n = d.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        // Packages only ("intel-rapl:0"), not their sub-domains ("intel-rapl:0:0").
        if !n.starts_with("intel-rapl:") || n.matches(':').count() != 1 {
            continue;
        }
        let name = read_trim(&d.join("name")).unwrap_or(n);
        let range = read_trim(&d.join("max_energy_range_uj")).and_then(|v| v.parse().ok()).unwrap_or(u64::MAX);
        out.push((name, d.to_string_lossy().into_owned(), range));
    }
    out
}

/// Energy counters for the root helper to hand out (`{"package-0": µJ}`).
pub fn rapl_snapshot() -> std::collections::HashMap<String, u64> {
    rapl_domains()
        .into_iter()
        .filter_map(|(name, dir, _)| {
            Some((name, read_trim(&std::path::Path::new(&dir).join("energy_uj"))?.parse().ok()?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psi_parsing_and_verdict() {
        let t = b"some avg10=12.50 avg60=3.00 avg300=1.00 total=100\nfull avg10=4.25 avg60=1.00 avg300=0.00 total=50\n";
        assert_eq!(parse_psi(t), (12.5, 4.25));
        assert_eq!(parse_psi(b"some avg10=0.00 avg60=0.00 avg300=0.00 total=1\n"), (0.0, 0.0));
        let mut p = Pressure::default();
        assert_eq!((p.level(), p.verdict()), (Level::Ok, "Responsive".to_string()));
        p.cpu = 40.0; // busy, but CPU waits get slack
        assert_eq!(p.verdict(), "Slowing down: waiting for the CPU");
        p.mem = 35.0;
        assert_eq!(p.verdict(), "Struggling: waiting for memory");
        assert!(p.score() > 66.0);
    }

    #[test]
    fn reads_this_machine() {
        want();
        let mut r = SensorReader::new();
        let mut buf = Vec::new();
        r.read(&mut buf, 1.0, None);
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let mut h = Hwmon::default();
        h.rescan();
        h.read(&mut buf);
        assert_eq!(r.sensors().len(), h.sensors.len(), "feed thread found the same sensors");
        // PSI exists on any kernel since 4.20 with it enabled; sensors depend on hardware.
        if std::path::Path::new("/proc/pressure/memory").exists() {
            assert!(r.pressure.is_some());
        }
        for s in r.sensors() {
            assert!(!s.chip.is_empty() && !s.label.is_empty());
        }
    }
}
