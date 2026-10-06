"""install.sh regressions with a stubbed release; no network or host changes.

The stubbed `curl` serves a release archive built on the fly, so the script's
download, checksum and install steps run for real against a fake `attempt`
that records how it was called (see install_sh_harness.py). What is pinned:

  * the binary is installed and verified, and nothing else happens unless the
    person agreed: `--yes`, ATTEMPTDB_ASSUME_YES=1, or "Y" at the prompt
    (the prompt itself is test_install_sh_interactive.py);
  * with no terminal and no `--yes` the machine is left alone and the script
    prints the exact command that would wire it;
  * a download cut off anywhere executes nothing.
"""
import os
import re
import unittest
from concurrent.futures import ThreadPoolExecutor
from unittest import mock

import install_sh_harness as h
from install_sh_harness import SCRIPT, SHELLS, Env


class FlowTests(unittest.TestCase):
    def test_no_terminal_and_no_yes_installs_the_binary_and_wires_nothing(self):
        # The agent / CI case: nobody can be asked, so nothing may be changed.
        for shell in SHELLS:
            with self.subTest(shell=shell):
                env = Env(self)
                result = env.run(shell=shell)
                self.assertEqual(result.returncode, 0, result.output)
                self.assertTrue(env.installed()["attempt"])
                self.assertTrue(env.installed()["attempt-hook"])
                # Only the read-only preview ran: `setup` without --dry-run never did.
                self.assertEqual(env.calls(), ["attempt setup --dry-run --source install.sh"])
                self.assertIn("Checksum verified.", result.stdout)
                self.assertIn("would install", result.stdout, "the preview is shown")
                self.assertIn("no terminal", result.stdout)
                self.assertIn("nothing was changed", result.stdout)
                # The exact commands that finish the job, copy-pasteable.
                self.assertIn(f"{env.bin_dir}/attempt setup".replace(str(env.home), "~"), result.stdout)
                self.assertIn(
                    "curl -fsSL https://raw.githubusercontent.com/nullarch/attemptdb/main/install.sh"
                    " | sh -s -- --yes",
                    result.stdout,
                )

    def test_yes_applies_setup_once_and_forwards_the_other_arguments(self):
        for flag in ("--yes", "-y"):
            with self.subTest(flag=flag):
                env = Env(self)
                result = env.run(["--capture-mode", "metadata_only", flag, "--no-daemon"])
                self.assertEqual(result.returncode, 0, result.output)
                self.assertTrue(env.installed()["attempt"])
                self.assertEqual(
                    env.calls(),
                    ["attempt setup --source install.sh --capture-mode metadata_only --no-daemon"],
                    "--yes is the installer's own: `attempt setup` never sees it",
                )
                self.assertNotIn("attempt init", result.stdout)
                self.assertNotIn("Apply these changes", result.output)

    def test_assume_yes_environment_variable_is_the_same_as_the_flag(self):
        env = Env(self)
        result = env.run(["--no-verify"], ATTEMPTDB_ASSUME_YES="1")
        self.assertEqual(result.returncode, 0, result.output)
        self.assertEqual(env.calls(), ["attempt setup --source install.sh --no-verify"])

    def test_assume_yes_only_means_yes_when_it_is_one(self):
        env = Env(self)
        result = env.run(ATTEMPTDB_ASSUME_YES="0")
        self.assertEqual(result.returncode, 0, result.output)
        self.assertEqual(env.calls(), ["attempt setup --dry-run --source install.sh"])

    def test_no_setup_installs_the_binary_only_even_with_yes(self):
        for extra in ({}, {"ATTEMPTDB_ASSUME_YES": "1"}):
            with self.subTest(extra=extra):
                env = Env(self)
                result = env.run(["--yes"], ATTEMPTDB_NO_SETUP="1", **extra)
                self.assertEqual(result.returncode, 0, result.output)
                self.assertTrue(env.installed()["attempt"])
                self.assertEqual(env.calls(), [], "not even the preview")
                self.assertIn("attempt setup --dry-run", result.stdout)
                self.assertIn("attempt setup ", result.stdout)

    def test_a_dry_run_argument_just_runs_it_without_asking(self):
        for shell in SHELLS:
            with self.subTest(shell=shell):
                env = Env(self)
                result = env.run(["--dry-run", "--capture-mode", "metadata_only"], shell=shell)
                self.assertEqual(result.returncode, 0, result.output)
                self.assertEqual(
                    env.calls(),
                    ["attempt setup --source install.sh --dry-run --capture-mode metadata_only"],
                    "once, as given: no second preview, no apply",
                )
                self.assertNotIn("Apply these changes", result.output)
                self.assertNotIn("nothing was changed", result.stdout)

    def test_a_failed_setup_is_the_scripts_exit_code(self):
        env = Env(self)
        result = env.run(["--yes"], SETUP_EXIT="1")
        self.assertEqual(result.returncode, 1, result.output)
        self.assertTrue(env.installed()["attempt"], "the binary stays installed")
        self.assertEqual(len(env.calls()), 1)

    def test_setup_refusing_its_arguments_is_surfaced_and_applies_nothing(self):
        # `attempt` exits 2 for a bad flag; that must not be asked about or hidden.
        env = Env(self)
        result = env.run(["--capture-mode", "bogus"], DRYRUN_EXIT="2")
        self.assertEqual(result.returncode, 2, result.output)
        self.assertEqual(env.calls(), ["attempt setup --dry-run --source install.sh --capture-mode bogus"])

    def test_a_preview_with_problems_in_it_is_still_shown_and_still_not_applied(self):
        # Exit 1 is a report with problems, not a crash: the person judges it.
        env = Env(self)
        result = env.run(DRYRUN_EXIT="1")
        self.assertEqual(result.returncode, 0, result.output)
        self.assertEqual(env.calls(), ["attempt setup --dry-run --source install.sh"])

    def test_arguments_with_spaces_survive_into_the_printed_command(self):
        env = Env(self)
        result = env.run(["--source", "my laptop", "--provider", "o'neil"])
        self.assertEqual(result.returncode, 0, result.output)
        self.assertEqual(
            env.calls(), ["attempt setup --dry-run --source install.sh --source my laptop --provider o'neil"])
        self.assertIn("setup --source 'my laptop' --provider 'o'\\''neil'", result.stdout)
        self.assertIn("sh -s -- --yes --source 'my laptop' --provider 'o'\\''neil'", result.stdout)

    def test_a_pinned_version_is_carried_into_the_rerun_command(self):
        env = Env(self)
        result = env.run(ATTEMPTDB_VERSION="9.9.9")
        self.assertEqual(result.returncode, 0, result.output)
        self.assertIn("| ATTEMPTDB_VERSION=9.9.9 sh -s -- --yes", result.stdout)
        self.assertNotIn("Resolving the latest release", result.stdout)

    def test_help_prints_usage_and_installs_nothing(self):
        env = Env(self)
        result = env.run(["--help"])
        self.assertEqual(result.returncode, 0, result.output)
        self.assertIn("--yes", result.stdout)
        self.assertFalse(env.bin_dir.exists())
        self.assertEqual(env.curl_calls(), [])

    def test_a_release_without_setup_gets_the_manual_steps(self):
        # An older pinned release (or main ahead of the newest release): the
        # binary installs, and the script says how to wire it instead of failing.
        env = Env(self)
        result = env.run(["--yes"], OLD_BINARY="1")
        self.assertEqual(result.returncode, 0, result.output)
        self.assertTrue(env.installed()["attempt"])
        self.assertEqual(env.calls(), [])
        self.assertIn("predates `attempt setup`", result.stdout)
        self.assertRegex(
            result.stdout,
            r"(?m)^  \S*attempt init\n  \S*attempt hook install\n  \S*attempt daemon install\n  \S*attempt doctor$")
        self.assertNotIn("setup --dry-run", result.stdout)

    def test_checksum_mismatch_refuses_to_install(self):
        env = Env(self)
        result = env.run(["--yes"], TAMPER="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum mismatch", result.stderr)
        self.assertFalse(env.installed()["attempt"], "nothing may be installed")
        self.assertEqual(env.calls(), [])

    def test_wget_is_used_when_there_is_no_curl(self):
        env = Env(self, wget_only=True)
        result = env.run(["--yes"])
        self.assertEqual(result.returncode, 0, result.output)
        self.assertTrue(env.installed()["attempt"])
        self.assertTrue(all(call.startswith("wget ") for call in env.curl_calls()), env.curl_calls())

    def test_a_missing_downloader_is_an_error(self):
        env = Env(self)
        (env.stubs / "curl").unlink()
        result = env.run(["--yes"])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("neither curl nor wget", result.stderr)

    def test_the_latest_release_is_resolved_when_no_version_is_given(self):
        env = Env(self)
        result = env.run(["--yes"])
        self.assertIn("Resolving the latest release", result.stdout)
        # From the release page's redirect, not the rate-limited API.
        self.assertIn("https://github.com/nullarch/attemptdb/releases/latest", env.curl_calls())
        self.assertNotIn("https://api.github.com/repos/nullarch/attemptdb/releases/latest", env.curl_calls())
        self.assertIn(
            "https://github.com/nullarch/attemptdb/releases/download/v9.9.9/"
            f"attempt-9.9.9-{h.target_for(os.uname().sysname)}.tar.gz",
            env.curl_calls())


