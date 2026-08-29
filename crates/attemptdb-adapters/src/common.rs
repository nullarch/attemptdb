//! Shared normalisation machinery used by every provider adapter.
//!
//! Two invariants of the canonical model are enforced here rather than in
//! each adapter: `attrs` only ever receive keys from [`ALLOWED_ATTR_KEYS`],
//! and content-bearing data is routed through [`Normaliser`] so it is dropped
//! in `metadata_only` capture mode. The classifiers (tool category, failure
//! class, command category, file facts) are pure functions shared by all
//! providers so the same input yields the same metadata regardless of which
//! agent produced it.

use crate::{ADAPTER_VERSION, AdapterError, CaptureContext};
use attemptdb_core::event::{EventContent, Provider};
use attemptdb_core::{
    AgentId, Event, EventKind, Outcome, OutcomeStatus, PortablePath, Timestamp, ToolCategory,
    ToolRef,
};
use serde_json::{Map, Value};

/// Largest tool output retained in `content.tool_output`, in bytes.
pub const TOOL_OUTPUT_LIMIT: usize = 64 * 1024;

/// Placeholder session id when a payload carries none.
pub const UNKNOWN_SESSION: &str = "unknown";

/// The only keys an adapter may write to `Event::attrs`. Everything here is
/// metadata: names, counts, booleans, classifications, normalised paths.
pub const ALLOWED_ATTR_KEYS: &[&str] = &[
    "hook_event_name",
    "source",
    "reason",
    "notification_type",
    "trigger",
    "stop_hook_active",
    "permission_mode",
    "cwd",
    "transcript_present",
    "prompt_chars",
    "prompt_lines",
    "prompt_has_code_fence",
    "prompt_has_question",
    "command_bytes",
    "command_category",
    "git_subcommand",
    "file_ext",
    "file_is_test",
    "file_is_config",
    "file_is_doc",
    "lines_added",
    "lines_removed",
    "error_class",
    "error_bytes",
    "is_subagent",
    "agent_type",
    "task_id",
    "task_status",
    "previous_cwd",
    "worktree_path",
    "config_source",
    "tool_output_truncated",
    "provider",
    // Transcript import (reconstructed events; see `crate::transcript`).
    "reconstructed",
    "reconstructed_from",
    "transcript_entry_type",
    "is_sidechain",
    "turn_index_hint",
];

/// Keys that are removed from the retained raw payload because they point at
/// transcripts on disk (which contain full conversation content).
const TRANSCRIPT_KEYS: &[&str] = &["transcript_path", "agent_transcript_path"];

// ---------------------------------------------------------------------------
// Payload view
// ---------------------------------------------------------------------------

/// Read-only view over a payload object. Keys starting with `_` are fixture
/// annotations and are invisible through this view.
#[derive(Clone, Copy)]
pub(crate) struct Payload<'a> {
    map: &'a Map<String, Value>,
}

impl<'a> Payload<'a> {
    pub fn from_value(value: &'a Value) -> Result<Self, AdapterError> {
        value
            .as_object()
            .map(|map| Self { map })
            .ok_or(AdapterError::PayloadNotObject)
    }

    pub fn get(self, key: &str) -> Option<&'a Value> {
        if key.starts_with('_') {
            return None;
        }
        self.map.get(key)
    }

    /// Non-empty string value.
    pub fn str(self, key: &str) -> Option<&'a str> {
        self.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    }

    pub fn first_str(self, keys: &[&str]) -> Option<&'a str> {
        keys.iter().find_map(|k| self.str(k))
    }

    pub fn object(self, key: &str) -> Option<&'a Map<String, Value>> {
        self.get(key).and_then(Value::as_object)
    }

    pub fn array(self, key: &str) -> Option<&'a Vec<Value>> {
        self.get(key).and_then(Value::as_array)
    }

    pub fn number(self, key: &str) -> Option<f64> {
        self.get(key).and_then(Value::as_f64)
    }

    pub fn bool(self, key: &str) -> Option<bool> {
        self.get(key).and_then(Value::as_bool)
    }

    /// The payload as retained in `Event::raw`: fixture annotations and
    /// transcript paths removed.
    pub fn retained_raw(self) -> Value {
        let map: Map<String, Value> = self
            .map
            .iter()
            .filter(|(k, _)| !k.starts_with('_') && !TRANSCRIPT_KEYS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        Value::Object(map)
    }
}

/// Resolve the provider event name: payload `hook_event_name`, else the hint.
pub(crate) fn event_name(payload: Payload<'_>, hint: Option<&str>) -> Result<String, AdapterError> {
    payload
        .str("hook_event_name")
        .or(hint)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or(AdapterError::MissingEventName)
}

/// Provider timestamp (`timestamp` as ISO text or epoch number), else the
/// capture time.
pub(crate) fn observed_at(payload: Payload<'_>, ctx: &CaptureContext) -> Timestamp {
    let parsed = match payload.get("timestamp") {
        Some(Value::String(s)) => Timestamp::parse(s),
        Some(Value::Number(n)) => n.as_f64().and_then(epoch_to_timestamp),
        _ => None,
    };
    parsed.unwrap_or(ctx.captured_at)
}

fn epoch_to_timestamp(n: f64) -> Option<Timestamp> {
    if !n.is_finite() || n <= 0.0 {
        return None;
    }
    let micros = if n > 1e14 {
        n
    } else if n > 1e10 {
        n * 1e3
    } else {
        n * 1e6
    };
    Some(Timestamp::from_micros(micros as i64))
}

