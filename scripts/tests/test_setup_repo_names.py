"""`setup.sh --repo-name`: the repository names a stage lets a run open a pull request in.

A new stage allows the repositories whose name starts with `hamh-e2e-`. `--repo-name NAME` (repeatable, or comma separated) of the full setup writes the stage's own
rule into the account file it generates (the optional `repoNames` of the entry; infra/lib/repo-names.ts and scripts/lib/aws_target.py read it, the CDK app hands it to
the control-api and delivery functions as `HAMH_REPO_NAMES`). An entry is a repository name, such a prefix followed by one `*`, or `*`. The cases run the real `setup.sh`
and `scripts/setup/*.sh` against the stand-in AWS CLI of setup_fake_aws.py:

- the account file carries `repoNames` only when the flag is given, so the golden file of a default setup is what it was;
- a value that is not an entry, more than eight entries, an entry twice and the flag with `--client-only` are usage errors before any AWS call;
- `--default-repo` must be covered by the rule that is in force (the default rule when no flag is given, with its old message);
- an update keeps the rule the stage has (the output `RepoNames` of the control stack) unless the flag names a new one;
- the shell matcher gives the answers of the shared table infra/test/fixtures/repo-names.json.
"""

from __future__ import annotations

import hashlib
import json
import re
import subprocess  # nosec B404: tests start bash with an argument list, never a shell string built from input
import sys
import unittest

from setup_full_support import FullBase, golden_accounts
from setup_waits import limit
from test_setup_full_state import StateBase
from test_setup_script import BASH, REPO, SCRIPT

sys.path.insert(0, str(REPO / "scripts" / "lib"))
import aws_target  # noqa: E402

TABLE = json.loads((REPO / "infra" / "test" / "fixtures" / "repo-names.json").read_text())
MARK = '      "invocationLogging": "read-only"\n'


def with_names(names: list, region: str = "us-east-1", operator: tuple | None = ("user", "setup-test")) -> bytes:
    """The golden account file of a default setup with the `repoNames` the flag gives."""
    text = golden_accounts(region, operator=operator).decode()
    assert MARK in text
    listed = ", ".join(json.dumps(n) for n in names)
    return text.replace(MARK, f'      "invocationLogging": "read-only",\n      "repoNames": [{listed}]\n', 1).encode()


def key_of(body: bytes) -> str:
    return f"config/accounts-{hashlib.sha256(body).hexdigest()[:12]}.json"


