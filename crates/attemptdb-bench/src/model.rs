//! Workload model: the distributions the generator draws from.
//!
//! Every table marked *sampled* was taken on 2026-08-29 from the live
//! AttemptDB database of one developer (2,564 events at sampling time, 7
//! sessions, 1 project, macOS, Claude Code with heavy subagent use; 45% of
//! the events had been reconstructed from transcripts). Only aggregates were
//! read through `attempt query`: counts, key names, and length percentiles.
//! No prompt, command, path, or output text was copied; every string the
//! generator emits comes from the word lists in `text.rs`.
//!
//! The sample is small and skewed (one session holds 96% of the events), so
//! the per-session and per-turn shapes are coarse and the tables marked
//! *assumed* are not from the data at all. The provider mix is the target
//! mix requested for the public workload, not the sample's (which was 99.9%
//! Claude Code).

use attemptdb_core::ToolCategory;
use attemptdb_core::event::Provider;

/// Date the live database was sampled.
pub const SAMPLE_DATE: &str = "2026-08-29";
/// Events in the live database when the aggregates were taken.
pub const SAMPLE_EVENTS: u64 = 2_564;

/// An empirical quantile table: `(probability, value)` pairs with
/// increasing probability, starting at `0.0` (minimum) and ending at `1.0`
/// (maximum). Sampling interpolates log-linearly between points (linearly
/// when a point is zero), which reproduces the heavy right tails of the
/// sampled length distributions without inventing a parametric shape.
#[derive(Clone, Copy, Debug)]
pub struct Quantiles(pub &'static [(f64, f64)]);

impl Quantiles {
    pub fn sample(&self, u: f64) -> f64 {
        let pts = self.0;
        if pts.is_empty() {
            return 0.0;
        }
        if u <= pts[0].0 {
            return pts[0].1;
        }
        for w in pts.windows(2) {
            let (p0, v0) = w[0];
            let (p1, v1) = w[1];
            if u <= p1 {
                let f = if p1 > p0 { (u - p0) / (p1 - p0) } else { 1.0 };
                return if v0 > 0.0 && v1 > 0.0 {
                    (v0.ln() + f * (v1.ln() - v0.ln())).exp()
                } else {
                    v0 + f * (v1 - v0)
                };
            }
        }
        pts[pts.len() - 1].1
    }
}

// ---------------------------------------------------------------------------
// Provider and session shape
// ---------------------------------------------------------------------------

/// Target provider mix for the public workload (requested, not sampled).
pub const PROVIDER_MIX: &[(Provider, f64)] = &[
    (Provider::ClaudeCode, 0.70),
    (Provider::Codex, 0.20),
    (Provider::Cursor, 0.07),
    (Provider::GeminiCli, 0.03),
];

/// Share of single-event noise sessions (`capture_test` / `unknown`).
/// Sampled: 4 of 2,564 events were capture tests; the workload uses 1% as
/// requested.
pub const NOISE_SESSION_SHARE: f64 = 0.01;

/// Share of Claude Code sessions that are transcript reconstructions
/// (`attrs.reconstructed = true`, `transcript:*` event names, no raw
/// payload). Sampled: 1,152 of 2,564 events (45%).
pub const RECONSTRUCTED_SESSION_SHARE: f64 = 0.45;

/// Turns per session. *Assumed* (the sample had one 7-turn session, one
/// 1-turn session, and five single-event sessions).
pub const TURNS_PER_SESSION: Quantiles =
    Quantiles(&[(0.0, 1.0), (0.5, 2.0), (0.8, 4.0), (0.95, 8.0), (1.0, 12.0)]);

/// Tool calls per turn. Sampled from five turns: p25 103, p50 176, p75 329,
/// max 857 — all from one autonomous multi-agent session. The generator uses
/// a wider table with a lower body because five turns cannot represent
/// interactive use; the upper half follows the sample.
pub const TOOL_CALLS_PER_TURN: Quantiles = Quantiles(&[
    (0.0, 1.0),
    (0.25, 12.0),
    (0.5, 60.0),
    (0.75, 200.0),
    (0.9, 450.0),
    (1.0, 900.0),
]);

/// Sessions whose first activity precedes any prompt (implicit turn 0).
/// Sampled: 1 of 2 active sessions.
pub const IMPLICIT_FIRST_TURN_RATE: f64 = 0.15;
/// Sessions with an observed `session_started`. Sampled: 1 of 7 (and that
/// one reconstructed); hook `SessionStart` was rarely wired.
pub const SESSION_STARTED_RATE: f64 = 0.30;
/// Sessions with an observed `session_ended`. Sampled: 0 of 7.
pub const SESSION_ENDED_RATE: f64 = 0.10;
/// Stop events per prompt. Sampled: 13 turn stops for 9 prompts.
pub const EXTRA_STOP_RATE: f64 = 0.4;
/// Turn stops carrying the final assistant message. Sampled: 8 of 13.
pub const TURN_STOP_MESSAGE_RATE: f64 = 0.6;
/// `idle_prompt` notification after a turn stop. Sampled: 7 idle prompts
/// for 13 stops; the other notification types were `injected_prompt` (2)
/// and `auth_success` (1).
pub const IDLE_NOTIFICATION_RATE: f64 = 0.55;
/// Notification type mix. Sampled.
pub const NOTIFICATION_TYPES: &[(&str, f64)] = &[
    ("idle_prompt", 0.70),
    ("injected_prompt", 0.20),
    ("auth_success", 0.10),
];
/// `agent_message` events per tool call in reconstructed sessions.
/// Sampled: 17 messages per 1,152 reconstructed events.
pub const AGENT_MESSAGE_PER_CALL: f64 = 0.03;
/// Events with `agent.model` populated. Sampled: 22%.
pub const MODEL_PRESENT_RATE: f64 = 0.22;

/// Share of tool calls executed by subagents. Sampled: 77% of events carried
/// `parent_agent_id`.
pub const SUBAGENT_CALL_SHARE: f64 = 0.77;
/// Subagent type mix. Sampled (names generalised).
pub const SUBAGENT_TYPES: &[(&str, f64)] = &[
    ("general-purpose", 0.87),
    ("explore", 0.08),
    ("fork", 0.03),
    ("guide", 0.02),
];
/// `subagent_started` events per subagent. Sampled: 19 starts for 25
/// subagent tool calls.
pub const SUBAGENT_STARTED_RATE: f64 = 0.75;
/// `subagent_stopped` events per subagent. Sampled: 163 stops for 25
/// subagent tool calls (the stop hook fires per subagent turn).
pub const SUBAGENT_STOPS_PER_SUBAGENT: Quantiles =
    Quantiles(&[(0.0, 1.0), (0.5, 5.0), (0.9, 12.0), (1.0, 20.0)]);

// ---------------------------------------------------------------------------
// Tool mix and outcomes
// ---------------------------------------------------------------------------

/// Tool category mix over `tool_call_started`. Sampled (1,112 starts):
/// shell 70.5%, edit 10.1%, read 8.9%, write 6.1%, subagent 2.2%, web 0.45%,
/// other 0.27%, mcp 0.18%; no search tool appeared.
pub const TOOL_MIX: &[(ToolCategory, f64)] = &[
    (ToolCategory::Shell, 0.705),
    (ToolCategory::FileEdit, 0.101),
    (ToolCategory::FileRead, 0.089),
    (ToolCategory::FileWrite, 0.061),
    (ToolCategory::Subagent, 0.022),
    (ToolCategory::Web, 0.0045),
    (ToolCategory::Other, 0.0027),
    (ToolCategory::Mcp, 0.0018),
    (ToolCategory::Search, 0.0018),
];

/// Failure rate per category. Sampled: 18 shell failures in 898 shell
/// ends (2.0%), 1 string mismatch among 112 edits; nothing else failed.
pub fn failure_rate(cat: ToolCategory) -> f64 {
    match cat {
        ToolCategory::Shell => 0.020,
        ToolCategory::FileEdit => 0.009,
        _ => 0.002,
    }
}

/// Failure classes per category. Sampled: file_not_found 12, nonzero_exit 5,
/// string_mismatch 1.
pub fn failure_classes(cat: ToolCategory) -> &'static [(&'static str, f64)] {
    match cat {
        ToolCategory::Shell => &[("file_not_found", 0.7), ("nonzero_exit", 0.3)],
        ToolCategory::FileEdit => &[("string_mismatch", 1.0)],
        _ => &[("file_not_found", 1.0)],
    }
}

