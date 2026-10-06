"""install.ps1 checks that need no PowerShell, so every platform runs them.

PowerShell itself (parse check, the PATH helpers, the consent logic with a fake
`attempt`) is tests/installers/test_install_ps1.ps1 on Windows CI. These pin the
properties that can be read off the text:

  * the file is pure ASCII: `irm | iex` on Windows PowerShell 5.1 can decode a
    UTF-8 download as Latin-1, and a stray typographic quote inside a string
    would change what runs;
  * nothing but function definitions, the `param` block and the final call sit at
    the top level, so a truncated download executes nothing (and strict mode and
    `$ErrorActionPreference` stay inside the installer's own scope rather than
    leaking into the caller's session, which `iex` runs in);
  * the PowerShell and shell installers speak the same environment variables.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PS1 = ROOT / "install.ps1"
SH = ROOT / "install.sh"

SHARED_VARIABLES = (
    "ATTEMPTDB_VERSION",
    "ATTEMPTDB_BIN_DIR",
    "ATTEMPTDB_NO_SETUP",
    "ATTEMPTDB_ASSUME_YES",
    "ATTEMPTDB_MODIFY_PATH",
    "ATTEMPTDB_NO_MODIFY_PATH",
    "ATTEMPTDB_VERIFY_ATTESTATION",
    "ATTEMPTDB_INSECURE_SKIP_CHECKSUM",
)


class InstallPs1StaticTests(unittest.TestCase):
    def setUp(self):
        self.text = PS1.read_text(encoding="utf-8")
        self.lines = self.text.split("\n")

    def test_the_file_is_pure_ascii(self):
        offenders = [(i, line) for i, line in enumerate(self.lines, 1) if not line.isascii()]
        self.assertEqual(offenders, [])

    def test_the_last_statement_is_the_call_and_nothing_else_runs_at_the_top_level(self):
        body = self.text.rstrip("\n").split("\n")
        self.assertRegex(body[-1], r"^Install-AttemptDb -AssumeYes \(\$Yes\.IsPresent\)$")
        in_function = False
        for number, line in enumerate(body[:-1], 1):
            if in_function:
                in_function = line != "}"
                continue
            if re.match(r"^function [A-Za-z-]+ \{$", line):
                in_function = True
                continue
            if line == "" or line.startswith("#") or line == "param([switch]$Yes)":
                continue
            self.fail(f"install.ps1:{number}: a top-level statement runs before the final call: {line!r}")

    def test_strict_mode_and_error_preference_are_set_inside_the_installer_only(self):
        for number, line in enumerate(self.lines, 1):
            if re.match(r"^(Set-StrictMode|\$ErrorActionPreference)", line):
                self.fail(f"install.ps1:{number}: leaks into the caller's session: {line!r}")
        self.assertIn("    Set-StrictMode -Version Latest", self.text)

    def test_the_user_path_is_never_written_through_the_flattening_api(self):
        # [Environment]::SetEnvironmentVariable('Path', ...) expands %VARIABLES%
        # and stores REG_SZ; the registry is written with the value's own type.
        self.assertNotRegex(self.text, r"SetEnvironmentVariable\('Path'")
        self.assertIn("DoNotExpandEnvironmentNames", self.text)
        self.assertRegex(self.text, r"SetValue\('Path', \$new, \$current\.Kind\)")

    def test_it_asks_before_it_applies_and_before_it_edits_path(self):
        self.assertIn("Apply these changes?", self.text)
        self.assertIn("Add it to your user PATH?", self.text)
        self.assertIn("ATTEMPTDB_MODIFY_PATH", self.text)

    def test_both_installers_document_and_read_the_same_variables(self):
        sh = SH.read_text()
        for name in SHARED_VARIABLES:
            with self.subTest(variable=name):
                self.assertIn(name, self.text)
                self.assertIn(name, sh)


if __name__ == "__main__":
    unittest.main()
