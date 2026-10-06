//! Default telemetry wiring for detected Claude Code and Codex installs.
//! Configuration is user-scoped, locked, backed up and atomically replaced
//! (through a symlinked settings file, see [`install::write_atomically`]),
//! and rewritten in the file's own style: indentation and line endings are
//! detected and kept. An ownership ledger restores only values that still
//! match our writes.
//!
//! A telemetry setting the user owns (their own OTLP exporter, or a value
//! they changed after we wrote it) is kept, not overwritten, and not an
//! installation failure: the hooks are installed either way, so the outcome
//! is a note, not `Failed`.

use crate::{
    agents::AgentKind,
    install::{self, InstallReport, Outcome, Scope},
    locator::Locator,
    otel::ReceiverConfig,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use toml_edit::{DocumentMut, Item};

/// A telemetry setting that belongs to the user and was left alone.
#[derive(Debug)]
struct Kept(String);

impl std::fmt::Display for Kept {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Kept {}

fn kept(message: String) -> anyhow::Error {
    anyhow::Error::new(Kept(message))
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Ledger {
    #[serde(default)]
    pending: bool,
    /// Unix permission bits the settings file had before this installer first
    /// touched it. Writing the OTLP bearer token makes the file private
    /// (`0600`); uninstalling removes the token and puts the bits back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    original_mode: Option<u32>,
    fields: BTreeMap<String, Owned>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Owned {
    previous: Option<Value>,
    installed: Value,
}

fn ledger_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "{}.attemptdb-otel.json",
        path.file_name().unwrap_or_default().to_string_lossy()
    ))
}
fn read_ledger(path: &Path) -> Result<Ledger> {
    let p = ledger_path(path);
    if !p.exists() {
        return Ok(Ledger::default());
    }
    serde_json::from_slice(&std::fs::read(p)?).map_err(|_| {
        anyhow::anyhow!("invalid telemetry ownership ledger; settings were left unchanged")
    })
}

fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    // Set the destination permission before creating any file containing a
    // bearer token. Atomic replacement preserves this permission.
    //
    // A file that is not there yet is not created empty first: a reader (the
    // daemon, a second `setup` started at the same moment) would take the
    // empty file for a broken configuration. The temp file the atomic write
    // makes is private from its first byte (`write_atomically`, mode 0600).
    #[cfg(unix)]
    if path.exists() {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    install::write_atomically(path, bytes)
}

fn receiver(locator: &Locator, dry_run: bool) -> Result<ReceiverConfig> {
    if let Some(c) = ReceiverConfig::load(locator)? {
        return Ok(c);
    }
    // The usual OTLP port can already belong to another collector. Select
    // and persist an available loopback port rather than disrupting it.
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 4318))
        .or_else(|_| std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)))?;
    let c = ReceiverConfig {
        port: listener.local_addr()?.port(),
        token: uuid::Uuid::new_v4().simple().to_string(),
    };
    if !dry_run {
        std::fs::create_dir_all(&locator.paths.config_dir)?;
        let path = ReceiverConfig::path(locator);
        let _lock = install::lock_config(&path)?;
        if let Some(existing) = ReceiverConfig::load(locator)? {
            return Ok(existing);
        }
        private_write(&path, &serde_json::to_vec_pretty(&c)?)?;
    }
    Ok(c)
}

fn claude_values(config: &ReceiverConfig) -> Map<String, Value> {
    let mut values = Map::new();
    for (key, value) in [
        ("CLAUDE_CODE_ENABLE_TELEMETRY", "1"),
        ("CLAUDE_CODE_ENHANCED_TELEMETRY_BETA", "1"),
        ("OTEL_METRICS_EXPORTER", "otlp"),
        ("OTEL_LOGS_EXPORTER", "otlp"),
        ("OTEL_TRACES_EXPORTER", "otlp"),
        // The conversation is exported: the user's prompt on `user_prompt`
        // and the reply on `assistant_response`. Both land in `content`
        // under the local capture mode and leave only under the `messages`
        // (or `full`) sync profile. Commands, tool arguments and tool output
        // stay off: they are captured by the hooks and never exported here.
        ("OTEL_LOG_USER_PROMPTS", "1"),
        ("OTEL_LOG_ASSISTANT_RESPONSES", "1"),
        ("OTEL_LOG_TOOL_DETAILS", "0"),
        ("OTEL_LOG_TOOL_CONTENT", "0"),
        ("OTEL_METRICS_INCLUDE_SESSION_ID", "true"),
    ] {
        values.insert(key.into(), json!(value));
    }
    for signal in ["LOGS", "METRICS", "TRACES"] {
        values.insert(
            format!("OTEL_EXPORTER_OTLP_{signal}_ENDPOINT"),
            json!(config.endpoint("claude_code", &signal.to_lowercase())),
        );
        values.insert(
            format!("OTEL_EXPORTER_OTLP_{signal}_PROTOCOL"),
            json!("http/json"),
        );
        values.insert(
            format!("OTEL_EXPORTER_OTLP_{signal}_COMPRESSION"),
            json!("none"),
        );
        values.insert(
            format!("OTEL_EXPORTER_OTLP_{signal}_HEADERS"),
            json!(format!("Authorization=Bearer {}", config.token)),
        );
    }
    values
}

