"""`setup.sh --teardown` removes the stage with the caller's own credentials.

The earlier teardown started a build of the account-local runner that ran `scripts/teardown.sh`, a program of about 110 direct AWS calls, with the runner role's
credentials. The runner role holds none of those permissions, so the build ended at its first denied call and the stage stayed. The teardown now needs nothing
from the runner and nothing from the clone: the wizard does the few things CloudFormation cannot do (stop running work, delete the `v<N>` endpoints that a deploy
made, empty the versioned buckets and the image repositories of the stage) and then deletes the stacks, importers before exporters. CloudFormation uses the
execution role it stored with each stack, so the caller needs no permission to pass it.

The cases run the real `setup.sh` and `scripts/setup/*.sh` against the stand-in AWS CLI of setup_fake_aws.py, which keeps a nine-stack stage with the dependencies
CloudFormation checks (an export that another stack imports, a bucket that holds a version, an image that is still there, an endpoint the stack did not make, a subnet
that AgentCore's interfaces hold). No live AWS account was used; the account-level behavior is read from the AWS documentation and the product's own templates.
"""

from __future__ import annotations

import shutil
import unittest
from pathlib import Path

from setup_fake_aws import RUNTIME_ID
from setup_full_support import FullBase
from test_setup_full_state import StateBase

TEAR = ("--teardown", "--profile", "work", "--region", "us-east-1", "--yes", "--acknowledge-teardown", "--account-id", "210987654321")
# The nine stacks, in the order a teardown must delete them: every importer before the stack that exports to it.
ORDER = ["observability", "delivery", "orchestration", "runtimes", "control", "packages", "data", "security", "network"]

READY_SESSION = {"agent": "coder", "run": "run-1", "arn": f"arn:aws:bedrock-agentcore:us-east-1:210987654321:runtime/{RUNTIME_ID}",
                 "session": "session-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}


class TeardownBase(StateBase):
    def stage(self, **more) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True, nine_stacks=True, **more)

    def tear(self, *extra: str, **env: str):
        return self.run_full(*TEAR, *extra, **env)

    def delete_order(self) -> list:
        return [c[c.index("--stack-name") + 1] for c in self.normalized_calls() if c[:2] == ["cloudformation", "delete-stack"]]

    def position(self, *words: str, nth: int = 0) -> int:
        hits = [i for i, c in enumerate(self.normalized_calls()) if c[: len(words)] == list(words)]
        self.assertGreater(len(hits), nth, f"no call {' '.join(words)} (nth {nth}); calls:\n" + "\n".join(" ".join(c) for c in self.normalized_calls()))
        return hits[nth]

    def position_of(self, words: list, containing: str) -> int:
        for i, c in enumerate(self.normalized_calls()):
            if c[: len(words)] == words and containing in c:
                return i
        self.fail(f"no call {words} with {containing}; calls:\n" + "\n".join(" ".join(c) for c in self.normalized_calls()))
        return -1

    def stacks_left(self) -> list:
        return sorted(n for n, st in self.world()["stacks"].items() if n.startswith("hamh-direct-") and st["status"] != "DELETE_COMPLETE_RETAINED")


