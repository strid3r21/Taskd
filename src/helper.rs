//! Optional privileged helper (`taskdm --helper`, a hardened systemd system service).
//!
//! Linux only shows per-process network traffic, and other users' disk I/O, to root. The helper
//! measures exactly that and nothing more, and hands out per-PID byte counters over a Unix socket
//! to root and members of the admin groups. It never sends names, command lines or packet data.
//!
//! Network attribution: an AF_PACKET (cooked) socket with a classic BPF filter that truncates every
//! packet to its first 128 bytes (headers only; payloads never reach userspace), read in batches of
//! 64 with recvmmsg. Each packet's local port is matched to a socket inode via /proc/net/{tcp,udp}{,6}
//! and the inode to a process via /proc/<pid>/fd. Loopback traffic is ignored.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const SOCKET: &str = "/run/taskd-helper/sock";

/// Socket path. TASKD_HELPER_SOCKET overrides it (tests; for the system service the environment
/// comes only from the unit file).
pub fn socket_path() -> String {
    std::env::var("TASKD_HELPER_SOCKET").unwrap_or_else(|_| SOCKET.to_string())
}
pub const PROTOCOL: u32 = 1;
const SNAPLEN: u32 = 128;

// ------------------------------------------------------------------ packet parsing

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Proto {
    Tcp,
    Udp,
}

/// For a packet starting at the IP header: (protocol, source port, destination port, IP length).
pub fn parse_ip(pkt: &[u8], ethertype: u16) -> Option<(Proto, u16, u16, u32)> {
    let (proto, l4, len) = match ethertype {
        0x0800 => {
            let ihl = (*pkt.first()? & 0x0f) as usize * 4;
            let total = u16::from_be_bytes([*pkt.get(2)?, *pkt.get(3)?]) as u32;
            // Only the first fragment carries the ports.
            let frag_off = u16::from_be_bytes([*pkt.get(6)?, *pkt.get(7)?]) & 0x1fff;
            if frag_off != 0 {
                return None;
            }
            (*pkt.get(9)?, ihl, total)
        }
        0x86DD => {
            let payload = u16::from_be_bytes([*pkt.get(4)?, *pkt.get(5)?]) as u32;
            (*pkt.get(6)?, 40usize, payload + 40)
        }
        _ => return None,
    };
    let proto = match proto {
        6 => Proto::Tcp,
        17 => Proto::Udp,
        _ => return None,
    };
    let sport = u16::from_be_bytes([*pkt.get(l4)?, *pkt.get(l4 + 1)?]);
    let dport = u16::from_be_bytes([*pkt.get(l4 + 2)?, *pkt.get(l4 + 3)?]);
    Some((proto, sport, dport, len))
}

/// (proto, local port) -> socket inode, from /proc/net/{tcp,udp}{,6} under `net` (e.g. "/proc/net").
pub fn port_table(net: &str) -> HashMap<(Proto, u16), u64> {
    let mut m = HashMap::new();
    for (file, proto) in [("tcp", Proto::Tcp), ("tcp6", Proto::Tcp), ("udp", Proto::Udp), ("udp6", Proto::Udp)] {
        let Ok(text) = std::fs::read_to_string(format!("{net}/{file}")) else { continue };
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            let (Some(local), Some(inode)) = (f.get(1), f.get(9)) else { continue };
            let (Some(port), Ok(inode)) =
                (local.rsplit_once(':').and_then(|(_, p)| u16::from_str_radix(p, 16).ok()), inode.parse::<u64>())
            else {
                continue;
            };
            // Prefer established sockets over listeners for the same port (both map to the same
            // process in practice; this just keeps the first entry stable).
            if inode != 0 {
                m.entry((proto, port)).or_insert(inode);
            }
        }
    }
    m
}

/// socket inode -> pid, by scanning every process's fds.
pub fn inode_owners() -> HashMap<u64, u32> {
    let mut m = HashMap::new();
    let Ok(rd) = std::fs::read_dir("/proc") else { return m };
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { continue };
        for f in fds.flatten() {
            if let Ok(t) = std::fs::read_link(f.path())
                && let Some(i) = t.to_str().and_then(|t| t.strip_prefix("socket:[")).and_then(|t| t.strip_suffix(']'))
                && let Ok(inode) = i.parse()
            {
                m.insert(inode, pid);
            }
        }
    }
    m
}

