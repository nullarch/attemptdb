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
3. **Cloud sync is disabled by default** and carries only what its profile
   names (§10.8). Under `metadata_only` and `semantic` no text leaves; under
   `messages` the user's prompts and the agent's replies leave, redacted of
   secrets; under `full` all content does. The profile is a recorded
   consent (§2), not a default. (This principle once said content leaves
   only under `full_sync`; the `messages` profile of 2026-09-09 is the
   deviation, recorded here so the text matches the code.)
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

Implementation status (2026-10-06): policy files and key management are
still planned; secret scanning, consent records for sync, the sync profiles,
server-side deletion and the `attrs` allowlist are implemented, and each
section says where the code departs from the text. Original status:
`crates/attemptdb-core/src/privacy.rs` defines
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
| `local_semantic` | Everything above **plus** `content` and `raw`, stored in encrypted local blobs (`blobs/`, planned). Until the blob store lands they are stored inline in the `content_json` / `raw_json` segment columns and in WAL/spool payloads (RFC 0002); those files live only under `.attemptdb/`. Used for local Tier 2 inference and local display. | Metadata rows plus derived projections; **and**, only when a peer's profile asks (`messages`, `full`, §10.8), the content fields that profile names, secret-redacted on the device and sent inline in the event envelope (no blobs, no content hashes). A server whose ceiling is `local_semantic` stores them **in plaintext**. | New installs. |
| `full_sync` | Everything in `local_semantic`. | Metadata rows **and** encrypted content blobs (see §10). Encrypted in transit (TLS) and at rest (per-blob AEAD, §7). | Nobody. Explicit opt-in by the user, or an organisation policy the user has accepted. |

Rules:

- **Existing VibeMon users stay `metadata_only`** until they explicitly
  consent to local content capture. Detecting an existing VibeMon hook or
  config on the machine forces the initial mode to `metadata_only`; the
  installer must not silently upgrade it.
- **Consent is a recorded event.** Implemented for sync (2026-10-06):
  `attempt sync connect`, `sync profile` and `sync policy` write an event of
  kind `config_changed` with `attrs.consent_version = "sync-consent-1"`,
  `x_attemptdb_sync_peer`, `x_attemptdb_sync_profile`,
  `x_attemptdb_sync_include_count`, `x_attemptdb_sync_exclude_count`,
  `x_attemptdb_sync_history` and `x_attemptdb_sync_change` (counts, never
  repository names; no free text), and keep the same facts in `sync.json`
  (`consent`: time, profile, policy, and the history watermark, §10.8). Changing the
  *capture mode* does not emit such an event yet (planned). The event is the
  audit trail; there is no separate consent database.
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

**Status (2026-10-06):** implemented as `attemptdb-core::secrets` (ruleset
`secrets-v3`), by hand-written scanners with no regex dependency. Three
families:

- *Issuer formats*, matched near-certainly: AWS access key ids, GitHub, GitLab,
  Slack, Google (API keys and `ya29.` OAuth tokens), Stripe (keys and `whsec_`
  webhook secrets), Anthropic, OpenAI (`sk-proj-…` and the legacy `sk-` + 32+
  letters and digits), Hugging Face, Groq, xAI, Notion, Shopify, npm, Supabase
  and Vercel token prefixes, Slack, Discord and Telegram webhook and bot
  tokens, PEM private-key blocks, JWTs. The short prefixes (`hf_`, `gsk_`,
  `xai-`, `ntn_`, `shpat_`) also need a long unbroken run mixing letters and
  digits, so an identifier that starts that way is not taken for one.
- *Structural rules*, matched by where the value sits: the value of a
  secret-named assignment (`DB_PASSWORD=…`, `"token": "…"`, `password: …`,
  `--password …`, `?access_token=…`, `비밀번호: …`, `<password>…</password>`,
  `_authToken=…`, a `{"name": "DB_PASSWORD", "value": "…"}` pair; the name
  must **end** in `password`, `passwd`, `passphrase`, `secret`, `token`,
  `api_key`, `secret_key`, `access_key`, `private_key`, …, so `token_count`,
  `max_tokens` and `secret_name` do not match, and `public` names are
  skipped), credentials in a URL's userinfo (`scheme://user:pass@host`, host
  kept), `Authorization: Bearer|Basic …` (the header stays), a `Cookie:` or
  `Set-Cookie:` header, Docker's `"auth"`, kubeconfig's `client-key-data`, and
  a 40-character value after an AWS secret-key label. A structural rule fires
  only when the value is shaped like a credential — never a variable, a call,
  a type, a placeholder, `$VAR`, a number or an ordinary lowercase word, nor a
  keyword argument that passes a variable of the same name along
  (`connect(password=password)`) — so `password = hunter` in prose is not
  found. That trade is deliberate: a false positive silently damages the
  record, a miss is the documented limit of a pattern scanner.
  `high_entropy` is **not** implemented.
