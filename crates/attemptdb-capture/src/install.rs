//! Hook configuration writers: install, upgrade, repair and uninstall the
//! `attempt hook <provider>` entries in each agent's config file.
//!
//! Invariants:
//! - Agent directories are created only for agents that were detected
//!   ([`crate::agents::detect_agents`]); never for undetected agents.
//! - JSON is parsed and updated structurally. Key order is preserved
//!   (`serde_json/preserve_order`), indentation is detected from the existing
//!   file and re-emitted, and unrelated content is never touched.
//! - Writes are locked, backed up (`<file>.attemptdb.bak-<ts>`, five newest
//!   kept) and atomic (temp file + rename). A config that is a symlink (a
//!   dotfiles manager's `settings.json`) is written *through* the link: the
//!   temp file is renamed over the link's target, so the link survives and
//!   the dotfiles copy is the one that changes. The lock file is removed
//!   again when the lock is released, so nothing accumulates in an agent's
//!   directory beyond the backups and (for OTel) one ownership ledger.
//! - Every operation is idempotent: all AttemptDB entries are removed from
//!   every event first (so stale events and old binary paths die on upgrade)
//!   and the current set is then (re)inserted at the position the old entry
//!   occupied.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow, bail};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::agents::{AgentKind, DetectOptions, DetectedAgent, detect_agents_with};
use crate::platform::{current_exe_stable_path, is_windows, quote_for_shell, stable_display_path};

/// Marker that identifies entries written by pre-1.0 builds regardless of the
/// binary name. Any command containing this string is treated as ours.
pub const LEGACY_MARKER: &str = "attemptdb-hook";

/// The dedicated hook executable (`crates/attempt-hook`): the capture crate
/// alone, a fraction of `attempt`'s size, so the page-in that dominated hook
/// wall time is gone. Preferred whenever it sits next to `attempt`.
pub const HOOK_BINARY_NAME: &str = "attempt-hook";

/// True when `binary` is the dedicated hook executable (by file name).
pub fn is_hook_binary(binary: &Path) -> bool {
    binary
        .file_name()
        .map(|n| n.to_string_lossy())
        .is_some_and(|n| n == HOOK_BINARY_NAME || n.eq_ignore_ascii_case("attempt-hook.exe"))
}

/// The executable hook commands should reference: the dedicated
/// `attempt-hook` next to `exe` when there is one, else `exe` itself.
pub fn preferred_hook_binary(exe: PathBuf) -> PathBuf {
    if is_hook_binary(&exe) {
        return exe;
    }
    let Some(dir) = exe.parent() else {
        return exe;
    };
    let name = if is_windows() {
        "attempt-hook.exe"
    } else {
        HOOK_BINARY_NAME
    };
    let sibling = dir.join(name);
    if sibling.is_file() { sibling } else { exe }
}
/// Prefix of the `name` field on Gemini CLI entries (also treated as ours).
pub const GEMINI_NAME_PREFIX: &str = "attemptdb-";

/// The VibeMon legacy thin client (`vibemon-hooks`) wrote one entry per
/// event that runs `bash ~/.vibemon/notify.sh <event> <provider>`; its Gemini
/// entries are additionally named `vibemon-<event>`. Those entries are left
/// alone by default and removed only with `InstallOptions::remove_legacy`.
pub const LEGACY_VIBEMON_SCRIPT: &str = ".vibemon/notify.sh";
/// The Windows legacy client's entries run `notify.py` (Python) or
/// `notify.ps1` from the same directory, with either path separator.
pub const LEGACY_VIBEMON_SCRIPTS: &[&str] = &[
    ".vibemon/notify.sh",
    ".vibemon/notify.py",
    ".vibemon/notify.ps1",
    ".vibemon\\notify.py",
    ".vibemon\\notify.ps1",
    ".vibemon\\notify.sh",
];
pub const LEGACY_VIBEMON_NAME_PREFIX: &str = "vibemon-";
/// Number of `<file>.attemptdb.bak-<ts>` backups to keep per config file.
pub const BACKUPS_TO_KEEP: usize = 5;

// The adapters own the passive subscription contract. Keep these re-exports
// for callers; installer and doctor must never maintain a second event list.
pub use attemptdb_adapters::claude_code::CLAUDE_CAPTURE_EVENTS as CLAUDE_EVENTS;
pub use attemptdb_adapters::codex::CODEX_EVENTS;
pub use attemptdb_adapters::cursor::CURSOR_CAPTURE_EVENTS as CURSOR_EVENTS;
pub use attemptdb_adapters::gemini_cli::GEMINI_CLI_EVENTS as GEMINI_EVENTS;

/// Claude Code timeout, seconds. SessionEnd's default budget is 1.5 s, so an
/// explicit timeout matters there.
pub const CLAUDE_TIMEOUT_SECS: u64 = 5;
/// Codex timeout, seconds.
pub const CODEX_TIMEOUT_SECS: u64 = 5;
/// Codex clamps SessionEnd hooks to at most 3 s and warns on every start-up
/// when the configured value is larger, so we write the maximum it accepts.
pub const CODEX_SESSION_END_TIMEOUT_SECS: u64 = 3;
/// Cursor timeout, SECONDS (a previous product wrote 5000 = 83 minutes).
pub const CURSOR_TIMEOUT_SECS: u64 = 10;
/// Gemini CLI timeout, MILLISECONDS.
pub const GEMINI_TIMEOUT_MILLIS: u64 = 5000;

/// Events installed for an agent.
pub fn events_for(kind: AgentKind) -> &'static [&'static str] {
    match kind {
        AgentKind::ClaudeCode => CLAUDE_EVENTS,
        AgentKind::Codex => CODEX_EVENTS,
        AgentKind::Cursor => CURSOR_EVENTS,
        AgentKind::GeminiCli => GEMINI_EVENTS,
    }
}

/// Where the hook config lives.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// `~/.<agent>/<config>`.
    #[default]
    User,
    /// `<project>/.<agent>/<config>`.
    Project(PathBuf),
    /// `<project>/.claude/settings.local.json` (Claude Code only).
    Local(PathBuf),
}

/// Options for [`install`] / [`uninstall`].
#[derive(Clone, Debug, Default)]
pub struct InstallOptions {
    pub scope: Scope,
    /// `None` = every detected agent.
    pub providers: Option<Vec<AgentKind>>,
    /// Binary to reference in hook commands; default: the running executable.
    pub binary_path: Option<PathBuf>,
    /// Compute and report everything, write nothing.
    pub dry_run: bool,
    /// Also remove the VibeMon legacy `~/.vibemon/notify.sh` entries
    /// ([`is_legacy_vibemon_hook_object`]). Never touches `~/.vibemon` itself.
    pub remove_legacy: bool,
    /// Claude Code config directories to use instead of detecting them
    /// (`--claude-config-dir`, repeatable). Empty = every existing one
    /// ([`crate::agents::claude_config_dirs`]). A directory that does not
    /// exist is reported as skipped, never created. User scope only.
    pub claude_config_dirs: Vec<PathBuf>,
}

/// Result of processing one agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub enum Outcome {
    /// Entries were added to a config that had none of ours.
    Installed,
    /// The config already contained exactly the current entry set.
    AlreadyCurrent,
    /// Old entries (stale path / old event set) were replaced.
    Updated,
    /// Our entries were removed (uninstall).
    Removed,
    /// Nothing was done, with a reason.
    Skipped(String),
    /// The operation failed, with the error text.
    Failed(String),
}

/// Per-agent line item in an [`InstallReport`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InstallAction {
    pub agent: AgentKind,
    pub config_path: PathBuf,
    pub outcome: Outcome,
    pub backup_path: Option<PathBuf>,
    pub entries_added: usize,
    pub entries_removed: usize,
    /// VibeMon legacy entries removed (only with `remove_legacy`).
    pub legacy_removed: usize,
    pub notes: Vec<String>,
}

impl InstallAction {
    fn new(agent: AgentKind, config_path: &Path, outcome: Outcome) -> Self {
        Self {
            agent,
            config_path: config_path.to_path_buf(),
            outcome,
            backup_path: None,
            entries_added: 0,
            entries_removed: 0,
            legacy_removed: 0,
            notes: Vec::new(),
        }
    }
}

/// Everything [`install`] / [`uninstall`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct InstallReport {
    pub actions: Vec<InstallAction>,
}

impl InstallReport {
    /// True when any action failed.
    pub fn has_failures(&self) -> bool {
        self.actions
            .iter()
            .any(|a| matches!(a.outcome, Outcome::Failed(_)))
    }
}

/// Counters returned by the pure merge/remove functions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct MergeStats {
    pub added: usize,
    pub removed: usize,
    /// Whether the value differs from its state before the call.
    pub changed: bool,
}

// ---------------------------------------------------------------------------
// Command line construction and recognition
// ---------------------------------------------------------------------------

/// The hook command for an agent, referencing `binary` by absolute path.
///
/// POSIX: `'<abs path>' hook <provider-id>`. Windows: `"<abs path>" hook
/// <provider-id>`; for Claude Code only, prefixed with the PowerShell call
/// operator (`& "..." hook claude-code`) because PowerShell treats a command
/// line that *starts* with a quoted string as a string expression rather than
/// an invocation.
pub fn hook_command(binary: &Path, kind: AgentKind) -> String {
    let quoted = quote_for_shell(binary);
    let id = kind.provider_id();
    // The dedicated hook binary takes the provider id directly.
    let verb = if is_hook_binary(binary) { "" } else { "hook " };
    if is_windows() && kind == AgentKind::ClaudeCode {
        format!("& {quoted} {verb}{id}")
    } else {
        format!("{quoted} {verb}{id}")
    }
}

/// True when `cmd` is one of ours: after stripping quotes (and a leading
/// PowerShell `&`), the first token's file name is `attempt` / `attempt.exe`
/// and the second token is `hook`; or the command carries [`LEGACY_MARKER`].
pub fn is_attempt_hook_command(cmd: &str) -> bool {
    if cmd.contains(LEGACY_MARKER) {
        return true;
    }
    let Some((first, rest)) = first_token(cmd) else {
        return false;
    };
    let name = file_name_of(&first);
    let next = rest.split_whitespace().next();
    let is_attempt = name == "attempt" || name.eq_ignore_ascii_case("attempt.exe");
    if is_attempt {
        return next == Some("hook");
    }
    let is_hook_bin = name == HOOK_BINARY_NAME || name.eq_ignore_ascii_case("attempt-hook.exe");
    is_hook_bin && next.is_some_and(|id| AgentKind::from_provider_id(id).is_some())
}

/// The binary path referenced by one of our commands (unquoted), if `cmd`
/// is recognised by [`is_attempt_hook_command`].
pub fn hook_command_binary(cmd: &str) -> Option<String> {
    if !is_attempt_hook_command(cmd) {
        return None;
    }
    first_token(cmd).map(|(first, _)| first)
}

