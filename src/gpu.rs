//! GPU telemetry. NVIDIA via NVML (loaded at runtime with dlopen, so there is no
//! link-time dependency), AMD (and anything else exposing `gpu_busy_percent`) via sysfs.
//!
//! Per-process usage: NVML's process utilization samples on NVIDIA; DRM `fdinfo`
//! engine counters (`drm-engine-*`) on amdgpu/i915, scanned only when such a GPU exists.

use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_int, c_uint, c_ulonglong, c_void};
use std::fmt::Write as _;

use crate::sampler::History;
use crate::util::CachedFile;

#[derive(Clone, Default)]
pub struct GpuInfo {
    pub name: String,
    pub vendor: &'static str,
    pub util: Option<f32>,
    pub mem_util: Option<f32>,
    pub mem_used: Option<u64>,
    pub mem_total: Option<u64>,
    pub temp_c: Option<f32>,
    pub power_w: Option<f32>,
    pub hist: History,
}

type NvmlDevice = *mut c_void;

#[repr(C)]
struct NvmlUtilization {
    gpu: c_uint,
    memory: c_uint,
}

#[repr(C)]
struct NvmlMemory {
    total: u64,
    free: u64,
    used: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NvmlProcessSample {
    pid: c_uint,
    timestamp: c_ulonglong,
    sm_util: c_uint,
    mem_util: c_uint,
    enc_util: c_uint,
    dec_util: c_uint,
}

const NVML_ERROR_INSUFFICIENT_SIZE: c_int = 7;
const NVML_PROC_EVERY: std::time::Duration = std::time::Duration::from_millis(1900);

type ProcUtilFn = unsafe extern "C" fn(NvmlDevice, *mut NvmlProcessSample, *mut c_uint, c_ulonglong) -> c_int;

struct Nvml {
    _lib: *mut c_void,
    /// Optional: older drivers may lack it.
    get_proc_util: Option<ProcUtilFn>,
    last_seen: Vec<c_ulonglong>,
    samples: Vec<NvmlProcessSample>,
    last_query: Option<std::time::Instant>,
    /// Per-process utilization from the last query, reused until the next one.
    cached: HashMap<u32, f32>,
    get_util: unsafe extern "C" fn(NvmlDevice, *mut NvmlUtilization) -> c_int,
    get_mem: unsafe extern "C" fn(NvmlDevice, *mut NvmlMemory) -> c_int,
    get_temp: unsafe extern "C" fn(NvmlDevice, c_int, *mut c_uint) -> c_int,
    get_power: unsafe extern "C" fn(NvmlDevice, *mut c_uint) -> c_int,
    devices: Vec<NvmlDevice>,
}

// SAFETY: NVML is documented as thread-safe; we only use it from the sampler thread anyway.
unsafe impl Send for Nvml {}

unsafe fn sym<T: Copy>(lib: *mut c_void, name: &CStr) -> Option<T> {
    // SAFETY: caller guarantees T is the correct fn-pointer type for `name`.
    unsafe {
        let p = libc::dlsym(lib, name.as_ptr());
        (!p.is_null()).then(|| std::mem::transmute_copy::<*mut c_void, T>(&p))
    }
}

impl Nvml {
    fn load(names: &mut Vec<String>) -> Option<Self> {
        // SAFETY: plain dlopen/dlsym of a C library with the documented NVML ABI.
        unsafe {
            let lib = libc::dlopen(c"libnvidia-ml.so.1".as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
            if lib.is_null() {
                return None;
            }
            let init: unsafe extern "C" fn() -> c_int = sym(lib, c"nvmlInit_v2")?;
            if init() != 0 {
                libc::dlclose(lib);
                return None;
            }
            let count_fn: unsafe extern "C" fn(*mut c_uint) -> c_int = sym(lib, c"nvmlDeviceGetCount_v2")?;
            let handle_fn: unsafe extern "C" fn(c_uint, *mut NvmlDevice) -> c_int =
                sym(lib, c"nvmlDeviceGetHandleByIndex_v2")?;
            let name_fn: unsafe extern "C" fn(NvmlDevice, *mut c_char, c_uint) -> c_int =
                sym(lib, c"nvmlDeviceGetName")?;
            let mut count = 0;
            if count_fn(&mut count) != 0 {
                return None;
            }
            let mut devices = Vec::new();
            for i in 0..count {
                let mut d: NvmlDevice = std::ptr::null_mut();
                if handle_fn(i, &mut d) == 0 {
                    let mut buf = [0 as c_char; 96];
                    let name = if name_fn(d, buf.as_mut_ptr(), buf.len() as c_uint) == 0 {
                        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
                    } else {
                        format!("NVIDIA GPU {i}")
                    };
                    names.push(name);
                    devices.push(d);
                }
            }
            Some(Nvml {
                _lib: lib,
                get_proc_util: sym(lib, c"nvmlDeviceGetProcessUtilization"),
                last_seen: vec![0; devices.len()],
                samples: Vec::new(),
                last_query: None,
                cached: HashMap::new(),
                get_util: sym(lib, c"nvmlDeviceGetUtilizationRates")?,
                get_mem: sym(lib, c"nvmlDeviceGetMemoryInfo")?,
                get_temp: sym(lib, c"nvmlDeviceGetTemperature")?,
                get_power: sym(lib, c"nvmlDeviceGetPowerUsage")?,
                devices,
            })
        }
    }
}

struct SysfsGpu {
    busy: CachedFile,
    vram_used: CachedFile,
    vram_total: CachedFile,
    temp: Option<CachedFile>,
    power: Option<CachedFile>,
}

pub struct GpuReader {
    nvml: Option<Nvml>,
    sysfs: Vec<SysfsGpu>,
    state: Vec<GpuInfo>,
    buf: Vec<u8>,
    /// Per-process GPU utilization (percent, summed over GPUs, capped at 100) from the last read.
    pub proc_util: HashMap<u32, f32>,
    /// pid -> fdinfo paths of its open DRM fds (only maintained when a sysfs GPU exists).
    drm_fds: HashMap<u32, Vec<String>>,
    /// (pid, drm-client-id, engine) -> busy ns at the previous read.
    drm_prev: HashMap<(u32, u64, String), u64>,
}

impl GpuReader {
    pub fn new() -> Self {
        let mut names = Vec::new();
        let nvml = Nvml::load(&mut names);
        let mut state: Vec<GpuInfo> =
            names.into_iter().map(|name| GpuInfo { name, vendor: "NVIDIA", ..Default::default() }).collect();

        let mut sysfs = Vec::new();
        if let Ok(rd) = std::fs::read_dir("/sys/class/drm") {
            let mut cards: Vec<_> = rd
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("card") && !n.contains('-'))
                .collect();
            cards.sort();
            for card in cards {
                let dev = format!("/sys/class/drm/{card}/device");
                let busy = format!("{dev}/gpu_busy_percent");
                if !std::path::Path::new(&busy).exists() {
                    continue;
                }
                let vendor = std::fs::read_to_string(format!("{dev}/vendor")).unwrap_or_default();
                let vendor = match vendor.trim() {
                    "0x1002" => "AMD",
                    "0x8086" => "Intel",
                    _ => "GPU",
                };
                let (mut temp, mut power) = (None, None);
                if let Ok(hw) = std::fs::read_dir(format!("{dev}/hwmon")) {
                    for h in hw.flatten() {
                        let p = h.path();
                        if p.join("temp1_input").exists() {
                            temp = Some(p.join("temp1_input").to_string_lossy().into_owned());
                        }
                        for f in ["power1_average", "power1_input"] {
                            if power.is_none() && p.join(f).exists() {
                                power = Some(p.join(f).to_string_lossy().into_owned());
                            }
                        }
                    }
                }
                let name = crate::util::read_string(&format!("{dev}/product_name"))
                    .unwrap_or_else(|| format!("{vendor} GPU ({card})"));
                state.push(GpuInfo { name, vendor, ..Default::default() });
                sysfs.push(SysfsGpu {
                    busy: CachedFile::new(busy, true),
                    vram_used: CachedFile::new(format!("{dev}/mem_info_vram_used"), true),
                    vram_total: CachedFile::new(format!("{dev}/mem_info_vram_total"), true),
                    temp: temp.map(|p| CachedFile::new(p, true)),
                    power: power.map(|p| CachedFile::new(p, true)),
                });
            }
        }
        GpuReader {
            nvml,
            sysfs,
            state,
            buf: Vec::with_capacity(64),
            proc_util: HashMap::new(),
            drm_fds: HashMap::new(),
            drm_prev: HashMap::new(),
        }
    }

