"""The words `setup.sh` prints: golden transcripts, the client's name in every message, and the numbering of the questions.

Renamed client: the names come from `scripts/setup/names.sh` (`HAMH_CLIENT_BIN`, `HAMH_CONFIG_DIR_NAME`). With a probe name (`widget`) for both, the help, the dry run, the install, the failed checks
and the questions on a terminal print the probe name wherever they print the client or its settings folder, and the default name `ham` only in the names of resources (`hamh-direct-control`,
`hamh-setup-<account>-<Region>`, the repository prefix `cc:hamh-e2e-`) and in the stage's parameter namespace `/hamh/<stage>/`. The static scan in `test_setup_names.py` holds
the same rule for every message, including the ones these runs do not reach.

Question numbers: the questions on a terminal are numbered in the order they are asked in the run, from 1, with no count announced in advance. The cases
below follow every route through the questions: the full setup and the client-only setup, each reached by the first question or by an option, with the build question
and the repository question present and absent. A typed `--client-only` does not offer the answer "deploy one for me" (reader finding W2).

Golden transcripts: `--help`, a client-only dry run, a client-only install and a failed check are run with the default names and compared byte for byte with the files in
`fixtures/setup/golden/`. A change to a message is then a change to one of those files, made on purpose. Set `HAMH_WRITE_GOLDEN=1` to write the files instead of comparing.
The temporary folder of the run is written as `<TMP>` and the earlier command name as `<earlier command>`.

Run with:
  $HAMH_PYTHON -m unittest discover -s scripts/tests -p test_setup_messages.py
"""

from __future__ import annotations

import os
import re
import unittest
from pathlib import Path

from test_setup_full_wizard import FullWizardBase
from test_setup_script import REPO, STUB_CARGO, STUB_HAM, SetupBase, sha256_of
from test_setup_wizard import PTY_OK, PtyRun
from setup_waits import limit

GOLDEN = REPO / "fixtures" / "setup" / "golden"
ENDPOINT = "https://abc123.execute-api.us-east-1.amazonaws.com/direct"


EARLIER_COMMAND = "cah"  # COMPAT-CAH: the earlier command name, which the install transcripts print (the golden files carry a placeholder instead)


class GoldenBase(SetupBase):
    def normalized(self, p) -> str:
        text = f"exit {p.returncode}\n--- stdout\n{p.stdout}--- stderr\n{p.stderr}"
        return text.replace(str(self.tmp), "<TMP>").replace(EARLIER_COMMAND, "<earlier command>")

    def assert_golden(self, name: str, text: str) -> None:
        path = GOLDEN / name
        if os.environ.get("HAMH_WRITE_GOLDEN") == "1":
            GOLDEN.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")
            return
        self.assertTrue(path.is_file(), f"{path.relative_to(REPO)} is missing: run with HAMH_WRITE_GOLDEN=1 to write it")
        self.assertEqual(text, path.read_text(encoding="utf-8"), f"{name} differs from its golden transcript")


class GoldenTranscriptCase(GoldenBase):
    def test_help(self) -> None:
        p = self.run_setup("--help")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assert_golden("help.txt", self.normalized(p))

    def test_a_client_only_dry_run(self) -> None:
        art = self.make_artifact()
        p = self.run_setup("--client-only", "--dry-run", "--from", str(art), "--endpoint", ENDPOINT, "--profile", "work", "--region", "us-east-1",
                           "--default-repo", "cc:hamh-e2e-demo")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assert_golden("client-dry-run.txt", self.normalized(p))

    def test_a_client_only_install(self) -> None:
        art = self.make_artifact()
        p = self.run_setup("--client-only", "--yes", "--from", str(art), "--endpoint", ENDPOINT, "--profile", "work", "--region", "us-east-1")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assert_golden("client-install.txt", self.normalized(p))

    def test_failed_checks(self) -> None:
        (self.home / ".aws" / "config").unlink()
        p = self.run_setup("--client-only", "--yes", "--profile", "nope", "--endpoint", ENDPOINT)
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assert_golden("client-failed-checks.txt", self.normalized(p))


