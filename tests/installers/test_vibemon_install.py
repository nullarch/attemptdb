"""Installer regressions with stubbed commands; no network or host changes."""
import re
import json
import os
from pathlib import Path
import subprocess
import shutil
import tempfile
import types
import unittest
from unittest import mock

from install_sh_harness import AGENT_CONFIG_VARS, scrubbed_environ

# VIBEMON_INSTALL_SCRIPT points the suite at another copy of the script, to show that
# a new test fails on the script it was written against.
SCRIPT = Path(os.environ.get("VIBEMON_INSTALL_SCRIPT") or Path(__file__).resolve().parents[2] / "docs/migration/vibemon-install.sh")

STUB = r'''#!/usr/bin/env python3
import json, os, pathlib, sys
name = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
with open(os.environ["CALLS"], "a") as out:
    out.write(json.dumps([name, *args]) + "\n")
if name == "attempt":
    with open(os.environ["CALLS"] + ".env", "a") as out:
        out.write(json.dumps({v: os.environ.get(v) for v in ("CLAUDE_CONFIG_DIR", "CODEX_HOME", "CURSOR_CONFIG_DIR", "GEMINI_CONFIG_DIR")}) + "\n")
if name == "attempt" and args[:1] == ["init"] and os.environ.get("LOCK_STATE_DIR"):
    # The disk filling up, or the directory going read-only, in the middle of an install.
    os.chmod(os.environ["LOCK_STATE_DIR"], 0o500)
if name == "uname":
    print(os.environ.get("FAKE_OS", "Linux") if args == ["-s"] else "x86_64")
elif name in ("systemctl", "launchctl"):
    sys.exit(int(os.environ.get("SERVICE_EXIT", "0")))
elif name == "attempt":
    if args == ["--version"]: print("attempt " + os.environ.get("PRESENT_VERSION", "__WORKSPACE_VERSION__"))
    elif args == ["sync", "status", "--json"]: print(json.dumps({"connected": os.environ.get("CONNECTED") == "1"}))
    elif args[:2] == ["daemon", "status"]:
        print(json.dumps({"endpoint": "unix:/fixture/daemon.sock", "running": True}))
        if os.environ.get("SESSION_STATUS_FAIL_AFTER"):
            history = [json.loads(line) for line in pathlib.Path(os.environ["CALLS"]).read_text().splitlines()]
            count = sum(c[:3] == ["attempt", "daemon", "status"] for c in history)
            if count > int(os.environ["SESSION_STATUS_FAIL_AFTER"]): sys.exit(1)
        sys.exit(int(os.environ.get("SESSION_STATUS_EXIT", "0")))
    elif args == ["daemon", "install"]:
        if os.environ.get("DIAGNOSTIC"): print(os.environ["DIAGNOSTIC"])
        if os.environ.get("DAEMON_STDERR"): print(os.environ["DAEMON_STDERR"], file=sys.stderr)
        print("service registration failed" if os.environ.get("DAEMON_EXIT") else "service registered")
        sys.exit(int(os.environ.get("DAEMON_EXIT", "0")))
    elif args == ["sync", "now"]: sys.exit(int(os.environ.get("UPLOAD_EXIT", "0")))
    # `status` succeeds when a database exists; `--json` says which mode it is in.
    elif args == ["status"]: sys.exit(int(os.environ.get("STATUS_EXIT", "0")))
    elif args == ["status", "--json"]: print(json.dumps({"capture_mode": os.environ.get("EXISTING_MODE", "local_semantic")}))
elif name == "curl":
    url = next((x for x in args if x.startswith("https://")), "")
    if url.endswith("/api/attemptdb/pair"):
        if os.environ.get("PAIR_API_ERROR"): print('{"error":"' + os.environ["PAIR_API_ERROR"] + '"}')
        else: print('{"token":"pair_fixture","sync_url":"https://sync.example.test"}')
    elif "/v1/pair/" in url:
        if os.environ.get("PAIR_CURL_FAIL"):
            # A request that never got an answer: curl -w still prints 000, then fails.
            print("000"); print("curl: (6) Could not resolve host: sync.example.test", file=sys.stderr); sys.exit(6)
        print("200")
    elif url.endswith("/install.sh") and "-o" in args:
        # The binary installer: record what it was asked to do, install nothing.
        pathlib.Path(args[args.index("-o") + 1]).write_text(
            '#!/bin/sh\nprintf \'["install.sh", "%s", "%s"]\\n\' "${ATTEMPTDB_NO_SETUP:-unset}" "${ATTEMPTDB_MODIFY_PATH:-unset}" >> "$CALLS"\n')
    elif url.endswith(".ps1") and "-o" in args:
        pathlib.Path(args[args.index("-o") + 1]).write_text("# fixture only\n")
    elif url.endswith("install-report"):
        pathlib.Path(os.environ["REPORT_FILE"]).write_text(args[args.index("--data") + 1])
elif name == "cygpath": print(args[-1])
elif name == "powershell.exe":
    # What the native installer would inherit from the environment.
    with open(os.environ["CALLS"], "a") as out:
        out.write(json.dumps(["powershell-env", os.environ.get("VIBEMON_CAPTURE_MODE", "")]) + "\n")
    sys.exit(int(os.environ.get("NATIVE_EXIT", "0")))
'''

