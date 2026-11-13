//! Global keyboard shortcut to open Taskd (Ctrl+Shift+Esc by default), registered with the
//! desktop's own custom-shortcut settings: Cinnamon and GNOME (gsettings), KDE
//! (kglobalshortcutsrc) and Xfce (xfconf). Plus single-instance: a second launch (e.g. from
//! the shortcut) just brings the running window to the front.

use std::io::{BufRead, BufReader, Write};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::process::Command;

use crate::actions::sys_bin;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Desktop {
    Cinnamon,
    Gnome,
    Kde,
    Xfce,
    Other,
}

pub fn desktop() -> Desktop {
    let d = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_lowercase();
    if d.contains("cinnamon") {
        Desktop::Cinnamon
    } else if d.contains("gnome") || d.contains("unity") || d.contains("budgie") {
        Desktop::Gnome
    } else if d.contains("kde") {
        Desktop::Kde
    } else if d.contains("xfce") {
        Desktop::Xfce
    } else {
        Desktop::Other
    }
}

/// Offered key combinations: (label, GTK accelerator, KDE form).
pub const CHOICES: [(&str, &str, &str); 2] =
    [("Ctrl+Shift+Esc", "<Primary><Shift>Escape", "Ctrl+Shift+Esc"), ("Super+Esc", "<Super>Escape", "Meta+Esc")];

pub fn label_for(accel: &str) -> String {
    CHOICES.iter().find(|c| c.1 == accel || c.2 == accel).map_or_else(|| accel.to_string(), |c| c.0.to_string())
}

/// Runs gsettings & co. (tests point them at a throwaway settings backend through `env`).
struct Runner {
    env: Vec<(String, String)>,
}

impl Runner {
    fn out(&self, prog: &str, args: &[&str]) -> Result<String, String> {
        let o = Command::new(sys_bin(prog))
            .args(args)
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .output()
            .map_err(|e| format!("{prog}: {e}"))?;
        if o.status.success() {
            Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
        } else {
            Err(format!("{prog}: {}", String::from_utf8_lossy(&o.stderr).trim()))
        }
    }
}

