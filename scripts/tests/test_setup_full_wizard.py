"""The wizard of `setup.sh` on a terminal, from the first question to the one confirmation.

The first question is "client only, or full (backend and client)"; the default is full. The full path asks for the profile and the Region when no option
gave them, reads the rest from the AWS CLI, shows the account, the Region, the zone, the source commit, the authority it grants and the cost items, and
asks ONE question: whether to go ahead. After that nothing is asked. The cases run the real script on a pseudo-terminal (PtyRun of test_setup_wizard.py)
against the stand-in AWS CLI, so a cancelled run is proved to have created nothing.
"""

from __future__ import annotations

import unittest

from setup_full_support import FullBase
from setup_waits import limit
from test_setup_script import STUB_CARGO, sha256_of, tree_state
from test_setup_wizard import PTY_OK, PtyRun


@unittest.skipUnless(PTY_OK, "no pseudo-terminal on this host")
class FullWizardBase(FullBase):
    def tty(self, *args: str, **env: str) -> PtyRun:
        return PtyRun(self, *args, script=self.repo / "setup.sh", **env)

    GIVEN = ("--profile", "work", "--region", "us-east-1")

    def finish(self, r: PtyRun, answers: dict | None = None) -> str:
        r.converse(answers or {}, timeout=limit(60))
        r.rc = r.finish(timeout=limit(120))
        return r.text


class ModeQuestionCase(FullWizardBase):
    def test_the_first_question_is_client_only_or_full_and_full_is_the_default(self) -> None:
        r = self.tty(*self.GIVEN)
        self.assertEqual(r.converse(stop_at="mode"), "mode", r.text)
        flat = " ".join(r.text.split())
        self.assertIn("What do you want to set up?", flat)
        self.assertIn("1) Full setup", flat)
        self.assertIn("backend", flat)
        self.assertIn("2) Client only", flat)
        self.assertIn("Choice [1]:", r.text)
        self.assertEqual(self.mutations(), [])
        r.send("")
        self.assertEqual(r.converse(stop_at="deploy"), "deploy", r.text)
        r.send("n")
        self.assertEqual(r.finish(), 4, r.text)

    def test_nothing_is_asked_about_the_backend_in_the_full_path(self) -> None:
        r = self.tty()
        text = self.finish(r, {"mode": "1", "q2": "work", "q3": "", "deploy": "n"})
        self.assertEqual(r.rc, 4, text)
        self.assertNotIn("the ham backend", text)
        self.assertNotIn("Do you have the URL", text)
        self.assertIn("Question 2: the AWS profile", text)
        self.assertIn("Question 3: the Region", text)

    def test_answering_2_goes_on_as_the_client_only_setup(self) -> None:
        self.ham_on_path()
        r = self.tty(*self.GIVEN)
        text = self.finish(r, {"mode": "2", "q1": "3", "go": "y"})
        self.assertEqual(r.rc, 0, text)
        self.assertEqual(self.calls(), [], "a client-only setup makes no AWS call")
        self.assertNotIn("Create these resources", text)

    def test_the_client_only_menu_can_still_lead_to_the_full_setup(self) -> None:
        r = self.tty(*self.GIVEN)
        text = self.finish(r, {"mode": "2", "q1": "2", "deploy": "n"})
        self.assertEqual(r.rc, 4, text)
        self.assertIn("Create these resources", text)

    def test_an_answer_that_is_not_1_or_2_is_asked_again_and_three_in_a_row_stop_the_run(self) -> None:
        r = self.tty(*self.GIVEN)
        for _ in range(3):
            self.assertEqual(r.wait_prompt(), "mode", r.text)
            r.send("maybe")
        self.assertEqual(r.finish(), 2, r.text)
        self.assertIn("--full", r.text)
        self.assertIn("--client-only", r.text)

    def test_end_of_input_at_the_first_question_stops_without_a_change(self) -> None:
        r = self.tty(*self.GIVEN)
        self.assertEqual(r.wait_prompt(), "mode", r.text)
        r.ctrl_d()
        self.assertEqual(r.finish(), 1, r.text)
        self.assertEqual(self.calls(), [])

    def test_the_mode_flags_skip_the_question(self) -> None:
        full = self.tty("--full", *self.GIVEN)
        self.assertEqual(full.converse(stop_at="deploy"), "deploy", full.text)
        self.assertNotIn("What do you want to set up?", full.text)
        full.send("n")
        full.finish()
        self.ham_on_path()
        client = self.tty("--client-only", *self.GIVEN)
        self.assertEqual(client.converse(stop_at="q1"), "q1", client.text)
        self.assertNotIn("What do you want to set up?", client.text)
        client.send("q")
        client.finish()

    def test_a_binary_or_an_endpoint_option_means_client_only_and_skips_the_question(self) -> None:
        art = self.make_artifact()
        r = self.tty("--from", str(art), *self.GIVEN)
        self.assertEqual(r.converse(stop_at="q1"), "q1", r.text)
        self.assertNotIn("What do you want to set up?", r.text)
        r.send("q")
        r.finish()


