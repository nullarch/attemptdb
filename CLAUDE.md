# CLAUDE.md — AttemptDB

**The instructions for this repository live in [`AGENTS.md`](AGENTS.md). Read
it first.** It is the canonical file for every coding agent; this one exists
so Claude Code finds it, and holds only what is specific to Claude Code.

Start of session, in order:

1. `AGENTS.md` — layout, commands, invariants, conventions.
2. `PROGRESS.md` — the execution log and the "what is next" list.
3. `attempt schema` — the table and column catalog, when the task involves
   querying. It needs no database and no network.

Claude Code specifics:

- The MCP server is already wired in `.mcp.json` (`attempt mcp`). Prefer
  `attempt_schema` before `attempt_query`, and `attempt_handoff_brief` at the
  start of a session that continues someone else's work.
- Hooks for this repository are installed by `attempt hook install`. Keep them
  on while working here: this repository's own history is the demo dataset.