/// gsettings array (`['a', 'b']`, `@as []`) -> items.
fn parse_list(s: &str) -> Vec<String> {
    s.trim_start_matches("@as")
        .trim()
        .trim_matches(['[', ']'])
        .split(',')
        .map(|x| x.trim().trim_matches('\'').to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

fn fmt_list(v: &[String]) -> String {
    format!("[{}]", v.iter().map(|x| format!("'{x}'")).collect::<Vec<_>>().join(", "))
}

/// Per-desktop gsettings layout of custom shortcuts.
struct Gs {
    list_schema: &'static str,
    list_key: &'static str,
    item_schema: &'static str,
    base: &'static str,
    /// Cinnamon lists names and stores `binding` as an array; GNOME lists paths and a string.
    by_path: bool,
}

const CINNAMON: Gs = Gs {
    list_schema: "org.cinnamon.desktop.keybindings",
    list_key: "custom-list",
    item_schema: "org.cinnamon.desktop.keybindings.custom-keybinding",
    base: "/org/cinnamon/desktop/keybindings/custom-keybindings/",
    by_path: false,
};
const GNOME: Gs = Gs {
    list_schema: "org.gnome.settings-daemon.plugins.media-keys",
    list_key: "custom-keybindings",
    item_schema: "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding",
    base: "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/",
    by_path: true,
};

impl Gs {
    fn path(&self, entry: &str) -> String {
        if self.by_path { entry.to_string() } else { format!("{}{entry}/", self.base) }
    }
    fn item(&self, entry: &str) -> String {
        format!("{}:{}", self.item_schema, self.path(entry))
    }

    /// Our entry: named "Taskd" or running taskdm. Returns (list entry, binding).
    fn find(&self, r: &Runner) -> Result<Option<(String, String)>, String> {
        for e in parse_list(&r.out("gsettings", &["get", self.list_schema, self.list_key])?) {
            let item = self.item(&e);
            let (Ok(name), Ok(cmd)) =
                (r.out("gsettings", &["get", &item, "name"]), r.out("gsettings", &["get", &item, "command"]))
            else {
                continue;
            };
            let cmd = cmd.trim_matches('\'');
            let ours = name.trim_matches('\'') == "Taskd"
                || cmd.rsplit('/').next().is_some_and(|b| b == "taskdm" || b.starts_with("taskdm "));
            if ours {
                let b = r.out("gsettings", &["get", &item, "binding"]).unwrap_or_default();
                let b = parse_list(&b).into_iter().next().unwrap_or_else(|| b.trim_matches('\'').to_string());
                return Ok(Some((e, b)));
            }
        }
        Ok(None)
    }

    fn set(&self, r: &Runner, accel: &str, command: &str) -> Result<(), String> {
        let mut list = parse_list(&r.out("gsettings", &["get", self.list_schema, self.list_key])?);
        let entry = match self.find(r)? {
            Some((e, _)) => e,
            None => {
                // Next free customN.
                let n = (0..)
                    .find(|n| !list.iter().any(|e| e.trim_end_matches('/').ends_with(&format!("custom{n}"))))
                    .unwrap_or(0);
                let e = if self.by_path { format!("{}custom{n}/", self.base) } else { format!("custom{n}") };
                list.push(e.clone());
                r.out("gsettings", &["set", self.list_schema, self.list_key, &fmt_list(&list)])?;
                e
            }
        };
        let item = self.item(&entry);
        r.out("gsettings", &["set", &item, "name", "Taskd"])?;
        r.out("gsettings", &["set", &item, "command", command])?;
        let binding = if self.by_path { accel.to_string() } else { fmt_list(&[accel.to_string()]) };
        r.out("gsettings", &["set", &item, "binding", &binding])?;
        Ok(())
    }

    fn remove(&self, r: &Runner) -> Result<(), String> {
        let Some((entry, _)) = self.find(r)? else { return Ok(()) };
        let list: Vec<String> = parse_list(&r.out("gsettings", &["get", self.list_schema, self.list_key])?)
            .into_iter()
            .filter(|e| *e != entry)
            .collect();
        let item = self.item(&entry);
        for k in ["name", "command", "binding"] {
            let _ = r.out("gsettings", &["reset", &item, k]);
        }
        r.out("gsettings", &["set", self.list_schema, self.list_key, &fmt_list(&list)]).map(|_| ())
    }
}

fn kwrite() -> Option<&'static str> {
    ["kwriteconfig6", "kwriteconfig5"].into_iter().find(|p| std::path::Path::new(&sys_bin(p)).is_absolute())
}

fn runner() -> Runner {
    Runner { env: Vec::new() }
}

/// The key combination that opens Taskd now, if one is set.
pub fn current() -> Option<String> {
    let r = runner();
    match desktop() {
        Desktop::Cinnamon => CINNAMON.find(&r).ok().flatten().map(|(_, b)| b),
        Desktop::Gnome => GNOME.find(&r).ok().flatten().map(|(_, b)| b),
        Desktop::Kde => {
            let read = kwrite()?.replace("write", "read");
            r.out(
                &read,
                &[
                    "--file",
                    "kglobalshortcutsrc",
                    "--group",
                    "services",
                    "--group",
                    "taskd-monitor.desktop",
                    "--key",
                    "_launch",
                ],
            )
            .ok()
            .map(|v| v.split(',').next().unwrap_or("").to_string())
            .filter(|v| !v.is_empty() && v != "none")
        }
        Desktop::Xfce => {
            let props = r.out("xfconf-query", &["-c", "xfce4-keyboard-shortcuts", "-l", "-v"]).ok()?;
            props
                .lines()
                .find(|l| l.contains("/commands/custom/") && l.trim_end().ends_with("taskdm"))
                .and_then(|l| l.split_whitespace().next()?.rsplit('/').next().map(String::from))
        }
        Desktop::Other => None,
    }
}

/// Register `choice` (index into CHOICES) to run `command`.
pub fn set(choice: usize, command: &str) -> Result<String, String> {
    let (label, accel, kde) = CHOICES.get(choice).copied().ok_or("unknown shortcut")?;
    let r = runner();
    match desktop() {
        Desktop::Cinnamon => CINNAMON.set(&r, accel, command)?,
        Desktop::Gnome => GNOME.set(&r, accel, command)?,
        Desktop::Kde => {
            let w = kwrite().ok_or("kwriteconfig isn't installed")?;
            r.out(
                w,
                &[
                    "--file",
                    "kglobalshortcutsrc",
                    "--group",
                    "services",
                    "--group",
                    "taskd-monitor.desktop",
                    "--key",
                    "_launch",
                    kde,
                ],
            )?;
            return Ok(format!("{label} set; KDE picks it up after you log out and back in"));
        }
        Desktop::Xfce => {
            remove_xfce(&r);
            r.out(
                "xfconf-query",
                &[
                    "-c",
                    "xfce4-keyboard-shortcuts",
                    "-p",
                    &format!("/commands/custom/{accel}"),
                    "-n",
                    "-t",
                    "string",
                    "-s",
                    command,
                ],
            )?;
        }
        Desktop::Other => {
            return Err("this desktop isn't supported; add a shortcut for `taskdm` in its keyboard settings".into());
        }
    }
    Ok(format!("{label} now opens Taskd"))
}

fn remove_xfce(r: &Runner) {
    if let Ok(props) = r.out("xfconf-query", &["-c", "xfce4-keyboard-shortcuts", "-l", "-v"]) {
        for l in props.lines().filter(|l| l.contains("/commands/custom/") && l.trim_end().ends_with("taskdm")) {
            if let Some(p) = l.split_whitespace().next() {
                let _ = r.out("xfconf-query", &["-c", "xfce4-keyboard-shortcuts", "-r", "-p", p]);
            }
        }
    }
}

pub fn remove() -> Result<(), String> {
    let r = runner();
    match desktop() {
        Desktop::Cinnamon => CINNAMON.remove(&r),
        Desktop::Gnome => GNOME.remove(&r),
        Desktop::Kde => {
            let w = kwrite().ok_or("kwriteconfig isn't installed")?;
            r.out(
                w,
                &[
                    "--file",
                    "kglobalshortcutsrc",
                    "--group",
                    "services",
                    "--group",
                    "taskd-monitor.desktop",
                    "--key",
                    "_launch",
                    "--delete",
                ],
            )
            .map(|_| ())
        }
        Desktop::Xfce => {
            remove_xfce(&r);
            Ok(())
        }
        Desktop::Other => Ok(()),
    }
}

// ------------------------------------------------------------------ single instance

fn instance_addr() -> std::io::Result<SocketAddr> {
    // Abstract socket (no file to clean up), per user.
    // SAFETY: getuid never fails.
    let uid = unsafe { libc::getuid() };
    SocketAddr::from_abstract_name(format!("taskd-monitor-gui-{uid}").as_bytes())
}

/// If Taskd is already running for this user, ask it to show its window and return true.
pub fn ask_running_to_show() -> bool {
    let Ok(addr) = instance_addr() else { return false };
    let Ok(mut s) = UnixStream::connect_addr(&addr) else { return false };
    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    if s.write_all(b"show\n").is_err() {
        return false;
    }
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).is_ok() && line.trim() == "ok"
}