API = "https://api.github.com/repos/nullarch/attemptdb/releases/latest"
PAGE = "https://github.com/nullarch/attemptdb/releases/latest"


class ReleaseLookupTests(unittest.TestCase):
    """Finding the newest release must not depend on GitHub's anonymous API
    (60 requests an hour per address) and must say what failed when it fails."""

    def test_either_spelling_of_the_location_header_is_read(self):
        for header in ("location", "Location"):
            for shell in SHELLS:
                with self.subTest(header=header, shell=shell):
                    env = Env(self)
                    result = env.run(["--yes"], shell=shell, LOCATION_HEADER=header)
                    self.assertEqual(result.returncode, 0, result.output)
                    self.assertIn(PAGE, env.curl_calls())
                    self.assertNotIn(API, env.curl_calls())
                    self.assertIn("Installed attempt 9.9.9", result.stdout)

    def test_a_page_answer_without_a_location_falls_back_to_the_api(self):
        env = Env(self)
        result = env.run(["--yes"], NO_LOCATION="1")
        self.assertEqual(result.returncode, 0, result.output)
        self.assertEqual(env.curl_calls()[:2], [PAGE, API])

    def test_a_failing_page_lookup_falls_back_to_the_api(self):
        env = Env(self)
        result = env.run(["--yes"], CURL_FAIL_REDIRECT="7")
        self.assertEqual(result.returncode, 0, result.output)
        self.assertIn(API, env.curl_calls())

    def test_both_lookups_failing_names_each_error_and_the_rate_limit(self):
        env = Env(self)
        result = env.run(["--yes"], CURL_FAIL_REDIRECT="7", CURL_FAIL_API="22:403")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("could not resolve the latest release", result.stderr)
        self.assertIn("Failed to connect", result.stderr, "the release page's error")
        self.assertIn("returned error: 403", result.stderr, "the API's error")
        self.assertIn("60 requests an hour", result.stderr)
        self.assertIn("ATTEMPTDB_VERSION=<version>", result.stderr)
        self.assertNotIn("Is one published yet", result.stderr, "a rate limit is not a missing release")
        self.assertFalse(env.installed()["attempt"])

    def test_a_repository_without_releases_still_asks_whether_one_is_published(self):
        env = Env(self)
        result = env.run(["--yes"], CURL_FAIL_REDIRECT="22:404", CURL_FAIL_API="22:404")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Is one published yet?", result.stderr)
        self.assertIn("cargo install", result.stderr)

    def test_wget_reads_the_redirect_as_well(self):
        env = Env(self, wget_only=True)
        result = env.run(["--yes"])
        self.assertEqual(result.returncode, 0, result.output)
        self.assertIn("wget " + PAGE, env.curl_calls())
        self.assertNotIn("wget " + API, env.curl_calls())

    def test_a_wget_that_cannot_show_headers_falls_back_to_the_api(self):
        env = Env(self, wget_only=True)
        result = env.run(["--yes"], WGET_NO_SPIDER="1")
        self.assertEqual(result.returncode, 0, result.output)
        self.assertIn("wget " + API, env.curl_calls())

    def test_a_tag_that_is_not_a_version_is_refused(self):
        env = Env(self)
        result = env.run(["--yes"], ATTEMPTDB_VERSION="9.9.9/../../x")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unexpected release version", result.stderr)
        self.assertEqual(env.curl_calls(), [])