class AuthorityCase(TeardownBase):
    def test_a_teardown_installs_nothing_starts_no_build_and_assumes_no_role(self) -> None:
        self.stage()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 0)
        self.assertEqual(self.count_calls("cloudformation", "deploy"), 0)
        self.assertEqual(self.count_calls("sts", "assume-role"), 0)
        self.assertEqual(self.count_calls("s3api", "put-object"), 0, "nothing is uploaded: no source archive and no account file")
        self.assertEqual(self.stacks_left(), [])

    def test_the_plan_says_the_teardown_uses_your_credentials_and_opens_no_authority_window(self) -> None:
        self.stage()
        p = self.run_full(*TEAR, "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        flat = " ".join(p.stdout.split()).lower()
        self.assertIn("your own", flat)
        self.assertIn("no authority window", flat)
        self.assertIn("installs nothing", flat)

    def test_a_teardown_needs_no_git_clone(self) -> None:
        """A zip download of the repository has the helper files and no .git; the teardown never reads the source."""
        self.stage()
        zipped = self.tmp / "zipdownload"
        (zipped / "scripts").mkdir(parents=True)
        shutil.copy2(self.repo / "setup.sh", zipped / "setup.sh")
        shutil.copytree(self.repo / "scripts" / "setup", zipped / "scripts" / "setup")
        self.assertFalse((zipped / ".git").exists())
        p = self.run_setup(*TEAR, script=zipped / "setup.sh")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.stacks_left(), [])

    def test_a_teardown_needs_no_clean_tree(self) -> None:
        self.stage()
        (self.repo / "README.md").write_text("edited and not committed\n")
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.stacks_left(), [])

    def test_a_dry_run_makes_no_change(self) -> None:
        self.stage()
        p = self.run_full(*TEAR, "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.mutations(), [])
        self.assertEqual(len(self.stacks_left()), 9)
        for name in ("hamh-direct-network", "hamh-direct-observability"):
            self.assertIn(name, p.stdout)


class OrderCase(TeardownBase):
    def test_the_stacks_go_importers_first(self) -> None:
        self.stage()
        self.assertEqual(self.tear().returncode, 0)
        self.assertEqual(self.delete_order(), [f"hamh-direct-{n}" for n in ORDER])

    def test_every_stack_is_waited_for_before_the_next_one_is_deleted(self) -> None:
        self.stage()
        self.assertEqual(self.tear().returncode, 0)
        calls = self.normalized_calls()
        for earlier, later in zip(ORDER, ORDER[1:]):
            a = calls.index(["cloudformation", "delete-stack", "--stack-name", f"hamh-direct-{earlier}"])
            w = calls.index(["cloudformation", "wait", "stack-delete-complete", "--stack-name", f"hamh-direct-{earlier}"])
            b = calls.index(["cloudformation", "delete-stack", "--stack-name", f"hamh-direct-{later}"])
            self.assertLess(a, w)
            self.assertLess(w, b)

    def test_a_stack_that_another_stack_still_imports_is_not_deleted_under_it(self) -> None:
        self.stage(stacks={"other-app": {"status": "CREATE_COMPLETE", "outputs": {}, "imports": ["hamh-direct-network:VpcId"], "resources": []}})
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertEqual(self.stacks_left(), ["hamh-direct-network"])
        self.assertNotIn(["cloudformation", "delete-stack", "--stack-name", "hamh-direct-network"], self.normalized_calls())
        text = p.stdout + p.stderr
        self.assertIn("other-app", text, "the stack that imports it is named")
        self.assertIn("STACKS LEFT: 1", text)
        self.assertNotIn("TEARDOWN-INVENTORY 0", text)

    def test_the_buckets_are_emptied_before_their_stacks_and_the_go_store_is_emptied_with_the_bypass(self) -> None:
        self.stage()
        self.assertEqual(self.tear().returncode, 0)
        harness, gomod = f"hamh-direct-harness-{self.ACCOUNT}", f"hamh-direct-gomod-{self.ACCOUNT}"
        h = self.position_of(["s3api", "delete-objects"], harness)
        g = self.position_of(["s3api", "delete-objects"], gomod)
        self.assertLess(h, self.position_of(["cloudformation", "delete-stack"], "hamh-direct-data"))
        self.assertLess(g, self.position_of(["cloudformation", "delete-stack"], "hamh-direct-packages"))
        calls = self.normalized_calls()
        self.assertIn("--bypass-governance-retention", calls[g])
        self.assertNotIn("--bypass-governance-retention", calls[h], "the bypass is for the Go module store alone")
        w = self.world()
        self.assertNotIn(harness, w["versions"])
        self.assertNotIn(gomod, w["versions"])

    def test_the_images_are_deleted_by_digest_before_the_stack_that_owns_the_repository(self) -> None:
        self.stage()
        self.assertEqual(self.tear().returncode, 0)
        for repo, stack in (("hamh-direct-worker-python", "hamh-direct-runtimes"), ("hamh-direct-delivery", "hamh-direct-delivery")):
            batch = self.position_of(["ecr", "batch-delete-image"], repo)
            self.assertLess(batch, self.position_of(["cloudformation", "delete-stack"], stack))
            call = self.normalized_calls()[batch]
            rest = call[call.index("--image-ids") + 1:]
            ids = rest[: next((i for i, w in enumerate(rest) if w.startswith("--")), len(rest))]
            self.assertEqual(len(ids), 3, "the three images of the repository, once each: a tagged image is deleted by its digest")
            self.assertTrue(all(i.startswith("imageDigest=sha256:") for i in ids), ids)
        self.assertEqual(self.world().get("ecr", {}), {})

    def test_the_endpoints_a_deploy_made_are_deleted_before_the_runtimes_stack_and_the_stacks_own_are_left_to_it(self) -> None:
        self.stage()
        self.assertEqual(self.tear().returncode, 0)
        gone = [c for c in self.normalized_calls() if c[:2] == ["bedrock-agentcore-control", "delete-agent-runtime-endpoint"] and "--generate-cli-skeleton" not in c]
        self.assertEqual(sorted(c[c.index("--endpoint-name") + 1] for c in gone), ["v1", "v2"], "DEFAULT and live belong to the stack")
        for call in gone:
            self.assertLess(self.normalized_calls().index(call), self.position_of(["cloudformation", "delete-stack"], "hamh-direct-runtimes"))


class QuiesceCase(TeardownBase):
    def test_rules_are_disabled_and_running_work_is_stopped_before_the_first_stack_goes(self) -> None:
        self.stage(executions={"hamh-direct-run": ["run-1", "run-2"]}, sessions=[
            dict(READY_SESSION, session="s" * 40), dict(READY_SESSION, session="e" * 40, endpoint="v1"),
            dict(READY_SESSION, session="x" * 40, arn="arn:aws:bedrock-agentcore:us-east-1:210987654321:runtime/somebody_elses-abcde"),
            dict(READY_SESSION, session="d" * 40, status="stopped")])
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        first_delete = self.position("cloudformation", "delete-stack")
        self.assertLess(self.position("events", "disable-rule"), self.position("stepfunctions", "stop-execution"))
        stops = [c for c in self.normalized_calls() if c[:2] == ["stepfunctions", "stop-execution"]]
        self.assertEqual(len(stops), 2)
        sessions = [c for c in self.normalized_calls() if c[:2] == ["bedrock-agentcore", "stop-runtime-session"] and "--generate-cli-skeleton" not in c]
        by_id = {c[c.index("--runtime-session-id") + 1]: c[c.index("--qualifier") + 1] for c in sessions}
        self.assertEqual(by_id, {"s" * 40: "live", "e" * 40: "v1"}, "a session pinned to an endpoint is stopped there; a stopped session, and one on a runtime that is not the stage's, are left alone")
        for c in stops + sessions:
            self.assertLess(self.normalized_calls().index(c), first_delete)
        for line in ("1 stage EventBridge rules have been disabled.", "2 Step Functions executions have been stopped.", "2 AgentCore sessions have been stopped."):
            self.assertIn(line, p.stdout)

    def test_a_stage_without_the_harness_table_has_nothing_to_stop(self) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True)
        w = self.open_world()
        del w.w["stacks"]["hamh-direct-data"]
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        stops = [c for c in self.normalized_calls() if c[:2] == ["bedrock-agentcore", "stop-runtime-session"] and "--generate-cli-skeleton" not in c]
        self.assertEqual(stops, [], "the only call of that kind is the preflight that asks the AWS CLI whether it knows the command")


