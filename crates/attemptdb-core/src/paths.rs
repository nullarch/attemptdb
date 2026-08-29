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
            while logical[1.min(logical.len())..].contains("//") {
                let head = &logical[..1.min(logical.len())];
                let tail = logical[1.min(logical.len())..].replace("//", "/");
                logical = format!("{head}{tail}");
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
        let p = PortablePath::from_raw("c:\\Users\\me\\proj\\src\\main.rs", Some("C:\\Users\\me\\proj"));
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
    fn outside_root_has_no_relative() {
        let p = PortablePath::from_raw("/etc/hosts", Some("/Users/me/proj"));
        assert_eq!(p.repo_relative, None);
    }
}
