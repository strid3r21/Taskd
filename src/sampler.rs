//! The sampler reads `/proc` and `/sys` directly into reused buffers and produces
//! immutable [`Snapshot`]s. It runs on its own thread; the UI only ever reads the
//! latest published snapshot, so a slow frame never delays sampling and vice versa.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use crate::gpu::{GpuInfo, GpuReader};
use crate::util::{
    CachedFile, fields, kv_u64, parse_i64, parse_u64, pread_record, read_into, read_record, read_string, read_u64, trim,
};

pub const HISTORY: usize = 120;

/// Fixed-capacity ring of recent samples, used for graphs.
#[derive(Clone, Default)]
pub struct History(VecDeque<f32>);

impl History {
    pub fn push(&mut self, v: f32) {
        if self.0.len() == HISTORY {
            self.0.pop_front();
        }
        self.0.push_back(v);
    }
    pub fn iter(&self) -> impl ExactSizeIterator<Item = f32> + '_ {
        self.0.iter().copied()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn max(&self) -> f32 {
        self.0.iter().copied().fold(0.0, f32::max)
    }
}

#[derive(Clone)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub name: Arc<str>,
    pub cmd: Arc<str>,
    pub user: Arc<str>,
    pub uid: u32,
    pub state: u8,
    /// Percent of the whole machine (0..=100).
    pub cpu: f32,
    /// Private resident memory (RSS minus file-backed/shared pages), bytes.
    pub mem: u64,
    pub rss: u64,
    pub threads: u32,
    pub nice: i32,
    pub read_rate: f64,
    pub write_rate: f64,
    /// Seconds since boot at which the process started.
    pub start: f64,
    pub kernel: bool,
    /// GPU utilization percent (NVML or DRM fdinfo), 0 when unknown.
    pub gpu: f32,
    /// systemd unit the process runs in: an app scope (`app-flatpak-com.discordapp.Discord-1234.scope`)
    /// or a service (`ssh.service`). `None` for session scopes and kernel threads.
    pub unit: Option<Arc<str>>,
}

#[derive(Clone, Default)]
pub struct CpuInfo {
    pub model: String,
    pub logical: usize,
    pub physical: usize,
    pub usage: f32,
    pub user: f32,
    pub system: f32,
    pub iowait: f32,
    pub per_core: Vec<f32>,
    pub freq_mhz: Option<f32>,
    pub temp_c: Option<f32>,
    pub ctxt_rate: f64,
    pub hist: History,
    pub core_hist: Vec<History>,
}

#[derive(Clone, Default)]
pub struct MemInfo {
    pub total: u64,
    pub available: u64,
    pub used: u64,
    pub cached: u64,
    pub buffers: u64,
    pub shmem: u64,
    pub dirty: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub hist: History,
    pub swap_hist: History,
}

#[derive(Clone, Default)]
pub struct DiskInfo {
    pub name: String,
    pub model: String,
    pub size: u64,
    pub rotational: bool,
    pub read_rate: f64,
    pub write_rate: f64,
    pub busy: f32,
    pub read_hist: History,
    pub write_hist: History,
    pub busy_hist: History,
    prev: (u64, u64, u64),
}

#[derive(Clone, Default)]
pub struct NetInfo {
    pub name: String,
    pub rx_rate: f64,
    pub tx_rate: f64,
    pub rx_total: u64,
    pub tx_total: u64,
    pub rx_hist: History,
    pub tx_hist: History,
    pub speed_mbps: Option<u64>,
    prev: (u64, u64),
}

#[derive(Clone, Default)]
pub struct Snapshot {
    pub generation: u64,
    pub elapsed: f64,
    pub cost: Duration,
    pub procs: Vec<ProcInfo>,
    pub cpu: CpuInfo,
    pub mem: MemInfo,
    pub disks: Vec<DiskInfo>,
    pub nets: Vec<NetInfo>,
    pub gpus: Vec<GpuInfo>,
    pub load: [f32; 3],
    pub uptime: f64,
    pub boot_time: u64,
    pub threads: u64,
    pub running: u64,
}

struct ProcEntry {
    start: u64,
    cpu: u64,
    rd: u64,
    wr: u64,
    name: Arc<str>,
    cmd: Arc<str>,
    user: Arc<str>,
    uid: u32,
    /// Raw `comm` as last seen: a change means the process exec()'d into another program.
    comm: Box<[u8]>,
    io_ok: bool,
    seen: u64,
    /// Number of samples this process has been seen in.
    age: u32,
    unit: Option<Arc<str>>,
    /// Open fds re-read with pread each tick (see `util::pread_record`).
    stat_fd: Option<File>,
    statm_fd: Option<File>,
    io_fd: Option<File>,
}

struct SysFiles {
    stat: CachedFile,
    meminfo: CachedFile,
    diskstats: CachedFile,
    netdev: CachedFile,
    loadavg: CachedFile,
    uptime: CachedFile,
}

