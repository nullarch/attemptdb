//! Per-user background service registration for the daemon (RFC 0005 §6).
//!
//! | OS | Unit | Activation |
//! |---|---|---|
//! | macOS | `~/Library/LaunchAgents/dev.attemptdb.daemon.plist` | `launchctl bootstrap gui/<uid>` / `bootout` |
//! | Linux | `~/.config/systemd/user/attemptdb.service` | `systemctl --user enable --now` / `disable --now` |
//! | Windows | Task Scheduler task `AttemptDB Sync` running `attempt maintenance` every minute | `schtasks /Create` / `/Delete` |
//!
//! Windows has no daemon yet, so the task is what stands in for it: opening
//! the database imports whatever the hooks spooled, `maintenance` uploads
//! everything after the cursor and applies the release policy once a day —
//! capture stays immediate, the server is at most a minute behind. The task runs the executable directly. It used to
//! run a PowerShell one-liner whose quoting depended on how `-Command`
//! stripped quotes; a task that runs one program with its arguments has no
//! such class of failure.
//!
//! Nothing here runs implicitly: only `attempt daemon install|uninstall`
//! calls into this module. The unit runs `attempt daemon run` *without*
//! `--foreground` (the daemon logs to `daemon.log`; the supervisor captures
//! stderr separately). Portable mode and an explicit database directory are
//! baked into the unit as `ATTEMPTDB_DATA_DIR` / `ATTEMPTDB_DIR` so the
//! service resolves the same paths the installing shell did.

use crate::daemon;
use crate::locator::{DbSource, Locator};
use crate::platform::{AppPaths, home_dir};
use crate::{CaptureError, Result, io_at};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// launchd label (macOS).
pub const LAUNCHD_LABEL: &str = "dev.attemptdb.daemon";
/// systemd user unit name (Linux).
pub const SYSTEMD_UNIT: &str = "attemptdb.service";
/// Task Scheduler task name (Windows).
pub const WINDOWS_TASK: &str = "AttemptDB Sync";
/// How often that task uploads, in minutes.
pub const WINDOWS_TASK_MINUTES: u32 = 1;

/// Where the service definition lives on this platform, if it has one.
pub fn service_path() -> Option<PathBuf> {
    let home = home_dir()?;
    if cfg!(target_os = "macos") {
        Some(
            home.join("Library")
                .join("LaunchAgents")
                .join(format!("{LAUNCHD_LABEL}.plist")),
        )
    } else if cfg!(target_os = "linux") {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".config"));
        Some(config.join("systemd").join("user").join(SYSTEMD_UNIT))
    } else {
        None
    }
}

/// Whether the service can be registered on this platform.
pub fn is_supported() -> bool {
    cfg!(any(target_os = "macos", target_os = "linux", windows))
}

/// True where the registration is a periodic uploader rather than a
/// supervised daemon, so callers do not wait for a daemon that will not
/// appear.
pub fn is_periodic_uploader() -> bool {
    cfg!(windows)
}

/// What to call the registration in output. Windows has no unit file.
pub fn service_label() -> String {
    if cfg!(windows) {
        format!("Task Scheduler \\ {WINDOWS_TASK}")
    } else {
        service_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(none)".into())
    }
}

/// What the Windows task runs: the executable, its arguments, nothing else.
/// A scheduled task inherits no environment, so a portable or explicit
/// database goes in as a flag rather than as `ATTEMPTDB_DATA_DIR`.
pub fn windows_task_action(locator: &Locator, binary: &Path) -> String {
    let mut action = format!("\"{}\"", binary.display());
    if is_portable(&locator.paths) {
        action.push_str(&format!(
            " --data-dir \"{}\"",
            locator.paths.data_dir.display()
        ));
    } else if locator.source != DbSource::Default {
        action.push_str(&format!(" --db \"{}\"", locator.db_dir.display()));
    }
    action.push_str(" maintenance");
    action
}

fn not_supported() -> CaptureError {
    CaptureError::Other(if cfg!(windows) {
        "daemon service registration is not implemented on Windows yet (planned: a per-user autostart entry under \
         HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run running `attempt daemon run`); \
         run `attempt daemon run` under your own supervisor for now"
            .to_string()
    } else {
        "no per-user service mechanism is known for this platform; run `attempt daemon run` under your own supervisor"
            .to_string()
    })
}

