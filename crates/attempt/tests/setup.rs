//! `attempt setup` end to end: one command wires a machine, and running it
//! again changes nothing. The agents are fakes under a temporary HOME — a
//! `~/.claude/settings.json` is all the installer needs to see Claude Code —
//! and the daemon step is opted out with `ATTEMPTDB_NO_DAEMON`, because a
//! test must not register a launchd agent or systemd unit for the user.

use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;

/// A PATH with no agent launchers on it: detection also looks for `codex`,
/// `gemini` and `cursor` binaries, and the developer's machine has them.
fn bare_path() -> String {
    if cfg!(windows) {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        format!("{root}\\System32")
    } else {
        "/usr/bin:/bin".into()
    }
}

/// The variables that point an agent at somewhere other than the fake HOME.
/// The developer running these tests may use a second Claude account, so none
/// of them may reach a child process (the tests below prove it).
const AGENT_CONFIG_VARS: [&str; 4] = [
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "CURSOR_CONFIG_DIR",
    "GEMINI_CONFIG_DIR",
];

fn command(home: &Path, data_dir: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_attempt"));
    c.arg("--data-dir")
        .arg(data_dir)
        .env("PATH", bare_path())
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("ATTEMPTDB_KEYRING", "off")
        .env("ATTEMPTDB_NO_DAEMON", "1")
        .env_remove("ATTEMPTDB_KEY_FILE")
        .env_remove("ATTEMPTDB_DIR")
        .env_remove("ATTEMPTDB_MANAGED_BY");
    for var in AGENT_CONFIG_VARS {
        c.env_remove(var);
    }
    // Codex is the one agent whose home is looked up by variable; point it at
    // the fake HOME.
    c.env("CODEX_HOME", home.join(".codex"));
    c
}

#[test]
fn the_harness_never_lets_an_agents_real_config_directory_through() {
    let m = machine(false);
    let c = command(&m.home, &m.data);
    let envs: std::collections::HashMap<_, _> = c
        .get_envs()
        .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|v| v.to_owned())))
        .collect();
    for var in [
        "CLAUDE_CONFIG_DIR",
        "CURSOR_CONFIG_DIR",
        "GEMINI_CONFIG_DIR",
    ] {
        assert_eq!(
            envs.get(var),
            Some(&None),
            "{var} must be removed, not inherited"
        );
    }
    assert_eq!(
        envs.get("CODEX_HOME").cloned().flatten().as_deref(),
        Some(m.home.join(".codex").as_os_str()),
        "CODEX_HOME points into the fake HOME"
    );
}

fn attempt(home: &Path, data_dir: &Path, args: &[&str]) -> (bool, String, String) {
    let out = command(home, data_dir)
        .args(args)
        .output()
        .expect("run attempt");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("not JSON ({e}):\n{text}"))
}

struct Machine {
    _tmp: tempfile::TempDir,
    home: std::path::PathBuf,
    data: std::path::PathBuf,
}

fn machine(with_claude: bool) -> Machine {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    fs::create_dir_all(&home).unwrap();
    if with_claude {
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(
            home.join(".claude/settings.json"),
            r#"{"permissions":{"allow":["Bash(ls:*)"]}}"#,
        )
        .unwrap();
    }
    Machine {
        _tmp: tmp,
        home,
        data,
    }
}

#[test]
fn a_dry_run_writes_nothing_and_says_what_it_would_do() {
    let m = machine(true);
    let (ok, out, err) = attempt(&m.home, &m.data, &["--json", "setup", "--dry-run"]);
    assert!(ok, "{out}{err}");
    let v = json(&out);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["ok"], true);
    assert_eq!(v["database"]["existed"], false);
    assert_eq!(v["database"]["created"], false);
    assert!(
        !m.data.join("db").exists(),
        "a dry run must not create the database"
    );
    let actions = v["hooks"]["actions"].as_array().unwrap();
    assert_eq!(actions.len(), 1, "{v:#}");
    assert_eq!(actions[0]["agent"], "claude-code");
    assert_eq!(actions[0]["outcome"]["kind"], "installed");
    let settings = fs::read_to_string(m.home.join(".claude/settings.json")).unwrap();
    assert!(
        !settings.contains("attempt"),
        "a dry run must not touch the agent's config:\n{settings}"
    );
    assert!(v["hooks"]["capture_tests"].as_array().unwrap().is_empty());
    assert_eq!(v["daemon"]["skipped"], "ATTEMPTDB_NO_DAEMON is set");
}