pub struct Sampler {
    buf: Vec<u8>,
    path: String,
    procs: HashMap<u32, ProcEntry>,
    users: HashMap<u32, Arc<str>>,
    snap: Snapshot,
    prev_cpu: Vec<[u64; 8]>,
    prev_ctxt: u64,
    last: Instant,
    page_size: u64,
    clk_tck: f64,
    freq_files: Vec<CachedFile>,
    temp_file: Option<CachedFile>,
    sys: SysFiles,
    gpu: GpuReader,
    generation: u64,
    empty: Arc<str>,
    fd_limit: u64,
    cache_fds: bool,
}

impl Sampler {
    pub fn new() -> Self {
        // SAFETY: sysconf is always safe to call.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
        let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;

        let mut s = Sampler {
            buf: Vec::with_capacity(8192),
            path: String::with_capacity(64),
            procs: HashMap::with_capacity(1024),
            users: load_users(),
            snap: Snapshot::default(),
            prev_cpu: Vec::new(),
            prev_ctxt: 0,
            last: Instant::now(),
            page_size,
            clk_tck,
            freq_files: Vec::new(),
            temp_file: find_cpu_temp().map(|p| CachedFile::new(p, true)),
            sys: SysFiles {
                stat: CachedFile::new("/proc/stat", false),
                meminfo: CachedFile::new("/proc/meminfo", false),
                diskstats: CachedFile::new("/proc/diskstats", false),
                netdev: CachedFile::new("/proc/net/dev", false),
                loadavg: CachedFile::new("/proc/loadavg", true),
                uptime: CachedFile::new("/proc/uptime", true),
            },
            gpu: GpuReader::new(),
            generation: 0,
            empty: Arc::from(""),
            fd_limit: raise_fd_limit(),
            cache_fds: false,
        };
        s.init_cpu_static();
        s.rescan_devices();
        if let Some(bt) = read_into("/proc/stat", &mut s.buf).ok().and_then(|_| kv_u64(&s.buf, b"btime ")) {
            s.snap.boot_time = bt;
        }
        // Prime the deltas, then drop what that pass pushed into the histories: its "rates"
        // are averages since boot (CPU) or zero, not real recent samples.
        s.sample();
        s.snap.cpu.hist = History::default();
        s.snap.cpu.core_hist.iter_mut().for_each(|h| *h = History::default());
        s.snap.mem.hist = History::default();
        s.snap.mem.swap_hist = History::default();
        s
    }

