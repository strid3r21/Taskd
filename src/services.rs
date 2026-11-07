//! systemd services, listed on demand via `systemctl` (never polled in the background).

use std::collections::HashMap;
use std::process::Command;

#[derive(Clone, Debug)]
pub struct Service {
    pub unit: String,
    pub load: String,
    pub active: String,
    pub sub: String,
    pub description: String,
    pub enabled: String,
}

pub fn list(user: bool) -> Result<Vec<Service>, String> {
    let scope: &[&str] = if user { &["--user"] } else { &[] };
    let units = run(scope, &["list-units", "--type=service", "--all", "--no-legend", "--no-pager", "--plain"])?;
    let files = run(scope, &["list-unit-files", "--type=service", "--no-legend", "--no-pager"]).unwrap_or_default();

    let mut enabled: HashMap<&str, &str> = HashMap::new();
    for line in files.lines() {
        let mut it = line.split_whitespace();
        if let (Some(u), Some(state)) = (it.next(), it.next()) {
            enabled.insert(u, state);
        }
    }
    let mut out: Vec<Service> = units.lines().filter_map(|l| parse_unit_line(l, &enabled)).collect();
    // Unit files that are installed but not loaded (e.g. disabled) are useful to see too.
    for (u, state) in &enabled {
        if !u.contains('@') && !out.iter().any(|s| s.unit == *u) {
            out.push(Service {
                unit: u.to_string(),
                load: "not-loaded".into(),
                active: "inactive".into(),
                sub: "dead".into(),
                description: String::new(),
                enabled: state.to_string(),
            });
        }
    }
    out.sort_by_cached_key(|s| s.unit.to_lowercase());
    Ok(out)
}

fn parse_unit_line(line: &str, enabled: &HashMap<&str, &str>) -> Option<Service> {
    let line = line.trim_start_matches(['●', '*', ' ']);
    let mut it = line.split_whitespace();
    let unit = it.next()?.to_string();
    if !unit.ends_with(".service") {
        return None;
    }
    let load = it.next()?.to_string();
    let active = it.next()?.to_string();
    let sub = it.next()?.to_string();
    let description = it.collect::<Vec<_>>().join(" ");
    let enabled = enabled.get(unit.as_str()).copied().unwrap_or("-").to_string();
    Some(Service { unit, load, active, sub, description, enabled })
}

fn run(scope: &[&str], args: &[&str]) -> Result<String, String> {
    let out = Command::new("systemctl").args(scope).args(args).output().map_err(|e| format!("systemctl: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// start / stop / restart / enable / disable. System units go through polkit automatically.
pub fn control(user: bool, verb: &str, unit: &str) -> Result<(), String> {
    let mut cmd = Command::new("systemctl");
    if user {
        cmd.arg("--user");
    }
    let out = cmd.args([verb, unit]).output().map_err(|e| format!("systemctl: {e}"))?;
    if out.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units() {
        let mut en = HashMap::new();
        en.insert("ssh.service", "enabled");
        let s = parse_unit_line("● ssh.service loaded active running OpenBSD Secure Shell server", &en).unwrap();
        assert_eq!(s.unit, "ssh.service");
        assert_eq!(s.sub, "running");
        assert_eq!(s.enabled, "enabled");
        assert_eq!(s.description, "OpenBSD Secure Shell server");
        assert!(parse_unit_line("foo.socket loaded active listening x", &en).is_none());
    }
}
