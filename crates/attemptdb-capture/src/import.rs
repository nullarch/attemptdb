//! Transcript import: reconstruct history from the transcript files Claude
//! Code keeps on disk.
//!
//! Hooks only see what happens after they are installed. Claude Code,
//! however, writes every conversation to
//! `<CLAUDE_CONFIG_DIR>/projects/<slug>/<session>.jsonl` (plus
//! `<session>/subagents/**/agent-<id>.jsonl` for subagents), so earlier
//! sessions can be *reconstructed* from those files. The parser lives in
//! `attemptdb-adapters::transcript`; this module finds the files, builds a
//! capture context per file (project from the transcript's `cwd`), streams
//! the lines through the parser and ingests the result in batches.
//!
//! Reconstructed events are never presented as captured fact: every one is
//! marked `attrs.reconstructed = true`, and their ids are derived from the
//! transcript entries so a re-import of a transcript that has grown only adds
//! the new entries (`Database::ingest` is idempotent by event id).
//!
//! A session that hooks captured live is not stored twice. A tool call has
//! the same id in both channels (`attemptdb_adapters::common::derive_event_id`
//! over the `tool_use_id`), so storage merges them; the rest of what a hook
//! also saw (prompts, turn ends, the session start) is reconciled here, before
//! writing, against what the database already holds ([`Reconciler`]), and a
//! session's project identity is the one its hook events already carry.

use crate::config::Config;
use crate::git::git_info;
use crate::import_common::{Batcher, DbSink, EventSink, Reconciler, Window};
use crate::platform::home_dir;
use crate::{Result, io_at};
use attemptdb_adapters::CaptureContext;
use attemptdb_adapters::transcript::{TranscriptOptions, parse_claude_transcript};
use attemptdb_core::event::{ProjectRef, Provider};
use attemptdb_core::{DeviceId, SessionId, Timestamp};
use attemptdb_storage::Database;
use serde_json::Value;
use std::cell::Cell;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

/// Environment variable Claude Code honours to relocate its config directory.
pub const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";

/// Events handed to the database per `ingest` call.
pub use crate::import_common::INGEST_BATCH;

/// Leading lines inspected for `cwd` / `gitBranch` before parsing.
const PEEK_LINES: usize = 200;

/// Directory depth searched below a projects directory (slug / session /
/// subagents / workflows / wf_id / file).
const MAX_WALK_DEPTH: usize = 6;

/// Warnings kept in an [`ImportSummary`]; the rest are counted.
const MAX_WARNINGS: usize = 200;

/// One transcript file to import.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct TranscriptSource {
    pub path: PathBuf,
    /// The `projects/<slug>` directory name the file was found under.
    pub project_slug: Option<String>,
    pub modified_at: Option<Timestamp>,
    pub bytes: u64,
}

impl TranscriptSource {
    /// Describe a file on disk (size and mtime are best effort).
    pub fn from_path(path: &Path) -> Self {
        let meta = std::fs::metadata(path).ok();
        Self {
            path: path.to_path_buf(),
            project_slug: slug_of(path),
            modified_at: meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| Timestamp::from_micros(d.as_micros() as i64)),
            bytes: meta.map(|m| m.len()).unwrap_or(0),
        }
    }

    /// Whether this is a subagent transcript (`.../subagents/.../agent-<id>.jsonl`).
    pub fn is_subagent(&self) -> bool {
        self.path.components().any(|c| c.as_os_str() == "subagents")
    }

    /// File stem: the session id for main transcripts, `agent-<id>` for
    /// subagent files.
    pub fn stem(&self) -> Option<String> {
        self.path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
    }
}

/// The `<slug>` directory name of a transcript path: the nearest ancestor
/// whose parent is named `projects`.
fn slug_of(path: &Path) -> Option<String> {
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d.parent()
            .and_then(Path::file_name)
            .is_some_and(|n| n == "projects")
        {
            return d.file_name().map(|n| n.to_string_lossy().into_owned());
        }
        dir = d.parent();
    }
    None
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Directories that may hold Claude Code project transcripts:
/// `$CLAUDE_CONFIG_DIR/projects` when the variable is set, then the default
/// `~/.claude/projects` when it exists and differs. Only existing
/// directories are returned.
pub fn claude_projects_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(v) = std::env::var_os(CLAUDE_CONFIG_DIR_ENV).filter(|v| !v.is_empty()) {
        dirs.push(PathBuf::from(v).join("projects"));
    }
    if let Some(home) = home_dir() {
        dirs.push(home.join(".claude").join("projects"));
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        let canonical = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        if out
            .iter()
            .any(|d| d == &dir || d.canonicalize().ok().as_ref() == Some(&canonical))
        {
            continue;
        }
        out.push(dir);
    }
    out
}

