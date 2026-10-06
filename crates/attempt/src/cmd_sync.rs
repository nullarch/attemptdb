//! `attempt sync` — connect this database to one or more sync servers
//! ("peers"), upload now, show status, disconnect. The daemon uploads on its
//! own once a peer is configured and picks up changes without a restart.

use crate::cli::Cli;
use crate::ctx::Ctx;
use crate::render::print_json;
use anyhow::{Context, Result, anyhow, bail};
use attemptdb_capture::sync;
use attemptdb_capture::sync::{
    Consent, DEFAULT_BATCH_EVENTS, DEFAULT_INFERENCE_INTERVAL_SECS, DEFAULT_INTERVAL_SECS,
    DEFAULT_PEER, PeerConfig, PolicyKey, RevokeOutcome, SyncConfig, SyncProfile, SyncState,
    UploadOptions, UploadReport, describe, entry_matches_seen, forget_remote, is_loopback_host,
    nearest_projects, parse_policy_entry, resolve_url_opts, retry_set_aside, revoke_key,
    seen_projects, upload_all_opts, upload_once_opts, validate_peer_name,
};
use attemptdb_core::event::Provider;
use attemptdb_core::{CaptureMode, Event, EventKind, ProjectRef, Timestamp};
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use std::path::Path;
use std::process::ExitCode;

/// The version of the consent text a `config_changed` event refers to.
const CONSENT_VERSION: &str = "sync-consent-1";

#[derive(Args, Debug)]
pub struct SyncArgs {
    #[command(subcommand)]
    pub cmd: SyncCmd,
}

#[derive(Subcommand, Debug)]
pub enum SyncCmd {
    /// Set peer `default`: server URL (or `vibemon`) and device key; the daemon starts uploading.
    Connect(ConnectArgs),
    /// Add a named peer: another server, or the same one under another profile.
    Add(AddArgs),
    /// One line per configured peer.
    List {
        #[arg(long)]
        json: bool,
    },
    /// End one peer by name: ask the server to revoke its key, then forget it here. Other peers and the local database are untouched.
    Remove {
        name: String,
        #[command(flatten)]
        leave: LeaveArgs,
    },
    /// Upload everything after each peer's cursor now.
    Now {
        /// Only this peer (default: every peer, one after another).
        #[arg(long, value_name = "NAME")]
        peer: Option<String>,
        /// Also compute and upload the inference set now, however recently it was computed (it is otherwise recomputed at most every 10 minutes after new events; see `inference_interval_secs` in sync.json).
        #[arg(long)]
        inferences: bool,
        #[arg(long)]
        json: bool,
    },
    /// Every peer: URL, profile, interval, cursor, last success and last error.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Change what leaves this device for a peer without re-pairing it: metadata_only, semantic, messages or full.
    Profile {
        /// The new profile.
        #[arg(value_name = "PROFILE", value_parser = parse_profile)]
        profile: SyncProfile,
        /// Which peer to change.
        #[arg(long, value_name = "NAME", default_value = DEFAULT_PEER)]
        peer: String,
    },
    /// End a peer (`default` when it is the only one): ask the server to revoke this device's key, forget the peer here. The local database is untouched; what was uploaded stays on the server unless `--forget` deletes it first.
    Disconnect {
        /// Required when more than one peer is configured.
        name: Option<String>,
        #[command(flatten)]
        leave: LeaveArgs,
    },
    /// Delete everything this device uploaded to a peer, on the server (the key stays valid; the local database and cursor are untouched).
    Forget {
        /// Which peer's server to ask.
        #[arg(long, value_name = "NAME", default_value = DEFAULT_PEER)]
        peer: String,
        /// Do it. Without this the command only says what it would delete.
        #[arg(long)]
        yes: bool,
    },
    /// Show or edit which repositories may upload to a peer (RFC 0006 §10.5).
    Policy(PolicyArgs),
    /// Choose whether history from before the connection (and anything imported since) is uploaded.
    History(HistoryArgs),
    /// Deliver the events the server once refused (see `attempt sync status`) a second time, one by one: after the server was upgraded, say.
    RetrySetAside {
        /// Which peer's list to retry.
        #[arg(long, value_name = "NAME", default_value = DEFAULT_PEER)]
        peer: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Debug)]
pub struct HistoryArgs {
    #[command(subcommand)]
    pub cmd: HistoryCmd,
}

#[derive(Subcommand, Debug)]
pub enum HistoryCmd {
    /// Upload what the database held before this peer was connected, and everything imported since: clears the peer's history watermark and sends the cursor back to the start (the server deduplicates what it already has). Needs no key: the stored one is used. Consent is this command.
    Include {
        /// Which peer (default: the only one, else `default`).
        #[arg(long, value_name = "NAME")]
        peer: Option<String>,
    },
}

/// What leaving a peer involves beyond forgetting it locally.
#[derive(Args, Debug, Default)]
pub struct LeaveArgs {
    /// First delete everything this device uploaded to the server (`attempt sync forget`); stops if that fails.
    #[arg(long)]
    pub forget: bool,
    /// Do not contact the server: only forget the peer here. The device key stays valid on the server until its operator revokes it.
    #[arg(long)]
    pub no_revoke: bool,
}

#[derive(Args, Debug)]
pub struct ConnectArgs {
    /// Server base URL, or `vibemon` for https://sync.vibemon.dev
    pub url: String,
    #[command(flatten)]
    pub peer: PeerArgs,
}

#[derive(Args, Debug)]
pub struct AddArgs {
    /// Peer name: letters, digits, `.`, `_`, `-` (at most 32).
    pub name: String,
    /// Server base URL, or `vibemon` for https://sync.vibemon.dev
    pub url: String,
    #[command(flatten)]
    pub peer: PeerArgs,
}

