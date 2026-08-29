# RFC 0006: Privacy, Capture Modes, and Sync

| | |
|---|---|
| **Status** | Draft |
| **Authors** | AttemptDB maintainers |
| **Created** | 2026-08-28 |
| **Related** | RFC 0001 (canonical event model), RFC 0002 (storage engine), RFC 0003 (fact/inference model), RFC 0005 (cross-platform runtime), `SECURITY.md` |

## 1. Motivation and principles

AttemptDB records prompts, commands, file effects, and tool output produced by
coding agents. That data is, by construction, the most sensitive material on a
developer's machine. This RFC defines what AttemptDB stores, where, under which
mode, and what may ever leave the device.

Principles:

1. **Privacy is a storage property, not a UI setting.** The capture mode
   decides which bytes are written to disk and which bytes may be transmitted.
   A display toggle that hides content the database already synced is not
   privacy.
2. **The local database is authoritative.** Every query, projection, and
   correction works with no account and no network. Cloud sync (VibeMon) is an
   optional, explicit, per-device opt-in.
3. **Cloud sync is disabled by default** and never carries content unless the
   user or an organisation policy has selected `full_sync`.
4. **The capture mode is recorded on every event.** `Event.capture_mode`
   (`crates/attemptdb-core/src/event.rs`, field id 24) tells every later reader
   which fields may legitimately be absent. A `metadata_only` event with no
   `content` is complete, not corrupt.
5. **Metadata and content never share a field.** `Event.attrs` holds only
   allowlisted, content-free metadata. Everything that could carry a prompt, a
   command line, file contents, or tool output lives in `Event.content`
   (`EventContent { prompt, command, message, error, tool_input, tool_output,
   extra }`) or `Event.raw`. This separation is enforced in code, not by
   convention (`Event::apply_capture_mode()`).
6. **Displayed content is untrusted input.** Tool output and prompt injection
   enter the log. Rendering must escape, never execute.

Implementation status: `crates/attemptdb-core/src/privacy.rs` defines
`CaptureMode` and the two predicates `persists_content_locally()` (true for
every mode except `metadata_only`) and `syncs_content()` (true only for
`full_sync`). `Event::apply_capture_mode()` strips `content` and `raw` when
the mode forbids local persistence. `crates/attemptdb-core/src/codec.rs`
provides `content_hash()` (SHA-256, hex) for content-addressed blobs.
Everything else in this RFC — policy files, secret scanning, key management,
blobs, sync — is **planned** and is described so that the implementation has
a fixed target.

## 2. Capture modes

| Mode | Persisted locally | May be synced (when sync is enabled) | Default for |
|---|---|---|---|
| `metadata_only` | Allowlisted `attrs`; tool names and categories; timestamps; path shapes (`logical`, `repo_relative`, extension); byte/char/line counts; outcome status and class; exit codes; durations; provider/adapter/hook versions; canonical and provider ids. **Never** prompts, commands, file contents, tool output, error bodies, or the raw payload — in any file (WAL, spool, segment, blob, log). | Same metadata rows plus derived projections labelled as derived (RFC 0003). | Existing VibeMon users (compatibility mode). |
| `local_semantic` | Everything above **plus** `content` and `raw`, stored in encrypted local blobs (`blobs/`, planned). Until the blob store lands they are stored inline in the `content_json` / `raw_json` segment columns and in WAL/spool payloads (RFC 0002); those files live only under `.attemptdb/`. Used for local Tier 2 inference and local display. | Redacted metadata rows plus derived projections. **No** content, `raw`, blobs, or content hashes that could be used to test for known plaintext. | New installs. |
| `full_sync` | Everything in `local_semantic`. | Metadata rows **and** encrypted content blobs (see §10). Encrypted in transit (TLS) and at rest (per-blob AEAD, §7). | Nobody. Explicit opt-in by the user, or an organisation policy the user has accepted. |

Rules:

- **Existing VibeMon users stay `metadata_only`** until they explicitly
  consent to local content capture. Detecting an existing VibeMon hook or
  config on the machine forces the initial mode to `metadata_only`; the
  installer must not silently upgrade it.
- **Consent is a recorded event** (planned): changing the mode emits an event
  of kind `config_changed` with `attrs.consent_version = "<policy text
  version>"` and `attrs.capture_mode = "<new mode>"`. No free text is stored.
  The event is the audit trail; there is no separate consent database.