# The fake `attempt` reports the workspace version — the same one the script
# pins — so the "present and recent enough; keeping it" path is exercised.
WORKSPACE_VERSION = re.search(
    r'^version = "([0-9]+\.[0-9]+\.[0-9]+)"',
    (Path(__file__).resolve().parents[2] / "Cargo.toml").read_text(),
    re.M,
).group(1)
STUB = STUB.replace("__WORKSPACE_VERSION__", WORKSPACE_VERSION)


def run_on_a_terminal(cmd, env, timeout=120):
    """What a person pasting the command sees: stdout and stderr are one pseudo-terminal.

    Returns an object like subprocess.run's, with the whole screen in `stdout`.
    """
    import errno
    import pty

    master, slave = pty.openpty()
    proc = subprocess.Popen(cmd, env=env, stdin=subprocess.DEVNULL, stdout=slave, stderr=slave, close_fds=True)
    os.close(slave)
    screen = b""
    try:
        while True:
            try:
                chunk = os.read(master, 4096)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                break
            if not chunk:
                break
            screen += chunk
        code = proc.wait(timeout=timeout)
    finally:
        if proc.poll() is None:
            proc.kill()
        os.close(master)
    return types.SimpleNamespace(returncode=code, stdout=screen.decode(errors="replace").replace("\r\n", "\n"), stderr="")


