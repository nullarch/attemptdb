//! Secrets are masked before content is stored (RFC 0006 §5), on every way
//! an event reaches the database: the spool (hooks), `write_events` (CLI
//! corrections), and an importer's direct sink. `redact_secrets: false` in
//! the config keeps content exactly as captured.

use attemptdb_capture::config::Config;
use attemptdb_capture::import_common::{EventSink, ImportTarget, open_import_target};
use attemptdb_capture::{Locator, ingest};
use attemptdb_core::event::{EventContent, Provider};
use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef};
use attemptdb_storage::{Database, ScanFilter, SpoolWriter};

const PASSWORD: &str = "hunter2-hunter2-hunter2";
const URL_SECRET: &str = "s3cretpass-s3cretpass";

struct Sandbox {
    _tmp: tempfile::TempDir,
    locator: Locator,
    device: DeviceId,
}

fn sandbox(config: Option<&str>) -> Sandbox {
    // Short prefix: a daemon socket path must fit sun_path.
    let tmp = tempfile::Builder::new().prefix("atdb").tempdir().unwrap();
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let locator = Locator::resolve(&project, Some(&tmp.path().join("data")), None);
    std::fs::create_dir_all(&locator.paths.config_dir).unwrap();
    match config {
        Some(text) => std::fs::write(Config::path(&locator.paths.config_dir), text).unwrap(),
        None => Config::default().save(&locator.paths.config_dir).unwrap(),
    }
    let device = DeviceId::new();
    Database::create(&locator.db_dir, device).unwrap();
    Sandbox {
        _tmp: tmp,
        locator,
        device,
    }
}

impl Sandbox {
    fn event(&self, n: usize) -> Event {
        let mut ev = Event::new(
            self.device,
            Provider::ClaudeCode,
            "UserPromptSubmit",
            EventKind::PromptSubmitted,
            ProjectRef::derive("/p", None, &self.device),
            format!("s-{n}"),
            CaptureMode::LocalSemantic,
            "test",
        );
        let text = format!(
            "deploy with password={PASSWORD} against postgres://admin:{URL_SECRET}@db.example.com/app ({n})"
        );
        ev.content = Some(EventContent {
            prompt: Some(text.clone()),
            ..Default::default()
        });
        ev.raw = Some(serde_json::json!({ "prompt": text }));
        ev
    }

    fn spool(&self, events: &[Event]) {
        SpoolWriter::new(&self.locator.db_dir)
            .unwrap()
            .append(events)
            .unwrap();
    }

