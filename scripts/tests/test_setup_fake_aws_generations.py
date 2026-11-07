"""The stand-in AWS CLI holds the stage of the previous names next to the stage of the current names (COMPAT-CAH-STAGE: the tests of the teardown by generation use it).

setup_fake_aws.py derives the previous generation from the current one (`leg`, `gen_text`): the first word of every name and the project tag change, and the ids that two
stages of one account cannot share (a VPC, a key, a runtime id) get their own. These cases pin that world; they call no script.
"""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

import setup_fake_aws as fake


def world(**scenario) -> fake.World:
    tmp = Path(tempfile.mkdtemp(prefix="fake-gen-"))
    path = tmp / "scenario.json"
    path.write_text(json.dumps(scenario))
    return fake.World(tmp, path)


class NamesCase(unittest.TestCase):
    def test_leg_translates_names_ids_and_the_project_tag(self) -> None:
        self.assertEqual(fake.leg("hamh-direct-network"), "cah-direct-network")
        self.assertEqual(fake.leg("/hamh/direct/deployment-digest"), "/cah/direct/deployment-digest")
        self.assertEqual(fake.leg("hamh_direct_worker_python-abcde12345"), "cah_direct_worker_python-abcde54321")
        self.assertEqual(fake.leg(fake.PROJECT_TAGS), {"project": "cline-agentcore-harness", "stage": "direct"})
        self.assertEqual(fake.leg(("a", ["hamh-x"])), ("a", ["cah-x"]))
        self.assertEqual(fake.gen_text("hamh-direct-network", "current"), "hamh-direct-network")

    def test_the_two_generations_share_no_physical_id(self) -> None:
        cur = fake.stage_model(True, fake.ACCOUNT, "us-east-1", "current")
        old = fake.stage_model(True, fake.ACCOUNT, "us-east-1", "legacy")
        ids = lambda m: {rid for spec in m.values() for _a, _t, rid in spec["resources"]}  # noqa: E731
        self.assertEqual(set(cur), set(old))
        self.assertEqual(ids(cur) & ids(old), set(), "no resource id appears in both stages")
        self.assertTrue(all(rid.startswith("arn:") or "cah" in rid or rid in ("key-leg0001", "leg123", "vpc-0leg1234", "subnet-0leg1234", "sg-0leg1234") for rid in ids(old)), ids(old))


class WorldCase(unittest.TestCase):
    def test_a_legacy_stage_has_the_previous_names_tags_and_parameters(self) -> None:
        w = world(legacy_stage_deployed=True, nine_stacks=True)
        names = sorted(w.w["stacks"])
        self.assertEqual(names, sorted(f"cah-direct-{n}" for n in ("network", "security", "data", "packages", "control", "runtimes", "orchestration", "delivery", "observability")))
        for st in w.w["stacks"].values():
            self.assertEqual(st["tags"], {"project": "cline-agentcore-harness", "stage": "direct"})
        self.assertIn("/cah/direct/deployment-digest", w.w["params"])
        self.assertEqual(w.w["param_tags"]["/cah/direct/deployment-digest"]["project"], "cline-agentcore-harness")
        self.assertIn(f"cah-direct-gomod-{fake.ACCOUNT}", w.w["buckets"])
        self.assertEqual(w.w["bucket_tags"][f"cah-direct-gomod-{fake.ACCOUNT}"]["project"], "cline-agentcore-harness")
        self.assertIn("vpc-0leg1234", w.w["ec2"]["vpcs"])
        self.assertEqual(w.w["ec2"]["vpcs"]["vpc-0leg1234"]["tags"]["aws:cloudformation:stack-name"], "cah-direct-network")
        self.assertTrue(w.w["stacks"]["cah-direct-control"]["outputs"]["V1ApiUrl"].startswith("https://leg123.execute-api."))

    def test_both_stages_live_in_one_world(self) -> None:
        w = world(stage_deployed=True, legacy_stage_deployed=True, nine_stacks=True, legacy_runner_present=True, runner_present=True)
        stacks = set(w.w["stacks"])
        self.assertIn("hamh-direct-control", stacks)
        self.assertIn("cah-direct-control", stacks)
        self.assertIn("hamh-setup-runner", stacks)
        self.assertIn("cah-setup-runner", stacks)
        self.assertEqual(w.w["stacks"]["cah-setup-runner"]["outputs"]["SetupBucket"], f"cah-setup-{fake.ACCOUNT}-us-east-1")
        self.assertEqual(w.w["stacks"]["cah-setup-runner"]["tags"]["project"], "cline-agentcore-harness")
        self.assertEqual(w.w["stacks"]["hamh-setup-runner"]["tags"]["project"], "hyperscale-agent-meta-harness")
        self.assertEqual(len(w.w["ec2"]["vpcs"]), 2)
        self.assertNotEqual(set(w.w["runtimes"]), set(), "the runtimes of both stages")
        self.assertEqual(len(w.w["runtimes"]), 4)

    def test_foreign_stacks_pair_a_name_with_the_other_generations_tag(self) -> None:
        w = world(foreign_stacks=[{"name": "cah-direct-network", "tags": "current"}, {"name": "hamh-direct-data", "tags": "legacy", "status": "UPDATE_COMPLETE"}])
        self.assertEqual(w.w["stacks"]["cah-direct-network"]["tags"]["project"], "hyperscale-agent-meta-harness")
        self.assertEqual(w.w["stacks"]["hamh-direct-data"]["tags"]["project"], "cline-agentcore-harness")
        self.assertEqual(w.w["stacks"]["hamh-direct-data"]["status"], "UPDATE_COMPLETE")

    def test_the_legacy_gomod_policy_names_the_legacy_runner_role(self) -> None:
        w = world(legacy_stage_deployed=True, nine_stacks=True)
        patterns = w.gomod_delete_patterns("legacy")
        self.assertIn(f"arn:aws:iam::{fake.ACCOUNT}:role/cah-setup-runner-role", patterns)
        self.assertNotIn(f"arn:aws:iam::{fake.ACCOUNT}:role/hamh-setup-runner-role", patterns)
        self.assertIn(f"arn:aws:iam::{fake.ACCOUNT}:role/hamh-setup-runner-role", w.gomod_delete_patterns("current"))

    def test_a_legacy_logging_record_is_written_at_the_legacy_path(self) -> None:
        w = world(legacy_stage_deployed=True, nine_stacks=True, legacy_logging_record={"loggingConfig": None})
        self.assertIn("/cah/direct/invocation-logging-prior", w.w["params"])
        self.assertNotIn("/hamh/direct/invocation-logging-prior", w.w["params"])

    def test_the_stage_stack_generation_is_told_by_the_name(self) -> None:
        self.assertEqual(fake.World.stage_stack_gen("hamh-direct-network"), "current")
        self.assertEqual(fake.World.stage_stack_gen("cah-direct-network"), "legacy")
        self.assertIsNone(fake.World.stage_stack_gen("other-app"))
        self.assertEqual(fake.World.runner_stack_gen("cah-setup-runner"), "legacy")
        self.assertEqual(fake.World.gomod_bucket_gen("cah-direct-gomod-111122223333"), "legacy")


if __name__ == "__main__":
    unittest.main()
