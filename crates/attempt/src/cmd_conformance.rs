//! `attempt conformance <FILE>` — is this stream AttemptDB Event v1?

use anyhow::{Context, Result};
use attemptdb_core::conformance::{Report, check_jsonl};
use clap::Args;
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Args, Debug)]
pub struct ConformanceArgs {
    /// Newline-delimited JSON of canonical events; `-` reads stdin.
    pub file: PathBuf,
    /// Print the report as JSON.
    #[arg(long)]
    pub json: bool,
    /// Show every finding, not just the first few per section.
    #[arg(long)]
    pub all: bool,
}

pub fn run(args: &ConformanceArgs) -> Result<ExitCode> {
    let text = if args.file.as_os_str() == "-" {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .context("reading stdin")?;
        s
    } else {
        std::fs::read_to_string(&args.file)
            .with_context(|| format!("reading {}", args.file.display()))?
    };
    let report = check_jsonl(&text);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&report, args.all);
    }
    Ok(if report.compatible() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn print_report(r: &Report, all: bool) {
    println!(
        "AttemptDB Event v{} · {} event(s) on {} line(s)\n",
        r.schema_version, r.events, r.lines
    );
    let limit = if all { usize::MAX } else { 5 };
    for (name, section) in r.sections() {
        let mark = if section.ok() { "✓" } else { "✗" };
        let summary = match (section.failures.len(), section.notes.len()) {
            (0, 0) => String::new(),
            (0, n) => format!("{n} note(s)"),
            (f, 0) => format!("{f} failure(s)"),
            (f, n) => format!("{f} failure(s), {n} note(s)"),
        };
        println!("{name:<20}{mark}   {summary}");
        for f in section.failures.iter().take(limit) {
            println!("{:<20}    line {}: {}", "", f.line, f.message);
        }
        if section.failures.len() > limit {
            println!(
                "{:<20}    … {} more (use --all)",
                "",
                section.failures.len() - limit
            );
        }
        for n in section.notes.iter().take(if all { usize::MAX } else { 2 }) {
            if n.line == 0 {
                println!("{:<20}    note: {}", "", n.message);
            } else {
                println!("{:<20}    note (line {}): {}", "", n.line, n.message);
            }
        }
        if !all && section.notes.len() > 2 {
            println!("{:<20}    … {} more note(s)", "", section.notes.len() - 2);
        }
    }
    println!();
    if r.compatible() {
        println!("COMPATIBLE");
    } else {
        println!("NOT COMPATIBLE — {} failure(s)", r.failure_count());
    }
}
