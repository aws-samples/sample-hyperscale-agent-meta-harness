"""`setup.sh --teardown` by generation of names.

One run covers both generations: it reads each with its own prefix, project tag, roots, bucket and runner names, shows ONE plan, asks ONE question and deletes the generations
one after the other with the same functions. A stage belongs to a generation only when its name prefix and its project tag are both from it; a mixed pair is foreign (a
`kept:` line, never touched). `--only current` or `--only legacy` covers one generation and says that the other was not read. A failed or denied read of any covered
generation makes the run INCOMPLETE (exit 8) and the zero line needs every covered generation to read zero.

The cases run the real `setup.sh` and `scripts/setup/*.sh` against the stand-in AWS CLI of setup_fake_aws.py, which keeps the stage of the previous names next to the stage of
the current names (`legacy_stage_deployed`). No live AWS account was used.
"""

from __future__ import annotations

import re
import unittest

import setup_fake_aws
from setup_fake_aws import ACCOUNT, GEN_TAGS, leg
from test_setup_teardown_local import ORDER, TEAR, TeardownBase
from test_setup_teardown_live import HelperCase
from test_setup_teardown_safety import denied, throttle

NEW = [f"hamh-direct-{n}" for n in ORDER]
OLD = [f"cah-direct-{n}" for n in ORDER]
OLD_BUCKET = f"cah-setup-{ACCOUNT}-us-east-1"
NEW_BUCKET = f"hamh-setup-{ACCOUNT}-us-east-1"
COVERS = "the current names (hamh-direct-*) and the previous names (cah-direct-*)"


class LegacyBase(TeardownBase):
    def legacy(self, **more) -> None:
        self.set_scenario(legacy_stage_deployed=True, bootstrap_present=True, nine_stacks=True, **more)

    def both(self, **more) -> None:
        self.set_scenario(stage_deployed=True, legacy_stage_deployed=True, bootstrap_present=True, nine_stacks=True, **more)

    def left(self, prefix: str) -> list:
        return sorted(n for n, st in self.world()["stacks"].items() if n.startswith(prefix) and st["status"] != "DELETE_COMPLETE_RETAINED")

    def deleted(self) -> list:
        return self.delete_order()


