"""The full setup of `setup.sh`, driven without a terminal against the stand-in AWS CLI.

`setup.sh --full --yes --acknowledge-costs` is the whole story: it reads the account, checks the clean checkout, installs the runner with the AWS CLI
alone, uploads the source archive and the generated account file as two objects, starts the build, follows it, reads `result.json`, downloads the
client from the user's own bucket, checks its SHA-256, removes the runner and then does what the client-only setup does. Every case here pins one
step of that flow or one way it must stop, and what a refused or failed setup left behind in the account (nothing, or only what the message says).
"""

from __future__ import annotations

import hashlib
import json
import shutil
import stat
import subprocess  # nosec B404: tests start the stand-in AWS CLI with argument lists, never a shell
import unittest
import zipfile

import setup_fake_aws
from setup_full_support import HOST_PLATFORM, FullBase, golden_accounts, normalize
from setup_waits import limit
from test_setup_script import REPO, STUB_CARGO, tree_state



def golden_digest() -> str:
    return hashlib.sha256(golden_accounts('us-east-1')).hexdigest()


class FullHappyPathCase(FullBase):
    def test_every_step_runs_in_order_and_the_client_is_installed_and_configured(self) -> None:
        commit = self.commit()
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        digest = golden_digest()
        bid = sorted(self.world()["builds"])[0]
        b = self.bucket
        self.assert_order(
            ["sts", "get-caller-identity"],
            ["ec2", "describe-availability-zones"],
            ["cloudformation", "deploy", "--stack-name", "hamh-setup-runner"],
            ["cloudformation", "describe-stacks", "--stack-name", "hamh-setup-runner"],
            ["s3api", "put-object", "--bucket", b, "--key", f"source/{commit}.zip"],
            ["s3api", "put-object", "--bucket", b, "--key", f"config/accounts-{digest[:12]}.json"],
            ["codebuild", "start-build"],
            ["codebuild", "batch-get-builds"],
            ["s3", "cp", f"s3://{b}/results/{bid}/result.json"],
            ["s3", "cp", f"s3://{b}/client/{bid}/SHA256SUMS"],
            ["s3", "cp", f"s3://{b}/client/{bid}/{HOST_PLATFORM}/ham"],
            ["cloudformation", "delete-stack", "--stack-name", "hamh-setup-runner"],
        )
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1)

    def test_the_source_and_the_account_file_go_up_as_two_objects(self) -> None:
        commit = self.commit()
        self.assertEqual(self.run_full(*self.ARGS, "--keep-runner").returncode, 0)
        digest = golden_digest()
        archive = self.state / "s3" / self.bucket / "source" / f"{commit}.zip"
        with zipfile.ZipFile(archive) as z:
            self.assertEqual(z.comment.decode(), commit, "git archive records the commit in the zip comment")
            self.assertIn("setup.sh", z.namelist())
            self.assertIn("infra/setup-runner.yaml", z.namelist())
            self.assertNotIn("accounts.json", [n for n in z.namelist() if n.startswith("config/")])
        self.assertEqual(self.s3_bytes(f"config/accounts-{digest[:12]}.json"), golden_accounts('us-east-1'))

    def test_the_build_starts_with_the_environment_of_the_contract_and_nothing_else(self) -> None:
        commit = self.commit()
        self.assertEqual(self.run_full(*self.ARGS, "--keep-runner").returncode, 0)
        env = self.start_build_env()
        self.assertEqual(env, {"SETUP_SOURCE_KEY": f"source/{commit}.zip", "SETUP_CONFIG_KEY": f"config/accounts-{golden_digest()[:12]}.json",
                               "SETUP_STAGE": "direct", "SETUP_AZS": "1", "SETUP_BINARY_NAME": "ham",
                               "SETUP_SOURCE_SHA256": hashlib.sha256(self.s3_bytes(f"source/{commit}.zip")).hexdigest(), "SETUP_CONFIG_SHA256": golden_digest()})

    def test_the_project_name_and_the_bucket_come_from_the_runner_stack_outputs(self) -> None:
        self.assertEqual(self.run_full(*self.ARGS).returncode, 0)
        start = [c for c in self.normalized_calls() if c[:2] == ["codebuild", "start-build"]][0]
        self.assertEqual(start[start.index("--project-name") + 1], "hamh-setup-runner-build")

    def test_the_client_is_downloaded_checked_and_installed_then_configured_through_the_client(self) -> None:
        self.assertEqual(self.run_full(*self.ARGS).returncode, 0)
        dest = self.installed()
        self.assertTrue(dest.is_file())
        self.assertEqual(dest.read_bytes(), self.client_file.read_bytes())
        self.assertTrue(dest.stat().st_mode & stat.S_IXUSR)
        lines = self.log_lines()
        order = [lines.index("models"), lines.index(f"config set api_endpoint {self.ENDPOINT}"), lines.index("config set aws_profile work"),
                 lines.index("config set region us-east-1"), lines.index("rules init"), lines.index("doctor")]
        self.assertEqual(order, sorted(order), lines)
        self.assertIn(f'apiEndpoint = "{self.ENDPOINT}"', self.config_file().read_text())

    def test_the_summary_names_the_endpoint_the_phases_what_stays_and_the_first_command(self) -> None:
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        flat = " ".join(p.stdout.split())
        self.assertIn(self.ENDPOINT, p.stdout)
        for phase in ("toolchain", "account-file", "deploy", "gomod", "scanner-rules", "client", "outputs"):
            self.assertIn(phase, p.stdout)
        self.assertIn("Setup summary", p.stdout)
        self.assertIn("Retained", p.stdout)
        self.assertIn("CDK bootstrap", flat)
        self.assertIn("./setup.sh --teardown", p.stdout)
        self.assertIn("First command", p.stdout)
        self.assertRegex(p.stdout, r'ham "[^"]+" --s3-only --direct')

    def test_the_runner_is_removed_after_success_and_the_bootstrap_stays(self) -> None:
        self.assertEqual(self.run_full(*self.ARGS).returncode, 0)
        stacks = self.world()["stacks"]
        self.assertNotIn("hamh-setup-runner", stacks)
        self.assertIn("CDKToolkit", stacks)
        self.assertIn("hamh-direct-control", stacks)
        self.assertGreaterEqual(self.count_calls("s3api", "delete-objects"), 1, "the versioned bucket is emptied before its stack goes")

    def test_keep_runner_keeps_the_stack_and_says_so(self) -> None:
        p = self.run_full(*self.ARGS, "--keep-runner")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("hamh-setup-runner", self.world()["stacks"])
        self.assertEqual(self.count_calls("cloudformation", "delete-stack"), 0)
        self.assertIn("runner", p.stdout.lower())
        self.assertIn("kept", p.stdout.lower())

    def test_an_existing_bootstrap_is_used_unchanged(self) -> None:
        self.set_scenario(bootstrap_present=True)
        self.assertEqual(self.run_full(*self.ARGS).returncode, 0)
        self.assertEqual(self.count_calls("cloudformation", "deploy", "--stack-name", "CDKToolkit"), 0)
        self.assertGreaterEqual(self.count_calls("ssm", "get-parameter"), 1)

    def test_every_aws_call_carries_the_profile_and_the_region_and_never_a_pager(self) -> None:
        self.assertEqual(self.run_full(*self.ARGS).returncode, 0)
        raw = [json.loads(line) for line in (self.state / "calls.jsonl").read_text().splitlines() if line.strip()]
        for call in raw:
            c = call["args"]
            if c[:1] == ["--version"]:
                continue
            # the calls of scripts/setup/empty-bucket.sh too: setup.sh starts it with the profile as --profile
            self.assertIn("--profile", c, c)
            self.assertEqual(c[c.index("--profile") + 1], "work")
            self.assertEqual(c[c.index("--region") + 1], "us-east-1")

    def test_a_log_of_the_run_is_kept_beside_the_receipt(self) -> None:
        self.assertEqual(self.run_full(*self.ARGS).returncode, 0)
        logs = sorted((self.receipt_path().parent / "logs").glob("setup-*.log"))
        self.assertEqual(len(logs), 1)
        text = logs[0].read_text()
        self.assertIn("SETUP-PASS", text)
        self.assertIn(self.ENDPOINT, text)

    def test_the_receipt_records_the_run_and_no_credential(self) -> None:
        commit = self.commit()
        self.assertEqual(self.run_full(*self.ARGS).returncode, 0)
        r = self.receipt()
        self.assertEqual((r["account"], r["region"], r["stage"], r["profile"], r["sourceCommit"]), (self.ACCOUNT, "us-east-1", "direct", "work", commit))
        self.assertEqual(r["configDigest"], golden_digest())
        self.assertEqual(r["state"], "installed")
        self.assertTrue(r["buildId"].startswith("hamh-setup-runner-build:"))

    def test_west_works_the_same_with_its_own_zone_ids(self) -> None:
        self.set_scenario(region="us-west-2")
        p = self.run_full("--full", "--profile", "work", "--region", "us-west-2", "--yes", "--acknowledge-costs", "--keep-runner")
        self.assertEqual(p.returncode, 0, self.out(p))
        want = golden_accounts("us-west-2")
        digest = hashlib.sha256(want).hexdigest()
        self.assertEqual(self.s3_bytes(f"config/accounts-{digest[:12]}.json", bucket=f"hamh-setup-{self.ACCOUNT}-us-west-2"), want)

    def test_the_profile_is_optional_the_aws_cli_then_uses_its_own_chain(self) -> None:
        (self.home / ".aws" / "config").write_text("[default]\nregion = us-east-1\ncredential_process = /usr/bin/env helper print\n")
        p = self.run_full("--full", "--yes", "--acknowledge-costs")
        self.assertEqual(p.returncode, 0, self.out(p))
        for c in self.calls():
            if c[:1] != ["--version"]:
                self.assertNotIn("--profile", c)
        self.assertNotIn("config set aws_profile", "\n".join(self.log_lines()))


