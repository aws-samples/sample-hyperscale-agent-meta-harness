"""What `setup.sh --teardown` must never do to a stranger's account, and what it must never claim (review of the local teardown, findings H1 to H4, M2 to M7, R2-03).

The program deletes things in an AWS account that belongs to someone else's work as much as to the stage. Each case here puts something that is not the stage's next to the
stage, or makes a read fail, and holds the program to two rules:

1. It touches only what the stage's own tags and stacks show to be the stage's. A name that merely starts like the stage's (a stack, a repository, a rule, a state machine, a
   log group, a runtime, a parameter) is not enough, and a VPC that a stack still owns is never taken apart behind CloudFormation's back.
2. It says "nothing to tear down" and prints the zero line only when every read that claim rests on passed and found nothing. A read that is throttled is repeated; a read
   that fails is named and the run is incomplete (exit 8).

The cases run the real `setup.sh` and `scripts/setup/*.sh` against the stand-in AWS CLI of setup_fake_aws.py. No live AWS account was used.
"""

from __future__ import annotations

import json
import os
import subprocess  # nosec B404: tests start bash with an argument list, never a shell string built from input
import unittest

import setup_fake_aws
from setup_fake_aws import ACCOUNT, RESEARCH_KEY_NAME, RUNTIME_ID
from setup_full_support import normalize
from setup_waits import limit
from test_setup_script import BASH, REPO
from test_setup_teardown_local import READY_SESSION, TEAR, TeardownBase

PLAN_END = "== Stopping what is running"


def throttle(match: str, times: int = 10 ** 6, after: int = 0) -> dict:
    return {"match": match, "stderr": "An error occurred (ThrottlingException) when calling the operation (reached max retries: 2): Rate exceeded", "exit": 254, "times": times,
            "after": after}


def denied(match: str, times: int = 10 ** 6, after: int = 0) -> dict:
    return {"match": match, "stderr": "An error occurred (AccessDeniedException) when calling the operation: User is not authorized to perform this action", "exit": 254,
            "times": times, "after": after}


class SafetyBase(TeardownBase):
    def foreign_world(self) -> None:
        """Things that share the stage's prefix and are not the stage's."""
        w = self.open_world()
        for key, empty in (("rules", []), ("machines", {}), ("lambdas", []), ("projects", []), ("runtimes", {}), ("log_groups", []), ("params", {}), ("ecr", {}), ("buckets", {})):
            w.w.setdefault(key, empty)
        # a stack with the stage's name prefix and no tags, with an image repository and a bucket of its own
        w.w["stacks"]["hamh-direct-playground"] = {"status": "CREATE_COMPLETE", "outputs": {}, "exports": {}, "imports": [], "tags": {},
                                                  "resources": [{"logical": "Repo", "type": "AWS::ECR::Repository", "id": "playground-repo"},
                                                                {"logical": "Notes", "type": "AWS::S3::Bucket", "id": "my-personal-notes"}]}
        w.w["stacks"]["hamh-direct-wrongtags"] = {"status": "CREATE_COMPLETE", "outputs": {}, "exports": {}, "imports": [], "tags": {"project": "somebody-else", "stage": "direct"},
                                                 "resources": [{"logical": "Repo", "type": "AWS::ECR::Repository", "id": "wrongtags-repo"}]}
        w.w["ecr"]["playground-repo"] = [{"digest": f"sha256:{n:064x}", "tags": ["x"]} for n in (11, 12)]
        w.w["ecr"]["wrongtags-repo"] = [{"digest": f"sha256:{n:064x}", "tags": []} for n in (21,)]
        w.w["buckets"]["my-personal-notes"] = True
        w.put("my-personal-notes", "notes.txt", b"mine\n")
        w.w["rules"].append("hamh-direct-other-rule")
        w.w["machines"]["hamh-direct-other-machine"] = f"arn:aws:states:us-east-1:{ACCOUNT}:stateMachine:hamh-direct-other-machine"
        w.w["lambdas"].append("hamh-direct-other-fn")
        w.w["projects"].append("hamh-direct-other-proj")
        w.w["runtimes"]["hamh_direct_foo-abcde99999"] = {"name": "hamh_direct_foo", "arn": f"arn:aws:bedrock-agentcore:us-east-1:{ACCOUNT}:runtime/hamh_direct_foo-abcde99999",
                                                        "endpoints": ["DEFAULT", "v1"], "tags": {}}
        for name in ("/aws/lambda/hamh-direct-other-fn", "/aws/codebuild/hamh-direct-other-proj", "/aws/bedrock-agentcore/runtimes/ham_directory_agent-abcde-DEFAULT",
                     "/aws/bedrock-agentcore/runtimes/hamh_direct_foo-abcde99999-DEFAULT"):
            w.w["log_groups"].append({"name": name, "stack": None})
        for name in ("/hamh/direct/user-param", "/hamh/directx/other", "/hamh/direct-extra/x"):
            w.w["params"][name] = "mine"
        w.save()

    def untouched(self) -> None:
        """Everything foreign is exactly as it was."""
        w = self.world()
        self.assertEqual({k for k in w["stacks"] if k.startswith("hamh-direct-")} & {"hamh-direct-playground", "hamh-direct-wrongtags"}, {"hamh-direct-playground", "hamh-direct-wrongtags"})
        self.assertEqual(len(w["ecr"]["playground-repo"]), 2)
        self.assertEqual(len(w["ecr"]["wrongtags-repo"]), 1)
        self.assertIn("my-personal-notes", w["versions"])
        self.assertNotIn("hamh-direct-other-rule", w.get("disabled_rules", []))
        self.assertNotIn("other-machine", " ".join(w.get("stopped_executions", [])))
        names = {g["name"] for g in w["log_groups"]}
        for g in ("/aws/lambda/hamh-direct-other-fn", "/aws/codebuild/hamh-direct-other-proj", "/aws/bedrock-agentcore/runtimes/ham_directory_agent-abcde-DEFAULT",
                  "/aws/bedrock-agentcore/runtimes/hamh_direct_foo-abcde99999-DEFAULT"):
            self.assertIn(g, names)
        self.assertIn("hamh_direct_foo-abcde99999", w["runtimes"])
        self.assertEqual(w["runtimes"]["hamh_direct_foo-abcde99999"]["endpoints"], ["DEFAULT", "v1"])
        for name in ("/hamh/direct/user-param", "/hamh/directx/other", "/hamh/direct-extra/x"):
            self.assertIn(name, w["params"])

    def no_call_names(self, *needles: str) -> None:
        for call in self.normalized_calls():
            text = " ".join(call)
            for n in needles:
                if call[:2] not in (["cloudformation", "list-stacks"], ["cloudformation", "describe-stacks"], ["cloudformation", "list-stack-resources"], ["s3api", "head-bucket"],
                                    ["s3api", "get-bucket-tagging"], ["logs", "describe-log-groups"], ["bedrock-agentcore-control", "list-tags-for-resource"],
                                    ["codebuild", "batch-get-projects"]):  # the last reads the log settings of every project of the account, which is how a writer of another name is found
                    self.assertNotIn(n, text, f"a call touched {n}: {text}")


