"""The operator identity of an accounts.json entry.

A stage deployed into a stranger's account trusted an IAM role named `Admin` as its operator, so a caller that is an IAM user, an IAM Identity Center role
(AWSReservedSSO_<permission set>_<hash>) or another role was refused by its own stage, and the deploy could not even create the roles that name `Admin` as a principal. The
optional field `operator` of an entry names the identity instead:

- the words are one table, infra/test/fixtures/operator-identity.json, which infra/test/operator.test.ts runs through the TypeScript rules (infra/lib/operator.ts);
  this file runs it through scripts/lib/aws_target.py, so the two parsers cannot drift apart;
- an entry without the field is what it always was: no operator, the committed accounts.json names none, and the deploy engine's caller check looks at the account only;
- an entry with the field makes the deploy engine's caller check accept that identity (and the setup runner of the account, which deploys with its own role), and
  refuse any other with a `DEPLOY-FAIL caller` line that names the identity the accounts file asks for and the caller it found.
"""

from __future__ import annotations

import json
import os
import subprocess  # nosec B404: tests start the script under test with argument lists, never a shell
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "scripts" / "lib"))
import aws_target  # noqa: E402
import deploy  # noqa: E402

TABLE = json.loads((REPO / "infra" / "test" / "fixtures" / "operator-identity.json").read_text())
EXAMPLE = json.loads((REPO / "fixtures" / "setup" / "accounts-generated.json").read_text())
ACCOUNT = "1111" + "2222" + "3333"
OTHER = "4444" + "5555" + "6666"


def fill(value):
    """A table value with the account placeholders filled in."""
    return json.loads(json.dumps(value).replace("{account}", ACCOUNT).replace("{other}", OTHER))


def write_accounts(op=None, *, has_operator: bool = False) -> Path:
    entry = dict(EXAMPLE["accounts"][0])
    if has_operator or op is not None:
        entry["operator"] = op
    d = Path(tempfile.mkdtemp(prefix="operator-identity-"))
    p = d / "accounts.json"
    p.write_text(json.dumps({**EXAMPLE, "accounts": [entry]}))
    return p


def cli(*args: str, accounts: Path | None = None) -> subprocess.CompletedProcess:
    env = {k: v for k, v in os.environ.items() if k not in ("HAMH_AWS_PROFILE", "HAMH_ACCOUNTS_FILE")}
    if accounts is not None:
        env["HAMH_ACCOUNTS_FILE"] = str(accounts)
    return subprocess.run([sys.executable, str(REPO / "scripts" / "lib" / "aws_target.py"), *args], capture_output=True, text=True, env=env)  # nosec B603: argument list, no shell, the interpreter and a repository script


class SharedTable(unittest.TestCase):
    def test_every_accepted_word_parses_to_the_identity_it_names(self) -> None:
        self.assertGreaterEqual(len(TABLE["accept"]), 5)
        for row in TABLE["accept"]:
            with self.subTest(row["label"]):
                op = aws_target.parse_operator(row["operator"], "accounts[0]")
                self.assertEqual((op.kind, op.name), (row["operator"]["kind"], row["operator"]["name"]))
                self.assertEqual(op.label, row["label"])

    def test_every_refused_word_is_refused_with_the_reason_of_the_table_and_the_place_in_the_file(self) -> None:
        self.assertGreaterEqual(len(TABLE["refuse"]), 15)
        for row in TABLE["refuse"]:
            with self.subTest(row["why"]):
                with self.assertRaises(aws_target.TargetError) as cm:
                    aws_target.parse_operator(fill(row["operator"]), "accounts.json accounts[2]")
                self.assertTrue(str(cm.exception).startswith("accounts.json accounts[2]: " + row["reason"]), str(cm.exception))

    def test_the_longest_accepted_name_has_64_characters_and_a_65_character_name_is_refused(self) -> None:
        self.assertEqual(max(len(r["operator"]["name"]) for r in TABLE["accept"]), 64)
        long = next(r for r in TABLE["refuse"] if r["why"] == "65 characters")
        self.assertEqual(len(long["operator"]["name"]), 65)

    def test_a_trailing_newline_is_not_part_of_a_name(self) -> None:
        # Python's `$` would let one through; the rule must refuse it as the TypeScript rule does.
        with self.assertRaises(aws_target.TargetError):
            aws_target.parse_operator({"kind": "user", "name": "alice\n"}, "accounts[0]")

    def test_a_caller_is_the_operator_when_it_is_the_identity_the_field_names_or_the_setup_runner(self) -> None:
        self.assertGreaterEqual(len(TABLE["callers"]), 15)
        for row in TABLE["callers"]:
            op = aws_target.Operator(**row["operator"])
            arn = fill(row["arn"])
            with self.subTest(f"{op.label} {arn}"):
                self.assertIs(aws_target.operator_may_deploy(ACCOUNT, op, arn), row["ok"])

    def test_the_setup_runner_role_is_the_one_the_runner_template_creates_and_the_stacks_know(self) -> None:
        self.assertEqual(aws_target.SETUP_RUNNER_ROLE, "hamh-setup-runner-role")
        self.assertIn(f"RoleName: {aws_target.SETUP_RUNNER_ROLE}\n", (REPO / "infra" / "setup-runner.yaml").read_text())
        self.assertIn(f'SETUP_RUNNER_ROLE = "{aws_target.SETUP_RUNNER_ROLE}"', (REPO / "infra" / "lib" / "operator.ts").read_text())