class HelpWordsCase(SetupBase):
    """The help text states what holds, in words that carry no date and no rule of the repository's own coding agents."""

    def help_text(self) -> str:
        p = self.run_setup("--help")
        self.assertEqual(p.returncode, 0, self.out(p))
        return " ".join(p.stdout.split())

    def test_no_time_relative_word(self) -> None:
        text = self.help_text()
        self.assertNotIn("today", text)
        self.assertIn("--stage NAME the stage (only direct is supported)", text)
        self.assertIn("(only 1 is supported)", text)

    def test_no_rule_for_the_coding_agents_of_the_repository(self) -> None:
        self.assertNotIn("oding agents", self.help_text())

    def test_update_says_when_runs_hold_it_back_and_how_long_it_takes(self) -> None:
        text = self.help_text()
        start = text.index("--update build and deploy")
        about = text[start : text.index("--revision ID", start)]
        for want in ("Active runs stop ./setup.sh --update with exit 7", "before anything is installed or built", "about 20 to 25 minutes", "about 21 to 27 minutes more"):
            self.assertIn(want, about)
        self.assertNotIn("keeps the guard", about)
        self.assertNotIn("do not block", about)


# The stand-in client of the renamed runs: the same stand-in, answering as the probe name `widget` and keeping its settings in the `widget` folder (its call log keeps its name,
# and the header name that setup.sh finds the client by stays what the real client holds).
PROBE = "widget"
STUB_HAM = STUB_HAM.replace("ham.log", "@LOG@").replace("x-hamh-backend-version", "@HEADER@").replace("ham", PROBE).replace("@LOG@", "ham.log").replace("@HEADER@", "x-hamh-backend-version")
# The letters of the default name that may stay in a message of a run under another name: names of resources and the stage's parameter namespace (the same rule as the static scan).
OLD_NAME = re.compile(r"(?<![A-Za-z0-9_])(?<![A-Za-z0-9_]-)ham(?![A-Za-z0-9_-])")
NAMESPACE = re.compile(r"(?<![^\s(\"'=:,])/hamh/")


def old_name_in(text: str) -> list:
    return [m.start() for m in OLD_NAME.finditer(NAMESPACE.sub("/___/", text))]