#[test]
fn setup_wires_a_machine_and_a_second_run_changes_nothing() {
    let m = machine(true);
    let (ok, out, err) = attempt(
        &m.home,
        &m.data,
        &[
            "--json",
            "setup",
            "--source",
            "test",
            "--capture-mode",
            "metadata_only",
        ],
    );
    assert!(ok, "{out}{err}");
    let v = json(&out);
    assert_eq!(v["ok"], true, "{v:#}");
    assert_eq!(v["database"]["created"], true);
    assert_eq!(v["database"]["capture_mode"], "metadata_only");
    assert!(m.data.join("db").exists(), "database directory");
    let actions = v["hooks"]["actions"].as_array().unwrap();
    assert_eq!(actions[0]["outcome"]["kind"], "installed");
    let tests = v["hooks"]["capture_tests"].as_array().unwrap();
    assert_eq!(tests.len(), 1, "{v:#}");
    assert_eq!(tests[0]["ok"], true, "{v:#}");
    let settings = fs::read_to_string(m.home.join(".claude/settings.json")).unwrap();
    assert!(settings.contains("attempt"), "{settings}");
    assert!(
        settings.contains("Bash(ls:*)"),
        "the user's own settings must survive:\n{settings}"
    );
    let check = v["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["agent"] == "claude-code")
        .cloned()
        .unwrap();
    assert_eq!(check["detected"], true);
    assert_ne!(check["state"], "not installed", "{check:#}");

    // Again, without asking for a capture mode: the database exists, the
    // hooks are current, and the mode it has is the mode it keeps.
    let (ok, out, err) = attempt(&m.home, &m.data, &["--json", "setup"]);
    assert!(ok, "{out}{err}");
    let v = json(&out);
    assert_eq!(v["database"]["existed"], true);
    assert_eq!(v["database"]["created"], false);
    assert_eq!(v["database"]["capture_mode"], "metadata_only");
    assert!(
        v["database"].get("capture_mode_changed_from").is_none(),
        "nothing was changed: {v:#}"
    );
    let actions = v["hooks"]["actions"].as_array().unwrap();
    assert_eq!(actions[0]["outcome"]["kind"], "already_current", "{v:#}");
    let again = fs::read_to_string(m.home.join(".claude/settings.json")).unwrap();
    assert_eq!(
        settings, again,
        "a repeated setup must not rewrite the config"
    );
}

#[test]
fn a_stale_hook_is_setups_job_not_the_users() {
    // Hooks written by an `attempt` that lived somewhere else (an older
    // install, a moved binary): a dry run says "would update" and the check
    // says "stale", but nothing lands under `needs you` — setup rewrites it.
    let m = machine(true);
    let (ok, out, err) = attempt(&m.home, &m.data, &["--json", "setup", "--no-verify"]);
    assert!(ok, "{out}{err}");
    let settings_path = m.home.join(".claude/settings.json");
    let settings = fs::read_to_string(&settings_path).unwrap();
    let json_inner = |p: &Path| {
        let quoted = serde_json::to_string(p.to_str().unwrap()).unwrap();
        quoted[1..quoted.len() - 1].to_string()
    };
    let exe_dir = Path::new(env!("CARGO_BIN_EXE_attempt")).parent().unwrap();
    let moved = settings.replace(&json_inner(exe_dir), &json_inner(&m.home.join("elsewhere")));
    assert_ne!(
        moved, settings,
        "the hook entries name {exe_dir:?}:\n{settings}"
    );
    fs::write(&settings_path, moved).unwrap();

    let (ok, out, err) = attempt(&m.home, &m.data, &["--json", "setup", "--dry-run"]);
    assert!(ok, "{out}{err}");
    let v = json(&out);
    let actions = v["hooks"]["actions"].as_array().unwrap();
    assert_eq!(actions[0]["outcome"]["kind"], "updated", "{v:#}");
    assert!(
        v["needs_you"].as_array().unwrap().is_empty(),
        "a stale hook is setup's job, not the user's:\n{v:#}"
    );
    let check = v["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["agent"] == "claude-code")
        .cloned()
        .unwrap();
    assert_eq!(check["state"], "stale", "{check:#}");
}

#[test]
fn no_agents_is_not_a_failure() {
    let m = machine(false);
    let (ok, out, err) = attempt(&m.home, &m.data, &["setup", "--no-verify"]);
    assert!(ok, "{out}{err}");
    assert!(out.contains("no coding agents detected"), "{out}");
    assert!(out.contains("database     created"), "{out}");
    assert!(out.contains("done."), "{out}");
}

#[test]
fn every_field_the_agent_install_guide_names_is_in_the_report() {
    // docs/install-for-agents.md tells a coding agent which fields of
    // `setup --json` to read. A renamed field would leave that agent reading
    // nulls and reporting success; this is what notices first.
    let guide = include_str!("../../../docs/install-for-agents.md");
    let fields: Vec<&str> = guide
        .lines()
        .filter_map(|l| l.strip_prefix("| `"))
        .filter_map(|l| l.split('`').next())
        .collect();
    assert!(fields.len() >= 5, "the guide's field table: {fields:?}");

    let m = machine(true);
    let (ok, out, err) = attempt(&m.home, &m.data, &["--json", "setup"]);
    assert!(ok, "{out}{err}");
    let v = json(&out);
    for field in fields {
        let mut node = &v;
        for part in field.split('.') {
            let (key, array) = match part.strip_suffix("[]") {
                Some(k) => (k, true),
                None => (part, false),
            };
            node = &node[key];
            assert!(!node.is_null(), "`{field}`: no `{key}` in\n{v:#}");
            if array {
                node = node
                    .as_array()
                    .and_then(|a| a.first())
                    .unwrap_or_else(|| panic!("`{field}`: `{key}` is not a non-empty array"));
            }
        }
    }
    let kinds = [
        "installed",
        "updated",
        "already_current",
        "skipped",
        "failed",
    ];
    let kind = v["hooks"]["actions"][0]["outcome"]["kind"]
        .as_str()
        .unwrap();
    assert!(kinds.contains(&kind), "{kind}");
    for k in kinds {
        assert!(guide.contains(&format!("`{k}`")), "the guide lists `{k}`");
    }
}

fn doctor_state(m: &Machine, agent: &str) -> Value {
    let (ok, out, err) = attempt(&m.home, &m.data, &["--json", "doctor"]);
    assert!(ok, "{out}{err}");
    let v = json(&out);
    v["diagnosis"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["agent"] == agent)
        .cloned()
        .unwrap_or_else(|| panic!("no {agent} in\n{v:#}"))
}

#[test]
fn doctor_says_verified_after_setup_and_active_after_the_first_real_hook() {
    let m = machine(true);
    let (ok, out, err) = attempt(&m.home, &m.data, &["setup"]);
    assert!(ok, "{out}{err}");
    let before = doctor_state(&m, "claude-code");
    assert_eq!(before["state"], "verified", "{before:#}");
    assert_eq!(before["activity"]["capture_test_seen"], true, "{before:#}");
    assert_eq!(before["activity"]["event_count"], 0, "{before:#}");

    // One real Claude Code hook, the way Claude Code runs it.
    let mut hook = Command::new(env!("CARGO_BIN_EXE_attempt"))
        .arg("--data-dir")
        .arg(&m.data)
        .args(["hook", "claude-code"])
        .env("PATH", bare_path())
        .env("HOME", &m.home)
        .env("USERPROFILE", &m.home)
        .env("ATTEMPTDB_KEYRING", "off")
        .env("ATTEMPTDB_NO_DAEMON", "1")
        .env_remove("ATTEMPTDB_DIR")
        .env_remove("CLAUDE_CONFIG_DIR")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("run the hook");
    std::io::Write::write_all(
        hook.stdin.as_mut().unwrap(),
        include_bytes!("../../../fixtures/providers/claude_code/post_tool_use_bash_unknown.json"),
    )
    .unwrap();
    assert!(hook.wait().unwrap().success(), "a hook always exits 0");

    let after = doctor_state(&m, "claude-code");
    assert_eq!(after["state"], "active", "{after:#}");
    assert_eq!(after["activity"]["event_count"], 1, "{after:#}");
    assert!(after["activity"]["last_event_at"].is_string(), "{after:#}");
}

// ---------------------------------------------------------------------------
// The history backfill: the first `attempt ui` shows the person's own work.
// ---------------------------------------------------------------------------

const CLAUDE_TRANSCRIPT: &[u8] =
    include_bytes!("../../../fixtures/transcripts/claude_code/basic_turn.jsonl");
const CODEX_ROLLOUT: &[u8] =
    include_bytes!("../../../fixtures/transcripts/codex/modern_turn.jsonl");
const CODEX_ROLLOUT_OLD: &[u8] =
    include_bytes!("../../../fixtures/transcripts/codex/classic_turn.jsonl");

/// `~/.claude/projects/...` and `~/.codex/sessions/...` under the fake HOME,
/// the way the agents leave them. Written (not copied) so every mtime is
/// "now" unless a test ages one; a copy may keep the fixture's own.
fn write_history(m: &Machine) -> (std::path::PathBuf, std::path::PathBuf) {
    let claude = m.home.join(
        ".claude/projects/-home-dev-example-project/11111111-1111-4111-8111-111111111111.jsonl",
    );
    fs::create_dir_all(claude.parent().unwrap()).unwrap();
    fs::write(&claude, CLAUDE_TRANSCRIPT).unwrap();
    let day = m.home.join(".codex/sessions/2026/08/28");
    fs::create_dir_all(&day).unwrap();
    let codex = day.join("rollout-2026-08-28T08-00-00-22222222-2222-4222-8222-222222222222.jsonl");
    fs::write(&codex, CODEX_ROLLOUT).unwrap();
    (claude, codex)
}

fn age(path: &Path, days: u64) {
    let when = std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86_400);
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(when)
        .unwrap();
}

fn events_in_database(m: &Machine) -> u64 {
    let (ok, out, err) = attempt(&m.home, &m.data, &["--json", "status"]);
    assert!(ok, "{out}{err}");
    json(&out)["events"].as_u64().unwrap()
}

fn history_provider<'a>(v: &'a Value, agent: &str) -> &'a Value {
    v["history"]["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["agent"] == agent)
        .unwrap_or_else(|| panic!("no {agent} in\n{v:#}"))
}

