"""Full PowerShell installer failures against a loopback report receiver."""
import http.server
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import unittest

SCRIPT = Path(__file__).resolve().parents[2] / "docs/migration/vibemon-install.ps1"
POWERSHELL = os.environ.get("ATTEMPT_TEST_POWERSHELL") or shutil.which("powershell.exe") or shutil.which("pwsh")


@unittest.skipUnless(POWERSHELL, "PowerShell is not installed")
class ReportTests(unittest.TestCase):
    def invoke(self, *, no_report=False, token="pair_fixture", api_key=None, preflight=200, download_script=None):
        reports = []

        class Web(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_GET(self):
                if self.path == "/download.ps1" and download_script:
                    self.send_response(200)
                    self.send_header("Content-Type", "text/plain; charset=utf-8")
                    self.end_headers()
                    self.wfile.write(download_script.encode("utf-8"))
                    return
                code = preflight if self.path.startswith("/v1/pair/") else 503
                self.send_response(code)
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(b'{"error":"fixture download unavailable"}')

            def do_POST(self):
                reports.append(json.loads(self.rfile.read(int(self.headers["Content-Length"])).decode("utf-8")))
                self.send_response(204)
                self.end_headers()

        web = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Web)
        thread = threading.Thread(target=web.serve_forever, daemon=True)
        thread.start()
        try:
            with tempfile.TemporaryDirectory(prefix="attempt-report-test-") as root:
                url = f"http://127.0.0.1:{web.server_port}"
                # 999 forces the fixture download failure even if a developer
                # has attempt on PATH. No init, hook or service step can run.
                env = dict(os.environ, HOME=root, USERPROFILE=root, LOCALAPPDATA=root,
                           ATTEMPTDB_DATA_DIR=root + "/data", ATTEMPTDB_BIN_DIR=root + "/bin",
                           ATTEMPTDB_VERSION="999.0.0", ATTEMPTDB_INSTALLER=url + "/download.ps1")
                credential_args = ["-ApiKey", api_key] if api_key is not None else ["-Pair", token]
                cmd = [POWERSHELL, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", str(SCRIPT),
                       *credential_args, "-Server", url, "-Web", url]
                if no_report:
                    cmd.append("-NoReport")
                result = subprocess.run(cmd, env=env, capture_output=True, timeout=40)
                self.assertFalse((Path(root) / ".claude").exists())
                if api_key is not None:
                    self.assertNotIn(api_key.encode(), result.stdout + result.stderr)
                return result.returncode, reports
        finally:
            web.shutdown()
            web.server_close()
            thread.join(timeout=5)

    def test_unhandled_download_exception_reports_the_stage_once(self):
        code, reports = self.invoke()
        self.assertNotEqual(code, 0)
        self.assertEqual(len(reports), 1)
        self.assertEqual((reports[0]["ok"], reports[0]["step"]), (False, "binary"))
        self.assertTrue(reports[0]["error"])
        self.assertTrue(reports[0]["unattended"])
        self.assertEqual(reports[0]["installer_version"], "0.2.9+install.1")

    def test_explicit_failure_is_not_reported_twice_by_the_trap(self):
        code, reports = self.invoke(preflight=410)
        self.assertNotEqual(code, 0)
        self.assertEqual(len(reports), 1)
        self.assertEqual((reports[0]["ok"], reports[0]["step"]), (False, "pair"))

    def test_no_report_is_respected_on_unhandled_failure(self):
        code, reports = self.invoke(no_report=True)
        self.assertNotEqual(code, 0)
        self.assertEqual(reports, [])

    def test_non_ascii_exception_and_large_escaped_log_are_valid_utf8_json(self):
        script = 'Write-Host (([string][char]0xAC00 + \'"\\\') * 3000); throw ("fixture " + [char]0xAC00)'
        code, reports = self.invoke(download_script=script)
        self.assertNotEqual(code, 0)
        self.assertEqual(len(reports), 1)
        self.assertIn("\uac00", reports[0]["error"])
        self.assertTrue(reports[0]["log_tail"])
        self.assertLessEqual(len(json.dumps(reports[0], ensure_ascii=False)), 8192)

    def test_error_and_log_never_contain_the_supplied_secret(self):
        code, reports = self.invoke(token="vbm_fixture_secret")
        self.assertNotEqual(code, 0)
        self.assertEqual(len(reports), 1)
        self.assertNotIn("vbm_fixture_secret", reports[0]["error"])
        self.assertNotIn("vbm_fixture_secret", reports[0]["log_tail"])

    def test_foreign_api_key_is_not_echoed_or_uploaded(self):
        secret = "foreign-service-fixture-credential-123456"
        code, reports = self.invoke(api_key=secret)
        self.assertNotEqual(code, 0)
        self.assertEqual(len(reports), 1)
        self.assertEqual((reports[0]["ok"], reports[0]["step"]), (False, "pair"))
        self.assertEqual(reports[0]["api_key"], "")
        self.assertNotIn(secret, json.dumps(reports[0]))
        self.assertIn("vibemon.dev/devices", reports[0]["error"])

    def test_foreign_pairing_input_is_redacted_from_report_and_transcript(self):
        secret = "foreign-service-fixture-pairing-123456"
        code, reports = self.invoke(token=secret)
        self.assertNotEqual(code, 0)
        self.assertEqual(len(reports), 1)
        self.assertNotIn(secret, json.dumps(reports[0]))


if __name__ == "__main__":
    unittest.main()