class LeftoverCase(TeardownBase):
    def test_log_groups_and_parameters_the_stacks_did_not_make_are_deleted_after_them_and_nothing_else(self) -> None:
        self.stage()
        w = self.open_world()
        w.w["log_groups"].append({"name": "/aws/lambda/some-other-app", "stack": None})
        w.w["params"]["/ham/other/keep-me"] = "1"
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        world = self.world()
        self.assertEqual([g["name"] for g in world["log_groups"]], ["/aws/lambda/some-other-app"])
        self.assertEqual(sorted(world["params"]), ["/ham/other/keep-me"])
        self.assertIn("2 log groups and 3 SSM parameters with no owning stack have been deleted.", p.stdout)
        self.assertGreater(self.position("logs", "delete-log-group"), self.position_of(["cloudformation", "delete-stack"], "hamh-direct-network"))
        self.assertGreater(self.position("ssm", "delete-parameters"), self.position_of(["cloudformation", "delete-stack"], "hamh-direct-network"))

    def test_pending_deletions_and_what_stays_are_listed_and_the_inventory_names_what_it_checked(self) -> None:
        self.stage()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        text = p.stdout
        self.assertRegex(text, r"(?m)^pending: .*key-0001.*2026-11-29")
        self.assertRegex(text, r"(?m)^deleted: secret hamh-direct-research-tavily-api-key ", "the research secret has a fixed name and is deleted without its recovery window")
        self.assertNotRegex(text, r"(?m)^pending: secret")
        self.assertRegex(text, r"(?mi)^kept:.*CDKToolkit")
        self.assertRegex(text, r"(?mi)^kept:.*bootstrap bucket")
        self.assertRegex(text, r"(?mi)^kept:.*receipt")
        self.assertRegex(text, r"(?m)^TEARDOWN-INVENTORY 0 stacks stage=direct runner=none")
        self.assertIn("untagged", text.lower(), "the line says what the inventory cannot see")


