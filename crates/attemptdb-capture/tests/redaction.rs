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
