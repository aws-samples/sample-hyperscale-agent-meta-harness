"""The root VERSION file: the one source of the `ham vN.NN | backend vN.NN` tag.

The client (`crates/ham`) and the backend (`crates/control-api`) each embed the file at build time. The file is
not an input of the worker image key or the deployment digest, so a version bump alone moves neither.
"""

from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "scripts" / "lib"))
import digests  # noqa: E402


class VersionFileTest(unittest.TestCase):
    def test_version_file_is_major_dot_two_digit_counter(self):
        text = (REPO / "VERSION").read_text()
        self.assertRegex(text, r"\A[0-9]+\.[0-9]{2}\n\Z")

    def test_version_file_is_no_input_of_the_image_key_or_the_digest(self):
        inputs = list(digests.IMAGE_PATHS) + list(digests.DEPLOYABLE_PATHS)
        self.assertNotIn("VERSION", inputs)

    def test_root_agents_names_the_version_rule(self):
        text = (REPO / "AGENTS.md").read_text()
        self.assertIsNotNone(re.search(r"^6\. .*`VERSION`", text, re.M))


if __name__ == "__main__":
    unittest.main()