class PreviousNamesCase(LegacyBase):
    def test_a_stage_under_the_previous_names_is_removed_and_the_zero_line_is_printed(self) -> None:
        self.legacy()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.left("cah-direct-"), [])
        self.assertEqual(self.deleted(), OLD, "the legacy stacks go importers first, and nothing else is deleted")
        self.assertRegex(p.stdout, r"(?m)^TEARDOWN-INVENTORY 0 stacks stage=direct runner=none")
        self.assertIn("The plan covers the generation using the previous names (cah-direct-*):", p.stdout)
        self.assertNotIn("The plan covers the generation using the current names", p.stdout, "nothing of the current names exists, so its plan is not shown")

    def test_both_generations_are_removed_by_one_run_with_one_plan(self) -> None:
        self.both()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.left("cah-direct-") + self.left("hamh-direct-"), [])
        self.assertEqual(sorted(self.deleted()), sorted(NEW + OLD))
        self.assertEqual(p.stdout.count("== What the teardown removes"), 1, "one plan")
        self.assertIn(f"One teardown plan covers {COVERS} with one confirmation.", p.stdout)
        self.assertIn("The plan covers the generation using the current names (hamh-direct-*):", p.stdout)
        self.assertIn("The plan covers the generation using the previous names (cah-direct-*):", p.stdout)
        self.assertEqual(len(re.findall(r"(?m)^TEARDOWN-INVENTORY 0 stacks", p.stdout)), 1, "one verdict")
        sentence = [ln for ln in p.stdout.splitlines() if ln.startswith("checked:")][-1]
        for part in ("CloudFormation stacks named hamh-direct-* and their tags", "CloudFormation stacks named cah-direct-* and their tags", "AgentCore runtimes named hamh_direct_*",
                     "AgentCore runtimes named cah_direct_*", "SSM parameters under /hamh/direct/", "SSM parameters under /cah/direct/"):
            self.assertIn(part, sentence)

    def test_the_stages_are_deleted_one_generation_after_the_other(self) -> None:
        self.both()
        self.assertEqual(self.tear().returncode, 0)
        order = self.deleted()
        names = [n.split("-direct-")[0] for n in order]
        self.assertEqual(names, ["hamh"] * 9 + ["cah"] * 9, "all stacks of one generation first")

    def test_the_resources_of_each_generation_are_found_by_its_own_names(self) -> None:
        self.legacy()
        self.assertEqual(self.tear().returncode, 0)
        flat = [" ".join(c) for c in self.normalized_calls()]
        self.assertTrue(any("starts_with(StackName,'cah-direct-')" in c for c in flat), "the stacks")
        self.assertTrue(any("starts_with(agentRuntimeName,'cah_direct_')" in c for c in flat), "the runtimes")
        self.assertTrue(any("--log-group-name-prefix /aws/lambda/cah-direct-" in c for c in flat), "the function log groups")
        self.assertTrue(any("--log-group-name-prefix /aws/bedrock-agentcore/runtimes/cah_direct_" in c for c in flat), "the runtime log groups")
        self.assertTrue(any("Values=/cah/direct" in c for c in flat), "the parameters")
        self.assertTrue(any("head-bucket --bucket cah-direct-harness-" in c for c in flat), "the buckets")
        self.assertTrue(any("Name=tag:project,Values=cline-agentcore-harness" in c for c in flat), "the tags of the previous generation")
        self.assertEqual(self.world().get("params", {}), {}, "the parameters under /cah/direct/ went with the stage")

    def test_the_buckets_of_the_previous_stage_are_emptied_with_the_bypass_for_the_go_store_alone(self) -> None:
        self.legacy()
        self.assertEqual(self.tear().returncode, 0)
        harness, gomod = f"cah-direct-harness-{ACCOUNT}", f"cah-direct-gomod-{ACCOUNT}"
        calls = self.normalized_calls()
        deletes = [c for c in calls if c[:2] == ["s3api", "delete-objects"]]
        self.assertTrue(any(harness in c for c in deletes), "the harness bucket is emptied")
        self.assertTrue(any(gomod in c and "--bypass-governance-retention" in c for c in deletes), "the Go module store is emptied with the bypass")
        self.assertFalse(any(harness in c and "--bypass-governance-retention" in c for c in deletes))
        self.assertEqual(self.world()["versions"].get(gomod, {}), {}, "the Go module store of the previous stage is empty")

    def test_a_dry_run_names_both_generations_and_changes_nothing(self) -> None:
        self.both()
        p = self.run_full(*TEAR, "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.mutations(), [])
        self.assertEqual(len(self.left("hamh-direct-")), 9)
        self.assertEqual(len(self.left("cah-direct-")), 9)
        self.assertIn(f"One teardown plan covers {COVERS} with one confirmation.", p.stdout)
        for name in ("hamh-direct-network", "cah-direct-network", "cah-direct-observability"):
            self.assertIn(name, p.stdout)
        self.assertEqual(p.stdout.count("== Dry run"), 1)

    def test_without_a_yes_and_a_terminal_nothing_of_either_generation_is_deleted(self) -> None:
        self.both()
        p = self.run_full("--teardown", "--profile", "work", "--region", "us-east-1")
        self.assertEqual(p.returncode, 4, self.out(p))
        self.assertEqual(self.mutations(), [])
        self.assertEqual(len(self.left("hamh-direct-")) + len(self.left("cah-direct-")), 18)

    def test_the_plan_says_what_a_removed_stage_loses_and_that_the_repositories_stay(self) -> None:
        self.both()
        p = self.run_full(*TEAR, "--dry-run")
        flat = " ".join(p.stdout.split())
        self.assertIn("Removing a stage loses its run records, delivered zips, engine settings, Tavily key and table rows;", flat)
        self.assertIn("CodeCommit repositories named hamh-e2e-* and cah-e2e-* remain", flat)
        self.assertIn("an open pull request keeps an approval rule naming a role of the removed stage", flat)


class RunnerOfThePreviousNamesCase(LegacyBase):
    def test_the_runner_stack_and_the_bucket_of_the_previous_names_go_first_and_the_bucket_is_emptied(self) -> None:
        self.legacy(legacy_runner_present=True)
        w = self.open_world()
        w.put(OLD_BUCKET, "results/b1/result.json", b"{}")
        w.put(OLD_BUCKET, "source/" + "a" * 40 + ".zip", b"zip")
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("cah-setup-runner", self.world()["stacks"])
        self.assertRegex(p.stdout, rf"(?m)^\s*emptied {re.escape(OLD_BUCKET)}: 2 versions and delete markers$")
        order = self.deleted()
        self.assertEqual(order[0], "cah-setup-runner", "the runner goes before the stage")
        self.assertEqual(order[1:], OLD)

    def test_a_runner_of_the_previous_names_with_the_current_tag_is_kept(self) -> None:
        self.set_scenario(bootstrap_present=True, legacy_runner_present=True)
        w = self.open_world()
        w.w["stacks"]["cah-setup-runner"]["tags"]["project"] = GEN_TAGS["current"]["project"]
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("cah-setup-runner", self.world()["stacks"])
        self.assertEqual(self.count_calls("cloudformation", "delete-stack"), 0)
        self.assertRegex(p.stdout, r"(?m)^kept: stack cah-setup-runner was not touched")
        self.assertIn("nothing to tear down", p.stdout.lower())

    def test_both_runners_go_and_each_bucket_is_emptied_with_its_own_name(self) -> None:
        self.both(runner_present=True, legacy_runner_present=True)
        w = self.open_world()
        w.put(OLD_BUCKET, "results/b1/result.json", b"{}")
        w.put(NEW_BUCKET, "results/b2/result.json", b"{}")
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertRegex(p.stdout, rf"(?m)^\s*emptied {re.escape(OLD_BUCKET)}: 1 versions and delete markers$")
        self.assertRegex(p.stdout, rf"(?m)^\s*emptied {re.escape(NEW_BUCKET)}: 1 versions and delete markers$")
        self.assertEqual(self.left("cah-") + self.left("hamh-"), [])


class MixedPairCase(LegacyBase):
    def test_a_name_of_the_previous_generation_with_the_current_tag_is_kept_and_never_touched(self) -> None:
        self.set_scenario(bootstrap_present=True, foreign_stacks=[{"name": "cah-direct-network", "tags": "current"}])
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.count_calls("cloudformation", "delete-stack"), 0)
        self.assertIn("cah-direct-network", self.world()["stacks"])
        self.assertRegex(p.stdout, r"(?m)^kept: stack cah-direct-network was not touched \(a name from the previous names \(cah-direct-\*\) with a project tag from the current names")
        self.assertIn("nothing to tear down", p.stdout.lower())

    def test_a_name_of_the_current_generation_with_the_previous_tag_is_kept_and_never_touched(self) -> None:
        self.set_scenario(bootstrap_present=True, foreign_stacks=[{"name": "hamh-direct-network", "tags": "legacy"}])
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.count_calls("cloudformation", "delete-stack"), 0)
        self.assertIn("hamh-direct-network", self.world()["stacks"])
        self.assertRegex(p.stdout, r"(?m)^kept: stack hamh-direct-network was not touched \(a name from the current names \(hamh-direct-\*\) with a project tag from the previous names")

    def test_mixed_pairs_next_to_the_stages_of_both_generations_stay_while_the_stages_go(self) -> None:
        self.both(foreign_stacks=[{"name": "cah-direct-extra", "tags": "current"}, {"name": "hamh-direct-extra", "tags": "legacy"}])
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.left("cah-direct-"), ["cah-direct-extra"])
        self.assertEqual(self.left("hamh-direct-"), ["hamh-direct-extra"])
        self.assertNotIn("cah-direct-extra", self.deleted())
        self.assertNotIn("hamh-direct-extra", self.deleted())
        self.assertRegex(p.stdout, r"(?m)^kept: stack cah-direct-extra was not touched")
        self.assertRegex(p.stdout, r"(?m)^kept: stack hamh-direct-extra was not touched")
        self.assertRegex(p.stdout, r"(?m)^TEARDOWN-INVENTORY 0 stacks stage=direct")

    def test_a_stack_with_the_name_and_neither_tag_keeps_the_old_reason(self) -> None:
        self.set_scenario(bootstrap_present=True, foreign_stacks=[{"name": "cah-direct-play", "tags": {}}])
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertRegex(p.stdout, r"(?m)^kept: stack cah-direct-play was not touched \(it has the name of a stage stack but not the tags project=cline-agentcore-harness and stage=direct\)")


