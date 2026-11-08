//! Per-app / per-service resource limits through systemd (MemoryMax, CPUQuota) or docker/podman.

use std::process::Command;

use crate::actions::sys_bin;

#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// A unit of the user's systemd instance (app scopes, user services): no password needed.
    UserUnit(String),
    /// A system unit (services): systemd asks for authorization through polkit.
    SystemUnit(String),
    /// A Docker/Podman container id.
    Container(String),
}

impl Target {
    pub fn describe(&self) -> String {
        match self {
            Target::UserUnit(u) => format!("user unit {u}"),
            Target::SystemUnit(u) => format!("system unit {u} (asks for your password)"),
            Target::Container(id) => format!("container {}", &id[..id.len().min(12)]),
        }
    }
    /// Only service units can keep limits across restarts (scopes are recreated on each launch).
    pub fn can_persist(&self) -> bool {
        matches!(self, Target::SystemUnit(u) | Target::UserUnit(u) if u.ends_with(".service"))
    }
}

/// The limitable thing a group of processes runs in, given their systemd unit (if any).
pub fn target(unit: Option<&str>, container_id: Option<&str>) -> Option<Target> {
    if let Some(id) = container_id {
        return Some(Target::Container(id.to_string()));
    }
    let unit = unit?;
    let user = Command::new(sys_bin("systemctl"))
        .args(["--user", "show", "-p", "LoadState", "--value", "--", unit])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "loaded");
    Some(if user { Target::UserUnit(unit.into()) } else { Target::SystemUnit(unit.into()) })
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Limits {
    /// Bytes; None = unlimited.
    pub memory: Option<u64>,
    /// Percent of one CPU (200 = two cores); None = unlimited.
    pub cpu_pct: Option<f64>,
}

/// `MemoryMax=` value: bytes or "infinity".
fn parse_mem(v: &str) -> Option<u64> {
    v.trim().parse().ok().filter(|b| *b > 0 && *b < u64::MAX / 2)
}

/// `CPUQuotaPerSecUSec=` value like "1.500000s", "750ms" or "infinity" -> percent of one CPU.
fn parse_quota(v: &str) -> Option<f64> {
    let v = v.trim();
    if let Some(ms) = v.strip_suffix("ms") {
        return ms.parse::<f64>().ok().map(|m| m / 10.0);
    }
    v.strip_suffix('s')?.parse::<f64>().ok().map(|s| s * 100.0)
}

pub fn current(t: &Target) -> Limits {
    match t {
        Target::UserUnit(u) | Target::SystemUnit(u) => {
            let mut c = Command::new(sys_bin("systemctl"));
            if matches!(t, Target::UserUnit(_)) {
                c.arg("--user");
            }
            let Ok(o) = c.args(["show", "-p", "MemoryMax", "-p", "CPUQuotaPerSecUSec", "--", u]).output() else {
                return Limits::default();
            };
            let text = String::from_utf8_lossy(&o.stdout);
            let get = |k: &str| text.lines().find_map(|l| l.strip_prefix(k)).unwrap_or("");
            Limits { memory: parse_mem(get("MemoryMax=")), cpu_pct: parse_quota(get("CPUQuotaPerSecUSec=")) }
        }
        Target::Container(id) => {
            let fmt = "{{.HostConfig.Memory}} {{.HostConfig.NanoCpus}}";
            for tool in ["docker", "podman"] {
                if let Ok(o) = Command::new(sys_bin(tool)).args(["inspect", "-f", fmt, "--", id]).output()
                    && o.status.success()
                {
                    let text = String::from_utf8_lossy(&o.stdout);
                    let mut it = text.split_whitespace();
                    let mem = it.next().and_then(|v| v.parse::<u64>().ok()).filter(|v| *v > 0);
                    let cpu = it.next().and_then(|v| v.parse::<f64>().ok()).filter(|v| *v > 0.0).map(|n| n / 1e7);
                    return Limits { memory: mem, cpu_pct: cpu };
                }
            }
            Limits::default()
        }
    }
}

/// systemd property arguments for these limits (unlimited = reset).
pub fn unit_properties(l: &Limits) -> Vec<String> {
    vec![
        format!("MemoryMax={}", l.memory.map_or("infinity".to_string(), |b| b.to_string())),
        format!("CPUQuota={}", l.cpu_pct.map_or(String::new(), |p| format!("{}%", p.round() as u64))),
    ]
}