    /// Whether the sampler should look for DRM fds (AMD/Intel per-process usage).
    pub fn wants_drm_scan(&self) -> bool {
        !self.sysfs.is_empty()
    }

    /// Record which of `pid`'s fds are DRM devices. Only readable for our own processes (or root).
    pub fn scan_drm_fds(&mut self, pid: u32) {
        let mut paths = Vec::new();
        if let Ok(rd) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
            for e in rd.flatten() {
                if let Ok(target) = std::fs::read_link(e.path())
                    && target.as_os_str().as_encoded_bytes().starts_with(b"/dev/dri/")
                {
                    let mut p = String::new();
                    let _ = write!(p, "/proc/{pid}/fdinfo/{}", e.file_name().to_string_lossy());
                    paths.push(p);
                }
            }
        }
        if paths.is_empty() {
            self.drm_fds.remove(&pid);
        } else {
            self.drm_fds.insert(pid, paths);
        }
    }

    pub fn forget_pid(&mut self, pid: u32) {
        self.drm_fds.remove(&pid);
    }

    /// NVML's per-process query costs 1-3 ms, so it runs at most every `NVML_PROC_EVERY`
    /// and the values are reused in between. NVML buffers samples, so each query still
    /// averages everything since the previous one.
    fn read_nvml_procs(&mut self) {
        let Some(n) = &mut self.nvml else { return };
        let Some(f) = n.get_proc_util else { return };
        if n.last_query.is_some_and(|t| t.elapsed() < NVML_PROC_EVERY) {
            for (&pid, &v) in &n.cached {
                self.proc_util.insert(pid, v);
            }
            return;
        }
        n.last_query = Some(std::time::Instant::now());
        n.cached.clear();
        for (di, &d) in n.devices.iter().enumerate() {
            // One call into a preallocated buffer; grow and retry only if NVML says it's too small.
            if n.samples.len() < 256 {
                n.samples.resize(256, NvmlProcessSample::default());
            }
            let mut count = n.samples.len() as c_uint;
            // SAFETY: the buffer holds `count` samples, as NVML requires.
            let mut r = unsafe { f(d, n.samples.as_mut_ptr(), &mut count, n.last_seen[di]) };
            if r == NVML_ERROR_INSUFFICIENT_SIZE {
                n.samples.resize(count as usize + 64, NvmlProcessSample::default());
                count = n.samples.len() as c_uint;
                // SAFETY: as above, with the larger buffer.
                r = unsafe { f(d, n.samples.as_mut_ptr(), &mut count, n.last_seen[di]) };
            }
            if r != 0 {
                continue; // includes NVML_ERROR_NOT_FOUND: no samples since last time
            }
            // Average each process's samples over the window since the last read.
            let mut acc: HashMap<u32, (u32, u32)> = HashMap::new();
            for s in &n.samples[..count as usize] {
                n.last_seen[di] = n.last_seen[di].max(s.timestamp);
                let e = acc.entry(s.pid).or_default();
                e.0 += s.sm_util.min(100);
                e.1 += 1;
            }
            for (pid, (sum, k)) in acc {
                let v = n.cached.entry(pid).or_default();
                *v = (*v + sum as f32 / k as f32).min(100.0);
            }
        }
        for (&pid, &v) in &n.cached {
            self.proc_util.insert(pid, v);
        }
    }