#[derive(Args, Debug)]
pub struct PeerArgs {
    /// Bearer key issued for this device (or use --pair with a one-time pairing token).
    #[arg(long, conflicts_with = "pair")]
    pub key: Option<String>,
    /// One-time pairing token from the product's "Connect device" page: exchanged for a
    /// device key bound to this database's device id, then spent.
    #[arg(long, value_name = "TOKEN")]
    pub pair: Option<String>,
    /// A label for this device on the server (with --pair).
    #[arg(long, value_name = "TEXT")]
    pub label: Option<String>,
    /// What leaves the device: metadata_only, semantic (default: adds inferences with evidence ids and confidence, never prompts or output), messages (adds your prompts and the agent's messages, secret-redacted; commands and tool output stay local), full (adds all content, secret-redacted).
    #[arg(long, value_name = "PROFILE", value_parser = parse_profile)]
    pub profile: Option<SyncProfile>,
    /// Also upload content (prompts, commands, tool output), on top of the profile.
    #[arg(long)]
    pub send_content: bool,
    /// Also upload the conversation (your prompts and the agent's messages), on top of the profile.
    #[arg(long)]
    pub send_messages: bool,
    /// Also upload this device's inferences (attempts, handoffs, work units, decisions), on top of the profile.
    #[arg(long)]
    pub send_inferences: bool,
    /// Seconds between daemon uploads to this peer.
    #[arg(long, default_value_t = DEFAULT_INTERVAL_SECS)]
    pub interval: u64,
    /// Skip the authenticated handshake (an empty batch under the key) that proves the key
    /// works for this device. Without it a wrong key only fails at the first upload.
    #[arg(long)]
    pub no_verify: bool,
    /// Never upload this repository (normalised remote `host/owner/repo` or `prj_…`). Repeatable.
    #[arg(long = "exclude", value_name = "REPO")]
    pub exclude: Vec<String>,
    /// Upload only these repositories. Repeatable; `--exclude` still wins.
    #[arg(long = "include", value_name = "REPO")]
    pub include: Vec<String>,
    /// Also upload events recorded before this connection. By default history from before you connected stays on this device; this is the explicit opt-in to send it.
    #[arg(long)]
    pub include_history: bool,
    /// Allow plain http:// to a host that is not this machine. The key and everything uploaded cross the network unencrypted.
    #[arg(long)]
    pub allow_insecure_http: bool,
}

fn parse_profile(s: &str) -> Result<SyncProfile, String> {
    s.parse().map_err(|e: anyhow::Error| e.to_string())
}

#[derive(Args, Debug)]
pub struct PolicyArgs {
    /// Which peer's policy to show or edit.
    #[arg(long, value_name = "NAME", default_value = DEFAULT_PEER)]
    pub peer: String,
    #[command(subcommand)]
    pub cmd: Option<PolicyCmd>,
}

#[derive(Subcommand, Debug)]
pub enum PolicyCmd {
    /// Never upload this repository (normalised remote or `prj_…` id).
    Exclude { repo: String },
    /// Upload only listed repositories; adds one to the list.
    Include { repo: String },
    /// Remove an entry from both lists.
    Remove { repo: String },
    /// Clear both lists: every repository uploads again.
    Clear,
}