class FullFollowCase(FullBase):
    """What the screen shows while the build runs."""

    def test_a_progress_line_comes_for_each_finished_phase_in_order(self) -> None:
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        lines = [ln for ln in p.stdout.splitlines() if ln.strip().startswith("phase ")]
        names = [ln.split(":", 1)[1].split()[0] for ln in lines]
        self.assertEqual(names, ["inputs", "fetch", "toolchain", "account-file", "deploy", "gomod", "scanner-rules", "client", "outputs"],
                         "the phases of scripts/setup/runner-build.sh, as it writes them")

    def test_the_marker_lines_of_the_runner_show_and_the_rest_of_its_log_shows_only_with_verbose(self) -> None:
        quiet = self.run_full(*self.ARGS)
        self.assertIn("SETUP-PASS stage=direct", quiet.stdout)
        self.assertNotIn("== phase toolchain ok", quiet.stdout)
        logs = sorted((self.receipt_path().parent / "logs").glob("setup-*.log"))
        self.assertIn("== phase toolchain ok", logs[-1].read_text(), "the whole build log is in the log of the run")
        self.state_reset()
        loud = self.run_full(*self.ARGS, "--verbose")
        self.assertIn("== phase toolchain ok", loud.stdout)

    def state_reset(self) -> None:
        import shutil
        shutil.rmtree(self.state)
        shutil.rmtree(self.home / ".config" / "ham" / "setup")

    def test_the_log_stream_named_by_codebuild_is_the_one_that_is_read(self) -> None:
        self.assertEqual(self.run_full(*self.ARGS).returncode, 0)
        bid = sorted(self.world()["builds"])[0]
        reads = [c for c in self.normalized_calls() if c[:2] == ["logs", "get-log-events"]]
        self.assertTrue(reads)
        self.assertEqual(reads[0][reads[0].index("--log-group-name") + 1], "/aws/codebuild/hamh-setup-runner")
        self.assertEqual(reads[0][reads[0].index("--log-stream-name") + 1], bid.split(":", 1)[1])

    def test_a_template_too_big_for_the_aws_cli_without_a_staging_bucket_is_refused_before_anything_is_created(self) -> None:
        (self.repo / "infra" / "setup-runner.yaml").write_text("# " + "x" * 52000 + "\n")
        self.git("commit", "-q", "-am", "a template of 52,000 bytes")
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertIn("51,200", p.stderr)
        self.assertEqual(self.mutations(), [])


