"""A stage deployed under the previous names: `--status`, `--doctor`, `--full` and `--update` (COMPAT-CAH-STAGE, LEGACY-TOMBSTONE, hand-edit item setup-02).
# COMPAT-CAH-STAGE: this file names the previous generation of names on purpose, and ends with the shim it tests.

A stage belongs to a generation of names only when its stack-name prefix and its project tag are both from that generation; a mixed pair is foreign and is not counted.
`--status` reports `state=legacy` (or `legacy-retained` when only a VPC, a stage bucket or a runner stack of that generation is left) and never `not-deployed`; `--doctor`
fails with the same sentence; `--full` and `--update` end with exit 7 before anything is created, with a message that says the old stage keeps running, that the client still
talks to it, and the four steps. `--revision` of a commit from before the rename is refused. The cases run the real scripts against the stand-in AWS CLI.
"""

from __future__ import annotations

import json
import os
import re
import subprocess  # nosec B404: tests start bash with an argument list, never a shell string built from data
import unittest
from pathlib import Path

from setup_full_support import FullBase
from setup_waits import limit

REPO = Path(__file__).resolve().parents[2]
NAMES_MANIFEST = REPO / "names.manifest"

OLD = "cah"  # the previous first word of the stage's names
OLD_PROJECT = "cline-agentcore-harness"
NEW_PROJECT = "hyperscale-agent-meta-harness"
OLD_URL = "https://oldapi1.execute-api.us-east-1.amazonaws.com/direct"
OLD_TAGS = {"project": OLD_PROJECT, "stage": "direct"}
STACKS = ("network", "data", "control")


class LegacyBase(FullBase):
    def legacy_stage(self, tags: dict | None = None, prefix: str = OLD, status: str = "CREATE_COMPLETE") -> None:
        stacks = {}
        for short in STACKS:
            stacks[f"{prefix}-direct-{short}"] = {"status": status, "outputs": {"V1ApiUrl": OLD_URL} if short == "control" else {}, "tags": dict(tags or OLD_TAGS), "resources": []}
        self.set_scenario(stacks=stacks)

    def legacy_vpc(self) -> None:
        w = self.open_world()
        w.w.setdefault("ec2", {"vpcs": {}})["vpcs"]["vpc-0old0001"] = {"subnets": [], "sgs": [], "tags": dict(OLD_TAGS)}
        w.save()

    def legacy_bucket(self, name: str | None = None, tags: dict | None = None) -> None:
        w = self.open_world()
        bucket = name or f"{OLD}-direct-harness-{self.ACCOUNT}"
        w.w.setdefault("buckets", {})[bucket] = True
        w.w.setdefault("bucket_tags", {})[bucket] = dict(tags or OLD_TAGS)
        w.save()

    def status(self, *extra: str):
        return self.run_full("--status", "--profile", "work", "--region", "us-east-1", *extra)

    def doctor(self):
        return self.run_full("--doctor", "--profile", "work", "--region", "us-east-1")