class OnlyCase(LegacyBase):
    def names_of(self, word: str) -> list:
        """The calls that name the generation, except the reads of every function and build project of the account, which decide whether something still writes to a log group."""
        account_wide = (["codebuild", "batch-get-projects"], ["codebuild", "list-projects"], ["lambda", "list-functions"])
        return [" ".join(c)[:160] for c in self.normalized_calls() if word in " ".join(c) and c[:2] not in account_wide]

    def test_only_legacy_reads_and_removes_the_previous_stage_and_leaves_the_current_one_unread(self) -> None:
        self.both()
        p = self.run_full(*TEAR, "--only", "legacy")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.left("cah-direct-"), [])
        self.assertEqual(len(self.left("hamh-direct-")), 9)
        self.assertEqual(self.names_of("hamh"), [], "no call names the current generation")
        self.assertIn("One teardown plan covers the previous names (cah-direct-*) with one confirmation.", p.stdout)
        self.assertRegex(p.stdout, r"(?m)^TEARDOWN-INVENTORY 0 stacks stage=direct runner=none")
        self.assertIn("the current names (hamh-direct-*)", p.stdout.split("TEARDOWN-INVENTORY 0 stacks stage=")[1], "the result says which generation was not read")
        self.assertIn("excluded by --only", p.stdout)

    def test_only_current_leaves_the_previous_stage_alone(self) -> None:
        self.both()
        p = self.run_full(*TEAR, "--only", "current")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.left("hamh-direct-"), [])
        self.assertEqual(len(self.left("cah-direct-")), 9)
        self.assertEqual(self.names_of("cah"), [], "no call names the previous generation")
        self.assertIn("excluded by --only", p.stdout)

    def test_nothing_in_the_covered_generation_is_nothing_to_tear_down_and_says_what_was_not_read(self) -> None:
        self.legacy()
        p = self.run_full(*TEAR, "--only", "current")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("nothing to tear down", p.stdout.lower())
        self.assertIn("under the current names (hamh-direct-*) in account", p.stdout)
        self.assertIn("The generation using the previous names (cah-direct-*) is excluded by --only and is not read.", p.stdout)
        self.assertEqual(len(self.left("cah-direct-")), 9)
        self.assertEqual(self.mutations(), [])