/// Split the first shell token (handling `'...'` with the `'\''` idiom and
/// `"..."`) from the remainder. Returns `None` for an empty command.
fn first_token(cmd: &str) -> Option<(String, &str)> {
    let mut s = cmd.trim_start();
    // PowerShell call operator: `& "C:\x\attempt.exe" hook ...`.
    if let Some(rest) = s.strip_prefix('&')
        && rest.starts_with(|c: char| c.is_whitespace())
    {
        s = rest.trim_start();
    }
    let first = s.chars().next()?;
    match first {
        '\'' => {
            let mut token = String::new();
            let mut rest = &s[1..];
            loop {
                let end = rest.find('\'')?;
                token.push_str(&rest[..end]);
                rest = &rest[end + 1..];
                // `'\''` idiom: closing quote, escaped quote, reopening quote.
                if let Some(after) = rest.strip_prefix("\\''") {
                    token.push('\'');
                    rest = after;
                } else {
                    return Some((token, rest));
                }
            }
        }
        '"' => {
            let body = &s[1..];
            let end = body.find('"')?;
            Some((body[..end].to_string(), &body[end + 1..]))
        }
        _ => {
            let end = s.find(char::is_whitespace).unwrap_or(s.len());
            Some((s[..end].to_string(), &s[end..]))
        }
    }
}

fn file_name_of(token: &str) -> &str {
    token.rsplit(['/', '\\']).next().unwrap_or(token)
}

/// True when a hook object (the innermost `{command: ...}` map) is ours.
pub fn is_attempt_hook_object(hook: &Value) -> bool {
    let Some(obj) = hook.as_object() else {
        return false;
    };
    if let Some(cmd) = obj.get("command").and_then(Value::as_str)
        && is_attempt_hook_command(cmd)
    {
        return true;
    }
    obj.get("name")
        .and_then(Value::as_str)
        .is_some_and(|n| n.starts_with(GEMINI_NAME_PREFIX))
}

/// True when a hook object was written by the VibeMon legacy thin client:
/// its command runs `~/.vibemon/notify.sh` (any home path, any event), or —
/// Gemini — its name starts with `vibemon-`. Our own entries never match.
pub fn is_legacy_vibemon_hook_object(hook: &Value) -> bool {
    let Some(obj) = hook.as_object() else {
        return false;
    };
    if is_attempt_hook_object(hook) {
        return false;
    }
    if let Some(cmd) = obj.get("command").and_then(Value::as_str)
        && LEGACY_VIBEMON_SCRIPTS.iter().any(|s| cmd.contains(s))
    {
        return true;
    }
    obj.get("name")
        .and_then(Value::as_str)
        .is_some_and(|n| n.starts_with(LEGACY_VIBEMON_NAME_PREFIX))
}

// ---------------------------------------------------------------------------
// Pure JSON shaping
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// `hooks.<Event>: [ { matcher?, hooks: [ {command...} ] } ]`
    Nested,
    /// `hooks.<event>: [ {command...} ]` (Cursor)
    Flat,
}

fn shape(kind: AgentKind) -> Shape {
    match kind {
        AgentKind::Cursor => Shape::Flat,
        _ => Shape::Nested,
    }
}

/// The innermost hook object for an event, exactly as the agent expects it.
pub fn hook_object(kind: AgentKind, event: &str, cmd: &str) -> Value {
    match kind {
        AgentKind::ClaudeCode => json!({
            "type": "command",
            "command": cmd,
            "timeout": CLAUDE_TIMEOUT_SECS,
        }),
        AgentKind::Codex => {
            let timeout = if matches!(event, "SessionEnd" | "Interrupt") {
                CODEX_SESSION_END_TIMEOUT_SECS
            } else {
                CODEX_TIMEOUT_SECS
            };
            // Codex rejects unknown fields: exactly type / command / timeout.
            json!({
                "type": "command",
                "command": cmd,
                "timeout": timeout,
            })
        }
        AgentKind::Cursor => json!({
            "command": cmd,
            "timeout": CURSOR_TIMEOUT_SECS,
        }),
        AgentKind::GeminiCli => json!({
            "name": format!("{GEMINI_NAME_PREFIX}{}", event.to_ascii_lowercase()),
            "type": "command",
            "command": cmd,
            "timeout": GEMINI_TIMEOUT_MILLIS,
        }),
    }
}

/// The base document written when no config file exists yet.
pub fn default_base(kind: AgentKind) -> Value {
    match kind {
        AgentKind::Cursor => json!({ "version": 1 }),
        _ => json!({}),
    }
}

/// The complete config a fresh install produces (pure; for tests and dry runs).
pub fn planned_config(kind: AgentKind, cmd: &str) -> Value {
    let mut v = default_base(kind);
    merge_into(kind, &mut v, cmd);
    v
}

/// Remove every AttemptDB entry from `existing` and insert the current set.
/// Pure. `existing` must be an object (or `null`, treated as `{}`); anything
/// else is left untouched with `changed == false`.
pub fn merge_into(kind: AgentKind, existing: &mut Value, cmd: &str) -> MergeStats {
    if existing.is_null() {
        *existing = default_base(kind);
    }
    if !existing.is_object() {
        return MergeStats::default();
    }
    let before = existing.clone();
    // Current events keep their (now empty) slot so the entry is re-inserted
    // in place: a repeated install is a no-op and key order never drifts.
    // Events we no longer install are dropped when we emptied them.
    let removal = remove_matching(kind, existing, Prune::Obsolete, is_attempt_hook_object);
    let added = add_ours(kind, existing, cmd, &removal.insert_at);
    MergeStats {
        added,
        removed: removal.removed,
        changed: *existing != before,
    }
}

/// Remove every AttemptDB entry from `existing`. Pure.
///
/// An event array (and, transitively, the `hooks` object) is dropped only
/// when removing our entries emptied it; arrays and objects that contained
/// none of our entries are never touched, so pre-existing empty arrays for
/// events we do not install survive untouched. The one case that cannot be
/// distinguished without extra state is an empty array that pre-existed for
/// an event we *do* install: it is dropped as well, which is semantically
/// identical for every agent (an empty array means "no hooks").
pub fn remove_from(kind: AgentKind, existing: &mut Value) -> MergeStats {
    if !existing.is_object() {
        return MergeStats::default();
    }
    let before = existing.clone();
    let removal = remove_matching(kind, existing, Prune::All, is_attempt_hook_object);
    MergeStats {
        added: 0,
        removed: removal.removed,
        changed: *existing != before,
    }
}

/// Remove every VibeMon legacy entry from `existing` (see
/// [`is_legacy_vibemon_hook_object`]); ours and the user's own hooks are
/// untouched. Event arrays emptied by this are dropped, as in
/// [`remove_from`]. Pure.
pub fn remove_legacy_from(kind: AgentKind, existing: &mut Value) -> MergeStats {
    if !existing.is_object() {
        return MergeStats::default();
    }
    let before = existing.clone();
    let removal = remove_matching(kind, existing, Prune::All, is_legacy_vibemon_hook_object);
    MergeStats {
        added: 0,
        removed: removal.removed,
        changed: *existing != before,
    }
}

#[derive(Default)]
struct Removal {
    removed: usize,
    /// event -> index (in the post-removal array) where our first entry was.
    insert_at: BTreeMap<String, usize>,
}

/// What to do with event arrays that removing our entries left empty.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Prune {
    /// Drop only events we no longer install (ghosts from older versions);
    /// keep current events in place so the merge can refill the same slot.
    Obsolete,
    /// Drop every emptied event, and the `hooks` object if that emptied it.
    All,
}

/// Strip every hook object matching `pred` from every event.
fn remove_matching(
    kind: AgentKind,
    root: &mut Value,
    prune: Prune,
    pred: fn(&Value) -> bool,
) -> Removal {
    let mut out = Removal::default();
    let Some(root_obj) = root.as_object_mut() else {
        return out;
    };
    let Some(hooks) = root_obj.get_mut("hooks").and_then(Value::as_object_mut) else {
        return out;
    };
    let mut emptied = Vec::new();
    for (event, arr) in hooks.iter_mut() {
        let Some(entries) = arr.as_array_mut() else {
            continue;
        };
        let (removed, insert_at) = match shape(kind) {
            Shape::Flat => strip_flat(entries, pred),
            Shape::Nested => strip_nested(entries, pred),
        };
        if removed > 0 {
            out.removed += removed;
            out.insert_at.insert(event.clone(), insert_at);
            let current = events_for(kind).contains(&event.as_str());
            if entries.is_empty() && (prune == Prune::All || !current) {
                emptied.push(event.clone());
            }
        }
    }
    for event in &emptied {
        // `shift_remove` keeps the order of the remaining keys (`remove` would
        // swap the last key into the hole).
        hooks.shift_remove(event);
    }
    if prune == Prune::All && hooks.is_empty() && !emptied.is_empty() {
        root_obj.shift_remove("hooks");
    }
    out
}

/// Cursor: the event array holds hook objects directly.
fn strip_flat(entries: &mut Vec<Value>, pred: fn(&Value) -> bool) -> (usize, usize) {
    let mut removed = 0;
    let mut kept = 0;
    let mut insert_at = None;
    entries.retain(|hook| {
        if pred(hook) {
            removed += 1;
            insert_at.get_or_insert(kept);
            false
        } else {
            kept += 1;
            true
        }
    });
    (removed, insert_at.unwrap_or(kept))
}

/// Claude / Codex / Gemini: the event array holds `{matcher?, hooks: [...]}`
/// groups. Matching hook objects are removed from every group; a group that
/// ends up empty because of that is dropped.
fn strip_nested(entries: &mut Vec<Value>, pred: fn(&Value) -> bool) -> (usize, usize) {
    let mut removed = 0;
    let mut kept = 0;
    let mut insert_at = None;
    entries.retain_mut(|entry| {
        let Some(hooks) = entry
            .as_object_mut()
            .and_then(|o| o.get_mut("hooks"))
            .and_then(Value::as_array_mut)
        else {
            kept += 1;
            return true;
        };
        let before = hooks.len();
        hooks.retain(|h| !pred(h));
        let here = before - hooks.len();
        if here > 0 {
            removed += here;
            insert_at.get_or_insert(kept);
            if hooks.is_empty() {
                return false;
            }
        }
        kept += 1;
        true
    });
    (removed, insert_at.unwrap_or(kept))
}

fn add_ours(
    kind: AgentKind,
    root: &mut Value,
    cmd: &str,
    insert_at: &BTreeMap<String, usize>,
) -> usize {
    let Some(root_obj) = root.as_object_mut() else {
        return 0;
    };
    if kind == AgentKind::Cursor && !root_obj.contains_key("version") {
        // Put `version` first, as Cursor's own examples do.
        let mut fresh = Map::new();
        fresh.insert("version".to_string(), json!(1));
        fresh.extend(std::mem::take(root_obj));
        *root_obj = fresh;
    }
    match root_obj.get("hooks") {
        Some(Value::Object(_)) => {}
        Some(_) => return 0, // validated earlier; never clobber user data
        None => {
            root_obj.insert("hooks".to_string(), Value::Object(Map::new()));
        }
    }
    let hooks = root_obj
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
        .expect("hooks is an object");
    let mut added = 0;
    for event in events_for(kind) {
        let arr = hooks
            .entry(*event)
            .or_insert_with(|| Value::Array(Vec::new()));
        let Some(arr) = arr.as_array_mut() else {
            continue;
        };
        let item = match shape(kind) {
            Shape::Flat => hook_object(kind, event, cmd),
            Shape::Nested => json!({ "hooks": [hook_object(kind, event, cmd)] }),
        };
        let pos = insert_at
            .get(*event)
            .copied()
            .unwrap_or(arr.len())
            .min(arr.len());
        arr.insert(pos, item);
        added += 1;
    }
    added
}