class RenamedClientCase(SetupBase):
    """setup.sh run with HAMH_CLIENT_BIN and HAMH_CONFIG_DIR_NAME set to a probe name: no message names the default client `ham`."""

    NAMES = {"HAMH_CLIENT_BIN": PROBE, "HAMH_CONFIG_DIR_NAME": PROBE, "HAMH_PRODUCT_NAME": "Widget Harness"}

    def setUp(self) -> None:
        super().setUp()
        self.ham_source = self.tmp / "ham-source-stub"
        self.ham_source.write_text(STUB_HAM)
        self.ham_source.chmod(0o755)

    def ham_artifact(self) -> Path:
        d = self.tmp / "probe-artifact"
        d.mkdir()
        (d / PROBE).write_text(STUB_HAM)
        (d / PROBE).chmod(0o755)
        (d / "SHA256SUMS").write_text(f"{sha256_of(d / PROBE)}  {PROBE}\n")
        return d

    def run_ham(self, *args: str, **env: str):
        return self.run_setup(*args, **{**self.NAMES, **env})

    def assert_renamed(self, text: str, label: str, wants: tuple = ()) -> None:
        hits = [text[max(0, i - 50) : i + 30].replace("\n", " ") for i in old_name_in(text)]
        self.assertEqual(hits, [], f"{label}: the client is still called ham in the text")
        for want in wants:
            self.assertIn(want, text, label)

    def test_the_help_text(self) -> None:
        p = self.run_ham("--help")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assert_renamed(p.stdout, "--help", ("terminal app", "`widget models`", "DIR/bin/widget", "cargo build --release --locked -p widget --bin widget"))

    def test_a_dry_run(self) -> None:
        art = self.ham_artifact()
        p = self.run_ham("--client-only", "--dry-run", "--from", str(art), "--endpoint", ENDPOINT, "--profile", "work", "--region", "us-east-1")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assert_renamed(self.out(p), "dry run", ("would run: widget models", "would run: widget doctor", "/.config/widget/config.toml", "no widget command runs"))

    def test_an_install_and_the_next_steps(self) -> None:
        art = self.ham_artifact()
        p = self.run_ham("--client-only", "--yes", "--from", str(art), "--endpoint", ENDPOINT, "--profile", "work", "--region", "us-east-1")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assert_renamed(self.out(p), "install", ("Installing widget", "widget runs and lists its models", "widget rules show", 'widget "Build a small command-line todo app'))

    def test_an_install_without_an_endpoint(self) -> None:
        art = self.ham_artifact()
        p = self.run_ham("--client-only", "--yes", "--from", str(art))
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assert_renamed(self.out(p), "no endpoint", ("widget config set api_endpoint <that URL>", "widget doctor"))

    def test_failed_checks(self) -> None:
        (self.home / ".aws" / "config").unlink()
        p = self.run_ham("--client-only", "--yes", "--profile", "nope", "--endpoint", ENDPOINT)
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assert_renamed(self.out(p), "failed checks", ("there is no widget binary to install", "No downloadable release of widget is published"))

    def test_no_region(self) -> None:
        (self.home / ".aws" / "config").write_text("[default]\ncredential_process = /usr/bin/env helper print\n")
        art = self.ham_artifact()
        p = self.run_ham("--client-only", "--yes", "--from", str(art), "--endpoint", ENDPOINT)
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assert_renamed(self.out(p), "no region", ("widget signs every call for one Region",))

    def test_a_doctor_that_fails(self) -> None:
        art = self.ham_artifact()
        p = self.run_ham("--client-only", "--yes", "--from", str(art), "--endpoint", ENDPOINT, "--region", "us-east-1", FAKE_DOCTOR="FAIL identity refused\nFAIL api down", FAKE_DOCTOR_RC="1")
        self.assertEqual(p.returncode, 3, self.out(p))
        self.assert_renamed(self.out(p), "doctor", ("widget is installed, but widget doctor found a problem",))

    def test_a_build_outside_a_checkout(self) -> None:
        self.write_stub("cargo", "#!/bin/sh\nexit 0\n")
        p = self.run_ham("--client-only", "--dry-run", "--build")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assert_renamed(self.out(p), "--build", ("crates/widget",))

    @unittest.skipUnless(PTY_OK, "no pseudo-terminal on this host")
    def test_the_questions_on_a_terminal(self) -> None:
        art = self.ham_artifact()
        r = PtyRun(self, "--client-only", "--from", str(art), **self.NAMES)
        r.converse({"q1": "3", "q2": "", "q3": "us-east-1", "go": "y"}, timeout=limit(30))
        r.finish()
        self.assert_renamed(r.text, "questions", ("the widget backend", "widget signs every call"))


def headings(text: str) -> list:
    """The titles of the questions in a transcript, in order: `Question 2: the AWS profile`."""
    return re.findall(r"(?m)^== (Question [^\n]*)$", text)