- *Command lines*, where a flag is a credential only for the command that gives
  it that meaning: `mysql -pSECRET`, `curl -u user:pass`, `sshpass -p`,
  `docker login -p`, `az login -p`, `openssl -pass pass:…`, `htpasswd -b`,
  `redis-cli -a`, `mongosh -p`, `skopeo --creds`, and a `.netrc` entry.

An `attrs` value containing a secret is dropped at ingestion (§4.3);
content that leaves the device under any text-bearing sync profile is
redacted to `[REDACTED:<rule>]` by `redact_event_content` (prompt, command,
message, error, tool input/output, extra, raw — JSON members are redacted by
key name as well as by text), and the event is stamped
`attrs.x_attemptdb_secrets_ruleset` (and `x_attemptdb_secrets_redacted`, the
count) so a later pass knows what ran; sanitised exports strip content
entirely. The first pass below — before persistence — is wired: the capture
ingest path (the daemon, the spool import and the history importers) calls
`redact_event_content` and `redact_event_metadata` through the content gate
(`ContentGate::apply`), behind the `redact_secrets` switch (default on). The
metadata pass scans the strings that say where an event happened (paths,
project root, name, remote and branch, model, tool name) and replaces only the
matching span, so a path stays a path.

Secret scanning runs **twice**:

1. **Before persistence**, in the daemon (or the process that imports the
   spool), on `content`, `raw` and the metadata strings above, before the
   events reach the WAL. The hook process itself never scans: it must finish
   in milliseconds and only appends to the spool.
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
| `generic_assignment` | secret-named key, `=`/`:`/flag, then a value (see the status above for what counts) | implemented, conservative |
| `authorization_header` | `Authorization: Bearer|Basic <token>` | implemented |
| `high_entropy` | Strings ≥ 32 chars with Shannon entropy above a threshold in a secret-like context | **not implemented** |
| `url_credentials` | `scheme://user:password@host` | implemented; credentials stripped, host kept |
| `cookie_header` | `Cookie:` / `Set-Cookie:` value holding a session or token | implemented; the header name stays |
| `registry_auth` | Docker `config.json` `"auth": "<base64>"` | implemented |
| `client_key_data` | kubeconfig `client-key-data` / `client-certificate-data` | implemented |
| `command_line_credential` | the password flag of `mysql`, `curl`, `sshpass`, `docker login`, `openssl`, `htpasswd`, … | implemented; only the credential goes |
| `netrc_password` | `password` in a `.netrc` entry | implemented |
| `google_oauth_token`, `gitlab_token`, `huggingface_token`, `groq_api_key`, `xai_api_key`, `notion_token`, `shopify_token`, `stripe_webhook_secret` | provider token prefixes (`ya29.`, `glpat-`, `hf_`, `gsk_`, `xai-`, `ntn_`, `shpat_`, `whsec_`) | implemented |
| `slack_webhook`, `discord_webhook`, `telegram_bot_token` | chat webhook URLs and bot tokens | implemented; the host stays |

Rules live in a versioned ruleset, `secrets-v3`. The ruleset id is recorded
in `attrs.x_attemptdb_secrets_ruleset` on every uploaded event that carried
text (not yet on every scanned event, since the first pass is not wired).

Redaction replaces each match with `[REDACTED:<rule id>]` in place and records
the count in `attrs.x_attemptdb_secrets_redacted` (`attrs.redactions` counts
attrs dropped by §4.3, a different thing). Redaction is **irreversible by design**: the
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