class FlagCase(FullBase):
    def test_the_help_lists_the_flag_with_the_default_rule(self) -> None:
        p = self.run_setup("--help")
        self.assertEqual(p.returncode, 0, self.out(p))
        flat = " ".join(p.stdout.split())
        self.assertIn("--repo-name NAME", flat)
        self.assertIn("hamh-e2e-*", flat)

    def test_the_names_go_into_the_account_file_the_runner_builds_from(self) -> None:
        p = self.run_full(*self.ARGS, "--keep-runner", "--repo-name", "my-app", "--repo-name", "team-*")
        self.assertEqual(p.returncode, 0, self.out(p))
        want = with_names(["my-app", "team-*"])
        self.assertEqual(self.s3_bytes(key_of(want)), want)

    def test_the_generated_file_is_one_the_readers_of_the_repository_accept(self) -> None:
        p = self.run_full(*self.ARGS, "--keep-runner", "--repo-name", "my-app", "--repo-name", "team-*")
        self.assertEqual(p.returncode, 0, self.out(p))
        generated = self.tmp / "generated-accounts.json"
        generated.write_bytes(self.s3_bytes(key_of(with_names(["my-app", "team-*"]))))
        (target,) = aws_target.load(generated)
        self.assertEqual(aws_target.repo_names_of(target, {}), ["my-app", "team-*"])

    def test_one_flag_with_a_comma_separated_list_is_the_same_rule(self) -> None:
        p = self.run_full(*self.ARGS, "--keep-runner", "--repo-name", "my-app,team-*")
        self.assertEqual(p.returncode, 0, self.out(p))
        want = with_names(["my-app", "team-*"])
        self.assertEqual(self.s3_bytes(key_of(want)), want)

    def test_the_equals_form_works(self) -> None:
        p = self.run_full(*self.ARGS, "--keep-runner", "--repo-name=my-app")
        self.assertEqual(p.returncode, 0, self.out(p))
        want = with_names(["my-app"])
        self.assertEqual(self.s3_bytes(key_of(want)), want)

    def test_without_the_flag_the_account_file_is_the_golden_file_and_has_no_repo_names(self) -> None:
        p = self.run_full(*self.ARGS, "--keep-runner")
        self.assertEqual(p.returncode, 0, self.out(p))
        want = golden_accounts("us-east-1")
        self.assertEqual(self.s3_bytes(key_of(want)), want)
        self.assertNotIn(b"repoNames", want)

    def test_the_plan_names_the_rule_only_when_it_is_not_the_default(self) -> None:
        p = self.run_full(*self.ARGS, "--repo-name", "my-app,team-*", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("repository names: my-app, team-*", " ".join(p.stdout.split()))
        q = self.run_full(*self.ARGS, "--dry-run")
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertNotIn("repository names", q.stdout)


class RefusalCase(FullBase):
    def usage(self, *args: str):
        p = self.run_full("--full", "--profile", "work", "--region", "us-east-1", "--yes", "--acknowledge-costs", *args)
        self.assertEqual(p.returncode, 2, self.out(p))
        self.assertEqual(self.calls(), [], "a usage error comes before any AWS call")
        return p

    def test_a_value_that_is_not_an_entry_is_a_usage_error_that_names_the_flag(self) -> None:
        for bad in ("my app", "team/app", "cc:my-app", "te*am", "team-**", "*-app", "a" * 101, "a" * 100 + "*", ""):
            with self.subTest(value=bad[:20]):
                self.assertIn("--repo-name", self.usage("--repo-name", bad).stderr)

    def test_an_empty_entry_in_a_list_is_a_usage_error(self) -> None:
        self.assertIn("--repo-name", self.usage("--repo-name", "a,,b").stderr)

    def test_more_than_eight_entries_are_a_usage_error(self) -> None:
        self.assertIn("at most 8", self.usage("--repo-name", ",".join(f"a{i}" for i in range(9))).stderr)

    def test_the_same_entry_twice_is_a_usage_error(self) -> None:
        self.assertIn("twice", self.usage("--repo-name", "a", "--repo-name", "a").stderr)

    def test_a_value_is_required(self) -> None:
        self.assertIn("--repo-name needs a value", self.usage("--repo-name").stderr)

    def test_the_flag_belongs_to_the_full_setup(self) -> None:
        art = self.make_artifact()
        p = self.run_setup("--client-only", "--from", str(art), "--repo-name", "my-app", "--yes", "--profile", "work", "--region", "us-east-1")
        self.assertEqual(p.returncode, 2, self.out(p))
        self.assertIn("--client-only does not take --repo-name", p.stderr)

    def test_the_flag_turns_the_full_setup_on_by_itself(self) -> None:
        p = self.run_full("--repo-name", "my-app", "--profile", "work", "--region", "us-east-1", "--yes", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("What setup will do", p.stdout)


class DefaultRepoCase(FullBase):
    def check(self, *args: str):
        return self.run_full("--full", "--profile", "work", "--region", "us-east-1", "--yes", "--acknowledge-costs", "--dry-run", *args)

    def test_a_default_repository_is_checked_against_the_rule_in_force(self) -> None:
        for rule, good in (("my-app", "cc:my-app"), ("team-*", "cc:team-b"), ("*", "cc:python-calc"), ("hamh-e2e-*", "cc:hamh-e2e-demo")):
            with self.subTest(rule=rule):
                p = self.check("--repo-name", rule, "--default-repo", good)
                self.assertEqual(p.returncode, 0, self.out(p))

    def test_a_default_repository_outside_the_rule_is_a_usage_error_that_names_the_rule(self) -> None:
        p = self.check("--repo-name", "my-app,team-*", "--default-repo", "cc:other")
        self.assertEqual(p.returncode, 2, self.out(p))
        self.assertIn("--default-repo should look like cc:NAME with a NAME the repository names (my-app, team-*) cover (got: cc:other)", p.stderr)

    def test_without_the_flag_the_message_of_the_fixed_prefix_is_kept(self) -> None:
        p = self.check("--default-repo", "cc:my-app")
        self.assertEqual(p.returncode, 2, self.out(p))
        self.assertIn("--default-repo should look like cc:hamh-e2e-NAME (got: cc:my-app)", p.stderr)

    def test_the_old_default_repository_still_passes_without_the_flag(self) -> None:
        p = self.check("--default-repo", "cc:hamh-e2e-demo")
        self.assertEqual(p.returncode, 0, self.out(p))


class UpdateCase(StateBase):
    UPDATE = ("--update", "--profile", "work", "--region", "us-east-1", "--yes", "--keep-runner")

    def deployed_with(self, output: str | None) -> None:
        self.assertEqual(self.run_full(*self.ARGS, "--keep-runner").returncode, 0)
        if output is not None:
            w = self.open_world()
            w.w["stacks"]["hamh-direct-control"]["outputs"]["RepoNames"] = output
            w.save()
        self.commit_more()

    def test_an_update_keeps_the_rule_the_stage_has(self) -> None:
        self.deployed_with("my-app,team-*")
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        want = with_names(["my-app", "team-*"])
        self.assertEqual(self.s3_bytes(key_of(want)), want)

    def test_an_update_with_the_flag_changes_the_rule(self) -> None:
        self.deployed_with("my-app")
        p = self.run_full(*self.UPDATE, "--repo-name", "other-*")
        self.assertEqual(p.returncode, 0, self.out(p))
        want = with_names(["other-*"])
        self.assertEqual(self.s3_bytes(key_of(want)), want)

    def test_an_update_of_a_stage_without_the_output_keeps_the_default_and_writes_no_field(self) -> None:
        self.deployed_with(None)
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        want = golden_accounts("us-east-1")
        self.assertEqual(self.s3_bytes(key_of(want)), want)

    def test_an_update_of_a_stage_on_the_default_rule_writes_no_field(self) -> None:
        self.deployed_with("hamh-e2e-*")
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        want = golden_accounts("us-east-1")
        self.assertEqual(self.s3_bytes(key_of(want)), want)


class ShellMatcherCase(unittest.TestCase):
    """The functions of setup.sh that check an entry and a name, run on their own against the rows of the shared table."""

    @staticmethod
    def functions() -> str:
        text = SCRIPT.read_text()
        out = []
        for name in ("valid_repo_entry", "repo_name_covered"):
            m = re.search(rf"(?ms)^{name}\(\) \{{.*?^\}}\n", text)
            assert m, f"{name} is not defined in setup.sh"
            out.append(m.group(0))
        return "\n".join(out)

    def call(self, command: str, *args: str) -> int:
        p = subprocess.run([BASH, "-c", self.functions() + "\n" + command, "x", *args], capture_output=True, text=True, timeout=limit(60))  # nosec B603: argument list, no shell string from input
        self.assertEqual(p.stderr, "")
        return p.returncode

    def test_every_entry_the_table_accepts_is_valid(self) -> None:
        for row in TABLE["accept"]:
            for entry in row["rule"]:
                with self.subTest(row["why"], entry=entry[:12]):
                    self.assertEqual(self.call('valid_repo_entry "$1"', entry), 0)

    def test_every_single_entry_the_table_refuses_is_not_valid(self) -> None:
        seen = 0
        for row in TABLE["refuse"]:
            entries = row["entry"]["repoNames"]
            if not isinstance(entries, list) or len(entries) != 1 or not isinstance(entries[0], str):
                continue
            seen += 1
            with self.subTest(row["why"]):
                self.assertNotEqual(self.call('valid_repo_entry "$1"', entries[0]), 0)
        self.assertGreaterEqual(seen, 9)

    def test_every_cover_row_agrees_with_the_shell_matcher(self) -> None:
        for row in TABLE["cover"]:
            rule = ",".join(row["rule"])
            for name, want in [(n, 0) for n in row["allows"]] + [(n, 1) for n in row["refuses"]]:
                if want == 0 and name.endswith("-") and any(e == name + "*" for e in row["rule"]):
                    continue  # a name that is only the prefix: the shell matcher asks for a character after it (a typo guard for --default-repo)
                with self.subTest(row["why"], name=name):
                    self.assertEqual(self.call('repo_name_covered "$1" "$2"', name, rule), want)


if __name__ == "__main__":
    unittest.main()