class MigrationTests(unittest.TestCase):
    def run_install(self, args=(), legacy=False, missing=(), attended=False, **settings):
        with tempfile.TemporaryDirectory(prefix="attempt-install-test-") as temp:
            root = Path(temp)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            # Keep command availability deterministic across macOS and Linux.
            for name in ("sh", "date", "mkdir", "mktemp", "sed", "head", "tr", "tail", "cut", "awk", "grep", "rm", "uname", "id", "hostname", "sha256sum", "shasum", "sleep", "cat", "wc", "mv", "chmod", "df"):
                source = shutil.which(name)
                if source:
                    (bin_dir / name).symlink_to(source)
            for name in ("nohup", "setsid", "flock"):
                if name not in missing:
                    (bin_dir / name).write_text("#!/bin/sh\nexit 0\n")
                    (bin_dir / name).chmod(0o755)
            for name in ("uname", "attempt", "curl", "systemctl", "launchctl", "cygpath", "powershell.exe"):
                path = bin_dir / name
                if path.is_symlink():
                    path.unlink()
                # Use this test's interpreter, independent of system Python.
                path.write_text(STUB.replace("#!/usr/bin/env python3", "#!" + os.sys.executable))
                path.chmod(0o755)
            if legacy:
                (root / ".vibemon").mkdir()
                (root / ".vibemon/api-key").write_text("vbm_fixture")
            env = {**scrubbed_environ(), "HOME": temp, "XDG_STATE_HOME": str(root / "state"),
                   "PATH": str(bin_dir), "ATTEMPTDB_BIN_DIR": str(bin_dir),
                   "CALLS": str(root / "calls"), "REPORT_FILE": str(root / "report"), **settings}
            cmd = [os.environ.get("VIBEMON_TEST_SHELL") or "/bin/sh", str(SCRIPT), *args]
            if attended:
                result = run_on_a_terminal(cmd, env)
            else:
                result = subprocess.run(cmd, env=env, capture_output=True, text=True)
            logs = [p for p in (root / ".vibemon" / "vibemon-install.log", Path(env["XDG_STATE_HOME"]) / "attemptdb" / "vibemon-install.log") if p.exists()]
            self.log_file = logs[0] if logs else None
            self.log_text = logs[0].read_text() if logs else None
            self.log_mode = (logs[0].stat().st_mode & 0o777) if logs else None
            calls =[json.loads(line) for line in (root / "calls").read_text().splitlines()] if (root / "calls").exists() else []
            report = json.loads((root / "report").read_text()) if (root / "report").exists() else None
            self.last = result
            seen = root / "calls.env"
            self.agent_env = [json.loads(line) for line in seen.read_text().splitlines()] if seen.exists() else []
            return result.returncode, calls, report

    def test_the_agents_config_variables_of_the_caller_never_reach_the_commands(self):
        sentinels = {name: "/nonexistent/real-" + name.lower() for name in AGENT_CONFIG_VARS}
        with mock.patch.dict(os.environ, sentinels):
            code, calls, report = self.run_install(legacy=True)
        self.assertEqual((code, report["step"]), (0, "done"))
        self.assertTrue(self.agent_env, "attempt was called")
        for seen in self.agent_env:
            self.assertEqual(seen, {name: None for name in AGENT_CONFIG_VARS})

    def test_no_credentials_is_noop(self):
        code, calls, report = self.run_install(SERVICE_EXIT="1")
        self.assertEqual((code, report["step"]), (0, "noop"))
        self.assertFalse(any(c[0] == "systemctl" for c in calls))

    def test_foreign_pairing_input_is_rejected_without_disclosure_or_exchange(self):
        secret = "foreign-service-fixture-credential-123456"
        code, calls, report = self.run_install(["--pair", secret], SERVICE_EXIT="1")
        self.assertEqual((code, report["step"]), (1, "pair"))
        self.assertNotIn(secret, json.dumps(report))
        self.assertIn("vibemon.dev/devices", report["error"])
        self.assertFalse(any("/v1/pair/" in " ".join(c) for c in calls))
        self.assertFalse(any(c[0] == "systemctl" for c in calls))

    def test_auto_migration_without_runtime_tools_skips_before_pairing(self):
        code, calls, report = self.run_install(legacy=True, missing=("setsid",), SERVICE_EXIT="1")
        self.assertEqual((code, report["step"]), (0, "skipped_environment"))
        self.assertIn("setsid", report["error"])
        self.assertFalse(any("/api/attemptdb/pair" in " ".join(c) for c in calls))
        self.assertFalse(any(c[:2] == ["attempt", "init"] for c in calls))
        self.assertNotIn("vbm_fixture", report["log_tail"])

    def test_explicit_install_without_any_runtime_fails_before_pairing(self):
        code, calls, report = self.run_install(["pair_fixture"], missing=("setsid",), SERVICE_EXIT="1")
        self.assertEqual((code, report["step"]), (1, "environment"))
        self.assertFalse(any("/v1/pair/" in " ".join(c) for c in calls))

    def test_linux_without_service_uses_session_runtime_before_connecting(self):
        code, calls, report = self.run_install(["pair_fixture"], SERVICE_EXIT="1")
        self.assertEqual((code, report["step"]), (0, "done"))
        self.assertNotIn(["attempt", "daemon", "install"], calls)
        self.assertLess(calls.index(["attempt", "daemon", "status"]),
                        next(i for i, c in enumerate(calls) if c[:3] == ["attempt", "sync", "connect"]))
        self.assertIn("Linux session sync", report["log_tail"])

    def test_connected_legacy_key_does_not_mint_another_pairing(self):
        code, calls, report = self.run_install(["vbm_fixture"], CONNECTED="1")
        self.assertEqual((code, report["step"]), (0, "done"))
        self.assertFalse(any("/api/attemptdb/pair" in " ".join(c) for c in calls))
        self.assertFalse(any(c[:3] == ["attempt", "sync", "connect"] for c in calls))

    def test_session_readiness_failure_preserves_legacy_and_does_not_pair(self):
        code, calls, report = self.run_install(["pair_fixture"], SERVICE_EXIT="1", SESSION_STATUS_EXIT="1")
        self.assertEqual((code, report["step"]), (1, "daemon"))
        self.assertFalse(any(c[:3] == ["attempt", "sync", "connect"] for c in calls))
        self.assertFalse(any(c[:3] == ["attempt", "hook", "install"] for c in calls))

    def test_session_loss_after_upload_still_preserves_legacy(self):
        code, calls, report = self.run_install(["pair_fixture"], SERVICE_EXIT="1", SESSION_STATUS_FAIL_AFTER="2")
        self.assertEqual((code, report["step"]), (1, "upload"))
        self.assertIn(["attempt", "sync", "now"], calls)
        self.assertFalse(any("--remove-legacy" in c for c in calls))

    def test_mac_without_gui_domain_remains_blocked(self):
        code, calls, report = self.run_install(["pair_fixture"], SERVICE_EXIT="1", FAKE_OS="Darwin")
        self.assertEqual((code, report["step"]), (1, "environment"))
        self.assertIn("desktop session", report["error"])

    def test_success_requires_service_then_upload_before_legacy_removal(self):
        code, calls, report = self.run_install(legacy=True)
        self.assertEqual((code, report["step"]), (0, "done"))
        service = calls.index(["systemctl", "--user", "show-environment"])
        pairing = next(i for i, c in enumerate(calls) if "/api/attemptdb/pair" in " ".join(c))
        self.assertLess(service, pairing)
        self.assertLess(calls.index(["attempt", "daemon", "install"]), calls.index(["attempt", "sync", "now"]))
        self.assertLess(calls.index(["attempt", "sync", "now"]), calls.index(["attempt", "hook", "install", "--remove-legacy", "vibemon"]))

    def test_binary_installer_is_asked_for_the_binary_only(self):
        # From 0.2.14 install.sh runs `attempt setup` unless told not to; here
        # that would wire hooks and a daemon before pairing.
        code, calls, report = self.run_install(legacy=True, PRESENT_VERSION="0.1.0")
        self.assertEqual((code, report["step"]), (0, "done"))
        # NO_SETUP=1 keeps it to the binary; MODIFY_PATH=0 keeps it from asking
        # about a shell profile in the middle of a pairing.
        self.assertIn(["install.sh", "1", "0"], calls)
        self.assertLess(calls.index(["install.sh", "1", "0"]), calls.index(["attempt", "hook", "install"]))

    def test_an_explicit_path_choice_reaches_the_binary_installer(self):
        code, calls, report = self.run_install(legacy=True, PRESENT_VERSION="0.1.0", ATTEMPTDB_MODIFY_PATH="1")
        self.assertEqual((code, report["step"]), (0, "done"))
        self.assertIn(["install.sh", "1", "1"], calls)

    # -- capture mode: RFC 0006 section 2, the installer never raises it ----------

    def init_call(self, calls):
        return next(c for c in calls if c[:2] == ["attempt", "init"])

    def test_an_existing_metadata_only_database_is_not_raised_by_default(self):
        code, calls, report = self.run_install(legacy=True, EXISTING_MODE="metadata_only")
        self.assertEqual((code, report["step"]), (0, "done"))
        self.assertEqual(self.init_call(calls), ["attempt", "init", "--source", "vibemon"],
                         "no --capture-mode: the mode is left exactly as it is")
        self.assertIn("capture mode: metadata_only (existing database, kept as it was)", report["log_tail"])
        # The default `messages` profile has nothing to upload from such a database: say so.
        self.assertIn("stores no conversation text", report["log_tail"])

    def test_an_existing_local_semantic_database_is_kept_as_it_is(self):
        code, calls, report = self.run_install(legacy=True, EXISTING_MODE="local_semantic")
        self.assertEqual(self.init_call(calls), ["attempt", "init", "--source", "vibemon"])
        self.assertIn("capture mode: local_semantic (existing database, kept as it was)", report["log_tail"])
        self.assertNotIn("stores no conversation text", report["log_tail"])

    def test_an_explicit_mode_changes_an_existing_database_either_way(self):
        cases = (
            (["--local-content"], {}, "metadata_only", "local_semantic"),
            (["--capture-mode", "local_semantic"], {}, "metadata_only", "local_semantic"),
            (["--capture-mode=metadata_only"], {}, "local_semantic", "metadata_only"),
            (["--metadata-only"], {}, "local_semantic", "metadata_only"),
            ([], {"VIBEMON_CAPTURE_MODE": "local_semantic"}, "metadata_only", "local_semantic"),
            (["--local-content"], {"VIBEMON_CAPTURE_MODE": "metadata_only"}, "metadata_only", "local_semantic"),
        )
        for flags, env, existing, wanted in cases:
            with self.subTest(flags=flags, env=env, existing=existing):
                code, calls, report = self.run_install(flags, legacy=True, EXISTING_MODE=existing, **env)
                self.assertEqual((code, report["step"]), (0, "done"))
                self.assertEqual(self.init_call(calls),
                                 ["attempt", "init", "--capture-mode", wanted, "--source", "vibemon"])
                self.assertIn(f"capture mode: {wanted} (set by you, was {existing})", report["log_tail"])

    def test_a_new_database_is_local_semantic_unless_a_mode_was_asked_for(self):
        for flags, wanted, note in (
            ([], "local_semantic", "new database"),
            (["--metadata-only"], "metadata_only", "new database, set by you"),
            (["--capture-mode", "full_sync"], "full_sync", "new database, set by you"),
        ):
            with self.subTest(flags=flags):
                code, calls, report = self.run_install(flags, legacy=True, STATUS_EXIT="1")
                self.assertEqual((code, report["step"]), (0, "done"))
                self.assertEqual(self.init_call(calls),
                                 ["attempt", "init", "--capture-mode", wanted, "--source", "vibemon"])
                self.assertIn(f"capture mode: {wanted} ({note})", report["log_tail"])

    def test_an_unknown_capture_mode_is_refused_before_anything_changes(self):
        code, calls, report = self.run_install(["pair_fixture", "--capture-mode", "everything"])
        self.assertEqual(code, 2)
        self.assertIn("unknown capture mode", self.last.stderr)
        self.assertEqual(calls, [], "no command ran: no network, no init, no service")

    def test_the_windows_handoff_forwards_exactly_the_mode_that_was_asked_for(self):
        for flags, present, absent, env_mode in (
            ([], (), ("-MetadataOnly", "-LocalContent"), ""),
            (["--metadata-only"], ("-MetadataOnly",), ("-LocalContent",), "metadata_only"),
            (["--local-content"], ("-LocalContent",), ("-MetadataOnly",), "local_semantic"),
            (["--capture-mode", "full_sync"], (), ("-MetadataOnly", "-LocalContent"), "full_sync"),
        ):
            with self.subTest(flags=flags):
                code, calls, report = self.run_install(["pair_fixture", *flags], FAKE_OS="MINGW64_NT-10.0")
                self.assertEqual(code, 0)
                native = next(c for c in calls if c[0] == "powershell.exe")
                for flag in present:
                    self.assertIn(flag, native)
                for flag in absent:
                    self.assertNotIn(flag, native)
                self.assertIn(["powershell-env", env_mode], calls)

    def test_service_and_upload_failure_preserve_legacy(self):
        for setting, step in (("DAEMON_EXIT", "daemon"), ("UPLOAD_EXIT", "upload")):
            with self.subTest(step=step):
                code, calls, report = self.run_install(legacy=True, **{setting: "1"})
                self.assertEqual((code, report["step"]), (1, step))
                self.assertTrue(report["error"])
                self.assertFalse(any("--remove-legacy" in c for c in calls))

    def test_windows_shell_hands_off_credentials_and_flags_once(self):
        for platform in ("MINGW64_NT-10.0", "MSYS_NT-10.0", "CYGWIN_NT-10.0"):
            with self.subTest(platform=platform):
                code, calls, report = self.run_install(
                    ["pair_fixture", "--keep-legacy", "--local-content", "--profile", "metadata_only", "--no-report"], FAKE_OS=platform)
                self.assertEqual(code, 0)
                self.assertIsNone(report)
                native = next(c for c in calls if c[0] == "powershell.exe")
                for flag in ("-KeepLegacy", "-LocalContent", "-NoReport", "pair_fixture", "metadata_only"):
                    self.assertIn(flag, native)
                self.assertFalse(any("/v1/pair/" in " ".join(c) for c in calls))

    def test_native_failure_is_not_reported_as_success(self):
        code, calls, report = self.run_install(legacy=True, FAKE_OS="MINGW64_NT-10.0", NATIVE_EXIT="7")
        self.assertEqual(code, 7)
        self.assertIsNone(report)  # Only the native installer reports.
        native = next(c for c in calls if c[0] == "powershell.exe")
        self.assertIn("vbm_fixture", native)

    def test_long_diagnostics_remain_valid_json_and_redacted(self):
        diagnostic = ('error: "fixture" \\ invalid vbm_fixture /home/dev/example/project\n' * 100)
        code, calls, report = self.run_install(legacy=True, DAEMON_EXIT="1", DIAGNOSTIC=diagnostic)
        self.assertEqual(code, 1)
        self.assertNotIn("vbm_fixture", report["log_tail"])
        self.assertNotIn("/home/dev", report["log_tail"])
        self.assertIn('"fixture"', report["log_tail"])
        self.assertIn('\\ invalid', report["log_tail"])