class FailedReadCase(LegacyBase):
    def test_a_denied_read_of_the_previous_generation_makes_the_run_incomplete_and_the_current_stage_still_goes(self) -> None:
        self.both(faults=[denied("Values=/cah/direct")])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertNotIn("TEARDOWN-INVENTORY 0", p.stdout)
        self.assertIn("TEARDOWN-INVENTORY INCOMPLETE", p.stdout)
        self.assertIn("could not read the SSM parameters under /cah/direct/", p.stdout)
        self.assertEqual(self.left("hamh-direct-"), [], "the stage of the other generation was removed in the same run")
        sentence = [ln for ln in p.stdout.splitlines() if ln.startswith("checked:")][-1]
        self.assertNotIn("SSM parameters", sentence, "a category with a failed read in any covered generation is not claimed")

    def test_a_failed_read_of_the_only_generation_that_exists_is_not_nothing_to_tear_down(self) -> None:
        self.set_scenario(bootstrap_present=True, faults=[denied("starts_with(StackName,'cah-direct-')")])
        p = self.tear()
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertNotIn("nothing to tear down", p.stdout.lower())
        self.assertEqual(self.mutations(), [])

    def test_a_read_that_stays_throttled_in_the_previous_generation_is_incomplete_and_hides_nothing(self) -> None:
        self.legacy(faults=[throttle("logs describe-log-groups --log-group-name-prefix /aws/lambda/cah-direct-")])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertNotIn("TEARDOWN-INVENTORY 0", p.stdout)
        self.assertEqual(self.left("cah-direct-"), [], "the stacks of the previous stage were removed")
        self.assertIn("TEARDOWN-INVENTORY INCOMPLETE", p.stdout)

    def test_a_failed_runtime_listing_never_authorises_the_deletion_of_a_log_group_of_that_generation(self) -> None:
        orphan = "/aws/bedrock-agentcore/runtimes/cah_direct_orphan-abcde77777-DEFAULT"
        self.legacy(faults=[denied("agentRuntimeName,'cah_direct_'")])
        w = self.open_world()
        w.w["log_groups"].append({"name": orphan, "stack": None})
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertNotIn("TEARDOWN-INVENTORY 0", p.stdout)
        self.assertIn(orphan, {g["name"] for g in self.world()["log_groups"]}, "the group of a runtime that could not be listed is kept")


