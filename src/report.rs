//! Shareable system report (Markdown) and history export (CSV).
//!
//! The report is meant to be pasted into bug reports and forum posts, so it leaves out the
//! host name, user names and command lines.

use std::fmt::Write as _;
use std::path::PathBuf;

use serde_json::Value;

use crate::groups::{Group, Grouper};
use crate::sampler::Snapshot;
use crate::util::{fmt_bytes, fmt_duration, fmt_rate};

/// "2026-11-13 14:05" in local time.
pub fn fmt_datetime(t: u64) -> String {
    let tt = t as libc::time_t;
    // SAFETY: localtime_r writes into the zeroed struct we pass.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&tt, &mut tm);
        tm
    };
    format!("{:04}-{:02}-{:02} {:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min)
}

fn os_name() -> String {
    let text = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    text.lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim_matches('"').to_string())
        .unwrap_or_else(|| "Linux".into())
}

fn kernel() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease").map(|s| s.trim().to_string()).unwrap_or_default()
}

/// Markdown report of the machine and its current state.
pub fn build(s: &Snapshot, g: &Grouper) -> String {
    let mut r = String::new();
    let now = crate::recorder::epoch();
    let m = &s.mem;
    let _ = writeln!(r, "# System report\n");
    let _ = writeln!(r, "Generated {} by Taskd {}\n", fmt_datetime(now), env!("CARGO_PKG_VERSION"));

    let _ = writeln!(r, "## System\n");
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let _ = writeln!(
        r,
        "- **OS:** {} (kernel {}){}",
        os_name(),
        kernel(),
        if desktop.is_empty() { String::new() } else { format!(", {desktop} desktop") }
    );
    let _ = writeln!(r, "- **CPU:** {} ({} cores, {} threads)", s.cpu.model, s.cpu.physical, s.cpu.logical);
    let _ = writeln!(r, "- **Memory:** {} RAM, {} swap", fmt_bytes(m.total), fmt_bytes(m.swap_total));
    for gp in &s.gpus {
        let mem = gp.mem_total.map_or(String::new(), |t| format!(", {}", fmt_bytes(t)));
        let _ = writeln!(r, "- **GPU:** {}{mem}", gp.name);
    }
    for d in &s.disks {
        let kind = if d.rotational { "HDD" } else { "SSD" };
        let _ = writeln!(r, "- **Disk:** {} {} {kind} ({})", d.name, d.model, fmt_bytes(d.size));
    }
    for b in &s.batteries {
        let _ = writeln!(r, "- **Battery:** {} at {:.0}% ({})", b.model, b.percent, b.status);
    }
    let _ = writeln!(r, "- **Up time:** {}\n", fmt_duration(s.uptime as u64));

    let _ = writeln!(r, "## Right now\n");
    if let Some(p) = &s.pressure {
        let _ = writeln!(
            r,
            "- **Responsiveness:** {} (waiting for CPU {:.1}%, memory {:.1}%, disk {:.1}% of the last 10 s)",
            p.verdict(),
            p.cpu,
            p.mem,
            p.io
        );
    }
    let mut cpu_line =
        format!("- **CPU:** {:.0}% used, load {:.2} / {:.2} / {:.2}", s.cpu.usage, s.load[0], s.load[1], s.load[2]);
    if let Some(t) = s.cpu.temp_c {
        let _ = write!(cpu_line, ", {t:.0} °C");
    }
    if let Some(w) = s.cpu.power_w {
        let _ = write!(cpu_line, ", {w:.0} W");
    }
    let _ = writeln!(r, "{cpu_line}");
    let pct = |a: u64, b: u64| if b == 0 { 0.0 } else { 100.0 * a as f64 / b as f64 };
    let _ = writeln!(
        r,
        "- **Memory:** {} of {} used ({:.0}%), {} available, swap {} used",
        fmt_bytes(m.used),
        fmt_bytes(m.total),
        pct(m.used, m.total),
        fmt_bytes(m.available),
        fmt_bytes(m.swap_used)
    );
    for d in &s.disks {
        let _ = writeln!(
            r,
            "- **{}:** {:.0}% busy, read {}, write {}",
            d.name,
            d.busy,
            fmt_rate(d.read_rate),
            fmt_rate(d.write_rate)
        );
    }
    for (i, gp) in s.gpus.iter().enumerate() {
        let used = gp.util.map_or("—".into(), |u| format!("{u:.0}%"));
        let temp = gp.temp_c.map_or(String::new(), |t| format!(", {t:.0} °C"));
        let _ = writeln!(r, "- **GPU {i}:** {used} used{temp}");
    }
    let _ = writeln!(r, "- **Processes:** {}, threads {}\n", s.procs.len(), s.threads);

    let top = |key: fn(&Group) -> f64| {
        let mut v: Vec<&Group> = g.groups.iter().filter(|x| key(x) > 0.0).collect();
        v.sort_by(|a, b| key(b).total_cmp(&key(a)));
        v.truncate(10);
        v
    };
    let _ = writeln!(r, "## Heaviest apps and services\n");
    let _ = writeln!(r, "| Name | Type | CPU | Memory | GPU | Disk I/O |\n|---|---|---|---|---|---|");
    // Ignore idle rounding noise (0.0% CPU) when ranking by CPU.
    let mut rows: Vec<&Group> = top(|x| if x.cpu >= 0.05 { x.cpu as f64 } else { 0.0 });
    for x in top(|x| x.mem as f64) {
        if !rows.iter().any(|y| y.key == x.key) {
            rows.push(x);
        }
    }
    for x in rows {
        let _ = writeln!(
            r,
            "| {} | {} | {:.1}% | {} | {:.0}% | {} |",
            x.name.replace('|', "/"),
            x.kind.label(),
            x.cpu,
            fmt_bytes(x.mem),
            x.gpu,
            fmt_rate(x.read + x.write)
        );
    }

    let _ = writeln!(r, "\n## Storage\n");
    let _ = writeln!(r, "| Mounted at | Type | Size | Used | Free |\n|---|---|---|---|---|");
    for f in crate::storage::filesystems() {
        let _ = writeln!(
            r,
            "| {} | {} | {} | {} ({:.0}%) | {} |",
            f.mount,
            f.fstype,
            fmt_bytes(f.total),
            fmt_bytes(f.used),
            f.used_pct(),
            fmt_bytes(f.avail)
        );
    }

    let failed = crate::mcp::failed_services();
    let failed = failed["failed"].as_array().cloned().unwrap_or_default();
    let _ = writeln!(r, "\n## Failed services\n");
    if failed.is_empty() {
        let _ = writeln!(r, "None.");
    }
    for f in failed {
        let _ = writeln!(r, "- {} ({})", f["unit"].as_str().unwrap_or("?"), f["scope"].as_str().unwrap_or(""));
    }

    let _ = writeln!(r, "\n## Crashes and out-of-memory kills (last 7 days)\n");
    match crate::events::load(now.saturating_sub(7 * 86_400)) {
        Ok(ev) => {
            let ev: Vec<_> = ev.into_iter().filter(|e| e.kind != crate::events::Kind::ServiceFailed).collect();
            if ev.is_empty() {
                let _ = writeln!(r, "None.");
            }
            // Group repeats: "x crashed (SIGSEGV)" ×12, last at …
            let mut seen: Vec<(String, usize, u64)> = Vec::new();
            for e in &ev {
                match seen.iter_mut().find(|s| s.0 == e.title) {
                    Some(s) => {
                        s.1 += 1;
                        s.2 = e.t;
                    }
                    None => seen.push((e.title.clone(), 1, e.t)),
                }
            }
            seen.sort_by_key(|s| std::cmp::Reverse(s.2));
            for (title, n, last) in seen.into_iter().take(20) {
                let times = if n > 1 { format!(" ×{n}") } else { String::new() };
                let _ = writeln!(r, "- {title}{times}, last {}", fmt_datetime(last));
            }
        }
        Err(e) => {
            let _ = writeln!(r, "Couldn't read the journal: {e}");
        }
    }
    r
}

