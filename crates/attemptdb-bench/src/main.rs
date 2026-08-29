//! `attemptdb-bench`: the AttemptDB workload benchmark program.
//!
//! ```text
//! attemptdb-bench run --events 1450000 --out /path/to/work --json
//! attemptdb-bench step ingest --events 100000 --out /path/to/work --relaxed
//! attemptdb-bench report /path/to/work/results.json
//! attemptdb-bench sample --events 20
//! ```
//!
//! `run` orchestrates every benchmark; each one executes in a child process
//! (`step`) so that peak RSS is attributable and a runaway step can be
//! killed at a memory or time cap without losing the results gathered so
//! far. `results.json` is rewritten after every step.

// `unsafe` is confined to two libc calls: `getrusage` (stats.rs) and the
// raw `fsync` / `F_FULLFSYNC` floor measurement (steps/wal.rs).

mod model;
mod report;
mod rng;
mod stats;
mod steps;
mod sysinfo;
mod text;
mod workload;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use steps::StepCtx;

#[derive(Parser, Debug)]
#[command(
    name = "attemptdb-bench",
    version,
    about = "AttemptDB workload benchmarks"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run the whole benchmark program and write `results.json` under --out.
    Run(RunArgs),
    /// Run one benchmark step in this process and print its JSON result.
    Step(StepArgs),
    /// Render Markdown tables from a results file.
    Report { results: PathBuf },
    /// Print synthetic events as JSON lines (inspect the workload).
    Sample {
        #[arg(long, default_value_t = 20)]
        events: u64,
        #[arg(long, default_value_t = DEFAULT_SEED)]
        seed: u64,
    },
}

const DEFAULT_SEED: u64 = 20_260_829;

#[derive(Args, Debug, Clone)]
struct CommonArgs {
    /// Working directory for datasets, scratch files, and results.
    #[arg(long, value_name = "DIR")]
    out: PathBuf,
    /// Target event count.
    #[arg(long, default_value_t = 1_450_000)]
    events: u64,
    #[arg(long, default_value_t = DEFAULT_SEED)]
    seed: u64,
    /// Soft time cap per step in seconds (loops stop early and report it).
    #[arg(long, default_value_t = 600)]
    time_cap_secs: u64,
    /// Path of the `attempt` binary for the hook and daemon benchmarks
    /// (default: `attempt` next to this binary, else `~/.cargo/bin/attempt`).
    #[arg(long, value_name = "PATH")]
    attempt_bin: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct RunArgs {
    #[command(flatten)]
    common: CommonArgs,
    /// Print the results JSON to stdout at the end.
    #[arg(long)]
    json: bool,
    /// Kill a step whose resident set exceeds this many GiB.
    #[arg(long, default_value_t = 36.0)]
    rss_cap_gb: f64,
    /// Only run steps whose label contains one of these (comma separated).
    #[arg(long, value_delimiter = ',')]
    only: Vec<String>,
    /// Skip steps whose label contains one of these (comma separated).
    #[arg(long, value_delimiter = ',')]
    skip: Vec<String>,
}

#[derive(Args, Debug)]
struct StepArgs {
    /// Step name (see `steps::STEP_NAMES`).
    name: String,
    #[command(flatten)]
    common: CommonArgs,
    /// Database directory for query steps.
    #[arg(long, value_name = "DIR")]
    db: Option<PathBuf>,
    /// Relaxed durability for ingest steps.
    #[arg(long)]
    relaxed: bool,
    /// Run a concurrent reader thread during ingest.
    #[arg(long)]
    reader: bool,
    /// Keep the ingested database.
    #[arg(long)]
    keep: bool,
    /// Memtable flush threshold override.
    #[arg(long)]
    flush_events: Option<usize>,
    /// Projection mode: materialized or streaming.
    #[arg(long, default_value = "materialized")]
    mode: String,
}

fn default_attempt_bin() -> Option<PathBuf> {
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("attempt")))
        .filter(|p| p.exists());
    sibling.or_else(|| {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".cargo/bin/attempt"))
            .filter(|p| p.exists())
    })
}