    fn read_drm_procs(&mut self, elapsed: f64) {
        if self.drm_fds.is_empty() {
            self.drm_prev.clear();
            return;
        }
        let mut next = HashMap::with_capacity(self.drm_prev.len());
        let mut dead = Vec::new();
        for (&pid, paths) in &self.drm_fds {
            let mut seen_clients = Vec::new();
            let mut best: f64 = 0.0;
            let mut alive = false;
            for p in paths {
                if crate::util::read_into(p, &mut self.buf).is_err() {
                    continue;
                }
                alive = true;
                let Some((client, engines)) = parse_fdinfo(&self.buf) else { continue };
                if seen_clients.contains(&client) {
                    continue; // several fds can share one DRM client
                }
                seen_clients.push(client);
                for (engine, ns) in engines {
                    let key = (pid, client, engine);
                    if let Some(&prev) = self.drm_prev.get(&key) {
                        let pct = ns.saturating_sub(prev) as f64 / (elapsed * 1e9) * 100.0;
                        best = best.max(pct);
                    }
                    next.insert(key, ns);
                }
            }
            if !alive {
                dead.push(pid);
            } else if best > 0.0 {
                let v = self.proc_util.entry(pid).or_default();
                *v = (*v + best as f32).min(100.0);
            }
        }
        for pid in dead {
            self.drm_fds.remove(&pid);
        }
        self.drm_prev = next;
    }