/// `taskdm --report`: sample for a second (rates need two samples) and print the report.
pub fn cli() -> i32 {
    crate::sensors::want();
    let mut sampler = crate::sampler::Sampler::new();
    sampler.sample();
    std::thread::sleep(std::time::Duration::from_secs(1));
    let snap = sampler.sample();
    let mut g = Grouper::default();
    g.prime_container_names();
    // SAFETY: getuid never fails.
    g.update(&snap, unsafe { libc::getuid() });
    print!("{}", build(&snap, &g));
    0
}

// ------------------------------------------------------------------ CSV

fn csv_cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Recorded history as CSV: one row per sample, one column per series.
pub fn history_csv(records: &[Value]) -> String {
    // Columns for every disk / interface / GPU that appears anywhere in the period.
    let mut disks: Vec<String> = Vec::new();
    let mut nets: Vec<String> = Vec::new();
    let mut gpus = 0;
    for r in records {
        for (map, list) in [(&r["disk"], &mut disks), (&r["net"], &mut nets)] {
            for k in map.as_object().into_iter().flatten().map(|(k, _)| k) {
                if !list.contains(k) {
                    list.push(k.clone());
                }
            }
        }
        gpus = gpus.max(r["gpu"].as_array().map_or(0, |a| a.len()));
    }
    let mut out = String::from(
        "time,unix_time,cpu_percent,memory_percent,swap_percent,load_1m,cpu_temp_c,cpu_power_w,wait_cpu_percent,wait_memory_percent,wait_disk_percent",
    );
    for i in 0..gpus {
        let _ = write!(out, ",gpu{i}_percent,gpu{i}_temp_c");
    }
    for d in &disks {
        let _ = write!(out, ",{d}_busy_percent,{d}_read_bps,{d}_write_bps");
    }
    for n in &nets {
        let _ = write!(out, ",{n}_receive_bps,{n}_send_bps");
    }
    out.push_str(",top_apps\n");
    for r in records {
        let t = r["t"].as_u64().unwrap_or(0);
        let _ = write!(out, "{},{t}", fmt_datetime(t));
        for k in ["cpu", "mem", "swap", "load", "ctemp", "cpw"] {
            let _ = write!(out, ",{}", csv_cell(&r[k]));
        }
        for i in 0..3 {
            let _ = write!(out, ",{}", csv_cell(&r["psi"][i]));
        }
        for i in 0..gpus {
            let _ = write!(out, ",{},{}", csv_cell(&r["gpu"][i][0]), csv_cell(&r["gpu"][i][1]));
        }
        for d in &disks {
            let x = &r["disk"][d];
            let _ = write!(out, ",{},{},{}", csv_cell(&x[0]), csv_cell(&x[1]), csv_cell(&x[2]));
        }
        for n in &nets {
            let x = &r["net"][n];
            let _ = write!(out, ",{},{}", csv_cell(&x[0]), csv_cell(&x[1]));
        }
        // Heaviest few by CPU, quoted: "Firefox 12.5%; Code 3.1%".
        let mut top: Vec<(&str, f64)> = r["top"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|e| (e[0].as_str().unwrap_or("?"), e[2].as_f64().unwrap_or(0.0)))
            .collect();
        top.sort_by(|a, b| b.1.total_cmp(&a.1));
        let names: Vec<String> = top.iter().take(3).map(|(n, c)| format!("{n} {c:.1}%")).collect();
        let _ = writeln!(out, ",\"{}\"", names.join("; ").replace('"', "'"));
    }
    out
}