// ------------------------------------------------------------------ accounting

#[derive(Default)]
struct Counters {
    /// pid -> (received, sent) bytes since the helper started. pid 0 = unattributed.
    net: HashMap<u32, (u64, u64)>,
}

struct Attributor {
    ports: HashMap<(Proto, u16), u64>,
    owners: HashMap<u64, u32>,
    ports_at: Option<Instant>,
    owners_at: Option<Instant>,
}

impl Attributor {
    fn new() -> Self {
        Attributor { ports: HashMap::new(), owners: HashMap::new(), ports_at: None, owners_at: None }
    }

    /// The pid owning the local socket, refreshing the tables (rate-limited) on a miss.
    fn pid(&mut self, proto: Proto, local_port: u16) -> u32 {
        let lookup = |s: &Self| s.ports.get(&(proto, local_port)).and_then(|i| s.owners.get(i)).copied();
        if let Some(p) = lookup(self) {
            return p;
        }
        if self.ports_at.is_none_or(|t| t.elapsed() > Duration::from_millis(500)) {
            self.ports = port_table("/proc/net");
            self.ports_at = Some(Instant::now());
        }
        if let Some(inode) = self.ports.get(&(proto, local_port))
            && !self.owners.contains_key(inode)
            && self.owners_at.is_none_or(|t| t.elapsed() > Duration::from_secs(2))
        {
            self.owners = inode_owners();
            self.owners_at = Some(Instant::now());
        }
        lookup(self).unwrap_or(0)
    }
}

// ------------------------------------------------------------------ capture

fn open_capture() -> std::io::Result<OwnedFd> {
    // SAFETY: plain socket/setsockopt syscalls with correctly sized arguments.
    unsafe {
        let fd = libc::socket(
            libc::AF_PACKET,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            (libc::ETH_P_ALL as u16).to_be() as i32,
        );
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = OwnedFd::from_raw_fd(fd);
        // Classic BPF: `ret #SNAPLEN` — keep only the first 128 bytes of every packet.
        let mut prog = [libc::sock_filter { code: 0x06, jt: 0, jf: 0, k: SNAPLEN }];
        let fprog = libc::sock_fprog { len: 1, filter: prog.as_mut_ptr() };
        if libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_ATTACH_FILTER,
            (&fprog as *const libc::sock_fprog).cast(),
            std::mem::size_of::<libc::sock_fprog>() as u32,
        ) != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let size: libc::c_int = 4 << 20;
        libc::setsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_RCVBUF, (&size as *const libc::c_int).cast(), 4);
        Ok(fd)
    }
}

fn loopback_index() -> i32 {
    // SAFETY: if_nametoindex reads a NUL-terminated string.
    unsafe { libc::if_nametoindex(c"lo".as_ptr()) as i32 }
}

fn capture_loop(counters: Arc<Mutex<Counters>>) -> std::io::Error {
    let fd = match open_capture() {
        Ok(f) => f,
        Err(e) => return e,
    };
    // Loopback is skipped (local traffic isn't "network"), except for tests in a private netns.
    let lo = if std::env::var_os("TASKD_HELPER_LOOPBACK").is_some() { -1 } else { loopback_index() };
    const BATCH: usize = 64;
    let mut bufs = vec![[0u8; SNAPLEN as usize]; BATCH];
    // SAFETY: zeroed sockaddr_ll / iovec / mmsghdr are valid initial states.
    let mut addrs: Vec<libc::sockaddr_ll> = vec![unsafe { std::mem::zeroed() }; BATCH];
    let mut iovs: Vec<libc::iovec> = vec![unsafe { std::mem::zeroed() }; BATCH];
    let mut msgs: Vec<libc::mmsghdr> = vec![unsafe { std::mem::zeroed() }; BATCH];
    let mut attr = Attributor::new();
    loop {
        for i in 0..BATCH {
            iovs[i] = libc::iovec { iov_base: bufs[i].as_mut_ptr().cast(), iov_len: SNAPLEN as usize };
            msgs[i].msg_hdr.msg_name = (&mut addrs[i] as *mut libc::sockaddr_ll).cast();
            msgs[i].msg_hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_ll>() as u32;
            msgs[i].msg_hdr.msg_iov = &mut iovs[i];
            msgs[i].msg_hdr.msg_iovlen = 1;
            msgs[i].msg_len = 0;
        }
        // SAFETY: every mmsghdr points at live buffers sized as declared above.
        let n = unsafe {
            libc::recvmmsg(fd.as_raw_fd(), msgs.as_mut_ptr(), BATCH as u32, libc::MSG_WAITFORONE, std::ptr::null_mut())
        };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return e;
        }
        let mut c = counters.lock().unwrap();
        for i in 0..n as usize {
            let a = &addrs[i];
            if a.sll_ifindex == lo {
                continue;
            }
            let got = (msgs[i].msg_len as usize).min(SNAPLEN as usize);
            let Some((proto, sport, dport, len)) = parse_ip(&bufs[i][..got], u16::from_be(a.sll_protocol)) else {
                continue;
            };
            let outgoing = a.sll_pkttype == libc::PACKET_OUTGOING;
            let local = if outgoing { sport } else { dport };
            let pid = attr.pid(proto, local);
            let e = c.net.entry(pid).or_default();
            if outgoing {
                e.1 += len as u64;
            } else {
                e.0 += len as u64;
            }
        }
    }
}

