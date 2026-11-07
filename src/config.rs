//! Tiny key=value settings file at `$XDG_CONFIG_HOME/taskd/config` (no serde needed).

use std::path::PathBuf;

#[derive(Clone, PartialEq)]
pub struct Config {
    pub interval_ms: u64,
    pub tab: String,
    pub sort_col: String,
    pub sort_desc: bool,
    pub tree: bool,
    pub show_kernel: bool,
    pub only_mine: bool,
    pub per_core_percent: bool,
    pub cpu_per_core_graphs: bool,
    pub hidden_cols: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            interval_ms: 1000,
            tab: "processes".into(),
            sort_col: "cpu".into(),
            sort_desc: true,
            tree: false,
            show_kernel: false,
            only_mine: false,
            per_core_percent: false,
            cpu_per_core_graphs: false,
            hidden_cols: String::new(),
        }
    }
}

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("taskd").join("config"))
}

impl Config {
    pub fn load() -> Self {
        let mut c = Config::default();
        let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else { return c };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            let b = v == "true";
            match k.trim() {
                "interval_ms" => c.interval_ms = v.parse().unwrap_or(1000).clamp(250, 60_000),
                "tab" => c.tab = v.into(),
                "sort_col" => c.sort_col = v.into(),
                "sort_desc" => c.sort_desc = b,
                "tree" => c.tree = b,
                "show_kernel" => c.show_kernel = b,
                "only_mine" => c.only_mine = b,
                "per_core_percent" => c.per_core_percent = b,
                "cpu_per_core_graphs" => c.cpu_per_core_graphs = b,
                "hidden_cols" => c.hidden_cols = v.into(),
                _ => {}
            }
        }
        c
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = format!(
            "interval_ms={}\ntab={}\nsort_col={}\nsort_desc={}\ntree={}\nshow_kernel={}\nonly_mine={}\n\
             per_core_percent={}\ncpu_per_core_graphs={}\nhidden_cols={}\n",
            self.interval_ms,
            self.tab,
            self.sort_col,
            self.sort_desc,
            self.tree,
            self.show_kernel,
            self.only_mine,
            self.per_core_percent,
            self.cpu_per_core_graphs,
            self.hidden_cols
        );
        let _ = std::fs::write(p, text);
    }
}
