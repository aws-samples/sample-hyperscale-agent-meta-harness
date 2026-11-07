"""What the first run of `./setup.sh --teardown` against a real account found, held in the stand-in AWS CLI (findings T1 to T6).

The run deleted exactly the stage's own resources and nothing else, and it ended with these defects, each of which the stand-in now models:

T1  The research secret was deleted before its stack. CloudFormation's delete of the secret's resource policy then failed once, because the secret was still "marked for
    deletion" for about 30 seconds. The stack is deleted first, the secret right after it, and the verdict waits until the secret is gone.
T2  The runner stack could not be removed in an account whose session policy refuses to delete a role that has been used. The screen named a bucket that was already gone and
    hid the cause, the result had no line for the runner, and a sentence told the person to delete the stage they had just deployed. The delete is tried again with the CDK
    CloudFormation execution role, and when that fails too the cause, a runner line in the result and the command that removes the stack are printed.
T3  The network stack took 1 hour 50 minutes of silent waiting although the held AgentCore interface was visible after the first failed round. The held resources are retained
    in one call as soon as the interface is seen, and a line is printed every few minutes while a stack is deleted.
T4  Wrong words: the run's own retained VPC was "late", the tag index lags for hours, the runner's key had no `pending:` line, the plan emptied a bucket that was gone.
T5  The screen had no count of the emptied buckets.
T6  A retained VPC became invisible once the stage was deployed again, because the owner of a VPC was decided by the stack's name and the new stack has the same name.

The cases run the real `setup.sh` and `scripts/setup/*.sh` against the stand-in of setup_fake_aws.py. No live AWS account is used here; the live facts they rest on were measured
in the account first: a resource that did not fail can be named in `--retain-resources`, a secret scheduled for deletion accepts `--force-delete-without-recovery` and is not found
about 30 seconds later, `delete-stack --role-arn` with the CDK CloudFormation execution role removes the runner stack the caller's session could not.
"""

from __future__ import annotations

import json
import math
import re
import subprocess  # nosec B404: tests start bash with an argument list, never a shell string built from input
import time
import unittest

import setup_fake_aws
from setup_fake_aws import ACCOUNT, OLD_STACK_UUID, PROJECT_TAGS, RESEARCH_KEY_NAME, RUNNER_KEY_ID, exec_role_arn
from setup_full_support import FullBase
from setup_waits import SCALE, limit
from test_setup_script import BASH
from test_setup_teardown_local import TEAR
from test_setup_teardown_safety import PLAN_END, SafetyBase, denied

try:
    import jmespath
except ImportError:  # the check below needs the package the AWS CLI itself uses for --query
    jmespath = None

SECRET_ARN = f"arn:aws:secretsmanager:us-east-1:{ACCOUNT}:secret:{RESEARCH_KEY_NAME}-AbCdEf"
EXEC_ROLE = exec_role_arn(ACCOUNT, "us-east-1")
RUNNER_BUCKET = f"hamh-setup-{ACCOUNT}-us-east-1"


# The ticker tests count the lines a program prints in windows of a second or two. A busy host stretches every step, so the interval, the polling and the waits of these tests
# are multiplied by one whole number, HAMH_TEST_WAIT_SCALE rounded up: the proportions the tests rely on stay, and the delay that fits in a window grows with the factor.
Q = max(1, math.ceil(SCALE))

class LiveBase(SafetyBase):
    def network_deletes(self) -> tuple:
        """The calls that delete the network stack: those that wait for CloudFormation to try, and those that retain resources."""
        calls = [c for c in self.normalized_calls() if c[:4] == ["cloudformation", "delete-stack", "--stack-name", "hamh-direct-network"]]
        return [c for c in calls if "--retain-resources" not in c], [c for c in calls if "--retain-resources" in c]

    def retained_ids(self, call: list) -> list:
        rest = call[call.index("--retain-resources") + 1:]
        return sorted(rest[: next((i for i, w in enumerate(rest) if w.startswith("--")), len(rest))])

    def vpc_changes(self) -> list:
        return [c for c in self.normalized_calls() if c[0] == "ec2" and c[1].startswith(("delete-", "detach-", "disassociate-"))]