class DownloadErrorTests(unittest.TestCase):
    """A download that failed is not an asset that was never published."""

    def test_network_errors_on_the_archive_say_could_not_download_with_the_url_and_curls_error(self):
        for status, words in (("6", "Could not resolve host"), ("7", "Failed to connect"),
                              ("18", "transfer closed"), ("28", "timed out")):
            with self.subTest(curl_exit=status):
                env = Env(self)
                result = env.run(["--yes"], ATTEMPTDB_VERSION="9.9.9", CURL_FAIL_TARBALL=status)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("could not download https://github.com/nullarch/attemptdb/releases/download/v9.9.9/attempt-9.9.9-",
                              result.stderr)
                self.assertIn(f"curl: ({status})", result.stderr)
                self.assertIn(words, result.stderr)
                self.assertNotIn("no release asset", result.stderr)
                self.assertFalse(env.installed()["attempt"])

    def test_a_404_on_the_archive_still_says_no_release_asset_and_names_the_url(self):
        env = Env(self)
        result = env.run(["--yes"], ATTEMPTDB_VERSION="9.9.9", CURL_FAIL_TARBALL="22:404")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no release asset for", result.stderr)
        self.assertIn("v9.9.9", result.stderr)
        self.assertIn(".tar.gz: not found, HTTP 404", result.stderr)
        self.assertNotIn("could not download", result.stderr)

    def test_a_403_on_the_archive_is_a_failed_download_not_a_missing_asset(self):
        env = Env(self)
        result = env.run(["--yes"], ATTEMPTDB_VERSION="9.9.9", CURL_FAIL_TARBALL="22:403")
        self.assertIn("could not download", result.stderr)
        self.assertIn("returned error: 403", result.stderr)
        self.assertNotIn("no release asset", result.stderr)

    def test_a_failed_checksum_fetch_shows_the_error_instead_of_not_published(self):
        env = Env(self)
        result = env.run(["--yes"], ATTEMPTDB_VERSION="9.9.9", CURL_FAIL_SUMS="6")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("could not download https://github.com/nullarch/attemptdb/releases/download/v9.9.9/SHA256SUMS",
                      result.stderr)
        self.assertIn("Could not resolve host", result.stderr)
        self.assertNotIn("is not published", result.stderr)
        self.assertIn("Refusing to install", result.stderr)
        self.assertFalse(env.installed()["attempt"], "an unverifiable download is never installed")

    def test_a_404_on_the_checksums_is_the_one_that_says_not_published(self):
        env = Env(self)
        result = env.run(["--yes"], ATTEMPTDB_VERSION="9.9.9", CURL_FAIL_SUMS="22:404")
        self.assertIn("SHA256SUMS is not published for v9.9.9", result.stderr)
        self.assertFalse(env.installed()["attempt"])

    def test_wget_failures_are_told_apart_by_status(self):
        env = Env(self, wget_only=True)
        result = env.run(["--yes"], ATTEMPTDB_VERSION="9.9.9", WGET_FAIL_TARBALL="4")
        self.assertIn("could not download", result.stderr)
        self.assertIn("wget exited with status 4", result.stderr)
        self.assertNotIn("no release asset", result.stderr)
        env = Env(self, wget_only=True)
        result = env.run(["--yes"], ATTEMPTDB_VERSION="9.9.9", WGET_FAIL_TARBALL="8")
        self.assertIn("no release asset for", result.stderr)
        env = Env(self, wget_only=True)
        result = env.run(["--yes"], ATTEMPTDB_VERSION="9.9.9", WGET_FAIL_SUMS="4")
        self.assertIn("could not download", result.stderr)
        self.assertNotIn("is not published", result.stderr)


