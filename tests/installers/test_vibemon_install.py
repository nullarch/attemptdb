"""Installer regressions with stubbed commands; no network or host changes."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[2] / "docs/migration/vibemon-install.sh"

STUB = r'''#!/usr/bin/env python3
import json, os, pathlib, sys
name = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
with open(os.environ["CALLS"], "a") as out:
    out.write(json.dumps([name, *args]) + "\n")
if name == "uname":
    print(os.environ.get("FAKE_OS", "Linux") if args == ["-s"] else "x86_64")
elif name in ("systemctl", "launchctl"):
    sys.exit(int(os.environ.get("SERVICE_EXIT", "0")))
elif name == "attempt":
    if args == ["--version"]: print("attempt 0.2.9")
    elif args == ["sync", "status", "--json"]: print('{"connected":false}')
    elif args == ["daemon", "install"]:
        if os.environ.get("DIAGNOSTIC"): print(os.environ["DIAGNOSTIC"])
        print("service registration failed" if os.environ.get("DAEMON_EXIT") else "service registered")
        sys.exit(int(os.environ.get("DAEMON_EXIT", "0")))
    elif args == ["sync", "now"]: sys.exit(int(os.environ.get("UPLOAD_EXIT", "0")))
elif name == "curl":
    url = next((x for x in args if x.startswith("https://")), "")
    if url.endswith("/api/attemptdb/pair"): print('{"token":"pair_fixture","sync_url":"https://sync.example.test"}')
    elif "/v1/pair/" in url: print("200")
    elif url.endswith(".ps1") and "-o" in args:
        pathlib.Path(args[args.index("-o") + 1]).write_text("# fixture only\n")
    elif url.endswith("install-report"):
        pathlib.Path(os.environ["REPORT_FILE"]).write_text(args[args.index("--data") + 1])
elif name == "cygpath": print(args[-1])
elif name == "powershell.exe": sys.exit(int(os.environ.get("NATIVE_EXIT", "0")))
'''


class MigrationTests(unittest.TestCase):
    def run_install(self, args=(), legacy=False, **settings):
        with tempfile.TemporaryDirectory(prefix="attempt-install-test-") as temp:
            root = Path(temp)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            for name in ("uname", "attempt", "curl", "systemctl", "launchctl", "cygpath", "powershell.exe"):
                path = bin_dir / name
                # Use this test's interpreter, independent of system Python.
                path.write_text(STUB.replace("#!/usr/bin/env python3", "#!" + os.sys.executable))
                path.chmod(0o755)
            if legacy:
                (root / ".vibemon").mkdir()
                (root / ".vibemon/api-key").write_text("vbm_fixture")
            env = {**os.environ, "HOME": temp, "XDG_STATE_HOME": str(root / "state"),
                   "PATH": f"{bin_dir}:/usr/bin:/bin", "ATTEMPTDB_BIN_DIR": str(bin_dir),
                   "CALLS": str(root / "calls"), "REPORT_FILE": str(root / "report"), **settings}
            result = subprocess.run(["/bin/sh", str(SCRIPT), *args], env=env, capture_output=True, text=True)
            calls = [json.loads(line) for line in (root / "calls").read_text().splitlines()]
            report = json.loads((root / "report").read_text()) if (root / "report").exists() else None
            return result.returncode, calls, report

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

    def test_ephemeral_auto_migration_skips_before_pairing(self):
        code, calls, report = self.run_install(legacy=True, SERVICE_EXIT="1")
        self.assertEqual((code, report["step"]), (0, "skipped_environment"))
        self.assertIn("systemd", report["error"])
        self.assertFalse(any("/api/attemptdb/pair" in " ".join(c) for c in calls))
        self.assertFalse(any(c[:2] == ["attempt", "init"] for c in calls))
        self.assertNotIn("vbm_fixture", report["log_tail"])

    def test_explicit_install_without_service_fails_before_pairing(self):
        code, calls, report = self.run_install(["pair_fixture"], SERVICE_EXIT="1")
        self.assertEqual((code, report["step"]), (1, "environment"))
        self.assertFalse(any("/v1/pair/" in " ".join(c) for c in calls))

    def test_success_requires_service_then_upload_before_legacy_removal(self):
        code, calls, report = self.run_install(legacy=True)
        self.assertEqual((code, report["step"]), (0, "done"))
        service = calls.index(["systemctl", "--user", "show-environment"])
        pairing = next(i for i, c in enumerate(calls) if "/api/attemptdb/pair" in " ".join(c))
        self.assertLess(service, pairing)
        self.assertLess(calls.index(["attempt", "daemon", "install"]), calls.index(["attempt", "sync", "now"]))
        self.assertLess(calls.index(["attempt", "sync", "now"]), calls.index(["attempt", "hook", "install", "--remove-legacy", "vibemon"]))

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


if __name__ == "__main__":
    unittest.main()