/// Claude Code's directory name for a project root: every character that is
/// not ASCII alphanumeric becomes `-` (`/Users/me/app` → `-Users-me-app`,
/// `/x/.claude/worktrees/a` → `-x--claude-worktrees-a`).
pub fn project_slug(root: &Path) -> String {
    root.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Every Claude Code transcript under the default projects directories, or
/// only those of the given project root.
pub fn discover_claude_transcripts(project_root: Option<&Path>) -> Vec<TranscriptSource> {
    discover_in(&claude_projects_dirs(), project_root)
}

/// [`discover_claude_transcripts`] over explicit projects directories.
pub fn discover_in(
    projects_dirs: &[PathBuf],
    project_root: Option<&Path>,
) -> Vec<TranscriptSource> {
    let wanted: Vec<String> = project_root
        .map(|root| {
            let mut slugs = vec![project_slug(root)];
            if let Ok(canonical) = root.canonicalize() {
                let s = project_slug(&canonical);
                if !slugs.contains(&s) {
                    slugs.push(s);
                }
            }
            slugs
        })
        .unwrap_or_default();
    let mut out = Vec::new();
    for dir in projects_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut slug_dirs: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        slug_dirs.sort();
        for slug_dir in slug_dirs {
            let name = slug_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if project_root.is_some()
                && !wanted
                    .iter()
                    .any(|w| w == &name || w.eq_ignore_ascii_case(&name))
            {
                continue;
            }
            walk(&slug_dir, 0, &mut out);
        }
    }
    sort_sources(&mut out);
    out
}

/// Main transcripts first (so a session exists before its subagents are
/// attributed to it), then by path; duplicates removed.
pub fn sort_sources(sources: &mut Vec<TranscriptSource>) {
    sources.sort_by(|a, b| {
        a.is_subagent()
            .cmp(&b.is_subagent())
            .then_with(|| a.path.as_os_str().cmp(b.path.as_os_str()))
    });
    sources.dedup_by(|a, b| a.path == b.path);
}

/// Transcripts at or below an explicit path: the file itself when it is a
/// `.jsonl`, otherwise every `.jsonl` found under the directory.
pub fn collect_transcripts(path: &Path) -> Vec<TranscriptSource> {
    let mut out = Vec::new();
    if path.is_file() {
        if is_transcript(path) {
            out.push(TranscriptSource::from_path(path));
        }
    } else if path.is_dir() {
        walk(path, 0, &mut out);
    }
    sort_sources(&mut out);
    out
}

fn is_transcript(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "jsonl")
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<TranscriptSource>) {
    if depth > MAX_WALK_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            // `memory/` holds the agent's notes, never a transcript.
            if path.file_name().is_some_and(|n| n == "memory") {
                continue;
            }
            walk(&path, depth + 1, out);
        } else if is_transcript(&path) {
            out.push(TranscriptSource::from_path(&path));
        }
    }
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

/// What an import run did.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct ImportSummary {
    /// Files attempted.
    pub files: usize,
    /// Files that could not be read at all.
    pub files_failed: usize,
    /// Events the parser produced across all files.
    pub events_seen: usize,
    /// Events newly stored.
    pub accepted: usize,
    /// Events already present (by id) and skipped.
    pub duplicates: usize,
    /// Reconstructed events not written because the database already holds
    /// the same real-world event: a tool call a hook captured (or an earlier
    /// import stored under an older id), or a prompt, turn end or session
    /// start a hook captured, matched by order. See
    /// `import_common::Reconciler`.
    pub skipped_captured: usize,
    /// Events handed to the spool because the daemon holds the writer lock:
    /// it counts accepted and duplicates when it imports them.
    pub queued: usize,
    /// Distinct sessions the events belong to.
    pub sessions: usize,
    /// Lines that could not be turned into events (malformed, oversized,
    /// a partial last line).
    pub lines_skipped: usize,
    /// Bytes of transcript read.
    pub bytes: u64,
    pub warnings: Vec<String>,
}

