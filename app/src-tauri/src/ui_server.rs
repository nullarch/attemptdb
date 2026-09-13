//! The local Agent Timeline, served by `attempt ui` as a child of the app.
//!
//! The app does not render the timeline itself: `attempt ui` already does,
//! on an authenticated loopback port, and a second implementation would
//! drift from it. The app starts it, reads the one-time URL it prints, opens
//! that URL in a window of its own, and keeps the child for as long as it
//! runs. The URL's token becomes the session cookie the server expects, so
//! the app can also ask the same server what needs the user's attention.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// Must match `attemptdb_ui::COOKIE_NAME`.
const COOKIE_NAME: &str = "attemptdb_ui";

pub struct UiServer {
    child: Child,
    /// The URL to open: carries `?token=`, which the server trades for a cookie.
    pub url: String,
    base: String,
    token: String,
}

impl UiServer {
    pub fn start(bin: &Path) -> Result<Self, String> {
        let mut child = Command::new(bin)
            .args(["ui", "--no-open"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("starting attempt ui: {e}"))?;
        let stdout = child.stdout.take().ok_or("attempt ui has no stdout")?;
        let stderr = child.stderr.take().ok_or("attempt ui has no stderr")?;
        // The reader outlives the wait: it keeps draining so the child never
        // blocks on a full pipe, and lets the wait below give up on a clock.
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
        let (etx, erx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let mut text = String::new();
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                text.push_str(&line);
                text.push('\n');
            }
            let _ = etx.send(text);
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let url = loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(left) {
                Ok(line) => {
                    if let Some(u) = line.trim().strip_prefix("url") {
                        break u.trim().to_string();
                    }
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let err = erx.try_recv().unwrap_or_default();
                    return Err(if err.trim().is_empty() {
                        "attempt ui did not print its URL within 60 s".into()
                    } else {
                        format!("attempt ui did not start:\n{}", err.trim())
                    });
                }
            }
        };
        let (base, token) = split_token(&url).ok_or_else(|| format!("no token in {url}"))?;
        Ok(Self {
            child,
            url,
            base,
            token,
        })
    }

    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// `GET <base><path>` with the session cookie, parsed as JSON.
    pub fn api(&self, path: &str) -> Result<serde_json::Value, String> {
        let resp = ureq::get(&format!("{}{path}", self.base))
            .set("Cookie", &format!("{COOKIE_NAME}={}", self.token))
            .timeout(Duration::from_secs(10))
            .call()
            .map_err(|e| format!("{path}: {e}"))?;
        let text = resp.into_string().map_err(|e| format!("{path}: {e}"))?;
        serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))
    }
}

impl Drop for UiServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `http://127.0.0.1:1234/?token=abc` → (`http://127.0.0.1:1234`, `abc`).
fn split_token(url: &str) -> Option<(String, String)> {
    let (before, query) = url.split_once('?')?;
    let token = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("token="))?
        .to_string();
    let base = before.trim_end_matches('/').to_string();
    Some((base, token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_printed_url_splits_into_origin_and_token() {
        assert_eq!(
            split_token("http://127.0.0.1:4173/?token=abc123"),
            Some(("http://127.0.0.1:4173".into(), "abc123".into()))
        );
        assert_eq!(
            split_token("http://127.0.0.1:4173/?token=abc&demo=1"),
            Some(("http://127.0.0.1:4173".into(), "abc".into()))
        );
        assert_eq!(split_token("http://127.0.0.1:4173/"), None);
    }
}
