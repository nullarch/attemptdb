//! `attempt` — the AttemptDB command-line interface.
//!
//! One binary, several modes: CLI client, hook entrypoint, (planned) daemon,
//! MCP server, and UI server. The hook entrypoint is dispatched before any
//! heavy initialisation so its startup cost stays in the low milliseconds.

mod cli;
mod cmd_daemon;
mod cmd_db;
mod cmd_hook;
mod cmd_import;
mod cmd_query;
mod ctx;
mod render;

use clap::Parser;
use cli::{Cli, Command};
use std::process::ExitCode;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match &cli.command {
        Command::Hook(args) => cmd_hook::run(&cli, args),
        Command::Init(args) => cmd_db::init(&cli, args),
        Command::Status => cmd_db::status(&cli),
        Command::Verify => cmd_db::verify(&cli),
        Command::Import(args) => match &args.source {
            None => cmd_db::import(&cli),
            Some(cli::ImportSource::ClaudeTranscripts(a)) => cmd_import::claude_transcripts(&cli, a),
        },
        Command::Events(args) => cmd_db::events(&cli, args),
        Command::Snapshot(args) => cmd_db::snapshot(&cli, args),
        Command::Doctor => cmd_hook::doctor(&cli),
        Command::Timeline(args) => cmd_query::timeline(&cli, args),
        Command::Query(args) => cmd_query::query(&cli, args),
        Command::Why(args) => cmd_query::why(&cli, args),
        Command::Trace(args) => cmd_query::trace(&cli, args),
        Command::Failures(args) => cmd_query::failures(&cli, args),
        Command::Handoffs(args) => cmd_query::handoffs(&cli, args),
        Command::Tables => cmd_query::tables(&cli),
        Command::Uninstall(args) => cmd_db::uninstall(&cli, args),
        Command::Daemon(args) => cmd_daemon::run(&cli, args),
        Command::Ui | Command::Mcp | Command::Update => cmd_db::not_yet(&cli),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}
