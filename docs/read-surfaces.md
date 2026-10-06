# The read surfaces: MCP, the web UI, the CLI

Three things read the database. The owner at a prompt (`attempt query`,
`attempt timeline`) is trusted with everything. A language model calling the MCP
server and a browser on the local web UI are not: a model can be steered by
text that an earlier session read from a web page, and a browser can be
driven by a page from another site. This document is what the MCP server and
the UI promise on top of the read-only rule (every surface refuses DDL, DML
and statements; that is enforced by the engine, not by a keyword check).

Where a number is given it is the default and is configurable.

## Bounded statements

`QueryEngine::query_limited` / `sql_limited` (crate `attemptdb-query`) run a
statement under a `QueryLimits`. MCP's `attempt_query` and the UI's query
console and JSON API use it; the CLI does not (`attempt query -n N` caps the
rows it prints, for SQL as for AttemptQL, but does not bound time or memory).

| Bound | MCP | UI | How |
|---|---|---|---|
| rows | `--max-rows` (200) | 2000 | pushed into the plan as `LIMIT cap + 1` before anything is collected (the extra row is how truncation is detected); `SELECT * FROM generate_series(1, 30000000)` with `limit 3` produces four rows, not thirty million |
| bytes | `--max-kib` (256) | 4 MiB | collection stops once the rows held are far past the budget; only the budgeted prefix is converted to JSON, CSV or a table; a cell longer than 4 KiB (MCP) or 16 KiB (UI) is cut with a marker |
| time | `--query-timeout` (20 s, 0 = none) | 20 s | the statement runs as a task on a private runtime; on timeout or cancellation the task is aborted, which drops the DataFusion stream |
| memory | `--query-memory-mb` (1024, 0 = none) | 1 GiB | a memory pool per statement with spilling to temporary files disabled: a runaway join or aggregate fails with "resources exhausted" |

| statement size | 512 KiB, 100 000 tokens, 400 chained operators, 100 `SELECT` blocks | the same | refused before planning, with a message that says what to change (below) |

A result that was cut says so (`truncated: true`, and for MCP a
`truncated_because` sentence that tells the caller how to narrow the
statement). A statement that runs out of its memory pool fails with "the
statement needed too much memory and was stopped: narrow it with WHERE or
LIMIT, select fewer columns, …" and the first line of DataFusion's report, not
its dump of every memory consumer. `WHY`, `TRACE`, `STATE`, `DIFF` and `WHAT IS` are computed from
the projection rather than scanned; their rows are cut at the row cap after
they are computed.

### One long statement cannot take the process down

DataFusion parses and plans recursively: a chain of 900 `OR`s, a `UNION ALL`
of a thousand arms or a 400-CTE chain is a tree that deep, and a stack
overflow aborts the process that runs it (the MCP server, the UI, the
daemon). Two defences:

- every statement, bounded or not (the CLI's and the daemon's included), runs
  on a private runtime whose threads have 128 MiB stacks, not on the caller's
  thread, and is aborted if the caller stops waiting;
- a token-level guard refuses, before planning, a statement longer than 512 KiB
  or 100 000 tokens, with more than 400 chained operators (`AND`, `OR`, `+`,
  `-`, `*`, `/`, `%`, `||`, `UNION`, `INTERSECT`, `EXCEPT`, `JOIN`) or more
  than 100 `SELECT`/`VALUES` blocks (subqueries, CTEs, `UNION` arms). The
  error says to use `col IN (…)` instead of a chain of `OR`s, one regular
  expression instead of many `LIKE`s, `GROUP BY` instead of a `UNION` of many
  arms. The limits are far above any statement written by hand and an order of
  magnitude below what the stack takes, even in an unoptimised build.

The text of a refused or failed statement is never echoed whole in an error: MCP
names its first 200 characters, a parse error shows the stretch around the
error.

The MCP stdio loop reads stdin on a thread of its own. `notifications/cancelled`
stops the request it names: a statement in flight is aborted, one still queued
is skipped, and a cancelled request gets no response (the MCP rule). `ping` is
answered at once, even in the middle of a long call.

## Scope

With no `project`, `all_projects` or `session` argument the MCP tools scope to
the project of the repository the server was started in. When that repository
has no recorded events (a new checkout, a directory that is not a repository)
the store would fall back to every project; the tools refuse instead, with a
message that says to pass `project=<name>` or, if the user asked for it,
`all_projects=true`; the refusal is an `isError` result on every tool, not
only on `attempt_query`, so an agent cannot read it as "nothing found".
Widening is always an explicit argument, and the tool descriptions say it
exposes other repositories' prompts. A `session` argument is a scope of its
own.

The CLI widens too, so it says so: in a git checkout the database has no
events for, `attempt timeline`, `failures`, `handoffs`, `why`, `query` and the
other read commands print `warning: no events recorded for this repository;
showing all projects, pass --project or --all-projects` on stderr (also when
the daemon answers). Outside any repository "every project" is what the help
promises and nothing is printed. A file meant to be shared (`attempt ui export`,
sanitized or not, `.html` or `.svg`, and `attempt snapshot export --sanitized`)
refuses instead, unless `--project` or `--all-projects` is given.