#[test]
fn setup_backfills_recent_history_and_a_second_run_adds_nothing() {
    let m = machine(true);
    write_history(&m);
    let (ok, out, err) = attempt(&m.home, &m.data, &["--json", "setup", "--no-verify"]);
    assert!(ok, "{out}{err}");
    let v = json(&out);
    assert_eq!(v["ok"], true, "{v:#}");
    let h = &v["history"];
    assert_eq!(h["enabled"], true);
    assert_eq!(h["days"], 30);
    assert_eq!(h["max_mib"], 512);
    assert!(h["error"].is_null(), "{h:#}");
    assert!(h["skipped"].is_null(), "{h:#}");
    let claude = history_provider(&v, "claude-code");
    let codex = history_provider(&v, "codex");
    assert_eq!(
        (claude["files"].as_u64(), claude["sessions"].as_u64()),
        (Some(1), Some(1))
    );
    assert_eq!(
        (codex["files"].as_u64(), codex["sessions"].as_u64()),
        (Some(1), Some(1))
    );
    assert!(claude["bytes"].as_u64().unwrap() > 1000);
    assert_eq!(claude["imported"]["accepted"], 12, "{claude:#}");
    // The Codex rollout was written just now, so its session may still be
    // running: 30 events, no `session_ended` yet (31 once it has been quiet).
    assert_eq!(codex["imported"]["accepted"], 30, "{codex:#}");
    assert_eq!(h["accepted"], 42);
    assert_eq!(h["queued"], 0);
    assert_eq!(
        events_in_database(&m),
        42,
        "the first timeline already has history"
    );

    // Again: the same history is found, nothing is stored twice.
    let (ok, out, err) = attempt(&m.home, &m.data, &["--json", "setup", "--no-verify"]);
    assert!(ok, "{out}{err}");
    let v = json(&out);
    assert_eq!(v["history"]["accepted"], 0, "{:#}", v["history"]);
    assert_eq!(
        history_provider(&v, "claude-code")["imported"]["duplicates"],
        12
    );
    assert_eq!(history_provider(&v, "codex")["imported"]["duplicates"], 30);
    assert_eq!(events_in_database(&m), 42, "idempotent: same event count");

    // The text form names the step.
    let (ok, out, err) = attempt(&m.home, &m.data, &["setup", "--no-verify"]);
    assert!(ok, "{out}{err}");
    assert!(out.contains("history"), "{out}");
    assert!(out.contains("already there"), "{out}");
}

