//! Detection of installed coding agents.
//!
//! Detection is strictly read-only: it never creates an agent's directory or
//! config file. An agent counts as "detected" when its home directory exists
//! or its launcher binary is on `PATH`.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::time::{Duration, Instant};

use crate::platform::home_dir;

/// A supported coding agent.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum AgentKind {
    ClaudeCode,
    Codex,
    Cursor,
    GeminiCli,
}

impl AgentKind {
    /// Every supported agent, in display order.
    pub const ALL: [AgentKind; 4] = [
        AgentKind::ClaudeCode,
        AgentKind::Codex,
        AgentKind::Cursor,
        AgentKind::GeminiCli,
    ];

    /// Stable provider id used by `attempt hook <provider-id>`.
    pub fn provider_id(self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "claude-code",
            AgentKind::Codex => "codex",
            AgentKind::Cursor => "cursor",
            AgentKind::GeminiCli => "gemini-cli",
        }
    }

    /// Parse a provider id. A few common aliases are accepted as well.
    pub fn from_provider_id(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude-code" | "claude_code" | "claude" | "claudecode" => Some(AgentKind::ClaudeCode),
            "codex" | "codex-cli" | "codex_cli" => Some(AgentKind::Codex),
            "cursor" => Some(AgentKind::Cursor),
            "gemini-cli" | "gemini_cli" | "gemini" => Some(AgentKind::GeminiCli),
            _ => None,
        }
    }

    /// Human-readable name.
    pub fn display_name(self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "Claude Code",
            AgentKind::Codex => "Codex CLI",
            AgentKind::Cursor => "Cursor",
            AgentKind::GeminiCli => "Gemini CLI",
        }
    }

    /// Launcher binary name looked up on `PATH`.
    pub fn binary_name(self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Cursor => "cursor",
            AgentKind::GeminiCli => "gemini",
        }
    }

    /// Name of the agent's dot-directory under `$HOME` (also used for the
    /// project-scoped directory under a repository root).
    pub fn dir_name(self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => ".claude",
            AgentKind::Codex => ".codex",
            AgentKind::Cursor => ".cursor",
            AgentKind::GeminiCli => ".gemini",
        }
    }

    /// File inside the agent directory that holds hook configuration.
    ///
    /// Codex reads hooks from `hooks.json`, not `settings.json`.
    pub fn config_file_name(self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "settings.json",
            AgentKind::Codex => "hooks.json",
            AgentKind::Cursor => "hooks.json",
            AgentKind::GeminiCli => "settings.json",
        }
    }

    /// Environment variable the agent itself honours to relocate its home
    /// directory. We honour the same variable so we write where the agent
    /// reads.
    pub fn home_env_var(self) -> Option<&'static str> {
        match self {
            AgentKind::ClaudeCode => Some("CLAUDE_CONFIG_DIR"),
            AgentKind::Codex => Some("CODEX_HOME"),
            AgentKind::Cursor | AgentKind::GeminiCli => None,
        }
    }

    /// The agent's user-level directory (`~/.claude`, `$CODEX_HOME`, ...).
    /// Returns `None` only when no home directory can be determined.
    pub fn agent_dir(self) -> Option<PathBuf> {
        if let Some(var) = self.home_env_var()
            && let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty())
        {
            return Some(PathBuf::from(v));
        }
        home_dir().map(|h| h.join(self.dir_name()))
    }

    /// User-scope hook config path.
    pub fn user_config_path(self) -> Option<PathBuf> {
        self.agent_dir().map(|d| d.join(self.config_file_name()))
    }

    /// Project-scope hook config path (`<project>/.claude/settings.json`, ...).
    pub fn project_config_path(self, project: &Path) -> PathBuf {
        project.join(self.dir_name()).join(self.config_file_name())
    }

    /// Project-local (git-ignored) hook config path. Only Claude Code has a
    /// dedicated local file (`.claude/settings.local.json`).
    pub fn local_config_path(self, project: &Path) -> Option<PathBuf> {
        match self {
            AgentKind::ClaudeCode => Some(project.join(".claude").join("settings.local.json")),
            _ => None,
        }
    }
}