class FullRefusalCase(FullBase):
    """What setup refuses, and that a refusal changes nothing in the account."""

    def refused(self, p, code: int) -> None:
        self.assertEqual(p.returncode, code, self.out(p))
        self.assertEqual(self.mutations(), [], "a refused setup must not change the account")
        self.assertFalse(self.installed().exists())

    def test_yes_without_acknowledge_costs_is_exit_4_and_names_the_flag(self) -> None:
        p = self.run_full("--full", "--profile", "work", "--region", "us-east-1", "--yes")
        self.refused(p, 4)
        self.assertIn("--acknowledge-costs", p.stderr)

    def test_no_terminal_and_no_yes_is_exit_4_not_a_hang(self) -> None:
        p = self.run_full("--full", "--profile", "work", "--region", "us-east-1")
        self.refused(p, 4)

    def test_no_input_without_yes_is_exit_4(self) -> None:
        self.refused(self.run_full("--full", "--profile", "work", "--region", "us-east-1", "--no-input"), 4)

    def test_yes_alone_never_selects_the_full_setup(self) -> None:
        art = self.make_artifact()
        p = self.run_full("--yes", "--profile", "work", "--region", "us-east-1", "--from", str(art))
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.calls(), [], "a client-only run makes no AWS call")
        p = self.run_full("--yes", "--profile", "work", "--region", "us-east-1", "--from", str(art), "--endpoint", self.ENDPOINT)
        self.assertEqual(self.calls(), [])

    def test_yes_with_no_mode_at_all_is_the_client_only_setup_of_before(self) -> None:
        art = self.make_artifact()
        p = self.run_full("--yes", "--from", str(art))
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.calls(), [])
        self.assertTrue(self.installed().is_file())

    def test_an_account_that_is_not_the_one_named_is_exit_7(self) -> None:
        p = self.run_full(*self.ARGS, "--account-id", "999988887777")
        self.refused(p, 7)
        self.assertIn(self.ACCOUNT, p.stderr)
        self.assertIn("999988887777", p.stderr)

    def test_an_account_id_that_is_not_twelve_digits_is_a_usage_error(self) -> None:
        for bad in ("12345", "<account id>", "21098765432x", ""):
            with self.subTest(bad=bad):
                self.assertEqual(self.run_full(*self.ARGS, "--account-id", bad).returncode, 2)

    def test_a_placeholder_account_from_the_identity_call_is_refused(self) -> None:
        self.set_scenario(account="111122223333")
        p = self.run_full(*self.ARGS)
        self.refused(p, 7)
        self.assertIn("placeholder", p.stderr.lower())

    def test_a_dirty_tracked_file_is_refused_and_named(self) -> None:
        (self.repo / "README.md").write_text("edited after the commit\n")
        p = self.run_full(*self.ARGS)
        self.refused(p, 7)
        self.assertIn("README.md", p.stderr)
        self.assertIn("commit", p.stderr)

    def test_a_staged_but_uncommitted_file_is_refused(self) -> None:
        (self.repo / "new.txt").write_text("staged\n")
        self.git("add", "new.txt")
        self.refused(self.run_full(*self.ARGS), 7)

    def test_an_untracked_file_is_not_part_of_the_commit_and_does_not_block(self) -> None:
        (self.repo / "scratch.txt").write_text("not tracked\n")
        p = self.run_full(*self.ARGS, "--keep-runner")
        self.assertEqual(p.returncode, 0, self.out(p))
        with zipfile.ZipFile(self.state / "s3" / self.bucket / "source" / f"{self.commit()}.zip") as z:
            self.assertNotIn("scratch.txt", z.namelist())

    def test_a_folder_that_is_not_a_git_clone_is_refused_with_the_way_out(self) -> None:
        import shutil
        shutil.rmtree(self.repo / ".git")
        p = self.run_full(*self.ARGS)
        self.refused(p, 1)
        self.assertIn("git clone", p.stderr)
        self.assertIn("--client-only", p.stderr)

    def test_a_region_the_stage_does_not_support_is_a_usage_error_before_any_aws_call(self) -> None:
        p = self.run_full("--full", "--profile", "work", "--region", "eu-west-1", "--yes", "--acknowledge-costs")
        self.refused(p, 2)
        self.assertEqual(self.calls(), [])
        self.assertIn("us-east-1", p.stderr)
        self.assertIn("us-west-2", p.stderr)

    def test_no_region_anywhere_is_a_usage_error_naming_the_flag(self) -> None:
        (self.home / ".aws" / "config").write_text("[profile work]\ncredential_process = x\n")
        p = self.run_full("--full", "--profile", "work", "--yes", "--acknowledge-costs")
        self.refused(p, 2)
        self.assertIn("--region", p.stderr)

    def test_other_options_that_cannot_go_with_the_full_setup_are_usage_errors(self) -> None:
        art = self.make_artifact()
        for args in (("--from", str(art)), ("--build",), ("--endpoint", self.ENDPOINT), ("--client-only",), ("--stage", "other"), ("--azs", "2"),
                     ("--build-on", "moon"), ("--invocation-logging", "loud"), ("--revision", "main")):
            with self.subTest(args=args):
                p = self.run_full(*self.ARGS, *args)
                self.assertEqual(p.returncode, 2, self.out(p))
                self.assertEqual(self.mutations(), [])

    def test_a_logging_mode_the_runner_cannot_apply_is_refused_before_anything_is_made(self) -> None:
        """scripts/setup/runner-build.sh stops in its account-file phase on invocationLogging enable-and-restore, because the runner role cannot write that Bedrock
        setting. The wizard says so before it installs anything, instead of after an hour of builds."""
        p = self.run_full(*self.ARGS, "--invocation-logging", "enable-and-restore")
        self.assertEqual(p.returncode, 2, self.out(p))
        self.assertEqual(self.mutations(), [])
        flat = " ".join(p.stderr.split())
        self.assertIn("--build-on local", flat)
        self.assertIn("enable-and-restore", flat)

    def test_full_only_options_without_the_full_setup_are_usage_errors(self) -> None:
        art = self.make_artifact()
        for flag in ("--keep-runner", "--acknowledge-costs", "--preflight-only"):
            with self.subTest(flag=flag):
                p = self.run_full("--client-only", "--yes", "--from", str(art), flag)
                self.assertEqual(p.returncode, 2, self.out(p))

    def test_credentials_that_do_not_work_say_how_to_sign_in_and_change_nothing(self) -> None:
        self.set_scenario(identity_error="Unable to locate credentials. You can configure credentials by running \"aws configure\".")
        p = self.run_full(*self.ARGS)
        self.refused(p, 1)
        self.assertIn("Unable to locate credentials", p.stderr)
        self.assertIn("aws sso login", p.stderr)

    def test_the_caller_becomes_the_operator_of_the_stage_for_each_identity_form(self) -> None:
        # An IAM user, an IAM Identity Center role and another role name each become the `operator` of the uploaded account file; the stage trusts that identity.
        forms = (("an IAM user", f"arn:aws:iam::{self.ACCOUNT}:user/team/alice", ("user", "alice")),
                 ("an IAM Identity Center role", f"arn:aws:sts::{self.ACCOUNT}:assumed-role/AWSReservedSSO_AdministratorAccess_0123456789abcdef/alice@example.com",
                  ("role", "AWSReservedSSO_AdministratorAccess_0123456789abcdef")),
                 ("another role", f"arn:aws:sts::{self.ACCOUNT}:assumed-role/StageOperator/session-1", ("role", "StageOperator")))
        for label, arn, (kind, name) in forms:
            with self.subTest(label):
                self.set_scenario(arn=arn)
                p = self.run_full("--full", "--profile", "work", "--region", "us-east-1", "--dry-run")
                self.assertEqual(p.returncode, 0, self.out(p))
                self.assertIn(f"operator:      {kind} {name}:", p.stdout)
                self.assertIn("the identity of your credentials", p.stdout)
                self.assertEqual(self.mutations(), [])

    def test_the_uploaded_account_file_names_the_caller_as_its_operator(self) -> None:
        arn = f"arn:aws:sts::{self.ACCOUNT}:assumed-role/StageOperator/session-1"
        self.set_scenario(arn=arn)
        self.assertEqual(self.run_full(*self.ARGS, "--keep-runner").returncode, 0)
        want = golden_accounts("us-east-1", operator=("role", "StageOperator"))
        key = f"config/accounts-{hashlib.sha256(want).hexdigest()[:12]}.json"
        self.assertEqual(self.s3_bytes(key), want)
        self.assertEqual(json.loads(want)["accounts"][0]["operator"], {"kind": "role", "name": "StageOperator"})

    def test_credentials_of_the_root_user_or_a_federated_user_are_refused_before_anything_is_created(self) -> None:
        for who in (f"arn:aws:iam::{self.ACCOUNT}:root", f"arn:aws:sts::{self.ACCOUNT}:federated-user/bob"):
            with self.subTest(who=who):
                self.set_scenario(arn=who)
                p = self.run_full(*self.ARGS)
                self.refused(p, 1)
                self.assertIn("IAM user", p.stderr)
                self.assertIn("aws sso login", p.stderr)
                self.assertEqual(self.mutations(), [])

    def test_an_aws_cli_older_than_version_two_is_refused_before_any_call(self) -> None:
        p = self.run_full(*self.ARGS, FAKE_AWS_VERSION="aws-cli/1.18.147 Python/3.8.10 Linux/5.4.0 botocore/1.17.70")
        self.refused(p, 1)
        self.assertEqual([c for c in self.calls() if c[:1] != ["--version"]], [])
        self.assertIn("version 2", p.stderr)

    def test_no_aws_cli_at_all_is_refused_with_the_install_page(self) -> None:
        (self.stub / "aws").unlink()
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("AWS CLI", p.stderr)
        self.assertIn("docs.aws.amazon.com", p.stderr)

    def test_an_account_with_no_zone_the_stage_can_use_stops_before_anything_is_created(self) -> None:
        self.set_scenario(azs=[["us-east-1a", "use1-az6"], ["us-east-1e", "use1-az3"]])
        p = self.run_full(*self.ARGS)
        self.refused(p, 1)
        self.assertIn("Availability Zone", p.stderr)
        self.assertIn("use1-az1", p.stderr)

    def test_a_machine_the_runner_builds_no_client_for_is_refused_before_anything_is_created(self) -> None:
        shim = self.tmp / "uname-shim"
        shim.mkdir()
        (shim / "uname").write_text('#!/bin/sh\ncase "$1" in -s) echo Darwin;; -m) echo x86_64;; *) echo Darwin;; esac\n')
        (shim / "uname").chmod(0o755)
        p = self.run_full(*self.ARGS, PATH=f"{shim}:{self.stub}:{self.sysbin}")
        self.refused(p, 1)
        self.assertIn("--from", p.stderr)
        self.assertIn("--client-only", p.stderr)