class OldReleaseTests(unittest.TestCase):
    """A release that predates `attempt setup` must never be told to run it."""

    def test_the_binary_only_path_prints_commands_that_exist_in_that_release(self):
        for shell in SHELLS:
            for extra in ({"ATTEMPTDB_NO_SETUP": "1"}, {}):
                with self.subTest(shell=shell, extra=extra):
                    env = Env(self)
                    result = env.run([], shell=shell, OLD_BINARY="1", **extra)
                    self.assertEqual(result.returncode, 0, result.output)
                    self.assertTrue(env.installed()["attempt"])
                    self.assertEqual(env.calls(), [], "nothing is run on the person's behalf")
                    out = result.stdout
                    self.assertIn("predates `attempt setup`", out)
                    for step in ("attempt init", "attempt hook install", "attempt daemon install", "attempt doctor"):
                        self.assertIn(step, out)
                    self.assertNotIn("setup --dry-run", out)
                    self.assertNotIn("attempt setup", out.replace("predates `attempt setup`", ""))

    def test_capture_mode_and_no_daemon_are_carried_over(self):
        env = Env(self)
        result = env.run(["--capture-mode", "metadata_only", "--no-daemon"], OLD_BINARY="1")
        self.assertEqual(result.returncode, 0, result.output)
        self.assertRegex(result.stdout, r"attempt init --capture-mode metadata_only\n")
        self.assertNotIn("daemon install", result.stdout)
        env = Env(self)
        result = env.run(["--capture-mode=full_sync"], OLD_BINARY="1")
        self.assertRegex(result.stdout, r"attempt init --capture-mode full_sync\n")
        self.assertIn("daemon install", result.stdout)


