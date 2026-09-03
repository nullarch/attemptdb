# Agent Timeline UI product specification

> Status: partly implemented — §16 Phase B/C in progress  
> Updated: 2026-09-03  
> Scope: the local `attempt ui` product, its shareable artifacts, and the
> boundary between AttemptDB and the hosted VibeMon companion

## 1. Decision summary

AttemptDB must include a useful local visual interface. The UI is part of the
open-source product, not a preview that requires a VibeMon account to become
useful.

The product split is:

```text
AttemptDB
  local facts, projections, queries, correction, and Agent Timeline

VibeMon
  optional hosted sync, multi-device and team state, mobile attention,
  durable public sharing, notifications, and the emotional/game layer
```

The local product is named **Agent Timeline** in the interface. The command
remains `attempt ui`; `attempt open` should be added as a discoverable alias.
“Dashboard” may describe the overview internally, but it is not the product
name because the primary value is the chronology and causality of agent work,
not aggregate metrics.

The target first-use sequence is:

```text
install AttemptDB
  -> work normally with a supported coding agent
  -> run `attempt ui`
  -> see the current work and one real failed/retried attempt
  -> inspect the evidence behind an inference
  -> optionally connect VibeMon for remote, mobile, or team use
```

The UI must not make an outbound network request unless the user explicitly
starts a sync, connection, update, or external-link action.

## 2. Product outcome

The UI succeeds when a developer can answer these questions without learning
the schema or writing a query:

1. What are my agents doing now?
2. What needs my input?
3. What approaches have already been tried?
4. What failed, and what replaced it?
5. Why does AttemptDB believe that?
6. What files, commits, artifacts, and decisions resulted?
7. What was the project state at an earlier time?
8. What moved from one agent or session to another?
9. What was captured, reconstructed, omitted, or synced?

The UI is not primarily a database administration console. It is a visual
explanation of the work graph stored by the database. AttemptQL and SQL remain
available for users who need exact or novel questions.

## 3. Users and jobs

### 3.1 Primary: a developer using one or more coding agents

The developer wants to return after an interruption, understand what an agent
did, avoid repeating failed approaches, and find work that is waiting for a
human. They normally enter through the current repository and should see that
project first.

### 3.2 Secondary: an open-source evaluator

The evaluator arrived from GitHub or Show HN. They want proof within one minute
that AttemptDB is a real local database with a distinct model, rather than a
hosted tracing dashboard or a landing page. A bundled, sanitized build-history
snapshot must make this possible before the evaluator has generated events.

### 3.3 Secondary: a team or multi-device user

This user starts with the same local interface but needs continuity away from
the current machine. The UI explains exactly what VibeMon adds and what data
would be synced. VibeMon owns account, organisation, device, team, notification,
retention, and billing workflows.

### 3.4 Non-user: a manager seeking covert surveillance

Manager productivity scoring, hidden installation, comparative rankings, and
individual performance monitoring are explicitly out of scope. Team features
must be framed around continuity, coordination, evidence, and governance.

## 4. Current implementation baseline

The existing UI is a functional engineering baseline, not a blank slate:

- `attempt ui` serves an authenticated random loopback URL and opens the system
  browser.
- The server embeds its own CSS and JavaScript and loads no external assets.
- Current pages cover Now, Timeline, Session, Attempt, Evidence, Failures,
  Handoffs, Why, State, and Query.
- Current visualizations include a session waterfall and a causal SVG graph.
- Current APIs expose status, projects, sessions, timeline, attempts, failures,
  handoffs, work units, decisions, explanations, traces, state, evidence, and
  query results.
- `attempt ui export` produces a self-contained HTML artifact and can sanitize
  content.
- Applied corrections are displayed in attempt details; authoring a correction
  or retraction is currently a CLI operation, not a UI operation.

### 4.1 Shipped since this specification was written (2026-09-03)

Still server-rendered Rust, not the React package of §11.1 — the surfaces below
are implemented against the existing shell so that the product questions could
be answered before the frontend rewrite is taken on:

- **Overview** (§8.1) is current project state, the Needs You strip, live
  execution and the attempt path; the database/status tables moved below the
  fold. A session counts as *live* only while it has been active in the last
  30 minutes: a provider that never sends an end event leaves sessions open
  forever, and the rest are counted honestly instead of shown as running.