class LeaseCase(LegacyBase):
    def test_a_running_build_in_the_previous_runner_stops_the_whole_run_with_exit_7_and_nothing_is_deleted(self) -> None:
        self.both(legacy_runner_present=True)
        bid = self.seed_build("IN_PROGRESS", {"SETUP_SOURCE_KEY": "source/" + "a" * 40 + ".zip", "SETUP_CONFIG_KEY": "config/accounts-0123456789ab.json", "SETUP_STAGE": "direct",
                                              "SETUP_AZS": "1", "SETUP_BINARY_NAME": "ham"}, build_id="cah-setup-runner-build:00000000-0000-4000-8000-a00000000009")
        self.seed_object("lease/direct.json", self.lease_json("a" * 40, bid), bucket=OLD_BUCKET)
        p = self.tear()
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertIn(bid, p.stdout + p.stderr)
        self.assertEqual(self.count_calls("cloudformation", "delete-stack"), 0)
        self.assertEqual(self.count_calls("s3api", "delete-objects"), 0)
        self.assertEqual(len(self.left("hamh-direct-")), 9, "the current stage is not deleted either: nothing is deleted when the plan is refused")


class RetainedCase(LegacyBase):
    LEG_VPC = "vpc-0legold000"

    def old_vpc(self, held: bool = False) -> None:
        w = self.open_world()
        tags = {**GEN_TAGS["legacy"], "aws:cloudformation:stack-name": "cah-direct-network", "aws:cloudformation:stack-id": w.stack_id("cah-direct-network", setup_fake_aws.OLD_STACK_UUID)}
        w.w.setdefault("ec2", {"vpcs": {}})["vpcs"][self.LEG_VPC] = {"subnets": ["subnet-0legold0"], "sgs": ["sg-0legold0"], "tags": tags,
                                                                   "enis": [{"id": "eni-0legold0", "status": "in-use", "managed": True}] if held else []}
        w.save()

    def test_a_retained_vpc_of_the_previous_names_is_present_never_nothing_and_is_swept(self) -> None:
        self.set_scenario(bootstrap_present=True)
        self.old_vpc()
        plan = self.run_full(*TEAR, "--dry-run")
        self.assertEqual(plan.returncode, 0, self.out(plan))
        self.assertNotIn("nothing to tear down", plan.stdout.lower())
        self.assertRegex(plan.stdout, rf"(?m)^  VPC {self.LEG_VPC}, left by an earlier teardown")
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.world()["ec2"]["vpcs"], {})
        self.assertIn(["ec2", "delete-vpc", "--vpc-id", self.LEG_VPC], self.normalized_calls())

    def test_a_retained_vpc_that_agentcore_still_holds_is_retained_and_the_run_is_not_zero(self) -> None:
        self.set_scenario(bootstrap_present=True)
        self.old_vpc(held=True)
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertIn(self.LEG_VPC, self.world()["ec2"]["vpcs"])
        self.assertRegex(p.stdout, rf"RETAINED {self.LEG_VPC}: AgentCore holds 1 network interface")
        self.assertNotIn("TEARDOWN-INVENTORY 0", p.stdout)
        self.assertNotIn("nothing to tear down", p.stdout.lower())

    def test_the_stack_of_the_previous_names_that_agentcore_holds_is_retained_like_the_current_one(self) -> None:
        self.legacy(eni_stuck=True)
        p = self.tear()
        self.assertEqual(p.returncode, 9, self.out(p))
        retain = [c for c in self.normalized_calls() if c[:2] == ["cloudformation", "delete-stack"] and "--retain-resources" in c]
        self.assertEqual(len(retain), 1)
        self.assertEqual(retain[0][retain[0].index("--stack-name") + 1], "cah-direct-network")
        self.assertIn("vpc-0leg1234", p.stdout)
        self.assertRegex(p.stdout, r"(?m)^TEARDOWN-INVENTORY RETAINED")