impl fmt::Display for AgentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.provider_id())
    }
}

impl FromStr for AgentKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        AgentKind::from_provider_id(s).ok_or_else(|| {
            format!(
                "unknown provider {s:?} (expected one of: {})",
                AgentKind::ALL
                    .iter()
                    .map(|k| k.provider_id())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
    }
}

/// An agent found on this machine.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct DetectedAgent {
    pub kind: AgentKind,
    /// User-scope hook config path for this agent.
    pub config_path: PathBuf,
    /// Whether `config_path` exists as a file.
    pub config_exists: bool,
    /// Evidence, e.g. `"~/.claude exists"`, `"`codex` on PATH (/usr/local/bin/codex)"`.
    pub detected_by: Vec<String>,
    /// Launcher binary on `PATH`, when found.
    pub binary_path: Option<PathBuf>,
    /// Output of `<bin> --version` (first line), best-effort.
    pub version: Option<String>,
}

/// Knobs for [`detect_agents_with`].
#[derive(Clone, Debug)]
pub struct DetectOptions {
    /// Run `<bin> --version` for every detected launcher (in parallel).
    pub probe_versions: bool,
    /// Per-binary timeout for the version probe.
    pub version_timeout: Duration,
}

impl Default for DetectOptions {
    fn default() -> Self {
        Self {
            probe_versions: true,
            version_timeout: Duration::from_secs(2),
        }
    }
}

/// Detect every supported agent, probing versions with a 2 s timeout each
/// (probes run concurrently, so the wall-clock cost is at most ~2 s).
pub fn detect_agents() -> Vec<DetectedAgent> {
    detect_agents_with(&DetectOptions::default())
}

/// Detect every supported agent with explicit options.
pub fn detect_agents_with(opts: &DetectOptions) -> Vec<DetectedAgent> {
    let mut found: Vec<DetectedAgent> = AgentKind::ALL
        .iter()
        .filter_map(|&k| detect_agent(k))
        .collect();
    if opts.probe_versions {
        let timeout = opts.version_timeout;
        std::thread::scope(|scope| {
            let handles: Vec<_> = found
                .iter()
                .map(|agent| {
                    let bin = agent.binary_path.clone();
                    scope.spawn(move || bin.and_then(|b| probe_version(&b, timeout)))
                })
                .collect();
            for (agent, handle) in found.iter_mut().zip(handles) {
                agent.version = handle.join().ok().flatten();
            }
        });
    }
    found
}

/// Detect a single agent without probing its version.
pub fn detect_agent(kind: AgentKind) -> Option<DetectedAgent> {
    let dir = kind.agent_dir()?;
    let mut detected_by = Vec::new();

    if let Some(var) = kind.home_env_var()
        && std::env::var_os(var).is_some_and(|v| !v.is_empty())
    {
        detected_by.push(format!("{var} is set ({})", display_path(&dir)));
    }
    if dir.is_dir() {
        detected_by.push(format!("{} exists", display_path(&dir)));
    }
    if kind == AgentKind::ClaudeCode
        && let Some(home) = home_dir()
        && home.join(".claude.json").is_file()
    {
        detected_by.push("~/.claude.json exists".to_string());
    }
    let binary_path = find_on_path(kind.binary_name());
    if let Some(bin) = &binary_path {
        detected_by.push(format!(
            "`{}` on PATH ({})",
            kind.binary_name(),
            bin.display()
        ));
    }

    // An env override alone (pointing at a directory that does not exist) is
    // not evidence that the agent is installed.
    let real_evidence = dir.is_dir()
        || binary_path.is_some()
        || detected_by
            .iter()
            .any(|d| d.ends_with(".claude.json exists"));
    if !real_evidence {
        return None;
    }

    let config_path = dir.join(kind.config_file_name());
    Some(DetectedAgent {
        kind,
        config_exists: config_path.is_file(),
        config_path,
        detected_by,
        binary_path,
        version: None,
    })
}