// ---------------------------------------------------------------------------
// Classifiers
// ---------------------------------------------------------------------------

/// Coarse category for a provider tool name (case-insensitive).
pub fn classify_tool(name: &str) -> ToolCategory {
    let lower = name.trim().to_ascii_lowercase();
    if lower.starts_with("mcp__") {
        return ToolCategory::Mcp;
    }
    match lower.as_str() {
        "bash" | "shell" | "run_shell_command" | "execute" | "run_command" | "run_terminal_cmd" => {
            ToolCategory::Shell
        }
        "read" | "read_file" | "read_many_files" => ToolCategory::FileRead,
        "write" | "write_file" | "create_file" => ToolCategory::FileWrite,
        "edit" | "multiedit" | "replace" | "apply_patch" | "str_replace" | "str_replace_editor"
        | "edit_file" => ToolCategory::FileEdit,
        "notebookedit" | "notebook_edit" => ToolCategory::Notebook,
        "glob"
        | "grep"
        | "search_file_content"
        | "list_directory"
        | "ls"
        | "codebase_search"
        | "grep_search"
        | "file_search" => ToolCategory::Search,
        "webfetch" | "websearch" | "web_fetch" | "web_search" | "google_web_search" => {
            ToolCategory::Web
        }
        "task" | "agent" | "spawn_agent" => ToolCategory::Subagent,
        "enterplanmode" | "exitplanmode" | "todowrite" | "todo_write" | "update_plan" => {
            ToolCategory::Plan
        }
        _ => ToolCategory::Other,
    }
}

/// Content-free failure classification derived from an error message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FailureClass {
    pub class: &'static str,
    pub exit_code: Option<i32>,
}

impl FailureClass {
    const fn simple(class: &'static str) -> Self {
        Self {
            class,
            exit_code: None,
        }
    }

    pub const UNKNOWN: Self = Self::simple("unknown");
}

/// Classify an error text. Only the class (and an exit code, when the text
/// names one) is ever persisted as metadata; the text itself is content.
pub fn classify_failure(text: &str) -> FailureClass {
    let lower = text.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));
    if has(&["string to replace not found", "old_string"]) {
        FailureClass::simple("string_mismatch")
    } else if has(&["no such file", "not found", "enoent", "does not exist"]) {
        FailureClass::simple("file_not_found")
    } else if has(&["permission denied", "eacces", "eperm"]) {
        FailureClass::simple("permission_denied")
    } else if has(&["timed out", "timeout"]) {
        FailureClass::simple("timeout")
    } else if has(&["interrupted", "cancelled", "canceled", "aborted"]) {
        FailureClass::simple("interrupted")
    } else if let Some(code) = exit_code_from_text(&lower) {
        FailureClass {
            class: "nonzero_exit",
            exit_code: Some(code),
        }
    } else if has(&["non-zero", "nonzero"]) {
        FailureClass::simple("nonzero_exit")
    } else {
        FailureClass::UNKNOWN
    }
}

fn exit_code_from_text(lower: &str) -> Option<i32> {
    const MARKERS: &[&str] = &[
        "exit code ",
        "exit status ",
        "exited with code ",
        "exited with ",
        "returned exit code ",
    ];
    MARKERS.iter().find_map(|marker| {
        let idx = lower.find(marker)?;
        let digits: String = lower[idx + marker.len()..]
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '-')
            .collect();
        digits.parse::<i32>().ok().filter(|c| *c != 0)
    })
}

/// Content-free description of a shell command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandFacts {
    /// One of `git`, `test`, `build`, `install`, `run`, `fs`, `network`,
    /// `other`.
    pub category: &'static str,
    /// Git subcommand when the command (or a chained segment) starts with
    /// `git`.
    pub git_subcommand: Option<String>,
}

impl CommandFacts {
    fn category(category: &'static str) -> Self {
        Self {
            category,
            git_subcommand: None,
        }
    }
}

const CATEGORY_PRIORITY: &[&str] = &[
    "git", "test", "build", "install", "network", "run", "fs", "other",
];

const GIT_PRIORITY: &[&str] = &[
    "commit",
    "push",
    "pull",
    "merge",
    "rebase",
    "cherry-pick",
    "revert",
    "reset",
    "checkout",
    "switch",
    "stash",
    "tag",
    "clone",
    "fetch",
    "add",
    "rm",
    "mv",
    "branch",
    "worktree",
    "status",
    "log",
    "diff",
    "show",
];

const COMMAND_WRAPPERS: &[&str] = &["sudo", "env", "time", "nohup", "exec", "command", "nice"];

const NETWORK_PROGRAMS: &[&str] = &[
    "curl", "wget", "ssh", "scp", "rsync", "ping", "nc", "dig", "nslookup", "gh", "http", "aws",
    "gcloud", "vercel", "firebase", "supabase", "netlify", "fly", "flyctl", "heroku", "kubectl",
];

const FS_PROGRAMS: &[&str] = &[
    "ls", "cat", "head", "tail", "sed", "awk", "grep", "rg", "find", "fd", "mkdir", "rm", "cp",
    "mv", "touch", "chmod", "chown", "ln", "pwd", "cd", "echo", "tree", "wc", "sort", "uniq",
    "diff", "stat", "du", "df", "tar", "zip", "unzip", "which", "xargs", "tee", "cut", "basename",
    "dirname", "realpath", "readlink",
];

