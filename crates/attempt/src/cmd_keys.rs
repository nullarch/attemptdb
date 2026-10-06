//! `attempt keys`: master-key management for encrypted content blobs.
//!
//! ```text
//! attempt keys status [--full]              source, key id, blobs, unencrypted segments
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
    /// The blob count and size are estimated from a few shard directories (instant); `--full`
    /// counts every blob.
    Status {
        /// Count every blob and read every header: exact, but minutes on millions of blobs.
        /// Reports progress; Ctrl-C stops it and prints what was counted so far.
        #[arg(long)]
        full: bool,
    },
    /// Create the master key for this database (OS key store by default).
    Init {
        /// Store the key in `<data_dir>/keys/<db_id>.key` (mode 0600) instead of the OS key store.
        #[arg(long)]
        /// Create a key file under the data directory instead of using the OS key store.
        #[arg(long = "file")]
        use_file: bool,
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
        KeysCmd::Status { full } => {
            let interrupted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let st = if *full {
                watch_ctrl_c(interrupted.clone());
                let mut report = |p: keys::ScanProgress| {
                    eprintln!(
                        "scanning blobs: shard {}/{} · {} blobs · {}s (Ctrl-C stops and prints what was counted)",
                        p.shards_done,
                        p.shards,
                        p.blobs,
                        p.elapsed.as_secs()
                    );
                };
                keys::status_with(
                    &ctx.locator,
                    &db_dir,
                    Some(store_opts),
                    keys::StatusDepth::Full {
                        cancel: &interrupted,
                        progress: &mut report,
                    },
                )?
            } else {
                keys::status(&ctx.locator, &db_dir, Some(store_opts))?
            };
            let waiting = attemptdb_capture::ingest::spool_waiting(&db_dir);
            let advice = key_advice(ctx.config.encryption, &st, waiting.events);
            if cli.json {
                print_json(&serde_json::json!({
                    "database": db_dir,
                    "db_id": st.db_id,
                    "source": st.source,
                    "key_id": st.key_id,
                    "key_state": advice.state,
                    "blob_key_ids": st.blob_key_ids,
                    "missing_key_ids": st.missing_key_ids,
                    "blobs": st.blobs,
                    "blob_bytes": st.blob_bytes,
                    "blobs_estimated": st.blobs_estimated,
                    "blob_shards": st.blob_shards,
                    "blob_shards_read": st.blob_shards_read,
                    "interrupted": st.interrupted,
                    "segments": st.segments,
                    "inline_segments": st.inline_segments,
                    "encryption": ctx.config.encryption.as_str(),
                    "events_waiting_for_key": waiting.events,
                    "notes": st.notes,
                }));
                return Ok(if st.interrupted {
                    ExitCode::from(130)
                } else {
                    ExitCode::SUCCESS
                });
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
            if st.interrupted {
                println!(
                    "blobs         at least {} ({}), interrupted after {} of {} shard directories",
                    st.blobs,
                    human_bytes(st.blob_bytes),
                    st.blob_shards_read,
                    st.blob_shards
                );
            } else if st.blobs_estimated {
                println!(
                    "blobs         about {} ({}), estimated from {} of {} shard directories; `attempt keys status --full` counts every one (minutes on a large database)",
                    st.blobs,
                    human_bytes(st.blob_bytes),
                    st.blob_shards_read,
                    st.blob_shards
                );
            } else {
                println!(
                    "blobs         {} ({})",
                    st.blobs,
                    human_bytes(st.blob_bytes)
                );
            }
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
            if let Some(text) = &advice.text {
                println!();
                println!("{text}");
            } else if st.inline_segments > 0 {
                println!();
                println!(
                    "{} older segment(s) keep inline content until compaction rewrites them (planned)",
                    st.inline_segments
                );
            }
            Ok(if st.interrupted {
                ExitCode::from(130)
            } else {
                ExitCode::SUCCESS
            })
        }
        KeysCmd::Init {
            use_file,
            passphrase_env,
        } => {
            let report = keys::init(
                &ctx.locator,
                db_id,
                &InitOptions {
                    key_file: *use_file,
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

/// What `attempt keys status` concludes about the key.
pub struct KeyAdvice {
    /// `ok`, `no_key_no_blobs`, `key_unreadable` or `key_missing`.
    pub state: &'static str,
    /// The paragraph printed under the numbers; `None` when there is
    /// nothing to say about the key.
    pub text: Option<String>,
}

/// Say what the missing key means, by case. "No key" is three different
/// situations with three different next steps, and one of them is a trap:
/// a database that already holds encrypted blobs must never be given a new
/// key with `keys init` (it would be a second key that opens none of them).
pub fn key_advice(
    mode: attemptdb_capture::EncryptionMode,
    st: &keys::KeysStatus,
    events_waiting: u64,
) -> KeyAdvice {
    use attemptdb_capture::EncryptionMode;
    if st.key_id.is_some() {
        return KeyAdvice {
            state: "ok",
            text: None,
        };
    }
    let has_blobs = st.blobs > 0 || !st.blob_key_ids.is_empty();
    if !has_blobs {
        let text = match mode {
            EncryptionMode::Off => "no key, and encryption is off: content is written inline, unencrypted, as configured".to_string(),
            EncryptionMode::Required => "no key, and encryption is required: new events are stored without their content until a key exists. Run `attempt keys init` to create one".to_string(),
            EncryptionMode::Auto => "no key yet and nothing is encrypted: content is written inline, unencrypted. Run `attempt keys init` to encrypt from the next flush on".to_string(),
        };
        return KeyAdvice {
            state: "no_key_no_blobs",
            text: Some(text),
        };
    }
    let ids = st
        .blob_key_ids
        .iter()
        .map(|k| k.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let blobs = if st.blobs_estimated {
        format!("about {}", st.blobs)
    } else {
        st.blobs.to_string()
    };
    let waiting = if events_waiting > 0 && mode != EncryptionMode::Off {
        format!(
            " {events_waiting} event(s) are waiting in the spool for it and are imported, with their content, as soon as it can be read."
        )
    } else {
        String::new()
    };
    let behaviour = if mode == EncryptionMode::Off {
        " Encryption is off, so new content is written inline."
    } else {
        " Until the key reads, new events wait in the spool; they are not lost."
    };
    let (state, what) = if st.notes.is_empty() {
        (
            "key_missing",
            format!(
                "KEY MISSING: this database holds {blobs} encrypted blob(s) (key id {ids}) and no key source has that key: it is not in the OS key store, no key file or passphrase is set, and nothing was reported unreadable. It was deleted, or it lives on another machine or in another user's key store. Restore it (a file made by `attempt keys export` works with ATTEMPTDB_KEY_FILE) and run this again."
            ),
        )
    } else {
        (
            "key_unreadable",
            format!(
                "KEY UNREADABLE: this database holds {blobs} encrypted blob(s) (key id {ids}) and a key source that should hold the key could not be read ({}). Unlock the OS key store (a macOS Keychain prompt must be answered in your own session, not in the background daemon), or set ATTEMPTDB_KEY_FILE / ATTEMPTDB_PASSPHRASE to the original key, and run this again.",
                st.notes.join("; ")
            ),
        )
    };
    KeyAdvice {
        state,
        text: Some(format!(
            "{what} Do NOT run `attempt keys init`: it would create a second key that opens none of these blobs.{behaviour}{waiting}"
        )),
    }
}

/// Set `flag` when the person presses Ctrl-C, without ending the process, so
/// a long scan can stop and say what it has.
fn watch_ctrl_c(flag: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        runtime.block_on(async {
            if tokio::signal::ctrl_c().await.is_ok() {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        });
    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use attemptdb_capture::EncryptionMode;

    fn status(key: bool, blobs: u64, notes: &[&str]) -> keys::KeysStatus {
        let id = uuid::Uuid::from_u128(7);
        keys::KeysStatus {
            db_id: uuid::Uuid::nil(),
            source: if key {
                KeySource::Keyring
            } else {
                KeySource::None
            },
            key_id: key.then_some(id),
            blob_key_ids: if blobs > 0 { vec![id] } else { vec![] },
            missing_key_ids: vec![],
            blobs,
            blob_bytes: blobs * 300,
            blobs_estimated: false,
            blob_shards: 256,
            blob_shards_read: 256,
            interrupted: false,
            segments: 3,
            inline_segments: 0,
            notes: notes.iter().map(|n| n.to_string()).collect(),
        }
    }

    #[test]
    fn a_key_needs_no_advice() {
        let a = key_advice(EncryptionMode::Auto, &status(true, 500, &[]), 0);
        assert_eq!(a.state, "ok");
        assert!(a.text.is_none());
    }

    #[test]
    fn no_key_and_no_blobs_says_content_is_inline_and_init_is_right() {
        let a = key_advice(EncryptionMode::Auto, &status(false, 0, &[]), 0);
        assert_eq!(a.state, "no_key_no_blobs");
        let text = a.text.unwrap();
        assert!(text.contains("written inline") && text.contains("attempt keys init"));
        assert!(!text.contains("Do NOT"), "{text}");

        // Required: content is not inline, it is withheld.
        let text = key_advice(EncryptionMode::Required, &status(false, 0, &[]), 0)
            .text
            .unwrap();
        assert!(text.contains("without their content") && text.contains("attempt keys init"));
        assert!(!text.contains("written inline"), "{text}");

        // Off: inline by choice.
        let text = key_advice(EncryptionMode::Off, &status(false, 0, &[]), 0)
            .text
            .unwrap();
        assert!(text.contains("encryption is off") && !text.contains("attempt keys init"));
    }

    #[test]
    fn an_unreadable_key_with_blobs_names_the_cause_and_forbids_init() {
        let a = key_advice(
            EncryptionMode::Auto,
            &status(
                false,
                4_090_000,
                &["OS key store unavailable: user interaction required"],
            ),
            640,
        );
        assert_eq!(a.state, "key_unreadable");
        let text = a.text.unwrap();
        assert!(text.contains("KEY UNREADABLE"), "{text}");
        assert!(text.contains("user interaction required"), "{text}");
        assert!(text.contains("4090000 encrypted blob"), "{text}");
        assert!(
            text.contains("Do NOT run `attempt keys init`") && text.contains("second key"),
            "{text}"
        );
        assert!(text.contains("640 event(s) are waiting"), "{text}");
        assert!(
            !text.contains("written inline"),
            "the stale message is gone: {text}"
        );
    }

    #[test]
    fn a_missing_key_with_blobs_says_where_it_might_be_and_forbids_init() {
        let a = key_advice(EncryptionMode::Auto, &status(false, 12, &[]), 0);
        assert_eq!(a.state, "key_missing");
        let text = a.text.unwrap();
        assert!(text.contains("KEY MISSING"), "{text}");
        assert!(text.contains("another machine") && text.contains("ATTEMPTDB_KEY_FILE"));
        assert!(text.contains("Do NOT run `attempt keys init`"), "{text}");
        assert!(!text.contains("waiting"), "nothing waits: {text}");
    }

    #[test]
    fn an_estimated_blob_count_is_labelled_in_the_advice() {
        let mut st = status(false, 4_000_000, &["OS key store unavailable: locked"]);
        st.blobs_estimated = true;
        let text = key_advice(EncryptionMode::Auto, &st, 0).text.unwrap();
        assert!(text.contains("about 4000000"), "{text}");
    }

    #[test]
    fn off_with_an_unreadable_key_says_new_content_is_inline() {
        let text = key_advice(
            EncryptionMode::Off,
            &status(false, 5, &["OS key store unavailable: locked"]),
            3,
        )
        .text
        .unwrap();
        assert!(text.contains("Encryption is off"), "{text}");
        assert!(!text.contains("waiting in the spool"), "{text}");
    }
}