class FullPreviewCase(FullBase):
    def test_preflight_only_reads_and_reports_and_changes_nothing_anywhere(self) -> None:
        commit = self.commit()
        before = tree_state(self.home)
        p = self.run_full("--full", "--profile", "work", "--region", "us-east-1", "--preflight-only")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.mutations(), [])
        self.assertEqual(before, tree_state(self.home), "no state is written before the confirmation")
        for needle in (self.ACCOUNT, "us-east-1", "us-east-1b", "use1-az1", commit):
            self.assertIn(needle, p.stdout)

    def test_dry_run_prints_the_plan_and_the_costs_and_changes_nothing(self) -> None:
        before = tree_state(self.home)
        p = self.run_full("--full", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.mutations(), [])
        self.assertEqual(before, tree_state(self.home))
        self.assertIn("DRY RUN", p.stdout)
        self.assertRegex(p.stdout, r"(?m)^would .*runner")
        self.assertIn("NAT gateway", p.stdout)
        self.assertFalse(self.installed().exists())

    def test_the_plan_names_the_account_zone_commit_permissions_and_every_cost_item(self) -> None:
        commit = self.commit()
        p = self.run_full("--full", "--profile", "work", "--region", "us-east-1", "--dry-run")
        text = p.stdout
        flat = " ".join(text.split())
        for needle in (self.ACCOUNT, "us-east-1", "use1-az1", commit[:12]):
            self.assertIn(needle, text)
        for item in ("NAT gateway", "Network Firewall", "Interface endpoints", "AgentCore sessions", "Bedrock calls", "CodeBuild", "Storage", "Logs"):
            self.assertIn(item, text, item)
        for needle in ("sandbox", "IAM", "CDK bootstrap", "--keep-runner", "./setup.sh --teardown", "8 hours"):
            self.assertIn(needle, flat, needle)
        self.assertNotRegex(text, r"\$\s?\d", "no dollar figure is invented for the cost items")


    def test_the_plan_says_what_authority_the_cdk_roles_hold(self) -> None:
        """The CloudFormation execution role of the CDK bootstrap holds AdministratorAccess, and the runner assumes the CDK roles, so the plan does not claim that a
        permissions boundary limits them. The boundary of infra/setup-runner.yaml is on the runner role itself."""
        p = self.run_full("--full", "--profile", "work", "--region", "us-east-1", "--dry-run")
        flat = " ".join(p.stdout.split())
        self.assertIn("AdministratorAccess", flat)
        self.assertIn("administrator", flat)
        self.assertNotIn("boundary limits the deployment and execution roles", flat)
        self.assertIn("boundary limits the runner role itself", flat)


