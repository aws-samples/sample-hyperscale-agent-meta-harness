"""scripts/teardown.sh while AgentCore holds a network interface in the stage VPC.

The network stack cannot go while AgentCore's interface is in the VPC, and AWS releases it up to 8 hours after the runtime is deleted. The program waits for the release for a
limit the person sets (`--network-wait-minutes N|full`; TEARDOWN_NETWORK_TIMEOUT_S, in seconds, is the default and the test knob; 60 minutes without either), says what it
waits for, and when the limit is reached while only AgentCore's interface holds the network resources, it names the case, the interface id and the command to run again, and ends
with exit 9 instead of the exit 1 of every other leftover.

The world is the one test_teardown.py builds: its `eni-agentcore` (type agentic_ai) is released after `release_after` attempts to delete the network stack, or, when the test sets
`release_looks`, after that many looks at the interfaces of the VPC.
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_teardown as tt  # noqa: E402

HELD = 10_000  # looks and attempts before AWS releases the interface: never, within a test


class HeldBase(tt.TeardownCase):
    def hold(self, after: int = HELD, looks: int | None = None) -> None:
        """AgentCore's interface is released after `after` attempts to delete the network stack, or after `looks` looks at the interfaces of the VPC."""
        state = self.get()
        eni = next(r for r in state["resources"] if r.get("id") == "eni-agentcore")
        eni["release_after"] = after
        if looks is not None:
            eni["release_looks"] = looks
        self.put(state)

    def teardown(self, *flags: str, **env: str):
        self.all_evidence()
        return self.run_teardown("--stage", self.STAGE, "--yes", *flags, **env)

    def network_destroys(self) -> int:
        return self.destroys().count("hamh-direct-network")


class HeldInterfaceCase(HeldBase):
    def test_b686_the_held_interface_is_named_with_its_id_the_8_hour_rule_and_the_command_to_run_again(self) -> None:
        self.hold()
        proc = self.teardown(TEARDOWN_NETWORK_TIMEOUT_S="0")
        flat = " ".join(proc.stdout.split())
        self.assertIn("AgentCore-managed network interface", flat)
        self.assertIn("eni-agentcore", flat)
        self.assertIn("up to 8 hours after the runtime is deleted", flat)
        self.assertIn("about 3 hours", flat)
        self.assertIn("30 to 60 minutes", flat)
        rerun = [ln.strip() for ln in proc.stdout.splitlines() if ln.strip().endswith("scripts/teardown.sh --stage direct --yes")]
        self.assertEqual(len(rerun), 1, proc.stdout[-1500:])
        self.assertEqual(rerun[0], "HAMH_AWS_PROFILE=history-profile scripts/teardown.sh --stage direct --yes")

    def test_b686_the_rerun_command_keeps_the_names_and_the_wait(self) -> None:
        self.hold()
        proc = self.teardown("--names", "new", "--network-wait-minutes", "0")
        self.assertEqual(proc.returncode, 9, proc.stdout + proc.stderr)
        rerun = [ln.strip() for ln in proc.stdout.splitlines() if ln.strip().endswith("scripts/teardown.sh --stage direct --yes --names new --network-wait-minutes 0")]
        self.assertEqual(len(rerun), 1, proc.stdout[-1500:])

    def test_b686_only_the_held_network_resources_left_ends_with_exit_9_and_the_inventory_line_stays_last(self) -> None:
        self.hold()
        proc = self.teardown("--network-wait-minutes", "0")
        self.assertEqual(proc.returncode, 9, proc.stdout + proc.stderr)
        inv = "\n".join(self.inventory(proc))
        self.assertIn("network-interface/eni-agentcore", inv)
        self.assertRegex(self.last(proc), r"^TEARDOWN-INVENTORY \d+ resources$")
        self.assertTrue(self.calls("secretsmanager", "delete-secret"), "the rest of the teardown still ran")

    def test_b686_another_leftover_next_to_the_held_interface_keeps_exit_1(self) -> None:
        self.hold()
        state = self.get()
        next(r for r in state["resources"] if r.get("name") == "hamh-direct-runtime-base")["protected"] = True
        self.put(state)
        proc = self.teardown("--network-wait-minutes", "0")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertIn("AgentCore-managed network interface", proc.stdout)

    def test_b686_another_interface_in_use_next_to_agentcore_s_keeps_exit_1(self) -> None:
        self.hold()
        state = self.get()
        other = dict(next(r for r in state["resources"] if r.get("id") == "eni-agentcore"))
        other.update(id="eni-other", itype="lambda", arn=other["arn"].replace("eni-agentcore", "eni-other"))
        state["resources"].append(other)
        self.put(state)
        proc = self.teardown("--network-wait-minutes", "0")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertIn("Other network interfaces in the VPC are in use as well (eni-other)", " ".join(proc.stdout.split()))

    def test_b686_without_a_held_interface_the_old_behaviour_is_unchanged(self) -> None:
        state = self.get()
        state["resources"] = [r for r in state["resources"] if r.get("id") != "eni-agentcore"]
        self.put(state)
        proc = self.teardown("--network-wait-minutes", "30")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertNotIn("AgentCore-managed", proc.stdout)
        self.assertRegex(self.last(proc), r"^TEARDOWN-INVENTORY 0 resources stage=direct kms-pending=\d+$")
        self.assertEqual(self.network_destroys(), 2, "the detached interface is deleted and the stack is tried again, as before")