- **Needs You** (§8.4) is `attemptdb_project::attention` — a shared, versioned
  inference with evidence, confidence and an uncertainty note per item, ranked
  permission gate → input request → repeated failure → work conflict. A
  completed turn, an idle session, a cleared signal, a single failed tool call
  and anything in an ended session are excluded by rule and by test.
- **Work** (§8.3) is a three-column board over inferred work units with a
  `/work/{id}` inspector (attempt chain, decisions, handoffs, commits). No
  `Next` column is fabricated.
- **Live updates** (§11.4): `GET /api/live` is a server-sent stream of an
  opaque revision derived from file sizes and mtimes — it opens no database and
  decodes no segment. Each `data-live` region refetches only its own resource;
  a membership change asks for a reload rather than re-rendering evidence from
  JSON. The top bar owns the live/pause state.
- **Demo mode** (§9.1) is `attempt ui --demo` / `?demo=1`: a separate generated
  database in the cache directory, every event marked `reconstructed`, a banner
  on every page, and the flag carried by every link and form.
- **Image export** (§8.11): a sanitized 1200×630 SVG summary card at
  `GET /card.svg` and `attempt ui export card.svg`. It is content-free by
  construction — outcomes, failure classes, counts and repository-relative
  paths only — and the UI tests assert it against the reference story's prompt
  text, command text and out-of-repository path. PNG rasterisation is not
  shipped: it needs a font rasteriser, and the single self-contained binary is
  worth more than the convenience.

The remaining product gaps:

- `attempt ui` is in the README first-use payoff, but there is still no
  screenshot and no sub-one-minute recording.
- Navigation still carries `Failures`, `Handoffs` and `Why` as top-level
  destinations rather than Timeline/Work presets.
- Correction and retraction authoring is still a CLI operation; the UI links to
  the command instead of writing the event.
- Sharing defaults to sanitized in the UI, but `attempt ui export *.html` still
  needs `--sanitized` explicitly.
- There is no local UI for VibeMon pairing, sync scope, or last-sync state.
- The current server-generated HTML is difficult to share with VibeMon's React
  surfaces without duplicating interactions and visual semantics.

## 5. Product principles

### 5.1 Local value before cloud value

Reading local history, inspecting content captured locally, running queries,
and correcting inferences require no account and no network. VibeMon is shown
only as an optional continuation after AttemptDB has demonstrated value.

### 5.2 Work first, database second

The default information hierarchy is project -> work unit -> session -> turn ->
attempt -> tool call/event. Tables, event IDs, SQL, storage generation, WAL size,
and inference versions remain inspectable but do not dominate the first screen.

### 5.3 Facts and inferences never look identical

- Observed facts use solid marks and the label `Observed`.
- Inferred attempts, blockers, decisions, handoffs, and work units use a distinct
  inferred mark and always expose confidence, algorithm version, and evidence.
- Reconstructed events use a separate dashed style and show their source.
- Human corrections use a human mark and show both the original inference and
  corrected value.
- Missing content says why it is missing; it is never rendered as an empty fact.

### 5.4 Progressive density

The first screen answers “now” and “needs me”. Detail expands in place or in an
inspector. Raw events and query results are available but never required to
understand the common path.

### 5.5 Safe sharing by default

Every share action defaults to sanitized output. Unsanitized export requires a
specific content review, a warning that names the included content classes, and
an explicit confirmation.

### 5.6 One semantic contract, two product shells

Local AttemptDB and hosted VibeMon use the same response types, identifiers,
status vocabulary, confidence display, and timeline components. Authentication,
team context, notification actions, and branding are supplied by the shell.

### 5.7 No vanity dashboard

Counts such as tokens, prompts, tool calls, and elapsed time are supporting
context. They do not replace the central questions: what is happening, what was
tried, why did it fail, what changed, and what needs a person?

## 6. Information architecture

The primary navigation is reduced to five user jobs:

| Destination | Question answered |
|---|---|
| **Overview** | What is happening now? |
| **Timeline** | What happened, in what order? |
| **Work** | What is being attempted and what resulted? |
| **Needs You** | What specifically requires human input? |
| **Explore** | What exact question do I want to ask? |

