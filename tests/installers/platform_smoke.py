"""Real release installers + real local server, only on disposable CI runners.

No production account, server, API key, webhook, or install report is used.
The only simulated component is the product web receiving installer reports.
The CLI, hooks, database, sync protocol and OS background service are real.
"""
import argparse
import http.server
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import threading
import time
import urllib.error
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[2]
WINDOWS = os.name == "nt"
RESULTS = ROOT / "install-smoke-results"
ADMIN = "fixture-admin-only-on-loopback"
TENANT = "installer_fixture"


def request(url, body=None, tenant=False):
    headers = {"Authorization": "Bearer " + ADMIN}
    if tenant:
        headers["X-AttemptDB-Tenant"] = TENANT
    if body is not None:
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=None if body is None else json.dumps(body).encode(), headers=headers)
    with urllib.request.urlopen(req, timeout=10) as response:
        return json.load(response)


def unused_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--server", required=True)
    args = parser.parse_args()
    if os.environ.get("GITHUB_ACTIONS") != "true":
        raise SystemExit("This test registers OS services; run only on a disposable GitHub runner.")
    RESULTS.mkdir(exist_ok=True)
    root = Path(os.environ["RUNNER_TEMP"]) / "attempt install fixture"
    root.mkdir(exist_ok=True)
    home = Path.home()  # The disposable runner account, never the developer's home.
    bin_dir = root / "bin"
    data = root / "client"
    exe = bin_dir / ("attempt.exe" if WINDOWS else "attempt")
    env = dict(os.environ, ATTEMPTDB_BIN_DIR=str(bin_dir), ATTEMPTDB_DATA_DIR=str(data),
               ATTEMPTDB_VERSION="0.2.8", ATTEMPTDB_NO_AUTO_UPDATE="1")
    env.pop("ATTEMPTDB_ADMIN_TOKEN", None)
    env.pop("ATTEMPTDB_WEBHOOK_URL", None)
    env.pop("ATTEMPTDB_WEBHOOK_SECRET", None)
    env["PATH"] = str(bin_dir) + os.pathsep + env["PATH"]
    reports = []
    outcomes = []
    server_url = "http://127.0.0.1:" + str(unused_port())

    class Web(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            if self.path != "/api/attemptdb/install-report":
                self.send_error(404)
                return
            try:
                raw = self.rfile.read(int(self.headers["Content-Length"]))
                value = json.loads(raw.decode("utf-8"))
                assert isinstance(value["ok"], bool) and isinstance(value["step"], str)
                assert len(raw.decode("utf-8")) <= 8192
                reports.append(value)
                self.send_response(204)
                self.end_headers()
            except Exception as error:
                reports.append({"invalid_report": str(error)})
                self.send_error(400)

    web = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Web)
    thread = threading.Thread(target=web.serve_forever, daemon=True)
    thread.start()
    web_url = "http://127.0.0.1:" + str(web.server_port)

    def run(cmd, name, *, child_env=None, payload=None, check=True, timeout=180):
        result = subprocess.run([str(x) for x in cmd], env=child_env or env, cwd=root,
                                input=payload, text=True, encoding="utf-8", errors="replace",
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=timeout)
        (RESULTS / (name + ".log")).write_text(result.stdout, encoding="utf-8")
        if check and result.returncode:
            raise AssertionError(f"{name}: exit {result.returncode}; see artifact log")
        return result

    def windows_diagnostics():
        if not WINDOWS:
            return
        script = """
$ErrorActionPreference = 'Continue'
whoami
Get-Process -Id $PID | Select-Object SessionId | ConvertTo-Json
Get-ScheduledTask -TaskName 'AttemptDB Sync' | Select-Object State,Actions,Principal,Settings,Triggers | ConvertTo-Json -Depth 6
Get-ScheduledTaskInfo -TaskName 'AttemptDB Sync' | ConvertTo-Json
Get-WinEvent -FilterHashtable @{LogName='Microsoft-Windows-TaskScheduler/Operational'; StartTime=(Get-Date).AddMinutes(-10)} -ErrorAction SilentlyContinue | Where-Object Message -Match 'AttemptDB' | Select-Object TimeCreated,Id,Message | ConvertTo-Json
"""
        run(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", script],
            "scheduled-task", check=False, timeout=30)

    def wait_for(predicate, message, seconds=150):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            try:
                value = predicate()
                if value:
                    return value
            except (urllib.error.URLError, OSError):
                pass
            time.sleep(3)
        raise AssertionError(message)

    def installation(token=None, native=False, child_env=None, name="install"):
        # Windows first enters through actual Git Bash, reproducing report #20.
        if WINDOWS and native:
            cmd = ["powershell.exe", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
                   "-File", ROOT / "docs/migration/vibemon-install.ps1", "-Web", web_url,
                   "-Server", server_url, "-Profile", "metadata_only"]
            if token:
                cmd += ["-Pair", token]
        else:
            bash = Path("C:/Program Files/Git/bin/bash.exe") if WINDOWS else Path("/bin/bash")
            script = str(ROOT / "docs/migration/vibemon-install.sh").replace("\\", "/")
            cmd = [bash, script, "--web", web_url, "--server", server_url, "--profile", "metadata_only"]
            if token:
                cmd += [token]
        return run(cmd, name, child_env=child_env, check=False)

    # Install discovery uses real agent config locations in this disposable VM.
    claude = home / ".claude"
    claude.mkdir(exist_ok=True)
    legacy = home / ".vibemon"
    legacy.mkdir(exist_ok=True)
    marker = "vibemon-legacy-fixture"
    old_command = str(legacy / "notify.sh").replace("\\", "/")
    settings = claude / "settings.json"
    if settings.exists():
        raise AssertionError("Expected a clean runner with no Claude settings")
    settings.write_text(json.dumps({"hooks": {"PostToolUse": [{"matcher": "", "hooks": [
        {"type": "command", "command": old_command + " " + marker}]}]}}), encoding="utf-8")
    (legacy / "notify.sh").write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    # The real server stores only synthetic events and never calls a webhook.
    keys = root / "keys.json"
    keys.write_text('{"keys":[]}', encoding="utf-8")
    server_log = (RESULTS / "server.log").open("w", encoding="utf-8")
    server = subprocess.Popen([str(Path(args.server).resolve()), "--port", server_url.rsplit(":", 1)[1],
                               "--data-dir", str(root / "server"), "--keys", str(keys),
                               "--admin-token", ADMIN], env=env, stdout=server_log, stderr=server_log)
    try:
        wait_for(lambda: request(server_url + "/v1/health"), "local server failed to start", 30)
        if not WINDOWS:
            # Real Linux without a user bus: skip before a token or device exists.
            (legacy / "api-key").write_text("vbm_fixture_not_a_real_account", encoding="utf-8")
            no_bus = dict(env, DBUS_SESSION_BUS_ADDRESS="unix:path=" + str(root / "missing-bus"),
                          XDG_RUNTIME_DIR=str(root / "no-runtime"))
            result = installation(child_env=no_bus, name="unsupported-linux")
            assert result.returncode == 0
            assert reports[-1]["step"] == "skipped_environment", reports
            assert "systemd" in reports[-1]["error"]
            assert marker in settings.read_text(encoding="utf-8")
            assert request(server_url + "/v1/admin/tenants")["tenants"] == []
            (legacy / "api-key").unlink()
            outcomes.append("No user bus: no pairing, no hook replacement, skip reason reported")

        pairing = request(server_url + "/v1/admin/pairings", {"tenant": TENANT, "label": "fixture", "ttl_secs": 600})
        before = len(reports)
        result = installation(pairing["token"], name="first-install")
        assert result.returncode == 0, "first install failed; inspect log artifacts"
        assert len(reports) == before + 1 and reports[-1].get("ok") and reports[-1].get("step") == "done", reports
        assert reports[-1]["log_tail"].strip(), "unattended install lost its log tail"
        assert marker not in settings.read_text(encoding="utf-8"), "legacy hooks not removed after upload"
        assert "0.2.8" in run([exe, "--version"], "version").stdout
        state = json.loads(run([exe, "sync", "status", "--json"], "sync-status").stdout)
        assert state["connected"]
        devices = request(server_url + "/v1/devices", tenant=True)["devices"]
        assert len(devices) == 1 and devices[0]["events"] > 0
        assert request(server_url + "/v1/live", tenant=True)["last_event"] is None, "capture tests counted as activity"
        outcomes.append("Fresh installer: published 0.2.8 checksummed binaries, pairing, hook tests, service and first upload")
        print("PASS: " + outcomes[-1], flush=True)

        # Disable optional auto-updates in the fixture so the task tests the
        # pinned binary, and has no reason to contact a release-policy server.
        config = data / "config/config.json"
        settings_value = json.loads(config.read_text(encoding="utf-8"))
        settings_value["auto_update"] = "off"
        config.write_text(json.dumps(settings_value), encoding="utf-8")

        def capture(label):
            session = "fixture-" + str(uuid.uuid4())
            payload = json.dumps({"_fixture_note": "Authored synthetic install smoke event",
                                  "hook_event_name": "UserPromptSubmit", "session_id": session,
                                  "cwd": str(root / "example/project"), "prompt": "synthetic fixture " + label})
            result = run([exe, "hook", "claude-code"], "capture-" + label, payload=payload)
            assert result.stdout == "", "hook must stay silent"
            return session

        def has_session(session):
            live = request(server_url + "/v1/live", tenant=True)
            last = live.get("last_event") or {}
            return last.get("kind") == "prompt_submitted" and any(
                s.get("provider_session_id") == session for s in
                request(server_url + "/v1/sessions", tenant=True).get("sessions", []))

        first = capture("first")
        # Deliberately do not run sync now/maintenance: the installed OS
        # background service must pick up both new events by itself.
        wait_for(lambda: has_session(first), "first real event never synced through the OS service")
        print("PASS: first automatic background upload", flush=True)
        first_sync = request(server_url + "/v1/devices", tenant=True)["devices"][0]["last_sync_at"]
        second = capture("second")
        wait_for(lambda: has_session(second), "follow-up event never synced through the OS service")
        assert request(server_url + "/v1/devices", tenant=True)["devices"][0]["last_sync_at"] != first_sync
        outcomes.append("Two distinct real-kind synthetic events synced automatically in separate cycles")

        before = len(reports)
        result = installation(native=WINDOWS, name="reinstall")
        assert result.returncode == 0
        assert len(reports) == before + 1 and reports[-1]["step"] == "done", reports
        assert len(request(server_url + "/v1/devices", tenant=True)["devices"]) == 1
        outcomes.append("Reinstall preserves the device and connection; no duplicate pairing")
        print("PASS: " + "; ".join(outcomes), flush=True)
    finally:
        windows_diagnostics()
        (RESULTS / "result.json").write_text(json.dumps({"platform": os.name, "checks": outcomes,
                                                        "reports": reports}, indent=2), encoding="utf-8")
        if exe.exists():
            run([exe, "daemon", "uninstall"], "cleanup-service", check=False, timeout=45)
            run([exe, "daemon", "stop"], "cleanup-daemon", check=False, timeout=30)
        for log in [legacy / "vibemon-install.log", legacy / "vibemon-install-powershell.log", data / "logs/daemon.log"]:
            if log.exists():
                shutil.copy2(log, RESULTS / log.name)
        server.terminate()
        try:
            server.wait(timeout=15)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait()
        server_log.close()
        web.shutdown()
        web.server_close()
        thread.join(timeout=5)


if __name__ == "__main__":
    main()
