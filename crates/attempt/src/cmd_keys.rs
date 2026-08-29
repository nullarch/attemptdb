//! `attempt keys`: master-key management for encrypted content blobs.
//!
//! ```text
//! attempt keys status                       source, key id, blobs, unencrypted segments
//! attempt keys init [--key-file] [--passphrase-env VAR]
//! attempt keys export <out> [--yes]         master key → 0600 hex file (typed confirmation)
//! attempt keys rotate [--forget-old] [--yes]
//! ```
//!
//! Key material is never printed; only key ids and sources are.

use crate::cli::Cli;
use crate::ctx::Ctx;
use crate::render::{human_bytes, print_json};
use anyhow::{Context, Result};
use attemptdb_capture::keys::{self, InitOptions, KeySource, KeyStoreOptions};
use attemptdb_storage::Identity;
use clap::{Args, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Args, Debug)]
pub struct KeysArgs {
    #[command(subcommand)]
    pub cmd: KeysCmd,
}

#[derive(Subcommand, Debug)]
pub enum KeysCmd {
    /// Key source, key id, blob count and size, and segments that still hold unencrypted content.
    Status,
    /// Create the master key for this database (OS key store by default).
    Init {
        /// Store the key in `<data_dir>/keys/<db_id>.key` (mode 0600) instead of the OS key store.
        #[arg(long)]
        key_file: bool,
        /// Derive the key from the passphrase in this environment variable; nothing is stored,
        /// so the variable must be set for every command.
        #[arg(long, value_name = "VAR")]
        passphrase_env: Option<String>,
    },
    /// Write the master key to a 0600 hex file (backup, or `--key-file` on another device).
    Export {
        /// Output path; must not exist.
        out: PathBuf,
        /// Skip the typed confirmation.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Generate a new master key and re-encrypt every blob under it. The old key stays
    /// retained under its id unless --forget-old.
    Rotate {
        /// Remove the old key from its source once every blob is rewritten. Irreversible.
        #[arg(long)]
        forget_old: bool,
        /// Skip the typed confirmation that --forget-old asks for.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

pub fn run(cli: &Cli, args: &KeysArgs) -> Result<ExitCode> {
    let ctx = Ctx::new(cli)?;
    let db_dir = ctx.locator.db_dir.clone();
    let identity = Identity::load(&db_dir).with_context(|| {
        format!(
            "no database at {} (run `attempt init` first)",
            db_dir.display()
        )
    })?;
    let db_id = identity.db_id;
    let store_opts = KeyStoreOptions::from_env();
    match &args.cmd {
        KeysCmd::Status => {
            let st = keys::status(&ctx.locator, &db_dir, Some(store_opts))?;
            if cli.json {
                print_json(&serde_json::json!({
                    "database": db_dir,
                    "db_id": st.db_id,
                    "source": st.source,
                    "key_id": st.key_id,
                    "blob_key_ids": st.blob_key_ids,
                    "missing_key_ids": st.missing_key_ids,
                    "blobs": st.blobs,
                    "blob_bytes": st.blob_bytes,
                    "segments": st.segments,
                    "inline_segments": st.inline_segments,
                    "encryption": ctx.config.encryption.as_str(),
                    "notes": st.notes,
                }));
                return Ok(ExitCode::SUCCESS);
            }
            println!("database      {}", db_dir.display());
            println!("encryption    {}", ctx.config.encryption);
            println!("key source    {}", st.source);
            println!(
                "key id        {}",
                st.key_id
                    .map(|k| k.to_string())
                    .unwrap_or_else(|| "none".into())
            );
            println!(
                "blobs         {} ({})",
                st.blobs,
                human_bytes(st.blob_bytes)
            );
            println!(
                "segments      {} total, {} with unencrypted inline content (format 1)",
                st.segments, st.inline_segments
            );
            if !st.missing_key_ids.is_empty() {
                println!(
                    "locked        content under key id(s) {} cannot be read: no source holds the key",
                    st.missing_key_ids
                        .iter()
                        .map(|k| k.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            for n in &st.notes {
                println!("note: {n}");
            }
            if st.key_id.is_none() {
                println!();
                println!(
                    "no key: content is written inline; run `attempt keys init` to encrypt from the next flush on"
                );
            } else if st.inline_segments > 0 {
                println!();
                println!(
                    "{} older segment(s) keep inline content until compaction rewrites them (planned)",
                    st.inline_segments
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        KeysCmd::Init {
            key_file,
            passphrase_env,
        } => {
            let report = keys::init(
                &ctx.locator,
                db_id,
                &InitOptions {
                    key_file: *key_file,
                    passphrase_env: passphrase_env.clone(),
                    store: Some(store_opts),
                },
            )?;
            if cli.json {
                print_json(&report);
                return Ok(ExitCode::SUCCESS);
            }
            if report.created {
                println!("created key {} ({})", report.key_id, report.reason);
            } else {
                println!("key {} already exists ({})", report.key_id, report.reason);
            }
            println!("key source    {}", report.source);
            if report.created {
                println!();
                println!(
                    "content is encrypted from the next flush on; earlier segments stay inline until compaction (planned)"
                );
                match report.source {
                    KeySource::Passphrase => println!(
                        "keep the passphrase safe: without it the content is unrecoverable"
                    ),
                    _ => println!(
                        "back it up with `attempt keys export <file>` and keep that file offline; without the key the content is unrecoverable"
                    ),
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        KeysCmd::Export { out, yes } => {
            if !*yes
                && !confirm(
                    "export",
                    "This writes the master key in clear to a file. Anyone holding it can read every content blob of this database.",
                )?
            {
                println!("aborted; nothing was written");
                return Ok(ExitCode::from(1));
            }
            let key_id = keys::export_master(&ctx.locator, db_id, Some(store_opts), out)?;
            if cli.json {
                print_json(&serde_json::json!({"key_id": key_id, "file": out}));
            } else {
                println!("wrote key {key_id} to {} (mode 0600)", out.display());
                println!(
                    "use it elsewhere with `ATTEMPTDB_KEY_FILE={}` or `--key-file`; store it offline",
                    out.display()
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        KeysCmd::Rotate { forget_old, yes } => {
            if *forget_old
                && !*yes
                && !confirm(
                    "rotate",
                    "--forget-old deletes the previous key once every blob is rewritten. Blobs that could not be rewritten would become unreadable.",
                )?
            {
                println!("aborted; keys unchanged");
                return Ok(ExitCode::from(1));
            }
            let report = keys::rotate(&ctx.locator, &db_dir, Some(store_opts), *forget_old)?;
            if cli.json {
                print_json(&report);
            } else {
                println!(
                    "rotated {} -> {} ({})",
                    report.old_key_id, report.new_key_id, report.source
                );
                println!(
                    "blobs         {} rewritten, {} already current, {} failed",
                    report.rewritten,
                    report.skipped,
                    report.failed.len()
                );
                for f in report.failed.iter().take(20) {
                    println!("failed: {f}");
                }
                if report.forgot_old {
                    println!("old key       removed from its source");
                } else if *forget_old {
                    println!(
                        "old key       kept: {} blob(s) still need it; fix them and run again",
                        report.failed.len()
                    );
                } else {
                    println!(
                        "old key       retained under its id; run `attempt keys rotate --forget-old` later to drop it"
                    );
                }
            }
            Ok(if report.failed.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
    }
}

/// Typed confirmation on a terminal; refuses when stdin is not one.
fn confirm(word: &str, warning: &str) -> Result<bool> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("refusing without confirmation; pass --yes to confirm non-interactively");
    }
    println!("{warning}");
    print!("type '{word}' to confirm: ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok();
    Ok(line.trim() == word)
}