class OwnClientVersusFullCase(FullWizardBase):
    """A client of the user's own (--from, --sha256, --build) belongs to the client-only setup, because the full setup builds the client in the user's account.

    On a terminal the full setup can be reached by an option (--full, or an option that only the full setup takes) or by the answer "deploy one for me" to
    question 1 of the client-only menu. Both stop with exit 2 before the profile and Region questions, before any AWS call and before anything is written. The
    answer is the only way a mode that began as client-only turns into the full setup after the options were read, so this case is the one that holds the
    check in setup.sh that runs after question 1. The options-only routes without a terminal are held by FullWithAClientOfYourOwnCase in test_setup_full_static.py."""

    MESSAGE = ("you chose the full setup, but --from, --sha256 or --build names a client of your own. The full setup builds the client in your AWS account: "
               "run again without them, or use --client-only.")
    TAIL = "builds the client in your AWS account, so --from, --sha256, --build and --endpoint do not go with it (they belong to --client-only)"

    def flag_message(self, flag: str) -> str:
        """What the refusal says for the option that turned the full setup on: the option the person typed."""
        return f"--full {self.TAIL}" if flag == "--full" else f"{flag} turns on the full setup, which {self.TAIL}"

    def own_clients(self) -> dict:
        art = self.make_artifact()
        return {"--from DIR": ("--from", str(art)), "--from FILE --sha256": ("--from", str(art / "ham"), "--sha256", sha256_of(art / "ham")),
                "--build": ("--build",)}

    def assert_stopped(self, r: PtyRun, text: str, message: str, label: str) -> None:
        self.assertEqual(r.rc, 2, f"{label}\n{text}")
        self.assertIn(message, text, label)
        self.assertIn("Try: setup.sh --help", text, label)
        for later in ("Question 2", "Question 3", "Create these resources", "Go ahead?", "== Checking"):
            self.assertNotIn(later, text, f"{label}: nothing comes after the refusal")
        self.assertEqual(self.calls(), [], f"{label}: no AWS call")
        self.assertFalse(self.installed().exists(), label)
        self.assertEqual(self.log_lines(), [], f"{label}: the client is not run")

    def test_the_answer_deploy_one_for_me_stops_before_the_profile_and_region_questions(self) -> None:
        own_clients = self.own_clients()
        for given_label, given in (("no profile or Region given", ()), ("profile and Region given", self.GIVEN)):
            for own_label, own in own_clients.items():
                label = f"{own_label}, {given_label}"
                with self.subTest(args=label):
                    before = tree_state(self.home)
                    r = self.tty(*own, *given)
                    text = self.finish(r, {"q1": "2"})
                    self.assertIn("Question 1: the ham backend", text, label)
                    self.assert_stopped(r, text, self.MESSAGE, label)
                    self.assertEqual(before, tree_state(self.home), f"{label}: nothing is written")

    def test_a_full_option_with_a_client_of_your_own_asks_no_question_at_all(self) -> None:
        own_clients = self.own_clients()
        for flag in (("--full",), ("--keep-runner",), ("--build-on", "runner")):
            for own_label, own in own_clients.items():
                label = f"{' '.join(flag)} {own_label}"
                with self.subTest(args=label):
                    r = self.tty(*flag, *own, *self.GIVEN)
                    self.assertIsNone(r.converse(timeout=limit(30)), f"{label}: no prompt may appear\n{r.text}")
                    r.rc = r.finish()
                    self.assertNotIn("What do you want to set up?", r.text, label)
                    self.assertNotIn("Question 1", r.text, label)
                    self.assert_stopped(r, r.text, self.flag_message(flag[0]), label)

    def test_the_same_answers_without_a_client_of_your_own_do_reach_the_full_setup(self) -> None:
        # TRIGGER PRESENT: the refusals above are only worth something if the same answers, with no client of the user's own, get as far as the AWS reads.
        r = self.tty(*self.GIVEN)
        text = self.finish(r, {"mode": "2", "q1": "2", "deploy": "n"})
        self.assertEqual(r.rc, 4, text)
        self.assertIn("Create these resources", text)
        self.assertGreaterEqual(self.count_calls("sts", "get-caller-identity"), 1, "the full setup reads the account before it asks to go ahead")
        self.assertEqual(self.mutations(), [])

    def test_answering_that_you_have_no_endpoint_yet_keeps_the_client_of_your_own(self) -> None:
        art = self.make_artifact()
        r = self.tty("--from", str(art), *self.GIVEN)
        text = self.finish(r, {"q1": "3", "go": "y"})
        self.assertEqual(r.rc, 0, text)
        self.assertTrue(self.installed().is_file())
        self.assertEqual(self.calls(), [], "the client-only setup makes no AWS call")