class WaitCase(HeldBase):
    def test_b686_the_default_limit_is_the_environment_setting_in_seconds_and_a_flag_beats_it(self) -> None:
        self.hold()
        proc = self.teardown(TEARDOWN_NETWORK_TIMEOUT_S="0")
        self.assertEqual(proc.returncode, 9, proc.stdout + proc.stderr)
        self.assertEqual(self.network_destroys(), 2, "the first attempt and the retry at the end of the run: a limit of 0 seconds does not wait in between")
        self.assertNotIn("waiting for AWS to release", proc.stdout)
        self.put(tt.deployed(self.STAGE))
        self.hold(looks=2)
        proc = self.teardown("--network-wait-minutes", "5", TEARDOWN_NETWORK_TIMEOUT_S="0")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertRegex(self.last(proc), r"^TEARDOWN-INVENTORY 0 resources stage=direct kms-pending=\d+$")

    def test_b686_a_wait_ends_as_soon_as_aws_releases_the_interface_and_the_stack_is_deleted_once_more(self) -> None:
        self.hold(looks=4)
        proc = self.teardown("--network-wait-minutes", "10")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        flat = " ".join(proc.stdout.split())
        self.assertIn("waiting for AWS to release the AgentCore-managed network interface", flat)
        self.assertIn("AWS released the AgentCore-managed network interface", flat)
        self.assertEqual(self.network_destroys(), 2, "the failed attempt, then one attempt after the release: no attempt while the interface is held")
        self.assertNotIn("retrying in", proc.stdout)

    def test_b686_a_wait_that_runs_out_says_how_long_it_waited(self) -> None:
        self.hold()
        proc = self.teardown(TEARDOWN_NETWORK_TIMEOUT_S="0.3", TEARDOWN_RETRY_SLEEP_S="0.05")
        self.assertEqual(proc.returncode, 9, proc.stdout + proc.stderr)
        self.assertRegex(" ".join(proc.stdout.split()), r"waited (0|1) minutes?")
        self.assertLessEqual(self.network_destroys(), 1 + 1, "no attempt while the interface is held")

    def test_b686_a_dry_run_plans_the_wait_and_waits_for_nothing(self) -> None:
        self.hold()
        proc = self.run_teardown("--stage", self.STAGE, "--dry-run", "--network-wait-minutes", "full")
        self.assertNotIn("Traceback", proc.stderr)
        flat = " ".join(proc.stdout.split())
        self.assertIn("480 minutes", flat)
        self.assertEqual(self.destroys(), [])
        self.assertEqual(self.writes(), [])

    def test_b686_a_value_out_of_range_or_no_number_exits_2_before_any_aws_call(self) -> None:
        for value in ("481", "-1", "abc", "1.5", "", "FULL"):
            with self.subTest(value=value):
                proc = self.run_teardown("--stage", self.STAGE, "--yes", "--network-wait-minutes", value)
                self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
                self.assertIn("--network-wait-minutes", proc.stderr)
                self.assertEqual(self.log(), [])
        for flags in (("--network-wait-minutes",), ("--network-wait-minutes", "5", "--network-wait-minutes", "6")):
            with self.subTest(flags=flags):
                proc = self.run_teardown("--stage", self.STAGE, "--yes", *flags)
                self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
                self.assertEqual(self.log(), [])

    def test_b686_a_value_of_the_environment_setting_that_is_no_number_exits_2(self) -> None:
        proc = self.run_teardown("--stage", self.STAGE, "--yes", TEARDOWN_NETWORK_TIMEOUT_S="soon")
        self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
        self.assertIn("TEARDOWN_NETWORK_TIMEOUT_S", proc.stderr)
        self.assertEqual(self.log(), [])


if __name__ == "__main__":
    unittest.main()
