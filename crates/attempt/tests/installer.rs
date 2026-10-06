//! The installer's reach into other tools' configuration, end to end: every
//! Claude Code config directory on a machine, `--claude-config-dir`,
//! `attempt uninstall`, and `attempt health` (what `attempt update` asks of a
//! new binary). Agents are fakes under a temporary HOME; the daemon step is
//! opted out with `ATTEMPTDB_NO_DAEMON` so no test registers (or removes) a
//! launchd agent or systemd unit.

use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn bare_path() -> String {
    if cfg!(windows) {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        format!("{root}\\System32")
    } else {
        "/usr/bin:/bin".into()
    }
}

/// Where an agent's configuration can be pointed away from the fake HOME.
/// `CODEX_HOME` is set to the fake HOME's own directory below; the others are
/// removed from every child.
const AGENT_CONFIG_VARS: [&str; 3] = [
    "CLAUDE_CONFIG_DIR",
    "CURSOR_CONFIG_DIR",
    "GEMINI_CONFIG_DIR",
];

struct Machine {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    data: PathBuf,
}

impl Machine {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        Self {
            data: tmp.path().join("data"),
            home,
            _tmp: tmp,
        }
    }

    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_attempt"));
        c.arg("--data-dir")
            .arg(&self.data)
            .env("PATH", bare_path())
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("CODEX_HOME", self.home.join(".codex"))
            .env("ATTEMPTDB_KEYRING", "off")
            .env("ATTEMPTDB_NO_DAEMON", "1")
            .env_remove("ATTEMPTDB_KEY_FILE")
            .env_remove("ATTEMPTDB_DIR")
            .env_remove("ATTEMPTDB_MANAGED_BY");
        // The developer running these tests may use a second Claude account
        // (or point any agent elsewhere): never let the real one leak into a
        // fake machine. `a_machine_never_sees_...` below proves it.
        for var in AGENT_CONFIG_VARS {
            c.env_remove(var);
        }
        c
    }

    fn run(&self, args: &[&str]) -> (bool, String, String) {
        let out = self.command().args(args).output().expect("run attempt");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    }

    fn claude(&self, dir: &str, settings: Option<&str>) -> PathBuf {
        let d = self.home.join(dir);
        fs::create_dir_all(d.join("projects")).unwrap();
        if let Some(s) = settings {
            fs::write(d.join("settings.json"), s).unwrap();
        }
        d
    }
}

fn json_of(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("not JSON ({e}):\n{text}"))
}

fn file_json(path: &Path) -> Value {
    json_of(&fs::read_to_string(path).unwrap())
}

fn actions(report: &Value) -> &Vec<Value> {
    report["hooks"]["actions"].as_array().unwrap()
}

fn action_for<'a>(report: &'a Value, dir: &Path) -> &'a Value {
    let want = dir.join("settings.json");
    actions(report)
        .iter()
        .find(|a| Path::new(a["config_path"].as_str().unwrap()) == want)
        .unwrap_or_else(|| panic!("no action for {}:\n{report:#}", dir.display()))
}

fn claude_entries(doctor: &Value) -> Vec<Value> {
    doctor["diagnosis"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["agent"] == "claude-code")
        .cloned()
        .collect()
}

