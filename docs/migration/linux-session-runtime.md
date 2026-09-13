# Linux session capture and optional hosted sync

The existing AttemptDB daemon can capture locally, import hook spools, and
upload configured peers without systemd. The VibeMon migration installer now
uses that runtime when `systemctl --user show-environment` is unavailable.
The client binary stays at the published 0.2.9 release; this is an installer
change, not a new storage format or hook implementation.

## Runtime selection and safety

- A working systemd user manager keeps the existing service installation.
- Linux without a user manager uses `nohup`, `setsid`, and `flock`, plus an
  available SHA-256 command. Missing tools still block before pairing.
- The session daemon must answer its own status probe before a pairing token
  is consumed or hooks are changed. A successful upload and another daemon
  probe are required before removing the old VibeMon hooks.
- A supervisor lock is scoped to the daemon endpoint. The daemon also retains
  its existing single-writer database lock. A responding daemon is reused.
- Unexpected exits restart with 1–30 second backoff. Eight consecutive short
  failures stop the supervisor to avoid an endless startup loop; a run of at
  least 60 seconds resets that budget. `attempt daemon stop` exits cleanly and
  is respected. This is process recovery, not machine recovery.
- Existing connections are reused when a legacy account key is supplied on
  reinstall. An explicitly supplied new pairing token still requests a new
  connection. No hook subprocess or network call was added.

Supervisor logs and locks are private files under
`${XDG_STATE_HOME:-$HOME/.local/state}/attemptdb/session`. The daemon keeps its
normal capture log too. Foreground mode prevents unattended binary replacement
by the daemon, since there is no registered OS service to restart that binary.

## Lifetime and recovery

The runtime survives the installer exiting and recovers from a daemon crash.
It cannot run after the host/container or its process namespace is destroyed.
It does not install a reboot entry or change shell startup files. Environment
policies that kill every descendant process can also end it.

After the environment restarts, rerun the installer **without a token** to
resume the saved connection. Preserve the home/data directory when recreating
a container; deleting the directory deletes the local identity and unsynced
spool as well. For automatic startup, run the same installer from the
environment's supported initialization command. Never copy an expired one-time
pairing token into that command.

An installation success only proves the runtime and initial upload checked by
that run. Recovery of a reported production user requires a later real agent
event received from that user's device; install self-tests alone do not prove
ongoing capture.

## Verification

`tests/installers/platform_smoke.py --linux-session` tests published,
checksummed client binaries against an isolated real server on a disposable
Linux runner/container: legacy-key installation without a user bus, two
separate automatic event uploads, SIGKILL recovery, explicit stop, connection
reuse, and automatic upload after reinstall. Test data and credentials are
synthetic; no production notifications are sent.

`tests/installers/Dockerfile.session` provides the same Linux/x86_64 fixture.
The `Install and sync smoke` workflow also keeps native Linux systemd and
Windows Task Scheduler coverage. Stub regressions cover missing runtime tools,
macOS GUI requirements, pairing order, report handling, and legacy preservation.

Process semantics references: [GNU nohup](https://www.gnu.org/software/coreutils/manual/html_node/nohup-invocation.html),
[util-linux setsid](https://man7.org/linux/man-pages/man1/setsid.1.html), and
[util-linux flock](https://man7.org/linux/man-pages/man1/flock.1.html).
