//! `attempt ui`: serve the local AgentTimeline web UI, or export a static,
//! self-contained HTML timeline.
//!
//! Wiring (in `cli.rs` / `main.rs`):
//!
//! ```text
//! /// Open the local AgentTimeline UI (or `ui export <out.html>`).
//! Ui(crate::cmd_ui::UiArgs),                        // cli.rs, replaces the unit variant
//! Command::Ui(args) => cmd_ui::run(&cli, args),     // main.rs; drop `Ui` from `not_yet`
//! ```

use crate::cli::{Cli, ScopeArgs};
use crate::ctx::Ctx;
use anyhow::{Context, Result, bail};
use attemptdb_storage::{Database, ScanFilter};
use attemptdb_ui::export::{ExportOptions, render_database};
use attemptdb_ui::{Server, UiConfig, open_browser, parse_bind};
use clap::{Args, Subcommand};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Args, Debug)]
pub struct UiArgs {
    #[command(subcommand)]
    pub cmd: Option<UiCmd>,
    /// Port to listen on (default: a random free port).
    #[arg(long, value_name = "N")]
    pub port: Option<u16>,
    /// Print the URL without opening the system browser.
    #[arg(long)]
    pub no_open: bool,
    /// Interface to bind (default 127.0.0.1). Anything but loopback also needs --allow-remote.
    #[arg(long, value_name = "ADDR")]
    pub bind: Option<String>,
    /// Allow binding a non-loopback interface: the per-run token becomes the only protection.
    #[arg(long)]
    pub allow_remote: bool,
    /// Open the bundled, clearly labelled build-history demo instead of this machine's database.
    #[arg(long)]
    pub demo: bool,
}

#[derive(Subcommand, Debug)]
pub enum UiCmd {
    /// Write the timeline as ONE self-contained HTML file, or a sanitized summary image (`.svg`). No server, no token.
    Export {
        /// Output path: `.html` for the full replay, `.svg` for the summary card.
        out: PathBuf,
        /// Strip prompts, commands, tool output, raw payloads, absolute paths and home directories.
        #[arg(long)]
        sanitized: bool,
        /// Omit the "Built with AttemptDB" footer.
        #[arg(long)]
        no_attribution: bool,
        #[command(flatten)]
        scope: ScopeArgs,
    },
}