    fn init_cpu_static(&mut self) {
        let info = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        let mut cores = HashSet::new();
        let mut phys = String::new();
        for line in info.lines() {
            let Some((k, v)) = line.split_once(':') else { continue };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "model name" | "Model" | "Hardware" if self.snap.cpu.model.is_empty() => {
                    self.snap.cpu.model = v.to_string()
                }
                "physical id" => phys = v.to_string(),
                "core id" => {
                    cores.insert(format!("{phys}:{v}"));
                }
                _ => {}
            }
        }
        // Online CPUs of the machine. Not available_parallelism(): that follows this process's
        // affinity/cgroup quota (e.g. under taskset) and would skew every per-core figure.
        let logical = std::fs::read_to_string("/sys/devices/system/cpu/online")
            .ok()
            .and_then(|s| count_cpu_list(s.trim()))
            .unwrap_or_else(|| thread::available_parallelism().map_or(1, |n| n.get()));
        self.snap.cpu.physical = if cores.is_empty() { logical } else { cores.len() };
        self.snap.cpu.logical = logical;
        for i in 0..1024 {
            let p = format!("/sys/devices/system/cpu/cpu{i}/cpufreq/scaling_cur_freq");
            if std::path::Path::new(&p).exists() {
                self.freq_files.push(CachedFile::new(p, true));
            } else if i >= logical {
                break;
            }
        }
    }

    /// Discover block devices and network interfaces (also re-run periodically for hotplug).
    fn rescan_devices(&mut self) {
        let mut disks = Vec::new();
        if let Ok(rd) = std::fs::read_dir("/sys/block") {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if ["loop", "ram", "zram", "dm-", "sr", "fd"].iter().any(|p| name.starts_with(p)) {
                    continue;
                }
                let base = format!("/sys/block/{name}");
                let size = read_u64(&format!("{base}/size"), &mut self.buf).unwrap_or(0) * 512;
                if size == 0 {
                    continue;
                }
                if let Some(old) = self.snap.disks.iter().position(|d| d.name == name) {
                    disks.push(self.snap.disks.swap_remove(old));
                    continue;
                }
                disks.push(DiskInfo {
                    model: read_string(&format!("{base}/device/model")).unwrap_or_default(),
                    rotational: read_u64(&format!("{base}/queue/rotational"), &mut self.buf) == Some(1),
                    size,
                    name,
                    ..Default::default()
                });
            }
        }
        disks.sort_by(|a, b| a.name.cmp(&b.name));
        self.snap.disks = disks;

        let mut nets = Vec::new();
        if let Ok(rd) = std::fs::read_dir("/sys/class/net") {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name == "lo" || ["veth", "docker", "br-", "virbr", "vnet"].iter().any(|p| name.starts_with(p)) {
                    continue;
                }
                let speed = read_into(&format!("/sys/class/net/{name}/speed"), &mut self.buf)
                    .ok()
                    .and_then(|_| parse_i64(trim(&self.buf)))
                    .filter(|&s| s > 0)
                    .map(|s| s as u64);
                if let Some(old) = self.snap.nets.iter().position(|n| n.name == name) {
                    let mut n = self.snap.nets.swap_remove(old);
                    n.speed_mbps = speed;
                    nets.push(n);
                    continue;
                }
                nets.push(NetInfo { name, speed_mbps: speed, ..Default::default() });
            }
        }
        nets.sort_by(|a, b| a.name.cmp(&b.name));
        self.snap.nets = nets;
    }

    /// Take one sample and return the resulting snapshot.
    pub fn sample(&mut self) -> Arc<Snapshot> {
        let t0 = Instant::now();
        let elapsed = t0.duration_since(self.last).as_secs_f64().max(1e-3);
        self.last = t0;
        self.generation += 1;
        if self.generation % 30 == 0 {
            self.rescan_devices();
        }

        let total_delta = self.sample_cpu(elapsed);
        self.sample_mem();
        self.sample_disks(elapsed);
        self.sample_net(elapsed);
        self.sample_misc();
        self.snap.gpus = self.gpu.read(elapsed);
        let procs = self.sample_procs(elapsed, total_delta);

        let mut out = self.snap.clone();
        out.threads = procs.iter().map(|p| p.threads as u64).sum();
        out.procs = procs;
        out.generation = self.generation;
        out.elapsed = elapsed;
        out.cost = t0.elapsed();
        Arc::new(out)
    }

    /// Returns the jiffies elapsed across all CPUs since the last sample.
    fn sample_cpu(&mut self, elapsed: f64) -> u64 {
        if self.sys.stat.read(&mut self.buf).is_err() {
            return 0;
        }
        let mut total_delta = 0;
        let mut seen_cores = 0usize;
        let cpu = &mut self.snap.cpu;
        let mut online = vec![false; cpu.per_core.len()];
        for line in self.buf.split(|&c| c == b'\n') {
            if line.starts_with(b"cpu") {
                let mut it = fields(line);
                let label = it.next().unwrap_or_default();
                let mut v = [0u64; 8];
                for (slot, f) in v.iter_mut().zip(it) {
                    *slot = parse_u64(f).unwrap_or(0);
                }
                // Key per-core data by the CPU *number* (cpu7 -> 7): offline CPUs are missing
                // from /proc/stat, so position would mislabel every core after a gap.
                let core = if label == b"cpu" { None } else { parse_u64(&label[3..]).map(|n| n as usize) };
                if label != b"cpu" && core.is_none() {
                    continue;
                }
                let idx = core.map_or(0, |c| c + 1);
                if self.prev_cpu.len() <= idx {
                    self.prev_cpu.resize(idx + 1, [0; 8]);
                }
                let p = self.prev_cpu[idx];
                let d: [u64; 8] = std::array::from_fn(|i| v[i].saturating_sub(p[i]));
                let total: u64 = d.iter().sum();
                let idle = d[3] + d[4];
                let usage = if total == 0 { 0.0 } else { 100.0 * (total - idle) as f32 / total as f32 };
                self.prev_cpu[idx] = v;
                if idx == 0 {
                    total_delta = total;
                    let pct = |x: u64| if total == 0 { 0.0 } else { 100.0 * x as f32 / total as f32 };
                    cpu.usage = usage;
                    cpu.user = pct(d[0] + d[1]);
                    cpu.system = pct(d[2] + d[5] + d[6]);
                    cpu.iowait = pct(d[4]);
                    cpu.hist.push(usage);
                } else if let Some(core) = core {
                    if cpu.per_core.len() <= core {
                        cpu.per_core.resize(core + 1, 0.0);
                        cpu.core_hist.resize(core + 1, History::default());
                        online.resize(core + 1, false);
                    }
                    cpu.per_core[core] = usage;
                    cpu.core_hist[core].push(usage);
                    online[core] = true;
                    seen_cores += 1;
                }
            } else if let Some(rest) = line.strip_prefix(b"ctxt ") {
                let c = parse_u64(rest).unwrap_or(0);
                if self.prev_ctxt > 0 {
                    cpu.ctxt_rate = c.saturating_sub(self.prev_ctxt) as f64 / elapsed;
                }
                self.prev_ctxt = c;
            } else if let Some(rest) = line.strip_prefix(b"procs_running ") {
                self.snap.running = parse_u64(rest).unwrap_or(0);
            }
        }
        // CPUs that went offline: show them idle instead of frozen at their last value.
        for (i, on) in online.iter().enumerate() {
            if !on {
                self.snap.cpu.per_core[i] = 0.0;
                self.snap.cpu.core_hist[i].push(0.0);
                if let Some(p) = self.prev_cpu.get_mut(i + 1) {
                    *p = [0; 8]; // its counters restart when it comes back
                }
            }
        }
        if seen_cores > 0 {
            self.snap.cpu.logical = seen_cores;
        }

        if !self.freq_files.is_empty() {
            let (mut sum, mut n) = (0u64, 0u64);
            for f in &mut self.freq_files {
                if let Some(khz) = f.read_u64(&mut self.buf) {
                    sum += khz;
                    n += 1;
                }
            }
            self.snap.cpu.freq_mhz = (n > 0).then(|| sum as f32 / n as f32 / 1000.0);
        }
        if let Some(f) = &mut self.temp_file {
            self.snap.cpu.temp_c = f.read_u64(&mut self.buf).map(|m| m as f32 / 1000.0);
        }

        total_delta
    }

    fn sample_mem(&mut self) {
        if self.sys.meminfo.read(&mut self.buf).is_err() {
            return;
        }
        let k = |key: &[u8]| kv_u64(&self.buf, key).unwrap_or(0) * 1024;
        let total = k(b"MemTotal:");
        let free = k(b"MemFree:");
        let available = kv_u64(&self.buf, b"MemAvailable:").map_or(free, |v| v * 1024);
        let m = &mut self.snap.mem;
        m.total = total;
        m.available = available;
        m.used = total.saturating_sub(available);
        m.buffers = k(b"Buffers:");
        m.cached = k(b"Cached:") + k(b"SReclaimable:");
        m.shmem = k(b"Shmem:");
        m.dirty = k(b"Dirty:");
        m.swap_total = k(b"SwapTotal:");
        m.swap_used = m.swap_total.saturating_sub(k(b"SwapFree:"));
        let pct = |a: u64, b: u64| if b == 0 { 0.0 } else { 100.0 * a as f32 / b as f32 };
        m.hist.push(pct(m.used, m.total));
        m.swap_hist.push(pct(m.swap_used, m.swap_total));
    }

    fn sample_disks(&mut self, elapsed: f64) {
        if self.snap.disks.is_empty() || self.sys.diskstats.read(&mut self.buf).is_err() {
            return;
        }
        for line in self.buf.split(|&c| c == b'\n') {
            let mut it = fields(line).skip(2);
            let Some(name) = it.next() else { continue };
            let Some(d) = self.snap.disks.iter_mut().find(|d| d.name.as_bytes() == name) else { continue };
            let f: Vec<u64> = it.take(10).map(|x| parse_u64(x).unwrap_or(0)).collect();
            if f.len() < 10 {
                continue;
            }
            let (rd, wr, io_ms) = (f[2] * 512, f[6] * 512, f[9]);
            if d.prev != (0, 0, 0) {
                d.read_rate = rd.saturating_sub(d.prev.0) as f64 / elapsed;
                d.write_rate = wr.saturating_sub(d.prev.1) as f64 / elapsed;
                d.busy = (io_ms.saturating_sub(d.prev.2) as f64 / (elapsed * 10.0)).min(100.0) as f32;
            }
            d.prev = (rd, wr, io_ms);
            d.read_hist.push(d.read_rate as f32);
            d.write_hist.push(d.write_rate as f32);
            d.busy_hist.push(d.busy);
        }
    }

    fn sample_net(&mut self, elapsed: f64) {
        if self.snap.nets.is_empty() || self.sys.netdev.read(&mut self.buf).is_err() {
            return;
        }
        for line in self.buf.split(|&c| c == b'\n') {
            let Some(colon) = line.iter().position(|&c| c == b':') else { continue };
            let name = trim(&line[..colon]);
            let Some(n) = self.snap.nets.iter_mut().find(|n| n.name.as_bytes() == name) else { continue };
            let f: Vec<u64> = fields(&line[colon + 1..]).take(9).map(|x| parse_u64(x).unwrap_or(0)).collect();
            if f.len() < 9 {
                continue;
            }
            let (rx, tx) = (f[0], f[8]);
            if n.prev != (0, 0) {
                n.rx_rate = rx.saturating_sub(n.prev.0) as f64 / elapsed;
                n.tx_rate = tx.saturating_sub(n.prev.1) as f64 / elapsed;
            }
            n.prev = (rx, tx);
            n.rx_total = rx;
            n.tx_total = tx;
            n.rx_hist.push(n.rx_rate as f32);
            n.tx_hist.push(n.tx_rate as f32);
        }
    }

    fn sample_misc(&mut self) {
        if self.sys.loadavg.read(&mut self.buf).is_ok() {
            let s = std::str::from_utf8(&self.buf).unwrap_or("");
            for (slot, f) in self.snap.load.iter_mut().zip(s.split_whitespace()) {
                *slot = f.parse().unwrap_or(0.0);
            }
        }
        if self.sys.uptime.read(&mut self.buf).is_ok() {
            let s = std::str::from_utf8(&self.buf).unwrap_or("");
            self.snap.uptime = s.split_whitespace().next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
        }
    }

    fn sample_procs(&mut self, elapsed: f64, total_delta: u64) -> Vec<ProcInfo> {
        let mut out = Vec::with_capacity(self.procs.len() + 16);
        let Ok(dir) = std::fs::read_dir("/proc") else { return out };
        let current = self.generation;
        let ncpu = self.snap.cpu.logical.max(1) as f64;
        // If /proc/stat failed, fall back to wall-clock based normalization.
        let jiffies = if total_delta > 0 { total_delta as f64 } else { elapsed * self.clk_tck * ncpu };

        // Keep up to 3 fds per process open, as long as that stays well inside RLIMIT_NOFILE.
        let cache = self.fd_limit > self.procs.len() as u64 * 3 + 2048;
        if self.cache_fds && !cache {
            for e in self.procs.values_mut() {
                (e.stat_fd, e.statm_fd, e.io_fd) = (None, None, None);
            }
        }
        self.cache_fds = cache;

        for e in dir.flatten() {
            let fname = e.file_name();
            let Some(pid) = parse_pid(fname.as_encoded_bytes()) else { continue };

            // Fast path: re-read through the cached fd. If that fails the process exited
            // (and the pid may have been reused), so forget it and start over by path.
            let mut have = false;
            if let Some(f) = self.procs.get(&pid).and_then(|e| e.stat_fd.as_ref()) {
                have = pread_record(f, &mut self.buf).is_ok();
                if !have {
                    self.procs.remove(&pid);
                }
            }
            let mut new_fd = None;
            if !have {
                self.path.clear();
                let _ = write!(self.path, "/proc/{pid}/stat");
                if cache {
                    let Ok(f) = File::open(&self.path) else { continue };
                    if pread_record(&f, &mut self.buf).is_err() {
                        continue;
                    }
                    new_fd = Some(f);
                } else if read_record(&self.path, &mut self.buf).is_err() {
                    continue; // exited between readdir and open
                }
            }
            let Some(st) = parse_stat(&self.buf) else { continue };
            let kernel = st.ppid == 2 || pid == 2;

            // 0 = known and unchanged, 1 = same process exec()'d into a new program, 2 = new.
            let status = match self.procs.get(&pid) {
                Some(e) if e.start == st.start => u8::from(*e.comm != self.buf[st.comm.clone()]),
                _ => 2,
            };
            if status != 0 {
                let comm = String::from_utf8_lossy(&self.buf[st.comm.clone()]).into_owned();
                let mut fresh = self.new_entry(pid, comm, st.start, kernel);
                if status == 1
                    && let Some(old) = self.procs.remove(&pid)
                {
                    // Keep the counters so rates stay continuous; io access is re-probed
                    // because the owner may have changed (setuid programs).
                    fresh.cpu = old.cpu;
                    fresh.rd = old.rd;
                    fresh.wr = old.wr;
                    fresh.seen = old.seen;
                    fresh.age = old.age;
                    fresh.stat_fd = old.stat_fd;
                    fresh.statm_fd = old.statm_fd;
                }
                self.procs.insert(pid, fresh);
            }
            let entry = self.procs.get_mut(&pid).unwrap();
            if new_fd.is_some() {
                entry.stat_fd = new_fd;
            }
            let fresh = entry.seen == 0;
            entry.seen = current;
            entry.age = entry.age.saturating_add(1);
            if !kernel {
                // Desktops may move a freshly launched app into its scope a moment later.
                if entry.age == 3 {
                    entry.unit = read_unit(pid, &mut self.path, &mut self.buf);
                }
                // AMD/Intel per-process GPU: find DRM fds shortly after start, then round-robin.
                if self.gpu.wants_drm_scan() && (entry.age == 2 || (pid as u64 + current) % 30 == 0) {
                    self.gpu.scan_drm_fds(pid);
                }
            }

            let cpu_ticks = st.utime + st.stime;
            let cpu = if fresh {
                0.0
            } else {
                (100.0 * cpu_ticks.saturating_sub(entry.cpu) as f64 / jiffies).min(100.0) as f32
            };
            entry.cpu = cpu_ticks;

            let (mut read_rate, mut write_rate) = (0.0, 0.0);
            let (mut mem, mut rss) = (0, st.rss_pages * self.page_size);
            if !kernel {
                if read_cached(&mut entry.statm_fd, cache, &mut self.path, pid, "statm", &mut self.buf).is_ok() {
                    let mut f = fields(&self.buf).skip(1).map(|x| parse_u64(x).unwrap_or(0));
                    let resident = f.next().unwrap_or(0);
                    let shared = f.next().unwrap_or(0);
                    rss = resident * self.page_size;
                    mem = resident.saturating_sub(shared) * self.page_size;
                }
                if entry.io_ok {
                    match read_cached(&mut entry.io_fd, cache, &mut self.path, pid, "io", &mut self.buf) {
                        Ok(()) => {
                            let rd = kv_u64(&self.buf, b"read_bytes:").unwrap_or(0);
                            let wr = kv_u64(&self.buf, b"write_bytes:").unwrap_or(0);
                            if !fresh {
                                read_rate = rd.saturating_sub(entry.rd) as f64 / elapsed;
                                write_rate = wr.saturating_sub(entry.wr) as f64 / elapsed;
                            }
                            entry.rd = rd;
                            entry.wr = wr;
                        }
                        Err(_) => {
                            entry.io_ok = false;
                            entry.io_fd = None;
                        }
                    }
                }
            }

            out.push(ProcInfo {
                pid,
                ppid: st.ppid,
                name: entry.name.clone(),
                cmd: entry.cmd.clone(),
                user: entry.user.clone(),
                uid: entry.uid,
                state: st.state,
                cpu,
                mem,
                rss,
                threads: st.threads,
                nice: st.nice,
                read_rate,
                write_rate,
                start: st.start as f64 / self.clk_tck,
                kernel,
                gpu: self.gpu.proc_util.get(&pid).copied().unwrap_or(0.0),
                unit: entry.unit.clone(),
            });
        }
        let gpu = &mut self.gpu;
        self.procs.retain(|&pid, e| {
            let keep = e.seen == current;
            if !keep {
                gpu.forget_pid(pid);
            }
            keep
        });
        out
    }

    fn new_entry(&mut self, pid: u32, comm: String, start: u64, kernel: bool) -> ProcEntry {
        let uid = std::fs::metadata(format!("/proc/{pid}")).map(|m| m.uid()).unwrap_or(0);
        let user = self.users.entry(uid).or_insert_with(|| Arc::from(uid.to_string())).clone();
        let mut cmd = String::new();
        if !kernel {
            self.path.clear();
            let _ = write!(self.path, "/proc/{pid}/cmdline");
            if read_into(&self.path, &mut self.buf).is_ok() {
                let raw = trim_nul(&self.buf);
                cmd = String::from_utf8_lossy(raw).replace('\0', " ");
            }
        }
        let comm_raw: Box<[u8]> = comm.as_bytes().into();
        let name = display_name(&comm, &cmd);
        let unit = if kernel { None } else { read_unit(pid, &mut self.path, &mut self.buf) };
        ProcEntry {
            start,
            cpu: 0,
            rd: 0,
            wr: 0,
            name: Arc::from(name),
            cmd: if cmd.is_empty() { self.empty.clone() } else { Arc::from(cmd) },
            user,
            uid,
            comm: comm_raw,
            io_ok: !kernel,
            seen: 0,
            age: 0,
            unit,
            stat_fd: None,
            statm_fd: None,
            io_fd: None,
        }
    }
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

