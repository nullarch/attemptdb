//! Provider adapters that normalise coding-agent hook payloads into canonical
//! AttemptDB events.
//!
//! Every supported provider (Claude Code, Codex CLI, Cursor, Gemini CLI) pipes
//! a JSON payload to its hook command on stdin. An [`Adapter`] turns exactly
//! one such payload into one [`Event`] of the canonical model defined in
//! `attemptdb-core`, applying the same rules everywhere:
//!
//! - provider event names are mapped onto [`EventKind`]; names without a
//!   canonical mapping become [`EventKind::Unknown`] and are never dropped,
//! - `attrs` only ever receive allowlisted, content-free metadata
//!   ([`common::ALLOWED_ATTR_KEYS`]),
//! - content-bearing fields (prompts, commands, tool output, file contents)
//!   live in `content` and are stripped in `metadata_only` capture mode,
//! - the original payload is retained in `raw` (minus transcript paths) when
//!   the capture mode permits it.

pub mod claude_code;
pub mod codex;
pub mod common;
pub mod cursor;
pub mod gemini_cli;
pub mod privacy;
pub mod transcript;
pub mod vibemon;
pub mod vibemon_export;

pub use claude_code::ClaudeCodeAdapter;
pub use codex::CodexAdapter;
pub use cursor::CursorAdapter;
pub use gemini_cli::GeminiCliAdapter;

use attemptdb_core::event::Provider;
use attemptdb_core::{CaptureMode, DeviceId, Event, ProjectRef, Timestamp};

/// Version recorded on every event as [`Event::adapter_version`].
pub const ADAPTER_VERSION: &str = "0.1.0";

/// Everything the hook process knows that is not part of the payload.
#[derive(Clone, Debug)]
pub struct CaptureContext {
    pub device_id: DeviceId,
    pub capture_mode: CaptureMode,
    /// Already derived by the caller (root, remote, branch, head).
    pub project: ProjectRef,
    pub captured_at: Timestamp,
    pub provider_version: Option<String>,
    pub hook_version: Option<String>,
}

/// Errors an adapter can report. Malformed input never panics; it either
/// yields one of these or an [`EventKind::Unknown`] event.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("payload carries no event name (no `hook_event_name` and no hint)")]
    MissingEventName,
    #[error("payload is not a JSON object")]
    PayloadNotObject,
    #[error("unsupported event `{0}`")]
    UnsupportedEvent(String),
    #[error("invalid payload: {0}")]
    Invalid(String),
}

/// One provider's normalisation logic.
pub trait Adapter: Send + Sync {
    fn provider(&self) -> Provider;

    /// Hook event names this adapter installs/understands (provider spelling).
    fn supported_events(&self) -> &'static [&'static str];

    /// Normalise one raw hook payload (stdin JSON). `event_name_hint` is an
    /// explicit event name passed on the command line for providers whose
    /// payload lacks one.
    fn normalise(
        &self,
        ctx: &CaptureContext,
        event_name_hint: Option<&str>,
        payload: &serde_json::Value,
    ) -> Result<Event, AdapterError>;
}

/// The adapter for a provider, if one is built in.
pub fn adapter_for(provider: &Provider) -> Option<Box<dyn Adapter>> {
    match provider {
        Provider::ClaudeCode => Some(Box::new(ClaudeCodeAdapter)),
        Provider::Codex => Some(Box::new(CodexAdapter)),
        Provider::Cursor => Some(Box::new(CursorAdapter)),
        Provider::GeminiCli => Some(Box::new(GeminiCliAdapter)),
        Provider::Other(_) => None,
    }
}

/// Every built-in adapter, in a stable order.
pub fn all_adapters() -> Vec<Box<dyn Adapter>> {
    vec![
        Box::new(ClaudeCodeAdapter),
        Box::new(CodexAdapter),
        Box::new(CursorAdapter),
        Box::new(GeminiCliAdapter),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_lookup_covers_builtin_providers() {
        for provider in [
            Provider::ClaudeCode,
            Provider::Codex,
            Provider::Cursor,
            Provider::GeminiCli,
        ] {
            let adapter = adapter_for(&provider).expect("built-in adapter");
            assert_eq!(adapter.provider(), provider);
            assert!(!adapter.supported_events().is_empty());
        }
        assert!(adapter_for(&Provider::Other("copilot".into())).is_none());
        assert_eq!(all_adapters().len(), 4);
    }
}