class FullRunNamesPlanCase(FullBase):
    """O6 of the live rehearsal of the public setup: the CDK printed `INFO HAMH_UTILITY_MODEL is unset ... run names are off here` for every stack, and the quickstart
    said nothing. The plan says it once, before the question, in plain words: whether this Region gets run names, what that means and what changes it."""

    def with_catalog(self) -> None:
        dest = self.repo / "crates" / "ham-core" / "assets"
        dest.mkdir(parents=True)
        shutil.copy2(REPO / "crates" / "ham-core" / "assets" / "utility-models.json", dest / "utility-models.json")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "the utility model catalog")

    def plan(self, region: str) -> str:
        self.set_scenario(region=region)
        p = self.run_full("--full", "--profile", "work", "--region", region, "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        return p.stdout

    def lines(self, text: str) -> list:
        return [ln for ln in text.splitlines() if "run names:" in ln]

    def test_a_region_with_the_model_says_run_names_are_on_and_the_setting_that_turns_them_off(self) -> None:
        self.with_catalog()
        lines = self.lines(self.plan("us-east-1"))
        self.assertEqual(len(lines), 1, lines)
        for fact in ("on:", "Claude Haiku 5.5", "--build-on local", "HAMH_UTILITY_MODEL=off"):
            self.assertIn(fact, lines[0])

    def test_a_region_without_the_model_says_run_names_are_off_and_how_to_get_them(self) -> None:
        self.with_catalog()
        lines = self.lines(self.plan("us-west-2"))
        self.assertEqual(len(lines), 1, lines)
        for fact in ("off in us-west-2", "Claude Haiku 5.5", "only in us-east-1", "--region us-east-1"):
            self.assertIn(fact, lines[0])

    def test_the_line_is_in_the_plan_before_the_confirmation_and_stands_once(self) -> None:
        self.with_catalog()
        text = self.plan("us-west-2")
        self.assertEqual(len(self.lines(text)), 1, text)
        self.assertLess(text.index("run names:"), text.index("== Dry run"))
        self.assertLess(text.index("What setup will do"), text.index("run names:"))

    def test_a_clone_without_the_catalog_prints_no_line_and_no_error(self) -> None:
        text = self.plan("us-west-2")
        self.assertEqual(self.lines(text), [])
        self.assertNotIn("json_flat", text)


class FullBootstrapCase(FullBase):
    """A fresh account has no CDK bootstrap stack. The runner role cannot make one and the desktop has no Node, so: the runner installs, a first build of the runner
    (SETUP_ACTION=bootstrap-template) prints the template with the pinned CDK CLI and puts it in the bucket, setup.sh downloads it, checks its SHA-256 against
    engine/policy/cdk-bootstrap-template.pin of the clone, and makes the stack with the user's credentials; the deploy build follows. The template build takes no lease."""

    def setUp(self) -> None:
        super().setUp()
        self.set_scenario(bootstrap_present=False)

    def bootstrap_call(self) -> list:
        calls = [c for c in self.normalized_calls() if c[:3] == ["cloudformation", "deploy", "--stack-name"] and c[3] == "CDKToolkit"]
        self.assertEqual(len(calls), 1, calls)
        return calls[0]

    def start_envs(self) -> list:
        return [self.start_build_env(n) for n in range(self.count_calls("codebuild", "start-build"))]

    def test_the_stack_is_made_between_the_template_build_and_the_deploy_build(self) -> None:
        commit = self.commit()
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        b = self.bucket
        tbid, dbid = sorted(self.world()["builds"])
        key = f"bootstrap/cdk-bootstrap-{setup_fake_aws.BOOTSTRAP_SHA[:12]}.yaml"
        self.assert_order(
            ["sts", "get-caller-identity"],
            ["cloudformation", "deploy", "--stack-name", "hamh-setup-runner"],
            ["s3api", "put-object", "--bucket", b, "--key", f"source/{commit}.zip"],
            ["codebuild", "start-build"],
            ["s3", "cp", f"s3://{b}/results/{tbid}/result.json"],
            ["s3", "cp", f"s3://{b}/{key}"],
            ["cloudformation", "deploy", "--stack-name", "CDKToolkit"],
            ["s3api", "put-object", "--bucket", b, "--key", "lease/direct.json"],
            ["codebuild", "start-build"],
            ["s3", "cp", f"s3://{b}/results/{dbid}/result.json"],
        )
        envs = self.start_envs()
        self.assertEqual([e.get("SETUP_ACTION") for e in envs], ["bootstrap-template", None])
        self.assertEqual(envs[0]["SETUP_SOURCE_KEY"], envs[1]["SETUP_SOURCE_KEY"], "both builds read the one archive")

    def test_the_stack_is_made_from_the_downloaded_template_with_the_execution_policy(self) -> None:
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        call = self.bootstrap_call()
        self.assertEqual(call[call.index("--parameter-overrides") + 1], "CloudFormationExecutionPolicies=arn:aws:iam::aws:policy/AdministratorAccess")
        self.assertEqual(self.world()["bootstrap_template_sha"], setup_fake_aws.BOOTSTRAP_SHA, "the stack was made from the pinned template")
        self.assertFalse(call[call.index("--template-file") + 1].endswith("infra/cdk-bootstrap.yaml"), "no template is committed")

    def test_the_template_build_takes_no_lease(self) -> None:
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        puts = [c for c in self.normalized_calls() if c[:2] == ["s3api", "put-object"] and "lease/direct.json" in c]
        self.assertEqual(len(puts), 2, "one to take the lease and one to record the deploy build; the template build is neither")

    def test_a_template_that_is_not_the_pinned_one_is_refused_and_nothing_is_made_from_it(self) -> None:
        self.set_scenario(bootstrap_template="other")
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 7, self.out(p))
        flat = " ".join(p.stderr.split())
        self.assertIn("cdk-bootstrap-template.pin", flat)
        self.assertEqual(self.count_calls("cloudformation", "deploy", "--stack-name", "CDKToolkit"), 0)
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1, "no deploy build after a template that cannot be trusted")

    def test_a_template_changed_in_the_bucket_after_the_build_is_refused(self) -> None:
        self.set_scenario(bootstrap_template="tampered")
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(self.count_calls("cloudformation", "deploy", "--stack-name", "CDKToolkit"), 0)

    def test_a_template_of_more_than_the_deploy_limit_is_refused(self) -> None:
        self.set_scenario(bootstrap_template="too_big")
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertIn("51,200", " ".join(p.stderr.split()))
        self.assertEqual(self.count_calls("cloudformation", "deploy", "--stack-name", "CDKToolkit"), 0)

    def test_a_template_build_that_fails_is_exit_5_with_its_phase_and_nothing_follows(self) -> None:
        self.set_scenario(template_build={"final": "FAILED", "result": "fail", "failed_phase": "template", "detail": "the CDK could not print the bootstrap template: no network"})
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 5, self.out(p))
        text = p.stderr
        self.assertIn("template", text)
        self.assertIn("no network", text)
        self.assertEqual(self.count_calls("cloudformation", "deploy", "--stack-name", "CDKToolkit"), 0)
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1)

    def test_the_bootstrap_stack_failing_is_exit_5_and_no_deploy_build_starts(self) -> None:
        self.set_scenario(faults=[{"match": "deploy --stack-name CDKToolkit", "exit": 254, "stderr": "An error occurred (AccessDenied) when calling the CreateStack operation: not allowed to create roles"}])
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertIn("not allowed to create roles", p.stderr)
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1)

    def test_a_clone_without_the_pin_is_exit_5_before_anything_is_made(self) -> None:
        self.git("rm", "-q", "engine/policy/cdk-bootstrap-template.pin")
        self.git("commit", "-q", "-m", "drop the pin")
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertIn("engine/policy/cdk-bootstrap-template.pin", p.stderr)
        self.assertEqual(self.mutations(), [])

    def test_an_existing_bootstrap_is_left_alone_and_needs_no_template_build(self) -> None:
        self.set_scenario(bootstrap_present=True)
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual([c for c in self.normalized_calls() if c[:2] == ["cloudformation", "deploy"] and "CDKToolkit" in c], [])
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1)

    def test_a_dry_run_says_how_the_bootstrap_would_be_made(self) -> None:
        p = self.run_full("--full", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        flat = " ".join(p.stdout.split())
        self.assertIn("print the CDK bootstrap template", flat)
        self.assertIn("cdk-bootstrap-template.pin", flat)
        self.assertEqual(self.mutations(), [])


class FullBuildFailureCase(FullBase):
    def test_a_failed_build_is_exit_5_with_the_failed_phase_and_the_detail(self) -> None:
        self.set_scenario(build={"final": "FAILED", "result": "fail", "failed_phase": "deploy", "detail": "DEPLOY-FAIL deploy cdk deploy hamh-direct-network failed"})
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertIn("deploy", p.stderr)
        self.assertIn("cdk deploy hamh-direct-network failed", p.stderr)
        self.assertFalse(self.installed().exists())
        self.assertNotIn("doctor", self.log_lines())

    def test_a_failed_build_keeps_the_runner_for_a_retry_and_says_how_to_look(self) -> None:
        self.set_scenario(build={"final": "FAILED", "result": "fail"})
        p = self.run_full(*self.ARGS)
        self.assertIn("hamh-setup-runner", self.world()["stacks"])
        self.assertEqual(self.count_calls("cloudformation", "delete-stack"), 0)
        self.assertIn("/aws/codebuild/hamh-setup-runner", self.out(p))
        self.assertIn("setup.sh", p.stderr)
        self.assertEqual(self.receipt()["state"], "failed")

    def test_a_failed_build_releases_the_lease_so_the_retry_starts_a_new_build(self) -> None:
        self.set_scenario(build={"final": "FAILED", "result": "fail"})
        self.assertEqual(self.run_full(*self.ARGS).returncode, 5)
        self.assertFalse(self.s3_exists("lease/direct.json"))
        self.set_scenario(build={})
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 2)

    def test_a_build_that_stopped_with_no_result_is_exit_5_and_shows_the_end_of_its_log(self) -> None:
        self.set_scenario(build={"final": "FAILED", "result": "none"})
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertIn("no result", p.stderr.lower())

    def test_a_pass_marked_build_whose_result_says_fail_is_exit_5(self) -> None:
        self.set_scenario(build={"final": "SUCCEEDED", "result": "fail", "failed_phase": "gomod", "detail": "module store fill failed"})
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertIn("gomod", p.stderr)
        self.assertFalse(self.installed().exists())

    def test_the_runner_stack_failing_is_exit_5_with_the_aws_text_and_the_permission_hint(self) -> None:
        self.set_scenario(faults=[{"match": "deploy --stack-name hamh-setup-runner", "exit": 254, "stderr": "An error occurred (AccessDenied) when calling the CreateStack operation: iam:CreateRole"}])
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertIn("iam:CreateRole", p.stderr)
        self.assertIn("IAM", p.stderr)
        self.assertEqual(self.count_calls("codebuild", "start-build"), 0)

    def test_a_missing_runner_template_is_exit_5_and_names_the_file(self) -> None:
        self.git("rm", "-q", "infra/setup-runner.yaml")
        self.git("commit", "-q", "-m", "drop the runner template")
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertIn("infra/setup-runner.yaml", p.stderr)
        self.assertEqual(self.mutations(), [])