/// The systemd unit a process runs in (app scope or service), from its cgroup.
fn read_unit(pid: u32, path: &mut String, buf: &mut Vec<u8>) -> Option<Arc<str>> {
    path.clear();
    let _ = write!(path, "/proc/{pid}/cgroup");
    read_into(path, buf).ok()?;
    unit_from_cgroup(std::str::from_utf8(buf).ok()?).map(Arc::from)
}

pub fn unit_from_cgroup(text: &str) -> Option<&str> {
    // cgroup v2: "0::/user.slice/.../app.slice/app-foo-123.scope"; hybrid v1 has a name=systemd line.
    // Services may nest sub-cgroups (e.g. docker.service/foo), so take the innermost unit-like component.
    let path = text.lines().find(|l| l.starts_with("0::") || l.contains(":name=systemd:"))?.rsplit_once(':')?.1;
    path.rsplit('/').find(|c| {
        (c.starts_with("app-") && c.ends_with(".scope"))
            || (c.ends_with(".service") && !c.starts_with("user@") && !c.starts_with("user-runtime-dir@"))
    })
}

/// Count CPUs in a kernel CPU list like "0-11,16,18-23".
fn count_cpu_list(s: &str) -> Option<usize> {
    let mut n = 0;
    for part in s.split(',').filter(|p| !p.is_empty()) {
        n += match part.split_once('-') {
            Some((a, b)) => b.parse::<usize>().ok()?.checked_sub(a.parse::<usize>().ok()?)? + 1,
            None => {
                part.parse::<usize>().ok()?;
                1
            }
        };
    }
    (n > 0).then_some(n)
}