class StageIdentityCase(SafetyBase):
    """H3: a name prefix alone never selects a resource."""

    def test_things_that_only_share_the_prefix_are_not_touched_and_are_not_in_the_plan(self) -> None:
        self.stage()
        self.foreign_world()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.untouched()
        plan = p.stdout.split(PLAN_END)[0]
        for name in ("playground", "wrongtags", "my-personal-notes", "other-rule", "other-machine", "other-fn", "other-proj", "ham_directory_agent", "hamh_direct_foo", "user-param",
                     "directx", "direct-extra"):
            self.assertNotIn(name, plan, f"the plan lists {name}, which is not the stage's")
        self.no_call_names("playground", "wrongtags", "my-personal-notes", "other-rule", "other-machine", "other-fn", "other-proj", "ham_directory_agent", "hamh_direct_foo",
                           "user-param", "directx", "direct-extra")
        self.assertRegex(p.stdout, r"(?m)^TEARDOWN-INVENTORY 0 stacks stage=direct runner=none")
        self.assertRegex(p.stdout, r"(?m)^kept: stack hamh-direct-playground was not touched")
        self.assertEqual(self.stacks_left(), ["hamh-direct-playground", "hamh-direct-wrongtags"])

    def test_with_only_the_look_alikes_in_the_account_there_is_nothing_to_tear_down(self) -> None:
        self.set_scenario(bootstrap_present=True)
        self.foreign_world()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("nothing to tear down", p.stdout.lower())
        self.assertEqual(self.mutations(), [])
        self.untouched()

    def test_a_stack_must_carry_both_tags_and_the_prefix(self) -> None:
        """The nine stacks of the stage are the stage's because of their tags; the same names without the tags are not."""
        self.stage()
        w = self.open_world()
        del w.w["stacks"]["hamh-direct-observability"]["tags"]["stage"]
        w.w["stacks"]["hamh-direct-observability"]["imports"] = []  # nobody waits for it
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("hamh-direct-observability", self.world()["stacks"], "a stack without the stage tag is not the stage's")
        self.assertNotIn(["cloudformation", "delete-stack", "--stack-name", "hamh-direct-observability"], self.normalized_calls())
        self.assertRegex(p.stdout, r"(?m)^kept: stack hamh-direct-observability was not touched")

    def test_the_runtime_log_group_prefix_ends_in_the_delimiter(self) -> None:
        self.stage()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        prefixes = [c[c.index("--log-group-name-prefix") + 1] for c in self.normalized_calls() if c[:2] == ["logs", "describe-log-groups"]]
        self.assertIn("/aws/bedrock-agentcore/runtimes/hamh_direct_", prefixes)
        self.assertIn("/aws/lambda/hamh-direct-", prefixes)
        self.assertIn("/aws/codebuild/hamh-direct-", prefixes)
        for pre in prefixes:
            self.assertTrue(pre.endswith("_") or pre.endswith("-"), pre)

    def test_a_log_group_of_a_function_that_still_exists_is_not_removed_but_one_of_a_function_that_is_gone_is(self) -> None:
        self.stage()
        w = self.open_world()
        w.w["log_groups"].append({"name": "/aws/lambda/hamh-direct-dead-fn", "stack": None})
        w.w["lambdas"].append("hamh-direct-live-fn")
        w.w["log_groups"].append({"name": "/aws/lambda/hamh-direct-live-fn", "stack": None})
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        names = {g["name"] for g in self.world()["log_groups"]}
        self.assertNotIn("/aws/lambda/hamh-direct-dead-fn", names)
        self.assertIn("/aws/lambda/hamh-direct-live-fn", names)

    def test_only_the_parameters_with_the_stage_tags_are_deleted(self) -> None:
        self.stage()
        w = self.open_world()
        w.w["params"]["/hamh/direct/by-hand"] = "x"  # under the path, but without the tags
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        params = self.world()["params"]
        self.assertIn("/hamh/direct/by-hand", params)
        self.assertNotIn("/hamh/direct/deployment-digest", params)

    def test_a_runner_stack_without_the_project_tag_is_not_removed(self) -> None:
        self.stage()
        self.seed_runner()
        w = self.open_world()
        w.w["stacks"]["hamh-setup-runner"]["tags"] = {}
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("hamh-setup-runner", self.world()["stacks"])
        self.assertNotIn(["cloudformation", "delete-stack", "--stack-name", "hamh-setup-runner"], self.normalized_calls())

    def test_a_dry_run_names_what_it_would_touch_with_the_log_groups_and_the_parameters(self) -> None:
        """M3: the plan the person confirms holds the log groups and the parameters by name."""
        self.stage()
        self.foreign_world()
        p = self.run_full(*TEAR, "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = p.stdout
        for name in ("/hamh/direct/deployment-digest", "/hamh/direct/image-digest", f"/aws/bedrock-agentcore/runtimes/{RUNTIME_ID}-DEFAULT", "/aws/lambda/hamh-direct-control-api",
                     "/aws/codebuild/hamh-direct-image-build", f"{RESEARCH_KEY_NAME}", "hamh-direct-run-ended", "hamh-direct-worker-python", f"hamh-direct-gomod-{ACCOUNT}"):
            self.assertIn(name, plan)
        for name in ("playground", "other-fn", "user-param", "ham_directory_agent"):
            self.assertNotIn(name, plan)
        self.assertEqual(self.mutations(), [])


class VpcCase(SafetyBase):
    """H2: the VPC is swept only when no stack owns it and no stack of the stage is left, and only what carries the stage tags goes."""

    def vpc(self) -> dict:
        return self.world()["ec2"]["vpcs"]["vpc-0abc1234"]

    def ec2_changes(self) -> list:
        return [c for c in self.normalized_calls() if c[0] == "ec2" and c[1].startswith(("delete-", "detach-", "disassociate-"))]

    def test_a_foreign_stack_that_imports_the_network_stack_keeps_the_vpc_its_subnets_and_its_groups(self) -> None:
        self.stage(stacks={"other-app": {"status": "CREATE_COMPLETE", "outputs": {}, "imports": ["hamh-direct-network:VpcId"], "resources": []}})
        w = self.open_world()
        w.w["ec2"]["vpcs"]["vpc-0abc1234"]["sgs"].append("sg-otherapp0")
        w.w["ec2"]["vpcs"]["vpc-0abc1234"]["untagged"] = ["sg-otherapp0"]
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertEqual(self.stacks_left(), ["hamh-direct-network"])
        self.assertEqual(self.ec2_changes(), [], "nothing in the VPC is touched while a stack owns it")
        v = self.vpc()
        self.assertEqual((v["subnets"], sorted(v["sgs"])), (["subnet-0abc1234"], ["sg-0abc1234", "sg-otherapp0"]))
        self.assertNotIn("AgentCore holds", p.stdout, "the cause is the importer, not AgentCore")
        self.assertIn("other-app", p.stdout)
        self.assertIn("STACKS LEFT: 1", p.stdout)
        self.assertNotIn("TEARDOWN-INVENTORY 0", p.stdout)

    def test_an_undeletable_go_store_keeps_the_packages_and_network_stacks_and_the_vpc(self) -> None:
        self.stage(arn=f"arn:aws:sts::{ACCOUNT}:assumed-role/SomebodyElse/session-1", operator=["user", "setup-test"])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        left = self.stacks_left()
        self.assertIn("hamh-direct-packages", left)
        self.assertIn("hamh-direct-network", left)
        self.assertEqual(self.ec2_changes(), [])
        self.assertEqual(self.vpc()["subnets"], ["subnet-0abc1234"])
        self.assertNotRegex(p.stdout, r"(?m)^RETAINED ", "no AgentCore interface is involved")
        self.assertIn("user/setup-test", p.stdout + p.stderr)

    def test_leftover_log_groups_and_parameters_stay_while_a_stack_of_the_stage_stays(self) -> None:
        self.stage(arn=f"arn:aws:sts::{ACCOUNT}:assumed-role/SomebodyElse/session-1", operator=["user", "setup-test"])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertEqual(self.count_calls("logs", "delete-log-group"), 0)
        self.assertEqual(self.count_calls("ssm", "delete-parameters"), 0)
        self.assertIn("/hamh/direct/deployment-digest", self.world()["params"])

    def retained_once(self, **more) -> None:
        self.stage(eni_stuck=True, **more)
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertEqual(self.stacks_left(), [])

    def test_the_sweep_of_a_retained_vpc_deletes_only_what_carries_the_stage_tags(self) -> None:
        self.retained_once()
        w = self.open_world()
        v = w.w["ec2"]["vpcs"]["vpc-0abc1234"]
        v["subnets"].append("subnet-foreign")
        v["sgs"].append("sg-foreign")
        v["untagged"] = ["subnet-foreign", "sg-foreign"]
        v["route_tables"] = [{"id": "rtb-main", "main": True, "assoc": [{"id": "rtbassoc-main", "main": True}]},
                             {"id": "rtb-stage", "assoc": [{"id": "rtbassoc-1", "subnet": "subnet-0abc1234"}]}, {"id": "rtb-foreign", "assoc": []}]
        v["untagged"] += ["rtb-main", "rtb-foreign"]
        v["igws"] = ["igw-stage"]
        v["endpoints"] = ["vpce-stage"]
        w.save()
        self.set_scenario(eni_stuck=False)
        q = self.tear()
        self.assertEqual(q.returncode, 8, self.out(q))
        v = self.vpc()
        self.assertEqual(v["subnets"], ["subnet-foreign"], "the subnet without the tags is not the stage's")
        self.assertEqual(v["sgs"], ["sg-foreign"])
        self.assertEqual([t["id"] for t in v["route_tables"]], ["rtb-main", "rtb-foreign"])
        self.assertEqual((v.get("igws"), v.get("endpoints")), ([], []))
        deleted = " ".join(" ".join(c) for c in self.ec2_changes())
        for foreign in ("subnet-foreign", "sg-foreign", "rtb-foreign", "rtb-main"):
            self.assertNotIn(foreign, deleted)
        self.assertNotIn("rtbassoc-1", deleted, "the association of a stage subnet went with the subnet; nothing is disassociated by hand")
        self.assertIn("does not carry the stage tags", q.stdout + q.stderr)
        self.assertNotIn("TEARDOWN-INVENTORY 0", q.stdout)

    def test_a_retained_vpc_with_nothing_foreign_in_it_is_removed_by_the_next_run(self) -> None:
        self.retained_once()
        w = self.open_world()
        v = w.w["ec2"]["vpcs"]["vpc-0abc1234"]
        v["route_tables"] = [{"id": "rtb-stage", "assoc": [{"id": "rtbassoc-1", "subnet": "subnet-0abc1234"}]}]
        v["igws"] = ["igw-stage"]
        v["endpoints"] = ["vpce-stage"]
        w.save()
        self.set_scenario(eni_stuck=False)
        q = self.tear()
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertEqual(self.world()["ec2"]["vpcs"], {})
        self.assertRegex(q.stdout, r"(?m)^TEARDOWN-INVENTORY 0 stacks")

    def test_a_vpc_endpoint_that_cannot_be_deleted_is_a_failure_that_is_said(self) -> None:
        self.retained_once()
        w = self.open_world()
        w.w["ec2"]["vpcs"]["vpc-0abc1234"]["endpoints"] = ["vpce-stage"]
        w.save()
        self.set_scenario(eni_stuck=False, endpoint_unsuccessful=True)
        q = self.tear()
        self.assertEqual(q.returncode, 8, self.out(q))
        self.assertIn("could not delete the VPC endpoints", q.stdout)
        self.assertIn("the endpoint cannot be deleted now", q.stdout)
        self.assertIn("vpc-0abc1234", self.world()["ec2"]["vpcs"])

    def test_one_retained_vpc_is_one_retained_resource(self) -> None:
        """L1: the count of retained resources is not the count of semicolons in the notes."""
        self.stage(eni_stuck=True)
        self.assertEqual(self.tear().returncode, 8)
        q = self.tear()
        self.assertEqual(q.returncode, 8, self.out(q))
        self.assertRegex(q.stdout, r"TEARDOWN-INVENTORY RETAINED 1 resource\(s\)")

    def test_managed_network_interfaces_are_asked_for_and_a_hidden_one_is_still_retained_not_a_dependency_failure(self) -> None:
        """M2: AgentCore's interfaces may be hidden from a plain describe; the check asks for managed resources."""
        self.retained_once()
        self.set_scenario(hide_managed_enis=True)
        q = self.tear()
        self.assertEqual(q.returncode, 8, self.out(q))
        calls = [c for c in self.normalized_calls() if c[:2] == ["ec2", "describe-network-interfaces"]]
        self.assertTrue(calls)
        for c in calls:
            self.assertIn("--include-managed-resources", c)
        self.assertRegex(q.stdout, r"RETAINED vpc-0abc1234: AgentCore holds 1 network interface")
        self.assertIn("8 hours", q.stdout)
        self.assertEqual([c for c in self.ec2_changes() if c[1] in ("delete-subnet", "delete-security-group", "delete-vpc")], [], "the interfaces are seen, so nothing is attempted")

    def test_an_aws_cli_that_does_not_know_the_option_is_asked_again_without_it_and_says_what_it_cannot_see(self) -> None:
        self.retained_once()
        self.set_scenario(old_cli_no_managed=True, hide_managed_enis=True)
        q = self.tear()
        self.assertEqual(q.returncode, 8, self.out(q))
        text = q.stdout + q.stderr
        self.assertIn("cannot list the network interfaces that AWS services manage", text)
        asked = [c for c in self.normalized_calls() if c[:2] == ["ec2", "describe-network-interfaces"]]
        self.assertTrue(any("--include-managed-resources" in c for c in asked) and any("--include-managed-resources" not in c for c in asked))
        self.assertNotIn("TEARDOWN-INVENTORY 0", text)


class ReadsCase(SafetyBase):
    """H4, M4, M6: no zero line and no "nothing to tear down" unless every read that the claim rests on passed."""

    def test_a_throttled_read_is_repeated_and_the_run_still_reaches_zero(self) -> None:
        self.stage()
        self.set_scenario(faults=[throttle("logs describe-log-groups", times=3), throttle("ssm describe-parameters", times=2)])
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertRegex(p.stdout, r"(?m)^TEARDOWN-INVENTORY 0 stacks stage=direct runner=none")
        self.assertGreaterEqual(self.count_calls("logs", "describe-log-groups"), 4)

    def stays_throttled(self, match: str, said: str, after: int = 0) -> None:
        self.stage()
        self.set_scenario(faults=[throttle(match, after=after)])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertNotIn("TEARDOWN-INVENTORY 0", p.stdout)
        self.assertIn("TEARDOWN-INVENTORY INCOMPLETE", p.stdout)
        self.assertIn("could not read", p.stdout)
        self.assertIn(said, p.stdout)

    def test_log_groups_that_stay_throttled_make_the_run_incomplete(self) -> None:
        self.stays_throttled("logs describe-log-groups", "log groups")

    def test_parameters_that_stay_throttled_make_the_run_incomplete(self) -> None:
        self.stays_throttled("ssm describe-parameters", "SSM parameters")

    def test_runtimes_that_stay_throttled_make_the_run_incomplete(self) -> None:
        # the first read of the inventory and the check of the AWS CLI pass; the read after the deletes does not
        self.stays_throttled("bedrock-agentcore-control list-agent-runtimes", "AgentCore runtimes", after=2)

    def test_vpcs_that_stay_throttled_make_the_run_incomplete(self) -> None:
        self.stays_throttled("ec2 describe-vpcs", "VPCs")

    def test_buckets_that_stay_throttled_make_the_run_incomplete(self) -> None:
        self.stays_throttled("s3api head-bucket", "bucket")

    def test_a_denied_read_is_not_repeated(self) -> None:
        self.stage()
        self.set_scenario(faults=[denied("ssm describe-parameters")])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertNotIn("TEARDOWN-INVENTORY 0", p.stdout)
        # a read of each covered generation before the plan, one in the pass of the generation that holds the stage, and one of each generation at the end: 6 here;
        # a repeated read (5 tries) of the denied call would show as 10 or more
        self.assertLessEqual(self.count_calls("ssm", "describe-parameters"), 8, "AccessDenied is not a reason to repeat")

    def test_a_failed_read_of_vpcs_is_not_nothing_to_tear_down(self) -> None:
        """The reviewer's case: only a retained VPC exists and the read that would find it is denied."""
        self.set_scenario(bootstrap_present=True)
        w = self.open_world()
        w.w.setdefault("ec2", {"vpcs": {}})["vpcs"]["vpc-0abc1234"] = {"subnets": ["subnet-0abc1234"], "sgs": [], "tags": dict(setup_fake_aws.PROJECT_TAGS)}
        w.save()
        self.set_scenario(faults=[denied("ec2 describe-vpcs")])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertNotIn("nothing to tear down", p.stdout.lower())
        self.assertIn("TEARDOWN-INVENTORY INCOMPLETE", p.stdout)
        self.assertIn("vpc-0abc1234", self.world()["ec2"]["vpcs"])

    def test_the_stack_list_that_cannot_be_read_stops_the_run_before_it_deletes_anything(self) -> None:
        self.stage()
        self.set_scenario(faults=[denied("cloudformation list-stacks")])
        p = self.tear()
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertEqual(self.mutations(), [])

    def test_leftovers_with_no_stack_are_found_and_removed_instead_of_nothing_to_tear_down(self) -> None:
        """M4: an interrupted run left log groups, a parameter, a runtime and a bucket; the next run does not say there is nothing to do."""
        self.set_scenario(bootstrap_present=True)
        w = self.open_world()
        w.w.setdefault("log_groups", []).extend([{"name": "/aws/lambda/hamh-direct-control-api", "stack": None}, {"name": f"/aws/bedrock-agentcore/runtimes/{RUNTIME_ID}-DEFAULT", "stack": None}])
        w.w.setdefault("params", {})["/hamh/direct/deployment-digest"] = "0123456789ab"
        w.w.setdefault("param_tags", {})["/hamh/direct/deployment-digest"] = dict(setup_fake_aws.PROJECT_TAGS)
        bucket = f"hamh-direct-harness-{ACCOUNT}"
        w.seed_entries(bucket, 3, 2)
        w.w.setdefault("bucket_tags", {})[bucket] = dict(setup_fake_aws.PROJECT_TAGS)
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("nothing to tear down", p.stdout.lower())
        world = self.world()
        self.assertEqual(world["log_groups"], [])
        self.assertEqual(world["params"], {})
        self.assertNotIn(bucket, world["versions"])
        self.assertRegex(p.stdout, r"(?m)^TEARDOWN-INVENTORY 0 stacks")

    def test_a_runtime_that_no_stack_lists_but_that_carries_the_stage_tags_is_reported_not_deleted(self) -> None:
        self.set_scenario(bootstrap_present=True)
        w = self.open_world()
        w.w["runtimes"] = {RUNTIME_ID: {"name": "hamh_direct_worker_python", "arn": f"arn:aws:bedrock-agentcore:us-east-1:{ACCOUNT}:runtime/{RUNTIME_ID}", "endpoints": ["DEFAULT", "v1"],
                                        "tags": dict(setup_fake_aws.PROJECT_TAGS)}}
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertNotIn("nothing to tear down", p.stdout.lower())
        self.assertIn(f"aws bedrock-agentcore-control delete-agent-runtime --agent-runtime-id {RUNTIME_ID}", p.stdout)
        self.assertIn(RUNTIME_ID, self.world()["runtimes"])
        self.assertNotIn("TEARDOWN-INVENTORY 0", p.stdout)

    def test_the_checked_sentence_names_the_reads_that_were_made_and_only_those(self) -> None:
        self.stage()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        sentence = [ln for ln in p.stdout.splitlines() if ln.startswith("checked:")][-1]
        for part in ("CloudFormation stacks named hamh-direct-* and their tags", "runner stack hamh-setup-runner", "VPCs that carry the stage tags",
                     "AgentCore runtimes named hamh_direct_*", "hamh-direct-harness-<account>", "log groups of the stage's functions, projects and runtimes",
                     "SSM parameters under /hamh/direct/"):
            self.assertIn(part, sentence)
        # the account holds no stack when the result is read, so no resource list was read and the sentence does not claim one
        self.assertNotIn("resource list", sentence)

    def test_a_read_that_fails_at_the_end_is_missing_from_the_checked_sentence_and_the_run_is_incomplete(self) -> None:
        """The inventory runs three times in a clean run (before, after the stacks, after the leftovers) and reads the log groups under three prefixes each time: the seventh
        call of that kind is the first one of the last read."""
        self.stage()
        self.set_scenario(faults=[throttle("logs describe-log-groups", after=6)])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        sentence = [ln for ln in p.stdout.splitlines() if ln.startswith("checked:")][-1]
        self.assertNotIn("log groups", sentence)
        self.assertIn("SSM parameters under /hamh/direct/", sentence)
        self.assertIn("could not read the log groups under /aws/bedrock-agentcore/runtimes/hamh_direct_", p.stdout)
        self.assertNotIn("TEARDOWN-INVENTORY 0", p.stdout)

    def test_a_list_of_endpoints_that_cannot_be_read_is_said(self) -> None:
        """M6: the failed listing used to vanish inside a command substitution and the endpoints stayed without a word."""
        self.stage()
        # the first call of that kind is the check of the AWS CLI (a skeleton); the listing is the second
        self.set_scenario(faults=[denied("bedrock-agentcore-control list-agent-runtime-endpoints", after=1)])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertIn(f"could not list the endpoints of the runtime {RUNTIME_ID}", p.stdout)
        self.assertIn("hamh-direct-runtimes", self.stacks_left())


class BucketsCase(SafetyBase):
    def test_a_bucket_with_delete_markers_among_more_than_a_thousand_entries_is_emptied_and_its_stack_goes(self) -> None:
        """H1 through the whole teardown: 500 versions and 600 delete markers made the earlier script send 1,100 keys; S3 refuses that, and the stack stayed for ever."""
        self.stage()
        w = self.open_world()
        bucket = f"hamh-direct-harness-{ACCOUNT}"
        w.w["versions"].pop(bucket, None)
        w.seed_entries(bucket, 500, 600)
        w.seed_entries(f"hamh-direct-gomod-{ACCOUNT}", 10, 1500, "versions-first")
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.stacks_left(), [])
        world = self.world()
        self.assertNotIn(bucket, world["versions"])
        self.assertTrue(world["delete_sizes"])
        self.assertLessEqual(max(world["delete_sizes"]), 1000)

    def test_the_bucket_step_runs_as_the_identity_of_the_plan(self) -> None:
        """M5: the profile of the run goes to the bucket step as --profile, like every other call; exported access keys cannot make it another identity."""
        self.stage()
        other_identity = dict(zip(("AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_PROFILE"), ("ASIA-test-other", "not-" + "a-key", "other")))  # a stand-in made at run time
        p = self.tear(**other_identity)
        self.assertEqual(p.returncode, 0, self.out(p))
        seen = 0
        for raw in self.calls():
            if normalize(raw)[:2] in (["s3api", "list-object-versions"], ["s3api", "delete-objects"], ["sts", "get-caller-identity"]):
                seen += 1
                self.assertEqual(raw[:2], ["--profile", "work"], raw)
        self.assertGreaterEqual(seen, 7)
        self.assertGreaterEqual(self.count_calls("sts", "get-caller-identity"), 3, "the plan's identity, and the identity of each bucket step")
        self.assertIn("as arn:aws:iam::210987654321:user/setup-test", p.stdout, "the output says which identity empties each bucket")

    def test_a_bucket_step_whose_credentials_are_another_identity_than_the_plans_lists_nothing_and_deletes_nothing(self) -> None:
        self.stage()
        self.set_scenario(arn_later=f"arn:aws:iam::{ACCOUNT}:user/someone-else")
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertIn("this step runs as arn:aws:iam::210987654321:user/someone-else. The plan names arn:aws:iam::210987654321:user/setup-test", p.stdout)
        self.assertEqual(self.count_calls("s3api", "list-object-versions"), 0)
        self.assertEqual(self.count_calls("s3api", "delete-objects"), 0)
        self.assertIn(f"hamh-direct-harness-{ACCOUNT}", self.world()["versions"])

    def test_images_of_a_manifest_list_are_deleted_in_two_rounds_and_a_stubborn_image_is_named(self) -> None:
        """M7: the children of an image index fail until the index is gone; the second round deletes them. An image that stays is named with the reason."""
        self.stage()
        w = self.open_world()
        w.w["ecr"]["hamh-direct-delivery"] = [{"digest": f"sha256:{1:064x}", "tags": ["index"]}, {"digest": f"sha256:{2:064x}", "tags": [], "child_of": f"sha256:{1:064x}"},
                                             {"digest": f"sha256:{3:064x}", "tags": [], "child_of": f"sha256:{1:064x}"}]
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.stacks_left(), [])
        self.assertGreaterEqual(self.count_calls("ecr", "batch-delete-image"), 3)

    def test_each_image_that_cannot_be_deleted_is_named_with_its_reason(self) -> None:
        self.stage(ecr_delete_fails=True)
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        for n in (1, 2, 3):
            self.assertIn(f"image sha256:{n:064x} in hamh-direct-delivery was not deleted: Requested image referenced by manifest list", p.stdout)
        self.assertEqual(len([ln for ln in p.stdout.splitlines() if ln.startswith("FAILED: could not delete the images of the repository hamh-direct-delivery")]), 1)