class FullClientVerifyCase(FullBase):
    def test_a_client_whose_hash_differs_from_the_result_is_never_installed(self) -> None:
        self.set_scenario(build={"client": "tamper"})
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertFalse(self.installed().exists())
        self.assertNotIn("models", self.log_lines())
        self.assertIn("SHA-256", p.stderr)
        want = hashlib.sha256(self.client_file.read_bytes()).hexdigest()
        self.assertIn(want, p.stderr)

    def test_a_manifest_that_disagrees_with_the_result_is_never_installed(self) -> None:
        self.set_scenario(build={"client": "sums-mismatch"})
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertFalse(self.installed().exists())
        self.assertIn("SHA256SUMS", p.stderr)

    def test_an_installed_client_is_left_alone_when_the_new_one_is_refused(self) -> None:
        self.installed().parent.mkdir(parents=True)
        self.installed().write_text("#!/bin/sh\necho previous\n")
        self.installed().chmod(0o755)
        self.set_scenario(build={"client": "tamper"})
        self.assertEqual(self.run_full(*self.ARGS).returncode, 5)
        self.assertEqual(self.installed().read_text(), "#!/bin/sh\necho previous\n")

    def test_a_client_missing_from_the_bucket_is_exit_5_and_names_the_key(self) -> None:
        self.set_scenario(build={"client": "missing"})
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertIn("client/", p.stderr)
        self.assertFalse(self.installed().exists())

    def test_a_result_that_names_another_commit_is_a_source_mismatch_exit_7(self) -> None:
        self.set_scenario(build={"result_extra": {"sourceCommit": "0" * 40}})
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertIn("sourceCommit", p.stderr)
        self.assertFalse(self.installed().exists())

    def test_a_result_that_names_another_account_file_is_a_configuration_mismatch_exit_7(self) -> None:
        self.set_scenario(build={"result_extra": {"configDigest": "f" * 64}})
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertIn("configDigest", p.stderr)

    def test_an_endpoint_that_is_not_a_url_is_refused(self) -> None:
        self.set_scenario(build={"result_extra": {"endpoint": "<api-id>.example"}})
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertNotIn("doctor", self.log_lines())


class FullOperatorToolCase(FullBase):
    """`ham-admin`, the operator tool, is built beside the client, listed in the result with its SHA-256, downloaded, checked like the client and installed in the
    same folder. Either file that fails its check stops the install of both."""

    def test_the_operator_tool_is_downloaded_checked_and_installed_beside_the_client(self) -> None:
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        bid = sorted(self.world()["builds"])[0]
        self.assert_order(
            ["s3", "cp", f"s3://{self.bucket}/client/{bid}/SHA256SUMS"],
            ["s3", "cp", f"s3://{self.bucket}/client/{bid}/{HOST_PLATFORM}/ham"],
            ["s3", "cp", f"s3://{self.bucket}/client/{bid}/{HOST_PLATFORM}/ham-admin"],
        )
        admin = self.installed_admin()
        self.assertTrue(admin.is_file())
        self.assertEqual(admin.parent, self.installed().parent, "the same folder as the client")
        self.assertEqual(admin.read_bytes(), self.admin_file.read_bytes())
        self.assertTrue(admin.stat().st_mode & stat.S_IXUSR)
        self.assertNotEqual(admin.read_bytes(), self.installed().read_bytes())

    def test_the_summary_and_the_screen_name_the_operator_tool_and_what_it_is_for(self) -> None:
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        flat = " ".join(p.stdout.split())
        self.assertIn(str(self.installed_admin()), flat)
        self.assertIn(hashlib.sha256(self.admin_file.read_bytes()).hexdigest(), flat, "the checksum of the file that was installed")
        self.assertRegex(flat, r"ham-admin repos allow")

    def test_an_operator_tool_whose_hash_differs_from_the_result_is_never_installed_and_neither_is_the_client(self) -> None:
        self.set_scenario(build={"admin": "tamper"})
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertFalse(self.installed().exists())
        self.assertFalse(self.installed_admin().exists())
        self.assertNotIn("models", self.log_lines())
        self.assertIn("ham-admin", p.stderr)
        self.assertIn("SHA-256", p.stderr)
        self.assertIn(hashlib.sha256(self.admin_file.read_bytes()).hexdigest(), p.stderr)

    def test_a_manifest_that_disagrees_with_the_result_about_the_operator_tool_installs_nothing(self) -> None:
        self.set_scenario(build={"admin": "sums-mismatch"})
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertFalse(self.installed().exists())
        self.assertFalse(self.installed_admin().exists())
        self.assertIn("SHA256SUMS", p.stderr)

    def test_an_operator_tool_missing_from_the_bucket_is_exit_5_and_names_the_key(self) -> None:
        self.set_scenario(build={"admin": "missing"})
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertRegex(p.stderr, r"client/[^ ]*/ham-admin")
        self.assertFalse(self.installed().exists())
        self.assertFalse(self.installed_admin().exists())

    def test_a_result_that_lists_no_operator_tool_installs_nothing_and_says_so(self) -> None:
        self.set_scenario(build={"admin": "none"})
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertIn("operator tool", p.stderr)
        self.assertFalse(self.installed().exists())
        self.assertFalse(self.installed_admin().exists())

    def test_an_operator_tool_that_is_already_installed_is_left_alone_when_the_new_one_is_refused(self) -> None:
        self.installed_admin().parent.mkdir(parents=True)
        self.installed_admin().write_text("#!/bin/sh\necho previous admin\n")
        self.installed_admin().chmod(0o755)
        self.set_scenario(build={"admin": "tamper"})
        self.assertEqual(self.run_full(*self.ARGS).returncode, 5)
        self.assertEqual(self.installed_admin().read_text(), "#!/bin/sh\necho previous admin\n")

    def test_an_operator_tool_with_the_same_checksum_is_left_as_it_is_on_the_next_run(self) -> None:
        self.assertEqual(self.run_full(*self.ARGS).returncode, 0)
        before = self.installed_admin().stat().st_mtime_ns
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.installed_admin().stat().st_mtime_ns, before)

    def test_a_dry_run_names_the_operator_tool_in_its_plan_and_installs_nothing(self) -> None:
        p = self.run_full("--full", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertFalse(self.installed_admin().exists())
        self.assertRegex(p.stdout, r"(?i)operator tool")


class FullCleanupCase(FullBase):
    def test_a_runner_that_cannot_be_removed_is_exit_8_after_the_client_is_working(self) -> None:
        self.set_scenario(runner_delete_fails=True)
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertTrue(self.installed().is_file())
        self.assertIn("doctor", self.log_lines())
        self.assertIn("RETAINED", p.stdout + p.stderr)
        self.assertIn("hamh-setup-runner", p.stdout + p.stderr)

    def test_a_bucket_that_cannot_be_emptied_is_exit_8_and_named(self) -> None:
        self.set_scenario(empty_bucket_fails=True)
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertIn(self.bucket, p.stdout + p.stderr)
        self.assertTrue(self.installed().is_file())

    def test_a_doctor_problem_is_exit_3_even_when_the_runner_stayed(self) -> None:
        self.set_scenario(runner_delete_fails=True)
        p = self.run_full(*self.ARGS, FAKE_DOCTOR="PASS identity ok\nFAIL api no", FAKE_DOCTOR_RC="1")
        self.assertEqual(p.returncode, 3, self.out(p))


class FullExistingStageCase(FullBase):
    def test_a_stage_that_is_already_deployed_is_not_deployed_again(self) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True)
        self.ham_on_path()
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.mutations(), [])
        self.assertIn("already", p.stdout.lower())
        self.assertIn(f"config set api_endpoint {self.ENDPOINT}", self.log_lines())

    def test_a_deployed_stage_and_no_client_names_the_ways_to_get_one(self) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True)
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertEqual(self.mutations(), [])
        self.assertIn("--update", p.stderr)
        self.assertIn("--from", p.stderr)

    def test_a_stage_in_a_failed_state_is_exit_7_and_points_at_teardown(self) -> None:
        w = self.open_world()
        w.deploy_stage(status="DELETE_FAILED")
        w.save()
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(self.mutations(), [])
        self.assertIn("DELETE_FAILED", p.stderr)
        self.assertIn("--teardown", p.stderr)


