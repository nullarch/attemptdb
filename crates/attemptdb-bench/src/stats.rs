//! Timing, percentiles, process memory, and disk usage helpers.

use serde::Serialize;
use std::path::Path;
use std::time::{Duration, Instant};

/// Percentile summary of a latency sample, in microseconds.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Summary {
    pub n: usize,
    pub min_us: f64,
    pub p50_us: f64,
    pub p95_us: f64,
    pub p99_us: f64,
    pub max_us: f64,
    pub mean_us: f64,
}

impl Summary {
    pub fn of_micros(v: &mut [f64]) -> Self {
        if v.is_empty() {
            return Self::default();
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pct = |p: f64| {
            let i = ((p * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1;
            v[i]
        };
        Self {
            n: v.len(),
            min_us: v[0],
            p50_us: pct(0.50),
            p95_us: pct(0.95),
            p99_us: pct(0.99),
            max_us: v[v.len() - 1],
            mean_us: v.iter().sum::<f64>() / v.len() as f64,
        }
    }
}

pub struct Stopwatch(Instant);

impl Stopwatch {
    pub fn start() -> Self {
        Self(Instant::now())
    }

    pub fn elapsed(&self) -> Duration {
        self.0.elapsed()
    }

    pub fn secs(&self) -> f64 {
        self.0.elapsed().as_secs_f64()
    }
}

/// Peak resident set size of this process in bytes, or `None` where the
/// platform offers no cheap way to ask.
///
/// macOS reports `ru_maxrss` in bytes, Linux in kilobytes; on Linux
/// `/proc/self/status` (`VmHWM`) is preferred because it is unambiguous.
/// Windows has neither and would need `GetProcessMemoryInfo`, which is not
/// worth a dependency for a benchmark field: the report shows a blank there
/// rather than a zero that would read as a measurement.
pub fn peak_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
            for line in s.lines() {
                if let Some(rest) = line.strip_prefix("VmHWM:") {
                    let kb: u64 = rest
                        .trim()
                        .trim_end_matches("kB")
                        .trim()
                        .parse()
                        .unwrap_or(0);
                    return Some(kb * 1024);
                }
            }
        }
    }
    platform_maxrss()
}

#[cfg(unix)]
fn platform_maxrss() -> Option<u64> {
    // SAFETY: getrusage writes into the zeroed struct we hand it; RUSAGE_SELF
    // is always valid for the calling process.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    if rc != 0 {
        return None;
    }
    let raw = usage.ru_maxrss as u64;
    Some(if cfg!(target_os = "macos") {
        raw
    } else {
        raw * 1024
    })
}

#[cfg(not(unix))]
fn platform_maxrss() -> Option<u64> {
    None
}

/// Total size in bytes of every regular file under `path`.
pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            total += dir_size(&p);
        } else if let Ok(m) = entry.metadata() {
            total += m.len();
        }
    }
    total
}

/// Bytes of a database's segment and WAL directories.
#[derive(Clone, Debug, Default, Serialize)]
pub struct DiskUsage {
    pub segments_bytes: u64,
    pub segment_files: u64,
    pub wal_bytes: u64,
    pub manifest_bytes: u64,
    pub total_bytes: u64,
}

pub fn disk_usage(db_root: &Path) -> DiskUsage {
    let seg = db_root.join("segments");
    let files = std::fs::read_dir(&seg)
        .map(|d| d.flatten().count() as u64)
        .unwrap_or(0);
    DiskUsage {
        segments_bytes: dir_size(&seg),
        segment_files: files,
        wal_bytes: dir_size(&db_root.join("wal")),
        manifest_bytes: dir_size(&db_root.join("manifest")),
        total_bytes: dir_size(db_root),
    }
}

pub fn human_bytes(b: f64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0} {}", UNITS[i])
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles() {
        let mut v: Vec<f64> = (1..=100).map(|i| i as f64).collect();
        let s = Summary::of_micros(&mut v);
        assert_eq!(s.n, 100);
        assert_eq!(s.p50_us, 50.0);
        assert_eq!(s.p95_us, 95.0);
        assert_eq!(s.p99_us, 99.0);
        assert_eq!(s.max_us, 100.0);
        assert!(Summary::of_micros(&mut []).n == 0);
    }

    #[test]
    fn rss_is_positive() {
        #[cfg(unix)]
        assert!(peak_rss_bytes().unwrap() > 1024 * 1024);
    }
}