class FailureCase(TeardownBase):
    def test_an_aws_cli_without_the_agentcore_commands_is_refused_before_anything_is_deleted(self) -> None:
        self.stage(cli_without_agentcore=True)
        p = self.tear()
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertEqual(self.mutations(), [])
        text = " ".join((p.stdout + p.stderr).split())
        self.assertIn("AgentCore", text)
        self.assertIn("upgrade", text.lower())
        self.assertEqual(len(self.stacks_left()), 9)

    def test_a_bucket_that_cannot_be_emptied_keeps_its_stack_and_the_stacks_it_depends_on_and_is_exit_8(self) -> None:
        self.stage(empty_bucket_fails=True)
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        text = p.stdout + p.stderr
        self.assertEqual(self.stacks_left(), ["hamh-direct-data", "hamh-direct-network", "hamh-direct-packages", "hamh-direct-security"])
        self.assertIn("STACKS LEFT: 4", text)
        self.assertIn(f"hamh-direct-harness-{self.ACCOUNT}", text)
        self.assertNotIn("TEARDOWN-INVENTORY 0", text)
        self.assertEqual(self.receipt()["state"], "teardown-incomplete")

    def test_another_identity_cannot_empty_the_go_module_store_and_the_run_names_the_one_that_can(self) -> None:
        self.stage(arn=f"arn:aws:sts::{self.ACCOUNT}:assumed-role/SomebodyElse/session-1", operator=["user", "setup-test"])
        plan = self.run_full(*TEAR, "--dry-run")
        self.assertEqual(plan.returncode, 0, self.out(plan))
        flat = " ".join(plan.stdout.split())
        self.assertIn("Go module store", flat)
        self.assertIn("user/setup-test", flat, "the identity the store trusts is named before anything is deleted")
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertIn("hamh-direct-packages", self.stacks_left())
        self.assertIn("user/setup-test", p.stdout + p.stderr)
        self.assertNotIn(f"hamh-direct-harness-{self.ACCOUNT}", self.world().get("buckets", {}), "everything else that this identity may delete is gone")

    def test_the_operator_is_trusted_whatever_role_session_it_signed_in_with(self) -> None:
        self.stage(arn=f"arn:aws:sts::{self.ACCOUNT}:assumed-role/Admin/session-9", operator=["role", "Admin"])
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.stacks_left(), [])

    def test_a_stack_that_stays_in_delete_failed_is_tried_again_by_the_next_run(self) -> None:
        self.stage(ecr_delete_fails=True)
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertIn("hamh-direct-runtimes", self.stacks_left())
        self.set_scenario(ecr_delete_fails=False)
        q = self.tear()
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertEqual(self.stacks_left(), [])

    def test_an_endpoint_that_cannot_be_deleted_stops_the_runtimes_stack_and_what_it_holds_up(self) -> None:
        self.stage(endpoint_delete_fails=True)
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        left = self.stacks_left()
        self.assertIn("hamh-direct-runtimes", left)
        self.assertIn("hamh-direct-network", left)
        self.assertIn("v1", p.stdout + p.stderr)

    def test_a_stack_that_is_being_changed_is_exit_7_and_nothing_is_deleted(self) -> None:
        self.stage()
        w = self.open_world()
        w.w["stacks"]["hamh-direct-control"].update({"status": "UPDATE_IN_PROGRESS", "polls_left": 10 ** 6})
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(self.mutations(), [])

    def test_a_setup_build_that_is_running_is_exit_7_and_nothing_is_deleted(self) -> None:
        self.stage()
        self.seed_runner()
        bid = self.seed_build("IN_PROGRESS", {"SETUP_SOURCE_KEY": "source/" + "a" * 40 + ".zip", "SETUP_CONFIG_KEY": "config/accounts-0123456789ab.json", "SETUP_STAGE": "direct",
                                              "SETUP_AZS": "1", "SETUP_BINARY_NAME": "ham"})
        self.seed_object("lease/direct.json", self.lease_json("a" * 40, bid))
        p = self.tear()
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertIn(bid, p.stdout + p.stderr)
        self.assertEqual(self.count_calls("cloudformation", "delete-stack"), 0)
        self.assertEqual(self.count_calls("s3api", "delete-objects"), 0)


