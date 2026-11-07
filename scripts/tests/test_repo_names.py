"""The repository-name rule of a stage (scripts/lib/aws_target.py).

A stage lets a run open a pull request only in a CodeCommit repository whose name the stage's rule covers (and that the operator allowed). The rule is the optional
`repoNames` of the account-file entry: a list of 1 to 8 entries, each an exact repository name, such a prefix followed by one `*`, or `*`. The environment variable
`HAMH_REPO_NAMES` (comma separated) wins for one run. Without either the rule is the default `hamh-e2e-*`.

- the words are one table, infra/test/fixtures/repo-names.json, which infra/test/repo-names.test.ts runs through the TypeScript rules (infra/lib/repo-names.ts); this
  file runs it through scripts/lib/aws_target.py, so the two parsers cannot drift apart;
- `aws_target.py get repoNames` prints the list comma separated, the form the CDK app and the Lambdas read as `HAMH_REPO_NAMES`;
- an entry that is not selected is checked as well, and a file without `repoNames` behaves as before.
"""

from __future__ import annotations

import json
import os
import re
import subprocess  # nosec B404: tests start the script under test with argument lists, never a shell
import sys
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "scripts" / "lib"))
import aws_target  # noqa: E402

TABLE = json.loads((REPO / "infra" / "test" / "fixtures" / "repo-names.json").read_text())
EXAMPLE = json.loads((REPO / "fixtures" / "setup" / "accounts-generated.json").read_text())


def write_accounts(entry: dict) -> Path:
    d = Path(tempfile.mkdtemp(prefix="repo-names-"))
    p = d / "accounts.json"
    p.write_text(json.dumps({**EXAMPLE, "accounts": [{**EXAMPLE["accounts"][0], **entry}]}))
    return p


def cli(*args: str, accounts: Path, names: str | None = None) -> subprocess.CompletedProcess:
    env = {k: v for k, v in os.environ.items() if k not in ("HAMH_AWS_PROFILE", "HAMH_ACCOUNTS_FILE", "HAMH_REPO_NAMES")}
    env["HAMH_ACCOUNTS_FILE"] = str(accounts)
    if names is not None:
        env["HAMH_REPO_NAMES"] = names
    return subprocess.run([sys.executable, "-B", str(REPO / "scripts" / "lib" / "aws_target.py"), *args], capture_output=True, text=True, env=env)  # nosec B603: argument list, no shell, the interpreter and a repository script


