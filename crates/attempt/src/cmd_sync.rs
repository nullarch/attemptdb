//! `attempt sync` — connect this database to one or more sync servers
//! ("peers"), upload now, show status, disconnect. The daemon uploads on its
//! own once a peer is configured and picks up changes without a restart.

use crate::cli::Cli;
use crate::ctx::Ctx;
use crate::render::print_json;
use anyhow::{Context, Result, anyhow, bail};
use attemptdb_capture::sync::{
    DEFAULT_BATCH_EVENTS, DEFAULT_INTERVAL_SECS, DEFAULT_PEER, PeerConfig, SyncConfig, SyncProfile,
    SyncState, UploadReport, describe, resolve_url, upload_all, upload_once_with,
    validate_peer_name,
};
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use std::path::Path;
use std::process::ExitCode;

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
    /// Forget one peer by name. Other peers and the local database are untouched.
    Remove { name: String },
    /// Upload everything after each peer's cursor now.
    Now {
        /// Only this peer (default: every peer, one after another).
        #[arg(long, value_name = "NAME")]
        peer: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Every peer: URL, profile, interval, cursor, last success and last error.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Forget a peer (`default` when it is the only one). The local database is untouched.
    Disconnect {
        /// Required when more than one peer is configured.
        name: Option<String>,
    },
    /// Show or edit which repositories may upload to a peer (RFC 0006 §10.5).
    Policy(PolicyArgs),
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
    /// Bearer key issued for this device.
    #[arg(long)]
    pub key: String,
    /// What leaves the device: metadata_only (default), semantic (adds inferences with evidence ids and confidence), full (adds content, secret-redacted).
    #[arg(long, value_name = "PROFILE", value_parser = parse_profile)]
    pub profile: Option<SyncProfile>,
    /// Also upload content (prompts, commands, tool output), on top of the profile.
    #[arg(long)]
    pub send_content: bool,
    /// Also upload this device's inferences (attempts, handoffs, work units, decisions), on top of the profile.
    #[arg(long)]
    pub send_inferences: bool,
    /// Seconds between daemon uploads to this peer.
    #[arg(long, default_value_t = DEFAULT_INTERVAL_SECS)]
    pub interval: u64,
    /// Skip the connectivity check.
    #[arg(long)]
    pub no_verify: bool,
    /// Never upload this repository (normalised remote `host/owner/repo` or `prj_…`). Repeatable.
    #[arg(long = "exclude", value_name = "REPO")]
    pub exclude: Vec<String>,
    /// Upload only these repositories. Repeatable; `--exclude` still wins.
    #[arg(long = "include", value_name = "REPO")]
    pub include: Vec<String>,
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
        SyncCmd::Connect(a) => add_peer(&ctx.locator, DEFAULT_PEER, &a.url, &a.peer),
        SyncCmd::Add(a) => add_peer(&ctx.locator, &a.name, &a.url, &a.peer),
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
        SyncCmd::Remove { name } => remove_peer(&config_dir, name),
        SyncCmd::Now { peer, json } => {
            let cfg = load_connected(&config_dir)?;
            let source = crate::inferences::source();
            let results: Vec<(String, Result<UploadReport>)> = match peer {
                Some(name) => {
                    let name = validate_peer_name(name)?;
                    let p = require_peer(&cfg, &name)?;
                    let r = upload_once_with(&ctx.locator, &name, p, Some(&source));
                    vec![(name, r)]
                }
                None => upload_all(&ctx.locator, &cfg, Some(&source)),
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
                        Ok(report) => println!("{name}: {}", describe(report)),
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
            match &p.cmd {
                None => {}
                Some(PolicyCmd::Exclude { repo }) => {
                    let r = repo.trim().to_string();
                    if !peer.exclude.contains(&r) {
                        peer.exclude.push(r);
                    }
                }
                Some(PolicyCmd::Include { repo }) => {
                    let r = repo.trim().to_string();
                    if !peer.include.contains(&r) {
                        peer.include.push(r);
                    }
                }
                Some(PolicyCmd::Remove { repo }) => {
                    let r = repo.trim();
                    peer.exclude.retain(|x| x != r);
                    peer.include.retain(|x| x != r);
                }
                Some(PolicyCmd::Clear) => {
                    peer.exclude.clear();
                    peer.include.clear();
                }
            }
            let (profile, include, exclude) =
                (peer.profile(), peer.include.clone(), peer.exclude.clone());
            if p.cmd.is_some() {
                cfg.save(&config_dir)?;
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
            }
            println!("evaluated on this device; excluded projects are unknown to the server");
            Ok(ExitCode::SUCCESS)
        }
        SyncCmd::Disconnect { name } => {
            if let Some(name) = name {
                return remove_peer(&config_dir, name);
            }
            let cfg = SyncConfig::load(&config_dir)?.unwrap_or_default();
            if cfg.is_empty() {
                println!("not connected");
                return Ok(ExitCode::SUCCESS);
            }
            if cfg.peers.len() == 1 && cfg.peers.contains_key(DEFAULT_PEER) {
                return remove_peer(&config_dir, DEFAULT_PEER);
            }
            let first = cfg.peers.keys().next().cloned().unwrap_or_default();
            bail!(
                "{} peer(s) configured ({}): name the one to disconnect, e.g. `attempt sync disconnect {first}`",
                cfg.peers.len(),
                cfg.names_list()
            );
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

/// `connect` (peer `default`) and `add <name>` share everything but the name.
fn add_peer(
    locator: &attemptdb_capture::locator::Locator,
    name: &str,
    url_input: &str,
    a: &PeerArgs,
) -> Result<ExitCode> {
    let config_dir: &Path = &locator.paths.config_dir;
    let name = validate_peer_name(name)?;
    let url = resolve_url(url_input)?;
    if a.key.trim().is_empty() {
        bail!("--key is empty");
    }
    let (send_content, send_inferences) =
        SyncProfile::resolve(a.profile, a.send_content, a.send_inferences);
    let peer = PeerConfig {
        url: url.clone(),
        key: a.key.trim().to_string(),
        send_content,
        send_inferences,
        batch_events: DEFAULT_BATCH_EVENTS,
        interval_secs: a.interval,
        include: a.include.iter().map(|s| s.trim().to_string()).collect(),
        exclude: a.exclude.iter().map(|s| s.trim().to_string()).collect(),
    };
    if !a.no_verify {
        let health = format!("{url}/v1/health");
        ureq::get(&health)
            .timeout(std::time::Duration::from_secs(10))
            .call()
            .with_context(|| format!("checking {health} (use --no-verify to skip)"))?;
    }
    let mut cfg = SyncConfig::load(config_dir)?.unwrap_or_default();
    let replaced = cfg.peers.insert(name.clone(), peer.clone()).is_some();
    cfg.save(config_dir)?;
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
    if cfg.peers.len() > 1 {
        println!("  peers       {}", cfg.names_list());
    }
    // A cursor belongs to the server it was advanced against.
    let (state, _) = SyncState::load_for(&locator.paths.data_dir, &locator.db_dir, &name)?;
    if let Some(prev) = state.url.as_deref()
        && prev != url
        && state.last_acked_source_seq > 0
    {
        println!(
            "  cursor      restarts from 0: peer {name} had uploaded {} event(s) to {prev}; the new server deduplicates anything it already holds",
            state.events
        );
    }
    println!(
        "the daemon uploads on that interval (no restart needed); `attempt sync now` uploads immediately"
    );
    Ok(ExitCode::SUCCESS)
}

fn remove_peer(config_dir: &Path, name: &str) -> Result<ExitCode> {
    let name = validate_peer_name(name)?;
    let mut cfg = SyncConfig::load(config_dir)?.unwrap_or_default();
    if cfg.peers.remove(&name).is_none() {
        if cfg.is_empty() {
            println!("not connected");
            return Ok(ExitCode::SUCCESS);
        }
        bail!(
            "peer `{name}` is not configured (peers: {})",
            cfg.names_list()
        );
    }
    cfg.save(config_dir)?;
    if cfg.is_empty() {
        println!("disconnected; the local database is untouched");
    } else {
        println!("removed peer {name}; remaining: {}", cfg.names_list());
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
        "interval_secs": p.interval_secs,
        "include": p.include,
        "exclude": p.exclude,
    })
}