/// Import transcripts into `db`. Each file gets its own capture context
/// whose project is derived from the transcript's `cwd` (through the
/// repository when the directory still exists on this machine). Events are
/// ingested in batches of [`INGEST_BATCH`] and the memtable is flushed at the
/// end so the reconstructed history lands in a segment.
pub fn import_claude_transcripts(
    db: &mut Database,
    sources: &[TranscriptSource],
    config: &Config,
    device: DeviceId,
) -> Result<ImportSummary> {
    import_claude_transcripts_to(&mut DbSink::new(db), sources, config, device)
}

/// [`import_claude_transcripts`] into any [`EventSink`]: the database
/// writer, or the spool when the daemon holds the writer lock.
pub fn import_claude_transcripts_to(
    sink: &mut dyn EventSink,
    sources: &[TranscriptSource],
    config: &Config,
    device: DeviceId,
) -> Result<ImportSummary> {
    let mut summary = ImportSummary::default();
    let mut suppressed = 0usize;
    let mut sessions: HashSet<SessionId> = HashSet::new();
    let mut warn = |summary: &mut ImportSummary, message: String| {
        if summary.warnings.len() < MAX_WARNINGS {
            summary.warnings.push(message);
        } else {
            suppressed += 1;
        }
    };

    // Read the head of every file first: which sessions the run touches
    // decides what is looked up in the database, once, before anything is
    // written.
    let peeks: Vec<Result<Peek>> = sources.iter().map(|s| peek(&s.path)).collect();
    let session_of = |source: &TranscriptSource, peek: &Peek| -> Option<String> {
        if source.is_subagent() {
            peek.session_id.clone()
        } else {
            source.stem().or_else(|| peek.session_id.clone())
        }
    };
    let wanted: HashSet<String> = sources
        .iter()
        .zip(&peeks)
        .filter_map(|(source, peek)| session_of(source, peek.as_ref().ok()?))
        .collect();
    let (firsts, lasts): (Vec<_>, Vec<_>) = sources
        .iter()
        .zip(&peeks)
        .filter_map(|(source, peek)| Some((peek.as_ref().ok()?.first_ts, source.modified_at)))
        .unzip();
    let mut reconciler = match sink.stored(
        &Provider::ClaudeCode,
        &wanted,
        Window::around(&firsts, &lasts),
    ) {
        Ok(index) => Reconciler::new(index),
        Err(why) => {
            warn(
                &mut summary,
                format!(
                    "could not read the database to compare with what hooks captured ({why}); \
                     every reconstructed event is written, so prompts and turns of sessions \
                     that hooks also saw may count twice"
                ),
            );
            Reconciler::default()
        }
    };

    // One batcher for the whole run (see `import_codex_rollouts`).
    let mut batcher = Batcher::new(sink);
    for (source, peek) in sources.iter().zip(peeks) {
        summary.files += 1;
        let label = source
            .project_slug
            .as_deref()
            .map(|slug| {
                format!(
                    "{slug}/{}",
                    source
                        .path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                )
            })
            .unwrap_or_else(|| source.path.display().to_string());
        let peek = match peek {
            Ok(p) => p,
            Err(e) => {
                summary.files_failed += 1;
                warn(&mut summary, format!("{label}: cannot read: {e}"));
                continue;
            }
        };
        let file = match File::open(&source.path) {
            Ok(f) => f,
            Err(e) => {
                summary.files_failed += 1;
                warn(&mut summary, format!("{label}: cannot open: {e}"));
                continue;
            }
        };

        let (mut project, project_warning) = project_for(&peek, source, &device);
        if let Some(w) = project_warning {
            warn(&mut summary, format!("{label}: {w}"));
        }
        // The session's own hook events say which project it was in. The
        // path-derived identity below is a fallback for a checkout that is
        // gone, and differs from the remote-derived one hooks computed.
        if let Some(hooked) = session_of(source, &peek).and_then(|s| reconciler.hooked_project(&s))
        {
            project = ProjectRef {
                branch: project.branch.take(),
                ..hooked
            };
        }
        let ctx = CaptureContext {
            device_id: device,
            capture_mode: config.capture_mode,
            project,
            captured_at: Timestamp::now(),
            provider_version: None,
            hook_version: None,
        };
        let mut opts = TranscriptOptions::for_capture_mode(config.capture_mode);
        opts.session_id_hint = if source.is_subagent() {
            None
        } else {
            source.stem()
        };
        if let Some(meta) = subagent_meta(&source.path) {
            opts.agent_type_hint = meta.agent_type;
            opts.parent_tool_use_id = meta.tool_use_id;
        }

        let lines = LossyLines::new(BufReader::new(file));
        let oversized = lines.oversized_counter();
        let import = parse_claude_transcript(lines, &ctx, &opts);
        summary.events_seen += import.events.len();
        summary.lines_skipped += import.stats.malformed_lines + oversized.get();
        if oversized.get() > 0 {
            warn(
                &mut summary,
                format!(
                    "{label}: {} line(s) longer than {} MiB skipped",
                    oversized.get(),
                    MAX_LINE_BYTES / (1024 * 1024)
                ),
            );
        }
        summary.bytes += source.bytes;
        for w in import.warnings {
            warn(&mut summary, format!("{label}: {w}"));
        }
        for ev in &import.events {
            sessions.insert(ev.session_id);
        }
        for (ev, legacy) in import.events.into_iter().zip(import.legacy_ids) {
            if reconciler.should_skip(&ev, legacy) {
                continue;
            }
            batcher.push(ev)?;
        }
    }
    batcher.flush()?;
    let written = batcher.total;
    summary.accepted += written.accepted;
    summary.duplicates += written.duplicates;
    summary.queued += written.queued;
    sink.finish()?;
    summary.skipped_captured = reconciler.skipped().total();
    summary.sessions = sessions.len();
    if suppressed > 0 {
        summary
            .warnings
            .push(format!("{suppressed} further warning(s) suppressed"));
    }
    Ok(summary)
}

