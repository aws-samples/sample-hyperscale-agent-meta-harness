"""The seconds after which a deploy lock nobody renews stops closing the stage are one number in one file, and that file is part of the
source tree a stage is built from.

The stage's API service embeds the number when it is built (`crates/control-api/src/admission.rs`, `STALE_AFTER_S`) and setup reads the same file
(`scripts/setup/runner.sh`, `RN_LOCK_STALE_SECONDS`). A stage counts as running a commit when the git tree ids of the paths of
`digests.DEPLOYABLE_PATHS` are the ones it deployed. A number file outside those paths could change without any deploy: setup would count with the new
number while the API service of the stage kept the old one. So the file lives under one of those paths, both readers name that one file, and a change
of the number alone changes the source tree.
"""

from __future__ import annotations

import os
import re
import subprocess  # nosec B404: tests start git with an argument list, never through a shell
import sys
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "scripts" / "lib"))
import digests  # noqa: E402

NUMBER = "crates/control-api/deploy-lock-stale-seconds.txt"
EARLIER = "scripts/lib/deploy-lock-stale-seconds.txt"
ADMISSION = "crates/control-api/src/admission.rs"
RUNNER = "scripts/setup/runner.sh"


def read(rel: str) -> str:
    return (REPO / rel).read_text(encoding="utf-8")


def git(repo: Path, *args: str) -> str:
    env = dict(os.environ, GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_SYSTEM=os.devnull, GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@example.invalid",
               GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@example.invalid")
    p = subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=True, env=env)  # nosec B603: argument list; B607: git is found on the PATH of the host
    if p.returncode != 0:
        raise AssertionError(f"git {' '.join(args)}: {p.stderr}")
    return p.stdout.strip()


class LockNumberCase(unittest.TestCase):
    def test_b841_number_the_file_lies_under_a_path_the_source_tree_of_a_stage_covers(self) -> None:
        self.assertEqual(read(NUMBER), "1800\n", "one integer line")
        self.assertTrue(any(NUMBER.startswith(root + "/") for root in digests.DEPLOYABLE_PATHS),
                        f"{NUMBER} is under none of {digests.DEPLOYABLE_PATHS}: a change of the number alone would deploy nothing")
        self.assertFalse((REPO / EARLIER).exists(), "the number has one file, and the earlier place holds no second copy")

    def test_b841_number_setup_and_the_api_service_read_that_one_file(self) -> None:
        line = next(ln for ln in read(RUNNER).splitlines() if ln.startswith("RN_LOCK_STALE_SECONDS="))
        self.assertIn('"${SELF_DIR:-.}/' + NUMBER + '"', line, "setup reads the file: " + line)
        self.assertTrue(line.rstrip().endswith(" " + read(NUMBER).strip() + ")"), "and a partial copy of the tree without the file gets the same number: " + line)
        embedded = re.search(r'pub const STALE_AFTER_S: u64 = seconds\(include_str!\(\s*"([^"]+)"\s*\)\);', read(ADMISSION))
        self.assertIsNotNone(embedded, "the API service embeds one file for STALE_AFTER_S")
        path = os.path.normpath(os.path.join(os.path.dirname(ADMISSION), embedded.group(1)))
        self.assertEqual(path, NUMBER, "the API service embeds the file setup reads")
        for text in (read(RUNNER), read(ADMISSION), read("scripts/lib/deploy.py")):
            self.assertNotIn(EARLIER, text, "no reader and no comment names the earlier place")

    def test_b841_number_a_change_of_the_number_alone_changes_the_source_tree(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp) / "repo"
            target = repo / NUMBER
            target.parent.mkdir(parents=True)
            git(repo, "init", "-q")
            target.write_text("1800\n", encoding="utf-8")
            git(repo, "add", "-A")
            git(repo, "commit", "-q", "-m", "the number")
            before = digests.source_tree(repo)
            target.write_text("900\n", encoding="utf-8")
            git(repo, "add", "-A")
            git(repo, "commit", "-q", "-m", "another number")
            self.assertNotEqual(digests.source_tree(repo), before, "the stage of the first commit does not run the second one")


if __name__ == "__main__":
    unittest.main()