Project, session, attempt, event, and historical-state screens are contextual
detail routes rather than primary navigation. Failures and handoffs become
saved filters/views inside Timeline and Work. “Why” becomes an action on any
inferred object. State/time travel becomes a project or timeline mode.

Recommended routes:

```text
/                         Overview
/timeline                 Timeline
/work                     Work board
/attention                Needs You
/explore                  Guided questions, AttemptQL, and SQL
/projects/:project_id     Project state and history
/sessions/:session_id     Session waterfall and attempts
/attempts/:attempt_id     Attempt, retry chain, cause, and evidence
/events/:event_id         Observed event inspector
/state?project=...&at=... Historical project state
/settings/capture         Capture coverage, privacy, and storage
/settings/sync            VibeMon connection and sync scope
```

Existing singular routes remain as redirects until public links and snapshots
have migrated.

## 7. Application shell

Desktop and laptop are the primary local targets. The shell uses three regions:

```text
+-----------------------------------------------------------------------+
| AttemptDB / Agent Timeline | project | live | Local only | search     |
+--------------+--------------------------------------+-----------------+
| Overview     |                                      | selected item   |
| Timeline     | main work surface                    | evidence and    |
| Work         |                                      | details         |
| Needs You  2 |                                      |                 |
| Explore      |                                      |                 |
|              |                                      |                 |
| Capture OK   |                                      |                 |
| Local only   |                                      |                 |
+--------------+--------------------------------------+-----------------+
```

- The top bar owns project scope, time scope, live/pause state, global search,
  and a permanent local/privacy indicator.
- The left rail owns primary navigation and compact capture/sync health.
- The main region owns the selected work view.
- A right inspector opens for an attempt, event, file, decision, handoff, or
  attention item without losing timeline context. Its selection is encoded in
  the URL and can be opened as a full page.
- At narrow widths, the left rail becomes a top tab bar and the inspector becomes
  a full-screen sheet. The local UI need not reproduce VibeMon's mobile control
  plane, but every read path and exported artifact must remain usable at 768 px.

## 8. Screen specifications

### 8.1 Overview

The Overview is the default HN screenshot and the normal return surface.

Above the fold:

1. **Current project state** — project name, current work unit/objective, phase,
   active agents, last meaningful activity, and evidence coverage.
2. **Needs You strip** — zero to three high-priority intervention items. It is
   absent when nothing needs a person; a completed turn alone is not attention.
3. **Live execution** — active sessions with current turn, in-flight tool, and
   time since the last event.
4. **Attempt path** — a compact visual chain such as
   `failed -> superseded -> in progress -> succeeded`, with click-through.

Below the fold:

- open and recently completed work units;
- recent decisions and their alternatives;
- changed paths, commits, and artifacts;
- recent cross-agent handoffs;
- capture coverage and privacy diagnostics.

When no session is active, the screen shows the most recent meaningful work
state rather than an empty “nothing running” page.

### 8.2 Timeline

Timeline provides two synchronized representations:

- a semantic feed for reading what changed;
- a zoomable execution plot for time relationships and concurrency.

Default hierarchy is project -> session -> turn -> attempt. Tool calls are
collapsed until a turn or attempt is expanded. The plot supports:

- pan, zoom, jump to now, and live tail;
- pause-on-selection so new events do not move the inspected item;
- lanes by agent/session, with provider identity visible but not over-emphasized;
- attempt outcome, retry/supersession edges, handoff edges, and waiting spans;
- captured, reconstructed, inferred, and corrected visual treatments;
- URL-backed filters for project, agent, outcome, item kind, source, path, and
  time range;
- keyboard navigation and a non-visual list/table equivalent.

Failures and handoffs are named presets, not separate top-level products:

```text
/timeline?view=failures
/timeline?view=handoffs
```

### 8.3 Work board

The Work page is an evidence-backed board over inferred WorkUnits. It is not a
user-authored task manager.

Columns:

- **Active** — open work whose phase is not blocked;
- **Blocked** — open work with a blocking phase or a high-precision attention
  signal;
- **Recently finished** — completed or abandoned work in the selected range.

A card contains objective or “content not captured”, phase, status, actors,
attempt count, failure count, touched paths, span, confidence, and evidence.
Selecting it opens a work inspector containing the full attempt chain,
decisions, artifacts, commits, and handoffs.