class TheField(unittest.TestCase):
    def test_the_committed_accounts_file_names_no_operator_so_the_owners_account_keeps_its_stage(self) -> None:
        self.assertNotIn("operator", (REPO / "accounts.json").read_text())
        for t in aws_target.load(REPO / "accounts.json"):
            self.assertIsNone(t.operator, t.profile)
        p = cli("get", "operator")
        self.assertEqual((p.returncode, p.stdout, p.stderr), (0, "\n", ""))

    def test_an_entry_without_the_field_has_no_operator(self) -> None:
        t = aws_target.load(write_accounts())[0]
        self.assertIsNone(t.operator)
        self.assertEqual(aws_target.field(t, "operator"), "")

    def test_an_entry_with_the_field_carries_the_identity_for_each_accepted_word(self) -> None:
        for row in TABLE["accept"]:
            with self.subTest(row["label"]):
                t = aws_target.load(write_accounts(row["operator"]))[0]
                self.assertEqual((t.operator.kind, t.operator.name), (row["operator"]["kind"], row["operator"]["name"]))
                p = cli("get", "operator", accounts=write_accounts(row["operator"]))
                self.assertEqual((p.returncode, p.stdout.strip()), (0, row["label"]))

    def test_the_other_fields_do_not_change_with_the_operator(self) -> None:
        plain = aws_target.load(write_accounts())[0]
        named = aws_target.load(write_accounts({"kind": "user", "name": "alice"}))[0]
        self.assertEqual((plain.account, plain.region, plain.profile, plain.role, plain.azs, plain.az_ids, plain.invocation_logging),
                         (named.account, named.region, named.profile, named.role, named.azs, named.az_ids, named.invocation_logging))
        self.assertEqual(named.core(), plain.core())

    def test_a_refused_word_is_a_refusal_of_the_whole_file_that_names_the_entry(self) -> None:
        for row in TABLE["refuse"]:
            with self.subTest(row["why"]):
                path = write_accounts(fill(row["operator"]), has_operator=True)
                with self.assertRaises(aws_target.TargetError) as cm:
                    aws_target.load(path)
                self.assertIn("accounts[0]: operator", str(cm.exception))
                p = cli("get", "account", accounts=path)
                self.assertEqual((p.returncode, p.stdout), (2, ""))
                self.assertIn("aws_target: ", p.stderr)
                self.assertIn(row["reason"], p.stderr)

    def test_a_null_operator_is_refused_not_ignored(self) -> None:
        with self.assertRaises(aws_target.TargetError):
            aws_target.load(write_accounts(None, has_operator=True))