class ResearchSecretCase(SafetyBase):
    """R2-03: the research secret has a fixed name; CloudFormation would keep it for 30 days and a new deploy in the same account and Region would fail."""

    def test_the_model_reproduces_the_collision_when_the_stack_deletes_the_secret(self) -> None:
        self.stage()
        w = self.open_world()
        w.drop_stack_things("hamh-direct-control", [])
        with self.assertRaises(setup_fake_aws.AwsError) as ctx:
            w.deploy_stage()
        self.assertIn("already scheduled for deletion", str(ctx.exception))

    def test_the_secret_is_deleted_without_recovery_right_after_its_stack_and_a_new_deploy_works(self) -> None:
        self.stage()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        calls = self.normalized_calls()
        mine = [c for c in calls if c[:2] == ["secretsmanager", "delete-secret"]]
        self.assertEqual(len(mine), 1)
        self.assertIn("--force-delete-without-recovery", mine[0])
        arn = f"arn:aws:secretsmanager:us-east-1:{ACCOUNT}:secret:{RESEARCH_KEY_NAME}-AbCdEf"
        self.assertEqual(mine[0][mine[0].index("--secret-id") + 1], arn)
        self.assertGreater(calls.index(mine[0]), calls.index(["cloudformation", "delete-stack", "--stack-name", "hamh-direct-control"]),
                           "CloudFormation deletes the resource policy of the secret, and cannot while the secret is marked for deletion")
        self.assertRegex(p.stdout, rf"(?m)^deleted: secret {RESEARCH_KEY_NAME} ")
        self.assertNotRegex(p.stdout, r"(?m)^pending: secret")
        self.assertEqual(self.world()["secrets"], {})
        # the name is free: the same account and Region take a new deploy
        w = self.open_world()
        w.deploy_stage()
        self.assertIn(f"arn:aws:secretsmanager:us-east-1:{ACCOUNT}:secret:{RESEARCH_KEY_NAME}-AbCdEf", w.w["secrets"])

    def test_only_that_exact_secret_is_deleted(self) -> None:
        self.stage()
        w = self.open_world()
        other = f"arn:aws:secretsmanager:us-east-1:{ACCOUNT}:secret:{RESEARCH_KEY_NAME}-other-AbCdEf"
        w.w["stacks"]["hamh-direct-control"]["resources"].append({"logical": "Other", "type": "AWS::SecretsManager::Secret", "id": other})
        w.w["secrets"][other] = {"name": f"{RESEARCH_KEY_NAME}-other", "deleted": None}
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        mine = [c for c in self.normalized_calls() if c[:2] == ["secretsmanager", "delete-secret"]]
        self.assertEqual(len(mine), 1)
        self.assertNotIn(other, mine[0])
        self.assertRegex(p.stdout, rf"(?m)^pending: secret {RESEARCH_KEY_NAME}-other ")

    def test_a_secret_that_cannot_be_deleted_without_recovery_is_a_failure_with_the_one_command_that_releases_the_name(self) -> None:
        self.stage()
        self.set_scenario(faults=[denied("secretsmanager delete-secret")])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertIn(f"aws secretsmanager delete-secret --secret-id {RESEARCH_KEY_NAME} --force-delete-without-recovery", p.stdout)
        self.assertNotIn("TEARDOWN-INVENTORY 0", p.stdout)


