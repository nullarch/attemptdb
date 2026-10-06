# Changelog

All notable changes to AttemptDB are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); this project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) and is pre-1.0, so
minor versions may carry breaking changes until 1.0.

On-disk format versions are tracked separately in
[`docs/storage-format.md`](docs/storage-format.md) §13 and change only with an
RFC; a release that bumps one says so here.

## [Unreleased]

<!-- 2026-10-06 review remediation: one section per theme -->

### Found and fixed by the pre-release bug hunt

- **A text ending in `Authorization: Bearer ` could brick a database.** The
  masker indexed one byte past the end and panicked; the event stayed in a
  claimed spool file, so `status`, `query`, `timeline` and `doctor` panicked on
  every run until the file was deleted by hand (and the daemon's writer thread
  died while the process said "running"). The scanner checks its bounds, masking
  runs under a panic guard (the event loses its content, not the database), a
  value is scanned at most 512 bytes (masking was quadratic: 175 s for 1.8 MB of
  `password=` lookalikes), and bare `attempt import` and `snapshot export` now
  go through the same content gate as the daemon (they stored secrets in the
  clear and, with a missing key, plaintext).
- **`ATTEMPTDB_NO_DAEMON` is honoured where the OS service manager is reached**
  (`hook install`, `daemon install`, `update`): launchd and systemd services are
  per user, not per `HOME`, so a test or sandbox with a temporary `HOME` used to
  rebind the real daemon.
- **`attempt uninstall --purge-data` deletes only what AttemptDB made.** A
  directory you named with `--data-dir` keeps its other files; `--db` and a
  project-local `.attemptdb` leave the per-user directories alone; a database
  directory needs its `ATTEMPTDB` marker.
- **Hooks stay fast under contention.** Creating a private spool file no longer
  syncs its header (100 parallel hooks: p50 269 ms -> 63 ms, 0.2.13: 90 ms), a
  hook waits up to 3 ms for the inbox lock before going private, a garbled
  `device.json` is repaired in milliseconds instead of costing every hook 0.4 s,
  and `attempt hook ...` never exits 2 because of its arguments.
- **The daemon stops burning CPU.** With sync connected it reopened the database
  every 5 s and loaded every segment's ids (~25% of a core idle, ~90% active, 16
  GB for the first inference): an idle tick now costs a few `stat` calls,
  opening with a few WAL events no longer reads any segment's ids, the inference
  set is recomputed at most every 10 minutes from a telemetry-free stream
  (skipped with a notice above 250,000 events), and OTel from a session no hook
  named no longer stalls the writer for ~1.2 s every 10 s.
- **A content key that cannot be read no longer costs the content for good.** A
  locked key store holds new events in the spool (24 hours / 512 MiB) and they
  are imported with their content once the key reads.
- **Sync and server:** a device cannot retract another device's session by
  changing the case of the target type; `sync forget` is no longer undone by the
  next upload; home directories are scrubbed wherever they appear near the front
  of a path (WSL, `/var/home`, `/Volumes/...`); `sync policy` accepts the
  spellings people paste and warns about an entry that matches nothing; the
  VibeMon migration's imported history is held until `attempt sync history
  include`, and every surface says so; the consent watermark is a sequence
  number, not the wall clock; `/v1/query` is bounded; an over-limit body gets a
  readable 413; the Docker/Caddy deployment overwrites `X-Forwarded-For`;
  `sync retry-set-aside` re-sends events a server refused.
- **Read surfaces:** one long statement no longer aborts the MCP server, the UI
  or the daemon (a 128 MiB stack, and a statement beyond 400 chained operators,
  100 SELECT blocks or 512 KiB is refused with advice); `attempt correct` and
  `attempt retract` no longer fail with "database is locked" without a daemon;
  `--failure-class` is kept and read back; exports require `--project` or
  `--all-projects` outside a repository AttemptDB knows; stale sessions are
  never shown as open; `--since -2h` works; `-n` caps SQL rows; retracted events'
  paths are masked on MCP and UI; MCP tools are held to the byte budget.
- **Setup and install:** `attempt setup --capture-mode` is validated and applies
  to new events on an existing database (it was silently ignored); Homebrew/Nix
  installs keep the stable symlink in hook commands; `install.sh` finds the
  latest release without the rate-limited API, says "could not download" for
  network errors and prints working steps for releases that predate `attempt
  setup`; `attempt update --to <older>` needs `--force`; `doctor` says how to fix
  stale wiring; explicit OpenTelemetry opt-outs are kept; `uninstall` exits 1
  when an agent could not be cleaned and honours `--json`; parallel first-time
  `setup` runs no longer fail.
- **Integrity messages:** `attempt verify` and `doctor` report (and exit non-zero
  for) a newest manifest generation that cannot be used; `repair` lists blobs it
  cannot restore; a damaged record in a spool file no longer drops the intact
  records after it; `attempt keys status` is instant on millions of blobs (an
  estimate; `--full` counts exactly); `attempt tables` needs no database.
- **History import:** a transcript line over 32 MiB is skipped and reported;
  many small rollouts import in shared batches (2,000 files: 30 s -> 0.5 s); an
  import through the daemon asks it to import now (14 s -> 0.7 s); masking is
  about five times faster.
- **Pairing made a phantom "active session".** `sync connect` records the
  person's consent as an event from the provider `attemptdb`, and the server's
  `/v1/live` (and `last_event` in the stored facts) counted it as an agent's
  activity: for ten minutes after pairing the live view showed a session of
  `attemptdb`. AttemptDB's own records still count as events, but not as
  activity — live, and again when the server restarts and reads the facts back.
  Found by the install smoke test on its first run.
- **A command could fail with "locked" while the daemon was merely slow.**
  `attempt correct` and the consent record of `sync connect` handed their events
  to the daemon with the hook's 100 ms budget; past it they tried to open the
  database themselves while the live daemon held the lock. They now wait as long
  as an interactive command does (250 ms to connect, 5 s for the answer); the
  hook keeps its budget.
- **Windows: `USERPROFILE` is honoured.** The home directory came from the shell
  API only, so a wrapper (or a test) that set `USERPROFILE` to another directory
  still had `setup` read and wire the real profile. `USERPROFILE` is read first,
  as `std` does; for a real user it is the same folder.

### Security and privacy

- **`rustls` 0.23.43 → 0.23.45** (RUSTSEC-2026-0285: TLS 1.3 handshake messages
  accepted across encryption-level boundaries). It is a transitive dependency of
  the sync client and server; only `Cargo.lock` changes.
- **A `config.json` that cannot be used now captures metadata only.** A typo
  (`"metadata-only"`), a trailing comma or an unknown `capture_mode` used to
  fall back to the default `local_semantic` and put full prompts in the
  plaintext spool. Any unusable file now means `metadata_only`; `attempt
  doctor` and `attempt status` say why, and `Config::save` keeps the broken
  file as `config.json.invalid-<time>`.
- **A project-local `.attemptdb` is trusted only if it is yours.** It must be
  a real directory owned by you, not world-writable, with no symlink inside
  `spool/`, `wal/` or `manifest/`; otherwise it is skipped and `attempt
  doctor` lists it. Spool and WAL files are created and opened without
  following symlinks, so a planted link can no longer truncate other files.
- **Secrets are masked before content is stored** (`redact_secrets`, on by
  default, in `config.json`), as RFC 0006 §5 says, not only at upload. The
  rules are `secrets-v3`: `password=…`, `"token": "…"`, `--password …`, URL
  credentials, `Authorization: Bearer …`, AWS secret keys and legacy `sk-`
  keys, Korean `비밀번호:`/`토큰:` labels, command-line passwords (`mysql -p…`,
  `curl -u user:pass`, `sshpass`, `docker login -p`, `openssl -pass pass:`,
  `htpasswd -b`), `.netrc`, XML `<password>`, Docker `auth`, kubeconfig key
  data, `Cookie`/`Set-Cookie`, ECS/Kubernetes name/value pairs, `.npmrc`
  tokens and more provider tokens (Google OAuth, GitLab, Hugging Face, Groq,
  xAI, Notion, Shopify, Stripe webhook, Slack/Discord webhook, Telegram bot),
  next to the issuer-prefixed tokens, PEM blocks and JWTs, with a
  false-positive corpus guarding prose, hashes, UUIDs and `connect(password=
  password)`-style pass-throughs. Paths, project root/name/remote/branch, the
  model and the tool name are scanned too (only the matching span is
  replaced). The spool is now
  imported through `Database::import_spool_with`, so hook-spooled events pass
  the same gate as everything else.
- **`encryption = off | required` take effect.** With `required`, or a
  database that already holds encrypted content, a missing key stores events
  without their content (and says so in the daemon log and `attempt doctor`)
  instead of writing plaintext.
- **Stored file paths no longer carry your home directory** (`~/…`), and
  uploaded paths are repo-relative or `~/…` in every profile but `full`.
- **Sync policy fails closed.** `--exclude`/`--include` entries are
  normalised (URL spellings, `.git`, `git@host:path`); an entry that names no
  repository is refused; telemetry not tied to a repository never uploads
  while a policy is set.
- **`sync connect` records consent** (`config_changed`) and keeps history from
  before the connection local unless `--include-history`; plain `http://` only
  for localhost unless `--allow-insecure-http`.
