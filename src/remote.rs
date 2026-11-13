//! Watching other machines: `taskdm --stream` prints a compact JSON summary every few
//! seconds, and the Machines tab runs it over `ssh` (key-based login, nothing to install
//! but taskdm itself on the other side, no open ports).

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::groups::Grouper;
use crate::sampler::Snapshot;

pub const INSTALL_URL: &str = "https://raw.githubusercontent.com/strid3r21/Taskd/main/install.sh";

/// One summary line for `--stream`.
pub fn summary(s: &Snapshot, g: &Grouper) -> Value {
    let r1 = |v: f32| (v as f64 * 10.0).round() / 10.0;
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").map(|h| h.trim().to_string()).unwrap_or_default();
    let mut top: Vec<&crate::groups::Group> = g.groups.iter().filter(|x| x.cpu >= 0.1 || x.mem > 50 << 20).collect();
    top.sort_by(|a, b| (b.cpu as f64 + b.mem as f64 / 1e9).total_cmp(&(a.cpu as f64 + a.mem as f64 / 1e9)));
    json!({
        "v": 1,
        "host": host,
        "version": env!("CARGO_PKG_VERSION"),
        "uptime": s.uptime.round(),
        "cpu": {"usage": r1(s.cpu.usage), "model": s.cpu.model, "logical": s.cpu.logical,
                "temp": s.cpu.temp_c.map(r1), "power": s.cpu.power_w.map(r1)},
        "load": [r1(s.load[0]), r1(s.load[1]), r1(s.load[2])],
        "mem": {"total": s.mem.total, "used": s.mem.used, "swap_total": s.mem.swap_total, "swap_used": s.mem.swap_used},
        "disks": s.disks.iter().map(|d| json!({"name": d.name, "busy": r1(d.busy), "read": d.read_rate.round(), "write": d.write_rate.round()})).collect::<Vec<_>>(),
        "nets": s.nets.iter().map(|n| json!({"name": n.name, "rx": n.rx_rate.round(), "tx": n.tx_rate.round()})).collect::<Vec<_>>(),
        "gpus": s.gpus.iter().map(|x| json!({"name": x.name, "util": x.util.map(r1), "temp": x.temp_c.map(r1)})).collect::<Vec<_>>(),
        "psi": s.pressure.as_ref().map(|p| json!({"verdict": p.verdict(), "cpu": r1(p.cpu), "mem": r1(p.mem), "io": r1(p.io)})),
        "space": crate::storage::filesystems().iter().map(|f| json!({"mount": f.mount, "total": f.total, "avail": f.avail})).collect::<Vec<_>>(),
        "top": top.iter().take(8).map(|x| json!([x.name, x.kind.label(), r1(x.cpu), x.mem])).collect::<Vec<_>>(),
    })
}

/// `taskdm --stream [ms]`: print a summary line every interval until stdout closes.
pub fn stream_cli(interval_ms: Option<&str>) -> i32 {
    let ms = interval_ms.and_then(|v| v.parse::<u64>().ok()).unwrap_or(2000).clamp(250, 60_000);
    let mut sampler = crate::sampler::Sampler::new();
    let mut g = Grouper::default();
    g.prime_container_names();
    // SAFETY: getuid never fails.
    let uid = unsafe { libc::getuid() };
    sampler.sample();
    let mut out = std::io::stdout().lock();
    loop {
        std::thread::sleep(Duration::from_millis(ms));
        let s = sampler.sample();
        g.update(&s, uid);
        if writeln!(out, "{}", summary(&s, &g)).and_then(|_| out.flush()).is_err() {
            return 0; // the viewer went away
        }
    }
}

/// `user@host` or an ssh_config alias; refuse anything ssh could read as an option.
pub fn valid_host(h: &str) -> bool {
    !h.is_empty() && !h.starts_with('-') && h.chars().all(|c| c.is_ascii_alphanumeric() || "@._-:[]".contains(c))
}

/// The command run on the remote side. `$HOME/.local/bin` is where the one-line installer
/// puts taskdm, and non-interactive ssh sessions often don't have it on PATH.
pub const REMOTE_CMD: &str = "PATH=\"$PATH:$HOME/.local/bin:/usr/local/bin\" exec taskdm --stream 2000";