fn codex_values(config: &ReceiverConfig) -> Map<String, Value> {
    let mut values = Map::new();
    // Codex exports the prompt text only; it has no reply event.
    values.insert("log_user_prompt".into(), json!(true));
    for (key, signal) in [
        ("exporter", "logs"),
        ("metrics_exporter", "metrics"),
        ("trace_exporter", "traces"),
    ] {
        values.insert(key.into(),json!({"otlp-http":{"endpoint":config.endpoint("codex",signal),"protocol":"json","headers":{"Authorization":format!("Bearer {}",config.token)}}}));
    }
    values
}

/// A setting the user wrote to switch something off: prompt or reply logging
/// set to a false value, or an exporter set to `none`. Setup never turns
/// these back on, and says that it kept them.
fn is_opt_out(key: &str, value: &Value) -> bool {
    let text = match value {
        Value::String(s) => s.trim().to_ascii_lowercase(),
        Value::Bool(b) => b.to_string(),
        _ => return false,
    };
    match key {
        "OTEL_LOG_USER_PROMPTS" | "OTEL_LOG_ASSISTANT_RESPONSES" | "log_user_prompt" => {
            matches!(text.as_str(), "0" | "false" | "no" | "off")
        }
        "OTEL_LOGS_EXPORTER"
        | "OTEL_METRICS_EXPORTER"
        | "OTEL_TRACES_EXPORTER"
        | "exporter"
        | "metrics_exporter"
        | "trace_exporter" => text == "none",
        _ => false,
    }
}

/// Claude Code's per-signal OTLP keys that only matter while that signal's
/// exporter is on: `OTEL_METRICS_EXPORTER` -> `OTEL_EXPORTER_OTLP_METRICS_`.
fn signal_prefix(exporter_key: &str) -> Option<String> {
    let signal = exporter_key
        .strip_prefix("OTEL_")?
        .strip_suffix("_EXPORTER")?;
    Some(format!("OTEL_EXPORTER_OTLP_{signal}_"))
}

