//! Portable path representation.
//!
//! A path observed on one operating system must remain meaningful when the
//! database is opened on another. We therefore never persist a bare native
//! path; we persist the original text plus a normalised logical form and, when
//! known, the repository-relative form.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct PortablePath {
    /// The path exactly as the provider reported it (UTF-8; lossy if needed).
    pub original: String,
    /// Forward-slash normalised logical path. Windows drive letters are kept
    /// as `C:/...`; UNC prefixes are preserved as `//server/share/...`.
    pub logical: String,
    /// Path relative to the project root, when the path lies inside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_relative: Option<String>,
    /// Windows drive letter (`C`) when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drive: Option<String>,
    /// True when the original path was a UNC path.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unc: bool,
}

/// Replace a leading home directory with `~`, deterministically and without
/// consulting the environment: `/Users/<name>/…`, `/home/<name>/…` and
/// `<D>:/Users/<name>/…` all become `~/…`. This is the form RFC 0006 §4.2
/// requires for paths that appear in `attrs`; `Event.paths` keeps the
/// original. A path that is exactly the home directory becomes `~`.
pub fn elide_home(logical: &str) -> String {
    let unix_prefixes = ["/Users/", "/home/"];
    for prefix in unix_prefixes {
        if let Some(rest) = logical.strip_prefix(prefix) {
            return match rest.split_once('/') {
                Some((_name, tail)) => format!("~/{tail}"),
                None => "~".to_string(),
            };
        }
    }
    // `C:/Users/<name>/…` (separators already normalised to `/`).
    let b = logical.as_bytes();
    if b.len() >= 9
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && logical[2..].starts_with("/Users/")
    {
        return match logical[9..].split_once('/') {
            Some((_name, tail)) => format!("~/{tail}"),
            None => "~".to_string(),
        };
    }
    logical.to_string()
}

impl PortablePath {
    /// Build from raw text (as provided by a hook payload) and an optional
    /// project root used for the repository-relative form.
    pub fn from_raw(raw: &str, project_root: Option<&str>) -> Self {
        let trimmed = raw.trim();
        let (unc, stripped) = if let Some(rest) = trimmed.strip_prefix("\\\\?\\UNC\\") {
            (true, format!("//{}", rest))
        } else if let Some(rest) = trimmed.strip_prefix("\\\\?\\") {
            (false, rest.to_string())
        } else if trimmed.starts_with("\\\\") {
            (true, trimmed.to_string())
        } else {
            (false, trimmed.to_string())
        };
        let mut logical = stripped.replace('\\', "/");
        // Collapse duplicate slashes except a leading `//` (UNC).
        let leading_unc = logical.starts_with("//");
        while logical.contains("///") {
            logical = logical.replace("///", "//");
        }
        if !leading_unc {
            // Keep the first character verbatim (it may be a root `/` or a
            // multibyte character) and collapse `//` runs in the rest. The
            // split must land on a char boundary: slicing at byte 1 panics
            // on a leading Korean/accented/emoji character.
            let split = logical.chars().next().map_or(0, char::len_utf8);
            let (head, tail) = logical.split_at(split);
            if tail.contains("//") {
                let mut collapsed = tail.to_string();
                while collapsed.contains("//") {
                    collapsed = collapsed.replace("//", "/");
                }
                logical = format!("{head}{collapsed}");
            }
        }
        let drive = drive_letter(&logical).map(|c| c.to_ascii_uppercase().to_string());
        if let Some(d) = &drive {
            // Normalise drive letter case: `c:/x` -> `C:/x`.
            logical.replace_range(0..1, d);
        }
        let repo_relative = project_root.and_then(|root| {
            let root = normalise_root(root);
            let candidate = if is_relative(&logical) {
                Some(logical.clone())
            } else {
                strip_root(&logical, &root)
            };
            candidate.filter(|s| !s.is_empty())
        });
        Self {
            original: raw.to_string(),
            logical,
            repo_relative,
            drive,
            unc,
        }
    }

    pub fn from_path(p: &Path, project_root: Option<&str>) -> Self {
        Self::from_raw(&p.to_string_lossy(), project_root)
    }

    /// File extension (without the dot) of the logical path, lowercased.
    pub fn extension(&self) -> Option<String> {
        let name = self.logical.rsplit('/').next()?;
        let (stem, ext) = name.rsplit_once('.')?;
        if stem.is_empty() {
            return None;
        }
        Some(ext.to_ascii_lowercase())
    }

    /// Best display form: repository-relative when available.
    pub fn display(&self) -> &str {
        self.repo_relative.as_deref().unwrap_or(&self.logical)
    }
}

fn drive_letter(s: &str) -> Option<char> {
    let mut chars = s.chars();
    let c = chars.next()?;
    if c.is_ascii_alphabetic() && chars.next() == Some(':') {
        Some(c)
    } else {
        None
    }
}

fn is_relative(s: &str) -> bool {
    !(s.starts_with('/') || drive_letter(s).is_some())
}

fn normalise_root(root: &str) -> String {
    let mut r = root.trim().replace('\\', "/");
    if let Some(d) = drive_letter(&r) {
        r.replace_range(0..1, &d.to_ascii_uppercase().to_string());
    }
    while r.len() > 1 && r.ends_with('/') {
        r.pop();
    }
    r
}