#[test]
fn no_backfill_skips_the_history() {
    let m = machine(true);
    write_history(&m);
    let (ok, out, err) = attempt(
        &m.home,
        &m.data,
        &["--json", "setup", "--no-verify", "--no-backfill"],
    );
    assert!(ok, "{out}{err}");
    let v = json(&out);
    assert_eq!(v["ok"], true);
    assert_eq!(v["history"]["enabled"], false);
    assert_eq!(v["history"]["skipped"], "--no-backfill");
    assert!(v["history"]["providers"].as_array().unwrap().is_empty());
    assert_eq!(events_in_database(&m), 0);

    let (ok, out, err) = attempt(&m.home, &m.data, &["setup", "--no-verify", "--no-backfill"]);
    assert!(ok, "{out}{err}");
    assert!(out.contains("skipped (--no-backfill)"), "{out}");
}

#[test]
fn a_dry_run_says_what_history_it_would_import_and_imports_none() {
    let m = machine(true);
    write_history(&m);
    let (ok, out, err) = attempt(&m.home, &m.data, &["--json", "setup", "--dry-run"]);
    assert!(ok, "{out}{err}");
    let v = json(&out);
    let claude = history_provider(&v, "claude-code");
    let codex = history_provider(&v, "codex");
    assert_eq!(claude["files"], 1);
    assert_eq!(codex["files"], 1);
    assert!(codex["bytes"].as_u64().unwrap() > 1000);
    assert!(claude["imported"].is_null() && codex["imported"].is_null());
    assert_eq!(v["history"]["accepted"], 0);
    assert!(!m.data.join("db").exists(), "a dry run creates no database");

    let (ok, out, err) = attempt(&m.home, &m.data, &["setup", "--dry-run"]);
    assert!(ok, "{out}{err}");
    assert!(
        out.contains("would import 1 file(s), 1 session(s)"),
        "{out}"
    );
}

