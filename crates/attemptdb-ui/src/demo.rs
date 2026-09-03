//! Demo mode: a bundled, clearly labelled build-history dataset for a
//! database that has not captured anything yet.
//!
//! An evaluator who arrives from GitHub has no events, and an empty timeline
//! proves nothing. `?demo=1` serves a *separate* database, generated from
//! the story below, that shows the surfaces AttemptDB exists for: a failed
//! attempt superseded by a working one, a cross-agent handoff, decisions,
//! commits, and one unanswered permission request in the Needs You queue.
//!
//! Three rules keep it honest:
//!
//! - it is a different database in a different directory — a demo event can
//!   never reach the user's own database;
//! - every event carries `reconstructed: true` and
//!   `reconstructed_from: "attemptdb-demo"`, so the coverage panels count it
//!   as reconstructed rather than hook-captured;
//! - every page served in demo mode carries a banner saying so.
//!
//! The story is AttemptDB's own storage-engine work, written out here rather
//! than exported from a real machine: a committed snapshot of somebody's
//! database would carry their paths, prompts and repository names.

use anyhow::{Context, Result};
use attemptdb_core::event::{EventContent, Provider};
use attemptdb_core::{
    CaptureMode, DeviceId, Event, EventId, EventKind, Outcome, OutcomeStatus, PortablePath,
    ProjectRef, Timestamp, ToolCategory, ToolRef,
};
use attemptdb_storage::{Database, OpenOptions};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Bump when the story changes: an existing demo database is rebuilt.
pub const DEMO_VERSION: u32 = 1;

/// The demo database is regenerated when its anchor is older than this, so
/// "live execution" on the demo is about the last few minutes rather than
/// about whenever the evaluator first opened it. It has to stay inside
/// [`crate::LIVE_WINDOW_MS`], or the demo opens on an empty live panel —
/// the one thing the Overview exists to show. Rebuilding is 43 events.
const MAX_AGE_US: i64 = 20 * 60 * 1_000_000;

/// The story ends this long before now.
const ENDS_AGO_US: i64 = 4 * 60 * 1_000_000;
/// ...and starts this long before now.
const SPAN_US: i64 = 100 * 60 * 1_000_000;

const ROOT: &str = "/home/dev/src/attemptdb";
const REMOTE: &str = "git@github.com:example/attemptdb.git";

/// Where the demo database lives, next to the snapshot cache.
pub fn demo_dir(cache_dir: &Path) -> PathBuf {
    cache_dir.join("demo").join(".attemptdb")
}

fn marker(db_dir: &Path) -> PathBuf {
    db_dir.with_file_name("demo-generated.json")
}

/// Build the demo database if it is missing, stale, or from an older story.
/// Returns the directory to open.
pub fn ensure(cache_dir: &Path) -> Result<PathBuf> {
    let db_dir = demo_dir(cache_dir);
    let now = Timestamp::now();
    if fresh(&db_dir, now) {
        return Ok(db_dir);
    }
    // A rebuild replaces the whole directory: the demo owns it exclusively.
    if db_dir.exists() {
        std::fs::remove_dir_all(&db_dir)
            .with_context(|| format!("removing the stale demo database {}", db_dir.display()))?;
    }
    let parent = db_dir.parent().expect("demo dir has a parent");
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let device = DeviceId::derive(&["attemptdb-demo-device"]);
    Database::create(&db_dir, device).context("creating the demo database")?;
    let mut db =
        Database::open(&db_dir, OpenOptions::default()).context("opening the demo database")?;
    db.ingest(events(now)).context("writing the demo events")?;
    db.flush().context("flushing the demo database")?;
    drop(db);
    std::fs::write(
        marker(&db_dir),
        serde_json::json!({ "version": DEMO_VERSION, "generated_at": now.to_rfc3339(), "generated_us": now.as_micros() })
            .to_string(),
    )
    .with_context(|| format!("writing {}", marker(&db_dir).display()))?;
    Ok(db_dir)
}

fn fresh(db_dir: &Path, now: Timestamp) -> bool {
    if !Database::exists(db_dir) {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(marker(db_dir)) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    if v.get("version").and_then(Value::as_u64) != Some(DEMO_VERSION as u64) {
        return false;
    }
    match v.get("generated_us").and_then(Value::as_i64) {
        Some(us) => now.as_micros() - us < MAX_AGE_US,
        None => false,
    }
}

// ---------------------------------------------------------------------------
// The story
// ---------------------------------------------------------------------------

struct Builder {
    device: DeviceId,
    project: ProjectRef,
    /// Story time zero.
    base: Timestamp,
    seq: u64,
    events: Vec<Event>,
}

struct Tool {
    name: &'static str,
    category: ToolCategory,
    call_id: &'static str,
    paths: &'static [&'static str],
}

