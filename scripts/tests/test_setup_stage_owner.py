"""The owner tag of a stage that exists, in the preflight of `setup.sh --full` and `--update`.

Every resource of a stage carries an `owner` tag. CloudFormation replaces the network stack's DNS query logging configuration
(AWS::Route53Resolver::ResolverQueryLoggingConfig) when its tags change, and the replacement fails, so an update that changes the owner of a stage rolls back after
the build has run for about twenty minutes. The deploy engine keeps the owner of a stage that exists (scripts/lib/deploy.py); setup reads the owner tag in its
preflight and stops, before it creates anything, when the build on this machine would get another value in HAMH_OWNER. The runner in the user's account does not
receive HAMH_OWNER, so a value that differs there is a warning: the stage keeps its owner.
"""

from __future__ import annotations

import unittest

import test_setup_full_flow as flow
import test_setup_full_state as state
from setup_full_support import FullBase

STAGE_OWNER = "keepme"
OTHER = "other-team"
UPDATE = ("--update", "--profile", "work", "--region", "us-east-1", "--yes")
UPDATE_LOCAL = ("--update", "--build-on", "local", "--profile", "work", "--region", "us-east-1", "--yes")


class RunnerRouteOwnerCase(state.StateBase):
    """The default route: the build runs in the user's account and does not receive HAMH_OWNER."""

    def stage(self, owner: str | None = STAGE_OWNER) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True, **({"stage_owner": owner} if owner else {}))
        self.ham_on_path()

    def test_the_update_of_a_stage_runs_with_no_word_about_the_owner_when_ham_owner_is_not_set(self) -> None:
        self.stage()
        p = self.run_full(*UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1)
        self.assertNotIn("HAMH_OWNER", p.stdout + p.stderr)

    def test_a_ham_owner_that_differs_is_a_warning_because_the_runner_does_not_receive_it(self) -> None:
        self.stage()
        p = self.run_full(*UPDATE, HAMH_OWNER=OTHER)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1, "the update runs: the build will keep the owner of the stage")
        warn = [ln for ln in p.stdout.splitlines() if "HAMH_OWNER" in ln]
        self.assertTrue(warn, p.stdout)
        text = " ".join(warn)
        for needle in (OTHER, STAGE_OWNER, "does not receive HAMH_OWNER"):
            self.assertIn(needle, text)

    def test_a_ham_owner_that_equals_the_owner_of_the_stage_is_no_news(self) -> None:
        self.stage()
        p = self.run_full(*UPDATE, HAMH_OWNER=STAGE_OWNER)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("HAMH_OWNER", p.stdout + p.stderr)

    def test_a_stage_without_an_owner_tag_has_nothing_to_compare(self) -> None:
        self.stage(owner=None)
        p = self.run_full(*UPDATE, HAMH_OWNER=OTHER)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("HAMH_OWNER", p.stdout + p.stderr)

    def test_the_owner_tag_is_read_with_one_call_and_nothing_is_changed_by_it(self) -> None:
        self.stage()
        self.run_full(*UPDATE, "--preflight-only", HAMH_OWNER=OTHER)
        reads = [c for c in self.normalized_calls() if c[:2] == ["cloudformation", "describe-stacks"] and "--query" in c and "Key=='owner'" in " ".join(c)]
        self.assertEqual(len(reads), 1, reads)
        self.assertEqual(self.mutations(), [])


