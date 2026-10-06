# Security Policy

## Reporting a vulnerability

Use GitHub private vulnerability reporting — **Security -> Report a
vulnerability** on <https://github.com/nullarch/attemptdb>. That is the
preferred channel and needs no email address on either side.

> A dedicated security mailbox is not published yet. Until one is, GitHub
> private reporting is the only supported private channel; if it is
> unavailable to you, open a public issue that says only that you have a
> security report and asks for a contact, with no details.

Do not open public issues, pull requests, or discussions for security
problems. Do not include real prompts, tool output, transcripts, or private
paths in a report; a synthetic reproduction is enough.

## Supported versions

AttemptDB is pre-1.0. Tagged releases are published on GitHub (the current
line is 0.2.x); a fix lands on `main` and ships in the next release, and
installed clients update themselves from the release policy
(`RELEASE.toml`).

| Version | Supported |
| --- | --- |
| `main` | Yes |
| the latest 0.2.x release | Yes |
| anything older | No: update |

## Protection goals

AttemptDB is a local-first database for coding-agent work history. The
following are commitments that a reported violation will be treated as a
security defect:

- **Local by default; sync is opt-in, per device, and says what leaves.**
  Nothing is uploaded until `attempt sync connect` (or the VibeMon installer)
  records a consent — a `config_changed` event and a marker in `sync.json`
  with the profile, the repository policy and a timestamp — and history from
  before that moment stays on the device unless `--include-history` asks for
  it. What leaves is the **profile**:
  - `metadata_only`: metadata only, no text, no inferences;
  - `semantic` (the default of a plain `sync connect`): metadata plus
    inferences, with `objective`/`rationale` text removed;
  - `messages` (the VibeMon installer's default): `semantic` plus the user's
    prompts and the agent's replies, secret-redacted on the device;
    commands, tool input and tool output stay local;
  - `full`: everything, secret-redacted.
  Under every profile short of `full`, paths leave as repository-relative or
  `~/…`, never with a home-directory name.
- **No content upload by default.** A plain `attempt sync connect` uploads no
  prompt, source, command line, file content or tool output (profile
  `semantic`). New installs capture in `local_semantic`, which keeps such
  content on the device only until a peer's profile (`messages`, `full`) says
  otherwise — see above for exactly what each sends.
- **The hosted server stores what a `messages` or `full` device sends in
  plaintext.** A server's capture mode is a ceiling; the hosted deployment's
  is `local_semantic`, so the conversation text of a `messages` device is
  kept as received on the server's volume, readable by the tenant's reader and
  admin keys and the operator's console, and **forwarded in the outbound
  webhook** to the product. It is not end-to-end encrypted. A device can
  delete what it uploaded (`attempt sync forget`, or `attempt sync
  disconnect --forget`) and revoke its key; that does not reach copies the
  product already received, nor backups of the volume. A retraction hides a
  session from projections but does not delete it. The server's
  `/v1/sync/forget` rewrites its segments without the device's rows and
  removes the old files; see `docs/server-api.md`.
- **`exclude` fails closed.** Policy entries are normalised (URL spellings,
  `.git`, case) when stored and when matched; an entry that names no
  repository is refused at `sync connect`/`sync policy` and stops uploads if
  found in `sync.json`; telemetry that cannot be tied to a repository does not
  upload while any policy is set.
- **Loopback-only, authenticated local APIs.** The daemon's HTTP and IPC
  endpoints bind to loopback (or a Unix socket / Named Pipe) and require
  authentication from local clients. Binding to a non-loopback address
  requires an explicit option and a warning.
- **Displayed content is untrusted.** Event content originates from tools,
  agents, and prompts and may contain prompt injection or hostile bytes.
  Every output surface (HTML, terminal, Markdown, URLs, paths) escapes it.
- **The installer never destroys existing configuration.** Hook installation
  detects agents before creating directories, edits JSON/TOML structurally,
  and locks, backs up, and atomically replaces configuration files.

## Known gaps

Stated so nobody has to find them:

- **Masking is a pattern scan, applied before content is stored.** Every way
  an event reaches the database (the daemon, the spool import, the history
  importers) masks secrets in its content (prompt, command, message, tool
  input and output, raw payload) and in the strings that say where it
  happened (paths, project root and name, remote, branch, model), unless
  `redact_secrets` is turned off in the config. A match becomes
  `[REDACTED:<rule>]`; the rest of the text, and a path around a token, stay.
  The ruleset is `secrets-v3` (`attemptdb-core::secrets`).
- **Secret detection is best-effort.** It finds credentials that identify
  themselves (AWS, GitHub, GitLab, Slack, Stripe, Anthropic, OpenAI, Google
  OAuth, Hugging Face, Groq, xAI, Notion, Shopify, Telegram, JWT, PEM, chat
  webhook URLs) and credentials by where they sit: `password=…`,
  `"token": "…"`, `--password …`, `비밀번호: …`, `<password>…</password>`,
  `{"name": "DB_PASSWORD", "value": "…"}` pairs, URL credentials,
  `Authorization: Bearer …`, `Cookie:` headers, `_authToken=…` in `.npmrc`,
  Docker's `"auth"`, kubeconfig's `client-key-data`, `.netrc` entries, and
  the flags of the commands that take a password (`mysql -pSECRET`,
  `curl -u user:pass`, `sshpass -p`, `docker login -p`, `openssl -pass
  pass:…`, `htpasswd -b`). Each rule deliberately skips anything that looks
  like a variable, a type, a placeholder or an ordinary word, so
  `password = hunter` in prose is not found, nor a password in a sentence
  (`the password is hunter2`), nor one in a format no rule knows. The scan
  never edits what it does not match, and it is no substitute for
  `metadata_only` capture, the one mode that stores no content.
- **The outbound webhook's signature has no timestamp**, so a captured
  delivery can be replayed.
- **Retraction is not deletion.** Only `forget` deletes, and only on the
  server's current files.

## Non-goals

The following are outside the threat model. Reports about them are welcome as
documentation improvements but are not treated as vulnerabilities:

- Protection against a compromised OS user account. The database, keys, and
  daemon run as the user; an attacker with that user's privileges can read
  what the user can read.
- Protection against a malicious coding agent running with the same
  privileges as the user. AttemptDB records what agents do; it does not
  sandbox them.
- Secure deletion guarantees on SSDs, journaled or copy-on-write filesystems,
  or backups. Deleting a record (locally, or on a server through `forget`)
  rewrites the segments that held it and removes the old files; physical
  erasure of prior bytes is not guaranteed.
- Manager surveillance or covert monitoring. AttemptDB is not designed to
  observe people without their knowledge, and features that would require it
  are out of scope.

The full threat model, including prompt-injection handling, key management
per operating system, and the sync protocol, is in
[`docs/rfcs/0006-privacy-and-sync.md`](docs/rfcs/0006-privacy-and-sync.md).

## Disclosure process

| Step | Target |
| --- | --- |
| Acknowledgement of the report | within 3 business days |
| Triage and severity assessment | within 7 business days |
| Fix and release | target 90 days from triage, sooner for actively exploited issues |
| Public disclosure | coordinated with the reporter after a fix is available |

Reporters are credited in the release notes unless they ask not to be. We
ask that reporters keep details private until the coordinated disclosure
date.

## Release signing

Release archives are **not code-signed yet**. What exists today:

- Every release publishes a `SHA256SUMS` file covering all archives, and both
  `install.sh` and `install.ps1` verify against it before installing.
  Verification is **mandatory**: if `SHA256SUMS` cannot be fetched, or no
  sha256 tool is available, the installers refuse to install rather than
  proceeding unverified. `ATTEMPTDB_INSECURE_SKIP_CHECKSUM=1` overrides that
  and says so loudly; there is no reason to use it against a real release.
- GitHub build provenance is attested for every release archive, and the step
  is required — a release that cannot attest its artifacts fails instead of
  shipping them. Verify a download yourself:

  ```sh
  gh attestation verify --repo nullarch/attemptdb \
    attempt-0.1.0-<target>.tar.gz --format json
  ```

  Use `--format json`. On success in a non-interactive shell the command
  prints nothing and exits 0, which is indistinguishable from a no-op if you
  are reading output rather than the exit status; the JSON form gives you the
  predicate type, the subject digest and the workflow that built it. A
  tampered archive fails with a 404 on its digest, because the attestation is
  bound to the bytes.

Not yet in place: Apple notarization for macOS and an Authenticode signature
for Windows. Until they are, a manually unpacked macOS archive triggers a
Gatekeeper prompt. `docs/releasing.md` tracks the exact status per platform.
Building from source with `cargo build --release` avoids the question
entirely.