class RoundsCase(SafetyBase):
    def test_a_stack_that_fails_in_every_round_is_one_failure(self) -> None:
        """The stack was tried again in every round and counted, and said, each time."""
        self.stage(empty_bucket_fails=True)
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        lines = [ln for ln in p.stdout.splitlines() if ln.startswith("FAILED: could not empty the bucket hamh-direct-harness")]
        self.assertEqual(len(lines), 1, lines)

    def test_a_stack_that_fails_once_for_an_asynchronous_reason_is_deleted_by_the_retry_in_the_same_run(self) -> None:
        """DELETE_FAILED because something detaches in the background; one more look after a pause finishes the job."""
        self.stage(fail_once=["hamh-direct-observability"])
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.stacks_left(), [])
        self.assertRegex(p.stdout, r"(?m)^TEARDOWN-INVENTORY 0 stacks")

    def test_a_stack_whose_status_cannot_be_read_after_the_waiter_is_not_called_deleted(self) -> None:
        """L10."""
        self.stage()
        self.set_scenario(faults=[{"match": "cloudformation wait stack-delete-complete --stack-name hamh-direct-observability",
                                   "stderr": "Waiter StackDeleteComplete failed: Max attempts exceeded", "exit": 255, "times": 1},
                                  denied("describe-stacks --stack-name hamh-direct-observability --query Stacks[0].StackStatus", times=1)])
        p = self.tear()
        said = "the stack hamh-direct-observability did not finish deleting in the time the AWS CLI waits, and its status could not be read"
        self.assertIn(said, p.stdout)
        # the stack is not called deleted at that point; the next round asks CloudFormation again, and the read-back at the end says what is there
        self.assertLess(p.stdout.index(said), p.stdout.index("ok    stack hamh-direct-observability deleted"))
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("hamh-direct-observability", self.stacks_left())

    def test_a_pending_key_whose_state_cannot_be_read_is_said(self) -> None:
        """L3."""
        self.stage()
        self.set_scenario(faults=[denied("kms describe-key")])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertIn("could not read the state of the KMS key key-0001", p.stdout)

    def lacks(self, command: str) -> None:
        self.stage(cli_lacks=[command])
        p = self.tear()
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertEqual(self.mutations(), [])
        self.assertIn(command.split()[1], p.stderr)

    def test_an_aws_cli_without_the_session_machine_command_is_refused_before_anything_is_deleted(self) -> None:
        """The run can issue delete-capacity-provider-session; a CLI that lacks it fails late otherwise."""
        self.lacks("bedrock-agentcore delete-capacity-provider-session")

    def test_an_aws_cli_without_the_endpoint_listing_is_refused_before_anything_is_deleted(self) -> None:
        self.lacks("bedrock-agentcore-control list-agent-runtime-endpoints")

    def test_a_stored_role_that_no_longer_exists_names_the_command_that_ends_the_dead_end(self) -> None:
        """The stack stores the CDK bootstrap's execution role; with that role gone CloudFormation refuses the delete, and --role-arn names the way out."""
        self.stage()
        refusal = (f"An error occurred (ValidationError) when calling the DeleteStack operation: Role arn:aws:iam::{ACCOUNT}:role/cdk-hnb659fds-cfn-exec-role-{ACCOUNT}-us-east-1 "
                   "is invalid or cannot be assumed")
        self.set_scenario(faults=[{"match": "cloudformation delete-stack --stack-name hamh-direct-observability", "stderr": refusal, "exit": 254}])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertIn("aws cloudformation delete-stack --stack-name hamh-direct-observability --role-arn", p.stdout)
        self.assertIn("no longer exists or cannot be assumed", p.stdout)
        self.assertNotIn("--role-arn", " ".join(" ".join(c) for c in self.normalized_calls() if c[:2] == ["cloudformation", "delete-stack"]), "teardown never picks a role for you")

    def test_a_dead_end_go_store_policy_names_the_command_that_ends_it(self) -> None:
        """L5."""
        self.stage(arn=f"arn:aws:sts::{ACCOUNT}:assumed-role/SomebodyElse/session-1", operator=["user", "setup-test"])
        p = self.run_full(*TEAR, "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn(f"aws s3api delete-bucket-policy --bucket hamh-direct-gomod-{ACCOUNT}", self.out(p))


class AccountPinCase(SafetyBase):
    def test_deleting_without_a_question_needs_the_account_id(self) -> None:
        """A shell with the wrong credentials would otherwise delete in the wrong account without a question."""
        self.stage()
        args = [a for a in TEAR if a not in ("--account-id", ACCOUNT)]
        p = self.run_full(*args)
        self.assertEqual(p.returncode, 4, self.out(p))
        self.assertIn("--account-id", self.out(p))
        self.assertEqual(self.mutations(), [])
        self.assertEqual(self.calls(), [], "it stops before it calls AWS or even asks the AWS CLI for its version")

    def test_a_dry_run_needs_no_account_id(self) -> None:
        self.stage()
        args = [a for a in TEAR if a not in ("--account-id", ACCOUNT)]
        p = self.run_full(*args, "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))

    def test_another_account_id_is_exit_7_and_nothing_changes(self) -> None:
        self.stage()
        args = [a for a in TEAR if a != ACCOUNT]
        p = self.run_full(*args, "999988887777")
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(self.mutations(), [])


class ConfirmedPlanCase(SafetyBase):
    """The plan the person confirmed is the boundary of what is deleted: a thing that another person creates while the run goes on is reported, never deleted."""

    def test_what_appears_after_the_confirmation_is_not_deleted_and_the_run_is_incomplete(self) -> None:
        tags = dict(setup_fake_aws.PROJECT_TAGS)
        self.stage(on_call=[{"match": "cloudformation delete-stack --stack-name hamh-direct-network", "params": {"/hamh/direct/appeared-later": tags},
                             "log_groups": ["/aws/lambda/hamh-direct-appeared-fn"]}])
        p = self.tear()
        world = self.world()
        self.assertIn("/hamh/direct/appeared-later", world["params"], "a parameter that was not in the plan is not deleted")
        self.assertIn("/aws/lambda/hamh-direct-appeared-fn", {g["name"] for g in world["log_groups"]})
        self.assertIn("appeared after the plan", p.stdout)
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertNotIn("TEARDOWN-INVENTORY 0", p.stdout)

    def test_a_stack_that_appears_in_delete_failed_after_the_confirmation_is_not_retried(self) -> None:
        self.stage(on_call=[{"match": "cloudformation delete-stack --stack-name hamh-direct-observability", "stack": "hamh-direct-appeared"}])
        p = self.tear()
        self.assertNotIn(["cloudformation", "delete-stack", "--stack-name", "hamh-direct-appeared", "--retain-resources", "Subnet"], self.normalized_calls())
        self.assertNotIn("hamh-direct-appeared", [c[3] for c in self.normalized_calls() if c[:2] == ["cloudformation", "delete-stack"]])
        self.assertEqual(p.returncode, 8, self.out(p))


class OwnershipEdgesCase(SafetyBase):
    """Corners of ownership that an independent panel of reviewers found in the code: what a name, an ARN, a table row or an association proves."""

    def test_only_the_endpoints_named_v_and_a_number_are_deleted(self) -> None:
        self.stage()
        w = self.open_world()
        w.w["runtimes"][RUNTIME_ID]["endpoints"] = ["DEFAULT", "live", "v1", "v10", "v1production2", "v01", "vx", "v1a", "v", "v2-b"]
        w.save()
        p = self.tear()
        gone = sorted(c[c.index("--endpoint-name") + 1] for c in self.normalized_calls() if c[:2] == ["bedrock-agentcore-control", "delete-agent-runtime-endpoint"]
                      and "--generate-cli-skeleton" not in c)
        self.assertEqual(gone, ["v1", "v10"])
        self.assertEqual(p.returncode, 8, "the endpoints that are not v<N> stay, and the runtimes stack with them")

    def sessions(self, **more) -> None:
        self.stage(executions={}, sessions=[dict(READY_SESSION, session="s" * 40), dict(READY_SESSION, session="o" * 40, arn=f"arn:aws:bedrock-agentcore:us-west-2:999988887777:runtime/{RUNTIME_ID}"),
                                            dict(READY_SESSION, session="i" * 40, arn=f"arn:aws:bedrock-agentcore:us-east-1:{ACCOUNT}:runtime/{setup_fake_aws.INSTANCES_RUNTIME_ID}")], **more)

    def test_a_session_is_stopped_only_when_its_whole_arn_is_the_arn_of_a_runtime_of_the_stage(self) -> None:
        self.sessions()
        w = self.open_world()
        w.w["params"]["/hamh/direct/instances-capacity-provider-id"] = "cp-1234567890"
        w.w["param_tags"]["/hamh/direct/instances-capacity-provider-id"] = dict(setup_fake_aws.PROJECT_TAGS)
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        stopped = sorted(c[c.index("--runtime-session-id") + 1][0] for c in self.normalized_calls() if c[:2] == ["bedrock-agentcore", "stop-runtime-session"]
                         and "--generate-cli-skeleton" not in c)
        self.assertEqual(stopped, ["i", "s"], "the session on a runtime of another account and Region is left alone")
        machines = [c for c in self.normalized_calls() if c[:2] == ["bedrock-agentcore", "delete-capacity-provider-session"] and "--generate-cli-skeleton" not in c]
        self.assertEqual(len(machines), 1)
        self.assertEqual(machines[0][machines[0].index("--capacity-provider-id") + 1], "cp-1234567890")

    def test_a_capacity_provider_id_that_does_not_carry_the_stage_tags_is_not_used_to_delete_a_machine(self) -> None:
        self.sessions()
        w = self.open_world()
        w.w["params"]["/hamh/direct/instances-capacity-provider-id"] = "cp-of-somebody-else"  # under the path, but untagged
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertEqual([c for c in self.normalized_calls() if c[:2] == ["bedrock-agentcore", "delete-capacity-provider-session"] and "--generate-cli-skeleton" not in c], [])
        self.assertIn("the session " + "i" * 40 + " is on the Instances runtime and the stage publishes no capacity provider id that carries the stage tags", p.stdout)

    def test_a_route_table_association_that_is_not_the_stages_is_not_removed(self) -> None:
        self.stage(eni_stuck=True)
        self.assertEqual(self.tear().returncode, 8)
        w = self.open_world()
        v = w.w["ec2"]["vpcs"]["vpc-0abc1234"]
        v["subnets"].append("subnet-foreign")
        v["untagged"] = ["subnet-foreign"]
        v["route_tables"] = [{"id": "rtb-stage", "assoc": [{"id": "rtbassoc-1", "subnet": "subnet-0abc1234"}, {"id": "rtbassoc-foreign", "subnet": "subnet-foreign"}]}]
        w.save()
        self.set_scenario(eni_stuck=False)
        q = self.tear()
        self.assertEqual(q.returncode, 8, self.out(q))
        text = " ".join(" ".join(c) for c in self.normalized_calls())
        self.assertNotIn("disassociate-route-table", text, "the association of a stage subnet goes with the subnet, and the other one is not the stage's")
        v = self.world()["ec2"]["vpcs"]["vpc-0abc1234"]
        self.assertEqual([a["id"] for a in v["route_tables"][0]["assoc"]], ["rtbassoc-foreign"])
        self.assertIn("could not delete the route table rtb-stage", q.stdout)
        self.assertIn("If an association of this route table does not belong to the stage", q.stdout, "the text claims no foreign association that no read found")

    def test_a_gateway_association_of_a_stage_internet_gateway_is_removed_before_its_route_table(self) -> None:
        self.stage(eni_stuck=True)
        self.assertEqual(self.tear().returncode, 8)
        w = self.open_world()
        v = w.w["ec2"]["vpcs"]["vpc-0abc1234"]
        v["igws"] = ["igw-stage"]
        v["route_tables"] = [{"id": "rtb-edge", "assoc": [{"id": "rtbassoc-edge", "gateway": "igw-stage"}]}]
        w.save()
        self.set_scenario(eni_stuck=False)
        q = self.tear()
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertIn("disassociate-route-table --association-id rtbassoc-edge", " ".join(" ".join(c) for c in self.normalized_calls()))
        self.assertEqual(self.world()["ec2"]["vpcs"], {})

    def test_a_resource_that_a_stack_lists_as_deleted_is_not_the_stages_any_more(self) -> None:
        """A repository that an update removed and another application created again under the same name."""
        self.stage()
        w = self.open_world()
        w.w["stacks"]["hamh-direct-delivery"]["resources"].append({"logical": "OldRepo", "type": "AWS::ECR::Repository", "id": "reused-repo", "status": "DELETE_COMPLETE"})
        w.w["ecr"]["reused-repo"] = [{"digest": f"sha256:{n:064x}", "tags": ["x"]} for n in (31, 32)]
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(len(self.world()["ecr"]["reused-repo"]), 2)
        self.assertNotIn("reused-repo", " ".join(" ".join(c) for c in self.normalized_calls()))

    def test_a_runner_stack_whose_output_names_another_bucket_than_its_own_is_left_alone(self) -> None:
        self.stage()
        self.seed_runner()
        w = self.open_world()
        w.w["stacks"]["hamh-setup-runner"]["outputs"]["SetupBucket"] = "hamh-setup-999988887777-us-east-1"
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("hamh-setup-runner", self.world()["stacks"])
        self.assertNotIn(["cloudformation", "delete-stack", "--stack-name", "hamh-setup-runner"], self.normalized_calls())
        self.assertRegex(p.stdout, r"(?m)^kept: stack hamh-setup-runner was not touched")


class VerdictCase(SafetyBase):
    """What the result line may claim: a failed read stays a failure for the whole run, a thing that a stack left alive is left, a count says what was done."""

    def test_a_read_that_failed_early_keeps_the_run_incomplete_although_a_later_read_passed(self) -> None:
        self.stage()
        self.set_scenario(faults=[throttle("ssm describe-parameters", times=5)])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertIn("could not read the SSM parameters", p.stdout)
        self.assertNotIn("TEARDOWN-INVENTORY 0", p.stdout)
        self.assertIn("TEARDOWN-INVENTORY INCOMPLETE", p.stdout)

    def test_a_key_that_the_stack_left_enabled_is_left_not_just_kept(self) -> None:
        self.stage(retain_keys=True)
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertRegex(p.stdout, r"(?m)^LEFT: KMS key key-0001 is Enabled")
        self.assertNotIn("TEARDOWN-INVENTORY 0", p.stdout)

    def test_a_secret_that_the_stack_left_without_a_deletion_date_is_left(self) -> None:
        self.stage(retain_secrets=True)
        w = self.open_world()
        other = f"arn:aws:secretsmanager:us-east-1:{ACCOUNT}:secret:hamh-direct-kept-AbCdEf"
        w.w["stacks"]["hamh-direct-control"]["resources"].append({"logical": "Kept", "type": "AWS::SecretsManager::Secret", "id": other})
        w.w["secrets"][other] = {"name": "hamh-direct-kept", "deleted": None}
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertRegex(p.stdout, r"(?m)^LEFT: secret hamh-direct-kept still exists")

    def test_the_count_of_deleted_parameters_is_the_count_of_parameters_that_went(self) -> None:
        self.stage()
        self.set_scenario(faults=[denied("ssm delete-parameters")])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertIn("0 SSM parameters", p.stdout)
        self.assertNotIn("3 SSM parameters", p.stdout)

    def test_a_dry_run_that_could_not_read_everything_is_incomplete(self) -> None:
        self.stage()
        self.set_scenario(faults=[denied("ssm describe-parameters")])
        p = self.run_full(*TEAR, "--dry-run")
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertIn("could not read", p.stdout)
        self.assertEqual(self.mutations(), [])

    def test_a_lease_that_cannot_be_read_stops_the_teardown_before_it_deletes_anything(self) -> None:
        self.stage()
        self.seed_runner()
        self.set_scenario(faults=[denied("lease/direct.json")])
        p = self.tear()
        self.assertEqual(p.returncode, 6, self.out(p))
        self.assertEqual(self.mutations(), [])
        self.assertIn("lease", p.stderr)

    def test_a_refusal_that_names_nosuchbucket_in_a_key_is_a_failure_of_the_bucket_step(self) -> None:
        self.stage(arn=f"arn:aws:sts::{ACCOUNT}:assumed-role/SomebodyElse/session-1", operator=["user", "setup-test"])
        w = self.open_world()
        w.seed_entries(f"hamh-direct-gomod-{ACCOUNT}", 0, 0)
        w.w["versions"][f"hamh-direct-gomod-{ACCOUNT}"]["NoSuchBucket/object.txt"] = [{"id": "v900", "marker": False}]
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertIn(f"FAILED: could not empty the bucket hamh-direct-gomod-{ACCOUNT}", p.stdout)
        self.assertIn("hamh-direct-packages", self.stacks_left())


try:
    import jmespath
except ImportError:  # the check below needs the package the AWS CLI itself uses for --query
    jmespath = None


class MissingCase(unittest.TestCase):
    """aws_missing reads the error code that the AWS CLI prints, `An error occurred (<Code>) when calling ...`. A word of the message (the name of a stack, a parameter or a bucket)
    decides nothing: a denied read of a resource whose name holds `NoSuchBucket` is a refusal, not an absence."""

    def classify(self, err: str) -> int:
        p = subprocess.run([BASH, "-c", "set -u\n. scripts/setup/names.sh\n. scripts/setup/lib.sh\nAWS_ERR=$ERR\naws_missing"], env={"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "ERR": err},  # nosec B603: argument list, a snippet of the test
                           cwd=str(REPO), capture_output=True, text=True, timeout=limit(60), stdin=subprocess.DEVNULL)
        self.assertEqual(p.stderr, "")
        return p.returncode

    def test_the_codes_of_a_thing_that_is_not_there_are_missing(self) -> None:
        for err in ("An error occurred (ValidationError) when calling the DescribeStacks operation: Stack with id hamh-direct-x does not exist",
                    "An error occurred (ParameterNotFound) when calling the GetParameter operation: ",
                    "An error occurred (NoSuchKey) when calling the GetObject operation: The specified key does not exist.",
                    'fatal error: An error occurred (404) when calling the HeadObject operation: Key "lease/direct.json" does not exist',
                    "An error occurred (NoSuchBucket) when calling the ListObjectVersions operation: The specified bucket does not exist",
                    "An error occurred (ResourceNotFoundException) when calling the DescribeSecret operation: Secrets Manager can't find the specified secret.",
                    "An error occurred (InvalidVpcID.NotFound) when calling the DescribeVpcs operation: The vpc ID 'vpc-1' does not exist",
                    "An error occurred (RepositoryNotFoundException) when calling the ListImages operation: The repository with name 'x' does not exist",
                    "An error occurred (NoSuchEntity) when calling the GetRole operation: The role with name x cannot be found."):
            with self.subTest(err=err[:70]):
                self.assertEqual(self.classify(err), 0)

    def test_a_refusal_that_names_a_missing_token_in_a_resource_name_is_not_missing(self) -> None:
        for err in ("An error occurred (AccessDenied) when calling the DescribeStacks operation: User: arn:aws:sts::111122223333:assumed-role/r/s is not authorized to perform: "
                    "cloudformation:DescribeStacks on resource: arn:aws:cloudformation:us-east-1:111122223333:stack/hamh-direct-NoSuchBucket/*",
                    "An error occurred (AccessDeniedException) when calling the GetParameter operation: not authorized on resource: arn:aws:ssm:us-east-1:111122223333:parameter/hamh/direct/ParameterNotFound",
                    "An error occurred (AccessDenied) when calling the GetObject operation: Access Denied for the key does not exist/x",
                    "An error occurred (ValidationError) when calling the DeleteStack operation: Stack [hamh-direct-x] cannot be deleted while in status DELETE_IN_PROGRESS",
                    "An error occurred (ThrottlingException) when calling the ListStacks operation: Rate exceeded",
                    "Unable to locate credentials. You can configure credentials by running \"aws configure\".",
                    'Could not connect to the endpoint URL: "https://cloudformation.us-east-1.amazonaws.com/"'):
            with self.subTest(err=err[:70]):
                self.assertEqual(self.classify(err), 1)

    def test_a_text_without_a_code_never_says_that_a_thing_is_gone(self) -> None:
        """A failure of the machine (an endpoint that cannot be reached, an expired SSO token) names no code, and a word in it decides nothing."""
        for err in ("Stack with id hamh-direct-x does not exist",
                    'Could not connect to the endpoint URL: "https://NoSuchBucket.invalid/"',
                    "Error loading SSO Token: Token for my-session does not exist",
                    "something else went wrong"):
            with self.subTest(err=err[:60]):
                self.assertEqual(self.classify(err), 1)


class ClassificationCase(SafetyBase):
    """The words of a refusal never turn a failed read into a thing that is not there (review round 2, finding 2)."""

    DENIED = ("An error occurred (AccessDenied) when calling the DescribeStacks operation: User: arn:aws:sts::%s:assumed-role/r/s is not authorized to perform: "
              "cloudformation:DescribeStacks on resource: arn:aws:cloudformation:us-east-1:%s:stack/hamh-direct-NoSuchBucket/*" % (ACCOUNT, ACCOUNT))

    def test_a_denied_tag_read_of_a_stack_whose_name_holds_a_missing_token_is_a_failed_read_not_nothing_to_tear_down(self) -> None:
        self.set_scenario(bootstrap_present=True)
        w = self.open_world()
        w.w["stacks"]["hamh-direct-NoSuchBucket"] = {"status": "CREATE_COMPLETE", "outputs": {}, "exports": {}, "imports": [], "tags": dict(setup_fake_aws.PROJECT_TAGS), "resources": []}
        w.save()
        self.set_scenario(faults=[{"match": "describe-stacks --stack-name hamh-direct-NoSuchBucket", "stderr": self.DENIED, "exit": 254}])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertIn("could not read the tags of the stack hamh-direct-NoSuchBucket", p.stdout)
        self.assertNotIn("nothing to tear down", p.stdout.lower())
        self.assertNotIn("TEARDOWN-INVENTORY 0", p.stdout)
        self.assertEqual(self.mutations(), [])

    def test_a_local_failure_that_holds_a_missing_token_is_a_failed_read_not_nothing_to_tear_down(self) -> None:
        self.set_scenario(bootstrap_present=True)
        w = self.open_world()
        w.w["stacks"]["hamh-direct-network"] = {"status": "CREATE_COMPLETE", "outputs": {}, "exports": {}, "imports": [], "tags": dict(setup_fake_aws.PROJECT_TAGS), "resources": []}
        w.save()
        self.set_scenario(faults=[{"match": "describe-stacks --stack-name hamh-direct-network --query Stacks[0].Tags", "stderr": 'Could not connect to the endpoint URL: "https://NoSuchBucket.invalid/"',
                                   "exit": 255}])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertIn("could not read the tags of the stack hamh-direct-network", p.stdout)
        self.assertNotIn("nothing to tear down", p.stdout.lower())
        self.assertEqual(self.mutations(), [])

    def test_a_denied_owner_read_of_a_vpc_whose_stack_name_holds_a_missing_token_keeps_the_vpc(self) -> None:
        self.set_scenario(bootstrap_present=True)
        w = self.open_world()
        w.w.setdefault("ec2", {"vpcs": {}})["vpcs"]["vpc-0own00"] = {"subnets": ["subnet-0own00"], "sgs": [],
                                                                    "tags": {**setup_fake_aws.PROJECT_TAGS, "aws:cloudformation:stack-name": "hamh-direct-NoSuchBucket"}}
        w.save()
        self.set_scenario(faults=[{"match": "describe-stacks --stack-name hamh-direct-NoSuchBucket", "stderr": self.DENIED, "exit": 254}])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertEqual([c for c in self.normalized_calls() if c[0] == "ec2" and c[1].startswith(("delete-", "detach-", "disassociate-"))], [], "no stack is shown to be gone")
        self.assertIn("vpc-0own00", self.world()["ec2"]["vpcs"])
        self.assertIn("could not read", p.stdout)


class LeftVpcCase(SafetyBase):
    """A VPC with the stage tags that the teardown does not touch is still the stage's by its tags: it is LEFT, never a zero (review round 2, finding 3)."""

    def hand_vpc(self, name: str = "vpc-0hand0", owner: str = "") -> None:
        w = self.open_world()
        tags = dict(setup_fake_aws.PROJECT_TAGS)
        if owner:
            tags["aws:cloudformation:stack-name"] = owner
        w.w.setdefault("ec2", {"vpcs": {}})["vpcs"][name] = {"subnets": ["subnet-0hand0"], "sgs": [], "tags": tags}
        w.save()

    def test_a_stage_tagged_vpc_without_the_cloudformation_owner_tag_is_left_and_the_run_is_incomplete(self) -> None:
        self.set_scenario(bootstrap_present=True)
        self.hand_vpc()
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertEqual([c for c in self.normalized_calls() if c[0] == "ec2" and c[1].startswith(("delete-", "detach-", "disassociate-"))], [])
        self.assertIn("vpc-0hand0", self.world()["ec2"]["vpcs"])
        self.assertRegex(p.stdout, r"(?m)^LEFT: VPC vpc-0hand0 carries the stage tags and was not touched")
        self.assertIn("TEARDOWN-INVENTORY INCOMPLETE", p.stdout)
        self.assertNotIn("TEARDOWN-INVENTORY 0", p.stdout)
        self.assertNotIn("nothing to tear down", p.stdout.lower())

    def test_a_stage_tagged_vpc_that_a_stack_outside_the_stage_owns_is_left(self) -> None:
        self.set_scenario(bootstrap_present=True)
        w = self.open_world()
        w.w["stacks"]["other-app"] = {"status": "CREATE_COMPLETE", "outputs": {}, "exports": {}, "imports": [], "tags": {}, "resources": []}
        w.save()
        self.hand_vpc("vpc-0own00", owner="other-app")
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertRegex(p.stdout, r"(?m)^LEFT: VPC vpc-0own00 carries the stage tags and was not touched: the stack other-app owns it")
        self.assertIn("vpc-0own00", self.world()["ec2"]["vpcs"])

    def test_a_run_that_deletes_the_whole_stage_does_not_print_the_zero_line_while_such_a_vpc_remains(self) -> None:
        self.stage()
        self.hand_vpc()
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertEqual(self.stacks_left(), [])
        self.assertRegex(p.stdout, r"(?m)^LEFT: VPC vpc-0hand0 carries the stage tags and was not touched")
        self.assertNotIn("TEARDOWN-INVENTORY 0", p.stdout)

    def test_a_vpc_that_a_stack_of_the_stage_owns_is_not_called_left_twice(self) -> None:
        """While the stage's own network stack stays, the VPC is that stack's: the stack is listed, the VPC is not LEFT on its own."""
        self.stage(arn=f"arn:aws:sts::{ACCOUNT}:assumed-role/SomebodyElse/session-1", operator=["user", "setup-test"])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertNotRegex(p.stdout, r"(?m)^LEFT: VPC")


class LogWriterCase(SafetyBase):
    """A log group is the stage's to delete only when nothing that exists writes to it. A function or a build project may be set to write to a group of another name than
    its own (review round 2, finding 1)."""

    def test_a_log_group_that_a_function_of_another_name_is_set_to_write_to_is_kept(self) -> None:
        self.stage()
        w = self.open_world()
        w.w["lambdas"].append("production-api")
        w.w.setdefault("lambda_logs", {})["production-api"] = "/aws/lambda/hamh-direct-old"
        w.w["log_groups"].append({"name": "/aws/lambda/hamh-direct-old", "stack": None})
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("/aws/lambda/hamh-direct-old", {g["name"] for g in self.world()["log_groups"]})
        self.assertNotIn("hamh-direct-old", " ".join(" ".join(c) for c in self.normalized_calls() if c[:2] == ["logs", "delete-log-group"]))
        self.assertNotIn("/aws/lambda/hamh-direct-old", p.stdout.split(PLAN_END)[0])

    def test_a_log_group_that_a_build_project_of_another_name_is_set_to_write_to_is_kept(self) -> None:
        self.stage()
        w = self.open_world()
        w.w["projects"].append("foreign-build")
        w.w.setdefault("project_logs", {})["foreign-build"] = "/aws/codebuild/hamh-direct-old-build"
        w.w["log_groups"].append({"name": "/aws/codebuild/hamh-direct-old-build", "stack": None})
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("/aws/codebuild/hamh-direct-old-build", {g["name"] for g in self.world()["log_groups"]})
        self.assertNotIn("hamh-direct-old-build", " ".join(" ".join(c) for c in self.normalized_calls() if c[:2] == ["logs", "delete-log-group"]))

    def test_a_log_group_of_a_function_that_a_build_project_is_set_to_write_to_is_kept(self) -> None:
        """The writer can be of the other service: a project whose logs settings name a group under /aws/lambda/."""
        self.stage()
        w = self.open_world()
        w.w["projects"].append("stranger-build")
        w.w.setdefault("project_logs", {})["stranger-build"] = "/aws/lambda/hamh-direct-old"
        w.w["log_groups"].append({"name": "/aws/lambda/hamh-direct-old", "stack": None})
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("/aws/lambda/hamh-direct-old", {g["name"] for g in self.world()["log_groups"]})

    def test_a_log_group_of_a_build_project_that_a_function_is_set_to_write_to_is_kept(self) -> None:
        self.stage()
        w = self.open_world()
        w.w["lambdas"].append("stranger-fn")
        w.w.setdefault("lambda_logs", {})["stranger-fn"] = "/aws/codebuild/hamh-direct-old-build"
        w.w["log_groups"].append({"name": "/aws/codebuild/hamh-direct-old-build", "stack": None})
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("/aws/codebuild/hamh-direct-old-build", {g["name"] for g in self.world()["log_groups"]})

    def test_the_destination_of_a_published_version_keeps_a_group_too(self) -> None:
        """list-functions answers the latest configuration of each function unless it is asked for all versions."""
        self.stage()
        w = self.open_world()
        w.w["lambdas"].append("stranger-worker")
        w.w.setdefault("lambda_logs", {})["stranger-worker"] = "/aws/lambda/stranger-worker"
        w.w.setdefault("lambda_versions", {})["stranger-worker"] = ["/aws/lambda/hamh-direct-old"]
        w.w["log_groups"].append({"name": "/aws/lambda/hamh-direct-old", "stack": None})
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("/aws/lambda/hamh-direct-old", {g["name"] for g in self.world()["log_groups"]})
        listing = [c for c in self.normalized_calls() if c[:2] == ["lambda", "list-functions"]]
        self.assertTrue(listing)
        for c in listing:
            self.assertEqual(c[c.index("--function-version") + 1], "ALL")

    def test_the_build_projects_are_read_in_batches_of_at_most_a_hundred(self) -> None:
        self.stage()
        w = self.open_world()
        w.w["projects"] += [f"other-build-{n:03d}" for n in range(230)]
        w.w["log_groups"].append({"name": "/aws/codebuild/hamh-direct-gone-build", "stack": None})
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        sizes = [c.index("--query") - c.index("--names") - 1 for c in self.normalized_calls() if c[:2] == ["codebuild", "batch-get-projects"]]
        self.assertEqual(sizes[:3], [100, 100, 31], "the first inventory asks for every project once, a hundred at most in a call")
        self.assertLessEqual(max(sizes), 100)
        self.assertNotIn("/aws/codebuild/hamh-direct-gone-build", {g["name"] for g in self.world()["log_groups"]}, "no project writes to it, so it goes")

    def test_settings_of_projects_that_cannot_be_read_keep_the_groups_and_make_the_run_incomplete(self) -> None:
        self.stage()
        w = self.open_world()
        w.w["log_groups"].append({"name": "/aws/codebuild/hamh-direct-gone-build", "stack": None})
        w.save()
        self.set_scenario(faults=[denied("codebuild batch-get-projects")])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertIn("/aws/codebuild/hamh-direct-gone-build", {g["name"] for g in self.world()["log_groups"]})
        self.assertIn("could not read", p.stdout)


class WatchedReadsCase(SafetyBase):
    """A read that decides whether the next step runs is a read the run depends on (review round 2, finding 4)."""

    def test_an_import_read_that_fails_keeps_the_stack_and_makes_the_run_incomplete(self) -> None:
        self.stage()
        self.set_scenario(faults=[denied("cloudformation list-imports")])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertEqual(self.delete_order(), ["hamh-direct-observability", "hamh-direct-delivery"], "only the stacks that export nothing are deleted")
        self.assertIn("could not read the stacks that import", p.stdout)
        self.assertNotIn("TEARDOWN-INVENTORY 0", p.stdout)

    def test_a_local_failure_that_holds_the_no_importers_words_is_a_failed_read(self) -> None:
        """Only CloudFormation's own answer for the requested export says that nobody imports it."""
        self.stage()
        self.set_scenario(faults=[{"match": "cloudformation list-imports", "stderr": "Error loading SSO Token: Token for is not imported by any stack does not exist", "exit": 255}])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertEqual(self.delete_order(), ["hamh-direct-observability", "hamh-direct-delivery"])
        self.assertIn("could not read the stacks that import", p.stdout)

    def test_the_no_importers_answer_for_another_export_is_not_the_answer_for_this_one(self) -> None:
        refusal = "An error occurred (ValidationError) when calling the ListImports operation: Export 'hamh-direct-other:Id' is not imported by any stack."
        self.stage()
        self.set_scenario(faults=[{"match": "cloudformation list-imports --export-name hamh-direct-network:VpcId", "stderr": refusal, "exit": 254}])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertIn("could not read the stacks that import the export hamh-direct-network:VpcId", p.stdout)
        self.assertNotIn("hamh-direct-network", self.delete_order())

    def test_an_export_that_nobody_imports_is_not_a_failed_read(self) -> None:
        self.stage()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("could not read the stacks that import", p.stdout)

    def retained(self, **vpc) -> None:
        self.stage(eni_stuck=True)
        self.assertEqual(self.tear().returncode, 8)
        w = self.open_world()
        w.w["ec2"]["vpcs"]["vpc-0abc1234"].update(vpc)
        w.save()

    def test_a_tagged_interface_read_that_fails_is_a_failure_and_the_vpc_stays(self) -> None:
        self.retained(enis=[{"id": "eni-free", "status": "available"}])
        self.set_scenario(eni_stuck=False, faults=[denied("describe-network-interfaces --filters Name=vpc-id,Values=vpc-0abc1234 Name=tag:project")])
        q = self.tear()
        self.assertEqual(q.returncode, 8, self.out(q))
        self.assertIn("could not list the network interfaces that carry the stage tags of vpc-0abc1234", q.stdout)
        self.assertEqual([c for c in self.normalized_calls() if c[:2] in (["ec2", "delete-vpc"], ["ec2", "delete-subnet"])], [])
        self.assertIn("vpc-0abc1234", self.world()["ec2"]["vpcs"])

    def test_an_endpoint_poll_that_fails_is_a_failure_and_the_vpc_stays(self) -> None:
        self.retained(endpoints=["vpce-stage"])
        self.set_scenario(eni_stuck=False, faults=[denied("describe-vpc-endpoints", after=1)])
        q = self.tear()
        self.assertEqual(q.returncode, 8, self.out(q))
        self.assertIn("could not list the VPC endpoints of vpc-0abc1234", q.stdout)
        self.assertEqual([c for c in self.normalized_calls() if c[:2] in (["ec2", "delete-vpc"], ["ec2", "delete-subnet"])], [])
        self.assertIn("vpc-0abc1234", self.world()["ec2"]["vpcs"])

    def test_what_a_stuck_stack_failed_to_delete_is_said_when_it_cannot_be_read(self) -> None:
        self.stage(eni_stuck=True)
        self.set_scenario(faults=[denied("list-stack-resources --stack-name hamh-direct-network --query StackResourceSummaries[?ResourceStatus==", times=30)])  # the hold check, the list of what failed and each retain round read it
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertIn("could not read what the stack hamh-direct-network failed to delete", p.stdout + p.stderr)


@unittest.skipIf(jmespath is None, "the jmespath package is not installed")
class QueryConformanceCase(SafetyBase):
    """The stand-in evaluates the --query of every call with a small JMESPath of its own. This case runs the teardown in several worlds, lets the stand-in write each query with the
    document it ran on and its result, and holds every result to the real JMESPath (the package the AWS CLI uses), so a query that the stand-in reads differently from the AWS CLI
    cannot pass here and fail in an account."""

    def test_every_query_of_the_teardown_gives_the_result_the_real_jmespath_gives(self) -> None:
        log = self.tmp / "queries.jsonl"
        self.stage(query_log=str(log), executions={"hamh-direct-run": ["run-1"]}, sessions=[dict(READY_SESSION, session="s" * 40), dict(READY_SESSION, session="e" * 40, endpoint="v1")], eni_stuck=True)
        self.seed_runner()
        self.foreign_world()
        self.assertEqual(self.tear().returncode, 8)
        w = self.open_world()
        v = w.w["ec2"]["vpcs"]["vpc-0abc1234"]
        v["route_tables"] = [{"id": "rtb-main", "main": True, "assoc": [{"id": "rtbassoc-main", "main": True}]}, {"id": "rtb-stage", "assoc": [{"id": "rtbassoc-1", "subnet": "subnet-0abc1234"}]}]
        v["untagged"] = ["rtb-main"]
        v["igws"] = ["igw-stage"]
        v["endpoints"] = ["vpce-stage"]
        w.w["log_groups"].append({"name": "/aws/lambda/hamh-direct-dead-fn", "stack": None})
        # a function and a build project that are set to write to a group of another name than their own, and a group that no project writes to
        w.w["lambdas"].append("production-api")
        w.w.setdefault("lambda_logs", {})["production-api"] = "/aws/lambda/hamh-direct-old"
        w.w["log_groups"].append({"name": "/aws/lambda/hamh-direct-old", "stack": None})
        w.w["projects"].append("foreign-build")
        w.w.setdefault("project_logs", {})["foreign-build"] = "/aws/codebuild/hamh-direct-old-build"
        w.w["log_groups"] += [{"name": "/aws/codebuild/hamh-direct-old-build", "stack": None}, {"name": "/aws/codebuild/hamh-direct-gone-build", "stack": None}]
        orphan = f"hamh-direct-harness-{ACCOUNT}"
        w.seed_entries(orphan, 2, 1)
        w.w.setdefault("bucket_tags", {})[orphan] = dict(setup_fake_aws.PROJECT_TAGS)
        w.save()
        self.set_scenario(eni_stuck=False, hide_managed_enis=True)
        self.tear()
        rows = [json.loads(line) for line in log.read_text().splitlines() if line.strip()]
        self.assertGreater(len(rows), 100)
        queries = {r["query"] for r in rows}
        for needle in ("Stacks[0].Tags[].[Key,Value]", "TagSet[].[Key,Value]", "Errors[].[Code,Key,VersionId,Message]", "Unsuccessful[].[ResourceId,Error.Message]",
                       "RouteTables[0].Associations[].[RouteTableAssociationId,Main,GatewayId]", "failures[].[imageId.imageDigest,failureReason]", "[tags.project,tags.stage]",
                       "Functions[].[FunctionName,LoggingConfig.LogGroup]", "projects[].logsConfig.cloudWatchLogs.groupName"):
            self.assertIn(needle, queries, f"the worlds of this case do not run {needle}")
        for r in rows:
            self.assertEqual(jmespath.search(r["query"], r["document"]), r["result"], r["query"])


if __name__ == "__main__":
    unittest.main()
