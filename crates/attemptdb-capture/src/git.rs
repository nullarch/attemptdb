//! Zero-subprocess git repository introspection.
//!
//! This runs inside every hook invocation, so it must stay well under a
//! millisecond: it only stats a handful of paths and reads a few tiny files.
//! No `git` binary is ever spawned. Every failure degrades to `None` fields.

use std::path::{Path, PathBuf};

/// Snapshot of the repository that contains a working directory.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct GitInfo {
    /// Working-tree root (the directory containing `.git`).
    pub root: PathBuf,
    /// `origin` URL (or the first remote when `origin` is absent).
    pub remote: Option<String>,
    /// Current branch name when `HEAD` is a `refs/heads/*` symref.
    pub branch: Option<String>,
    /// Commit hash `HEAD` resolves to, when resolvable.
    pub head: Option<String>,
}

/// How many parent directories to inspect before giving up.
const MAX_DEPTH: usize = 64;
/// Guard against symref loops (`ref: refs/heads/a` -> `ref: refs/heads/b` -> ...).
const MAX_SYMREF_DEPTH: usize = 8;

/// Locate the repository containing `cwd` and read its basic state.
pub fn git_info(cwd: &Path) -> Option<GitInfo> {
    let (root, git_dir) = find_git_dir(cwd)?;
    let common = common_dir(&git_dir);
    let (branch, head) = read_head(&git_dir, &common);
    let remote = std::fs::read_to_string(common.join("config"))
        .ok()
        .and_then(|text| parse_remote_url(&text, "origin"));
    Some(GitInfo {
        root,
        remote,
        branch,
        head,
    })
}

/// Walk up from `start` until a `.git` directory or gitdir-pointer file is
/// found. Returns `(worktree_root, git_dir)`.
fn find_git_dir(start: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut dir = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(start)
    };
    if dir.is_file() {
        dir.pop();
    }
    for _ in 0..MAX_DEPTH {
        let dot_git = dir.join(".git");
        match std::fs::metadata(&dot_git) {
            Ok(meta) if meta.is_dir() => return Some((dir, dot_git)),
            Ok(meta) if meta.is_file() => {
                // Worktree or submodule: `.git` is a file containing `gitdir: <path>`.
                let content = std::fs::read_to_string(&dot_git).ok()?;
                let git_dir = parse_gitdir_pointer(&content, &dir)?;
                return Some((dir, git_dir));
            }
            _ => {}
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// Parse the contents of a `.git` *file*.
fn parse_gitdir_pointer(content: &str, base: &Path) -> Option<PathBuf> {
    let line = content
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("gitdir:"))?;
    let target = line["gitdir:".len()..].trim();
    if target.is_empty() {
        return None;
    }
    let path = Path::new(target);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    })
}

/// Resolve the "common" git dir. Worktree git dirs contain a `commondir`
/// file pointing (usually relatively) at the main repository's `.git`, which
/// is where refs, packed-refs and config live.
fn common_dir(git_dir: &Path) -> PathBuf {
    match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(text) => {
            let target = text.trim();
            if target.is_empty() {
                git_dir.to_path_buf()
            } else if Path::new(target).is_absolute() {
                PathBuf::from(target)
            } else {
                git_dir.join(target)
            }
        }
        Err(_) => git_dir.to_path_buf(),
    }
}

/// Read `HEAD` and resolve it. Returns `(branch, commit)`.
fn read_head(git_dir: &Path, common: &Path) -> (Option<String>, Option<String>) {
    let Ok(text) = std::fs::read_to_string(git_dir.join("HEAD")) else {
        return (None, None);
    };
    let head = text.trim();
    if let Some(refname) = head.strip_prefix("ref:") {
        let refname = refname.trim();
        let branch = refname.strip_prefix("refs/heads/").map(str::to_string);
        let sha = resolve_ref(common, git_dir, refname, 0);
        (branch, sha)
    } else if is_hex_oid(head) {
        (None, Some(head.to_string()))
    } else {
        (None, None)
    }
}