class BucketCase(LegacyBase):
    HARNESS = f"cah-direct-harness-{ACCOUNT}"

    def test_a_bucket_of_the_previous_names_that_carries_its_tags_and_no_stack_lists_is_present_and_is_emptied(self) -> None:
        self.set_scenario(bootstrap_present=True)
        w = self.open_world()
        w.seed_entries(self.HARNESS, 3, 2)
        w.w.setdefault("bucket_tags", {})[self.HARNESS] = dict(GEN_TAGS["legacy"])
        w.save()
        plan = self.run_full(*TEAR, "--dry-run")
        self.assertEqual(plan.returncode, 0, self.out(plan))
        self.assertNotIn("nothing to tear down", plan.stdout.lower())
        self.assertIn(f"bucket {self.HARNESS} carries the stage tags and no stack lists it", plan.stdout)
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertRegex(p.stdout, rf"(?m)^\s*emptied {re.escape(self.HARNESS)}: 5 versions and delete markers$")
        self.assertNotIn(self.HARNESS, self.world().get("buckets", {}))

    def test_a_bucket_with_the_name_of_the_previous_stage_and_the_current_tags_is_kept(self) -> None:
        self.set_scenario(bootstrap_present=True)
        w = self.open_world()
        w.seed_entries(self.HARNESS, 2, 0)
        w.w.setdefault("bucket_tags", {})[self.HARNESS] = dict(GEN_TAGS["current"])
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn(self.HARNESS, self.world()["buckets"])
        self.assertEqual(self.count_calls("s3api", "delete-objects"), 0)
        self.assertRegex(p.stdout, rf"(?m)^kept: bucket {re.escape(self.HARNESS)} was not touched")


class LoggingCase(LegacyBase):
    GROUP = "/cah/direct/bedrock-invocations"

    def test_the_invocation_logging_setting_is_put_back_from_the_record_of_the_previous_stage(self) -> None:
        self.legacy(legacy_logging_record={"loggingConfig": None}, bedrock_logging={"cloudWatchConfig": {"logGroupName": self.GROUP, "roleArn": "arn:aws:iam::1:role/x"}})
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn(["bedrock", "delete-model-invocation-logging-configuration"], self.normalized_calls())
        self.assertIsNone(self.world().get("bedrock_logging"))
        self.assertIn("/cah/direct/invocation-logging-prior", p.stdout)
        self.assertRegex(p.stdout, r"Restored Bedrock model invocation logging in Region us-east-1 to unset")

    def test_the_setting_that_names_the_current_group_is_not_restored_from_the_previous_record(self) -> None:
        self.legacy(legacy_logging_record={"loggingConfig": None}, bedrock_logging={"cloudWatchConfig": {"logGroupName": "/hamh/direct/bedrock-invocations", "roleArn": "arn:aws:iam::1:role/x"}})
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn(["bedrock", "delete-model-invocation-logging-configuration"], self.normalized_calls())
        self.assertIsNotNone(self.world().get("bedrock_logging"))
        self.assertIn("Left Bedrock model invocation logging as it is", p.stdout)


class SecretsAcrossGenerationsCase(LegacyBase):
    """The research secret of a stack goes without its recovery window only when that stack is gone; a pass for one generation never judges the stacks of the other."""

    RESEARCH_KEY_NAME = "hamh-direct-research-tavily-api-key"

    def test_a_current_stack_that_stays_keeps_its_secret_while_the_previous_stage_is_removed(self) -> None:
        self.both(faults=[denied("cloudformation delete-stack --stack-name hamh-direct-control")])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertIn("hamh-direct-control", self.left("hamh-direct-"), "the stack that could not be deleted is still there")
        self.assertEqual(self.left("cah-direct-"), [], "the previous stage went")
        secrets = [(rid, sec) for rid, sec in self.world().get("secrets", {}).items() if sec["name"] == self.RESEARCH_KEY_NAME]
        self.assertEqual(len(secrets), 1, self.world().get("secrets"))
        self.assertIsNone(secrets[0][1]["deleted"], "the secret of the stack that stays is untouched")
        forced = [c for c in self.normalized_calls() if c[:2] == ["secretsmanager", "delete-secret"] and self.RESEARCH_KEY_NAME in " ".join(c)]
        self.assertEqual(forced, [], "no delete-secret call names the secret of the stack that stays")


