"""The shared accounts fixture: `fixtures/accounts-three.json` and the helper `accounts_fixture.py` agree, hold placeholders only and load through the scripts' own reader."""

from __future__ import annotations

import json
import os
import subprocess  # nosec B404: tests start the reader under test with an argument list, never a shell
import sys
import tempfile
import unittest
from pathlib import Path

import accounts_fixture as fx

REPO = Path(__file__).resolve().parents[2]


class SharedFixture(unittest.TestCase):
    def test_the_file_is_the_document_of_the_helper(self) -> None:
        self.assertEqual(json.loads(fx.FIXTURE.read_text(encoding="utf-8")), fx.document())
        self.assertEqual(fx.FIXTURE.read_text(encoding="utf-8"), json.dumps(fx.document(), indent=2) + "\n")

    def test_three_entries_with_the_three_roles_and_the_placeholder_values(self) -> None:
        doc = json.loads(fx.FIXTURE.read_text(encoding="utf-8"))
        self.assertEqual([e["role"] for e in doc["accounts"]], ["history", "active", "test"])
        self.assertEqual([e["profile"] for e in doc["accounts"]], ["history-profile", "my-profile", "test-profile"])
        self.assertEqual([e["account"] for e in doc["accounts"]], ["111111111111", "222222222222", "333333333333"])
        self.assertEqual(doc["selector"], "HAMH_AWS_PROFILE")

    def test_the_writer_gives_an_absolute_path_with_the_bytes_of_the_file(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            p = fx.accounts_three(d)
            self.assertTrue(p.is_absolute())
            self.assertEqual(p.read_bytes(), fx.FIXTURE.read_bytes())
            q = fx.write_accounts(d, "variant.json", entries=(fx.ACTIVE,))
            self.assertTrue(q.is_absolute())
            self.assertEqual([e["profile"] for e in json.loads(q.read_text())["accounts"]], ["my-profile"])

    def test_the_scripts_reader_accepts_the_file_and_selects_by_profile(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            p = fx.accounts_three(d)
            env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HAMH_ACCOUNTS_FILE": str(p), "LC_ALL": "C"}
            def get(profile: str = "") -> str:
                e = dict(env)
                if profile:
                    e["HAMH_AWS_PROFILE"] = profile
                r = subprocess.run([sys.executable, "-B", str(REPO / "scripts" / "lib" / "aws_target.py"), "get", "account"], capture_output=True, text=True, env=e, timeout=60)  # nosec B603: argument list, no shell
                self.assertEqual(r.returncode, 0, r.stderr)
                return r.stdout.strip()
            self.assertEqual(get(), "222222222222")
            self.assertEqual(get("history-profile"), "111111111111")
            self.assertEqual(get("test-profile"), "333333333333")


if __name__ == "__main__":
    unittest.main()