class HomeUnsetTests(unittest.TestCase):
    """HOME can be missing (a minimal container, `env -i`, cron)."""

    def test_an_explicit_install_directory_does_not_need_home(self):
        for shell in SHELLS:
            with self.subTest(shell=shell):
                env = Env(self)
                result = env.run(["--yes"], shell=shell, HOME=None)
                self.assertEqual(result.returncode, 0, result.output)
                self.assertNotIn("unbound variable", result.output)
                self.assertTrue(env.installed()["attempt"])
                # No profile to edit without a home: the hint names the directory.
                self.assertIn("is not on your PATH", result.stdout)
                self.assertIn(str(env.bin_dir), result.stdout)

    def test_no_home_and_no_install_directory_is_a_clear_error(self):
        for shell in SHELLS:
            with self.subTest(shell=shell):
                env = Env(self)
                result = env.run(["--yes"], shell=shell, HOME=None, ATTEMPTDB_BIN_DIR=None)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn("unbound variable", result.output)
                self.assertIn("HOME is not set", result.stderr)
                self.assertIn("ATTEMPTDB_BIN_DIR", result.stderr)
                self.assertEqual(env.curl_calls(), [], "nothing was downloaded")

    def test_an_empty_home_is_handled_like_an_unset_one(self):
        env = Env(self)
        result = env.run(["--yes"], HOME="", ATTEMPTDB_BIN_DIR=None)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("HOME is not set", result.stderr)
        env = Env(self)
        result = env.run(["--yes"], HOME="")
        self.assertEqual(result.returncode, 0, result.output)
        self.assertIn(f"{env.bin_dir}/attempt", result.stdout.replace("~", str(env.home)))