    /// Everything stored, as text a person could grep for.
    fn stored_text(&self) -> String {
        let db = ingest::open_reader(&self.locator).unwrap();
        let events = db.scan(&ScanFilter::default()).unwrap();
        assert!(!events.is_empty(), "nothing was stored");
        events
            .iter()
            .map(|e| {
                format!(
                    "{} {}",
                    e.content
                        .as_ref()
                        .and_then(|c| c.prompt.clone())
                        .unwrap_or_default(),
                    e.raw.as_ref().map(|r| r.to_string()).unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn assert_masked(text: &str) {
    assert!(!text.contains(PASSWORD), "password reached the database");
    assert!(
        !text.contains(URL_SECRET),
        "URL credential reached the database"
    );
    assert!(text.contains("[REDACTED"), "no mask marker in {text:?}");
    assert!(
        text.contains("deploy with"),
        "the rest of the prompt is kept"
    );
}

#[test]
fn the_spool_import_masks_secrets_by_default() {
    let sb = sandbox(None);
    sb.spool(&[sb.event(1), sb.event(2)]);
    let (mut db, gate) = ingest::open_writer_guarded(&sb.locator, false).unwrap();
    ingest::import_spool(&mut db, &gate).unwrap();
    db.flush().unwrap();
    drop(db);
    assert_masked(&sb.stored_text());
}

#[test]
fn redact_secrets_false_keeps_content_exactly_as_captured() {
    let sb = sandbox(Some(r#"{"redact_secrets": false}"#));
    sb.spool(&[sb.event(1)]);
    let (mut db, gate) = ingest::open_writer_guarded(&sb.locator, false).unwrap();
    ingest::import_spool(&mut db, &gate).unwrap();
    db.flush().unwrap();
    drop(db);
    let text = sb.stored_text();
    assert!(
        text.contains(PASSWORD) && text.contains(URL_SECRET),
        "{text}"
    );
}

#[test]
fn an_unusable_config_still_masks() {
    // Fail-closed config: capture_mode metadata_only, redaction on.
    let sb = sandbox(Some(r#"{"capture_mode": "metadata-only",}"#));
    let config = Config::load_or_default(&sb.locator.paths.config_dir);
    assert!(config.load_error.is_some());
    assert!(config.redact_secrets);
}

#[test]
fn write_events_masks_secrets() {
    let sb = sandbox(None);
    ingest::write_events(&sb.locator, vec![sb.event(3)]).unwrap();
    assert_masked(&sb.stored_text());
}

#[test]
fn an_importers_direct_sink_masks_secrets() {
    let sb = sandbox(None);
    let mut target = open_import_target(&sb.locator).unwrap();
    assert!(matches!(target, ImportTarget::Direct { .. }));
    target.write(vec![sb.event(4)]).unwrap();
    target.finish().unwrap();
    drop(target);
    assert_masked(&sb.stored_text());
}

const GITHUB_TOKEN: &str = "ghp_aB3dE5gH7jK9mN1pQ2rS4tU6vW8xY0zA1b2C";
const STRIPE_KEY: &str = "\x73k_live_aB3dE5gH7jK9mN1pQ2rS4tU6";
const AWS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";

impl Sandbox {
    /// An event whose *metadata* (paths, project, branch, model) carries
    /// secrets and whose content is clean.
    fn event_with_secret_metadata(&self, n: usize) -> Event {
        let mut ev = self.event(n);
        ev.content = None;
        ev.raw = None;
        let path = format!("/work/app/{GITHUB_TOKEN}.txt");
        ev.paths = vec![attemptdb_core::PortablePath {
            original: path.clone(),
            logical: path,
            ..Default::default()
        }];
        ev.project.root = format!("/work/{STRIPE_KEY}/app");
        ev.project.branch = Some(format!("feat/{AWS_KEY}-fix"));
        ev.agent.model = Some(format!("claude-{STRIPE_KEY}"));
        ev
    }

    /// Every stored event as JSON text.
    fn stored_events_json(&self) -> String {
        let db = ingest::open_reader(&self.locator).unwrap();
        let events = db.scan(&ScanFilter::default()).unwrap();
        assert!(!events.is_empty(), "nothing was stored");
        events
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn import_through_the_gate(sb: &Sandbox, events: &[Event]) {
    sb.spool(events);
    let (mut db, gate) = ingest::open_writer_guarded(&sb.locator, false).unwrap();
    ingest::import_spool(&mut db, &gate).unwrap();
    db.flush().unwrap();
}

#[test]
fn secrets_in_paths_branches_and_models_are_masked_and_only_their_span() {
    let sb = sandbox(None);
    import_through_the_gate(&sb, &[sb.event_with_secret_metadata(1)]);
    let text = sb.stored_events_json();
    for leaked in [GITHUB_TOKEN, STRIPE_KEY, AWS_KEY] {
        assert!(!text.contains(leaked), "{leaked} reached the database");
    }
    // Only the span went: the path, the root and the branch stay readable.
    for kept in [
        "/work/app/[REDACTED:github_token].txt",
        "/work/[REDACTED:stripe_key]/app",
        "feat/[REDACTED:aws_access_key_id]-fix",
        "claude-[REDACTED:stripe_key]",
    ] {
        assert!(text.contains(kept), "{kept} missing from {text}");
    }
}

#[test]
fn redact_secrets_false_keeps_the_metadata_as_captured_too() {
    let sb = sandbox(Some(r#"{"redact_secrets": false}"#));
    import_through_the_gate(&sb, &[sb.event_with_secret_metadata(1)]);
    let text = sb.stored_events_json();
    for kept in [GITHUB_TOKEN, STRIPE_KEY, AWS_KEY] {
        assert!(text.contains(kept), "{kept} was masked with masking off");
    }
}

#[test]
fn ordinary_metadata_reaches_the_database_untouched() {
    let sb = sandbox(None);
    let mut ev = sb.event(1);
    ev.content = None;
    ev.raw = None;
    ev.paths = vec![attemptdb_core::PortablePath {
        original: "C:\\Users\\dev\\proj\\src\\main.rs".into(),
        logical: "C:/Users/dev/proj/src/main.rs".into(),
        ..Default::default()
    }];
    ev.project.repo_remote = Some("https://github.com/acme/app.git".into());
    ev.project.branch = Some("feature/password-reset".into());
    ev.agent.model = Some("claude-opus-4-1-20250805".into());
    import_through_the_gate(&sb, &[ev]);
    let text = sb.stored_events_json();
    assert!(!text.contains("[REDACTED"), "{text}");
    for kept in [
        "C:\\\\Users\\\\dev\\\\proj\\\\src\\\\main.rs",
        "C:/Users/dev/proj/src/main.rs",
        "https://github.com/acme/app.git",
        "feature/password-reset",
        "claude-opus-4-1-20250805",
    ] {
        assert!(text.contains(kept), "{kept} missing from {text}");
    }
}