/// Make a path absolute so child processes started in other working
/// directories (the hook runs inside the fake project) resolve it the same
/// way.
fn absolute(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    }
}

fn step_ctx(common: &CommonArgs) -> StepCtx {
    StepCtx {
        out: absolute(&common.out),
        events: common.events,
        seed: common.seed,
        time_cap: Duration::from_secs(common.time_cap_secs),
        attempt_bin: common.attempt_bin.clone().or_else(default_attempt_bin),
        db: None,
        relaxed: false,
        reader: false,
        keep: false,
        flush_events: None,
        mode: "materialized".into(),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Run(args) => run(args),
        Cmd::Step(args) => {
            let mut ctx = step_ctx(&args.common);
            ctx.db = args.db.as_deref().map(absolute);
            ctx.relaxed = args.relaxed;
            ctx.reader = args.reader;
            ctx.keep = args.keep;
            ctx.flush_events = args.flush_events;
            ctx.mode = args.mode;
            std::fs::create_dir_all(&ctx.out)?;
            let v = steps::run_step(&args.name, &ctx)?;
            println!("{}", serde_json::to_string(&v)?);
            Ok(())
        }
        Cmd::Report { results } => {
            let text = std::fs::read_to_string(&results)
                .with_context(|| format!("reading {}", results.display()))?;
            let v: Value = serde_json::from_str(&text)?;
            print!("{}", report::render(&v));
            Ok(())
        }
        Cmd::Sample { events, seed } => {
            for ev in workload::Workload::new(workload::GenConfig::new(seed, events)) {
                println!("{}", serde_json::to_string(&ev)?);
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

/// One planned child invocation.
struct Planned {
    label: String,
    step: &'static str,
    events: u64,
    db: Option<PathBuf>,
    relaxed: bool,
    reader: bool,
    keep: bool,
    mode: &'static str,
}

impl Planned {
    fn new(label: impl Into<String>, step: &'static str) -> Self {
        Self {
            label: label.into(),
            step,
            events: 0,
            db: None,
            relaxed: false,
            reader: false,
            keep: false,
            mode: "materialized",
        }
    }
}

fn plan(events: u64, out: &Path) -> Vec<Planned> {
    let full_db = out.join("db-full");
    let mut p = Vec::new();
    p.push(Planned::new("size_by_kind", "size_by_kind"));
    p.push(Planned::new("wal_latency", "wal_latency"));
    p.push(Planned::new("hook", "hook"));

    let curve: Vec<u64> = [10_000u64, 100_000]
        .into_iter()
        .filter(|n| *n < events)
        .collect();
    for n in &curve {
        for relaxed in [false, true] {
            let mut s = Planned::new(
                format!(
                    "ingest_{}_{}",
                    if relaxed { "relaxed" } else { "strict" },
                    short(*n)
                ),
                "ingest",
            );
            s.events = *n;
            s.relaxed = relaxed;
            p.push(s);
        }
    }
    let mut s = Planned::new("ingest_strict_full", "ingest");
    s.events = events;
    s.keep = true;
    s.db = Some(full_db.clone());
    p.push(s);
    let mut s = Planned::new("ingest_relaxed_full", "ingest");
    s.events = events;
    s.relaxed = true;
    p.push(s);

    let reader_n = events.min(200_000);
    if !curve.contains(&reader_n) && reader_n != events {
        let mut s = Planned::new(format!("ingest_strict_{}", short(reader_n)), "ingest");
        s.events = reader_n;
        p.push(s);
    }
    let mut s = Planned::new(
        format!(
            "ingest_strict_{}_reader",
            if reader_n == events {
                "full".to_string()
            } else {
                short(reader_n)
            }
        ),
        "ingest",
    );
    s.events = reader_n;
    s.reader = true;
    p.push(s);

    let mut s = Planned::new("segments_100k", "segments");
    s.events = events.min(100_000);
    p.push(s);

    let mut sizes: Vec<u64> = [10_000u64, 100_000, 500_000]
        .into_iter()
        .filter(|n| *n < events)
        .collect();
    sizes.push(events);
    for n in &sizes {
        for mode in ["streaming", "materialized"] {
            let mut s = Planned::new(
                format!(
                    "projection_{mode}_{}",
                    if *n == events {
                        "full".to_string()
                    } else {
                        short(*n)
                    }
                ),
                "projection",
            );
            s.events = *n;
            s.mode = mode;
            p.push(s);
        }
    }

    let mut s = Planned::new("recent_timeline", "recent_timeline");
    s.db = Some(full_db.clone());
    p.push(s);
    let mut s = Planned::new("scan_project_full", "scan_project");
    s.db = Some(full_db.clone());
    p.push(s);
    // The filtered engine path re-encodes the scan into one Arrow batch,
    // whose i32 string offsets overflow somewhere between 300k and 400k
    // events of realistic content; the steps bracket that.
    let mut engine_sizes: Vec<u64> = [100_000u64, 200_000, 300_000, 400_000, 500_000]
        .into_iter()
        .filter(|n| *n < events)
        .collect();
    engine_sizes.push(events);
    for n in engine_sizes {
        let mut s = Planned::new(
            format!(
                "engine_{}",
                if n == events {
                    "full".to_string()
                } else {
                    short(n)
                }
            ),
            "engine",
        );
        s.events = n;
        s.db = Some(full_db.clone());
        p.push(s);
    }
    let mut s = Planned::new("trace_chain", "trace_chain");
    s.events = 50_000.min(events);
    p.push(s);
    p
}

fn short(n: u64) -> String {
    if n.is_multiple_of(1_000_000) {
        format!("{}m", n / 1_000_000)
    } else if n.is_multiple_of(1_000) {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

/// Resident set of a process in bytes, via `ps` (portable enough for
/// macOS and Linux).
fn rss_of(pid: u32) -> Option<u64> {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .ok()
        .map(|kb| kb * 1024)
}

fn run_child(args: &RunArgs, planned: &Planned) -> Value {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return json!({"status": "failed", "reason": format!("current_exe: {e}")}),
    };
    let mut cmd = Command::new(exe);
    cmd.arg("step")
        .arg(planned.step)
        .arg("--out")
        .arg(&args.common.out)
        .arg("--events")
        .arg(planned.events.to_string())
        .arg("--seed")
        .arg(args.common.seed.to_string())
        .arg("--time-cap-secs")
        .arg(args.common.time_cap_secs.to_string())
        .arg("--mode")
        .arg(planned.mode);
    if let Some(bin) = args.common.attempt_bin.clone().or_else(default_attempt_bin) {
        cmd.arg("--attempt-bin").arg(bin);
    }
    if let Some(db) = &planned.db {
        cmd.arg("--db").arg(db);
    }
    if planned.relaxed {
        cmd.arg("--relaxed");
    }
    if planned.reader {
        cmd.arg("--reader");
    }
    if planned.keep {
        cmd.arg("--keep");
    }
    let stderr_path = args
        .common
        .out
        .join("logs")
        .join(format!("{}.stderr.log", planned.label));
    let _ = std::fs::create_dir_all(stderr_path.parent().expect("logs dir"));
    let stderr = match std::fs::File::create(&stderr_path) {
        Ok(f) => Stdio::from(f),
        Err(_) => Stdio::null(),
    };
    let started = Instant::now();
    let mut child = match cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return json!({"status": "failed", "reason": format!("spawn: {e}")}),
    };
    let pid = child.id();
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = std::io::Read::read_to_string(&mut stdout, &mut buf);
        buf
    });
    let rss_cap = (args.rss_cap_gb * 1024.0 * 1024.0 * 1024.0) as u64;
    let hard_cap = Duration::from_secs(args.common.time_cap_secs * 2 + 60);
    let mut peak_observed = 0u64;
    let mut killed: Option<String> = None;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(e) => {
                killed = Some(format!("wait: {e}"));
                break;
            }
        }
        if let Some(rss) = rss_of(pid) {
            peak_observed = peak_observed.max(rss);
            if rss > rss_cap {
                killed = Some(format!(
                    "killed: resident set {} exceeded the {} cap after {:.0} s",
                    stats::human_bytes(rss as f64),
                    stats::human_bytes(rss_cap as f64),
                    started.elapsed().as_secs_f64()
                ));
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
        if started.elapsed() > hard_cap {
            killed = Some(format!(
                "killed: exceeded the hard time cap of {} s (peak RSS {})",
                hard_cap.as_secs(),
                stats::human_bytes(peak_observed as f64)
            ));
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let wall = started.elapsed().as_secs_f64();
    let output = reader.join().unwrap_or_default();
    let status = child.try_wait().ok().flatten();
    let mut v = if let Some(reason) = killed {
        json!({"status": "not_run", "reason": reason})
    } else {
        match serde_json::from_str::<Value>(output.trim()) {
            Ok(v) if status.is_some_and(|s| s.success()) => {
                let mut v = v;
                v["status"] = json!("ok");
                v
            }
            _ => {
                let tail = std::fs::read_to_string(&stderr_path)
                    .map(|s| {
                        s.lines()
                            .rev()
                            .take(5)
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                            .collect::<Vec<_>>()
                            .join(" | ")
                    })
                    .unwrap_or_default();
                json!({"status": "failed", "reason": format!("exit {:?}: {tail}", status.map(|s| s.code()))})
            }
        }
    };
    v["step_wall_secs"] = json!(wall);
    v["peak_rss_observed_bytes"] = json!(peak_observed);
    v["label"] = json!(planned.label);
    v["step"] = json!(planned.step);
    v["events_requested"] = json!(planned.events);
    v
}

fn run(mut args: RunArgs) -> Result<()> {
    args.common.out = absolute(&args.common.out);
    let out = &args.common.out;
    std::fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    let attempt_bin = args.common.attempt_bin.clone().or_else(default_attempt_bin);
    let machine = sysinfo::collect(attempt_bin.as_deref());
    let started_at = chrono_like_now();
    let mut results = json!({
        "benchmark": "attemptdb-bench",
        "version": env!("CARGO_PKG_VERSION"),
        "started_at": started_at,
        "machine": machine,
        "params": {
            "events": args.common.events,
            "seed": args.common.seed,
            "batch_size": steps::ingest::BATCH,
            "time_cap_secs": args.common.time_cap_secs,
            "rss_cap_bytes": (args.rss_cap_gb * 1024.0 * 1024.0 * 1024.0) as u64,
            "sample_date": model::SAMPLE_DATE,
            "sample_events": model::SAMPLE_EVENTS,
        },
        "steps": {},
    });
    let results_path = out.join("results.json");
    let plan = plan(args.common.events, out);
    let total = plan.len();
    for (i, planned) in plan.iter().enumerate() {
        let selected = (args.only.is_empty()
            || args.only.iter().any(|o| planned.label.contains(o)))
            && !args.skip.iter().any(|s| planned.label.contains(s));
        if !selected {
            results["steps"][&planned.label] = json!({
                "status": "not_run",
                "reason": "skipped by --only/--skip",
                "step": planned.step,
                "events_requested": planned.events,
            });
            continue;
        }
        eprintln!(
            "[{}/{}] {} ({} events)",
            i + 1,
            total,
            planned.label,
            planned.events
        );
        let v = run_child(&args, planned);
        eprintln!(
            "      {} in {:.1} s, peak RSS {}{}",
            v["status"].as_str().unwrap_or("?"),
            v["step_wall_secs"].as_f64().unwrap_or(0.0),
            stats::human_bytes(v["peak_rss_observed_bytes"].as_f64().unwrap_or(0.0)),
            v.get("reason")
                .and_then(Value::as_str)
                .map(|r| format!(" — {r}"))
                .unwrap_or_default()
        );
        results["steps"][&planned.label] = v;
        results["finished_at"] = json!(chrono_like_now());
        std::fs::write(&results_path, serde_json::to_string_pretty(&results)?)?;
    }
    std::fs::write(&results_path, serde_json::to_string_pretty(&results)?)?;
    std::fs::write(out.join("results.md"), report::render(&results))?;
    eprintln!("results: {}", results_path.display());
    if args.json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    }
    Ok(())
}

/// RFC 3339 UTC timestamp without pulling in a date crate.
fn chrono_like_now() -> String {
    attemptdb_core::Timestamp::now().to_rfc3339()
}