/// `permission_denied` per tool start. Sampled: 1 in 1,112.
pub const PERMISSION_DENIED_RATE: f64 = 0.001;

/// Tool duration in milliseconds by category (`duration_ms` on the end
/// event). Sampled for shell (n=398), read (73), edit (43), write (23), mcp
/// (2); web is assumed.
pub fn duration_ms(cat: ToolCategory) -> Quantiles {
    match cat {
        ToolCategory::Shell => Quantiles(&[
            (0.0, 3.0),
            (0.5, 40.0),
            (0.9, 6_077.0),
            (0.95, 10_723.0),
            (0.99, 61_964.0),
            (1.0, 290_000.0),
        ]),
        ToolCategory::FileRead => Quantiles(&[(0.0, 0.0), (0.5, 1.0), (0.9, 315.0), (1.0, 412.0)]),
        ToolCategory::FileEdit => Quantiles(&[(0.0, 2.0), (0.5, 3.0), (0.9, 4.0), (1.0, 6.0)]),
        ToolCategory::FileWrite => Quantiles(&[(0.0, 1.0), (0.5, 2.0), (0.9, 5.0), (1.0, 5.0)]),
        ToolCategory::Mcp => Quantiles(&[(0.0, 399.0), (0.5, 521.0), (1.0, 644.0)]),
        ToolCategory::Web => Quantiles(&[
            (0.0, 500.0),
            (0.5, 2_000.0),
            (0.9, 8_000.0),
            (1.0, 15_000.0),
        ]),
        _ => Quantiles(&[(0.0, 1.0), (1.0, 7.0)]),
    }
}