/// `taskdm --export-history [1h|6h|24h|7d]`: print recorded history as CSV.
pub fn export_cli(range: Option<&str>) -> i32 {
    let secs = match range.unwrap_or("24h") {
        "1h" => 3600,
        "6h" => 6 * 3600,
        "24h" | "1d" => 86_400,
        "7d" => 7 * 86_400,
        other => {
            eprintln!("unknown range {other}: use 1h, 6h, 24h or 7d");
            return 2;
        }
    };
    let recs = crate::recorder::load(crate::recorder::epoch().saturating_sub(secs));
    if recs.is_empty() {
        eprintln!("No recorded history for this period (is the background recorder on?)");
        return 1;
    }
    print!("{}", history_csv(&recs));
    0
}

/// The user's Downloads folder (XDG user dirs), else the home folder.
pub fn downloads_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    let cfg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
    let text = std::fs::read_to_string(cfg.join("user-dirs.dirs")).unwrap_or_default();
    text.lines()
        .find_map(|l| l.strip_prefix("XDG_DOWNLOAD_DIR="))
        .map(|v| v.trim_matches('"').replace("$HOME", &home.to_string_lossy()))
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or(home)
}

/// Write `text` to a new file in Downloads, e.g. `taskd-report-2026-11-13-1405.md`.
pub fn save_to_downloads(stem: &str, ext: &str, text: &str) -> Result<PathBuf, String> {
    let stamp = fmt_datetime(crate::recorder::epoch()).replace([' ', ':'], "-");
    let p = downloads_dir().join(format!("{stem}-{stamp}.{ext}"));
    std::fs::write(&p, text).map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn csv_has_a_column_per_device() {
        let recs = vec![
            json!({"t": 1_700_000_000u64, "cpu": 12.5, "mem": 40.0, "swap": 0.0, "load": 1.2, "ctemp": 55.0,
                   "gpu": [[30.0, 60.0]], "disk": {"nvme0n1": [5.0, 1000.0, 2000.0]}, "net": {"eth0": [10.0, 20.0]},
                   "psi": [1.0, 0.0, 2.5], "cpw": 30.5, "top": [["Firefox", "App", 10.0, 900, 0.0], ["Code", "App", 2.0, 500, 0.0]]}),
            json!({"t": 1_700_000_010u64, "cpu": 3.0, "mem": 41.0, "disk": {"sda": [1.0, 0.0, 0.0]}, "top": []}),
        ];
        let csv = history_csv(&recs);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 3);
        let header: Vec<&str> = lines[0].split(',').collect();
        assert!(header.contains(&"nvme0n1_busy_percent") && header.contains(&"sda_busy_percent"));
        assert!(header.contains(&"eth0_receive_bps") && header.contains(&"gpu0_percent"));
        assert!(lines[1].contains(",12.5,40.0,") && lines[1].ends_with("\"Firefox 10.0%; Code 2.0%\""));
        // Every row has the same number of columns (top_apps is the only quoted, comma-free field).
        let cols = |l: &str| l.split(',').count();
        assert!(lines.iter().all(|l| cols(l) == header.len()), "{csv}");
    }

    #[test]
    fn report_mentions_the_basics() {
        let mut sampler = crate::sampler::Sampler::new();
        let s = sampler.sample();
        let mut g = Grouper::default();
        g.update(&s, 0);
        let r = build(&s, &g);
        for h in ["# System report", "## System", "## Right now", "## Storage", "## Failed services"] {
            assert!(r.contains(h), "missing {h}");
        }
    }
}
