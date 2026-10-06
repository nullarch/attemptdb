//! What `attempt doctor` and `attempt status` say about the capture path
//! itself: an unusable config (capture fails closed to metadata-only), a
//! project-local database that is not trusted, a key that is required but
//! missing. Fakes under a temporary HOME, no daemon, no OS key store.

use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn bare_path() -> String {
    if cfg!(windows) {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        format!("{root}\\System32")
    } else {
        "/usr/bin:/bin".into()
    }
}

/// Variables that point an agent (and so `attempt`) at its real, per-user
/// configuration. A temporary HOME does not protect against them.
const AGENT_ENV: [&str; 4] = [
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "CURSOR_CONFIG_DIR",
    "GEMINI_CONFIG_DIR",
];

struct Machine {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    data: PathBuf,
    cwd: PathBuf,
}

fn machine() -> Machine {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cwd = tmp.path().join("work");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    Machine {
        data: tmp.path().join("data"),
        home,
        cwd,
        _tmp: tmp,
    }
}

impl Machine {
    /// Everything a child process of these tests inherits is decided here:
    /// a temporary HOME, a PATH with no agent launchers, no agent config
    /// variables, no OS key store, no daemon.
    fn isolate(&self, cmd: &mut Command) {
        cmd.env("PATH", bare_path())
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("ATTEMPTDB_KEYRING", "off")
            .env("ATTEMPTDB_NO_DAEMON", "1");
        for var in AGENT_ENV {
            cmd.env_remove(var);
        }
        for var in [
            "ATTEMPTDB_KEY_FILE",
            "ATTEMPTDB_PASSPHRASE",
            "ATTEMPTDB_DIR",
            "ATTEMPTDB_DATA_DIR",
        ] {
            cmd.env_remove(var);
        }
    }

    fn attempt_in(&self, cwd: &Path, args: &[&str]) -> (Option<i32>, String, String) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_attempt"));
        cmd.arg("--data-dir")
            .arg(&self.data)
            .args(args)
            .current_dir(cwd);
        self.isolate(&mut cmd);
        let out = cmd.output().expect("run attempt");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    }

    fn attempt(&self, args: &[&str]) -> (Option<i32>, String, String) {
        self.attempt_in(&self.cwd, args)
    }

    fn config(&self) -> PathBuf {
        self.data.join("config").join("config.json")
    }

    /// One real hook invocation the way an agent runs it: payload on stdin.
    fn hook(&self, provider: &str, payload: &Value) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_attempt"));
        cmd.arg("--data-dir")
            .arg(&self.data)
            .args(["hook", provider])
            .current_dir(&self.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        self.isolate(&mut cmd);
        let mut child = cmd.spawn().expect("run the hook");
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success(), "a hook always exits 0");
    }

    fn prompt(&self, session: &str, text: &str) -> Value {
        serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": session,
            "cwd": self.cwd,
            "prompt": text,
        })
    }
}

/// The harness above is the only thing between these tests and the owner's
/// real `~/.claude*`: prove it hides what an inheriting shell would pass.
#[cfg(unix)]
#[test]
fn the_harness_hides_the_owners_agent_config_from_children() {
    let m = machine();
    let mut cmd = Command::new("/usr/bin/env");
    // What a shell with the owner's configuration hands down.
    for var in AGENT_ENV {
        cmd.env(var, "/owner/real/agent/config");
    }
    cmd.env("ATTEMPTDB_DIR", "/owner/real/db");
    m.isolate(&mut cmd);
    let out = cmd.output().unwrap();
    let env = String::from_utf8_lossy(&out.stdout);
    for var in AGENT_ENV.into_iter().chain(["ATTEMPTDB_DIR"]) {
        assert!(
            !env.lines().any(|l| l.starts_with(&format!("{var}="))),
            "{var} reached the child:\n{env}"
        );
    }
    assert!(env.contains(&format!("HOME={}", m.home.display())), "{env}");
    assert!(env.contains("ATTEMPTDB_KEYRING=off"), "{env}");
}