class NetworkCase(TeardownBase):
    def test_interfaces_agentcore_still_holds_are_retained_and_reported_and_the_next_run_removes_them(self) -> None:
        self.stage(eni_stuck=True)
        p = self.tear()
        self.assertEqual(p.returncode, 9, self.out(p))
        retain = [c for c in self.normalized_calls() if c[:2] == ["cloudformation", "delete-stack"] and "--retain-resources" in c]
        self.assertEqual(len(retain), 1)
        self.assertEqual(sorted(retain[0][retain[0].index("--retain-resources") + 1:]), ["PrivateSubnet", "Vpc", "WorkerSecurityGroup"], "the VPC cannot go while its subnet stays, so it is retained with it")
        text = p.stdout + p.stderr
        for needle in ("RETAINED", "subnet-0abc1234", "sg-0abc1234", "vpc-0abc1234", "8 hours", "./setup.sh --teardown"):
            self.assertIn(needle, text)
        self.assertNotIn("TEARDOWN-INVENTORY 0", text)
        self.assertEqual(self.stacks_left(), [])
        self.assertIn("subnet-0abc1234", self.receipt()["retained"])
        # AWS lets the interfaces go: the same command removes the network resources that were retained, found by the project and stage tags of their VPC
        self.set_scenario(eni_stuck=False)
        q = self.tear()
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertEqual(self.world()["ec2"]["vpcs"], {})
        self.assertRegex(q.stdout, r"(?m)^TEARDOWN-INVENTORY 0 stacks stage=direct")
        self.assertEqual(self.receipt()["state"], "torn-down")
        self.assertEqual(self.receipt()["retained"], "")

    def test_a_rerun_while_the_interfaces_are_still_there_changes_nothing_and_is_exit_9(self) -> None:
        self.stage(eni_stuck=True)
        self.assertEqual(self.tear().returncode, 9)
        before = self.world()["ec2"]
        q = self.tear()
        self.assertEqual(q.returncode, 9, self.out(q))
        self.assertEqual(self.world()["ec2"], before)
        self.assertIn("8 hours", q.stdout + q.stderr)

    def test_a_vpc_of_another_project_is_never_touched(self) -> None:
        self.stage()
        w = self.open_world()
        w.w["ec2"]["vpcs"]["vpc-0foreign9"] = {"subnets": ["subnet-0foreign9"], "sgs": [], "tags": {"project": "somebody-else", "stage": "direct"}}
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("vpc-0foreign9", self.world()["ec2"]["vpcs"])
        self.assertNotIn("subnet-0foreign9", " ".join(" ".join(c) for c in self.normalized_calls() if c[:2] == ["ec2", "delete-subnet"]))


