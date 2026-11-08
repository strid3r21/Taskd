//! Process control (signals, priority) and on-demand process details.

use std::process::Command;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Signal {
    Term,
    Kill,
    Stop,
    Cont,
    Hup,
    Int,
}

impl Signal {
    pub fn raw(self) -> i32 {
        match self {
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
            Signal::Stop => libc::SIGSTOP,
            Signal::Cont => libc::SIGCONT,
            Signal::Hup => libc::SIGHUP,
            Signal::Int => libc::SIGINT,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Signal::Term => "TERM",
            Signal::Kill => "KILL",
            Signal::Stop => "STOP",
            Signal::Cont => "CONT",
            Signal::Hup => "HUP",
            Signal::Int => "INT",
        }
    }
    pub fn verb(self) -> &'static str {
        match self {
            Signal::Term => "End",
            Signal::Kill => "Kill",
            Signal::Stop => "Suspend",
            Signal::Cont => "Resume",
            Signal::Hup => "Hang up",
            Signal::Int => "Interrupt",
        }
    }
}

pub enum ActionError {
    /// Needs elevated privileges; the UI can offer to retry via pkexec.
    Permission,
    Other(String),
}

pub fn send_signal(pid: u32, sig: Signal) -> Result<(), ActionError> {
    // SAFETY: kill(2) has no memory-safety preconditions.
    let r = unsafe { libc::kill(pid as libc::pid_t, sig.raw()) };
    if r == 0 { Ok(()) } else { Err(last_error()) }
}

pub fn set_nice(pid: u32, nice: i32) -> Result<(), ActionError> {
    // SAFETY: setpriority(2) has no memory-safety preconditions.
    let r = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid as libc::id_t, nice) };
    if r == 0 { Ok(()) } else { Err(last_error()) }
}

fn last_error() -> ActionError {
    let e = std::io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::EPERM) | Some(libc::EACCES) => ActionError::Permission,
        Some(libc::ESRCH) => ActionError::Other("process no longer exists".into()),
        _ => ActionError::Other(e.to_string()),
    }
}

/// Run a command through pkexec (polkit prompt). Blocking: call from a worker thread.
/// Absolute path of a system tool, so a planted program earlier in $PATH (say ~/.local/bin/kill)
/// is never what pkexec runs as root.
pub fn sys_bin(name: &str) -> String {
    ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
        .iter()
        .map(|d| format!("{d}/{name}"))
        .find(|p| std::path::Path::new(p).exists())
        .unwrap_or_else(|| name.to_string())
}

pub fn run_elevated(args: &[String]) -> Result<(), String> {
    let out = Command::new(sys_bin("pkexec")).args(args).output().map_err(|e| format!("pkexec: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() { format!("failed ({})", out.status) } else { err })
    }
}

pub fn elevated_signal_args(pid: u32, sig: Signal) -> Vec<String> {
    vec![sys_bin("kill"), "-s".into(), sig.name().into(), pid.to_string()]
}

pub fn elevated_nice_args(pid: u32, nice: i32) -> Vec<String> {
    vec![sys_bin("renice"), "-n".into(), nice.to_string(), "-p".into(), pid.to_string()]
}

/// Everything shown in the process details window, read on demand.
#[derive(Default, Clone)]
pub struct Details {
    pub pid: u32,
    pub exe: String,
    pub cwd: String,
    pub cmdline: String,
    pub status: Vec<(String, String)>,
    pub cgroup: String,
    pub open_fds: Option<usize>,
    pub env_count: Option<usize>,
    pub limits_open_files: Option<String>,
}

pub fn read_details(pid: u32) -> Details {
    let base = format!("/proc/{pid}");
    let link = |p: &str| {
        std::fs::read_link(format!("{base}/{p}"))
            .map(|l| l.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "(access denied)".into())
    };
    let mut d = Details { pid, exe: link("exe"), cwd: link("cwd"), ..Default::default() };
    if let Ok(b) = std::fs::read(format!("{base}/cmdline")) {
        d.cmdline = String::from_utf8_lossy(&b).trim_end_matches('\0').replace('\0', " ");
    }
    const KEYS: &[&str] = &[
        "Name",
        "State",
        "PPid",
        "Tgid",
        "Uid",
        "Gid",
        "Threads",
        "VmPeak",
        "VmSize",
        "VmHWM",
        "VmRSS",
        "RssAnon",
        "RssFile",
        "RssShmem",
        "VmSwap",
        "voluntary_ctxt_switches",
        "nonvoluntary_ctxt_switches",
        "Cpus_allowed_list",
        "Seccomp",
        "NoNewPrivs",
    ];
    if let Ok(s) = std::fs::read_to_string(format!("{base}/status")) {
        for line in s.lines() {
            if let Some((k, v)) = line.split_once(':')
                && KEYS.contains(&k)
            {
                d.status.push((k.to_string(), v.split_whitespace().collect::<Vec<_>>().join(" ")));
            }
        }
    }
    if let Ok(s) = std::fs::read_to_string(format!("{base}/cgroup")) {
        d.cgroup = s.lines().map(|l| l.splitn(3, ':').nth(2).unwrap_or(l)).collect::<Vec<_>>().join("\n");
    }
    d.open_fds = std::fs::read_dir(format!("{base}/fd")).ok().map(|r| r.count());
    d.env_count =
        std::fs::read(format!("{base}/environ")).ok().map(|b| b.split(|&c| c == 0).filter(|s| !s.is_empty()).count());
    if let Ok(s) = std::fs::read_to_string(format!("{base}/limits")) {
        d.limits_open_files = s
            .lines()
            .find(|l| l.starts_with("Max open files"))
            .map(|l| l["Max open files".len()..].split_whitespace().take(2).collect::<Vec<_>>().join(" / "));
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn state(pid: u32) -> u8 {
        let s = std::fs::read(format!("/proc/{pid}/stat")).unwrap();
        crate::sampler::parse_stat(&s).unwrap().state
    }

    #[test]
    fn signals_and_priority_on_child() {
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();

        assert!(send_signal(pid, Signal::Stop).is_ok());
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(state(pid), b'T');
        assert!(send_signal(pid, Signal::Cont).is_ok());
        std::thread::sleep(Duration::from_millis(50));
        assert_ne!(state(pid), b'T');

        // Raising nice (lowering priority) never needs privileges.
        assert!(set_nice(pid, 10).is_ok());
        let d = read_details(pid);
        assert!(d.cmdline.starts_with("sleep 30"), "{}", d.cmdline);
        assert!(d.status.iter().any(|(k, _)| k == "VmRSS"));

        assert!(send_signal(pid, Signal::Term).is_ok());
        let status = child.wait().unwrap();
        assert!(!status.success());
        assert!(matches!(send_signal(pid, Signal::Term), Err(ActionError::Other(_))));
    }

    #[test]
    fn permission_error_is_detected() {
        // PID 1 belongs to root; signalling it as a normal user must report Permission.
        if unsafe { libc::getuid() } != 0 {
            assert!(matches!(send_signal(1, Signal::Cont), Err(ActionError::Permission)));
        }
    }
}
