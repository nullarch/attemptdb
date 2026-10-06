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
            .env_remove("ATTEMPTDB_MANAGED_BY")
            // The developer running these tests may use a second Claude
            // account: never let the real one leak into a fake machine.
            .env_remove("CLAUDE_CONFIG_DIR");
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
    assert!(ok, "a typo is reported, not fatal: {out}{err}");
    let report = json_of(&out);
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