/// Check that an existing config can be merged into without destroying
/// anything (top-level object, `hooks` is an object, our events are arrays).
pub fn validate_config_shape(kind: AgentKind, root: &Value) -> Result<(), String> {
    let obj = root
        .as_object()
        .ok_or_else(|| "top-level JSON value is not an object".to_string())?;
    if let Some(h) = obj.get("hooks") {
        let hooks = h
            .as_object()
            .ok_or_else(|| "\"hooks\" is not a JSON object".to_string())?;
        for event in events_for(kind) {
            if let Some(v) = hooks.get(*event)
                && !v.is_array()
            {
                return Err(format!("\"hooks.{event}\" is not an array"));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Formatting preservation
// ---------------------------------------------------------------------------

/// Indentation style of a JSON file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Indent {
    Spaces(usize),
    Tabs,
}

impl Default for Indent {
    fn default() -> Self {
        Indent::Spaces(2)
    }
}

/// Detect the indentation unit from the first indented line. Defaults to two
/// spaces (also for minified single-line files).
pub fn detect_indent(text: &str) -> Indent {
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.len() == line.len() {
            continue;
        }
        let ws = &line[..line.len() - trimmed.len()];
        return if ws.starts_with('\t') {
            Indent::Tabs
        } else {
            Indent::Spaces(ws.chars().count().max(1))
        };
    }
    Indent::default()
}

/// Formatting details recovered from an existing file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    pub indent: Indent,
    pub trailing_newline: bool,
    pub crlf: bool,
    /// The file starts with a UTF-8 byte order mark (Windows editors write
    /// one). It is not part of the JSON, and it is written back.
    pub bom: bool,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            indent: Indent::default(),
            trailing_newline: true,
            crlf: false,
            bom: false,
        }
    }
}

/// The UTF-8 byte order mark.
pub const UTF8_BOM: &str = "\u{feff}";

impl Style {
    pub fn detect(text: &str) -> Self {
        let bom = text.starts_with(UTF8_BOM);
        let text = text.strip_prefix(UTF8_BOM).unwrap_or(text);
        Self {
            indent: detect_indent(text),
            trailing_newline: text.ends_with('\n') || text.trim().is_empty(),
            crlf: text.contains("\r\n"),
            bom,
        }
    }
}

/// `text` with `//` and `/* */` comments blanked out (strings are respected,
/// newlines kept). Used only to tell "this is JSON with comments" from "this
/// is not JSON": the result is never written anywhere.
pub fn strip_json_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let (mut in_string, mut escaped) = (false, false);
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut previous = '\0';
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                    }
                    if previous == '*' && n == '/' {
                        break;
                    }
                    previous = n;
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// Why `text` is not JSON, said the way a person can act on it: JSON with
/// comments (Gemini CLI accepts them) is named as such, because the fix is
/// different from a syntax error.
pub fn explain_json_failure(path: &Path, text: &str, error: &serde_json::Error) -> String {
    let body = text.strip_prefix(UTF8_BOM).unwrap_or(text);
    if serde_json::from_str::<Value>(&strip_json_comments(body)).is_ok() {
        // The path is not repeated: every line this error appears on names
        // the file already (the report's config path).
        return "the file contains comments (// or /* */), which this installer cannot rewrite without losing them; nothing was changed. Move the comments out of the file and run `attempt setup` again."
            .to_string();
    }
    format!(
        "{} is not valid JSON ({error}); refusing to modify it. Fix or move the file and re-run.",
        path.display()
    )
}

/// Serialise with the given style. Newlines inside strings are escaped by the
/// JSON encoder, so raw `\n` bytes are formatting only and CRLF conversion is
/// safe.
///
/// Known limit, deliberately not worked around: the document goes through
/// `serde_json::Value`, so a number is written back in its canonical form, not
/// as the user typed it. `1E3` becomes `1000.0`, `1.50` becomes `1.5`, and an
/// integer beyond `u64`/`i64` is held as a float and loses digits. The value
/// is the same for every consumer that reads JSON numbers as doubles (the
/// agents' settings do), and keeping the original spelling would need an
/// `arbitrary_precision` build of serde_json (slower, a different `Value`) or
/// a text-level editor. Comments are the case that cannot be tolerated, and
/// those are refused by name instead (`explain_json_failure`).
pub fn render_json(value: &Value, style: Style) -> anyhow::Result<Vec<u8>> {
    let indent: Vec<u8> = match style.indent {
        Indent::Tabs => b"\t".to_vec(),
        Indent::Spaces(n) => vec![b' '; n],
    };
    let mut buf = Vec::with_capacity(4096);
    if style.bom {
        buf.extend_from_slice(UTF8_BOM.as_bytes());
    }
    {
        let formatter = serde_json::ser::PrettyFormatter::with_indent(&indent);
        let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
        value
            .serialize(&mut ser)
            .context("serialising config JSON")?;
    }
    if style.trailing_newline {
        buf.push(b'\n');
    }
    if style.crlf {
        let mut out = Vec::with_capacity(buf.len() + 64);
        for b in buf {
            if b == b'\n' {
                out.push(b'\r');
            }
            out.push(b);
        }
        buf = out;
    }
    Ok(buf)
}

// ---------------------------------------------------------------------------
// File operations
// ---------------------------------------------------------------------------

struct Loaded {
    value: Value,
    style: Style,
    existed: bool,
}

fn load_config(kind: AgentKind, path: &Path) -> anyhow::Result<Loaded> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Loaded {
                value: default_base(kind),
                style: Style::default(),
                existed: false,
            });
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let text = String::from_utf8(bytes).map_err(|_| {
        anyhow!(
            "{} is not valid UTF-8; refusing to modify it",
            path.display()
        )
    })?;
    let style = Style::detect(&text);
    if text.trim_start_matches(UTF8_BOM).trim().is_empty() {
        // An empty file is treated like a missing one (but still backed up).
        return Ok(Loaded {
            value: default_base(kind),
            style,
            existed: true,
        });
    }
    let body = text.strip_prefix(UTF8_BOM).unwrap_or(&text);
    let value: Value =
        serde_json::from_str(body).map_err(|e| anyhow!(explain_json_failure(path, &text, &e)))?;
    validate_config_shape(kind, &value)
        .map_err(|why| anyhow!("{}: {why}; refusing to modify it", path.display()))?;
    Ok(Loaded {
        value,
        style,
        existed: true,
    })
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{name}{suffix}"))
}

/// Exclusive advisory lock on `<config>.attemptdb.lock`, released on drop.
///
/// On Unix the lock file is unlinked again on release, so a config directory
/// does not keep a stale `.attemptdb.lock` per file we ever touched. Unlinking
/// races with a process that has already opened the file and is queued on the
/// lock; [`lock_config`] closes that race by checking, once it holds the lock,
/// that the path still names the file it locked, and retrying otherwise. On
/// Windows the file is left in place (deleting a file another process has
/// open makes new opens fail until every handle closes).
pub(crate) struct ConfigLock {
    file: File,
    #[cfg_attr(not(unix), allow(dead_code))]
    path: PathBuf,
}

impl Drop for ConfigLock {
    fn drop(&mut self) {
        // Unlink while still holding the lock: whoever was queued behind us
        // then locks a file that no longer has a name and starts over.
        #[cfg(unix)]
        let _ = fs::remove_file(&self.path);
        let _ = self.file.unlock();
    }
}

pub(crate) fn lock_config(path: &Path) -> anyhow::Result<ConfigLock> {
    let lock_path = sibling(path, ".attemptdb.lock");
    // Only the Unix branch below retries; elsewhere the first pass returns.
    #[cfg_attr(not(unix), allow(clippy::never_loop))]
    for _ in 0..200 {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("opening lock file {}", lock_path.display()))?;
        file.lock()
            .with_context(|| format!("locking {}", lock_path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let same = match (file.metadata(), fs::symlink_metadata(&lock_path)) {
                (Ok(held), Ok(named)) => held.dev() == named.dev() && held.ino() == named.ino(),
                _ => false,
            };
            if !same {
                // The previous holder unlinked it after we opened it.
                let _ = file.unlock();
                continue;
            }
        }
        return Ok(ConfigLock {
            file,
            path: lock_path,
        });
    }
    bail!("could not take the lock {}", lock_path.display())
}

/// Copy `path` to `<path>.attemptdb.bak-<unix ts>` and prune old backups.
pub(crate) fn backup_config(path: &Path) -> anyhow::Result<PathBuf> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut dst = sibling(path, &format!(".attemptdb.bak-{ts}"));
    let mut n = 1;
    while dst.exists() {
        dst = sibling(path, &format!(".attemptdb.bak-{ts}-{n}"));
        n += 1;
    }
    fs::copy(path, &dst)
        .with_context(|| format!("backing up {} to {}", path.display(), dst.display()))?;
    prune_backups(path);
    Ok(dst)
}

/// The newest `<file>.attemptdb.bak-*` backup of `path`, if any.
pub fn newest_backup(path: &Path) -> Option<PathBuf> {
    let (dir, name) = (path.parent()?, path.file_name()?);
    let prefix = format!("{}.attemptdb.bak-", name.to_string_lossy());
    let mut backups: Vec<(u64, String, PathBuf)> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let file_name = e.file_name().to_string_lossy().into_owned();
            let secs: u64 = file_name
                .strip_prefix(&prefix)?
                .split('-')
                .next()?
                .parse()
                .ok()?;
            Some((secs, file_name, e.path()))
        })
        .collect();
    backups.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    backups.into_iter().next().map(|(_, _, p)| p)
}

fn prune_backups(path: &Path) {
    prune_backups_keeping(path, BACKUPS_TO_KEEP);
}

/// Delete all but the `keep` newest `<file>.attemptdb.bak-*` backups of
/// `path`. An uninstall keeps one: enough to undo it, nothing piling up.
pub(crate) fn prune_backups_keeping(path: &Path, keep: usize) {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return;
    };
    let prefix = format!("{}.attemptdb.bak-", name.to_string_lossy());
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut backups: Vec<(u64, String, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let file_name = e.file_name().to_string_lossy().into_owned();
            let rest = file_name.strip_prefix(&prefix)?;
            let secs: u64 = rest.split('-').next()?.parse().ok()?;
            Some((secs, file_name.clone(), e.path()))
        })
        .collect();
    backups.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    for (_, _, p) in backups.into_iter().skip(keep) {
        let _ = fs::remove_file(p);
    }
}

