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

fn attempt(home: &Path, data_dir: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_attempt"))
        .arg("--data-dir")
        .arg(data_dir)
        .args(args)
        .env("PATH", bare_path())
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("CODEX_HOME", home.join(".codex"))
        .env("ATTEMPTDB_KEYRING", "off")
        .env("ATTEMPTDB_NO_DAEMON", "1")
        .env_remove("ATTEMPTDB_KEY_FILE")
        .env_remove("ATTEMPTDB_DIR")
        .env_remove("CLAUDE_CONFIG_DIR")
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

    // Again: the database exists, the hooks are current, the capture mode
    // requested for a new database is not applied to an existing one.
    let (ok, out, err) = attempt(
        &m.home,
        &m.data,
        &["--json", "setup", "--capture-mode", "local_semantic"],
    );
    assert!(ok, "{out}{err}");
    let v = json(&out);
    assert_eq!(v["database"]["existed"], true);
    assert_eq!(v["database"]["created"], false);
    assert_eq!(v["database"]["capture_mode"], "metadata_only");
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