The UI must not fabricate a `Next` column from weak inference. Planned work
belongs to VibeMon project memory or to a future explicit event type.

### 8.4 Needs You

Needs You contains only high-precision intervention items. Ranking is:

1. unresolved permission or approval gate;
2. explicit question, credential, choice, or review request;
3. repeated equivalent failures with no successful superseding attempt;
4. another evidence-backed blocker above the configured confidence threshold.

Each item shows:

- the requested human action in one sentence;
- affected project, session, and agent;
- how long it has been waiting;
- why it was classified, its confidence, and evidence;
- `Open session`, `Show why`, and `Copy continuation brief` actions;
- `Not blocked` / `Correct` where the inference is wrong.

Normal completion, ordinary inactivity, and a single generic tool failure must
not enter this queue.

### 8.5 Project detail and historical state

Project detail combines current state, open work, recent timeline, decisions,
artifacts, commits, and handoffs. A time control changes the entire page to
`State at <timestamp>`; current and historical state never appear mixed.

The time control supports exact timestamps and human forms such as `-2h`,
`today`, and `yesterday`. Moving it updates the URL after a short debounce so
the state can be reopened or exported.

### 8.6 Session detail

Session detail contains:

- provider, project, agent identities, capture source, and coverage;
- objective or the explicit reason objective text is unavailable;
- start/end state and active/waiting/blocked state;
- a turn/tool waterfall;
- attempts nested under turns;
- input signals and whether they were cleared;
- tool calls, paths, commits, and artifacts;
- handoffs entering or leaving the session.

The initial view shows semantic units. Raw event order is a secondary tab.

### 8.7 Attempt detail

Attempt detail is the proof surface for AttemptDB's product thesis. It shows:

- objective and inferred approach;
- outcome, failure class, duration, and affected paths;
- `supersedes` / `superseded by` as an explicit attempt chain;
- a causal graph with observed and derived edges visually separated;
- the explanation for failure or success when supported;
- tool calls and evidence events;
- confidence, uncertainty, algorithm version, and computation source;
- the original inferred value and any human correction.

`Correct inference` opens a form that writes a Correction event; it never edits
the inferred record in place. Retraction is placed behind a separate destructive
flow with a preview of affected events and explicit confirmation.

### 8.8 Event inspector

An event is labelled **Observed fact** or **Reconstructed event**. The inspector
shows the canonical fields first, then content and raw payload in collapsed
sections. Content is escaped and never interpreted as HTML or instructions.

When a field is absent, the inspector distinguishes:

- not emitted by the provider;
- excluded by capture mode;
- redacted by policy;
- unavailable in a reconstructed source;
- unavailable because a decryption key is missing.

### 8.9 Explore

Explore starts with guided questions rather than an empty editor:

- Why is this project blocked?
- Show failed attempts that were later superseded.
- Trace the causes of this attempt.
- Show handoffs between providers.
- Reconstruct project state at a point in time.

It then exposes AttemptQL and SQL modes, schema/table browsing, syntax help,
keyboard execution, bounded results, CSV/JSON copy, and ID links back into the
visual interface. Scope and statement are encoded in the URL.

### 8.10 Capture and privacy

Capture health is always available from the shell and expands into:

- effective capture mode and the policy source that selected it;
- per-provider hook status, last event, adapter version, and coverage;
- captured versus reconstructed counts;
- dropped/redacted field classes without leaking their values;
- database path, size, daemon state, and read-only/snapshot state;
- a precise explanation of what stays local and what may sync.

Hook wiring is presented as a detected status and repair action. The UI must not
teach users to manually enter hidden slash commands. Installation remains a
single installer/CLI operation, and the UI calls the same structured installer
logic if a user chooses `Repair capture`.

### 8.11 Sharing

Share/export is available from project, work unit, session, attempt, and
time-range scopes.

Default outputs:

- self-contained sanitized HTML replay;
- sanitized PNG/SVG summary card for README, issue, or social use;
- sanitized `.atdb` snapshot when the recipient should run queries.

Before export, a manifest lists included item counts and excluded content
classes. Sanitization removes prompts, commands, tool output, raw payloads,
absolute paths, home directories, secrets, and identifiers that are not needed
for causal continuity. `Built with AttemptDB` attribution is on by default and
may be disabled.

