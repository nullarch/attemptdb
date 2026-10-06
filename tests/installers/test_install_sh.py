"""install.sh regressions with a stubbed release; no network or host changes.

The stubbed `curl` serves a release archive built on the fly, so the script's
download, checksum and install steps run for real against a fake `attempt`
that records how it was called. What is being pinned: after installing the
binary the script hands over to `attempt setup` exactly once, forwards its
own arguments to it, and ATTEMPTDB_NO_SETUP=1 keeps the older behaviour of
touching nothing but the binary.
"""
import hashlib
import io
import json
import os
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[2] / "install.sh"
VERSION = "9.9.9"

FAKE_ATTEMPT = """#!/bin/sh
if [ "$1" = "setup" ] && [ -n "$OLD_BINARY" ]; then
  echo "error: unrecognized subcommand 'setup'" >&2
  exit 2
fi
[ "$1 $2" = "setup --help" ] && exit 0
printf '%s\\n' "attempt $*" >> "$CALLS"
if [ "$1" = "setup" ]; then
  echo "attempt setup 9.9.9"
  exit "${SETUP_EXIT:-0}"
fi
echo "attempt 9.9.9"
"""

CURL_STUB = r'''#!/usr/bin/env python3
import os, shutil, sys
args = sys.argv[1:]
url = next(a for a in args if a.startswith("https://"))
out = args[args.index("-o") + 1] if "-o" in args else None
release = os.environ["FAKE_RELEASE"]
if url.endswith("/releases/latest"):
    print('{"tag_name": "v9.9.9"}')
elif url.endswith("SHA256SUMS"):
    src = os.path.join(release, "SHA256SUMS")
    text = open(src).read()
    if os.environ.get("TAMPER"):
        text = "0" * 64 + text[64:]
    if out:
        open(out, "w").write(text)
    else:
        print(text, end="")
elif url.endswith(".tar.gz"):
    src = os.path.join(release, os.path.basename(url))
    if not os.path.exists(src):
        sys.exit(22)
    shutil.copy(src, out)
else:
    sys.exit(22)
'''


def target():
    os_name = os.uname().sysname
    arch = os.uname().machine
    arch = {"x86_64": "x86_64", "amd64": "x86_64", "arm64": "aarch64", "aarch64": "aarch64"}[arch]
    if os_name == "Darwin":
        return f"{arch}-apple-darwin"
    return f"{arch}-unknown-linux-musl"


class InstallShTests(unittest.TestCase):
    def run_install(self, args=(), **settings):
        with tempfile.TemporaryDirectory(prefix="attempt-install-sh-test-") as temp:
            root = Path(temp)
            release = root / "release"
            release.mkdir()
            stem = f"attempt-{VERSION}-{target()}"
            archive = release / f"{stem}.tar.gz"
            with tarfile.open(archive, "w:gz") as tar:
                for name, body in (("attempt", FAKE_ATTEMPT), ("attempt-hook", FAKE_ATTEMPT)):
                    data = body.encode()
                    info = tarfile.TarInfo(f"{stem}/{name}")
                    info.size = len(data)
                    info.mode = 0o755
                    tar.addfile(info, io.BytesIO(data))
            digest = hashlib.sha256(archive.read_bytes()).hexdigest()
            (release / "SHA256SUMS").write_text(f"{digest}  {archive.name}\n")
            stubs = root / "stubs"
            stubs.mkdir()
            curl = stubs / "curl"
            curl.write_text(CURL_STUB.replace("#!/usr/bin/env python3", "#!" + sys.executable))
            curl.chmod(0o755)
            bin_dir = root / "bin"
            calls = root / "calls"
            env = {**os.environ, "HOME": temp, "PATH": f"{stubs}:/usr/bin:/bin",
                   "ATTEMPTDB_BIN_DIR": str(bin_dir), "FAKE_RELEASE": str(release),
                   "CALLS": str(calls), **settings}
            env.pop("ATTEMPTDB_VERSION", None)
            result = subprocess.run(["/bin/sh", str(SCRIPT), *args], env=env, capture_output=True, text=True)
            recorded = calls.read_text().splitlines() if calls.exists() else []
            # The directory is gone once this block exits; decide here.
            installed = {name: (bin_dir / name).exists() for name in ("attempt", "attempt-hook")}
            return result, recorded, installed

    def test_install_then_setup_once_with_forwarded_arguments(self):
        result, calls, installed = self.run_install(["--capture-mode", "metadata_only"])
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue(installed["attempt"])
        self.assertTrue(installed["attempt-hook"])
        self.assertEqual(calls, ["attempt setup --source install.sh --capture-mode metadata_only"])
        self.assertIn("Checksum verified.", result.stdout)
        self.assertNotIn("attempt init", result.stdout)

    def test_no_setup_installs_the_binary_only(self):
        result, calls, installed = self.run_install(ATTEMPTDB_NO_SETUP="1")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue(installed["attempt"])
        self.assertEqual(calls, [])
        self.assertIn("attempt setup", result.stdout)

    def test_a_failed_setup_is_the_scripts_exit_code(self):
        result, calls, installed = self.run_install(SETUP_EXIT="1")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertTrue(installed["attempt"], "the binary stays installed")
        self.assertEqual(len(calls), 1)

    def test_a_release_without_setup_gets_the_manual_steps(self):
        # An older pinned release (or main ahead of the newest release): the
        # binary installs, and the script says how to wire it instead of failing.
        result, calls, installed = self.run_install(OLD_BINARY="1")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue(installed["attempt"])
        self.assertEqual(calls, [])
        self.assertIn("predates `attempt setup`", result.stdout)
        self.assertIn("attempt init && attempt hook install", result.stdout)

    def test_checksum_mismatch_refuses_to_install(self):
        result, calls, installed = self.run_install(TAMPER="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum mismatch", result.stderr)
        self.assertFalse(installed["attempt"], "nothing may be installed")
        self.assertEqual(calls, [])


if __name__ == "__main__":
    unittest.main()
