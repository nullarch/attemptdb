//! Where the `attempt` binary is, and how it gets onto the machine.
//!
//! The app ships `attempt` and `attempt-hook` as sidecars, beside its own
//! executable inside the bundle. It does not run them from there: hooks and
//! the daemon reference an absolute path, and a path inside an app bundle
//! breaks the moment the app is moved, updated or deleted. So the first
//! thing setup does is copy the pair to `~/.local/bin` — the same place the
//! terminal installer uses — and everything after that drives that copy.
//! `attempt update` keeps it current from then on, app or no app.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The two executables a release carries, in this order.
pub const NAMES: [&str; 2] = ["attempt", "attempt-hook"];

pub fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// A binary that was found, and what it says it is.
#[derive(Clone, Debug, Serialize)]
pub struct Located {
    pub path: PathBuf,
    /// `attempt --version` without the leading word; `None` when it did not run.
    pub version: Option<String>,
    /// It knows `attempt setup` — releases before 0.2.10 do not, and the
    /// app has nothing to say to a binary that cannot wire a machine.
    pub supports_setup: bool,
}

impl Located {
    fn at(path: PathBuf) -> Option<Self> {
        path.is_file().then(|| Self {
            version: version_of(&path),
            supports_setup: supports_setup(&path),
            path,
        })
    }
}

fn supports_setup(path: &Path) -> bool {
    Command::new(path)
        .args(["setup", "--help"])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// `~/.local/bin`: where the terminal installer puts the binary too, so the
/// two paths onto a machine end in the same place.
pub fn install_dir(home: &Path) -> PathBuf {
    home.join(".local").join("bin")
}

/// The sidecar directory: beside this executable, in the bundle or in a
/// development target directory. `None` when this build carries none.
pub fn bundled_dir() -> Option<PathBuf> {
    let exe_path = std::env::current_exe().ok()?;
    let dir = exe_path.parent()?.to_path_buf();
    dir.join(exe("attempt")).is_file().then_some(dir)
}

pub fn version_of(path: &Path) -> Option<String> {
    let out = Command::new(path).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next()?.trim();
    Some(
        line.strip_prefix("attempt ")
            .unwrap_or(line)
            .trim()
            .to_string(),
    )
}

/// An `attempt` on `PATH`, for a machine that was set up from the terminal
/// or `cargo install` before the app arrived.
fn on_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(exe("attempt")))
        .find(|p| p.is_file())
}

#[derive(Clone, Debug, Serialize)]
pub struct Binaries {
    /// The sidecar this build carries.
    pub bundled: Option<Located>,
    /// The copy the machine runs: `~/.local/bin/attempt`, else one on `PATH`.
    pub installed: Option<Located>,
}

impl Binaries {
    pub fn locate(home: &Path) -> Self {
        let bundled = bundled_dir().and_then(|d| Located::at(d.join(exe("attempt"))));
        let installed = Located::at(install_dir(home).join(exe("attempt")))
            .or_else(|| on_path().and_then(Located::at));
        Self { bundled, installed }
    }