- **The active mode is always visible.** `attempt status`, `attempt doctor`,
  and the UI header display the effective mode and its source (global,
  organisation, repository). A user must never have to guess.
- **Downgrading is immediate; upgrading is prospective.** Switching to a more
  restrictive mode stops persisting content from the next event on. Existing
  content is not deleted automatically; `attempt forget` (§8) does that.
- The mode on an event is the mode in force **when the hook captured it**.
  Readers must not re-derive it from the current configuration.

## 3. Policy scoping

Three policy layers, evaluated for every event:

| Layer | Source | Status |
|---|---|---|
| Global | User config (`config.toml` in the config directory, RFC 0005), `[capture] mode = "..."` | planned |
| Organisation | Delivered through VibeMon team settings or a signed policy file placed by an administrator (`policy.signed.json`) | planned |
| Repository | `.attemptdb/policy.toml` in the repository, or a `[capture]` table in an existing project config | planned |

Precedence: **the most restrictive layer wins for content.** Ordered from
least to most restrictive: `full_sync` > `local_semantic` > `metadata_only`.
The effective mode is the minimum over all applicable layers.

Consequences:

- A repository policy can only **lower** capture (for example force
  `metadata_only` for a client repository). It can never raise capture above
  the global mode, and it can never enable sync.
- An **untrusted repository** — one the user has not explicitly trusted with
  `attempt trust <path>` — cannot change global policy, cannot enable sync,
  cannot set exclusion patterns that hide its own activity from the user, and
  cannot install hooks. Its policy file is read only to apply *further*
  restriction.
- Organisation policy can lower the ceiling for every device it applies to and
  may require `metadata_only` for repositories matching a remote pattern. An
  organisation policy that *raises* capture (for example requires
  `full_sync`) takes effect only after the user accepts it locally, which
  records the consent event of §2.
- Rationale: a cloned repository is attacker-controlled input. Nothing inside
  it may cause data to leave the machine.

Policy evaluation is part of the hook path, so the decision is made before
the payload is written anywhere.

## 4. The `attrs` allowlist and forbidden fields

`Event.attrs` is a `Map<String, Value>` of content-free metadata. Adapters may
only write the keys below, or provider-specific keys named `x_<provider>_*`
whose values pass the value-level check.

### 4.1 Allowlisted keys (v1)

| Key | Type | Meaning |
|---|---|---|
| `tool_input_bytes` | integer | Size of the tool input before any redaction |
| `tool_output_bytes` | integer | Size of the tool output |
| `prompt_chars` | integer | Length of a user prompt in characters |
| `message_chars` | integer | Length of an agent message |
| `file_count` | integer | Number of paths touched by the event |
| `line_count` | integer | Lines added/removed/total as reported |
| `exit_code` | integer | Process exit code (duplicated in `outcome.exit_code`) |
| `duration_ms` | integer | Duration (duplicated in `Event.duration_ms`) |
| `permission_mode` | string | Provider permission mode name |
| `permission_decision` | string | `allow` / `deny` / `ask` |
| `notification_type` | string | Provider notification category |
| `stop_reason` | string | Provider stop reason token |
| `compaction_trigger` | string | `auto` / `manual` |
| `task_status` | string | Provider task status token |
| `subagent_type` | string | Subagent type name |
| `model` | string | Model identifier |
| `cwd_logical` | string | Logical working directory with the home prefix elided (`~/proj`) |
| `capture_gap` | boolean | The adapter believes events were missed before this one |
| `consent_version` | string | Version of the consent text accepted (§2) |
| `coverage_grade` | string | Session coverage grade assigned by the adapter |
| `git_dirty` | boolean | Working tree had uncommitted changes |
| `path_extensions` | array of string | Lower-cased extensions of touched paths |
| `matcher` | string | Hook matcher that fired |
| `hook_event_name` | string | Provider hook event name as received |

### 4.2 Forbidden keys and values

The following must never appear as an `attrs` key, and their contents must
never appear as an `attrs` value. The rule is enforced by a schema check at
ingestion and by the canary tests of §6.

