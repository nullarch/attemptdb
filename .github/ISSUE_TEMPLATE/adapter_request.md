---
name: Adapter request
about: Request support for a coding agent that AttemptDB does not yet capture
title: "adapter: "
labels: adapter
---

<!--
Do NOT paste real prompts, tool output, transcripts, or private paths.
Payload examples must be synthetic or scrubbed.
-->

## Provider

- Name and stable identifier (e.g. `opencode`):
- Vendor / project URL:
- Version(s) you use:

## Hook or configuration mechanism

- How does the provider expose lifecycle and tool events (hooks, plugin API, structured log, MCP, other)?
- Configuration file path(s) and format (JSON / TOML / other):
- Does the user need to approve or trust hooks after installation? How?
- Command invocation shape (argv, stdin JSON, environment variables, timeout units):

## Events

<!-- List every event name the provider emits, with a one-line description.
Note which ones carry a session id, turn id, tool-call id, or agent id. -->

| Provider event | Description | Ids present |
| --- | --- | --- |
| | | |

## Payload shape availability

- Are payload shapes documented publicly? Link:
- Have you observed real payloads for every event listed above? Which are unverified?
- Are there fields that vary by version?

## Synthetic fixture

- Can you provide a synthetic or scrubbed fixture for each event under `fixtures/<provider>/`? (yes / partially / no)

## Privacy considerations

<!-- Which payload fields contain prompts, commands, file contents, tool
output, transcript paths, or user identifiers? These must be routed to
`content`/`raw`, never `attrs`. -->

## Additional context
