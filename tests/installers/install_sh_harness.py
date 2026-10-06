"""A stubbed release and a fake machine for running install.sh for real.

The script's download, checksum and install steps run unmodified against:

  * a `curl` (or `wget`) stub that serves a release archive built on the fly;
  * a fake `attempt` inside that archive that records how it was called;
  * a temporary HOME, an isolated TMPDIR, and a PATH made only of symlinks to
    the handful of tools the script uses, so the host's `gh`, `xattr`, or shell
    profile can never change what a test sees.

Nothing here touches the network, the real HOME, or the real PATH.

Runs come in two kinds. `Env.run` has no controlling terminal (a new session,
which is how an agent or CI job runs it); `Env.run_tty` gives the script a real
pseudo-terminal as its controlling terminal while stdin is still the pipe the
script text arrives on, which is what `curl | sh` looks like to a person.
"""
import errno
import fcntl
import hashlib
import io
import os
import pty
import select
import shutil
import subprocess
import tarfile
import tempfile
import termios
import threading
import time
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[2] / "install.sh"
VERSION = "9.9.9"

# Where the coding agents keep their configuration when the person moved it.
# A test that wires agents must never see the real ones: the variables are
# removed from every child environment (and tests assert it, see
# AgentConfigIsolationTests), not merely overridden.
AGENT_CONFIG_VARS = ("CLAUDE_CONFIG_DIR", "CODEX_HOME", "CURSOR_CONFIG_DIR", "GEMINI_CONFIG_DIR")


def scrubbed_environ(base=None):
    """A copy of the environment (os.environ by default) without the agent config variables."""
    env = dict(os.environ if base is None else base)
    for name in AGENT_CONFIG_VARS:
        env.pop(name, None)
    return env


# Shells the script must work under: dash on Debian/Ubuntu, bash on macOS (its
# /bin/sh is bash in POSIX mode), plus bash proper. Absent ones are skipped.
SHELLS = [s for s in ("/bin/sh", "/bin/dash", "/bin/bash") if os.path.exists(s)]

# Every external command install.sh may run (uname is stubbed so the OS can be
# faked). Linked from the host into the test PATH; a missing one is skipped
# (sha256sum on macOS, shasum on slim Linux images).
TOOLS = ("mkdir", "tar", "gzip", "mktemp", "sed", "head", "tail", "awk",
         "grep", "cat", "cp", "chmod", "mv", "rm", "sha256sum", "shasum", "tr",
         "wc", "ls", "rmdir", "basename", "dirname")

FAKE_ATTEMPT = """#!/bin/sh
# What every call saw of the agents' configuration variables (all must be unset).
printf '%s\\n' "CLAUDE_CONFIG_DIR=${CLAUDE_CONFIG_DIR-unset}" "CODEX_HOME=${CODEX_HOME-unset}" \\
  "CURSOR_CONFIG_DIR=${CURSOR_CONFIG_DIR-unset}" "GEMINI_CONFIG_DIR=${GEMINI_CONFIG_DIR-unset}" \\
  "HOME=$HOME" >> "$CALLS.env"
if [ "$1" = "setup" ] && [ -n "$OLD_BINARY" ]; then
  echo "error: unrecognized subcommand 'setup'" >&2
  exit 2
fi
[ "$1 $2" = "setup --help" ] && exit 0
printf '%s\\n' "attempt $*" >> "$CALLS"
if [ "$1" = "setup" ]; then
  case " $* " in
    *" --dry-run "*)
      echo "attempt setup 9.9.9 (dry run - nothing was written)"
      echo "hooks        Claude Code   would install    /fake/.claude/settings.json"
      exit "${DRYRUN_EXIT:-0}"
      ;;
  esac
  echo "attempt setup 9.9.9"
  exit "${SETUP_EXIT:-0}"
fi
echo "attempt 9.9.9"
"""

# The download stubs are shell, not Python: this interpreter takes about half a
# second to start, and a run downloads three files.
CURL_STUB = """#!/bin/sh
url=""
out=""
prev=""
for a in "$@"; do
  case "$a" in https://*) url="$a" ;; esac
  [ "$prev" = "-o" ] && out="$a"
  prev="$a"
done
printf '%s\\n' "$url" >> "$CURL_LOG"
serve() {
  # serve FILE-OR-"-": the SHA256SUMS text, optionally tampered with.
  if [ -n "$TAMPER" ]; then { printf '%064d' 0; tail -c +65 "$FAKE_RELEASE/SHA256SUMS"; }; else cat "$FAKE_RELEASE/SHA256SUMS"; fi
}
case "$url" in
  */releases/latest) echo '{"tag_name": "v9.9.9"}' ;;
  */SHA256SUMS)
    if [ -n "$out" ]; then serve > "$out"; else serve; fi
    ;;
  *.tar.gz)
    src="$FAKE_RELEASE/${url##*/}"
    [ -f "$src" ] || exit 22
    cp "$src" "$out"
    ;;
  *) exit 22 ;;
esac
"""