class StatusAndDoctor(LegacyBase):
    def test_status_reports_legacy_with_exit_7_and_the_endpoint(self) -> None:
        self.legacy_stage()
        p = self.status()
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertRegex(p.stdout, r"(?m)^STATUS stage=direct state=legacy account=\d+ region=us-east-1$")
        self.assertIn("under the previous names", p.stdout)
        self.assertIn(OLD_URL, p.stdout)
        self.assertEqual(self.mutations(), [])

    def test_a_stack_with_the_previous_prefix_and_the_current_project_tag_is_foreign(self) -> None:
        self.legacy_stage(tags={"project": NEW_PROJECT, "stage": "direct"})
        p = self.status()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertRegex(p.stdout, r"(?m)^STATUS .*state=not-deployed")

    def test_a_stack_with_the_previous_prefix_and_another_stage_tag_is_foreign(self) -> None:
        self.legacy_stage(tags={"project": OLD_PROJECT, "stage": "other"})
        p = self.status()
        self.assertRegex(p.stdout, r"(?m)^STATUS .*state=not-deployed")

    def test_only_a_retained_vpc_is_legacy_retained_with_exit_8(self) -> None:
        self.legacy_vpc()
        p = self.status()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertRegex(p.stdout, r"(?m)^STATUS .*state=legacy-retained")
        self.assertIn("vpc-0old0001", p.stdout)

    def test_only_a_stage_bucket_of_the_previous_names_is_legacy_retained(self) -> None:
        self.legacy_bucket()
        p = self.status()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertRegex(p.stdout, r"(?m)^STATUS .*state=legacy-retained")

    def test_a_bucket_of_that_name_without_the_project_tag_is_not_the_stage(self) -> None:
        self.legacy_bucket(tags={"project": "something-else"})
        p = self.status()
        self.assertRegex(p.stdout, r"(?m)^STATUS .*state=not-deployed")

    def test_a_runner_stack_of_the_previous_names_is_legacy_retained(self) -> None:
        self.set_scenario(stacks={f"{OLD}-setup-runner": {"status": "CREATE_COMPLETE", "outputs": {}, "tags": {"project": OLD_PROJECT}, "resources": []}})
        p = self.status()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertRegex(p.stdout, r"(?m)^STATUS .*state=legacy-retained")

    def test_a_current_stage_next_to_a_previous_one_shows_both(self) -> None:
        self.legacy_stage()
        w = self.open_world()
        w.deploy_stage()
        w.save()
        p = self.status()
        self.assertRegex(p.stdout, r"(?m)^STATUS .*state=deployed")
        self.assertRegex(p.stdout, r"(?m)^\s*previous:.*under the previous names")

    def test_doctor_fails_with_the_same_sentence_and_exit_3(self) -> None:
        self.legacy_stage()
        p = self.doctor()
        self.assertEqual(p.returncode, 3, self.out(p))
        self.assertRegex(p.stdout, r"(?m)^FAIL stage direct: Your account holds the stage under the previous names")
        status = self.status().stdout
        sentence = [l for l in p.stdout.splitlines() if l.startswith("FAIL stage direct: ")][0].split(": ", 1)[1]
        self.assertIn(sentence, status, "status and doctor share the sentence")

    def test_doctor_on_a_retained_leftover_fails_too(self) -> None:
        self.legacy_vpc()
        p = self.doctor()
        self.assertEqual(p.returncode, 3, self.out(p))
        self.assertIn("previous names", p.stdout)

    def test_a_status_without_any_previous_resource_makes_only_reads(self) -> None:
        p = self.status()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.mutations(), [])


class FullAndUpdate(LegacyBase):
    UPDATE = ("--update", "--profile", "work", "--region", "us-east-1", "--yes")

    def assert_refused_with_the_four_steps(self, p) -> None:
        self.assertEqual(p.returncode, 7, self.out(p))
        text = " ".join(p.stderr.split())
        self.assertIn("under the previous names", text)
        self.assertIn("created or changed nothing", text)
        self.assertIn("keeps running", text)
        self.assertIn("pull <runId> works against the old endpoint", text)
        self.assertRegex(p.stderr, r"(?m)^\s*1\. Install the new client against the old endpoint: ./setup.sh --client-only --profile work --region us-east-1 --endpoint ")
        self.assertIn("--from <directory with ham and SHA256SUMS>", p.stderr, "the command names where the client comes from")
        self.assertRegex(p.stderr, r"(?m)^\s*2\. Run ham pull <runId>")
        self.assertRegex(p.stderr, r"(?m)^\s*3\. Run ./setup.sh --teardown")
        self.assertRegex(p.stderr, r"(?m)^\s*4\. Run ./setup.sh --full")
        self.assertIn(f"{OLD}-e2e-*", p.stderr, "the repositories that stay")
        self.assertEqual(self.mutations(), [], "nothing may be created or changed")
        self.assertEqual(self.count_calls("cloudformation", "deploy"), 0, "no runner is installed")

    def test_full_is_refused_before_anything_is_created(self) -> None:
        self.legacy_stage()
        p = self.run_full(*self.ARGS)
        self.assert_refused_with_the_four_steps(p)
        self.assertIn(f"--endpoint {OLD_URL}", p.stderr, "the endpoint of the previous stage is named")

    def test_update_is_refused_before_anything_is_created(self) -> None:
        self.legacy_stage()
        self.commit_more()
        p = self.run_full(*self.UPDATE)
        self.assert_refused_with_the_four_steps(p)

    def test_a_dry_run_is_refused_too(self) -> None:
        self.legacy_stage()
        p = self.run_full(*self.ARGS, "--dry-run")
        self.assertEqual(p.returncode, 7, self.out(p))

    def test_the_hint_for_an_unknown_endpoint_names_the_output_to_read(self) -> None:
        stacks = {f"{OLD}-direct-network": {"status": "CREATE_COMPLETE", "outputs": {}, "tags": dict(OLD_TAGS), "resources": []}}
        self.set_scenario(stacks=stacks)
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertIn("--endpoint <old endpoint URL>", p.stderr)
        self.assertIn(f"Read the URL from the output V1ApiUrl of the CloudFormation stack {OLD}-direct-control.", p.stderr)

    def test_a_retained_leftover_refuses_too(self) -> None:
        self.legacy_vpc()
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertIn("vpc-0old0001", p.stderr)
        self.assertEqual(self.mutations(), [])
        self.assertRegex(p.stderr, r"(?m)^\s*1\. Run ./setup.sh --teardown")
        self.assertRegex(p.stderr, r"(?m)^\s*2\. Run ./setup.sh --full")
        self.assertNotIn("pull <runId>", p.stderr, "no run and no endpoint are left to move to")

    def test_a_foreign_stack_does_not_stop_a_full_setup(self) -> None:
        self.legacy_stage(tags={"project": NEW_PROJECT, "stage": "direct"})
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))


