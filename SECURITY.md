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

AttemptDB is pre-release. There are no tagged releases yet.

| Version | Supported |
| --- | --- |
| `main` | Yes |
| anything else | No |

Once releases exist, this table will list which release lines receive fixes.

## Protection goals

AttemptDB is a local-first database for coding-agent work history. The
following are commitments that a reported violation will be treated as a
security defect:

- **Local by default.** Contents of a local database never leave the device
  unless the user or an organisation policy explicitly enables the
  `full_sync` capture mode. `metadata_only` and `local_semantic` never sync
  content-bearing fields.
- **No content upload by default.** Prompts, source code, command lines, file
  contents, and tool output are not uploaded by default. New installs default
  to `local_semantic`, which keeps such content on the device only.
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
  or backups. Deleting a record removes it from the manifest and from future
  segments; physical erasure of prior bytes is not guaranteed.
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