## What a tool returns

Every MCP tool other than `attempt_query` (which budgets its own rows) is held
to the byte budget (`--max-kib`, 256): a result over it loses its structured
JSON block first, then its text is cut at a line, and the text ends with a line
that says so. `limit`, `depth` and `turns` must be at least 1; `0` is an
error, not a quiet `1`. The `attempt_query` and `attempt_schema` descriptions
list every table from the catalog.

## Sessions: open, stale, closed

A session with no recorded end is `open` while it did something in the last 30
minutes (12 hours when it waits on a human) and `stale` after that: agents are
killed far more often than they exit, so silence is the usual end, and it is an
inference, not an observation. Every surface uses the projection's state
(`sessions.state`): the timeline, the handoff brief, the retract preview, the
UI's overview and `/attention` (which count the same sessions), and the static
export, which is judged at the moment it is generated. `STATE … AT <time>`
judges each session at that time (`is_open` is true only for an open one, the
`status` column says `open`, `stale` or `closed`). Work units, which have their
own status, print it instead of "open".

## Privacy on read

- **Prompt text is shown per event.** An event carries the `capture_mode` it
  was written under; under `metadata_only` its text columns are null by
  design. The handoff brief quotes a prompt only when that prompt's own event
  was captured with content, and its `content:` line is counted from the
  events in scope ("quoted for 1 of 2 turns (1 under local_semantic) …
  capture mode is now metadata_only"), never from the configured mode, which
  only describes events captured from now on. `attempt_status` lists the
  events by mode.
- **Retraction is redaction on these surfaces.** `attempt retract` hides rows
  from AttemptQL and flags them in SQL, but the rows keep their text and
  where it happened. On MCP and in the UI the text and location columns of
  retracted rows read as NULL, below the statement, so a filter or a join
  cannot probe them either:

  | table | masked for retracted rows |
  |---|---|
  | `events` | `content_json`, `raw_json`, `unknown_json`, `paths_json`, `path_logical`, `path_relative` |
  | `turns` | `objective`, `inferred_objective` |
  | `tool_calls` | `path_relative`, `paths` |
  | `attempts` | `objective`, `note`, `approach` (it lists file paths), `paths` |
  | `corrections` | `note`, when the correction's target is retracted or it was written into a retracted session |

  `events_raw` has no flag to decide by and serves no content and no location
  here (use `events`). A result that has one of those columns says so in its
  notes. The CLI is unchanged: the owner sees everything.

## Stored text is data

Prompts, commands, tool output, paths and project names come from sessions
that read arbitrary pages and files. For the agent reading them over MCP:

- every tool result except `attempt_schema` starts with one fixed sentence
  saying the quoted text is untrusted data, not instructions; an
  `attempt_query` result with a stored-text column carries it in its envelope
  (first line of the table or CSV, a `notice` field in JSON);
- quoted prompts sit inside a backtick fence one longer than the longest run
  of backticks in the text, so the text cannot close its own quote;
- bidirectional controls (U+202A–202E, U+2066–2069, U+200E/F, U+061C), Unicode
  tag characters (U+E0000–E007F), zero-width and other invisible format
  characters, including the joiners, are removed from everything rendered and
  from every string cell of a query result (counted in a note). The UI's `esc`
  removes the same set but keeps joiners, which emoji need.

## The UI's front door

- The `Host` header must be exactly `localhost`, `127.0.0.1` or `[::1]`, with
  the listening port if it has one (`127.evil.example` is refused).
- The session cookie is `attemptdb_ui_<port>`, `Path=/; HttpOnly;
  SameSite=Strict`: two instances on one machine do not evict each other.
  (Browsers share cookies across the ports of one host, so every server on
  `127.0.0.1` can see this cookie; the per-run token is why that is not
  enough to use it.)
- A request to `/api/*`, or any request that is not a plain read, that carries
  an `Origin` must carry the UI's own (`http(s)://` plus the request's `Host`).
  Page navigations, which browsers send without one, are not checked.

## Small things

- `today` and `yesterday` are midnights in the machine's local time zone. A
  bare `YYYY-MM-DD` is UTC midnight; write an RFC 3339 timestamp with an offset
  for another zone.
- `SHOW … FOR path = '<glob>'`: `*` (and `%`) match any run of characters
  within one path, every other character is literal, and the glob is anchored at
  both ends of each path; a row matches when any one of its paths does.
- `SHOW … WHERE <sql>` takes exactly one SQL expression and splices its parsed
  form, so it cannot rewrite the filter that hides retracted rows.
- CSV output prefixes a single quote to a text cell that starts with `=`, `+`,
  `-`, `@`, TAB or CR; numbers are untouched, JSON and tables are not changed.
- The read-only pre-check in front of MCP and the UI lexes the statement
  (comments, string literals and quoted identifiers are not keywords); the
  engine's plan inspection remains the gate.