class StepOneCanBeFollowed(LegacyBase):
    """Step 1 of the message is a command that works as printed on the account it was printed for: it names the profile and the Region the refused run used, the endpoint, and where the
    client comes from; and it says what the step does to the rules pack."""

    STEP_1 = rf"(?m)^\s*1\. Install the new client against the old endpoint: ./setup.sh --client-only --profile work --region us-east-1 --endpoint {re.escape(OLD_URL)} --from <directory with ham and SHA256SUMS>$"
    PACK = ("Step 1 replaces the unedited rules pack of the earlier client with the current one. Until the stage is torn down and redeployed, runs on it show 'No test command was found' "
            "on the delivery card; the code and the tests are unaffected.")

    def refusal(self):
        self.legacy_stage()
        return self.run_full(*self.ARGS)

    def test_step_1_names_the_profile_the_region_the_endpoint_and_the_source_of_the_client(self) -> None:
        p = self.refusal()
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertRegex(p.stderr, self.STEP_1)

    def test_the_other_way_to_a_client_is_named_and_the_step_creates_nothing(self) -> None:
        flat = " ".join(self.refusal().stderr.split())
        self.assertIn("Instead of --from, --build compiles ham from this checkout (a Rust toolchain is needed). This command creates nothing in your account.", flat)

    def test_the_message_says_what_step_1_does_to_the_rules_pack_and_what_the_user_sees_on_the_old_stage(self) -> None:
        flat = " ".join(self.refusal().stderr.split())
        self.assertIn(self.PACK, flat)

    def test_the_message_offers_neither_full_nor_update_as_a_way_to_a_client(self) -> None:
        step = [ln for ln in self.refusal().stderr.splitlines() if "client-only" in ln]
        self.assertTrue(step, "the command line is there")
        for ln in step:
            self.assertNotIn("--full", ln)
            self.assertNotIn("--update", ln)

    def test_a_run_without_a_profile_leaves_the_option_out(self) -> None:
        body = ('LEGACY_STATE=legacy; LEGACY_PREFIX=cah; LEGACY_PROJECT=cline-agentcore-harness; LEGACY_E2E=cah-e2e-; LEGACY_ENDPOINT=' + OLD_URL + '; STAGE=direct; REGION=us-east-1; PROFILE_EFF=""\n'
                'pf_legacy_refuse')
        p = subprocess.run(["bash", "-c", "set -u\n. scripts/setup/names.sh\n. scripts/setup/lib.sh\n. scripts/setup/preflight.sh\n" + body], capture_output=True, text=True, cwd=str(REPO), timeout=limit(60),  # nosec B603: argument list, test snippet; B607: bash on PATH
                           env={"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LC_ALL": "C", "HOME": "/nonexistent-home"})
        self.assertEqual(p.returncode, 7, p.stdout + p.stderr)
        self.assertRegex(p.stderr, rf"(?m)^\s*1\. Install the new client against the old endpoint: ./setup.sh --client-only --region us-east-1 --endpoint {re.escape(OLD_URL)} --from <directory with ham and SHA256SUMS>$")
        self.assertNotIn("--profile", p.stderr)


