# COMPAT-CAH-STAGE: this file tests the teardown of a stage deployed under the previous names, which scripts/lib/teardown.py still removes for the window of those names.
"""scripts/teardown.sh and the two generations of names.

`--names new|old|both` (default both) selects the generations a run covers. A generation owns a name only when the name prefix and the project tag are both its own; a
mixed pair is foreign, is named on a `kept:` line and is never touched. The stage under the previous names is removed through CloudFormation (the CDK app no longer
defines it). The zero-resource line is printed once, and only when every selected generation reads zero.

The world is the one test_teardown.py builds (`deployed`), written out again under the other generation's names.

Run with `python3 -B -m unittest discover -s scripts/tests -p test_teardown_generations.py`.
"""

from __future__ import annotations

import json
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_teardown as tt  # noqa: E402

NEW_PROJECT = tt.PROJECT
OLD_PROJECT = "cline-agentcore-harness"


def previous(world: dict) -> dict:
    """The world of test_teardown.py as a stage deployed under the previous names: every name, path and project tag of the current generation, in the other."""
    text = json.dumps(world).replace(NEW_PROJECT, OLD_PROJECT).replace("hamh", "cah")
    return json.loads(text)


class PreviousStage(tt.TeardownCase):
    """A world that holds only the stage under the previous names."""

    def setUp(self) -> None:
        super().setUp()
        self.put(previous(tt.deployed(self.STAGE)))

    def all_evidence(self) -> None:
        """The evidence copies the teardown wants before it deletes an E2E repository: the repositories of this world carry the previous prefix."""
        for repo in ("cah-e2e-python-calc-20260925", "cah-e2e-new-calc-%s" % self.STAGE):
            self.copy_repo_evidence(repo)
        for run in ("r-e2e", "r-new"):
            self.copy_run_evidence(run)

    def go(self, *args: str, **env: str):
        self.all_evidence()
        return self.run_teardown("--stage", "direct", *args, **env)

    def old_stacks(self) -> list[str]:
        return sorted(n for n in self.get()["stacks"] if n.startswith("cah-direct-"))

    def zero_lines(self, proc) -> list[str]:
        return [line for line in proc.stdout.splitlines() if line.startswith("TEARDOWN-INVENTORY")]

    def test_names_old_removes_the_previous_stage_through_cloudformation(self) -> None:
        proc = self.go("--names", "old", "--yes")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.old_stacks(), [])
        self.assertEqual(self.destroys(), [], "the CDK app does not define the previous names: no cdk destroy")
        deleted = [c["args"][c["args"].index("--stack-name") + 1] for c in self.calls("cloudformation", "delete-stack")]
        self.assertIn("cah-direct-network", deleted)
        self.assertIn("cah-direct-runtimes", deleted)
        self.assertLess(deleted.index("cah-direct-orchestration"), deleted.index("cah-direct-control"))
        self.assertLess(deleted.index("cah-direct-control"), deleted.index("cah-direct-network"), "the network goes last")
        self.assertEqual(self.zero_lines(proc), ["TEARDOWN-INVENTORY 0 resources stage=direct kms-pending=%s" % self.zero_lines(proc)[0].rsplit("=", 1)[1]])
        self.assertEqual(self.inventory(proc), [])

    def test_names_old_restores_the_invocation_logging_from_the_previous_parameter_path(self) -> None:
        proc = self.go("--names", "old", "--yes")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.names("ssm") & {"/cah/direct/deployment-digest", "/cah/direct/image-digest"}, set())
        reads = [c["args"] for c in self.log() if c["args"][:2] == ["ssm", "get-parameter"]]
        self.assertIn("/cah/direct/deployment-digest", [a[a.index("--name") + 1] for a in reads])

    def test_the_default_covers_both_generations_and_prints_one_zero_line(self) -> None:
        proc = self.go("--yes")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.old_stacks(), [])
        self.assertEqual(len(self.zero_lines(proc)), 1, proc.stdout)
        self.assertTrue(self.zero_lines(proc)[0].startswith("TEARDOWN-INVENTORY 0 resources stage=direct"), proc.stdout)

    def test_names_new_leaves_the_previous_stage_alone_and_says_so(self) -> None:
        proc = self.go("--names", "new", "--yes")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(len(self.old_stacks()), 9 + 1, "the previous stage (nine stacks and the one the app never defined) is untouched")
        self.assertEqual(self.writes(), [], "no write call at all")
        self.assertIn("previous names were not covered", proc.stdout)

    def test_a_dry_run_of_both_names_changes_nothing_and_plans_the_previous_stage(self) -> None:
        proc = self.run_teardown("--stage", "direct", "--dry-run")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertEqual(self.writes(), [])
        self.assertIn("plan delete-stack cah-direct-network", proc.stdout)

    def test_the_evidence_of_the_previous_generation_has_its_own_name(self) -> None:
        proc = self.go("--names", "old", "--yes")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        names = sorted(p.name for p in self.evidence.glob("*.json"))
        self.assertEqual(len(names), 1, names)
        self.assertTrue(names[0].endswith("-teardown-direct-previous-names.json"), names)
        rec = json.loads((self.evidence / names[0]).read_text())
        self.assertEqual(rec["inputs"]["names"], "old")


