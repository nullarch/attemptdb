# Compatibility matrix

This document records, per provider and per platform, what AttemptDB has
actually verified. It is deliberately conservative: an entry is only
`verified` when this repository contains a real-shape fixture and a golden
test for it. Anything else is marked with the weaker level that applies.

Last updated: 2026-08-28. The adapter crate (`attemptdb-adapters`) is still a
stub, so no row is `verified` yet.

## Verification levels

| Level | Meaning | Counts toward Tier 1 adapter claim |
|---|---|---|
| `documented` | The event and its payload shape are described in the provider's official documentation; AttemptDB has not yet captured a real payload. | No |
| `observed` | The event name has been seen in a real configuration file or a real payload (for example a production installer or a user's config), but no fixture and golden test exist in this repository. | No |
| `verified` | A synthetic or scrubbed real-shape fixture under `fixtures/<provider>/` plus a golden normalised envelope and a passing test exist in this repository. | Yes |
| `unverified` | Not yet tested in any way; the mapping is an intention. | No |

Only `verified` counts. A provider is a Tier 1 adapter only when every event
AttemptDB claims to capture for it is `verified` on every Tier 1 platform.

## Provider versions

Provider versions are not pinned yet. Every table below applies to
"any / not yet pinned". Once real fixtures land, each row will carry the
provider version the fixture was captured from and the range it has been
tested against. Until then, treat a provider upgrade as potentially breaking
capture and re-run `attempt doctor` after upgrading.

## Claude Code (`claude_code`)

Config mechanism: `~/.claude/settings.json` (user scope) or
`.claude/settings.json` / `.claude/settings.local.json` (project scope),
`hooks` keyed by event name, matcher groups with
`{ "type": "command", "command", "timeout" }`, timeout in seconds. Payload on
stdin as JSON. See RFC 0005 section 9.1.

| Provider event | Canonical kind | Config mechanism | Verification | Notes |
|---|---|---|---|---|
| `SessionStart` | `session_started` | `settings.json` hooks | `documented` | Official docs, Aug 2026 |
| `SessionEnd` | `session_ended` | `settings.json` hooks | `documented` | |
| `UserPromptSubmit` | `prompt_submitted` | `settings.json` hooks | `documented` | Prompt text is content; gated by capture mode |
| `PreToolUse` | `tool_call_started` | `settings.json` hooks | `documented` | Carries `tool_use_id` used for pairing |
| `PostToolUse` | `tool_call_finished` | `settings.json` hooks | `documented` | |
| `PostToolUseFailure` | `tool_call_failed` | `settings.json` hooks | `documented` | Error body is content; only `outcome.class` is metadata |
| `PermissionRequest` | `permission_requested` | `settings.json` hooks | `documented` | |
| `PermissionDenied` | `permission_denied` | `settings.json` hooks | `documented` | |
| `Notification` | `notification` | `settings.json` hooks | `documented` | |
| `Stop` | `turn_stopped` | `settings.json` hooks | `documented` | |
| `StopFailure` | `turn_failed` | `settings.json` hooks | `documented` | |
| `SubagentStart` | `subagent_started` | `settings.json` hooks | `documented` | `agent_id` / `agent_type` feed `AgentRef` |
| `SubagentStop` | `subagent_stopped` | `settings.json` hooks | `documented` | |
| `TaskCreated` | `task_created` | `settings.json` hooks | `documented` | |
| `TaskCompleted` | `task_completed` | `settings.json` hooks | `documented` | |
| `PreCompact` | `compaction_started` | `settings.json` hooks | `documented` | |
| `PostCompact` | `compaction_finished` | `settings.json` hooks | `documented` | `compact_summary` is a forbidden attr (RFC 0006) |
| `ConfigChange` | `config_changed` | `settings.json` hooks | `documented` | |
| `CwdChanged` | `cwd_changed` | `settings.json` hooks | `documented` | |
| `FileChanged` | `file_changed` | `settings.json` hooks | `documented` | |
| `WorktreeCreate` | `worktree_created` | `settings.json` hooks | `documented` | |
| `WorktreeRemove` | `worktree_removed` | `settings.json` hooks | `documented` | |
| any other | `unknown` | `settings.json` hooks | `unverified` | Never dropped; `provider_event_name` preserved |

## Codex (`codex`)

Config mechanism: `~/.codex/hooks.json` (not `settings.json`, which Codex
ignores), same shape as Claude Code, `type` required, unknown fields
rejected, timeout in seconds. The user must approve hooks with `/hooks`;
trust hashes live in `~/.codex/config.toml` `[hooks.state]`, which AttemptDB
never writes. See RFC 0005 section 9.2.

| Provider event | Canonical kind | Config mechanism | Verification | Notes |
|---|---|---|---|---|
| `SessionStart` | `session_started` | `hooks.json` | `observed` | Seen in a real `~/.codex/hooks.json` |
| `SessionEnd` | `session_ended` | `hooks.json` | `observed` | |
| `UserPromptSubmit` | `prompt_submitted` | `hooks.json` | `observed` | Turn id expected; not yet confirmed in payload |
| `PreToolUse` | `tool_call_started` | `hooks.json` | `observed` | |
| `PostToolUse` | `tool_call_finished` | `hooks.json` | `observed` | |
| `PostToolUseFailure` | `tool_call_failed` | `hooks.json` | `unverified` | Not seen in the real config |
| `PermissionRequest` | `permission_requested` | `hooks.json` | `observed` | |
| `PermissionDenied` | `permission_denied` | `hooks.json` | `unverified` | |
| `Notification` | `notification` | `hooks.json` | `unverified` | |
| `Stop` | `turn_stopped` | `hooks.json` | `observed` | Final assistant message is content |
| `StopFailure` | `turn_failed` | `hooks.json` | `unverified` | |
| `SubagentStart` | `subagent_started` | `hooks.json` | `observed` | |
| `SubagentStop` | `subagent_stopped` | `hooks.json` | `observed` | |
| `TaskCreated` / `TaskCompleted` | `task_created` / `task_completed` | `hooks.json` | `unverified` | |
| `PreCompact` / `PostCompact` | `compaction_started` / `compaction_finished` | `hooks.json` | `unverified` | |
| `ConfigChange`, `CwdChanged`, `FileChanged`, `WorktreeCreate`, `WorktreeRemove` | as Claude Code | `hooks.json` | `unverified` | |
| `codex exec --json` structured output | various | import command (planned) | `unverified` | Batch import, not a hook |
| any other | `unknown` | `hooks.json` | `unverified` | Never dropped |

## Cursor (`cursor`)

Config mechanism: `~/.cursor/hooks.json`, flat
`{ "version": 1, "hooks": { "<event>": [ { "command", "timeout" } ] } }`,
timeout in seconds. Event set taken from a production installer; payload
shapes partially verified. See RFC 0005 section 9.3.

| Provider event | Canonical kind | Config mechanism | Verification | Notes |
|---|---|---|---|---|
| `sessionStart` | `session_started` | `hooks.json` | `observed` | Production installer |
| `sessionEnd` | `session_ended` | `hooks.json` | `observed` | |
| `beforeSubmitPrompt` | `prompt_submitted` | `hooks.json` | `observed` | Payload partially verified |
| `stop` | `turn_stopped` | `hooks.json` | `observed` | |
| `afterFileEdit` | `tool_call_finished` | `hooks.json` | `observed` | Category `file_edit`; no matching start event, so pairing is by FIFO (RFC 0003) |
| `afterShellExecution` | `tool_call_finished` or `tool_call_failed` | `hooks.json` | `observed` | Split by exit code; command line is content |
| `postToolUseFailure` | `tool_call_failed` | `hooks.json` | `observed` | |
| any other | `unknown` | `hooks.json` | `unverified` | Never dropped; ghost entries reported by `doctor` |

## Gemini CLI (`gemini_cli`)

Config mechanism: `~/.gemini/settings.json`, hook entries with `name` and
`timeout` in **milliseconds**. Event set taken from a production installer;
payload shapes partially verified. See RFC 0005 section 9.4.

| Provider event | Canonical kind | Config mechanism | Verification | Notes |
|---|---|---|---|---|
| `SessionStart` | `session_started` | `settings.json` hooks | `observed` | Production installer |
| `SessionEnd` | `session_ended` | `settings.json` hooks | `observed` | |
| `BeforeAgent` | `prompt_submitted` | `settings.json` hooks | `observed` | Payload partially verified |
| `AfterAgent` | `turn_stopped` | `settings.json` hooks | `observed` | |
| `BeforeTool` | `tool_call_started` | `settings.json` hooks | `observed` | |
| `AfterTool` | `tool_call_finished` | `settings.json` hooks | `observed` | Failure detection from payload not yet confirmed |
| any other | `unknown` | `settings.json` hooks | `unverified` | Never dropped |

## Other providers

GitHub Copilot, OpenCode, Pi, and other coding agents are `unverified` and
have no adapter. They are added only through the adapter contract in
`CONTRIBUTING.md` and the same fixture and privacy-canary tests.

## Platform tiers

Tier 1 means the full durability, privacy, adapter, query, snapshot,
upgrade, and uninstall test suites pass natively on that target (the matrix
in RFC 0005 section 10). "Compiles" is not sufficient. Until CI runs those
suites, every target below is a target, not a claim.

| Target | Triple | Tier | Status |
|---|---|---|---|
| macOS ARM64 | `aarch64-apple-darwin` | 1 | target, not yet verified |
| macOS x86_64 | `x86_64-apple-darwin` | 1 | target, not yet verified |
| Windows 10/11 x86_64 MSVC | `x86_64-pc-windows-msvc` | 1 | target, not yet verified |
| Windows 11 ARM64 MSVC | `aarch64-pc-windows-msvc` | 1 | target, not yet verified |
| Linux glibc x86_64 | `x86_64-unknown-linux-gnu` | 1 | target, not yet verified |
| Linux glibc ARM64 | `aarch64-unknown-linux-gnu` | 1 | target, not yet verified |
| Linux musl x86_64 (static) | `x86_64-unknown-linux-musl` | 1 | target, not yet verified |
| Linux musl ARM64 (static) | `aarch64-unknown-linux-musl` | 1 | target, not yet verified |
| Anything else (FreeBSD, 32-bit, WASM) | — | none | not targeted; the on-disk format is byte-compatible, so a database created elsewhere can be read if someone builds it |

## Support policy

- The on-disk formats (`.attemptdb/` live database and `.atdb` snapshots)
  are byte-compatible across every operating system and CPU architecture;
  a database created on one Tier 1 platform opens on every other.
- A release is cut only when all eight Tier 1 targets build and pass their
  native smoke tests. A target that fails its full suite is demoted in this
  document before the release, not after.
- Provider adapters follow the provider, not the other way round: when a
  provider changes its hook mechanism, the row is downgraded to `observed`
  or `unverified` until a new fixture is verified. `attempt doctor` reports
  `stale` when a previously active provider stops delivering events.
- Old databases: a newer binary reads every earlier format and schema
  version listed as readable in `docs/storage-format.md`; an older binary
  refuses newer format versions cleanly and preserves unknown schema fields
  it cannot interpret.

## How to contribute a verification

1. Read `CONTRIBUTING.md` (adapter contract and the fixtures rule).
2. Capture a real payload for the event, then scrub it: no real prompts,
   commands, file contents, tool output, emails, home paths, tokens, or
   repository names other than `attemptdb` itself. Synthetic payloads with
   the real shape are preferred.
3. Add the payload as `fixtures/<provider>/<event>.json` and the expected
   canonical event as `fixtures/<provider>/<event>.expected.json`.
4. Add or extend the golden test in `crates/attemptdb-adapters` so the
   fixture normalises to the expected envelope, and make sure the privacy
   canary tests pass.
5. Update the row in this document from `documented` / `observed` /
   `unverified` to `verified`, adding the provider version the payload came
   from.
6. Open a pull request using the template; note the provider version and
   how the payload was obtained.
