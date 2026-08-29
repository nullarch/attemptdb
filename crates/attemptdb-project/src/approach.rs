//! Content-free approach summaries.
//!
//! An approach string describes *what kind* of things an attempt did, built
//! only from tool categories and repository-relative paths, e.g.
//! `edit src/lib.rs, src/main.rs · shell ×3 · read ×2`. It never contains
//! prompt text, command lines, or tool output.

use crate::model::ToolCall;
use attemptdb_core::{PortablePath, ToolCategory};

/// Maximum number of paths listed per mutating category before eliding.
const MAX_PATHS: usize = 4;

/// Non-mutating categories in render order with their labels.
const COUNTED: &[(ToolCategory, &str)] = &[
    (ToolCategory::Shell, "shell"),
    (ToolCategory::FileRead, "read"),
    (ToolCategory::Search, "search"),
    (ToolCategory::Web, "web"),
    (ToolCategory::Mcp, "mcp"),
    (ToolCategory::Subagent, "subagent"),
    (ToolCategory::Plan, "plan"),
    (ToolCategory::Other, "other"),
];

/// The path key used everywhere the projection compares or lists paths:
/// repository-relative when known, else the normalised logical path.
pub(crate) fn path_key(p: &PortablePath) -> String {
    p.display().to_string()
}

#[derive(Default)]
struct Bucket {
    count: u32,
    paths: Vec<String>,
}

impl Bucket {
    fn add(&mut self, call: &ToolCall) {
        self.count += 1;
        for p in &call.paths {
            let key = path_key(p);
            if !self.paths.contains(&key) {
                self.paths.push(key);
            }
        }
    }

    fn render(&self, verb: &str) -> Option<String> {
        if self.count == 0 {
            return None;
        }
        if self.paths.is_empty() {
            return Some(counted(verb, self.count));
        }
        let shown = self
            .paths
            .iter()
            .take(MAX_PATHS)
            .cloned()
            .collect::<Vec<_>>();
        let mut list = shown.join(", ");
        if self.paths.len() > MAX_PATHS {
            list.push_str(&format!(" +{} more", self.paths.len() - MAX_PATHS));
        }
        Some(format!("{verb} {list}"))
    }
}

fn counted(verb: &str, n: u32) -> String {
    if n == 1 {
        verb.to_string()
    } else {
        format!("{verb} \u{d7}{n}")
    }
}

pub(crate) fn summarise<'a>(calls: impl IntoIterator<Item = &'a ToolCall>) -> String {
    let mut edit = Bucket::default();
    let mut write = Bucket::default();
    let mut notebook = Bucket::default();
    let mut counts = vec![0u32; COUNTED.len()];
    for call in calls {
        match call.tool.category {
            ToolCategory::FileEdit => edit.add(call),
            ToolCategory::FileWrite => write.add(call),
            ToolCategory::Notebook => notebook.add(call),
            other => {
                if let Some(i) = COUNTED.iter().position(|(c, _)| *c == other) {
                    counts[i] += 1;
                }
            }
        }
    }
    let mut segments: Vec<String> = Vec::new();
    segments.extend(edit.render("edit"));
    segments.extend(write.render("write"));
    segments.extend(notebook.render("notebook"));
    for (i, (_, label)) in COUNTED.iter().enumerate() {
        if counts[i] > 0 {
            segments.push(counted(label, counts[i]));
        }
    }
    if segments.is_empty() {
        "no tool calls".to_string()
    } else {
        segments.join(" \u{b7} ")
    }
}
