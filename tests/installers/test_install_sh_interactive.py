"""install.sh with a person at the keyboard, and with the shell profile.

`curl | sh` has the script on stdin, so the questions it asks go to /dev/tty.
These tests give the script a real pseudo-terminal as its controlling terminal
(and keep stdin a pipe, as in life) and type the answers. What is pinned:

  * "Apply these changes? [Y/n]": Enter or y applies, n or Ctrl-D leaves the
    machine untouched and prints the exact command for later;
  * the question is asked even when stdout is redirected (`curl | sh > log`);
  * the shell-profile edit that puts the install directory on PATH happens only
    with consent (prompt, or ATTEMPTDB_MODIFY_PATH=1), only once, only for the
    login shell, and never through a symlink.
"""
import os
import subprocess
import unittest

from install_sh_harness import SHELLS, Env

APPLY = "Apply these changes? [Y/n]"
DRY_RUN_CALL = "attempt setup --dry-run --source install.sh"
SETUP_CALL = "attempt setup --source install.sh"
MARK = "# added by the AttemptDB installer"


class SetupPromptTests(unittest.TestCase):
    def test_enter_applies_the_default_is_yes(self):
        for shell in SHELLS:
            with self.subTest(shell=shell):
                env = Env(self)
                result = env.run_tty(shell=shell, answers=[(APPLY, b"\n")])
                self.assertEqual(result.returncode, 0, result.output)
                self.assertEqual(env.calls(), [DRY_RUN_CALL, SETUP_CALL])
                self.assertIn("would install", result.output, "the preview came first")
                self.assertTrue(env.installed()["attempt"])

    def test_yes_answers_apply(self):
        for answer in (b"y\n", b"Y\n", b"yes\n", b"YES\n"):
            with self.subTest(answer=answer):
                env = Env(self)
                result = env.run_tty(answers=[(APPLY, answer)])
                self.assertEqual(result.returncode, 0, result.output)
                self.assertEqual(env.calls(), [DRY_RUN_CALL, SETUP_CALL])

    def test_no_leaves_the_machine_untouched_and_says_how_to_apply_later(self):
        for shell in SHELLS:
            with self.subTest(shell=shell):
                env = Env(self)
                result = env.run_tty(shell=shell, answers=[(APPLY, b"n\n")])
                self.assertEqual(result.returncode, 0, result.output)
                self.assertEqual(env.calls(), [DRY_RUN_CALL], "the preview only: nothing was applied")
                self.assertTrue(env.installed()["attempt"], "the binary stays")
                self.assertIn("Nothing was changed", result.output)
                self.assertIn("~/.local/bin/attempt setup", result.output)
                self.assertNotIn("sh -s -- --yes", result.output, "a person at a terminal is not told to re-pipe")

    def test_anything_but_yes_is_no(self):
        for answer in (b"no\n", b"N\n", b"maybe\n", b"yep\n", b"n o\n"):
            with self.subTest(answer=answer):
                env = Env(self)
                result = env.run_tty(answers=[(APPLY, answer)])
                self.assertEqual(result.returncode, 0, result.output)
                self.assertEqual(env.calls(), [DRY_RUN_CALL])

    def test_end_of_input_is_not_a_yes(self):
        env = Env(self)
        result = env.run_tty(answers=[(APPLY, b"\x04")])  # Ctrl-D
        self.assertEqual(result.returncode, 0, result.output)
        self.assertEqual(env.calls(), [DRY_RUN_CALL])
        self.assertIn("Nothing was changed", result.output)

    def test_the_question_reaches_the_terminal_even_when_stdout_is_redirected(self):
        # `curl | sh > install.log`: the log gets the report, the terminal the question.
        env = Env(self)
        result = env.run_tty(answers=[(APPLY, b"\n")], redirect_stdout=True)
        self.assertEqual(result.returncode, 0, result.output)
        self.assertEqual(env.calls(), [DRY_RUN_CALL, SETUP_CALL])
        self.assertIn(APPLY, result.tty)
        self.assertNotIn(APPLY, result.stdout)
        self.assertIn("Installed attempt", result.stdout)

    def test_a_failed_setup_after_a_yes_is_the_exit_code(self):
        env = Env(self)
        result = env.run_tty(answers=[(APPLY, b"\n")], SETUP_EXIT="1")
        self.assertEqual(result.returncode, 1, result.output)

    def test_forwarded_arguments_reach_both_the_preview_and_the_apply(self):
        env = Env(self)
        result = env.run_tty(["--capture-mode", "metadata_only"], answers=[(APPLY, b"\n")])
        self.assertEqual(result.returncode, 0, result.output)
        self.assertEqual(
            env.calls(),
            [f"{DRY_RUN_CALL} --capture-mode metadata_only", f"{SETUP_CALL} --capture-mode metadata_only"],
        )

    def test_yes_does_not_ask(self):
        env = Env(self)
        result = env.run_tty(["--yes"])
        self.assertEqual(result.returncode, 0, result.output)
        self.assertEqual(env.calls(), [SETUP_CALL])
        self.assertNotIn("Apply these changes", result.output)

    def test_a_dry_run_argument_does_not_ask(self):
        env = Env(self)
        result = env.run_tty(["--dry-run"])
        self.assertEqual(result.returncode, 0, result.output)
        self.assertEqual(env.calls(), [f"{SETUP_CALL} --dry-run"])
        self.assertNotIn("Apply these changes", result.output)

    def test_no_setup_does_not_ask(self):
        env = Env(self)
        result = env.run_tty(ATTEMPTDB_NO_SETUP="1")
        self.assertEqual(result.returncode, 0, result.output)
        self.assertEqual(env.calls(), [])
        self.assertNotIn("Apply these changes", result.output)