pub fn ssh_argv(host: &str) -> Vec<String> {
    [
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=2",
        "--",
        host,
        REMOTE_CMD,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Connecting,
    Connected,
    /// Message and when to retry.
    Error(String),
}

pub struct State {
    pub status: Status,
    pub last: Option<Value>,
    pub at: Option<Instant>,
    pub cpu_hist: VecDeque<f32>,
    pub mem_hist: VecDeque<f32>,
}

/// A connection to one machine, kept alive (reconnecting with back-off) until dropped.
pub struct Remote {
    pub host: String,
    pub state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
}

const HIST: usize = 90;

/// Turn a stderr/exit into a hint the user can act on.
fn explain(stderr: &str, code: Option<i32>) -> String {
    let e = stderr.trim();
    if e.contains("Permission denied") {
        "SSH login refused. Taskd needs key-based login: run `ssh-copy-id HOST` once in a terminal.".into()
    } else if e.contains("Host key verification failed") || e.contains("authenticity of host") {
        "Unknown host key. Connect once with `ssh HOST` in a terminal to accept it.".into()
    } else if code == Some(127) || e.contains("taskdm: not found") || e.contains("command not found") {
        "Taskd isn't installed there (or isn't on PATH). Use “Install there” or run the one-line installer on it."
            .into()
    } else if e.contains("Could not resolve")
        || e.contains("No route")
        || e.contains("timed out")
        || e.contains("refused")
    {
        format!("Can't reach it: {}", e.lines().last().unwrap_or(e))
    } else if e.is_empty() {
        format!("Disconnected{}", code.map_or(String::new(), |c| format!(" (exit {c})")))
    } else {
        e.lines().last().unwrap_or(e).to_string()
    }
}

impl Remote {
    /// Connect with `ssh` (or, in tests, any program printing summary lines).
    pub fn start(host: &str, program: &str, args: Vec<String>, on_update: impl Fn() + Send + 'static) -> Remote {
        let state = Arc::new(Mutex::new(State {
            status: Status::Connecting,
            last: None,
            at: None,
            cpu_hist: VecDeque::new(),
            mem_hist: VecDeque::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let child: Arc<Mutex<Option<Child>>> = Arc::default();
        let (st, sp, ch, program) = (state.clone(), stop.clone(), child.clone(), program.to_string());
        std::thread::spawn(move || {
            let mut backoff = 5;
            while !sp.load(Ordering::Relaxed) {
                st.lock().unwrap().status = Status::Connecting;
                on_update();
                let spawned = Command::new(crate::actions::sys_bin(&program))
                    .args(&args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn();
                let mut c = match spawned {
                    Ok(c) => c,
                    Err(e) => {
                        st.lock().unwrap().status = Status::Error(format!("{program}: {e}"));
                        on_update();
                        return;
                    }
                };
                let (out, err) = (c.stdout.take(), c.stderr.take());
                *ch.lock().unwrap() = Some(c);
                // Collect stderr on the side (ssh writes its errors there).
                let errbuf = Arc::new(Mutex::new(String::new()));
                if let Some(err) = err {
                    let eb = errbuf.clone();
                    std::thread::spawn(move || {
                        for l in BufReader::new(err).lines().map_while(Result::ok) {
                            let mut b = eb.lock().unwrap();
                            if b.len() < 4096 {
                                b.push_str(&l);
                                b.push('\n');
                            }
                        }
                    });
                }
                let mut got_any = false;
                if let Some(out) = out {
                    for line in BufReader::new(out).lines().map_while(Result::ok) {
                        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                        if v["v"].as_u64() != Some(1) {
                            continue;
                        }
                        got_any = true;
                        backoff = 5;
                        let mut guard = st.lock().unwrap();
                        let s = &mut *guard;
                        s.status = Status::Connected;
                        let cpu = v["cpu"]["usage"].as_f64().unwrap_or(0.0) as f32;
                        let (used, total) =
                            (v["mem"]["used"].as_f64().unwrap_or(0.0), v["mem"]["total"].as_f64().unwrap_or(1.0));
                        for (h, x) in
                            [(&mut s.cpu_hist, cpu), (&mut s.mem_hist, (100.0 * used / total.max(1.0)) as f32)]
                        {
                            if h.len() == HIST {
                                h.pop_front();
                            }
                            h.push_back(x);
                        }
                        s.last = Some(v);
                        s.at = Some(Instant::now());
                        drop(guard);
                        on_update();
                    }
                }
                let code = ch.lock().unwrap().take().and_then(|mut c| c.wait().ok()).and_then(|s| s.code());
                if sp.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100)); // let the stderr reader finish
                let msg = explain(&errbuf.lock().unwrap(), code);
                st.lock().unwrap().status = Status::Error(msg);
                on_update();
                // Retry: quickly after a working session dropped, slower while it keeps failing.
                let wait = if got_any { 5 } else { backoff };
                backoff = (backoff * 2).min(120);
                for _ in 0..wait * 10 {
                    if sp.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });
        Remote { host: host.to_string(), state, stop, child }
    }

    pub fn ssh(host: &str, on_update: impl Fn() + Send + 'static) -> Remote {
        Remote::start(host, "ssh", ssh_argv(host), on_update)
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(c) = self.child.lock().unwrap().as_mut() {
            let _ = c.kill();
        }
    }
}

/// Install Taskd on `host` with the one-line installer over ssh (returns its output).
pub fn install_over_ssh(host: &str) -> Result<String, String> {
    if !valid_host(host) {
        return Err("invalid host".into());
    }
    let o = Command::new(crate::actions::sys_bin("ssh"))
        .args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "--", host])
        .arg(format!("curl -fsSL {INSTALL_URL} | sh"))
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("ssh: {e}"))?;
    let text = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    if o.status.success() { Ok(text.trim().to_string()) } else { Err(explain(&text, o.status.code())) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_and_argv() {
        assert!(valid_host("root@tower") && valid_host("nas.local") && valid_host("user@[fe80::1]"));
        assert!(!valid_host("-oProxyCommand=x") && !valid_host("a b") && !valid_host("a;rm") && !valid_host(""));
        let a = ssh_argv("root@tower");
        let i = a.iter().position(|x| x == "--").unwrap();
        assert_eq!(&a[i + 1..], ["root@tower", REMOTE_CMD]);
        assert!(a.contains(&"BatchMode=yes".to_string()));
        assert!(explain("root@tower: Permission denied (publickey).", Some(255)).contains("ssh-copy-id"));
        assert!(explain("sh: 1: exec: taskdm: not found", Some(127)).contains("isn't installed"));
    }

    #[test]
    fn summary_has_the_essentials() {
        let mut sampler = crate::sampler::Sampler::new();
        let s = sampler.sample();
        let mut g = Grouper::default();
        g.update(&s, 0);
        let v = summary(&s, &g);
        assert_eq!(v["v"], 1);
        assert!(v["cpu"]["usage"].is_number() && v["mem"]["total"].as_u64().unwrap() > 0);
        assert!(v["space"].as_array().is_some_and(|a| !a.is_empty()));
    }

    /// The whole path with a stand-in for ssh: the real binary's --stream, read back.
    #[test]
    fn streams_from_a_child_process() {
        let exe = std::env::current_exe().unwrap();
        let bin = exe.parent().and_then(|d| d.parent()).map(|d| d.join("taskdm")).unwrap();
        if !bin.exists() {
            return; // only the test binary was built
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let r = Remote::start("local", &bin.to_string_lossy(), vec!["--stream".into(), "250".into()], move || {
            let _ = tx.send(());
        });
        let end = Instant::now() + Duration::from_secs(15);
        while Instant::now() < end && r.state.lock().unwrap().cpu_hist.len() < 2 {
            let _ = rx.recv_timeout(Duration::from_millis(200));
        }
        let s = r.state.lock().unwrap();
        assert_eq!(s.status, Status::Connected);
        assert!(s.cpu_hist.len() >= 2);
        assert_eq!(s.last.as_ref().unwrap()["version"], env!("CARGO_PKG_VERSION"));
        drop(s);
        drop(r); // kills the child
    }

    #[test]
    fn reports_a_failing_command() {
        let r = Remote::start(
            "x",
            "sh",
            vec!["-c".into(), "echo 'sh: 1: exec: taskdm: not found' >&2; exit 127".into()],
            || {},
        );
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            if let Status::Error(e) = &r.state.lock().unwrap().status {
                assert!(e.contains("isn't installed"), "{e}");
                break;
            }
            assert!(Instant::now() < end, "error reported");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
