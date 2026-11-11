//! Tiny key=value settings file at `$XDG_CONFIG_HOME/taskd/config` (no serde needed).
//!
//! Every setting is declared exactly once in the `settings!` table below: field, type,
//! default, and how a loaded value is sanitised. Load and save are generated from it.

use std::path::PathBuf;

/// Parses a config value for one field type.
trait Field: Sized {
    fn parse(v: &str) -> Option<Self>;
    fn show(&self) -> String;
}

impl Field for bool {
    fn parse(v: &str) -> Option<Self> {
        Some(v == "true")
    }
    fn show(&self) -> String {
        self.to_string()
    }
}

impl Field for String {
    fn parse(v: &str) -> Option<Self> {
        Some(v.to_string())
    }
    fn show(&self) -> String {
        self.replace('\n', " ")
    }
}

macro_rules! num_field {
    ($($t:ty),*) => {$(
        impl Field for $t {
            fn parse(v: &str) -> Option<Self> { v.parse().ok() }
            fn show(&self) -> String { self.to_string() }
        }
    )*};
}
num_field!(u64, f32);

macro_rules! settings {
    ($( $(#[$doc:meta])* $name:ident : $ty:ty = $default:expr $(, clamp $lo:expr, $hi:expr)? ;)*) => {
        #[derive(Clone, PartialEq)]
        pub struct Config {
            $( $(#[$doc])* pub $name: $ty, )*
        }

        impl Default for Config {
            fn default() -> Self {
                Config { $( $name: $default.into(), )* }
            }
        }

        impl Config {
            fn set(&mut self, key: &str, v: &str) {
                match key {
                    $( stringify!($name) => {
                        if let Some(x) = <$ty as Field>::parse(v) {
                            self.$name = x $( .clamp($lo, $hi) )?;
                        }
                    } )*
                    _ => {}
                }
            }

            fn lines(&self) -> String {
                let mut s = String::new();
                $( s += &format!("{}={}\n", stringify!($name), Field::show(&self.$name)); )*
                s
            }
        }
    };
}

settings! {
    interval_ms: u64 = 1000u64, clamp 250, 60_000;
    tab: String = "processes";
    sort_col: String = "cpu";
    sort_desc: bool = true;
    tree: bool = false;
    show_kernel: bool = false;
    only_mine: bool = false;
    per_core_percent: bool = false;
    cpu_per_core_graphs: bool = false;
    hidden_cols: String = "";
    auto_update: bool = true;
    /// Performance sidebar width.
    perf_sidebar: f32 = 230.0f32, clamp 170.0, 420.0;
    /// Performance sidebar order: comma-separated device keys.
    perf_order: String = "";
    /// Nicknames: `key=name;key=name` (names percent-escape '%' and ';').
    perf_names: String = "";
    /// Hidden sidebar items: comma-separated device keys.
    perf_hidden: String = "";
    /// Top tab order: comma-separated tab keys.
    tab_order: String = "";
    /// "system", "light" or "dark".
    theme: String = "system";
    /// Last window geometry: "w,h,x,y,maximized" (empty = default).
    window: String = "";

    /// Performance time range: "live", "1h", "6h", "24h" or "7d".
    perf_range: String = "live";
    /// System tray icon, and whether closing the window keeps Taskd running there.
    tray: bool = true;
    close_to_tray: bool = false;
    /// AI assistant: provider key (see ai::Provider), model id, API base URL.
    ai_provider: String = "ollama";
    ai_model: String = "";
    ai_base: String = "";
    /// Let the assistant end processes / change priority (each action still asks).
    ai_allow_actions: bool = false;
    /// Background recorder (history + alerts) as a systemd user service.
    record: bool = true;
    record_interval_s: u64 = 10u64, clamp 2, 300;

    /// Alerts (0 = off). Memory: alert when *available* memory drops below this percent.
    alert_mem_pct: f32 = 10.0f32, clamp 0.0, 90.0;
    /// CPU above this percent for `alert_cpu_mins` minutes.
    alert_cpu_pct: f32 = 90.0f32, clamp 0.0, 100.0;
    alert_cpu_mins: f32 = 5.0f32, clamp 1.0, 60.0;
    /// A disk busier than this percent for 5 minutes.
    alert_disk_pct: f32 = 95.0f32, clamp 0.0, 100.0;
    alert_cpu_temp: f32 = 90.0f32, clamp 0.0, 120.0;
    alert_gpu_temp: f32 = 85.0f32, clamp 0.0, 120.0;
    alert_swap: bool = true;
    alert_battery_pct: f32 = 10.0f32, clamp 0.0, 90.0;
    alert_failed_services: bool = true;
    /// Per-app memory limits: `App name=GB;App name=GB`.
    alert_app_mem: String = "";
    /// Alert notifications are muted until this time (Unix seconds); `u64::MAX` = until unmuted.
    alerts_muted_until: u64 = 0u64;
}

pub fn path() -> Option<PathBuf> {
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
            if let Some((k, v)) = line.split_once('=') {
                c.set(k.trim(), v.trim());
            }
        }
        c
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Write-then-rename so the background recorder never reads a half-written file.
        let tmp = p.with_extension("tmp");
        if std::fs::write(&tmp, self.lines()).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_clamps() {
        let c = Config {
            theme: "dark".into(),
            alert_app_mem: "Firefox Web Browser=4".into(),
            perf_sidebar: 300.0,
            ..Default::default()
        };
        let mut d = Config::default();
        for line in c.lines().lines() {
            let (k, v) = line.split_once('=').unwrap();
            d.set(k, v);
        }
        assert!(c == d);
        d.set("interval_ms", "5");
        assert_eq!(d.interval_ms, 250);
        d.set("interval_ms", "garbage");
        assert_eq!(d.interval_ms, 250, "unparsable values keep the current value");
        d.set("unknown_key", "x");
    }
}