- New **`attempt sync forget`** and **`sync disconnect [--forget]`** (which
  also revokes the device key), and server routes `/v1/sync/forget`,
  `/v1/sync/revoke`, `DELETE /v1/admin/devices/{id}/events`.
- **Server hardening:** the legacy `/v1/vibemon/hook` now needs a device key;
  a device can retract or correct only its own events; pairing cannot take
  over another user's device; the key file, pairings and cursors are written
  atomically behind one lock; the rate limiter is bounded and trusts only
  `fly-client-ip` (`--client-ip-header`); an admin token shorter than 24
  characters is refused at start; an empty webhook cursor pauses delivery
  instead of replaying history.
- **MCP and UI reads are bounded and honest.** The row cap goes into the plan,
  results have a byte budget, statements have a 20 s limit and a memory pool;
  `attempt mcp` answers `ping` and honours `notifications/cancelled`. Text of
  retracted rows is NULL in MCP and UI queries. Stored text is fenced,
  stripped of invisible and bidi characters, and every MCP result opens with a
  "data, not instructions" notice. With no project argument the tools no
  longer fall back to all projects when the repository is unknown. The UI
  accepts only `localhost`, `127.0.0.1`, `[::1]` as `Host`, names its cookie
  per port, and refuses a foreign `Origin`.
- **`attempt update` trusts less:** malformed version names are rejected,
  redirects are followed only within GitHub, `attempt-hook` is checked before
  and after the swap and both binaries roll back on failure, and the health
  check is a light `attempt health` instead of `status`. Homebrew, Nix, Scoop
  and cargo installs (or `ATTEMPTDB_MANAGED_BY`) are never auto-updated.

### Data safety

- A hook or import no longer panics on a path that starts with a non-ASCII
  character (Korean, accented, emoji); the event used to be lost.
- Events from a newer hook or daemon no longer become undecodable: unknown
  event kinds, tool categories and outcome statuses read as `unknown`/`other`,
  an unknown capture mode as `metadata_only`.
- Spool records the importer cannot decode, and spool files it cannot read, go
  to `spool/quarantine/` instead of being deleted; one bad spool file no
  longer blocks `status`, `query` or the daemon's sweep.
- An event too large for one WAL record is rejected on its own instead of
  being acknowledged and later dropped with the rest of the log.
- A damaged existing blob is rewritten rather than trusted; one corrupt
  segment no longer fails every open and ingest.
- Encrypted flushes make their blobs durable in one barrier instead of two
  fsyncs per blob (2,000 content events: about 34 s to about 9–16 s on a
  loaded machine).
- Device identity is created race-free; a corrupt `device.json` is moved
  aside and reported.
- `attempt-hook` exits 0 when the provider argument is missing, stops waiting
  for a stdin that never closes, and drains oversize payloads. Unparseable
  and oversize payloads keep their session, event name and, where allowed,
  raw bytes.
