"""scripts/teardown.sh --stage direct.

Teardown finds a stage's resources by the name prefix `hamh-<stage>` / `hamh_<stage>` and by the tag pair `project` + `stage`. The world here
is a deployed direct stage next to the foreign resources of the account (the model of test_teardown.py): `--stage direct` lists and
deletes the direct resources only, and the stages the line retired (live, drill) are no longer a choice.
"""

from __future__ import annotations

import json
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_teardown as tt  # noqa: E402


class DirectStage(tt.TeardownCase):
    STAGE = "direct"

    def setUp(self) -> None:
        super().setUp()
        self.put(tt.deployed("direct"))

    def owned(self, state: dict, stage: str) -> list[dict]:
        return [r for r in state["resources"] if (r.get("tags") or {}).get("project") == tt.PROJECT and (r.get("tags") or {}).get("stage") == stage]

    def test_a_direct_dry_run_lists_direct_resources_only(self) -> None:
        proc = self.run_teardown("--stage", "direct", "--dry-run")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        inv = self.inventory(proc)
        self.assertGreater(len(inv), 20)
        for line in inv:
            self.assertNotRegex(line, r"ham[-_]live|/hamh/live|ham[-_]drill|/hamh/drill", line)
        self.assertTrue(any("hamh-direct" in line or "hamh_direct" in line for line in inv))
        planned = [ln for ln in proc.stdout.splitlines() if "plan " in ln]
        self.assertTrue(planned)

    def test_a_dry_run_changes_nothing(self) -> None:
        before = self.get()
        self.run_teardown("--stage", "direct", "--dry-run")
        self.assertEqual(self.get(), before)
        self.assertEqual(self.writes(), [])

    def test_a_full_direct_teardown_removes_every_direct_resource(self) -> None:
        self.assertGreater(len(self.owned(self.get(), "direct")), 30)
        proc = self.full()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertRegex(self.last(proc), r"^TEARDOWN-INVENTORY 0 resources stage=direct kms-pending=\d+$")
        after = self.get()
        # A KMS key stays as a pending deletion (7 days), counted in `kms-pending`; nothing else of the stage is left.
        left = [r for r in self.owned(after, "direct") if not (r["type"] == "kms" and r.get("state") == "PendingDeletion")]
        self.assertEqual(left, [])
        self.assertEqual({s for s in after["stacks"] if s.startswith("hamh-direct-")}, set())
        self.assertTrue(all(d.startswith("hamh-direct-") for d in self.destroys()), self.destroys())

    def test_the_image_build_project_of_stage_direct_goes_with_its_runtimes_stack(self) -> None:
        # review S3: the stage builds its images in hamh-direct-image-build, held by its runtimes stack; the teardown removes it with the
        # stack, by the same name and tag proof as every other resource of the stage
        state = self.get()
        name = "hamh-direct-image-build"
        state["resources"].append({
            "type": "project", "region": "us-west-2", "stack": "hamh-direct-runtimes", "tags": tt.tags("direct"), "name": name,
            "arn": f"arn:aws:codebuild:us-west-2:{tt.ACCOUNT}:project/{name}", "logical": "ImageBuild"})
        self.put(state)
        proc = self.run_teardown("--stage", "direct", "--dry-run")
        inv = self.inventory(proc)
        self.assertTrue(any("project/hamh-direct-image-build" in line for line in inv), inv)
        proc = self.full()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertRegex(self.last(proc), r"^TEARDOWN-INVENTORY 0 resources stage=direct kms-pending=\d+$")
        left = {r.get("name") for r in self.get()["resources"] if r["type"] == "project"}
        self.assertNotIn("hamh-direct-image-build", left)

    def test_direct_is_the_only_stage(self) -> None:
        for stage in ("prod", "live", "drill"):
            proc = self.run_teardown("--stage", stage, "--yes")
            self.assertEqual(proc.returncode, 2, stage)
            self.assertIn("direct", proc.stderr)
        self.assertEqual(self.writes(), [])


if __name__ == "__main__":
    unittest.main()
