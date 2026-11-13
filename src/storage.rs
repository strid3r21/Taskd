//! Storage space: mounted filesystems (like `df`) and a folder-size scanner ("what's filling
//! this disk?").

use std::collections::HashMap;
use std::ffi::CString;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Clone, Debug)]
pub struct Fs {
    pub mount: String,
    pub device: String,
    pub fstype: String,
    pub total: u64,
    pub used: u64,
    pub avail: u64,
}

impl Fs {
    pub fn used_pct(&self) -> f32 {
        if self.total == 0 { 0.0 } else { 100.0 * self.used as f32 / (self.used + self.avail).max(1) as f32 }
    }
}

/// Pseudo and virtual filesystems that aren't "storage".
const SKIP: &[&str] = &[
    "proc",
    "sysfs",
    "devtmpfs",
    "devpts",
    "tmpfs",
    "cgroup",
    "cgroup2",
    "securityfs",
    "pstore",
    "bpf",
    "debugfs",
    "tracefs",
    "configfs",
    "fusectl",
    "mqueue",
    "hugetlbfs",
    "autofs",
    "binfmt_misc",
    "efivarfs",
    "ramfs",
    "nsfs",
    "rpc_pipefs",
    "squashfs",
    "overlay",
    "fuse.portal",
    "fuse.gvfsd-fuse",
    "fuse.snapfuse",
    "selinuxfs",
    "zram",
];

/// `/proc/self/mounts` escapes spaces etc. as octal (`\040`).
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Mounted filesystems with their space, one entry per device (bind mounts and btrfs
/// subvolumes of the same device show once, at the shortest mount point).
pub fn filesystems() -> Vec<Fs> {
    let text = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    let mut by_dev: HashMap<String, Fs> = HashMap::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(dev), Some(mnt), Some(fstype)) = (it.next(), it.next(), it.next()) else { continue };
        if SKIP.contains(&fstype) || mnt.starts_with("/snap/") || mnt.starts_with("/proc") || mnt.starts_with("/sys") {
            continue;
        }
        let mount = unescape(mnt);
        let Ok(c) = CString::new(mount.as_bytes()) else { continue };
        // SAFETY: statvfs fills the zeroed struct for a valid NUL-terminated path.
        let st = unsafe {
            let mut st: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(c.as_ptr(), &mut st) != 0 {
                continue;
            }
            st
        };
        let bs = st.f_frsize as u64;
        let total = st.f_blocks as u64 * bs;
        if total == 0 {
            continue;
        }
        let avail = st.f_bavail as u64 * bs;
        let used = total.saturating_sub(st.f_bfree as u64 * bs);
        let fs = Fs { mount, device: unescape(dev), fstype: fstype.to_string(), total, used, avail };
        let key = if dev.starts_with('/') { dev.to_string() } else { format!("{dev}@{}", fs.mount) };
        match by_dev.get(&key) {
            Some(old) if old.mount.len() <= fs.mount.len() => {}
            _ => {
                by_dev.insert(key, fs);
            }
        }
    }
    let mut v: Vec<Fs> = by_dev.into_values().collect();
    v.sort_by(|a, b| a.mount.cmp(&b.mount));
    v
}

// ------------------------------------------------------------------ folder sizes

#[derive(Clone, Debug, Default)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    /// Bytes actually allocated on disk (sparse files and hard links counted once).
    pub size: u64,
    pub files: u64,
    pub is_dir: bool,
}

/// Progress of a running scan, readable from the UI thread.
#[derive(Default)]
pub struct Progress {
    pub bytes: AtomicU64,
    pub files: AtomicU64,
    pub cancel: AtomicBool,
}

pub struct ScanResult {
    pub path: PathBuf,
    /// Immediate children, largest first.
    pub children: Vec<Entry>,
    pub total: u64,
    /// Folders we weren't allowed to read.
    pub denied: u64,
    pub cancelled: bool,
}