| Forbidden | Why |
|---|---|
| `prompt`, `message`, `user_input`, `text` | Prompt or message bodies |
| `tool_response`, `tool_input` bodies, `tool_output` | Tool payloads |
| `stdout`, `stderr` | Command output |
| Error bodies (`error`, `error_message`, stack traces) | Frequently contain paths, secrets, and source |
| `transcript_path` | Absolute path under the home directory that points at full content |
| `last_assistant_message`, `compact_summary`, `custom_instructions` | Content in disguise |
| Email addresses | Identity |
| Home-directory absolute paths (`/Users/<name>/...`, `C:/Users/<name>/...`, `/home/<name>/...`) | Identity; store `repo_relative`, or a logical path with the home prefix replaced by `~` |
| API keys, tokens, passwords, private keys | Secrets |

Paths are stored in `Event.paths` as `PortablePath` values
(`crates/attemptdb-core/src/paths.rs`); in `attrs` only `cwd_logical` (home
elided) and `path_extensions` are permitted.

### 4.3 Value-level check (planned)

Every string value written to `attrs` is checked at ingestion:

- length > 256 characters → dropped;
- contains `\n` or `\r` → dropped;
- matches any rule in the secret ruleset (§5) → dropped;
- matches the email pattern or a home-directory prefix → dropped;
- key not in the allowlist and not matching `^x_[a-z0-9_]+_[a-z0-9_]+$` → dropped.

Each drop increments `attrs.redactions` (integer; the one key that ingestion
itself may write). Dropping is silent to the agent and visible to the user
through `attempt doctor`, which reports adapters with a non-zero redaction
rate as a bug to file.

## 5. Secret scanning

Secret scanning runs **twice**:

1. **Before persistence**, in the hook process or daemon, on `content` and
   `raw`, before the WAL or spool frame is written.
2. **Before export or sync**, on every row and blob leaving the database
   (`attempt snapshot export`, sync upload, sanitized timeline export).

The second pass exists because rulesets improve over time and because data may
have been imported from older captures.

| Rule id | Pattern family | Example shape |
|---|---|---|
| `aws_access_key` | AWS access key id | `AKIA[0-9A-Z]{16}` |
| `aws_secret_key` | AWS secret near an `aws_secret` label | 40-char base64 |
| `github_token` | GitHub tokens | `ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`, `github_pat_` prefixes |
| `anthropic_key` | Anthropic API key | `sk-ant-` prefix |
| `openai_key` | OpenAI API key | `sk-` prefix followed by ≥ 20 key characters |
| `google_key` | Google API key | `AIza` + 35 characters |
| `slack_token` | Slack tokens | `xox[abprs]-` prefix |
| `private_key_block` | PEM private key | `-----BEGIN [A-Z ]*PRIVATE KEY-----` |
| `jwt` | JSON Web Token | three base64url segments starting with `eyJ` |
| `generic_assignment` | `password=`, `passwd=`, `secret=`, `token=`, `api_key=` followed by a value | case-insensitive, quotes optional |
| `high_entropy` | Strings ≥ 32 chars with Shannon entropy above a threshold in a secret-like context | heuristic, lowest priority |
| `url_credentials` | `scheme://user:password@host` | credentials stripped, host kept |

Rules live in a versioned ruleset, `secrets-v1`. The ruleset id is recorded
in `attrs.x_attemptdb_secrets_ruleset` on every event that was scanned so a
later pass knows what has already been applied.

Redaction replaces each match with `[REDACTED:<rule id>]` in place and records
the count in `attrs.redactions`. Redaction is **irreversible by design**: the
original bytes are never written. The per-blob content hash (§7) is computed
**after** redaction.

Scanning is best-effort. Pattern-based detection misses secrets that do not
look like secrets and produces false positives on random-looking strings.
AttemptDB documents this limit rather than claiming completeness, and the
`metadata_only` mode remains the only mode that guarantees no content is
stored.

## 6. Privacy canaries

Every provider fixture under `fixtures/<provider>/` embeds unique sentinel
strings in every content-bearing position of the payload. Tests then assert
that no sentinel appears anywhere it is not allowed.

| Canary class | Sentinel example | Placed in |
|---|---|---|
| Prompt | `CANARY_PROMPT_7f3a` | prompt / user input fields |
| Assistant message | `CANARY_MESSAGE_2c91` | last assistant message, notifications, compaction summaries |
| Command | `CANARY_CMD_5e08 --flag` | shell command, tool input |
| File content | `CANARY_FILE_b6d4` | edit/write tool input, diffs |
| Tool output | `CANARY_STDOUT_91af`, `CANARY_STDERR_0c37` | stdout, stderr, tool response |
| Error body | `CANARY_ERROR_44e2` | error strings |
| Email | `canary.7f3a@example.invalid` | any user field |
| Token | `ghp_CANARY0000000000000000000000000000` | command, env, output |
| Home path | `/Users/canary7f3a/secret-project/x.ts` and `C:/Users/canary7f3a/x.ts` | cwd, transcript path, file paths |
| Custom instructions | `CANARY_INSTRUCTIONS_ae12` | custom instructions / system prompt fields |
| Provider-specific | `CANARY_<PROVIDER>_<hex>` | every field not otherwise covered |

