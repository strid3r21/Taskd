//! Open files and network connections of processes (like `lsof` / `ss`), read from /proc.
//! Only processes we may inspect (our own, or all when root) are visible.

use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr};

#[derive(Clone, Debug)]
pub struct OpenFile {
    pub fd: u32,
    pub kind: &'static str,
    pub target: String,
}

#[derive(Clone, Debug)]
pub struct Conn {
    pub proto: &'static str,
    pub local: String,
    pub remote: String,
    pub state: &'static str,
}

#[derive(Clone, Debug, Default)]
pub struct ProcFiles {
    pub files: Vec<OpenFile>,
    pub conns: Vec<Conn>,
    pub truncated: bool,
}

const MAX_FDS: usize = 2000;

/// Open fds of `pid`, with sockets resolved to TCP/UDP/Unix connections.
pub fn read(pid: u32) -> Result<ProcFiles, String> {
    let dir = std::fs::read_dir(format!("/proc/{pid}/fd")).map_err(|e| match e.kind() {
        std::io::ErrorKind::PermissionDenied => "access denied (another user's process; run as root to see it)".into(),
        _ => e.to_string(),
    })?;
    let mut out = ProcFiles::default();
    let mut sockets: Vec<(u32, u64)> = Vec::new();
    let mut fds: Vec<(u32, String)> = dir
        .flatten()
        .filter_map(|e| {
            let fd = e.file_name().to_str()?.parse().ok()?;
            let target = std::fs::read_link(e.path()).ok()?.to_string_lossy().into_owned();
            Some((fd, target))
        })
        .collect();
    fds.sort_by_key(|(fd, _)| *fd);
    if fds.len() > MAX_FDS {
        fds.truncate(MAX_FDS);
        out.truncated = true;
    }
    for (fd, target) in fds {
        let kind = if let Some(inode) = target.strip_prefix("socket:[").and_then(|s| s.strip_suffix(']')) {
            if let Ok(i) = inode.parse() {
                sockets.push((fd, i));
            }
            "socket"
        } else if target.starts_with("pipe:") {
            "pipe"
        } else if target.starts_with("anon_inode:") {
            "anon"
        } else if target.starts_with("/dev/") {
            "device"
        } else if target.starts_with('/') {
            "file"
        } else {
            "other"
        };
        out.files.push(OpenFile { fd, kind, target });
    }
    if !sockets.is_empty() {
        // The process's own network namespace (containers have their own tables).
        let table = socket_table(&format!("/proc/{pid}/net"));
        for (_, inode) in sockets {
            if let Some(c) = table.get(&inode) {
                out.conns.push(c.clone());
            }
        }
        out.conns.sort_by(|a, b| a.proto.cmp(b.proto).then(a.state.cmp(b.state)).then(a.local.cmp(&b.local)));
    }
    Ok(out)
}

/// inode -> connection for all TCP/UDP (v4/v6) and Unix sockets in a /proc/<pid>/net directory.
fn socket_table(net: &str) -> HashMap<u64, Conn> {
    let mut m = HashMap::new();
    for (file, proto, v6) in
        [("tcp", "TCP", false), ("tcp6", "TCP", true), ("udp", "UDP", false), ("udp6", "UDP", true)]
    {
        let Ok(text) = std::fs::read_to_string(format!("{net}/{file}")) else { continue };
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 10 {
                continue;
            }
            let (Some(local), Some(remote), Ok(inode)) = (addr(f[1], v6), addr(f[2], v6), f[9].parse::<u64>()) else {
                continue;
            };
            let state = if proto == "TCP" {
                tcp_state(f[3])
            } else if f[2].ends_with(":0000") {
                ""
            } else {
                "connected"
            };
            let remote = if remote.ends_with(":0") && remote.starts_with(['0', '[']) { String::new() } else { remote };
            m.insert(
                inode,
                Conn {
                    proto: if v6 { if proto == "TCP" { "TCP6" } else { "UDP6" } } else { proto },
                    local,
                    remote,
                    state,
                },
            );
        }
    }
    if let Ok(text) = std::fs::read_to_string(format!("{net}/unix")) {
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 7
                && let Ok(inode) = f[6].parse::<u64>()
            {
                let path = f.get(7).map_or("(unnamed)", |p| p).to_string();
                m.insert(inode, Conn { proto: "Unix", local: path, remote: String::new(), state: "" });
            }
        }
    }
    m
}