class AgentConfigIsolationTests(unittest.TestCase):
    """The owner's shell may export CLAUDE_CONFIG_DIR and friends; a test machine never sees them."""

    SENTINELS = {name: f"/nonexistent/real-{name.lower()}" for name in h.AGENT_CONFIG_VARS}

    def test_the_agents_config_variables_never_reach_the_script_or_the_binary(self):
        with mock.patch.dict(os.environ, self.SENTINELS):
            env = Env(self)
            result = env.run(["--yes"], **self.SENTINELS)  # even passed in explicitly
            self.assertEqual(result.returncode, 0, result.output)
            seen = env.agent_env_seen()
            self.assertTrue(seen, "the fake attempt recorded what it saw")
            for name in h.AGENT_CONFIG_VARS:
                self.assertIn(f"{name}=unset", seen)
                self.assertNotIn(self.SENTINELS[name], "\n".join(seen))
            self.assertIn(f"HOME={env.home}", seen, "it saw the fake home")

    def test_a_terminal_run_is_scrubbed_too(self):
        with mock.patch.dict(os.environ, self.SENTINELS):
            env = Env(self)
            result = env.run_tty(["--yes"])
            self.assertEqual(result.returncode, 0, result.output)
            seen = env.agent_env_seen()
            self.assertTrue(seen)
            for name in h.AGENT_CONFIG_VARS:
                self.assertIn(f"{name}=unset", seen)

    def test_the_scrub_helper_removes_exactly_those_variables(self):
        cleaned = h.scrubbed_environ({**self.SENTINELS, "HOME": "/x", "PATH": "/y"})
        self.assertEqual(cleaned, {"HOME": "/x", "PATH": "/y"})


class QuarantineTests(unittest.TestCase):
    """curl and wget do not quarantine, so the script must not pretend they do."""

    def test_the_script_no_longer_claims_or_clears_a_quarantine(self):
        text = SCRIPT.read_text()
        self.assertNotIn("xattr", text)
        self.assertNotIn("quarantines anything", text)
        self.assertNotIn("what Homebrew does", text)

    def test_the_macos_install_runs_no_xattr(self):
        env = Env(self, fake_os="Darwin")
        env.add_recording_tool("xattr")
        result = env.run(["--yes"])
        self.assertEqual(result.returncode, 0, result.output)
        self.assertTrue(env.installed()["attempt"])
        self.assertEqual(env.tools(), [], "no xattr (or anything else recorded) was run")