const fn edit(call_id: &'static str, paths: &'static [&'static str]) -> Tool {
    Tool {
        name: "Edit",
        category: ToolCategory::FileEdit,
        call_id,
        paths,
    }
}

const fn read(call_id: &'static str, paths: &'static [&'static str]) -> Tool {
    Tool {
        name: "Read",
        category: ToolCategory::FileRead,
        call_id,
        paths,
    }
}

const fn shell(call_id: &'static str) -> Tool {
    Tool {
        name: "Bash",
        category: ToolCategory::Shell,
        call_id,
        paths: &[],
    }
}

const fn patch(call_id: &'static str, paths: &'static [&'static str]) -> Tool {
    Tool {
        name: "apply_patch",
        category: ToolCategory::FileEdit,
        call_id,
        paths,
    }
}

impl Builder {
    fn new(base: Timestamp) -> Self {
        let device = DeviceId::derive(&["attemptdb-demo-device"]);
        let project = ProjectRef::derive(ROOT, Some(REMOTE), &device);
        Self {
            device,
            project,
            base,
            seq: 0,
            events: Vec::new(),
        }
    }

    fn at(&self, secs: i64) -> Timestamp {
        Timestamp::from_micros(self.base.as_micros() + secs * 1_000_000)
    }

    fn push(
        &mut self,
        provider: Provider,
        session: &str,
        kind: EventKind,
        name: &str,
        secs: i64,
    ) -> &mut Event {
        let mut ev = Event::new(
            self.device,
            provider,
            name,
            kind,
            self.project.clone(),
            session.to_string(),
            CaptureMode::LocalSemantic,
            "attemptdb-demo",
        );
        self.seq += 1;
        // Ids are derived from the story position, not from the clock: the
        // same demo has the same ids on every machine and every rebuild.
        ev.event_id = EventId::derive(&[
            "attemptdb-demo",
            &DEMO_VERSION.to_string(),
            &self.seq.to_string(),
        ]);
        ev.observed_at = self.at(secs);
        ev.captured_at = ev.observed_at;
        ev.attrs.insert("reconstructed".into(), Value::Bool(true));
        ev.attrs
            .insert("reconstructed_from".into(), Value::from("attemptdb-demo"));
        self.events.push(ev);
        self.events.last_mut().expect("just pushed")
    }

    fn session_start(&mut self, provider: Provider, session: &str, secs: i64) {
        let ev = self.push(
            provider,
            session,
            EventKind::SessionStarted,
            "SessionStart",
            secs,
        );
        ev.attrs.insert("source".into(), Value::from("startup"));
    }

    fn session_end(&mut self, provider: Provider, session: &str, secs: i64) {
        let ev = self.push(
            provider,
            session,
            EventKind::SessionEnded,
            "SessionEnd",
            secs,
        );
        ev.attrs.insert("reason".into(), Value::from("exit"));
    }

    fn prompt(&mut self, provider: Provider, session: &str, secs: i64, text: &str) {
        let ev = self.push(
            provider,
            session,
            EventKind::PromptSubmitted,
            "UserPromptSubmit",
            secs,
        );
        ev.content = Some(EventContent {
            prompt: Some(text.to_string()),
            ..Default::default()
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn tool(
        &mut self,
        provider: Provider,
        session: &str,
        kind: EventKind,
        name: &str,
        secs: i64,
        t: &Tool,
        command: Option<&str>,
    ) -> &mut Event {
        let root = self.project.root.clone();
        let paths: Vec<PortablePath> = t
            .paths
            .iter()
            .map(|p| PortablePath::from_raw(&format!("{root}/{p}"), Some(&root)))
            .collect();
        let ev = self.push(provider, session, kind, name, secs);
        ev.tool = Some(ToolRef {
            name: t.name.to_string(),
            category: t.category,
            call_id: Some(t.call_id.to_string()),
        });
        ev.paths = paths;
        if let Some(cmd) = command {
            ev.content = Some(EventContent {
                command: Some(cmd.to_string()),
                ..Default::default()
            });
        }
        ev
    }

    fn call_ok(
        &mut self,
        provider: Provider,
        session: &str,
        span: (i64, i64),
        t: &Tool,
        cmd: Option<&str>,
    ) {
        self.tool(
            provider.clone(),
            session,
            EventKind::ToolCallStarted,
            "PreToolUse",
            span.0,
            t,
            cmd,
        );
        let ev = self.tool(
            provider,
            session,
            EventKind::ToolCallFinished,
            "PostToolUse",
            span.1,
            t,
            cmd,
        );
        ev.outcome = Some(Outcome::success());
    }

    fn call_failed(
        &mut self,
        provider: Provider,
        session: &str,
        span: (i64, i64),
        t: &Tool,
        class: &str,
        cmd: Option<&str>,
    ) {
        self.tool(
            provider.clone(),
            session,
            EventKind::ToolCallStarted,
            "PreToolUse",
            span.0,
            t,
            cmd,
        );
        let ev = self.tool(
            provider,
            session,
            EventKind::ToolCallFailed,
            "PostToolUseFailure",
            span.1,
            t,
            cmd,
        );
        ev.outcome = Some(Outcome::failure(Some(class.to_string())));
    }

    fn commit(
        &mut self,
        provider: Provider,
        session: &str,
        span: (i64, i64),
        call_id: &'static str,
        sha: &str,
    ) {
        let t = shell(call_id);
        self.tool(
            provider.clone(),
            session,
            EventKind::ToolCallStarted,
            "PreToolUse",
            span.0,
            &t,
            Some("git commit -m 'storage: framed WAL with CRC32C per frame'"),
        );
        let ev = self.tool(
            provider,
            session,
            EventKind::ToolCallFinished,
            "PostToolUse",
            span.1,
            &t,
            Some("git commit -m 'storage: framed WAL with CRC32C per frame'"),
        );
        ev.outcome = Some(Outcome {
            status: OutcomeStatus::Success,
            class: None,
            exit_code: Some(0),
        });
        ev.attrs
            .insert("git_subcommand".into(), Value::from("commit"));
        ev.attrs
            .insert("command_category".into(), Value::from("git"));
        ev.project.head = Some(sha.to_string());
        ev.project.branch = Some("main".to_string());
    }

    fn stop(&mut self, provider: Provider, session: &str, secs: i64) {
        self.push(provider, session, EventKind::TurnStopped, "Stop", secs);
    }

    fn permission_request(&mut self, provider: Provider, session: &str, secs: i64, t: &Tool) {
        let ev = self.push(
            provider,
            session,
            EventKind::PermissionRequested,
            "PermissionRequest",
            secs,
        );
        ev.tool = Some(ToolRef {
            name: t.name.to_string(),
            category: t.category,
            call_id: Some(t.call_id.to_string()),
        });
    }
}

/// The demo event stream, ending a few minutes before `now`.
///
/// Claude Code builds the framed WAL: the first attempt at the recovery test
/// fails on a torn tail, the retry works, the work is committed. Codex picks
/// the same file up for the CRC check (a handoff), and a third session is
/// waiting for permission to run a destructive command — the one item in
/// Needs You.
pub fn events(now: Timestamp) -> Vec<Event> {
    let base = Timestamp::from_micros(now.as_micros() - ENDS_AGO_US - SPAN_US);
    let mut b = Builder::new(base);
    let claude = Provider::ClaudeCode;
    let codex = Provider::Codex;
    let s1 = "demo-claude-wal";
    let s2 = "demo-codex-crc";
    let s3 = "demo-claude-release";
    let wal = &["crates/attemptdb-storage/src/wal.rs"];
    let wal_test = &["crates/attemptdb-storage/tests/recovery.rs"];

    // --- Session 1: the WAL, one failed attempt and its retry -------------
    b.session_start(claude.clone(), s1, 0);
    b.prompt(
        claude.clone(),
        s1,
        30,
        "Write the framed WAL: length, CRC32C and payload per frame, and recover a torn tail by truncating to the last good frame.",
    );
    b.call_ok(claude.clone(), s1, (40, 44), &read("r1", wal), None);
    b.call_ok(claude.clone(), s1, (60, 70), &edit("e1", wal), None);
    b.call_ok(
        claude.clone(),
        s1,
        (80, 130),
        &shell("t1"),
        Some("cargo test -p attemptdb-storage"),
    );
    b.stop(claude.clone(), s1, 140);

    b.prompt(
        claude.clone(),
        s1,
        180,
        "The recovery test should kill the process mid-append and reopen the WAL.",
    );
    b.call_ok(claude.clone(), s1, (190, 196), &edit("e2", wal_test), None);
    // The attempt that fails, and stays failed in the history.
    b.call_failed(
        claude.clone(),
        s1,
        (200, 260),
        &shell("t2"),
        "test_failure",
        Some("cargo test -p attemptdb-storage recovery"),
    );
    b.call_ok(claude.clone(), s1, (280, 292), &edit("e3", wal), None);
    b.call_ok(
        claude.clone(),
        s1,
        (300, 355),
        &shell("t3"),
        Some("cargo test -p attemptdb-storage recovery"),
    );
    b.commit(
        claude.clone(),
        s1,
        (360, 366),
        "g1",
        "9f2c1ab4d3e57081cc9a2f60b7d41e5a83c02b19",
    );
    b.stop(claude.clone(), s1, 380);
    b.session_end(claude.clone(), s1, 420);

    // --- Session 2: Codex continues on the same file (a handoff) ----------
    b.session_start(codex.clone(), s2, 600);
    b.prompt(
        codex.clone(),
        s2,
        620,
        "Check the CRC of every frame on recovery and add a corrupted-frame case to the test.",
    );
    b.call_ok(codex.clone(), s2, (640, 652), &patch("p1", wal), None);
    b.call_failed(
        codex.clone(),
        s2,
        (660, 700),
        &shell("t4"),
        "test_failure",
        Some("cargo test -p attemptdb-storage"),
    );
    b.call_ok(codex.clone(), s2, (720, 733), &patch("p2", wal), None);
    b.call_ok(
        codex.clone(),
        s2,
        (740, 800),
        &shell("t5"),
        Some("cargo test -p attemptdb-storage"),
    );
    b.commit(
        codex.clone(),
        s2,
        (810, 816),
        "g2",
        "1d77e5c0a94b2386ff10d4e7c5b93a20e6f814dd",
    );
    b.stop(codex.clone(), s2, 830);
    b.session_end(codex.clone(), s2, 860);

    // --- Session 3: still open, waiting for a person ----------------------
    let release_secs = SPAN_US / 1_000_000 - 300;
    b.session_start(claude.clone(), s3, release_secs);
    b.prompt(
        claude.clone(),
        s3,
        release_secs + 20,
        "Cut the 0.3.0 release: bump the versions, run the full suite and tag it.",
    );
    b.call_ok(
        claude.clone(),
        s3,
        (release_secs + 30, release_secs + 40),
        &edit("e4", &["Cargo.toml"]),
        None,
    );
    b.call_ok(
        claude.clone(),
        s3,
        (release_secs + 50, release_secs + 200),
        &shell("t6"),
        Some("cargo test --workspace"),
    );
    // The last event in the session: nothing has answered it.
    b.permission_request(claude.clone(), s3, release_secs + 210, &shell("t7"));

    b.events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_story_is_deterministic_and_labelled() {
        let now = Timestamp::now();
        let a = events(now);
        let b = events(Timestamp::from_micros(now.as_micros() + 5_000_000));
        assert!(a.len() > 40, "{} events", a.len());
        // Ids do not depend on the anchor; timestamps do.
        let ids_a: Vec<_> = a.iter().map(|e| e.event_id).collect();
        let ids_b: Vec<_> = b.iter().map(|e| e.event_id).collect();
        assert_eq!(ids_a, ids_b);
        assert_ne!(a[0].observed_at, b[0].observed_at);
        for e in &a {
            assert_eq!(e.attrs.get("reconstructed"), Some(&Value::Bool(true)));
            assert_eq!(
                e.attrs.get("reconstructed_from").and_then(Value::as_str),
                Some("attemptdb-demo")
            );
            assert!(!e.project.root.contains("chung"), "sanitised root");
        }
    }

    #[test]
    fn the_story_shows_what_the_product_is_about() {
        let evs = events(Timestamp::now());
        let p = attemptdb_project::project(&evs);
        assert!(
            p.attempts.iter().any(|a| a.outcome.is_failure()),
            "a failed attempt"
        );
        assert!(
            p.attempts.iter().any(|a| a.superseded_by.is_some()),
            "a superseded attempt"
        );
        assert!(!p.handoffs.is_empty(), "a cross-agent handoff");
        assert!(!p.decisions.is_empty(), "a derived decision");
        assert!(!p.commits.is_empty(), "linked commits");
        let queue = p.attention_at(Timestamp::now(), attemptdb_project::DEFAULT_MIN_CONFIDENCE);
        assert_eq!(queue.len(), 1, "one Needs You item: {queue:#?}");
        assert_eq!(
            queue[0].kind,
            attemptdb_project::AttentionKind::PermissionGate
        );
    }

    #[test]
    fn the_demo_never_goes_stale_enough_to_empty_the_live_panel() {
        assert!(
            MAX_AGE_US < crate::LIVE_WINDOW_MS as i64 * 1_000,
            "a demo older than the live window opens on an empty Live execution card"
        );
        // The story's own tail has to be inside the window too.
        assert!(ENDS_AGO_US + MAX_AGE_US < crate::LIVE_WINDOW_MS as i64 * 1_000);
    }

    #[test]
    fn a_generated_database_is_reused_until_it_goes_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = ensure(tmp.path()).unwrap();
        assert!(Database::exists(&dir));
        let first = std::fs::read_to_string(marker(&dir)).unwrap();
        let again = ensure(tmp.path()).unwrap();
        assert_eq!(dir, again);
        assert_eq!(first, std::fs::read_to_string(marker(&dir)).unwrap());
        // An old marker forces a rebuild.
        std::fs::write(
            marker(&dir),
            serde_json::json!({ "version": DEMO_VERSION, "generated_us": 0 }).to_string(),
        )
        .unwrap();
        ensure(tmp.path()).unwrap();
        assert_ne!(first, std::fs::read_to_string(marker(&dir)).unwrap());
    }
}