/// Classify a shell command line. Chained commands (`a && b | c`) are
/// classified per segment and the most significant segment wins.
pub fn classify_command(command: &str) -> CommandFacts {
    split_segments(command)
        .into_iter()
        .map(classify_segment)
        .reduce(|best, next| if outranks(&next, &best) { next } else { best })
        .unwrap_or_else(|| CommandFacts::category("other"))
}

fn outranks(a: &CommandFacts, b: &CommandFacts) -> bool {
    let rank = |c: &str| {
        CATEGORY_PRIORITY
            .iter()
            .position(|p| *p == c)
            .unwrap_or(usize::MAX)
    };
    let git_rank = |s: Option<&str>| match s {
        Some(sub) => GIT_PRIORITY
            .iter()
            .position(|g| *g == sub)
            .unwrap_or(GIT_PRIORITY.len()),
        None => GIT_PRIORITY.len() + 1,
    };
    match rank(a.category).cmp(&rank(b.category)) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Greater => false,
        std::cmp::Ordering::Equal => {
            git_rank(a.git_subcommand.as_deref()) < git_rank(b.git_subcommand.as_deref())
        }
    }
}

fn split_segments(command: &str) -> Vec<&str> {
    let bytes = command.as_bytes();
    let mut segments = Vec::new();
    let (mut start, mut i) = (0, 0);
    while i < bytes.len() {
        let cut = match bytes[i] {
            b'&' | b'|' if bytes.get(i + 1) == Some(&bytes[i]) => 2,
            b'&' | b'|' | b';' | b'\n' => 1,
            _ => 0,
        };
        if cut == 0 {
            i += 1;
            continue;
        }
        segments.push(&command[start..i]);
        i += cut;
        start = i;
    }
    segments.push(&command[start..]);
    segments
        .into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

fn is_env_assignment(token: &str) -> bool {
    match token.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !name.starts_with(|c: char| c.is_ascii_digit())
        }
        None => false,
    }
}

fn classify_segment(segment: &str) -> CommandFacts {
    let tokens: Vec<&str> = segment
        .split_whitespace()
        .skip_while(|t| is_env_assignment(t) || COMMAND_WRAPPERS.contains(t))
        .collect();
    let Some(first) = tokens.first() else {
        return CommandFacts::category("other");
    };
    let program = first
        .rsplit('/')
        .next()
        .unwrap_or(first)
        .trim_matches(['\'', '"']);
    let rest = &tokens[1..];
    let sub = rest.iter().copied().find(|t| !t.starts_with('-'));
    let category = match program {
        "git" => {
            return CommandFacts {
                category: "git",
                git_subcommand: git_subcommand(rest),
            };
        }
        "bash" | "sh" | "zsh" | "dash" | "fish" => {
            let inner: Vec<&str> = rest
                .iter()
                .copied()
                .skip_while(|t| t.starts_with('-'))
                .collect();
            if inner.is_empty() {
                return CommandFacts::category("run");
            }
            return classify_command(&inner.join(" "));
        }
        "npm" | "pnpm" | "yarn" | "bun" => package_manager_category(sub, rest),
        "npx" | "bunx" | "pnpx" => return classify_segment(&rest.join(" ")),
        "cargo" => match sub {
            Some("test" | "nextest") => "test",
            Some("build" | "check" | "clippy" | "doc") => "build",
            Some("run") => "run",
            Some("add" | "install" | "update" | "fetch") => "install",
            _ => "other",
        },
        "go" => match sub {
            Some("test") => "test",
            Some("build" | "vet" | "generate") => "build",
            Some("run") => "run",
            Some("get" | "mod" | "install") => "install",
            _ => "other",
        },
        "swift" | "dotnet" | "mix" => match sub {
            Some("test") => "test",
            Some("build") => "build",
            Some("run") => "run",
            _ => "other",
        },
        "docker" | "podman" => match sub {
            Some("build") => "build",
            Some("run" | "compose" | "exec" | "up") => "run",
            Some("push" | "pull" | "login") => "network",
            _ => "other",
        },
        "make" | "cmake" | "ninja" | "tsc" | "webpack" | "esbuild" | "rollup" | "gradle"
        | "gradlew" | "mvn" | "xcodebuild" | "xcodegen" => "build",
        "jest" | "vitest" | "pytest" | "mocha" | "rspec" | "phpunit" | "playwright" | "cypress"
        | "ava" => "test",
        "pip" | "pip3" | "uv" | "poetry" | "brew" | "apt" | "apt-get" | "gem" | "composer"
        | "conda" => match sub {
            Some(
                "install" | "i" | "add" | "sync" | "remove" | "uninstall" | "upgrade" | "update",
            ) => "install",
            _ => "other",
        },
        "node" | "python" | "python3" | "deno" | "ruby" | "php" | "java" | "uvicorn" | "flask"
        | "rails" | "ts-node" | "tsx" => "run",
        p if NETWORK_PROGRAMS.contains(&p) => "network",
        p if FS_PROGRAMS.contains(&p) => "fs",
        p if first.starts_with("./")
            || p.ends_with(".sh")
            || p.ends_with(".py")
            || p.ends_with(".js") =>
        {
            "run"
        }
        _ => "other",
    };
    CommandFacts::category(category)
}