class FullLocalBuildCase(FullBase):
    """--build-on local: the same deploy engine on this machine, for a person who already has the toolchain."""

    TOOLS = ("cargo", "cargo-zigbuild", "zig", "node", "docker")
    DEPLOY = ('#!/bin/sh\nprintf "%s|%s|%s\\n" "$*" "${HAMH_ACCOUNTS_FILE:-}" "${HAMH_AWS_PROFILE:-}" >> "$STUB_LOG/deploy.log"\n'
              'aws cloudformation deploy --stack-name hamh-direct-control --template-file infra/setup-runner.yaml --capabilities CAPABILITY_NAMED_IAM >/dev/null\n'
              'echo "DEPLOY-PASS stage=direct digest=aaaaaaaaaaaa sourceTree=bbbbbbbbbbbb"\n')

    def make_local_checkout(self) -> None:
        d = self.repo / "scripts"
        d.mkdir(exist_ok=True)
        (d / "deploy.sh").write_text(self.DEPLOY)
        (d / "deploy.sh").chmod(0o755)
        (self.repo / "Cargo.toml").write_text("[workspace]\n")
        (self.repo / "crates" / "ham").mkdir(parents=True)
        (self.repo / "crates" / "ham" / "Cargo.toml").write_text("[package]\nname = \"ham\"\n")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "local build files")

    def tools_on_path(self, *names: str) -> None:
        for n in names:
            self.write_stub(n, "#!/bin/sh\nexit 0\n")
        self.write_stub("cargo", STUB_CARGO)

    ARGS_LOCAL = ("--full", "--build-on", "local", "--profile", "work", "--region", "us-east-1", "--yes", "--acknowledge-costs")

    def test_missing_tools_are_listed_and_nothing_is_created(self) -> None:
        self.make_local_checkout()
        self.tools_on_path("cargo-zigbuild", "node")
        p = self.run_full(*self.ARGS_LOCAL, STUB_HAM_SOURCE=str(self.ham_source))
        self.assertEqual(p.returncode, 1, self.out(p))
        for missing in ("zig", "docker"):
            self.assertRegex(p.stderr, rf"\b{missing}\b")
        self.assertEqual(self.mutations(), [])

    def test_with_the_toolchain_the_deploy_engine_runs_here_with_the_generated_account_file(self) -> None:
        self.make_local_checkout()
        self.tools_on_path(*self.TOOLS)
        p = self.run_full(*self.ARGS_LOCAL, STUB_HAM_SOURCE=str(self.ham_source))
        self.assertEqual(p.returncode, 0, self.out(p))
        line = self.log_lines("deploy.log")[0]
        args, accounts_file, profile = line.split("|")
        self.assertEqual(args, "--stage direct --azs 1 --fork-source build")
        self.assertEqual(profile, "work")
        self.assertTrue(accounts_file.startswith(str(self.home)), "the generated file lives outside the checkout, so the tree stays clean")
        self.assertEqual(self.git("status", "--porcelain", "--untracked-files=no"), "")
        self.assertEqual(self.count_calls("codebuild", "start-build"), 0)
        self.assertEqual(self.count_calls("cloudformation", "deploy", "--stack-name", "hamh-setup-runner"), 0)
        self.assertIn(f"config set api_endpoint {self.ENDPOINT}", self.log_lines())

    def test_the_local_route_compiles_the_client_and_the_operator_tool_in_one_cargo_run_and_installs_both(self) -> None:
        self.make_local_checkout()
        (self.repo / "crates" / "ham-admin").mkdir(parents=True)
        (self.repo / "crates" / "ham-admin" / "Cargo.toml").write_text("[package]\nname = \"ham-admin\"\n")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "the operator tool crate")
        self.tools_on_path(*self.TOOLS)
        p = self.run_full(*self.ARGS_LOCAL, STUB_HAM_SOURCE=str(self.ham_source))
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.log_lines("cargo.log"), [f"{self.repo}|build --release --locked -p ham -p ham-admin --bin ham --bin ham-admin"])
        self.assertTrue(self.installed().is_file())
        self.assertTrue(self.installed_admin().is_file())
        self.assertEqual(self.installed_admin().parent, self.installed().parent)
        self.assertIn("ham-admin v9.99 built here", self.installed_admin().read_text())
        self.assertTrue(self.installed_admin().stat().st_mode & stat.S_IXUSR)

    # O4 of the live rehearsal of the public setup: the screen of the local route carried about 450 lines of CDK notices that repeat for every stack. The screen is quiet
    # about the known repeating ones; the log of the run keeps every line; --verbose shows every line on the screen.
    NOISE = ('[WARNING] aws-cdk-lib.CfnResource#addDependency is deprecated.', '  Use `addResourceDependency` instead.',
             '  This API will be removed in the next major release.',
             '`cdk synth` may hang in Docker on Linux 5.6-5.10. See https://github.com/aws/aws-cdk/issues/21379 for workarounds.',
             'INFO HAMH_UTILITY_MODEL is unset: the default utility model is smoked in us-east-1 and the stage Region is us-west-2, so run names are off here (Construct Annotations)',
             '   hamh-direct-orchestration', "   Acknowledge with 'Construct-Annotations::info-annotation'")
    KEEP = ("hamh-direct-network: deploying [1/9]", "[Warning at /hamh-direct-data] a notice that this filter does not know", "   an indented line that belongs to no notice")

    def noisy_deploy(self) -> None:
        body = "\n".join(self.NOISE[:4] + self.KEEP[:1] + self.NOISE[4:] + self.KEEP[1:])
        text = self.DEPLOY.replace("echo \"DEPLOY-PASS", "cat <<'NOISE'\n" + body + "\nNOISE\necho \"DEPLOY-PASS", 1)
        (self.repo / "scripts" / "deploy.sh").write_text(text)
        self.git("commit", "-q", "-am", "a deploy that prints CDK notices")

    def setup_log(self) -> str:
        logs = sorted(self.home.rglob("setup-*.log"))
        self.assertTrue(logs, "the run keeps a log")
        return logs[-1].read_text()

    def test_the_screen_leaves_out_the_repeating_cdk_notices_and_the_log_keeps_them(self) -> None:
        self.make_local_checkout()
        self.tools_on_path(*self.TOOLS)
        self.noisy_deploy()
        p = self.run_full(*self.ARGS_LOCAL, STUB_HAM_SOURCE=str(self.ham_source))
        self.assertEqual(p.returncode, 0, self.out(p))
        for line in self.NOISE:
            self.assertNotIn(line.strip(), p.stdout, line)
        for line in self.KEEP:
            self.assertIn(line, p.stdout, "a line the filter does not know stays on the screen")
        self.assertIn("DEPLOY-PASS stage=direct", p.stdout)
        log = self.setup_log()
        for line in (*self.NOISE, *self.KEEP):
            self.assertIn("deploy: " + line, log, line)

    def test_verbose_shows_every_line_on_the_screen(self) -> None:
        self.make_local_checkout()
        self.tools_on_path(*self.TOOLS)
        self.noisy_deploy()
        p = self.run_full(*self.ARGS_LOCAL, "--verbose", STUB_HAM_SOURCE=str(self.ham_source))
        self.assertEqual(p.returncode, 0, self.out(p))
        for line in (*self.NOISE, *self.KEEP):
            self.assertIn(line.strip(), p.stdout, line)

    # D7 of the live rehearsal of the public setup: the receipt of a local run that succeeded still held the build id, the failed phase and the reason of an earlier
    # run on the runner route, because a new run only added keys.
    def test_a_local_run_after_a_failed_runner_run_leaves_no_failure_and_no_build_in_the_receipt(self) -> None:
        self.make_local_checkout()
        self.tools_on_path(*self.TOOLS)
        self.set_scenario(build={"final": "FAILED", "result": "fail", "failed_phase": "deploy", "detail": "synth cdk synth failed"})
        first = self.run_full(*self.ARGS)
        self.assertEqual(first.returncode, 5, self.out(first))
        failed = self.receipt()
        self.assertEqual((failed["state"], failed["failedPhase"]), ("failed", "deploy"))
        self.assertTrue(failed["buildId"])
        self.set_scenario(build={})
        second = self.run_full(*self.ARGS_LOCAL, STUB_HAM_SOURCE=str(self.ham_source))
        self.assertEqual(second.returncode, 0, self.out(second))
        done = self.receipt()
        self.assertEqual(done["state"], "installed")
        self.assertEqual(done["mode"], "local")
        self.assertEqual((done.get("failedPhase", ""), done.get("detail", ""), done.get("buildId", "")), ("", "", ""), done)
        status = self.run_full("--status", "--profile", "work", "--region", "us-east-1")
        self.assertNotIn("failed in phase", status.stdout)

    def test_the_file_the_local_deploy_reads_has_the_users_profile_and_loads(self) -> None:
        import sys
        sys.path.insert(0, str(REPO / "scripts" / "lib"))
        import aws_target
        self.make_local_checkout()
        self.tools_on_path(*self.TOOLS)
        (self.repo / "scripts" / "deploy.sh").write_text(self.DEPLOY.replace('printf', 'cp "$HAMH_ACCOUNTS_FILE" "$STUB_LOG/accounts-used.json"\nprintf', 1))
        self.git("commit", "-q", "-am", "keep a copy of the file")
        p = self.run_full(*self.ARGS_LOCAL, STUB_HAM_SOURCE=str(self.ham_source))
        self.assertEqual(p.returncode, 0, self.out(p))
        t = aws_target.load(self.logs / "accounts-used.json")[0]
        self.assertEqual((t.account, t.profile, t.role), (self.ACCOUNT, "work", "active"))


