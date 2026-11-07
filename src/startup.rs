//! XDG autostart entries (`/etc/xdg/autostart` + `~/.config/autostart`).
//! Disabling writes a per-user override, never touching system files.

use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct StartupEntry {
    pub file: String,
    pub name: String,
    pub exec: String,
    pub comment: String,
    pub enabled: bool,
    pub user: bool,
    /// Shown only in some desktops and the current one isn't among them.
    pub not_here: bool,
}

fn user_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("autostart")
}

pub fn list() -> Vec<StartupEntry> {
    let desktops: Vec<String> =
        std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().split(':').map(|s| s.to_lowercase()).collect();
    let mut out: Vec<StartupEntry> = Vec::new();
    for (dir, user) in [(PathBuf::from("/etc/xdg/autostart"), false), (user_dir(), true)] {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let file = e.file_name().to_string_lossy().into_owned();
            if !file.ends_with(".desktop") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(e.path()) else { continue };
            let entry = parse(&file, &text, user, &desktops);
            // A user file overrides the system file of the same name.
            out.retain(|x| x.file != file);
            out.push(entry);
        }
    }
    out.sort_by_key(|e| e.name.to_lowercase());
    out
}

fn parse(file: &str, text: &str, user: bool, desktops: &[String]) -> StartupEntry {
    let mut e = StartupEntry {
        file: file.to_string(),
        name: file.trim_end_matches(".desktop").to_string(),
        exec: String::new(),
        comment: String::new(),
        enabled: true,
        user,
        not_here: false,
    };
    let mut in_main = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_main = line == "[Desktop Entry]";
            continue;
        }
        if !in_main {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "Name" => e.name = v.to_string(),
            "Exec" => e.exec = v.to_string(),
            "Comment" => e.comment = v.to_string(),
            "Hidden" if v.eq_ignore_ascii_case("true") => e.enabled = false,
            "X-GNOME-Autostart-enabled" | "X-MATE-Autostart-enabled" if v.eq_ignore_ascii_case("false") => {
                e.enabled = false
            }
            "OnlyShowIn" => {
                let list = v.to_lowercase();
                e.not_here |= !desktops.iter().any(|d| list.split(';').any(|x| x == d));
            }
            "NotShowIn" => {
                let list = v.to_lowercase();
                e.not_here |= desktops.iter().any(|d| list.split(';').any(|x| x == d));
            }
            _ => {}
        }
    }
    e
}

/// Enable/disable by writing (or updating) the user's copy of the entry.
pub fn set_enabled(file: &str, enabled: bool) -> std::io::Result<()> {
    let dir = user_dir();
    std::fs::create_dir_all(&dir)?;
    let user_path = dir.join(file);
    let src = if user_path.exists() { user_path.clone() } else { PathBuf::from("/etc/xdg/autostart").join(file) };
    let text = std::fs::read_to_string(&src)?;
    std::fs::write(&user_path, rewrite(&text, enabled))
}

fn rewrite(text: &str, enabled: bool) -> String {
    let mut out = String::with_capacity(text.len() + 64);
    let mut in_main = false;
    let mut wrote = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            if in_main && !wrote {
                out.push_str(&format!("X-GNOME-Autostart-enabled={enabled}\n"));
                wrote = true;
            }
            in_main = t == "[Desktop Entry]";
        } else if in_main {
            let key = t.split('=').next().unwrap_or("").trim();
            if key == "Hidden" {
                continue;
            }
            if key == "X-GNOME-Autostart-enabled" {
                out.push_str(&format!("X-GNOME-Autostart-enabled={enabled}\n"));
                wrote = true;
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    if !wrote {
        out.push_str(&format!("X-GNOME-Autostart-enabled={enabled}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggles() {
        let src = "[Desktop Entry]\nName=Foo\nExec=foo\nHidden=true\n[Desktop Action x]\nName=X\n";
        let e = parse("foo.desktop", src, false, &[]);
        assert!(!e.enabled);
        let on = rewrite(src, true);
        let e = parse("foo.desktop", &on, true, &[]);
        assert!(e.enabled, "{on}");
        assert!(on.contains("[Desktop Action x]"));
        let off = rewrite(&on, false);
        assert!(!parse("foo.desktop", &off, true, &[]).enabled);
        assert_eq!(off.matches("X-GNOME-Autostart-enabled").count(), 1);
    }

    #[test]
    fn desktop_filter() {
        let d = vec!["x-cinnamon".to_string()];
        assert!(parse("a.desktop", "[Desktop Entry]\nOnlyShowIn=GNOME;\n", false, &d).not_here);
        assert!(!parse("a.desktop", "[Desktop Entry]\nOnlyShowIn=X-Cinnamon;\n", false, &d).not_here);
    }
}