/// Content-free facts read from the first lines of a transcript.
#[derive(Debug, Default)]
struct Peek {
    cwd: Option<String>,
    git_branch: Option<String>,
    /// The first `sessionId` seen: the parent's for a subagent file.
    session_id: Option<String>,
    /// The first parseable `timestamp`.
    first_ts: Option<Timestamp>,
}

fn peek(path: &Path) -> Result<Peek> {
    let file = File::open(path).map_err(|e| io_at(path, e))?;
    let mut peek = Peek::default();
    for line in LossyLines::new(BufReader::new(file)).take(PEEK_LINES) {
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        if peek.cwd.is_none() {
            peek.cwd = text("cwd");
        }
        if peek.git_branch.is_none() {
            peek.git_branch = text("gitBranch");
        }
        if peek.session_id.is_none() {
            peek.session_id = text("sessionId");
        }
        if peek.first_ts.is_none() {
            peek.first_ts = text("timestamp").and_then(|t| Timestamp::parse(&t));
        }
        if peek.cwd.is_some()
            && peek.git_branch.is_some()
            && peek.session_id.is_some()
            && peek.first_ts.is_some()
        {
            break;
        }
    }
    Ok(peek)
}

/// The project a transcript belongs to. Identity comes from the repository
/// containing the transcript's `cwd` when that directory still exists (so
/// reconstructed and hook-captured events of the same repository share a
/// project id); otherwise from the `cwd` text alone. The branch is the one
/// recorded in the transcript (the branch *at the time*), and `head` is left
/// unknown for the same reason.
///
/// This is the fallback. When hook events of the same session are already
/// stored, their project identity replaces this one (see
/// `import_claude_transcripts_to`): a worktree that was deleted, or a
/// repository without a remote that moved, would otherwise get a path-hash id
/// that differs from the remote-based one the hooks computed.
fn project_for(
    peek: &Peek,
    source: &TranscriptSource,
    device: &DeviceId,
) -> (ProjectRef, Option<String>) {
    match &peek.cwd {
        Some(cwd) => {
            let cwd_path = Path::new(cwd);
            let git = if cwd_path.is_dir() {
                git_info(cwd_path)
            } else {
                None
            };
            let mut project = match &git {
                Some(g) => {
                    ProjectRef::derive(&g.root.to_string_lossy(), g.remote.as_deref(), device)
                }
                None => ProjectRef::derive(cwd, None, device),
            };
            project.branch = peek
                .git_branch
                .clone()
                .or_else(|| git.as_ref().and_then(|g| g.branch.clone()));
            (project, None)
        }
        None => {
            let root = source
                .path
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "unknown".to_string());
            let project = ProjectRef::derive(&root, None, device);
            (
                project,
                Some(
                    "no `cwd` in the transcript; project derived from the transcript directory"
                        .to_string(),
                ),
            )
        }
    }
}