#[test]
fn the_backfill_window_and_budget_choose_what_is_read() {
    let m = machine(true);
    let (_, codex) = write_history(&m);
    // An older rollout, 40 days back; and a large one (over 1 MiB) a day old.
    let day = codex.parent().unwrap();
    let old = day.join("rollout-2026-07-18T09-00-00-33333333-3333-4333-8333-333333333333.jsonl");
    fs::write(&old, CODEX_ROLLOUT_OLD).unwrap();
    age(&old, 40);
    let big = day.join("rollout-2026-08-27T09-00-00-66666666-6666-4666-8666-666666666666.jsonl");
    let filler = format!(
        "{}\n{}",
        r#"{"timestamp":"2026-08-27T09:00:00.000Z","type":"session_meta","payload":{"id":"66666666-6666-4666-8666-666666666666","cwd":"/home/dev/example/project","cli_version":"0.154.0","source":"cli"}}"#,
        r#"{"timestamp":"2026-08-27T09:00:01.000Z","type":"event_msg","payload":{"type":"agent_message","message":"filler filler filler filler filler filler filler filler filler filler filler filler","phase":"commentary"}}
"#
        .repeat(13_000)
    );
    fs::write(&big, filler).unwrap();
    age(&big, 1);

    // Default: 30 days, 512 MiB: the old one is left out, the big one is in.
    let (ok, out, err) = attempt(&m.home, &m.data, &["--json", "setup", "--dry-run"]);
    assert!(ok, "{out}{err}");
    let codex_plan = history_provider(&json(&out), "codex").clone();
    assert_eq!(codex_plan["files"], 2, "{codex_plan:#}");
    assert_eq!(codex_plan["skipped_old"], 1);
    assert_eq!(codex_plan["skipped_over_budget"], 0);

    // 1 MiB: the big file does not fit, the newest small one does.
    let (ok, out, err) = attempt(
        &m.home,
        &m.data,
        &["--json", "setup", "--dry-run", "--backfill-max-mib", "1"],
    );
    assert!(ok, "{out}{err}");
    let codex_plan = history_provider(&json(&out), "codex").clone();
    assert_eq!(codex_plan["files"], 1, "{codex_plan:#}");
    assert_eq!(codex_plan["skipped_over_budget"], 1);

    // 0 days: all of it, the 40-day-old rollout included.
    let (ok, out, err) = attempt(
        &m.home,
        &m.data,
        &["--json", "setup", "--dry-run", "--backfill-days", "0"],
    );
    assert!(ok, "{out}{err}");
    let v = json(&out);
    assert!(v["history"]["days"].is_null());
    assert_eq!(history_provider(&v, "codex")["files"], 3);
}

