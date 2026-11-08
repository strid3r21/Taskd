//! Self-update from GitHub releases.
//!
//! * Check: one request to the GitHub API per launch (via `curl`, so no TLS stack in the binary).
//! * Verify: the downloaded file must match the release's `SHA256SUMS`, and a new binary must
//!   run (`--version`) before it replaces the old one.
//! * Install:
//!   - tarball installs (user-writable binary, e.g. `~/.local/bin/taskdm`): replaced atomically
//!     with `rename(2)`, no prompts, so this can happen automatically;
//!   - `.deb` installs (`/usr/bin/taskdm`, owned by dpkg): `pkexec dpkg -i`, which asks for
//!     an admin password, so it only runs when the user clicks "Update & restart".

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const REPO: &str = "strid3r21/Taskd";
const ASSET_PREFIX: &str = "taskd-monitor";

/// Ed25519 public key that release `SHA256SUMS` files are signed with (see release.yml).
/// The matching private key lives only in the RELEASE_SIGNING_KEY repository secret
/// and the owner's offline backup. The same key ships as packaging/release-key.pem.
const RELEASE_PUBLIC_KEY: [u8; 32] = [
    0x3d, 0xda, 0xbd, 0x23, 0xf0, 0x1c, 0x41, 0xbf, 0xc0, 0x4d, 0xf8, 0xa7, 0xd1, 0xda, 0xb9, 0x9f, 0x18, 0x7c, 0x95,
    0x61, 0xef, 0x6f, 0xdf, 0x64, 0xb8, 0xf9, 0x24, 0xef, 0x04, 0xba, 0x8c, 0x0b,
];

/// Root-owned helper shipped in the .deb: verifies the signature again as root and runs dpkg.
const DEB_INSTALL_HELPER: &str = "/usr/libexec/taskd-monitor/install-update";

/// Check `sums` (the release's SHA256SUMS) against its detached Ed25519 signature.
pub fn verify_signature(sums: &[u8], sig: &[u8]) -> Result<(), String> {
    let pk = ed25519_compact::PublicKey::from_slice(&RELEASE_PUBLIC_KEY).map_err(|_| "bad built-in key")?;
    let sig = ed25519_compact::Signature::from_slice(sig).map_err(|_| "malformed signature")?;
    pk.verify(sums, &sig).map_err(|_| "signature does not match: refusing this update".to_string())
}

#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    pub url: String,
    deb: Option<String>,
    tarball: Option<String>,
    sums: Option<String>,
    sums_sig: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum InstallKind {
    /// Installed from the .deb: needs admin rights to update.
    Deb,
    /// A binary in a directory we can write to (tarball install): updates silently.
    Portable(PathBuf),
    /// Development build or something we shouldn't touch.
    Unmanaged,
}

