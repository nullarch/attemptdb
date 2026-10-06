//! What `attempt doctor` and `attempt status` say about the capture path
//! itself: an unusable config (capture fails closed to metadata-only), a
//! project-local database that is not trusted, a key that is required but
//! missing. Fakes under a temporary HOME, no daemon, no OS key store.

use serde_json::Value;
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
    fn attempt_in(&self, cwd: &Path, args: &[&str]) -> (Option<i32>, String, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_attempt"))
            .arg("--data-dir")
            .arg(&self.data)
            .args(args)
            .current_dir(cwd)
            .env("PATH", bare_path())
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("CODEX_HOME", self.home.join(".codex"))
            .env("ATTEMPTDB_KEYRING", "off")
            .env("ATTEMPTDB_NO_DAEMON", "1")
            .env_remove("ATTEMPTDB_KEY_FILE")
            .env_remove("ATTEMPTDB_PASSPHRASE")
            .env_remove("ATTEMPTDB_DIR")
            .env_remove("CLAUDE_CONFIG_DIR")
            .output()
            .expect("run attempt");
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