### 8.12 VibeMon connection

The shell footer shows one of:

- `Local only`;
- `VibeMon connected · last synced ...`;
- `Sync paused`;
- a specific sync error.

`Connect VibeMon` appears after the user has captured at least one useful
session, or when they explicitly open Sync settings. It opens a browser pairing
flow and returns a device credential without asking the user to paste a key.

The consent screen lists the selected profile and examples of fields that will
and will not leave the device. The recommended `semantic` profile syncs redacted
metadata and labelled inferences, not prompts, commands, source, or tool output.
Full content sync is a separate explicit choice.

VibeMon adds:

- multi-device and organisation aggregation;
- hosted history and stable public/team links;
- mobile Needs You and push notifications;
- team handoff, overlap/conflict, RBAC, policy, audit, and retention;
- VibeMon's optional emotional and game layer.

It must not gate the local Timeline, Work, evidence, query, correction, or
sanitized export features.

## 9. First-run and failure states

### 9.1 No events yet

Show a three-step status, not an empty chart:

1. database created;
2. detected agents and hook status;
3. waiting for the first real event.

Offer `Open the AttemptDB build-history demo` and `Run capture diagnostics`.
The demo is a bundled sanitized snapshot and is visually marked `Demo data`.

### 9.2 Partial capture

Render available history and show the missing boundaries. Do not imply that a
session started or ended when the provider did not emit those events.

### 9.3 Writer lock or snapshot

Read-only state is visible but non-alarming. Correction, retraction, capture
configuration, and sync mutation actions are disabled with the exact reason.

### 9.4 Decryption key unavailable

Metadata and projections remain usable. Content areas explain that encrypted
content exists but cannot be opened with the current key context.

### 9.5 Large history

The shell and recent scoped state render first. All-history materialization runs
only for a view that requested it and reports progress without freezing current
work. Every collection is cursor-paginated or windowed.

## 10. Visual language

The local UI should feel like a precise development instrument, not enterprise
BI and not VibeMon's game surface.

- Neutral background, high information contrast, compact but readable type.
- Monospace is reserved for identifiers, time, paths, queries, and raw values.
- Agent/provider identity has a stable color and glyph.
- Outcome uses both icon/text and color: succeeded, failed, superseded,
  abandoned, in progress, unknown.
- Time always runs left to right in plots.
- Solid line/shape = observed; dashed = inferred or reconstructed as labelled.
- Evidence is reachable from every inference in one interaction.
- Confidence is shown as a value plus a plain-language explanation; it is not a
  decorative progress ring.
- Reduced-motion, dark, and light modes are first-class.

The HN launch image should show a recognizable sequence with at least one failed
attempt, a superseding attempt, a successful outcome, a handoff, and linked
evidence. Aggregate charts are secondary.

## 11. Frontend and API architecture

### 11.1 Shared React package

Replace the local product surface with a React + TypeScript application built as
static assets. Use Vite for the local build; do not run Node on an end user's
machine. Assets are embedded in the Rust binary and the CSP permits no external
scripts, fonts, styles, frames, or connections by default.

Recommended packages:

```text
web/agent-timeline/
  src/domain/          generated API types and semantic formatters
  src/components/      timeline, work, attention, evidence components
  src/local/           local shell and local capabilities
  src/data/            TimelineDataSource interface

crates/attemptdb-api/
  shared Rust response DTOs and read handlers

crates/attemptdb-ui/
  loopback host, auth, embedded assets, local mutation endpoints, export
```

Publish the pure domain/components layer as a versioned package that VibeMon can
consume. VibeMon supplies its own Next.js shell and hosted data source. Components
must not import Supabase, local filesystem concepts, auth, billing, or VibeMon
game state.

VibeMon's existing Mission Control, Timeline, and Waterfall code should first be
converted to pure presentational components over AttemptDB response types. Do not
copy its current legacy `hook_events` derivation into AttemptDB.

### 11.2 One read contract

Local UI and `attemptdb-server` should expose the same versioned response DTOs
for:

```text
overview, live, projects, sessions, timeline, work, attention,
attempts, decisions, state, events, query, devices, and sync status
```