// ------------------------------------------------------------------ serving

/// Root, or a member of an admin group (by the peer's kernel-verified uid).
fn peer_allowed(stream: &UnixStream) -> bool {
    let mut cred = libc::ucred { pid: 0, uid: u32::MAX, gid: u32::MAX };
    let mut len = std::mem::size_of::<libc::ucred>() as u32;
    // SAFETY: SO_PEERCRED fills a ucred of the given size.
    let ok = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    } == 0;
    ok && (cred.uid == 0 || user_in_admin_group(cred.uid))
}

fn user_in_admin_group(uid: u32) -> bool {
    let Ok(passwd) = std::fs::read_to_string("/etc/passwd") else { return false };
    let Some((name, gid)) = passwd.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() > 3 && f[2].parse::<u32>().ok() == Some(uid)).then(|| (f[0].to_string(), f[3].to_string()))
    }) else {
        return false;
    };
    let Ok(group) = std::fs::read_to_string("/etc/group") else { return false };
    group.lines().any(|l| {
        let f: Vec<&str> = l.split(':').collect();
        f.len() > 3
            && ["sudo", "adm", "wheel", "admin"].contains(&f[0])
            && (f[2] == gid || f[3].split(',').any(|m| m == name))
    })
}

fn snapshot_json(counters: &Mutex<Counters>) -> Value {
    let net: serde_json::Map<String, Value> =
        counters.lock().unwrap().net.iter().map(|(p, (rx, tx))| (p.to_string(), json!([rx, tx]))).collect();
    // Disk I/O of every process (unprivileged readers only see their own).
    let mut io = serde_json::Map::new();
    if let Ok(rd) = std::fs::read_dir("/proc") {
        let mut buf = Vec::with_capacity(512);
        for e in rd.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
            if crate::util::read_into(&format!("/proc/{pid}/io"), &mut buf).is_ok() {
                let r = crate::util::kv_u64(&buf, b"read_bytes:").unwrap_or(0);
                let w = crate::util::kv_u64(&buf, b"write_bytes:").unwrap_or(0);
                if r + w > 0 {
                    io.insert(pid.to_string(), json!([r, w]));
                }
            }
        }
    }
    json!({"v": PROTOCOL, "version": env!("CARGO_PKG_VERSION"), "net": net, "io": io})
}

/// `taskdm --helper` (as root, normally via systemd).
pub fn run() -> i32 {
    // SAFETY: geteuid never fails.
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("taskdm --helper must run as root (it is normally started by systemd)");
        return 1;
    }
    let counters = Arc::new(Mutex::new(Counters::default()));
    {
        let c = counters.clone();
        std::thread::Builder::new()
            .name("capture".into())
            .spawn(move || {
                let e = capture_loop(c);
                eprintln!("taskd-helper: packet capture stopped: {e} (network attribution unavailable)");
            })
            .expect("spawn capture thread");
    }
    let sock = socket_path();
    if let Some(dir) = std::path::Path::new(&sock).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::remove_file(&sock);
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("taskd-helper: {sock}: {e}");
            return 1;
        }
    };
    // Anyone may connect; peer_allowed() decides who gets an answer.
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o666));
    eprintln!("taskd-helper {} listening on {sock}", env!("CARGO_PKG_VERSION"));
    for stream in listener.incoming().flatten() {
        let c = counters.clone();
        std::thread::spawn(move || serve(stream, &c));
    }
    0
}