/// `agent-<id>.meta.json` next to a subagent transcript.
#[derive(Debug, Default)]
struct SubagentMeta {
    agent_type: Option<String>,
    tool_use_id: Option<String>,
}

fn subagent_meta(path: &Path) -> Option<SubagentMeta> {
    if !path
        .components()
        .any(|c| matches!(c, Component::Normal(n) if n == "subagents"))
    {
        return None;
    }
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    let meta_path = path.with_file_name(format!("{stem}.meta.json"));
    let text = std::fs::read_to_string(meta_path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let short = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() <= 64 && !s.chars().any(char::is_whitespace))
            .map(str::to_string)
    };
    Some(SubagentMeta {
        agent_type: short("agentType"),
        tool_use_id: short("toolUseId"),
    })
}

/// Longest line the importers keep, in bytes (the Codex importer's cap, for
/// the same reason: the largest line seen in the wild is 12 MB, and a corrupt
/// or hostile file must not make the importer allocate without bound — one
/// 420 MB line took 2.96 GB of memory before this cap).
pub use attemptdb_adapters::transcript::codex::DEFAULT_MAX_LINE_BYTES as MAX_LINE_BYTES;

/// Line iterator that never fails on invalid UTF-8 (replaced lossily) and
/// strips the trailing newline. A line longer than the cap is read through a
/// buffer that never grows past it: the rest of the line is consumed and
/// thrown away, an empty line stands in for it (so the numbering of the lines
/// after it is the file's own), and it is counted, see
/// [`LossyLines::oversized_counter`].
pub(crate) struct LossyLines<R: BufRead> {
    reader: R,
    buf: Vec<u8>,
    cap: usize,
    oversized: Rc<Cell<usize>>,
}

impl<R: BufRead> LossyLines<R> {
    pub(crate) fn new(reader: R) -> Self {
        Self::with_cap(reader, MAX_LINE_BYTES)
    }

    pub(crate) fn with_cap(reader: R, cap: usize) -> Self {
        Self {
            reader,
            buf: Vec::with_capacity(8 * 1024),
            cap,
            oversized: Rc::new(Cell::new(0)),
        }
    }

    /// How many lines over the cap were skipped so far. A handle, so the
    /// count can be read after the iterator was handed to a parser.
    pub(crate) fn oversized_counter(&self) -> Rc<Cell<usize>> {
        Rc::clone(&self.oversized)
    }
}

impl<R: BufRead> Iterator for LossyLines<R> {
    type Item = String;

    fn next(&mut self) -> Option<String> {
        self.buf.clear();
        let mut oversized = false;
        let mut any = false;
        loop {
            let chunk = match self.reader.fill_buf() {
                Ok(c) => c,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return None,
            };
            if chunk.is_empty() {
                break;
            }
            any = true;
            let newline = chunk.iter().position(|&b| b == b'\n');
            let body = newline.unwrap_or(chunk.len());
            let consume = newline.map_or(chunk.len(), |i| i + 1);
            if !oversized {
                if self.buf.len() + body > self.cap {
                    oversized = true;
                    self.buf.clear();
                    self.buf.shrink_to(8 * 1024);
                } else {
                    self.buf.extend_from_slice(&chunk[..body]);
                }
            }
            self.reader.consume(consume);
            if newline.is_some() {
                break;
            }
        }
        if !any {
            return None;
        }
        if oversized {
            self.oversized.set(self.oversized.get() + 1);
            return Some(String::new());
        }
        while matches!(self.buf.last(), Some(b'\n' | b'\r')) {
            self.buf.pop();
        }
        Some(String::from_utf8_lossy(&self.buf).into_owned())
    }
}