/// Gap in seconds between a tool call's end and the next event of the same
/// agent ("think time"). Sampled over 1,405 hook-captured consecutive
/// events: p10 12 ms, p25 41 ms, p50 0.7 s, p75 3.3 s, p90 9.7 s, p95 19.5
/// s, p99 80 s; the maximum (17 h) is capped at 10 minutes.
pub const THINK_GAP_SECS: Quantiles = Quantiles(&[
    (0.0, 0.0),
    (0.1, 0.012),
    (0.25, 0.041),
    (0.5, 0.703),
    (0.75, 3.31),
    (0.9, 9.696),
    (0.95, 19.507),
    (0.99, 80.464),
    (1.0, 600.0),
]);

/// Gap in seconds between consecutive session starts. *Assumed*.
pub const SESSION_START_GAP_SECS: Quantiles =
    Quantiles(&[(0.0, 20.0), (0.5, 600.0), (0.9, 5_400.0), (1.0, 28_800.0)]);

/// Sessions that may be active at the same time (parallel agents).
/// *Assumed* from the sample's overlapping sessions.
pub const CONCURRENT_SESSIONS: usize = 3;

/// Hook in-process overhead in microseconds (`attrs.hook_us`). Sampled
/// (n=1,412): p50 231, p90 356, p95 409, p99 575, max 2,433.
pub const HOOK_US: Quantiles = Quantiles(&[
    (0.0, 92.0),
    (0.5, 231.0),
    (0.9, 356.0),
    (0.95, 409.0),
    (0.99, 575.0),
    (1.0, 2_433.0),
]);

// ---------------------------------------------------------------------------
// Paths and projects
// ---------------------------------------------------------------------------

/// File extension mix on file tool events. Sampled (n=560).
pub const FILE_EXT_MIX: &[(&str, f64)] = &[
    ("rs", 0.82),
    ("txt", 0.085),
    ("jsonl", 0.035),
    ("py", 0.025),
    ("md", 0.02),
    ("sh", 0.005),
    ("json", 0.005),
    ("toml", 0.005),
];

/// Depth (path components) of repository-relative paths. Sampled: 4
/// components 91%, 5 components 7%, shallower 2%.
pub const PATH_DEPTH: &[(usize, f64)] = &[(1, 0.01), (2, 0.005), (3, 0.01), (4, 0.91), (5, 0.07)];

/// Distinct paths per project pool. Sampled: 97 distinct paths in 2,564
/// events; the pool is larger so long workloads keep some churn.
pub const PATHS_PER_PROJECT: usize = 160;

