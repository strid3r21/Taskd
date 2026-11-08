//! Groups processes into apps, services and lone processes.
//!
//! * **Apps**: processes in a systemd app scope (`app-<launcher>-<id>-<n>.scope`, used by
//!   VS Code, Flatpaks, GNOME/KDE launches), or whose executable matches an installed
//!   `.desktop` entry (apps launched from menus that don't get a scope, e.g. on Cinnamon).
//!   Child processes inherit their parent's app.
//! * **Services**: processes in a systemd `.service` cgroup (system or user).
//! * Everything else stays an individual process.
//!
//! Computed on the UI thread only while a tab that needs it is visible, once per snapshot.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::sampler::{ProcInfo, Snapshot};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    App,
    Service,
    Container,
    Vm,
    Process,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::App => "App",
            Kind::Service => "Service",
            Kind::Container => "Container",
            Kind::Vm => "VM",
            Kind::Process => "Process",
        }
    }
}

pub struct Group {
    pub key: String,
    pub name: String,
    pub kind: Kind,
    /// Indices into `Snapshot::procs`, in pid order.
    pub members: Vec<u32>,
    /// Whether any member belongs to the current user.
    pub mine: bool,
    pub cpu: f32,
    pub mem: u64,
    pub gpu: f32,
    pub read: f64,
    pub write: f64,
    /// Network bytes/s, received + sent (needs the helper).
    pub net: f64,
}

// ------------------------------------------------------------------ desktop entries

struct DesktopEntry {
    id: String,
    name: String,
    exec: Option<String>,
}

#[derive(Default)]
pub struct DesktopIndex {
    entries: Vec<DesktopEntry>,
    by_exec: HashMap<String, usize>,
    by_id: HashMap<String, usize>,
}

/// Launchers/interpreters: an entry whose Exec starts with one of these can't be matched by binary name.
const GENERIC: &[&str] = &[
    "sh", "bash", "dash", "zsh", "env", "python", "python3", "perl", "ruby", "java", "node", "gjs", "flatpak", "snap",
    "electron", "wine", "exo-open", "xdg-open", "gio", "pkexec", "sudo", "nice", "ionice",
];

impl DesktopIndex {
    pub fn load() -> Self {
        let mut idx = DesktopIndex::default();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".local/share")));
        let mut dirs: Vec<PathBuf> = Vec::new();
        // Highest priority first: the first entry seen for an id wins.
        if let Some(d) = &data_home {
            dirs.push(d.join("applications"));
            dirs.push(d.join("flatpak/exports/share/applications"));
        }
        for d in std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into()).split(':') {
            if !d.is_empty() {
                dirs.push(PathBuf::from(d).join("applications"));
            }
        }
        dirs.push("/var/lib/flatpak/exports/share/applications".into());
        dirs.push("/var/lib/snapd/desktop/applications".into());

        for dir in dirs {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let file = e.file_name().to_string_lossy().into_owned();
                let Some(stem) = file.strip_suffix(".desktop") else { continue };
                if idx.by_id.contains_key(&stem.to_lowercase()) {
                    continue;
                }
                if let Ok(text) = std::fs::read_to_string(e.path()) {
                    idx.add(stem, &text);
                }
            }
        }
        idx
    }

    fn add(&mut self, stem: &str, text: &str) {
        let (mut name, mut exec, mut wmclass) = (None, None, None);
        let (mut visible, mut app, mut terminal) = (true, false, false);
        let mut in_main = false;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                in_main = line == "[Desktop Entry]";
                continue;
            }
            let Some((k, v)) = line.split_once('=').filter(|_| in_main) else { continue };
            let v = v.trim();
            match k.trim() {
                "Name" => name = Some(v.to_string()),
                "Exec" => exec = Some(v.to_string()),
                "StartupWMClass" => wmclass = Some(v.to_lowercase()),
                "Type" => app = v == "Application",
                "Terminal" => terminal = v == "true",
                "NoDisplay" | "Hidden" if v == "true" => visible = false,
                _ => {}
            }
        }
        let (Some(name), true) = (name, app) else { return };
        let i = self.entries.len();
        self.entries.push(DesktopEntry { id: stem.to_string(), name, exec: exec.clone() });
        self.by_id.insert(stem.to_lowercase(), i);
        if let Some(w) = wmclass {
            self.by_id.entry(w).or_insert(i);
        }
        // Entries that open a URL through another program (e.g. Steam game shortcuts:
        // `steam steam://rungameid/…`) describe the target, not the running binary.
        let launches_url = exec.as_deref().is_some_and(|e| e.contains("://"));
        if visible
            && !terminal
            && !launches_url
            && let Some(bin) = exec.as_deref().and_then(exec_binary)
        {
            // Prefer the entry named after the binary (steam.desktop for `steam`).
            let named_after = stem.eq_ignore_ascii_case(&bin)
                || stem.rsplit('.').next().is_some_and(|last| last.eq_ignore_ascii_case(&bin));
            if named_after {
                self.by_exec.insert(bin, i);
            } else {
                self.by_exec.entry(bin).or_insert(i);
            }
        }
    }

    fn find_exec(&self, bin: &str) -> Option<usize> {
        self.by_exec.get(bin).copied()
    }

    fn find_id(&self, id: &str) -> Option<usize> {
        let id = id.to_lowercase();
        self.by_id.get(&id).copied().or_else(|| {
            // e.g. "org.gnome.Nautilus" vs "nautilus.desktop"
            id.rsplit('.').next().and_then(|last| self.by_id.get(last).copied())
        })
    }
}