# wget spells it `wget -qO FILE URL` (or `-qO-` for stdout); same release.
WGET_STUB = """#!/bin/sh
url=""
out=""
prev=""
for a in "$@"; do
  case "$a" in https://*) url="$a" ;; esac
  [ "$prev" = "-qO" ] && out="$a"
  prev="$a"
done
printf 'wget %s\\n' "$url" >> "$CURL_LOG"
case "$url" in
  */releases/latest) echo '{"tag_name": "v9.9.9"}' ;;
  */SHA256SUMS)
    if [ -n "$out" ]; then cat "$FAKE_RELEASE/SHA256SUMS" > "$out"; else cat "$FAKE_RELEASE/SHA256SUMS"; fi
    ;;
  *.tar.gz)
    src="$FAKE_RELEASE/${url##*/}"
    [ -f "$src" ] || exit 8
    cp "$src" "$out"
    ;;
  *) exit 8 ;;
esac
"""

# `uname -s` can be faked (to run the Darwin branches on Linux); everything else
# is the host's.
UNAME_STUB = """#!/bin/sh
if [ "$1" = "-s" ] && [ -n "$FAKE_OS" ]; then
  echo "$FAKE_OS"
  exit 0
fi
exec "$REAL_UNAME" "$@"
"""

# Commands the script must never run on its own, or must run in a known way.
# Each records its arguments in TOOL_LOG and exits with `<NAME>_EXIT`.
RECORDING_STUB = """#!/bin/sh
printf '%s\\n' "$(basename "$0") $*" >> "$TOOL_LOG"
name="$(basename "$0")"
if [ "$name" = "gh" ]; then
  exit "${GH_EXIT:-0}"
fi
exit 0
"""


def target_for(os_name, arch=None):
    arch = arch or os.uname().machine
    arch = {"x86_64": "x86_64", "amd64": "x86_64", "arm64": "aarch64", "aarch64": "aarch64"}[arch]
    if os_name == "Darwin":
        return f"{arch}-apple-darwin"
    return f"{arch}-unknown-linux-musl"


def write_executable(path, text):
    path.write_text(text)
    path.chmod(0o755)


class Run:
    """One run of the script: what it returned and what it said."""

    def __init__(self, returncode, stdout, stderr="", tty=""):
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = stderr
        # Everything the pseudo-terminal showed (prompts and echoed answers).
        self.tty = tty

    @property
    def output(self):
        return self.stdout + self.stderr + self.tty


