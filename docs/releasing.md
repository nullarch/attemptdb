# Releasing

AttemptDB ships one binary, `attempt`. A release is cut by pushing a tag; the
`Release` workflow builds every target, publishes checksums, and attaches the
archives to a GitHub Release.

```sh
# 1. Bump the workspace version.
#    Cargo.toml -> [workspace.package] version = "0.1.0"
cargo update -w                      # refresh Cargo.lock for the new version
cargo test --workspace               # must be green
cargo clippy --workspace --all-targets -- -D warnings

# 2. Tag and push.
git tag v0.1.0
git push origin v0.1.0
```

`workflow_dispatch` on the same workflow builds the whole matrix without
publishing, which is the way to exercise the pipeline before a real tag exists.

## What a release publishes

| Target | Runner | Tier |
| --- | --- | --- |
| `aarch64-apple-darwin` | `macos-15` | core |
| `x86_64-apple-darwin` | `macos-15-intel` | core |
| `x86_64-unknown-linux-gnu` | `ubuntu-22.04` | core |
| `x86_64-unknown-linux-musl` | `ubuntu-22.04` | core |
| `x86_64-pc-windows-msvc` | `windows-2022` | core |
| `aarch64-unknown-linux-gnu` | `ubuntu-22.04-arm` | optional |
| `aarch64-unknown-linux-musl` | `ubuntu-22.04-arm` | optional |
| `aarch64-pc-windows-msvc` | `windows-11-arm` | optional |

A `workflow_dispatch` dry run on 2026-08-30 built **all eight** targets,
including the three optional ones: the ARM64 Linux and ARM64 Windows runner
images are available on this account. They stay classified optional anyway,
because runner availability is a property of the plan rather than of the code,
and a release must not become un-cuttable the day that changes.

The `publish` job refuses to create a release unless all five **core** targets
built. The three **optional** targets run on ARM64 runner images that are not
available on every GitHub plan; when one is unavailable the release still goes
out and the release notes list only the targets that actually shipped. Nothing
is silently dropped — the table in the notes is generated from the artifacts on
disk, not from this document.

glibc builds run on `ubuntu-22.04` rather than the newest image on purpose: a
binary linked against a newer glibc than the user's distribution ships will not
start. The musl builds are fully static and the workflow fails the build if `file`
reports a dynamically linked binary. That check is deliberately not written
against `ldd` output: a static binary makes `ldd` print `statically linked` on
x86_64 but `not a dynamic executable` on aarch64, and the first version of the
check allow-listed only the aarch64 phrasing — which would have failed every
x86_64 musl release. The dry run caught it before any tag existed.

Every release carries a `SHA256SUMS` file covering all archives. Both
installers download it and refuse to install on a mismatch.

Runner labels are load-bearing. A job that asks for a **retired** label is not
rejected — it queues until GitHub's 24-hour limit. The first CI run on this
repository sat for 3h52m on `macos-13`, which was retired, while every other
platform had already reported. Every job therefore carries an explicit
`timeout-minutes`, and a runner-label change is a real change to review, not a
cosmetic one. `macos-14` is deprecated and `ubuntu-22.04` will enter
deprecation now that 26.04 is GA; both need a deliberate bump before they are
pulled.

## What CI costs

GitHub Actions is free on **public** repositories and billed on private ones,
and the multipliers are not close to each other: Linux 1x, Windows 2x, **macOS
10x**. The first eight runs on this repository consumed roughly 4,200 billable
minutes, of which macOS was about 87%. GitHub Free allows 2,000 minutes a
month and Pro 3,000, so the account hit its spending limit and later runs
failed before any job started — with the annotation "The job was not started
because recent account payments have failed or your spending limit needs to be
increased."

That failure looks exactly like eight simultaneous build failures. It is not:
the jobs have no steps at all. Check a job's annotations before debugging code
that never ran.

Making the repository public removes the cost entirely. Until then the levers,
in order of effect, are: drop one of the two macOS jobs, run the macOS matrix
only on tags and pull requests rather than every push to main, or raise the
spending limit. The workflows already cancel superseded runs and check
formatting on a single runner.


## Signing status

Not signed yet, and the release notes say so.

| Platform | Current | Needed |
| --- | --- | --- |
| macOS | unsigned, unnotarized | Apple Developer Program membership, then `codesign` + `notarytool` in the workflow |
| Windows | unsigned | code-signing certificate (an EV certificate avoids the SmartScreen reputation ramp) |
| All | SHA256 checksums | GitHub build provenance attestation is wired up but is an Enterprise feature on private repositories, so it is `continue-on-error` until this repo is public |

Until signing exists, Homebrew is the recommended macOS path because `brew`
does not apply the quarantine attribute. `install.sh` clears it explicitly for
the direct-download path. A user who unpacks the archive by hand on macOS will
see a Gatekeeper prompt; that is expected and documented in the release notes.

## Homebrew tap

The formula lives in `nullarch/homebrew-attemptdb` and is regenerated by the
`homebrew` job on every tag:

```sh
brew tap nullarch/attemptdb
brew install attempt
```

That job is inert until a `HOMEBREW_TAP_TOKEN` secret (a PAT with `repo` scope
on the tap repository) is added to this repository. Without it the job records
a note in the workflow summary and does nothing, so a missing token can never
fail a release.

## Not yet automated

Tracked in `TODO.md` under *Distribution*:

- signed and notarized macOS `.dmg`/`.pkg`
- signed Windows MSI and a `winget` manifest
- Linux `.deb` and `.rpm` packages
- Scoop manifest
- publishing the crates to crates.io (the workspace has ten crates; only
  `attempt` needs to be installable, but the library crates must be published
  first for `cargo install attempt` to work from the registry)

## Self-update

`attempt update` (implemented) resolves the latest release through the GitHub
API (`--to <version>` pins one), downloads `attempt-<version>-<target>.tar.gz`
(`.zip` on Windows) and `SHA256SUMS` into a staging directory next to the
binary, refuses anything without a matching digest, extracts with the
platform's `tar`, stages the new file as `attempt.new`, runs it (`--version`,
then `status --json` against the live database), swaps it in with the old
binary kept as `attempt.prev`, runs the swapped binary again, and restores
`attempt.prev` if that fails. `attempt update --rollback` restores the kept
binary at any time. A running daemon is restarted through launchd / systemd
when the service is installed, else stopped and respawned. Binaries under
Homebrew, cargo, Scoop, or Nix paths are refused with the manager's own
upgrade command. The target triple comes from `crates/attemptdb-capture/build.rs`.
`ATTEMPTDB_UPDATE_API` / `ATTEMPTDB_UPDATE_DOWNLOAD` point the command at a
different host (the e2e test serves a fake release locally).