pub fn run(cli: &Cli, args: &SyncArgs) -> Result<ExitCode> {
    let ctx = Ctx::new(cli)?;
    let config_dir = ctx.locator.paths.config_dir.clone();
    match &args.cmd {
        SyncCmd::Connect(a) => {
            let newest = newest_seq_after_spool(&ctx, cli)?;
            add_peer(&ctx.locator, DEFAULT_PEER, &a.url, &a.peer, newest)
        }
        SyncCmd::Add(a) => {
            let newest = newest_seq_after_spool(&ctx, cli)?;
            add_peer(&ctx.locator, &a.name, &a.url, &a.peer, newest)
        }
        SyncCmd::List { json } => {
            let cfg = SyncConfig::load(&config_dir)?.unwrap_or_default();
            if *json {
                let peers: serde_json::Map<String, Value> = cfg
                    .peers
                    .iter()
                    .map(|(n, p)| (n.clone(), peer_json(p)))
                    .collect();
                print_json(&json!({ "connected": !cfg.is_empty(), "peers": peers }));
                return Ok(ExitCode::SUCCESS);
            }
            if cfg.is_empty() {
                println!("not connected");
                return Ok(ExitCode::SUCCESS);
            }
            for (name, p) in &cfg.peers {
                println!(
                    "{name:<12} {:<13} {:>5}s  {}  (key {})",
                    p.profile(),
                    p.interval_secs,
                    p.url,
                    p.masked_key()
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        SyncCmd::Remove { name, leave } => leave_peer(&ctx.locator, name, leave),
        SyncCmd::Profile { profile, peer } => {
            let name = validate_peer_name(peer)?;
            let mut cfg = SyncConfig::load(&config_dir)?.unwrap_or_default();
            let names = cfg.names_list();
            let Some(p) = cfg.peers.get_mut(&name) else {
                bail!("peer `{name}` is not configured (peers: {names})");
            };
            let before = p.profile();
            p.set_profile(*profile);
            let after = p.profile();
            if before != after {
                refresh_consent(p, Timestamp::now());
            }
            let snapshot = p.clone();
            cfg.save(&config_dir)?;
            if before != after {
                record_consent(&ctx.locator, &name, &snapshot, "profile_changed");
            }
            if before == after {
                println!(
                    "peer {name}: profile {after} — {} (unchanged)",
                    after.summary()
                );
            } else {
                println!(
                    "peer {name}: profile {before} → {after} — {}",
                    after.summary()
                );
                if narrows(before, after) {
                    // Narrowing changes what leaves from now on. It does not
                    // reach back: the server keeps what it was already sent.
                    println!(
                        "what was already uploaded under {before} stays on {} — this only stops new uploads from carrying it. `attempt sync forget --peer {name}` deletes it from the server.",
                        snapshot.url
                    );
                }
                println!(
                    "the daemon picks this up on its next tick; `attempt sync now` uploads at once"
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        SyncCmd::Now {
            peer,
            inferences,
            json,
        } => {
            let cfg = load_connected(&config_dir)?;
            // Hooks only spool when no daemon is running. The uploader is
            // read-only, so import pending capture before taking its view.
            // Release the writer before the network request; a live daemon
            // keeps ownership and open() falls back to its read-only view.
            drop(ctx.open(cli)?);
            let source = crate::inferences::source();
            let opts = UploadOptions {
                force_inferences: *inferences,
            };
            let results: Vec<(String, Result<UploadReport>)> = match peer {
                Some(name) => {
                    let name = validate_peer_name(name)?;
                    let p = require_peer(&cfg, &name)?;
                    let r = upload_once_opts(&ctx.locator, &name, p, Some(&source), opts);
                    vec![(name, r)]
                }
                None => upload_all_opts(&ctx.locator, &cfg, Some(&source), opts),
            };
            let failed = results.iter().filter(|(_, r)| r.is_err()).count();
            if *json {
                let peers: serde_json::Map<String, Value> = results
                    .iter()
                    .map(|(n, r)| {
                        let v = match r {
                            Ok(report) => json!({ "ok": true, "report": report }),
                            Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
                        };
                        (n.clone(), v)
                    })
                    .collect();
                print_json(&json!({ "ok": failed == 0, "peers": peers }));
            } else {
                for (name, r) in &results {
                    match r {
                        Ok(report) => {
                            println!("{name}: {}", describe(report));
                            if report.before_consent > 0 {
                                println!(
                                    "{name}: to upload what predates the connection: `{}`",
                                    history_command(name)
                                );
                            }
                        }
                        Err(e) => println!("{name}: error: {e:#}"),
                    }
                }
            }
            Ok(if failed == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        SyncCmd::Status { json } => {
            let cfg = SyncConfig::load(&config_dir)?.unwrap_or_default();
            let mut rows: Vec<(&String, &PeerConfig, SyncState)> = Vec::new();
            for (name, p) in &cfg.peers {
                let (state, _) =
                    SyncState::load_for(&ctx.locator.paths.data_dir, &ctx.locator.db_dir, name)?;
                rows.push((name, p, state));
            }
            if *json {
                let peers: serde_json::Map<String, Value> = rows
                    .iter()
                    .map(|(n, p, s)| {
                        let mut v = peer_json(p);
                        v["state"] = json!(s);
                        ((*n).clone(), v)
                    })
                    .collect();
                print_json(&json!({ "connected": !cfg.is_empty(), "peers": peers }));
                return Ok(ExitCode::SUCCESS);
            }
            if cfg.is_empty() {
                println!("not connected");
                return Ok(ExitCode::SUCCESS);
            }
            for (name, p, state) in &rows {
                println!("peer {name}: {}  (key {})", p.url, p.masked_key());
                println!("  profile     {}  — {}", p.profile(), p.profile().summary());
                println!("  interval    {}s", p.interval_secs);
                if !p.include.is_empty() || !p.exclude.is_empty() {
                    println!(
                        "  policy      {} include, {} exclude  (`attempt sync policy --peer {name}`)",
                        p.include.len(),
                        p.exclude.len()
                    );
                }
                println!(
                    "  cursor      source_seq {}  ({} batch(es), {} event(s), {} duplicate(s), {} rejected)",
                    state.last_acked_source_seq,
                    state.batches,
                    state.events,
                    state.duplicates,
                    state.rejected
                );
                if let Some(c) = &p.consent {
                    println!(
                        "  consent     {} ({}); {}",
                        c.at.to_rfc3339(),
                        c.profile,
                        match c.history_before {
                            Some(t) => format!(
                                "history before {} stays on this device ({} event(s) withheld so far)",
                                t.to_rfc3339(),
                                state.before_consent
                            ),
                            None if c.history_before_seq.is_some() => format!(
                                "history in the database when it was set stays on this device ({} event(s) withheld so far)",
                                state.before_consent
                            ),
                            None => "history before the connection was included".to_string(),
                        }
                    );
                    // Events the next run is going to hold back as well: what
                    // lies between the cursor and the watermark.
                    let waiting = c
                        .history_before_seq
                        .map_or(0, |seq| seq.saturating_sub(state.last_acked_source_seq));
                    if c.has_watermark() && (state.before_consent > 0 || waiting > 0) {
                        println!(
                            "  history     {} event(s) are kept local (from before you connected, or imported since); to upload them: `{}`",
                            state.before_consent + waiting,
                            history_command(name)
                        );
                    }
                }
                if state.quarantined > 0 || !state.quarantine.is_empty() {
                    println!(
                        "  set aside   {} event(s) the server refused ({} kept their metadata); streak {}{}",
                        state.quarantined,
                        state.content_withheld,
                        state.quarantine_streak,
                        if state.set_aside_retried > 0 {
                            format!("; {} delivered by a retry", state.set_aside_retried)
                        } else {
                            String::new()
                        }
                    );
                    for r in state.quarantine.iter().rev().take(5) {
                        println!(
                            "              ev_{} seq {} {} ({}{}): {}",
                            r.event_id,
                            r.source_seq,
                            r.action,
                            r.status,
                            r.server_version
                                .as_deref()
                                .map(|v| format!(", server {v}"))
                                .unwrap_or_default(),
                            r.reason
                        );
                    }
                    if !state.quarantine.is_empty() {
                        println!(
                            "              after the server is upgraded: `attempt sync retry-set-aside --peer {name}` delivers them again"
                        );
                    }
                }
                if state.failures > 0 {
                    println!(
                        "  failing     {} run(s) in a row; the daemon backs off from 5 s up to 15 min",
                        state.failures
                    );
                }
                if let Some(t) = state.last_forget_at {
                    println!(
                        "  forgot      this device's events were deleted from the server at {}",
                        t.to_rfc3339()
                    );
                }
                if let Some(t) = state.last_ok_at {
                    println!("  last ok     {}", t.to_rfc3339());
                }
                if let Some(t) = state.last_inference_at {
                    println!(
                        "  inferences  {} item(s) stored, last {} ({} upload(s))",
                        state.inference_items,
                        t.to_rfc3339(),
                        state.inference_uploads
                    );
                }
                if let Some(e) = &state.last_error {
                    let when = state
                        .last_error_at
                        .map(|t| t.to_rfc3339())
                        .unwrap_or_default();
                    println!("  last err    {when} {e}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        SyncCmd::Policy(p) => {
            let mut cfg = load_connected(&config_dir)?;
            let name = validate_peer_name(&p.peer)?;
            let names = cfg.names_list();
            let Some(peer) = cfg.peers.get_mut(&name) else {
                bail!("peer `{name}` is not configured (peers: {names})");
            };
            // What this command adds, to say afterwards whether it matches
            // anything this device has recorded.
            let mut added: Option<(&str, String)> = None;
            match &p.cmd {
                None => {}
                Some(PolicyCmd::Exclude { repo }) => {
                    let r = canonical_entry(repo)?;
                    if !peer.exclude.contains(&r) {
                        peer.exclude.push(r.clone());
                    }
                    added = Some(("exclude", r));
                }
                Some(PolicyCmd::Include { repo }) => {
                    let r = canonical_entry(repo)?;
                    if !peer.include.contains(&r) {
                        peer.include.push(r.clone());
                    }
                    added = Some(("include", r));
                }
                Some(PolicyCmd::Remove { repo }) => {
                    // The way it was typed, or the way it is stored: either
                    // names the entry, so an old hand-written spelling can
                    // still be removed.
                    let raw = repo.trim().to_string();
                    let canonical = parse_policy_entry(repo).map(|k| k.canonical());
                    let named = |x: &String| {
                        *x == raw
                            || canonical.as_deref() == Some(x.as_str())
                            || parse_policy_entry(x).map(|k| k.canonical()) == canonical
                                && canonical.is_some()
                    };
                    peer.exclude.retain(|x| !named(x));
                    peer.include.retain(|x| !named(x));
                }
                Some(PolicyCmd::Clear) => {
                    peer.exclude.clear();
                    peer.include.clear();
                }
            }
            if p.cmd.is_some() {
                refresh_consent(peer, Timestamp::now());
            }
            let snapshot = peer.clone();
            let (profile, include, exclude) =
                (peer.profile(), peer.include.clone(), peer.exclude.clone());
            if p.cmd.is_some() {
                cfg.save(&config_dir)?;
                record_consent(&ctx.locator, &name, &snapshot, "policy_changed");
            }
            if include.is_empty() && exclude.is_empty() {
                println!("policy (peer {name}, {profile}): every repository uploads");
            } else {
                println!("policy (peer {name}, {profile}):");
                if !include.is_empty() {
                    println!("include (only these upload):");
                    for r in &include {
                        println!("  {r}");
                    }
                }
                if !exclude.is_empty() {
                    println!("exclude (never upload, not even metadata):");
                    for r in &exclude {
                        println!("  {r}");
                    }
                }
                println!(
                    "OpenTelemetry records that cannot be tied to a repository never upload while a policy is set"
                );
            }
            println!("evaluated on this device; excluded projects are unknown to the server");
            if let Some((kind, entry)) = added {
                warn_unmatched(&ctx.locator, &name, kind, std::slice::from_ref(&entry));
            }
            Ok(ExitCode::SUCCESS)
        }
        SyncCmd::Disconnect { name, leave } => {
            if let Some(name) = name {
                return leave_peer(&ctx.locator, name, leave);
            }
            let cfg = SyncConfig::load(&config_dir)?.unwrap_or_default();
            if cfg.is_empty() {
                println!("not connected");
                return Ok(ExitCode::SUCCESS);
            }
            if cfg.peers.len() == 1 && cfg.peers.contains_key(DEFAULT_PEER) {
                return leave_peer(&ctx.locator, DEFAULT_PEER, leave);
            }
            let first = cfg.peers.keys().next().cloned().unwrap_or_default();
            bail!(
                "{} peer(s) configured ({}): name the one to disconnect, e.g. `attempt sync disconnect {first}`",
                cfg.peers.len(),
                cfg.names_list()
            );
        }
        SyncCmd::Forget { peer, yes } => {
            let mut cfg = load_connected(&config_dir)?;
            let name = validate_peer_name(peer)?;
            let p = require_peer(&cfg, &name)?;
            let (state, state_path) =
                SyncState::load_for(&ctx.locator.paths.data_dir, &ctx.locator.db_dir, &name)?;
            if !*yes {
                println!(
                    "this would delete everything this device uploaded to {} (peer {name}): about {} event(s) and the inference documents derived from them",
                    p.url, state.events
                );
                println!(
                    "it cannot be undone. The local database is not touched; from then on everything it holds now stays local and is neither uploaded again nor used to rebuild the inference documents (`{}` is the way back).",
                    history_command(&name)
                );
                bail!("pass --yes to delete");
            }
            // Close the range first: the daemon re-reads sync.json on every
            // tick, so from here on nothing recorded so far is uploaded, nor
            // fed to the inference documents, while the server is deleting it
            // — and not afterwards either. Put back if the server refuses.
            let newest = newest_seq_after_spool(&ctx, cli)?;
            let previous = p.consent.clone();
            let now = Timestamp::now();
            let peer_cfg = cfg.peers.get_mut(&name).expect("checked above");
            close_history(peer_cfg, now, newest);
            let closed = peer_cfg.clone();
            cfg.save(&config_dir)?;
            let report = match forget_remote(&closed) {
                Ok(r) => r,
                Err(e) => {
                    cfg.peers.get_mut(&name).expect("still there").consent = previous;
                    cfg.save(&config_dir)?;
                    return Err(e).with_context(|| {
                        format!("asking {} to delete this device's events", closed.url)
                    });
                }
            };
            let mut state = state.bound_to(&closed.url);
            state.last_forget_at = Some(Timestamp::now());
            // What the inference documents were built from is gone too.
            state.last_inference_digest = None;
            state.save(&state_path)?;
            record_consent(&ctx.locator, &name, &closed, "history_forgotten");
            println!(
                "deleted {} event(s) and {} inference document(s) of this device from {} ({} row(s) of other devices and the server's own remain)",
                report.events_deleted,
                report.inference_documents_removed,
                closed.url,
                report.events_kept
            );
            println!("the server's deletion record notes how many, never what");
            if !report.not_reached.is_empty() {
                println!("not reached by this deletion:");
                for n in &report.not_reached {
                    println!("  - {n}");
                }
            }
            println!(
                "everything this database holds now ({newest} event(s) so far) stays on this device: it is not uploaded again, and no inference document is rebuilt from it. What you record from here on uploads as before. `{}` is the way back.",
                history_command(&name)
            );
            println!("your key is still valid. `attempt sync disconnect` ends the connection.");
            Ok(ExitCode::SUCCESS)
        }
        SyncCmd::History(h) => match &h.cmd {
            HistoryCmd::Include { peer } => {
                let mut cfg = load_connected(&config_dir)?;
                let name = history_peer(&cfg, peer.as_deref())?;
                let p = cfg.peers.get_mut(&name).expect("resolved above");
                if !p.consent.as_ref().is_some_and(Consent::has_watermark) {
                    println!(
                        "peer {name}: nothing is held back; history from before the connection is already included"
                    );
                    return Ok(ExitCode::SUCCESS);
                }
                if let Some(c) = p.consent.as_mut() {
                    c.clear_watermark();
                }
                refresh_consent(p, Timestamp::now());
                let snapshot = p.clone();
                cfg.save(&config_dir)?;
                // The events a watermark kept back sit behind the cursor: it
                // goes back to the start, and the server deduplicates what it
                // already holds. The stored key is used; nothing is asked.
                let (state, state_path) =
                    SyncState::load_for(&ctx.locator.paths.data_dir, &ctx.locator.db_dir, &name)?;
                let withheld = state.before_consent;
                let mut state = state.bound_to(&snapshot.url);
                state.last_acked_source_seq = 0;
                state.last_acked_hlc = 0;
                state.before_consent = 0;
                state.save(&state_path)?;
                record_consent(&ctx.locator, &name, &snapshot, "history_included");
                println!(
                    "peer {name}: history from before the connection (and anything imported since) is now included{}",
                    if withheld > 0 {
                        format!(" — {withheld} event(s) were being held back")
                    } else {
                        String::new()
                    }
                );
                println!(
                    "the cursor goes back to the start, so the next sync uploads everything the policy allows ({}); the server deduplicates what it already has",
                    snapshot.profile().summary()
                );
                println!(
                    "the daemon uploads on its next tick; `attempt sync now --peer {name}` uploads at once. This is consent: it is recorded in the log (config_changed)."
                );
                Ok(ExitCode::SUCCESS)
            }
        },
        SyncCmd::RetrySetAside { peer, json } => {
            let cfg = load_connected(&config_dir)?;
            let name = validate_peer_name(peer)?;
            let p = require_peer(&cfg, &name)?;
            let report = retry_set_aside(&ctx.locator, &name, p)
                .with_context(|| format!("retrying the events {} refused", p.url))?;
            if *json {
                print_json(&json!({ "peer": name, "report": report }));
            } else if report.tried == 0 {
                println!("peer {name}: no event was set aside");
            } else {
                println!(
                    "peer {name}: {} set-aside event(s) tried: {} delivered, {} refused again, {} no longer deliverable (gone from the database, or no longer allowed by the policy)",
                    report.tried, report.delivered, report.refused_again, report.gone
                );
                if report.refused_again > 0 {
                    println!(
                        "the refused ones stay listed in `attempt sync status` with the server's new answer"
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// The configuration, or the "not connected" error every subcommand that
/// needs a peer prints.
fn load_connected(config_dir: &Path) -> Result<SyncConfig> {
    match SyncConfig::load(config_dir)? {
        Some(cfg) if !cfg.is_empty() => Ok(cfg),
        _ => bail!("not connected: run `attempt sync connect <url> --key <key>` first"),
    }
}

fn require_peer<'a>(cfg: &'a SyncConfig, name: &str) -> Result<&'a PeerConfig> {
    cfg.get(name).ok_or_else(|| {
        anyhow!(
            "peer `{name}` is not configured (peers: {})",
            cfg.names_list()
        )
    })
}

/// A policy entry in the one stored spelling, or an error that says what an
/// entry may be. An entry that names nothing would promise an exclusion it
/// does not perform, so it is refused here, not stored.
fn canonical_entry(entry: &str) -> Result<String> {
    match parse_policy_entry(entry) {
        Some(k) => Ok(k.canonical()),
        None => bail!(
            "`{}` is neither a project id (prj_…) nor a git remote: use host/owner/repo, \
             https://host/owner/repo.git or git@host:owner/repo (the remote of the repository, as \
             `git remote get-url origin` prints it)",
            entry.trim()
        ),
    }
}

/// Normalise a list of entries for storage, saying what each became.
fn canonical_entries(kind: &str, entries: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for e in entries {
        let c = canonical_entry(e).with_context(|| format!("--{kind} {e}"))?;
        if c != e.trim() {
            println!("{kind}: {} → {c}", e.trim());
        }
        if !out.contains(&c) {
            out.push(c);
        }
    }
    Ok(out)
}

/// Set the consent marker's moving parts to what is in force now. The
/// history watermark is never moved: widening what leaves does not make
/// older history uploadable.
fn refresh_consent(peer: &mut PeerConfig, at: Timestamp) {
    let history_before = peer.consent.as_ref().and_then(|c| c.history_before);
    let history_before_seq = peer.consent.as_ref().and_then(|c| c.history_before_seq);
    peer.consent = Some(Consent {
        at,
        profile: peer.profile(),
        include: peer.include.clone(),
        exclude: peer.exclude.clone(),
        history_before,
        history_before_seq,
    });
}

/// Keep everything the database holds (`seq` is its newest `source_seq`) on
/// this device from now on: the watermark moves to now, creating the consent
/// for a peer that was configured before consent was recorded.
fn close_history(peer: &mut PeerConfig, at: Timestamp, seq: u64) {
    let (profile, include, exclude) = (peer.profile(), peer.include.clone(), peer.exclude.clone());
    peer.consent
        .get_or_insert(Consent {
            at,
            profile,
            include,
            exclude,
            history_before: None,
            history_before_seq: None,
        })
        .advance_watermark(at, seq);
}

/// The exact command that uploads what a peer's watermark keeps local.
pub fn history_command(peer: &str) -> String {
    format!("attempt sync history include --peer {peer}")
}

/// The database's newest `source_seq` once the hooks' spool is imported (0
/// when there is no database yet): where a history watermark is set. An event
/// the hooks spooled in the last second while a daemon owned the writer lock
/// is imported by the daemon a moment later, after this reading.
fn newest_seq_after_spool(ctx: &Ctx, cli: &Cli) -> Result<u64> {
    if !attemptdb_storage::Database::exists(&ctx.locator.db_dir) {
        return Ok(0);
    }
    let opened = ctx.open(cli)?;
    Ok(opened.db.stats().last_source_seq)
}

/// The peer `history include` means: the one named, else the only one, else
/// `default`.
fn history_peer(cfg: &SyncConfig, named: Option<&str>) -> Result<String> {
    if let Some(n) = named {
        let n = validate_peer_name(n)?;
        require_peer(cfg, &n)?;
        return Ok(n);
    }
    if cfg.peers.len() == 1 {
        return Ok(cfg.peers.keys().next().cloned().unwrap_or_default());
    }
    if cfg.peers.contains_key(DEFAULT_PEER) {
        return Ok(DEFAULT_PEER.to_string());
    }
    bail!(
        "{} peers configured ({}): say which one, e.g. `attempt sync history include --peer {}`",
        cfg.peers.len(),
        cfg.names_list(),
        cfg.peers.keys().next().cloned().unwrap_or_default()
    )
}

/// Whether going from `before` to `after` stops something leaving.
fn narrows(before: SyncProfile, after: SyncProfile) -> bool {
    let (bc, bi, bm) = before.flags();
    let (ac, ai, am) = after.flags();
    (bc && !ac) || (bi && !ai) || (bm && !am)
}

/// After an import: say so when a connected peer is going to keep what was
/// just read in local, because it predates the connection, and give the one
/// command that changes that. `imported` is the number of events stored or
/// queued.
pub fn print_import_notice(locator: &attemptdb_capture::locator::Locator, imported: usize) {
    if imported == 0 {
        return;
    }
    let Ok(Some(cfg)) = SyncConfig::load(&locator.paths.config_dir) else {
        return;
    };
    for (name, p) in &cfg.peers {
        let Some(c) = p.consent.as_ref().filter(|c| c.has_watermark()) else {
            continue;
        };
        let since = c
            .history_before
            .map(|t| format!(" (the connection dates from {})", t.to_rfc3339()))
            .unwrap_or_default();
        println!();
        println!(
            "sync: peer {name} keeps history from before you connected on this device{since}; what was just imported is history, and stays local unless you say otherwise."
        );
        println!("      to upload it: `{}`", history_command(name));
    }
}

/// A policy entry that names no project this device has recorded does
/// nothing yet: an `exclude` leaves everything uploading, an `include` leaves
/// nothing. Say so, with the nearest recorded projects: an ssh alias
/// (`git@github-work:acme/private.git`) is a different spelling of the same
/// repository that cannot be resolved without the user's ssh configuration.
fn warn_unmatched(
    locator: &attemptdb_capture::locator::Locator,
    peer: &str,
    kind: &str,
    entries: &[String],
) {
    if entries.is_empty() {
        return;
    }
    let seen = match seen_projects(locator) {
        Ok(s) => s,
        Err(e) => {
            println!("(could not read this device's projects to check the {kind} entry: {e:#})");
            return;
        }
    };
    for entry in entries {
        let Some(key) = parse_policy_entry(entry) else {
            continue;
        };
        if entry_matches_seen(&key, &seen) {
            continue;
        }
        let what = match kind {
            "exclude" => "it excludes nothing yet",
            _ => "nothing uploads under it until such a project is recorded",
        };
        if seen.is_empty() {
            println!(
                "note: {kind} `{entry}`: this device has not recorded any project yet, so {what}"
            );
            continue;
        }
        println!("warning: {kind} `{entry}` matches no project this device has recorded; {what}.");
        let near = nearest_projects(&key, &seen);
        if near.is_empty() {
            println!(
                "         (`attempt query \"SELECT DISTINCT repo_remote FROM events\"` lists the remotes it has seen)"
            );
        } else {
            println!("         nearest recorded:");
            for p in &near {
                if let Some(r) = &p.remote {
                    println!("           {r}  ({} event(s))", p.events);
                }
            }
            if let (PolicyKey::Remote(_), Some(first)) =
                (&key, near.first().and_then(|p| p.remote.as_ref()))
            {
                println!(
                    "         if one of them is the repository you mean — an ssh alias in front of a different host name, say — write the entry with that spelling: `attempt sync policy --peer {peer} {kind} {first}`"
                );
            }
        }
    }
}

/// Write the `config_changed` event that records a consent (RFC 0006 §2):
/// the peer's name, profile and how many repositories the policy names —
/// counts only, never a repository name. Best effort: a database that does
/// not exist yet, or one that cannot be written now, is said, not fatal.
/// Returns whether the event was written.
fn record_consent(
    locator: &attemptdb_capture::locator::Locator,
    peer: &str,
    cfg: &PeerConfig,
    why: &str,
) -> bool {
    let Some(at) = cfg.consent.as_ref().map(|c| c.at) else {
        return false;
    };
    if !attemptdb_storage::Database::exists(&locator.db_dir) {
        println!(
            "consent not logged: there is no database yet ({} recorded in sync.json)",
            at.to_rfc3339()
        );
        return false;
    }
    let device = match attemptdb_capture::ingest::open_reader(locator) {
        Ok(db) => db.device_id(),
        Err(e) => {
            println!("consent not logged: cannot open the database ({e:#})");
            return false;
        }
    };
    let mut ev = Event::new(
        device,
        Provider::Other("attemptdb".into()),
        "SyncConsent",
        EventKind::ConfigChanged,
        ProjectRef::derive("attemptdb/sync", None, &device),
        "attemptdb-sync-consent",
        CaptureMode::MetadataOnly,
        env!("CARGO_PKG_VERSION"),
    );
    ev.observed_at = at;
    ev.captured_at = at;
    let c = cfg.consent.as_ref().expect("checked above");
    ev.attrs
        .insert("consent_version".into(), json!(CONSENT_VERSION));
    ev.attrs.insert("x_attemptdb_sync_peer".into(), json!(peer));
    ev.attrs
        .insert("x_attemptdb_sync_profile".into(), json!(c.profile.as_str()));
    ev.attrs.insert(
        "x_attemptdb_sync_include_count".into(),
        json!(c.include.len()),
    );
    ev.attrs.insert(
        "x_attemptdb_sync_exclude_count".into(),
        json!(c.exclude.len()),
    );
    ev.attrs.insert(
        "x_attemptdb_sync_history".into(),
        json!(if c.history_before.is_some() {
            "after_consent"
        } else {
            "included"
        }),
    );
    ev.attrs
        .insert("x_attemptdb_sync_change".into(), json!(why));
    match attemptdb_capture::ingest::write_events(locator, vec![ev]) {
        Ok(_) => true,
        Err(e) => {
            println!("consent not logged: {e:#}");
            false
        }
    }
}

/// `connect` (peer `default`) and `add <name>` share everything but the name.
fn add_peer(
    locator: &attemptdb_capture::locator::Locator,
    name: &str,
    url_input: &str,
    a: &PeerArgs,
    newest_seq: u64,
) -> Result<ExitCode> {
    let config_dir: &Path = &locator.paths.config_dir;
    let name = validate_peer_name(name)?;
    let url = resolve_url_opts(url_input, a.allow_insecure_http)?;
    let insecure = a.allow_insecure_http
        && url.starts_with("http://")
        && !is_loopback_host(
            url.trim_start_matches("http://")
                .split(['/', ':'])
                .next()
                .unwrap_or(""),
        );
    // The repository policy, in its stored spelling, before anything is
    // spent or changed: an entry that names nothing stops the command.
    let include = canonical_entries("include", &a.include)?;
    let exclude = canonical_entries("exclude", &a.exclude)?;
    // The key: given, or obtained now by spending a pairing token. The
    // token is checked first so a bad one fails before anything changes.
    let mut paired_note: Option<String> = None;
    let key = match (a.key.as_deref(), a.pair.as_deref()) {
        (Some(k), _) if !k.trim().is_empty() => k.trim().to_string(),
        (_, Some(token)) if !token.trim().is_empty() => {
            let tenant = sync::check_pairing(&url, token)
                .with_context(|| format!("checking the pairing token with {url}"))?;
            let paired = sync::pair(locator, &url, token, a.label.as_deref())
                .with_context(|| format!("pairing this device with {url}"))?;
            paired_note = Some(format!(
                "paired: tenant {}{}, device dev_{} (label {:?}); the token is spent",
                if tenant.is_empty() {
                    paired.tenant.clone()
                } else {
                    tenant
                },
                paired
                    .user_id
                    .as_deref()
                    .map(|u| format!(" as {u}"))
                    .unwrap_or_default(),
                paired.device_id,
                paired.label
            ));
            paired.key
        }
        _ => bail!("give --key <device key> or --pair <pairing token>"),
    };
    let (send_content, send_inferences, send_messages) = SyncProfile::resolve(
        a.profile,
        a.send_content,
        a.send_inferences,
        a.send_messages,
    );
    let mut cfg = SyncConfig::load(config_dir)?.unwrap_or_default();
    let now = Timestamp::now();
    // History before the connection is not agreed to. A peer that already
    // existed against this same server keeps the watermark it had (a re-pair
    // must not lose the events recorded while it was offline, nor withhold
    // ones a connected peer would have sent); a new peer, or one pointed at a
    // different server, starts it now unless `--include-history` says not to.
    // The watermark is the time (for display, and for imports) and the
    // database's newest `source_seq` (what decides for everything captured
    // live: a sequence does not care what the clock says).
    let (history_before, history_before_seq) = if a.include_history {
        (None, None)
    } else {
        match cfg.peers.get(&name) {
            Some(prev) if prev.url == url => prev
                .consent
                .as_ref()
                .map_or((None, None), |c| (c.history_before, c.history_before_seq)),
            _ => (Some(now), Some(newest_seq)),
        }
    };
    let peer = PeerConfig {
        url: url.clone(),
        key,
        send_content,
        send_inferences,
        send_messages,
        batch_events: DEFAULT_BATCH_EVENTS,
        interval_secs: a.interval,
        inference_interval_secs: cfg
            .peers
            .get(&name)
            .map_or(DEFAULT_INFERENCE_INTERVAL_SECS, |prev| {
                prev.inference_interval_secs
            }),
        include,
        exclude,
        allow_insecure_http: insecure,
        consent: None,
    };
    let mut peer = peer;
    peer.consent = Some(Consent {
        at: now,
        profile: peer.profile(),
        include: peer.include.clone(),
        exclude: peer.exclude.clone(),
        history_before,
        history_before_seq,
    });
    // Save first: a key obtained by pairing exists nowhere else. Then prove
    // it works for this device, and undo the save if it does not.
    let previous = cfg.peers.insert(name.clone(), peer.clone());
    let replaced = previous.is_some();
    cfg.save(config_dir)?;
    if let Some(n) = &paired_note {
        println!("{n}");
    }
    if !a.no_verify {
        match sync::handshake(locator, &peer) {
            Ok(h) => println!(
                "authenticated: {} accepts this device (dev_{})",
                h.url, h.device_id
            ),
            Err(e) => {
                // Put the previous connection back (or none) so a wrong key
                // never sits in the config as if it worked.
                match previous {
                    Some(p) => {
                        cfg.peers.insert(name.clone(), p);
                    }
                    None => {
                        cfg.peers.remove(&name);
                    }
                }
                cfg.save(config_dir)?;
                return Err(e.context(
                    "the key was not saved; fix the cause and connect again (or --no-verify to keep it anyway)",
                ));
            }
        }
    }
    if url_input.trim() != url {
        println!("{} → {url}", url_input.trim());
    }
    println!(
        "{}: {url}  (peer {name})",
        if replaced { "updated" } else { "connected" }
    );
    println!(
        "  key         {}\n  profile     {}  — {}\n  interval    {}s\n  config      {}",
        peer.masked_key(),
        peer.profile(),
        peer.profile().summary(),
        peer.interval_secs,
        SyncConfig::path(config_dir).display()
    );
    if peer.sends_any_content() {
        println!(
            "  note        the text this uploads is stored by the server as it receives it (not end-to-end encrypted)\n              and may be forwarded to the product that runs it; `attempt sync forget` deletes it from the server later"
        );
    }
    if insecure {
        eprintln!(
            "WARNING: {url} is plain http. The key and everything uploaded cross the network unencrypted; anyone on the path can read or alter it."
        );
    }
    match peer.consent.as_ref().and_then(|c| c.history_before) {
        Some(t) => println!(
            "  history     events recorded before {} stay on this device; `{}` uploads them (and anything you import later)",
            t.to_rfc3339(),
            history_command(&name)
        ),
        None => {
            println!("  history     everything already recorded is included in the first upload")
        }
    }
    if !peer.include.is_empty() || !peer.exclude.is_empty() {
        println!(
            "  policy      {} include, {} exclude; OpenTelemetry records that cannot be tied to a repository never upload under a policy",
            peer.include.len(),
            peer.exclude.len()
        );
    }
    warn_unmatched(locator, &name, "exclude", &peer.exclude);
    warn_unmatched(locator, &name, "include", &peer.include);
    if cfg.peers.len() > 1 {
        println!("  peers       {}", cfg.names_list());
    }
    // A cursor belongs to the server it was advanced against.
    let (state, state_path) = SyncState::load_for(&locator.paths.data_dir, &locator.db_dir, &name)?;
    if let Some(prev) = state.url.as_deref()
        && prev != url
        && state.last_acked_source_seq > 0
    {
        println!(
            "  cursor      restarts from 0: peer {name} had uploaded {} event(s) to {prev}; the new server deduplicates anything it already holds",
            state.events
        );
    }
    if a.include_history
        && replaced
        && state.last_acked_source_seq > 0
        && state.url.as_deref() == Some(url.as_str())
    {
        // History that an earlier watermark kept back sits behind the cursor;
        // to send it the cursor goes back to the start (the server dedupes).
        let mut reset = state.clone();
        reset.last_acked_source_seq = 0;
        reset.last_acked_hlc = 0;
        reset.save(&state_path)?;
        println!(
            "  cursor      reset to 0 so the history is uploaded; the server deduplicates what it holds"
        );
    }
    // The consent is logged once the connection is proven: an audit event
    // for a connection that was never made would be false.
    if record_consent(
        locator,
        &name,
        &peer,
        if replaced { "reconnected" } else { "connected" },
    ) {
        println!("  consent     recorded in the log (config_changed)");
    }
    println!(
        "the daemon uploads on that interval (no restart needed); `attempt sync now` uploads immediately"
    );
    Ok(ExitCode::SUCCESS)
}

/// Leave a peer: optionally have the server delete this device's events,
/// ask it to revoke the key, then forget the peer here — and say plainly
/// what that does and does not remove.
fn leave_peer(
    locator: &attemptdb_capture::locator::Locator,
    name: &str,
    leave: &LeaveArgs,
) -> Result<ExitCode> {
    let config_dir = &locator.paths.config_dir;
    let name = validate_peer_name(name)?;
    let mut cfg = SyncConfig::load(config_dir)?.unwrap_or_default();
    let Some(peer) = cfg.peers.get(&name).cloned() else {
        if cfg.is_empty() {
            println!("not connected");
            return Ok(ExitCode::SUCCESS);
        }
        bail!(
            "peer `{name}` is not configured (peers: {})",
            cfg.names_list()
        );
    };
    let (state, state_path) = SyncState::load_for(&locator.paths.data_dir, &locator.db_dir, &name)?;
    let uploaded = state.events;
    let mut forgotten = false;
    if leave.forget {
        // Forgetting needs the key, so it runs before the revoke that ends it.
        let report = forget_remote(&peer).with_context(|| {
            format!(
                "deleting this device's events from {} failed; the peer was NOT removed — fix the cause, or drop --forget",
                peer.url
            )
        })?;
        forgotten = true;
        let mut st = state.clone().bound_to(&peer.url);
        st.last_forget_at = Some(Timestamp::now());
        st.save(&state_path)?;
        println!(
            "deleted {} event(s) of this device from {}",
            report.events_deleted, peer.url
        );
    }
    let mut revoked = false;
    if leave.no_revoke {
        println!(
            "key NOT revoked (--no-revoke): it stays valid on {} until its operator revokes it",
            peer.url
        );
    } else {
        match revoke_key(&peer) {
            RevokeOutcome::Revoked => {
                revoked = true;
                println!("key revoked on {}", peer.url);
            }
            RevokeOutcome::AlreadyGone => {
                revoked = true;
                println!("{} no longer knows this key (already revoked)", peer.url);
            }
            RevokeOutcome::Unsupported => println!(
                "{} runs an older server with no revoke route: the key stays valid there until its operator revokes it",
                peer.url
            ),
            RevokeOutcome::Unreachable(why) => println!(
                "could not reach the server to revoke the key ({why}): it stays valid there until its operator revokes it, or you run this again with the server reachable"
            ),
            RevokeOutcome::Refused(status, why) => println!(
                "the server would not revoke the key ({status}: {why}): it stays valid there until its operator revokes it"
            ),
        }
    }
    cfg.peers.remove(&name);
    cfg.save(config_dir)?;
    if cfg.is_empty() {
        println!("disconnected; the local database is untouched");
    } else {
        println!("removed peer {name}; remaining: {}", cfg.names_list());
    }
    // What this did and did not do, in plain words.
    if forgotten {
        println!(
            "the server no longer holds this device's events; what the product already received through its webhook, and backups of the server's disk, are not reachable from here"
        );
    } else if uploaded > 0 {
        println!(
            "still on {}: the {uploaded} event(s) this device uploaded. Disconnecting stops uploads and (above) revokes the key; it does not delete data — `attempt sync disconnect --forget` does both, or ask the operator",
            peer.url
        );
    }
    if !revoked && !leave.no_revoke {
        println!("(the key could not be confirmed revoked; see above)");
    }
    Ok(ExitCode::SUCCESS)
}

/// A peer for `--json` output: the key masked, the profile named.
fn peer_json(p: &PeerConfig) -> Value {
    json!({
        "url": p.url,
        "key": p.masked_key(),
        "profile": p.profile(),
        "send_content": p.send_content,
        "send_inferences": p.send_inferences,
        "send_messages": p.send_messages,
        "interval_secs": p.interval_secs,
        "inference_interval_secs": p.inference_interval_secs,
        "include": p.include,
        "exclude": p.exclude,
        "allow_insecure_http": p.allow_insecure_http,
        "consent": p.consent,
    })
}