/// Read `/proc/<pid>/<name>` through `slot`'s cached fd, opening (and caching) it if needed.
fn read_cached(
    slot: &mut Option<File>,
    cache: bool,
    path: &mut String,
    pid: u32,
    name: &str,
    buf: &mut Vec<u8>,
) -> std::io::Result<()> {
    if let Some(f) = slot {
        return pread_record(f, buf);
    }
    path.clear();
    let _ = write!(path, "/proc/{pid}/{name}");
    if !cache {
        return read_record(path, buf);
    }
    let f = File::open(path.as_str())?;
    pread_record(&f, buf)?;
    *slot = Some(f);
    Ok(())
}

/// Raise the soft fd limit to the hard limit; returns the resulting soft limit.
fn raise_fd_limit() -> u64 {
    // SAFETY: getrlimit/setrlimit with a valid, initialised struct.
    unsafe {
        let mut r = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) != 0 {
            return 1024;
        }
        if r.rlim_cur < r.rlim_max {
            let want = libc::rlimit { rlim_cur: r.rlim_max, rlim_max: r.rlim_max };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &want) == 0 {
                r.rlim_cur = r.rlim_max;
            }
        }
        #[allow(clippy::unnecessary_cast)] // rlim_t is not u64 on every target
        let cur = r.rlim_cur as u64;
        cur
    }
}