fn serve(stream: UnixStream, counters: &Mutex<Counters>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    if !peer_allowed(&stream) {
        let _ = (&stream).write_all(b"{\"error\":\"not allowed\"}\n");
        return;
    }
    // One request per line ("get"); a client may keep the connection open.
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    while reader.read_line(&mut line).is_ok_and(|n| n > 0) {
        if line.trim() == "get" {
            let reply = snapshot_json(counters).to_string() + "\n";
            if (&stream).write_all(reply.as_bytes()).is_err() {
                return;
            }
        }
        line.clear();
    }
}

// ------------------------------------------------------------------ client (sampler side)

/// Cumulative counters from the helper, keyed by pid.
#[derive(Default)]
pub struct HelperData {
    pub net: HashMap<u32, (u64, u64)>,
    pub io: HashMap<u32, (u64, u64)>,
    pub version: String,
}

pub struct Client {
    conn: Option<BufReader<UnixStream>>,
    retry_at: Option<Instant>,
}

impl Client {
    pub fn new() -> Self {
        Client { conn: None, retry_at: None }
    }

    /// Fetch counters, or None if no helper is running/allowed (retried every 10 s).
    pub fn fetch(&mut self) -> Option<HelperData> {
        if self.conn.is_none() {
            if self.retry_at.is_some_and(|t| t.elapsed() < Duration::from_secs(10)) {
                return None;
            }
            self.retry_at = Some(Instant::now());
            let s = UnixStream::connect(socket_path()).ok()?;
            let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
            self.conn = Some(BufReader::new(s));
        }
        let res = (|| {
            let r = self.conn.as_mut()?;
            r.get_mut().write_all(b"get\n").ok()?;
            let mut line = String::new();
            r.read_line(&mut line).ok()?;
            let v: Value = serde_json::from_str(&line).ok()?;
            if v["v"].as_u64()? != PROTOCOL as u64 {
                return None;
            }
            let pairs = |key: &str| -> HashMap<u32, (u64, u64)> {
                v[key]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter_map(|(p, a)| Some((p.parse().ok()?, (a[0].as_u64()?, a[1].as_u64()?))))
                    .collect()
            };
            Some(HelperData { net: pairs("net"), io: pairs("io"), version: v["version"].as_str().unwrap_or("").into() })
        })();
        if res.is_none() {
            self.conn = None; // reconnect next time (rate-limited)
        }
        res
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

// ------------------------------------------------------------------ install / uninstall (as root)

const INSTALLED_BIN: &str = "/usr/local/libexec/taskd/taskd-helper";
const UNIT_PATH: &str = "/etc/systemd/system/taskd-helper.service";
const DEB_UNIT: &str = "/usr/lib/systemd/system/taskd-helper.service";

pub fn unit_text(exec: &str) -> String {
    format!(
        "[Unit]\nDescription=Taskd privileged helper (per-app network and disk I/O)\nDocumentation=https://github.com/strid3r21/Taskd\n\n\
         [Service]\nExecStart={exec} --helper\nRestart=on-failure\nRestartSec=5\nNice=5\n\
         RuntimeDirectory=taskd-helper\nRuntimeDirectoryMode=0755\n\
         CapabilityBoundingSet=CAP_NET_RAW CAP_DAC_READ_SEARCH CAP_SYS_PTRACE\n\
         AmbientCapabilities=CAP_NET_RAW CAP_DAC_READ_SEARCH CAP_SYS_PTRACE\n\
         NoNewPrivileges=yes\nProtectSystem=strict\nProtectHome=yes\nPrivateTmp=yes\nPrivateDevices=yes\n\
         ProtectKernelTunables=yes\nProtectKernelModules=yes\nProtectControlGroups=yes\nProtectClock=yes\n\
         RestrictAddressFamilies=AF_UNIX AF_PACKET\nRestrictNamespaces=yes\nLockPersonality=yes\n\
         MemoryDenyWriteExecute=yes\nSystemCallArchitectures=native\nMemoryMax=96M\n\n\
         [Install]\nWantedBy=multi-user.target\n"
    )
}

fn systemctl(args: &[&str]) -> Result<(), String> {
    let o = std::process::Command::new(crate::actions::sys_bin("systemctl"))
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if o.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&o.stderr).trim().to_string()) }
}