/// Resolve a ref name to an object id via loose refs, then `packed-refs`.
fn resolve_ref(common: &Path, git_dir: &Path, refname: &str, depth: usize) -> Option<String> {
    if depth > MAX_SYMREF_DEPTH || refname.is_empty() || refname.contains("..") {
        return None;
    }
    // Loose ref: per-worktree refs (e.g. refs/bisect) live in git_dir, shared
    // refs in the common dir. Try both; the common dir is the usual hit.
    for base in [common, git_dir] {
        if let Ok(text) = std::fs::read_to_string(base.join(refname)) {
            let value = text.trim();
            if let Some(target) = value.strip_prefix("ref:") {
                return resolve_ref(common, git_dir, target.trim(), depth + 1);
            }
            if is_hex_oid(value) {
                return Some(value.to_string());
            }
            return None;
        }
        if base == git_dir {
            break;
        }
    }
    let packed = std::fs::read_to_string(common.join("packed-refs")).ok()?;
    packed
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('^'))
        .find_map(|line| {
            let (sha, name) = line.split_once(' ')?;
            (name.trim() == refname && is_hex_oid(sha)).then(|| sha.to_string())
        })
}

fn is_hex_oid(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Extract `url` from `[remote "<name>"]` in a git config file. Falls back to
/// the first remote's URL when the named remote is absent.
pub fn parse_remote_url(config: &str, name: &str) -> Option<String> {
    let mut current_remote: Option<String> = None;
    let mut wanted: Option<String> = None;
    let mut first: Option<String> = None;
    for raw in config.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            current_remote = parse_remote_header(line);
            continue;
        }
        let Some(remote) = &current_remote else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "url" {
            continue;
        }
        let value = unquote_config_value(value.trim());
        if value.is_empty() {
            continue;
        }
        if remote == name && wanted.is_none() {
            wanted = Some(value.clone());
        }
        if first.is_none() {
            first = Some(value);
        }
    }
    wanted.or(first)
}

/// `[remote "origin"]` -> `Some("origin")`; any other section -> `None`.
fn parse_remote_header(line: &str) -> Option<String> {
    let inner = line.strip_prefix('[')?.split(']').next()?.trim();
    let rest = inner.strip_prefix("remote")?.trim_start();
    let quoted = rest.strip_prefix('"')?;
    let end = quoted.find('"')?;
    Some(quoted[..end].to_string())
}