Every inferred DTO carries `evidence`, `confidence`, `algorithm_version`, and
`computed_by`. Every content field carries availability information. Generate
TypeScript types and a checked API schema from the Rust contract.

Current `/api/*` routes remain until the equivalent `/api/v1/*` routes have
parity. The hosted `/v1/*` read API and local `/api/v1/*` differ only in base URL,
authentication, tenancy, and declared capabilities.

### 11.3 Data source capability model

The frontend selects behavior from explicit capabilities, not hostname checks:

```text
local | hosted
live_updates
write_corrections
write_retractions
configure_capture
configure_sync
teams
public_links
notifications
```

Filters, selected item, inspector state, and historical time are URL state. UI
preferences such as theme and panel width may remain browser-local.

### 11.4 Live updates

Add a lightweight invalidation stream that announces database generation,
affected project/session IDs, and last event time. The client refetches the
smallest affected resource. Server-sent events are sufficient locally; polling
is the fallback. Hosted VibeMon can implement the same invalidation interface
over its realtime transport.

### 11.5 UI server lifecycle

The normal `attempt ui` / `attempt open` path asks the per-user daemon to expose
the UI on loopback, exchanges a one-time URL token for an HttpOnly, SameSite
cookie, and returns control to the terminal after opening the browser. A
foreground mode remains available for diagnostics and systems without the
daemon.

Non-loopback serving stays an explicit expert option with a warning. The normal
remote experience is VibeMon or an SSH tunnel, not an accidentally public local
server.

### 11.6 Static export

Static export remains a self-contained renderer with no live API dependency.
It shares tokens and visual semantics with the application but intentionally
does not include correction, sync, or raw query capabilities.

## 12. Privacy and security requirements

- Bind loopback by default and reject public binding without explicit consent.
- Exchange the URL token once; remove it from browser history immediately.
- Use HttpOnly, SameSite=Strict session cookies and a strict CSP.
- Embed all runtime assets. A network test must prove zero unsolicited requests.
- Escape all event and content fields; never inject captured text as markup.
- Treat prompt/tool content as untrusted data, not application instructions.
- Keep read-only query limits, timeouts, and DDL/DML rejection.
- Require CSRF protection and origin validation for every local mutation.
- Show a diff/impact preview before correction or retraction writes.
- Default every export to sanitized and run privacy canaries over every format.
- Never add third-party analytics to the local interface.
- Display effective capture and sync policy continuously, not only in settings.

## 13. Cross-platform and accessibility requirements

- The same embedded web build runs on macOS, Windows, and Linux.
- Browser opening uses the operating system's normal URL mechanism and failure is
  non-fatal; the URL is always printed.
- Support current stable Safari, Chrome, Edge, and Firefox where available.
- All primary actions are keyboard reachable, with a visible focus state.
- Plot information has a list/table equivalent and screen-reader labels.
- Status never relies on color alone.
- Text and controls meet WCAG 2.2 AA contrast and target-size requirements.
- Honor reduced-motion and operating-system light/dark preference.
- Long paths, IDs, prompts, and tool output wrap or scroll without changing page
  width.

## 14. Performance budgets

Performance is a product requirement even when implementation capacity is not a
constraint.

- The shell and cached Overview become usable within 1 second on the reference
  machine.
- A warm Overview or recent Timeline request completes within 500 ms at p95 on
  the 200 k-event benchmark.
- The first screen never waits for an all-history projection.
- Live activity appears within 2 seconds of durable ingestion.
- Timeline pan, zoom, selection, and inspector opening stay responsive with 500
  visible semantic items; larger ranges are virtualized/aggregated.
- APIs return bounded, cursor-paginated results and expose truncation.
- The 1.45 M-event benchmark has a dedicated UI scenario measuring time to first
  recent state, peak memory, and background all-history completion.

## 15. Product measurement without local telemetry

AttemptDB does not phone home to measure usage. Product validation uses:

- scripted time-to-first-insight tests on clean machines;
- moderated tests with design partners;
- GitHub release/download and issue data;
- opt-in VibeMon pairing attribution;
- an optional manually submitted diagnostic bundle that previews every field.

Launch usability targets:

- a new evaluator opens the bundled demo and finds a failed/superseding attempt
  in under 60 seconds;