class RunnerCase(TeardownBase):
    def test_a_runner_that_was_kept_is_removed_first_with_the_callers_credentials(self) -> None:
        self.stage()
        self.seed_runner()
        self.seed_object("results/b1/result.json", b"{}")
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("hamh-setup-runner", self.world()["stacks"])
        self.assertLess(self.position_of(["cloudformation", "delete-stack"], "hamh-setup-runner"), self.position_of(["cloudformation", "delete-stack"], "hamh-direct-observability"),
                        "the runner and its authority go before the long deletes begin")
        self.assertGreaterEqual(self.count_calls("s3api", "delete-objects"), 1)
        self.assertRegex(p.stdout, r"(?m)^TEARDOWN-INVENTORY 0 stacks stage=direct runner=none")

    def test_the_plan_names_the_runner_and_the_end_of_its_authority(self) -> None:
        self.stage()
        self.seed_runner()
        w = self.open_world()
        w.w["stacks"]["hamh-setup-runner"]["params"] = {"AuthorityExpires": "2099-01-01T00:00:00Z"}
        w.save()
        p = self.run_full(*TEAR, "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        flat = " ".join(p.stdout.split())
        self.assertIn("hamh-setup-runner", flat)
        self.assertIn("2099-01-01T00:00:00Z", flat)

    def test_a_runner_that_cannot_be_removed_does_not_stop_the_stage_from_going_and_is_exit_8(self) -> None:
        self.stage(runner_delete_fails=True)
        self.seed_runner()
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertEqual(self.stacks_left(), [])
        self.assertIn("RETAINED", p.stdout + p.stderr)
        self.assertIn("hamh-setup-runner", p.stdout + p.stderr)

    def test_with_nothing_left_it_says_so(self) -> None:
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.mutations(), [])
        self.assertIn("nothing to tear down", p.stdout.lower())


class ConfirmationCase(TeardownBase):
    def test_without_the_acknowledgement_nothing_is_deleted(self) -> None:
        self.stage()
        for args in (("--teardown", "--yes"), ("--teardown", "--no-input"), ("--teardown",)):
            with self.subTest(args=args):
                p = self.run_full(*args, "--profile", "work", "--region", "us-east-1")
                self.assertEqual(p.returncode, 4, self.out(p))
                self.assertEqual(self.mutations(), [])
        self.assertEqual(len(self.stacks_left()), 9)


REPO = Path(__file__).resolve().parents[2]


class ScriptCase(unittest.TestCase):
    """What the teardown script may not do, read from its own code: each of these would install something, delegate authority or need the source."""

    def code(self) -> str:
        lines = (REPO / "scripts" / "setup" / "teardown.sh").read_text(encoding="utf-8").splitlines()
        return "\n".join(ln for ln in lines if not ln.lstrip().startswith("#"))

    def test_the_teardown_installs_nothing_delegates_nothing_and_reads_no_source(self) -> None:
        code = self.code()
        for banned in ("rn_install", "rn_upload", "rn_start", "rn_follow", "rn_lease_take", "rn_bootstrap", "pf_source", "pf_installer", "pf_zones", "pf_templates", "codebuild start-build", "aws_run codebuild",
                       "cloudformation deploy", "assume-role", "git archive", "SETUP_ACTION", "put-object", "git -C"):
            self.assertNotIn(banned, code, f"teardown.sh must not use {banned}")

    def test_the_runner_stack_is_deleted_before_the_stacks_of_the_stage(self) -> None:
        body = self.code()
        body = body[body.index("run_teardown() {"):]
        self.assertLess(body.index("rn_remove"), body.index("td_delete_stacks"))
        self.assertLess(body.index("rn_remove"), body.index("td_quiesce"))


class FakeStageCase(FullBase):
    def test_the_stand_in_stage_has_the_nine_stacks_of_the_product(self) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True, nine_stacks=True)
        self.assertEqual(sorted(self.open_world().w["stacks"]), sorted(["CDKToolkit"] + [f"hamh-direct-{n}" for n in ORDER]))


if __name__ == "__main__":
    unittest.main()