/// Projects in the workload with Zipf-like weights. *Assumed* (the sample
/// had one project).
pub const PROJECT_COUNT: usize = 12;

// ---------------------------------------------------------------------------
// Content sizes (bytes of the JSON-encoded field). All sampled.
// ---------------------------------------------------------------------------

/// `content.command` on shell calls (n=880).
pub const SHELL_COMMAND: Quantiles = Quantiles(&[
    (0.0, 8.0),
    (0.5, 234.0),
    (0.9, 2_879.0),
    (0.95, 6_025.0),
    (0.99, 29_795.0),
    (1.0, 69_469.0),
]);
/// `content.tool_output` on finished shell calls (n=880; the provider
/// truncates at ~30 KB).
pub const SHELL_OUTPUT: Quantiles = Quantiles(&[
    (0.0, 2.0),
    (0.5, 1_169.0),
    (0.9, 13_606.0),
    (0.95, 24_943.0),
    (0.99, 31_322.0),
    (1.0, 32_161.0),
]);
/// Probability that a shell result is a bare string rather than an object
/// with stdout/stderr fields (sampled: 379 of 880).
pub const SHELL_OUTPUT_STRING_RATE: f64 = 0.43;

/// `content.tool_input` on edit calls (n=112).
pub const EDIT_INPUT: Quantiles = Quantiles(&[
    (0.0, 240.0),
    (0.5, 585.0),
    (0.9, 1_777.0),
    (0.95, 2_565.0),
    (0.99, 3_196.0),
    (1.0, 3_562.0),
]);
/// Edit results that echo the original file as an object (sampled: 43 of
/// 112); the rest are a ~180-byte confirmation string.
pub const EDIT_OUTPUT_OBJECT_RATE: f64 = 0.38;
pub const EDIT_OUTPUT_OBJECT: Quantiles = Quantiles(&[
    (0.0, 1_000.0),
    (0.5, 9_000.0),
    (0.9, 38_755.0),
    (0.95, 49_174.0),
    (1.0, 52_087.0),
]);

/// `content.tool_input` on read calls (n=99) was 57–179 bytes: the path
/// plus optional `limit` / `offset`, which the generator builds structurally.
/// `content.tool_output` on read calls (n=99; the provider truncates at
/// 64 KB).
pub const READ_OUTPUT: Quantiles = Quantiles(&[
    (0.0, 1_480.0),
    (0.5, 11_825.0),
    (0.9, 47_325.0),
    (0.95, 50_881.0),
    (0.99, 65_534.0),
    (1.0, 65_534.0),
]);
/// Read results shaped as `{file: {...}, type}` (sampled: 73 of 99).
pub const READ_OUTPUT_OBJECT_RATE: f64 = 0.74;

/// `content.tool_input` on write calls (n=69): the whole file content.
pub const WRITE_INPUT: Quantiles = Quantiles(&[
    (0.0, 952.0),
    (0.5, 11_015.0),
    (0.9, 43_640.0),
    (0.95, 46_948.0),
    (0.99, 57_307.0),
    (1.0, 57_307.0),
]);
/// Write results that echo content and a patch (sampled: 23 of 69).
pub const WRITE_OUTPUT_OBJECT_RATE: f64 = 0.33;
pub const WRITE_OUTPUT_OBJECT: Quantiles = Quantiles(&[
    (0.0, 1_000.0),
    (0.5, 12_000.0),
    (0.9, 30_108.0),
    (0.95, 42_274.0),
    (1.0, 48_609.0),
]);

/// Subagent dispatch prompt (n=25).
pub const SUBAGENT_INPUT: Quantiles = Quantiles(&[
    (0.0, 1_776.0),
    (0.5, 7_849.0),
    (0.9, 13_300.0),
    (1.0, 14_374.0),
]);
/// Subagent result (n=25).
pub const SUBAGENT_OUTPUT: Quantiles = Quantiles(&[
    (0.0, 39.0),
    (0.5, 1_065.0),
    (0.9, 8_094.0),
    (0.95, 8_648.0),
    (1.0, 9_732.0),
]);