/// `taskdm --install-helper` (run through pkexec). The .deb ships its own unit; tarball installs
/// get a root-owned copy of the binary so the root service never runs a user-writable file.
pub fn install() -> i32 {
    let r = (|| -> Result<(), String> {
        // SAFETY: geteuid never fails.
        if unsafe { libc::geteuid() } != 0 {
            return Err("must run as root (use pkexec)".into());
        }
        if !std::path::Path::new(DEB_UNIT).exists() {
            let me = std::fs::read("/proc/self/exe").map_err(|e| e.to_string())?;
            std::fs::create_dir_all("/usr/local/libexec/taskd").map_err(|e| e.to_string())?;
            let tmp = format!("{INSTALLED_BIN}.new");
            std::fs::write(&tmp, me).map_err(|e| e.to_string())?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
            std::fs::rename(&tmp, INSTALLED_BIN).map_err(|e| e.to_string())?;
            std::fs::write(UNIT_PATH, unit_text(INSTALLED_BIN)).map_err(|e| e.to_string())?;
            systemctl(&["daemon-reload"])?;
        }
        systemctl(&["enable", "--now", "taskd-helper.service"])?;
        systemctl(&["restart", "taskd-helper.service"])
    })();
    match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("taskdm --install-helper: {e}");
            1
        }
    }
}

pub fn uninstall() -> i32 {
    let _ = systemctl(&["disable", "--now", "taskd-helper.service"]);
    if !std::path::Path::new(DEB_UNIT).exists() {
        let _ = std::fs::remove_file(UNIT_PATH);
        let _ = std::fs::remove_file(INSTALLED_BIN);
        let _ = systemctl(&["daemon-reload"]);
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ipv4_tcp_and_ipv6_udp() {
        // IPv4, IHL 5, total length 60, TCP, ports 443 -> 51000.
        let mut v4 = vec![0u8; 40];
        v4[0] = 0x45;
        v4[2..4].copy_from_slice(&60u16.to_be_bytes());
        v4[9] = 6;
        v4[20..22].copy_from_slice(&443u16.to_be_bytes());
        v4[22..24].copy_from_slice(&51000u16.to_be_bytes());
        assert_eq!(parse_ip(&v4, 0x0800), Some((Proto::Tcp, 443, 51000, 60)));
        // A non-first fragment has no ports.
        v4[7] = 1;
        assert_eq!(parse_ip(&v4, 0x0800), None);
        // IPv6, payload 100, UDP, ports 53 -> 40000.
        let mut v6 = vec![0u8; 48];
        v6[0] = 0x60;
        v6[4..6].copy_from_slice(&100u16.to_be_bytes());
        v6[6] = 17;
        v6[40..42].copy_from_slice(&53u16.to_be_bytes());
        v6[42..44].copy_from_slice(&40000u16.to_be_bytes());
        assert_eq!(parse_ip(&v6, 0x86DD), Some((Proto::Udp, 53, 40000, 140)));
        assert_eq!(parse_ip(&v6[..10], 0x86DD), None, "truncated packets are ignored");
        assert_eq!(parse_ip(&v6, 0x0806), None, "ARP is ignored");
    }

    #[test]
    fn maps_own_socket_to_own_pid() {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = sock.local_addr().unwrap().port();
        let table = port_table("/proc/self/net");
        let inode = *table.get(&(Proto::Udp, port)).expect("our UDP socket is listed");
        assert_eq!(inode_owners().get(&inode), Some(&std::process::id()));
    }

    #[test]
    fn packaged_unit_matches_generated() {
        let packaged = include_str!("../packaging/taskd-helper.service");
        assert_eq!(packaged, unit_text("/usr/bin/taskdm"), "regenerate packaging/taskd-helper.service");
    }

    #[test]
    fn unit_is_hardened() {
        let u = unit_text("/usr/bin/taskdm");
        for k in [
            "NoNewPrivileges=yes",
            "ProtectSystem=strict",
            "ProtectHome=yes",
            "RestrictAddressFamilies=AF_UNIX AF_PACKET",
        ] {
            assert!(u.contains(k), "{k}");
        }
    }
}
