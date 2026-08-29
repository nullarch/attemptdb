//! Fault injection for crash-consistency and disk-full tests.
//!
//! Production code calls [`hit`] and [`io`] at the places where a crash or
//! a failed write would be most interesting. Both are no-ops unless a
//! failpoint is armed, and the cost of the disarmed path is one relaxed
//! atomic load per call: the environment is parsed once, lazily, on the
//! first call, and the result is cached in a process-wide flag.
//!
//! Two kinds of points exist:
//!
//! * **Abort points** ([`hit`]) end the process with
//!   [`std::process::abort`] when `ATTEMPTDB_FAILPOINT` equals the point's
//!   name, or `name:N` to abort on the N-th time the point is reached. They
//!   model a crash (SIGKILL, kernel panic as seen by the file system) at a
//!   precise step of a multi-step protocol. Nothing is flushed before the
//!   abort, exactly like a real crash; a line naming the point goes to
//!   stderr (unbuffered) first so test logs show where the process died.
//! * **I/O points** ([`io`]) return `ErrorKind::StorageFull` ("simulated
//!   ENOSPC") once when `ATTEMPTDB_FAILPOINT_IO` equals the point's name
//!   (or `name:N`, fail on the N-th call), and succeed on every other call.
//!   Tests running inside one process arm them with [`arm_io`] instead,
//!   which is thread-local so parallel tests cannot steal each other's
//!   fault.
//!
//! # Abort points
//!
//! | name | fires |
//! |---|---|
//! | `wal.append.after_write` | in `Wal::append`, after the records were written to the active WAL file, before any sync |
//! | `wal.append.after_sync` | in `Wal::sync`, after the fsync that precedes the ingest acknowledgement (also reached by `flush` and `import_spool`, which sync too) |
//! | `segment.after_tmp_write` | in `segment::write_segment`, after `seg-*.arrow.tmp` is fully written and fsynced, before the rename |
//! | `segment.after_rename` | in `segment::write_segment`, after the rename made the segment file visible (directory synced), before the manifest references it |
//! | `manifest.after_tmp_write` | in `Manifest::write`, after `gen-*.json.tmp` is written and fsynced, before the rename |
//! | `manifest.after_rename` | in `Manifest::write`, after the new generation is visible and the directory is synced |
//! | `flush.after_manifest_before_wal_truncate` | in `Database::flush`, after the new generation is durable and adopted in memory, before old WAL files are deleted |
//! | `wal.truncate.mid` | in `Wal::truncate_before`, after each individual WAL file deletion (so `name:2` crashes between the second and third deletion) |
//! | `spool.append.after_write` | in `SpoolWriter::append_with`, after the records were written to the inbox, before the optional sync and the committed-length sidecar |
//! | `spool.committed.before_write` | in `SpoolWriter::append_with`, after the optional sync, immediately before the committed-length sidecar is written |
//!
//! # I/O points
//!
//! | name | fires |
//! |---|---|
//! | `wal.write` | in `FrameWriter::append` for WAL files: half of the batch is written, then the write fails |
//! | `spool.write` | in `FrameWriter::append` for spool files, same shape |
//! | `segment.write` | in `segment::write_segment`: half of the temp file is written, then the write fails (a torn `.tmp` stays behind) |
//! | `manifest.write` | in `Manifest::write`, same shape |
//!
//! The I/O points deliberately leave a partial write behind, because that
//! is what a real `ENOSPC` does, and the interesting question is whether
//! the engine recovers from it rather than whether it propagates the error.

use std::cell::RefCell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

pub const WAL_APPEND_AFTER_WRITE: &str = "wal.append.after_write";
pub const WAL_APPEND_AFTER_SYNC: &str = "wal.append.after_sync";
pub const SEGMENT_AFTER_TMP_WRITE: &str = "segment.after_tmp_write";
pub const SEGMENT_AFTER_RENAME: &str = "segment.after_rename";
pub const MANIFEST_AFTER_TMP_WRITE: &str = "manifest.after_tmp_write";
pub const MANIFEST_AFTER_RENAME: &str = "manifest.after_rename";
pub const FLUSH_AFTER_MANIFEST_BEFORE_WAL_TRUNCATE: &str = "flush.after_manifest_before_wal_truncate";
pub const WAL_TRUNCATE_MID: &str = "wal.truncate.mid";
pub const SPOOL_APPEND_AFTER_WRITE: &str = "spool.append.after_write";
pub const SPOOL_COMMITTED_BEFORE_WRITE: &str = "spool.committed.before_write";

