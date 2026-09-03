//! `attempt schema` — what every queryable table and column means.
//!
//! The one command here that never opens the database: the catalog comes
//! from the code, so it answers on a fresh clone, before a single event has
//! been captured. That is the point — an agent reading this repository can
//! learn how to query it before there is anything to query.

use crate::cli::{Cli, SchemaArgs, SchemaFormat};
use crate::render::print_json;
use anyhow::{Result, bail};
use attemptdb_query::catalog;
use std::process::ExitCode;

const WIDTH: usize = 88;

/// Wrap `text` to [`WIDTH`], indenting every line by `indent` spaces.
fn wrap(text: &str, indent: usize) -> String {
    wrap_from("", text, indent)
}

/// [`wrap`] with `lead` written into the indent of the first line, so a
/// numbered item keeps its number on the same line as its text.
fn wrap_from(lead: &str, text: &str, indent: usize) -> String {
    let mut first = if lead.is_empty() {
        String::new()
    } else {
        format!("{lead:<indent$}")
    };
    let pad = " ".repeat(indent);
    let mut out = String::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > WIDTH - indent {
            out.push_str(if first.is_empty() { &pad } else { &first });
            first.clear();
            out.push_str(&line);
            out.push('\n');
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push_str(if first.is_empty() { &pad } else { &first });
        out.push_str(&line);
        out.push('\n');
    }
    out
}

fn print_examples() {
    println!("Example questions\n");
    for e in catalog::examples() {
        println!("  {}", e.question);
        println!("    {}", e.statement);
        print!("{}", wrap(e.note, 4));
        println!();
    }
    println!(
        "  Placeholders ({}) stand for a real id.",
        catalog::PLACEHOLDERS.join(", ")
    );
}

fn print_table(t: &catalog::Table) {
    println!(
        "{}  ·  {}  ·  one row per {}\n",
        t.name,
        t.layer.as_str(),
        t.grain
    );
    print!("{}", wrap(t.summary, 2));
    if !t.joins.is_empty() {
        let j = t
            .joins
            .iter()
            .map(|j| format!("{} -> {}", j.column, j.target))
            .collect::<Vec<_>>()
            .join(", ");
        println!();
        print!("{}", wrap(&format!("joins: {j}"), 2));
    }
    println!();
    for c in &t.columns {
        let null = if c.nullable { "  null" } else { "" };
        println!("  {:<26} {}{}", c.name, c.data_type, null);
        print!("{}", wrap(c.doc, 4));
        if !c.values.is_empty() {
            let label = if c.open { "common values" } else { "values" };
            print!("{}", wrap(&format!("{label}: {}", c.values.join(", ")), 4));
        }
    }
}

fn print_overview() {
    print!("{}", wrap(catalog::OVERVIEW, 0));
    println!();
    println!("Rules\n");
    for (i, r) in catalog::RULES.iter().enumerate() {
        print!("{}", wrap_from(&format!("  {}.", i + 1), r, 6));
        println!();
    }
    println!("Tables\n");
    for t in catalog::catalog() {
        println!(
            "  {:<14} {:<10} {:>2} column(s) · one row per {}",
            t.name,
            t.layer.as_str(),
            t.columns.len(),
            t.grain
        );
    }
    println!();
    println!("  attempt schema <table>            what one table's columns mean");
    println!("  attempt schema --examples         questions and the statements answering them");
    println!("  attempt schema --format markdown  the whole catalog (docs/query-context.md)");
}

pub fn run(cli: &Cli, args: &SchemaArgs) -> Result<ExitCode> {
    let format = if cli.json {
        SchemaFormat::Json
    } else {
        args.format
    };
    if let Some(name) = &args.table {
        let Some(t) = catalog::table(name) else {
            bail!(
                "unknown table {name:?}; known tables: {}",
                attemptdb_query::TABLE_NAMES.join(", ")
            );
        };
        match format {
            SchemaFormat::Json => print_json(
                &catalog::json()["tables"]
                    .as_array()
                    .and_then(|a| a.iter().find(|v| v["name"] == t.name).cloned())
                    .unwrap_or_default(),
            ),
            SchemaFormat::Markdown => {
                print!("{}", catalog::markdown_table(t.name).unwrap_or_default())
            }
            SchemaFormat::Text => print_table(&t),
        }
        return Ok(ExitCode::SUCCESS);
    }
    if args.examples {
        match format {
            SchemaFormat::Json => print_json(&catalog::json()["examples"]),
            SchemaFormat::Markdown | SchemaFormat::Text => print_examples(),
        }
        return Ok(ExitCode::SUCCESS);
    }
    match format {
        SchemaFormat::Json => print_json(&catalog::json()),
        SchemaFormat::Markdown => print!("{}", catalog::markdown()),
        SchemaFormat::Text => print_overview(),
    }
    Ok(ExitCode::SUCCESS)
}
