//! Symlink-safe file creation and opening for the files other processes can
//! influence: spool files (written by hooks, living in a directory a
//! repository can pre-populate), WAL files, and their sidecars.
//!
//! The threat is a spool directory that contains a planted symbolic link
//! (`inbox.spool.committed.tmp -> ~/important`). `std::fs::write` and
//! `OpenOptions::create` follow it, and `FrameWriter` used to `set_len(0)` a
//! small file it opened, so an attacker-controlled directory could truncate
//! or overwrite any file the user can write.
//!
//! The rules here:
//!
//! - a file that is supposed to be new is created with `create_new`
//!   (`O_CREAT | O_EXCL`), which fails on any existing entry, including a
//!   dangling symlink; a stale entry is unlinked first (unlinking removes the
//!   link, never its target);
//! - an existing file is opened with `O_NOFOLLOW` (unix) so a symlink in the
//!   final path component fails instead of being followed, and is refused
//!   unless it is a regular file (no FIFOs, devices, directories). `O_NONBLOCK`
//!   keeps a planted FIFO from blocking a hook;
//! - on Windows the final component is checked with `symlink_metadata`
//!   (reparse points are refused). That check is not atomic with the open:
//!   `FILE_FLAG_OPEN_REPARSE_POINT` would close the gap but cannot be
//!   exercised here, so it is left as a documented limitation.
//!
//! Directory components of the path are not checked: the database root and
//! its `spool/` directory are created by this crate, and a link planted
//! *above* them is outside what a file-level check can defend.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

fn hardened(opts: &mut OpenOptions) -> &mut OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    opts
}

fn refuse(path: &Path, what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "refusing to use {}: {what}, not a regular file we created",
            path.display()
        ),
    )
}

/// Translate the OS error for "the final component is a symlink" (`ELOOP`)
/// into something a person can act on.
fn explain(path: &Path, e: io::Error) -> io::Error {
    #[cfg(unix)]
    if e.raw_os_error() == Some(libc::ELOOP) {
        return refuse(path, "it is a symbolic link");
    }
    let _ = path;
    e
}

/// Fail unless the open handle is a regular file.
fn require_regular(path: &Path, file: &File) -> io::Result<()> {
    if file.metadata()?.is_file() {
        Ok(())
    } else {
        Err(refuse(path, "it is not a regular file"))
    }
}

/// Windows: refuse a reparse point (symlink or junction) at `path`.
#[cfg(not(unix))]
fn refuse_reparse_point(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => Err(refuse(path, "it is a symbolic link")),
        Ok(m) if !m.is_file() => Err(refuse(path, "it is not a regular file")),
        _ => Ok(()),
    }
}

/// Open `path` for reading and writing without truncating, creating it
/// (`create_new`) when it does not exist. An existing entry must be a regular
/// file and not a symlink.
pub(crate) fn open_rw(path: &Path) -> io::Result<File> {
    let mut create = OpenOptions::new();
    create.read(true).write(true).create_new(true);
    match hardened(&mut create).open(path) {
        Ok(f) => return Ok(f),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(explain(path, e)),
    }
    // `create_new` reports an existing entry of any kind, a dangling symlink
    // included; the plain open below follows nothing and checks the type.
    #[cfg(not(unix))]
    refuse_reparse_point(path)?;
    let mut existing = OpenOptions::new();
    existing.read(true).write(true);
    let file = hardened(&mut existing)
        .open(path)
        .map_err(|e| explain(path, e))?;
    require_regular(path, &file)?;
    Ok(file)
}

/// Open an existing regular file for writing without truncating or creating
/// it (a missing file is an error). Used to fsync a file written earlier.
pub(crate) fn open_existing_rw(path: &Path) -> io::Result<File> {
    #[cfg(not(unix))]
    refuse_reparse_point(path)?;
    let mut opts = OpenOptions::new();
    opts.write(true);
    let file = hardened(&mut opts)
        .open(path)
        .map_err(|e| explain(path, e))?;
    require_regular(path, &file)?;
    Ok(file)
}

/// Create a file that must not exist yet. A stale plain file or symlink at
/// `path` is unlinked first (the link, not its target); a directory is
/// refused. Then `create_new` guarantees the file we write is ours.
pub(crate) fn create_new_replacing(path: &Path) -> io::Result<File> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => return Err(refuse(path, "it is a directory")),
        Ok(_) => match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut create = OpenOptions::new();
    create.write(true).create_new(true);
    hardened(&mut create)
        .open(path)
        .map_err(|e| explain(path, e))
}

/// Open an existing regular file read-only, never following a symlink in the
/// final component and never blocking on a FIFO.
pub(crate) fn open_read(path: &Path) -> io::Result<File> {
    #[cfg(not(unix))]
    refuse_reparse_point(path)?;
    let mut opts = OpenOptions::new();
    opts.read(true);
    let file = hardened(&mut opts)
        .open(path)
        .map_err(|e| explain(path, e))?;
    require_regular(path, &file)?;
    Ok(file)
}

/// Open (creating if needed) an advisory lock file. Not truncated, and a
/// symlink in the final component is refused instead of followed, so a
/// planted link cannot make the lock open create or lock a file elsewhere.
pub(crate) fn open_lock(path: &Path) -> io::Result<File> {
    #[cfg(not(unix))]
    refuse_reparse_point(path)?;
    let mut opts = OpenOptions::new();
    opts.create(true).write(true).truncate(false);
    let file = hardened(&mut opts)
        .open(path)
        .map_err(|e| explain(path, e))?;
    require_regular(path, &file)?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn open_rw_refuses_a_symlink_and_leaves_the_target_alone() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        std::fs::write(&victim, b"precious").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        assert!(open_rw(&link).is_err());
        assert!(create_new_replacing(&link).is_ok());
        assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
        // The link itself was replaced by a regular file.
        assert!(std::fs::symlink_metadata(&link).unwrap().is_file());
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_is_not_created_through() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("does-not-exist");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(open_rw(&link).is_err());
        assert!(open_lock(&link).is_err());
        assert!(!target.exists());
    }

    #[test]
    fn regular_files_and_new_files_work() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        {
            use std::io::Write;
            let mut f = open_rw(&p).unwrap();
            f.write_all(b"abc").unwrap();
        }
        let again = open_rw(&p).unwrap();
        assert_eq!(again.metadata().unwrap().len(), 3);
        assert!(open_read(&p).is_ok());
        assert!(open_lock(&dir.path().join("lock")).is_ok());
        assert!(open_read(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn a_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        assert!(open_rw(&sub).is_err());
        assert!(create_new_replacing(&sub).is_err());
        assert!(open_read(&sub).is_err());
    }
}