@unittest.skipUnless(PTY_OK, "no pseudo-terminal on this host")
class QuestionNumbersCase(FullWizardBase):
    """The titles of the questions count 1, 2, 3 ... in the order the run asks them. Each route is run on a pseudo-terminal against the stand-in AWS CLI."""

    ENDPOINT_URL = "https://abc123.execute-api.us-east-1.amazonaws.com/direct"
    TOOLS = ("cargo-zigbuild", "zig", "node", "docker")

    def assert_sequence(self, text: str, titles: list) -> None:
        self.assertEqual(headings(text), titles, text)
        self.assertNotIn(" of 4", text)
        self.assertNotIn("up to four", text)

    def test_the_full_setup_from_the_first_question(self) -> None:
        r = self.tty()
        text = self.finish(r, {"mode": "1", "q2": "work", "q3": "", "deploy": "n"})
        self.assertEqual(r.rc, 4, text)
        self.assert_sequence(text, ["Question 1: What do you want to set up?", "Question 2: the AWS profile", "Question 3: the Region"])

    def test_the_full_setup_with_the_build_question(self) -> None:
        for n in self.TOOLS:
            self.write_stub(n, "#!/bin/sh\nexit 0\n")
        self.write_stub("cargo", STUB_CARGO)
        r = self.tty()
        text = self.finish(r, {"mode": "1", "q2": "work", "q3": "", "build": "", "deploy": "n"})
        self.assertEqual(r.rc, 4, text)
        self.assert_sequence(text, ["Question 1: What do you want to set up?", "Question 2: the AWS profile", "Question 3: the Region",
                                    "Question 4: Where to build the backend"])

    def test_the_full_setup_by_option_starts_at_question_1(self) -> None:
        r = self.tty("--full")
        text = self.finish(r, {"q2": "work", "q3": "", "deploy": "n"})
        self.assertEqual(r.rc, 4, text)
        self.assert_sequence(text, ["Question 1: the AWS profile", "Question 2: the Region"])
        r = self.tty("--full", "--profile", "work")
        text = self.finish(r, {"q3": "", "deploy": "n"})
        self.assertEqual(r.rc, 4, text)
        self.assert_sequence(text, ["Question 1: the Region"])

    def test_the_client_only_setup_from_the_first_question(self) -> None:
        self.ham_on_path()
        r = self.tty()
        text = self.finish(r, {"mode": "2", "q1": ["1", self.ENDPOINT_URL], "q2": "work", "q3": "", "q4": "", "go": "n"})
        self.assertEqual(r.rc, 1, text)
        self.assert_sequence(text, ["Question 1: What do you want to set up?", "Question 2: the ham backend", "Question 3: the AWS profile", "Question 4: the Region",
                                    "Question 5 (optional): a default repository"])

    def test_the_client_only_setup_by_option_skips_the_numbers_of_the_questions_it_does_not_ask(self) -> None:
        self.ham_on_path()
        r = self.tty("--client-only", "--profile", "work")
        text = self.finish(r, {"q1": ["1", self.ENDPOINT_URL], "q3": "", "q4": "", "go": "n"})
        self.assertEqual(r.rc, 1, text)
        self.assert_sequence(text, ["Question 1: the ham backend", "Question 2: the Region", "Question 3 (optional): a default repository"])

    def test_the_stored_endpoint_question_is_numbered_too(self) -> None:
        art = self.make_artifact()
        first = self.run_full("--client-only", "--yes", "--from", str(art), "--endpoint", self.ENDPOINT_URL, "--profile", "work", "--region", "us-east-1")
        self.assertEqual(first.returncode, 0, self.out(first))
        r = self.tty("--client-only")
        text = self.finish(r, {"q1": "", "q2": "", "q3": "", "q4": "", "go": "n"})
        self.assertEqual(r.rc, 1, text)
        self.assertEqual(headings(text)[:1], ["Question 1: the stage endpoint"], text)
        self.assertTrue(all(h.startswith(f"Question {i}") for i, h in enumerate(headings(text), 1)), headings(text))

    def test_the_numbers_count_the_questions_asked_whatever_the_route(self) -> None:
        """TRIGGER PRESENT: the sequences above are only worth something if the run asks more than one question; here the numbers are checked in general."""
        self.ham_on_path()
        for args, answers in (((), {"mode": "2", "q1": ["1", self.ENDPOINT_URL], "q2": "work", "q3": "", "q4": "", "go": "n"}),
                              (("--client-only",), {"q1": ["1", self.ENDPOINT_URL], "q2": "work", "q3": "", "q4": "", "go": "n"}),
                              (("--full",), {"q2": "work", "q3": "", "deploy": "n"})):
            with self.subTest(args=args):
                r = self.tty(*args)
                text = self.finish(r, answers)
                found = headings(text)
                self.assertGreaterEqual(len(found), 2, text)
                self.assertEqual([int(re.match(r"Question (\d+)", h).group(1)) for h in found], list(range(1, len(found) + 1)), found)

    def test_the_banner_announces_no_count(self) -> None:
        r = self.tty(*self.GIVEN)
        self.assertEqual(r.converse(stop_at="mode"), "mode", r.text)
        self.assertIn("one numbered question at a time", " ".join(r.text.split()))
        self.assertNotRegex(r.text, r"(?i)up to (four|five|\d)")
        r.send("q")
        r.finish()