class ConfirmationCase(FullWizardBase):
    def test_the_screen_before_the_question_shows_the_account_the_zone_the_commit_the_authority_and_every_cost(self) -> None:
        commit = self.commit()
        r = self.tty("--full", *self.GIVEN)
        self.assertEqual(r.converse(stop_at="deploy"), "deploy", r.text)
        text = r.text
        flat = " ".join(text.split())
        for needle in (self.ACCOUNT, "us-east-1", "us-east-1b", "use1-az1", commit[:12], "sandbox", "IAM", "CDK bootstrap", "--keep-runner", "8 hours"):
            self.assertIn(needle, flat, needle)
        for item in ("NAT gateway", "Network Firewall", "Interface endpoints", "AgentCore sessions", "Bedrock calls", "CodeBuild", "Storage", "Logs"):
            self.assertIn(item, text, item)
        self.assertEqual(self.mutations(), [], "nothing is created before the answer")
        r.send("n")
        r.finish()

    def test_no_cancels_with_exit_4_and_creates_nothing_anywhere(self) -> None:
        r = self.tty("--full", *self.GIVEN)
        text = self.finish(r, {"deploy": "n"})
        self.assertEqual(r.rc, 4, text)
        self.assertEqual(self.mutations(), [])
        self.assertFalse((self.home / ".config" / "ham" / "setup").exists())
        self.assertFalse(self.installed().exists())

    def test_enter_alone_is_a_no(self) -> None:
        r = self.tty("--full", *self.GIVEN)
        text = self.finish(r, {"deploy": ""})
        self.assertEqual(r.rc, 4, text)
        self.assertEqual(self.mutations(), [])

    def test_the_end_of_input_at_the_confirmation_is_a_no(self) -> None:
        r = self.tty("--full", *self.GIVEN)
        self.assertEqual(r.converse(stop_at="deploy"), "deploy", r.text)
        r.ctrl_d()
        self.assertEqual(r.finish(), 4, r.text)
        self.assertEqual(self.mutations(), [])

    def test_yes_goes_ahead_and_nothing_more_is_asked(self) -> None:
        r = self.tty("--full", *self.GIVEN)
        self.assertEqual(r.converse(stop_at="deploy"), "deploy", r.text)
        r.send("y")
        self.assertIsNone(r.converse(timeout=limit(60)), "after the confirmation no question may appear:\n" + r.text)
        self.assertEqual(r.finish(timeout=limit(120)), 0, r.text)
        self.assertNotIn("Go ahead?", r.text, "the client-only confirmation is not asked a second time")
        self.assertTrue(self.installed().is_file())
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1)

    def test_yes_and_acknowledge_costs_on_a_terminal_ask_nothing_at_all(self) -> None:
        r = self.tty("--full", "--yes", "--acknowledge-costs", *self.GIVEN)
        self.assertIsNone(r.converse(timeout=limit(60)), r.text)
        self.assertEqual(r.finish(timeout=limit(120)), 0, r.text)
        self.assertNotIn("What do you want to set up?", r.text)

    def test_yes_alone_with_full_on_a_terminal_is_exit_4_not_a_question(self) -> None:
        r = self.tty("--full", "--yes", *self.GIVEN)
        self.assertIsNone(r.converse(), r.text)
        self.assertEqual(r.finish(), 4, r.text)
        self.assertEqual(self.mutations(), [])

    def test_the_profile_menu_and_the_region_question_are_the_ones_of_the_client_setup(self) -> None:
        r = self.tty("--full")
        self.assertEqual(r.converse(stop_at="q2"), "q2", r.text)
        self.assertIn("Question 1: the AWS profile", r.text)
        r.send("work")
        self.assertEqual(r.wait_prompt(), "q3", r.text)
        r.send("")
        self.assertEqual(r.wait_prompt(), "deploy", r.text)
        r.send("n")
        self.assertEqual(r.finish(), 4, r.text)

    def test_a_region_the_stage_does_not_support_is_refused_after_the_question(self) -> None:
        r = self.tty("--full", "--profile", "work")
        self.assertEqual(r.wait_prompt(), "q3", r.text)
        r.send("eu-west-1")
        self.assertEqual(r.finish(), 2, r.text)
        self.assertIn("us-west-2", r.text)
        self.assertEqual(self.mutations(), [])


