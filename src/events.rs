//! "Why did it die?": crashes (systemd-coredump), out-of-memory kills (kernel and
//! systemd-oomd) and failed services, read from the systemd journal.

use std::io::BufRead;
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::actions::sys_bin;

const COREDUMP: &str = "MESSAGE_ID=fc2e22bc6ee647b6b90729ab34a250b1";
const UNIT_FAILED: &str = "MESSAGE_ID=d9b373ed55a64feb8242e02dbe79a49c";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Crash,
    /// The kernel ran out of memory and killed something.
    OomKill,
    /// A program hit its own memory limit (cgroup memory.max) and was killed.
    LimitKill,
    /// systemd-oomd ended something because memory pressure stayed too high.
    Oomd,
    ServiceFailed,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Crash => "Crash",
            Kind::OomKill => "Out of memory",
            Kind::LimitKill => "Memory limit",
            Kind::Oomd => "Memory pressure",
            Kind::ServiceFailed => "Service failed",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// Unix seconds.
    pub t: u64,
    pub kind: Kind,
    /// The program or unit it happened to.
    pub name: String,
    pub title: String,
    pub detail: String,
}

/// journalctl matches: coredumps OR unit failures OR kernel errors OR systemd-oomd.
fn matches() -> Vec<&'static str> {
    vec![
        COREDUMP,
        UNIT_FAILED,
        "+",
        "_TRANSPORT=kernel",
        "PRIORITY=3",
        "PRIORITY=4",
        "+",
        "_SYSTEMD_UNIT=systemd-oomd.service",
    ]
}

/// Journal fields can be strings or (for non-UTF-8 data) arrays of bytes.
fn field(e: &Value, k: &str) -> Option<String> {
    match &e[k] {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => {
            let bytes: Vec<u8> = a.iter().filter_map(|b| b.as_u64().map(|b| b as u8)).collect();
            Some(String::from_utf8_lossy(&bytes).into_owned())
        }
        _ => None,
    }
}

/// Text between `start` and the next `end` after it.
fn between<'a>(s: &'a str, start: &str, end: char) -> Option<&'a str> {
    let i = s.find(start)? + start.len();
    let rest = &s[i..];
    Some(&rest[..rest.find(end).unwrap_or(rest.len())])
}

/// `app-gnome-firefox-1234.scope` -> `firefox`; `foo.service` stays.
fn unit_name(u: &str) -> String {
    let base = u.rsplit('/').next().unwrap_or(u);
    if let Some(app) = base.strip_prefix("app-").and_then(|a| a.strip_suffix(".scope")) {
        let parts: Vec<&str> = app.split('-').collect();
        // app-<launcher>-<name>-<id>.scope or app-<name>-<id>.scope
        let keep: Vec<&str> =
            parts.iter().copied().filter(|p| !p.chars().all(|c| c.is_ascii_digit() || c == '@')).collect();
        if let Some(n) = keep.last() {
            return n.replace("\\x2d", "-");
        }
    }
    base.to_string()
}

pub fn parse(e: &Value) -> Option<Event> {
    let t = field(e, "__REALTIME_TIMESTAMP")?.parse::<u64>().ok()? / 1_000_000;
    let msg = field(e, "MESSAGE").unwrap_or_default();
    let first = msg.lines().next().unwrap_or("").to_string();
    let id = field(e, "MESSAGE_ID").unwrap_or_default();
    if COREDUMP.ends_with(&id) && !id.is_empty() {
        // "Process 1234 (name) of user 1000 dumped core."
        let name = field(e, "COREDUMP_COMM")
            .or_else(|| between(&first, "(", ')').map(String::from))
            .unwrap_or_else(|| "a program".into());
        let exe = field(e, "COREDUMP_EXE").unwrap_or_default();
        let sig = field(e, "COREDUMP_SIGNAL_NAME").map_or(String::new(), |s| format!(" ({s})"));
        let unit = field(e, "COREDUMP_USER_UNIT").or_else(|| field(e, "COREDUMP_UNIT")).unwrap_or_default();
        let mut detail = exe;
        if !unit.is_empty() && !unit.starts_with("user@") {
            detail += &format!("  ·  {unit}");
        }
        return Some(Event { t, kind: Kind::Crash, title: format!("{name} crashed{sig}"), name, detail });
    }
    if UNIT_FAILED.ends_with(&id) && !id.is_empty() {
        let unit = field(e, "UNIT").or_else(|| field(e, "USER_UNIT"))?;
        let result = field(e, "UNIT_RESULT").unwrap_or_default();
        return Some(Event {
            t,
            kind: Kind::ServiceFailed,
            title: format!("{unit} failed"),
            detail: if result.is_empty() { first } else { format!("result: {result}") },
            name: unit,
        });
    }
    if field(e, "_TRANSPORT").as_deref() == Some("kernel") {
        // "Out of memory: Killed process 4321 (firefox) total-vm:…, anon-rss:123456kB, …"
        // "Memory cgroup out of memory: Killed process …" when a memory limit was hit.
        if !first.contains("Killed process") {
            return None;
        }
        let name = between(&first, "(", ')').unwrap_or("a process").to_string();
        let rss = between(&first, "anon-rss:", 'k').and_then(|k| k.parse::<u64>().ok());
        let limit = first.starts_with("Memory cgroup");
        let mut detail =
            if limit { "it hit its memory limit".to_string() } else { "the system ran out of memory".to_string() };
        if let Some(k) = rss {
            detail += &format!("  ·  was using {}", crate::util::fmt_bytes(k * 1024));
        }
        return Some(Event {
            t,
            kind: if limit { Kind::LimitKill } else { Kind::OomKill },
            title: format!("{name} was killed (out of memory)"),
            name,
            detail,
        });
    }
    if field(e, "_SYSTEMD_UNIT").as_deref() == Some("systemd-oomd.service") && first.starts_with("Killed ") {
        // "Killed /user.slice/…/app-firefox-1234.scope due to memory pressure for … being 52% > 50% for > 20s …"
        let path = between(&first, "Killed ", ' ').unwrap_or("");
        let name = unit_name(path);
        let why = first.split_once(" due to ").map_or(String::new(), |(_, w)| format!("due to {w}"));
        return Some(Event {
            t,
            kind: Kind::Oomd,
            title: format!("{name} was closed by systemd-oomd"),
            name,
            detail: why,
        });
    }
    None
}