fn shown(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn merge(
    values: &mut Map<String, Value>,
    mut wanted: Map<String, Value>,
    ledger: &mut Ledger,
    remove: bool,
    notes: &mut Vec<String>,
) -> Result<bool> {
    let before = values.clone();
    if !remove {
        // An earlier install overwrote an explicit opt-out: put it back (the
        // ledger kept what was there before) and stop owning that key.
        let overwritten: Vec<String> = ledger
            .fields
            .iter()
            .filter(|(key, owned)| {
                owned
                    .previous
                    .as_ref()
                    .is_some_and(|previous| is_opt_out(key, previous))
                    && values.get(*key) == Some(&owned.installed)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in overwritten {
            if let Some(owned) = ledger.fields.remove(&key)
                && let Some(previous) = owned.previous
            {
                notes.push(format!(
                    "OTel: restored your {key}={}, which an earlier install had overwritten",
                    shown(&previous)
                ));
                values.insert(key, previous);
            }
        }
        // Keep the user's explicit opt-outs; never turn prompt logging on
        // against a 0 or false.
        let kept: Vec<(String, Value)> = wanted
            .keys()
            .filter_map(|key| {
                let current = values.get(key)?;
                (is_opt_out(key, current) && !ledger.fields.contains_key(key))
                    .then(|| (key.clone(), current.clone()))
            })
            .collect();
        for (key, current) in kept {
            wanted.remove(&key);
            if let Some(prefix) = signal_prefix(&key) {
                wanted.retain(|k, _| !k.starts_with(&prefix));
            }
            notes.push(if is_opt_out_logging(&key) {
                format!(
                    "OTel: kept your {key}={}; AttemptDB did not turn it on (message text then comes from hooks and history only)",
                    shown(&current)
                )
            } else {
                format!(
                    "OTel: kept your {key}={}; that signal is not sent to AttemptDB",
                    shown(&current)
                )
            });
        }
    }
    if remove {
        for (key, owned) in &ledger.fields {
            if values.get(key) == Some(&owned.installed) {
                if let Some(previous) = &owned.previous {
                    values.insert(key.clone(), previous.clone());
                } else {
                    values.remove(key);
                }
            }
        }
        return Ok(*values != before);
    }
    // A different collector is not ours to replace. An explicit reinstall
    // may change owned fields, but never silently steals a foreign exporter.
    for (key, current) in values.iter() {
        let exporter = key == "exporter"
            || key == "metrics_exporter"
            || key == "trace_exporter"
            || matches!(
                key.as_str(),
                "OTEL_LOGS_EXPORTER" | "OTEL_METRICS_EXPORTER" | "OTEL_TRACES_EXPORTER"
            )
            || key.contains("OTLP") && key.ends_with("ENDPOINT");
        if exporter
            && current != &json!("none")
            && current != &json!("statsig")
            && current != &json!("otlp")
            && !current.is_null()
            && !ledger
                .fields
                .get(key)
                .is_some_and(|o| &o.installed == current)
            && wanted.get(key) != Some(current)
        {
            return Err(kept(format!(
                "existing external OTel exporter preserved ({key}); configure collector forwarding to AttemptDB before replacing it"
            )));
        }
    }
    for (key, value) in wanted {
        if let Some(owned) = ledger.fields.get(&key)
            && values.get(&key) != Some(&owned.installed)
            && !(ledger.pending && values.get(&key) == owned.previous.as_ref())
            && values.get(&key) != Some(&value)
        {
            return Err(kept(format!(
                "telemetry setting {key} changed outside AttemptDB; preserved for review"
            )));
        }
        ledger
            .fields
            .entry(key.clone())
            .and_modify(|o| o.installed = value.clone())
            .or_insert_with(|| Owned {
                previous: values.get(&key).cloned(),
                installed: value.clone(),
            });
        values.insert(key, value);
    }
    Ok(*values != before)
}

fn is_opt_out_logging(key: &str) -> bool {
    matches!(
        key,
        "OTEL_LOG_USER_PROMPTS" | "OTEL_LOG_ASSISTANT_RESPONSES" | "log_user_prompt"
    )
}

fn toml_value(v: &Value) -> Result<toml_edit::Value> {
    Ok(match v {
        Value::Bool(b) => toml_edit::Value::from(*b),
        Value::String(s) => toml_edit::Value::from(s.as_str()),
        Value::Object(values) => {
            let mut table = toml_edit::InlineTable::new();
            for (k, v) in values {
                table.insert(k, toml_value(v)?);
            }
            toml_edit::Value::InlineTable(table)
        }
        _ => bail!("unsupported telemetry setting type"),
    })
}
fn item_json(item: &Item) -> Result<Value> {
    if let Some(s) = item.as_str() {
        return Ok(json!(s));
    }
    if let Some(b) = item.as_bool() {
        return Ok(json!(b));
    }
    if let Some(t) = item.as_table_like() {
        let mut m = Map::new();
        for (k, v) in t.iter() {
            m.insert(k.into(), item_json(v)?);
        }
        return Ok(Value::Object(m));
    }
    bail!("unsupported existing telemetry setting type")
}

pub fn configure(
    kind: AgentKind,
    path: &Path,
    config: &ReceiverConfig,
    remove: bool,
    dry_run: bool,
) -> Result<bool> {
    configure_with_notes(kind, path, config, remove, dry_run).map(|(changed, _)| changed)
}

/// [`configure`], plus the notes worth showing: settings of the user's that
/// were kept (an explicit `OTEL_LOG_USER_PROMPTS=0`, an exporter set to
/// `none`).
pub fn configure_with_notes(
    kind: AgentKind,
    path: &Path,
    config: &ReceiverConfig,
    remove: bool,
    dry_run: bool,
) -> Result<(bool, Vec<String>)> {
    let mut notes = Vec::new();
    if remove && !ledger_path(path).exists() {
        return Ok((false, notes));
    }
    let parent = path.parent().context("agent settings have no parent")?;
    if !dry_run {
        std::fs::create_dir_all(parent)?;
    }
    let _lock = if !dry_run {
        Some(install::lock_config(path)?)
    } else {
        None
    };
    let source = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };
    let mut ledger = read_ledger(path)?;
    let style = install::Style::detect(&source);
    // A byte order mark is not part of the document; `style` writes it back.
    let source = source
        .strip_prefix(install::UTF8_BOM)
        .unwrap_or(&source)
        .to_string();
    let (changed, bytes) = match kind {
        AgentKind::ClaudeCode => {
            let mut doc: Value = if source.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&source).map_err(|_| {
                    anyhow::anyhow!("invalid Claude JSON settings; file was left unchanged")
                })?
            };
            let root = doc
                .as_object_mut()
                .context("Claude settings must be an object")?;
            let env = root
                .entry("env")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .context("Claude env must be an object")?;
            let changed = merge(env, claude_values(config), &mut ledger, remove, &mut notes)?;
            if env.is_empty() {
                root.remove("env");
            }
            (changed, install::render_json(&doc, style)?)
        }
        AgentKind::Codex => {
            let mut doc = source.parse::<DocumentMut>().map_err(|_| {
                anyhow::anyhow!("invalid Codex TOML settings; file was left unchanged")
            })?;
            let mut values = Map::new();
            if !doc.contains_key("otel") {
                doc["otel"] = Item::Table(toml_edit::Table::new());
            }
            let table = doc["otel"]
                .as_table_like_mut()
                .context("Codex otel must be a table")?;
            for key in [
                "exporter",
                "trace_exporter",
                "metrics_exporter",
                "log_user_prompt",
            ] {
                if let Some(v) = table.get(key) {
                    values.insert(key.into(), item_json(v)?);
                }
            }
            let changed = merge(
                &mut values,
                codex_values(config),
                &mut ledger,
                remove,
                &mut notes,
            )?;
            for key in [
                "exporter",
                "trace_exporter",
                "metrics_exporter",
                "log_user_prompt",
            ] {
                if let Some(v) = values.get(key) {
                    table.insert(key, Item::Value(toml_value(v)?));
                } else {
                    table.remove(key);
                }
            }
            if table.is_empty() {
                doc.remove("otel");
            }
            let mut text = doc.to_string();
            if style.crlf {
                // New keys come out with `\n`; make the whole file agree.
                text = text.replace("\r\n", "\n").replace('\n', "\r\n");
            }
            (changed, text.into_bytes())
        }
        _ => return Ok((false, notes)),
    };
    if dry_run {
        return Ok((changed, notes));
    }
    if changed {
        if path.exists() {
            install::backup_config(path)?;
            // Writing the token makes the file private; remember what it was
            // so that uninstalling can put that back.
            if !remove && ledger.original_mode.is_none() {
                ledger.original_mode = file_mode(path);
            }
        }
        // Persist ownership before the edit, making interrupted installs
        // retryable and ensuring uninstall can recover the previous values.
        if !remove {
            ledger.pending = true;
            private_write(&ledger_path(path), &serde_json::to_vec_pretty(&ledger)?)?;
        }
        private_write(path, &bytes)?;
        if !remove {
            ledger.pending = false;
            private_write(&ledger_path(path), &serde_json::to_vec_pretty(&ledger)?)?;
        }
    }
    if remove {
        // The token is gone: the file is as visible as it was before. One
        // backup is enough to undo an uninstall; the older ones go.
        if changed {
            restore_file_mode(path, ledger.original_mode);
            install::prune_backups_keeping(path, 1);
        }
        if ledger_path(path).exists() {
            std::fs::remove_file(ledger_path(path))?;
        }
    }
    Ok((changed, notes))
}

