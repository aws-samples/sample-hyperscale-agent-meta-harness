"""Image-key neutrality (scripts/lib/digests.py image_key), enforced on this workspace.

The worker image key hashes the git trees of agentd's local crate closure, so a change to a crate
outside it (the Lambda crates control-api, dispatcher, worker-api, run-session) reuses the image. A
dependency edge from agentd to one of those crates would silently change the key with every edit to
it; this test fails on such an edge.
"""

from __future__ import annotations

import json
import shutil
import subprocess  # nosec B404: tests start the scripts under test and git with argument lists, never a shell
import sys
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "scripts" / "lib"))
import digests  # noqa: E402

LAMBDA_CRATES = {"control-api", "dispatcher", "worker-api", "run-session"}
WORKER_BINARIES = ("agentd",)


@unittest.skipUnless(shutil.which("cargo"), "cargo is not on PATH (CI sources env.sh)")
class WorkerImageClosure(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        proc = subprocess.run(["cargo", "metadata", "--format-version", "1", "--offline"], cwd=REPO,  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
                              capture_output=True, text=True)
        if proc.returncode != 0:
            raise AssertionError(f"cargo metadata failed: {proc.stderr[-400:]}")
        cls.meta = json.loads(proc.stdout)

    def test_the_workers_binaries_depend_on_none_of_the_lambda_crates(self):
        for package in WORKER_BINARIES:
            names = {Path(d).name for d in digests.local_crate_closure(self.meta, package)}
            self.assertIn(package, names)
            self.assertEqual(names & LAMBDA_CRATES, set(), f"{package} builds from {sorted(names & LAMBDA_CRATES)}")


if __name__ == "__main__":
    unittest.main()
