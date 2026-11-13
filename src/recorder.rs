//! Background recorder (`taskdm --record`, run as a systemd user service): keeps a compact
//! history of system metrics and the heaviest apps, and raises desktop alerts, even while the
//! Taskd window is closed.
//!
//! Storage: one JSON object per line in `$XDG_DATA_HOME/taskd/history/YYYY-MM-DD.jsonl`
//! (UTC dates), pruned after `RETENTION_DAYS`.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::config::Config;
use crate::groups::{Group, Grouper, Kind};
use crate::sampler::{Sampler, Snapshot};
use crate::util::fmt_bytes;

pub const RETENTION_DAYS: u64 = 7;
const SERVICE: &str = "taskd-recorder.service";

pub fn history_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("taskd").join("history"))
}

pub fn epoch() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// UTC calendar date for an epoch time (civil-from-days, Howard Hinnant's algorithm).
pub fn utc_date(t: u64) -> (i64, u32, u32) {
    let z = (t / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn day_file(t: u64) -> String {
    let (y, m, d) = utc_date(t);
    format!("{y:04}-{m:02}-{d:02}.jsonl")
}

/// One compact history record (also used by the GUI's history view).
pub fn record(s: &Snapshot, g: &Grouper) -> Value {
    let pct = |a: u64, b: u64| if b == 0 { 0.0 } else { (1000.0 * a as f64 / b as f64).round() / 10.0 };
    // Round in f64: an f32 like 1.3 would serialize as 1.2999999523162842.
    let r1 = |v: f32| (v as f64 * 10.0).round() / 10.0;
    // Heaviest apps/services/containers: the top 5 by each resource the history graphs show
    // (CPU, memory, GPU, disk I/O, network), so hovering any graph can say who was using it.
    // Entry: [name, kind, cpu %, memory MiB, gpu %, disk I/O B/s, network B/s].
    // Lone processes only count when they're actually heavy.
    let io = |x: &Group| x.read + x.write;
    let gs: Vec<&Group> = g
        .groups
        .iter()
        .filter(|x| {
            x.kind != Kind::Process
                || x.cpu > 1.0
                || x.mem > 200 * 1_048_576
                || x.gpu > 1.0
                || io(x) > 1_048_576.0
                || x.net > 102_400.0
        })
        .collect();
    let metrics: [&dyn Fn(&Group) -> f64; 5] =
        [&|x| x.cpu as f64, &|x| x.mem as f64, &|x| x.gpu as f64, &io, &|x| x.net];
    let mut top: Vec<Value> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for m in metrics {
        let mut v: Vec<&Group> = gs.iter().copied().filter(|x| m(x) > 0.0).collect();
        v.sort_by(|a, b| m(b).total_cmp(&m(a)));
        for x in v.into_iter().take(5) {
            if seen.insert(&x.key) {
                top.push(json!([
                    x.name,
                    x.kind.label(),
                    r1(x.cpu),
                    x.mem / 1_048_576,
                    r1(x.gpu),
                    io(x).round(),
                    x.net.round()
                ]));
            }
        }
    }
    json!({
        "t": epoch(),
        "cpu": r1(s.cpu.usage),
        "mem": pct(s.mem.used, s.mem.total),
        "swap": pct(s.mem.swap_used, s.mem.swap_total),
        "load": r1(s.load[0]),
        "ctemp": s.cpu.temp_c.map(r1),
        "gpu": s.gpus.iter().map(|x| json!([x.util.map(r1), x.temp_c.map(r1)])).collect::<Vec<_>>(),
        "disk": s.disks.iter().map(|d| (d.name.clone(), json!([r1(d.busy), d.read_rate.round(), d.write_rate.round()]))).collect::<serde_json::Map<_, _>>(),
        "net": s.nets.iter().map(|n| (n.name.clone(), json!([n.rx_rate.round(), n.tx_rate.round()]))).collect::<serde_json::Map<_, _>>(),
        "bat": s.batteries.first().map(|b| json!([r1(b.percent), b.power_w.map(r1)])),
        // Pressure stall (some avg10): waiting for CPU, memory, disk.
        "psi": s.pressure.as_ref().map(|p| json!([r1(p.cpu), r1(p.mem), r1(p.io)])),
        "cpw": s.cpu.power_w.map(r1),
        "top": top,
    })
}

/// Read records with `t >= since` (oldest first). Reads only the relevant day files.
pub fn load(since: u64) -> Vec<Value> {
    let Some(dir) = history_dir() else { return Vec::new() };
    let mut out = Vec::new();
    let now = epoch();
    let mut day = since - since % 86_400;
    while day <= now {
        if let Ok(text) = std::fs::read_to_string(dir.join(day_file(day))) {
            out.extend(
                text.lines()
                    .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                    .filter(|v| v["t"].as_u64().is_some_and(|t| t >= since)),
            );
        }
        day += 86_400;
    }
    out
}

fn prune(dir: &std::path::Path) {
    let keep_from = day_file(epoch().saturating_sub(RETENTION_DAYS * 86_400));
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.ends_with(".jsonl") && n < keep_from {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

// ------------------------------------------------------------------ alerts

pub fn notify(title: &str, body: &str, urgent: bool) {
    let _ = Command::new(crate::actions::sys_bin("notify-send"))
        .args([
            "--app-name=Taskd",
            "--icon=taskd-monitor",
            "-u",
            if urgent { "critical" } else { "normal" },
            title,
            body,
        ])
        .status();
}

/// Whether alert notifications are muted at `now` (Unix seconds).
pub fn muted(cfg: &Config, now: u64) -> bool {
    now < cfg.alerts_muted_until
}

/// "until 15:30" / "until Sat 09:00" / "until you unmute them"; None when not muted.
pub fn mute_text(cfg: &Config, now: u64) -> Option<String> {
    let until = cfg.alerts_muted_until;
    muted(cfg, now).then(|| match until {
        u64::MAX => "until you unmute them".to_string(),
        t => format!("until {}", crate::ui_history::fmt_time(t, t - now > 20 * 3600)),
    })
}

/// Alert rules, evaluated on every recorded sample with a per-rule cooldown.
pub struct Alerts {
    recent: VecDeque<(u64, f32, Vec<f32>)>, // (time, cpu %, per-disk busy %)
    swap: VecDeque<(u64, f32)>,
    fired: HashMap<String, Instant>,
    last_service_check: Option<Instant>,
    /// When the system started struggling (pressure stall), if it still is.
    struggling_since: Option<u64>,
}

const COOLDOWN: Duration = Duration::from_secs(30 * 60);

impl Alerts {
    pub fn new() -> Self {
        Alerts {
            recent: VecDeque::new(),
            swap: VecDeque::new(),
            fired: HashMap::new(),
            last_service_check: None,
            struggling_since: None,
        }
    }

    fn fire(&mut self, key: &str, title: &str, body: String, urgent: bool, out: &mut Vec<(String, String, bool)>) {
        if self.fired.get(key).is_some_and(|t| t.elapsed() < COOLDOWN) {
            return;
        }
        self.fired.insert(key.to_string(), Instant::now());
        out.push((title.to_string(), body, urgent));
    }

    /// A crash or out-of-memory kill seen in the journal (one alert per program per cooldown).
    pub fn on_event(&mut self, e: &crate::events::Event, cfg: &Config) -> Option<(String, String, bool)> {
        use crate::events::Kind;
        if !cfg.alert_crashes || e.kind == Kind::ServiceFailed {
            return None; // failed services have their own rule
        }
        let mut out = Vec::new();
        let urgent = e.kind != Kind::Crash;
        self.fire(&format!("ev:{}", e.name), &e.title, e.detail.clone(), urgent, &mut out);
        out.pop()
    }

    /// Evaluate all rules; returns the notifications to show (title, body, urgent).
    pub fn check(&mut self, s: &Snapshot, g: &Grouper, cfg: &Config) -> Vec<(String, String, bool)> {
        let mut out = Vec::new();
        let now = epoch();
        let busy: Vec<f32> = s.disks.iter().map(|d| d.busy).collect();
        self.recent.push_back((now, s.cpu.usage, busy));
        self.swap.push_back((
            now,
            if s.mem.swap_total == 0 { 0.0 } else { 100.0 * s.mem.swap_used as f32 / s.mem.swap_total as f32 },
        ));
        while self.recent.front().is_some_and(|(t, ..)| now - t > 15 * 60) {
            self.recent.pop_front();
        }
        while self.swap.front().is_some_and(|(t, _)| now - t > 10 * 60) {
            self.swap.pop_front();
        }
        let top_cpu = || {
            let mut v: Vec<_> = g.groups.iter().collect();
            v.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
            v.iter().take(3).map(|x| format!("{} ({:.0}%)", x.name, x.cpu)).collect::<Vec<_>>().join(", ")
        };
        let top_mem = || {
            let mut v: Vec<_> = g.groups.iter().collect();
            v.sort_by_key(|x| std::cmp::Reverse(x.mem));
            v.iter().take(3).map(|x| format!("{} ({})", x.name, fmt_bytes(x.mem))).collect::<Vec<_>>().join(", ")
        };
        // Sustained conditions are worked out up front (they only read the sample window).
        let cpu_maxed = cfg.alert_cpu_pct > 0.0
            && sustained(&self.recent, now, cfg.alert_cpu_mins, |(_, c, _)| *c >= cfg.alert_cpu_pct);
        let saturated: Vec<usize> = if cfg.alert_disk_pct > 0.0 {
            (0..s.disks.len())
                .filter(|&i| {
                    sustained(&self.recent, now, 5.0, |(_, _, b)| b.get(i).is_some_and(|x| *x >= cfg.alert_disk_pct))
                })
                .collect()
        } else {
            Vec::new()
        };

        if cfg.alert_mem_pct > 0.0 && s.mem.total > 0 {
            let avail = 100.0 * s.mem.available as f32 / s.mem.total as f32;
            if avail < cfg.alert_mem_pct {
                self.fire(
                    "mem",
                    "Memory is running low",
                    format!("Only {avail:.0}% available. Biggest: {}", top_mem()),
                    true,
                    &mut out,
                );
            }
        }
        if cpu_maxed {
            self.fire(
                "cpu",
                "CPU has been maxed out",
                format!("Over {:.0}% for {:.0} min. Busiest: {}", cfg.alert_cpu_pct, cfg.alert_cpu_mins, top_cpu()),
                false,
                &mut out,
            );
        }
        for i in saturated {
            let d = &s.disks[i];
            self.fire(
                &format!("disk:{}", d.name),
                "A disk is saturated",
                format!("{} has been over {:.0}% busy for 5 min", d.name, cfg.alert_disk_pct),
                false,
                &mut out,
            );
        }
        if cfg.alert_cpu_temp > 0.0
            && let Some(t) = s.cpu.temp_c.filter(|t| *t >= cfg.alert_cpu_temp)
        {
            self.fire("ctemp", "CPU is very hot", format!("{t:.0} °C. Busiest: {}", top_cpu()), true, &mut out);
        }
        if cfg.alert_gpu_temp > 0.0 {
            for (i, gp) in s.gpus.iter().enumerate() {
                if let Some(t) = gp.temp_c.filter(|t| *t >= cfg.alert_gpu_temp) {
                    self.fire(
                        &format!("gtemp{i}"),
                        "GPU is very hot",
                        format!("{} at {t:.0} °C", gp.name),
                        true,
                        &mut out,
                    );
                }
            }
        }
        if cfg.alert_swap
            && let (Some(first), Some(last)) = (self.swap.front(), self.swap.back())
            && now - first.0 >= 5 * 60
            && last.1 - first.1 > 10.0
        {
            self.fire(
                "swap",
                "The system is swapping",
                format!(
                    "Swap use rose from {:.0}% to {:.0}% in a few minutes; things will feel slow. Biggest: {}",
                    first.1,
                    last.1,
                    top_mem()
                ),
                false,
                &mut out,
            );
        }
        let struggling = s.pressure.as_ref().filter(|p| p.level() == crate::sensors::Level::Struggling);
        self.struggling_since = struggling.map(|_| self.struggling_since.unwrap_or(now));
        if cfg.alert_pressure
            && let (Some(p), Some(since)) = (struggling, self.struggling_since)
            && now - since >= 120
        {
            let blame = if p.mem / 30.0 >= (p.io / 40.0).max(p.cpu / 70.0) { top_mem() } else { top_cpu() };
            self.fire(
                "psi",
                "Your computer is struggling",
                format!("{} for over 2 minutes. Biggest: {blame}", p.verdict().trim_start_matches("Struggling: ")),
                false,
                &mut out,
            );
        }
        if cfg.alert_battery_pct > 0.0
            && let Some(b) = s.batteries.iter().find(|b| b.discharging() && b.percent <= cfg.alert_battery_pct)
        {
            self.fire("bat", "Battery is low", format!("{:.0}% left", b.percent), true, &mut out);
        }
        // Per-app memory limits: "Name=GB;Name=GB".
        for (name, gb) in cfg
            .alert_app_mem
            .split(';')
            .filter_map(|e| e.split_once('='))
            .filter_map(|(n, v)| Some((n.trim(), v.trim().parse::<f64>().ok()?)))
        {
            if let Some(x) = g.groups.iter().find(|x| x.name.eq_ignore_ascii_case(name))
                && x.mem as f64 > gb * 1_073_741_824.0
            {
                self.fire(
                    &format!("app:{name}"),
                    &format!("{} is using a lot of memory", x.name),
                    format!("{} (your limit: {gb} GB)", fmt_bytes(x.mem)),
                    false,
                    &mut out,
                );
            }
        }
        if cfg.alert_failed_services && self.last_service_check.is_none_or(|t| t.elapsed() >= Duration::from_secs(300))
        {
            self.last_service_check = Some(Instant::now());
            for unit in failed_units() {
                self.fire(
                    &format!("svc:{unit}"),
                    "A service failed",
                    format!("{unit} is in the failed state"),
                    false,
                    &mut out,
                );
            }
        }
        out
    }
}

/// True if the window covers at least `mins` minutes and every sample in it satisfies `pred`.
fn sustained(
    recent: &VecDeque<(u64, f32, Vec<f32>)>,
    now: u64,
    mins: f32,
    pred: impl Fn(&(u64, f32, Vec<f32>)) -> bool,
) -> bool {
    let span = mins * 60.0;
    let w: Vec<_> = recent.iter().filter(|(t, ..)| (now - t) as f32 <= span).collect();
    w.first().is_some_and(|(t, ..)| (now - t) as f32 >= span - 15.0) && w.into_iter().all(pred)
}

impl Default for Alerts {
    fn default() -> Self {
        Self::new()
    }
}

fn failed_units() -> Vec<String> {
    let mut v = Vec::new();
    for user in [false, true] {
        let mut c = Command::new(crate::actions::sys_bin("systemctl"));
        if user {
            c.arg("--user");
        }
        if let Ok(o) = c.args(["list-units", "--state=failed", "--no-legend", "--plain"]).output() {
            v.extend(
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter_map(|l| l.trim_start_matches(['●', '*', ' ']).split_whitespace().next().map(String::from)),
            );
        }
    }
    v
}

// ------------------------------------------------------------------ daemon

/// `taskdm --record`: sample, append history, check alerts, forever.
pub fn run() -> i32 {
    let Some(dir) = history_dir() else {
        eprintln!("taskdm --record: no home directory");
        return 1;
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("taskdm --record: {}: {e}", dir.display());
        return 1;
    }
    // One recorder per user: hold an exclusive lock for our lifetime.
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(".lock"));
    let Ok(lock) = lock else { return 1 };
    // SAFETY: flock on a valid fd.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        eprintln!("taskdm --record: another recorder is already running");
        return 0;
    }
    // Be a good background citizen.
    // SAFETY: plain syscalls on our own process.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
    }

    let cfg_path_mtime = || crate::config::path().and_then(|p| std::fs::metadata(p).ok()?.modified().ok());
    let mut cfg = Config::load();
    let mut cfg_mtime = cfg_path_mtime();
    let mut sampler = Sampler::new();
    let mut grouper = Grouper::default();
    grouper.prime_container_names();
    // SAFETY: getuid never fails.
    let uid = unsafe { libc::getuid() };
    let mut alerts = Alerts::new();
    // Crashes / OOM kills arrive from a journal follower thread.
    let (ev_tx, ev_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || crate::events::follow(|e| drop(ev_tx.send(e))));
    let mut last_prune = None::<Instant>;
    eprintln!("taskdm recorder {} writing to {}", env!("CARGO_PKG_VERSION"), dir.display());
    loop {
        let interval = Duration::from_secs(cfg.record_interval_s.clamp(2, 300));
        std::thread::sleep(interval);
        if cfg_path_mtime() != cfg_mtime {
            cfg = Config::load();
            cfg_mtime = cfg_path_mtime();
        }
        let snap = sampler.sample();
        grouper.update(&snap, uid);
        let rec = record(&snap, &grouper);
        let file = dir.join(day_file(epoch()));
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&file) {
            let _ = writeln!(f, "{rec}");
        }
        // Rules keep running while muted (so cooldowns stay current); only the popups stop.
        let mut fired = alerts.check(&snap, &grouper, &cfg);
        while let Ok(e) = ev_rx.try_recv() {
            fired.extend(alerts.on_event(&e, &cfg));
        }
        if !muted(&cfg, epoch()) {
            for (title, body, urgent) in fired {
                notify(&title, &body, urgent);
            }
        }
        if last_prune.is_none_or(|t| t.elapsed() > Duration::from_secs(3600)) {
            prune(&dir);
            last_prune = Some(Instant::now());
        }
    }
}

