//! Automatic rules, applied by the background recorder whenever a matching app is running:
//! "always run X at low priority", "always cap Y at 4 GB / 50% CPU", and game mode
//! ("while a game runs, give my other apps less CPU").
//!
//! Priority uses cgroup CPU/IO weights on the app's own systemd scope, which (unlike nice
//! values) can be raised again without root. Apps that aren't in their own scope fall back
//! to `nice` for "low" (raising priority there needs root, so "high" is skipped).

use std::collections::{HashMap, HashSet};
use std::process::Command;

use crate::actions::sys_bin;
use crate::groups::{Grouper, Kind};
use crate::limits::{Limits, Target};
use crate::sampler::Snapshot;

pub const LOW_WEIGHT: u32 = 20;
pub const HIGH_WEIGHT: u32 = 500;
const DEFAULT_WEIGHT: u32 = 100;

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// true = low priority, false = high.
    Priority(bool),
    /// Memory cap in GB.
    Memory(f64),
    /// CPU cap in percent of one core.
    Cpu(f64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub app: String,
    pub action: Action,
}

/// `priority:low:Firefox Web Browser;memory:4:Steam;cpu:50:Code` (name last; it may contain ':').
pub fn parse(s: &str) -> Vec<Rule> {
    s.split(';')
        .filter_map(|e| {
            let mut it = e.splitn(3, ':');
            let (kind, val, app) = (it.next()?, it.next()?, it.next()?.trim());
            if app.is_empty() {
                return None;
            }
            let action = match kind {
                "priority" => Action::Priority(val != "high"),
                "memory" => Action::Memory(val.parse().ok().filter(|v: &f64| *v > 0.0)?),
                "cpu" => Action::Cpu(val.parse().ok().filter(|v: &f64| *v > 0.0)?),
                _ => return None,
            };
            Some(Rule { app: app.to_string(), action })
        })
        .collect()
}

pub fn encode(rules: &[Rule]) -> String {
    rules
        .iter()
        .filter(|r| !r.app.trim().is_empty())
        .map(|r| {
            let app = r.app.replace([';', '\n'], " ");
            match r.action {
                Action::Priority(low) => format!("priority:{}:{app}", if low { "low" } else { "high" }),
                Action::Memory(gb) => format!("memory:{gb}:{app}"),
                Action::Cpu(p) => format!("cpu:{p}:{app}"),
            }
        })
        .collect::<Vec<_>>()
        .join(";")
}

pub fn names(list: &str) -> Vec<String> {
    list.split([',', ';']).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// What one target (unit / container / process) should have.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Desired {
    pub weight: Option<u32>,
    pub limits: Limits,
    /// For log messages.
    pub app: String,
}

impl Desired {
    fn signature(&self) -> String {
        format!("{:?}/{:?}/{:?}", self.weight, self.limits.memory, self.limits.cpu_pct)
    }
}

/// Where an app runs, for applying rules.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Place {
    /// A unit of the user's systemd (scope or service).
    Unit(String),
    Container(String),
    /// A process outside any unit of its own (nice only).
    Pid(u32),
}

/// One running app as the rules see it.
pub struct AppInfo {
    pub name: String,
    pub kind: Kind,
    pub places: Vec<Place>,
}

/// Running apps that belong to the user, with where they run.
pub fn running_apps(s: &Snapshot, g: &Grouper, uid: u32) -> Vec<AppInfo> {
    g.groups
        .iter()
        .filter(|x| x.mine && matches!(x.kind, Kind::App | Kind::Container | Kind::Process | Kind::Service))
        .map(|x| {
            let mut places: Vec<Place> = Vec::new();
            if let Some(id) = x.key.strip_prefix("ctr:") {
                places.push(Place::Container(id.to_string()));
            } else {
                for &m in &x.members {
                    let p = &s.procs[m as usize];
                    if p.uid != uid {
                        continue;
                    }
                    let own_unit = p
                        .unit
                        .as_deref()
                        .filter(|u| (u.ends_with(".scope") || u.ends_with(".service")) && !u.starts_with("session-"))
                        .filter(|u| !u.starts_with("user@") && !u.starts_with("init.scope"));
                    let place = match own_unit {
                        Some(u) => Place::Unit(u.to_string()),
                        None => Place::Pid(p.pid),
                    };
                    if !places.contains(&place) {
                        places.push(place);
                    }
                }
            }
            AppInfo { name: x.name.clone(), kind: x.kind, places }
        })
        .collect()
}