pub const WAL_WRITE: &str = "wal.write";
pub const SEGMENT_WRITE: &str = "segment.write";
pub const MANIFEST_WRITE: &str = "manifest.write";
pub const SPOOL_WRITE: &str = "spool.write";

/// Every abort point, in protocol order. Tests iterate over this list so
/// that a point added to the engine cannot be forgotten by the suite.
pub const ABORT_POINTS: &[&str] = &[
    WAL_APPEND_AFTER_WRITE,
    WAL_APPEND_AFTER_SYNC,
    SEGMENT_AFTER_TMP_WRITE,
    SEGMENT_AFTER_RENAME,
    MANIFEST_AFTER_TMP_WRITE,
    MANIFEST_AFTER_RENAME,
    FLUSH_AFTER_MANIFEST_BEFORE_WAL_TRUNCATE,
    WAL_TRUNCATE_MID,
    SPOOL_APPEND_AFTER_WRITE,
    SPOOL_COMMITTED_BEFORE_WRITE,
];

/// Every I/O point.
pub const IO_POINTS: &[&str] = &[WAL_WRITE, SEGMENT_WRITE, MANIFEST_WRITE, SPOOL_WRITE];

/// Environment variable naming the abort point (`name` or `name:N`).
pub const ENV_ABORT: &str = "ATTEMPTDB_FAILPOINT";
/// Environment variable naming the I/O point (`name` or `name:N`).
pub const ENV_IO: &str = "ATTEMPTDB_FAILPOINT_IO";

const UNINIT: u8 = 0;
const OFF: u8 = 1;
const ON: u8 = 2;

/// Fast-path switch. `OFF` means "nothing armed anywhere in this process",
/// which is the steady state in production.
static MODE: AtomicU8 = AtomicU8::new(UNINIT);
static ENV: OnceLock<EnvConfig> = OnceLock::new();

thread_local! {
    static IO_OVERRIDE: RefCell<Option<Point>> = const { RefCell::new(None) };
}

struct Point {
    name: String,
    nth: u64,
    hits: AtomicU64,
}

impl Point {
    fn parse(spec: &str) -> Option<Self> {
        let spec = spec.trim();
        if spec.is_empty() {
            return None;
        }
        let (name, nth) = match spec.rsplit_once(':') {
            Some((name, n)) => match n.parse::<u64>() {
                Ok(n) if n >= 1 => (name, n),
                _ => (spec, 1),
            },
            None => (spec, 1),
        };
        Some(Self { name: name.to_string(), nth, hits: AtomicU64::new(0) })
    }

    /// Count one arrival at `name`; true exactly when this is the N-th.
    fn arrive(&self, name: &str) -> bool {
        if self.name != name {
            return false;
        }
        self.hits.fetch_add(1, Ordering::Relaxed) + 1 == self.nth
    }
}

#[derive(Default)]
struct EnvConfig {
    abort: Option<Point>,
    io: Option<Point>,
}

fn env_config() -> &'static EnvConfig {
    ENV.get_or_init(|| {
        let cfg = EnvConfig {
            abort: std::env::var(ENV_ABORT).ok().and_then(|s| Point::parse(&s)),
            io: std::env::var(ENV_IO).ok().and_then(|s| Point::parse(&s)),
        };
        let armed = cfg.abort.is_some() || cfg.io.is_some();
        // A programmatic `arm_io` may already have switched the mode on;
        // never switch it back off.
        let _ = MODE.compare_exchange(UNINIT, if armed { ON } else { OFF }, Ordering::AcqRel, Ordering::Acquire);
        cfg
    })
}