#[test]
fn history_keeps_the_databases_capture_mode() {
    let m = machine(true);
    write_history(&m);
    let (ok, out, err) = attempt(
        &m.home,
        &m.data,
        &[
            "--json",
            "setup",
            "--no-verify",
            "--capture-mode",
            "metadata_only",
        ],
    );
    assert!(ok, "{out}{err}");
    let v = json(&out);
    assert_eq!(v["history"]["capture_mode"], "metadata_only");
    assert_eq!(v["history"]["accepted"], 42);
    let (ok, out, err) = attempt(
        &m.home,
        &m.data,
        &[
            "--json",
            "query",
            "SELECT count(*) AS n FROM events WHERE content_json IS NOT NULL OR raw_json IS NOT NULL",
        ],
    );
    assert!(ok, "{out}{err}");
    assert_eq!(
        json(&out)[0]["n"],
        0,
        "metadata_only stores no content: {out}"
    );
}

/// The daemon holds the database's writer lock while setup runs: the history
/// goes through the spool, setup still succeeds, and the daemon stores it.
#[test]
fn a_running_daemon_gets_the_history_through_the_spool() {
    use attemptdb_storage::{Database, OpenOptions};
    let m = machine(true);
    write_history(&m);
    let (ok, out, err) = attempt(
        &m.home,
        &m.data,
        &["--json", "setup", "--no-verify", "--no-backfill"],
    );
    assert!(ok, "{out}{err}");
    let db_dir = m.data.join("db").join(".attemptdb");
    assert!(db_dir.exists(), "{db_dir:?}");
    let mut daemon = Database::open(
        &db_dir,
        OpenOptions {
            create: false,
            ..Default::default()
        },
    )
    .unwrap();

    let (ok, out, err) = attempt(&m.home, &m.data, &["--json", "setup", "--no-verify"]);
    assert!(ok, "the held lock must not fail setup:\n{out}{err}");
    let v = json(&out);
    assert_eq!(v["ok"], true, "{v:#}");
    assert!(v["history"]["error"].is_null(), "{:#}", v["history"]);
    assert_eq!(
        v["history"]["accepted"], 0,
        "nothing stored by setup itself"
    );
    assert_eq!(v["history"]["queued"], 42, "{:#}", v["history"]);
    assert_eq!(history_provider(&v, "codex")["imported"]["queued"], 30);

    let report = daemon.import_spool().unwrap();
    assert_eq!(report.accepted, 42, "the daemon imports what setup queued");
    drop(daemon);
    assert_eq!(events_in_database(&m), 42);
}