@unittest.skipUnless(PTY_OK, "no pseudo-terminal on this host")
class TypedClientOnlyCase(FullWizardBase):
    """A typed `--client-only` is a promise that nothing is created in your AWS account, so the endpoint question does not offer to deploy a stage (reader finding W2)."""

    ENDPOINT_URL = "https://abc123.execute-api.us-east-1.amazonaws.com/direct"

    def test_the_endpoint_question_has_two_choices_when_client_only_was_typed(self) -> None:
        self.ham_on_path()
        r = self.tty("--client-only", *self.GIVEN)
        self.assertEqual(r.converse(stop_at="q1"), "q1", r.text)
        flat = " ".join(r.text.split())
        self.assertIn("1) Yes, I have its URL (it ends with /direct)", flat)
        self.assertIn("2) Not now; install ham and I will set the endpoint later", flat)
        self.assertNotIn("deploy a stage", flat)
        self.assertNotIn("3)", flat)
        self.assertIn("Choice [2]: ", r.text)
        r.send("3")
        self.assertEqual(r.wait_prompt(), "q1", r.text)
        self.assertIn("Type 1 or 2.", r.text)
        r.send("1:2")  # a colon does not turn an answer into a choice
        self.assertEqual(r.wait_prompt(), "q1", r.text)
        self.assertEqual(r.text.count("Type 1 or 2."), 2, r.text)
        r.send("2")
        text = self.finish(r, {"go": "y"})
        self.assertEqual(r.rc, 0, text)
        self.assertEqual(self.calls(), [], "a typed --client-only makes no AWS call")
        self.assertNotIn("Create these resources", text)

    def test_a_url_still_works_and_enter_means_not_now(self) -> None:
        self.ham_on_path()
        r = self.tty("--client-only", *self.GIVEN)
        text = self.finish(r, {"q1": ["1", self.ENDPOINT_URL], "go": "y"})
        self.assertEqual(r.rc, 0, text)
        self.assertEqual(self.calls(), [])
        r = self.tty("--client-only", *self.GIVEN)
        text = self.finish(r, {"go": "y"})
        self.assertEqual(r.rc, 0, text)
        self.assertEqual(self.calls(), [])

    def test_choosing_client_only_from_the_first_question_keeps_the_deploy_answer(self) -> None:
        """The answer to a menu is not a typed promise: question 1 offers the client only, and the endpoint question that follows may still switch to the full setup."""
        r = self.tty(*self.GIVEN)
        self.assertEqual(r.converse({"mode": "2"}, stop_at="q1"), "q1", r.text)
        flat = " ".join(r.text.split())
        self.assertIn("2) No, deploy a stage into my own AWS account for me (full setup)", flat)
        self.assertIn("Choice [3]: ", r.text)
        r.send("q")
        r.finish()


if __name__ == "__main__":
    unittest.main()