Assertions:

| Location | `metadata_only` | `local_semantic` | `full_sync` |
|---|---|---|---|
| WAL, spool, segment files on disk | no sentinel | content sentinels allowed only in `content_json` / `raw_json` / blobs | same as `local_semantic` |
| `attrs` (any file, any API) | no sentinel | no sentinel | no sentinel |
| Sync payload (metadata rows) | no sentinel | no sentinel | no sentinel |
| Sync payload (blobs) | not sent | not sent | encrypted; plaintext sentinel must not appear on the wire |
| Sanitized snapshot export | no sentinel | no sentinel | no sentinel |
| Logs (daemon, hook, installer) | no sentinel | no sentinel | no sentinel |
| Error messages and panics | no sentinel | no sentinel | no sentinel |
| Token, email, home-path classes | no sentinel anywhere, in any mode, after scanning (§5) | | |

Canary tests are mandatory for adapter pull requests (`CONTRIBUTING.md`). A
failing canary is a release blocker.

## 7. Key management (planned)

### 7.1 Key hierarchy

```text
OS key store (Keychain / DPAPI / Secret Service / passphrase / key file)
  └── wraps: database key  K_db  (256-bit, random, one per data directory)
        └── HKDF-SHA256(K_db, info = "attemptdb/blob/v1" || scope || content_hash)
              └── per-blob key  K_blob
                    └── AEAD(K_blob, nonce, ciphertext, tag)  → blobs/<content_hash>
```

- `scope` = `tenant_id || device_id || project_id` (16 bytes each, nil UUID
  where absent). Binding the blob key to scope means a blob reference copied
  into another project or device cannot be decrypted there.
- `content_hash` = SHA-256 of the **redacted** plaintext
  (`codec::content_hash`). Deduplication works within a scope without exposing
  plaintext hashes externally: the hash is stored only locally and in
  encrypted form when synced (§10).
- Every blob is authenticated; a failed tag is reported as corruption, never
  decrypted partially.
- Nonce policy: a random 192-bit nonce per blob (XChaCha20-Poly1305) or a
  96-bit nonce derived from a per-key counter (AES-256-GCM). The AEAD choice
  is an open question; the format reserves a one-byte cipher id in the blob
  header so both can coexist.

### 7.2 Per-OS storage of `K_db`

| OS | Store | Details |
|---|---|---|
| macOS | Keychain | Generic password item, service `dev.attemptdb.dbkey`, account = data directory id; per-user, access group limited to the `attempt` binary once signed |
| Windows | DPAPI + Credential Manager | `CryptProtectData` with `CRYPTPROTECT_UI_FORBIDDEN`, user scope; the wrapped key is stored as a generic credential `AttemptDB/<db_id>` |
| Linux desktop | Secret Service (`org.freedesktop.secrets`) | Collection `default`, attributes `application=attemptdb`, `db_id=<db_id>` |
| Linux headless | Passphrase or key file | Passphrase → Argon2id (m = 64 MiB, t = 3, p = 1, 16-byte salt stored in `ATTEMPTDB` identity file) → wrapping key; or an explicit key file path in `ATTEMPTDB_KEY_FILE`, which must have mode `0600` and be owned by the current user |

If no store is available and no key is configured, the daemon refuses to
enter `local_semantic` or `full_sync` and falls back to `metadata_only` with a
visible warning; it never writes plaintext content because a key store was
missing.

### 7.3 Portable snapshot keys

A `.atdb` snapshot (RFC 0002) is exported in one of two forms:

- **Sanitized** — metadata rows only, content and raw columns null, blobs
  omitted. Readable anywhere without a key. This is the form used for public
  demos and for sharing with people who should not see content.
- **Encrypted** — content blobs included, each re-wrapped under a **snapshot
  key** derived from a user-supplied passphrase with Argon2id. The snapshot
  key is independent of every OS key store, so the file can be opened on any
  Tier 1 OS. `K_db` is never exported.