#[test]
fn setup_wires_every_claude_config_dir_and_doctor_judges_each_one() {
    let m = Machine::new();
    let main = m.claude(".claude", Some(r#"{"model":"opus"}"#));
    let acct2 = m.claude(".claude-acct2", None);
    let (ok, out, err) = m.run(&["--json", "setup", "--no-verify"]);
    assert!(ok, "{out}{err}");
    let report = json_of(&out);
    assert_eq!(actions(&report).len(), 2, "{report:#}");
    for dir in [&main, &acct2] {
        assert_eq!(
            action_for(&report, dir)["outcome"]["kind"],
            "installed",
            "{report:#}"
        );
        assert!(file_json(&dir.join("settings.json"))["hooks"]["Stop"].is_array());
    }
    assert_eq!(
        file_json(&main.join("settings.json"))["model"],
        "opus",
        "the user's own settings survive"
    );
    assert_eq!(report["hooks"]["detected"], json!(["claude-code"]));
    let checks: Vec<_> = report["agents"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["agent"] == "claude-code")
        .collect();
    assert_eq!(checks.len(), 2, "one check per directory:\n{report:#}");

    // Again: nothing changes anywhere.
    let before = fs::read_to_string(acct2.join("settings.json")).unwrap();
    let (ok, out, err) = m.run(&["--json", "setup", "--no-verify"]);
    assert!(ok, "{out}{err}");
    let again = json_of(&out);
    for dir in [&main, &acct2] {
        assert_eq!(
            action_for(&again, dir)["outcome"]["kind"],
            "already_current"
        );
    }
    assert_eq!(
        fs::read_to_string(acct2.join("settings.json")).unwrap(),
        before
    );

    // Doctor lists each directory with its own state.
    let (ok, out, err) = m.run(&["--json", "doctor"]);
    assert!(ok, "{out}{err}");
    let entries = claude_entries(&json_of(&out));
    assert_eq!(entries.len(), 2, "{entries:#?}");
    for e in &entries {
        assert_eq!(e["state"], "configured", "{e:#}");
        assert!(e["config_dir"].is_string());
    }

    // A directory that loses its hooks is reported as such, and a human run
    // of doctor says so with a failing exit code, not "active" for the agent.
    fs::write(acct2.join("settings.json"), r#"{"permissions":{}}"#).unwrap();
    let (_, out, _) = m.run(&["--json", "doctor"]);
    let entries = claude_entries(&json_of(&out));
    let states: Vec<_> = entries
        .iter()
        .map(|e| {
            (
                e["config_dir"].as_str().unwrap().to_string(),
                e["state"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert!(
        states.contains(&(main.display().to_string(), "configured".into()))
            && states.contains(&(acct2.display().to_string(), "not_installed".into())),
        "{states:?}"
    );
    let (ok, out, _) = m.run(&["doctor"]);
    assert!(!ok, "a half-wired agent is a problem:\n{out}");
    assert!(out.contains("not captured"), "{out}");
}

#[test]
fn claude_config_dir_from_the_environment_is_wired_beside_the_default() {
    let m = Machine::new();
    let main = m.claude(".claude", Some("{}"));
    let work = m.home.join("work").join("claude-home");
    fs::create_dir_all(&work).unwrap();
    let out = m
        .command()
        .args(["--json", "setup", "--no-verify"])
        .env("CLAUDE_CONFIG_DIR", &work)
        .output()
        .unwrap();
    assert!(out.status.success());
    let report = json_of(&String::from_utf8_lossy(&out.stdout));
    assert_eq!(actions(&report).len(), 2, "{report:#}");
    assert_eq!(action_for(&report, &work)["outcome"]["kind"], "installed");
    assert_eq!(action_for(&report, &main)["outcome"]["kind"], "installed");
    // The directory the variable names is first.
    assert_eq!(
        Path::new(actions(&report)[0]["config_path"].as_str().unwrap()),
        work.join("settings.json")
    );
}

#[test]
fn claude_config_dir_flag_replaces_detection_and_never_creates_a_directory() {
    let m = Machine::new();
    let main = m.claude(".claude", Some(r#"{"model":"opus"}"#));
    let acct2 = m.claude(".claude-acct2", Some("{}"));
    let ci = m.home.join("ci").join("claude");
    fs::create_dir_all(&ci).unwrap();
    let typo = m.home.join("ci").join("claudee");
    let main_before = fs::read_to_string(main.join("settings.json")).unwrap();
    let (ok, out, err) = m.run(&[
        "--json",
        "setup",
        "--no-verify",
        "--claude-config-dir",
        ci.to_str().unwrap(),
        "--claude-config-dir",
        typo.to_str().unwrap(),
    ]);
    // A directory that is not there is an error to fix (exit 1) — but what
    // could be wired still was, and the report says which is which.
    assert!(!ok, "a typo must not exit 0: {out}{err}");
    let report = json_of(&out);
    assert_eq!(report["ok"], false, "{report:#}");
    assert!(
        report["problems"][0]
            .as_str()
            .unwrap()
            .contains("does not exist"),
        "{report:#}"
    );
    assert_eq!(action_for(&report, &ci)["outcome"]["kind"], "installed");
    let skipped = action_for(&report, &typo);
    assert_eq!(skipped["outcome"]["kind"], "skipped", "{report:#}");
    assert!(
        skipped["outcome"]["detail"]
            .as_str()
            .unwrap()
            .contains("not creating it")
    );
    assert!(!typo.exists(), "a directory that was not there is not made");
    assert_eq!(
        fs::read_to_string(main.join("settings.json")).unwrap(),
        main_before,
        "detection is replaced, so ~/.claude is left alone"
    );
    assert_eq!(
        fs::read_to_string(acct2.join("settings.json")).unwrap(),
        "{}"
    );

    // `hook install --dry-run` takes the same flag and writes nothing.
    let (ok, out, err) = m.run(&[
        "--json",
        "hook",
        "install",
        "--dry-run",
        "--claude-config-dir",
        acct2.to_str().unwrap(),
    ]);
    assert!(ok, "{out}{err}");
    let dry = json_of(&out);
    let acts = dry["actions"].as_array().unwrap();
    assert_eq!(acts.len(), 1, "{dry:#}");
    assert_eq!(acts[0]["outcome"]["kind"], "installed");
    assert_eq!(
        fs::read_to_string(acct2.join("settings.json")).unwrap(),
        "{}"
    );
}

/// The user's own settings, hooks of their own beside the ones we add, and a
/// hook of theirs on an event we also use.
const USERS_SETTINGS: &str = r#"{
  "model": "opus",
  "permissions": { "allow": ["Bash(ls:*)"] },
  "env": { "MY_VAR": "1" },
  "hooks": {
    "Stop": [ { "hooks": [ { "type": "command", "command": "/usr/local/bin/my-stop-hook" } ] } ],
    "PostToolUse": [ { "matcher": "Edit", "hooks": [ { "type": "command", "command": "/usr/local/bin/my-linter" } ] } ],
    "MyOwnEvent": [ { "hooks": [ { "type": "command", "command": "attempt status" } ] } ]
  }
}"#;

#[test]
fn uninstall_removes_only_what_setup_wrote_keeps_history_and_is_idempotent() {
    let m = Machine::new();
    let main = m.claude(".claude", Some(USERS_SETTINGS));
    let acct2 = m.claude(".claude-acct2", Some(USERS_SETTINGS));
    let original: Value = json_of(USERS_SETTINGS);
    let (ok, out, err) = m.run(&["--json", "setup"]);
    assert!(ok, "{out}{err}");
    for dir in [&main, &acct2] {
        let v = file_json(&dir.join("settings.json"));
        assert_ne!(v, original, "setup changed {}", dir.display());
        assert!(
            v["hooks"]["Stop"].as_array().unwrap().len() > 1,
            "ours sits beside the user's Stop hook"
        );
    }

    // History: one real hook event into the database.
    let mut hook = m
        .command()
        .args(["hook", "claude-code"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(
        hook.stdin.as_mut().unwrap(),
        include_bytes!("../../../fixtures/providers/claude_code/post_tool_use_bash_unknown.json"),
    )
    .unwrap();
    assert!(hook.wait().unwrap().success());
    let events = |m: &Machine| -> u64 {
        let (ok, out, err) = m.run(&["--json", "status"]);
        assert!(ok, "{out}{err}");
        let v = json_of(&out);
        v["events"]
            .as_u64()
            .or_else(|| v["total_events"].as_u64())
            .unwrap_or_else(|| panic!("no event count in status:\n{v:#}"))
    };
    let events_before = events(&m);
    assert!(events_before >= 1);

    // A dry run changes nothing.
    let snapshot = |dir: &Path| fs::read(dir.join("settings.json")).unwrap();
    let (main_installed, acct2_installed) = (snapshot(&main), snapshot(&acct2));
    let (ok, out, err) = m.run(&["uninstall", "--dry-run"]);
    assert!(ok, "{out}{err}");
    assert!(out.contains("would remove"), "{out}");
    assert_eq!(
        (snapshot(&main), snapshot(&acct2)),
        (main_installed, acct2_installed)
    );

    // The real thing: both directories go back to exactly what the user had,
    // key for key — their hooks, their env, their events.
    let (ok, out, err) = m.run(&["uninstall"]);
    assert!(ok, "{out}{err}");
    assert!(out.contains("hooks removed"), "{out}");
    assert!(
        out.contains("ATTEMPTDB_NO_DAEMON"),
        "the service manager is left alone when opted out: {out}"
    );
    for dir in [&main, &acct2] {
        assert_eq!(
            file_json(&dir.join("settings.json")),
            original,
            "{}",
            dir.display()
        );
    }
    assert!(!out.contains("FAILED"), "{out}");

    // History is kept, the database still opens.
    assert!(m.data.join("db").exists());
    assert_eq!(events(&m), events_before, "uninstall never touches history");

    // Idempotent: a second run finds nothing and rewrites nothing.
    let (main_after, acct2_after) = (snapshot(&main), snapshot(&acct2));
    let (ok, out, err) = m.run(&["uninstall"]);
    assert!(ok, "{out}{err}");
    assert!(out.contains("no hooks present"), "{out}");
    assert_eq!(
        (snapshot(&main), snapshot(&acct2)),
        (main_after, acct2_after)
    );

    // Only with --purge-data does data go, and only after confirmation.
    let (ok, _, err) = m.run(&["uninstall", "--purge-data"]);
    assert!(!ok, "no confirmation, no purge");
    assert!(err.contains("--yes"), "{err}");
    assert!(m.data.join("db").exists());
}

#[test]
fn uninstall_leaves_a_users_lookalike_hooks_and_unrelated_agents_alone() {
    let m = Machine::new();
    let lookalikes = r#"{
      "hooks": {
        "Stop": [ { "hooks": [
          { "type": "command", "command": "/opt/tools/attempt-notify hook claude-code" },
          { "type": "command", "command": "attempt status --json" },
          { "type": "command", "command": "echo attempt hook" }
        ] } ]
      }
    }"#;
    let main = m.claude(".claude", Some(lookalikes));
    let cursor = m.home.join(".cursor");
    fs::create_dir_all(&cursor).unwrap();
    fs::write(
        cursor.join("hooks.json"),
        r#"{"version":1,"hooks":{"stop":[{"command":"./mine.sh"}]}}"#,
    )
    .unwrap();
    let (ok, out, err) = m.run(&["--json", "setup", "--no-verify"]);
    assert!(ok, "{out}{err}");
    assert!(
        file_json(&cursor.join("hooks.json"))["hooks"]["stop"]
            .as_array()
            .unwrap()
            .len()
            > 1
    );
    let (ok, out, err) = m.run(&["uninstall"]);
    assert!(ok, "{out}{err}");
    assert_eq!(file_json(&main.join("settings.json")), json_of(lookalikes));
    assert_eq!(
        file_json(&cursor.join("hooks.json")),
        json!({"version":1,"hooks":{"stop":[{"command":"./mine.sh"}]}})
    );
}

#[test]
fn health_answers_cheaply_and_fails_when_the_database_cannot_be_read() {
    let m = Machine::new();
    // No database: healthy, and it says so.
    let (ok, out, err) = m.run(&["--json", "health"]);
    assert!(ok, "{out}{err}");
    let v = json_of(&out);
    assert_eq!(v["ok"], true);
    assert_eq!(v["database"]["state"], "absent");
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));

    m.claude(".claude", Some("{}"));
    let (ok, out, err) = m.run(&["--json", "setup", "--no-verify"]);
    assert!(ok, "{out}{err}");
    let (ok, out, err) = m.run(&["--json", "health"]);
    assert!(ok, "{out}{err}");
    let v = json_of(&out);
    assert_eq!(v["database"]["state"], "ok", "{v:#}");
    assert!(v["database"]["detail"]["generation"].is_u64());

    // A manifest this binary cannot read: the failure an update must catch.
    let manifests = m.data.join("db").join(".attemptdb").join("manifest");
    for entry in fs::read_dir(&manifests).unwrap().flatten() {
        fs::write(entry.path(), "{ not a manifest").unwrap();
    }
    let (ok, out, _) = m.run(&["--json", "health"]);
    assert!(!ok, "{out}");
    let v = json_of(&out);
    assert_eq!(v["ok"], false);
    assert_eq!(v["database"]["state"], "unreadable", "{v:#}");
}

#[test]
fn update_says_who_owns_a_managed_install_and_makes_no_request() {
    let m = Machine::new();
    let out = m
        .command()
        .args(["update", "--check"])
        .env("ATTEMPTDB_MANAGED_BY", "ansible")
        // Nothing may be asked of the network: point it nowhere.
        .env("ATTEMPTDB_UPDATE_API", "http://127.0.0.1:9")
        .env("ATTEMPTDB_UPDATE_DOWNLOAD", "http://127.0.0.1:9")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("managed by ansible") && text.contains("ATTEMPTDB_MANAGED_BY"),
        "{text}"
    );

    // Doctor says so too, instead of reporting a check that never happens.
    let out = m
        .command()
        .args(["--json", "doctor"])
        .env("ATTEMPTDB_MANAGED_BY", "ansible")
        .output()
        .unwrap();
    let v = json_of(&String::from_utf8_lossy(&out.stdout));
    assert_eq!(v["update"]["managed_by"], "ansible", "{v:#}");
    assert_eq!(v["update"]["auto_update"], "off");
}

#[test]
fn a_machine_never_sees_the_developers_agent_config_variables() {
    let m = Machine::new();
    let c = m.command();
    let envs: std::collections::HashMap<_, _> = c
        .get_envs()
        .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|v| v.to_owned())))
        .collect();
    for var in AGENT_CONFIG_VARS {
        assert_eq!(
            envs.get(var),
            Some(&None),
            "{var} must be removed, not inherited"
        );
    }
    assert_eq!(
        envs.get("CODEX_HOME").cloned().flatten().as_deref(),
        Some(m.home.join(".codex").as_os_str())
    );
    assert_eq!(
        envs.get("ATTEMPTDB_NO_DAEMON")
            .cloned()
            .flatten()
            .as_deref(),
        Some(std::ffi::OsStr::new("1")),
        "a test never reaches the service manager"
    );
}

#[test]
fn a_typo_in_claude_config_dir_is_an_error_not_no_agents() {
    let m = Machine::new();
    let typo = m.home.join("claudee");
    for json in [false, true] {
        let mut args = vec![
            "setup",
            "--no-verify",
            "--no-backfill",
            "--claude-config-dir",
        ];
        let typo_str = typo.to_str().unwrap().to_string();
        args.push(&typo_str);
        if json {
            args.insert(0, "--json");
        }
        let (ok, out, err) = m.run(&args);
        assert!(
            !ok,
            "a directory that is not there must not exit 0: {out}{err}"
        );
        assert!(out.contains("does not exist"), "{out}");
        assert!(!out.contains("no coding agents detected"), "{out}");
        assert!(!typo.exists(), "and it is not made");
        if json {
            let v = json_of(&out);
            assert_eq!(v["ok"], false);
            assert!(
                v["problems"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|p| p.as_str().unwrap().contains("does not exist")),
                "{v:#}"
            );
        }
    }
    // `hook install` and `uninstall` say the same.
    for args in [vec!["hook", "install", "--no-verify"], vec!["uninstall"]] {
        let mut full = args.clone();
        let typo_str = typo.to_str().unwrap().to_string();
        full.extend(["--claude-config-dir", &typo_str]);
        let (ok, out, err) = m.run(&full);
        assert!(!ok, "{full:?}: {out}{err}");
        assert!(out.contains("does not exist"), "{full:?}: {out}");
    }
}

#[cfg(unix)]
#[test]
fn claude_config_dir_pointing_nowhere_is_not_created_by_setup() {
    let m = Machine::new();
    // A `claude` launcher on PATH is evidence of the agent, not of the directory.
    let bin = m.home.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let launcher = bin.join("claude");
    fs::write(&launcher, "#!/bin/sh\nexit 0\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&launcher, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let nowhere = m.home.join("nowhere").join("claude-config");
    let out = m
        .command()
        .args(["setup", "--no-verify", "--no-backfill"])
        .env("CLAUDE_CONFIG_DIR", &nowhere)
        .env("PATH", format!("{}:{}", bin.display(), bare_path()))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !nowhere.exists(),
        "setup made a directory an `export` named"
    );
    assert!(!nowhere.parent().unwrap().exists());
    assert!(
        text.contains("CLAUDE_CONFIG_DIR is set to") && text.contains("not creating it"),
        "{text}"
    );
    // Doctor names the variable too, instead of "no .claude directory".
    let doctor = m
        .command()
        .args(["doctor"])
        .env("CLAUDE_CONFIG_DIR", &nowhere)
        .env("PATH", format!("{}:{}", bin.display(), bare_path()))
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&doctor.stdout).contains("CLAUDE_CONFIG_DIR is set to"),
        "{}",
        String::from_utf8_lossy(&doctor.stdout)
    );
}

#[test]
fn hook_install_json_is_one_json_document_and_a_missing_receiver_is_a_warning() {
    let m = Machine::new();
    m.claude(".claude", Some("{}"));
    let (ok, out, err) = m.run(&["init"]);
    assert!(ok, "{out}{err}");
    // ATTEMPTDB_NO_DAEMON is set by the machine: the receiver cannot start.
    let (ok, out, err) = m.run(&["--json", "hook", "install"]);
    assert!(
        ok,
        "the hooks are written, so the install is complete: {out}{err}"
    );
    let v = json_of(&out); // the whole of stdout must be one document
    let action = &actions_of(&v)[0];
    assert_eq!(action["outcome"]["kind"], "installed", "{v:#}");
    assert_eq!(v["capture_tests"][0]["ok"], true, "{v:#}");
    let warnings = v["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1, "{v:#}");
    assert!(
        warnings[0]
            .as_str()
            .unwrap()
            .contains("ATTEMPTDB_NO_DAEMON")
            && warnings[0]
                .as_str()
                .unwrap()
                .contains("attempt daemon install"),
        "{v:#}"
    );

    // The text form says the same thing as a note, and does not call it FAILED.
    let (ok, out, err) = m.run(&["hook", "install"]);
    assert!(ok, "{out}{err}");
    assert!(!out.contains("FAILED"), "{out}");
    assert!(
        out.contains("note: the local OpenTelemetry receiver was not started"),
        "{out}"
    );
}

fn actions_of(report: &Value) -> &Vec<Value> {
    report["actions"].as_array().unwrap()
}

#[test]
fn uninstall_exits_1_when_an_agent_could_not_be_cleaned_and_json_is_one_document() {
    let m = Machine::new();
    let dir = m.claude(".claude", Some(r#"{"model":"opus"}"#));
    let (ok, out, err) = m.run(&["setup", "--no-verify", "--no-backfill"]);
    assert!(ok, "{out}{err}");
    assert!(dir.join("settings.json").is_file());

    // `--json` is honoured and the whole of stdout is the report.
    let (ok, out, err) = m.run(&["--json", "uninstall", "--dry-run"]);
    assert!(ok, "{out}{err}");
    let v = json_of(&out);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["ok"], true);
    assert_eq!(v["hooks"][0]["outcome"]["kind"], "removed");

    // Make the settings directory unwritable: the file cannot be replaced.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        let probe = dir.join(".probe");
        let can_write_anyway = fs::write(&probe, "").is_ok(); // root ignores modes
        let _ = fs::remove_file(&probe);
        if !can_write_anyway {
            let (ok, out, _) = m.run(&["uninstall"]);
            assert!(!ok, "an agent that FAILED must not exit 0: {out}");
            assert!(out.contains("FAILED"), "{out}");
            assert!(out.contains("problems:"), "{out}");
            let (ok, out, _) = m.run(&["--json", "uninstall"]);
            assert!(!ok, "{out}");
            let v = json_of(&out);
            assert_eq!(v["ok"], false, "{v:#}");
            assert!(!v["problems"].as_array().unwrap().is_empty());
            assert_eq!(v["hooks"][0]["outcome"]["kind"], "failed");
        }
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    // With the directory writable again the same command succeeds.
    let (ok, out, err) = m.run(&["uninstall"]);
    assert!(ok, "{out}{err}");
}

#[cfg(unix)]
#[test]
fn uninstall_restores_the_settings_files_permissions_and_keeps_one_backup() {
    use std::os::unix::fs::PermissionsExt;
    let m = Machine::new();
    let dir = m.claude(".claude", Some(r#"{"model":"opus"}"#));
    let settings = dir.join("settings.json");
    fs::set_permissions(&settings, fs::Permissions::from_mode(0o644)).unwrap();
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
    for _ in 0..3 {
        // Each run that changes something leaves a backup behind.
        let (ok, out, err) = m.run(&["setup", "--no-verify", "--no-backfill"]);
        assert!(ok, "{out}{err}");
        let mut v = file_json(&settings);
        v["theme"] = json!(format!("t{}", fs::read_dir(&dir).unwrap().count()));
        fs::write(&settings, v.to_string()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
    }
    assert_eq!(
        mode(&settings),
        0o600,
        "the bearer token makes it private while installed"
    );
    let backups = |dir: &Path| {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".attemptdb.bak-"))
            .count()
    };
    assert!(backups(&dir) > 1);
    let (ok, out, err) = m.run(&["uninstall"]);
    assert!(ok, "{out}{err}");
    assert_eq!(mode(&settings), 0o644, "the file is as visible as it was");
    assert_eq!(
        backups(&dir),
        1,
        "one backup is enough to undo an uninstall"
    );
    assert!(out.contains("the newest backup is kept at"), "{out}");
}

#[test]
fn four_first_time_setups_at_once_all_succeed() {
    let m = Machine::new();
    m.claude(".claude", Some("{}"));
    let runs: Vec<_> = (0..4)
        .map(|_| {
            let mut c = m.command();
            c.args(["--json", "setup", "--no-verify", "--no-backfill"]);
            std::thread::spawn(move || c.output().unwrap())
        })
        .collect();
    for (i, run) in runs.into_iter().enumerate() {
        let out = run.join().unwrap();
        assert!(
            out.status.success(),
            "run {i} failed:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let v = file_json(&m.data.join("config").join("config.json"));
    assert_eq!(v["capture_mode"], "local_semantic");
    let leftovers: Vec<_> = fs::read_dir(m.data.join("config"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

/// A Homebrew-shaped prefix inside the target directory (so the real `attempt`
/// can be hard-linked into it): `bin/attempt` and `bin/attempt-hook` are links
/// into `Cellar/attemptdb/<version>/bin`.
#[cfg(unix)]
struct Prefix {
    root: tempfile::TempDir,
}

#[cfg(unix)]
impl Prefix {
    fn new() -> Self {
        let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let prefix = Self { root };
        prefix.install("0.2.14");
        prefix
    }

    fn dir(&self) -> PathBuf {
        fs::canonicalize(self.root.path()).unwrap()
    }

    /// Install a version into the Cellar and point `bin` at it.
    fn install(&self, version: &str) {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let cellar = self
            .dir()
            .join("Cellar/attemptdb")
            .join(version)
            .join("bin");
        fs::create_dir_all(&cellar).unwrap();
        fs::hard_link(env!("CARGO_BIN_EXE_attempt"), cellar.join("attempt")).unwrap();
        fs::write(cellar.join("attempt-hook"), "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(
            cellar.join("attempt-hook"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fs::create_dir_all(self.dir().join("bin")).unwrap();
        for name in ["attempt", "attempt-hook"] {
            let link = self.dir().join("bin").join(name);
            let _ = fs::remove_file(&link);
            symlink(format!("../Cellar/attemptdb/{version}/bin/{name}"), &link).unwrap();
        }
    }

    fn remove(&self, version: &str) {
        fs::remove_dir_all(self.dir().join("Cellar/attemptdb").join(version)).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn hooks_name_the_stable_link_not_the_cellar_so_an_upgrade_cannot_break_them() {
    let m = Machine::new();
    let dir = m.claude(".claude", Some("{}"));
    let prefix = Prefix::new();
    let attempt = |m: &Machine, args: &[&str]| {
        let out = Command::new(prefix.dir().join("bin/attempt"))
            .arg("--data-dir")
            .arg(&m.data)
            .args(args)
            .env("PATH", bare_path())
            .env("HOME", &m.home)
            .env("CODEX_HOME", m.home.join(".codex"))
            .env("ATTEMPTDB_KEYRING", "off")
            .env("ATTEMPTDB_NO_DAEMON", "1")
            // Nothing may be asked of the network.
            .env("ATTEMPTDB_UPDATE_API", "http://127.0.0.1:9")
            .env("ATTEMPTDB_UPDATE_DOWNLOAD", "http://127.0.0.1:9")
            .env_remove("CLAUDE_CONFIG_DIR")
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    };
    let (ok, out, err) = attempt(&m, &["--json", "setup", "--no-verify", "--no-backfill"]);
    assert!(ok, "{out}{err}");
    let report = json_of(&out);
    let stable_hook = prefix.dir().join("bin/attempt-hook");
    assert_eq!(
        Path::new(report["hook_binary"].as_str().unwrap()),
        stable_hook,
        "{report:#}"
    );
    let settings = fs::read_to_string(dir.join("settings.json")).unwrap();
    assert!(
        settings.contains(&stable_hook.display().to_string()),
        "the hook command names the link the package manager keeps current:\n{settings}"
    );
    assert!(!settings.contains("Cellar"), "{settings}");

    // The stable link is for config files only. `attempt update` still sees the
    // real file under the Cellar and leaves a package-managed install alone,
    // instead of replacing the link `brew` keeps.
    let (ok, out, _) = attempt(&m, &["update", "--check"]);
    assert!(!ok, "{out}");
    assert!(out.contains("brew upgrade"), "{out}");
    assert!(
        fs::symlink_metadata(prefix.dir().join("bin/attempt"))
            .unwrap()
            .file_type()
            .is_symlink()
    );

    // `brew upgrade` + `brew cleanup`: the new version is linked, the old one is gone.
    prefix.install("0.2.15");
    prefix.remove("0.2.14");
    assert!(
        stable_hook.is_file(),
        "the hook command still names a binary"
    );
    let (_, out, _) = attempt(&m, &["--json", "doctor"]);
    let doctor = json_of(&out);
    let claude = &claude_entries(&doctor)[0];
    assert_ne!(claude["state"], "stale", "{claude:#}");
    assert!(
        claude["notes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| !n.as_str().unwrap().contains("does not exist")),
        "{claude:#}"
    );
    // Setup again after the upgrade changes nothing: the entries are current.
    let (ok, out, err) = attempt(&m, &["--json", "setup", "--no-verify", "--no-backfill"]);
    assert!(ok, "{out}{err}");
    assert_eq!(
        actions(&json_of(&out))[0]["outcome"]["kind"],
        "already_current"
    );
}

#[test]
fn doctor_on_wiring_from_an_older_release_says_how_to_fix_it_and_collapses_codex_trust_lines() {
    let m = Machine::new();
    let claude = m.claude(".claude", Some("{}"));
    let codex = m.home.join(".codex");
    fs::create_dir_all(&codex).unwrap();
    let (ok, out, err) = m.run(&["setup", "--no-verify", "--no-backfill"]);
    assert!(ok, "{out}{err}");

    // What 0.2.13 left behind: an event that no longer exists, one missing.
    let mut settings = file_json(&claude.join("settings.json"));
    let hooks = settings["hooks"].as_object_mut().unwrap();
    let (first, _) = hooks
        .iter()
        .next()
        .map(|(k, v)| (k.clone(), v.clone()))
        .unwrap();
    let moved = hooks.remove(&first).unwrap();
    hooks.insert("RetiredEvent".into(), moved);
    fs::write(claude.join("settings.json"), settings.to_string()).unwrap();

    let (ok, out, _) = m.run(&["doctor"]);
    assert!(!ok, "a stale config still exits 1: {out}");
    assert!(out.contains("stale"), "{out}");
    assert!(
        out.contains("fix: run `attempt setup` to refresh"),
        "doctor must say what to do: {out}"
    );
    // Codex was never trusted: one line says so, with the count, not one per event.
    let untrusted: Vec<&str> = out
        .lines()
        .filter(|l| l.contains("not yet trusted"))
        .collect();
    assert_eq!(untrusted.len(), 1, "{out}");
    assert!(untrusted[0].contains("all "), "{}", untrusted[0]);
    assert!(
        out.contains("fix: approve the new entries inside the agent"),
        "{out}"
    );

    // JSON carries the same advice.
    let (_, out, _) = m.run(&["--json", "doctor"]);
    let v = json_of(&out);
    let fixes = v["fixes"].as_array().unwrap();
    assert!(
        fixes
            .iter()
            .any(|f| f["agent"] == "claude-code"
                && f["fix"].as_str().unwrap().contains("attempt setup")),
        "{v:#}"
    );

    // Doing what it says clears the verdict.
    let (ok, out, err) = m.run(&["setup", "--no-verify", "--no-backfill"]);
    assert!(ok, "{out}{err}");
    let (_, out, _) = m.run(&["--json", "doctor"]);
    let claude_state = claude_entries(&json_of(&out))[0]["state"].clone();
    assert_ne!(claude_state, "stale");
}

#[test]
fn gemini_settings_with_comments_are_refused_by_name_and_left_untouched() {
    let m = Machine::new();
    let gemini = m.home.join(".gemini");
    fs::create_dir_all(&gemini).unwrap();
    let original = "// my settings\n{\n  \"theme\": \"dark\" // keep\n}\n";
    fs::write(gemini.join("settings.json"), original).unwrap();
    let (ok, out, err) = m.run(&["setup", "--no-verify", "--no-backfill"]);
    assert!(
        !ok,
        "an agent that could not be wired is a problem: {out}{err}"
    );
    assert!(out.contains("contains comments"), "{out}");
    assert!(out.contains("nothing was changed"), "{out}");
    assert!(!out.contains("key must be a string"), "{out}");
    assert_eq!(
        fs::read_to_string(gemini.join("settings.json")).unwrap(),
        original
    );
    // On Windows the config lock file is left in place by design.
    let names: Vec<_> = fs::read_dir(&gemini)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| !(cfg!(windows) && name == "settings.json.attemptdb.lock"))
        .collect();
    assert_eq!(
        names,
        vec!["settings.json".to_string()],
        "no backup, lock or temp file"
    );
}

#[test]
fn a_byte_order_mark_in_settings_is_accepted_kept_and_restored_exactly() {
    let m = Machine::new();
    let dir = m.claude(".claude", None);
    let original = "\u{feff}{\n  \"model\": \"opus\"\n}\n";
    fs::write(dir.join("settings.json"), original).unwrap();
    let (ok, out, err) = m.run(&["setup", "--no-verify", "--no-backfill"]);
    assert!(ok, "a BOM is not an error: {out}{err}");
    let bytes = fs::read(dir.join("settings.json")).unwrap();
    assert!(bytes.starts_with(b"\xEF\xBB\xBF"), "the mark is kept");
    let v: Value = serde_json::from_slice(&bytes[3..]).unwrap();
    assert!(v["hooks"].is_object() && v["env"].is_object(), "{v:#}");
    let (ok, out, err) = m.run(&["uninstall"]);
    assert!(ok, "{out}{err}");
    assert_eq!(
        fs::read_to_string(dir.join("settings.json")).unwrap(),
        original
    );
}