/// `comm` is truncated to 15 bytes; prefer the full argv[0] basename when it extends comm.
fn display_name(comm: &str, cmd: &str) -> String {
    if comm.len() >= 15 {
        let argv0 = cmd.split(' ').next().unwrap_or("");
        let base = argv0.rsplit('/').next().unwrap_or("");
        if base.starts_with(comm) {
            return base.to_string();
        }
    }
    comm.to_string()
}

fn trim_nul(b: &[u8]) -> &[u8] {
    let end = b.iter().rposition(|&c| c != 0).map_or(0, |e| e + 1);
    &b[..end]
}

#[inline]
fn parse_pid(b: &[u8]) -> Option<u32> {
    if b.is_empty() || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    parse_u64(b).map(|v| v as u32)
}

pub struct Stat {
    /// Byte range of `comm` within the parsed buffer.
    pub comm: std::ops::Range<usize>,
    pub state: u8,
    pub ppid: u32,
    pub utime: u64,
    pub stime: u64,
    pub nice: i32,
    pub threads: u32,
    pub start: u64,
    pub rss_pages: u64,
}

/// Parse `/proc/<pid>/stat`. `comm` may contain spaces and parentheses, so split on the last ')'.
pub fn parse_stat(b: &[u8]) -> Option<Stat> {
    let open = b.iter().position(|&c| c == b'(')?;
    let close = b.iter().rposition(|&c| c == b')')?;
    let comm = open + 1..close.max(open + 1);
    let mut f = fields(b.get(close + 1..)?);
    let mut nth = |n: usize| f.nth(n);
    // Field indices relative to the field after comm (state = 0).
    let state = *nth(0)?.first()?;
    let ppid = parse_u64(nth(0)?)? as u32; // 1
    let utime = parse_u64(nth(9)?)?; // 11
    let stime = parse_u64(nth(0)?)?; // 12
    let nice = parse_i64(nth(3)?)? as i32; // 16
    let threads = parse_u64(nth(0)?)? as u32; // 17
    let start = parse_u64(nth(1)?)?; // 19
    let rss_pages = parse_u64(nth(1)?)?; // 21
    Some(Stat { comm, state, ppid, utime, stime, nice, threads, start, rss_pages })
}