/// Work out what every place should have from the rules and game mode (pure; tested).
pub fn desired(apps: &[AppInfo], rules: &[Rule], game_mode: bool, games: &[String]) -> HashMap<Place, Desired> {
    let is = |a: &str, b: &str| a.eq_ignore_ascii_case(b.trim());
    let mut out: HashMap<Place, Desired> = HashMap::new();
    let game_running = game_mode && apps.iter().any(|a| games.iter().any(|g| is(&a.name, g)));
    for a in apps {
        let mine: Vec<&Rule> = rules.iter().filter(|r| is(&a.name, &r.app)).collect();
        let mut d = Desired { app: a.name.clone(), ..Default::default() };
        for r in &mine {
            match r.action {
                Action::Priority(low) => d.weight = Some(if low { LOW_WEIGHT } else { HIGH_WEIGHT }),
                Action::Memory(gb) => d.limits.memory = Some((gb * 1_073_741_824.0) as u64),
                Action::Cpu(p) => d.limits.cpu_pct = Some(p),
            }
        }
        // Game mode: other desktop apps step back, unless a rule says otherwise.
        let is_game = games.iter().any(|g| is(&a.name, g));
        if game_running && !is_game && a.kind == Kind::App && d.weight.is_none() {
            d.weight = Some(LOW_WEIGHT);
        }
        if d.weight.is_none() && d.limits == Limits::default() {
            continue;
        }
        for p in &a.places {
            out.insert(p.clone(), d.clone());
        }
    }
    out
}

/// Applies `desired` incrementally: only changes are sent to systemd/docker, and places
/// whose rule went away are put back to normal.
#[derive(Default)]
pub struct Engine {
    applied: HashMap<Place, Desired>,
    /// Whether a unit is the user's (cached: `systemctl show` per tick would be wasteful).
    user_unit: HashMap<String, bool>,
    game_was_on: bool,
}

impl Engine {
    /// Apply rules for this sample; returns log lines for what changed.
    pub fn tick(&mut self, s: &Snapshot, g: &Grouper, uid: u32, cfg: &crate::config::Config) -> Vec<String> {
        let rules = parse(&cfg.rules);
        let games = names(&cfg.game_apps);
        if rules.is_empty() && !cfg.game_mode && self.applied.is_empty() {
            return Vec::new();
        }
        let apps = running_apps(s, g, uid);
        let want = desired(&apps, &rules, cfg.game_mode, &games);
        let mut log = Vec::new();
        let game_on = cfg.game_mode && apps.iter().any(|a| games.iter().any(|x| a.name.eq_ignore_ascii_case(x)));
        if game_on != self.game_was_on {
            log.push(if game_on { "Game mode on".to_string() } else { "Game mode off".to_string() });
            self.game_was_on = game_on;
        }
        let alive: HashSet<&Place> = apps.iter().flat_map(|a| a.places.iter()).collect();
        // Put back places whose rule went away (if they're still running).
        let gone: Vec<Place> = self.applied.keys().filter(|p| !want.contains_key(*p)).cloned().collect();
        for p in gone {
            let old = self.applied.remove(&p).unwrap_or_default();
            if alive.contains(&p) {
                match self.apply(&p, &Desired::default()) {
                    Ok(()) => log.push(format!("{}: back to normal", old.app)),
                    Err(e) => log.push(format!("{}: couldn't reset: {e}", old.app)),
                }
            }
        }
        for (p, d) in want {
            if self.applied.get(&p).is_some_and(|a| a.signature() == d.signature()) {
                continue;
            }
            match self.apply(&p, &d) {
                Ok(()) => log.push(format!("{}: {}", d.app, describe(&d))),
                Err(e) => log.push(format!("{}: {e}", d.app)),
            }
            // Remember failures too, so a refused change isn't retried every few seconds.
            self.applied.insert(p, d);
        }
        // Forget places that no longer exist (their scopes are gone).
        self.applied.retain(|p, _| alive.contains(p));
        log
    }

    fn is_user_unit(&mut self, u: &str) -> bool {
        *self.user_unit.entry(u.to_string()).or_insert_with(|| {
            Command::new(sys_bin("systemctl"))
                .args(["--user", "show", "-p", "LoadState", "--value", "--", u])
                .output()
                .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "loaded")
        })
    }

    fn apply(&mut self, p: &Place, d: &Desired) -> Result<(), String> {
        match p {
            Place::Unit(u) => {
                if !self.is_user_unit(u) {
                    return Err("system services need a password; rules only change your own apps".into());
                }
                let w = d.weight.unwrap_or(DEFAULT_WEIGHT);
                let ok = Command::new(sys_bin("systemctl"))
                    .args(["--user", "set-property", "--runtime", "--", u])
                    .arg(format!("CPUWeight={w}"))
                    .arg(format!("IOWeight={w}"))
                    .status()
                    .is_ok_and(|s| s.success());
                if !ok {
                    return Err(format!("couldn't set the priority of {u}"));
                }
                crate::limits::apply(&Target::UserUnit(u.clone()), &d.limits, false)
            }
            Place::Container(id) => {
                let shares = match d.weight {
                    Some(LOW_WEIGHT) => 256,
                    Some(_) => 4096,
                    None => 1024,
                };
                for tool in ["docker", "podman"] {
                    if Command::new(sys_bin(tool))
                        .args(["update", "--cpu-shares", &shares.to_string(), "--", id])
                        .output()
                        .is_ok_and(|o| o.status.success())
                    {
                        break;
                    }
                }
                crate::limits::apply(&Target::Container(id.clone()), &d.limits, false)
            }
            Place::Pid(pid) => {
                // Without a unit only "low" is possible (raising priority needs root).
                if d.weight == Some(LOW_WEIGHT) {
                    // SAFETY: plain syscalls on a pid; failure is reported via errno.
                    unsafe {
                        let cur = libc::getpriority(libc::PRIO_PROCESS, *pid);
                        if cur < 10 && libc::setpriority(libc::PRIO_PROCESS, *pid, 10) != 0 {
                            return Err(format!("couldn't lower the priority of process {pid}"));
                        }
                    }
                }
                if d.limits != Limits::default() {
                    return Err(
                        "memory/CPU caps need the app to run in its own scope (start it from your app menu)".into()
                    );
                }
                Ok(())
            }
        }
    }
}

