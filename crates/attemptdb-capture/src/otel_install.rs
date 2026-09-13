//! Default telemetry wiring for detected Claude Code and Codex installs.
//! Configuration is user-scoped, locked, backed up and atomically replaced.
//! An ownership ledger restores only values that still match our writes.

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

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Ledger {
    #[serde(default)]
    pending: bool,
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
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

fn merge(
    values: &mut Map<String, Value>,
    wanted: Map<String, Value>,
    ledger: &mut Ledger,
    remove: bool,
) -> Result<bool> {
    let before = values.clone();
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
            bail!(
                "existing external OTel exporter preserved ({key}); configure collector forwarding to AttemptDB before replacing it"
            );
        }
    }
    for (key, value) in wanted {
        if let Some(owned) = ledger.fields.get(&key)
            && values.get(&key) != Some(&owned.installed)
            && !(ledger.pending && values.get(&key) == owned.previous.as_ref())
            && values.get(&key) != Some(&value)
        {
            bail!("telemetry setting {key} changed outside AttemptDB; preserved for review");
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
    if remove && !ledger_path(path).exists() {
        return Ok(false);
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
            let changed = merge(env, claude_values(config), &mut ledger, remove)?;
            if env.is_empty() {
                root.remove("env");
            }
            (changed, serde_json::to_vec_pretty(&doc)?)
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
            let changed = merge(&mut values, codex_values(config), &mut ledger, remove)?;
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
            (changed, doc.to_string().into_bytes())
        }
        _ => return Ok(false),
    };
    if dry_run {
        return Ok(changed);
    }
    if changed {
        if path.exists() {
            install::backup_config(path)?;
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
    if remove && ledger_path(path).exists() {
        std::fs::remove_file(ledger_path(path))?;
    }
    Ok(changed)
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
        match configure(action.agent, &path, &config, remove, dry_run) {
            Ok(changed) => {
                if changed && action.outcome == Outcome::AlreadyCurrent {
                    action.outcome = if remove {
                        Outcome::Removed
                    } else {
                        Outcome::Updated
                    };
                }
                action.notes.push(if remove {"Owned OTel settings restored; externally changed settings preserved.".into()}else{format!("OTel logs, metrics and traces configured locally (port {}); restart this agent to apply. Run attempt doctor to check receipts. Existing sessions keep their previous exporter settings.",config.port)});
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
        assert_eq!(installed["env"]["OTEL_LOG_USER_PROMPTS"], "1");
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
        merge(&mut values, claude_values(&config()), &mut ledger, false).unwrap();
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