fn load_users() -> HashMap<u32, Arc<str>> {
    let mut m = HashMap::new();
    if let Ok(s) = std::fs::read_to_string("/etc/passwd") {
        for line in s.lines() {
            let mut it = line.split(':');
            if let (Some(name), _, Some(uid)) = (it.next(), it.next(), it.next())
                && let Ok(uid) = uid.parse()
            {
                m.insert(uid, Arc::from(name));
            }
        }
    }
    m
}

fn find_cpu_temp() -> Option<String> {
    let rd = std::fs::read_dir("/sys/class/hwmon").ok()?;
    for e in rd.flatten() {
        let base = e.path();
        let name = std::fs::read_to_string(base.join("name")).unwrap_or_default();
        if matches!(name.trim(), "k10temp" | "coretemp" | "zenpower" | "cpu_thermal" | "k8temp") {
            let p = base.join("temp1_input");
            if p.exists() {
                return Some(p.to_string_lossy().into_owned());
            }
        }
    }
    None
}

/// State shared between the sampler thread and the UI.
pub struct Shared {
    latest: Mutex<Arc<Snapshot>>,
    pub interval_ms: AtomicU64,
    pub paused: AtomicBool,
    thread: OnceLock<Thread>,
}

impl Shared {
    pub fn latest(&self) -> Arc<Snapshot> {
        self.latest.lock().unwrap().clone()
    }
    /// Wake the sampler (e.g. after the interval changed or "refresh now").
    pub fn poke(&self) {
        if let Some(t) = self.thread.get() {
            t.unpark();
        }
    }
}