class BuildRouteCase(FullWizardBase):
    TOOLS = ("cargo-zigbuild", "zig", "node", "docker")

    def test_the_local_route_is_not_offered_without_the_toolchain(self) -> None:
        r = self.tty("--full", *self.GIVEN)
        text = self.finish(r, {"deploy": "n"})
        self.assertNotIn("Where to build", text)

    def test_with_the_whole_toolchain_the_runner_is_the_default_and_the_local_route_is_offered(self) -> None:
        for n in self.TOOLS:
            self.write_stub(n, "#!/bin/sh\nexit 0\n")
        self.write_stub("cargo", STUB_CARGO)
        r = self.tty("--full", *self.GIVEN)
        self.assertEqual(r.converse(stop_at="build"), "build", r.text)
        flat = " ".join(r.text.split())
        self.assertIn("1) The runner in my AWS account", flat)
        self.assertIn("2) This machine", flat)
        r.send("")
        self.assertEqual(r.wait_prompt(), "deploy", r.text)
        r.send("n")
        self.assertEqual(r.finish(), 4, r.text)
        self.assertEqual(self.count_calls("codebuild", "start-build"), 0)

    def test_the_flag_answers_the_question(self) -> None:
        for n in self.TOOLS:
            self.write_stub(n, "#!/bin/sh\nexit 0\n")
        self.write_stub("cargo", STUB_CARGO)
        r = self.tty("--full", "--build-on", "runner", *self.GIVEN)
        self.assertEqual(r.converse(stop_at="deploy"), "deploy", r.text)
        self.assertNotIn("Where to build", r.text)
        r.send("n")
        r.finish()


class TeardownConfirmationCase(FullWizardBase):
    def test_teardown_lists_what_goes_and_asks_one_yes_no(self) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True)
        r = self.tty("--teardown", *self.GIVEN)
        self.assertEqual(r.converse(stop_at="teardown"), "teardown", r.text)
        for needle in ("hamh-direct-control", "hamh-direct-network", self.ACCOUNT, "us-east-1"):
            self.assertIn(needle, r.text)
        self.assertEqual(self.mutations(), [])
        r.send("n")
        self.assertEqual(r.finish(), 4, r.text)
        self.assertEqual(self.mutations(), [])
        self.assertIn("hamh-direct-control", self.world()["stacks"])

    def test_yes_deletes_and_nothing_more_is_asked(self) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True)
        r = self.tty("--teardown", *self.GIVEN)
        self.assertEqual(r.converse(stop_at="teardown"), "teardown", r.text)
        r.send("y")
        self.assertIsNone(r.converse(timeout=limit(60)), r.text)
        self.assertEqual(r.finish(timeout=limit(120)), 0, r.text)
        self.assertEqual([s for s in self.world()["stacks"] if s.startswith("hamh-direct-")], [])


if __name__ == "__main__":
    unittest.main()