// ------------------------------------------------------------------ service management

fn unit_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("systemd/user").join(SERVICE))
}

fn systemctl_user(args: &[&str]) -> Result<String, String> {
    let o = Command::new(crate::actions::sys_bin("systemctl"))
        .arg("--user")
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&o.stderr).trim().to_string())
    }
}

/// Install (or update) and start the user service running `exe --record`.
pub fn enable(exe: &std::path::Path) -> Result<(), String> {
    let path = unit_path().ok_or("no config directory")?;
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    let unit = format!(
        "[Unit]\nDescription=Taskd background recorder (history and alerts)\n\n\
         [Service]\nExecStart=\"{}\" --record\nRestart=on-failure\nRestartSec=30\nNice=10\nIOSchedulingClass=idle\n\n\
         [Install]\nWantedBy=default.target\n",
        exe.display()
    );
    if std::fs::read_to_string(&path).ok().as_deref() != Some(unit.as_str()) {
        std::fs::write(&path, unit).map_err(|e| e.to_string())?;
        systemctl_user(&["daemon-reload"])?;
    }
    systemctl_user(&["enable", "--now", SERVICE]).map(|_| ())
}

pub fn disable() -> Result<(), String> {
    systemctl_user(&["disable", "--now", SERVICE]).map(|_| ())
}