#[cfg(test)]
mod dedup_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use attemptdb_core::EventKind;
    use attemptdb_storage::{OpenOptions, ScanFilter};

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/transcripts/claude_code")
            .join(format!("{name}.jsonl"))
    }

    fn open_db(root: &Path) -> (Database, DeviceId) {
        let device = DeviceId::derive(&["import-tests"]);
        let dir = root.join(".attemptdb");
        Database::create(&dir, device).unwrap();
        let db = Database::open(
            &dir,
            OpenOptions {
                create: false,
                ..Default::default()
            },
        )
        .unwrap();
        (db, device)
    }

    #[test]
    fn slugs_follow_claude_code_rules() {
        assert_eq!(
            project_slug(Path::new("/home/dev/example/project")),
            "-home-dev-example-project"
        );
        assert_eq!(
            project_slug(Path::new("/Users/me/.claude/worktrees/a_b")),
            "-Users-me--claude-worktrees-a-b"
        );
        assert_eq!(project_slug(Path::new("C:\\code\\proj")), "C--code-proj");
        assert_eq!(
            slug_of(Path::new("/x/projects/-home-dev-p/s.jsonl")).as_deref(),
            Some("-home-dev-p")
        );
        assert_eq!(
            slug_of(Path::new(
                "/x/projects/-home-dev-p/s/subagents/agent-1.jsonl"
            ))
            .as_deref(),
            Some("-home-dev-p")
        );
        assert_eq!(slug_of(Path::new("/tmp/loose.jsonl")), None);
    }

    #[test]
    fn discovery_filters_by_project_root_and_finds_subagents() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let slug = projects.join("-home-dev-example-project");
        let other = projects.join("-home-dev-other");
        let session = "11111111-1111-4111-8111-111111111111";
        std::fs::create_dir_all(slug.join(session).join("subagents")).unwrap();
        std::fs::create_dir_all(slug.join("memory")).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::copy(fixture("basic_turn"), slug.join(format!("{session}.jsonl"))).unwrap();
        std::fs::copy(
            fixture("subagent_sidechain"),
            slug.join(session)
                .join("subagents")
                .join("agent-a1b2c3d4.jsonl"),
        )
        .unwrap();
        std::fs::write(
            slug.join(session)
                .join("subagents")
                .join("agent-a1b2c3d4.meta.json"),
            r#"{"agentType":"Explore","description":"x","toolUseId":"toolu_0003"}"#,
        )
        .unwrap();
        std::fs::write(slug.join("memory").join("notes.jsonl"), "{}\n").unwrap();
        std::fs::copy(
            fixture("interrupted_turn"),
            other.join("33333333-3333-4333-8333-333333333333.jsonl"),
        )
        .unwrap();

        let all = discover_in(std::slice::from_ref(&projects), None);
        assert_eq!(all.len(), 3, "{all:?}");
        assert!(all.iter().all(|s| s.bytes > 0 && s.modified_at.is_some()));

        let mine = discover_in(
            std::slice::from_ref(&projects),
            Some(Path::new("/home/dev/example/project")),
        );
        assert_eq!(mine.len(), 2, "{mine:?}");
        assert_eq!(mine.iter().filter(|s| s.is_subagent()).count(), 1);
        assert!(
            mine.iter()
                .all(|s| s.project_slug.as_deref() == Some("-home-dev-example-project"))
        );
        assert_eq!(mine[0].stem().as_deref(), Some(session));
        assert!(
            !mine[0].is_subagent(),
            "main transcript sorts before its subagents"
        );

        let none = discover_in(
            std::slice::from_ref(&projects),
            Some(Path::new("/home/dev/nothing")),
        );
        assert!(none.is_empty());
        // Case-insensitive match (macOS file systems) and duplicate dirs.
        let ci = discover_in(
            &[projects.clone(), projects.clone()],
            Some(Path::new("/HOME/dev/example/PROJECT")),
        );
        assert_eq!(ci.len(), 2);
        // Explicit paths: a file, or a directory walked recursively.
        assert_eq!(collect_transcripts(&fixture("basic_turn")).len(), 1);
        assert_eq!(
            collect_transcripts(&slug).len(),
            2,
            "explicit dirs are walked; memory/ is skipped"
        );

        let meta = subagent_meta(&mine[1].path).unwrap();
        assert_eq!(meta.agent_type.as_deref(), Some("Explore"));
        assert_eq!(meta.tool_use_id.as_deref(), Some("toolu_0003"));
        assert!(subagent_meta(&mine[0].path).is_none());
    }

    #[test]
    fn reimport_is_idempotent_and_growth_adds_only_new_events() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut db, device) = open_db(tmp.path());
        let config = Config::default();
        let all = std::fs::read_to_string(fixture("basic_turn")).unwrap();
        let lines: Vec<&str> = all.lines().collect();
        let projects = tmp
            .path()
            .join("projects")
            .join("-home-dev-example-project");
        std::fs::create_dir_all(&projects).unwrap();
        let path = projects.join("11111111-1111-4111-8111-111111111111.jsonl");
        // First 16 lines: turn one complete (through `turn_duration`).
        std::fs::write(&path, format!("{}\n", lines[..16].join("\n"))).unwrap();
        let sources = collect_transcripts(&path);
        assert_eq!(sources.len(), 1);

        let first = import_claude_transcripts(&mut db, &sources, &config, device).unwrap();
        assert_eq!(
            (
                first.files,
                first.events_seen,
                first.accepted,
                first.duplicates,
                first.sessions
            ),
            (1, 9, 9, 0, 1)
        );
        assert!(first.warnings.is_empty(), "{:?}", first.warnings);

        let second = import_claude_transcripts(&mut db, &sources, &config, device).unwrap();
        assert_eq!(
            (second.events_seen, second.accepted, second.duplicates),
            (9, 0, 9)
        );

        // The session continues: append the remaining lines.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        writeln!(f, "{}", lines[16..].join("\n")).unwrap();
        drop(f);
        let third =
            import_claude_transcripts(&mut db, &collect_transcripts(&path), &config, device)
                .unwrap();
        assert_eq!(
            (third.events_seen, third.accepted, third.duplicates),
            (12, 3, 9)
        );

        let events = db.scan(&ScanFilter::default()).unwrap();
        assert_eq!(events.len(), 12);
        assert!(
            events
                .iter()
                .all(|e| e.attrs.get("reconstructed") == Some(&Value::Bool(true)))
        );
        assert!(
            events
                .iter()
                .all(|e| e.hook_version.is_none() && e.raw.is_none())
        );
        assert!(events.iter().all(|e| e.is_ingested()));
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == EventKind::PromptSubmitted)
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == EventKind::TurnStopped)
                .count(),
            2
        );
        // The cwd does not exist on this machine: project derived from it.
        assert!(
            events
                .iter()
                .all(|e| e.project.root == "/home/dev/example/project"
                    && e.project.branch.as_deref() == Some("main"))
        );
        assert_eq!(db.stats().memtable_rows, 0, "flushed at the end");
    }

    #[test]
    fn metadata_only_import_stores_no_content() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut db, device) = open_db(tmp.path());
        let config = Config {
            capture_mode: attemptdb_core::CaptureMode::MetadataOnly,
            ..Config::default()
        };
        let sources = collect_transcripts(&fixture("basic_turn"));
        let summary = import_claude_transcripts(&mut db, &sources, &config, device).unwrap();
        assert_eq!(summary.accepted, 12);
        let events = db.scan(&ScanFilter::default()).unwrap();
        let serialised = serde_json::to_string(&events).unwrap();
        assert!(!serialised.contains("CANARY_"));
        assert!(events.iter().all(|e| e.content.is_none()));
    }

    #[test]
    fn unreadable_and_odd_sources_are_reported_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut db, device) = open_db(tmp.path());
        let missing = TranscriptSource {
            path: tmp.path().join("missing.jsonl"),
            project_slug: None,
            modified_at: None,
            bytes: 0,
        };
        let no_cwd = tmp.path().join("no-cwd.jsonl");
        std::fs::write(&no_cwd, "{\"type\":\"user\",\"sessionId\":\"s-1\",\"uuid\":\"u1\",\"timestamp\":\"2026-08-20T09:00:00.000Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n\u{FF}not json\n").unwrap();
        let sources = vec![missing, TranscriptSource::from_path(&no_cwd)];
        let summary =
            import_claude_transcripts(&mut db, &sources, &Config::default(), device).unwrap();
        assert_eq!(
            (summary.files, summary.files_failed, summary.accepted),
            (2, 1, 2)
        );
        assert!(summary.warnings.iter().any(|w| w.contains("cannot read")));
        assert!(summary.warnings.iter().any(|w| w.contains("no `cwd`")));
        assert!(summary.warnings.iter().any(|w| w.contains("invalid JSON")));
    }

    #[test]
    fn a_line_over_the_cap_is_counted_and_never_buffered() {
        use std::io::Cursor;
        let mut text = b"first\n".to_vec();
        text.extend(std::iter::repeat_n(b'x', 50_000));
        text.extend_from_slice(b"\nsecond\r\n");
        text.extend(std::iter::repeat_n(b'y', 3_000));
        text.extend_from_slice(b"\nlast, no newline");
        // A small reader buffer: the long line arrives in many chunks.
        let mut lines =
            LossyLines::with_cap(BufReader::with_capacity(256, Cursor::new(text)), 1024);
        let count = lines.oversized_counter();
        let got: Vec<String> = lines.by_ref().collect();
        // An empty line stands where an oversized one was, so the lines after
        // it keep the file's own numbers.
        assert_eq!(got, ["first", "", "second", "", "last, no newline"]);
        assert_eq!(count.get(), 2);
        assert!(
            lines.buf.capacity() <= 16 * 1024,
            "the buffer grew to {} bytes",
            lines.buf.capacity()
        );
        // A line exactly at the cap is kept.
        let at_cap = format!("{}\nnext\n", "z".repeat(1024));
        let mut lines = LossyLines::with_cap(Cursor::new(at_cap.into_bytes()), 1024);
        assert_eq!(lines.next().map(|l| l.len()), Some(1024));
        assert_eq!(lines.next().as_deref(), Some("next"));
        assert_eq!(lines.oversized_counter().get(), 0);
    }

    #[test]
    fn a_huge_line_costs_one_skipped_line_not_the_import() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut db, device) = open_db(tmp.path());
        let user = "{\"type\":\"user\",\"sessionId\":\"s-1\",\"uuid\":\"u1\",\"cwd\":\"/work/p\",\"timestamp\":\"2026-08-20T09:00:00.000Z\",\"message\":{\"role\":\"user\",\"content\":\"before\"}}";
        let after = "{\"type\":\"user\",\"sessionId\":\"s-1\",\"uuid\":\"u2\",\"cwd\":\"/work/p\",\"timestamp\":\"2026-08-20T09:01:00.000Z\",\"message\":{\"role\":\"user\",\"content\":\"after\"}}";
        let path = tmp.path().join("huge.jsonl");
        {
            use std::io::Write;
            let mut f = std::io::BufWriter::new(File::create(&path).unwrap());
            writeln!(f, "{user}").unwrap();
            // One line a megabyte over the cap, written in pieces.
            let piece = vec![b'x'; 1024 * 1024];
            for _ in 0..=(MAX_LINE_BYTES / piece.len()) {
                f.write_all(&piece).unwrap();
            }
            f.write_all(b"\n").unwrap();
            writeln!(f, "{after}").unwrap();
        }
        let sources = vec![TranscriptSource::from_path(&path)];
        let summary =
            import_claude_transcripts(&mut db, &sources, &Config::default(), device).unwrap();
        assert_eq!(summary.lines_skipped, 1, "{summary:?}");
        assert!(
            summary
                .warnings
                .iter()
                .any(|w| w.contains("1 line(s) longer than 32 MiB skipped")),
            "{:?}",
            summary.warnings
        );
        // Both prompts around it are imported.
        let prompts = db
            .scan(&ScanFilter::default())
            .unwrap()
            .iter()
            .filter(|e| e.kind == EventKind::PromptSubmitted)
            .count();
        assert_eq!(prompts, 2);
    }
}