class FullSourceCase(FullBase):
    """How the runner build gets its source. The CodeBuild project of infra/setup-runner.yaml has an S3 source with a placeholder location, so the build finds
    the archive only when `start-build` names it: `--source-location-override <bucket>/<key>` and, as the runner script reads it by that version
    (CODEBUILD_SOURCE_VERSION), `--source-version <the version id of the upload>`. A start-build without them ends in DOWNLOAD_SOURCE with no result."""

    def start_args(self) -> list:
        return [c for c in self.normalized_calls() if c[:2] == ["codebuild", "start-build"]][0]

    def stored_version(self, key: str) -> str:
        return self.world()["versions"][self.bucket][key][-1]["id"]

    def test_the_build_is_told_which_archive_to_read(self) -> None:
        commit = self.commit()
        self.assertEqual(self.run_full(*self.ARGS).returncode, 0)
        start = self.start_args()
        self.assertEqual(start[start.index("--source-location-override") + 1], f"{self.bucket}/source/{commit}.zip")

    def test_the_build_is_pinned_to_the_version_of_the_uploaded_archive(self) -> None:
        commit = self.commit()
        self.assertEqual(self.run_full(*self.ARGS, "--keep-runner").returncode, 0)
        start = self.start_args()
        self.assertEqual(start[start.index("--source-version") + 1], self.stored_version(f"source/{commit}.zip"))

    def test_the_stand_in_codebuild_fails_a_build_that_was_given_no_source(self) -> None:
        """The stand-in must be as strict as CodeBuild, or the cases above prove nothing."""
        self.set_scenario(runner_present=True, bootstrap_present=True)
        env = ("name=SETUP_SOURCE_KEY,value=source/%s.zip name=SETUP_CONFIG_KEY,value=config/accounts-0123456789ab.json name=SETUP_STAGE,value=direct "
               "name=SETUP_AZS,value=1 name=SETUP_BINARY_NAME,value=ham") % self.commit()
        aws = str(self.stub / "aws")
        run = lambda *a: subprocess.run([aws, *a], capture_output=True, text=True, env=self.env(), timeout=limit(60))  # nosec B603: argument list, the stand-in AWS CLI of the tests
        started = run("codebuild", "start-build", "--project-name", "hamh-setup-runner-build", "--environment-variables-override", *env.split())
        self.assertEqual(started.returncode, 0, started.stderr)
        build_id = json.loads(started.stdout)["build"]["id"]
        for _ in range(4):
            status = run("codebuild", "batch-get-builds", "--ids", build_id, "--query", "builds[0].[buildStatus,currentPhase]", "--output", "text")
        self.assertEqual(status.stdout.split(), ["FAILED", "DOWNLOAD_SOURCE"], status.stdout + status.stderr)


class FullBucketCase(FullBase):
    """Taking the runner away empties its versioned bucket first. scripts/setup/empty-bucket.sh does it (it accepts only a setup bucket and names the account of
    the bucket as its expected owner on every call); setup.sh runs it with the profile of the run."""

    EMPTYING = (["s3api", "list-object-versions"], ["s3api", "delete-objects"])

    def raw_calls(self) -> list:
        return [json.loads(line) for line in (self.state / "calls.jsonl").read_text().splitlines() if line.strip()]

    def test_every_call_that_empties_the_bucket_names_the_account_as_its_owner(self) -> None:
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        calls = [c for c in self.normalized_calls() if c[:2] in self.EMPTYING]
        self.assertGreaterEqual(len(calls), 2)
        for c in calls:
            self.assertEqual(c[c.index("--expected-bucket-owner") + 1], self.ACCOUNT, c)
            self.assertEqual(c[c.index("--bucket") + 1], self.bucket, c)

    def test_the_objects_go_in_payload_files_and_the_stack_goes_after_them(self) -> None:
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        deletes = [c for c in self.normalized_calls() if c[:2] == ["s3api", "delete-objects"]]
        self.assertGreaterEqual(len(deletes), 1)
        for c in deletes:
            self.assertTrue(c[c.index("--delete") + 1].startswith("file://"), c)
        self.assert_order(["s3api", "delete-objects"], ["cloudformation", "delete-stack", "--stack-name", "hamh-setup-runner"])

    def test_the_profile_of_the_run_reaches_the_script_as_an_option_like_every_other_call(self) -> None:
        """M5: the script gets the profile as --profile (HAMH_EMPTY_BUCKET_PROFILE), which beats exported access keys, instead of AWS_PROFILE, which does not."""
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        emptying = [c for c in self.raw_calls() if normalize(c["args"])[:2] in self.EMPTYING]
        self.assertTrue(emptying)
        for c in emptying:
            self.assertEqual(c["args"][:2], ["--profile", "work"], c["args"])
            self.assertNotIn("AWS_PROFILE", c["env"], "the profile is an option of the call, not a variable of the script")

    def test_a_bucket_that_is_already_gone_is_not_a_failure(self) -> None:
        self.set_scenario(runner_present=True, bootstrap_present=True)
        w = self.open_world()
        w.w["versions"].pop(self.bucket, None)
        w.w["buckets"].pop(self.bucket, None)
        w.save()
        p = self.run_full("--teardown", "--profile", "work", "--region", "us-east-1", "--yes", "--acknowledge-teardown", "--account-id", "210987654321")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("hamh-setup-runner", self.world()["stacks"])


if __name__ == "__main__":
    unittest.main()
