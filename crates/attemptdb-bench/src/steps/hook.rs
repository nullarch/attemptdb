//! Hook process wall clock: spawn `attempt hook claude-code` with a fixed
//! PostToolUse payload against a temporary data directory, with no daemon
//! (spool path) and with a daemon started by the benchmark (Strict and
//! `--relaxed`). Process-spawn floors (`attempt --version`, `/usr/bin/true`)
//! put the numbers in context.

use super::{StepCtx, fresh_dir, open_writer};
use crate::rng::Rng;
use crate::stats::Summary;
use crate::text;
use anyhow::{Context, Result};
use attemptdb_storage::ScanFilter;
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const SPAWNS: usize = 200;
const WARMUP: usize = 5;

fn attempt_bin(ctx: &StepCtx) -> Result<PathBuf> {
    ctx.attempt_bin
        .clone()
        .filter(|p| p.exists())
        .context("no `attempt` binary found; pass --attempt-bin")
}

/// A project directory with a minimal `.git` so the hook resolves a
/// repository the way it does in real use (three small file reads).
fn fake_project(dir: &Path) -> Result<()> {
    fresh_dir(dir)?;
    let git = dir.join(".git");
    std::fs::create_dir_all(git.join("refs/heads"))?;
    std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n")?;
    std::fs::write(
        git.join("refs/heads/main"),
        "0123456789abcdef0123456789abcdef01234567\n",
    )?;
    std::fs::write(
        git.join("config"),
        "[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n\turl = git@example.invalid:bench/hook-project.git\n",
    )?;
    Ok(())
}

/// The fixed payload: a PostToolUse for a shell command with ~2 KB of
/// output, close to the sampled median tool result.
fn payload(project: &Path) -> Vec<u8> {
    let mut rng = Rng::new(0x4007);
    let stdout = text::log(&mut rng, 2_048);
    serde_json::to_vec(&json!({
        "hook_event_name": "PostToolUse",
        "session_id": "bench-hook-session-0000",
        "transcript_path": project.join(".transcripts/bench.jsonl"),
        "cwd": project,
        "permission_mode": "default",
        "tool_name": "Bash",
        "tool_use_id": "toolu_bench000000000000000001",
        "tool_input": {"command": "cargo test -p bench -- --nocapture", "description": "run the tests"},
        "tool_response": {"stdout": stdout, "stderr": "", "interrupted": false, "isImage": false, "noOutputExpected": false},
    }))
    .expect("payload serialises")
}

