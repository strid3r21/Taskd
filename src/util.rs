//! Small, allocation-free helpers shared by the sampler and the UI.

use std::fs::File;
use std::io::{self, Read};

/// Read a whole (small) file into `buf`, reusing its allocation.
///
/// Uses raw open/read/close: `std::fs::read_to_end` probes the size with statx + lseek
/// first, which doubles the syscall count for procfs files (whose size is always 0 anyway).
#[inline]
pub fn read_into(path: &str, buf: &mut Vec<u8>) -> io::Result<()> {
    read_raw(path, buf, false)
}

/// Like [`read_into`] but for single-record procfs files (`/proc/<pid>/stat`, `statm`, `io`):
/// a short read means the record is complete, saving the trailing EOF `read`.
#[inline]
pub fn read_record(path: &str, buf: &mut Vec<u8>) -> io::Result<()> {
    read_raw(path, buf, true)
}

/// Re-read a single-record procfs file from offset 0 through an fd we keep open.
/// procfs regenerates the content on each read at offset 0, so this is 1 syscall
/// instead of open + read + close (and skips the costly /proc/<pid> path lookup).
pub fn pread_record(file: &File, buf: &mut Vec<u8>) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    buf.clear();
    if buf.capacity() < 4096 {
        buf.reserve(4096);
    }
    loop {
        let spare = buf.capacity() - buf.len();
        // SAFETY: reading into spare capacity; len only grows by bytes written.
        let n = unsafe {
            libc::pread(file.as_raw_fd(), buf.as_mut_ptr().add(buf.len()).cast(), spare, buf.len() as libc::off_t)
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if n == 0 {
            // A live process never yields an empty stat/statm/io record.
            return if buf.is_empty() { Err(io::ErrorKind::UnexpectedEof.into()) } else { Ok(()) };
        }
        unsafe { buf.set_len(buf.len() + n as usize) };
        if (n as usize) < spare {
            return Ok(());
        }
        buf.reserve(buf.capacity());
    }
}

/// Read a whole procfs/sysfs file through an fd kept open, from offset 0 until EOF.
/// Handles multi-record seq_files where a short read does not imply EOF.
pub fn pread_all(file: &File, buf: &mut Vec<u8>) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    buf.clear();
    if buf.capacity() < 4096 {
        buf.reserve(4096);
    }
    loop {
        if buf.len() == buf.capacity() {
            buf.reserve(buf.capacity());
        }
        let spare = buf.capacity() - buf.len();
        // SAFETY: reading into spare capacity; len only grows by bytes written.
        let n = unsafe {
            libc::pread(file.as_raw_fd(), buf.as_mut_ptr().add(buf.len()).cast(), spare, buf.len() as libc::off_t)
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if n == 0 {
            return Ok(());
        }
        unsafe { buf.set_len(buf.len() + n as usize) };
    }
}

/// A system file (e.g. `/proc/stat`, a sysfs attribute) re-read every tick via a cached fd.
pub struct CachedFile {
    path: String,
    file: Option<File>,
    /// Single-value/single-record file: a short read means EOF.
    single: bool,
}

impl CachedFile {
    pub fn new(path: impl Into<String>, single: bool) -> Self {
        CachedFile { path: path.into(), file: None, single }
    }

    pub fn read(&mut self, buf: &mut Vec<u8>) -> io::Result<()> {
        for attempt in 0..2 {
            if self.file.is_none() {
                self.file = Some(File::open(&self.path)?);
            }
            let f = self.file.as_ref().unwrap();
            let r = if self.single { pread_record(f, buf) } else { pread_all(f, buf) };
            match r {
                Ok(()) => return Ok(()),
                // Device went away or the attribute was recreated: reopen once.
                Err(e) if attempt == 1 => return Err(e),
                Err(_) => self.file = None,
            }
        }
        unreachable!()
    }

    pub fn read_u64(&mut self, buf: &mut Vec<u8>) -> Option<u64> {
        self.read(buf).ok()?;
        parse_u64(trim(buf))
    }
}