pub fn run(cli: &Cli, args: &UiArgs) -> Result<ExitCode> {
    match &args.cmd {
        Some(UiCmd::Export {
            out,
            sanitized,
            no_attribution,
            scope,
        }) => export(cli, out, *sanitized, !*no_attribution, scope),
        None => serve(cli, args),
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the tokio runtime")
}

fn serve(cli: &Cli, args: &UiArgs) -> Result<ExitCode> {
    let ctx = Ctx::new(cli)?;
    // `--demo` serves a separate, generated database, so the usual "you have
    // no database yet" check does not apply to it.
    if !args.demo && cli.snapshot.is_none() && !Database::exists(&ctx.locator.db_dir) {
        bail!(
            "no database at {}\n  run `attempt init` first (or `attempt init --local` for a project-local database)",
            ctx.locator.db_dir.display()
        );
    }
    let bind = parse_bind(args.bind.as_deref())?;
    if !bind.is_loopback() {
        if !args.allow_remote {
            bail!(
                "--bind {bind} is not a loopback address; add --allow-remote to expose the UI to the network (not recommended)"
            );
        }
        eprintln!("WARNING: binding {bind}: the UI will be reachable from other machines.");
        eprintln!(
            "WARNING: the per-run token in the URL is the ONLY protection; anyone who obtains it"
        );
        eprintln!(
            "WARNING: can read every prompt, path and tool call in this database. Prefer an SSH tunnel."
        );
    }
    let config = UiConfig {
        db_dir: ctx.locator.db_dir.clone(),
        data_dir: cli.data_dir.clone(),
        snapshot: cli.snapshot.clone(),
        project_root: Some(ctx.cwd.clone()),
        bind,
        port: args.port.unwrap_or(0),
        allow_remote: args.allow_remote,
    };
    let source = attemptdb_ui::describe_source(&config);
    let rt = runtime()?;
    rt.block_on(async move {
        let server = Server::bind(config).await?;
        let url = if args.demo {
            format!("{}&demo=1", server.url())
        } else {
            server.url()
        };
        println!("AttemptDB AgentTimeline UI");
        println!("  url       {url}");
        if args.demo {
            println!(
                "  database  bundled demo (synthesized build history, labelled on every page)"
            );
        } else {
            println!("  database  {source}");
        }
        println!(
            "  bound to  {} (token required; the browser keeps a session cookie)",
            server.addr()
        );
        println!("  Ctrl+C stops the server");
        if !args.no_open
            && let Err(e) = open_browser(&url)
        {
            eprintln!("could not open a browser ({e}); open the URL above yourself");
        }
        server
            .run(async {
                let _ = tokio::signal::ctrl_c().await;
                eprintln!("stopping");
            })
            .await
    })?;
    Ok(ExitCode::SUCCESS)
}

fn export(
    cli: &Cli,
    out: &Path,
    sanitized: bool,
    attribution: bool,
    scope: &ScopeArgs,
) -> Result<ExitCode> {
    let ctx = Ctx::new(cli)?;
    let opened = ctx.open(cli)?;
    let facts = opened.load()?.facts;
    let filter = ctx.filter(scope, &facts)?;
    let scope_label = scope_label(&facts, &filter, scope);
    // `.svg` writes the summary card. It carries no content by construction,
    // so `--sanitized` is not a choice there: an image is shared, and an
    // image cannot be reviewed line by line before it is.
    if out
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("svg"))
    {
        return export_card(&opened, &filter, &facts, out, attribution, scope_label);
    }
    let options = ExportOptions {
        sanitized,
        attribution,
        session_limit: scope.limit.unwrap_or(50),
        scope_label,
        capture_mode: ctx.config.capture_mode,
    };
    let rt = runtime()?;
    let html = rt.block_on(render_database(&opened.db, &filter, options))?;
    drop(opened);
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(out, &html).with_context(|| format!("writing {}", out.display()))?;
    println!(
        "wrote {} ({} bytes){}",
        out.display(),
        html.len(),
        if sanitized {
            " — sanitized: no prompts, commands, tool output, absolute paths or home directories"
        } else {
            " — NOT sanitized: contains prompt text and full paths; review before sharing (or use --sanitized)"
        }
    );
    Ok(ExitCode::SUCCESS)
}

/// The sanitized summary card: one SVG for a README, an issue or a social
/// preview.
fn export_card(
    opened: &crate::ctx::Opened,
    filter: &ScanFilter,
    facts: &attemptdb_query::StreamFacts,
    out: &Path,
    attribution: bool,
    scope_label: String,
) -> Result<ExitCode> {
    let events = opened.db.scan(filter).context("scanning events")?;
    let projection = attemptdb_project::project(&events);
    let project = filter
        .project_id
        .and_then(|pid| facts.projects.get(&pid).map(|p| p.name.clone()));
    let svg = attemptdb_ui::card::render(
        &projection,
        &attemptdb_ui::card::CardOptions {
            project,
            window: Some(scope_label),
            attribution,
        },
    );
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(out, &svg).with_context(|| format!("writing {}", out.display()))?;
    println!(
        "wrote {} ({} bytes) — {}×{} sanitized summary card: outcomes, failure classes, counts and repository-relative paths only",
        out.display(),
        svg.len(),
        attemptdb_ui::card::WIDTH as u32,
        attemptdb_ui::card::HEIGHT as u32,
    );
    Ok(ExitCode::SUCCESS)
}

/// `project acme/repo · since …` for the export header, without printing
/// any path.
fn scope_label(
    facts: &attemptdb_query::StreamFacts,
    filter: &ScanFilter,
    scope: &ScopeArgs,
) -> String {
    let mut parts = Vec::new();
    match filter.project_id {
        Some(pid) => {
            let name = facts
                .projects
                .get(&pid)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| format!("prj_{pid}"));
            parts.push(format!("project {name}"));
        }
        None => parts.push("all projects".to_string()),
    }
    if let Some(s) = &scope.session {
        parts.push(format!("session {s}"));
    }
    if let Some(t) = filter.since {
        parts.push(format!("since {}", t.to_rfc3339()));
    }
    if let Some(t) = filter.until {
        parts.push(format!("until {}", t.to_rfc3339()));
    }
    if scope.captured_only {
        parts.push("hook-captured events only".to_string());
    }
    parts.join(" · ")
}