class FailureDiagnosticsTests(unittest.TestCase):
    """A failed install must be readable afterwards, attended or not.

    Four Linux installs failed at the `daemon` step in two weeks with an empty
    log in the report: they were run by a person at a terminal, and only an
    unattended run kept a log. The reason a service step fails is the failing
    command's own message plus a few facts about the machine, so both go in.
    """

    run_install = MigrationTests.run_install

    STDERR = "Failed to enable unit: Unit dev.attemptdb.daemon.service not found. XDG_RUNTIME_DIR is not set"

    def lines(self, text):
        """What was shown, without the log's own bookkeeping (header, command markers)."""
        return [l for l in text.splitlines() if l and not l.startswith(("===", "$ "))]

    # -- the failing command's output reaches the report ------------------------

    def test_an_attended_daemon_failure_reports_the_commands_stderr_and_the_machine(self):
        code, calls, report = self.run_install(["pair_fixture"], attended=True, DAEMON_EXIT="1", DAEMON_STDERR=self.STDERR)
        self.assertEqual((code, report["step"], report["unattended"]), (1, "daemon", False))
        tail = report["log_tail"]
        self.assertIn(self.STDERR, tail)
        self.assertIn("--- environment", tail)
        for fact in ("uid=", "root=", "XDG_RUNTIME_DIR=", "user_manager=", "hints="):
            self.assertIn(fact, tail)
        # The END of the log is what a report keeps: the command's words come
        # before the facts, and both come after everything that went well.
        self.assertLess(tail.index(self.STDERR), tail.index("--- environment"))
        self.assertLess(tail.index("capture mode"), tail.index(self.STDERR))
        # The person saw the same words on the terminal, as before.
        self.assertIn(self.STDERR, self.last.stdout)
        self.assertIn("background service registration failed", self.last.stdout)
        # And the full log is on disk, private to the user.
        self.assertIn(self.STDERR, self.log_text)
        self.assertEqual(self.log_mode, 0o600)

    def test_an_unattended_daemon_failure_reports_the_same(self):
        code, calls, report = self.run_install(["pair_fixture"], DAEMON_EXIT="1", DAEMON_STDERR=self.STDERR)
        self.assertEqual((code, report["step"], report["unattended"]), (1, "daemon", True))
        self.assertIn(self.STDERR, report["log_tail"])
        self.assertIn("--- environment", report["log_tail"])
        self.assertLess(report["log_tail"].index(self.STDERR), report["log_tail"].index("--- environment"))

    def test_the_environment_facts_are_written_for_the_environment_step_and_the_service_step_only(self):
        code, calls, report = self.run_install(["pair_fixture"], attended=True, missing=("setsid",), SERVICE_EXIT="1")
        self.assertEqual((code, report["step"]), (1, "environment"))
        self.assertIn("--- environment", report["log_tail"])
        code, calls, report = self.run_install(["pair_fixture"], attended=True, UPLOAD_EXIT="1")
        self.assertEqual((code, report["step"]), (1, "upload"))
        self.assertNotIn("--- environment", report["log_tail"])

    def test_the_facts_reach_the_report_even_when_there_is_nowhere_to_keep_a_log(self):
        with tempfile.TemporaryDirectory(prefix="attempt-nolog-") as other:
            blocked = Path(other) / "a-file-not-a-directory"
            blocked.write_text("")
            code, calls, report = self.run_install(["pair_fixture"], attended=True, DAEMON_EXIT="1", XDG_STATE_HOME=str(blocked))
        self.assertIsNone(self.log_file, "no place to write: no log, silently")
        self.assertEqual((code, report["step"]), (1, "daemon"))
        self.assertIn("--- environment", report["log_tail"])

    def test_the_binary_installer_and_the_first_upload_leave_a_trace_in_the_log(self):
        code, calls, report = self.run_install(["pair_fixture"], attended=True, PRESENT_VERSION="0.1.0", UPLOAD_EXIT="1")
        self.assertEqual((code, report["step"]), (1, "upload"))
        # `sync now` and the binary installer run through the same capture.
        self.assertIn("$ attempt sync now", self.log_text)
        self.assertIn("$ sh", self.log_text)

    # -- what the person sees does not change ------------------------------------

    def test_a_successful_attended_run_prints_what_an_unattended_run_logs(self):
        code, calls, report = self.run_install(["pair_fixture"], attended=True)
        self.assertEqual((code, report["step"], report["unattended"]), (0, "done", False))
        shown = self.lines(self.last.stdout)
        code, calls, report = self.run_install(["pair_fixture"])
        self.assertEqual((code, report["step"]), (0, "done"))
        logged = self.lines(self.log_text)
        self.assertEqual(shown, logged)
        self.assertIn("done. https://vibemon.dev/devices shows this device", self.last.stdout + "\n".join(logged))

    def test_the_exit_status_of_a_failed_command_is_the_commands_own(self):
        for setting, step in (("DAEMON_EXIT", "daemon"), ("UPLOAD_EXIT", "upload")):
            with self.subTest(step=step):
                code, calls, report = self.run_install(["pair_fixture"], attended=True, **{setting: "1"})
                self.assertEqual((code, report["step"]), (1, step))
                self.assertFalse(any("--remove-legacy" in c for c in calls))

    def test_an_attended_dry_run_writes_no_log(self):
        # A dry run prints the pairing token; nothing of it may land on disk.
        code, calls, report = self.run_install(["pair_fixture", "--dry-run"], attended=True)
        self.assertEqual(code, 0)
        self.assertIn("+ curl -fsS", self.last.stdout)
        self.assertIsNone(self.log_file)

    # -- what goes in the report is safe and bounded -----------------------------

    def test_captured_output_is_scrubbed_before_it_is_reported(self):
        words = ("error: key atk_0123456789abcdef refused for /home/dev/secret/project; "
                 "Authorization: Bearer sk-live-0123456789 and vbm_fixturekey0123")
        for attended in (True, False):
            with self.subTest(attended=attended):
                code, calls, report = self.run_install(["pair_fixture"], attended=attended, DAEMON_EXIT="1", DAEMON_STDERR=words)
                self.assertEqual(report["step"], "daemon")
                text = json.dumps(report)
                for secret in ("atk_0123456789abcdef", "/home/dev", "sk-live-0123456789", "vbm_fixturekey0123"):
                    self.assertNotIn(secret, text)
                self.assertIn("atk_", report["log_tail"])  # the shape stays, the secret does not
                self.assertIn("Bearer", report["log_tail"])

    def test_a_flood_of_output_keeps_the_end_and_the_report_inside_the_receivers_limits(self):
        flood = "\n".join(f'line {i}: "quoted" \\ backslash {"x" * 150}' for i in range(300))
        code, calls, report = self.run_install(["pair_fixture"], attended=True, DAEMON_EXIT="1", DIAGNOSTIC=flood, DAEMON_STDERR=self.STDERR)
        self.assertEqual(report["step"], "daemon")
        self.assertLessEqual(len(json.dumps(report)), 8192, "the web answers 413 above 8 KB and drops the report")
        self.assertLessEqual(len(report["log_tail"]), 4096, "the web keeps the first 4 KB of the tail")
        self.assertIn(self.STDERR, report["log_tail"], "the end of the log is what is kept")
        self.assertIn("--- environment", report["log_tail"])

    def test_terminal_control_characters_never_break_the_json(self):
        code, calls, report = self.run_install(["pair_fixture"], attended=True, DAEMON_EXIT="1", DAEMON_STDERR="\x1b[31mred\x1b[0m error\x07")
        self.assertEqual(report["step"], "daemon")
        self.assertIn("red", report["log_tail"])
        self.assertNotIn("\x1b", report["log_tail"])

    def test_a_command_is_never_not_run_because_the_log_cannot_be_written(self):
        # The state directory goes read-only after the first command: the files a
        # command's output is held in cannot be made. Every later command still runs.
        with tempfile.TemporaryDirectory(prefix="attempt-lockedlog-") as state:
            (Path(state) / "attemptdb").mkdir()
            code, calls, report = self.run_install(["pair_fixture"], attended=True,
                                                   XDG_STATE_HOME=state, LOCK_STATE_DIR=str(Path(state) / "attemptdb"))
            os.chmod(Path(state) / "attemptdb", 0o700)  # so the temporary directory can be removed
        self.assertEqual((code, report["step"]), (0, "done"), self.last.stdout)
        for command in (["attempt", "sync", "connect"], ["attempt", "hook", "install"], ["attempt", "daemon", "install"], ["attempt", "sync", "now"]):
            self.assertTrue(any(c[:3] == command or c[:len(command)] == command for c in calls), command)

    def test_the_log_stays_bounded_across_runs(self):
        with tempfile.TemporaryDirectory(prefix="attempt-biglog-") as state:
            big = Path(state) / "attemptdb"
            big.mkdir()
            (big / "vibemon-install.log").write_text("old line of an earlier run\n" * 20000)
            self.assertGreater((big / "vibemon-install.log").stat().st_size, 200000)
            code, calls, report = self.run_install(["pair_fixture"], attended=True, XDG_STATE_HOME=state)
            self.assertEqual((code, report["step"]), (0, "done"))
            self.assertLess(len(self.log_text), 100000)
            self.assertIn("done. https://vibemon.dev/devices", self.log_text)

    # -- the pairing check says what is wrong ------------------------------------

    def test_an_unreachable_server_is_reported_as_unreachable(self):
        # curl -w prints 000 for a request that never got an answer; the script
        # added another 000 and then said "the server answered 000000".
        code, calls, report = self.run_install(["pair_fixture"], attended=True, PAIR_CURL_FAIL="1")
        self.assertEqual((code, report["step"]), (1, "pair"))
        self.assertIn("cannot reach", report["error"])
        self.assertNotIn("answered", report["error"])
        self.assertIn("Could not resolve host", report["log_tail"])
        self.assertIn("Could not resolve host", self.last.stdout)

    def test_a_refused_api_key_exchange_is_a_pairing_failure_not_an_environment_one(self):
        code, calls, report = self.run_install(["vbm_fixturekey0123"], PAIR_API_ERROR="This operation was aborted")
        self.assertEqual(code, 1)
        self.assertEqual(report["step"], "pair")
        self.assertIn("This operation was aborted", report["error"])
        self.assertNotIn("--- environment", report["log_tail"])


if __name__ == "__main__":
    unittest.main()