/// True when every directory hangs off the data root (`--data-dir` /
/// `ATTEMPTDB_DATA_DIR`).
pub(crate) fn is_portable(paths: &AppPaths) -> bool {
    paths.config_dir == paths.data_dir.join("config")
        && paths.cache_dir == paths.data_dir.join("cache")
        && paths.runtime_dir == paths.data_dir.join("run")
        && paths.log_dir == paths.data_dir.join("logs")
}

/// Environment the unit must carry so the daemon resolves the same paths.
pub fn service_env(locator: &Locator) -> Vec<(String, String)> {
    let mut env = Vec::new();
    if is_portable(&locator.paths) {
        env.push((
            crate::platform::DATA_DIR_ENV.to_string(),
            locator.paths.data_dir.to_string_lossy().into_owned(),
        ));
    }
    if locator.source != DbSource::Default {
        env.push((
            crate::locator::DB_DIR_ENV.to_string(),
            locator.db_dir.to_string_lossy().into_owned(),
        ));
    }
    env
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The launchd property list (macOS).
pub fn render_launchd_plist(locator: &Locator, binary: &Path) -> String {
    let logs = &locator.paths.log_dir;
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n");
    out.push_str("<plist version=\"1.0\">\n<dict>\n");
    out.push_str(&format!(
        "\t<key>Label</key>\n\t<string>{LAUNCHD_LABEL}</string>\n"
    ));
    out.push_str("\t<key>ProgramArguments</key>\n\t<array>\n");
    for arg in [binary.to_string_lossy().as_ref(), "daemon", "run"] {
        out.push_str(&format!("\t\t<string>{}</string>\n", xml_escape(arg)));
    }
    out.push_str("\t</array>\n");
    let env = service_env(locator);
    if !env.is_empty() {
        out.push_str("\t<key>EnvironmentVariables</key>\n\t<dict>\n");
        for (k, v) in &env {
            out.push_str(&format!(
                "\t\t<key>{}</key>\n\t\t<string>{}</string>\n",
                xml_escape(k),
                xml_escape(v)
            ));
        }
        out.push_str("\t</dict>\n");
    }
    out.push_str("\t<key>RunAtLoad</key>\n\t<true/>\n");
    out.push_str("\t<key>KeepAlive</key>\n\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t</dict>\n");
    out.push_str("\t<key>ProcessType</key>\n\t<string>Background</string>\n");
    out.push_str("\t<key>ThrottleInterval</key>\n\t<integer>10</integer>\n");
    out.push_str(&format!(
        "\t<key>StandardOutPath</key>\n\t<string>{}</string>\n",
        xml_escape(&logs.join("daemon.stdout.log").to_string_lossy())
    ));
    out.push_str(&format!(
        "\t<key>StandardErrorPath</key>\n\t<string>{}</string>\n",
        xml_escape(&logs.join("daemon.stderr.log").to_string_lossy())
    ));
    out.push_str("</dict>\n</plist>\n");
    out
}

/// Quote a value for a systemd unit line (`ExecStart=`, `Environment=`):
/// `%` is a specifier prefix, backslash and double quote need escaping.
fn systemd_quote(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('%', "%%")
        .replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// The systemd user unit (Linux).
pub fn render_systemd_unit(locator: &Locator, binary: &Path) -> String {
    let mut out = String::new();
    out.push_str("[Unit]\nDescription=AttemptDB capture daemon\nDocumentation=https://github.com/streamize/attemptdb\n\n");
    out.push_str("[Service]\nType=simple\n");
    out.push_str(&format!(
        "ExecStart={} daemon run\n",
        systemd_quote(&binary.to_string_lossy())
    ));
    for (k, v) in service_env(locator) {
        out.push_str(&format!(
            "Environment={}\n",
            systemd_quote(&format!("{k}={v}"))
        ));
    }
    out.push_str("Restart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n");
    out
}

fn run_cmd(program: &str, args: &[&str]) -> std::result::Result<String, String> {
    match Command::new(program).args(args).output() {
        Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(o) => Err(format!(
            "`{program} {}` failed ({}): {}",
            args.join(" "),
            o.status,
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => Err(format!("cannot run `{program}`: {e}")),
    }
}

fn uid_string() -> String {
    crate::ipc::current_uid()
        .map(|u| u.to_string())
        .unwrap_or_else(|| "0".into())
}

fn write_atomically(path: &Path, content: &str) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| CaptureError::Other(format!("{} has no parent", path.display())))?;
    std::fs::create_dir_all(dir).map_err(|e| io_at(dir, e))?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&tmp, content).map_err(|e| io_at(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| io_at(path, e))?;
    Ok(())
}

/// Stop a daemon started by hand so the supervised one can take the lock.
fn stop_foreground_daemon(locator: &Locator) -> Result<()> {
    if daemon::stop(locator)? && !daemon::wait_until_stopped(locator, Duration::from_secs(15)) {
        return Err(CaptureError::Other(
            "a running daemon did not stop within 15 s; stop it before installing the service"
                .into(),
        ));
    }
    Ok(())
}

/// Write the unit for `binary`, register it with the OS, and start it.
/// Returns the unit path. Only `attempt daemon install` calls this.
pub fn install_service(locator: &Locator, binary: &Path) -> Result<PathBuf> {
    if cfg!(windows) {
        let binary = crate::platform::canonical_display_path(binary);
        let action = windows_task_action(locator, &binary);
        run_cmd(
            "schtasks",
            &[
                "/Create",
                "/F",
                "/SC",
                "MINUTE",
                "/MO",
                &WINDOWS_TASK_MINUTES.to_string(),
                "/TN",
                WINDOWS_TASK,
                "/TR",
                &action,
            ],
        )
        .map_err(CaptureError::Other)?;
        return Ok(PathBuf::from(service_label()));
    }
    let Some(path) = service_path() else {
        return Err(not_supported());
    };
    let binary = crate::platform::canonical_display_path(binary);
    let _ = std::fs::create_dir_all(&locator.paths.log_dir);
    stop_foreground_daemon(locator)?;

    if cfg!(target_os = "macos") {
        write_atomically(&path, &render_launchd_plist(locator, &binary))?;
        let domain = format!("gui/{}", uid_string());
        // A previous registration must be unloaded before bootstrap accepts the file again.
        let _ = run_cmd(
            "launchctl",
            &["bootout", &format!("{domain}/{LAUNCHD_LABEL}")],
        );
        // launchd tears the old service down asynchronously; a bootstrap
        // that lands during the teardown fails with "Input/output error"
        // (exit 5). Seen on the first upgrade of a running daemon. A short
        // wait and a retry is what the manual fix amounted to.
        let plist = path.to_string_lossy().to_string();
        let mut last = None;
        for attempt in 0..5 {
            match run_cmd("launchctl", &["bootstrap", &domain, &plist]) {
                Ok(_) => {
                    last = None;
                    break;
                }
                Err(e) => {
                    last = Some(e);
                    std::thread::sleep(Duration::from_millis(300 * (attempt + 1)));
                }
            }
        }
        if let Some(e) = last {
            return Err(CaptureError::Other(format!(
                "{e}\nthe agent file was written to {}; load it with `launchctl bootstrap {domain} {}`",
                path.display(),
                path.display()
            )));
        }
    } else if cfg!(target_os = "linux") {
        write_atomically(&path, &render_systemd_unit(locator, &binary))?;
        run_cmd("systemctl", &["--user", "daemon-reload"])
            .and_then(|_| run_cmd("systemctl", &["--user", "enable", "--now", SYSTEMD_UNIT]))
            .map_err(|e| {
                CaptureError::Other(format!(
                    "{e}\nthe unit was written to {}; enable it with `systemctl --user enable --now {SYSTEMD_UNIT}`",
                    path.display()
                ))
            })?;
    } else {
        return Err(not_supported());
    }
    Ok(path)
}

/// Unregister and remove the unit. Returns the removed path, or `None` when
/// nothing was registered.
/// Restart the daemon through the per-user service manager when the service
/// is installed (`launchctl kickstart -k` / `systemctl --user restart`).
/// Returns `Ok(false)` when no service is registered, so the caller can fall
/// back to stopping and respawning the daemon itself.
pub fn restart_service(locator: &Locator) -> Result<bool> {
    let _ = locator;
    let Some(path) = service_path() else {
        return Ok(false);
    };
    if !path.is_file() {
        return Ok(false);
    }
    if cfg!(target_os = "macos") {
        run_cmd(
            "launchctl",
            &[
                "kickstart",
                "-k",
                &format!("gui/{}/{LAUNCHD_LABEL}", uid_string()),
            ],
        )
        .map_err(CaptureError::Other)?;
        Ok(true)
    } else if cfg!(target_os = "linux") {
        run_cmd("systemctl", &["--user", "restart", SYSTEMD_UNIT]).map_err(CaptureError::Other)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

pub fn uninstall_service(locator: &Locator) -> Result<Option<PathBuf>> {
    if cfg!(windows) {
        // `/Delete` fails when there is no such task; that is "nothing was
        // registered", not an error.
        return Ok(
            match run_cmd("schtasks", &["/Delete", "/F", "/TN", WINDOWS_TASK]) {
                Ok(_) => Some(PathBuf::from(service_label())),
                Err(_) => None,
            },
        );
    }
    let Some(path) = service_path() else {
        return Err(not_supported());
    };
    if cfg!(target_os = "macos") {
        let _ = run_cmd(
            "launchctl",
            &["bootout", &format!("gui/{}/{LAUNCHD_LABEL}", uid_string())],
        );
    } else if cfg!(target_os = "linux") {
        let _ = run_cmd("systemctl", &["--user", "disable", "--now", SYSTEMD_UNIT]);
    } else {
        return Err(not_supported());
    }
    // The supervisor sends SIGTERM; give the daemon a moment to flush.
    let _ = daemon::wait_until_stopped(locator, Duration::from_secs(15));
    if !path.exists() {
        return Ok(None);
    }
    std::fs::remove_file(&path).map_err(|e| io_at(&path, e))?;
    if cfg!(target_os = "linux") {
        let _ = run_cmd("systemctl", &["--user", "daemon-reload"]);
    }
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_windows_task_runs_one_program_with_its_arguments() {
        let tmp = tempfile::tempdir().unwrap();
        let locator = portable_locator(tmp.path());
        let action = windows_task_action(&locator, Path::new("C:\\Users\\a b\\attempt.exe"));
        // The executable is quoted (paths have spaces), the portable data
        // directory rides along as a flag because a scheduled task inherits
        // no environment, and nothing here needs a shell to parse it.
        assert!(
            action.starts_with("\"C:\\Users\\a b\\attempt.exe\""),
            "{action}"
        );
        assert!(action.contains("--data-dir \""), "{action}");
        assert!(action.ends_with(" maintenance"), "{action}");
        assert!(!action.contains("powershell"), "{action}");
        assert!(!action.contains(';'), "{action}");
    }

    fn portable_locator(root: &Path) -> Locator {
        Locator::resolve(root, Some(&root.join("data")), None)
    }

    #[test]
    fn plist_escapes_and_carries_portable_env() {
        let tmp = tempfile::tempdir().unwrap();
        let loc = portable_locator(tmp.path());
        let plist = render_launchd_plist(&loc, Path::new("/opt/a&b/attempt"));
        assert!(plist.contains("<string>dev.attemptdb.daemon</string>"));
        assert!(plist.contains("<string>/opt/a&amp;b/attempt</string>"));
        assert!(plist.contains("<string>daemon</string>\n\t\t<string>run</string>"));
        assert!(plist.contains("<key>ATTEMPTDB_DATA_DIR</key>"));
        assert!(plist.contains("<key>SuccessfulExit</key>\n\t\t<false/>"));
        assert!(
            !plist.contains("ATTEMPTDB_DIR</key>"),
            "default db must not be pinned"
        );
    }

    #[test]
    fn systemd_unit_quotes_specifiers() {
        let tmp = tempfile::tempdir().unwrap();
        let loc = Locator::resolve(
            tmp.path(),
            Some(&tmp.path().join("data")),
            Some(&tmp.path().join("x")),
        );
        let unit = render_systemd_unit(&loc, Path::new("/opt/100%/att\"empt"));
        assert!(unit.contains("ExecStart=\"/opt/100%%/att\\\"empt\" daemon run"));
        assert!(unit.contains("Environment=\"ATTEMPTDB_DATA_DIR="));
        assert!(unit.contains("Environment=\"ATTEMPTDB_DIR="));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=default.target"));
    }
}