pub fn apply(t: &Target, l: &Limits, persist: bool) -> Result<(), String> {
    let out = match t {
        Target::UserUnit(u) | Target::SystemUnit(u) => {
            let mut c = Command::new(sys_bin("systemctl"));
            if matches!(t, Target::UserUnit(_)) {
                c.arg("--user");
            }
            c.arg("set-property");
            if !(persist && t.can_persist()) {
                c.arg("--runtime"); // until the unit restarts / reboot
            }
            c.arg("--").arg(u).args(unit_properties(l));
            c.output()
        }
        Target::Container(id) => {
            let mut last = Err(std::io::Error::other("neither docker nor podman is available"));
            for tool in ["docker", "podman"] {
                let mut c = Command::new(sys_bin(tool));
                c.arg("update");
                if let Some(m) = l.memory {
                    // Same swap ceiling as memory, so the limit can't be dodged by swapping.
                    c.args(["--memory", &m.to_string(), "--memory-swap", &m.to_string()]);
                }
                c.args(["--cpus", &format!("{:.2}", l.cpu_pct.unwrap_or(0.0) / 100.0)]);
                c.arg("--").arg(id);
                last = c.output();
                if last.as_ref().is_ok_and(|o| o.status.success()) {
                    break;
                }
            }
            last
        }
    };
    let o = out.map_err(|e| e.to_string())?;
    if o.status.success() {
        Ok(())
    } else {
        let e = String::from_utf8_lossy(&o.stderr).trim().to_string();
        Err(if e.is_empty() { format!("failed ({})", o.status) } else { e })
    }
}

/// Start a new instance of an app inside its own limited user scope. The scope is named
/// `app-taskd-…` so Taskd groups it as an app (and its limits can be changed later).
pub fn launch_limited(exec: &[String], name: &str, l: &Limits) -> Result<(), String> {
    if exec.is_empty() {
        return Err("don't know how to start this app".into());
    }
    let slug: String = name.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_lowercase();
    let unit = format!("app-taskd-{slug}-{}.scope", std::process::id() ^ (crate::recorder::epoch() as u32));
    let mut c = Command::new(sys_bin("systemd-run"));
    c.args(["--user", "--scope", "--collect", "--quiet", &format!("--unit={unit}")]);
    for p in unit_properties(l) {
        if !p.ends_with('=') {
            c.arg("-p").arg(p);
        }
    }
    c.arg("--").args(exec);
    c.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    // The scope lives as long as the app; don't wait for it.
    c.spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// Split a desktop entry `Exec=` line into argv, dropping field codes (%u, %F, …).
pub fn exec_argv(exec: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let (mut quoted, mut any) = (false, false);
    let mut chars = exec.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            '\\' if quoted => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            c if c.is_whitespace() && !quoted => {
                if any || !cur.is_empty() {
                    args.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        args.push(cur);
    }
    args.into_iter().filter(|a| !(a.len() == 2 && a.starts_with('%'))).map(|a| a.replace("%%", "%")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing() {
        assert_eq!(parse_mem("infinity"), None);
        assert_eq!(parse_mem("4294967296"), Some(4_294_967_296));
        assert_eq!(parse_quota("infinity"), None);
        assert_eq!(parse_quota("1.500000s"), Some(150.0));
        assert_eq!(parse_quota("500ms"), Some(50.0));
        let l = Limits { memory: Some(1 << 30), cpu_pct: Some(150.0) };
        assert_eq!(unit_properties(&l), vec!["MemoryMax=1073741824", "CPUQuota=150%"]);
        assert_eq!(unit_properties(&Limits::default()), vec!["MemoryMax=infinity", "CPUQuota="]);
    }

    #[test]
    fn exec_lines() {
        assert_eq!(exec_argv("firefox %u"), vec!["firefox"]);
        assert_eq!(exec_argv("/usr/bin/code --unity-launch %F"), vec!["/usr/bin/code", "--unity-launch"]);
        assert_eq!(exec_argv("\"/opt/My App/run\" --x"), vec!["/opt/My App/run", "--x"]);
        assert_eq!(exec_argv("env FOO=1 app"), vec!["env", "FOO=1", "app"]);
    }

    #[test]
    fn limits_a_real_user_scope() {
        // Needs a user systemd instance (skipped quietly in containers without one).
        let unit = format!("taskd-test-{}.scope", std::process::id());
        let Ok(mut child) = Command::new(sys_bin("systemd-run"))
            .args(["--user", "--scope", "--quiet", &format!("--unit={unit}"), "sleep", "20"])
            .spawn()
        else {
            return;
        };
        std::thread::sleep(std::time::Duration::from_millis(800));
        let t = Target::UserUnit(unit.clone());
        if apply(&t, &Limits { memory: Some(256 << 20), cpu_pct: Some(50.0) }, false).is_ok() {
            let now = current(&t);
            assert_eq!(now.memory, Some(256 << 20));
            assert_eq!(now.cpu_pct, Some(50.0));
            apply(&t, &Limits::default(), false).unwrap();
            assert_eq!(current(&t), Limits::default());
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}