class CurrentStagePlusPrevious(tt.TeardownCase):
    """The ordinary world (current names) with a previous generation that is clean, or has a stack that a mixed pair makes foreign."""

    def test_a_clean_previous_generation_adds_no_evidence_file_and_no_second_zero_line(self) -> None:
        proc = self.full()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(len([ln for ln in proc.stdout.splitlines() if ln.startswith("TEARDOWN-INVENTORY")]), 1, proc.stdout)
        self.assertEqual(len(list(self.evidence.glob("*-teardown-direct.json"))), 1)
        self.assertEqual(list(self.evidence.glob("*previous-names*")), [])
        self.assertIn("previous names: nothing found", proc.stdout)

    def test_a_stack_with_one_prefix_and_the_other_project_tag_is_kept_and_never_touched(self) -> None:
        state = self.get()
        state["stacks"]["cah-direct-mixed"] = {"status": "CREATE_COMPLETE", "region": "us-west-2", "tags": tt.tags("direct"),
                                               "arn": "arn:aws:cloudformation:us-west-2:%s:stack/cah-direct-mixed/uuid-m" % tt.ACCOUNT}
        state["stacks"]["hamh-direct-mixed"] = {"status": "CREATE_COMPLETE", "region": "us-west-2",
                                                "tags": {**tt.tags("direct"), "project": OLD_PROJECT},
                                                "arn": "arn:aws:cloudformation:us-west-2:%s:stack/hamh-direct-mixed/uuid-n" % tt.ACCOUNT}
        self.put(state)
        proc = self.full()
        stacks = self.get()["stacks"]
        self.assertIn("cah-direct-mixed", stacks)
        self.assertIn("hamh-direct-mixed", stacks)
        deleted = [c["args"][c["args"].index("--stack-name") + 1] for c in self.calls("cloudformation", "delete-stack")]
        self.assertNotIn("cah-direct-mixed", deleted)
        self.assertNotIn("hamh-direct-mixed", deleted)
        self.assertIn("kept: cah-direct-mixed", proc.stdout)
        self.assertIn("kept: hamh-direct-mixed", proc.stdout)
        self.assertNotIn("TEARDOWN-INVENTORY 0 resources", proc.stdout, "a foreign stack in the account is no reason for the zero line, and no reason to touch it")

    def test_a_failed_read_makes_the_run_incomplete_and_prints_no_zero_line(self) -> None:
        proc = self.full(FAKE_FAIL_OP="cloudformation list-stacks")
        self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertNotIn("TEARDOWN-INVENTORY 0 resources", proc.stdout)

    def test_names_must_be_new_old_or_both(self) -> None:
        proc = self.run_teardown("--stage", "direct", "--names", "other", "--dry-run")
        self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
        self.assertEqual(self.log(), [], "no AWS call before the usage error")


if __name__ == "__main__":
    unittest.main()