/// Web fetch input (n=5) and output (n=5).
pub const WEB_INPUT: Quantiles = Quantiles(&[(0.0, 126.0), (0.5, 171.0), (1.0, 192.0)]);
pub const WEB_OUTPUT: Quantiles =
    Quantiles(&[(0.0, 215.0), (0.5, 2_053.0), (0.9, 6_190.0), (1.0, 6_190.0)]);

/// Small tools (search, MCP, other): output (n=4). Their inputs were 13–113
/// bytes and are built structurally.
pub const SMALL_OUTPUT: Quantiles =
    Quantiles(&[(0.0, 80.0), (0.5, 222.0), (0.9, 6_487.0), (1.0, 7_253.0)]);

/// Human prompt (n=9): bimodal, mostly short replies with occasional pasted
/// specifications.
pub const PROMPT: Quantiles = Quantiles(&[
    (0.0, 9.0),
    (0.5, 32.0),
    (0.8, 300.0),
    (0.9, 5_000.0),
    (0.99, 11_102.0),
    (1.0, 11_225.0),
]);
/// Assistant text message in reconstructed sessions (n=17).
pub const AGENT_MESSAGE: Quantiles = Quantiles(&[
    (0.0, 90.0),
    (0.5, 5_463.0),
    (0.9, 10_900.0),
    (0.95, 15_547.0),
    (1.0, 16_070.0),
]);
/// Subagent stop message (n=150).
pub const SUBAGENT_STOP_MESSAGE: Quantiles = Quantiles(&[
    (0.0, 21.0),
    (0.5, 37.0),
    (0.9, 46.0),
    (0.95, 144.0),
    (0.99, 7_751.0),
    (1.0, 9_963.0),
]);
/// Final message on a turn stop (n=8).
pub const TURN_STOP_MESSAGE: Quantiles =
    Quantiles(&[(0.0, 70.0), (0.5, 1_747.0), (0.9, 2_669.0), (1.0, 2_719.0)]);
/// Error text on a failed call (n=18).
pub const FAILURE_ERROR: Quantiles = Quantiles(&[
    (0.0, 1_230.0),
    (0.5, 4_436.0),
    (0.9, 10_040.0),
    (1.0, 10_040.0),
]);

/// Sampled kind mix (share of events), used to check the generator's
/// output rather than to drive it: the generator produces kinds
/// structurally from sessions, turns, and tool calls.
pub const SAMPLED_KIND_MIX: &[(&str, f64)] = &[
    ("tool_call_finished", 0.466),
    ("tool_call_started", 0.434),
    ("subagent_stopped", 0.064),
    ("subagent_started", 0.007),
    ("tool_call_failed", 0.007),
    ("agent_message", 0.007),
    ("turn_stopped", 0.005),
    ("notification", 0.004),
    ("prompt_submitted", 0.0035),
    ("unknown", 0.0016),
    ("session_started", 0.0004),
    ("permission_denied", 0.0004),
];

/// Sampled sizes of the live database, for calibration of the synthetic
/// content: 2,625 events occupied 4.94 MB of segments; their JSON fields
/// summed to 17.4 MB of `content`, 8.7 MB of `raw`, 0.66 MB of `attrs`,
/// 0.13 MB of paths, i.e. roughly 10.8 KB of JSON per event compressed
/// about 5.7:1.
pub const SAMPLED_JSON_BYTES_PER_EVENT: f64 = 10_800.0;
pub const SAMPLED_SEGMENT_BYTES_PER_EVENT: f64 = 1_880.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_interpolate_monotonically() {
        let q = SHELL_OUTPUT;
        let mut last = 0.0;
        for i in 0..=100 {
            let v = q.sample(i as f64 / 100.0);
            assert!(v >= last, "{v} < {last}");
            last = v;
        }
        assert_eq!(q.sample(0.0), 2.0);
        assert_eq!(q.sample(1.0), 32_161.0);
        assert!((q.sample(0.5) - 1_169.0).abs() < 1e-6);
    }

    #[test]
    fn mixes_are_normalised_enough() {
        let total: f64 = TOOL_MIX.iter().map(|(_, w)| w).sum();
        assert!((total - 1.0).abs() < 0.02, "{total}");
        let total: f64 = PROVIDER_MIX.iter().map(|(_, w)| w).sum();
        assert!((total - 1.0).abs() < 1e-9);
    }
}