- a new installed user reaches their first real visual timeline without an
  account or manual hook command;
- at least 80% of design-partner sessions correctly identify which displayed
  items are facts versus inferences;
- no normal completed turn is classified as Needs You in the acceptance corpus;
- the complete HN demo is reproducible in under one minute;
- AttemptDB-to-VibeMon connection is measured only after a user has received
  local value.

## 16. Delivery plan

Phases are ordered by product dependency and validation, not engineering effort.

### Phase A — contract and visual foundation

- Define versioned shared read DTOs and frontend capabilities.
- Freeze the demo snapshot and screenshot corpus.
- Establish visual tokens and fact/inference/reconstruction grammar.
- Add Playwright, accessibility, network-isolation, and privacy-canary harnesses.
- Preserve existing pages as a behavioral reference during migration.

### Phase B — HN-ready local product

- Ship the new shell, Overview, Timeline, Session, Attempt, Event, and Explore.
- Add first-run diagnostics and bundled demo mode.
- Add live invalidation and URL-backed filters/inspectors.
- Make sanitized sharing the default and add visual export.
- Put `attempt ui` in the README install payoff with a real screenshot and
  sub-one-minute recording.
- Keep the product fully useful offline and without VibeMon.

### Phase C — evidence-backed work control

- Ship Work and the dedicated high-precision Needs You queue.
- Add correction authoring and guarded retraction flows.
- Complete project time travel, capture health, and privacy explanations.
- Add continuation-brief copy and deep links among work, sessions, attempts, and
  evidence.

### Phase D — VibeMon companion

- Consume the shared component/domain package in VibeMon.
- Replace VibeMon's legacy `hook_events` derivation with AttemptDB read DTOs.
- Ship passwordless device pairing, explicit sync profile selection, and device
  status in the local UI.
- Add hosted multi-device timeline, stable sharing, mobile Needs You, and team
  views without changing the local feature contract.

### Phase E — post-launch learning

- Tune attention precision against real correction data.
- Add saved views only after repeated user behavior justifies them.
- Add richer build replay and PR summaries after the core timeline is validated.
- Consider a thin Tauri/tray shell only if users repeatedly need persistent
  background access beyond `attempt open`.

## 17. Release acceptance gates

The HN-ready UI is complete only when all of the following are true:

- A clean macOS, Windows, and Linux install opens the local UI without Node,
  Docker, signup, email, or API keys.
- The correct current repository is selected automatically.
- A new event updates active work without a manual page reload.
- The reference failed attempt can be followed to its superseding attempt and
  underlying evidence.
- Facts, inferences, reconstructed events, and corrections are visually and
  textually distinguishable.
- Partial coverage and omitted content are explained rather than hidden.
- The bundled snapshot produces the same demo on every Tier 1 platform.
- Sanitized HTML, image, and snapshot exports pass privacy-canary tests.
- No local page makes an unsolicited external network request.
- Keyboard-only and screen-reader users can reach every core fact shown in the
  visual timeline.
- README commands, screenshot, recording, and outputs match a tagged release.
- VibeMon is optional, below the local value proposition, and its consent screen
  precisely describes sync scope.

## 18. Deliberate non-goals

- Generic BI/chart builder.
- Prompt playground or model gateway.
- Full source-code diff/review replacement.
- User-authored issue tracker or project-management board.
- Team productivity score or agent leaderboard.
- Mandatory cloud account, cloud query, or hosted decryption.
- Desktop-native database implementation inside a Tauri/Electron process.
- A second hosted timeline product independent from VibeMon.

## 19. External product patterns

The boundary follows established local-first/open-source patterns:

- DuckDB ships a local browser UI while MotherDuck connection is an explicit
  opt-in: <https://duckdb.org/docs/current/core_extensions/ui>.
- Jaeger's query service includes the Web UI used to search and analyze stored
  traces: <https://www.jaegertracing.io/docs/1.76/architecture/>.
- Supabase enables a local Studio dashboard while keeping the hosted platform a
  broader product: <https://supabase.com/docs/guides/local-development/cli/config>.

These examples do not determine AttemptDB's visual design. They support the
product boundary: local data must have a local inspection path, while hosted
coordination can remain a separate optional business.