class PathTests(unittest.TestCase):
    """The shell-profile edit. SHELL says whose profile; the default (/bin/sh) is nobody's."""

    def run_path(self, env, *args, shell_env="/bin/zsh", **extra):
        settings = {"ATTEMPTDB_NO_SETUP": "1", "SHELL": shell_env}
        settings.update(extra)
        return env.run(list(args), **settings)

    def posix_line(self, env):
        home = str(env.home)
        directory = str(env.bin_dir)
        if directory.startswith(home + "/"):
            directory = "$HOME" + directory[len(home):]
        return f'export PATH="{directory}:$PATH" {MARK}'

    def path_after_sourcing(self, env, profile):
        out = subprocess.run(
            ["/bin/sh", "-c", '. "$1"; printf %s "$PATH"', "sh", str(profile)],
            env={"HOME": str(env.home), "PATH": "/usr/bin:/bin"}, capture_output=True, text=True)
        return out.stdout

    def dotfiles(self, env):
        return sorted(p.name for p in env.home.iterdir() if p.name != ".local")

    # -- without consent ---------------------------------------------------------

    def test_without_consent_nothing_is_written_and_the_line_is_printed(self):
        env = Env(self)
        result = self.run_path(env)
        self.assertEqual(result.returncode, 0, result.output)
        self.assertEqual(self.dotfiles(env), [], "no profile was created or touched")
        self.assertIn("is not on your PATH", result.stdout)
        self.assertIn(f'export PATH="{env.bin_dir}:$PATH"', result.stdout)
        self.assertIn("ATTEMPTDB_MODIFY_PATH=1", result.stdout)

    def test_modify_path_zero_and_the_opt_out_leave_the_profile_alone(self):
        for settings in ({"ATTEMPTDB_MODIFY_PATH": "0"}, {"ATTEMPTDB_NO_MODIFY_PATH": "1"},
                         {"ATTEMPTDB_MODIFY_PATH": "1", "ATTEMPTDB_NO_MODIFY_PATH": "1"}):
            with self.subTest(settings=settings):
                env = Env(self)
                (env.home / ".zshrc").write_text("alias ll='ls -l'\n")
                result = self.run_path(env, **settings)
                self.assertEqual(result.returncode, 0, result.output)
                self.assertEqual((env.home / ".zshrc").read_text(), "alias ll='ls -l'\n")
                self.assertIn("is not on your PATH", result.stdout)

    def test_a_directory_already_on_path_is_left_alone_silently(self):
        env = Env(self)
        result = self.run_path(env, on_path=True, ATTEMPTDB_MODIFY_PATH="1")
        self.assertEqual(result.returncode, 0, result.output)
        self.assertEqual(self.dotfiles(env), [])
        self.assertNotIn("PATH", result.stdout)

    # -- with consent ------------------------------------------------------------

    def test_modify_path_one_adds_one_marked_line_and_only_once(self):
        env = Env(self)
        result = self.run_path(env, ATTEMPTDB_MODIFY_PATH="1")
        self.assertEqual(result.returncode, 0, result.output)
        profile = env.home / ".zshrc"
        self.assertEqual(profile.read_text(), self.posix_line(env) + "\n")
        self.assertIn("Added", result.stdout)
        self.assertIn(str(env.bin_dir), self.path_after_sourcing(env, profile).split(":"))
        # Run it again: the profile is not edited twice.
        again = self.run_path(env, ATTEMPTDB_MODIFY_PATH="1")
        self.assertEqual(again.returncode, 0, again.output)
        self.assertEqual(profile.read_text(), self.posix_line(env) + "\n")
        self.assertIn("already mentions", again.stdout)

    def test_the_line_never_glues_onto_a_last_line_without_a_newline(self):
        env = Env(self)
        profile = env.home / ".zshrc"
        profile.write_text("export EDITOR=vim")
        self.run_path(env, ATTEMPTDB_MODIFY_PATH="1")
        self.assertEqual(profile.read_text(), "export EDITOR=vim\n" + self.posix_line(env) + "\n")

    def test_a_profile_that_already_names_the_directory_is_not_edited(self):
        env = Env(self)
        profile = env.home / ".zshrc"
        profile.write_text('export PATH="$HOME/.local/bin:$PATH"\n')
        result = self.run_path(env, ATTEMPTDB_MODIFY_PATH="1")
        self.assertEqual(profile.read_text(), 'export PATH="$HOME/.local/bin:$PATH"\n')
        self.assertIn("already mentions", result.stdout)

    def test_a_directory_outside_home_is_written_as_is(self):
        env = Env(self, bin_rel="tools/bin")
        result = self.run_path(env, ATTEMPTDB_MODIFY_PATH="1")
        self.assertEqual(result.returncode, 0, result.output)
        self.assertEqual((env.home / ".zshrc").read_text(), f'export PATH="{env.bin_dir}:$PATH" {MARK}\n')

    def test_a_directory_name_that_cannot_be_quoted_safely_is_never_written(self):
        for name in ('we"ird', "dollar$sign", "back`tick", "back\\slash"):
            with self.subTest(name=name):
                env = Env(self, bin_rel=f"{name}/bin")
                result = self.run_path(env, ATTEMPTDB_MODIFY_PATH="1")
                self.assertEqual(result.returncode, 0, result.output)
                self.assertFalse((env.home / ".zshrc").exists())
                self.assertIn("is not on your PATH", result.stdout)

    # -- which profile -------------------------------------------------------------

    def test_zsh_honours_zdotdir(self):
        env = Env(self)
        zdot = env.root / "zdot"
        zdot.mkdir()
        self.run_path(env, ATTEMPTDB_MODIFY_PATH="1", ZDOTDIR=str(zdot))
        self.assertTrue((zdot / ".zshrc").exists())
        self.assertFalse((env.home / ".zshrc").exists())

    def test_bash_on_linux_uses_bashrc(self):
        env = Env(self, fake_os="Linux")
        self.run_path(env, shell_env="/usr/bin/bash", ATTEMPTDB_MODIFY_PATH="1")
        self.assertEqual((env.home / ".bashrc").read_text(), self.posix_line(env) + "\n")
        self.assertEqual(self.dotfiles(env), [".bashrc"])

    def test_bash_on_macos_uses_the_login_file_that_exists(self):
        cases = (
            ((), ".bash_profile"),
            ((".bash_profile",), ".bash_profile"),
            ((".profile",), ".profile"),  # creating .bash_profile would hide it
            ((".bashrc",), ".bash_profile"),  # login shells never read .bashrc
        )
        for existing, expected in cases:
            with self.subTest(existing=existing):
                env = Env(self, fake_os="Darwin")
                for name in existing:
                    (env.home / name).write_text("# mine\n")
                self.run_path(env, shell_env="/bin/bash", ATTEMPTDB_MODIFY_PATH="1")
                self.assertIn(self.posix_line(env), (env.home / expected).read_text())
                self.assertEqual(self.dotfiles(env), sorted(set(existing) | {expected}))

    def test_fish_gets_its_own_conf_d_file(self):
        env = Env(self)
        result = self.run_path(env, shell_env="/opt/homebrew/bin/fish", ATTEMPTDB_MODIFY_PATH="1")
        self.assertEqual(result.returncode, 0, result.output)
        conf = env.home / ".config" / "fish" / "conf.d" / "attemptdb.fish"
        self.assertEqual(
            conf.read_text(),
            f'{MARK}\nif not contains -- "$HOME/.local/bin" $PATH\n    set -gx PATH "$HOME/.local/bin" $PATH\nend\n',
        )
        self.assertEqual(self.dotfiles(env), [".config"])
        self.assertIn("fish_add_path", self.run_path(
            Env(self), shell_env="/usr/bin/fish").stdout, "the hint speaks fish too")
        self.run_path(env, shell_env="/opt/homebrew/bin/fish", ATTEMPTDB_MODIFY_PATH="1")
        self.assertEqual(conf.read_text().count("set -gx"), 1, "not twice")

    def test_fish_honours_xdg_config_home(self):
        env = Env(self)
        xdg = env.root / "xdg"
        self.run_path(env, shell_env="/usr/bin/fish", ATTEMPTDB_MODIFY_PATH="1", XDG_CONFIG_HOME=str(xdg))
        self.assertTrue((xdg / "fish" / "conf.d" / "attemptdb.fish").exists())
        self.assertEqual(self.dotfiles(env), [])

    def test_a_shell_it_does_not_know_gets_the_hint_and_no_files(self):
        for shell_env in ("/bin/sh", "/bin/dash", "/usr/bin/ksh", "", "/usr/bin/nu"):
            with self.subTest(shell=shell_env):
                env = Env(self)
                result = self.run_path(env, shell_env=shell_env, ATTEMPTDB_MODIFY_PATH="1")
                self.assertEqual(result.returncode, 0, result.output)
                self.assertEqual(self.dotfiles(env), [], "no profile for a shell that is not in use")
                self.assertIn("is not on your PATH", result.stdout)
                self.assertIn("your shell profile", result.stdout)

    def test_a_symlinked_profile_is_never_edited(self):
        env = Env(self)
        dotfiles = env.root / "dotfiles"
        dotfiles.mkdir()
        target = dotfiles / "zshrc"
        target.write_text("# managed elsewhere\n")
        (env.home / ".zshrc").symlink_to(target)
        result = self.run_path(env, ATTEMPTDB_MODIFY_PATH="1")
        self.assertEqual(result.returncode, 0, result.output)
        self.assertEqual(target.read_text(), "# managed elsewhere\n")
        self.assertIn("symlink", result.stdout)
        self.assertIn("Add this line yourself", result.stdout)

    @unittest.skipIf(os.geteuid() == 0, "root can write anything")
    def test_an_unwritable_profile_is_reported_not_fatal(self):
        env = Env(self)
        profile = env.home / ".zshrc"
        profile.write_text("# read-only\n")
        profile.chmod(0o444)
        result = self.run_path(env, ATTEMPTDB_MODIFY_PATH="1")
        self.assertEqual(result.returncode, 0, result.output)
        self.assertEqual(profile.read_text(), "# read-only\n")
        self.assertIn("Could not write", result.stdout)
        self.assertIn("is not on your PATH", result.stdout)

    # -- the question, at a terminal -------------------------------------------------

    def test_at_a_terminal_it_asks_and_enter_means_yes(self):
        env = Env(self)
        result = env.run_tty(answers=[("Add it in", b"\n")], ATTEMPTDB_NO_SETUP="1", SHELL="/bin/zsh")
        self.assertEqual(result.returncode, 0, result.output)
        self.assertEqual((env.home / ".zshrc").read_text(), self.posix_line(env) + "\n")
        self.assertIn(f"Add it in {env.home}/.zshrc? [Y/n]", result.output)

    def test_at_a_terminal_no_means_no_and_the_hint_is_printed(self):
        env = Env(self)
        result = env.run_tty(answers=[("Add it in", b"n\n")], ATTEMPTDB_NO_SETUP="1", SHELL="/bin/zsh")
        self.assertEqual(result.returncode, 0, result.output)
        self.assertFalse((env.home / ".zshrc").exists())
        self.assertIn("is not on your PATH", result.output)

    def test_both_questions_in_order_and_each_answer_is_its_own(self):
        env = Env(self)
        result = env.run_tty(
            answers=[(APPLY, b"\n"), ("Add it in", b"n\n")], SHELL="/bin/zsh")
        self.assertEqual(result.returncode, 0, result.output)
        self.assertEqual(env.calls(), [DRY_RUN_CALL, SETUP_CALL])
        self.assertFalse((env.home / ".zshrc").exists())

    def test_yes_covers_the_setup_question_only_not_the_profile_edit(self):
        env = Env(self)
        result = env.run_tty(["--yes"], answers=[("Add it in", b"n\n")], SHELL="/bin/zsh")
        self.assertEqual(result.returncode, 0, result.output)
        self.assertEqual(env.calls(), [SETUP_CALL])
        self.assertFalse((env.home / ".zshrc").exists())

    def test_yes_with_no_terminal_never_edits_a_profile(self):
        env = Env(self)
        result = env.run(["--yes"], SHELL="/bin/zsh")
        self.assertEqual(result.returncode, 0, result.output)
        self.assertEqual(env.calls(), [SETUP_CALL])
        self.assertEqual(self.dotfiles(env), [])

    @unittest.skipIf(os.geteuid() == 0, "root can write anything")
    def test_an_unwritable_profile_is_not_even_offered(self):
        env = Env(self)
        profile = env.home / ".zshrc"
        profile.write_text("# read-only\n")
        profile.chmod(0o444)
        result = env.run_tty(ATTEMPTDB_NO_SETUP="1", SHELL="/bin/zsh")  # no answers: must not wait for one
        self.assertEqual(result.returncode, 0, result.output)
        self.assertNotIn("Add it in", result.output)
        self.assertEqual(profile.read_text(), "# read-only\n")

    def test_the_modify_path_variable_skips_the_question(self):
        env = Env(self)
        result = env.run_tty(ATTEMPTDB_NO_SETUP="1", SHELL="/bin/zsh", ATTEMPTDB_MODIFY_PATH="1")
        self.assertEqual(result.returncode, 0, result.output)
        self.assertNotIn("Add it in", result.output)
        self.assertEqual((env.home / ".zshrc").read_text(), self.posix_line(env) + "\n")


if __name__ == "__main__":
    unittest.main()