/// Render a path with the home directory abbreviated to `~`.
pub fn display_path(path: &Path) -> String {
    if let Some(home) = home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        if rest.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display());
    }
    path.display().to_string()
}

/// Locate an executable on `PATH` (honouring `PATHEXT` on Windows).
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for candidate in candidates_in(&dir, name) {
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn candidates_in(dir: &Path, name: &str) -> Vec<PathBuf> {
    if cfg!(windows) {
        let exts: Vec<String> = std::env::var("PATHEXT")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|v| {
                v.split(';')
                    .map(|e| e.trim().to_string())
                    .filter(|e| !e.is_empty())
                    .collect()
            })
            .unwrap_or_else(|| vec![".COM".into(), ".EXE".into(), ".BAT".into(), ".CMD".into()]);
        let mut out = vec![dir.join(name)];
        out.extend(exts.iter().map(|e| dir.join(format!("{name}{e}"))));
        out
    } else {
        vec![dir.join(name)]
    }
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Run `<bin> --version` and return the first non-empty line of stdout.
/// Returns `None` on spawn failure, non-UTF-8 output, or timeout (the child
/// is killed).
pub fn probe_version(bin: &Path, timeout: Duration) -> Option<String> {
    use std::io::Read;
    let mut child = Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    // Drain stdout on a helper thread so a chatty child cannot dead-lock on a
    // full pipe while we poll for exit.
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.take(64 * 1024).read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
    let out = reader.join().ok()?;
    let line = out.lines().map(str::trim).find(|l| !l.is_empty())?;
    let mut line = line.to_string();
    if line.len() > 120 {
        line.truncate(
            line.char_indices()
                .nth(120)
                .map(|(i, _)| i)
                .unwrap_or(line.len()),
        );
    }
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_ids_round_trip() {
        for k in AgentKind::ALL {
            assert_eq!(AgentKind::from_provider_id(k.provider_id()), Some(k));
            assert_eq!(k.to_string().parse::<AgentKind>().unwrap(), k);
            assert_eq!(
                serde_json::to_value(k).unwrap(),
                serde_json::json!(k.provider_id())
            );
        }
        assert_eq!(
            AgentKind::from_provider_id("Claude"),
            Some(AgentKind::ClaudeCode)
        );
        assert!(AgentKind::from_provider_id("copilot").is_none());
    }

    #[test]
    fn config_paths_follow_agent_layout() {
        let p = Path::new("/proj");
        assert_eq!(
            AgentKind::Codex.project_config_path(p),
            PathBuf::from("/proj/.codex/hooks.json")
        );
        assert_eq!(
            AgentKind::ClaudeCode.local_config_path(p),
            Some(PathBuf::from("/proj/.claude/settings.local.json"))
        );
        assert_eq!(AgentKind::Cursor.local_config_path(p), None);
        assert!(
            AgentKind::Codex
                .user_config_path()
                .is_none_or(|p| p.ends_with("hooks.json"))
        );
    }

    #[test]
    fn detection_does_not_panic_and_never_creates_dirs() {
        let before: Vec<bool> = AgentKind::ALL
            .iter()
            .map(|k| k.agent_dir().is_some_and(|d| d.exists()))
            .collect();
        let _ = detect_agents_with(&DetectOptions {
            probe_versions: false,
            ..Default::default()
        });
        let after: Vec<bool> = AgentKind::ALL
            .iter()
            .map(|k| k.agent_dir().is_some_and(|d| d.exists()))
            .collect();
        assert_eq!(before, after);
    }

    #[test]
    fn find_on_path_finds_common_tools() {
        if cfg!(windows) {
            return;
        }
        // `sh` exists on every Unix; an unlikely name does not.
        assert!(find_on_path("sh").is_some());
        assert!(find_on_path("attemptdb-definitely-not-a-binary").is_none());
    }
}