class AttestationTests(unittest.TestCase):
    def test_off_by_default_even_when_gh_is_installed(self):
        env = Env(self)
        env.add_recording_tool("gh")
        result = env.run(["--yes"])
        self.assertEqual(result.returncode, 0, result.output)
        self.assertEqual(env.tools(), [])

    def test_without_gh_and_without_the_variable_nothing_is_said(self):
        env = Env(self)
        result = env.run(["--yes"])
        self.assertEqual(result.returncode, 0, result.output)
        self.assertNotIn("gh", result.output.replace("--", ""))
        self.assertNotIn("provenance", result.output)

    def test_a_verified_archive_installs(self):
        env = Env(self)
        env.add_recording_tool("gh")
        result = env.run(["--yes"], ATTEMPTDB_VERIFY_ATTESTATION="1")
        self.assertEqual(result.returncode, 0, result.output)
        self.assertTrue(env.installed()["attempt"])
        self.assertIn("Build provenance verified.", result.stdout)
        (line,) = env.tools()
        self.assertRegex(
            line,
            r"^gh attestation verify \S+/attempt-9\.9\.9-[a-z0-9_-]+\.tar\.gz --repo nullarch/attemptdb$",
        )

    def test_a_failed_verification_installs_nothing(self):
        env = Env(self)
        env.add_recording_tool("gh")
        result = env.run(["--yes"], ATTEMPTDB_VERIFY_ATTESTATION="1", GH_EXIT="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("provenance verification failed", result.stderr)
        self.assertFalse(env.installed()["attempt"])
        self.assertEqual(env.calls(), [])

    def test_asking_for_verification_without_gh_stops_instead_of_downgrading(self):
        env = Env(self)
        result = env.run(["--yes"], ATTEMPTDB_VERIFY_ATTESTATION="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("needs the GitHub CLI", result.stderr)
        self.assertFalse(env.installed()["attempt"])
        self.assertEqual(env.calls(), [])

    def test_the_checksum_is_still_checked_first(self):
        env = Env(self)
        env.add_recording_tool("gh")
        result = env.run(["--yes"], ATTEMPTDB_VERIFY_ATTESTATION="1", TAMPER="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum mismatch", result.stderr)
        self.assertEqual(env.tools(), [], "gh is not even asked about a tampered archive")


class TruncationTests(unittest.TestCase):
    """`curl | sh` runs what has arrived. A cut-off script must run nothing."""

    def cut_points(self, script):
        """Every line start, the middle of every line, and every byte near the end."""
        body = script.rstrip(b"\n")
        points = {0}
        offset = 0
        for line in script.split(b"\n"):
            points.add(offset)
            points.add(offset + len(line) // 2)
            points.add(offset + len(line))
            offset += len(line) + 1
        points.update(range(max(0, len(body) - 160), len(body)))
        # The whole script minus its final newline is a complete script.
        return sorted(p for p in points if 0 <= p < len(body))

    def test_the_last_line_of_the_script_is_the_call_to_main(self):
        lines = SCRIPT.read_text().rstrip("\n").split("\n")
        self.assertRegex(lines[-1], r'^main "\$@" ')
        # Everything outside a function body is a comment, `set`, or a plain
        # assignment: nothing that touches the machine runs before main.
        in_function = False
        for line in lines[:-1]:
            if in_function:
                in_function = line != "}"
                continue
            if re.match(r"^[a-z_]+\(\) \{$", line):
                in_function = True
                continue
            if re.match(r"^[a-z_]+\(\) \{.*\}$", line):  # one-line function
                continue
            self.assertRegex(
                line,
                r'^(|#.*|set -eu|[A-Z_]+="[^"]*")$',
                f"top-level statement that runs before main: {line!r}",
            )

    def test_every_prefix_of_the_script_leaves_the_machine_alone(self):
        script = SCRIPT.read_bytes()
        env = Env(self)
        before = env.tree()
        cuts = self.cut_points(script)
        self.assertGreater(len(cuts), 400)

        def attempt(n):
            result = env.run(["--yes"], script=script[:n], ATTEMPTDB_VERSION="9.9.9")
            return n, result

        with ThreadPoolExecutor(max_workers=8) as pool:
            results = list(pool.map(attempt, cuts))
        for n, result in results:
            where = f"cut at byte {n}: {script[max(0, n - 40):n]!r}"
            self.assertEqual(env.curl_calls(), [], where)
            self.assertEqual(env.calls(), [], where)
            self.assertEqual(env.tools(), [], where)
            self.assertNotIn("Installed attempt", result.stdout, where)
        self.assertEqual(env.tree(), before, "nothing was created or changed anywhere")
        self.assertFalse(env.bin_dir.exists())

    def test_the_whole_script_does_install_so_the_check_above_can_fail(self):
        env = Env(self)
        script = SCRIPT.read_bytes()
        result = env.run(["--yes"], script=script.rstrip(b"\n"), ATTEMPTDB_VERSION="9.9.9")
        self.assertEqual(result.returncode, 0, result.output)
        self.assertTrue(env.installed()["attempt"])

    def test_a_cut_right_after_main_does_not_run_without_its_arguments(self):
        # `main` alone is a complete command; without the end marker it must not
        # go on to install with the arguments it lost (here: --yes).
        script = SCRIPT.read_bytes()
        env = Env(self)
        stop = script.rindex(b'main "$@" ') + len(b"main")
        for n in (stop, stop + 1, stop + len(b' "$@"')):
            with self.subTest(prefix=script[stop - 4:n]):
                result = env.run(["--yes"], script=script[:n], ATTEMPTDB_VERSION="9.9.9")
                self.assertNotEqual(result.returncode, 0, result.output)
                self.assertIn("cut off", result.stderr)
                self.assertFalse(env.bin_dir.exists())
                self.assertEqual(env.curl_calls(), [])

    def test_the_end_marker_is_only_the_marker_when_it_is_last(self):
        env = Env(self)
        # The marker anywhere but last (a user cannot send it, an edited copy might).
        script = SCRIPT.read_bytes()
        edited = script.replace(b'main "$@" "$END_MARKER"', b'main "$END_MARKER" "$@"')
        result = env.run(["--yes"], script=edited, ATTEMPTDB_VERSION="9.9.9")
        self.assertNotEqual(result.returncode, 0, result.output)
        self.assertFalse(env.bin_dir.exists())


if __name__ == "__main__":
    unittest.main()