/// "0100007F:0035" -> "127.0.0.1:53"; IPv6 is four little-endian 32-bit words.
fn addr(s: &str, v6: bool) -> Option<String> {
    let (ip, port) = s.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    if v6 {
        if ip.len() != 32 {
            return None;
        }
        let mut b = [0u8; 16];
        for w in 0..4 {
            let word = u32::from_str_radix(&ip[w * 8..w * 8 + 8], 16).ok()?;
            b[w * 4..w * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        let a = Ipv6Addr::from(b);
        Some(match a.to_ipv4_mapped() {
            Some(v4) => format!("{v4}:{port}"),
            None => format!("[{a}]:{port}"),
        })
    } else {
        let v = u32::from_str_radix(ip, 16).ok()?;
        Some(format!("{}:{port}", Ipv4Addr::from(v.to_le_bytes())))
    }
}

fn tcp_state(hex: &str) -> &'static str {
    match hex {
        "01" => "ESTABLISHED",
        "02" => "SYN_SENT",
        "03" => "SYN_RECV",
        "04" => "FIN_WAIT1",
        "05" => "FIN_WAIT2",
        "06" => "TIME_WAIT",
        "07" => "CLOSE",
        "08" => "CLOSE_WAIT",
        "09" => "LAST_ACK",
        "0A" => "LISTEN",
        "0B" => "CLOSING",
        _ => "?",
    }
}

#[derive(Clone, Debug)]
pub struct Holder {
    pub pid: u32,
    pub name: String,
    pub fd: u32,
    pub path: String,
}

/// Every process (that we may inspect) holding a file whose path contains `needle`.
/// Returns the matches and how many processes couldn't be inspected.
pub fn find_holders(needle: &str) -> (Vec<Holder>, usize) {
    let mut found = Vec::new();
    let mut denied = 0;
    let Ok(rd) = std::fs::read_dir("/proc") else { return (found, 0) };
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            denied += 1;
            continue;
        };
        let mut name = None;
        for f in fds.flatten() {
            let Ok(t) = std::fs::read_link(f.path()) else { continue };
            let t = t.to_string_lossy();
            if t.contains(needle) {
                let name = name.get_or_insert_with(|| {
                    std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default().trim().to_string()
                });
                found.push(Holder {
                    pid,
                    name: name.clone(),
                    fd: f.file_name().to_str().and_then(|s| s.parse().ok()).unwrap_or(0),
                    path: t.into_owned(),
                });
                if found.len() >= 500 {
                    return (found, denied);
                }
            }
        }
    }
    (found, denied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(addr("0100007F:0035", false).unwrap(), "127.0.0.1:53");
        assert_eq!(addr("00000000:0050", false).unwrap(), "0.0.0.0:80");
        assert_eq!(addr("00000000000000000000000001000000:01BB", true).unwrap(), "[::1]:443");
        assert_eq!(addr("0000000000000000FFFF00000100007F:0035", true).unwrap(), "127.0.0.1:53");
        assert_eq!(tcp_state("0A"), "LISTEN");
    }

    #[test]
    fn reads_own_files_and_sockets() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let f = read(std::process::id()).unwrap();
        assert!(f.files.iter().any(|x| x.kind == "socket"));
        assert!(
            f.conns.iter().any(|c| c.proto == "TCP" && c.state == "LISTEN" && c.local.ends_with(&format!(":{port}"))),
            "{:?}",
            f.conns
        );
        let (holders, _) = find_holders("/dev/null");
        let _ = holders; // may or may not be held; must not panic
    }
}