- Readers survive a compaction deleting a segment mid-read (retry from a fresh
  manifest) and an undecodable segment is reported instead of silently
  dropped.

### Speed

- **`attempt status` and `doctor` read a few columns per segment:** 0.8 s and
  26 MB on a 1,000,000-event database that took 8–14 s and 2.3 GB. A
  project-scoped `query`, `events`, the MCP server and the UI read only that
  scope's rows; other projects' history and OTel telemetry are not decoded.
- Read commands release the writer lock after importing the spool, so a long
  read no longer keeps the daemon from starting.
- The MCP and UI stores take their change fingerprint before reading, keep
  three scopes warm and treat a relative `--since` as one scope.

### Capture

- **`attempt setup` imports your history.** The last 30 days (up to 512 MiB
  per agent) of Claude Code and Codex sessions, so the first `attempt ui` or
  `attempt timeline` shows your own work. Opt out with `--no-backfill`; tune
  with `--backfill-days` and `--backfill-max-mib`; `setup --dry-run` says
  what it would import.
- New **`attempt import codex`** reconstructs sessions from `~/.codex/sessions`
  rollouts (shell, file changes, MCP, web, interrupts, token counts),
  streamed: a 600 MB rollout is read in constant memory. History imports no
  longer fail while the daemon is running; the events are queued in the spool.
- **Telemetry retention is `otel-retention-v3`.** Besides bare spans and
  `codex.sse_event`, Codex's own log-database metrics (`codex.sqlite.*`) and
  Claude's hook-runner telemetry (`hook_execution_*`) are no longer stored.
  Tokens, cost, latency, tool results and decisions are kept.
- Codex `exec_command`/`exec`, Cursor `MCP:*` and `Delete`, Gemini `mcp_*` and
  Claude `AskUserQuestion` classify to their real category and shell facts
  read Codex's `cmd`; Codex `apply_patch` records lines added and removed and
  every touched file.
- Cursor captures the assistant's replies (`afterAgentResponse`; re-run
  `attempt hook install` to subscribe); an aborted or failed Cursor turn is
  `turn_failed` with an outcome; `sessionEnd` keeps its duration and status.
- An interrupted tool call is recorded as cancelled, not as a success or an
  unknown failure. A hook payload without a session id is marked
  `capture_gap = missing_session_id`.
- Hooks are installed into every Claude Code config directory
  (`CLAUDE_CONFIG_DIR`, `~/.claude`, `~/.claude-*`); `--claude-config-dir`
  overrides detection and `attempt doctor` reports each directory.
- The installer writes through a symlinked `settings.json`, keeps file mode,
  indentation and CRLF, and a user's own OTLP exporter is kept and reported
  as a note instead of failing the install.
- macOS: the daemon socket no longer depends on `$TMPDIR`, so hooks from
  sandboxed shells reach the daemon.
- `attempt uninstall` honours `ATTEMPTDB_NO_DAEMON` and only touches the
  service manager when this home has the unit.

### Inference

- **Sessions are `open`, `stale` or `closed`.** A session silent for 30
  minutes with no end event is `stale`; `sessions.confidence` follows how much
  of it was observed; heuristic edges, FIFO-paired tool calls and signals no
  longer claim confidence 1.0; every inferred table carries
  `algorithm_version` (now `tier1-v5`).
- Needs You no longer queues an agent that is idle after finishing its turn,
  and a permission wait is cleared only by the agent that raised it.
- Work units no longer fuse unrelated days of work through a changelog or
  other hot file; handoffs survive a round trip between agents and no longer
  count files both sides only read; attempts split per agent and a failing
  `grep` no longer ends one.
- Attempt ids come from evidence, so corrections and retractions stay on the
  right attempt; `attempt retract --reason privacy` also hides the prompt text
  from turns, attempts and work units.
- `SHOW … FOR path = 'glob'` matches each path on its own; AttemptQL `WHERE`
  must be exactly one SQL expression, so it can no longer reveal retracted
  rows; `today` and `yesterday` use local midnight; CSV output neutralises
  formula cells.

### Installers

- **`install.sh` asks before wiring anything.** It installs the checksummed
  binary, shows what `attempt setup` would change and asks `Apply these
  changes? [Y/n]` on the terminal; with no terminal it installs the binary
  only and prints the command; `--yes`/`ATTEMPTDB_ASSUME_YES=1` applies
  without asking. A download that is cut off runs nothing. It offers to add
  the install directory to your zsh/bash/fish profile (never without a yes;
  `ATTEMPTDB_MODIFY_PATH` / `ATTEMPTDB_NO_MODIFY_PATH`), and
  `ATTEMPTDB_VERIFY_ATTESTATION=1` verifies build provenance with `gh`
  before installing. It no longer clears (or claims to clear) a macOS
  quarantine attribute: a `curl` download carries none.
- `install.ps1` mirrors the consent, truncation and attestation behaviour,
  no longer flattens `%VARIABLES%` in the user PATH, and edits PATH only with
  consent (`-Yes`). **Unexecuted on Windows; run Windows CI before release.**
- The VibeMon installers no longer raise an existing `metadata_only`
  database; they print the capture mode in effect.
- Releases get a post-publish smoke install on macOS and Linux; installer CI
  runs `shellcheck` and the tests on Ubuntu and macOS.

### Added

- **`attempt setup`: a machine in one command.** The database, hook entries
  in every detected agent, the background daemon and a check, in that order,
  each step reported rather than fatal — a machine without a GUI session
  cannot register a launchd agent, and the report says exactly that while
  the hooks spool to disk. Idempotent: a second run changes nothing and says
  so. `--dry-run` produces the same report without writing, `--json` makes
  it a document, and what only the user can finish (trusting Codex's new
  hook entries) is listed under `needs you` instead of failing the command;
  a stale entry never is — rewriting it is what setup is for. The
  installers call it and carry no wiring logic of their own.