/// Events at or after `since` (Unix seconds), oldest first.
pub fn load(since: u64) -> Result<Vec<Event>, String> {
    let out = Command::new(sys_bin("journalctl"))
        .args(["-o", "json", "--no-pager", "-q", &format!("--since=@{since}")])
        .args(matches())
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("journalctl: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut v: Vec<Event> =
        text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter_map(|e| parse(&e)).collect();
    v.sort_by_key(|e| e.t);
    // systemd sometimes logs the same failure twice (unit + its manager); keep one.
    v.dedup_by(|a, b| a.kind == b.kind && a.name == b.name && a.t.abs_diff(b.t) <= 1);
    Ok(v)
}

/// Follow the journal and call `on` for each new event (blocks; run on its own thread).
pub fn follow(mut on: impl FnMut(Event)) {
    loop {
        let child = Command::new(sys_bin("journalctl"))
            .args(["-f", "-n0", "-o", "json", "--no-pager", "-q"])
            .args(matches())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else { return };
        if let Some(out) = child.stdout.take() {
            for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                if let Some(e) = serde_json::from_str::<Value>(&line).ok().as_ref().and_then(parse) {
                    on(e);
                }
            }
        }
        let _ = child.wait();
        // journalctl exited (journal rotated, daemon restarted…): try again shortly.
        std::thread::sleep(std::time::Duration::from_secs(30));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_each_kind() {
        let ts = "1700000000123456";
        let crash = json!({"__REALTIME_TIMESTAMP": ts, "MESSAGE_ID": "fc2e22bc6ee647b6b90729ab34a250b1",
            "MESSAGE": "Process 42 (WebKitWebProces) of user 1000 dumped core.\n\nStack…",
            "COREDUMP_EXE": "/usr/lib/webkit/WebKitWebProcess", "COREDUMP_SIGNAL_NAME": "SIGBUS"});
        let e = parse(&crash).unwrap();
        assert_eq!((e.t, e.kind, e.title.as_str()), (1_700_000_000, Kind::Crash, "WebKitWebProces crashed (SIGBUS)"));

        let failed = json!({"__REALTIME_TIMESTAMP": ts, "MESSAGE_ID": "d9b373ed55a64feb8242e02dbe79a49c",
            "USER_UNIT": "xdg-desktop-portal-gtk.service", "UNIT_RESULT": "exit-code",
            "MESSAGE": "xdg-desktop-portal-gtk.service: Failed with result 'exit-code'."});
        let e = parse(&failed).unwrap();
        assert_eq!(
            (e.kind, e.name.as_str(), e.detail.as_str()),
            (Kind::ServiceFailed, "xdg-desktop-portal-gtk.service", "result: exit-code")
        );

        let oom = json!({"__REALTIME_TIMESTAMP": ts, "_TRANSPORT": "kernel",
            "MESSAGE": "Out of memory: Killed process 4321 (firefox) total-vm:9000kB, anon-rss:2097152kB, file-rss:0kB"});
        let e = parse(&oom).unwrap();
        assert_eq!((e.kind, e.name.as_str()), (Kind::OomKill, "firefox"));
        assert!(e.detail.ends_with("was using 2.00 GiB"), "{}", e.detail);

        let limit = json!({"__REALTIME_TIMESTAMP": ts, "_TRANSPORT": "kernel",
            "MESSAGE": "Memory cgroup out of memory: Killed process 77 (node) total-vm:1kB, anon-rss:1024kB"});
        assert_eq!(parse(&limit).unwrap().kind, Kind::LimitKill);

        let oomd = json!({"__REALTIME_TIMESTAMP": ts, "_SYSTEMD_UNIT": "systemd-oomd.service",
            "MESSAGE": "Killed /user.slice/user-1000.slice/user@1000.service/app.slice/app-gnome-firefox-1234.scope due to memory pressure for /user.slice being 52.13% > 50.00% for > 20s with reclaim activity"});
        let e = parse(&oomd).unwrap();
        assert_eq!((e.kind, e.name.as_str()), (Kind::Oomd, "firefox"));
        assert!(e.detail.starts_with("due to memory pressure"));

        let noise =
            json!({"__REALTIME_TIMESTAMP": ts, "_TRANSPORT": "kernel", "MESSAGE": "CIFS: VFS: reconnect failed"});
        assert!(parse(&noise).is_none());
        // Non-UTF-8 journal fields arrive as byte arrays.
        let bytes = json!({"__REALTIME_TIMESTAMP": ts, "MESSAGE_ID": "fc2e22bc6ee647b6b90729ab34a250b1",
            "MESSAGE": [80,114,111,99,101,115,115,32,49,32,40,120,41]});
        assert_eq!(parse(&bytes).unwrap().name, "x");
    }

    #[test]
    fn loads_from_this_journal() {
        // Works wherever journalctl exists; an empty result is fine.
        if let Ok(v) = load(crate::recorder::epoch().saturating_sub(7 * 86_400)) {
            assert!(v.windows(2).all(|w| w[0].t <= w[1].t));
        }
    }
}