fn unquote_config_value(value: &str) -> String {
    let v = value.trim();
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        v[1..v.len() - 1].replace("\\\"", "\"")
    } else {
        v.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const SHA2: &str = "fedcba9876543210fedcba9876543210fedcba98";

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn make_repo(root: &Path) {
        write(&root.join(".git/HEAD"), "ref: refs/heads/main\n");
        write(&root.join(".git/refs/heads/main"), &format!("{SHA}\n"));
        write(
            &root.join(".git/config"),
            "[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n\turl = git@github.com:acme/widgets.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n[branch \"main\"]\n\tremote = origin\n",
        );
    }

    #[test]
    fn loose_ref_repo() {
        let tmp = tempfile::tempdir().unwrap();
        make_repo(tmp.path());
        let info = git_info(tmp.path()).expect("repo detected");
        assert_eq!(info.root, tmp.path());
        assert_eq!(info.branch.as_deref(), Some("main"));
        assert_eq!(info.head.as_deref(), Some(SHA));
        assert_eq!(
            info.remote.as_deref(),
            Some("git@github.com:acme/widgets.git")
        );
    }

    #[test]
    fn found_from_nested_directory() {
        let tmp = tempfile::tempdir().unwrap();
        make_repo(tmp.path());
        let nested = tmp.path().join("src/deep/er");
        fs::create_dir_all(&nested).unwrap();
        let info = git_info(&nested).unwrap();
        assert_eq!(info.root, tmp.path());
    }

    #[test]
    fn packed_refs_case() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join(".git/HEAD"), "ref: refs/heads/feature/x\n");
        write(
            &tmp.path().join(".git/packed-refs"),
            &format!(
                "# pack-refs with: peeled fully-peeled sorted \n{SHA2} refs/heads/feature/x\n^{SHA}\n{SHA} refs/tags/v1\n"
            ),
        );
        write(
            &tmp.path().join(".git/config"),
            "[remote \"upstream\"]\n\turl = https://example.com/r.git\n",
        );
        let info = git_info(tmp.path()).unwrap();
        assert_eq!(info.branch.as_deref(), Some("feature/x"));
        assert_eq!(info.head.as_deref(), Some(SHA2));
        // No origin: falls back to the first remote.
        assert_eq!(info.remote.as_deref(), Some("https://example.com/r.git"));
    }

    #[test]
    fn worktree_gitdir_file() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        make_repo(&main);
        write(
            &main.join(".git/refs/heads/wt-branch"),
            &format!("{SHA2}\n"),
        );
        let wt = tmp.path().join("wt");
        let wt_gitdir = main.join(".git/worktrees/wt");
        write(&wt_gitdir.join("HEAD"), "ref: refs/heads/wt-branch\n");
        write(&wt_gitdir.join("commondir"), "../..\n");
        write(
            &wt_gitdir.join("gitdir"),
            &format!("{}/.git\n", wt.display()),
        );
        write(
            &wt.join(".git"),
            &format!("gitdir: {}\n", wt_gitdir.display()),
        );
        let info = git_info(&wt).unwrap();
        assert_eq!(info.root, wt);
        assert_eq!(info.branch.as_deref(), Some("wt-branch"));
        assert_eq!(info.head.as_deref(), Some(SHA2));
        assert_eq!(
            info.remote.as_deref(),
            Some("git@github.com:acme/widgets.git")
        );
    }

    #[test]
    fn relative_gitdir_pointer_and_detached_head() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("sub");
        fs::create_dir_all(&sub).unwrap();
        write(
            &tmp.path().join(".git/modules/sub/HEAD"),
            &format!("{SHA}\n"),
        );
        write(&sub.join(".git"), "gitdir: ../.git/modules/sub\n");
        let info = git_info(&sub).unwrap();
        assert_eq!(info.root, sub);
        assert_eq!(info.branch, None);
        assert_eq!(info.head.as_deref(), Some(SHA));
        assert_eq!(info.remote, None);
    }

    #[test]
    fn garbled_files_degrade_gracefully() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join(".git/HEAD"), "this is not a head\n");
        write(&tmp.path().join(".git/config"), "[remote \"origin\"\nurl\n");
        let info = git_info(tmp.path()).unwrap();
        assert_eq!(info.branch, None);
        assert_eq!(info.head, None);
        assert_eq!(info.remote, None);

        write(&tmp.path().join(".git/HEAD"), "ref: refs/heads/missing\n");
        let info = git_info(tmp.path()).unwrap();
        assert_eq!(info.branch.as_deref(), Some("missing"));
        assert_eq!(info.head, None);

        let empty = tempfile::tempdir().unwrap();
        // A temp dir has no repository above it in CI; if the host has one we
        // still must not panic.
        let _ = git_info(empty.path());
    }

    #[test]
    fn remote_parsing_edge_cases() {
        let cfg = "[remote \"origin\"]\r\n\turl = \"https://x.example/a b.git\"\r\n[remote \"other\"]\r\n\turl = ssh://y\r\n";
        assert_eq!(
            parse_remote_url(cfg, "origin").as_deref(),
            Some("https://x.example/a b.git")
        );
        assert_eq!(parse_remote_url(cfg, "other").as_deref(), Some("ssh://y"));
        assert_eq!(
            parse_remote_url(cfg, "nope").as_deref(),
            Some("https://x.example/a b.git")
        );
        assert_eq!(parse_remote_url("[core]\n\tbare = false\n", "origin"), None);
    }

    #[test]
    fn real_repo_lookup_is_fast() {
        // The workspace itself is a git checkout in development; in a tarball
        // build this is simply skipped.
        let here = std::env::current_dir().unwrap();
        let Some(info) = git_info(&here) else {
            return;
        };
        assert!(here.starts_with(&info.root));
        let start = std::time::Instant::now();
        for _ in 0..200 {
            let _ = git_info(&here);
        }
        let per_call = start.elapsed() / 200;
        eprintln!(
            "git_info: {per_call:?} per call, root={} branch={:?} head={:?} remote={:?}",
            info.root.display(),
            info.branch,
            info.head,
            info.remote
        );
        assert!(
            per_call < std::time::Duration::from_millis(5),
            "git_info too slow: {per_call:?}"
        );
    }
}