class AdviceOnALegacyAccount(LegacyBase):
    """What status and doctor advise for the operator tool and what they compare the client with, on an account whose stage is under the previous names."""

    OTHER = "https://zzz999.execute-api.us-east-1.amazonaws.com/direct"

    def client_pointing_at(self, endpoint: str) -> None:
        self.ham_on_path()
        self.run_full("--client-only", "--yes", "--profile", "work", "--region", "us-east-1", "--endpoint", endpoint)

    def test_status_does_not_advise_an_update_for_the_missing_operator_tool(self) -> None:
        self.legacy_stage()
        self.client_pointing_at(OLD_URL)
        p = self.status()
        line = [ln for ln in p.stdout.splitlines() if ln.strip().startswith("operator tool:")]
        self.assertEqual(len(line), 1, p.stdout)
        self.assertIn("not installed", line[0])
        self.assertNotIn("--update", line[0], "--update is refused on this account")
        self.assertIn("a new stage brings it: ./setup.sh --teardown, then ./setup.sh --full", line[0])

    def test_doctor_does_not_advise_an_update_for_the_missing_operator_tool(self) -> None:
        self.legacy_stage()
        self.client_pointing_at(OLD_URL)
        p = self.doctor()
        line = [ln for ln in p.stdout.splitlines() if ln.startswith("WARN operator-tool")]
        self.assertEqual(len(line), 1, p.stdout)
        self.assertNotIn("--update", line[0])
        self.assertIn("a new stage brings it: ./setup.sh --teardown, then ./setup.sh --full", line[0])

    def test_status_compares_the_client_with_the_previous_endpoint(self) -> None:
        self.legacy_stage()
        self.client_pointing_at(OLD_URL)
        p = self.status()
        self.assertRegex(p.stdout, r"(?m)^\s*client:.*its endpoint matches the stage under the previous names")

    def test_status_says_when_the_client_points_somewhere_else(self) -> None:
        self.legacy_stage()
        self.client_pointing_at(self.OTHER)
        p = self.status()
        self.assertRegex(p.stdout, r"(?m)^\s*client:.*its endpoint points to another stage")

    def test_doctor_passes_the_endpoint_check_against_the_previous_endpoint(self) -> None:
        self.legacy_stage()
        self.client_pointing_at(OLD_URL)
        p = self.doctor()
        self.assertEqual(p.returncode, 3, "the stage check still fails")
        self.assertRegex(p.stdout, rf"(?m)^PASS endpoint the client points at the stage under the previous names \({re.escape(OLD_URL)}\)$")
        self.assertNotIn("no endpoint to compare with", p.stdout)

    def test_doctor_names_the_previous_endpoint_when_the_client_points_somewhere_else(self) -> None:
        self.legacy_stage()
        self.client_pointing_at(self.OTHER)
        p = self.doctor()
        self.assertRegex(p.stdout, rf"(?m)^FAIL endpoint the client points at {re.escape(self.OTHER)}, the stage under the previous names is at {re.escape(OLD_URL)}$")
        self.assertNotIn("no endpoint to compare with", p.stdout)

    def test_with_no_endpoint_known_the_sentence_stays(self) -> None:
        self.legacy_vpc()
        self.client_pointing_at(self.OTHER)
        p = self.doctor()
        self.assertRegex(p.stdout, r"(?m)^FAIL endpoint the stage has no endpoint to compare with \(client: ")


