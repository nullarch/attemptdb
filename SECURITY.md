# Security Policy

## Reporting a vulnerability

Report vulnerabilities privately to **security@attemptdb.dev**.

> Placeholder address: this mailbox must be confirmed before public launch.
> Until it is, contact a maintainer directly through the repository.

Once the repository is public, GitHub private vulnerability reporting
("Report a vulnerability" under the Security tab) is also accepted and is the
preferred channel.

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

Planned, not yet implemented: release artifacts for every Tier 1 platform
will be signed, and checksums plus build provenance will be published with
each release. Until that exists, there are no official binaries; build from
source with `cargo build --release`.