fn strip_root(logical: &str, root: &str) -> Option<String> {
    if root.is_empty() {
        return None;
    }
    let rest = logical.strip_prefix(root)?;
    if rest.is_empty() {
        return Some(String::new());
    }
    rest.strip_prefix('/').map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_path_inside_root() {
        let p = PortablePath::from_raw("/Users/me/proj/src/main.rs", Some("/Users/me/proj"));
        assert_eq!(p.logical, "/Users/me/proj/src/main.rs");
        assert_eq!(p.repo_relative.as_deref(), Some("src/main.rs"));
        assert_eq!(p.extension().as_deref(), Some("rs"));
        assert!(!p.unc);
    }

    #[test]
    fn windows_path_normalised() {
        let p = PortablePath::from_raw(
            "c:\\Users\\me\\proj\\src\\main.rs",
            Some("C:\\Users\\me\\proj"),
        );
        assert_eq!(p.logical, "C:/Users/me/proj/src/main.rs");
        assert_eq!(p.drive.as_deref(), Some("C"));
        assert_eq!(p.repo_relative.as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn unc_and_extended_paths() {
        let p = PortablePath::from_raw("\\\\?\\UNC\\server\\share\\a.txt", None);
        assert!(p.unc);
        assert_eq!(p.logical, "//server/share/a.txt");
        let q = PortablePath::from_raw("\\\\?\\D:\\x\\y.md", None);
        assert_eq!(q.logical, "D:/x/y.md");
        assert!(!q.unc);
    }

    #[test]
    fn non_ascii_paths_survive() {
        let p = PortablePath::from_raw("/tmp/한글 폴더/emoji 🚀/파일.ts", Some("/tmp/한글 폴더"));
        assert_eq!(p.repo_relative.as_deref(), Some("emoji 🚀/파일.ts"));
        assert_eq!(p.extension().as_deref(), Some("ts"));
    }

    #[test]
    fn leading_multibyte_character_does_not_panic() {
        // Regression: slicing at byte 1 panicked inside the hook on a path
        // whose first character is not ASCII, dropping the event.
        let cases = [
            "문서/기획.md",
            "é.md",
            "🚀/launch.md",
            "𝒳/four-byte.md",
            "한",
            "é",
            "🚀",
            "한글",
            "문서//기획//a.md",
            "é//x",
        ];
        for raw in cases {
            let p = PortablePath::from_raw(raw, Some("/tmp/proj"));
            assert!(!p.logical.is_empty(), "{raw}");
            assert_eq!(p.original, raw);
        }
        let p = PortablePath::from_raw("문서/기획.md", Some("/tmp/proj"));
        assert_eq!(p.logical, "문서/기획.md");
        assert_eq!(p.repo_relative.as_deref(), Some("문서/기획.md"));
        assert_eq!(p.extension().as_deref(), Some("md"));
        // Duplicate slashes after a multibyte head still collapse.
        assert_eq!(
            PortablePath::from_raw("문서//기획//a.md", None).logical,
            "문서/기획/a.md"
        );
        assert_eq!(PortablePath::from_raw("é//x", None).logical, "é/x");
        assert_eq!(PortablePath::from_raw("한", None).logical, "한");
    }

    #[test]
    fn edge_shaped_paths_keep_their_documented_form() {
        assert_eq!(PortablePath::from_raw("", None).logical, "");
        assert_eq!(PortablePath::from_raw("", Some("/r")).repo_relative, None);
        let unc = PortablePath::from_raw("//server/share/a.txt", None);
        assert_eq!(unc.logical, "//server/share/a.txt");
        let drive = PortablePath::from_raw("c:\\x", None);
        assert_eq!(drive.logical, "C:/x");
        assert_eq!(drive.drive.as_deref(), Some("C"));
        assert_eq!(PortablePath::from_raw("/a//b///c", None).logical, "/a/b/c");
        assert_eq!(PortablePath::from_raw("a//b", None).logical, "a/b");
        assert_eq!(PortablePath::from_raw("/", None).logical, "/");
        assert_eq!(PortablePath::from_raw("//", None).logical, "//");
    }

    #[test]
    fn generated_strings_never_panic() {
        // A deterministic pseudo-random walk over an alphabet that mixes
        // ASCII, separators, drive-letter shapes and 1- to 4-byte characters.
        let alphabet: Vec<&str> = vec![
            "a", "Z", "/", "\\", ":", ".", " ", "~", "é", "ñ", "한", "글", "문", "🚀", "𝒳", "?",
            "c:", "//", "\\\\?\\",
        ];
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let len = (next() % 12) as usize;
            let raw: String = (0..len)
                .map(|_| alphabet[(next() % alphabet.len() as u64) as usize])
                .collect();
            for root in [None, Some("/tmp/proj"), Some("C:\\Users\\me"), Some("한글")] {
                let p = PortablePath::from_raw(&raw, root);
                let _ = (p.extension(), p.display().len());
                let _ = elide_home(&p.logical);
            }
        }
    }

    #[test]
    fn outside_root_has_no_relative() {
        let p = PortablePath::from_raw("/etc/hosts", Some("/Users/me/proj"));
        assert_eq!(p.repo_relative, None);
    }
}

#[cfg(test)]
mod elide_tests {
    use super::elide_home;

    #[test]
    fn home_prefixes_become_tilde() {
        assert_eq!(
            elide_home("/Users/dev/streamize/attemptdb"),
            "~/streamize/attemptdb"
        );
        assert_eq!(elide_home("/home/dev/example/project"), "~/example/project");
        assert_eq!(elide_home("C:/Users/dev/proj"), "~/proj");
        assert_eq!(elide_home("/home/dev"), "~");
        assert_eq!(elide_home("/opt/build"), "/opt/build");
        assert_eq!(elide_home("~/already"), "~/already");
        assert_eq!(elide_home("/Users"), "/Users");
    }
}