/// Listen for "show" requests from later launches; `on_show` runs on a background thread.
/// Only the same user can connect (checked with SO_PEERCRED).
pub fn listen(on_show: impl Fn() + Send + 'static) {
    let Ok(addr) = instance_addr() else { return };
    let Ok(l) = UnixListener::bind_addr(&addr) else { return };
    std::thread::spawn(move || {
        // SAFETY: getuid never fails.
        let me = unsafe { libc::getuid() };
        for mut s in l.incoming().flatten() {
            if crate::helper::peer_uid(&s) != Some(me) {
                continue;
            }
            let mut line = String::new();
            let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(2)));
            if BufReader::new(&s).read_line(&mut line).is_ok() && line.trim() == "show" {
                on_show();
                let _ = s.write_all(b"ok\n");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_format() {
        assert_eq!(parse_list("@as []"), Vec::<String>::new());
        assert_eq!(parse_list("['custom1', 'custom0', '__dummy__']"), vec!["custom1", "custom0", "__dummy__"]);
        assert_eq!(fmt_list(&["a".into(), "b".into()]), "['a', 'b']");
        assert_eq!(label_for("<Super>Escape"), "Super+Esc");
    }

    /// Set, find, update and remove a shortcut in a throwaway keyfile settings backend.
    #[test]
    fn cinnamon_and_gnome_round_trip() {
        let dir = std::env::temp_dir().join(format!("taskd-gs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let r = Runner {
            env: vec![
                ("GSETTINGS_BACKEND".into(), "keyfile".into()),
                ("XDG_CONFIG_HOME".into(), dir.to_string_lossy().into_owned()),
            ],
        };
        for gs in [CINNAMON, GNOME] {
            // Skip desktops whose schemas aren't installed here.
            if r.out("gsettings", &["get", gs.list_schema, gs.list_key]).is_err() {
                continue;
            }
            assert_eq!(gs.find(&r).unwrap(), None);
            gs.set(&r, "<Primary><Shift>Escape", "/usr/bin/taskdm").unwrap();
            let (entry, b) = gs.find(&r).unwrap().expect("found");
            assert_eq!(b, "<Primary><Shift>Escape");
            // Setting again updates the same entry instead of adding another.
            gs.set(&r, "<Super>Escape", "/usr/bin/taskdm").unwrap();
            assert_eq!(gs.find(&r).unwrap(), Some((entry, "<Super>Escape".into())));
            assert_eq!(parse_list(&r.out("gsettings", &["get", gs.list_schema, gs.list_key]).unwrap()).len(), 1);
            gs.remove(&r).unwrap();
            assert_eq!(gs.find(&r).unwrap(), None);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn second_launch_reaches_the_first() {
        // Uses the real per-user name: skip if a Taskd window is already running.
        if ask_running_to_show() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        listen(move || {
            let _ = tx.send(());
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(ask_running_to_show());
        assert!(rx.recv_timeout(std::time::Duration::from_secs(2)).is_ok());
    }
}
