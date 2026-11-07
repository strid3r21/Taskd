//! GPU telemetry. NVIDIA via NVML (loaded at runtime with dlopen, so there is no
//! link-time dependency), AMD (and anything else exposing `gpu_busy_percent`) via sysfs.

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};

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

struct Nvml {
    _lib: *mut c_void,
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
        GpuReader { nvml, sysfs, state, buf: Vec::with_capacity(64) }
    }

    pub fn read(&mut self) -> Vec<GpuInfo> {
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

impl Default for GpuReader {
    fn default() -> Self {
        Self::new()
    }
}
