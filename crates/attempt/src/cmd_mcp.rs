//! `attempt mcp`: serve AttemptDB over the Model Context Protocol (stdio),
//! print the registration snippets for coding agents, or install them.
//!
//! Wiring (in `cli.rs` / `main.rs`):
//!
//! ```text
//! Mcp(crate::cmd_mcp::McpArgs)                       // cli.rs, replaces the unit variant
//! Command::Mcp(args) => cmd_mcp::run(&cli, args),    // main.rs
//! ```

use crate::cli::Cli;
use crate::ctx::Ctx;
use crate::render::print_json;
use anyhow::{Context, Result, anyhow, bail};
use attemptdb_capture::agents::AgentKind;
use attemptdb_capture::install::{Style, render_json};
use attemptdb_capture::platform::{current_exe_path, quote_for_shell};
use attemptdb_mcp::{DEFAULT_MAX_ROWS, ServerConfig, serve_stdio};
use attemptdb_storage::Database;
use clap::Args;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Key under which the server is registered in every agent's config.
pub const SERVER_KEY: &str = "attemptdb";
const CURSOR_FILE: &str = "mcp.json";
const CODEX_FILE: &str = "config.toml";

#[derive(Args, Debug)]
pub struct McpArgs {
    /// Maximum rows/lines per tool result (default 200).
    #[arg(long, value_name = "N")]
    pub max_rows: Option<usize>,
    /// Print the snippets that register this server in Claude Code, Codex and Cursor, then exit.
    #[arg(long)]
    pub print_config: bool,
    /// Write the server entry into Cursor (~/.cursor/mcp.json) and Codex (~/.codex/config.toml); print the Claude Code command.
    #[arg(long)]
    pub install: bool,
}

pub fn run(cli: &Cli, args: &McpArgs) -> Result<ExitCode> {
    if args.print_config {
        return print_config(cli, args);
    }
    if args.install {
        return install(cli, args);
    }
    let ctx = Ctx::new(cli)?;
    if cli.snapshot.is_none() && !Database::exists(&ctx.locator.db_dir) {
        eprintln!(
            "attemptdb mcp: no database at {} yet; tools will say so until `attempt init` has run",
            ctx.locator.db_dir.display()
        );
    }
    let config = ServerConfig {
        db_dir: ctx.locator.db_dir.clone(),
        data_dir: cli.data_dir.clone(),
        snapshot: cli.snapshot.clone(),
        project_root: Some(ctx.cwd.clone()),
        max_rows: args.max_rows.unwrap_or(DEFAULT_MAX_ROWS),
    };
    serve_stdio(config)?;
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------
// Registration snippets
// ---------------------------------------------------------------------------

/// How an agent should launch the server: the absolute binary plus the
/// flags that pin the same database this CLI invocation used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registration {
    pub command: String,
    pub args: Vec<String>,
}

impl Registration {
    fn from_cli(cli: &Cli, args: &McpArgs) -> Self {
        let mut a = vec!["mcp".to_string()];
        if let Some(db) = &cli.db {
            a.push("--db".into());
            a.push(db.display().to_string());
        }
        if let Some(dir) = &cli.data_dir {
            a.push("--data-dir".into());
            a.push(dir.display().to_string());
        }
        if let Some(n) = args.max_rows {
            a.push("--max-rows".into());
            a.push(n.to_string());
        }
        Self {
            command: current_exe_path().to_string_lossy().into_owned(),
            args: a,
        }
    }

    fn entry(&self) -> Value {
        json!({ "command": self.command, "args": self.args })
    }

    /// `{"mcpServers": {"attemptdb": {...}}}` — the shape Cursor and Claude
    /// Code's `.mcp.json` share.
    pub fn mcp_json(&self) -> Value {
        json!({ "mcpServers": { SERVER_KEY: self.entry() } })
    }

    /// The `[mcp_servers.attemptdb]` table for Codex's `config.toml`.
    pub fn codex_toml(&self) -> String {
        let mut doc = toml_edit::DocumentMut::new();
        insert_codex_entry(&mut doc, self);
        doc.to_string()
    }

    /// The one-shot Claude Code registration command.
    pub fn claude_command(&self) -> String {
        let mut parts = vec![
            format!("claude mcp add {SERVER_KEY} --"),
            quote_for_shell(Path::new(&self.command)),
        ];
        parts.extend(self.args.iter().map(|a| quote_for_shell(Path::new(a))));
        parts.join(" ")
    }
}

fn cursor_path() -> Option<PathBuf> {
    AgentKind::Cursor.agent_dir().map(|d| d.join(CURSOR_FILE))
}