class SecretQueueCase(HelperCase):
    """The unit cases of the queue of research secrets (`TD_FORCE_SECRETS` holds the entries of both generations; a pass judges only the stacks of its own)."""

    def test_the_reconciliation_of_one_generation_leaves_the_queue_entries_of_the_other_alone(self) -> None:
        body = ("td_force_secrets() { echo \"FORCE $1\"; }\ntd_read_stacks() { return 0; }\nSTAGE=direct\ntd_use_generation legacy\n"
                "TD_STACKS_LISTED=1\nTD_STACKS=\"cah-direct-network${TAB}CREATE_COMPLETE${NL}\"\n"
                "TD_FORCE_SECRETS=\"hamh-direct-control${TAB}arn:one${NL}cah-direct-control${TAB}arn:two${NL}\"\ntd_force_gone_secrets\necho done")
        p = self.sh(body, "lib", "teardown")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(p.stdout.splitlines(), ["FORCE cah-direct-control", "done"],
                         "the pass of the previous names judges only the stacks it lists: a stack of the current names that is not in its listing is not gone")

    LATE = ("td_force_secrets() { echo \"FORCE $1\"; }\ntd_plan_thaw() { :; }\nSTAGE=direct\nTD_FOUND_GENS=\" current legacy\"\nTD_SECRETS=\"\"\n"
            "td_read_stacks() { TD_STACKS_LISTED=1; case $GEN in current) TD_STACKS=\"$CURRENT_LISTING\" ;; *) TD_STACKS=\"cah-direct-network${TAB}CREATE_COMPLETE${NL}\" ;; esac; }\n"
            "TD_FORCE_SECRETS=\"hamh-direct-control${TAB}arn:one${NL}cah-direct-control${TAB}arn:two${NL}\"\n")

    def test_after_the_passes_a_stack_that_went_late_gets_its_secret_deleted_for_its_own_generation(self) -> None:
        body = self.LATE + 'CURRENT_LISTING=""\nTD_SECRETS_FORCED="arn:two${NL}"\ntd_force_gone_secrets_all\necho done'
        p = self.sh(body, "lib", "teardown")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(p.stdout.splitlines(), ["FORCE hamh-direct-control", "done"], "the current stack is gone by now; the secret of the previous stage was dealt with in its own pass")

    def test_after_the_passes_a_stack_that_is_still_listed_keeps_its_secret(self) -> None:
        body = self.LATE + 'CURRENT_LISTING="hamh-direct-control${TAB}DELETE_IN_PROGRESS${NL}"\nTD_SECRETS_FORCED="arn:two${NL}"\ntd_force_gone_secrets_all\necho done'
        p = self.sh(body, "lib", "teardown")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(p.stdout.splitlines(), ["done"])

    def test_no_stacks_are_read_for_a_generation_that_has_no_secret_waiting(self) -> None:
        body = self.LATE + 'CURRENT_LISTING=""\nTD_SECRETS_FORCED="arn:one${NL}arn:two${NL}"\ntd_read_stacks() { echo READ; }\ntd_force_gone_secrets_all\necho done'
        p = self.sh(body, "lib", "teardown")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(p.stdout.splitlines(), ["done"], "every secret was dealt with: no read of the account")


