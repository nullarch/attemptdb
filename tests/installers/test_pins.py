"""The installers' binary pin follows the workspace version.

The connect step passes `--profile messages`, which only 0.2.12+ binaries
know. On 2026-09-09 the 0.2.12 and 0.2.13 releases shipped installer scripts
that still downloaded 0.2.11, so every new install failed at pairing with
`unknown profile messages` (install report #86). A release that bumps
Cargo.toml must bump both scripts with it; this test is the reminder.
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def workspace_version() -> str:
    text = (ROOT / "Cargo.toml").read_text()
    match = re.search(r'^version = "([0-9]+\.[0-9]+\.[0-9]+)"', text, re.M)
    assert match, "workspace version not found in Cargo.toml"
    return match.group(1)


class PinTests(unittest.TestCase):
    def test_shell_installer_pins_the_workspace_version(self):
        text = (ROOT / "docs/migration/vibemon-install.sh").read_text()
        version = workspace_version()
        self.assertIn(f'ATTEMPTDB_VERSION="${{ATTEMPTDB_VERSION:-{version}}}"', text)
        self.assertRegex(text, rf'^INSTALLER_VERSION="{re.escape(version)}\+install\.[0-9]+"$')

    def test_powershell_installer_pins_the_workspace_version(self):
        text = (ROOT / "docs/migration/vibemon-install.ps1").read_text()
        version = workspace_version()
        self.assertIn(f'else {{ "{version}" }}', text)
        self.assertRegex(text, rf'^\$InstallerVersion = "{re.escape(version)}\+install\.[0-9]+"$')

    def test_messages_profile_needs_a_binary_that_knows_it(self):
        sh = (ROOT / "docs/migration/vibemon-install.sh").read_text()
        self.assertIn('PROFILE="messages"', sh)
        major, minor, patch = (int(n) for n in workspace_version().split("."))
        self.assertGreaterEqual((major, minor, patch), (0, 2, 12))


if __name__ == "__main__":
    unittest.main()