class SharedTable(unittest.TestCase):
    def test_the_default_is_the_prefix_of_the_authors_end_to_end_repositories(self) -> None:
        self.assertEqual(list(aws_target.DEFAULT_REPO_NAMES), ["hamh-e2e-*"])
        self.assertEqual(TABLE["default"], list(aws_target.DEFAULT_REPO_NAMES))

    def test_every_accepted_entry_gives_the_rule_of_the_table(self) -> None:
        self.assertGreaterEqual(len(TABLE["accept"]), 9)
        for row in TABLE["accept"]:
            with self.subTest(row["why"]):
                (target,) = aws_target.load(write_accounts(row["entry"]))
                self.assertEqual(aws_target.repo_names_of(target, {}), row["rule"])

    def test_every_refused_entry_is_refused_with_the_reason_of_the_table_and_the_place(self) -> None:
        self.assertGreaterEqual(len(TABLE["refuse"]), 14)
        for row in TABLE["refuse"]:
            with self.subTest(row["why"]):
                with self.assertRaises(aws_target.TargetError) as caught:
                    aws_target.load(write_accounts(row["entry"]))
                self.assertRegex(str(caught.exception), "^accounts\\[0\\]: " + re.escape(row["reason"]))

    def test_an_entry_that_is_not_selected_is_checked_as_well(self) -> None:
        second = {**EXAMPLE["accounts"][0], "profile": "other", "role": "test", "account": "444455556666", "repoNames": ["bad name"]}
        p = Path(tempfile.mkdtemp(prefix="repo-names-")) / "accounts.json"
        p.write_text(json.dumps({**EXAMPLE, "accounts": [EXAMPLE["accounts"][0], second]}))
        with self.assertRaisesRegex(aws_target.TargetError, "accounts\\[1\\]: repoNames entry"):
            aws_target.load(p)

    def test_ham_repo_names_wins_over_the_entry_for_every_row_of_the_table(self) -> None:
        self.assertEqual(aws_target.REPO_NAMES_ENV, "HAMH_REPO_NAMES")
        for row in TABLE["env"]:
            with self.subTest(row["why"]):
                (target,) = aws_target.load(write_accounts(row["entry"]))
                self.assertEqual(aws_target.repo_names_of(target, {"HAMH_REPO_NAMES": row["env"]}), row["rule"])

    def test_an_empty_ham_repo_names_is_unset(self) -> None:
        (target,) = aws_target.load(write_accounts({"repoNames": ["a-*"]}))
        self.assertEqual(aws_target.repo_names_of(target, {"HAMH_REPO_NAMES": ""}), ["a-*"])
        self.assertEqual(aws_target.repo_names_of(target, {}), ["a-*"])

    def test_a_ham_repo_names_that_is_not_a_rule_is_a_refusal_that_names_the_variable(self) -> None:
        self.assertGreaterEqual(len(TABLE["envRefuse"]), 5)
        for row in TABLE["envRefuse"]:
            with self.subTest(row["why"]):
                (target,) = aws_target.load(write_accounts(row["entry"]))
                with self.assertRaises(aws_target.TargetError) as caught:
                    aws_target.repo_names_of(target, {"HAMH_REPO_NAMES": row["env"]})
                self.assertRegex(str(caught.exception), "^HAMH_REPO_NAMES: " + re.escape(row["reason"]))

    def test_every_cover_row_agrees_with_the_matcher(self) -> None:
        self.assertGreaterEqual(len(TABLE["cover"]), 5)
        for row in TABLE["cover"]:
            for name in row["allows"]:
                with self.subTest(row["why"], allows=name):
                    self.assertTrue(aws_target.repo_name_allowed(row["rule"], name))
            for name in row["refuses"]:
                with self.subTest(row["why"], refuses=name):
                    self.assertFalse(aws_target.repo_name_allowed(row["rule"], name))


class Command(unittest.TestCase):
    def test_get_repo_names_prints_the_list_comma_separated(self) -> None:
        p = cli("get", "repoNames", accounts=write_accounts({"repoNames": ["my-app", "team-*"]}))
        self.assertEqual((p.returncode, p.stdout, p.stderr), (0, "my-app,team-*\n", ""))

    def test_get_repo_names_of_a_file_without_the_field_is_the_default(self) -> None:
        p = cli("get", "repoNames", accounts=write_accounts({}))
        self.assertEqual((p.returncode, p.stdout, p.stderr), (0, "hamh-e2e-*\n", ""))

    def test_get_repo_names_follows_the_environment_variable(self) -> None:
        p = cli("get", "repoNames", accounts=write_accounts({"repoNames": ["a-*"]}), names="b-*, c")
        self.assertEqual((p.returncode, p.stdout, p.stderr), (0, "b-*,c\n", ""))

    def test_a_bad_environment_variable_is_a_refusal_with_nothing_on_stdout(self) -> None:
        p = cli("get", "repoNames", accounts=write_accounts({}), names="a,,b")
        self.assertEqual((p.returncode, p.stdout), (2, ""))
        self.assertIn("aws_target: HAMH_REPO_NAMES: repoNames entry \"\"", p.stderr)

    def test_a_bad_file_is_a_refusal_with_nothing_on_stdout(self) -> None:
        p = cli("get", "repoNames", accounts=write_accounts({"repoNames": "my-app"}))
        self.assertEqual((p.returncode, p.stdout), (2, ""))
        self.assertIn("accounts[0]: repoNames must be a list of 1 to 8", p.stderr)

    def test_the_committed_account_file_still_loads_and_names_the_default_rule(self) -> None:
        for t in aws_target.load(REPO / "accounts.json"):
            self.assertEqual(aws_target.repo_names_of(t, {}), ["hamh-e2e-*"])


if __name__ == "__main__":
    unittest.main()
