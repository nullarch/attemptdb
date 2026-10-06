//! Small file-system helpers shared by the server's state files (`keys.json`,
//! `pairings.json`, webhook cursors, stored inference documents).
//!
//! Every one of those files is replaced, never edited in place, and more than
//! one task can replace the same file: a fixed `<name>.tmp` shared by two
//! writers lets one rename the other's half-written bytes into place. A write
//! here goes to a temp name that is unique to this process and call, is
//! flushed to disk, and only then renamed over the target, so a reader (or a
//! restart after a crash) sees the old file or the whole new one.

use anyhow::{Context, Result};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Replace `path` with `bytes` atomically and durably. `private` makes the
/// file mode 0600 from creation (Unix). The parent directory is created.
pub fn write_atomic(path: &Path, bytes: &[u8], private: bool) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        #[cfg(not(unix))]
        let _ = private;
        let mut f = options
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("syncing {}", tmp.display()))?;
        drop(f);
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        // The rename itself must survive a power cut: sync the directory.
        #[cfg(unix)]
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty())
            && let Ok(d) = std::fs::File::open(dir)
        {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_writers_never_leave_a_torn_file_or_a_temp_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for n in 0..50 {
                        let body = format!(
                            "{{\"writer\":{i},\"n\":{n},\"pad\":\"{}\"}}",
                            "x".repeat(4096)
                        );
                        write_atomic(&path, body.as_bytes(), true).unwrap();
                        let seen = std::fs::read_to_string(&path).unwrap();
                        let v: serde_json::Value = serde_json::from_str(&seen)
                            .expect("a whole document, never a torn one");
                        assert!(v["writer"].is_u64());
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "state.json")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