class RevisionFromBeforeTheRename(LegacyBase):
    def old_commit(self) -> str:
        """A commit whose scripts/setup/names.sh does not define HAMH_CLIENT_BIN, as the setup of a release before the rename."""
        names = self.repo / "scripts" / "setup" / "names.sh"
        original = names.read_text()
        names.write_text("# the names of the earlier release\n: \"${CAH_CLIENT_BIN:=cah}\"\n")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "an earlier setup")
        old = self.commit()
        names.write_text(original)
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "this setup")
        return old

    def test_update_of_such_a_revision_is_refused_with_exit_2(self) -> None:
        old = self.old_commit()
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--yes", "--revision", old)
        self.assertEqual(p.returncode, 2, self.out(p))
        self.assertIn("predates the rename", p.stderr)
        self.assertIn(old, p.stderr)
        self.assertEqual(self.mutations(), [])
        self.assertEqual(self.count_calls("cloudformation", "list-stacks"), 0, "refused before the account is read for the stage")

    def test_a_revision_of_this_line_is_accepted(self) -> None:
        self.old_commit()
        head = self.commit()
        self.set_scenario(stage_deployed=True)
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--yes", "--revision", head)
        self.assertEqual(p.returncode, 0, self.out(p))


class GenerationTable(unittest.TestCase):
    """The table of lib.sh (setup_gen_use) holds the same names as the manifest of the rename, for both generations."""

    def row(self, generation: str) -> dict:
        script = f'. scripts/setup/lib.sh; setup_gen_use {generation} || exit 9; printf "%s\\n" "$GEN" "$GEN_PREFIX" "$GEN_ROOT" "$GEN_PROJECT" "$GEN_SETUP" "$GEN_E2E" "$SETUP_RUNNER_STACK" "$SETUP_PROJECT_TAG"'
        p = subprocess.run(["bash", "-c", script], capture_output=True, text=True, cwd=str(REPO), timeout=limit(60),  # nosec B603: argument list; B607: bash on PATH as the other tests do
                           env={"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LC_ALL": "C"})
        self.assertEqual(p.returncode, 0, p.stderr)
        keys = ("gen", "prefix", "root", "project", "setup", "e2e", "runner", "tag")
        return dict(zip(keys, p.stdout.split("\n")))

    def test_each_generation_points_the_runner_and_the_tag_at_its_own_names(self) -> None:
        cur, old = self.row("current"), self.row("legacy")
        self.assertEqual((cur["gen"], old["gen"]), ("current", "legacy"))
        self.assertEqual((cur["runner"], cur["tag"]), (cur["setup"] + "-runner", cur["project"]))
        self.assertEqual((old["runner"], old["tag"]), (old["setup"] + "-runner", old["project"]))
        self.assertNotEqual(cur["project"], old["project"])
        self.assertNotEqual(cur["prefix"], old["prefix"])

    def test_the_rows_equal_the_manifest(self) -> None:
        if not NAMES_MANIFEST.is_file():
            self.skipTest("the names manifest of the compatibility window is not in this tree")
        doc = json.loads(NAMES_MANIFEST.read_text(encoding="utf-8"))
        for generation, key in (("current", "current"), ("legacy", "legacy")):
            with self.subTest(generation=generation):
                row, want = self.row(generation), doc[key]
                self.assertEqual(row["project"], want["projectTagValue"])
                self.assertEqual(row["prefix"] + "-" + doc["common"]["stageWord"], want["stagePrefix"])
                self.assertEqual(row["root"] + "/", want["ssmRoot"])
                self.assertEqual(row["runner"], want["setupRunnerStack"])
                self.assertEqual(row["setup"] + "-", want["setupBucketPrefix"])
                self.assertEqual(row["e2e"], want["codeCommitPrefix"])

    def test_a_pair_is_a_generation_only_when_both_words_belong_to_it(self) -> None:
        def pair(prefix: str, tag: str) -> str:
            p = subprocess.run(["bash", "-c", f'. scripts/setup/lib.sh; setup_gen_pair "{prefix}" "{tag}"; echo "rc=$?"'], capture_output=True, text=True, cwd=str(REPO), timeout=limit(60),  # nosec B603: argument list; B607: bash on PATH
                               env={"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LC_ALL": "C"})
            return p.stdout.strip()
        self.assertEqual(pair("hamh", NEW_PROJECT), "currentrc=0")
        self.assertEqual(pair(OLD, OLD_PROJECT), "legacyrc=0")
        self.assertEqual(pair(OLD, NEW_PROJECT), "rc=1")
        self.assertEqual(pair("hamh", OLD_PROJECT), "rc=1")
        self.assertEqual(pair("other", NEW_PROJECT), "rc=1")


if __name__ == "__main__":
    unittest.main()