fn package_manager_category(sub: Option<&str>, rest: &[&str]) -> &'static str {
    let script = rest.iter().copied().filter(|t| !t.starts_with('-')).nth(1);
    match sub {
        None
        | Some(
            "install" | "i" | "ci" | "add" | "remove" | "rm" | "update" | "up" | "upgrade" | "link",
        ) => "install",
        Some("test" | "t") => "test",
        Some("build") => "build",
        Some("run" | "run-script") => match script {
            Some(s) if s.starts_with("test") || s.contains(":test") || s.contains("test:") => {
                "test"
            }
            Some(s) if s.starts_with("build") || s.contains(":build") => "build",
            _ => "run",
        },
        Some(s) if s.starts_with("test") => "test",
        Some(s) if s.starts_with("build") => "build",
        Some(s) if s.starts_with("lint") || s.starts_with("format") => "other",
        Some(_) => "run",
    }
}

fn git_subcommand(rest: &[&str]) -> Option<String> {
    let mut iter = rest.iter().copied();
    while let Some(token) = iter.next() {
        match token {
            "-C" | "-c" | "--git-dir" | "--work-tree" => {
                iter.next();
            }
            t if t.starts_with('-') => {}
            t => return Some(t.to_ascii_lowercase()),
        }
    }
    None
}

/// Content-free facts about a file path.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct FileFacts {
    pub ext: Option<String>,
    pub is_test: bool,
    pub is_config: bool,
    pub is_doc: bool,
}

const CONFIG_EXTS: &[&str] = &[
    "json",
    "yaml",
    "yml",
    "toml",
    "ini",
    "cfg",
    "conf",
    "env",
    "properties",
    "plist",
    "lock",
];
const CONFIG_NAMES: &[&str] = &[
    "dockerfile",
    "makefile",
    "justfile",
    "procfile",
    ".gitignore",
    ".gitattributes",
    ".editorconfig",
    ".npmrc",
    ".nvmrc",
    ".dockerignore",
    "cargo.toml",
    "package.json",
];
const DOC_EXTS: &[&str] = &["md", "mdx", "rst", "txt", "adoc"];
const DOC_STEMS: &[&str] = &[
    "readme",
    "changelog",
    "license",
    "licence",
    "contributing",
    "authors",
    "notice",
];
const TEST_DIRS: &[&str] = &["/tests/", "/test/", "/__tests__/", "/spec/", "/specs/"];

pub fn file_facts(path: &PortablePath) -> FileFacts {
    let logical = path.logical.as_str();
    let name = logical
        .rsplit('/')
        .next()
        .unwrap_or(logical)
        .to_ascii_lowercase();
    let lower_path = logical.to_ascii_lowercase();
    let ext = path.extension();
    let ext_str = ext.as_deref().unwrap_or("");
    let stem = name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s);
    let is_test = TEST_DIRS.iter().any(|d| lower_path.contains(d))
        || name.starts_with("test_")
        || [".test.", ".spec.", "_test.", "_spec."]
            .iter()
            .any(|m| name.contains(m));
    let is_config = CONFIG_EXTS.contains(&ext_str)
        || CONFIG_NAMES.contains(&name.as_str())
        || name.starts_with(".env")
        || name.contains(".config.")
        || name.starts_with(".eslintrc")
        || name.starts_with(".prettierrc")
        || name.starts_with(".babelrc");
    let is_doc = DOC_EXTS.contains(&ext_str) || DOC_STEMS.contains(&stem);
    FileFacts {
        ext,
        is_test,
        is_config,
        is_doc,
    }
}

/// Content-free shape of a prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptFacts {
    pub chars: u64,
    pub lines: u64,
    pub has_code_fence: bool,
    pub has_question: bool,
}

/// User-message prefixes that the client injects rather than a human types
/// (task notifications, local command output, system reminders). Both the
/// hook adapter and the transcript parser treat these as notifications, not
/// prompts, so they never open a new turn.
pub const INJECTED_PROMPT_PREFIXES: &[&str] = &[
    "<task-notification>",
    "<system-reminder>",
    "<local-command-stdout>",
    "<local-command-caveat>",
    "<bash-stdout>",
    "<bash-stderr>",
    "[SYSTEM NOTIFICATION",
];

/// Content-free tag for an injected prompt, or `None` for a human prompt.
pub fn injected_prompt_kind(prompt: &str) -> Option<&'static str> {
    let t = prompt.trim_start();
    INJECTED_PROMPT_PREFIXES.iter().copied().find(|p| t.starts_with(p)).map(|p| match p {
        "<task-notification>" | "[SYSTEM NOTIFICATION" => "task_notification",
        "<system-reminder>" => "system_reminder",
        "<local-command-stdout>" | "<local-command-caveat>" => "local_command",
        _ => "shell_output",
    })
}

pub fn prompt_facts(prompt: &str) -> PromptFacts {
    PromptFacts {
        chars: prompt.chars().count() as u64,
        lines: line_count(prompt),
        has_code_fence: prompt.contains("```"),
        has_question: prompt.contains('?') || prompt.contains('？'),
    }
}

/// Number of lines in a text; empty text has zero lines.
pub fn line_count(text: &str) -> u64 {
    if text.is_empty() {
        0
    } else {
        text.lines().count() as u64
    }
}