class LocalRouteOwnerCase(FullBase):
    """`--build-on local`: scripts/deploy.sh runs on this machine and reads HAMH_OWNER from the environment of setup."""

    TOOLS = flow.FullLocalBuildCase.TOOLS
    DEPLOY = flow.FullLocalBuildCase.DEPLOY
    make_local_checkout = flow.FullLocalBuildCase.make_local_checkout
    tools_on_path = flow.FullLocalBuildCase.tools_on_path

    def stage(self, owner: str | None = STAGE_OWNER, **more) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True, **({"stage_owner": owner} if owner else {}), **more)
        self.make_local_checkout()
        self.tools_on_path(*self.TOOLS)
        self.ham_on_path()

    def test_an_owner_that_differs_stops_the_update_before_anything_is_created_and_says_how_to_go_on(self) -> None:
        self.stage()
        p = self.run_full(*UPDATE_LOCAL, HAMH_OWNER=OTHER, STUB_HAM_SOURCE=str(self.ham_source))
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(self.mutations(), [])
        self.assertEqual(self.log_lines("deploy.log"), [], "scripts/deploy.sh did not run")
        for needle in (STAGE_OWNER, OTHER, f"HAMH_OWNER={STAGE_OWNER}", "owner tag", "DNS query logging configuration", "./setup.sh --teardown"):
            self.assertIn(needle, p.stderr)
        self.assertIn("Nothing was changed.", p.stderr)

    def test_the_refusal_also_ends_a_preflight_only_run(self) -> None:
        self.stage()
        p = self.run_full(*UPDATE_LOCAL, "--preflight-only", HAMH_OWNER=OTHER, STUB_HAM_SOURCE=str(self.ham_source))
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(self.mutations(), [])

    def test_the_owner_of_the_stage_deploys(self) -> None:
        self.stage()
        p = self.run_full(*UPDATE_LOCAL, HAMH_OWNER=STAGE_OWNER, STUB_HAM_SOURCE=str(self.ham_source))
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(len(self.log_lines("deploy.log")), 1)

    def test_no_owner_named_deploys_and_leaves_the_decision_to_the_deploy_engine(self) -> None:
        self.stage()
        p = self.run_full(*UPDATE_LOCAL, STUB_HAM_SOURCE=str(self.ham_source))
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(len(self.log_lines("deploy.log")), 1)

    def test_a_stage_without_an_owner_tag_deploys_with_any_owner(self) -> None:
        self.stage(owner=None)
        p = self.run_full(*UPDATE_LOCAL, HAMH_OWNER=OTHER, STUB_HAM_SOURCE=str(self.ham_source))
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(len(self.log_lines("deploy.log")), 1)

    def test_a_deployed_stage_is_not_deployed_again_by_the_full_setup_so_its_owner_is_not_checked(self) -> None:
        self.stage()
        p = self.run_full("--full", "--build-on", "local", "--profile", "work", "--region", "us-east-1", "--yes", "--acknowledge-costs", HAMH_OWNER=OTHER,
                          STUB_HAM_SOURCE=str(self.ham_source))
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("already deployed", p.stdout)
        self.assertEqual(self.log_lines("deploy.log"), [])

    def test_a_stack_that_rolled_back_on_its_first_create_holds_nothing_and_decides_nothing(self) -> None:
        self.set_scenario(bootstrap_present=True)
        self.make_local_checkout()
        self.tools_on_path(*self.TOOLS)
        w = self.open_world()
        w.w["stacks"]["hamh-direct-network"] = {"status": "ROLLBACK_COMPLETE", "outputs": {}, "tags": {"owner": STAGE_OWNER}}
        w.save()
        p = self.run_full("--full", "--build-on", "local", "--profile", "work", "--region", "us-east-1", "--yes", "--acknowledge-costs", HAMH_OWNER=OTHER,
                          STUB_HAM_SOURCE=str(self.ham_source))
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(len(self.log_lines("deploy.log")), 1)

    def test_an_unfinished_deploy_is_deployed_again_and_so_its_owner_is_checked(self) -> None:
        self.stage(unfinished_deploy=True)
        p = self.run_full("--full", "--build-on", "local", "--profile", "work", "--region", "us-east-1", "--yes", "--acknowledge-costs", HAMH_OWNER=OTHER,
                          STUB_HAM_SOURCE=str(self.ham_source))
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(self.mutations(), [])


if __name__ == "__main__":
    unittest.main()