Key loss: if the OS key store item or the snapshot passphrase is lost, content
is unrecoverable. Metadata-only queries continue to work because metadata is
never encrypted with `K_db`. `attempt doctor` reports "content locked" in this
state.

### 7.4 Re-keying and rotation

Procedure shape (planned): generate `K_db'`; for every blob, derive
`K_blob'`, decrypt with the old key, re-encrypt, write to a temporary file,
fsync, rename; write a new manifest generation referencing the new blobs;
tombstone the old blobs; replace the wrapped `K_db` in the OS store; delete
old blobs after the manifest is durable. The operation is resumable because
each blob is content-addressed and idempotent.

## 8. Secure deletion: what is and is not guaranteed

Deleting an event or blob:

1. removes it from the next manifest generation and tombstones the containing
   file (RFC 0002);
2. physically deletes the file only after the next generation is durable and
   no reader holds it;
3. for blobs, also discards the per-blob key derivation input by removing the
   reference.

What AttemptDB **cannot** guarantee:

- bytes may survive inside old segments until compaction rewrites them;
- filesystem journals, copy-on-write snapshots (APFS, Btrfs, ZFS), and SSD
  wear-levelling retain freed blocks;
- Time Machine, File History, and other backups keep copies;
- any synced copy on another device or in VibeMon must be deleted separately
  (§11);
- process memory and swap may hold plaintext transiently.

`attempt forget <selector>` (planned) rewrites every segment containing the
selected events without them, deletes the referenced blobs, re-keys the
affected scope, and prints exactly the list above so the user knows the limit.

Deletions are recorded as events of kind `config_changed` with
`attrs.deletion_reason` (an enumerated token: `user_request`, `retention`,
`policy`, `secret_found`), the count deleted, and the affected time range —
never the deleted content. The deletion record is itself retained.

## 9. Threat model

### 9.1 Assets

Prompts, agent messages, commands, file contents, tool output, repository
paths and names, model and provider usage, and the derived projections that
summarise all of it.

### 9.2 Actors and mitigations

| Actor | Attack | Mitigation |
|---|---|---|
| Malicious tool output / prompt injection | Text inside a tool result, file, or web page is captured and later rendered to the user, an agent, or a shared timeline; may contain HTML, terminal escapes, Markdown, links, or fake evidence | All displayed content is untrusted (§9.3). Inferences cite evidence ids, never trust content assertions (RFC 0003). MCP responses are data, not instructions. |
| Malicious repository | `.attemptdb/policy.toml`, hook files, or config inside a cloned repo tries to raise capture, enable sync, or run commands | Repository policy can only restrict (§3). Untrusted repositories cannot install hooks or change global state. |
| Another local user | Reads the data directory | Data directory created `0700`; key store items are per-user; content blobs are encrypted. |
| Stolen or lost laptop | Disk read offline | Content blobs are encrypted with a key held in the OS store; full-disk encryption is still recommended. Metadata is not encrypted (see non-goals). |
| Local network attacker | Connects to the daemon's HTTP or IPC endpoint | Loopback-only bind; random port; per-install bearer token (RFC 0005); Unix socket / Named Pipe with owner-only permissions. |
| Hosted service compromise (VibeMon) | Server-side data disclosure | Content is never uploaded below `full_sync`; in `full_sync` blobs are end-to-end encrypted unless hosted-decrypt is chosen (open question). Metadata rows are the only plaintext the server holds. |
| Malicious adapter or plugin | Community adapter writes content into `attrs` | Allowlist + value check (§4), canaries (§6), adapters cannot bypass ingestion validation. |

### 9.3 Untrusted display rule

Every renderer applies these rules to every string that originated in an
event:

| Output | Rule |
|---|---|
| HTML (UI, exported timeline) | Escape `<`, `>`, `&`, `"`, `'`; never insert into `innerHTML`; CSP with no inline scripts |
| Terminal (CLI) | Strip or escape ESC (`0x1B`), CSI, OSC, and C1 (`0x80`–`0x9F`) sequences and other C0 controls except `\n` and `\t`; truncate to a display width |
| Markdown (PR summaries, exports) | Escape Markdown control characters; render content in fenced blocks with a fence longer than any fence in the content |
| URLs | Only `http`, `https`, `mailto` schemes are linkable; `javascript:`, `data:`, `file:`, and unknown schemes are rendered as inert text; no auto-linking of bare text |
| Paths | Rendered as text; never opened, executed, or passed to a shell automatically; `..` and drive/UNC prefixes shown verbatim |