/// Where a write to `path` must land: `path` itself, or — when `path` is a
/// symlink — the file the link finally names. Relative links resolve against
/// the directory that holds the link. A dangling link resolves to the missing
/// target (the write then creates it, provided its directory exists).
pub(crate) fn resolve_write_target(path: &Path) -> anyhow::Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..40 {
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let link = fs::read_link(&current)
                    .with_context(|| format!("reading the symlink {}", current.display()))?;
                current = if link.is_absolute() {
                    link
                } else {
                    current.parent().map(|dir| dir.join(&link)).unwrap_or(link)
                };
            }
            _ => return Ok(current),
        }
    }
    bail!("{} is a symlink loop", path.display())
}

/// Write `bytes` to `path` via `<target>.attemptdb.tmp` + rename, where
/// `target` is `path` with symlinks resolved ([`resolve_write_target`]): the
/// temp file lives next to the file that is replaced, so the rename is atomic
/// and a symlinked config stays a symlink. The target's permission bits are
/// kept.
///
/// Unix: `rename(2)` is atomic and the directory is fsynced afterwards.
/// Windows: `std::fs::rename` maps to `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`
/// which replaces in place; if that is refused (e.g. the target is open with
/// a conflicting share mode) we fall back to remove-then-rename, which has a
/// short window in which the config file does not exist.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let target = resolve_write_target(path)?;
    let tmp = sibling(&target, ".attemptdb.tmp");
    let result = (|| -> anyhow::Result<()> {
        {
            let mut options = fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut f = options
                .open(&tmp)
                .with_context(|| format!("creating {}", tmp.display()))?;
            if let Ok(meta) = fs::metadata(&target) {
                f.set_permissions(meta.permissions())?;
            }
            f.write_all(bytes)?;
            f.sync_all()?;
        }
        if let Ok(meta) = fs::metadata(&target) {
            let _ = fs::set_permissions(&tmp, meta.permissions());
        }
        replace_file(&tmp, &target)?;
        #[cfg(unix)]
        if let Some(dir) = target.parent()
            && let Ok(d) = File::open(dir)
        {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("replacing {}", path.display()))
}

#[cfg(not(windows))]
fn replace_file(tmp: &Path, path: &Path) -> std::io::Result<()> {
    fs::rename(tmp, path)
}

#[cfg(windows)]
fn replace_file(tmp: &Path, path: &Path) -> std::io::Result<()> {
    match fs::rename(tmp, path) {
        Ok(()) => Ok(()),
        Err(_) => {
            // Non-atomic window: the target is absent between these two calls.
            if path.exists() {
                fs::remove_file(path)?;
            }
            fs::rename(tmp, path)
        }
    }
}

fn provider_notes(kind: AgentKind) -> Vec<String> {
    match kind {
        AgentKind::ClaudeCode => vec![
            "Claude Code reloads settings automatically; if events do not appear in `attempt doctor` after the next tool call, restart the session.".into(),
        ],
        AgentKind::Codex => vec![
            "Codex requires approval of new or changed hooks: run /hooks inside Codex and trust the AttemptDB entries.".into(),
        ],
        AgentKind::Cursor => vec!["Restart Cursor (or reload the window) so it re-reads hooks.json.".into()],
        AgentKind::GeminiCli => vec!["Gemini CLI reads settings.json at start-up: restart running sessions.".into()],
    }
}

/// Install (or upgrade / repair) our entries in one config file.
///
/// The parent directory is created when missing; callers are responsible for
/// only calling this for detected agents (see [`install`]).
pub fn install_to(
    kind: AgentKind,
    config_path: &Path,
    cmd: &str,
    dry_run: bool,
) -> anyhow::Result<InstallAction> {
    install_to_with(kind, config_path, cmd, dry_run, false)
}

fn legacy_note(removed: usize) -> String {
    format!(
        "removed {removed} VibeMon legacy hook entr{} (~/.vibemon/notify.sh); the script itself was not touched — delete ~/.vibemon once every agent is migrated",
        if removed == 1 { "y" } else { "ies" }
    )
}

/// [`install_to`] that can also strip VibeMon legacy entries first, so the
/// two capture paths never run side by side after a migration.
pub fn install_to_with(
    kind: AgentKind,
    config_path: &Path,
    cmd: &str,
    dry_run: bool,
    remove_legacy: bool,
) -> anyhow::Result<InstallAction> {
    let parent = config_path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", config_path.display()))?;
    if !dry_run {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let _lock = if dry_run || !parent.is_dir() {
        None
    } else {
        Some(lock_config(config_path)?)
    };

    let loaded = load_config(kind, config_path)?;
    let mut value = loaded.value.clone();
    let legacy = if remove_legacy {
        remove_legacy_from(kind, &mut value)
    } else {
        MergeStats::default()
    };
    let stats = merge_into(kind, &mut value, cmd);
    let mut action = InstallAction::new(kind, config_path, Outcome::AlreadyCurrent);
    action.entries_added = stats.added;
    action.entries_removed = stats.removed;
    action.legacy_removed = legacy.removed;
    if legacy.removed > 0 {
        action.notes.push(legacy_note(legacy.removed));
    }
    if !stats.changed && !legacy.changed {
        return Ok(action);
    }
    let bytes = render_json(&value, loaded.style)?;
    let outcome = if stats.removed > 0 || legacy.removed > 0 {
        Outcome::Updated
    } else {
        Outcome::Installed
    };
    if dry_run {
        action.outcome = outcome;
        action.notes.push("dry run: no files were written".into());
        action.notes.extend(provider_notes(kind));
        return Ok(action);
    }
    if loaded.existed {
        action.backup_path = Some(backup_config(config_path)?);
    }
    write_atomically(config_path, &bytes)?;
    action.outcome = outcome;
    action.notes.extend(provider_notes(kind));
    Ok(action)
}

/// Remove our entries from one config file. Never deletes the file itself.
pub fn uninstall_from(
    kind: AgentKind,
    config_path: &Path,
    dry_run: bool,
) -> anyhow::Result<InstallAction> {
    uninstall_from_with(kind, config_path, dry_run, false)
}

/// [`uninstall_from`] that can also strip VibeMon legacy entries.
pub fn uninstall_from_with(
    kind: AgentKind,
    config_path: &Path,
    dry_run: bool,
    remove_legacy: bool,
) -> anyhow::Result<InstallAction> {
    if !config_path.is_file() {
        return Ok(InstallAction::new(
            kind,
            config_path,
            Outcome::Skipped("config file does not exist".into()),
        ));
    }
    let _lock = if dry_run {
        None
    } else {
        Some(lock_config(config_path)?)
    };
    let loaded = load_config(kind, config_path)?;
    let mut value = loaded.value.clone();
    let legacy = if remove_legacy {
        remove_legacy_from(kind, &mut value)
    } else {
        MergeStats::default()
    };
    let stats = remove_from(kind, &mut value);
    let mut action = InstallAction::new(kind, config_path, Outcome::AlreadyCurrent);
    action.entries_removed = stats.removed;
    action.legacy_removed = legacy.removed;
    if legacy.removed > 0 {
        action.notes.push(legacy_note(legacy.removed));
    }
    if !stats.changed && !legacy.changed {
        action.notes.push("no AttemptDB hook entries found".into());
        return Ok(action);
    }
    if dry_run {
        action.outcome = Outcome::Removed;
        action.notes.push("dry run: no files were written".into());
        return Ok(action);
    }
    let bytes = render_json(&value, loaded.style)?;
    action.backup_path = Some(backup_config(config_path)?);
    write_atomically(config_path, &bytes)?;
    // One backup is enough to undo an uninstall; the five that install and
    // upgrade keep would otherwise outlive the tool.
    prune_backups_keeping(config_path, 1);
    action.outcome = Outcome::Removed;
    if kind == AgentKind::Codex {
        action
            .notes
            .push("Codex keeps trust records for removed hooks in ~/.codex/config.toml [hooks.state]; they are harmless.".into());
    }
    Ok(action)
}

/// Resolve the config path for an agent under a scope. `detected` supplies
/// the user-scope path (which honours `CLAUDE_CONFIG_DIR` / `CODEX_HOME`).
pub fn config_path_for(
    kind: AgentKind,
    scope: &Scope,
    detected: Option<&DetectedAgent>,
) -> Option<PathBuf> {
    match scope {
        Scope::User => detected
            .map(|d| d.config_path.clone())
            .or_else(|| kind.user_config_path()),
        Scope::Project(dir) => Some(kind.project_config_path(dir)),
        Scope::Local(dir) => kind.local_config_path(dir),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Install,
    Uninstall,
}

/// Install hooks for every requested (default: every detected) agent.
pub fn install(opts: &InstallOptions) -> anyhow::Result<InstallReport> {
    run(opts, Mode::Install)
}

/// Remove our hooks from every requested (default: every detected) agent.
pub fn uninstall(opts: &InstallOptions) -> anyhow::Result<InstallReport> {
    run(opts, Mode::Uninstall)
}

fn run(opts: &InstallOptions, mode: Mode) -> anyhow::Result<InstallReport> {
    // The path goes into other tools' config files and has to outlive the
    // next upgrade: a symlink kept current by a package manager stays as it is.
    let binary = match &opts.binary_path {
        Some(p) => stable_display_path(p),
        None => preferred_hook_binary(current_exe_stable_path()),
    };
    if mode == Mode::Install && !binary.is_absolute() {
        bail!(
            "cannot determine an absolute path for the attempt binary ({})",
            binary.display()
        );
    }
    // Detect BEFORE touching the filesystem; no version probes (slow, unneeded).
    let detected = detect_agents_with(&DetectOptions {
        probe_versions: false,
        claude_config_dirs: opts.claude_config_dirs.clone(),
        ..DetectOptions::default()
    });
    let mut kinds: Vec<AgentKind> = match &opts.providers {
        Some(list) => list.clone(),
        None => detected.iter().map(|d| d.kind).collect(),
    };
    // Directories named with `--claude-config-dir` are reported even when none
    // of them exists (so no Claude Code was detected): a typo must not look
    // like "no coding agents here".
    if opts.providers.is_none()
        && opts.scope == Scope::User
        && !opts.claude_config_dirs.is_empty()
        && !kinds.contains(&AgentKind::ClaudeCode)
    {
        kinds.insert(0, AgentKind::ClaudeCode);
    }
    // So is a home directory an environment variable points at that is not there.
    if opts.providers.is_none() && opts.scope == Scope::User {
        for kind in AgentKind::ALL {
            if kind.missing_home_of_installed_agent().is_some() && !kinds.contains(&kind) {
                kinds.push(kind);
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    kinds.retain(|k| seen.insert(*k));

    let mut report = InstallReport::default();
    for kind in kinds {
        let mut dets: Vec<&DetectedAgent> = detected.iter().filter(|d| d.kind == kind).collect();
        if kind == AgentKind::ClaudeCode && opts.scope == Scope::User {
            // A directory the user named that is not there is a typo or a
            // directory Claude Code has not made yet: say so, never create it.
            for dir in opts.claude_config_dirs.iter().filter(|d| !d.is_dir()) {
                report.actions.push(InstallAction::new(
                    kind,
                    &dir.join(kind.config_file_name()),
                    Outcome::Skipped(format!("{} does not exist; not creating it", dir.display())),
                ));
            }
        }
        // Likewise a home directory an environment variable points at.
        let dirs_named = kind == AgentKind::ClaudeCode && !opts.claude_config_dirs.is_empty();
        if opts.scope == Scope::User
            && !dirs_named
            && let Some((var, dir)) = kind.missing_home_of_installed_agent()
        {
            report.actions.push(InstallAction::new(
                kind,
                &dir.join(kind.config_file_name()),
                Outcome::Skipped(if mode == Mode::Install {
                    format!(
                        "{var} is set to {}, which does not exist; not creating it (create it, or run {} once with it, then repeat this)",
                        dir.display(),
                        kind.display_name()
                    )
                } else {
                    format!(
                        "{var} is set to {}, which does not exist; nothing to remove there",
                        dir.display()
                    )
                }),
            ));
            if dets.is_empty() {
                continue;
            }
        }
        if dets.is_empty() {
            if kind == AgentKind::ClaudeCode
                && opts.scope == Scope::User
                && !opts.claude_config_dirs.is_empty()
            {
                continue; // every named directory was reported above
            }
            let path = kind.user_config_path().unwrap_or_default();
            report.actions.push(InstallAction::new(
                kind,
                &path,
                Outcome::Skipped(format!(
                    "{} not detected (no {} directory and no `{}` on PATH); not creating it",
                    kind.display_name(),
                    kind.dir_name(),
                    kind.binary_name()
                )),
            ));
            continue;
        }
        if opts.scope != Scope::User {
            // Project and local configs do not depend on which user-level
            // directory was found; one entry is enough, and one write.
            dets.truncate(1);
        }
        for det in dets {
            let Some(config_path) = config_path_for(kind, &opts.scope, Some(det)) else {
                report.actions.push(InstallAction::new(
                    kind,
                    &det.config_path,
                    Outcome::Skipped(format!(
                        "{} has no local-scope config file",
                        kind.display_name()
                    )),
                ));
                continue;
            };
            let result = match mode {
                Mode::Install => install_to_with(
                    kind,
                    &config_path,
                    &hook_command(&binary, kind),
                    opts.dry_run,
                    opts.remove_legacy,
                ),
                Mode::Uninstall => {
                    uninstall_from_with(kind, &config_path, opts.dry_run, opts.remove_legacy)
                }
            };
            report.actions.push(match result {
                Ok(action) => action,
                Err(e) => InstallAction::new(kind, &config_path, Outcome::Failed(format!("{e:#}"))),
            });
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscriptions_match_adapter_contracts_and_remove_behavior_overrides() {
        for kind in AgentKind::ALL {
            let provider = kind.provider_id().parse().unwrap();
            let adapter = attemptdb_adapters::adapter_for(&provider).unwrap();
            assert_eq!(events_for(kind), adapter.capture_events());
        }
        let mut config = json!({"hooks": {"WorktreeCreate": [{"hooks": [
            {"type": "command", "command": CMD},
            {"type": "command", "command": "create-my-worktree"}
        ]}]}});
        merge_into(AgentKind::ClaudeCode, &mut config, CMD);
        assert_eq!(
            config["hooks"]["WorktreeCreate"][0]["hooks"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            config["hooks"]["WorktreeCreate"][0]["hooks"][0]["command"],
            "create-my-worktree"
        );
        let mut ours_only = json!({"hooks": {"WorktreeCreate": [{"hooks": [
            {"type": "command", "command": CMD}
        ]}]}});
        merge_into(AgentKind::ClaudeCode, &mut ours_only, CMD);
        assert!(ours_only["hooks"].get("WorktreeCreate").is_none());
        assert!(!merge_into(AgentKind::ClaudeCode, &mut ours_only, CMD).changed);
        assert_eq!(
            hook_object(AgentKind::Codex, "Interrupt", CMD)["timeout"],
            3
        );
    }

    #[test]
    fn the_dedicated_hook_binary_is_recognised_rendered_and_preferred() {
        let hook_bin = Path::new("/opt/attemptdb/attempt-hook");
        let cmd = hook_command(hook_bin, AgentKind::Codex);
        assert!(
            !cmd.contains(" hook "),
            "no subcommand for attempt-hook: {cmd}"
        );
        assert!(cmd.ends_with(" codex"), "{cmd}");
        assert!(is_attempt_hook_command(&cmd));
        assert_eq!(
            hook_command_binary(&cmd).as_deref(),
            Some("/opt/attemptdb/attempt-hook")
        );
        // The provider id must follow; anything else is not ours.
        assert!(is_attempt_hook_command("'/x/attempt-hook' claude-code"));
        assert!(is_attempt_hook_command(
            "\"C:\\x\\attempt-hook.exe\" gemini-cli"
        ));
        assert!(!is_attempt_hook_command(
            "'/x/attempt-hook' hook claude-code"
        ));
        assert!(!is_attempt_hook_command("'/x/attempt-hook' --version"));
        // `attempt` keeps its `hook` verb.
        let cmd = hook_command(Path::new("/opt/attemptdb/attempt"), AgentKind::Codex);
        assert!(cmd.contains(" hook codex"), "{cmd}");

        let tmp = tempfile::tempdir().unwrap();
        let attempt = tmp.path().join("attempt");
        fs::write(&attempt, b"").unwrap();
        assert_eq!(
            preferred_hook_binary(attempt.clone()),
            attempt,
            "no sibling yet"
        );
        let sibling = tmp.path().join(if is_windows() {
            "attempt-hook.exe"
        } else {
            "attempt-hook"
        });
        fs::write(&sibling, b"").unwrap();
        assert_eq!(
            preferred_hook_binary(attempt.clone()),
            sibling,
            "sibling wins"
        );
        assert_eq!(preferred_hook_binary(sibling.clone()), sibling);
    }

    const CMD: &str = "'/opt/attemptdb/attempt' hook claude-code";

    fn cmd_for(kind: AgentKind) -> String {
        format!("'/opt/attemptdb/attempt' hook {}", kind.provider_id())
    }

    fn keys(v: &Value) -> Vec<String> {
        v.as_object().unwrap().keys().cloned().collect()
    }

    #[test]
    fn recognises_our_commands() {
        assert!(is_attempt_hook_command("'/x/attempt' hook claude-code"));
        assert!(is_attempt_hook_command(
            r#""C:\Program Files\AttemptDB\attempt.exe" hook codex"#
        ));
        assert!(is_attempt_hook_command(
            r#"& "C:\Program Files\AttemptDB\attempt.exe" hook claude-code"#
        ));
        assert!(is_attempt_hook_command(
            "/usr/local/bin/attempt hook cursor"
        ));
        assert!(is_attempt_hook_command("attempt hook gemini-cli"));
        assert!(is_attempt_hook_command("'/it'\\''s/attempt' hook codex"));
        assert!(is_attempt_hook_command("bash -c 'attemptdb-hook legacy'"));

        assert!(!is_attempt_hook_command(
            "bash ~/.vibemon/notify.sh activity claude_code"
        ));
        assert!(!is_attempt_hook_command("attempt"));
        assert!(!is_attempt_hook_command("attempt status"));
        assert!(!is_attempt_hook_command("/x/attempted hook codex"));
        assert!(!is_attempt_hook_command("hook attempt"));
        assert!(!is_attempt_hook_command(""));
        assert!(!is_attempt_hook_command("'/x/attempt hook codex"));
    }

    #[test]
    fn extracts_binary_path() {
        assert_eq!(
            hook_command_binary(r#""C:\Program Files\AttemptDB\attempt.exe" hook codex"#)
                .as_deref(),
            Some(r"C:\Program Files\AttemptDB\attempt.exe")
        );
        assert_eq!(
            hook_command_binary("'/it'\\''s/attempt' hook codex").as_deref(),
            Some("/it's/attempt")
        );
        assert_eq!(hook_command_binary("bash x"), None);
    }

    #[test]
    fn hook_command_shape() {
        let c = hook_command(Path::new("/opt/attemptdb/attempt"), AgentKind::Codex);
        if is_windows() {
            assert_eq!(c, "\"/opt/attemptdb/attempt\" hook codex");
        } else {
            assert_eq!(c, "'/opt/attemptdb/attempt' hook codex");
        }
        assert!(is_attempt_hook_command(&c));
    }

    #[test]
    fn fresh_claude_shape() {
        let v = planned_config(AgentKind::ClaudeCode, CMD);
        let expected_events: Vec<&str> = CLAUDE_EVENTS.to_vec();
        assert_eq!(keys(&v), vec!["hooks"]);
        assert_eq!(keys(&v["hooks"]), expected_events);
        for ev in CLAUDE_EVENTS {
            assert_eq!(
                v["hooks"][ev],
                json!([{ "hooks": [{ "type": "command", "command": CMD, "timeout": 5 }] }]),
                "event {ev}"
            );
        }
    }

    #[test]
    fn fresh_codex_shape_has_exactly_type_command_timeout() {
        let cmd = cmd_for(AgentKind::Codex);
        let v = planned_config(AgentKind::Codex, &cmd);
        assert_eq!(keys(&v), vec!["hooks"]);
        assert_eq!(keys(&v["hooks"]), CODEX_EVENTS.to_vec());
        for ev in CODEX_EVENTS {
            let timeout = if matches!(*ev, "SessionEnd" | "Interrupt") {
                3
            } else {
                5
            };
            assert_eq!(
                v["hooks"][ev],
                json!([{ "hooks": [{ "type": "command", "command": cmd, "timeout": timeout }] }]),
                "event {ev}"
            );
            let hook = &v["hooks"][ev][0]["hooks"][0];
            assert_eq!(keys(hook), vec!["type", "command", "timeout"]);
        }
    }

    #[test]
    fn fresh_cursor_shape_is_flat_with_second_timeouts() {
        let cmd = cmd_for(AgentKind::Cursor);
        let v = planned_config(AgentKind::Cursor, &cmd);
        assert_eq!(keys(&v), vec!["version", "hooks"]);
        assert_eq!(v["version"], json!(1));
        assert_eq!(keys(&v["hooks"]), CURSOR_EVENTS.to_vec());
        for ev in CURSOR_EVENTS {
            assert_eq!(
                v["hooks"][ev],
                json!([{ "command": cmd, "timeout": 10 }]),
                "event {ev}"
            );
        }
        assert!(v["hooks"].get("afterFileCreate").is_none());
    }

    #[test]
    fn fresh_gemini_shape_has_name_and_millis() {
        let cmd = cmd_for(AgentKind::GeminiCli);
        let v = planned_config(AgentKind::GeminiCli, &cmd);
        assert_eq!(keys(&v["hooks"]), GEMINI_EVENTS.to_vec());
        for ev in GEMINI_EVENTS {
            assert_eq!(
                v["hooks"][ev],
                json!([{ "hooks": [{
                    "name": format!("attemptdb-{}", ev.to_ascii_lowercase()),
                    "type": "command",
                    "command": cmd,
                    "timeout": 5000
                }] }]),
                "event {ev}"
            );
        }
    }

    fn existing_claude() -> Value {
        json!({
            "permissions": { "allow": ["Bash(ls:*)"] },
            "model": "opus",
            "hooks": {
                "PostToolUse": [
                    { "matcher": "Edit|Write", "hooks": [ { "type": "command", "command": "bash ~/.vibemon/notify.sh activity claude_code", "timeout": 10 } ] }
                ],
                "Stop": [],
                "Setup": [ { "hooks": [ { "type": "command", "command": "echo idle" } ] } ]
            },
            "theme": "dark"
        })
    }

    #[test]
    fn merge_preserves_unrelated_content_and_order() {
        let mut v = existing_claude();
        let stats = merge_into(AgentKind::ClaudeCode, &mut v, CMD);
        assert_eq!(stats.added, CLAUDE_EVENTS.len());
        assert_eq!(stats.removed, 0);
        assert!(stats.changed);
        assert_eq!(keys(&v), vec!["permissions", "model", "hooks", "theme"]);
        assert_eq!(v["permissions"], json!({ "allow": ["Bash(ls:*)"] }));
        assert_eq!(v["theme"], json!("dark"));
        // Pre-existing event keys keep their position; ours are appended.
        let hk = keys(&v["hooks"]);
        assert_eq!(&hk[..3], &["PostToolUse", "Stop", "Setup"]);
        // Foreign entries untouched, ours appended after them.
        let ptu = v["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(ptu.len(), 2);
        assert_eq!(ptu[0]["matcher"], json!("Edit|Write"));
        assert_eq!(
            ptu[1],
            json!({ "hooks": [{ "type": "command", "command": CMD, "timeout": 5 }] })
        );
        assert_eq!(v["hooks"]["Setup"], existing_claude()["hooks"]["Setup"]);
    }

    #[test]
    fn install_is_idempotent() {
        let mut v = existing_claude();
        merge_into(AgentKind::ClaudeCode, &mut v, CMD);
        let once = v.clone();
        let stats = merge_into(AgentKind::ClaudeCode, &mut v, CMD);
        assert!(!stats.changed);
        assert_eq!(stats.removed, CLAUDE_EVENTS.len());
        assert_eq!(stats.added, CLAUDE_EVENTS.len());
        assert_eq!(v, once);
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            serde_json::to_string(&once).unwrap()
        );
    }

    #[test]
    fn stale_path_is_replaced_in_place() {
        let old = "'/old/place/attempt' hook claude-code";
        let mut v = json!({
            "hooks": {
                "Stop": [
                    { "hooks": [ { "type": "command", "command": old, "timeout": 5 } ] },
                    { "hooks": [ { "type": "command", "command": "echo after" } ] }
                ],
                "Obsolete": [ { "hooks": [ { "type": "command", "command": old, "timeout": 5 } ] } ]
            }
        });
        let stats = merge_into(AgentKind::ClaudeCode, &mut v, CMD);
        assert_eq!(stats.removed, 2);
        assert_eq!(stats.added, CLAUDE_EVENTS.len());
        assert!(stats.changed);
        assert!(v["hooks"].get("Obsolete").is_none(), "ghost event removed");
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(
            stop[0]["hooks"][0]["command"],
            json!(CMD),
            "new entry took the old slot"
        );
        assert_eq!(stop[1]["hooks"][0]["command"], json!("echo after"));
        let text = serde_json::to_string(&v).unwrap();
        assert!(!text.contains("/old/place/"));
    }

    #[test]
    fn uninstall_restores_original_but_keeps_preexisting_empty_arrays() {
        for kind in AgentKind::ALL {
            let original = match kind {
                AgentKind::Cursor => json!({
                    "version": 1,
                    "hooks": { "stop": [ { "command": "echo hi", "timeout": 3 } ], "beforeMCPExecution": [] }
                }),
                _ => json!({
                    "other": true,
                    "hooks": { "Stop": [ { "hooks": [ { "type": "command", "command": "echo hi" } ] } ], "Setup": [] }
                }),
            };
            let mut v = original.clone();
            merge_into(kind, &mut v, &cmd_for(kind));
            assert_ne!(v, original);
            let stats = remove_from(kind, &mut v);
            assert_eq!(stats.removed, events_for(kind).len(), "{kind}");
            assert!(stats.changed);
            assert_eq!(v, original, "{kind}: uninstall must restore the original");
            assert_eq!(keys(&v), keys(&original));
            assert_eq!(keys(&v["hooks"]), keys(&original["hooks"]));
        }
    }

    #[test]
    fn preexisting_empty_array_for_our_event_survives_install_but_not_uninstall() {
        // `SessionEnd: []` pre-exists for an event we install. Install keeps the
        // key in place (no order drift, idempotent); uninstall cannot tell it
        // apart from a key we created and drops the (semantically empty) array.
        let original = json!({ "hooks": { "SessionEnd": [], "Stop": [ { "hooks": [ { "type": "command", "command": "echo" } ] } ] } });
        let mut v = original.clone();
        merge_into(AgentKind::ClaudeCode, &mut v, CMD);
        assert_eq!(&keys(&v["hooks"])[..2], &["SessionEnd", "Stop"]);
        let once = v.clone();
        assert!(!merge_into(AgentKind::ClaudeCode, &mut v, CMD).changed);
        assert_eq!(v, once);
        remove_from(AgentKind::ClaudeCode, &mut v);
        assert_eq!(
            v,
            json!({ "hooks": { "Stop": [ { "hooks": [ { "type": "command", "command": "echo" } ] } ] } })
        );
    }

    #[test]
    fn uninstall_from_fresh_leaves_base_without_hooks() {
        for kind in AgentKind::ALL {
            let mut v = planned_config(kind, &cmd_for(kind));
            remove_from(kind, &mut v);
            assert_eq!(v, default_base(kind), "{kind}");
        }
        // Pre-existing empty `hooks` object survives; nothing of ours to remove.
        let mut v = json!({ "hooks": {} });
        let stats = remove_from(AgentKind::ClaudeCode, &mut v);
        assert!(!stats.changed);
        assert_eq!(v, json!({ "hooks": {} }));
    }

    #[test]
    fn foreign_entries_are_never_touched() {
        let mut v = json!({
            "hooks": { "Stop": [ { "hooks": [ { "type": "command", "command": "attempt status" } ] } ] }
        });
        let stats = remove_from(AgentKind::ClaudeCode, &mut v);
        assert_eq!(stats.removed, 0);
        assert!(!stats.changed);
    }

    #[test]
    fn indent_detection_and_rendering() {
        assert_eq!(detect_indent("{\n    \"a\": 1\n}"), Indent::Spaces(4));
        assert_eq!(detect_indent("{\n  \"a\": 1\n}"), Indent::Spaces(2));
        assert_eq!(detect_indent("{\n\t\"a\": 1\n}"), Indent::Tabs);
        assert_eq!(detect_indent("{\"a\":1}"), Indent::Spaces(2));
        let style = Style::detect("{\r\n    \"a\": 1\r\n}\r\n");
        assert_eq!(
            style,
            Style {
                indent: Indent::Spaces(4),
                trailing_newline: true,
                crlf: true,
                bom: false
            }
        );
        let out = render_json(&json!({ "a": [1] }), style).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\r\n    \"a\": [\r\n        1\r\n    ]\r\n}\r\n"
        );
        let no_nl = render_json(
            &json!({}),
            Style {
                indent: Indent::Tabs,
                trailing_newline: false,
                crlf: false,
                bom: false,
            },
        )
        .unwrap();
        assert_eq!(no_nl, b"{}");
    }

    #[test]
    fn install_to_creates_backs_up_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".codex").join("hooks.json");
        let cmd = cmd_for(AgentKind::Codex);

        let a = install_to(AgentKind::Codex, &path, &cmd, false).unwrap();
        assert_eq!(a.outcome, Outcome::Installed);
        assert_eq!(a.entries_added, CODEX_EVENTS.len());
        assert!(a.backup_path.is_none(), "no backup for a new file");
        let written: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written, planned_config(AgentKind::Codex, &cmd));
        assert!(fs::read_to_string(&path).unwrap().ends_with('\n'));
        assert!(a.notes.iter().any(|n| n.contains("/hooks")));

        let b = install_to(AgentKind::Codex, &path, &cmd, false).unwrap();
        assert_eq!(b.outcome, Outcome::AlreadyCurrent);
        assert!(b.backup_path.is_none());

        let new_cmd = "'/new/attempt' hook codex";
        let c = install_to(AgentKind::Codex, &path, new_cmd, false).unwrap();
        assert_eq!(c.outcome, Outcome::Updated);
        assert_eq!(c.entries_removed, CODEX_EVENTS.len());
        let backup = c.backup_path.clone().expect("backup created");
        assert!(backup.exists());
        assert_eq!(
            serde_json::from_str::<Value>(&fs::read_to_string(&backup).unwrap()).unwrap(),
            planned_config(AgentKind::Codex, &cmd)
        );
        assert!(
            !tmp.path()
                .join(".codex")
                .join("hooks.json.attemptdb.tmp")
                .exists()
        );

        let d = uninstall_from(AgentKind::Codex, &path, false).unwrap();
        assert_eq!(d.outcome, Outcome::Removed);
        assert_eq!(d.entries_removed, CODEX_EVENTS.len());
        let after: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after, json!({}));

        let e = uninstall_from(AgentKind::Codex, &path, false).unwrap();
        assert_eq!(e.outcome, Outcome::AlreadyCurrent);
    }

    #[test]
    fn install_to_preserves_four_space_indent_and_foreign_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        let original = "{\n    \"security\": {\n        \"auth\": {\n            \"selectedType\": \"gemini-api-key\"\n        }\n    },\n    \"hooks\": {\n        \"AfterTool\": [\n            {\n                \"matcher\": \"write_file\",\n                \"hooks\": [\n                    {\n                        \"name\": \"vibemon-exp\",\n                        \"type\": \"command\",\n                        \"command\": \"bash ~/.vibemon/notify.sh activity gemini_cli\",\n                        \"timeout\": 5000\n                    }\n                ]\n            }\n        ]\n    },\n    \"mcpServers\": {}\n}\n";
        fs::write(&path, original).unwrap();
        let cmd = cmd_for(AgentKind::GeminiCli);
        let a = install_to(AgentKind::GeminiCli, &path, &cmd, false).unwrap();
        assert_eq!(a.outcome, Outcome::Installed);
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("{\n    \"security\": {\n        \"auth\""),
            "4-space indent kept:\n{text}"
        );
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(keys(&v), vec!["security", "hooks", "mcpServers"]);
        assert_eq!(
            v["hooks"]["AfterTool"][0]["hooks"][0]["name"],
            json!("vibemon-exp")
        );
        assert_eq!(
            v["hooks"]["AfterTool"][1]["hooks"][0]["name"],
            json!("attemptdb-aftertool")
        );

        // Uninstall yields the original bytes (same formatting rules apply).
        uninstall_from(AgentKind::GeminiCli, &path, false).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn invalid_json_is_refused_and_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hooks.json");
        fs::write(&path, "{ not json").unwrap();
        let err =
            install_to(AgentKind::Cursor, &path, "'/x/attempt' hook cursor", false).unwrap_err();
        assert!(err.to_string().contains("not valid JSON"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ not json");

        fs::write(&path, "{\"hooks\": []}").unwrap();
        let err =
            install_to(AgentKind::Cursor, &path, "'/x/attempt' hook cursor", false).unwrap_err();
        assert!(err.to_string().contains("not a JSON object"), "{err}");
    }

    #[test]
    fn dry_run_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".cursor").join("hooks.json");
        let a = install_to(AgentKind::Cursor, &path, "'/x/attempt' hook cursor", true).unwrap();
        assert_eq!(a.outcome, Outcome::Installed);
        assert_eq!(a.entries_added, CURSOR_EVENTS.len());
        assert!(!path.exists());
        assert!(
            !tmp.path().join(".cursor").exists(),
            "dry run must not create directories"
        );
    }

    #[test]
    fn legacy_vibemon_objects_are_recognised_and_ours_are_not() {
        for legacy in [
            json!({"type": "command", "command": "bash ~/.vibemon/notify.sh activity claude_code"}),
            json!({"command": "bash /home/dev/.vibemon/notify.sh stop cursor", "timeout": 10}),
            json!({"name": "vibemon-exp", "type": "command", "command": "bash ~/.vibemon/notify.sh activity gemini_cli"}),
            // The Windows client: Python, either separator.
            json!({"type": "command", "command": "python \"C:\\Users\\dev\\.vibemon\\notify.py\" activity claude_code"}),
            json!({"type": "command", "command": "py C:/Users/dev/.vibemon/notify.py stop codex"}),
            json!({"type": "command", "command": "powershell -File C:\\Users\\dev\\.vibemon\\notify.ps1 prompt cursor"}),
        ] {
            assert!(is_legacy_vibemon_hook_object(&legacy), "{legacy}");
            assert!(!is_attempt_hook_object(&legacy), "{legacy}");
        }
        for other in [
            json!({"type": "command", "command": CMD}),
            json!({"name": "attemptdb-aftertool", "command": "x"}),
            json!({"type": "command", "command": "echo vibemon"}),
            json!({"command": "~/.vibemon-rc/notify.sh"}),
            json!("bash ~/.vibemon/notify.sh"),
        ] {
            assert!(!is_legacy_vibemon_hook_object(&other), "{other}");
        }
    }

    #[test]
    fn legacy_entries_survive_a_plain_install_and_go_with_remove_legacy() {
        // Claude (nested): a group holding a legacy hook next to the user's
        // own hook keeps the user's hook; a legacy-only group is dropped; an
        // event that held only legacy entries disappears entirely.
        let original = json!({
            "hooks": {
                "PostToolUse": [
                    { "matcher": "Edit|Write", "hooks": [
                        { "type": "command", "command": "bash ~/.vibemon/notify.sh activity claude_code" },
                        { "type": "command", "command": "/usr/local/bin/my-linter" }
                    ] }
                ],
                "Notification": [
                    { "hooks": [ { "type": "command", "command": "bash ~/.vibemon/notify.sh notify claude_code" } ] }
                ],
                "LegacyOnlyEvent": [
                    { "hooks": [ { "type": "command", "command": "bash ~/.vibemon/notify.sh other claude_code" } ] }
                ]
            }
        });
        let mut plain = original.clone();
        merge_into(AgentKind::ClaudeCode, &mut plain, CMD);
        assert!(plain.to_string().contains(".vibemon/notify.sh"));
        assert_eq!(
            plain["hooks"]["PostToolUse"][0]["hooks"]
                .as_array()
                .unwrap()
                .len(),
            2
        );

        let mut migrated = original.clone();
        let legacy = remove_legacy_from(AgentKind::ClaudeCode, &mut migrated);
        assert_eq!(legacy.removed, 3);
        assert!(legacy.changed);
        let stats = merge_into(AgentKind::ClaudeCode, &mut migrated, CMD);
        assert!(stats.added > 0);
        assert!(!migrated.to_string().contains(".vibemon/notify.sh"));
        assert_eq!(
            migrated["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
            json!("/usr/local/bin/my-linter"),
            "the user's own hook keeps its place"
        );
        // An event we install is refilled with exactly our entry; one we do
        // not install is gone with its last legacy entry.
        let notification = migrated["hooks"]["Notification"].as_array().unwrap();
        assert_eq!(notification.len(), 1);
        assert!(is_attempt_hook_object(&notification[0]["hooks"][0]));
        assert!(migrated["hooks"].get("LegacyOnlyEvent").is_none());
        // Idempotent: a second pass finds nothing.
        let again = remove_legacy_from(AgentKind::ClaudeCode, &mut migrated.clone());
        assert_eq!(again.removed, 0);
        assert!(!again.changed);

        // Cursor (flat) and Gemini (named) shapes.
        let mut cursor = json!({ "version": 1, "hooks": { "stop": [
            { "command": "bash ~/.vibemon/notify.sh stop cursor", "timeout": 10 },
            { "command": "./mine.sh", "timeout": 3 }
        ] } });
        assert_eq!(
            remove_legacy_from(AgentKind::Cursor, &mut cursor).removed,
            1
        );
        assert_eq!(cursor["hooks"]["stop"].as_array().unwrap().len(), 1);
        let mut gemini = json!({ "hooks": { "AfterTool": [ { "matcher": "write_file", "hooks": [
            { "name": "vibemon-exp", "type": "command", "command": "bash ~/.vibemon/notify.sh activity gemini_cli", "timeout": 5000 }
        ] } ] }, "mcpServers": {} });
        assert_eq!(
            remove_legacy_from(AgentKind::GeminiCli, &mut gemini).removed,
            1
        );
        assert!(
            gemini.get("hooks").is_none(),
            "emptied hooks object is dropped"
        );
        assert!(gemini.get("mcpServers").is_some(), "unrelated keys survive");
    }

    #[test]
    fn install_to_with_remove_legacy_reports_and_writes_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let legacy_only = r#"{ "hooks": { "Stop": [ { "hooks": [ { "type": "command", "command": "bash ~/.vibemon/notify.sh stop claude_code" } ] } ] } }"#;
        fs::write(&path, legacy_only).unwrap();
        let cmd = cmd_for(AgentKind::ClaudeCode);
        // Plain install leaves the legacy entry in place.
        let a = install_to(AgentKind::ClaudeCode, &path, &cmd, false).unwrap();
        assert_eq!(a.outcome, Outcome::Installed);
        assert_eq!(a.legacy_removed, 0);
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains(".vibemon/notify.sh")
        );
        // Migration: ours already current, so the only change is the legacy removal.
        let b = install_to_with(AgentKind::ClaudeCode, &path, &cmd, false, true).unwrap();
        assert_eq!(b.outcome, Outcome::Updated);
        assert_eq!(b.legacy_removed, 1);
        assert!(b.backup_path.is_some());
        assert!(b.notes.iter().any(|n| n.contains("VibeMon legacy")));
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains(".vibemon/notify.sh"));
        assert!(text.contains("hook claude-code"));
        // Nothing left to do.
        let c = install_to_with(AgentKind::ClaudeCode, &path, &cmd, false, true).unwrap();
        assert_eq!(c.outcome, Outcome::AlreadyCurrent);
        assert_eq!(c.legacy_removed, 0);
        // Uninstall with the flag also clears a legacy entry that came back.
        fs::write(&path, legacy_only).unwrap();
        let d = uninstall_from_with(AgentKind::ClaudeCode, &path, false, true).unwrap();
        assert_eq!(d.outcome, Outcome::Removed);
        assert_eq!(d.legacy_removed, 1);
        assert!(!fs::read_to_string(&path).unwrap().contains("vibemon"));
    }

    #[test]
    fn dry_run_install_writes_nothing_and_does_not_fail() {
        // Hermetic: one named Claude directory in a temp dir, and only
        // Claude Code requested, so no real agent config is read or touched.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("claude");
        fs::create_dir_all(&dir).unwrap();
        let opts = InstallOptions {
            dry_run: true,
            binary_path: Some(std::env::current_exe().unwrap()),
            providers: Some(vec![AgentKind::ClaudeCode]),
            claude_config_dirs: vec![dir.clone()],
            ..InstallOptions::default()
        };
        let report = install(&opts).unwrap();
        assert!(!report.has_failures(), "{report:?}");
        assert_eq!(report.actions.len(), 1, "{report:?}");
        assert_eq!(report.actions[0].outcome, Outcome::Installed);
        assert!(report.actions[0].backup_path.is_none());
        let report = uninstall(&opts).unwrap();
        assert!(!report.has_failures(), "{report:?}");
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            0,
            "a dry run writes nothing"
        );
    }

    #[test]
    fn every_named_claude_dir_is_wired_and_a_missing_one_is_reported_not_made() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("claude-a");
        let b = tmp.path().join("claude-b");
        let missing = tmp.path().join("claude-typo");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(b.join("settings.json"), r#"{"model":"opus"}"#).unwrap();
        let bin = tmp.path().join("bin").join("attempt");
        fs::create_dir_all(bin.parent().unwrap()).unwrap();
        fs::write(&bin, b"").unwrap();
        let opts = InstallOptions {
            binary_path: Some(bin.clone()),
            providers: Some(vec![AgentKind::ClaudeCode]),
            claude_config_dirs: vec![a.clone(), b.clone(), missing.clone()],
            ..InstallOptions::default()
        };
        let report = install(&opts).unwrap();
        let by_path = |p: &Path| {
            report
                .actions
                .iter()
                .find(|x| x.config_path == p.join("settings.json"))
                .unwrap_or_else(|| panic!("no action for {}: {report:?}", p.display()))
        };
        assert_eq!(by_path(&a).outcome, Outcome::Installed);
        assert_eq!(by_path(&b).outcome, Outcome::Installed);
        assert!(
            matches!(&by_path(&missing).outcome, Outcome::Skipped(why) if why.contains("not creating")),
            "{report:?}"
        );
        assert!(
            !missing.exists(),
            "a directory that was not there is not made"
        );
        for dir in [&a, &b] {
            let v: Value =
                serde_json::from_str(&fs::read_to_string(dir.join("settings.json")).unwrap())
                    .unwrap();
            assert!(v["hooks"]["Stop"].is_array(), "{}", dir.display());
        }
        let kept: Value =
            serde_json::from_str(&fs::read_to_string(b.join("settings.json")).unwrap()).unwrap();
        assert_eq!(kept["model"], "opus", "the user's own keys survive");
        // Idempotent in every directory; uninstall cleans every directory.
        let again = install(&opts).unwrap();
        assert_eq!(by_path_in(&again, &a).outcome, Outcome::AlreadyCurrent);
        assert_eq!(by_path_in(&again, &b).outcome, Outcome::AlreadyCurrent);
        let gone = uninstall(&opts).unwrap();
        assert_eq!(by_path_in(&gone, &a).outcome, Outcome::Removed);
        assert_eq!(by_path_in(&gone, &b).outcome, Outcome::Removed);
        let b_after: Value =
            serde_json::from_str(&fs::read_to_string(b.join("settings.json")).unwrap()).unwrap();
        assert_eq!(b_after, json!({"model": "opus"}));
    }

    fn by_path_in<'a>(report: &'a InstallReport, dir: &Path) -> &'a InstallAction {
        report
            .actions
            .iter()
            .find(|x| x.config_path == dir.join("settings.json"))
            .unwrap_or_else(|| panic!("no action for {}: {report:?}", dir.display()))
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_config_is_written_through_the_link() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let tmp = tempfile::tempdir().unwrap();
        let dotfiles = tmp.path().join("dotfiles");
        let agent = tmp.path().join(".claude");
        fs::create_dir_all(&dotfiles).unwrap();
        fs::create_dir_all(&agent).unwrap();
        let real = dotfiles.join("claude-settings.json");
        fs::write(&real, "{\n  \"model\": \"opus\"\n}\n").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o640)).unwrap();
        let link = agent.join("settings.json");
        // Relative, through two links: a dotfiles manager's usual shape.
        symlink("../dotfiles/claude-settings.json", agent.join("hop")).unwrap();
        symlink("hop", &link).unwrap();
        let cmd = cmd_for(AgentKind::ClaudeCode);

        let a = install_to(AgentKind::ClaudeCode, &link, &cmd, false).unwrap();
        assert_eq!(a.outcome, Outcome::Installed);
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link is still a link"
        );
        let written: Value = serde_json::from_str(&fs::read_to_string(&real).unwrap()).unwrap();
        assert!(
            written["hooks"]["Stop"].is_array(),
            "the dotfiles copy changed"
        );
        assert_eq!(written["model"], "opus");
        assert_eq!(
            fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o640,
            "the file's mode is kept"
        );
        let backup = a.backup_path.expect("backup of the old contents");
        assert_eq!(
            backup.parent().unwrap(),
            agent,
            "backups stay beside the link"
        );
        assert_eq!(
            fs::read_to_string(backup).unwrap(),
            "{\n  \"model\": \"opus\"\n}\n"
        );
        assert!(
            fs::read_dir(&dotfiles).unwrap().count() == 1,
            "no temp file is left in the dotfiles directory"
        );

        let b = install_to(AgentKind::ClaudeCode, &link, &cmd, false).unwrap();
        assert_eq!(b.outcome, Outcome::AlreadyCurrent);
        let c = uninstall_from(AgentKind::ClaudeCode, &link, false).unwrap();
        assert_eq!(c.outcome, Outcome::Removed);
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(&real).unwrap(),
            "{\n  \"model\": \"opus\"\n}\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_link_is_filled_in_and_a_link_loop_is_refused() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        fs::create_dir_all(&agent).unwrap();
        let cmd = cmd_for(AgentKind::Cursor);
        let target = tmp.path().join("hooks-managed.json");
        let link = agent.join("hooks.json");
        symlink(&target, &link).unwrap();
        let a = install_to(AgentKind::Cursor, &link, &cmd, false).unwrap();
        assert_eq!(a.outcome, Outcome::Installed);
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(target.is_file(), "written where the link points");

        let (x, y) = (agent.join("x.json"), agent.join("y.json"));
        symlink(&y, &x).unwrap();
        symlink(&x, &y).unwrap();
        let err = write_atomically(&x, b"{}").unwrap_err();
        assert!(format!("{err:#}").contains("symlink loop"), "{err:#}");
    }

    #[test]
    fn crlf_and_tab_indented_json_keeps_its_style() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        let original = "{\r\n\t\"model\": \"opus\",\r\n\t\"permissions\": {\r\n\t\t\"allow\": []\r\n\t}\r\n}\r\n";
        fs::write(&path, original).unwrap();
        let cmd = cmd_for(AgentKind::ClaudeCode);
        let a = install_to(AgentKind::ClaudeCode, &path, &cmd, false).unwrap();
        assert_eq!(a.outcome, Outcome::Installed);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("{\r\n\t\"model\""), "{text:?}");
        assert!(text.contains("\r\n\t\t\t"), "tabs at every depth: {text:?}");
        assert!(
            !text.replace("\r\n", "").contains('\n'),
            "no bare line feed: {text:?}"
        );
        assert!(!text.contains("  \""), "no space indentation: {text:?}");
        assert!(text.ends_with("}\r\n"));
        uninstall_from(AgentKind::ClaudeCode, &path, false).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn lock_files_are_removed_after_use_and_stale_ones_are_cleaned_up() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        fs::write(&path, "{}").unwrap();
        // Left behind by an older build.
        let stale = tmp.path().join("settings.json.attemptdb.lock");
        fs::write(&stale, b"").unwrap();
        let cmd = cmd_for(AgentKind::ClaudeCode);
        install_to(AgentKind::ClaudeCode, &path, &cmd, false).unwrap();
        let leftovers = |dir: &Path| -> Vec<String> {
            fs::read_dir(dir)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".lock") || n.ends_with(".tmp"))
                .collect()
        };
        if cfg!(unix) {
            assert!(
                leftovers(tmp.path()).is_empty(),
                "{:?}",
                leftovers(tmp.path())
            );
        }
        uninstall_from(AgentKind::ClaudeCode, &path, false).unwrap();
        if cfg!(unix) {
            assert!(
                leftovers(tmp.path()).is_empty(),
                "{:?}",
                leftovers(tmp.path())
            );
        }
    }

    #[test]
    fn concurrent_installers_still_exclude_each_other_while_lock_files_come_and_go() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        fs::write(&path, r#"{"model":"opus"}"#).unwrap();
        let cmd = cmd_for(AgentKind::ClaudeCode);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|i| {
                    let (path, cmd) = (path.clone(), cmd.clone());
                    scope.spawn(move || {
                        for round in 0..4 {
                            // Every thread installs and removes: a lost update or a
                            // torn file would show as invalid JSON or a stuck entry.
                            let r = if (i + round) % 2 == 0 {
                                install_to(AgentKind::ClaudeCode, &path, &cmd, false)
                            } else {
                                uninstall_from(AgentKind::ClaudeCode, &path, false)
                            };
                            r.unwrap();
                        }
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }
        });
        let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["model"], "opus");
        install_to(AgentKind::ClaudeCode, &path, &cmd, false).unwrap();
        let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        // Exactly one entry per event, whatever the interleaving was.
        for event in CLAUDE_EVENTS {
            assert_eq!(v["hooks"][event].as_array().unwrap().len(), 1, "{event}");
        }
        let backups = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".attemptdb.bak-"))
            .count();
        assert!(backups <= BACKUPS_TO_KEEP, "{backups} backups");
    }

    #[test]
    fn backups_are_pruned_to_five() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        fs::write(&path, "{}").unwrap();
        for i in 0..8 {
            let cmd = format!("'/v{i}/attempt' hook claude-code");
            let a = install_to(AgentKind::ClaudeCode, &path, &cmd, false).unwrap();
            assert!(a.backup_path.is_some());
        }
        let backups = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".attemptdb.bak-"))
            .count();
        assert_eq!(backups, BACKUPS_TO_KEEP);
    }

    #[test]
    fn a_utf8_byte_order_mark_is_accepted_and_written_back() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        let original = "\u{feff}{\n  \"model\": \"opus\"\n}\n";
        fs::write(&path, original).unwrap();
        let cmd = cmd_for(AgentKind::GeminiCli);
        let action = install_to(AgentKind::GeminiCli, &path, &cmd, false).unwrap();
        assert_eq!(action.outcome, Outcome::Installed, "{action:?}");
        let bytes = fs::read(&path).unwrap();
        assert!(bytes.starts_with(b"\xEF\xBB\xBF"), "the mark is kept");
        let v: Value = serde_json::from_slice(&bytes[3..]).unwrap();
        assert_eq!(v["model"], "opus");
        assert!(v["hooks"].is_object());
        // Idempotent with the mark in place, and uninstall restores it exactly.
        let again = install_to(AgentKind::GeminiCli, &path, &cmd, false).unwrap();
        assert_eq!(again.outcome, Outcome::AlreadyCurrent);
        let gone = uninstall_from(AgentKind::GeminiCli, &path, false).unwrap();
        assert_eq!(gone.outcome, Outcome::Removed);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn json_with_comments_is_refused_by_name_and_nothing_is_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        let original = "// my gemini settings\n{\n  /* theme */ \"theme\": \"dark\", // keep\n  \"url\": \"http://example.com/a\"\n}\n";
        fs::write(&path, original).unwrap();
        let cmd = cmd_for(AgentKind::GeminiCli);
        for run in [
            install_to(AgentKind::GeminiCli, &path, &cmd, false),
            install_to(AgentKind::GeminiCli, &path, &cmd, true),
            uninstall_from(AgentKind::GeminiCli, &path, false),
        ] {
            let message = format!("{:#}", run.unwrap_err());
            assert!(message.contains("contains comments"), "{message}");
            assert!(message.contains("nothing was changed"), "{message}");
            assert!(!message.contains("key must be a string"), "{message}");
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        let leftovers: Vec<_> = fs::read_dir(tmp.path()).unwrap().flatten().collect();
        assert_eq!(
            leftovers.len(),
            1,
            "no backup, lock or temp file: {leftovers:?}"
        );
        // A real syntax error keeps its own message.
        fs::write(&path, "{ \"a\": ").unwrap();
        let message = format!(
            "{:#}",
            install_to(AgentKind::GeminiCli, &path, &cmd, false).unwrap_err()
        );
        assert!(message.contains("not valid JSON"), "{message}");
    }

    #[test]
    fn comment_stripping_respects_strings_and_keeps_lines() {
        let text = "{\"a\": \"// not a comment\", /* x\n y */ \"b\": \"/* nor this */\"} // end";
        let stripped = strip_json_comments(text);
        let v: Value = serde_json::from_str(&stripped).unwrap();
        assert_eq!(v["a"], "// not a comment");
        assert_eq!(v["b"], "/* nor this */");
        assert_eq!(stripped.matches('\n').count(), text.matches('\n').count());
    }

    #[test]
    fn uninstall_keeps_the_newest_backup_only() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        fs::write(&path, "{}").unwrap();
        for i in 0..4 {
            let cmd = format!("'/v{i}/attempt' hook claude-code");
            install_to(AgentKind::ClaudeCode, &path, &cmd, false).unwrap();
        }
        let count = |dir: &Path| {
            fs::read_dir(dir)
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().contains(".attemptdb.bak-"))
                .count()
        };
        assert!(count(tmp.path()) > 1);
        let gone = uninstall_from(AgentKind::ClaudeCode, &path, false).unwrap();
        assert_eq!(gone.outcome, Outcome::Removed);
        assert_eq!(count(tmp.path()), 1, "one backup is enough to undo it");
        assert_eq!(newest_backup(&path), gone.backup_path);
    }
}