#[test]
fn the_provider_filter_limits_the_backfill_too() {
    let m = machine(true);
    write_history(&m);
    let (ok, out, err) = attempt(
        &m.home,
        &m.data,
        &[
            "--json",
            "setup",
            "--no-verify",
            "--provider",
            "claude-code",
        ],
    );
    assert!(ok, "{out}{err}");
    let v = json(&out);
    let agents: Vec<&str> = v["history"]["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["agent"].as_str().unwrap())
        .collect();
    assert_eq!(agents, vec!["claude-code"]);
    assert_eq!(v["history"]["accepted"], 12);
}

fn config_capture_mode(m: &Machine) -> String {
    let config: Value = serde_json::from_str(
        &fs::read_to_string(m.data.join("config").join("config.json")).unwrap(),
    )
    .unwrap();
    config["capture_mode"].as_str().unwrap().to_string()
}

#[test]
fn a_capture_mode_on_an_existing_database_is_applied_to_new_events_and_said_plainly() {
    let m = machine(true);
    let (ok, out, err) = attempt(
        &m.home,
        &m.data,
        &["--json", "setup", "--no-verify", "--no-backfill"],
    );
    assert!(ok, "{out}{err}");
    assert_eq!(json(&out)["database"]["capture_mode"], "local_semantic");
    assert_eq!(config_capture_mode(&m), "local_semantic");

    // A dry run says what it would do and writes nothing.
    let (ok, out, err) = attempt(
        &m.home,
        &m.data,
        &[
            "setup",
            "--dry-run",
            "--no-backfill",
            "--capture-mode",
            "metadata_only",
        ],
    );
    assert!(ok, "{out}{err}");
    assert!(
        out.contains("would change from local_semantic to metadata_only"),
        "{out}"
    );
    assert_eq!(config_capture_mode(&m), "local_semantic");

    // The real run changes it for events captured from now on, and the report
    // does not pretend the stored events were touched.
    let (ok, out, err) = attempt(
        &m.home,
        &m.data,
        &[
            "setup",
            "--no-verify",
            "--no-backfill",
            "--capture-mode",
            "metadata_only",
        ],
    );
    assert!(ok, "{out}{err}");
    assert!(
        out.contains("changed from local_semantic to metadata_only"),
        "{out}"
    );
    assert!(
        out.contains("events already stored keep the content"),
        "{out}"
    );
    assert!(
        out.contains("(metadata_only"),
        "the database line shows it: {out}"
    );
    assert_eq!(config_capture_mode(&m), "metadata_only");

    // JSON carries the same fact; repeating is quiet; asking for what is
    // already there is not a change.
    let (ok, out, err) = attempt(
        &m.home,
        &m.data,
        &[
            "--json",
            "setup",
            "--no-verify",
            "--no-backfill",
            "--capture-mode",
            "local_semantic",
        ],
    );
    assert!(ok, "{out}{err}");
    let v = json(&out);
    assert_eq!(v["database"]["capture_mode"], "local_semantic");
    assert_eq!(v["database"]["capture_mode_changed_from"], "metadata_only");
    let (ok, out, err) = attempt(
        &m.home,
        &m.data,
        &[
            "--json",
            "setup",
            "--no-verify",
            "--no-backfill",
            "--capture-mode",
            "local",
        ],
    );
    assert!(ok, "{out}{err}");
    let v = json(&out);
    assert_eq!(v["database"]["capture_mode"], "local_semantic");
    assert!(
        v["database"].get("capture_mode_changed_from").is_none(),
        "{v:#}"
    );

    // `init` is the same switch and says the same thing.
    let (ok, out, err) = attempt(
        &m.home,
        &m.data,
        &["init", "--capture-mode", "metadata_only"],
    );
    assert!(ok, "{out}{err}");
    assert!(out.contains("changed from local_semantic"), "{out}");
}

#[test]
fn a_capture_mode_that_is_not_one_is_refused_before_anything_is_written() {
    let m = machine(true);
    for bad in ["bogus", "metadata-onlyy", ""] {
        let (ok, out, err) = attempt(&m.home, &m.data, &["setup", "--capture-mode", bad]);
        assert!(!ok, "{bad:?} must not be accepted: {out}{err}");
        assert!(
            err.contains("unknown capture mode") && err.contains("metadata_only"),
            "{err}"
        );
    }
    assert!(
        !m.data.exists(),
        "nothing was written: the data directory is still absent"
    );
    let settings = fs::read_to_string(m.home.join(".claude/settings.json")).unwrap();
    assert!(!settings.contains("attempt"), "{settings}");
}

#[test]
fn explicit_otel_opt_outs_survive_setup_and_uninstall_and_setup_says_so() {
    let m = machine(false);
    fs::create_dir_all(m.home.join(".claude")).unwrap();
    let original = serde_json::json!({
        "env": {"OTEL_LOG_USER_PROMPTS": "0", "OTEL_METRICS_EXPORTER": "none"},
        "model": "opus",
    });
    fs::write(m.home.join(".claude/settings.json"), original.to_string()).unwrap();
    let (ok, out, err) = attempt(&m.home, &m.data, &["setup", "--no-verify", "--no-backfill"]);
    assert!(ok, "{out}{err}");
    assert!(out.contains("kept your OTEL_LOG_USER_PROMPTS=0"), "{out}");
    assert!(
        out.contains("kept your OTEL_METRICS_EXPORTER=none"),
        "{out}"
    );
    let after: Value =
        serde_json::from_str(&fs::read_to_string(m.home.join(".claude/settings.json")).unwrap())
            .unwrap();
    assert_eq!(after["env"]["OTEL_LOG_USER_PROMPTS"], "0");
    assert_eq!(after["env"]["OTEL_METRICS_EXPORTER"], "none");
    assert_eq!(after["env"]["OTEL_LOGS_EXPORTER"], "otlp");
    let (ok, out, err) = attempt(&m.home, &m.data, &["uninstall"]);
    assert!(ok, "{out}{err}");
    let back: Value =
        serde_json::from_str(&fs::read_to_string(m.home.join(".claude/settings.json")).unwrap())
            .unwrap();
    assert_eq!(back, original);
}