struct Walker<'a> {
    dev: u64,
    seen: std::collections::HashSet<(u64, u64)>,
    denied: u64,
    progress: &'a Progress,
}

impl Walker<'_> {
    /// Total allocated size and file count under `p`, staying on one filesystem.
    fn size(&mut self, p: &Path) -> (u64, u64) {
        if self.progress.cancel.load(Ordering::Relaxed) {
            return (0, 0);
        }
        let Ok(rd) = std::fs::read_dir(p) else {
            self.denied += 1;
            return (0, 0);
        };
        let (mut bytes, mut files) = (0u64, 0u64);
        for e in rd.flatten() {
            let Ok(md) = e.metadata() else { continue }; // symlink_metadata: links aren't followed
            if md.dev() != self.dev {
                continue; // another filesystem mounted inside
            }
            if md.nlink() > 1 && !md.is_dir() && !self.seen.insert((md.dev(), md.ino())) {
                continue; // hard link already counted
            }
            let own = md.blocks() * 512;
            if md.is_dir() {
                let (b, f) = self.size(&e.path());
                bytes += own + b;
                files += f;
            } else {
                bytes += own;
                files += 1;
                self.progress.files.fetch_add(1, Ordering::Relaxed);
                self.progress.bytes.fetch_add(own, Ordering::Relaxed);
            }
        }
        (bytes, files)
    }
}

/// Sizes of everything directly inside `path` (one filesystem, symlinks not followed).
pub fn scan(path: &Path, progress: &Progress) -> ScanResult {
    let dev = std::fs::symlink_metadata(path).map(|m| m.dev()).unwrap_or(0);
    let mut w = Walker { dev, seen: Default::default(), denied: 0, progress };
    let mut children = Vec::new();
    if let Ok(rd) = std::fs::read_dir(path) {
        for e in rd.flatten() {
            let Ok(md) = std::fs::symlink_metadata(e.path()) else { continue };
            if md.dev() != dev {
                continue;
            }
            let (size, files) = if md.is_dir() {
                let (b, f) = w.size(&e.path());
                (b + md.blocks() * 512, f)
            } else {
                progress.files.fetch_add(1, Ordering::Relaxed);
                (md.blocks() * 512, 1)
            };
            children.push(Entry {
                name: e.file_name().to_string_lossy().into_owned(),
                path: e.path(),
                size,
                files,
                is_dir: md.is_dir(),
            });
        }
    } else {
        w.denied += 1;
    }
    children.sort_by_key(|c| std::cmp::Reverse(c.size));
    let total = children.iter().map(|c| c.size).sum();
    ScanResult {
        path: path.to_path_buf(),
        children,
        total,
        denied: w.denied,
        cancelled: progress.cancel.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mounts_and_unescape() {
        assert_eq!(unescape(r"/media/My\040Disk"), "/media/My Disk");
        let fs = filesystems();
        assert!(fs.iter().any(|f| f.mount == "/"), "root filesystem listed");
        assert!(fs.iter().all(|f| f.used <= f.total && !SKIP.contains(&f.fstype.as_str())));
    }

    #[test]
    fn scans_a_tree() {
        let base = std::env::temp_dir().join(format!("taskd-scan-{}", std::process::id()));
        std::fs::create_dir_all(base.join("big/inner")).unwrap();
        std::fs::create_dir_all(base.join("small")).unwrap();
        std::fs::write(base.join("big/inner/a"), vec![1u8; 300_000]).unwrap();
        std::fs::write(base.join("small/b"), vec![1u8; 10_000]).unwrap();
        std::fs::hard_link(base.join("big/inner/a"), base.join("big/a-link")).unwrap();
        let r = scan(&base, &Progress::default());
        assert_eq!(r.children[0].name, "big");
        assert_eq!(r.children[0].files, 1, "hard link counted once");
        assert!(r.children[0].size >= 300_000 && r.children[0].size < 600_000);
        assert!(r.children[1].size >= 10_000);
        std::fs::remove_dir_all(&base).ok();
    }
}