/// Lines added/removed for an edit-like input. A single `old_string` /
/// `new_string` pair, an `edits[]` array of such pairs, or a written
/// `content` (all lines added). This is a size measure, not a diff.
pub fn edit_line_delta(input: &Map<String, Value>) -> Option<(u64, u64)> {
    let pair = |m: &Map<String, Value>| {
        let old = m.get("old_string").and_then(Value::as_str);
        let new = m.get("new_string").and_then(Value::as_str);
        (old.is_some() || new.is_some())
            .then(|| (line_count(new.unwrap_or("")), line_count(old.unwrap_or(""))))
    };
    if let Some(edits) = input.get("edits").and_then(Value::as_array) {
        let deltas: Vec<(u64, u64)> = edits
            .iter()
            .filter_map(Value::as_object)
            .filter_map(pair)
            .collect();
        return (!deltas.is_empty()).then(|| {
            deltas
                .iter()
                .fold((0, 0), |acc, d| (acc.0 + d.0, acc.1 + d.1))
        });
    }
    if let Some(delta) = pair(input) {
        return Some(delta);
    }
    input
        .get("content")
        .and_then(Value::as_str)
        .map(|content| (line_count(content), 0))
}

/// Shell command text from a tool input: a string, or an argv array.
pub fn command_from_input(input: &Map<String, Value>) -> Option<String> {
    match input.get("command")? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Array(parts) => {
            let joined: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
            (!joined.is_empty()).then(|| joined.join(" "))
        }
        _ => None,
    }
}

/// File paths referenced by a tool input (never command text).
pub fn input_paths(input: &Map<String, Value>) -> Vec<String> {
    let mut paths: Vec<String> = ["file_path", "notebook_path", "path", "absolute_path"]
        .iter()
        .filter_map(|k| input.get(*k).and_then(Value::as_str))
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .collect();
    if let Some(patch) = input
        .get("patch")
        .or_else(|| input.get("input"))
        .and_then(Value::as_str)
    {
        paths.extend(patch_paths(patch).into_iter().map(str::to_string));
    }
    paths
}

/// File paths named by `apply_patch` headers.
pub fn patch_paths(patch: &str) -> Vec<&str> {
    const HEADERS: &[&str] = &["*** Add File: ", "*** Update File: ", "*** Delete File: "];
    patch
        .lines()
        .map(str::trim)
        .filter_map(|line| HEADERS.iter().find_map(|h| line.strip_prefix(h)))
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect()
}

/// `exit_code` reported inside a tool response object.
pub fn response_exit_code(response: &Value) -> Option<i32> {
    let map = response.as_object()?;
    map.get("exit_code")
        .or_else(|| map.get("metadata").and_then(|m| m.get("exit_code")))
        .and_then(Value::as_i64)
        .map(|c| c as i32)
}

/// Bound a tool output to [`TOOL_OUTPUT_LIMIT`]. Returns the (possibly
/// truncated) value and whether truncation happened.
pub fn bounded_output(value: &Value) -> (Value, bool) {
    match value {
        Value::String(s) if s.len() > TOOL_OUTPUT_LIMIT => {
            (Value::String(truncate_utf8(s, TOOL_OUTPUT_LIMIT)), true)
        }
        Value::String(_) => (value.clone(), false),
        other => {
            let text = other.to_string();
            if text.len() > TOOL_OUTPUT_LIMIT {
                (Value::String(truncate_utf8(&text, TOOL_OUTPUT_LIMIT)), true)
            } else {
                (other.clone(), false)
            }
        }
    }
}