### 9.4 Local API protection

The daemon's HTTP API and UI bind to `127.0.0.1` / `::1` only, on a random
port recorded in the runtime directory, and require the per-install token
(RFC 0005) in an `Authorization` header or a `HttpOnly`, `SameSite=Strict`
cookie set on first open. Binding to any other address requires an explicit
flag and prints a warning on every start.

### 9.5 Non-goals

- Protection against malware running as the same OS user (it can read the key
  store and the socket).
- Protection against a compromised operating system or kernel.
- Forensic-grade deletion (§8).
- Hiding **metadata** (tool names, timestamps, path shapes) from someone with
  read access to the data directory. Metadata is not encrypted in v1 so that
  `metadata_only` databases remain readable without a key store.
- Preventing a coding agent with the same privileges from reading the
  database; the agent already has access to everything the database records.

## 10. Sync protocol (planned)

Designed for VibeMon; usable by any server that implements the same contract.

### 10.1 Identity and idempotency

- The sync key of an event is `(device_id, event_id, source_seq)`.
  `event_id` alone is unique; `device_id` and `source_seq` are included so the
  server can detect gaps and re-sent batches without inspecting payloads.
- Upload batches are idempotent. The server deduplicates by key; a re-sent
  batch, in whole or in part, is a no-op and returns the same acknowledgement.
- Ordering: per device by `source_seq` (strictly increasing, assigned by the
  single writer, RFC 0001); across devices by `hlc`, with `device_id` as the
  tie-breaker.
- Each device keeps a `sync_state` cursor: `{ last_acked_source_seq,
  last_acked_hlc, ruleset, policy_hash }`. Offline devices accumulate and
  upload later; nothing is lost because the local database is authoritative.

### 10.2 What is synced per mode

| Payload | `metadata_only` | `local_semantic` | `full_sync` |
|---|---|---|---|
| Metadata rows (segment columns minus `content_json`, `raw_json`; paths filtered by policy) | yes | yes | yes |
| Derived projections (attempts, work units, decisions), marked `derived` with algorithm version | yes | yes | yes |
| Content blobs (encrypted) | no | no | yes |
| Blob references (`content_hash`, scope) | no | no | yes, wrapped under the sync key so the server cannot test for known plaintext |
| Corrections (RFC 0003) | yes (metadata only) | yes | yes |
| Mutable preferences (mute, pin, labels) | yes | yes | yes |

Metadata rows and blobs travel in **separate streams** with separate
acknowledgements so a content-free row is never delayed by a blob upload, and
so the metadata stream can be audited independently.

### 10.3 Record sketch

```json
{
  "sync_version": 1,
  "device_id": "5d2e9a0c-3f7b-4c1d-9e8a-2b6f1c4d7e90",
  "batch_id": "0192a7c4-2b3e-7f10-8d4a-0e1f2a3b4c5d",
  "capture_mode": "local_semantic",
  "ruleset": "secrets-v1",
  "events": [
    {
      "event_id": "0192a7c4-2b3f-7a11-9c2b-1f2e3d4c5b6a",
      "source_seq": 4812,
      "hlc": 115322189145636864,
      "kind": "tool_call_finished",
      "observed_at": 1756368000123456,
      "provider": "claude_code",
      "session_id": "b1a7e6d4-2c3f-5e1a-8b9c-0d1e2f3a4b5c",
      "project_id": "9c8b7a6f-5e4d-5c3b-8a29-1f0e2d3c4b5a",
      "tool_name": "Edit",
      "tool_category": "file_edit",
      "paths_json": "[{\"repo_relative\":\"src/auth.ts\"}]",
      "outcome_status": "success",
      "duration_ms": 41,
      "attrs_json": "{\"tool_input_bytes\":812,\"path_extensions\":[\"ts\"]}"
    }
  ],
  "blobs": [
    {
      "ref": "<content_hash wrapped under sync key, base64>",
      "scope": "<tenant||device||project, base64>",
      "cipher": 1,
      "bytes": 2048,
      "sha256_ciphertext": "…"
    }
  ],
  "corrections": [],
  "preferences": [
    { "key": "work_unit/…/pinned", "value": true, "hlc": 115322189145636870 }
  ]
}
```

The `blobs` array is present only in `full_sync`. Rows carry the same column
names as the segment schema (RFC 0002) so that server-side storage uses the
same Arrow layout.