- **The one-line install sets the machine up.** `install.sh` and
  `install.ps1` run `attempt setup` after installing the binary; the earlier
  "now run three more commands" is gone. `ATTEMPTDB_NO_SETUP=1` keeps the
  old binary-only behaviour, and arguments after `sh -s --` reach setup.
  Four regression tests run the shell installer against a stubbed release.
- **Your coding agent can install it.** `docs/install-for-agents.md` is the
  procedure an agent follows when asked to install AttemptDB: the binary
  alone, `attempt setup --dry-run` shown to the person, `setup --json`
  applied only after they agree, and which report fields to read back. The
  README leads with the sentence to paste. A test fails if the guide names
  a report field that `setup --json` no longer has.

### Changed

- **The Agent Timeline has a new design.** `attempt ui` is dark only now:
  the icon's slate with a violet bias, hairlines where the
  subject changes instead of cards, a state as a coloured dot and a word
  instead of a bordered pill (the `✓ ✗ ↻ ▶` glyphs are gone with it), and
  every session drawn as a stem with its turns branching off — the mark on
  the icon, at the size of the data. The header carries the mark; the
  waterfall, the causal graph and the work board follow the same palette.
  Sanitized exports embed the same stylesheet.
- **`attempt doctor` reads columns, not events.** Per-agent hook activity
  (live hook events, the latest capture, whether a capture test was
  stored) and the stored OpenTelemetry receipts now come from the same
  column-derived facts `attempt status` uses, instead of decoding every
  event. On a 2.8-million-event database: 32 s instead of 271 s. The
  states it reports are unchanged.
- **`attempt --help` lists the eight commands a person needs** — setup,
  doctor, status, query, schema, ui, mcp, uninstall — and names the rest
  underneath, grouped (history, corrections, data, upkeep, step by step).
  Every command still works; `attempt help <command>` explains any of them.
- **The VibeMon installers ask `install.sh` for the binary only**
  (`ATTEMPTDB_NO_SETUP=1`, installer revision `0.2.13+install.2`). From
  this release the binary installer runs `attempt setup`, which inside the
  VibeMon installer would wire hooks and the daemon before pairing and
  create a database before its capture-mode choice. Harmless while the pin
  is 0.2.13; required before it moves. `docs/companion-boundary.md` lists
  every place a companion meets AttemptDB.
- Projection algorithm advances to `tier1-v4`: the 0.2.13 changes
  (`tier1-v3`) and the hook-architecture audit's (`tier1-v2`, which stayed
  on a local branch until now) are one projector again. Derived caches from
  either rebuild on first read; no storage-format change.

### Fixed

- **A telemetry span without a session is not kept** (`otel-retention-v1`).
  Codex exports every internal `tracing` span (`receiving`,
  `handle_responses`, `append_items`, `persist_rollout_items`, …) — none
  carries a conversation id, none is read by any projection or console,
  and one device wrote 560,000 of them in a day (2026-09-10; 920,000 of the
  1.06 million events it held were such spans). The local receiver now
  drops them (`dropped` in the OTLP receipt) and the sync server rejects
  them from older clients with the reason `telemetry span without a
  session is not retained`; span *events* (Codex's structured API
  observations), log records, metric samples and spans that carry a
  session stay exactly as before.
- **`POST /v1/admin/tenants/{tenant}/purge-telemetry`** rewrites a
  tenant's segments without the rows the rule refuses, one manifest
  generation per rewritten segment (`Database::purge`), for what was
  uploaded before the rule. A clean segment is not touched, and the
  tenant's writer is released between segments so uploads keep flowing.
- **A webhook page costs a page of memory, not the backlog.** The
  server's event scan behind the webhook worker and `GET /v1/events`
  decoded every event after the cursor into memory and kept 500 of them.
  With one tenant's cursor 600,000 rows behind (the spans above, which the
  product never mirrors), the worker's first page after boot was itself
  the OOM — the server died ~95 s after every start with no read traffic
  at all, so the cursor never moved. The scan now walks the segments in
  sequence order one batch at a time and stops when the page is full.
  `Database::purge` reads a segment one batch at a time too and writes
  its kept rows in segments of at most 16,384 rows.
- **A compaction step holds at most `max_run_rows` rows** (65,536 by
  default). A run was every consecutive small segment, and a step read
  the whole run into memory: a tenant of 115 small segments and 1.2
  million rows was one run, so the idle sweep's close — every two minutes
  — was a 3.6 GB read on a 2 GB machine. A longer run is now merged in
  pieces, oldest first.
- **A young view is served as it is** (`--view-max-age-secs` /
  `ATTEMPTDB_VIEW_MAX_AGE_SECS`, Fly: 20). Devices upload every 5 s, and a
  console read is six to eight statements each loading the tenant view, so
  nearly every statement found a new fingerprint and rebuilt: 5 s for
  nothing new, 49 s with one new segment, 90–120 s cold on the shared vCPU
  — past the web's 15 s budget every time. `/v1/status` `view_built_at`
  says how old the served view is. `ATTEMPTDB_MAX_OPEN` goes from 3 to 8
  on Fly: with 22 devices uploading, three slots evicted the tenants
  people read between two statements of one read.
- **`--view-max-events` / `ATTEMPTDB_VIEW_MAX_EVENTS`**: the server holds
  at most that many segment rows of a tenant's window resident — the newest
  segments, whole. The day window was not a bound: a resident row costs
  ~3.5 KiB and one tenant's fourteen days outgrew the 2 GB machine
  (`attemptdb-sync` OOM-looped every 10–13 minutes on 2026-09-10, every
  upload failing meanwhile). `/v1/status` reports `view_window.max_events`
  and the `since` the held history actually starts at.
### Changed

- The local OTLP receiver discards Codex `codex.sse_event` records, as log
  records and as span events. They are per-chunk stream observations that
  nothing derives sessions, attempts or work from, and they were the largest
  source of storage growth. Discarded records are acknowledged as received
  (not reported as rejected) and counted as `dropped` in the receipt; a batch
  with nothing left to store no longer wakes the writer. Sync also never
  uploads such a row from an older database. Codex completion tokens that only
  appeared on these records are no longer recorded.