class LoggingRecordAcrossGenerationsCase(LegacyBase):
    """The record of the invocation logging setting stays after a failed restore of its own generation, and only then; the decision belongs to the generation that was restored."""

    CURRENT_GROUP = "/hamh/direct/bedrock-invocations"
    CURRENT_RECORD = "/hamh/direct/invocation-logging-prior"
    LEGACY_RECORD = "/cah/direct/invocation-logging-prior"
    OURS = {"cloudWatchConfig": {"logGroupName": CURRENT_GROUP, "roleArn": "arn:aws:iam::1:role/x"}}

    def params(self) -> dict:
        return self.world().get("params", {})

    def parameter_deletes(self) -> list:
        return [c for c in self.normalized_calls() if c[:2] == ["ssm", "delete-parameters"] or c[:2] == ["ssm", "delete-parameter"]]

    def test_a_failed_restore_of_the_current_generation_keeps_its_record_whatever_the_other_generation_does(self) -> None:
        self.both(logging_record={"loggingConfig": None}, bedrock_logging=self.OURS, faults=[denied("bedrock delete-model-invocation-logging-configuration")])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertIn(self.CURRENT_RECORD, self.params(), "the record that a second teardown needs is still in the account")
        self.assertIsNotNone(self.world().get("bedrock_logging"), "the setting was not put back")
        self.assertFalse([c for c in self.parameter_deletes() if self.CURRENT_RECORD in " ".join(c)], "no call deletes the record")
        self.assertIn("The record stays at " + self.CURRENT_RECORD, p.stdout + p.stderr)

    def test_a_restored_record_is_deleted_although_the_other_generation_kept_its_own(self) -> None:
        self.both(logging_record={"loggingConfig": None}, bedrock_logging=self.OURS, legacy_logging_record="this is not JSON")
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertIsNone(self.world().get("bedrock_logging"), "the current generation put the setting back")
        self.assertNotIn(self.CURRENT_RECORD, self.params(), "its record is done with and goes with the other parameters")
        self.assertIn(self.LEGACY_RECORD, self.params(), "the record that could not be used stays")


class AgainTextCase(LegacyBase):
    """The text a run prints for a second run says what the second run does: without --yes it asks, with --yes --acknowledge-teardown it deletes without a question."""

    ASK = "Without --yes, teardown asks before it deletes; answer n to keep that stage."
    YES = "With --yes --acknowledge-teardown, teardown deletes everything in its plan without asking, including any stage deployed after this run."

    def test_a_run_without_yes_says_it_asks_and_does_not_say_it_deletes_without_asking(self) -> None:
        self.legacy()
        p = self.run_full("--teardown", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        flat = " ".join(p.stdout.split())
        self.assertIn(self.ASK, flat)
        self.assertNotIn(self.YES, flat)
        self.assertNotIn("It asks before it deletes", flat)

    def test_a_run_with_yes_and_the_acknowledgement_says_it_deletes_without_asking_and_never_that_it_asks(self) -> None:
        self.legacy(eni_stuck=True)
        for extra in (("--dry-run",), ()):
            p = self.run_full(*TEAR, *extra)
            flat = " ".join(p.stdout.split())
            self.assertIn(self.YES, flat, self.out(p))
            self.assertNotIn(self.ASK, flat)
            self.assertNotIn("answer n to keep that stage", flat)
        self.assertIn("After AWS releases the network interface, run ./setup.sh --teardown again", flat)


class GenerationNamesCase(unittest.TestCase):
    """The words of the previous generation live in one place of the product (the legacy row of lib.sh); the stand-in derives its world from the current names."""

    def test_the_stand_in_agrees_with_the_legacy_row_of_lib_sh(self) -> None:
        text = (setup_fake_aws.REPO / "scripts" / "setup" / "lib.sh").read_text()
        row = re.search(r"(?m)^\s*legacy\)\n\s*GEN=legacy; GEN_PREFIX=(\S+); GEN_PROJECT=(\S+); GEN_SETUP=(\S+); GEN_E2E=(\S+) ;;", text)
        self.assertIsNotNone(row, "the legacy row of lib.sh")
        prefix, project, setup_word, e2e = (g.rstrip(";") for g in row.groups())
        self.assertEqual(prefix, setup_fake_aws.LEGACY_WORD)
        self.assertEqual(project, setup_fake_aws.LEGACY_PROJECT)
        self.assertEqual(setup_word, leg("hamh-setup"))
        self.assertEqual(e2e, leg("hamh-e2e-"))


if __name__ == "__main__":
    unittest.main()