/// Spawn the sampler thread. `on_sample` is called after each publish (used to request a repaint).
pub fn spawn(interval_ms: u64, on_sample: impl Fn() + Send + 'static) -> Arc<Shared> {
    let mut sampler = Sampler::new();
    // Let the first published sample cover a real interval; a few-ms window quantises
    // per-core usage to 0/100% and makes rates spike (which then sets graph scales).
    thread::sleep(Duration::from_millis(250));
    let first = sampler.sample();
    let shared = Arc::new(Shared {
        latest: Mutex::new(first),
        interval_ms: AtomicU64::new(interval_ms),
        paused: AtomicBool::new(false),
        thread: OnceLock::new(),
    });
    let s = shared.clone();
    let handle = thread::Builder::new()
        .name("taskd-sampler".into())
        .spawn(move || {
            let mut next = Instant::now();
            loop {
                let interval = Duration::from_millis(s.interval_ms.load(Ordering::Relaxed).max(100));
                next += interval;
                let now = Instant::now();
                if next > now {
                    thread::park_timeout(next - now);
                }
                // Woken early (poke) or late (suspend): resync the schedule.
                let now = Instant::now();
                if now + Duration::from_millis(5) < next || now > next + interval {
                    next = now;
                }
                if s.paused.load(Ordering::Relaxed) {
                    continue;
                }
                let snap = sampler.sample();
                *s.latest.lock().unwrap() = snap;
                on_sample();
            }
        })
        .expect("spawn sampler thread");
    let _ = shared.thread.set(handle.thread().clone());
    shared
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_with_weird_comm() {
        let line = b"1234 (my (weird) proc) S 1 1234 1234 0 -1 4194560 100 0 0 0 250 50 0 0 20 -5 7 0 98765 1000000 256 18446744073709551615";
        let s = parse_stat(line).unwrap();
        assert_eq!(&line[s.comm.clone()], b"my (weird) proc");
        assert_eq!(s.state, b'S');
        assert_eq!(s.ppid, 1);
        assert_eq!(s.utime, 250);
        assert_eq!(s.stime, 50);
        assert_eq!(s.nice, -5);
        assert_eq!(s.threads, 7);
        assert_eq!(s.start, 98765);
        assert_eq!(s.rss_pages, 256);
    }

    #[test]
    fn cpu_lists() {
        assert_eq!(count_cpu_list("0-23"), Some(24));
        assert_eq!(count_cpu_list("0-11,16,18-23"), Some(19));
        assert_eq!(count_cpu_list("0"), Some(1));
        assert_eq!(count_cpu_list(""), None);
    }

    #[test]
    fn exec_refreshes_identity() {
        let mut s = Sampler::new();
        // A shell that sleeps briefly, then exec()s into a different program (same pid/start).
        let mut child = std::process::Command::new("sh").args(["-c", "sleep 0.3; exec sleep 5"]).spawn().unwrap();
        let pid = child.id();
        s.sample();
        std::thread::sleep(std::time::Duration::from_millis(700));
        let snap = s.sample();
        let p = snap.procs.iter().find(|p| p.pid == pid).expect("child visible");
        assert_eq!(&*p.name, "sleep", "name refreshed after exec");
        assert!(p.cmd.starts_with("sleep 5"), "{}", p.cmd);
        child.kill().ok();
        child.wait().ok();
    }

    #[test]
    fn units() {
        let vscode =
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-com.microsoft.VSCode-3809464.scope\n";
        assert_eq!(unit_from_cgroup(vscode), Some("app-com.microsoft.VSCode-3809464.scope"));
        assert_eq!(unit_from_cgroup("0::/user.slice/user-1000.slice/session-c1.scope\n"), None);
        assert_eq!(unit_from_cgroup("0::/system.slice/ssh.service\n"), Some("ssh.service"));
        assert_eq!(unit_from_cgroup("0::/user.slice/user-1000.slice/user@1000.service/init.scope\n"), None);
        assert_eq!(
            unit_from_cgroup("0::/user.slice/user-1000.slice/user@1000.service/session.slice/pipewire.service\n"),
            Some("pipewire.service")
        );
    }

    #[test]
    fn names() {
        assert_eq!(display_name("bash", "/bin/bash -l"), "bash");
        assert_eq!(display_name("gnome-terminal-", "/usr/libexec/gnome-terminal-server"), "gnome-terminal-server");
        assert_eq!(parse_pid(b"123"), Some(123));
        assert_eq!(parse_pid(b"self"), None);
    }

    #[test]
    fn cached_fds_track_process_lifetime() {
        let mut s = Sampler::new();
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let snap = s.sample();
        assert!(snap.procs.iter().any(|p| p.pid == pid && &*p.name == "sleep"));
        if s.cache_fds {
            assert!(s.procs[&pid].stat_fd.is_some());
        }
        child.kill().unwrap();
        child.wait().unwrap();
        let snap = s.sample();
        assert!(!snap.procs.iter().any(|p| p.pid == pid));
        assert!(!s.procs.contains_key(&pid));
    }

    #[test]
    fn samples_live_system() {
        let mut s = Sampler::new();
        let snap = s.sample();
        assert!(!snap.procs.is_empty());
        assert!(snap.mem.total > 0);
        assert!(snap.cpu.logical >= 1);
        assert!(snap.procs.iter().any(|p| p.pid == std::process::id()));
    }
}