### Fixed

- A long-lived daemon no longer holds its whole database in memory to compute
  inferences. The whole-history read behind the inference upload kept every
  OTel observation — which the projection ignores, and which are most of an old
  database — so a 3.8 M-event database reached a footprint above 20 GiB. They
  are now dropped while each segment batch is decoded; the inference set is
  unchanged.
- Inference uploads stay under the server's request limit. The server replaces
  a kind's document on every upload, so a kind cannot be split across requests:
  items beyond 3 MiB are dropped oldest first (counted as `truncated`) instead
  of the whole upload being refused with 413 on every tick.
- A failed inference upload is retried after a minute, not on every tick.

## [0.2.13] — 2026-09-09

- The uploader reads flushed content back with the database key. Under
  `messages` (and `full`) an event whose content the daemon's periodic
  flush had already moved into an encrypted blob was uploaded as bare
  metadata: the sync path opened the database without a key provider and
  the blob reader yielded nothing, silently. Events uploaded within a few
  seconds of capture were never affected; anything held back by an
  outage, a paused daemon or a large backlog was. Now a blob that cannot
  be read holds the upload with a clear error (restore the key, or switch
  the profile to `semantic`) instead of sending stripped events. Rows
  already uploaded without their text stay that way — the server does not
  backfill content for a duplicate event id.
- Under `messages` only the kinds that can carry something said open their
  blobs; the inference recomputation never opens one.

- Export the conversation over OTel by default: `attempt hook install`
  sets `OTEL_LOG_USER_PROMPTS=1` and `OTEL_LOG_ASSISTANT_RESPONSES=1` for
  Claude Code and `log_user_prompt = true` for Codex. The adapter stores the
  prompt of a `user_prompt` record and the reply of an `assistant_response`
  record as `content` under the capture mode; the sizes become
  `x_otel_prompt_chars` / `x_otel_response_chars`. Tool arguments and tool
  content remain off.
- New sync profile `messages`: `semantic` plus the conversation — the prompt
  of a submitted prompt and the message of a turn stop, an agent message or
  an OTel prompt/reply record, secret-redacted on the device. Commands, tool
  input, tool output, errors and raw payloads never leave. `attempt sync
  profile <name>` changes a configured peer without re-pairing; `sync.json`
  gains `send_messages`.
- The VibeMon migration installers default to `--profile messages` and
  create new databases as `local_semantic` (`--metadata-only` opts out);
  existing databases keep their mode.

## [0.2.11] — 2026-09-08

- Resolve OTel session/project identity from filtered metadata, without
  decrypting historical prompts and tool output. Large existing databases
  no longer block intake on that lookup and exhaust short-lived SDK exports.
- Keep the session/project cache scoped to both session and device, and
  select the latest matching hook across segments and unflushed events.
- No storage, protocol, exporter configuration or projection-version change.

## [0.2.10] — 2026-09-08

- Hook installation enables local Claude Code and Codex OTel logs, metrics
  and traces by default, starts/checks the authenticated loopback receiver,
  and reports actual stored observations through `attempt doctor`.
- OTel uses the existing durable database and optional sync path. Retries
  are deduplicated; metadata privacy, metric temporality and trace/span
  context are preserved. Codex's zero log timestamp and structured span
  events are supported. Telemetry does not create tasks or revive idle work.
- Exporter settings are private, backed up and reversible. Foreign
  collectors and Codex trust are preserved. Existing clients must upgrade
  and reinstall hooks; running agents must restart to load their exporters.
- Windows keeps a persistent scheduled daemon for continuous collection and
  sync, with no battery/execution cutoff and bounded named-pipe clients.
- No storage-format change. Projection algorithm advances to `tier1-v3`.

## [0.2.9] — 2026-09-06

- **Windows scheduled sync imports pending hooks.** `maintenance` and
  `sync now` previously read only the database, leaving newly captured
  events in the spool when no daemon was present. The scheduled task could
  exit successfully forever without uploading new work. Both commands now
  import pending capture before upload and release the writer before HTTP.
  A real-server CLI regression covers two batches without intervening reads.
- Installer reports use explicit UTF-8 on Windows, catch unexpected
  PowerShell exceptions, preserve a transcript when Git Bash holds the
  normal log open, redact errors, and bound the escaped JSON payload.

- Check the user service manager before VibeMon pairing or hook changes.
  Unattended legacy upgrades in temporary Linux/macOS environments skip
  migration and retain the existing collector instead of creating empty
  paired devices. Explicit installs explain the missing service requirement.
- Git Bash, MSYS and Cygwin invoke the native Windows migration installer,
  preserving credentials and options. Failed installer downloads and daemon
  registration now produce an explicit failure reason. Eight isolated
  installer regression tests cover ordering, failure recovery and handoff.
  PowerShell step output no longer hides a failing exit code. This
  installer-only fix is tagged `install-2026-09-06` and uses v0.2.8 binaries.

### Added

- **An unattended install keeps a log, and its failure report carries the
  tail.** The report said which step failed and one line of why; for a run
  with every stream on /dev/null that was all anyone would ever see.
  Unattended runs of `vibemon-install.sh` / `.ps1` now log to
  `~/.vibemon/vibemon-install.log` (next to the older client when it is
  there) or `~/.local/state/attemptdb/` (`%LOCALAPPDATA%\AttemptDB\state`
  on Windows), and the report includes the last 40 lines with keys, tokens
  and home paths blanked — the installer blanks them, the web blanks them
  again. The hourly watch quotes the telling line in Discord with the
  report's id.

## [0.2.8] — 2026-09-05

### Added

- **Installed clients update themselves.** Every release now publishes a
  policy, `update.json`, beside its assets (from `RELEASE.toml`): the newest
  version and `required_below`, the floor under which a client must update
  at once. The daemon reads it once a day and installs a required release
  immediately, an optional one within a day at a quiet moment — through the
  same `attempt update` path as before (SHA-256 verified, health-checked,
  rollback-safe), then restarts on the new binary. `auto_update` in
  `config.json` is `on` (default), `required` or `off`;
  `ATTEMPTDB_NO_AUTO_UPDATE=1` is `off` for a CI image or a container.
  `attempt doctor` says what the last check decided; `attempt update
  --check` says whether a release is required.
