"""Tests for the questions of setup.sh (S-16 to S-18 of the setup.sh audit).

On a terminal, setup.sh asks, one at a time and numbered in the order it asks them, for what no option gave: the stage endpoint (or a deploy of your own), the
AWS profile, the Region and an optional default repository. Each question has a default in brackets and an option of its own, nothing is written before the
plan is confirmed, and the writes go only through `ham config set`. `--yes`, `--no-input` and a missing terminal ask nothing.

The cases run the real script in a fresh fake HOME with a PATH of stub commands (the helpers of test_setup_script.py). The questions are asked
on a real pseudo-terminal (PtyRun below, standard library only), because setup.sh decides whether to ask with `[ -t 0 ]`. The pseudo-terminal
does not echo what the test types, so a transcript holds only what setup.sh printed.
"""

from __future__ import annotations

import codecs
import os
import re
import select
import shlex
import subprocess  # nosec B404: tests start the script under test with an argument list, never a shell
import time
import unittest

from setup_waits import limit
from test_setup_script import BASH, BASH3, KEY_ID, SCRIPT, SetupBase, tree_state  # the sibling module: the suite is run with `unittest discover -s scripts/tests`


def _pty_works() -> bool:
    try:
        import pty

        master, slave = pty.openpty()
        os.close(master)
        os.close(slave)
        return True
    except Exception:  # no pseudo-terminal on this host: the terminal cases skip, the cases without a terminal still run
        return False


PTY_OK = _pty_works()

# How long a pty case waits for a prompt or a text before it gives up, and for the program to end. They are ceilings, not delays: a wait ends when its event comes.
# 10 s was too short for a host at load 20 to 32; HAMH_TEST_WAIT_SCALE stretches both for a busier one.
PROMPT_WAIT = limit(60.0)
FINISH_WAIT = limit(30.0)

# The prompts of setup.sh as the tests recognise them: a prompt ends without a newline, at the start of a line.
PROMPTS = (
    ("mode", r"(?m)^Choice \[1\]: "),
    ("build", r"(?m)^Where to build \[1\]: "),
    ("deploy", r"(?m)^Create these resources and start the deploy\? \[y/N\] "),
    ("teardown", r"(?m)^Delete these resources\? \[y/N\] "),
    ("src", r"(?m)^Source \[\d\]: "),
    ("srcdir", r"(?m)^Directory with \S+ and SHA256SUMS: "),
    ("q1", r"(?m)^(Choice \[[23]\]: |Endpoint URL of the stage, from the V1ApiUrl[^\n]*\]: |Endpoint URL \(https://[^\n]*\): )"),
    ("q2", r"(?m)^Profile number or name[^\n]*: "),
    ("q3", r"(?m)^Region (\[[^\]\n]*\]|\([^)\n]*\)): "),
    ("q4", r"(?m)^Default repository \[[^\]\n]*\]: "),
    ("keep", r"(?m)^Keep [^\n]* anyway\? \[y/N\] "),
    ("go", r"(?m)^Go ahead\? \[y/N\] "),
)


class PtyRun:
    """setup.sh on a pseudo-terminal: wait for a prompt, answer it, end the input, collect the output and the exit code.

    `converse` answers every prompt it recognises from a table (Enter, or `n` for a "Keep ... anyway?" question, when the table has no answer left
    for it), so a case names only the answers it cares about and does not depend on which other questions are due.
    """

    def __init__(self, case: SetupBase, *args: str, script=SCRIPT, bash: str = BASH, echo: bool = False, **env: str) -> None:
        import pty
        import termios

        master, slave = pty.openpty()
        if not echo:
            attrs = termios.tcgetattr(slave)
            attrs[3] &= ~termios.ECHO
            termios.tcsetattr(slave, termios.TCSANOW, attrs)
        self.master = master
        self.proc = subprocess.Popen(  # nosec B603: argument list, no shell, the program under test is the interpreter and a repository script
            [bash, str(script), *args], stdin=slave, stdout=slave, stderr=slave, env=case.env(**env), cwd=str(case.tmp),
            close_fds=True, start_new_session=True)
        os.close(slave)
        self.decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
        self.raw = ""
        self.pos = 0
        self.eof = False
        case.addCleanup(self.kill)

    # ---- reading ---------------------------------------------------------------------------------
    def pump(self, interval: float = 0.1) -> None:
        if self.eof:
            return
        ready, _, _ = select.select([self.master], [], [], interval)
        if not ready:
            return
        try:
            data = os.read(self.master, 4096)
        except OSError:  # EIO: the child closed its side
            data = b""
        if not data:
            self.eof = True
            return
        self.raw += self.decoder.decode(data)

    @property
    def text(self) -> str:
        return self.raw.replace("\r\n", "\n").replace("\r", "")

    def wait_prompt(self, timeout: float = PROMPT_WAIT):
        """The key of the next prompt on the screen, or None when the program ended first."""
        deadline = time.monotonic() + timeout
        while True:
            tail = self.raw[self.pos:]
            hits = [(m.start(), m.end(), key) for key, pat in PROMPTS for m in [re.search(pat, tail)] if m]
            if hits:
                start, end, key = min(hits)
                self.pos += end
                return key
            if self.eof:
                return None
            if time.monotonic() > deadline:
                raise AssertionError(f"no prompt within {timeout:.0f} s; what setup.sh printed so far:\n{self.text}")
            self.pump()

    def expect(self, text: str, timeout: float = PROMPT_WAIT) -> None:
        deadline = time.monotonic() + timeout
        while text not in self.raw[self.pos:]:
            if self.eof or time.monotonic() > deadline:
                raise AssertionError(f"{text!r} did not appear; what setup.sh printed so far:\n{self.text}")
            self.pump()
        self.pos += self.raw[self.pos:].index(text) + len(text)

    # ---- writing ---------------------------------------------------------------------------------
    def send(self, line: str) -> None:
        os.write(self.master, (line + "\n").encode())

    def ctrl_d(self) -> None:
        os.write(self.master, b"\x04")

    # ---- conversation ----------------------------------------------------------------------------
    def converse(self, answers: dict | None = None, stop_at: str | None = None, timeout: float = PROMPT_WAIT):
        """Answer prompts from `answers` ({"q2": ["my-sso", "r", "work"], "go": "y"}); return the key of the prompt named `stop_at` (left
        unanswered), or None when the program ended."""
        queues = {k: (list(v) if isinstance(v, (list, tuple)) else [v]) for k, v in (answers or {}).items()}
        while True:
            key = self.wait_prompt(timeout)
            if key is None or key == stop_at:
                return key
            queue = queues.get(key) or []
            self.send(queue.pop(0) if queue else ("n" if key == "keep" else ""))

    def finish(self, timeout: float = FINISH_WAIT) -> int:
        deadline = time.monotonic() + timeout
        while not self.eof:
            if time.monotonic() > deadline:
                self.kill()
                raise AssertionError(f"setup.sh did not end within {timeout:.0f} s; output:\n{self.text}")
            self.pump()
        rc = self.proc.wait(timeout=timeout)
        os.close(self.master)
        self.master = -1
        return rc

    def kill(self) -> None:
        if self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait()
        if self.master >= 0:
            try:
                os.close(self.master)
            except OSError:
                pass
            self.master = -1