    /// The binary to drive: the installed one (its path is what the hooks
    /// and the daemon reference) when it can run setup, else the sidecar —
    /// for read-only probes until setup puts it in place.
    pub fn active(&self) -> Option<&Located> {
        self.installed
            .as_ref()
            .filter(|i| i.supports_setup)
            .or(self.bundled.as_ref())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct InstallOutcome {
    pub path: PathBuf,
    pub version: Option<String>,
    /// The sidecar was copied into place (false: what was there is current).
    pub copied: bool,
    pub note: String,
}

fn semver(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((it.next()??, it.next()??, it.next()??))
}

/// Whether the sidecar should replace what is installed: yes when nothing
/// is known about either, otherwise only for a strictly newer version. A
/// machine that `attempt update` moved ahead of the app keeps its binary.
fn should_copy(bundled: &Option<String>, installed: &Option<String>) -> bool {
    match (
        bundled.as_deref().and_then(semver),
        installed.as_deref().and_then(semver),
    ) {
        (Some(b), Some(i)) => b > i,
        _ => true,
    }
}

/// Put the sidecars in `~/.local/bin` when that is an improvement, and say
/// which binary the machine will run either way.
pub fn install(home: &Path) -> Result<InstallOutcome, String> {
    let found = Binaries::locate(home);
    let (bundled, installed) = (found.bundled, found.installed);
    let Some(bundled) = bundled else {
        return match installed {
            Some(i) => Ok(InstallOutcome {
                path: i.path,
                version: i.version,
                copied: false,
                note: "this build carries no attempt binary; using the installed one".into(),
            }),
            None => Err(
                "this build carries no attempt binary and none is installed. Install from the terminal: curl -fsSL https://raw.githubusercontent.com/nullarch/attemptdb/main/install.sh | sh"
                    .into(),
            ),
        };
    };
    if let Some(i) = &installed
        && i.supports_setup
        && !should_copy(&bundled.version, &i.version)
    {
        return Ok(InstallOutcome {
            path: i.path.clone(),
            version: i.version.clone(),
            copied: false,
            note: format!(
                "installed attempt {} is current (this build carries {})",
                i.version.as_deref().unwrap_or("?"),
                bundled.version.as_deref().unwrap_or("?")
            ),
        });
    }
    let dir = install_dir(home);
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let src_dir = bundled
        .path
        .parent()
        .ok_or("sidecar has no parent directory")?;
    for name in NAMES {
        let src = src_dir.join(exe(name));
        if !src.is_file() {
            if name == "attempt" {
                return Err(format!("sidecar missing: {}", src.display()));
            }
            continue;
        }
        copy_atomically(&src, &dir.join(exe(name)))?;
    }
    let path = dir.join(exe("attempt"));
    Ok(InstallOutcome {
        version: version_of(&path),
        path,
        copied: true,
        note: match installed {
            Some(i) => format!(
                "replaced attempt {} with {}",
                i.version.as_deref().unwrap_or("?"),
                bundled.version.as_deref().unwrap_or("?")
            ),
            None => format!(
                "installed attempt {}",
                bundled.version.as_deref().unwrap_or("?")
            ),
        },
    })
}

/// Write beside the target and rename over it, so a running daemon keeps its
/// open file and a crash mid-copy leaves the old binary intact. On macOS the
/// copy inherits the download's quarantine flag, which Gatekeeper would then
/// enforce on every hook run; clearing it is what the terminal installer and
/// Homebrew do as well.
fn copy_atomically(src: &Path, dst: &Path) -> Result<(), String> {
    let tmp = dst.with_file_name(format!(
        ".{}.new",
        dst.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attempt")
    ));
    std::fs::copy(src, &tmp)
        .map_err(|e| format!("copying {} to {}: {e}", src.display(), tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, dst)
        .map_err(|e| format!("renaming {} to {}: {e}", tmp.display(), dst.display()))?;
    if cfg!(target_os = "macos") {
        let _ = Command::new("xattr")
            .args(["-d", "com.apple.quarantine"])
            .arg(dst)
            .output();
    }
    Ok(())
}

/// Run `attempt --json <args>` and parse what it printed. A non-zero exit
/// with a JSON report (setup finishing with problems) is still a report.
pub fn run_json(bin: &Path, args: &[&str]) -> Result<serde_json::Value, String> {
    let out = Command::new(bin)
        .arg("--json")
        .args(args)
        .output()
        .map_err(|e| format!("running {}: {e}", bin.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    match serde_json::from_str::<serde_json::Value>(stdout.trim()) {
        Ok(v) => Ok(v),
        Err(_) => Err(format!(
            "attempt {} exited with {}:\n{}{}",
            args.join(" "),
            out.status,
            stdout,
            String::from_utf8_lossy(&out.stderr)
        )),
    }
}

/// Run `attempt <args>` for its text: what a terminal would have shown.
pub fn run_text(bin: &Path, args: &[&str]) -> Result<(bool, String), String> {
    let out = Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| format!("running {}: {e}", bin.display()))?;
    Ok((
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_newer_sidecar_replaces_and_an_older_one_does_not() {
        let v = |s: &str| Some(s.to_string());
        assert!(should_copy(&v("0.3.0"), &v("0.2.9")));
        assert!(!should_copy(&v("0.2.9"), &v("0.2.9")));
        assert!(!should_copy(&v("0.2.9"), &v("0.2.13")));
        assert!(should_copy(&v("0.3.0"), &None));
        assert!(should_copy(&None, &v("0.2.9")));
        assert!(should_copy(&v("0.3.0-rc.1"), &v("0.2.9")));
    }
}