/// The program an `Exec=` line runs, as a basename (`None` for generic launchers).
fn exec_binary(exec: &str) -> Option<String> {
    let tok = exec
        .split_whitespace()
        .map(|t| t.trim_matches(|c| c == '"' || c == '\''))
        .find(|t| *t != "env" && !t.contains('='))?;
    let base = tok.rsplit('/').next()?;
    (!base.is_empty() && !GENERIC.contains(&base)).then(|| base.to_string())
}

/// `app-flatpak-com.discordapp.Discord-3117941.scope` -> `com.discordapp.Discord`.
pub fn app_id_from_unit(unit: &str) -> Option<String> {
    let s = unit.strip_prefix("app-")?;
    let s = s.strip_suffix(".scope").or_else(|| s.strip_suffix(".service"))?;
    let s = s.replace("\\x2d", "-");
    let mut s = s.as_str();
    // Trailing instance suffix: pid or random hex.
    if let Some((head, tail)) = s.rsplit_once('-')
        && !tail.is_empty()
        && tail.chars().all(|c| c.is_ascii_hexdigit())
    {
        s = head;
    }
    for launcher in ["flatpak-", "gnome-", "kde-", "cinnamon-", "xfce-", "mate-", "snap-", "dbus-", "lxqt-"] {
        if let Some(rest) = s.strip_prefix(launcher)
            && !rest.is_empty()
        {
            s = rest;
            break;
        }
    }
    (!s.is_empty()).then(|| s.to_string())
}

fn argv0_base(cmd: &str) -> &str {
    cmd.split(' ').next().unwrap_or("").rsplit('/').next().unwrap_or("")
}

// ------------------------------------------------------------------ grouping

#[derive(Clone)]
enum Tag {
    Scope(Arc<str>),
    Desktop(usize),
    Service(Arc<str>),
    /// Docker/Podman container: full container id.
    Container(String),
    /// libvirt/nspawn machine: its name.
    Vm(String),
}

/// `docker-<id>.scope` / `libpod-<id>.scope` -> container id.
fn container_id(unit: &str) -> Option<&str> {
    let s = unit.strip_suffix(".scope")?;
    s.strip_prefix("docker-").or_else(|| s.strip_prefix("libpod-")).filter(|id| !id.starts_with("conmon"))
}

/// `machine-qemu\x2d1\x2dwin11.scope` -> `win11`; `machine-foo.scope` -> `foo`.
fn machine_name(unit: &str) -> Option<String> {
    let s = unit.strip_prefix("machine-")?.strip_suffix(".scope")?.replace("\\x2d", "-");
    // libvirt names scopes qemu-<n>-<name>.
    let name = match s.strip_prefix("qemu-").and_then(|r| r.split_once('-')) {
        Some((n, rest)) if n.chars().all(|c| c.is_ascii_digit()) => rest.to_string(),
        _ => s,
    };
    (!name.is_empty()).then_some(name)
}

/// Container names from `docker ps` / `podman ps`, refreshed in the background (the CLIs take
/// tens of milliseconds and may hang if a daemon is down, so never on the UI thread).
#[derive(Default, Clone)]
pub struct ContainerNames(Arc<std::sync::Mutex<NameCache>>);

#[derive(Default)]
struct NameCache {
    names: HashMap<String, String>,
    refreshed: Option<std::time::Instant>,
}

impl ContainerNames {
    fn get(&self, id: &str) -> Option<String> {
        self.0.lock().ok()?.names.get(id).cloned()
    }