class DeployCallerCheck(unittest.TestCase):
    """scripts/lib/deploy.py check_account: the account first, then (only when the entry names an operator) the identity."""

    def deployer(self, path: Path | None) -> "deploy.Deployer":
        env = {k: v for k, v in os.environ.items() if k not in ("HAMH_AWS_PROFILE", "HAMH_ACCOUNTS_FILE")}
        if path is not None:
            env["HAMH_ACCOUNTS_FILE"] = str(path)
        with mock.patch.dict(os.environ, env, clear=True):
            return deploy.Deployer("direct")

    def check(self, d: "deploy.Deployer", doc: dict) -> None:
        with mock.patch.object(deploy.Deployer, "aws", return_value=doc) as aws:
            d.check_account()
        aws.assert_called_once_with("sts", "get-caller-identity")

    def test_without_the_field_only_the_account_is_checked_and_an_answer_without_an_arn_passes(self) -> None:
        d = self.deployer(None)
        self.check(d, {"Account": d.account})
        self.check(d, {"Account": d.account, "Arn": "arn:aws:iam::" + d.account + ":user/anybody"})
        with self.assertRaises(deploy.Fail) as cm:
            self.check(d, {"Account": OTHER})
        self.assertEqual(str(cm.exception), f"account {OTHER}")

    def test_the_account_of_the_credentials_is_checked_before_the_identity(self) -> None:
        d = self.deployer(write_accounts({"kind": "role", "name": "StageOperator"}))
        with self.assertRaises(deploy.Fail) as cm:
            self.check(d, {"Account": OTHER, "Arn": f"arn:aws:sts::{OTHER}:assumed-role/StageOperator/x"})
        self.assertEqual(str(cm.exception), f"account {OTHER}")

    def test_the_caller_table_decides_for_each_row_and_a_refusal_names_the_identity_and_the_caller(self) -> None:
        for row in TABLE["callers"]:
            path = write_accounts(row["operator"])
            d = self.deployer(path)
            self.assertEqual(d.account, EXAMPLE["accounts"][0]["account"])
            arn = fill(row["arn"])
            with self.subTest(f"{row['operator']['kind']}/{row['operator']['name']} {arn}"):
                doc = {"Account": d.account, "Arn": arn}
                if row["ok"]:
                    self.check(d, doc)
                    continue
                with self.assertRaises(deploy.Fail) as cm:
                    self.check(d, doc)
                text = str(cm.exception)
                self.assertTrue(text.startswith("caller "), text)
                self.assertIn(f"{row['operator']['kind']}/{row['operator']['name']}", text)
                self.assertIn(arn or "no identity", text)
                self.assertIn(str(path), text)

    def test_a_credential_answer_without_an_arn_is_refused_when_the_entry_names_an_operator(self) -> None:
        d = self.deployer(write_accounts({"kind": "user", "name": "alice"}))
        with self.assertRaises(deploy.Fail) as cm:
            self.check(d, {"Account": d.account})
        self.assertIn("no identity", str(cm.exception))
        self.assertIn("user/alice", str(cm.exception))

    def test_the_failure_prints_as_deploy_fail_caller_and_the_guard_command_uses_the_same_check(self) -> None:
        d = self.deployer(write_accounts({"kind": "role", "name": "StageOperator"}))
        with mock.patch.object(deploy.Deployer, "aws", return_value={"Account": d.account, "Arn": f"arn:aws:sts::{d.account}:assumed-role/Other/x"}):
            with self.assertRaises(deploy.Fail) as cm:
                d.check_guard()
        self.assertEqual(f"DEPLOY-FAIL {cm.exception}".split(" ")[:2], ["DEPLOY-FAIL", "caller"])

    def test_the_refusal_for_the_committed_file_says_so(self) -> None:
        # Not reachable today (the committed file names no operator); the text must still be right when a committed entry names one.
        d = self.deployer(write_accounts({"kind": "role", "name": "StageOperator"}))
        d.config_digest = "committed"
        with self.assertRaises(deploy.Fail) as cm:
            self.check(d, {"Account": d.account, "Arn": f"arn:aws:sts::{d.account}:assumed-role/Other/x"})
        self.assertIn("accounts.json", str(cm.exception))


if __name__ == "__main__":
    unittest.main()