fn codex_path() -> Option<PathBuf> {
    AgentKind::Codex.agent_dir().map(|d| d.join(CODEX_FILE))
}

fn display(p: &Option<PathBuf>, fallback: &str) -> String {
    p.as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| fallback.to_string())
}

fn print_config(cli: &Cli, args: &McpArgs) -> Result<ExitCode> {
    let reg = Registration::from_cli(cli, args);
    let cursor = cursor_path();
    let codex = codex_path();
    if cli.json {
        print_json(&json!({
            "binary": reg.command,
            "args": reg.args,
            "claude_code": { "command": reg.claude_command(), "mcp_json": reg.mcp_json() },
            "codex": { "path": codex, "toml": reg.codex_toml() },
            "cursor": { "path": cursor, "mcp_json": reg.mcp_json() },
        }));
        return Ok(ExitCode::SUCCESS);
    }
    let pretty = serde_json::to_string_pretty(&reg.mcp_json())?;
    let indent = |s: &str| {
        s.lines()
            .map(|l| format!("  {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    println!("AttemptDB MCP server — registration snippets");
    println!("binary   {}", reg.command);
    println!("launch   {} {}", reg.command, reg.args.join(" "));
    println!();
    println!(
        "Claude Code — run once in the project (add --scope user to share it across projects):"
    );
    println!("  {}", reg.claude_command());
    println!("  or commit this as .mcp.json in the project root:");
    println!("{}", indent(&pretty));
    println!();
    println!(
        "Codex — add to {}:",
        display(&codex, "~/.codex/config.toml")
    );
    println!("{}", indent(reg.codex_toml().trim_end()));
    println!();
    println!(
        "Cursor — add to {} (or <project>/.cursor/mcp.json):",
        display(&cursor, "~/.cursor/mcp.json")
    );
    println!("{}", indent(&pretty));
    println!();
    println!(
        "`attempt mcp --install` writes the Cursor and Codex entries (backups kept) and prints the Claude Code command."
    );
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------
// --install
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Installed {
    Added { backup: Option<PathBuf> },
    Updated { backup: Option<PathBuf> },
    AlreadyCurrent,
}

fn install(cli: &Cli, args: &McpArgs) -> Result<ExitCode> {
    let reg = Registration::from_cli(cli, args);
    let mut failed = false;
    let mut report = |agent: &str, path: &Path, result: Result<Installed>| {
        let label = match &result {
            Ok(Installed::Added { .. }) => "installed".to_string(),
            Ok(Installed::Updated { .. }) => "updated".to_string(),
            Ok(Installed::AlreadyCurrent) => "already current".to_string(),
            Err(e) => {
                failed = true;
                format!("FAILED: {e:#}")
            }
        };
        println!("{agent:<12} {label:<16} {}", path.display());
        if let Ok(Installed::Added { backup: Some(b) } | Installed::Updated { backup: Some(b) }) =
            &result
        {
            println!("{:<12} backup: {}", "", b.display());
        }
    };
    match cursor_path() {
        Some(path) if path.parent().is_some_and(Path::is_dir) => {
            let r = install_cursor(&path, &reg);
            report("Cursor", &path, r);
        }
        _ => println!(
            "{:<12} not detected (no ~/.cursor directory); snippet: attempt mcp --print-config",
            "Cursor"
        ),
    }
    match codex_path() {
        Some(path) if path.parent().is_some_and(Path::is_dir) => {
            let r = install_codex(&path, &reg);
            report("Codex", &path, r);
        }
        _ => println!(
            "{:<12} not detected (no ~/.codex directory); snippet: attempt mcp --print-config",
            "Codex"
        ),
    }
    println!("{:<12} run:  {}", "Claude Code", reg.claude_command());
    println!(
        "{:<12} (Claude Code keeps MCP servers in its own store; add --scope user to share across projects)",
        ""
    );
    println!();
    println!("restart the agents (or reload their windows) so they pick up the new server.");
    Ok(if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// Structural edit of Cursor's `mcp.json`: only `mcpServers.attemptdb.command`
/// and `.args` are touched; other keys of the entry (`env`, …), other servers
/// and the file's indentation survive.
pub fn install_cursor(path: &Path, reg: &Registration) -> Result<Installed> {
    let (mut root, style, existed) = match std::fs::read(path) {
        Ok(bytes) => {
            let text = String::from_utf8(bytes).map_err(|_| {
                anyhow!(
                    "{} is not valid UTF-8; refusing to modify it",
                    path.display()
                )
            })?;
            let style = Style::detect(&text);
            let value: Value = if text.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&text).map_err(|e| {
                    anyhow!(
                        "{} is not valid JSON ({e}); fix or move the file and re-run",
                        path.display()
                    )
                })?
            };
            (value, style, true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (json!({}), Style::default(), false),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let Value::Object(obj) = &mut root else {
        bail!(
            "{} must contain a JSON object; refusing to modify it",
            path.display()
        );
    };
    let servers = obj.entry("mcpServers").or_insert_with(|| json!({}));
    let Value::Object(servers) = servers else {
        bail!(
            "\"mcpServers\" in {} is not an object; refusing to modify it",
            path.display()
        );
    };
    let present = servers.contains_key(SERVER_KEY);
    let entry = servers.entry(SERVER_KEY).or_insert_with(|| json!({}));
    let Value::Object(entry) = entry else {
        bail!(
            "\"mcpServers.{SERVER_KEY}\" in {} is not an object; refusing to modify it",
            path.display()
        );
    };
    let command = json!(reg.command);
    let args = json!(reg.args);
    if entry.get("command") == Some(&command) && entry.get("args") == Some(&args) {
        return Ok(Installed::AlreadyCurrent);
    }
    entry.insert("command".into(), command);
    entry.insert("args".into(), args);
    let bytes = render_json(&root, style)?;
    let backup = if existed {
        Some(backup_file(path)?)
    } else {
        None
    };
    write_atomically(path, &bytes)?;
    Ok(if present {
        Installed::Updated { backup }
    } else {
        Installed::Added { backup }
    })
}

fn insert_codex_entry(doc: &mut toml_edit::DocumentMut, reg: &Registration) {
    let root = doc.as_table_mut();
    let servers = root.entry("mcp_servers").or_insert_with(|| {
        let mut t = toml_edit::Table::new();
        t.set_implicit(true);
        toml_edit::Item::Table(t)
    });
    if let Some(servers) = servers.as_table_mut() {
        let entry = servers
            .entry(SERVER_KEY)
            .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
        if let Some(entry) = entry.as_table_like_mut() {
            entry.insert("command", toml_edit::value(reg.command.clone()));
            let mut arr = toml_edit::Array::new();
            for a in &reg.args {
                arr.push(a.clone());
            }
            entry.insert("args", toml_edit::value(arr));
        }
    }
}

/// Structural edit of Codex's `config.toml` with `toml_edit`: comments,
/// ordering and formatting of everything else are preserved.
pub fn install_codex(path: &Path, reg: &Registration) -> Result<Installed> {
    let (text, existed) = match std::fs::read_to_string(path) {
        Ok(t) => (t, true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), false),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| {
        anyhow!(
            "{} is not valid TOML ({e}); fix or move the file and re-run",
            path.display()
        )
    })?;
    let servers = doc.get("mcp_servers");
    if servers.is_some_and(|s| !s.is_table()) {
        bail!(
            "[mcp_servers] in {} is not a table; refusing to modify it",
            path.display()
        );
    }
    let existing = servers.and_then(|s| s.get(SERVER_KEY));
    if existing.is_some_and(|e| !e.is_table_like()) {
        bail!(
            "[mcp_servers.{SERVER_KEY}] in {} is not a table; refusing to modify it",
            path.display()
        );
    }
    let present = existing.is_some();
    let current_command = existing
        .and_then(|e| e.get("command"))
        .and_then(|i| i.as_str())
        .map(str::to_string);
    let current_args: Option<Vec<String>> = existing
        .and_then(|e| e.get("args"))
        .and_then(|i| i.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        });
    if current_command.as_deref() == Some(reg.command.as_str())
        && current_args.as_deref() == Some(reg.args.as_slice())
    {
        return Ok(Installed::AlreadyCurrent);
    }
    insert_codex_entry(&mut doc, reg);
    let backup = if existed {
        Some(backup_file(path)?)
    } else {
        None
    };
    write_atomically(path, doc.to_string().as_bytes())?;
    Ok(if present {
        Installed::Updated { backup }
    } else {
        Installed::Added { backup }
    })
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{name}{suffix}"))
}

/// Copy `path` to `<path>.attemptdb.bak-<unix ts>` (same convention as the
/// hook installer).
fn backup_file(path: &Path) -> Result<PathBuf> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut dst = sibling(path, &format!(".attemptdb.bak-{ts}"));
    let mut n = 1;
    while dst.exists() {
        dst = sibling(path, &format!(".attemptdb.bak-{ts}-{n}"));
        n += 1;
    }
    std::fs::copy(path, &dst)
        .with_context(|| format!("backing up {} to {}", path.display(), dst.display()))?;
    Ok(dst)
}

/// Write via `<path>.attemptdb.tmp` + rename.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = sibling(path, ".attemptdb.tmp");
    let result = (|| -> Result<()> {
        std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
        if let Ok(meta) = std::fs::metadata(path) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        if std::fs::rename(&tmp, path).is_err() {
            // Windows may refuse to replace an open file; fall back to
            // remove-then-rename (short window without the file).
            if path.exists() {
                std::fs::remove_file(path)
                    .with_context(|| format!("replacing {}", path.display()))?;
            }
            std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> Registration {
        Registration {
            command: "/opt/bin/attempt".into(),
            args: vec!["mcp".into()],
        }
    }

    #[test]
    fn cursor_install_is_structural_and_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mcp.json");
        std::fs::write(
            &path,
            "{\n    \"mcpServers\": {\n        \"other\": {\"command\": \"x\"},\n        \"attemptdb\": {\"command\": \"/old/attempt\", \"args\": [\"mcp\"], \"env\": {\"A\": \"1\"}}\n    },\n    \"keep\": true\n}\n",
        )
        .unwrap();
        let r = install_cursor(&path, &reg()).unwrap();
        assert!(matches!(r, Installed::Updated { backup: Some(_) }));
        let text = std::fs::read_to_string(&path).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["mcpServers"]["attemptdb"]["command"], "/opt/bin/attempt");
        assert_eq!(v["mcpServers"]["attemptdb"]["env"]["A"], "1");
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert_eq!(v["keep"], true);
        assert!(
            text.contains("\n    \"mcpServers\""),
            "indentation kept: {text}"
        );
        assert_eq!(
            install_cursor(&path, &reg()).unwrap(),
            Installed::AlreadyCurrent
        );
        // Fresh file.
        let fresh = tmp.path().join("new").join("mcp.json");
        std::fs::create_dir_all(fresh.parent().unwrap()).unwrap();
        assert_eq!(
            install_cursor(&fresh, &reg()).unwrap(),
            Installed::Added { backup: None }
        );
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&fresh).unwrap()).unwrap();
        assert_eq!(v, reg().mcp_json());
        // Broken file is refused.
        std::fs::write(&path, "{ nope").unwrap();
        assert!(install_cursor(&path, &reg()).is_err());
    }

    #[test]
    fn codex_install_preserves_formatting_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "# my codex config\nmodel = \"o3\"\n\n[mcp_servers.other]\ncommand = \"x\"\n",
        )
        .unwrap();
        let r = install_codex(&path, &reg()).unwrap();
        assert!(matches!(r, Installed::Added { backup: Some(_) }));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("# my codex config\nmodel = \"o3\"\n"),
            "{text}"
        );
        assert!(
            text.contains("[mcp_servers.other]\ncommand = \"x\"\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "[mcp_servers.attemptdb]\ncommand = \"/opt/bin/attempt\"\nargs = [\"mcp\"]\n"
            ),
            "{text}"
        );
        assert_eq!(
            install_codex(&path, &reg()).unwrap(),
            Installed::AlreadyCurrent
        );
        let changed = Registration {
            command: "/new/attempt".into(),
            args: vec!["mcp".into(), "--max-rows".into(), "50".into()],
        };
        assert!(matches!(
            install_codex(&path, &changed).unwrap(),
            Installed::Updated { .. }
        ));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("command = \"/new/attempt\"\nargs = [\"mcp\", \"--max-rows\", \"50\"]"),
            "{text}"
        );
        assert_eq!(text.matches("[mcp_servers.attemptdb]").count(), 1);
        // Fresh file and refusal of broken TOML.
        let fresh = tmp.path().join("fresh.toml");
        assert_eq!(
            install_codex(&fresh, &reg()).unwrap(),
            Installed::Added { backup: None }
        );
        assert_eq!(std::fs::read_to_string(&fresh).unwrap(), reg().codex_toml());
        std::fs::write(&path, "[broken").unwrap();
        assert!(install_codex(&path, &reg()).is_err());
    }

    #[test]
    fn snippets() {
        let r = reg();
        assert_eq!(
            r.claude_command(),
            "claude mcp add attemptdb -- '/opt/bin/attempt' 'mcp'"
        );
        assert_eq!(
            r.codex_toml(),
            "[mcp_servers.attemptdb]\ncommand = \"/opt/bin/attempt\"\nargs = [\"mcp\"]\n"
        );
        assert_eq!(r.mcp_json()["mcpServers"]["attemptdb"]["args"][0], "mcp");
    }
}