    /// Kick off a refresh if containers are present and the cache is older than 15 s.
    fn refresh_if_stale(&self) {
        let Ok(mut g) = self.0.lock() else { return };
        if g.refreshed.is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(15)) {
            return;
        }
        g.refreshed = Some(std::time::Instant::now());
        let me = self.clone();
        std::thread::spawn(move || me.refresh_now());
    }

    /// Query docker/podman now (blocking).
    fn refresh_now(&self) {
        {
            let mut names = HashMap::new();
            for tool in ["docker", "podman"] {
                let Ok(out) = std::process::Command::new(crate::actions::sys_bin(tool))
                    .args(["ps", "--no-trunc", "--format", "{{.ID}} {{.Names}}"])
                    .stderr(std::process::Stdio::null())
                    .output()
                else {
                    continue;
                };
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    if let Some((id, name)) = line.split_once(' ') {
                        names.insert(id.to_string(), name.to_string());
                    }
                }
            }
            if let Ok(mut g) = self.0.lock() {
                g.names = names;
                g.refreshed = Some(std::time::Instant::now());
            }
        }
    }
}

#[derive(Default)]
pub struct Grouper {
    index: Option<DesktopIndex>,
    generation: u64,
    pub groups: Vec<Group>,
    containers: ContainerNames,
}

impl Grouper {
    /// Resolve container names before the first grouping (for the background recorder, which
    /// can afford to wait and shouldn't record a container under its id first).
    /// The desktop entry `Exec=` line of an app group (`app:<desktop id>` keys).
    pub fn exec_for(&self, key: &str) -> Option<String> {
        let id = key.strip_prefix("app:")?;
        let idx = self.index.as_ref()?;
        idx.entries.iter().find(|e| e.id == id)?.exec.clone()
    }

    pub fn prime_container_names(&self) {
        self.containers.refresh_now();
    }

    pub fn update(&mut self, snap: &Snapshot, my_uid: u32) {
        if self.generation == snap.generation && !self.groups.is_empty() {
            return;
        }
        let index = self.index.get_or_insert_with(DesktopIndex::load);
        if snap.procs.iter().any(|p| p.unit.as_deref().is_some_and(|u| container_id(u).is_some())) {
            self.containers.refresh_if_stale();
        }
        self.groups = group(snap, my_uid, index, &self.containers);
        self.generation = snap.generation;
    }
}

fn own_tag(p: &ProcInfo, my_uid: u32, index: &DesktopIndex) -> Option<Tag> {
    if p.kernel {
        return None;
    }
    if let Some(u) = &p.unit {
        if let Some(id) = container_id(u) {
            return Some(Tag::Container(id.to_string()));
        }
        if let Some(name) = machine_name(u) {
            return Some(Tag::Vm(name));
        }
        return Some(if u.starts_with("app-") { Tag::Scope(u.clone()) } else { Tag::Service(u.clone()) });
    }
    if p.uid != my_uid {
        return None;
    }
    let argv0 = argv0_base(&p.cmd);
    [argv0, &p.name, p.name.trim_end_matches("-bin")]
        .into_iter()
        .filter(|c| !c.is_empty())
        .find_map(|c| index.find_exec(c))
        .map(Tag::Desktop)
}