fn truncate_utf8(s: &str, max: usize) -> String {
    let mut end = max.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Lowercase snake_case form of a short provider token (`Rate-Limit` →
/// `rate_limit`).
pub fn to_snake(token: &str) -> String {
    token.trim().to_ascii_lowercase().replace([' ', '-'], "_")
}

/// True for short identifier-like strings (no whitespace), as opposed to
/// free-form messages.
pub fn is_token(s: &str) -> bool {
    !s.is_empty() && s.len() <= 48 && !s.chars().any(char::is_whitespace)
}

// ---------------------------------------------------------------------------
// Normaliser
// ---------------------------------------------------------------------------

/// Builds one [`Event`] from one payload. All writes to `attrs` and `content`
/// go through this type so the allowlist and the capture mode are enforced
/// in one place.
pub(crate) struct Normaliser<'a> {
    ctx: &'a CaptureContext,
    payload: Payload<'a>,
    pub event: Event,
    content: EventContent,
}

impl<'a> Normaliser<'a> {
    pub fn new(
        ctx: &'a CaptureContext,
        payload: Payload<'a>,
        provider: Provider,
        event_name: &str,
        kind: EventKind,
        provider_session_id: &str,
    ) -> Self {
        let mut event = Event::new(
            ctx.device_id,
            provider,
            event_name,
            kind,
            ctx.project.clone(),
            provider_session_id,
            ctx.capture_mode,
            ADAPTER_VERSION,
        );
        event.observed_at = observed_at(payload, ctx);
        event.captured_at = ctx.captured_at;
        event.provider_version = ctx.provider_version.clone();
        event.hook_version = ctx.hook_version.clone();
        let mut this = Self {
            ctx,
            payload,
            event,
            content: EventContent::default(),
        };
        this.attr("hook_event_name", event_name);
        this
    }

    pub fn payload(&self) -> Payload<'a> {
        self.payload
    }

    fn project_root(&self) -> &str {
        &self.ctx.project.root
    }

    // --- attrs -----------------------------------------------------------

    /// Set an allowlisted metadata attribute. Keys outside
    /// [`ALLOWED_ATTR_KEYS`] are a programming error and are ignored.
    pub fn attr(&mut self, key: &'static str, value: impl Into<Value>) {
        debug_assert!(
            ALLOWED_ATTR_KEYS.contains(&key),
            "attr `{key}` is not allowlisted"
        );
        if ALLOWED_ATTR_KEYS.contains(&key) {
            self.event.attrs.insert(key.to_string(), value.into());
        }
    }

    pub fn attr_opt<T: Into<Value>>(&mut self, key: &'static str, value: Option<T>) {
        if let Some(v) = value {
            self.attr(key, v);
        }
    }

    /// Copy content-free scalar payload fields under `attrs["provider"]`.
    /// Strings longer than 64 bytes are skipped as a guard against
    /// free-form text.
    pub fn copy_provider_attrs(&mut self, keys: &[&str]) {
        let mut extra = Map::new();
        for key in keys {
            match self.payload.get(key) {
                Some(Value::String(s)) if !s.is_empty() && s.len() <= 64 => {
                    extra.insert((*key).to_string(), Value::String(s.clone()));
                }
                Some(v @ (Value::Number(_) | Value::Bool(_))) => {
                    extra.insert((*key).to_string(), v.clone());
                }
                _ => {}
            }
        }
        if !extra.is_empty() {
            self.attr("provider", Value::Object(extra));
        }
    }

    pub fn provider_attr(&mut self, key: &str, value: impl Into<Value>) {
        let entry = self
            .event
            .attrs
            .entry("provider".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(map) = entry.as_object_mut() {
            map.insert(key.to_string(), value.into());
        }
    }

    // --- common payload fields --------------------------------------------

    pub fn set_cwd(&mut self) {
        if let Some(cwd) = self.payload.str("cwd") {
            let logical = PortablePath::from_raw(cwd, Some(self.project_root())).logical;
            self.attr("cwd", logical);
        }
    }

    pub fn set_permission_mode(&mut self) {
        let mode = self.payload.str("permission_mode").map(str::to_string);
        self.attr_opt("permission_mode", mode);
    }

    /// Record whether a transcript path was supplied, never the path.
    pub fn set_transcript_present(&mut self) {
        if let Some(value) = self.payload.get("transcript_path") {
            let present = value.as_str().is_some_and(|s| !s.is_empty());
            self.attr("transcript_present", present);
        }
    }

    pub fn set_model(&mut self) {
        self.event.agent.model = self.payload.str("model").map(str::to_string);
    }

    pub fn set_duration(&mut self, keys: &[&str]) {
        self.event.duration_ms = keys
            .iter()
            .find_map(|k| self.payload.number(k))
            .filter(|d| d.is_finite() && *d >= 0.0)
            .map(|d| d as u64);
    }

    /// Attribute the event to a subagent of this session.
    pub fn set_subagent(&mut self, provider_agent_id: &str, agent_type: Option<&str>) {
        let session = self.event.session_id.to_string();
        let parent = AgentId::derive(&["session", &session]);
        self.event.agent.agent_id = AgentId::derive(&["subagent", &session, provider_agent_id]);
        self.event.agent.provider_agent_id = Some(provider_agent_id.to_string());
        self.event.agent.agent_type = agent_type.map(str::to_string);
        self.event.agent.parent_agent_id = Some(parent);
        self.attr("is_subagent", true);
        self.attr_opt("agent_type", agent_type.map(str::to_string));
    }

    // --- paths --------------------------------------------------------------

    /// Add a path (deduplicated by logical form). Returns the normalised
    /// path when it was added.
    pub fn add_path(&mut self, raw: &str) -> Option<PortablePath> {
        if raw.trim().is_empty() {
            return None;
        }
        let path = PortablePath::from_raw(raw, Some(self.project_root()));
        if self.event.paths.iter().any(|p| p.logical == path.logical) {
            return None;
        }
        self.event.paths.push(path.clone());
        Some(path)
    }

    /// Add a file path and, for the first file of the event, its facts.
    pub fn add_file(&mut self, raw: &str) {
        let Some(path) = self.add_path(raw) else {
            return;
        };
        if self.event.attrs.contains_key("file_is_test") {
            return;
        }
        let facts = file_facts(&path);
        self.attr_opt("file_ext", facts.ext);
        self.attr("file_is_test", facts.is_test);
        self.attr("file_is_config", facts.is_config);
        self.attr("file_is_doc", facts.is_doc);
    }

    // --- tool ---------------------------------------------------------------

    pub fn set_tool(&mut self, name: &str, call_id: Option<&str>) {
        self.event.tool = Some(ToolRef {
            name: name.to_string(),
            category: classify_tool(name),
            call_id: call_id.map(str::to_string),
        });
    }

    /// Derive paths, command facts, edit deltas and the subagent type from
    /// a tool input object, and retain the object as content.
    pub fn apply_tool_input(&mut self, input: &Value) {
        self.set_tool_input(input);
        let Some(map) = input.as_object() else { return };
        for raw in input_paths(map) {
            self.add_file(&raw);
        }
        if let Some(command) = command_from_input(map) {
            self.set_command(&command);
        }
        if let Some((added, removed)) = edit_line_delta(map) {
            self.set_edit_delta(added, removed);
        }
        if let Some(agent_type) = map.get("subagent_type").and_then(Value::as_str) {
            self.attr("agent_type", agent_type);
        }
    }

    pub fn set_command(&mut self, command: &str) {
        let facts = classify_command(command);
        self.attr("command_bytes", command.len() as u64);
        self.attr("command_category", facts.category);
        self.attr_opt("git_subcommand", facts.git_subcommand);
        self.content.command = Some(command.to_string());
    }

    pub fn set_edit_delta(&mut self, added: u64, removed: u64) {
        self.attr("lines_added", added);
        self.attr("lines_removed", removed);
    }

    pub fn set_tool_input(&mut self, input: &Value) {
        self.content.tool_input = Some(input.clone());
    }

    pub fn set_tool_output(&mut self, output: &Value) {
        let (bounded, truncated) = bounded_output(output);
        if truncated {
            self.attr("tool_output_truncated", true);
        }
        self.content.tool_output = Some(bounded);
    }

    // --- outcome ------------------------------------------------------------

    pub fn set_success(&mut self, exit_code: Option<i32>) {
        self.event.outcome = Some(Outcome {
            exit_code,
            ..Outcome::success()
        });
    }

    /// Mark the event failed, classifying `text` when present. An explicit
    /// `exit_code` overrides whatever the text implies.
    pub fn set_failure(&mut self, text: Option<&str>, exit_code: Option<i32>) {
        let mut fc = text.map(classify_failure).unwrap_or(FailureClass::UNKNOWN);
        if let Some(code) = exit_code {
            fc.exit_code = Some(code);
            if fc.class == "unknown" {
                fc.class = "nonzero_exit";
            }
        }
        self.finish_failure(fc.class.to_string(), fc.exit_code, text);
    }

    /// Mark the event failed with a provider-supplied class.
    pub fn set_failure_with_class(&mut self, class: &str, text: Option<&str>) {
        let exit_code = text.and_then(|t| classify_failure(t).exit_code);
        self.finish_failure(to_snake(class), exit_code, text);
    }

    fn finish_failure(&mut self, class: String, exit_code: Option<i32>, text: Option<&str>) {
        self.attr("error_class", class.as_str());
        if let Some(t) = text {
            self.attr("error_bytes", t.len() as u64);
            self.content.error = Some(t.to_string());
        }
        self.event.outcome = Some(Outcome {
            status: OutcomeStatus::Failure,
            class: Some(class),
            exit_code,
        });
    }

    // --- content ------------------------------------------------------------

    pub fn set_prompt(&mut self, prompt: &str) {
        let facts = prompt_facts(prompt);
        self.attr("prompt_chars", facts.chars);
        self.attr("prompt_lines", facts.lines);
        self.attr("prompt_has_code_fence", facts.has_code_fence);
        self.attr("prompt_has_question", facts.has_question);
        self.content.prompt = Some(prompt.to_string());
    }

    pub fn set_message(&mut self, message: &str) {
        self.content.message = Some(message.to_string());
    }

    pub fn set_extra(&mut self, key: &str, value: impl Into<Value>) {
        self.content.extra.insert(key.to_string(), value.into());
    }

    // --- finish -------------------------------------------------------------

    /// Attach content and raw payload as the capture mode permits.
    pub fn finish(self) -> Event {
        let Normaliser {
            ctx,
            payload,
            mut event,
            content,
        } = self;
        if ctx.capture_mode.persists_content_locally() {
            if !content.is_empty() {
                event.content = Some(content);
            }
            event.raw = Some(payload.retained_raw());
        }
        event.apply_capture_mode();
        event
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_categories() {
        assert_eq!(classify_tool("Bash"), ToolCategory::Shell);
        assert_eq!(classify_tool("run_shell_command"), ToolCategory::Shell);
        assert_eq!(classify_tool("MultiEdit"), ToolCategory::FileEdit);
        assert_eq!(classify_tool("apply_patch"), ToolCategory::FileEdit);
        assert_eq!(
            classify_tool("mcp__github__create_issue"),
            ToolCategory::Mcp
        );
        assert_eq!(classify_tool("google_web_search"), ToolCategory::Web);
        assert_eq!(classify_tool("Task"), ToolCategory::Subagent);
        assert_eq!(classify_tool("update_plan"), ToolCategory::Plan);
        assert_eq!(classify_tool("NotebookEdit"), ToolCategory::Notebook);
        assert_eq!(classify_tool("Something"), ToolCategory::Other);
    }

    #[test]
    fn failure_classes_in_priority_order() {
        assert_eq!(
            classify_failure("String to replace not found in file").class,
            "string_mismatch"
        );
        assert_eq!(
            classify_failure("old_string not found").class,
            "string_mismatch"
        );
        assert_eq!(
            classify_failure("ENOENT: no such file").class,
            "file_not_found"
        );
        assert_eq!(
            classify_failure("EACCES: permission denied").class,
            "permission_denied"
        );
        assert_eq!(
            classify_failure("Command timed out after 30s").class,
            "timeout"
        );
        assert_eq!(
            classify_failure("Operation cancelled by user").class,
            "interrupted"
        );
        let exit = classify_failure("Command failed with exit code 127");
        assert_eq!((exit.class, exit.exit_code), ("nonzero_exit", Some(127)));
        assert_eq!(classify_failure("process exited with 2").exit_code, Some(2));
        assert_eq!(classify_failure("something odd").class, "unknown");
    }

    #[test]
    fn command_categories() {
        let git = classify_command("git commit -m 'x'");
        assert_eq!(
            (git.category, git.git_subcommand.as_deref()),
            ("git", Some("commit"))
        );
        let chain = classify_command("git add . && git commit -m x && git push");
        assert_eq!(chain.git_subcommand.as_deref(), Some("commit"));
        assert_eq!(
            classify_command("npm run test --watch=false").category,
            "test"
        );
        assert_eq!(classify_command("npm run build").category, "build");
        assert_eq!(classify_command("npm install").category, "install");
        assert_eq!(classify_command("cargo test -p x").category, "test");
        assert_eq!(classify_command("CI=1 sudo make -j4").category, "build");
        assert_eq!(classify_command("npx vitest run").category, "test");
        assert_eq!(
            classify_command("bash -lc cargo test -p x").category,
            "test"
        );
        assert_eq!(
            classify_command("sh -c 'git push && echo done'")
                .git_subcommand
                .as_deref(),
            Some("push")
        );
        assert_eq!(classify_command("bash").category, "run");
        assert_eq!(classify_command("cd src && ls -la").category, "fs");
        assert_eq!(
            classify_command("curl https://example.com").category,
            "network"
        );
        assert_eq!(classify_command("./scripts/dev.sh").category, "run");
        assert_eq!(classify_command("some-tool --flag").category, "other");
        assert_eq!(classify_command("").category, "other");
        assert_eq!(
            classify_command("git -C /tmp/x status")
                .git_subcommand
                .as_deref(),
            Some("status")
        );
    }

    #[test]
    fn file_facts_detect_shape() {
        let p = PortablePath::from_raw("/p/src/__tests__/app.test.ts", Some("/p"));
        let f = file_facts(&p);
        assert!(f.is_test && !f.is_config && !f.is_doc);
        assert_eq!(f.ext.as_deref(), Some("ts"));
        assert!(file_facts(&PortablePath::from_raw("/p/package.json", None)).is_config);
        assert!(file_facts(&PortablePath::from_raw("/p/.env.local", None)).is_config);
        assert!(file_facts(&PortablePath::from_raw("/p/README", None)).is_doc);
        assert!(file_facts(&PortablePath::from_raw("/p/docs/guide.md", None)).is_doc);
    }

    #[test]
    fn edit_deltas() {
        let single: Map<String, Value> =
            serde_json::from_str(r#"{"old_string":"a","new_string":"b\nc\nd"}"#).unwrap();
        assert_eq!(edit_line_delta(&single), Some((3, 1)));
        let edits: Map<String, Value> = serde_json::from_str(
            r#"{"edits":[{"old_string":"a\nb","new_string":"c"},{"old_string":"","new_string":"x\ny"}]}"#,
        )
        .unwrap();
        assert_eq!(edit_line_delta(&edits), Some((3, 2)));
        let write: Map<String, Value> = serde_json::from_str(r#"{"content":"l1\nl2\n"}"#).unwrap();
        assert_eq!(edit_line_delta(&write), Some((2, 0)));
        let none: Map<String, Value> = serde_json::from_str(r#"{"command":"ls"}"#).unwrap();
        assert_eq!(edit_line_delta(&none), None);
    }

    #[test]
    fn command_and_patch_extraction() {
        let argv: Map<String, Value> =
            serde_json::from_str(r#"{"command":["bash","-lc","cargo test"]}"#).unwrap();
        assert_eq!(
            command_from_input(&argv).as_deref(),
            Some("bash -lc cargo test")
        );
        let patch =
            "*** Begin Patch\n*** Update File: src/a.rs\n@@\n*** Add File: b.rs\n*** End Patch";
        assert_eq!(patch_paths(patch), vec!["src/a.rs", "b.rs"]);
    }

    #[test]
    fn output_is_bounded() {
        let big = Value::String("x".repeat(TOOL_OUTPUT_LIMIT + 10));
        let (bounded, truncated) = bounded_output(&big);
        assert!(truncated);
        assert_eq!(bounded.as_str().map(str::len), Some(TOOL_OUTPUT_LIMIT));
        let multibyte = Value::String("한".repeat(TOOL_OUTPUT_LIMIT));
        let (bounded, _) = bounded_output(&multibyte);
        assert!(bounded.as_str().unwrap().len() <= TOOL_OUTPUT_LIMIT);
        let (same, truncated) = bounded_output(&serde_json::json!({"ok": true}));
        assert!(!truncated && same == serde_json::json!({"ok": true}));
    }

    #[test]
    fn epoch_numbers_are_scaled() {
        assert_eq!(
            epoch_to_timestamp(1_756_368_000.0),
            Some(Timestamp::from_micros(1_756_368_000_000_000))
        );
        assert_eq!(
            epoch_to_timestamp(1_756_368_000_123.0),
            Some(Timestamp::from_micros(1_756_368_000_123_000))
        );
        assert_eq!(
            epoch_to_timestamp(1_756_368_000_123_456.0),
            Some(Timestamp::from_micros(1_756_368_000_123_456))
        );
        assert_eq!(epoch_to_timestamp(-1.0), None);
    }

    #[test]
    fn payload_ignores_private_keys_and_strips_transcripts() {
        let v = serde_json::json!({"_note": "x", "transcript_path": "/t.jsonl", "a": 1});
        let p = Payload::from_value(&v).unwrap();
        assert!(p.get("_note").is_none());
        let raw = p.retained_raw();
        assert_eq!(raw, serde_json::json!({"a": 1}));
        assert!(matches!(
            Payload::from_value(&Value::Null),
            Err(AdapterError::PayloadNotObject)
        ));
    }
}