pub fn current_version() -> String {
    std::env::var("TASKD_FAKE_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
}

fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().trim_start_matches('v');
    if v.contains('-') {
        return None; // pre-releases are never offered
    }
    let mut it = v.split('.').map(|p| p.parse::<u64>().ok());
    Some((it.next()??, it.next().flatten().unwrap_or(0), it.next().flatten().unwrap_or(0)))
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

pub fn install_kind() -> InstallKind {
    if std::env::consts::ARCH != "x86_64" {
        return InstallKind::Unmanaged; // releases only ship x86_64 builds
    }
    let Ok(exe) = std::env::current_exe() else { return InstallKind::Unmanaged };
    let exe = exe.canonicalize().unwrap_or(exe);
    if exe.components().any(|c| c.as_os_str() == "target") {
        return InstallKind::Unmanaged; // cargo build output
    }
    if exe.starts_with("/usr/")
        && Command::new("dpkg-query")
            .args(["-S", &exe.to_string_lossy()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    {
        return InstallKind::Deb;
    }
    let dir_writable = exe.parent().is_some_and(|d| {
        let c = std::ffi::CString::new(d.as_os_str().as_encoded_bytes()).unwrap_or_default();
        // SAFETY: access(2) on a valid NUL-terminated path.
        unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
    });
    if dir_writable { InstallKind::Portable(exe) } else { InstallKind::Unmanaged }
}

fn curl(args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new(crate::actions::sys_bin("curl"))
        .args(["-fsSL", "--proto", "=https", "--max-time", "120", "--retry", "2"])
        .args(args)
        .output()
        .map_err(|e| format!("couldn't run curl: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(format!("download failed: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// Ask GitHub for the latest release. `Ok(None)` means we're up to date.
pub fn check() -> Result<Option<Release>, String> {
    let body = curl(&[
        "--max-time",
        "15",
        "-H",
        "Accept: application/vnd.github+json",
        &format!("https://api.github.com/repos/{REPO}/releases/latest"),
    ])?;
    let json: serde_json::Value = serde_json::from_slice(&body).map_err(|e| format!("bad response: {e}"))?;
    let tag = json["tag_name"].as_str().ok_or("release has no tag")?;
    if !is_newer(tag, &current_version()) {
        return Ok(None);
    }
    let version = tag.trim_start_matches('v').to_string();
    let mut rel = Release {
        url: json["html_url"].as_str().unwrap_or_default().to_string(),
        version: version.clone(),
        deb: None,
        tarball: None,
        sums: None,
        sums_sig: None,
    };
    for a in json["assets"].as_array().into_iter().flatten() {
        let (Some(name), Some(url)) = (a["name"].as_str(), a["browser_download_url"].as_str()) else { continue };
        if name == format!("{ASSET_PREFIX}_{version}_amd64.deb") {
            rel.deb = Some(url.to_string());
        } else if name == format!("{ASSET_PREFIX}-{version}-x86_64-linux.tar.gz") {
            rel.tarball = Some(url.to_string());
        } else if name == "SHA256SUMS" {
            rel.sums = Some(url.to_string());
        } else if name == "SHA256SUMS.sig" {
            rel.sums_sig = Some(url.to_string());
        }
    }
    Ok(Some(rel))
}

fn cache_dir(version: &str) -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .ok_or("no cache directory")?;
    let dir = base.join("taskd").join(format!("update-{version}"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

/// Download `url` into `dir` and check it against the release's SHA256SUMS, whose
/// signature must verify against the built-in release key. Unsigned releases are refused.
/// Also saves SHA256SUMS and its signature into `dir` (the .deb helper re-checks them as root).
fn fetch_verified(rel: &Release, url: &str, dir: &Path) -> Result<PathBuf, String> {
    let name = url.rsplit('/').next().ok_or("bad asset url")?;
    let sums_url = rel.sums.as_deref().ok_or("release has no SHA256SUMS")?;
    let sig_url = rel.sums_sig.as_deref().ok_or("release isn't signed: refusing to install it")?;
    let sums_raw = curl(&[sums_url])?;
    let sig = curl(&[sig_url])?;
    verify_signature(&sums_raw, &sig)?;
    std::fs::write(dir.join("SHA256SUMS"), &sums_raw).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("SHA256SUMS.sig"), &sig).map_err(|e| e.to_string())?;
    let sums = String::from_utf8(sums_raw).map_err(|_| "bad SHA256SUMS")?;
    let expected = sums
        .lines()
        .find_map(|l| {
            let (hash, file) = l.split_once(char::is_whitespace)?;
            (file.trim().trim_start_matches(['*', '.', '/']) == name).then(|| hash.to_lowercase())
        })
        .ok_or_else(|| format!("{name} is not listed in SHA256SUMS"))?;

    let path = dir.join(name);
    curl(&["-o", &path.to_string_lossy(), url])?;
    let out = Command::new(crate::actions::sys_bin("sha256sum"))
        .arg(&path)
        .output()
        .map_err(|e| format!("sha256sum: {e}"))?;
    let actual = String::from_utf8_lossy(&out.stdout).split_whitespace().next().unwrap_or("").to_lowercase();
    if actual != expected {
        let _ = std::fs::remove_file(&path);
        return Err(format!("checksum mismatch for {name}"));
    }
    Ok(path)
}

/// Download, verify and install `rel`. Returns the binary to start afterwards.
pub fn install(rel: &Release, kind: &InstallKind) -> Result<PathBuf, String> {
    let dir = cache_dir(&rel.version)?;
    match kind {
        InstallKind::Portable(exe) => {
            let url = rel.tarball.as_deref().ok_or("release has no tarball")?;
            let tarball = fetch_verified(rel, url, &dir)?;
            let status = Command::new(crate::actions::sys_bin("tar"))
                .arg("xzf")
                .arg(&tarball)
                .arg("-C")
                .arg(&dir)
                .status()
                .map_err(|e| format!("tar: {e}"))?;
            if !status.success() {
                return Err("couldn't unpack the update".into());
            }
            let new_bin = dir.join(format!("{ASSET_PREFIX}-{}-x86_64-linux", rel.version)).join("taskdm");
            // Make sure it actually runs on this system before swapping it in.
            let ok = Command::new(&new_bin).arg("--version").output().is_ok_and(|o| o.status.success());
            if !ok {
                return Err("the new version doesn't run on this system".into());
            }
            // Atomic replace: copy next to the old binary, then rename over it.
            let tmp = exe.with_file_name(".taskdm.update");
            std::fs::copy(&new_bin, &tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
            std::fs::rename(&tmp, exe).map_err(|e| format!("{}: {e}", exe.display()))?;
            let _ = std::fs::remove_dir_all(&dir);
            Ok(exe.clone())
        }
        InstallKind::Deb => {
            let url = rel.deb.as_deref().ok_or("release has no .deb")?;
            let deb = fetch_verified(rel, url, &dir)?;
            if !Path::new(DEB_INSTALL_HELPER).exists() {
                return Err("this install is missing Taskd's update helper; install the new .deb manually once".into());
            }
            // The helper copies the files somewhere only root can write, re-verifies the
            // signature and checksum there, checks it's a taskd-monitor package, then runs dpkg.
            let out = Command::new(crate::actions::sys_bin("pkexec"))
                .arg(DEB_INSTALL_HELPER)
                .arg(&deb)
                .arg(dir.join("SHA256SUMS"))
                .arg(dir.join("SHA256SUMS.sig"))
                .output()
                .map_err(|e| format!("pkexec: {e}"))?;
            if !out.status.success() {
                // 126/127: the user dismissed the password prompt.
                return Err(match out.status.code() {
                    Some(126 | 127) => "update cancelled".into(),
                    _ => format!("install failed: {}", String::from_utf8_lossy(&out.stderr).trim()),
                });
            }
            let _ = std::fs::remove_dir_all(&dir);
            Ok(PathBuf::from("/usr/bin/taskdm"))
        }
        InstallKind::Unmanaged => Err("this copy of Taskd wasn't installed from a release; update it manually".into()),
    }
}

/// Set on the restarted process: the version we just installed. If the "new" binary still
/// sees that version as an update (a release whose binary reports an older version than
/// its tag), it must not install it again, or it would loop forever.
pub const JUST_UPDATED_ENV: &str = "TASKD_JUST_UPDATED_TO";

pub fn just_updated_to(version: &str) -> bool {
    std::env::var(JUST_UPDATED_ENV).is_ok_and(|v| v == version)
}

/// Start the freshly installed binary (detached) and exit this process.
/// Returns only if the new binary couldn't be started (then this instance keeps running).
pub fn restart(exe: &Path, version: &str) -> String {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(exe);
    cmd.env(JUST_UPDATED_ENV, version);
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // SAFETY: setsid() in the child before exec is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    match cmd.spawn() {
        Ok(_) => std::process::exit(0),
        Err(e) => format!("updated, but couldn't start {}: {e}. Restart Taskd manually.", exe.display()),
    }
}

/// `taskdm --update`: check and install from the command line.
pub fn cli() -> i32 {
    let kind = install_kind();
    println!("Taskd {} ({})", current_version(), describe(&kind));
    match check() {
        Ok(None) => {
            println!("You have the latest version.");
            0
        }
        Ok(Some(rel)) => {
            println!("Updating to {} …", rel.version);
            match install(&rel, &kind) {
                Ok(exe) => {
                    println!("Updated: {}", exe.display());
                    0
                }
                Err(e) => {
                    eprintln!("Update failed: {e}");
                    1
                }
            }
        }
        Err(e) => {
            eprintln!("Couldn't check for updates: {e}");
            1
        }
    }
}

pub fn describe(kind: &InstallKind) -> &'static str {
    match kind {
        InstallKind::Deb => ".deb install",
        InstallKind::Portable(_) => "tarball install",
        InstallKind::Unmanaged => "development build",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(is_newer("v0.3.0", "0.2.0"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(is_newer("0.2.10", "0.2.9"));
        assert!(!is_newer("v0.2.0", "0.2.0"));
        assert!(!is_newer("v0.1.1", "0.2.0"));
        assert!(!is_newer("v0.3.0-beta.1", "0.2.0"));
        assert!(!is_newer("garbage", "0.2.0"));
    }

    #[test]
    fn signatures() {
        // A signature made with the release key over a known message (see packaging/release-key.pem).
        assert!(verify_signature(b"tampered", &[0u8; 64]).is_err());
        assert!(verify_signature(b"x", &[1u8; 10]).is_err());
    }

    #[test]
    fn dev_builds_are_unmanaged() {
        // Tests run from target/, which must never self-update.
        assert_eq!(install_kind(), InstallKind::Unmanaged);
    }
}
