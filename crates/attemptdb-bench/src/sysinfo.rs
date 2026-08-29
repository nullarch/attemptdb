//! Machine and build description for the methodology section.

use serde::Serialize;
use std::path::Path;
use std::process::Command;

#[derive(Clone, Debug, Default, Serialize)]
pub struct MachineInfo {
    pub os: String,
    pub os_version: String,
    pub arch: String,
    pub cpu: String,
    pub logical_cpus: usize,
    pub memory_bytes: u64,
    pub disk: String,
    pub rustc: String,
    pub commit: String,
    pub profile: String,
    pub attempt_binary: String,
    pub attempt_binary_bytes: u64,
}

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    run_in(cmd, args, None)
}

/// Run a command, optionally in a specific directory (git must run inside
/// the repository whatever the benchmark's working directory is).
fn run_in(cmd: &str, args: &[&str], dir: Option<&Path>) -> Option<String> {
    let mut c = Command::new(cmd);
    c.args(args);
    if let Some(d) = dir {
        c.current_dir(d);
    }
    let out = c.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn sysctl(key: &str) -> Option<String> {
    run("sysctl", &["-n", key])
}

pub fn collect(attempt_bin: Option<&Path>) -> MachineInfo {
    let mut info = MachineInfo {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        logical_cpus: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0),
        rustc: run("rustc", &["--version"]).unwrap_or_default(),
        commit: {
            let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
            let head =
                run_in("git", &["rev-parse", "--short", "HEAD"], Some(repo)).unwrap_or_default();
            let dirty = run_in("git", &["status", "--porcelain"], Some(repo))
                .is_some_and(|s| !s.is_empty());
            if dirty && !head.is_empty() {
                format!("{head} (working tree had uncommitted changes)")
            } else {
                head
            }
        },
        profile: if cfg!(debug_assertions) {
            "debug".into()
        } else {
            "release".into()
        },
        ..Default::default()
    };
    if cfg!(target_os = "macos") {
        info.cpu = sysctl("machdep.cpu.brand_string").unwrap_or_default();
        info.memory_bytes = sysctl("hw.memsize")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        info.os_version = run("sw_vers", &["-productVersion"])
            .map(|v| format!("macOS {v}"))
            .unwrap_or_default();
        if let Some(out) = run("diskutil", &["info", "/"]) {
            let mut parts = Vec::new();
            for line in out.lines() {
                let l = line.trim();
                for key in ["File System Personality", "Protocol", "Solid State"] {
                    if let Some(v) = l.strip_prefix(key)
                        && let Some(v) = v.trim().strip_prefix(':')
                    {
                        parts.push(format!("{key}: {}", v.trim()));
                    }
                }
            }
            info.disk = parts.join(", ");
        }
    } else if cfg!(target_os = "linux") {
        if let Ok(s) = std::fs::read_to_string("/proc/cpuinfo") {
            info.cpu = s
                .lines()
                .find_map(|l| l.strip_prefix("model name"))
                .and_then(|l| l.split(':').nth(1))
                .map(|v| v.trim().to_string())
                .unwrap_or_default();
        }
        if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
            info.memory_bytes = s
                .lines()
                .find_map(|l| l.strip_prefix("MemTotal:"))
                .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
                .map(|kb| kb * 1024)
                .unwrap_or(0);
        }
        info.os_version = std::fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                    .map(|v| v.trim_matches('"').to_string())
            })
            .unwrap_or_default();
    }
    if let Some(p) = attempt_bin {
        info.attempt_binary = p.display().to_string();
        info.attempt_binary_bytes = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    }
    info
}