fn hook_command(bin: &Path, data_dir: &Path, project: &Path) -> Command {
    let mut cmd = Command::new(bin);
    cmd.args(["hook", "claude-code"])
        .env("ATTEMPTDB_DATA_DIR", data_dir)
        .env_remove("ATTEMPTDB_DIR")
        .env_remove("ATTEMPTDB_HOOK_TRACE")
        .current_dir(project)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// Spawn the hook `n` times with the payload on stdin; wall time per spawn.
fn spawn_hooks(
    bin: &Path,
    data_dir: &Path,
    project: &Path,
    payload: &[u8],
    n: usize,
) -> Result<Vec<f64>> {
    let mut us = Vec::with_capacity(n);
    for _ in 0..n {
        let t = Instant::now();
        let mut child = hook_command(bin, data_dir, project).spawn()?;
        {
            let mut stdin = child.stdin.take().context("hook stdin")?;
            stdin.write_all(payload)?;
        }
        let status = child.wait()?;
        us.push(t.elapsed().as_secs_f64() * 1e6);
        if !status.success() {
            anyhow::bail!("hook exited with {status}");
        }
    }
    Ok(us)
}

fn spawn_plain(cmd: &mut Command, n: usize) -> Result<Vec<f64>> {
    let mut us = Vec::with_capacity(n);
    for _ in 0..n {
        let t = Instant::now();
        let status = cmd.status()?;
        us.push(t.elapsed().as_secs_f64() * 1e6);
        if !status.success() {
            anyhow::bail!("baseline exited with {status}");
        }
    }
    Ok(us)
}

fn daemon_status(bin: &Path, data_dir: &Path) -> Option<Value> {
    let out = Command::new(bin)
        .args(["daemon", "status", "--json"])
        .env("ATTEMPTDB_DATA_DIR", data_dir)
        .env_remove("ATTEMPTDB_DIR")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    serde_json::from_slice(&out.stdout).ok()
}

fn start_daemon(bin: &Path, data_dir: &Path, relaxed: bool) -> Result<Child> {
    let log = std::fs::File::create(data_dir.join("daemon-stdout.log"))?;
    let mut cmd = Command::new(bin);
    cmd.args(["daemon", "run", "--foreground"]);
    if relaxed {
        cmd.arg("--relaxed");
    }
    let child = cmd
        .env("ATTEMPTDB_DATA_DIR", data_dir)
        .env_remove("ATTEMPTDB_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()
        .context("spawning the daemon")?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if daemon_status(bin, data_dir)
            .and_then(|v| v.get("running").and_then(Value::as_bool))
            .unwrap_or(false)
        {
            return Ok(child);
        }
        if Instant::now() > deadline {
            anyhow::bail!(
                "daemon did not answer within 20 s (log: {})",
                data_dir.join("daemon-stdout.log").display()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn stop_daemon(bin: &Path, data_dir: &Path, mut child: Child) -> Result<f64> {
    let t = Instant::now();
    let _ = Command::new(bin)
        .args(["daemon", "stop"])
        .env("ATTEMPTDB_DATA_DIR", data_dir)
        .env_remove("ATTEMPTDB_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("daemon did not exit within 30 s of `daemon stop`; killed");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(t.elapsed().as_secs_f64())
}

/// Count events and collect their in-process `hook_us` from the database
/// under a data dir (importing the spool first).
fn durable_events(data_dir: &Path) -> Result<(u64, Summary)> {
    let root = data_dir.join("db").join(".attemptdb");
    let mut db = open_writer(&root, false, None)?;
    db.import_spool()?;
    let events = db.scan(&ScanFilter::default())?;
    let mut us: Vec<f64> = events
        .iter()
        .filter_map(|e| e.attrs.get("hook_us").and_then(Value::as_u64))
        .map(|v| v as f64)
        .collect();
    Ok((events.len() as u64, Summary::of_micros(&mut us)))
}

fn mode(
    label: &str,
    bin: &Path,
    base: &Path,
    project: &Path,
    payload: &[u8],
    daemon: Option<bool>,
) -> Result<Value> {
    let data_dir = base.join(format!("data-{label}"));
    fresh_dir(&data_dir)?;
    let child = match daemon {
        Some(relaxed) => Some(start_daemon(bin, &data_dir, relaxed)?),
        None => None,
    };
    let result = (|| -> Result<(Vec<f64>, Vec<f64>)> {
        let warm = spawn_hooks(bin, &data_dir, project, payload, WARMUP)?;
        let timed = spawn_hooks(bin, &data_dir, project, payload, SPAWNS)?;
        Ok((warm, timed))
    })();
    let stop_secs = match child {
        Some(c) => Some(stop_daemon(bin, &data_dir, c)?),
        None => None,
    };
    let (_warm, mut timed) = result?;
    let (durable, hook_us) = durable_events(&data_dir)?;
    let delivered = match daemon {
        Some(_) => "daemon",
        None => "spool",
    };
    Ok(json!({
        "delivery": delivered,
        "daemon_relaxed": daemon,
        "spawns": SPAWNS,
        "wall": Summary::of_micros(&mut timed),
        "in_process_hook_us": hook_us,
        "events_durable_after": durable,
        "expected_events": (SPAWNS + WARMUP) as u64,
        "daemon_stop_secs": stop_secs,
    }))
}

pub fn run(ctx: &StepCtx) -> Result<Value> {
    let bin = attempt_bin(ctx)?;
    let base = ctx.out.join("hook");
    fresh_dir(&base)?;
    let project = base.join("project");
    fake_project(&project)?;
    let payload = payload(&project);

    let mut true_us = spawn_plain(
        Command::new("/usr/bin/true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
        SPAWNS,
    )?;
    let mut version_us = spawn_plain(
        Command::new(&bin)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
        SPAWNS,
    )?;

    let spool = mode("spool", &bin, &base, &project, &payload, None)?;
    let strict = mode(
        "daemon-strict",
        &bin,
        &base,
        &project,
        &payload,
        Some(false),
    )?;
    let relaxed = mode(
        "daemon-relaxed",
        &bin,
        &base,
        &project,
        &payload,
        Some(true),
    )?;
    let _ = std::fs::remove_dir_all(&base);
    Ok(json!({
        "attempt_binary": bin.display().to_string(),
        "attempt_binary_bytes": std::fs::metadata(&bin).map(|m| m.len()).unwrap_or(0),
        "payload_bytes": payload.len(),
        "spawn_floor_true": Summary::of_micros(&mut true_us),
        "spawn_floor_attempt_version": Summary::of_micros(&mut version_us),
        "spool": spool,
        "daemon_strict": strict,
        "daemon_relaxed": relaxed,
    }))
}