- **`attempt maintenance`**: upload to every peer, then apply the release
  policy — what the daemon does in the background, as one command. The
  Windows scheduled task runs it every minute instead of `sync now`, so
  Windows machines update too.
- The resolver reads the policy from the `releases/latest` redirect — a
  plain download, no API call, so a fleet behind one address never meets
  GitHub's unauthenticated rate limit. Releases without a policy (before
  0.2.8) still resolve through the API.

## [0.2.7] — 2026-09-05

### Fixed

- **A successful migration was reported as a failure.** `vibemon-install.sh`
  ended with `attempt doctor`, whose exit code grades the machine (an
  untrusted Codex hook is a 1) — under `set -e` that killed the script after
  every real step had succeeded, so the last line never printed and the
  report said `failed at done`. Found by running the 0.2.6 script unattended
  in a sandbox against production. Doctor's verdict no longer decides the
  script's.

## [0.2.6] — 2026-09-05

### Added

- **The installers report how they ended.** `vibemon-install.sh` and
  `vibemon-install.ps1` send one line to vibemon.dev when they exit — ok or
  failed, the step they stopped at, OS, versions, and the account key if it
  was used (resolved to the account on the web, never stored; never paths or
  hostnames). `--no-report` / `-NoReport` opts out. An unattended install
  that failed used to be indistinguishable from a machine that never ran it.
- **Unattended migration.** The older client's stored key
  (`~/.vibemon/api-key`) now pairs whether or not a person is at the
  terminal; whether the older client's daily poll runs the installer at all
  is the web's decision (`install.sh?v`), not the script's. The Windows
  installer gained the same stored-key path — it had none, so the app's
  argument-less update command did nothing on Windows.

### Fixed