class OptionsCase(SetupBase):
    """A: the options around the questions, and what a run without a terminal does."""

    ENDPOINT = "https://abc123.execute-api.us-east-1.amazonaws.com/direct"

    def test_no_input_flag_is_accepted(self) -> None:
        p = self.run_setup("--no-input", "--dry-run", "--from", str(self.make_artifact()))
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("unknown option", p.stderr)

    def test_no_input_without_yes_or_dry_run_stops_like_no_terminal(self) -> None:
        art = self.make_artifact()
        before = tree_state(self.home)
        p = self.run_setup("--no-input", "--from", str(art))
        self.assertEqual(p.returncode, 2, self.out(p))
        self.assertIn("--no-input was given", p.stderr)
        self.assertIn("--yes", p.stderr)
        self.assertIn("--dry-run", p.stderr)
        self.assertEqual(before, tree_state(self.home))
        self.assertFalse((self.prefix / "bin" / "ham").exists())

    def test_no_input_with_yes_goes_ahead_without_a_question(self) -> None:
        p = self.run_setup("--no-input", "--yes", "--from", str(self.make_artifact()))
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertTrue((self.prefix / "bin" / "ham").is_file())

    def test_help_lists_the_new_options_and_says_questions_are_asked_on_a_terminal(self) -> None:
        p = self.run_setup("--help")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("--no-input", p.stdout)
        flat = " ".join(p.stdout.split())
        self.assertIn("on a terminal, setup.sh asks one numbered question at a time", flat)
        self.assertNotIn("up to four questions", flat)
        for topic in ("endpoint", "profile", "Region", "repository"):
            self.assertIn(topic, flat)
        self.assertIn("--yes, --no-input and a missing terminal ask nothing", flat)
        self.assertIn("--default-repo", p.stdout)

    def test_default_repo_flag_is_written_through_ham_config_set(self) -> None:
        p = self.run_setup("--from", str(self.make_artifact()), "--endpoint", self.ENDPOINT, "--default-repo", "cc:hamh-e2e-demo", "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn('defaultRepo = "cc:hamh-e2e-demo"', self.config_file().read_text())
        self.assertIn("config set default_repo cc:hamh-e2e-demo", self.log_lines())

    def test_default_repo_flag_shape_is_checked(self) -> None:
        for good in ("cc:hamh-e2e-x", "cc:hamh-e2e-my.repo_1-b"):
            with self.subTest(good=good):
                p = self.run_setup("--from", str(self.make_artifact(f"ok-{len(good)}")), "--default-repo", good, "--dry-run")
                self.assertEqual(p.returncode, 0, self.out(p))
        for bad in ("gh:x", "cc:other", "cc:hamh-e2e-", "cc:hamh-e2e-a b", "<name>", "cc:hamh-e2e-a;b", "cc:hamh-e2e-<name>"):
            with self.subTest(bad=bad):
                p = self.run_setup("--from", str(self.make_artifact(f"bad-{abs(hash(bad))}")), "--default-repo", bad, "--yes")
                self.assertEqual(p.returncode, 2, self.out(p))
                self.assertIn("--default-repo should look like cc:hamh-e2e-NAME", p.stderr)
                self.assertNotIn("unknown option", p.stderr)
                self.assertFalse((self.prefix / "bin" / "ham").exists())

    def test_no_terminal_with_missing_profile_names_the_option_to_give(self) -> None:
        (self.home / ".aws" / "config").write_text("[profile work]\nregion = us-east-1\ncredential_process = x\n")
        p = self.run_setup("--from", str(self.make_artifact()), "--yes")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("--profile", p.stderr)
        self.assertIn("work", p.stderr)

    def test_no_endpoint_message_names_both_routes(self) -> None:
        p = self.run_setup("--from", str(self.make_artifact()), "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        flat = " ".join(p.stdout.split())
        self.assertIn("Ask the person who runs the stage for its URL", flat)
        self.assertIn("Deploy a stage into your own AWS account", flat)
        self.assertIn("./setup.sh --full", flat)
        self.assertIn("ham config set api_endpoint", flat)
        self.assertIn("V1ApiUrl", flat)



# ---- the terminal cases ---------------------------------------------------------------------------------

WIZ_CONFIG = (
    "[default]\nregion = us-east-1\ncredential_process = /usr/bin/env helper print\n\n"
    "[profile work]\nregion = us-east-1\ncredential_process = /usr/bin/env helper print\n\n"
    "[profile my-sso]\nregion = eu-west-1\nsso_session = corp\nsso_account_id = 1\n\n"
    "[profile jump]\nrole_arn = arn:aws:iam::1:role/x\nsource_profile = work\n")
WIZ_CREDENTIALS = "[team]\naws_access_key_id = " + KEY_ID + "\naws_secret_access_key = made-up\n"


@unittest.skipUnless(PTY_OK, "no pseudo-terminal on this host")
class WizardBase(SetupBase):
    """A fake home with five profiles of four kinds, a stub ham, and helpers that start setup.sh on a pseudo-terminal."""

    ENDPOINT = "https://abc123.execute-api.us-east-1.amazonaws.com/direct"
    REGION = "us-east-1"

    def setUp(self) -> None:
        super().setUp()
        (self.home / ".aws" / "config").write_text(WIZ_CONFIG)
        (self.home / ".aws" / "credentials").write_text(WIZ_CREDENTIALS)
        self.aws_state = tree_state(self.home / ".aws")
        self.art = self.make_artifact()

    def wiz(self, *args: str, **env: str) -> PtyRun:
        """setup.sh --from <artifact> with the given options, on a terminal."""
        return PtyRun(self, "--from", str(self.art), *args, **env)

    def profile_due(self, *args: str, **env: str) -> PtyRun:
        """The endpoint and the Region are given, so the profile question is the one that is due."""
        return self.wiz("--endpoint", self.ENDPOINT, "--region", self.REGION, *args, **env)

    def rows(self, text: str) -> dict:
        """The numbered rows of the profile menu in `text` (the last menu): {name: (number, rest of the row)}."""
        found: dict = {}
        for ln in text.splitlines():
            m = re.match(r"^\s*(\d+)\) +(\S+) +(.*)$", ln)
            if m:
                found[m.group(2)] = (int(m.group(1)), m.group(3))
        return found

    def default_row_name(self, text: str) -> str:
        """The profile the last profile prompt offers as its default (the number in brackets), or '' when it offers none."""
        prompts = re.findall(r"(?m)^Profile number or name[^\n]*?(?:\[(\d+)\])?: ", text)
        number = prompts[-1] if prompts else ""
        if not number:
            return ""
        for name, (n, _rest) in self.rows(text).items():
            if n == int(number):
                return name
        return ""

    def written(self) -> str:
        return self.config_file().read_text() if self.config_file().is_file() else ""

    def finish_run(self, r: PtyRun, answers: dict | None = None) -> str:
        """Answer the rest of the questions (Enter) and the confirmation (y), and return the transcript; the exit code is in r.rc."""
        a = dict(answers or {})
        a.setdefault("go", "y")
        r.converse(a)
        r.rc = r.finish()
        return r.text


class SourceQuestionCase(WizardBase):
    """R2-19: with no ham installed and no --from, --sha256 or --build, the client route asked the endpoint, the profile and the Region and only then ended with "there is no ham
    binary to install". The source is asked right after the endpoint, before any question whose answer would be thrown away."""

    def bare(self, *args: str, **env: str) -> PtyRun:
        return PtyRun(self, "--client-only", "--endpoint", self.ENDPOINT, *args, **env)

    def test_the_source_is_asked_before_the_profile_and_the_region(self) -> None:
        r = self.bare()
        self.assertEqual(r.converse(stop_at="src"), "src", r.text)
        shown = r.text
        self.assertIn("Question 1: where the ham program comes from", shown)
        self.assertIn("not installed on this machine", shown)
        self.assertIn("1) Directory where I built it before", shown)
        self.assertNotIn("the AWS profile", shown, "no question that needs the client came first")
        r.send("q")
        self.assertEqual(r.finish(), 1)

    def test_a_directory_answer_installs_that_client_and_the_other_questions_follow(self) -> None:
        r = self.bare("--region", self.REGION)
        self.assertEqual(r.converse(stop_at="src"), "src", r.text)
        r.send("1")
        self.assertEqual(r.wait_prompt(), "srcdir", r.text)
        r.send(str(self.art))
        self.assertEqual(r.wait_prompt(), "q2", r.text)
        r.send("work")
        self.assertEqual(r.converse({"go": "y"}), None, r.text)
        self.assertEqual(r.finish(), 0, r.text)
        self.assertTrue((self.prefix / "bin" / "ham").is_file())
        self.assertLess(r.text.index("where the ham program comes from"), r.text.index("the AWS profile"))

    def test_stopping_there_names_the_ways_to_get_a_client_and_changes_nothing(self) -> None:
        before = tree_state(self.home)
        r = self.bare()
        self.assertEqual(r.converse(stop_at="src"), "src", r.text)
        r.send("2")
        self.assertIsNone(r.wait_prompt(), r.text)
        self.assertEqual(r.finish(), 1)
        for needle in ("there is no ham binary to install", "--from DIR", "--full", "Nothing was changed."):
            self.assertIn(needle, r.text)
        self.assertEqual(before, tree_state(self.home))

    def test_the_build_answer_is_offered_when_there_is_a_checkout_and_cargo(self) -> None:
        self.write_stub("cargo", "#!/bin/sh\nexit 0\n")
        r = self.bare()
        self.assertEqual(r.converse(stop_at="src"), "src", r.text)
        self.assertIn("1) Build it on this machine from this checkout", r.text)
        self.assertIn("2) Directory where I built it before", r.text)
        r.send("q")
        self.assertEqual(r.finish(), 1)

    def test_a_ham_that_is_already_installed_means_no_source_question(self) -> None:
        self.ham_on_path()
        r = self.bare("--region", self.REGION)
        self.assertEqual(r.wait_prompt(), "q2", r.text)
        self.assertNotIn("where the ham program comes from", r.text)
        r.send("q")
        r.finish()

    def test_the_options_that_name_a_source_skip_the_question(self) -> None:
        r = PtyRun(self, "--client-only", "--endpoint", self.ENDPOINT, "--from", str(self.art), "--region", self.REGION)
        self.assertEqual(r.wait_prompt(), "q2", r.text)
        self.assertNotIn("where the ham program comes from", r.text)
        r.send("q")
        r.finish()

    def test_the_longest_run_asks_as_many_questions_as_the_help_says(self) -> None:
        """A person with no ham on the machine who picks the client-only setup at the first question and answers every later one gets six questions: what to set up,
        the stage endpoint, where the program comes from, the profile, the Region and the default repository. `setup.sh --help` and the documents name that number."""
        words = {"four": 4, "five": 5, "six": 6, "seven": 7}
        said = re.search(r"You get at most (\w+) questions", SCRIPT.read_text(encoding="utf-8"))
        self.assertIsNotNone(said, "setup.sh --help no longer says how many questions it asks at most")
        r = PtyRun(self)
        answers = {"mode": "2", "q1": ["1", self.ENDPOINT], "src": "1", "srcdir": str(self.art), "q2": "work", "go": "y"}
        r.converse(answers)
        self.assertEqual(r.finish(), 0, r.text)
        asked = [int(n) for n in re.findall(r"(?m)^\S*\s*Question (\d+)(?: \(optional\))?: ", r.text)]
        self.assertEqual(asked, [1, 2, 3, 4, 5, 6], r.text)
        self.assertEqual(words[said.group(1)], max(asked), "the help says at most %s questions; this run asked %d" % (said.group(1), max(asked)))

    def test_a_directory_that_is_not_there_is_asked_again_and_three_failures_name_the_option(self) -> None:
        r = self.bare("--region", self.REGION)
        self.assertEqual(r.converse(stop_at="src"), "src", r.text)
        r.send("1")
        for _ in range(3):
            self.assertEqual(r.wait_prompt(), "srcdir", r.text)
            r.send(str(self.tmp / "nothing-here"))
        self.assertIsNone(r.wait_prompt(), r.text)
        self.assertEqual(r.finish(), 2)
        self.assertIn("--from DIR", r.text)


class ProfileQuestionCase(WizardBase):
    """D: question 2, the AWS profile."""

    def test_wizard_lists_profiles_from_both_files_with_their_kind(self) -> None:
        r = self.profile_due()
        self.assertEqual(r.converse(stop_at="q2"), "q2", r.text)
        shown = r.text
        for name, kind, advice in (("default", "credential_process", "usable"), ("work", "credential_process", "usable"),
                                   ("team", "static keys", "usable"), ("my-sso", "SSO", "usable"),
                                   ("jump", "assumes a role", "usable")):
            self.assertRegex(shown, rf"(?m)^\s*\d+\) +{re.escape(name)} +{re.escape(kind)} +{re.escape(advice)}")
        self.assertIn("Question 1: the AWS profile", shown)
        self.assertIn("must belong to the", shown)
        r.send("q")
        r.finish()

    def test_wizard_list_skips_sso_session_and_services_sections_and_bare_config_sections(self) -> None:
        (self.home / ".aws" / "config").write_text(
            WIZ_CONFIG + "\n[sso-session corp]\nsso_start_url = x\n\n[services s1]\ndynamodb = x\n\n[bare]\nregion = x\n")
        r = self.profile_due()
        self.assertEqual(r.converse(stop_at="q2"), "q2", r.text)
        names = set(self.rows(r.text))
        self.assertEqual(names, {"default", "work", "my-sso", "jump", "team"}, r.text)
        r.send("q")
        r.finish()

    def test_wizard_list_handles_crlf_files_and_odd_characters_in_names(self) -> None:
        odd = "with.dot_and-dash+plus@at:colon"
        text = f"[profile {odd}]\r\nregion = us-east-1\r\ncredential_process = x\r\n\r\n[profile plain]\r\ncredential_process = y\r\n"
        (self.home / ".aws" / "config").write_text(text)
        (self.home / ".aws" / "credentials").unlink()
        r = self.profile_due()
        self.assertEqual(r.converse(stop_at="q2"), "q2", r.text)
        shown = self.rows(r.text)
        self.assertEqual(set(shown), {odd, "plain"}, r.text)
        self.assertTrue(shown[odd][1].startswith("credential_process"), shown)
        r.send(odd)
        text_out = self.finish_run(r)
        self.assertEqual(r.rc, 0, text_out)
        self.assertIn(f'awsProfile = "{odd}"', self.written())

    def test_wizard_default_is_the_config_profile_then_aws_profile_then_default_profile(self) -> None:
        cases = (("the ham config profile", "team", {"AWS_PROFILE": "work"}),
                 ("AWS_PROFILE", "work", {"AWS_PROFILE": "work"}),
                 ("the profile named default", "default", {}))
        for label, expected, env in cases:
            with self.subTest(source=label):
                self.config_file().parent.mkdir(parents=True, exist_ok=True)
                self.config_file().write_text('awsProfile = "team"\n' if expected == "team" else "")
                r = self.profile_due(**env)
                self.assertEqual(r.converse(stop_at="q2"), "q2", r.text)
                self.assertEqual(self.default_row_name(r.text), expected, r.text)
                r.send("")
                self.finish_run(r)
                self.assertEqual(r.rc, 0, r.text)
                self.assertIn(f'awsProfile = "{expected}"', self.written())

    def test_wizard_default_is_the_only_usable_profile(self) -> None:
        (self.home / ".aws" / "config").write_text(
            "[profile work]\nregion = us-east-1\ncredential_process = x\n\n[profile bare]\nregion = us-east-1\n")
        (self.home / ".aws" / "credentials").unlink()
        r = self.profile_due()
        self.assertEqual(r.converse(stop_at="q2"), "q2", r.text)
        self.assertEqual(self.default_row_name(r.text), "work", r.text)
        r.send("")
        self.finish_run(r)
        self.assertIn('awsProfile = "work"', self.written())

    def test_wizard_has_no_default_when_two_are_usable(self) -> None:
        (self.home / ".aws" / "config").write_text(
            "[profile a]\nregion = us-east-1\ncredential_process = x\n\n[profile b]\nregion = us-east-1\ncredential_process = x\n")
        (self.home / ".aws" / "credentials").unlink()
        r = self.profile_due()
        self.assertEqual(r.converse(stop_at="q2"), "q2", r.text)
        self.assertEqual(self.default_row_name(r.text), "", r.text)
        r.send("")  # Enter with no default asks again
        self.assertEqual(r.wait_prompt(), "q2", r.text)
        r.send("q")
        self.assertEqual(r.finish(), 1)

    def test_wizard_env_keys_add_row_0_only_when_both_keys_are_set(self) -> None:
        both = {"AWS_ACCESS_KEY_ID": KEY_ID, "AWS_SECRET_ACCESS_KEY": "made-up"}
        r = self.profile_due(**both)
        self.assertEqual(r.converse(stop_at="q2"), "q2", r.text)
        self.assertRegex(r.text, r"(?m)^\s*0\) +the AWS_\* variables of this shell \(nothing is saved")
        r.send("0")
        text_out = self.finish_run(r)
        self.assertEqual(r.rc, 0, text_out)
        self.assertNotIn("awsProfile", self.written(), "choosing the AWS_* variables writes no aws_profile")
        one = self.profile_due(AWS_ACCESS_KEY_ID=KEY_ID)
        self.assertEqual(one.converse(stop_at="q2"), "q2", one.text)
        self.assertNotRegex(one.text, r"(?m)^\s*0\)")
        one.send("q")
        one.finish()

    def test_wizard_accepts_a_number_or_a_name(self) -> None:
        r = self.profile_due()
        self.assertEqual(r.converse(stop_at="q2"), "q2", r.text)
        number = self.rows(r.text)["work"][0]
        r.send(str(number))
        self.finish_run(r)
        self.assertEqual(r.rc, 0, r.text)
        self.assertIn('awsProfile = "work"', self.written())
        again = self.profile_due()
        self.assertEqual(again.converse(stop_at="q2"), "q2", again.text)
        again.send("team")
        self.finish_run(again)
        self.assertEqual(again.rc, 0, again.text)
        self.assertIn('awsProfile = "team"', self.written())

    def test_wizard_unknown_name_is_refused_three_times_then_exit_2_naming_the_flag(self) -> None:
        r = self.profile_due()
        for _ in range(3):
            self.assertEqual(r.wait_prompt(), "q2", r.text)
            r.send("nonesuch")
        self.assertEqual(r.finish(), 2, r.text)
        self.assertEqual(r.text.count("There is no profile with that name in the list"), 3, r.text)
        self.assertIn("--profile NAME", r.text)
        self.assertFalse((self.prefix / "bin" / "ham").exists())

    def test_wizard_does_not_echo_a_rejected_profile_answer(self) -> None:
        pasted = "AKIA" + "ZZZZZZZZZZZZZZZZ"  # key-shaped, built at run time
        r = self.profile_due()
        for _ in range(3):
            self.assertEqual(r.wait_prompt(), "q2", r.text)
            r.send(pasted)
        self.assertEqual(r.finish(), 2, r.text)
        self.assertNotIn(pasted, r.text)
        self.assertNotIn(pasted, " ".join(self.log_lines()))

    def test_wizard_sso_choice_is_accepted_and_written_with_the_sign_in_reminder(self) -> None:
        self.aws_on_path()
        r = self.profile_due()
        self.assertEqual(r.converse(stop_at="q2"), "q2", r.text)
        r.send("my-sso")
        self.finish_run(r)
        self.assertEqual(r.rc, 0, r.text)
        self.assertEqual(r.text.count("Profile number or name"), 1, "the question is not asked again")
        self.assertIn("aws sso login --profile my-sso", r.text)
        self.assertIn('awsProfile = "my-sso"', self.written())
        self.assertNotIn("for-ham", r.text)
        self.assertEqual(tree_state(self.home / ".aws"), self.aws_state)

    def test_wizard_assume_role_choice_is_accepted(self) -> None:
        self.aws_on_path()
        r = self.profile_due()
        self.assertEqual(r.converse(stop_at="q2"), "q2", r.text)
        r.send("jump")
        self.finish_run(r)
        self.assertEqual(r.rc, 0, r.text)
        self.assertIn('awsProfile = "jump"', self.written())

    def test_wizard_sso_choice_without_an_aws_cli_stops_before_anything_is_written(self) -> None:
        before = tree_state(self.home)
        r = self.profile_due()
        self.assertEqual(r.converse(stop_at="q2"), "q2", r.text)
        r.send("my-sso")
        r.converse({"go": "y"})
        self.assertEqual(r.finish(), 1, r.text)
        for needle in ("AWS CLI", "2.9.0", "https://docs.aws.amazon.com/cli/", "Nothing was changed"):
            self.assertIn(needle, r.text)
        self.assertEqual(before, tree_state(self.home))
        self.assertEqual(self.log_lines(), [])

    def test_wizard_r_reads_the_files_again_and_lists_a_profile_added_meanwhile(self) -> None:
        r = self.profile_due()
        self.assertEqual(r.converse(stop_at="q2"), "q2", r.text)
        added = "\n[profile added-later]\nregion = eu-west-1\ncredential_process = helper print\n"
        with open(self.home / ".aws" / "config", "a") as f:
            f.write(added)
        r.send("r")
        self.assertEqual(r.wait_prompt(), "q2", r.text)
        last_menu = r.text.rsplit("Profile number or name", 2)[-2]
        self.assertRegex(last_menu, r"(?m)^\s*\d+\) +added-later +credential_process +usable")
        r.send("added-later")
        self.finish_run(r)
        self.assertEqual(r.rc, 0, r.text)
        self.assertIn('awsProfile = "added-later"', self.written())

    def test_wizard_q_at_the_profile_question_stops_with_nothing_changed(self) -> None:
        before = tree_state(self.home)
        r = self.profile_due()
        self.assertEqual(r.converse(stop_at="q2"), "q2", r.text)
        r.send("q")
        self.assertEqual(r.finish(), 1, r.text)
        self.assertIn("Nothing was changed", r.text)
        self.assertEqual(before, tree_state(self.home))
        self.assertEqual(self.log_lines(), [])

    def test_wizard_long_profile_list_is_cut_at_20_rows_and_names_still_work(self) -> None:
        names = [f"p{i:02d}" for i in range(1, 26)]
        (self.home / ".aws" / "config").write_text("".join(f"[profile {n}]\nregion = us-east-1\ncredential_process = x\n\n" for n in names))
        (self.home / ".aws" / "credentials").unlink()
        r = self.profile_due()
        self.assertEqual(r.converse(stop_at="q2"), "q2", r.text)
        self.assertEqual(len(self.rows(r.text)), 20, r.text)
        self.assertIn("... and 5 more; type a name", r.text)
        r.send("p25")
        self.finish_run(r)
        self.assertEqual(r.rc, 0, r.text)
        self.assertIn('awsProfile = "p25"', self.written())

    def test_wizard_no_profile_files_prints_guidance_and_exits_1_without_asking_for_credentials(self) -> None:
        (self.home / ".aws" / "config").unlink()
        (self.home / ".aws" / "credentials").unlink()
        before = tree_state(self.home)
        for name in ("aws",):
            self.write_stub(name, f'#!/bin/sh\nprintf \'%s %s\\n\' {name} "$*" >> "${{STUB_LOG:?}}/calls.log"\nexit 0\n')
        r = self.profile_due()
        self.assertIsNone(r.converse(stop_at="q2"), r.text)  # the run ends before the question
        self.assertEqual(r.finish(), 1, r.text)
        for needle in ("aws configure sso", "aws configure", "credential_process", "https://docs.aws.amazon.com/cli/", "Nothing was changed"):
            self.assertIn(needle, r.text)
        self.assertFalse((self.logs / "calls.log").exists())
        self.assertEqual(before, tree_state(self.home))

    def test_wizard_profile_answer_is_written_as_aws_profile(self) -> None:
        r = self.profile_due()
        self.finish_run(r, {"q2": "work"})
        self.assertEqual(r.rc, 0, r.text)
        self.assertIn('awsProfile = "work"', self.written())
        self.assertIn("config set aws_profile work", self.log_lines())

    def test_wizard_never_asks_for_a_key_secret_or_token(self) -> None:
        secrets = {"AWS_ACCESS_KEY_ID": KEY_ID, "AWS_SECRET_ACCESS_KEY": "SECRETKEYVALUE0123456789abcdef",
                   "AWS_SESSION_TOKEN": "SESSIONTOKENVALUE0123456789abcdef"}
        r = self.profile_due(**secrets)
        self.finish_run(r, {"q2": "work"})
        self.assertEqual(r.rc, 0, r.text)
        for banned in ("access key", "secret", "token", "password"):
            self.assertNotIn(banned, r.text.lower())
        for value in secrets.values():
            self.assertNotIn(value, r.text)
            self.assertNotIn(value, self.written())


class TerminalSafetyCase(WizardBase):
    """G: what a terminal run does and does not do around the questions."""

    def test_yes_on_a_terminal_asks_no_question(self) -> None:
        for extra in (("--endpoint", self.ENDPOINT, "--region", self.REGION), ()):
            with self.subTest(options=extra):
                r = self.wiz("--yes", *extra)
                self.assertIsNone(r.converse(), r.text)
                self.assertEqual(r.finish(), 0, r.text)
                self.assertNotIn("Question", r.text)
                self.assertNotIn("Go ahead?", r.text)

    def test_no_input_on_a_terminal_asks_no_question_and_requires_yes_or_dry_run(self) -> None:
        r = self.wiz("--no-input")
        self.assertIsNone(r.converse(), r.text)
        self.assertEqual(r.finish(), 2, r.text)
        self.assertNotIn("Question", r.text)
        self.assertIn("--no-input was given", r.text)
        d = self.wiz("--no-input", "--dry-run")
        self.assertIsNone(d.converse(), d.text)
        self.assertEqual(d.finish(), 0, d.text)
        self.assertNotIn("Question", d.text)

    def test_dry_run_on_a_terminal_asks_the_due_questions_and_writes_nothing(self) -> None:
        before = tree_state(self.home)
        r = self.profile_due("--dry-run")
        r.converse({"q2": "work"})
        self.assertEqual(r.finish(), 0, r.text)
        self.assertIn("Question 1: the AWS profile", r.text)
        self.assertIn("set api_endpoint aws_profile region", r.text)
        self.assertIn("Dry run finished", r.text)
        self.assertEqual(before, tree_state(self.home))
        self.assertEqual(self.log_lines(), [])

    def test_nothing_is_written_before_the_plan_is_confirmed(self) -> None:
        before = tree_state(self.home)
        r = self.profile_due()
        r.converse({"q2": "work", "go": "n"})
        self.assertEqual(r.finish(), 1, r.text)
        self.assertIn("Question 1: the AWS profile", r.text, "the question must have been asked before the plan")
        self.assertIn("stopped at your request. Nothing was changed", r.text)
        self.assertEqual(before, tree_state(self.home))
        self.assertEqual(self.log_lines(), [])

    def test_all_options_given_asks_nothing_and_matches_todays_output(self) -> None:
        args = ("--from", str(self.art), "--endpoint", self.ENDPOINT, "--profile", "work", "--region", self.REGION,
                "--default-repo", "cc:hamh-e2e-demo", "--dry-run")
        plain = self.run_setup(*args)
        self.assertEqual(plain.returncode, 0, self.out(plain))
        r = self.wiz(*args[2:])
        self.assertIsNone(r.converse(), r.text)
        self.assertEqual(r.finish(), 0, r.text)
        self.assertNotIn("Question", r.text)
        self.assertNotIn("again:", r.text)
        self.assertEqual(r.text, plain.stdout, "a terminal run with every option given prints what a run without a terminal prints")

    def test_ctrl_d_at_any_question_stops_with_nothing_changed(self) -> None:
        for question, args in (("q1", ()), ("q2", ()), ("q3", ()), ("q4", ("--endpoint", self.ENDPOINT))):
            with self.subTest(question=question):
                before = tree_state(self.home)
                r = self.wiz(*args)  # nothing else given: the question is due
                self.assertEqual(r.converse(stop_at=question), question, r.text)
                r.ctrl_d()
                self.assertEqual(r.finish(), 1, r.text)
                self.assertIn("Nothing was changed", r.text)
                self.assertEqual(before, tree_state(self.home))
                self.assertEqual(self.log_lines(), [])

    def test_q_at_any_question_stops_with_nothing_changed(self) -> None:
        for question, args in (("q1", ()), ("q2", ()), ("q3", ()), ("q4", ("--endpoint", self.ENDPOINT))):
            with self.subTest(question=question):
                before = tree_state(self.home)
                r = self.wiz(*args)  # nothing else given: the question is due
                self.assertEqual(r.converse(stop_at=question), question, r.text)
                r.send("q")
                self.assertEqual(r.finish(), 1, r.text)
                self.assertIn("Nothing was changed", r.text)
                self.assertEqual(before, tree_state(self.home))
                self.assertEqual(self.log_lines(), [])



class BackendQuestionCase(WizardBase):
    """C: question 1, the ham backend (the stage endpoint)."""

    BAD_ENDPOINTS = ("http://example.com/direct", "ftp://x", "https://<api-id>.execute-api.us-east-1.amazonaws.com/direct", "https://a b",
                     "not-a-url")

    def backend_due(self, *args: str, **env: str) -> PtyRun:
        """The profile and the Region are given, so the backend question is the one that is due."""
        return self.wiz("--profile", "work", "--region", self.REGION, *args, **env)

    def test_wizard_asks_for_the_backend_when_no_endpoint_is_known(self) -> None:
        r = self.backend_due()
        self.assertEqual(r.converse(stop_at="q1"), "q1", r.text)
        for needle in ("Question 1: the ham backend", "1) Yes, I have its URL (it ends with /direct)",
                       "2) No, deploy a stage into my own AWS account for me (full setup)", "3) Not now; install ham and I will set the endpoint later"):
            self.assertIn(needle, r.text)
        r.send("q")
        self.assertEqual(r.finish(), 1, r.text)

    def test_wizard_endpoint_answer_is_written_and_doctor_runs(self) -> None:
        r = self.backend_due()
        text = self.finish_run(r, {"q1": ["1", self.ENDPOINT]})
        self.assertEqual(r.rc, 0, text)
        self.assertIn(f'apiEndpoint = "{self.ENDPOINT}"', self.written())
        self.assertIn(f"config set api_endpoint {self.ENDPOINT}", self.log_lines())
        self.assertEqual(self.log_lines().count("doctor"), 1)

    def test_wizard_endpoint_is_validated_like_the_flag(self) -> None:
        for bad in self.BAD_ENDPOINTS:
            with self.subTest(bad=bad):
                if self.config_file().exists():
                    self.config_file().unlink()  # each answer starts with no endpoint stored, so question 1 shows its menu
                r = self.backend_due()
                text = self.finish_run(r, {"q1": ["1", bad, self.ENDPOINT]})
                self.assertEqual(r.rc, 0, text)
                self.assertEqual(text.count("That is not an https URL without spaces"), 1, text)
                self.assertNotIn(bad, text, "a rejected answer is never echoed")
                self.assertIn(f'apiEndpoint = "{self.ENDPOINT}"', self.written())

    def test_wizard_three_bad_endpoints_exit_2_and_name_the_flag(self) -> None:
        r = self.backend_due()
        self.assertEqual(r.wait_prompt(), "q1", r.text)
        r.send("1")
        for _ in range(3):
            self.assertEqual(r.wait_prompt(), "q1", r.text)
            r.send("nonsense")
        self.assertEqual(r.finish(), 2, r.text)
        self.assertIn("--endpoint URL", r.text)
        self.assertFalse((self.prefix / "bin" / "ham").exists())

    def test_wizard_endpoint_without_slash_direct_warns_but_is_accepted(self) -> None:
        odd = "https://abc123.execute-api.us-east-1.amazonaws.com/v1"
        r = self.backend_due()
        text = self.finish_run(r, {"q1": ["1", odd]})
        self.assertEqual(r.rc, 0, text)
        self.assertIn("the endpoint does not end with /direct", text)
        self.assertIn(f'apiEndpoint = "{odd}"', self.written())

    def test_wizard_skips_question_1_when_endpoint_flag_is_given(self) -> None:
        r = self.profile_due()  # --endpoint and --region given
        text = self.finish_run(r, {"q2": "work"})
        self.assertEqual(r.rc, 0, text)
        self.assertNotIn("Do you have the URL of one?", text)

    def test_wizard_offers_the_config_endpoint_as_the_default_and_enter_keeps_it(self) -> None:
        first = self.run_setup("--from", str(self.art), "--endpoint", self.ENDPOINT, "--profile", "work", "--region", self.REGION, "--yes")
        self.assertEqual(first.returncode, 0, self.out(first))
        after_first = tree_state(self.home)
        r = self.wiz()
        self.assertEqual(r.converse(stop_at="q1"), "q1", r.text)
        self.assertIn(f"[{self.ENDPOINT}]: ", r.text)
        self.assertNotIn("Do you have the URL of one?", r.text)
        r.send("")
        text = self.finish_run(r)
        self.assertEqual(r.rc, 0, text)
        self.assertIn("api_endpoint is already", text)
        self.assertEqual(after_first, tree_state(self.home))

    def test_wizard_choice_3_matches_todays_no_endpoint_run(self) -> None:
        r = self.backend_due()
        text = self.finish_run(r, {"q1": ""})
        self.assertEqual(r.rc, 0, text)
        self.assertNotIn("doctor", self.log_lines())
        self.assertNotIn("apiEndpoint", self.written())
        self.assertIn("Skipped: no apiEndpoint is set yet.", text)

    def test_wizard_choice_2_with_a_binary_option_is_a_usage_error_not_a_silent_switch(self) -> None:
        r = self.backend_due()
        text = self.finish_run(r, {"q1": "2"})
        self.assertEqual(r.rc, 2, text)
        self.assertIn("--from", text)
        self.assertIn("full setup", text)
        self.assertNotIn("doctor", self.log_lines())
        self.assertFalse((self.prefix / "bin" / "ham").exists())



class RegionQuestionCase(WizardBase):
    """E: question 3, the Region."""

    EAST = "https://abc123.execute-api.us-east-1.amazonaws.com/direct"
    WEST = "https://abc123.execute-api.us-west-2.amazonaws.com/direct"

    def region_due(self, *args: str, **env: str) -> PtyRun:
        """The endpoint and the profile are given, so the Region question is the one that is due."""
        return self.wiz("--profile", "work", *args, **env)

    def test_region_default_comes_from_the_endpoint_host(self) -> None:
        r = self.region_due("--endpoint", self.WEST)
        self.assertEqual(r.converse(stop_at="q3"), "q3", r.text)
        self.assertIn("Region [us-west-2]: ", r.text)
        r.send("")
        text = self.finish_run(r)
        self.assertEqual(r.rc, 0, text)
        self.assertIn('region = "us-west-2"', self.written())

    def test_region_default_comes_from_the_profile_when_there_is_no_endpoint(self) -> None:
        with open(self.home / ".aws" / "config", "a") as f:
            f.write("\n[profile eu]\nregion = eu-west-1\ncredential_process = x\n")
        r = self.wiz("--profile", "eu")
        self.assertEqual(r.converse(stop_at="q3"), "q3", r.text)  # question 1 got Enter: not now
        self.assertIn("Region [eu-west-1]: ", r.text)
        r.send("")
        text = self.finish_run(r)
        self.assertEqual(r.rc, 0, text)
        self.assertIn('region = "eu-west-1"', self.written())

    def test_region_default_comes_from_aws_region_before_the_profile(self) -> None:
        r = self.wiz("--profile", "work", AWS_REGION="ap-southeast-2")
        self.assertEqual(r.converse(stop_at="q3"), "q3", r.text)
        self.assertIn("Region [ap-southeast-2]: ", r.text)
        r.send("q")
        r.finish()

    def test_region_with_no_default_accepts_enter_and_writes_nothing(self) -> None:
        with open(self.home / ".aws" / "config", "a") as f:
            f.write("\n[profile bare]\ncredential_process = x\n")
        r = self.wiz("--profile", "bare")
        self.assertEqual(r.converse(stop_at="q3"), "q3", r.text)
        self.assertIn("Region (for example us-east-1; Enter leaves it to AWS_REGION or the profile): ", r.text)
        r.send("")
        text = self.finish_run(r)
        self.assertEqual(r.rc, 0, text)
        self.assertNotIn("region", self.written())
        self.assertFalse(any(ln.startswith("config set region") for ln in self.log_lines()))

    def test_region_prompt_rejects_what_the_flag_rejects(self) -> None:
        r = self.region_due("--endpoint", self.EAST)
        text = self.finish_run(r, {"q3": ["US East", "us_east_1", "us-east-1"]})
        self.assertEqual(r.rc, 0, text)
        self.assertEqual(text.count("That does not look like a Region"), 2, text)
        self.assertNotIn("US East", text)
        self.assertNotIn("us_east_1", text)
        self.assertIn('region = "us-east-1"', self.written())

    def test_region_differing_from_the_endpoint_warns_and_asks_to_keep(self) -> None:
        r = self.region_due("--endpoint", self.EAST)
        text = self.finish_run(r, {"q3": ["us-west-2", ""], "keep": ["n"]})  # refuse, then Enter takes the endpoint's Region
        self.assertEqual(r.rc, 0, text)
        self.assertIn("the endpoint is in us-east-1, you chose us-west-2", text)
        self.assertIn("Keep us-west-2 anyway? [y/N]", text)
        self.assertIn('region = "us-east-1"', self.written())
        again = self.region_due("--endpoint", self.EAST)
        text = self.finish_run(again, {"q3": ["us-west-2"], "keep": ["y"]})
        self.assertEqual(again.rc, 0, text)
        self.assertIn('region = "us-west-2"', self.written())

    def test_region_question_skipped_when_flag_given(self) -> None:
        r = self.region_due("--endpoint", self.EAST, "--region", "us-east-1")
        text = self.finish_run(r)
        self.assertEqual(r.rc, 0, text)
        self.assertNotIn("The stage runs in one AWS Region", text)


class PlanConsistencyCase(WizardBase):
    """G: the equivalent command, idempotence, the writes, the identity reminder and the shape of the script."""

    ANSWERS = {"q1": ["1", WizardBase.ENDPOINT], "q2": "work", "q3": "us-east-1", "go": "y"}

    def full_run(self, home_env: dict | None = None, answers: dict | None = None) -> PtyRun:
        r = PtyRun(self, "--from", str(self.art), **(home_env or {}))
        r.converse(answers or self.ANSWERS)
        r.rc = r.finish()
        return r

    def test_plan_prints_the_equivalent_command_and_running_it_gives_the_same_config(self) -> None:
        r = self.full_run()
        self.assertEqual(r.rc, 0, r.text)
        lines = [ln for ln in r.text.splitlines() if re.match(r"^\s+again:\s", ln)]
        self.assertEqual(len(lines), 1, r.text)
        tokens = shlex.split(lines[0].split("again:", 1)[1])
        self.assertEqual(tokens[0], str(SCRIPT))
        self.assertEqual(tokens[-1], "--yes")
        for pair in (("--from", str(self.art)), ("--endpoint", self.ENDPOINT), ("--profile", "work"), ("--region", "us-east-1")):
            self.assertIn(pair[0], tokens)
            self.assertEqual(tokens[tokens.index(pair[0]) + 1], pair[1])
        home2 = self.tmp / "home2"
        (home2 / ".aws").mkdir(parents=True)
        for name in ("config", "credentials"):
            (home2 / ".aws" / name).write_text((self.home / ".aws" / name).read_text())
        p = self.run_setup(*tokens[1:], HOME=str(home2))
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual((home2 / ".config" / "ham" / "config.toml").read_text(), self.written())

    def test_no_again_line_without_a_question(self) -> None:
        p = self.run_setup("--from", str(self.art), "--endpoint", self.ENDPOINT, "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("again:", p.stdout)

    def test_rerun_with_enter_everywhere_changes_nothing(self) -> None:
        first = self.full_run()
        self.assertEqual(first.rc, 0, first.text)
        after_first = tree_state(self.home)
        second = PtyRun(self, "--from", str(self.art))
        second.converse({"go": "y"})  # Enter at every question: the stored config supplies each default
        second.rc = second.finish()
        self.assertEqual(second.rc, 0, second.text)
        self.assertEqual(after_first, tree_state(self.home))
        self.assertIn("already", second.text)

    def test_the_script_writes_only_through_ham_config_set(self) -> None:
        r = self.full_run()
        self.assertEqual(r.rc, 0, r.text)
        sets = [ln.split(" ", 3)[2:] for ln in self.log_lines() if ln.startswith("config set ")]
        allowed = {"api_endpoint": "apiEndpoint", "aws_profile": "awsProfile", "region": "region", "default_repo": "defaultRepo"}
        expected: list = []
        for key, value in sets:
            self.assertIn(key, allowed)
            expected = [ln for ln in expected if not ln.startswith(allowed[key] + " = ")] + [f'{allowed[key]} = "{value}"']
        self.assertEqual(self.written().splitlines(), expected)
        self.assertEqual(len(sets), 3)

    def test_doctor_identity_line_is_repeated_with_the_account_reminder(self) -> None:
        arn = "arn:aws:sts::111111111111:assumed-role/Admin/me"
        doctor = f"PASS identity {arn} account 111111111111\nPASS api ok\nPASS catalog ok"
        p = self.run_setup("--from", str(self.art), "--endpoint", self.ENDPOINT, "--profile", "work", "--region", "us-east-1", "--yes",
                           FAKE_DOCTOR=doctor)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn(f"ham signs as {arn} in account 111111111111. That must be the account that hosts the stage.", p.stdout)
        q = self.run_setup("--from", str(self.art), "--endpoint", self.ENDPOINT, "--profile", "work", "--region", "us-east-1", "--yes",
                           FAKE_DOCTOR="PASS identity arn ok\nPASS api ok\nPASS catalog ok")
        self.assertNotIn("ham signs as", q.stdout, "no account in the identity line, so no reminder")

    def test_prompt_text_is_ascii_and_never_mentions_a_secret(self) -> None:
        text = SCRIPT.read_text()
        self.assertIsNone(re.search(r"[^\x00-\x7f]", text))
        prompts = [ln for ln in text.splitlines() if re.search(r'\bask "|\bsay "|prompt=', ln)]
        self.assertGreater(len(prompts), 20)
        for ln in prompts:
            self.assertIsNone(re.search(r"(?i)secret|password|\btoken|access key", ln), ln)

    def test_validators_are_shared_by_flags_and_prompts(self) -> None:
        text = SCRIPT.read_text()
        for name in ("valid_endpoint", "valid_region", "valid_profile_name"):
            self.assertEqual(len(re.findall(rf"(?m)^{name}\(\) \{{", text)), 1, name)
            self.assertGreaterEqual(len(re.findall(rf'(?m)^\s*{name} "\$\{{?(?:ENDPOINT|REGION|PROFILE)\}}?" \|\| usage_die', text)), 1,
                                    f"{name} is not called by an option check")
            self.assertGreaterEqual(len(re.findall(rf'(?m)^\s*(?:if )?(?:! )?{name} "\$ANSWER"', text)), 1, f"{name} is not called by a question")
        self.assertNotRegex(text, r"(?m)^\s*case \$(ENDPOINT|REGION|PROFILE) in")

    @unittest.skipUnless(BASH3, "no bash 3.x on this host: this case runs on a Mac and nowhere else")
    def test_wizard_cases_also_pass_under_bash_3(self) -> None:
        r = PtyRun(self, "--from", str(self.art), bash=BASH3)
        r.converse(self.ANSWERS)
        rc = r.finish()
        self.assertEqual(rc, 0, r.text)
        self.assertIn('awsProfile = "work"', self.written())
        self.aws_on_path()
        d = PtyRun(self, "--from", str(self.art), "--profile", "my-sso", bash=BASH3)
        d.converse(self.ANSWERS)
        self.assertEqual(d.finish(), 0, d.text)
        self.assertIn("aws sso login --profile my-sso", d.text)



class RepositoryQuestionCase(WizardBase):
    """F: question 4, an optional default repository."""

    def repo_due(self, *args: str, **env: str) -> PtyRun:
        """The endpoint, the profile and the Region are given, so the repository question is the one that is due."""
        return self.wiz("--endpoint", self.ENDPOINT, "--profile", "work", "--region", self.REGION, *args, **env)

    def test_repository_question_is_asked_only_when_an_endpoint_is_known(self) -> None:
        r = self.repo_due()
        self.assertEqual(r.converse(stop_at="q4"), "q4", r.text)
        for needle in ("Question 1 (optional): a default repository", "cc:hamh-e2e-<name>", "(runs then return a zip)",
                       "Default repository [none]: "):
            self.assertIn(needle, r.text)
        r.send("q")
        r.finish()
        none = self.wiz("--profile", "work", "--region", self.REGION)  # no endpoint: question 1 gets Enter ("not now")
        self.assertIsNone(none.converse({"go": "y"}), none.text)
        self.assertNotIn("a default repository", none.text)
        self.assertEqual(none.finish(), 0, none.text)

    def test_repository_enter_skips_and_writes_nothing(self) -> None:
        r = self.repo_due()
        text = self.finish_run(r)
        self.assertEqual(r.rc, 0, text)
        self.assertNotIn("defaultRepo", self.written())
        self.assertFalse(any("default_repo" in ln for ln in self.log_lines()))

    def test_repository_cc_name_is_written_as_default_repo(self) -> None:
        r = self.repo_due()
        text = self.finish_run(r, {"q4": "cc:hamh-e2e-demo"})
        self.assertEqual(r.rc, 0, text)
        self.assertIn('defaultRepo = "cc:hamh-e2e-demo"', self.written())
        self.assertIn("config set default_repo cc:hamh-e2e-demo", self.log_lines())

    def test_repository_bad_shape_is_refused_like_the_flag(self) -> None:
        for bad in ("gh:x", "cc:other", "cc:hamh-e2e-", "cc:hamh-e2e-a b", "<name>"):
            with self.subTest(bad=bad):
                r = self.repo_due()
                text = self.finish_run(r, {"q4": [bad, "cc:hamh-e2e-demo"]})
                self.assertEqual(r.rc, 0, text)
                self.assertEqual(text.count("That is not a repository name of the form cc:hamh-e2e-<name>"), 1, text)
                if bad not in SCRIPT.read_text():  # two of the answers are also pieces of the prompt's own text
                    self.assertNotIn(bad, text, "a rejected answer is never echoed")
                self.assertIn('defaultRepo = "cc:hamh-e2e-demo"', self.written())

    def test_repository_three_bad_answers_exit_2_and_name_the_flag(self) -> None:
        r = self.repo_due()
        self.assertEqual(r.converse(stop_at="q4"), "q4", r.text)
        r.send("nonsense")
        for _ in range(2):
            self.assertEqual(r.wait_prompt(), "q4", r.text)
            r.send("nonsense")
        self.assertEqual(r.finish(), 2, r.text)
        self.assertIn("--default-repo REPO", r.text)

    def test_repository_stored_default_is_offered_and_enter_keeps_it(self) -> None:
        first = self.run_setup("--from", str(self.art), "--endpoint", self.ENDPOINT, "--profile", "work", "--region", self.REGION,
                               "--default-repo", "cc:hamh-e2e-old", "--yes")
        self.assertEqual(first.returncode, 0, self.out(first))
        after_first = tree_state(self.home)
        r = self.repo_due()
        self.assertEqual(r.converse(stop_at="q4"), "q4", r.text)
        self.assertIn("Default repository [cc:hamh-e2e-old]: ", r.text)
        r.send("")
        text = self.finish_run(r)
        self.assertEqual(r.rc, 0, text)
        self.assertIn("default_repo is already", text)
        self.assertEqual(after_first, tree_state(self.home))


class PromptWaitCase(unittest.TestCase):
    """The pty cases waited a fixed 10 s for a prompt. Under the 32-way scripts suite on a loaded host (load 20 to 32 on 36 CPUs) setup.sh needed
    longer and `test_yes_deletes_and_nothing_more_is_asked` failed with "no prompt within 10 s" although the script was right. A wait is a ceiling: it
    costs nothing when the prompt comes, so the default is long enough for a loaded host."""

    def test_the_default_waits_are_one_long_ceiling(self) -> None:
        import inspect

        self.assertGreaterEqual(PROMPT_WAIT, 60.0)
        for method in (PtyRun.wait_prompt, PtyRun.expect, PtyRun.converse):
            with self.subTest(method=method.__name__):
                self.assertEqual(inspect.signature(method).parameters["timeout"].default, PROMPT_WAIT)

    def test_the_waits_are_the_limits_of_a_free_host_times_the_scale(self) -> None:
        self.assertEqual(PROMPT_WAIT, limit(60.0))
        self.assertEqual(FINISH_WAIT, limit(30.0))


if __name__ == "__main__":
    unittest.main()