pub fn describe(d: &Desired) -> String {
    let mut parts = Vec::new();
    match d.weight {
        Some(LOW_WEIGHT) => parts.push("low priority".to_string()),
        Some(_) => parts.push("high priority".to_string()),
        None => {}
    }
    if let Some(m) = d.limits.memory {
        parts.push(format!("memory capped at {}", crate::util::fmt_bytes(m)));
    }
    if let Some(c) = d.limits.cpu_pct {
        parts.push(format!("CPU capped at {c:.0}%"));
    }
    if parts.is_empty() { "normal".into() } else { parts.join(", ") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, kind: Kind, places: Vec<Place>) -> AppInfo {
        AppInfo { name: name.into(), kind, places }
    }

    #[test]
    fn round_trip() {
        let r =
            parse("priority:low:Firefox Web Browser;memory:4:Steam;cpu:50:a:b;bogus:1:x;memory:-1:y;priority:high:");
        assert_eq!(
            r,
            vec![
                Rule { app: "Firefox Web Browser".into(), action: Action::Priority(true) },
                Rule { app: "Steam".into(), action: Action::Memory(4.0) },
                Rule { app: "a:b".into(), action: Action::Cpu(50.0) },
            ]
        );
        assert_eq!(parse(&encode(&r)), r);
        assert_eq!(names(" Steam, Heroic ;"), vec!["Steam", "Heroic"]);
    }

    #[test]
    fn rules_and_game_mode() {
        let ff = Place::Unit("app-firefox-1.scope".into());
        let steam = Place::Unit("app-steam-2.scope".into());
        let code = Place::Unit("app-code-3.scope".into());
        let apps = vec![
            app("Firefox", Kind::App, vec![ff.clone()]),
            app("Steam", Kind::App, vec![steam.clone()]),
            app("Code", Kind::App, vec![code.clone(), Place::Pid(7)]),
            app("sshd", Kind::Service, vec![Place::Unit("sshd.service".into())]),
        ];
        let rules = parse("memory:2:firefox;priority:high:Code");
        // No game running: only the rules.
        let d = desired(&apps, &rules, true, &["Heroic".into()]);
        assert_eq!(d.len(), 3, "{d:?}");
        assert_eq!(d[&ff].limits.memory, Some(2 << 30));
        assert_eq!(d[&ff].weight, None);
        assert_eq!(d[&Place::Pid(7)].weight, Some(HIGH_WEIGHT));
        // Steam counts as a game: other apps step back, rules win, services are left alone.
        let d = desired(&apps, &rules, true, &["steam".into()]);
        assert_eq!(d[&ff].weight, Some(LOW_WEIGHT));
        assert_eq!(d[&code].weight, Some(HIGH_WEIGHT));
        assert!(!d.contains_key(&steam) && !d.contains_key(&Place::Unit("sshd.service".into())));
        // Game mode off: same as no game.
        assert_eq!(desired(&apps, &rules, false, &["steam".into()]).get(&ff).and_then(|x| x.weight), None);
    }

    #[test]
    fn applies_weight_to_a_real_user_scope() {
        // Needs a user systemd instance (skipped quietly without one).
        let unit = format!("taskd-rules-test-{}.scope", std::process::id());
        let Ok(mut child) = Command::new(sys_bin("systemd-run"))
            .args(["--user", "--scope", "--quiet", &format!("--unit={unit}"), "sleep", "20"])
            .spawn()
        else {
            return;
        };
        std::thread::sleep(std::time::Duration::from_millis(800));
        let show = || {
            Command::new(sys_bin("systemctl"))
                .args(["--user", "show", "-p", "CPUWeight", "--value", "--", &unit])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default()
        };
        let mut e = Engine::default();
        let p = Place::Unit(unit.clone());
        if e.is_user_unit(&unit) {
            let low = Desired { weight: Some(LOW_WEIGHT), app: "t".into(), ..Default::default() };
            e.apply(&p, &low).unwrap();
            assert_eq!(show(), "20");
            e.apply(&p, &Desired::default()).unwrap();
            assert_eq!(show(), "100");
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}