class SecretOrderCase(LiveBase):
    """T1."""

    def test_the_stand_in_fails_a_stack_whose_secret_was_force_deleted_a_moment_before(self) -> None:
        self.stage()
        w = self.open_world()
        self.assertEqual(w.stage_blockers("hamh-direct-control"), [])
        w.w["secrets"][SECRET_ARN]["vanish"] = 5  # force-deleted, and not gone yet
        self.assertEqual([b[1] for b in w.stage_blockers("hamh-direct-control")], ["AWS::SecretsManager::ResourcePolicy"])

    def test_the_stack_is_deleted_once_without_a_failed_round_and_the_secret_right_after_it(self) -> None:
        self.stage()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        calls = self.normalized_calls()
        stack = [i for i, c in enumerate(calls) if c[:4] == ["cloudformation", "delete-stack", "--stack-name", "hamh-direct-control"]]
        secret = [i for i, c in enumerate(calls) if c[:2] == ["secretsmanager", "delete-secret"]]
        self.assertEqual(len(stack), 1, "the stack of the secret is deleted once: it does not fail on a secret that was deleted a moment before")
        self.assertEqual(len(secret), 1)
        self.assertGreater(secret[0], stack[0])
        self.assertIn("--force-delete-without-recovery", calls[secret[0]])
        self.assertEqual(calls[secret[0]][calls[secret[0]].index("--secret-id") + 1], SECRET_ARN)
        self.assertNotIn("DELETE_FAILED", p.stdout)
        self.assertNotIn("could not be deleted", p.stdout)
        self.assertLess(p.stdout.index("ok    stack hamh-direct-control deleted"), p.stdout.index("deleted: secret"))
        self.assertEqual(self.world()["secrets"], {})
        self.assertRegex(p.stdout, r"(?m)^TEARDOWN-INVENTORY 0 stacks stage=direct runner=none")

    def test_the_name_is_free_for_a_new_deploy_when_the_run_ends(self) -> None:
        self.stage()
        self.assertEqual(self.tear().returncode, 0)
        w = self.open_world()
        w.deploy_stage()
        self.assertIn(SECRET_ARN, w.w["secrets"])

    def test_the_verdict_waits_until_the_secret_is_gone(self) -> None:
        self.stage(secret_vanish_calls=200)
        p = self.tear(**{"HAMH_SETUP_SECRET_LOOKS": "2000"})
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertGreaterEqual(self.count_calls("secretsmanager", "describe-secret"), 5, "it looks again and again until the secret is not found")
        self.assertEqual(self.world()["secrets"], {})
        self.assertRegex(p.stdout, rf"(?m)^deleted: secret {RESEARCH_KEY_NAME} ")

    def test_a_secret_that_stays_after_the_bounded_wait_is_left_not_deleted(self) -> None:
        self.stage(secret_vanish_calls=10 ** 6)
        p = self.tear(**{"HAMH_SETUP_SECRET_LOOKS": "3"})
        self.assertEqual(p.returncode, 8, "the fixed name is not free for a new deploy: the stage is not clean")
        self.assertEqual(self.count_calls("secretsmanager", "describe-secret"), 3)
        self.assertRegex(p.stdout, rf"(?m)^LEFT: secret {RESEARCH_KEY_NAME} is still marked for deletion after 3 looks")
        self.assertNotRegex(p.stdout, rf"(?m)^deleted: secret {RESEARCH_KEY_NAME} ")
        self.assertNotIn("TEARDOWN-INVENTORY 0", p.stdout)

    def test_a_secret_that_the_force_delete_did_not_mark_is_left_after_three_looks(self) -> None:
        self.stage(secret_force_ignored=True)
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertEqual(self.count_calls("secretsmanager", "describe-secret"), 3, "the mark may take a moment to show: three looks in a row decide")
        self.assertRegex(p.stdout, rf"(?m)^LEFT: secret {RESEARCH_KEY_NAME} still exists")
        self.assertNotRegex(p.stdout, r"(?m)^pending: secret")
        self.assertNotIn("TEARDOWN-INVENTORY 0", p.stdout)

    def test_a_stack_that_stays_keeps_its_secret_untouched(self) -> None:
        self.stage()
        self.set_scenario(faults=[denied("cloudformation delete-stack --stack-name hamh-direct-control")])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertEqual(self.count_calls("secretsmanager", "delete-secret"), 0, "the secret belongs to a stack that is still there")
        self.assertNotIn("vanish", self.world()["secrets"][SECRET_ARN])

    def test_the_plan_says_when_the_secret_goes(self) -> None:
        self.stage()
        p = self.run_full(*TEAR, "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        line = [ln for ln in p.stdout.splitlines() if ln.startswith(f"  secret {RESEARCH_KEY_NAME}-AbCdEf")]
        self.assertEqual(len(line), 1, p.stdout)
        self.assertIn("after its stack", line[0])
        self.assertNotIn("at once", line[0])


class RunnerRoleCase(LiveBase):
    """T2."""

    def runner_world(self, mode: str = "caller") -> None:
        self.stage(runner_role_denied=mode)
        self.seed_runner()

    def runner_deletes(self) -> list:
        return [c for c in self.normalized_calls() if c[:4] == ["cloudformation", "delete-stack", "--stack-name", "hamh-setup-runner"]]

    def test_a_refused_role_delete_is_tried_again_with_the_cdk_execution_role(self) -> None:
        self.runner_world()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        deletes = self.runner_deletes()
        self.assertEqual(len(deletes), 2)
        self.assertNotIn("--role-arn", deletes[0])
        self.assertEqual(deletes[1][deletes[1].index("--role-arn") + 1], EXEC_ROLE)
        self.assertNotIn("hamh-setup-runner", self.world()["stacks"])
        removing = p.stdout.split("== Removing the runner")[1].split("== Stopping")[0]
        self.assertIn("is not authorized to perform: iam:DeleteRole", removing, "the cause is on the screen")
        self.assertIn("RunnerRole", removing)
        self.assertIn(EXEC_ROLE, removing, "the role that does it is named")
        self.assertIn("runner removed", removing)
        self.assertNotRegex(p.stdout, r"(?m)^RETAINED")
        self.assertRegex(p.stdout, r"(?m)^TEARDOWN-INVENTORY 0 stacks stage=direct runner=none")

    def test_the_runner_key_has_a_pending_line_like_the_keys_of_the_stage(self) -> None:
        self.runner_world()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertRegex(p.stdout, rf"(?m)^pending: KMS key {RUNNER_KEY_ID} has deletion scheduled for 2026-11-06")
        self.assertRegex(p.stdout, r"(?m)^pending: KMS key key-0001 ")

    def test_a_runner_that_cannot_be_removed_is_said_in_the_result_with_its_cause_and_the_command(self) -> None:
        self.runner_world("always")
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        removing = p.stdout.split("== Removing the runner")[1].split("== Stopping")[0]
        block = removing[removing.index("RETAINED stack hamh-setup-runner"):]
        self.assertIn("RETAINED stack hamh-setup-runner (DELETE_FAILED)", block)
        self.assertIn("RunnerRole (AWS::IAM::Role)", block)
        self.assertIn("is not authorized to perform: iam:DeleteRole", block)
        self.assertIn("aws cloudformation delete-stack --stack-name hamh-setup-runner --role-arn <ARN of a role that may delete the stack's IAM role>", block,
                      "the execution role was tried and failed, so the command asks for another role")
        self.assertNotIn(f"--role-arn {EXEC_ROLE}", block)
        self.assertIn(EXEC_ROLE, block, "the role that was tried is named")
        self.assertNotIn(RUNNER_BUCKET, block, "the bucket went with the stack's first attempt: the line does not name a bucket that is not there")
        self.assertNotIn("waiter", removing.lower())
        result = p.stdout.split("== Teardown result")[1]
        self.assertRegex(result, r"(?m)^LEFT: stack hamh-setup-runner ")
        self.assertIn("TEARDOWN-INVENTORY INCOMPLETE", result)
        self.assertEqual(self.stacks_left(), [])
        self.assertEqual(len(self.runner_deletes()), 2, "the call with the role is made once")

    def test_a_failure_that_is_not_a_refusal_is_not_tried_with_another_role(self) -> None:
        self.stage(runner_delete_fails=True)
        self.seed_runner()
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertNotIn("--role-arn", " ".join(" ".join(c) for c in self.normalized_calls()))
        self.assertRegex(p.stdout, r"(?m)^LEFT: stack hamh-setup-runner ")

    def test_the_plan_names_the_role_that_may_be_used_for_the_runner(self) -> None:
        self.runner_world()
        p = self.run_full(*TEAR, "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = p.stdout.split("== Dry run")[0]
        self.assertIn(f"deleted again with the CDK CloudFormation execution role {EXEC_ROLE}", plan, "the person confirms the plan, so the plan names the fallback")

    def test_a_refusal_that_is_no_iam_action_of_the_caller_is_not_worked_around_with_the_execution_role(self) -> None:
        self.runner_world("bucket_policy")
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertNotIn("--role-arn", " ".join(" ".join(c) for c in self.normalized_calls()), "an S3 policy that denies the delete is not the caller's IAM refusal")
        removing = p.stdout.split("== Removing the runner")[1].split("== Stopping")[0]
        block = removing[removing.index("RETAINED stack hamh-setup-runner"):]
        self.assertIn("Access Denied", block)
        self.assertIn("aws cloudformation delete-stack --stack-name hamh-setup-runner", block)
        self.assertNotIn("--role-arn", block, "the plain delete is what is offered")

    def test_a_refusal_of_another_principal_is_not_worked_around(self) -> None:
        self.runner_world("other_principal")
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertNotIn("--role-arn", " ".join(" ".join(c) for c in self.normalized_calls()), "the refused principal is not the caller, so another role for this delete is no remedy")
        removing = p.stdout.split("== Removing the runner")[1].split("== Stopping")[0]
        self.assertIn("OldCfnRole", removing)
        self.assertNotIn("--role-arn", removing[removing.index("RETAINED stack hamh-setup-runner"):])

    def test_a_denial_that_comes_late_in_a_long_reason_still_decides_the_retry_and_the_screen_shows_less(self) -> None:
        self.runner_world("long_caller")
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(sum(1 for c in self.normalized_calls() if "--role-arn" in c), 1, "the whole reason decides, not the 400 characters that are shown")
        removing = p.stdout.split("== Removing the runner")[1].split("== Stopping")[0]
        self.assertIn("...", removing, "the reason on the screen is shortened")

    def test_a_bucket_that_cannot_be_emptied_is_still_named(self) -> None:
        self.stage(empty_bucket_fails=True)
        self.seed_runner()
        self.seed_object("results/b1/result.json", b"{}")  # a bucket with nothing in it is emptied without a single delete call
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        removing = p.stdout.split("== Removing the runner")[1].split("== Stopping")[0]
        block = removing[removing.index("RETAINED stack hamh-setup-runner"):]
        self.assertIn(f"bucket {RUNNER_BUCKET}", block, "the bucket is still there, so the line names it")
        self.assertIn("could not be emptied", block)

    def test_the_plan_of_a_second_run_does_not_empty_a_bucket_that_is_gone(self) -> None:
        self.runner_world("always")
        self.assertEqual(self.tear().returncode, 8)
        q = self.run_full(*TEAR, "--dry-run")
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertNotIn(f"empty the bucket {RUNNER_BUCKET}", q.stdout)
        line = [ln for ln in q.stdout.splitlines() if ln.startswith("  stack hamh-setup-runner")]
        self.assertEqual(len(line), 1, q.stdout)
        self.assertIn("bucket", line[0])
        self.assertIn("is gone", line[0])

    def test_a_second_run_with_the_role_removes_the_runner_that_the_first_could_not(self) -> None:
        self.runner_world("always")
        self.assertEqual(self.tear().returncode, 8)
        self.set_scenario(runner_role_denied="caller")
        q = self.tear()
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertNotIn("hamh-setup-runner", self.world()["stacks"])


class EventWindowCase(LiveBase):
    """The reason of the latest delete decides the retry with the execution role, not a denial that an earlier try met."""

    def test_a_denial_of_an_earlier_try_does_not_send_the_next_try_to_the_execution_role(self) -> None:
        self.stage(runner_role_denied="always")
        self.seed_runner()
        self.assertEqual(self.tear().returncode, 8)
        before = sum(1 for c in self.normalized_calls() if "--role-arn" in c)
        self.assertEqual(before, 1)
        self.set_scenario(runner_role_denied=None, runner_fails_otherwise=True)
        q = self.tear()
        self.assertEqual(q.returncode, 8, self.out(q))
        self.assertEqual(sum(1 for c in self.normalized_calls() if "--role-arn" in c), before, "the second run's failure is no denial: the older denial in the events is not read")
        removing = q.stdout.split("== Removing the runner")[1].split("== Teardown result")[0]
        self.assertIn("SetupBucketPolicy", removing)
        self.assertNotIn("is not authorized to perform", removing)
        self.assertNotIn("--role-arn", removing, "no denial of the caller, so the command offered is the plain delete")


class HelperCase(FullBase):
    """Run a snippet of bash with the helper files sourced, in the checkout."""

    def sh(self, body: str, *files: str, **env: str):
        sources = ". scripts/setup/names.sh\n" + "".join(f". scripts/setup/{f}.sh\n" for f in files)
        return subprocess.run([BASH, "-c", "set -u\n" + sources + body], capture_output=True, text=True, env=self.env(**env), cwd=str(self.repo),  # nosec B603: argument list, test snippets
                              timeout=limit(60), stdin=subprocess.DEVNULL)


class ReviewCase(HelperCase):
    """The findings of the review of the diff: a ticker that ends with its parent, numbers with a leading zero, a stack that went another way."""

    SAY = 'say() { printf "%s\\n" "$*"; }\n'

    def test_a_ticker_whose_parent_is_gone_ends_without_printing(self) -> None:
        out = self.tmp / "ticker.out"
        body = "set -u\n. scripts/setup/names.sh\n. scripts/setup/lib.sh\n" + self.SAY + 'setup_ticker_start "stack X"\nexit 0'
        with open(out, "w", encoding="utf-8") as fh:  # a file, not a pipe: the test does not wait for a pipe that the ticker still holds
            p = subprocess.run([BASH, "-c", body], stdout=fh, stderr=subprocess.STDOUT, env=self.env(HAMH_SETUP_PROGRESS_SECONDS=str(Q)), cwd=str(self.repo),  # nosec B603: argument list, test snippet
                               timeout=limit(30), stdin=subprocess.DEVNULL)
        self.assertEqual(p.returncode, 0)
        time.sleep(2.5 * Q)
        self.assertEqual(out.read_text(encoding="utf-8"), "", "the parent was gone when the ticker woke: it prints nothing and ends")

    def test_a_ticker_whose_parent_is_gone_does_not_hold_a_pipe_for_the_interval(self) -> None:
        body = "set -u\n. scripts/setup/names.sh\n. scripts/setup/lib.sh\n" + self.SAY + 'setup_ticker_start "stack X"\nexit 0'
        p = subprocess.Popen([BASH, "-c", body], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, cwd=str(self.repo),  # nosec B603: argument list, test snippet
                             env=self.env(HAMH_SETUP_PROGRESS_SECONDS="3600", HAMH_SETUP_TICKER_CHECK_SECONDS="1"), stdin=subprocess.DEVNULL)
        p.wait(timeout=limit(30))  # the parent is collected here, so the ticker finds it gone at its next look
        out, _ = p.communicate(timeout=limit(20))  # a ticker that looked only once an hour would hold the pipe for an hour
        self.assertEqual(out, "")

    def test_stopping_the_ticker_leaves_no_sleep_behind(self) -> None:
        p = self.sh(self.SAY + 'setup_ticker_start "stack X"\nsleep 0.4\nsetup_ticker_stop\nsleep 0.4', "lib", HAMH_SETUP_PROGRESS_SECONDS="7123")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(p.stderr, "")
        left = subprocess.run(["pgrep", "-f", "sleep 7123"], capture_output=True, text=True, timeout=limit(30))  # nosec B603: argument list, no shell; B607: pgrep of the system, looked up on PATH
        self.assertEqual(left.returncode, 1, f"a sleep of the ticker is still there: {left.stdout}")

    def test_a_ticker_prints_while_its_parent_waits_and_stops_when_asked(self) -> None:
        p = self.sh(self.SAY + 'setup_ticker_start "stack X"\nsleep ' + f"{2.5 * Q:g}" + '\nsetup_ticker_stop\nsleep ' + f"{1.5 * Q:g}", "lib", HAMH_SETUP_PROGRESS_SECONDS=str(Q))
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertRegex(p.stdout, rf"still waiting for stack X \({Q} seconds so far\)")
        self.assertEqual(p.stdout.count("still waiting"), 2, p.stdout)
        self.assertEqual(p.stderr, "")

    def test_a_second_start_stops_the_first_ticker(self) -> None:
        p = self.sh(self.SAY + 'setup_ticker_start "stack A"\nsetup_ticker_start "stack B"\nsleep ' + f"{1.5 * Q:g}" + '\nsetup_ticker_stop', "lib", HAMH_SETUP_PROGRESS_SECONDS=str(Q))
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertNotIn("stack A", p.stdout)
        self.assertIn("stack B", p.stdout)

    def test_a_number_with_a_leading_zero_is_a_decimal_number_and_zero_or_junk_turns_the_ticker_off(self) -> None:
        p = self.sh(self.SAY + 'setup_ticker_start "stack X"\nsleep ' + f"{1.5 * Q:g}" + '\nsetup_ticker_stop', "lib", HAMH_SETUP_PROGRESS_SECONDS=f"0{Q}")
        self.assertIn("still waiting", p.stdout)
        self.assertEqual(p.stderr, "")
        for value in ("08", "00", "0", "abc", "1x", "-3"):
            with self.subTest(value=value):
                q = self.sh(self.SAY + 'setup_ticker_start "stack X"\nsleep 0.3\nsetup_ticker_stop\necho "pid=[${SETUP_TICKER_PID}]"', "lib", HAMH_SETUP_PROGRESS_SECONDS=value)
                self.assertEqual(q.returncode, 0, q.stderr)
                self.assertEqual(q.stderr, "", f"{value} must not reach the arithmetic as an octal number")
                self.assertNotIn("still waiting", q.stdout)

    def test_a_queued_secret_of_a_stack_that_went_without_td_delete_one_is_deleted_by_the_reconciliation(self) -> None:
        body = ("td_force_secrets() { echo \"FORCE $1\"; }\ntd_read_stacks() { return 0; }\nSTAGE=direct\n"
                "TD_STACKS_LISTED=1\nTD_STACKS=\"hamh-direct-network${TAB}CREATE_COMPLETE${NL}\"\n"
                "TD_FORCE_SECRETS=\"hamh-direct-control${TAB}arn:one${NL}hamh-direct-network${TAB}arn:two${NL}\"\ntd_force_gone_secrets\n"
                "TD_STACKS_LISTED=0\ntd_force_gone_secrets\necho done")
        p = self.sh(body, "lib", "teardown")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(p.stdout.splitlines(), ["FORCE hamh-direct-control", "done"], "a stack that is listed keeps its secret, and an unread list decides nothing")

    def test_only_an_iam_refusal_of_the_caller_that_is_no_service_control_policy_is_a_denial(self) -> None:
        caller = "arn:aws:sts::210987654321:assumed-role/Admin/me"
        deny = f"User: {caller} is not authorized to perform: iam:DeleteRole on resource: role r with an explicit deny in a session policy"
        cases = [
            (deny, True),
            (f"Resource handler returned message: \"{'x' * 600} {deny}\"", True),
            (deny.replace(caller, "arn:aws:sts::210987654321:assumed-role/OldCfnRole/s"), False),
            (deny.replace("session policy", "service control policy"), False),
            (f"User: {caller} is not authorized to perform: s3:DeleteBucket on resource: b", False),
            ("Access Denied (Service: S3, Status Code: 403)", False),
            ("", False),
        ]
        for text, want in cases:
            with self.subTest(text=text[:70]):
                p = self.sh('rn_is_denial "$TEXT" && echo yes || echo no', "lib", "runner", CALLER_ARN=caller, TEXT=text)
                self.assertEqual(p.stdout.strip(), "yes" if want else "no", p.stderr)
        q = self.sh('CALLER_ARN=""; rn_is_denial "$TEXT" && echo yes || echo no', "lib", "runner", TEXT=deny)
        self.assertEqual(q.stdout.strip(), "no", "without a caller there is nothing to compare, so nothing is worked around")

    def test_a_secret_is_forced_once(self) -> None:
        body = ("td_try() { echo \"CALL $*\"; return 0; }\n"
                "STAGE=direct\nTD_FORCE_SECRETS=\"hamh-direct-control${TAB}arn:one${NL}\"\nTD_SECRETS_FORCED=\"\"\nTD_SECRETS=\"\"\n"
                "td_force_secrets hamh-direct-control\ntd_force_secrets hamh-direct-control\nprintf '%s' \"$TD_SECRETS_FORCED\"")
        p = self.sh(body, "lib", "teardown")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(len([ln for ln in p.stdout.splitlines() if ln.startswith("CALL secretsmanager delete-secret")]), 1, p.stdout)
        self.assertIn("arn:one", p.stdout)


class SecretBoundCase(LiveBase):
    """The numbers that bound the wait for the secret."""

    def test_the_secret_wait_takes_a_leading_zero_as_a_decimal_number(self) -> None:
        self.stage(secret_vanish_calls=10 ** 6)
        p = self.tear(**{"HAMH_SETUP_SECRET_LOOKS": "04"})
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertEqual(self.count_calls("secretsmanager", "describe-secret"), 4)
        self.assertEqual(p.stderr, "")

    def test_a_bound_of_the_secret_wait_that_is_no_number_is_the_default_of_twenty_looks(self) -> None:
        self.stage(secret_vanish_calls=10 ** 6)
        p = self.tear(**{"HAMH_SETUP_SECRET_LOOKS": "abc"})
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertEqual(self.count_calls("secretsmanager", "describe-secret"), 20)
        self.assertEqual(p.stderr, "")


class SetupCleanupCase(FullBase):
    """T2: the cleanup that setup itself does after a deploy uses the same removal."""

    def test_setup_removes_its_runner_with_the_cdk_execution_role_when_the_session_may_not(self) -> None:
        self.set_scenario(runner_role_denied="caller")
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("hamh-setup-runner", self.world()["stacks"])
        self.assertTrue(self.installed().is_file())
        deletes = [c for c in self.normalized_calls() if c[:4] == ["cloudformation", "delete-stack", "--stack-name", "hamh-setup-runner"]]
        self.assertEqual([("--role-arn" in c) for c in deletes], [False, True])

    def test_a_runner_that_stays_does_not_tell_the_person_to_delete_the_stage(self) -> None:
        self.set_scenario(runner_role_denied="always")
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertTrue(self.installed().is_file())
        text = p.stdout + p.stderr
        self.assertIn("aws cloudformation delete-stack --stack-name hamh-setup-runner --role-arn <ARN of a role that may delete the stack's IAM role>", text)
        done = [ln for ln in p.stderr.splitlines() if "setup is done" in ln]
        self.assertEqual(len(done), 1, p.stderr)
        self.assertNotIn("--teardown later", done[0], "teardown removes the whole stage")
        self.assertIn("remove only the runner", done[0])


class NetworkHoldCase(LiveBase):
    """T3."""

    def test_the_first_failed_round_shows_the_hold_and_one_call_retains_the_subnet_the_group_and_the_vpc(self) -> None:
        self.stage(eni_stuck=True)
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        plain, retain = self.network_deletes()
        self.assertEqual(len(plain), 1, "no second try of a delete that the held interface blocks")
        self.assertEqual(len(retain), 1)
        self.assertEqual(self.retained_ids(retain[0]), ["PrivateSubnet", "Vpc", "WorkerSecurityGroup"])
        calls = self.normalized_calls()
        looked = [i for i, c in enumerate(calls) if c[:2] == ["ec2", "describe-network-interfaces"] and calls.index(plain[0]) < i < calls.index(retain[0])]
        self.assertTrue(looked, "the interface is looked at between the failed round and the retain call")
        self.assertEqual(self.stacks_left(), [])

    def test_the_reason_the_retained_resources_and_the_commands_are_on_the_screen(self) -> None:
        self.stage(eni_stuck=True)
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        text = p.stdout
        self.assertRegex(text, r"1 network interface\(s\) in vpc-0abc1234 are still in use")
        for physical, logical in (("subnet-0abc1234", "PrivateSubnet"), ("sg-0abc1234", "WorkerSecurityGroup"), ("vpc-0abc1234", "Vpc")):
            self.assertRegex(text, rf"(?m)^RETAINED {physical} \(AWS::EC2::\w+ {logical} in stack hamh-direct-network\)")
        self.assertRegex(text, r"TEARDOWN-INVENTORY RETAINED 3 resource\(s\)")
        commands = ["aws ec2 delete-subnet --subnet-id subnet-0abc1234", "aws ec2 delete-security-group --group-id sg-0abc1234", "aws ec2 delete-vpc --vpc-id vpc-0abc1234"]
        at = [text.index(c) for c in commands]
        self.assertEqual(at, sorted(at), "the commands are in the order that works: the subnet, the group, then the VPC")
        self.assertTrue(all(ln.strip() in commands for ln in text.splitlines() if ln.strip().startswith("aws ec2 delete-")))
        self.assertEqual(self.receipt()["retained"].count("vpc-0abc1234"), 1)

    def test_the_time_and_the_hold_are_said_before_anything_is_deleted(self) -> None:
        self.stage(eni_stuck=True)
        p = self.tear()
        plan = p.stdout.split(PLAN_END)[0]
        self.assertIn("about 20 to 40 minutes when AgentCore has no network interface in the stage's VPC, and about 50 minutes when AgentCore still holds its network interface", plan)
        self.assertIn("up to 8 hours", plan)
        self.assertIn("retained", plan)

    def test_a_retain_that_cloudformation_refuses_is_not_reported_as_retained(self) -> None:
        self.stage(eni_stuck=True)
        self.set_scenario(faults=[denied("--stack-name hamh-direct-network --retain-resources")])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertNotRegex(p.stdout, r"(?m)^RETAINED ")
        self.assertIn("could not delete the stack hamh-direct-network again", p.stdout)
        self.assertIn("hamh-direct-network", self.stacks_left())
        self.assertNotIn("TEARDOWN-INVENTORY RETAINED", p.stdout)
        self.assertNotIn("TEARDOWN-INVENTORY 0", p.stdout)
        self.assertEqual(self.receipt().get("retained", ""), "")

    def test_a_failed_round_without_a_held_interface_is_tried_again_and_nothing_is_retained(self) -> None:
        self.stage(fail_once=["hamh-direct-network"])
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        plain, retain = self.network_deletes()
        self.assertEqual((len(plain), len(retain)), (2, 0))
        self.assertNotRegex(p.stdout, r"(?m)^RETAINED")

    def test_when_the_interfaces_cannot_be_read_the_rounds_of_before_apply(self) -> None:
        self.stage(eni_stuck=True)
        self.set_scenario(faults=[denied("ec2 describe-network-interfaces")])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        plain, retain = self.network_deletes()
        self.assertGreaterEqual(len(plain), 2, "no hold was seen, so the delete was tried again")
        self.assertGreaterEqual(len(retain), 1)
        self.assertEqual(self.stacks_left(), [])

    def test_a_cloudformation_that_takes_only_the_resources_that_failed_is_given_those_and_then_the_vpc(self) -> None:
        self.stage(eni_stuck=True, retain_only_failed=True)
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        plain, retain = self.network_deletes()
        self.assertEqual(len(plain), 1)
        self.assertEqual([self.retained_ids(c) for c in retain], [["PrivateSubnet", "Vpc", "WorkerSecurityGroup"], ["PrivateSubnet", "WorkerSecurityGroup"], ["Vpc"]])
        self.assertEqual(self.stacks_left(), [])
        self.assertRegex(p.stdout, r"TEARDOWN-INVENTORY RETAINED 3 resource\(s\)")

    def test_a_long_wait_prints_a_line_every_few_minutes(self) -> None:
        self.stage(wait_seconds={"hamh-direct-observability": 2.6 * Q})
        p = self.tear(HAMH_SETUP_PROGRESS_SECONDS=str(Q))
        self.assertEqual(p.returncode, 0, self.out(p))
        lines = [ln for ln in p.stdout.splitlines() if "still waiting" in ln]
        self.assertGreaterEqual(len(lines), 1, p.stdout)
        self.assertTrue(all("hamh-direct-observability" in ln for ln in lines), lines)
        self.assertLess(p.stdout.index("still waiting"), p.stdout.index("ok    stack hamh-direct-observability deleted"))
        self.assertNotIn("Terminated", p.stdout + p.stderr, "the shell does not report the ticker it stopped")

    def test_a_wait_shorter_than_the_interval_prints_no_line(self) -> None:
        self.stage(wait_seconds={"hamh-direct-observability": 1.2})
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("still waiting", p.stdout, "the default is a line every three minutes")

    def test_the_progress_lines_stop_with_the_wait(self) -> None:
        self.stage(wait_seconds={"hamh-direct-observability": 1.3 * Q})
        p = self.tear(HAMH_SETUP_PROGRESS_SECONDS=str(Q))
        self.assertEqual(p.returncode, 0, self.out(p))
        after = p.stdout.split("ok    stack hamh-direct-observability deleted")[1]
        self.assertNotIn("still waiting", after)


class WordsCase(LiveBase):
    """T4 and T5."""

    def test_the_stage_s_own_retained_vpc_is_not_called_a_late_arrival(self) -> None:
        self.stage(eni_stuck=True)
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertNotIn("appeared after the plan", p.stdout)
        self.assertNotIn("Removing the network resources an earlier teardown retained", p.stdout, "this run retained it; the run does not sweep it")
        self.assertEqual(self.vpc_changes(), [])

    def test_the_tag_index_can_lag_for_hours(self) -> None:
        self.stage(tag_index=["arn:aws:ec2:us-east-1:210987654321:vpc-endpoint/vpce-0123"])
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        line = [ln for ln in p.stdout.splitlines() if ln.startswith("The tag index lists")]
        self.assertEqual(len(line), 1, p.stdout)
        self.assertIn("lag behind a delete by hours", line[0])
        self.assertNotIn("several minutes", line[0])

    def bucket_entries(self, bucket: str) -> int:
        return sum(len(vs) for vs in self.world()["versions"].get(bucket, {}).values())

    def test_each_emptied_bucket_has_its_count_on_the_screen(self) -> None:
        self.stage()
        self.seed_runner()
        self.seed_object("results/b1/result.json", b"{}")
        harness, gomod = f"hamh-direct-harness-{ACCOUNT}", f"hamh-direct-gomod-{ACCOUNT}"
        want = {b: self.bucket_entries(b) for b in (harness, gomod, RUNNER_BUCKET)}
        self.assertTrue(all(n > 0 for n in want.values()), want)
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        for bucket, n in want.items():
            self.assertRegex(p.stdout, rf"(?m)^\s*emptied {re.escape(bucket)}: {n} versions and delete markers$")

    def test_an_orphan_bucket_has_its_count_too(self) -> None:
        self.set_scenario(bootstrap_present=True)
        w = self.open_world()
        bucket = f"hamh-direct-harness-{ACCOUNT}"
        w.seed_entries(bucket, 3, 2)
        w.w.setdefault("bucket_tags", {})[bucket] = dict(PROJECT_TAGS)
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertRegex(p.stdout, rf"(?m)^\s*emptied {re.escape(bucket)}: 5 versions and delete markers$")


class TwoGenerationsCase(LiveBase):
    """T6: a VPC that an earlier network stack retained is found by the id of its stack, also when a stack of the same name exists now."""

    OLD = "vpc-0old000000"

    def old_vpc(self, held: bool = False, tags: dict | None = None) -> None:
        w = self.open_world()
        vpc_tags = {**PROJECT_TAGS, "aws:cloudformation:stack-name": "hamh-direct-network", "aws:cloudformation:stack-id": w.stack_id("hamh-direct-network", OLD_STACK_UUID)}
        vpc_tags.update(tags or {})
        w.w.setdefault("ec2", {"vpcs": {}})["vpcs"][self.OLD] = {"subnets": ["subnet-0old000"], "sgs": ["sg-0old000"], "tags": vpc_tags,
                                                               "enis": [{"id": "eni-0old000", "status": "in-use", "managed": True}] if held else []}
        w.save()

    def test_the_plan_of_a_deployed_stage_names_the_vpc_of_the_earlier_stack(self) -> None:
        self.stage()
        self.old_vpc()
        p = self.run_full(*TEAR, "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = p.stdout.split("== Dry run")[0]
        self.assertRegex(plan, rf"(?m)^  VPC {self.OLD}, left by an earlier teardown")
        self.assertNotRegex(plan, r"(?m)^  VPC vpc-0abc1234", "the VPC of the stack that exists is that stack's")
        self.assertEqual(self.mutations(), [])

    def test_the_vpc_of_the_earlier_stack_is_removed_after_the_new_stage_when_its_interfaces_are_gone(self) -> None:
        self.stage()
        self.old_vpc()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.world()["ec2"]["vpcs"], {})
        self.assertIn(["ec2", "delete-vpc", "--vpc-id", self.OLD], self.normalized_calls())
        self.assertNotIn(["ec2", "delete-vpc", "--vpc-id", "vpc-0abc1234"], self.normalized_calls(), "the VPC of the new stack goes with its stack")
        self.assertRegex(p.stdout, r"(?m)^TEARDOWN-INVENTORY 0 stacks stage=direct")
        self.assertNotIn("appeared after the plan", p.stdout)

    def test_the_vpc_of_the_earlier_stack_that_AgentCore_still_holds_is_retained_with_the_commands(self) -> None:
        self.stage()
        self.old_vpc(held=True)
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertEqual(list(self.world()["ec2"]["vpcs"]), [self.OLD])
        self.assertEqual(self.stacks_left(), [])
        self.assertRegex(p.stdout, rf"RETAINED {self.OLD}: AgentCore holds 1 network interface")
        for command in ("aws ec2 delete-subnet --subnet-id subnet-0old000", "aws ec2 delete-security-group --group-id sg-0old000", f"aws ec2 delete-vpc --vpc-id {self.OLD}"):
            self.assertIn(command, p.stdout)
        self.assertNotIn(["ec2", "delete-subnet", "--subnet-id", "subnet-0old000"], self.normalized_calls())

    def test_the_vpc_of_the_earlier_stack_is_not_swept_while_a_stack_of_the_stage_stays(self) -> None:
        self.stage(arn=f"arn:aws:sts::{ACCOUNT}:assumed-role/SomebodyElse/session-1", operator=["user", "setup-test"])
        self.old_vpc()
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertIn("hamh-direct-network", self.stacks_left())
        self.assertIn(self.OLD, self.world()["ec2"]["vpcs"])
        self.assertEqual(self.vpc_changes(), [])
        self.assertRegex(p.stdout, rf"(?m)^LEFT: VPC {self.OLD} ")

    def test_a_vpc_whose_stack_id_is_the_id_of_the_stack_that_exists_is_that_stacks(self) -> None:
        self.stage()
        self.old_vpc(tags={"aws:cloudformation:stack-id": self.open_world().stack_id("hamh-direct-network")})
        p = self.run_full(*TEAR, "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn(self.OLD, p.stdout.split("== Dry run")[0], "the same stack id is the same stack: nobody sweeps its VPC behind it")

    def test_a_vpc_with_a_stack_name_and_no_stack_id_is_decided_by_the_name_as_before(self) -> None:
        self.stage()
        w = self.open_world()
        w.w["ec2"]["vpcs"]["vpc-0abc1234"]["tags"].pop("aws:cloudformation:stack-id")
        w.save()
        p = self.run_full(*TEAR, "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotRegex(p.stdout, r"(?m)^  VPC vpc-0abc1234")


class SweepCase(LiveBase):
    """The sweep of a retained VPC against the stand-in, from the run that retains it to the run that removes it: the live test could not reach it, because the stage was
    deployed again before AWS released the interface."""

    COMMANDS = ["aws ec2 delete-subnet --subnet-id subnet-0abc1234", "aws ec2 delete-security-group --group-id sg-0abc1234", "aws ec2 delete-vpc --vpc-id vpc-0abc1234"]

    def test_retained_then_still_held_then_swept(self) -> None:
        self.stage(eni_stuck=True)
        first = self.tear()
        self.assertEqual(first.returncode, 8, self.out(first))
        w = self.open_world()
        v = w.w["ec2"]["vpcs"]["vpc-0abc1234"]
        v["route_tables"] = [{"id": "rtb-stage", "assoc": [{"id": "rtbassoc-1", "subnet": "subnet-0abc1234"}]}]
        v["igws"] = ["igw-stage"]
        v["endpoints"] = ["vpce-stage"]
        w.save()
        # a second run while AWS still holds the interface changes nothing and prints the same commands
        second = self.tear()
        self.assertEqual(second.returncode, 8, self.out(second))
        self.assertEqual(self.vpc_changes(), [])
        for command in self.COMMANDS:
            self.assertIn(command, second.stdout)
        self.assertRegex(second.stdout, r"RETAINED vpc-0abc1234: AgentCore holds 1 network interface")
        # AWS lets the interface go: the third run removes what the first retained, and nothing else
        self.set_scenario(eni_stuck=False)
        third = self.tear()
        self.assertEqual(third.returncode, 0, self.out(third))
        self.assertEqual(self.world()["ec2"]["vpcs"], {})
        order = [c[1] for c in self.vpc_changes()]
        self.assertLess(order.index("delete-vpc-endpoints"), order.index("delete-subnet"))
        self.assertLess(order.index("delete-subnet"), order.index("delete-security-group"))
        self.assertLess(order.index("delete-security-group"), order.index("delete-vpc"))
        self.assertRegex(third.stdout, r"(?m)^TEARDOWN-INVENTORY 0 stacks stage=direct")
        self.assertEqual(self.receipt()["state"], "torn-down")

    def test_a_sweep_that_cannot_delete_prints_the_commands_for_what_is_left(self) -> None:
        self.stage(eni_stuck=True)
        self.assertEqual(self.tear().returncode, 8)
        self.set_scenario(eni_stuck=False, faults=[denied("ec2 delete-subnet")])
        q = self.tear()
        self.assertEqual(q.returncode, 8, self.out(q))
        self.assertIn("could not delete the subnet subnet-0abc1234", q.stdout)
        self.assertIn("vpc-0abc1234", self.world()["ec2"]["vpcs"])
        self.assertIn("aws ec2 delete-subnet --subnet-id subnet-0abc1234", q.stdout)
        self.assertIn("aws ec2 delete-vpc --vpc-id vpc-0abc1234", q.stdout)
        self.assertNotIn("aws ec2 delete-security-group --group-id sg-0abc1234", q.stdout, "the group went; the commands are for what is left")


class EndpointWaitCase(LiveBase):
    """W2: the wait for AgentCore to delete the `v<N>` endpoints of a runtime printed nothing (6 minutes 18 seconds of silence in a live teardown). It prints a line every few minutes
    like the wait for a stack."""

    LINE = r"still waiting for AgentCore to delete the versioned runtime endpoints \((\d+) seconds so far\)"

    def test_a_wait_for_an_endpoint_that_stays_deleting_prints_a_line_while_it_waits(self) -> None:
        self.stage(endpoint_deleting_looks=3)
        p = self.tear(HAMH_SETUP_POLL_SECONDS=str(Q), HAMH_SETUP_PROGRESS_SECONDS=str(Q))
        self.assertEqual(p.returncode, 0, self.out(p))
        lines = re.findall(self.LINE, p.stdout)
        self.assertGreaterEqual(len(lines), 1, p.stdout)
        stopping = p.stdout.split("== Stopping what is running")[1].split("== Deleting the stacks")[0]
        self.assertIn("Deleting endpoint v1 of runtime", stopping)
        self.assertRegex(stopping, self.LINE, "the line comes in the step that deletes the endpoints")
        self.assertNotIn("Terminated", p.stdout + p.stderr, "the shell does not report the ticker it stopped")

    def test_the_line_stops_with_the_wait(self) -> None:
        self.stage(endpoint_deleting_looks=2)
        p = self.tear(HAMH_SETUP_POLL_SECONDS=str(Q), HAMH_SETUP_PROGRESS_SECONDS=str(Q))
        self.assertEqual(p.returncode, 0, self.out(p))
        after = p.stdout.split("== Deleting the stacks")[1]
        self.assertNotIn("AgentCore to delete", after)

    def test_a_wait_shorter_than_the_interval_prints_no_line(self) -> None:
        self.stage(endpoint_deleting_looks=3)
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("AgentCore to delete", p.stdout, "the default is a line every three minutes")

    def test_a_wait_that_ends_at_once_leaves_no_ticker_behind(self) -> None:
        self.stage()
        p = self.tear(HAMH_SETUP_POLL_SECONDS=str(Q), HAMH_SETUP_PROGRESS_SECONDS=str(3 * Q))  # the first listing must come back within three intervals, however busy the host
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("AgentCore to delete", p.stdout)
        self.assertEqual(p.stderr, "")

    def test_an_endpoint_listing_that_fails_stops_the_line_too(self) -> None:
        self.stage(endpoint_deleting_looks=3, faults=[denied("list-agent-runtime-endpoints", after=2)])  # the plan reads the endpoints once, the step once, the third listing is the first look
        p = self.tear(HAMH_SETUP_POLL_SECONDS=str(Q), HAMH_SETUP_PROGRESS_SECONDS=str(Q))
        text = self.out(p)
        self.assertIn("could not check whether the v<N> endpoints of the runtime", text)
        after = text.split("could not check whether the v<N> endpoints of the runtime")[1]
        self.assertNotIn("AgentCore to delete", after)
        self.assertNotIn("Terminated", text)


class PlanSentenceCase(LiveBase):
    """W3: the plan promised "about 20 to 40 minutes" also when it held no stack to delete (a second run that only had a retained VPC took 59 seconds)."""

    SENTENCE = "Teardown takes about 20 to 40 minutes when AgentCore has no network interface in the stage's VPC, and about 50 minutes when AgentCore still holds its network interface."
    PROGRESS = ("Deleting the stacks takes about 20 to 40 minutes when AgentCore has no network interface in the stage's VPC, and about 50 minutes when AgentCore still holds its network interface. "
                "A line shows what the program is waiting for every 3 minutes.")
    RETRY = "This run only retries removing network resources retained by an earlier teardown; resources AgentCore still holds stay and appear as RETAINED."

    def test_a_plan_that_holds_only_a_retained_vpc_names_no_stack_deletion_time(self) -> None:
        self.stage(eni_stuck=True)
        self.assertEqual(self.tear().returncode, 8)
        p = self.run_full(*TEAR, "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = p.stdout.split("== Dry run")[0]
        self.assertRegex(plan, r"(?m)^  VPC vpc-0abc1234, left by an earlier teardown")
        self.assertNotRegex(plan, r"(?m)^  stack ", "the plan holds no stack")
        self.assertNotIn(self.SENTENCE, plan)
        self.assertNotIn("20 to 40 minutes", plan)
        self.assertNotIn("50 minutes", plan)
        self.assertIn("  " + self.RETRY, plan)

    def test_a_plan_with_a_stack_keeps_the_sentence_and_does_not_say_it_only_retries(self) -> None:
        self.stage(eni_stuck=True)
        p = self.run_full(*TEAR, "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = p.stdout.split("== Dry run")[0]
        self.assertIn("  " + self.SENTENCE + " AgentCore can keep that interface for up to 8 hours after its runtime is deleted. In that case the network stack is deleted while the resources the interface holds are retained.", plan)
        self.assertNotIn("only retries", plan)
        self.assertNotIn("AgentCore holds", plan, "the VPC safety test searches the output for this phrase as the sign of the held-interface notice")

    def test_the_line_before_the_stacks_delete_gives_both_times(self) -> None:
        self.stage()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        lines = p.stdout.splitlines()
        self.assertEqual(lines.count(self.PROGRESS), 1, "the line before the stacks are deleted states both teardown times once")
        self.assertNotIn("Deleting the stacks takes about 20 to 40 minutes. ", p.stdout)
        self.assertNotIn("AgentCore holds", p.stdout, "the VPC safety test searches the output for this phrase as the sign of the held-interface notice")

    def test_a_plan_with_a_stack_and_a_retained_vpc_keeps_the_sentence(self) -> None:
        self.stage()
        w = self.open_world()
        vpc_tags = {**PROJECT_TAGS, "aws:cloudformation:stack-name": "hamh-direct-network", "aws:cloudformation:stack-id": w.stack_id("hamh-direct-network", OLD_STACK_UUID)}
        w.w["ec2"]["vpcs"]["vpc-0old000000"] = {"subnets": ["subnet-0old000"], "sgs": ["sg-0old000"], "tags": vpc_tags, "enis": []}
        w.save()
        p = self.run_full(*TEAR, "--dry-run")
        plan = p.stdout.split("== Dry run")[0]
        self.assertRegex(plan, r"(?m)^  VPC vpc-0old000000, left by an earlier teardown")
        self.assertIn(self.SENTENCE, plan)
        self.assertNotIn("only retries", plan)


class DryRunLinesCase(LiveBase):
    """D12: a dry run closed with "would delete the log groups and the SSM parameters listed above" also when the plan listed none (a second run that only had a retained VPC),
    and with "would remove the network resources an earlier teardown retained" also when the plan listed no such VPC. Each line is printed only when the plan above it lists the things."""

    LOGS = "would delete the log groups and the SSM parameters listed above when no stack of the stage is left"
    VPC = "would remove the network resources an earlier teardown retained, when AWS has released the interfaces"

    def test_a_plan_that_holds_only_a_retained_vpc_does_not_promise_to_delete_log_groups_or_parameters(self) -> None:
        self.stage(eni_stuck=True)
        self.assertEqual(self.tear().returncode, 8)
        p = self.run_full(*TEAR, "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        plan, dry = p.stdout.split("== Dry run")
        self.assertRegex(plan, r"(?m)^  VPC vpc-0abc1234, left by an earlier teardown")
        self.assertNotIn("Log groups, deleted when", plan)
        self.assertNotIn("SSM parameters that carry the stage tags", plan)
        self.assertNotIn("would delete the log groups", dry)
        self.assertIn(self.VPC, dry)

    def test_a_plan_with_log_groups_and_parameters_says_it_would_delete_them(self) -> None:
        self.stage()
        p = self.run_full(*TEAR, "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        plan, dry = p.stdout.split("== Dry run")
        self.assertIn("Log groups, deleted when", plan)
        self.assertIn("SSM parameters that carry the stage tags", plan)
        self.assertIn(self.LOGS, dry)

    def test_a_plan_without_a_retained_vpc_does_not_say_it_would_remove_one(self) -> None:
        self.stage()
        p = self.run_full(*TEAR, "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        plan, dry = p.stdout.split("== Dry run")
        self.assertNotIn("left by an earlier teardown", plan)
        self.assertNotIn("would remove the network resources", dry)


class PendingDateCase(LiveBase):
    """W4: the `pending:` line showed the UTC timestamp of the AWS CLI with microseconds."""

    def test_a_pending_key_shows_the_date_only(self) -> None:
        self.stage()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertRegex(p.stdout, r"(?m)^pending: KMS key key-0001 has deletion scheduled for 2026-11-29\. AWS deletes a key after a waiting period of 7 to 30 days\.$")
        self.assertNotRegex(p.stdout, r"has deletion scheduled for \S*T\d\d:", "no time of day")
        self.assertNotIn(".865000", p.stdout)



class DateOnlyCase(HelperCase):
    """W4: td_date_only keeps the date of an ISO timestamp and leaves everything else as it is."""

    def test_the_date_of_a_timestamp_is_what_is_before_the_t(self) -> None:
        for given, want in (("2026-11-30T03:14:47.865000+00:00", "2026-11-30"), ("2026-11-30T03:14:47+00:00", "2026-11-30"), ("2026-11-30", "2026-11-30"),
                            ("None", "None"), ("", ""), ("1793000000.5", "1793000000.5")):
            with self.subTest(given=given):
                p = self.sh(f'td_date_only "{given}"', "lib", "teardown")
                self.assertEqual(p.returncode, 0, p.stderr)
                self.assertEqual(p.stdout, want)


class HeldInterfaceNoticeCase(LiveBase):
    """B1: CloudFormation retries a held subnet and security group for about 20 minutes before the network stack's delete fails, and the screen said nothing about why. A line before that
    delete says it when AgentCore holds an interface (type agentic_ai) in the stage VPC. The interfaces the network stack makes itself (NAT gateway, VPC endpoints) are requester-managed,
    so a requester-managed interface is no sign of AgentCore."""

    NOTICE = ("AgentCore holds 1 network interface(s) in vpc-0abc1234 and may keep them for up to 8 hours after runtime deletion. CloudFormation can wait about 20 minutes and then "
              "report the network stack as DELETE_FAILED; teardown then retains and lists the network resources those interfaces hold.")

    def interface_reads(self) -> list:
        return [i for i, c in enumerate(self.normalized_calls()) if c[:2] == ["ec2", "describe-network-interfaces"]]

    def test_the_line_comes_before_the_delete_of_the_network_stack(self) -> None:
        self.stage(eni_stuck=True)
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        lines = [ln for ln in p.stdout.splitlines() if ln.startswith("AgentCore holds 1 network interface(s) in vpc-0abc1234 and may keep them")]
        self.assertEqual(lines, [self.NOTICE])
        before = p.stdout.split(self.NOTICE)[0]
        self.assertNotIn("Deleting stack hamh-direct-network", before)
        self.assertIn("Deleting stack hamh-direct-network", p.stdout.split(self.NOTICE)[1])
        self.assertIn("Deleting stack hamh-direct-security", before, "the stacks before it are deleted already")
        calls = self.normalized_calls()
        delete_at = calls.index(["cloudformation", "delete-stack", "--stack-name", "hamh-direct-network"])
        self.assertEqual(len([i for i in self.interface_reads() if i < delete_at]), 1, "one read of the interfaces before the delete, and none before the other stacks")

    def test_no_line_when_the_interfaces_are_the_network_stacks_own(self) -> None:
        self.stage()
        w = self.open_world()
        w.w["ec2"]["vpcs"]["vpc-0abc1234"]["enis"] = [{"id": "eni-nat", "status": "in-use", "managed": True, "type": "nat_gateway"},
                                                      {"id": "eni-endpoint", "status": "in-use", "managed": True, "type": "vpc_endpoint"}]
        w.save()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("may keep them for up to 8 hours", p.stdout)
        calls = self.normalized_calls()
        delete_at = calls.index(["cloudformation", "delete-stack", "--stack-name", "hamh-direct-network"])
        self.assertEqual(len([i for i in self.interface_reads() if i < delete_at]), 1)

    def test_no_line_when_nothing_holds_the_vpc(self) -> None:
        self.stage()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("may keep them for up to 8 hours", p.stdout)

    def test_an_interface_read_that_fails_costs_only_the_line(self) -> None:
        self.stage(eni_stuck=True, faults=[denied("describe-network-interfaces", times=1)])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertNotIn("may keep them for up to 8 hours", p.stdout)
        self.assertRegex(p.stdout, r"(?m)^RETAINED vpc-0abc1234 ", "the hold is still found after the delete fails")
        self.assertNotIn("could not list the network interfaces", p.stdout, "the read before the delete is information only")

    def test_other_stacks_do_not_read_the_interfaces(self) -> None:
        self.stage()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        calls = self.normalized_calls()
        reads = self.interface_reads()
        self.assertEqual(len(reads), 1, "one read for the one stack that has a VPC")
        security_at = calls.index(["cloudformation", "delete-stack", "--stack-name", "hamh-direct-security"])
        self.assertGreater(reads[0], security_at)


@unittest.skipIf(jmespath is None, "the jmespath package is not installed")
class QueryConformanceCase(LiveBase):
    """The stand-in evaluates every --query with a small JMESPath of its own. The queries that the findings added (the events of a failed stack, the id of the stack that made a VPC,
    the VPC and key resources of a stack, the subnets and groups of a VPC) give the result the real JMESPath, the package the AWS CLI uses, gives on the same documents."""

    def test_every_new_query_gives_the_result_the_real_jmespath_gives(self) -> None:
        log = self.tmp / "queries.jsonl"
        self.stage(query_log=str(log), eni_stuck=True, runner_role_denied="always", secret_vanish_calls=150)
        self.seed_runner()
        self.seed_object("results/b1/result.json", b"{}")
        self.assertEqual(self.tear(**{"HAMH_SETUP_SECRET_LOOKS": "500"}).returncode, 8)
        w = self.open_world()
        vpc_tags = {**PROJECT_TAGS, "aws:cloudformation:stack-name": "hamh-direct-network", "aws:cloudformation:stack-id": w.stack_id("hamh-direct-network", OLD_STACK_UUID)}
        w.w["ec2"]["vpcs"]["vpc-0old000000"] = {"subnets": ["subnet-0old000"], "sgs": ["sg-0old000"], "tags": vpc_tags, "enis": [{"id": "eni-0old000", "status": "in-use", "managed": True}]}
        w.save()
        self.set_scenario(eni_stuck=False)
        self.tear()
        rows = [json.loads(line) for line in log.read_text().splitlines() if line.strip()]
        queries = {r["query"] for r in rows}
        for needle in ("StackEvents[].[ResourceStatus,LogicalResourceId,ResourceType,ResourceStatusReason]", "Vpcs[0].Tags[].[Key,Value]", "Stacks[0].StackId",
                       "StackResourceSummaries[?ResourceType=='AWS::EC2::VPC'].[LogicalResourceId,PhysicalResourceId,ResourceStatus]",
                       "StackResourceSummaries[?ResourceType=='AWS::KMS::Key'].PhysicalResourceId", "[Name,DeletedDate]", "Subnets[].SubnetId"):
            self.assertIn(needle, queries, f"the worlds of this case do not run {needle}")
        for r in rows:
            self.assertEqual(jmespath.search(r["query"], r["document"]), r["result"], r["query"])


if __name__ == "__main__":
    unittest.main()