class Env:
    """A fake machine: HOME, a stubbed release, a PATH of known tools."""

    def __init__(self, test, *, fake_os=None, bin_rel=None, wget_only=False):
        self._tmp = tempfile.TemporaryDirectory(prefix="attempt-install-sh-test-")
        test.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)
        self.home = self.root / "home"
        self.home.mkdir()
        self.tmpdir = self.root / "tmp"
        self.tmpdir.mkdir()
        # Default: where the script puts it (under HOME). `bin_rel` is a path under
        # the fake machine's root, outside HOME, for tests of other locations.
        self.bin_dir = self.root / bin_rel if bin_rel else self.home / ".local" / "bin"
        self.fake_os = fake_os
        self.calls_file = self.root / "calls"
        self.tool_log = self.root / "tools"
        self.curl_log = self.root / "curl"
        self.stubs = self.root / "stubs"
        self.stubs.mkdir()
        self._build_release()
        for name in TOOLS:
            source = shutil.which(name)
            if source:
                (self.stubs / name).symlink_to(source)
        self._real_uname = shutil.which("uname")
        write_executable(self.stubs / "uname", UNAME_STUB)
        if wget_only:
            write_executable(self.stubs / "wget", WGET_STUB)
        else:
            write_executable(self.stubs / "curl", CURL_STUB)

    def _build_release(self):
        self.release = self.root / "release"
        self.release.mkdir()
        stem = f"attempt-{VERSION}-{target_for(self.fake_os or os.uname().sysname)}"
        archive = self.release / f"{stem}.tar.gz"
        with tarfile.open(archive, "w:gz") as tar:
            for name in ("attempt", "attempt-hook"):
                data = FAKE_ATTEMPT.encode()
                info = tarfile.TarInfo(f"{stem}/{name}")
                info.size = len(data)
                info.mode = 0o755
                tar.addfile(info, io.BytesIO(data))
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        (self.release / "SHA256SUMS").write_text(f"{digest}  {archive.name}\n")

    # -- stubs ---------------------------------------------------------------

    def add_recording_tool(self, name):
        write_executable(self.stubs / name, RECORDING_STUB)

    # -- the process environment ---------------------------------------------

    def environ(self, extra=None, on_path=False):
        path = str(self.stubs)
        if on_path:
            path = f"{self.bin_dir}:{path}"
        env = {
            "HOME": str(self.home),
            "PATH": path,
            "TMPDIR": str(self.tmpdir),
            "SHELL": "/bin/sh",
            "LC_ALL": "C",
            "ATTEMPTDB_BIN_DIR": str(self.bin_dir),
            "FAKE_RELEASE": str(self.release),
            "CALLS": str(self.calls_file),
            "TOOL_LOG": str(self.tool_log),
            "CURL_LOG": str(self.curl_log),
            "REAL_UNAME": self._real_uname,
        }
        if self.fake_os:
            env["FAKE_OS"] = self.fake_os
        env.update(extra or {})
        for name in AGENT_CONFIG_VARS:
            env.pop(name, None)  # never inherited, never passed in
        return env

    # -- running ---------------------------------------------------------------

    def run(self, args=(), *, shell="/bin/sh", script=None, on_path=False, **extra):
        """No controlling terminal: how an agent, CI, or a cron job runs it."""
        proc = subprocess.run(
            [shell, "-s", "--", *args],
            input=SCRIPT.read_bytes() if script is None else script,
            env=self.environ(extra, on_path),
            capture_output=True,
            start_new_session=True,
            timeout=180,
        )
        return Run(proc.returncode, proc.stdout.decode(), proc.stderr.decode())

    def run_tty(self, args=(), answers=(), *, shell="/bin/sh", on_path=False,
                redirect_stdout=False, timeout=120, **extra):
        """A person at a terminal.

        The script text arrives on a pipe (stdin), the pseudo-terminal is the
        controlling terminal, and each (prompt, reply) pair in `answers` types
        `reply` once `prompt` has appeared. With `redirect_stdout` the script's
        stdout goes to a file, as in `curl | sh > log`: only the prompts are
        left on the terminal.
        """
        master, slave = pty.openpty()
        script_read, script_write = os.pipe()
        stdout_path = self.root / "tty-stdout"
        stdout_target = open(stdout_path, "wb") if redirect_stdout else slave

        def become_session_leader_with_this_terminal():
            os.setsid()
            fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

        proc = subprocess.Popen(
            [shell, "-s", "--", *args],
            stdin=script_read,
            stdout=stdout_target,
            stderr=slave,
            env=self.environ(extra, on_path),
            preexec_fn=become_session_leader_with_this_terminal,
            close_fds=True,
        )
        os.close(script_read)
        os.close(slave)
        if redirect_stdout:
            stdout_target.close()
        script = SCRIPT.read_bytes()

        def feed():
            try:
                view = memoryview(script)
                while view:
                    view = view[os.write(script_write, view):]
            except OSError:
                pass  # the script ended before reading it all: fine
            finally:
                os.close(script_write)

        writer = threading.Thread(target=feed, daemon=True)
        writer.start()

        seen = b""
        consumed = 0
        pending = list(answers)
        deadline = time.monotonic() + timeout
        try:
            while True:
                if time.monotonic() > deadline:
                    proc.kill()
                    raise AssertionError(
                        "install.sh did not finish; terminal so far:\n" + seen.decode(errors="replace"))
                ready, _, _ = select.select([master], [], [], 0.1)
                if ready:
                    try:
                        chunk = os.read(master, 4096)
                    except OSError as error:
                        if error.errno != errno.EIO:
                            raise
                        chunk = b""
                    if chunk:
                        seen += chunk
                    elif proc.poll() is not None:
                        break
                if pending and pending[0][0].encode() in seen[consumed:]:
                    prompt, reply = pending.pop(0)
                    consumed = seen.index(prompt.encode(), consumed) + len(prompt)
                    os.write(master, reply)
                elif not ready and proc.poll() is not None:
                    break
            proc.wait(timeout=10)
        finally:
            if proc.poll() is None:
                proc.kill()
            writer.join(timeout=5)
            os.close(master)
        if pending:
            raise AssertionError(
                f"never saw the prompt {pending[0][0]!r}; terminal:\n" + seen.decode(errors="replace"))
        stdout = stdout_path.read_text() if redirect_stdout else ""
        text = seen.decode(errors="replace").replace("\r\n", "\n")
        return Run(proc.returncode, stdout if redirect_stdout else text, "", text if redirect_stdout else "")

    # -- what happened ---------------------------------------------------------

    def calls(self):
        return self.calls_file.read_text().splitlines() if self.calls_file.exists() else []

    def agent_env_seen(self):
        """Every `NAME=value` line the fake attempt saw for the agent config variables and HOME."""
        path = Path(str(self.calls_file) + ".env")
        return path.read_text().splitlines() if path.exists() else []

    def tools(self):
        return self.tool_log.read_text().splitlines() if self.tool_log.exists() else []

    def curl_calls(self):
        return self.curl_log.read_text().splitlines() if self.curl_log.exists() else []

    def installed(self):
        return {name: (self.bin_dir / name).exists() for name in ("attempt", "attempt-hook")}

    def read_home(self, relative):
        path = self.home / relative
        return path.read_text() if path.exists() else None

    def tree(self):
        """Every path under the fake machine with its size and mtime, to prove nothing moved."""
        entries = {}
        for path in sorted(self.root.rglob("*")):
            if path in (self.curl_log, self.tool_log, self.calls_file, Path(str(self.calls_file) + ".env")):
                continue
            info = path.lstat()
            entries[str(path.relative_to(self.root))] = (info.st_size, info.st_mtime_ns)
        return entries