`attempt forget <selector>` (planned, locally) rewrites every segment containing the
selected events without them, deletes the referenced blobs, re-keys the
affected scope, and prints exactly the list above so the user knows the limit.
The server half exists (2026-10-06): `POST /v1/sync/forget` and `attempt sync
forget` delete everything one device uploaded, by the engine's purge and a
deletion record (below), and name in their response what they cannot reach
(§10.10).

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
| Hosted service compromise (VibeMon) | Server-side data disclosure | Below the `messages` profile no text is uploaded. **Under `messages` and `full` the server stores the uploaded text in plaintext** (its capture-mode ceiling is `local_semantic`; the engine's blob encryption is not used there) and forwards it in the webhook: a compromised server or product discloses it. End-to-end encryption of synced content (blobs under a sync key) was designed (§10.2) and is **not implemented**. Mitigations that exist: secrets are redacted on the device first; the device can delete its upload (`forget`) and revoke its key; below `messages` there is nothing to disclose but metadata. |
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

## 10. Sync protocol

Designed for VibeMon; usable by any server that implements the same contract.

**Status (2026-08-30).** The server side of §10.1–10.3 is implemented in
`crates/attemptdb-server` (`POST /v1/sync`, bearer keys, one database per
tenant, capture-mode ceiling, engine-enforced `attrs` contract). Two
deliberate deviations from the sketch below, both recorded here so the sketch
is not read as the contract:

- `events` are **RFC 0001 canonical envelopes** — the JSON `attempt hook`
  already spools — not flattened segment rows. Every adapter produces that
  shape and `Database::ingest` consumes it, so a client uploads what it has
  and the server stores it through the same code path as a local write. The
  segment layout remains an on-disk concern (RFC 0002).
- The server's capture mode is a **ceiling**: an event uploaded under a more
  permissive mode is clamped and its `content`/`raw` removed before the WAL,
  and the acknowledgement reports how many were stripped. With the hosted
  ceiling at `metadata_only`, §10.2's blob stream does not exist yet.

The client is implemented too (2026-08-30): `attempt sync connect <url>
--key <key>` stores the server and device key (`sync.json`, mode 0600);
`attempt sync now` and the daemon (on the configured interval) upload every
event after the per-database `sync_state` cursor, one batch in flight, in
`source_seq` order, and advance the cursor only on an acknowledgement. By
default no text leaves (the profile `semantic`: metadata and inferences);
`--profile messages`, `--send-messages` and `--send-content` are the opt-ins
(§10.8), and the server's ceiling still applies. The legacy VibeMon envelope (v2) is accepted
at `POST /v1/vibemon/hook` through `attemptdb_adapters::vibemon` so installs
that have not moved to `attempt hook` keep working by changing one URL.
Key issuance (2026-08-30): `attemptdb-server` exposes `/v1/admin/keys`
behind an admin bearer token (`--admin-token` / `ATTEMPTDB_ADMIN_TOKEN`;
absent token → the routes answer 404). `POST` mints a random `atk_…` key,
returns it once, and stores only its SHA-256 digest in the key file, which is
rewritten atomically and reloaded; `DELETE /{sha256}` revokes; `GET` lists
digests and bindings, never keys; `POST /reload` and SIGHUP re-read a
hand-edited file. A device is bound at issuance: the server mints the device
id unless the caller supplies one.

§10.4 (mutable preferences) and §10.6 (hosted decryption) remain planned.
Several peers with different profiles (2026-08-30): §10.8.

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

**Status (2026-10-06): the blob stream does not exist.** Content a profile
sends (§10.8) travels inline in the event envelope over TLS, redacted of
secrets on the device, and is stored by the server as received — in
plaintext on a `local_semantic` server. The "Content blobs (encrypted)" and
"Blob references" rows are the design target, not what runs. "Paths filtered
by policy" is implemented as: for every profile short of `full`, each path is
sent as its `repo_relative` form (or `~/…` when outside a repository) and
`project.root` as `~/…`; `Event.paths[].original` never leaves.

### 10.3 Record sketch

Implemented request and acknowledgement (v1):

```
POST /v1/sync
Authorization: Bearer <device key>
Content-Type: application/json

{ "sync_version": 1, "device_id": "<uuid>", "batch_id": "<client-chosen>",
  "capture_mode": "local_semantic", "events": [ <RFC 0001 envelope>, … ] }

200 { "sync_version": 1, "batch_id": "…", "accepted": 3, "duplicates": 0,
      "rejected": [ { "event_id": "…", "reason": "…" } ],
      "redactions": 0, "stripped_content": 3 }
401 unknown key · 403 batch device ≠ key's device · 400 wrong sync_version ·
413 body or batch too large · 503 storage failed — keep the batch, retry
```

`event_id` is minted by the client (UUIDv7 at hook time), so a re-sent batch
is acknowledged as `duplicates` and stored once. A client's own `source_seq`
is preserved as `attrs.device_seq`; the server assigns its database's
`source_seq` and `hlc` at ingest. Batches from one device are sent one at a
time, in spool order, which is what keeps per-device order without a
server-side reorder buffer.

Original sketch, kept for the fields the client cursor and blob stream will
need:

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

**Status (2026-08-30):** implemented. `sync.json` carries `include` and
`exclude` lists of normalised remotes or project ids; `attempt sync policy`
edits them and `attempt sync connect --exclude/--include` seeds them. The
uploader evaluates the policy on the device; the cursor still advances over
excluded events so they are never re-examined, and the server sees nothing
of them.

A per-repository sync policy selects `include` / `exclude` by normalised
remote (`host/owner/repo`, RFC 0001) or by project id. Excluded repositories
are not uploaded at all, not even metadata. The policy is evaluated on the
device; the server never learns about excluded projects.

**Fails closed (2026-10-06).** One function reads an entry — when it is
stored and when it is matched against an event's remote — so
`https://GitHub.com/Acme/Private.git`, `git@github.com:acme/private` and
`github.com/acme/private/` are one entry, and `prj_<uuid>` and the bare uuid
are one. An entry that is neither a project id nor `host/owner/repo` is refused
by `sync connect` / `sync policy` (an `exclude` that matches nothing would
promise what it does not do), and found in a hand-edited `sync.json` it stops
every upload with an error. An OpenTelemetry record that cannot be tied to a
repository (`x_otel_project_attributed` is not `true`: the receiver stores the
placeholder project `otel/unattributed` until a hook names the session's real
one) **never uploads while any include or exclude is configured**, because a
prompt or reply of an excluded repository can arrive in exactly that state.
Without a policy it uploads as before. A web URL that continues past
`owner/repo` (`…/tree/main`) is read as a longer remote and matches nothing;
use the clone URL.

### 10.6 Hosted decryption

Whether VibeMon may hold a decryption key for `full_sync` content (to render
content in the hosted timeline and mobile app) is an open question. The
default design is end-to-end: the server stores ciphertext and the client
decrypts. Hosted-decrypt, if offered, must be a separate, explicit opt-in
that is displayed alongside the capture mode.

### 10.7 Inference sync (implemented 2026-08-30)

Facts sync by default; inferences sync only on request, and only with their
provenance. `attempt sync connect --send-inferences` (default off) makes the
uploader compute the device's Tier-1 projection over the policy-allowed
events after each fact upload and send four tables — `attempt`, `handoff`,
`work_unit`, `decision` — as `attemptdb.inference/v1` documents
(`spec/inference-v1.schema.json`), one `POST /v1/sync/inferences` per kind.

Rules, in the order they are applied on the device:

1. an item without evidence ids, or of a kind outside the four, never leaves;
2. under `send_content == false` the content-bearing fields (`objective`,
   `rationale`) are removed; with content on, secrets are redacted (§5);
3. the set is sorted and digested; an unchanged set is not re-sent;
4. at most 20,000 items per kind are sent (newest kept) and the count of
   dropped items is reported, never hidden.

The server validates the same provenance rules per item, applies its
capture-mode ceiling to the content fields, and writes one document per
`(device, kind)` under `<tenant>/inferences/<device_id>/<kind>.json`,
replaced wholesale on each upload. Inferences are never ingested as events:
a tenant whose device uploads only inferences has no event database at all.
`GET /v1/inferences[?kind=…]` returns a device's stored documents to its own
key. Sessions, turns, tool calls, and causal edges are not synced; they are
one-to-one with facts or derivable from them by anyone holding the events.

### 10.8 Peers and profiles (implemented 2026-08-30)

One device may upload to several servers — a team's VibeMon and a private
`attemptdb-server`, say — each with its own profile, interval, repository
policy, and cursor. `sync.json` holds the peer set:

```json
{ "peers": {
    "default": { "url": "https://sync.vibemon.dev", "key": "atk_…",
                 "send_content": false, "send_inferences": true,
                 "batch_events": 1000, "interval_secs": 30,
                 "include": [], "exclude": ["github.com/acme/private"] },
    "lab":     { "url": "http://127.0.0.1:8797", "key": "atk_…",
                 "send_content": true, "send_inferences": true, "interval_secs": 5 } } }
```

A file in the earlier single-server shape (top-level `url`) is read as peer
`default` and rewritten with `peers` on the next save. Peer names match
`[A-Za-z0-9._-]{1,32}`; `attempt sync connect` sets `default`, `attempt sync
add <name> <url>` adds another, `list` / `remove` / `disconnect [<name>]`
manage them (`disconnect` without a name only removes `default` when it is
the only peer — it never drops several peers silently). `now [--peer <name>]`
uploads to every peer, one after another, and reports each on its own line;
one peer's failure never stops the others. `policy --peer <name>` edits one
peer's repository lists.

**Cursor per peer.** `<data_dir>/sync/<hash of db dir>.<peer>.json`
(RFC 0006 §10.1 `sync_state`). The single-server file `<hash>.json` is
peer `default`'s cursor: it is read when `<hash>.default.json` is absent and
left in place; writes go to the per-peer name. Cursors, inference digests,
and error records never mix between peers, so a peer that was unreachable
catches up from its own position while the others move on.

**Profiles** name the two stored flags; the flags stay the truth on disk so
older files keep working, and `--send-content` / `--send-inferences` still
apply on top of a profile (they only ever add):

| `--profile` | `send_content` | `send_inferences` | `send_messages` | What leaves the device |
|---|---|---|---|---|
| `metadata_only` | false | false | false | metadata rows only |
| `semantic` (the default of `sync connect`) | false | true | false | metadata + inferences with provenance (`objective`/`rationale` removed) |
| `messages` (the VibeMon installer's default, 2026-09-09) | false | true | true | `semantic` + the conversation: `content.prompt` of a submitted prompt and `content.message` of a turn stop, an agent message or an OTel `user_prompt` / `assistant_response` record, secret-redacted on the device. Commands, tool input, tool output, errors and `raw` never leave. |
| `full` | true | true | true | metadata + inferences + content (secret-redacted on the device; server ceiling still applies) |

`send_content = true` reports `full` whatever the other flags say, and
`send_messages = true` without `send_content` reports `messages`: the stronger
signal names the peer, and a reader must never see `metadata_only` or
`semantic` on a peer that receives any text. `attempt sync profile <name>`
changes a configured peer's profile without re-pairing it. The profile is
shown by `connect`, `status`, and `status --json` (`"profile"`).

Whatever the profile, every text that leaves is redacted with the ruleset of
§5 and every path is reduced as in §10.2 (short of `full`). `attempt sync
connect` prints what the profile sends, and, for `messages` and `full`, that
the server stores the text as received.

Under `messages` the uploader keeps only the two conversation fields of a
message event (`prompt_submitted`, `turn_stopped`, `agent_message`, and the
OTel prompt/reply records) and strips every other event to metadata before
serialisation, exactly as `metadata_only` does; the batch is sent as
`local_semantic` so a server whose ceiling allows content persists the text.
Under a `metadata_only` server ceiling the text is dropped on arrival and the
acknowledgement counts it in `stripped_content`.

**Daemon reload.** The daemon re-reads `sync.json` on every tick (at most
the smallest configured interval apart; every 10 s while no peer is
configured, so a daemon started before `attempt sync connect` picks the
peer up on its own). Peers added, removed, or changed take effect without a
restart and are logged once; an unreadable file pauses uploads and is logged
once until it is readable again. Each peer uploads when its own interval has
elapsed since its last attempt.

**Alias.** `attempt sync connect vibemon` and `attempt sync add <name>
vibemon` resolve to `https://sync.vibemon.dev` (`VIBEMON_SYNC_URL`), or to
the environment variable `VIBEMON_SYNC_URL` when it is set and non-empty
(validated like any other URL). The resolved URL is printed; nothing else
about the alias differs from a spelled-out URL.

**Consent and history (2026-10-06).** `connect` records the consent in
`sync.json` — `consent: { at, profile, include, exclude, history_before }` —
and logs it as a `config_changed` event (§2). `history_before` is the
moment of connection: events observed **before** it are never uploaded, so a
database that already holds months of work does not ship them the moment a
key is pasted. `--include-history` clears the watermark (and, on an existing
peer, resets the cursor so the history is sent; the server deduplicates).
Re-connecting the same server keeps the watermark; a peer pointed at a new
server starts one. A `sync.json` written before this field existed uploads as
it always did. `sync profile` and `sync policy` refresh the marker and log the
change but never move the watermark. Events withheld by it are counted in
`sync status`.

**Transport.** A URL must be `https://`, or `http://` for this machine
(`localhost`, `127.0.0.0/8`, `::1`). Plain http to another host needs
`--allow-insecure-http` at `connect` (stored in the peer, announced with a
warning) — the key and everything uploaded would cross the network in the
clear — and the uploader refuses a hand-edited `sync.json` that names such a
host without it. `VIBEMON_SYNC_URL` is validated the same way. A URL carrying
credentials is refused.

**When the server says no.** A `413` or `422` (and a `400` that is not about
`sync_version`) for a batch means some event in it is at fault: the batch is
halved until the event is alone; the text of an event the server refuses on
its own is withheld and its metadata sent; failing that it is skipped. Either
way a quarantine record (event id, sequence, status, the server's reason —
never content) goes into the cursor file and `sync status`, the counters rise,
and the cursor moves on, so one oversized prompt cannot stop a device's sync
forever. After 25 skips in a row with nothing accepted between them the
uploader stops skipping and reports that the server is refusing this client.
`401`, `403`, `429`, `5xx`, transport errors and a `sync_version` mismatch are
never blamed on events. The daemon retries a failing peer with exponential
backoff and jitter, 5 s doubling to 15 min, reset by a success, and does not
re-read the backlog on every tick. The cursor and `sync.json` are written
through unique, flushed temp files.

### 10.9 Read side (implemented 2026-08-30)

The server serves a tenant's work graph back to the product from the
tenant's own database: `GET /v1/sessions`, `/v1/timeline`, `/v1/work`,
`/v1/attention`, `/v1/state`, `/v1/events`, `/v1/status` and
`POST /v1/query`, for keys of scope `reader` or `admin` (a device key gets
403). The contract — parameters, response shapes, status codes — is
[`docs/server-api.md`](../server-api.md); this section records what the
design guarantees.

- **Same projection as the device.** Reads are computed by the same
  `attemptdb-project` / `attemptdb-query` code the local UI runs, over a
  per-tenant engine cache (`attemptdb_query::EngineCache`, shared with the
  UI and the MCP server): a refresh after ingest decodes only newly listed
  segments and re-projects only the sessions new events touched. Ids,
  counts and confidences are the ones `attempt ui` shows for the same
  events.
- **Inferences stay inferences.** Every attempt, handoff, work unit,
  decision, blocked explanation and session state carries `evidence`,
  `confidence`, `algorithm_version` and `computed_by` (`server` or
  `device`). Events are returned as stored (`/v1/events`, in `source_seq`
  order, for a consumer that streams).
- **Merge rule for device uploads (§10.7).** For the same `(kind, id)` the
  device's item is returned only when its `algorithm_version` is the same
  as or newer than the server's, compared within one version family
  (`tier1-v<n>`); anything else, including a version that does not parse,
  yields the server's item. The two are never mixed field by field.
- **Read-only at the engine layer.** `/v1/query` runs through the same
  DataFusion options that refuse DDL, DML, `SET` and `COPY` in every
  entry point; rows are capped and the statement has a wall-clock budget.
- **Tenancy is the directory.** A reader key resolves to one tenant
  directory; the cache lives in the tenant's registry slot and is evicted
  with it.

### 10.10 Key scopes and device removal (implemented 2026-08-30)

Every bearer key carries a **scope**: `device` (an installer's key: may
upload one device's events and inferences, and read back that device's own
inference documents), `reader` (the product's backend: may read the tenant
through §10.9, never write), or `admin` (a reader that may also manage the
tenant). Key files written before scopes existed keep reading as device
keys. A key may be bound to an opaque `user_id` the product supplies; it is
echoed in listings and carried on the principal for attribution, never
interpreted by the server. The upload routes refuse a non-device key with
403 and the read routes refuse a device key the same way; the admin *token*
(`ServerConfig::admin_token`) remains a separate, single operator credential
for `/v1/admin/*` and is not a key.

`DELETE /v1/admin/devices/{device_id}[?tenant=…]` is how a device leaves:
its device keys are revoked (the next upload gets 401), then in each tenant
concerned one Retraction event (RFC 0003 §8, reason `revoked`) is written
per session the device produced. The facts stay in the tenant's segments;
every projection — and the read side — behaves as if those sessions never
happened. A repeat call reports sessions already retracted instead of
retracting them again; `?tenant=` also lets an operator retract a device
whose keys were already revoked. This hides; it does not delete.

**Deleting (2026-10-06).** `POST /v1/sync/forget` (the device's own key) and
`DELETE /v1/admin/devices/{device_id}/events` (the operator) delete every event
the device uploaded: the engine's purge rewrites each segment that holds a row
of the device without it, tombstones the old file, and a deletion record — a
`config_changed` event from the server's writer with the count and the reason
(`device` or `operator`), never content — whose flush removes the last old
file. The device's stored inference documents go with it, and the tenant's
live facts are rebuilt. The response lists what is not reached: copies the
product already received through the webhook, backups and snapshots of the
volume, the operator's logs. `POST /v1/sync/revoke` revokes the presenting
key. On the device, `attempt sync forget [--peer] --yes`, `attempt sync
disconnect --forget` (delete, then revoke, then forget the peer) and plain
`disconnect` (best-effort revoke, then a plain statement of what stays on the
server) call them. Not implemented: a retention schedule (`attempt
retention`), and a local `attempt forget`.

**Same-tenant limits (2026-10-06).** A device key's `Retraction` and
`Correction` events must target the device's own sessions, events, attempts
and turns (rejected otherwise, §server-api): the projector honours a
retraction from any device, so without the check one member could hide
another's work. A pairing token minted for a user will not bind a device that
already holds a key in the tenant for another user, nor the server's own
writer id, the nil id or a reader/admin key's device. Every upload route
(`UPLOAD_ROUTES`, including the legacy `/v1/vibemon/hook`) refuses
non-device keys.

`GET /v1/devices` (reader scope) lists every device the tenant knows with
its key bindings, `connected` (a device key still exists), event and
session counts, and `last_sync_at` — the server receipt time of the
device's newest event. It is facts only; the product's "Connected · last
sync N s ago" row reads from it.

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
  the deletion as pending. **Implemented differently (2026-10-06):** deletion
  of what a device uploaded is a request/response (`POST /v1/sync/forget`,
  §10.10) whose answer carries the count, recorded as `last_forget_at` in the
  cursor file and in the server's deletion record; there is no tombstone
  stream and no `attempt retention`.
- The audit trail of deletions (§8) is retained under the local-facts
  retention and is itself synced as metadata.
- Retention expiry produces the same tombstone events as manual deletion, with
  `deletion_reason = "retention"`.

## Decisions

- Capture mode is a storage property recorded on every event; the three modes
  are `metadata_only`, `local_semantic` (default for new installs), and
  `full_sync` (explicit opt-in only).
- Existing VibeMon installations stay `metadata_only` until an explicit,
  recorded consent event. (As of 2026-10-06 the VibeMon installer raises an
  existing `metadata_only` database to `local_semantic` so the conversation can
  be kept and uploaded under `messages`; the sync consent it records is the
  `config_changed` event of §2, but the capture-mode change itself is not
  logged. A known deviation, not a promise.)
- `attrs` is allowlisted (§4.1); forbidden fields (§4.2) are rejected at
  ingestion and guarded by canary tests.
- Policy precedence is most-restrictive-wins; repository policy can only
  restrict; untrusted repositories cannot change global policy or enable sync.
- Secret scanning runs before persistence and again before export/sync, with
  a versioned ruleset and irreversible `[REDACTED:<rule>]` replacement. (The
  second pass runs; the first is specified and its function exists, but the
  capture ingest path does not call it yet.)
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