### 10.4 Corrections and preferences

- **Corrections** are first-class events (RFC 0003). They are immutable,
  totally ordered per device by `source_seq`, and merged across devices by
  HLC. Because facts never change and corrections only append, no CRDT is
  required: the server applies corrections in HLC order and the result is
  deterministic.
- **Mutable preferences** (mute, pin, labels, collapsed groups) live outside
  the fact log. They sync as last-writer-wins keyed by HLC. Losing a
  preference write on a conflict is acceptable; losing a fact is not, which is
  why they are separated.

### 10.5 Repository sync policy

A per-repository sync policy selects `include` / `exclude` by normalised
remote (`host/owner/repo`, RFC 0001) or by project id. Excluded repositories
are not uploaded at all, not even metadata. The policy is evaluated on the
device; the server never learns about excluded projects.

### 10.6 Hosted decryption

Whether VibeMon may hold a decryption key for `full_sync` content (to render
content in the hosted timeline and mobile app) is an open question. The
default design is end-to-end: the server stores ciphertext and the client
decrypts. Hosted-decrypt, if offered, must be a separate, explicit opt-in
that is displayed alongside the capture mode.

## 11. Retention and deletion visibility

| Data class | Where | Default retention | Controlled by |
|---|---|---|---|
| Local facts (events, WAL, segments) | `.attemptdb/` | unlimited | user (`attempt retention set local <duration>`) |
| Local content blobs | `.attemptdb/blobs/` | same as local facts unless set separately | user |
| Cloud metadata rows | VibeMon | plan-dependent, displayed at opt-in | user / organisation |
| Synced content blobs | VibeMon object storage | plan-dependent, displayed at opt-in | user / organisation |
| Derived projections | local and cloud | rebuilt from facts; may be discarded at any time | system |

- `attempt retention show` prints the effective retention for every class and
  where each copy of the data lives.
- Deletion propagates as a tombstone event carried by the sync protocol; the
  server acknowledges deletion with the count removed, and the client records
  that acknowledgement. Until acknowledged, `attempt retention show` reports
  the deletion as pending.
- The audit trail of deletions (§8) is retained under the local-facts
  retention and is itself synced as metadata.
- Retention expiry produces the same tombstone events as manual deletion, with
  `deletion_reason = "retention"`.

## Decisions

- Capture mode is a storage property recorded on every event; the three modes
  are `metadata_only`, `local_semantic` (default for new installs), and
  `full_sync` (explicit opt-in only).
- Existing VibeMon installations stay `metadata_only` until an explicit,
  recorded consent event.
- `attrs` is allowlisted (§4.1); forbidden fields (§4.2) are rejected at
  ingestion and guarded by canary tests.
- Policy precedence is most-restrictive-wins; repository policy can only
  restrict; untrusted repositories cannot change global policy or enable sync.
- Secret scanning runs before persistence and again before export/sync, with
  a versioned ruleset and irreversible `[REDACTED:<rule>]` replacement.
- Content is stored in encrypted, authenticated, content-addressed blobs whose
  keys are bound to scope; metadata is not encrypted in v1.
- Portable snapshots are either sanitized (metadata only) or encrypted under a
  passphrase-derived key independent of OS key stores.
- Secure-deletion limits are documented, not hidden; deletions are recorded
  as events.
- All displayed content is untrusted; local APIs are loopback-only and
  authenticated.
- Sync is idempotent by `(device_id, event_id, source_seq)`; corrections are
  ordered events; no CRDT; preferences are last-writer-wins outside the fact
  log.

## Open questions

- AEAD choice: XChaCha20-Poly1305 (random nonces, simpler) versus AES-256-GCM
  (hardware acceleration, counter nonces).
- Whether to offer hosted decryption for `full_sync` content in VibeMon, and
  how to present it so it is never confused with the capture mode.
- Delivery mechanism for organisation policy: VibeMon team settings, a signed
  policy file, or both; signature scheme and key distribution.
- Whether a machine with an existing VibeMon install should default to
  `local_semantic` after consent, or require a second explicit choice.
- Whether to publish the exact `secrets-v1` ruleset (helps auditing, helps
  evasion) or only its rule ids and test corpus.
- Whether metadata should also be encrypted at rest in a later format version,
  at the cost of requiring a key store for `metadata_only` databases.
- Exact semantics of `attempt forget` on segments shared with unrelated
  events (rewrite cost versus deletion latency).