fn group(snap: &Snapshot, my_uid: u32, index: &DesktopIndex, containers: &ContainerNames) -> Vec<Group> {
    let procs = &snap.procs;
    let by_pid: HashMap<u32, usize> = procs.iter().enumerate().map(|(i, p)| (p.pid, i)).collect();
    let own: Vec<Option<Tag>> = procs.iter().map(|p| own_tag(p, my_uid, index)).collect();

    // Resolve inherited tags: unit-less processes take their nearest tagged ancestor's tag.
    let mut resolved: Vec<Option<Option<Tag>>> = vec![None; procs.len()];
    let mut chain = Vec::new();
    for start in 0..procs.len() {
        let mut i = start;
        let result = loop {
            if let Some(r) = &resolved[i] {
                break r.clone();
            }
            if own[i].is_some() {
                break own[i].clone();
            }
            chain.push(i);
            let p = &procs[i];
            match by_pid.get(&p.ppid) {
                Some(&parent) if p.unit.is_none() && !p.kernel && p.ppid > 1 && chain.len() < 64 => i = parent,
                _ => break None,
            }
        };
        resolved[i] = Some(result.clone());
        for c in chain.drain(..) {
            resolved[c] = Some(result.clone());
        }
    }

    let mut groups: HashMap<String, Group> = HashMap::new();
    let mut scope_members: HashMap<Arc<str>, Vec<u32>> = HashMap::new();
    let mut add = |key: String, name: String, kind: Kind, i: u32| {
        groups
            .entry(key.clone())
            .or_insert_with(|| Group {
                key,
                name,
                kind,
                members: Vec::new(),
                mine: false,
                cpu: 0.0,
                mem: 0,
                gpu: 0.0,
                read: 0.0,
                write: 0.0,
                net: 0.0,
            })
            .members
            .push(i);
    };
    for (i, tag) in resolved.into_iter().enumerate() {
        let p = &procs[i];
        match tag.flatten() {
            Some(Tag::Desktop(d)) => {
                let e = &index.entries[d];
                add(format!("app:{}", e.id), e.name.clone(), Kind::App, i as u32);
            }
            Some(Tag::Scope(u)) => scope_members.entry(u).or_default().push(i as u32),
            Some(Tag::Container(id)) => {
                let name = containers.get(&id).unwrap_or_else(|| format!("container {}", &id[..id.len().min(12)]));
                add(format!("ctr:{id}"), name, Kind::Container, i as u32);
            }
            Some(Tag::Vm(name)) => add(format!("vm:{name}"), name, Kind::Vm, i as u32),
            Some(Tag::Service(u)) => {
                let name = u.trim_end_matches(".service").replace("\\x2d", "-");
                add(format!("svc:{u}"), name, Kind::Service, i as u32);
            }
            None => add(format!("pid:{}", p.pid), p.name.to_string(), Kind::Process, i as u32),
        }
    }
    // Name scope apps: by app id, else by the executable of their oldest process, else the id itself.
    for (unit, members) in scope_members {
        let id = app_id_from_unit(&unit).unwrap_or_else(|| unit.to_string());
        let entry = index.find_id(&id).or_else(|| {
            let oldest =
                members.iter().min_by(|&&a, &&b| procs[a as usize].start.total_cmp(&procs[b as usize].start))?;
            let p = &procs[*oldest as usize];
            index.find_exec(argv0_base(&p.cmd)).or_else(|| index.find_exec(&p.name))
        });
        let (key, name) = match entry {
            Some(d) => (format!("app:{}", index.entries[d].id), index.entries[d].name.clone()),
            None => (format!("app:{id}"), id.rsplit('.').next().unwrap_or(&id).to_string()),
        };
        for i in members {
            add(key.clone(), name.clone(), Kind::App, i);
        }
    }

    let mut out: Vec<Group> = groups.into_values().collect();
    for g in &mut out {
        g.members.sort_by_key(|&i| procs[i as usize].pid);
        for &i in &g.members {
            let p = &procs[i as usize];
            g.mine |= p.uid == my_uid;
            g.cpu += p.cpu;
            g.mem += p.mem;
            g.gpu += p.gpu;
            g.read += p.read_rate;
            g.write += p.write_rate;
            g.net += p.net_rx + p.net_tx;
        }
        g.gpu = g.gpu.min(100.0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_parsing() {
        assert_eq!(exec_binary("firefox %u").as_deref(), Some("firefox"));
        assert_eq!(exec_binary("/usr/bin/steam %U").as_deref(), Some("steam"));
        assert_eq!(exec_binary("env GDK_BACKEND=x11 /opt/foo/bar --x").as_deref(), Some("bar"));
        assert_eq!(exec_binary("sh -c 'echo hi'"), None);
        assert_eq!(exec_binary("flatpak run com.discordapp.Discord"), None);
    }

    #[test]
    fn app_ids() {
        assert_eq!(
            app_id_from_unit("app-flatpak-com.discordapp.Discord-3117941.scope").unwrap(),
            "com.discordapp.Discord"
        );
        assert_eq!(app_id_from_unit("app-com.microsoft.VSCode-3809464.scope").unwrap(), "com.microsoft.VSCode");
        assert_eq!(app_id_from_unit("app-gnome-org.gnome.Nautilus-4242.scope").unwrap(), "org.gnome.Nautilus");
        assert_eq!(app_id_from_unit("ssh.service"), None);
    }

    #[test]
    fn containers_and_vms() {
        assert_eq!(container_id("docker-ff0c.scope"), Some("ff0c"));
        assert_eq!(container_id("libpod-abc.scope"), Some("abc"));
        assert_eq!(container_id("libpod-conmon-abc.scope"), None);
        assert_eq!(machine_name("machine-qemu\\x2d1\\x2dwin11.scope").as_deref(), Some("win11"));
        assert_eq!(machine_name("machine-debian.scope").as_deref(), Some("debian"));
    }

    #[test]
    fn groups_live_system() {
        let mut s = crate::sampler::Sampler::new();
        let snap = s.sample();
        let uid = unsafe { libc::getuid() };
        let groups = group(&snap, uid, &DesktopIndex::load(), &ContainerNames::default());
        let total: usize = groups.iter().map(|g| g.members.len()).sum();
        assert_eq!(total, snap.procs.len(), "every process belongs to exactly one group");
    }
}