/// Abort the process here if this point is armed. Free when nothing is.
#[inline]
pub fn hit(name: &'static str) {
    if MODE.load(Ordering::Relaxed) == OFF {
        return;
    }
    hit_slow(name);
}

#[cold]
#[inline(never)]
fn hit_slow(name: &'static str) {
    if let Some(p) = &env_config().abort
        && p.arrive(name)
    {
        eprintln!("attemptdb failpoint: aborting at `{name}` (hit {})", p.nth);
        std::process::abort();
    }
}

/// Return a simulated `ENOSPC` if this point is armed (once), else `Ok`.
#[inline]
pub fn io(name: &'static str) -> std::io::Result<()> {
    if MODE.load(Ordering::Relaxed) == OFF {
        return Ok(());
    }
    io_slow(name)
}

#[cold]
#[inline(never)]
fn io_slow(name: &'static str) -> std::io::Result<()> {
    if let Some(p) = &env_config().io
        && p.arrive(name)
    {
        return Err(enospc(name));
    }
    let fired = IO_OVERRIDE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let fired = slot.as_ref().is_some_and(|p| p.arrive(name));
        if fired {
            *slot = None;
        }
        fired
    });
    if fired { Err(enospc(name)) } else { Ok(()) }
}

fn enospc(name: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::StorageFull,
        format!("simulated ENOSPC at failpoint `{name}`"),
    )
}

/// Arm an I/O point for the calling thread only (`name` or `name:N`).
/// It fires once and disarms itself. Intended for in-process tests; the
/// environment variable is the way to arm a child process.
pub fn arm_io(spec: &str) {
    let point = Point::parse(spec);
    IO_OVERRIDE.with(|slot| *slot.borrow_mut() = point);
    // Make sure the environment has been read so the mode is settled, then
    // force the slow path on for everyone (cheap; only tests get here).
    let _ = env_config();
    MODE.store(ON, Ordering::Release);
}

/// Remove a thread-local I/O point armed with [`arm_io`] that has not fired.
pub fn disarm_io() {
    IO_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
}

/// Whether a thread-local I/O point is still armed (has not fired yet).
pub fn io_armed() -> bool {
    IO_OVERRIDE.with(|slot| slot.borrow().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_name_and_nth() {
        let p = Point::parse("wal.write").unwrap();
        assert_eq!((p.name.as_str(), p.nth), ("wal.write", 1));
        let p = Point::parse("wal.write:3").unwrap();
        assert_eq!((p.name.as_str(), p.nth), ("wal.write", 3));
        // A non-numeric suffix is part of the name, a zero count means 1.
        let p = Point::parse("odd:name").unwrap();
        assert_eq!((p.name.as_str(), p.nth), ("odd:name", 1));
        let p = Point::parse("x:0").unwrap();
        assert_eq!((p.name.as_str(), p.nth), ("x:0", 1));
        assert!(Point::parse("  ").is_none());
    }

    #[test]
    fn io_point_fires_once_on_nth_call_and_is_thread_local() {
        arm_io("segment.write:2");
        assert!(io_armed());
        assert!(io(SEGMENT_WRITE).is_ok());
        assert!(io(WAL_WRITE).is_ok(), "other names are untouched");
        let err = io(SEGMENT_WRITE).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::StorageFull);
        assert!(!io_armed());
        assert!(io(SEGMENT_WRITE).is_ok(), "fires only once");
        // Another thread never sees this thread's arming.
        arm_io("manifest.write");
        let other = std::thread::spawn(|| io(MANIFEST_WRITE).is_ok()).join().unwrap();
        assert!(other);
        assert!(io(MANIFEST_WRITE).is_err());
        disarm_io();
    }

    #[test]
    fn hit_is_a_no_op_without_env() {
        // The unit test process never sets the variable; every point must
        // simply return.
        for name in ABORT_POINTS {
            hit_slow(name);
        }
        for name in IO_POINTS {
            assert!(io_slow(name).is_ok(), "{name}");
        }
    }
}