#[test]
fn doctor_and_status_say_when_the_config_is_unusable() {
    let m = machine();
    let (code, out, err) = m.attempt(&["init", "--no-encryption"]);
    assert_eq!(code, Some(0), "{out}{err}");
    std::fs::write(m.config(), br#"{"capture_mode":"metadata-only"}"#).unwrap();

    let (code, out, _) = m.attempt(&["doctor"]);
    assert_eq!(code, Some(1), "a config being ignored is a problem:\n{out}");
    assert!(out.contains("capture mode metadata_only"), "{out}");
    let line = out.lines().find(|l| l.starts_with("config ")).expect(&out);
    assert!(
        line.contains("PROBLEM") && line.contains("metadata-only"),
        "{line}"
    );

    let (_, out, _) = m.attempt(&["--json", "doctor"]);
    let json: Value = serde_json::from_str(&out).unwrap();
    assert!(
        json["capture"]["config_error"]
            .as_str()
            .unwrap()
            .contains("metadata-only"),
        "{out}"
    );
    assert_eq!(json["capture_mode"], "metadata_only");

    let (_, out, _) = m.attempt(&["status"]);
    assert!(
        out.contains("warning: ") && out.contains("capturing metadata only"),
        "{out}"
    );

    // Fixing the file clears it: nothing is remembered.
    std::fs::write(m.config(), br#"{"capture_mode":"metadata_only"}"#).unwrap();
    let (_, out, _) = m.attempt(&["doctor"]);
    assert!(!out.lines().any(|l| l.starts_with("config ")), "{out}");
}

#[test]
fn init_over_a_broken_config_keeps_the_original() {
    let m = machine();
    m.attempt(&["init", "--no-encryption"]);
    std::fs::write(m.config(), b"{ \"capture_mode\": \"local_semantic\", }").unwrap();
    let (code, out, err) = m.attempt(&["init", "--no-encryption"]);
    assert_eq!(code, Some(0), "{out}{err}");
    assert!(err.contains("kept it as"), "{err}");
    let kept: Vec<_> = std::fs::read_dir(m.config().parent().unwrap())
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .contains("config.json.invalid-")
        })
        .collect();
    assert_eq!(kept.len(), 1);
    // And the new file is the fail-closed one, stated in the summary.
    assert!(out.contains("capture mode  metadata_only"), "{out}");
}

#[cfg(unix)]
#[test]
fn doctor_lists_a_project_local_database_it_will_not_use() {
    use std::os::unix::fs::symlink;
    let m = machine();
    let repo = m.cwd.join("cloned");
    std::fs::create_dir_all(&repo).unwrap();
    let (code, out, err) = m.attempt_in(&repo, &["init", "--local", "--no-encryption"]);
    assert_eq!(code, Some(0), "{out}{err}");
    let (_, out, _) = m.attempt_in(&repo, &["doctor"]);
    assert!(
        !out.contains("local db "),
        "a database this user made is trusted:\n{out}"
    );

    // What a cloned repository can carry.
    let victim = m.cwd.join("victim.txt");
    std::fs::write(&victim, "precious").unwrap();
    symlink(
        &victim,
        repo.join(".attemptdb")
            .join("spool")
            .join("inbox.spool.committed.tmp"),
    )
    .unwrap();
    let (_, out, _) = m.attempt_in(&repo, &["doctor"]);
    let line = out
        .lines()
        .find(|l| l.starts_with("local db "))
        .expect(&out);
    assert!(
        line.contains("ignored") && line.contains("symbolic link"),
        "{line}"
    );
    assert!(line.contains("your own database"), "{line}");
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
}

#[test]
fn doctor_reports_a_required_key_that_is_missing() {
    let m = machine();
    let (code, out, err) = m.attempt(&["init", "--no-encryption"]);
    assert_eq!(code, Some(0), "{out}{err}");
    let mut config: Value = serde_json::from_slice(&std::fs::read(m.config()).unwrap()).unwrap();
    config["encryption"] = Value::String("required".into());
    std::fs::write(m.config(), serde_json::to_vec(&config).unwrap()).unwrap();
    // `status` opens the writer, which records the state; doctor reads it.
    let (_, out, _) = m.attempt(&["status"]);
    assert!(out.contains("encryption is required"), "{out}");
    let (code, out, _) = m.attempt(&["doctor"]);
    assert_eq!(code, Some(1), "{out}");
    let line = out
        .lines()
        .find(|l| l.starts_with("encryption "))
        .expect(&out);
    assert!(
        line.contains("PROBLEM") && line.contains("without their content"),
        "{line}"
    );
}

// ---------------------------------------------------------------------------
// Regressions found by the pre-release bug hunt.
// ---------------------------------------------------------------------------

/// A tool call whose headers end in `Authorization: Bearer ` (an empty token
/// variable) panicked the masker; the spool file stayed claimed, so every read
/// panicked again until the file was deleted by hand.
#[test]
fn a_header_that_used_to_panic_the_masker_is_stored_and_reads_keep_working() {
    let m = machine();
    let (code, out, err) = m.attempt(&["init", "--no-encryption"]);
    assert_eq!(code, Some(0), "{out}{err}");
    m.hook(
        "claude-code",
        &serde_json::json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "mcp__http__request",
            "tool_use_id": "t1",
            "session_id": "s1",
            "cwd": m.cwd,
            "tool_input": {"headers": {"Authorization": "Bearer "}},
            "tool_response": {"status": 401},
        }),
    );
    for _ in 0..2 {
        let (code, out, err) = m.attempt(&["status"]);
        assert_eq!(code, Some(0), "{out}{err}");
        assert!(!err.contains("panicked"), "{err}");
        assert!(
            out.contains("events        1 "),
            "the event is stored:\n{out}"
        );
    }
    let spool = m.data.join("db").join(".attemptdb").join("spool");
    let claimed = std::fs::read_dir(&spool)
        .map(|d| {
            d.filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().starts_with("claimed-"))
                .count()
        })
        .unwrap_or(0);
    assert_eq!(claimed, 0, "no poisoned spool file is left behind");
}