fn read_raw(path: &str, buf: &mut Vec<u8>, single_record: bool) -> io::Result<()> {
    buf.clear();
    let mut cpath = [0u8; 256];
    let b = path.as_bytes();
    if b.len() >= cpath.len() || b.contains(&0) {
        File::open(path)?.read_to_end(buf)?;
        return Ok(());
    }
    cpath[..b.len()].copy_from_slice(b);
    // SAFETY: cpath is NUL-terminated; fd is closed below on every path.
    let fd = unsafe { libc::open(cpath.as_ptr().cast(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    if buf.capacity() < 4096 {
        buf.reserve(4096);
    }
    let res = loop {
        if buf.len() == buf.capacity() {
            buf.reserve(buf.capacity());
        }
        let spare = buf.capacity() - buf.len();
        // SAFETY: we read into the vector's spare capacity and only extend len by what was written.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().add(buf.len()).cast(), spare) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break Err(e);
        }
        if n == 0 {
            break Ok(());
        }
        unsafe { buf.set_len(buf.len() + n as usize) };
        if single_record && (n as usize) < spare {
            break Ok(());
        }
    };
    // SAFETY: fd is a valid descriptor we opened.
    unsafe { libc::close(fd) };
    res
}

/// Read a sysfs/procfs file containing a single integer.
pub fn read_u64(path: &str, buf: &mut Vec<u8>) -> Option<u64> {
    read_into(path, buf).ok()?;
    parse_u64(trim(buf))
}

pub fn read_string(path: &str) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

#[inline]
pub fn trim(b: &[u8]) -> &[u8] {
    let start = b.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(b.len());
    let end = b.iter().rposition(|c| !c.is_ascii_whitespace()).map_or(start, |e| e + 1);
    &b[start..end]
}

/// Parse an unsigned decimal; stops at the first non-digit. `None` if no digits.
#[inline]
pub fn parse_u64(b: &[u8]) -> Option<u64> {
    let mut n: u64 = 0;
    let mut any = false;
    for &c in b {
        if !c.is_ascii_digit() {
            break;
        }
        n = n.wrapping_mul(10).wrapping_add((c - b'0') as u64);
        any = true;
    }
    any.then_some(n)
}

#[inline]
pub fn parse_i64(b: &[u8]) -> Option<i64> {
    match b.first() {
        Some(b'-') => parse_u64(&b[1..]).map(|v| -(v as i64)),
        _ => parse_u64(b).map(|v| v as i64),
    }
}

/// Iterate whitespace-separated fields without allocating.
#[inline]
pub fn fields(b: &[u8]) -> impl Iterator<Item = &[u8]> {
    b.split(|c| c.is_ascii_whitespace()).filter(|f| !f.is_empty())
}

/// Find `key` at the start of a line in a `Key:  value unit` file and return the value.
pub fn kv_u64(b: &[u8], key: &[u8]) -> Option<u64> {
    for line in b.split(|&c| c == b'\n') {
        if line.starts_with(key) {
            let rest = &line[key.len()..];
            return fields(rest).next().and_then(parse_u64);
        }
    }
    None
}

pub fn fmt_bytes(b: u64) -> String {
    const U: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", U[i])
    } else if v >= 10.0 {
        format!("{v:.1} {}", U[i])
    } else {
        format!("{v:.2} {}", U[i])
    }
}

pub fn fmt_rate(bps: f64) -> String {
    if bps < 0.5 { "0 B/s".into() } else { format!("{}/s", fmt_bytes(bps as u64)) }
}

pub fn fmt_bits(bps: f64) -> String {
    let bits = bps * 8.0;
    const U: [&str; 5] = ["bps", "Kbps", "Mbps", "Gbps", "Tbps"];
    let mut v = bits;
    let mut i = 0;
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 { format!("{v:.0} {}", U[i]) } else { format!("{v:.1} {}", U[i]) }
}

pub fn fmt_duration(secs: u64) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if d > 0 { format!("{d}d {h:02}:{m:02}:{s:02}") } else { format!("{h:02}:{m:02}:{s:02}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numbers() {
        assert_eq!(parse_u64(b"12345 kB"), Some(12345));
        assert_eq!(parse_u64(b"x"), None);
        assert_eq!(parse_i64(b"-5"), Some(-5));
        assert_eq!(trim(b"  42\n"), b"42");
        assert_eq!(trim(b"   "), b"");
    }

    #[test]
    fn key_values() {
        let s = b"MemTotal:       32768 kB\nMemFree:  100 kB\n";
        assert_eq!(kv_u64(s, b"MemFree:"), Some(100));
        assert_eq!(kv_u64(s, b"Nope:"), None);
    }

    #[test]
    fn formats() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1536), "1.50 KiB");
        assert_eq!(fmt_duration(90061), "1d 01:01:01");
        assert_eq!(fmt_bits(125_000.0), "1.0 Mbps");
    }
}