    pub fn read(&mut self, elapsed: f64) -> Vec<GpuInfo> {
        self.proc_util.clear();
        self.read_nvml_procs();
        self.read_drm_procs(elapsed);
        let mut i = 0;
        if let Some(n) = &self.nvml {
            for &d in &n.devices {
                let g = &mut self.state[i];
                // SAFETY: device handles come from NVML and stay valid while it is initialised.
                unsafe {
                    let mut u = NvmlUtilization { gpu: 0, memory: 0 };
                    if (n.get_util)(d, &mut u) == 0 {
                        g.util = Some(u.gpu as f32);
                        g.mem_util = Some(u.memory as f32);
                    }
                    let mut m = NvmlMemory { total: 0, free: 0, used: 0 };
                    if (n.get_mem)(d, &mut m) == 0 {
                        g.mem_used = Some(m.used);
                        g.mem_total = Some(m.total);
                    }
                    let mut t = 0;
                    if (n.get_temp)(d, 0, &mut t) == 0 {
                        g.temp_c = Some(t as f32);
                    }
                    let mut p = 0;
                    if (n.get_power)(d, &mut p) == 0 {
                        g.power_w = Some(p as f32 / 1000.0);
                    }
                }
                g.hist.push(g.util.unwrap_or(0.0));
                i += 1;
            }
        }
        for s in &mut self.sysfs {
            let g = &mut self.state[i];
            g.util = s.busy.read_u64(&mut self.buf).map(|v| v as f32);
            g.mem_used = s.vram_used.read_u64(&mut self.buf);
            g.mem_total = s.vram_total.read_u64(&mut self.buf);
            g.temp_c = s.temp.as_mut().and_then(|f| f.read_u64(&mut self.buf)).map(|v| v as f32 / 1000.0);
            g.power_w = s.power.as_mut().and_then(|f| f.read_u64(&mut self.buf)).map(|v| v as f32 / 1e6);
            g.hist.push(g.util.unwrap_or(0.0));
            i += 1;
        }
        self.state.clone()
    }
}

/// Parse a DRM fdinfo file: `drm-client-id` plus busy-time counters of the engines
/// that represent "GPU usage" (graphics/compute/render/video), in nanoseconds.
pub fn parse_fdinfo(b: &[u8]) -> Option<(u64, Vec<(String, u64)>)> {
    let text = std::str::from_utf8(b).ok()?;
    let mut client = None;
    let mut engines = Vec::new();
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        if k == "drm-client-id" {
            client = v.parse().ok();
        } else if let Some(engine) = k.strip_prefix("drm-engine-")
            && engine != "capacity"
            && let Some(ns) = v.strip_suffix(" ns").and_then(|n| n.trim().parse().ok())
        {
            engines.push((engine.to_string(), ns));
        }
    }
    Some((client?, engines))
}

impl Default for GpuReader {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_amdgpu_fdinfo() {
        let s = b"pos:\t0\nflags:\t02100002\ndrm-driver:\tamdgpu\ndrm-client-id:\t42\n\
drm-memory-vram:\t1024 KiB\ndrm-engine-gfx:\t123456789 ns\ndrm-engine-compute:\t5 ns\n\
drm-engine-capacity-gfx:\t2\n";
        let (client, engines) = parse_fdinfo(s).unwrap();
        assert_eq!(client, 42);
        assert_eq!(engines, vec![("gfx".to_string(), 123456789), ("compute".to_string(), 5)]);
        assert!(parse_fdinfo(b"pos:\t0\n").is_none());
    }
}
