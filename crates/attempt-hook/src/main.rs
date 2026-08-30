//! `attempt-hook <provider> [--event <name>] [--data-dir <dir>] [--db <dir>]`
//!
//! The same hot path as `attempt hook <provider>` — normalise the payload,
//! hand it to the daemon or append it to the spool, print the provider's
//! acknowledgement, exit 0 — in a binary that links only the capture crate.
//! `attempt` carries DataFusion, the UI and the MCP server too, and paging
//! that executable in was about 85 % of a hook's wall time; this one is a
//! fraction of the size. `attempt hook install` references it whenever it
//! sits next to `attempt`, and `attempt update` keeps the pair in step.
//!
//! Never prints anything but the acknowledgement, never exits non-zero on
//! the hook path: an agent must not be able to notice a capture problem.

use attemptdb_capture::hook::{HookInput, read_stdin, run_hook};
use std::io::Write;
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut provider: Option<String> = None;
    let mut event: Option<String> = None;
    let mut data_dir: Option<PathBuf> = None;
    let mut db: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--event" => {
                event = args.get(i + 1).cloned();
                i += 2;
            }
            "--data-dir" => {
                data_dir = args.get(i + 1).map(PathBuf::from);
                i += 2;
            }
            "--db" => {
                db = args.get(i + 1).map(PathBuf::from);
                i += 2;
            }
            "--version" | "-V" => {
                println!("attempt-hook {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--help" | "-h" => {
                eprintln!(
                    "usage: attempt-hook <provider-id> [--event <name>] [--data-dir <dir>] [--db <dir>]\n\
                     the hook entrypoint `attempt hook install` wires into your agents; reads one event on stdin"
                );
                return;
            }
            a if provider.is_none() && !a.starts_with('-') => {
                provider = Some(a.to_string());
                i += 1;
            }
            _ => i += 1,
        }
    }
    let Some(provider) = provider else {
        // Not the hook path (no agent runs us without a provider): a human
        // typed this, so say what is missing and fail like a CLI would.
        eprintln!("attempt-hook: missing <provider-id> (claude-code, codex, cursor, gemini-cli)");
        std::process::exit(2);
    };
    let payload = read_stdin();
    let outcome = run_hook(HookInput {
        provider_id: &provider,
        event_hint: event.as_deref(),
        payload_bytes: payload,
        cwd_hint: std::env::var_os("CLAUDE_PROJECT_DIR").map(Into::into),
        data_dir_override: data_dir,
        db_override: db,
    });
    if let Some(s) = &outcome.stdout {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(s.as_bytes());
        let _ = out.flush();
    }
    // Errors went to hook.log; the agent never sees them.
}
