//! Transcript importers: reconstruct canonical events from the conversation
//! logs coding agents keep on disk, for sessions that ran before hooks were
//! installed (or to enrich hook-captured sessions).
//!
//! Everything produced here is *reconstructed*, never a captured fact. Every
//! event carries `attrs.reconstructed = true` and `attrs.reconstructed_from`,
//! has no `hook_version`, keeps no `raw` payload, and derives its id
//! deterministically from the transcript entry it came from, so importing a
//! transcript that has grown since the last import only adds what is new
//! (the storage layer deduplicates by event id).

pub mod claude_code;

pub use claude_code::{
    RECONSTRUCTED_FROM, TranscriptImport, TranscriptOptions, TranscriptStats,
    parse_claude_transcript,
};

#[cfg(test)]
mod tests;