- `--no-commit-msg` (the older client's flag, still emitted by /setup) made
  the installer exit 2. It is accepted and ignored.

## [0.2.5] — 2026-09-04

### Added

- **AttemptDB has a mark.** A session marker, the stem that runs down from
  it, and the attempts branching off — one short because it never finished.
  Windows builds carry it as their icon and version information (Explorer,
  the taskbar, Alt-Tab and the SmartScreen dialog all read those); the
  console, the local UI and its single-file export use it as their favicon.
  One master, `assets/icon/render.py`, generates every size — including a
  separate simpler drawing below 48 px, and an `.ico` whose small frames are
  DIB rather than PNG, which is what the Windows shell reliably reads.
- **Windows has a background registration.** `attempt daemon install` and
  `attempt daemon uninstall` now register and remove the `AttemptDB Sync`
  scheduled task, which uploads every minute — Windows still has no daemon,
  and this is what stands in for it. The installer calls the CLI instead of
  running `schtasks` itself.

### Fixed

- **Windows uploads could stop after the install.** The scheduled task ran a
  PowerShell one-liner (`"…attempt.exe" import; "…attempt.exe" sync now`)
  whose behaviour depended on how `-Command` stripped its quotes; a quoted
  path followed by a bare argument is a parse error in PowerShell. The task
  now runs the executable directly with its arguments — one program, nothing
  to re-parse — and one command is enough, because opening the database
  imports whatever the hooks spooled. Re-running the installer replaces the
  old task.
- **`attempt uninstall` left the background registration running** on every
  platform: the launchd agent, the systemd unit, or a scheduled task pointed
  at a binary the user may have deleted. It now unregisters it (and says so).

## [0.2.4] — 2026-09-04

### Added

- **The console is designed.** `/admin` now carries a real visual identity —
  monospace facts with tabular numbers, a token set that follows the system
  theme or an explicit light/dark choice, a tenant rail with a filter (`/`),
  hairline readouts and tables, in-page confirmations and toasts instead of
  browser dialogs, and copy controls on ids. The login page matches.

### Fixed

- **Redaction panicked on non-ASCII text — and took a live tenant down.** The
  secret scanner indexed bytes and sliced the string at every one, so any
  Korean, accented or emoji character aborted the scan with *"is not a char
  boundary"*. On the sync server that panic happened while a tenant's lock was
  held: the mutex was poisoned and every later request for that tenant
  answered `cannot load the tenant: tenant database poisoned` until the
  process restarted. It also killed `attempt snapshot export --sanitized`.
  Indices inside a character are now skipped.
- **A panicking request can no longer take a tenant off the air.** The
  registry reopens a tenant whose lock was poisoned instead of handing out the
  poisoned handle — the same recovery a restart performs, which the engine is
  built for (the WAL is the durability boundary).
- **A release deployed nothing.** `deploy.yml` triggers on `release:
  published`, but a release created with `GITHUB_TOKEN` does not fire that
  event, so 0.2.3 published its assets and the server was never updated. The
  Release workflow now dispatches the deploy explicitly.
- The console's webhook readout reported a lag against a cursor of zero on
  servers with no webhook configured; it now says the webhook is off.

## [0.2.3] — 2026-09-04

### Added

- **The console.** `attemptdb-server` serves `/admin` when an admin token
  is configured: sign in with the token once, then browse every tenant —
  keys and devices with last syncs, the live state, sessions and turns,
  work and attention, raw events by sequence, SQL — and revoke keys or
  remove devices, all through the same `/v1` API a curl would use.
  `GET /v1/admin/tenants` summarises tenants without opening a database.
- **The server ships as a release asset** (`attemptdb-server-<version>-
  <target>.tar.gz`, static Linux, attested), and `deploy/Dockerfile`
  downloads and verifies it instead of building: a deploy is seconds. The
  Release workflow's last step now deploys the app (`deploy.yml`). The
  `server` cargo profile (no LTO, parallel codegen) is what the server is
  built with; `deploy/Dockerfile.source` builds an unreleased tree.

### Changed

- An operator read (admin token + tenant header) of a tenant nothing has
  been stored for answers `404` instead of creating an empty tenant.

## [0.2.2] — 2026-09-04

### Fixed

- The VibeMon installer, typed by a person at a terminal on a machine
  with the older client and no argument (the app's "update available"
  command is exactly `curl … | bash`), now uses the older client's stored
  account key (`~/.vibemon/api-key`) to pair — before, that command changed
  nothing once `/install.sh` served this installer, so users below the
  legacy version could not follow it. Detached runs (the legacy daily
  poll: no terminal) still exit 0 having changed nothing.

## [0.2.1] — 2026-09-03

### Added

- **Outbound webhook.** `--webhook-url` / `--webhook-secret`: after each
  accepted batch the server delivers the new events to the product's
  endpoint, HMAC-SHA256 signed, from a durable per-tenant cursor (pages of
  500, retries, a 60 s sweep, catch-up after a restart). `/v1/health`
  reports the counters. The product applies its own rules to the events;
  the server knows none of them.
- Keys record `issued_at`; `/v1/devices` and the webhook expose it as the
  device's pairing time.
- The VibeMon installers accept the older install command's `vbm_…` API
  key by exchanging it for a pairing token at the web first, so the
  canonical `/install.sh` can serve them without breaking commands already
  in people's hands.

### Fixed

- `attempt daemon install` on macOS retries `launchctl bootstrap` through
  launchd's asynchronous teardown of the previous registration ("Input/
  output error" on the first upgrade of a running daemon).

## [0.2.0] — 2026-09-02

The one-line install release: a product's web page mints a one-time
pairing token, and `attempt sync connect --pair` turns it into a device key
bound to this machine's own device id, proven before it is saved.

### Added

- **Pairing** (RFC 0006 §10). Server: `POST /v1/admin/pairings` mints a
  `pair_…` token (digest only on disk, 10-minute default TTL, one use);
  `GET /v1/pair/{token}` reports valid / expired / used / unknown; `POST
  /v1/pair` exchanges the token plus the local `device_id` for a device key
  bound to that id, retiring the same device's earlier keys. Device:
  `attempt sync connect --pair <token>` (or `--key`), with an authenticated
  handshake (an empty batch under the key: `401` unknown, `403` another
  device's) before the key is saved, and the previous connection restored
  on failure.
- **The operator's read.** The admin token plus `X-AttemptDB-Tenant`
  reads any tenant, so a product backend needs no reader key per tenant.
- **Rate limiting.** A token bucket per client address on the public
  pairing routes and per bearer key elsewhere (`--rate-limit`,
  `--pair-rate-limit`; `429` with `Retry-After`).
- `attempt doctor` shows the sync peer, masked key, profile, interval and
  last sync; `--remove-legacy vibemon` recognises the Windows client's
  `notify.py` / `notify.ps1` hook entries.
- The VibeMon installers (`docs/migration/vibemon-install.{sh,ps1}`) are
  token-first and sync-success-first, pin the AttemptDB release they were
  written against, and exit 0 without changing anything when run with no
  token on a machine that was never connected.
- CI runs a RustSec audit.
- **Work conflicts** (`conflict-v0`, RFC 0003 §5.8): two open work units of
  one project editing the same file in overlapping windows, neither committed
  since. Per shared path: each side's edit size and commit state; evidence is
  the edit events. Surfaced as the `conflicts` SQL table, `/v1/work`'s
  `conflicts`, and `/v1/attention` items with `reason = "work_conflict"`.
- **Countable test signals.** Adapters read a test runner's summary line
  (cargo, nextest, jest, vitest, pytest, mocha, rspec, phpunit, dotnet,
  `go test -v`) into `attrs.tests_passed` / `tests_failed` / `tests_skipped`;
  `/v1/work` carries a work unit's newest test run and build as `signal`.
- **Server read API for a console:** `GET /v1/live` (newest event and active
  sessions, answered from facts kept next to the writer), `GET
  /v1/events/{id}`, `POST /v1/corrections` (a reader or admin key records a
  correction or retraction, attributed to its user), and `user_id` /
  `users` on sessions and work units from the tenant's device keys.
- `tool_calls` gains `lines_added` / `lines_removed`; `attempt` CLI reads
  through the daemon's resident engine (IPC `QUERY`/`RESULT`) when one
  serves the database.

### Changed

- **Sync defaults:** upload profile `semantic`, interval 5 s. Each upload
  tick reads only past its cursor and never opens content blobs unless the
  profile sends content; the inference set is recomputed only after a tick
  that uploaded something.
- **`ALGORITHM_VERSION` is `tier1-v1`.** Work-unit rule 1 (shared path) no
  longer links turns of different sessions whose active spans overlap:
  concurrent sessions on one file are two units (and a conflict), sequential
  ones remain continuity. Every other Tier 1 entity is computed as before;
  device-uploaded `tier1-v0` items are superseded by the server's `tier1-v1`
  under the merge rule.
- The read path keeps segments as Arrow only, derives per-segment facts and
  id maps from the columns, builds the SQL layer and each projection table
  on first use, resolves content only for the rows and columns a reader
  asks for, and carries a per-session index on the projection. Measured at
  200 k events: first read after a change 432 → 44–117 ms, resident memory
  1,996 → ~800 MiB, `STATE … AT` from 188 ms to run noise.

## [0.1.2] — 2026-08-31

### Fixed

- **v0.1.1's binaries reported themselves as `0.1.0`.** The tag was cut without
  bumping `[workspace.package] version`, and every path in the release workflow
  derives the version from the tag name — so the archives were *named* 0.1.1
  while the binary inside was 0.1.0, and `attempt update` went on offering
  0.1.1 to someone who had just installed it. v0.1.1 should not be used; this
  release carries the same fixes with the version it claims.
- The release workflow now refuses a tag that disagrees with the workspace
  version, before it builds anything. Nothing checked that before, because
  every step took the version from the tag and none of them from the crates.

## [0.1.1] — 2026-08-31

**Superseded by 0.1.2 — do not use.** Its binaries report `0.1.0`, which puts
`attempt update` in a loop. The fixes below shipped correctly in 0.1.2.


Two Linux-only defects in `attempt update`, both found by CI within hours of
v0.1.0 and neither reachable on macOS, which is why every manual check of the
release missed them.

### Fixed

- `attempt update` could fail on Linux with "Text file busy". Linux refuses to
  `execve` a file any process still holds open for writing; spawning forks, and
  a child forked by one thread inherits a write handle another thread is about
  to close, which keeps the freshly staged binary unexecutable for a few
  milliseconds. `update::spawn_executable` retries on `ETXTBSY` for up to two
  seconds and the health check goes through it.
- The daemon respawn had the same race with a worse outcome. On the fallback
  branch — no launchd or systemd unit, so nothing else restarts the daemon —
  the spawn result was discarded, so `attempt update` could report **success**
  while leaving the capture daemon stopped. It now retries, and a failure is
  reported in the output and in `--json` instead of being swallowed.
- Both installers refused nothing when verification was impossible: a missing
  `SHA256SUMS`, or (in `install.sh`) no `sha256sum` and no `shasum`, warned and
  installed anyway. For a script run as `curl … | sh` that turns a hard failure
  into a silent unverified install, so both now refuse.
  `ATTEMPTDB_INSECURE_SKIP_CHECKSUM=1` is the deliberate override.
- `attempt update` could report the pid of a daemon it had not restarted. The
  service manager's unit is registered per user and `restart_service` ignores
  the locator, so a restart through it always bounces the user's daemon — while
  the status query afterwards is scoped to `--data-dir`/`--db`. With a scoped
  locator those are two different processes, and one success line named the
  wrong one. No pid is reported in that case now, with a line saying why.
- Both installers advertised `cargo install attemptdb` on the path where
  release resolution had already failed — a second failing command, since the
  crates are not published. They now print the clone-and-build commands, which
  work.

### Changed

- Build provenance attestation is required rather than best-effort. It was
  `continue-on-error` because attestation is an Enterprise feature on private
  repositories; this one is public and the step succeeded for every v0.1.0
  archive, so a release that cannot attest its artifacts now fails instead of
  shipping them unattested.

## [0.1.0] — 2026-08-31

The first tagged release: the whole local pipeline — capture, storage, query,
projections, MCP, UI, sync — in one binary, plus the sync server.

### Added

- Segment compaction: contiguous runs of small segments merge into one through
  a new manifest generation, crash-safe at three injection points; `attempt
  compact [--dry-run]`. The daemon compacts after each periodic flush and the
  sync server when a tenant is flushed and closed.
- `attempt-hook`: a dedicated hook entrypoint that links only the capture
  crate — 0.8 MB against the CLI's 76 MB, which takes hook wall time from
  6.6 ms to 4.2 ms (macOS ARM64, 40 runs including process spawn).
  `attempt hook install` prefers it when it sits next to `attempt`, and
  `attempt update` installs and rolls back the pair.
- Commit linkage: a successful `git commit` tool call is tied to the sha
  `HEAD` moved to, using only the repository head the hook already records —
  no command output is read. New `commits` projection and query table,
  `SHOW COMMITS`, and `commit_shas` on attempts and work units.
- Sync peers and profiles: several servers per device, each with its own
  cursor, interval, repository policy, and profile (`metadata_only`,
  `semantic`, `full`). The daemon re-reads its configuration every tick, so
  connecting or changing a peer needs no restart.
- Sync server read API: `GET /v1/sessions`, `/v1/timeline`, `/v1/work`,
  `/v1/attention`, `/v1/state`, `/v1/events`, `/v1/devices`, `/v1/status` and
  `POST /v1/query`, served from a per-tenant engine cache shared with the
  local UI and MCP server. Documented in [`docs/server-api.md`](docs/server-api.md).
- Key scopes (`device`, `reader`, `admin`) with an optional user binding, and
  `DELETE /v1/admin/devices/{id}`, which revokes a device's keys and retracts
  its sessions from every projection while leaving the facts on disk.
- `attempt import vibemon-export`: deterministic, idempotent backfill of a
  legacy `hook_events` export.
- On-disk compatibility fixture and test suite: a database and snapshot
  written by an earlier build are read, continued, and restored by the current
  one, and an unknown format version is refused rather than misread.
- Deployment files (`deploy/`) and [`docs/deploy.md`](docs/deploy.md).

### Changed

- The command-line crate is published as `attemptdb` (the crates.io name
  `attempt` belongs to an unrelated crate). The installed binary is still
  `attempt`: `cargo install attemptdb`.

### Security

- Secret scanning (`secrets-v1`) drops attribute values containing a
  credential at ingest and redacts content before any upload.

[Unreleased]: https://github.com/nullarch/attemptdb/compare/v0.2.11...HEAD
[0.2.11]: https://github.com/nullarch/attemptdb/releases/tag/v0.2.11
[0.2.10]: https://github.com/nullarch/attemptdb/releases/tag/v0.2.10
[0.2.9]: https://github.com/nullarch/attemptdb/releases/tag/v0.2.9
[0.2.8]: https://github.com/nullarch/attemptdb/releases/tag/v0.2.8
[0.2.7]: https://github.com/nullarch/attemptdb/releases/tag/v0.2.7
[0.2.6]: https://github.com/nullarch/attemptdb/releases/tag/v0.2.6
[0.2.5]: https://github.com/nullarch/attemptdb/releases/tag/v0.2.5
[0.2.4]: https://github.com/nullarch/attemptdb/releases/tag/v0.2.4
[0.2.3]: https://github.com/nullarch/attemptdb/releases/tag/v0.2.3
[0.2.2]: https://github.com/nullarch/attemptdb/releases/tag/v0.2.2
[0.2.1]: https://github.com/nullarch/attemptdb/releases/tag/v0.2.1
[0.2.0]: https://github.com/nullarch/attemptdb/releases/tag/v0.2.0
[0.1.2]: https://github.com/nullarch/attemptdb/releases/tag/v0.1.2
[0.1.1]: https://github.com/nullarch/attemptdb/releases/tag/v0.1.1
[0.1.0]: https://github.com/nullarch/attemptdb/releases/tag/v0.1.0
