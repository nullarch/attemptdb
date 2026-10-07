"""vibemon-install.ps1: properties of the failure diagnostics that can be read off the text.

PowerShell is not available on every runner, so the behaviour is covered by
test_reports.py where it is (Windows CI). These pin what must stay true of the
script itself, mirroring FailureDiagnosticsTests for the shell script:

  * an attended run keeps a transcript too (it kept none, so a failure on a
    person's own console reported nothing), and a dry run keeps none;
  * a transcript this script started is stopped on every way out - the
    attended command runs in the person's own session, where a transcript
    left running would keep recording what they type afterwards;
  * a failed service step ends the transcript with facts about the machine,
    the same marker the shell script writes, and no secret goes into them.
"""
import os
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
# VIBEMON_INSTALL_PS1 points the suite at another copy, to show that a new test
# fails on the script it was written against.
PS1 = Path(os.environ.get("VIBEMON_INSTALL_PS1") or ROOT / "docs/migration/vibemon-install.ps1")
SH = ROOT / "docs/migration/vibemon-install.sh"
MARKER = "--- environment (facts only; for support) ---"


def function_body(text, name):
    start = text.index(f"function {name} ")
    depth, i = 0, text.index("{", start)
    begin = i
    while True:
        if text[i] == "{":
            depth += 1
        elif text[i] == "}":
            depth -= 1
            if depth == 0:
                return text[begin:i + 1]
        i += 1


class VibemonPs1StaticTests(unittest.TestCase):
    def setUp(self):
        self.text = PS1.read_text(encoding="utf-8")

    def test_code_is_ascii(self):
        # Windows PowerShell 5.1 can decode a UTF-8 download as Latin-1: a stray
        # typographic character in code would change what runs. (Comments are
        # not code, and some predate this check.)
        offenders = [(n, l) for n, l in enumerate(self.text.split("\n"), 1)
                     if not l.isascii() and not l.lstrip().startswith("#")]
        self.assertEqual(offenders, [])

    def test_every_run_but_an_attended_dry_run_keeps_a_transcript(self):
        self.assertIn("if ($Unattended -or -not $DryRun) {", self.text)
        self.assertNotRegex(self.text, r"(?m)^if \(\$Unattended\) \{\s*$", "the transcript is not for unattended runs only")

    def test_a_transcript_is_only_stopped_if_this_script_started_it(self):
        starts = re.findall(r"Start-Transcript[^;]*;\s*\$script:TranscriptOn = \$true", self.text)
        self.assertEqual(len(starts), 2, "both the normal and the Git Bash fallback transcript record that they started")
        self.assertEqual(self.text.count("Stop-Transcript"), 1, "one place stops it")
        body = function_body(self.text, "Stop-InstallTranscript")
        self.assertIn("if ($script:TranscriptOn)", body)
        self.assertIn("Stop-Transcript", body)

    def test_it_is_stopped_on_every_way_out(self):
        send = function_body(self.text, "Send-Report")
        # -NoReport, -DryRun and "already reported" returned without stopping it.
        self.assertRegex(send, r"\$script:Reported\) \{ Stop-InstallTranscript; return \}")
        self.assertGreaterEqual(send.count("Stop-InstallTranscript"), 3)
        # Every exit path reports (Fail, the trap, the noop gate, the end).
        self.assertIn("Send-Report $false; Write-Error", self.text)
        self.assertRegex(self.text, r"(?s)trap \{.*?Send-Report \$false.*?exit 1")
        self.assertTrue(self.text.rstrip().endswith("Send-Report $true"))

    def test_the_report_keeps_the_end_of_the_transcript(self):
        self.assertIn("$tail.Substring($tail.Length - 4000)", self.text)
        self.assertIn("$report.log_tail.Substring([int][Math]::Ceiling($report.log_tail.Length / 2))", self.text)

    def test_a_failed_service_step_writes_facts_about_the_machine(self):
        body = function_body(self.text, "Write-Fingerprint")
        self.assertIn(MARKER, body)
        self.assertRegex(body, r'\$Step -ne "daemon" -and \$Step -ne "environment"')
        self.assertIn("-DryRun", self.text)
        self.assertIn("AttemptDB Sync", body)
        for secret_or_content in ("$Pair", "$ApiKey", "Get-Content", "api-key", "$env:USERNAME", "$HOME"):
            self.assertNotIn(secret_or_content, body)
        fail = re.search(r"function Fail \{[^\n]*\}", self.text).group(0)
        self.assertLess(fail.index("Write-Fingerprint"), fail.index("Send-Report"), "the facts come before the report reads the transcript")
        trap = re.search(r"(?s)\ntrap \{.*?\n\}", self.text).group(0)
        self.assertLess(trap.index("Write-Fingerprint"), trap.index("Send-Report"))

    def test_both_installers_write_the_same_marker(self):
        self.assertIn(MARKER, SH.read_text(encoding="utf-8"))
        self.assertIn(MARKER, PS1.read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