/// Unix permission bits of `path` (always `None` elsewhere).
fn file_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .ok()
            .map(|m| m.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Put the permission bits recorded by [`configure_with_notes`] back, through
/// a symlink (the file that was written is the link's target).
fn restore_file_mode(path: &Path, mode: Option<u32>) {
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        // `set_permissions` follows symlinks.
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
}

pub fn apply(
    locator: &Locator,
    scope: &Scope,
    report: &mut InstallReport,
    remove: bool,
    dry_run: bool,
) -> Result<()> {
    // Project hooks share user-level telemetry settings. Removing one
    // project's hooks must not disable telemetry in every other project.
    if remove && !matches!(scope, Scope::User) {
        return Ok(());
    }
    let eligible = report.actions.iter().any(|a| {
        matches!(a.agent, AgentKind::ClaudeCode | AgentKind::Codex)
            && !matches!(a.outcome, Outcome::Failed(_) | Outcome::Skipped(_))
    });
    if !eligible {
        return Ok(());
    }
    let config = if remove {
        ReceiverConfig::load(locator)?.unwrap_or(ReceiverConfig {
            port: 4318,
            token: "0".repeat(32),
        })
    } else {
        receiver(locator, dry_run)?
    };
    for action in &mut report.actions {
        if !matches!(action.agent, AgentKind::ClaudeCode | AgentKind::Codex)
            || matches!(action.outcome, Outcome::Failed(_) | Outcome::Skipped(_))
        {
            continue;
        }
        let base = if matches!(scope, Scope::User) {
            action.config_path.clone()
        } else {
            action
                .agent
                .user_config_path()
                .context("cannot locate user-scoped telemetry settings")?
        };
        let path = if action.agent == AgentKind::Codex {
            base.with_file_name("config.toml")
        } else {
            base
        };
        match configure_with_notes(action.agent, &path, &config, remove, dry_run) {
            Ok((changed, notes)) => {
                action.notes.extend(notes);
                if changed && action.outcome == Outcome::AlreadyCurrent {
                    action.outcome = if remove {
                        Outcome::Removed
                    } else {
                        Outcome::Updated
                    };
                }
                action.notes.push(if remove {"Owned OTel settings restored; externally changed settings preserved.".into()}else{format!("OTel logs, metrics and traces configured locally (port {}); restart this agent to apply. Run attempt doctor to check receipts. Existing sessions keep their previous exporter settings.",config.port)});
            }
            Err(e) if e.downcast_ref::<Kept>().is_some() => {
                // The hooks are in place whatever happens here, so this is
                // not a failure of the installation: the user's own exporter
                // stays, and the note says what that means.
                action.notes.push(format!(
                    "OTel: kept your exporter configuration ({e}). Hooks capture normally; to also receive this agent's OpenTelemetry, forward your collector to the receiver shown by `attempt doctor`."
                ));
            }
            Err(e) => {
                action.outcome = Outcome::Failed(format!("OTel configuration: {e:#}"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> ReceiverConfig {
        ReceiverConfig {
            port: 54318,
            token: "1".repeat(32),
        }
    }

    #[test]
    fn claude_default_exports_the_conversation_but_no_tool_content_idempotent_and_reversible() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        let original = json!({"env":{"OTHER_SETTING":"kept","OTEL_LOG_USER_PROMPTS":"0"},"hooks":{"Stop":[{"hooks":[{"type":"command","command":"other-hook"}]}]}});
        std::fs::write(&path, original.to_string()).unwrap();
        assert!(configure(AgentKind::ClaudeCode, &path, &config(), false, false).unwrap());
        let first = std::fs::read(&path).unwrap();
        let installed: Value = serde_json::from_slice(&first).unwrap();
        assert_eq!(installed["hooks"], original["hooks"]);
        // The user's explicit opt-out stays; the reply export we turn on.
        assert_eq!(installed["env"]["OTEL_LOG_USER_PROMPTS"], "0");
        assert_eq!(installed["env"]["OTEL_LOG_ASSISTANT_RESPONSES"], "1");
        assert_eq!(installed["env"]["OTEL_LOG_TOOL_DETAILS"], "0");
        assert_eq!(installed["env"]["OTEL_LOG_TOOL_CONTENT"], "0");
        assert_eq!(
            installed["env"]["OTEL_EXPORTER_OTLP_LOGS_PROTOCOL"],
            "http/json"
        );
        assert_eq!(
            installed["env"]["OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"],
            config().endpoint("claude_code", "traces")
        );
        assert!(!configure(AgentKind::ClaudeCode, &path, &config(), false, false).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), first);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(ledger_path(&path))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(configure(AgentKind::ClaudeCode, &path, &config(), true, false).unwrap());
        assert_eq!(
            serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).unwrap(),
            original
        );
        assert!(!ledger_path(&path).exists());
    }

    #[test]
    fn codex_keeps_trust_comments_and_other_otel_options() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let original = "# owner comment\nmodel = \"fixture-model\"\n[hooks.state]\nexample = \"trusted\"\n[otel]\nenvironment = \"development\"\nmetrics_exporter = \"statsig\"\n";
        std::fs::write(&path, original).unwrap();
        assert!(configure(AgentKind::Codex, &path, &config(), false, false).unwrap());
        let installed = std::fs::read_to_string(&path).unwrap();
        assert!(installed.contains("# owner comment"));
        let doc = installed.parse::<DocumentMut>().unwrap();
        assert_eq!(doc["hooks"]["state"]["example"].as_str(), Some("trusted"));
        assert_eq!(doc["otel"]["environment"].as_str(), Some("development"));
        assert_eq!(doc["otel"]["log_user_prompt"].as_bool(), Some(true));
        assert_eq!(
            doc["otel"]["exporter"]["otlp-http"]["protocol"].as_str(),
            Some("json")
        );
        assert!(!configure(AgentKind::Codex, &path, &config(), false, false).unwrap());
        assert!(configure(AgentKind::Codex, &path, &config(), true, false).unwrap());
        let restored = std::fs::read_to_string(&path)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            restored["otel"]["metrics_exporter"].as_str(),
            Some("statsig")
        );
        assert!(
            !restored["otel"]
                .as_table()
                .unwrap()
                .contains_key("exporter")
        );
        assert_eq!(
            restored["hooks"]["state"]["example"].as_str(),
            Some("trusted")
        );
    }

    #[test]
    fn foreign_collectors_are_not_overwritten_and_dry_run_writes_nothing() {
        for (kind, source, filename) in [
            (
                AgentKind::Codex,
                "[otel]\nexporter = { otlp-http = { endpoint = \"https://example.com/v1/logs\", protocol = \"json\" } }",
                "config.toml",
            ),
            (
                AgentKind::ClaudeCode,
                "{\"env\":{\"OTEL_EXPORTER_OTLP_ENDPOINT\":\"https://example.com\"}}",
                "settings.json",
            ),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join(filename);
            std::fs::write(&path, source).unwrap();
            assert!(configure(kind, &path, &config(), false, false).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
            assert!(!ledger_path(&path).exists());
        }
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("missing/settings.json");
        assert!(configure(AgentKind::ClaudeCode, &path, &config(), false, true).unwrap());
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn interrupted_install_can_finish_but_completed_ownership_cannot_erase_user_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        std::fs::write(&path, "{}").unwrap();
        let mut ledger = Ledger {
            pending: true,
            ..Default::default()
        };
        let mut values = Map::new();
        merge(
            &mut values,
            claude_values(&config()),
            &mut ledger,
            false,
            &mut Vec::new(),
        )
        .unwrap();
        std::fs::write(ledger_path(&path), serde_json::to_vec(&ledger).unwrap()).unwrap();
        assert!(configure(AgentKind::ClaudeCode, &path, &config(), false, false).unwrap());
        assert!(!read_ledger(&path).unwrap().pending);
        let mut doc: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        // The user turns tool-argument logging on by hand: an owned key whose
        // value no longer matches what the installer wrote.
        doc["env"]["OTEL_LOG_TOOL_DETAILS"] = json!("1");
        std::fs::write(&path, doc.to_string()).unwrap();
        assert!(configure(AgentKind::ClaudeCode, &path, &config(), false, false).is_err());
        assert!(configure(AgentKind::ClaudeCode, &path, &config(), true, false).unwrap());
        let restored: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(restored["env"]["OTEL_LOG_TOOL_DETAILS"], "1");
    }

    #[test]
    fn crlf_and_tab_indented_settings_keep_their_style_through_the_otel_edit() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        let original = "{\r\n\t\"model\": \"opus\",\r\n\t\"env\": {\r\n\t\t\"OTHER\": \"kept\"\r\n\t}\r\n}\r\n";
        std::fs::write(&path, original).unwrap();
        assert!(configure(AgentKind::ClaudeCode, &path, &config(), false, false).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("{\r\n\t\"model\""), "{text:?}");
        assert!(
            text.contains("\r\n\t\t\"CLAUDE_CODE_ENABLE_TELEMETRY\""),
            "{text:?}"
        );
        assert!(
            !text.replace("\r\n", "").contains('\n'),
            "no bare line feed: {text:?}"
        );
        assert!(!text.contains("  \""), "no space indentation: {text:?}");
        assert!(configure(AgentKind::ClaudeCode, &path, &config(), true, false).unwrap());
        assert_eq!(
            serde_json::from_str::<Value>(&std::fs::read_to_string(&path).unwrap()).unwrap(),
            serde_json::from_str::<Value>(original).unwrap()
        );

        // Four spaces stay four spaces; a Codex TOML with CRLF stays CRLF.
        std::fs::write(&path, "{\n    \"model\": \"opus\"\n}\n").unwrap();
        assert!(configure(AgentKind::ClaudeCode, &path, &config(), false, false).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("{\n    \"model\""), "{text:?}");
        assert!(
            text.contains("\n        \"CLAUDE_CODE_ENABLE_TELEMETRY\""),
            "{text:?}"
        );
        let toml = tmp.path().join("config.toml");
        std::fs::write(
            &toml,
            "model = \"x\"\r\n[otel]\r\nenvironment = \"dev\"\r\n",
        )
        .unwrap();
        assert!(configure(AgentKind::Codex, &toml, &config(), false, false).unwrap());
        let text = std::fs::read_to_string(&toml).unwrap();
        assert!(
            !text.replace("\r\n", "").contains('\n'),
            "no bare line feed: {text:?}"
        );
        assert!(text.contains("log_user_prompt"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_settings_file_stays_a_link_through_the_otel_edit() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let tmp = tempfile::tempdir().unwrap();
        let dotfiles = tmp.path().join("dotfiles");
        let agent = tmp.path().join(".claude");
        std::fs::create_dir_all(&dotfiles).unwrap();
        std::fs::create_dir_all(&agent).unwrap();
        let real = dotfiles.join("settings.json");
        std::fs::write(&real, "{\"model\":\"opus\"}").unwrap();
        let link = agent.join("settings.json");
        symlink(&real, &link).unwrap();
        assert!(configure(AgentKind::ClaudeCode, &link, &config(), false, false).unwrap());
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let v: Value = serde_json::from_slice(&std::fs::read(&real).unwrap()).unwrap();
        assert_eq!(v["env"]["CLAUDE_CODE_ENABLE_TELEMETRY"], "1");
        assert_eq!(v["model"], "opus");
        // The token lives in this file, so it is private whatever it was.
        assert_eq!(
            std::fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(configure(AgentKind::ClaudeCode, &link, &config(), true, false).unwrap());
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&std::fs::read(&real).unwrap()).unwrap(),
            json!({"model": "opus"})
        );
    }

    #[test]
    fn a_users_own_otlp_endpoint_is_kept_and_does_not_fail_the_hook_install() {
        let tmp = tempfile::tempdir().unwrap();
        let loc = Locator::resolve(tmp.path(), Some(&tmp.path().join("data")), None);
        let settings = tmp.path().join("claude").join("settings.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        let source =
            "{\"env\":{\"OTEL_EXPORTER_OTLP_ENDPOINT\":\"https://collector.example.com\"}}";
        std::fs::write(&settings, source).unwrap();
        let hooks = install::install_to(
            AgentKind::ClaudeCode,
            &settings,
            "'/opt/attemptdb/attempt' hook claude-code",
            false,
        )
        .unwrap();
        assert_eq!(hooks.outcome, Outcome::Installed);
        let mut report = InstallReport {
            actions: vec![hooks],
        };
        apply(&loc, &Scope::User, &mut report, false, false).unwrap();
        let action = &report.actions[0];
        assert_eq!(
            action.outcome,
            Outcome::Installed,
            "the hooks are in place: {action:?}"
        );
        assert!(!report.has_failures(), "{report:?}");
        assert!(
            action
                .notes
                .iter()
                .any(|n| n.contains("kept your exporter")),
            "{:?}",
            action.notes
        );
        let after: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(
            after["env"]["OTEL_EXPORTER_OTLP_ENDPOINT"],
            "https://collector.example.com"
        );
        assert!(
            after["env"].get("OTEL_LOGS_EXPORTER").is_none(),
            "nothing of ours"
        );
        assert!(after["hooks"]["Stop"].is_array());
        assert!(!ledger_path(&settings).exists());

        // A real failure is still a failure: settings that are not JSON.
        let broken = tmp.path().join("claude2").join("settings.json");
        std::fs::create_dir_all(broken.parent().unwrap()).unwrap();
        std::fs::write(&broken, "{ not json").unwrap();
        let mut report = InstallReport {
            actions: vec![install::InstallAction {
                agent: AgentKind::ClaudeCode,
                config_path: broken.clone(),
                outcome: Outcome::Installed,
                backup_path: None,
                entries_added: 0,
                entries_removed: 0,
                legacy_removed: 0,
                notes: Vec::new(),
            }],
        };
        apply(&loc, &Scope::User, &mut report, false, false).unwrap();
        assert!(
            matches!(&report.actions[0].outcome, Outcome::Failed(_)),
            "{report:?}"
        );
    }

    fn notes_of(kind: AgentKind, path: &Path) -> Vec<String> {
        configure_with_notes(kind, path, &config(), false, false)
            .unwrap()
            .1
    }

    #[test]
    fn prompt_logging_is_turned_on_only_when_the_user_did_not_say_otherwise() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        std::fs::write(&path, "{}").unwrap();
        assert!(notes_of(AgentKind::ClaudeCode, &path).is_empty());
        let v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["env"]["OTEL_LOG_USER_PROMPTS"], "1");
        assert_eq!(v["env"]["OTEL_METRICS_EXPORTER"], "otlp");
    }

    #[test]
    fn explicit_claude_opt_outs_are_kept_said_aloud_and_survive_every_rerun_and_uninstall() {
        for off in [json!("0"), json!("false"), json!("FALSE"), json!(false)] {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("settings.json");
            let original = json!({"env": {
                "OTEL_LOG_USER_PROMPTS": off,
                "OTEL_METRICS_EXPORTER": "none",
                "KEEP": "me",
            }});
            std::fs::write(&path, original.to_string()).unwrap();
            let notes = notes_of(AgentKind::ClaudeCode, &path);
            let v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(
                v["env"]["OTEL_LOG_USER_PROMPTS"],
                original["env"]["OTEL_LOG_USER_PROMPTS"]
            );
            assert_eq!(v["env"]["OTEL_METRICS_EXPORTER"], "none");
            // Nothing of ours is left for the signal that was switched off ...
            assert!(
                v["env"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .all(|k| !k.starts_with("OTEL_EXPORTER_OTLP_METRICS_")),
                "{v}"
            );
            // ... while the other signals are wired as usual.
            assert_eq!(v["env"]["OTEL_LOGS_EXPORTER"], "otlp");
            assert_eq!(v["env"]["CLAUDE_CODE_ENABLE_TELEMETRY"], "1");
            let said = notes.join("\n");
            assert!(
                said.contains("OTEL_LOG_USER_PROMPTS") && said.contains("kept your"),
                "{said}"
            );
            assert!(said.contains("OTEL_METRICS_EXPORTER=none"), "{said}");
            // Repeating changes nothing and says the same; uninstalling gives
            // back exactly what was there.
            let first = std::fs::read(&path).unwrap();
            assert!(!configure(AgentKind::ClaudeCode, &path, &config(), false, false).unwrap());
            assert_eq!(std::fs::read(&path).unwrap(), first);
            assert!(configure(AgentKind::ClaudeCode, &path, &config(), true, false).unwrap());
            assert_eq!(
                serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).unwrap(),
                original
            );
        }
    }

    #[test]
    fn codex_log_user_prompt_false_and_exporter_none_are_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let original = "[otel]\nlog_user_prompt = false\nmetrics_exporter = \"none\"\n";
        std::fs::write(&path, original).unwrap();
        let notes = notes_of(AgentKind::Codex, &path);
        let doc = std::fs::read_to_string(&path)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(doc["otel"]["log_user_prompt"].as_bool(), Some(false));
        assert_eq!(doc["otel"]["metrics_exporter"].as_str(), Some("none"));
        assert!(doc["otel"]["exporter"]["otlp-http"].is_table_like());
        let said = notes.join("\n");
        assert!(said.contains("log_user_prompt=false"), "{said}");
        assert!(said.contains("metrics_exporter=none"), "{said}");
        assert!(configure(AgentKind::Codex, &path, &config(), true, false).unwrap());
        let back = std::fs::read_to_string(&path).unwrap();
        assert!(back.contains("log_user_prompt = false"), "{back}");
        assert!(back.contains("metrics_exporter = \"none\""), "{back}");
        assert!(!back.contains("otlp-http"), "{back}");
    }

    #[test]
    fn an_opt_out_an_older_install_overwrote_is_given_back() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        // What the released 0.2.13 left behind: our "1" over the user's "0",
        // and a ledger that remembers the "0".
        std::fs::write(&path, "{}").unwrap();
        let mut values = Map::new();
        values.insert("OTEL_LOG_USER_PROMPTS".into(), json!("0"));
        let mut ledger = Ledger::default();
        merge(
            &mut values,
            claude_values(&config()),
            &mut ledger,
            false,
            &mut Vec::new(),
        )
        .unwrap();
        // The pre-fix installer overwrote the opt-out, which the ledger records.
        values.insert("OTEL_LOG_USER_PROMPTS".into(), json!("1"));
        ledger.fields.insert(
            "OTEL_LOG_USER_PROMPTS".into(),
            Owned {
                previous: Some(json!("0")),
                installed: json!("1"),
            },
        );
        std::fs::write(&path, json!({ "env": values }).to_string()).unwrap();
        std::fs::write(ledger_path(&path), serde_json::to_vec(&ledger).unwrap()).unwrap();
        let notes = notes_of(AgentKind::ClaudeCode, &path);
        let v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["env"]["OTEL_LOG_USER_PROMPTS"], "0");
        assert!(
            notes
                .join("\n")
                .contains("restored your OTEL_LOG_USER_PROMPTS=0"),
            "{notes:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn uninstall_gives_the_settings_file_its_permissions_back_and_keeps_one_backup() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        std::fs::write(&path, "{\"model\":\"opus\"}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        assert!(configure(AgentKind::ClaudeCode, &path, &config(), false, false).unwrap());
        assert_eq!(mode(&path), 0o600, "the token makes it private");
        // A second change and a third: backups pile up while installed.
        let mut doc: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        doc["theme"] = json!("dark");
        std::fs::write(&path, doc.to_string()).unwrap();
        install::backup_config(&path).unwrap();
        install::backup_config(&path).unwrap();
        assert!(configure(AgentKind::ClaudeCode, &path, &config(), true, false).unwrap());
        assert_eq!(mode(&path), 0o644, "the original permissions are back");
        let backups: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".attemptdb.bak-"))
            .collect();
        assert_eq!(backups.len(), 1, "{backups:?}");
        assert!(!ledger_path(&path).exists());
    }

    #[test]
    fn receiver_configuration_survives_reinstall_and_uses_an_available_port() {
        let tmp = tempfile::tempdir().unwrap();
        let loc = Locator::resolve(tmp.path(), Some(&tmp.path().join("data")), None);
        let c = receiver(&loc, false).unwrap();
        assert_eq!(receiver(&loc, false).unwrap(), c);
        assert!(c.port > 0 && c.token.len() == 32);
        let other = Locator::resolve(tmp.path(), Some(&tmp.path().join("other")), None);
        let _ = receiver(&other, true).unwrap();
        assert!(!ReceiverConfig::path(&other).exists());
    }
}