/// After an update replaced the binary, run the new version.
pub fn restart_if_running() {
    let _ = systemctl_user(&["try-restart", SERVICE]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(utc_date(0), (1970, 1, 1));
        assert_eq!(utc_date(951_782_400), (2000, 2, 29));
        assert_eq!(utc_date(1_794_096_000), (2026, 11, 8));
        assert_eq!(day_file(1_794_096_000), "2026-11-08.jsonl");
    }

    #[test]
    fn records_are_compact_json() {
        let mut s = Sampler::new();
        let snap = s.sample();
        let mut g = Grouper::default();
        g.update(&snap, unsafe { libc::getuid() });
        let r = record(&snap, &g);
        let line = r.to_string();
        assert!(line.len() < 4000, "record too big: {} bytes", line.len());
        // `top` may legitimately be empty on an idle machine (e.g. a CI container).
        assert!(r["top"].is_array() && r["cpu"].is_number() && r["mem"].is_number() && r["t"].is_u64());
    }

    #[test]
    fn sustained_needs_full_window() {
        let mut r = VecDeque::new();
        for t in [1000u64, 1100, 1200, 1300] {
            r.push_back((t, 95.0, vec![]));
        }
        assert!(sustained(&r, 1300, 5.0, |(_, c, _)| *c > 90.0)); // covers 300 s
        assert!(!sustained(&r, 1300, 10.0, |(_, c, _)| *c > 90.0)); // window too short
        r.push_back((1310, 20.0, vec![]));
        assert!(!sustained(&r, 1310, 5.0, |(_, c, _)| *c > 90.0)); // one dip breaks it
    }

    #[test]
    fn alert_cooldown() {
        let mut a = Alerts::new();
        let mut out = Vec::new();
        a.fire("x", "t", "b".into(), false, &mut out);
        a.fire("x", "t", "b".into(), false, &mut out);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn muting() {
        let mut cfg = Config::default();
        assert!(!muted(&cfg, 1000) && mute_text(&cfg, 1000).is_none());
        cfg.alerts_muted_until = 1000 + 3600;
        assert!(muted(&cfg, 1000) && mute_text(&cfg, 1000).unwrap().starts_with("until "));
        assert!(!muted(&cfg, 1000 + 3600), "a timed mute ends by itself");
        cfg.alerts_muted_until = u64::MAX;
        assert_eq!(mute_text(&cfg, 1000).as_deref(), Some("until you unmute them"));
    }
}