/// Bare `attempt import` (and `snapshot export`) drained the spool without
/// the content gate: secrets were stored in the clear.
#[test]
fn bare_import_masks_secrets_like_the_daemon_does() {
    let m = machine();
    let (code, out, err) = m.attempt(&["init", "--no-encryption"]);
    assert_eq!(code, Some(0), "{out}{err}");
    m.hook(
        "claude-code",
        &m.prompt("s1", "deploy with DB_PASSWORD=hunter2abc now"),
    );
    let (code, out, err) = m.attempt(&["import"]);
    assert_eq!(code, Some(0), "{out}{err}");
    let (_, out, _) = m.attempt(&[
        "--json",
        "query",
        "--all-projects",
        "SELECT content_json FROM events WHERE kind = 'prompt_submitted'",
    ]);
    assert!(out.contains("REDACTED"), "{out}");
    assert!(!out.contains("hunter2abc"), "the secret was stored:\n{out}");
}

/// `--purge-data` on a directory the user named deleted the whole directory,
/// their own files included.
#[test]
fn purge_data_leaves_the_files_in_a_directory_you_named() {
    let m = machine();
    std::fs::create_dir_all(m.data.join("sub")).unwrap();
    std::fs::write(m.data.join("notes.txt"), "mine").unwrap();
    std::fs::write(m.data.join("sub").join("thesis.docx"), "mine too").unwrap();
    let (code, out, err) = m.attempt(&["init", "--no-encryption"]);
    assert_eq!(code, Some(0), "{out}{err}");
    assert!(m.data.join("db").exists(), "init made the database");

    let (code, out, err) = m.attempt(&["uninstall", "--purge-data", "--dry-run"]);
    assert_eq!(code, Some(0), "{out}{err}");
    assert!(out.contains("and leave in place"), "{out}");
    assert!(
        m.data.join("notes.txt").exists(),
        "a dry run deletes nothing"
    );

    let (code, out, err) = m.attempt(&["uninstall", "--purge-data", "--yes"]);
    assert_eq!(code, Some(0), "{out}{err}");
    assert!(!m.data.join("db").exists(), "the database is gone:\n{out}");
    assert_eq!(
        std::fs::read_to_string(m.data.join("notes.txt")).unwrap(),
        "mine",
        "{out}"
    );
    assert!(m.data.join("sub").join("thesis.docx").exists(), "{out}");
}

/// `--db <project>/.attemptdb --purge-data` purged that one database and also
/// the per-user data directory (the global database and the keys).
#[test]
fn purge_data_with_an_explicit_database_touches_only_that_database() {
    let m = machine();
    let (code, out, err) = m.attempt(&["init", "--no-encryption"]);
    assert_eq!(code, Some(0), "{out}{err}");
    let user_db = m.data.join("db");
    assert!(user_db.exists());
    let project_db = m.cwd.join(".attemptdb");
    let project_db_arg = project_db.to_string_lossy().to_string();
    let (code, out, err) = m.attempt(&["--db", &project_db_arg, "init", "--no-encryption"]);
    assert_eq!(code, Some(0), "{out}{err}");
    assert!(project_db.exists());

    let (code, out, err) = m.attempt(&[
        "--db",
        &project_db_arg,
        "uninstall",
        "--purge-data",
        "--yes",
    ]);
    assert_eq!(code, Some(0), "{out}{err}");
    assert!(!project_db.exists(), "the named database is gone:\n{out}");
    assert!(user_db.exists(), "the user's own database stays:\n{out}");
}

/// The hook runs inside an agent's tool call. `attempt hook` with an argument
/// clap rejects used to exit 2, which blocks (for example) a Claude Code Stop
/// hook. Management subcommands and everything else keep clap's behaviour.
#[test]
fn attempt_hook_never_exits_2_because_of_its_arguments() {
    let m = machine();
    for args in [
        vec!["hook"],
        vec!["hook", "claude-code", "--bogus"],
        vec!["hook", "claude-code", "--event"],
        vec!["hook", "--bogus"],
    ] {
        let (code, out, err) = m.attempt(&args);
        assert_eq!(code, Some(0), "{args:?}: {out}{err}");
        assert!(out.is_empty(), "{args:?} printed {out:?}");
    }
    for args in [
        vec!["status", "--bogus"],
        vec!["hook", "install", "--bogus"],
        vec!["definitely-not-a-command"],
    ] {
        let (code, _, _) = m.attempt(&args);
        assert_eq!(code, Some(2), "{args:?} keeps clap's exit status");
    }
}
