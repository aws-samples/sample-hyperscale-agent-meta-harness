"""Fixes after the review of the integrated full setup, driven without a terminal against the stand-in AWS CLI.

The review of `b694-int` found eleven defects. Five of them are about trust: the source archive and the account file are written into a bucket that
other principals of the account may write, so the runner that executes them with deployment authority and the client that setup.sh installs and runs must
not believe what lies there (the archive is uploaded every time and pinned by the version id the upload answered with, its SHA-256 and the account file's
travel in the build environment, the bucket policy lets only the installer and the runner write, the build records the SHA-256 of its result in CodeBuild
where no bucket writer reaches it, and the endpoint is compared with the stack output). One is about how long the runner's authority lasts. The others are
about the order of the flow: a stage that is being deployed or that rolled back, a preview that must not install, a client that must be installed before the
runner goes, a runner stack that rolled back, and a profile that does not exist.

Every case names what it pins in its title. The cases that change an existing behaviour are in the files that held the old one.
"""

from __future__ import annotations

import datetime as dt
import hashlib
import json
import shutil
import unittest
import zipfile

import setup_fake_aws
from setup_full_support import STUB_ADMIN, FullBase
from test_setup_full_flow import golden_digest
from test_setup_full_state import DIGEST, GOLDEN_EAST, StateBase
from test_setup_full_units import HelperCase
from test_setup_script import STUB_HAM, STUB_CARGO


def option_values(call: list, option: str) -> list:
    """The values after an option of an AWS CLI call, up to the next option."""
    out, at = [], call.index(option) + 1
    while at < len(call) and not call[at].startswith("--"):
        out.append(call[at])
        at += 1
    return out


def overrides(call: list) -> dict:
    return dict(item.split("=", 1) for item in option_values(call, "--parameter-overrides"))


class ReviewBase(StateBase):
    def runner_deploy(self, n: int = 0) -> list:
        calls = [c for c in self.normalized_calls() if c[:4] == ["cloudformation", "deploy", "--stack-name", "hamh-setup-runner"]]
        self.assertGreater(len(calls), n, "no deploy of the runner stack")
        return calls[n]

    def source_puts(self, commit: str) -> list:
        return [c for c in self.normalized_calls() if c[:2] == ["s3api", "put-object"] and f"source/{commit}.zip" in c]

    def stored_version(self, key: str) -> str:
        return self.world()["versions"][self.bucket][key][-1]["id"]

    def start_args(self, n: int = 0) -> list:
        return [c for c in self.normalized_calls() if c[:2] == ["codebuild", "start-build"]][n]


class SourceIntegrityCase(ReviewBase):
    """The archive that the runner executes with deployment authority is the one setup.sh uploaded, and the build reads exactly that upload."""

    def test_the_archive_is_put_with_a_checksum_and_the_build_is_pinned_to_the_version_the_put_answered(self) -> None:
        commit = self.commit()
        self.assertEqual(self.run_full(*self.ARGS, "--keep-runner").returncode, 0)
        puts = self.source_puts(commit)
        self.assertEqual(len(puts), 1, puts)
        self.assertEqual(option_values(puts[0], "--checksum-algorithm"), ["SHA256"])
        self.assertEqual(option_values(puts[0], "--expected-bucket-owner"), [self.ACCOUNT], "a bucket of another account under this name is not written to")
        self.assertEqual(option_values(puts[0], "--query"), ["VersionId"], "the version id comes from the answer of the upload")
        self.assertEqual(self.count_calls("s3api", "head-object", "--bucket", self.bucket, "--key", f"source/{commit}.zip"), 0,
                         "a later look at the key can name a version somebody else wrote in between")
        self.assertEqual(option_values(self.start_args(), "--source-version"), [self.stored_version(f"source/{commit}.zip")])

    def test_an_archive_planted_under_the_key_is_replaced_and_never_run(self) -> None:
        commit = self.commit()
        self.set_scenario(runner_present=True, bootstrap_present=True)
        self.seed_object(f"source/{commit}.zip", b"an archive another principal of the account wrote")
        planted = self.stored_version(f"source/{commit}.zip")
        p = self.run_full(*self.ARGS, "--keep-runner")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("already in your bucket", p.stdout)
        with zipfile.ZipFile(self.state / "s3" / self.bucket / "source" / f"{commit}.zip") as z:
            self.assertEqual(z.comment.decode(), commit)
        now = self.stored_version(f"source/{commit}.zip")
        self.assertNotEqual(now, planted)
        self.assertEqual(option_values(self.start_args(), "--source-version"), [now])

    def test_an_account_file_planted_under_the_key_is_replaced(self) -> None:
        self.set_scenario(runner_present=True, bootstrap_present=True)
        self.seed_object(f"config/accounts-{DIGEST[:12]}.json", b'{"accounts": []}')
        p = self.run_full(*self.ARGS, "--keep-runner")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.s3_bytes(f"config/accounts-{DIGEST[:12]}.json"), GOLDEN_EAST)

    def test_the_build_is_told_the_sha256_of_the_archive_and_of_the_account_file(self) -> None:
        commit = self.commit()
        self.assertEqual(self.run_full(*self.ARGS, "--keep-runner").returncode, 0)
        env = self.start_build_env()
        self.assertEqual(env["SETUP_SOURCE_SHA256"], hashlib.sha256(self.s3_bytes(f"source/{commit}.zip")).hexdigest())
        self.assertEqual(env["SETUP_CONFIG_SHA256"], golden_digest(), "all 64 digits, not the 12 of the key")
        self.assertRegex(env["SETUP_SOURCE_SHA256"], r"^[0-9a-f]{64}$")

    def test_the_template_build_and_the_deploy_build_are_told_the_same(self) -> None:
        self.set_scenario(bootstrap_present=False)
        self.assertEqual(self.run_full(*self.ARGS, "--keep-runner").returncode, 0)
        for n in (0, 1):
            env = self.start_build_env(n)
            self.assertIn("SETUP_SOURCE_SHA256", env, n)
            self.assertIn("SETUP_CONFIG_SHA256", env, n)
        self.assertEqual(self.start_build_env(0)["SETUP_ACTION"], "bootstrap-template")

    def test_a_bucket_that_gives_no_version_id_is_refused_before_a_build_starts(self) -> None:
        self.set_scenario(no_version_ids=True)
        p = self.run_full(*self.ARGS, "--keep-runner")
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 0)
        self.assertIn("version", p.stderr)
        self.assertFalse(self.installed().exists())


class InstallerPrincipalCase(ReviewBase):
    """The bucket lets only the runner role and the identity that runs setup.sh write or delete. setup.sh passes that identity to the stack."""

    def test_a_user_is_named_exactly(self) -> None:
        self.assertEqual(self.run_full(*self.ARGS, "--keep-runner").returncode, 0)
        want = f"arn:aws:iam::{self.ACCOUNT}:user/setup-test"
        o = overrides(self.runner_deploy())
        self.assertEqual((o["InstallerPrincipal"], o["InstallerPrincipalWithPath"]), (want, want))

    def test_an_assumed_role_is_named_by_its_role_with_and_without_a_path(self) -> None:
        role = "AWSReservedSSO_Admin_0123456789abcdef"
        self.set_scenario(arn=f"arn:aws:sts::{self.ACCOUNT}:assumed-role/{role}/me@example.invalid")
        self.assertEqual(self.run_full(*self.ARGS, "--keep-runner").returncode, 0)
        o = overrides(self.runner_deploy())
        self.assertEqual(o["InstallerPrincipal"], f"arn:aws:iam::{self.ACCOUNT}:role/{role}")
        self.assertEqual(o["InstallerPrincipalWithPath"], f"arn:aws:iam::{self.ACCOUNT}:role/*/{role}")

    def test_the_root_user_cannot_be_the_operator_of_the_stage_so_the_setup_stops_before_anything_is_made(self) -> None:
        # The stage trusts the identity behind the credentials as its operator, and the root user is neither an IAM role nor an IAM user.
        self.set_scenario(arn=f"arn:aws:iam::{self.ACCOUNT}:root")
        p = self.run_full(*self.ARGS, "--keep-runner")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertEqual(self.mutations(), [])
        self.assertIn("root user", p.stderr)

    def test_an_identity_that_cannot_be_named_stops_the_setup_before_anything_is_made(self) -> None:
        self.set_scenario(arn=f"arn:aws:iam::{self.ACCOUNT}:group/not-an-identity")
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertEqual(self.mutations(), [])
        self.assertIn("group/not-an-identity", p.stderr)

    def test_a_teardown_does_not_install_the_runner_stack_again(self) -> None:
        """The earlier teardown installed the stack again so that the bucket named the caller, which opened a new authority window. The teardown now empties the
        bucket and deletes the stack with the caller's own credentials and installs nothing."""
        self.set_scenario(stage_deployed=True, runner_present=True, bootstrap_present=True)
        p = self.run_full("--teardown", "--profile", "work", "--region", "us-east-1", "--yes", "--acknowledge-teardown", "--account-id", "210987654321")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.count_calls("cloudformation", "deploy"), 0)
        self.assertEqual(self.count_calls("codebuild", "start-build"), 0)


class RunnerLifetimeCase(ReviewBase):
    """The runner's authority to assume the CDK roles ends by itself, and every later run says that the runner is still there."""

    @staticmethod
    def now() -> dt.datetime:
        return dt.datetime.now(dt.timezone.utc).replace(tzinfo=None)

    def expires(self) -> dt.datetime:
        text = overrides(self.runner_deploy())["AuthorityExpires"]
        self.assertRegex(text, r"^20\d\d-\d\d-\d\dT\d\d:\d\d:\d\dZ$")
        return dt.datetime.strptime(text, "%Y-%m-%dT%H:%M:%SZ")

    def test_the_install_writes_an_expiry_twelve_hours_ahead(self) -> None:
        self.assertEqual(self.run_full(*self.ARGS, "--keep-runner").returncode, 0)
        ahead = (self.expires() - self.now()).total_seconds() / 3600
        self.assertGreater(ahead, 11.5)
        self.assertLess(ahead, 12.5)

    def test_the_length_can_be_changed(self) -> None:
        self.assertEqual(self.run_full(*self.ARGS, "--keep-runner", HAMH_SETUP_AUTHORITY_HOURS="6").returncode, 0)
        ahead = (self.expires() - self.now()).total_seconds() / 3600
        self.assertGreater(ahead, 5.5)
        self.assertLess(ahead, 6.5)

    def test_a_length_that_is_not_five_to_seventy_two_hours_is_refused_before_anything_is_made(self) -> None:
        for bad in ("0", "x", "-3", "4", "999"):
            with self.subTest(bad=bad):
                p = self.run_full(*self.ARGS, HAMH_SETUP_AUTHORITY_HOURS=bad)
                self.assertEqual(p.returncode, 2, self.out(p))
                self.assertEqual(self.mutations(), [])
                self.assertIn("HAMH_SETUP_AUTHORITY_HOURS", p.stderr)

    def failed_first_run(self) -> None:
        self.set_scenario(build={"final": "FAILED", "result": "fail"})
        self.assertEqual(self.run_full(*self.ARGS).returncode, 5)

    def test_a_failed_setup_says_when_the_runner_authority_ends(self) -> None:
        self.set_scenario(build={"final": "FAILED", "result": "fail"})
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 5, self.out(p))
        flat = " ".join(self.out(p).split())
        self.assertIn(overrides(self.runner_deploy())["AuthorityExpires"], flat)
        self.assertIn("./setup.sh --teardown", flat)

    def test_a_later_run_says_that_the_runner_is_still_installed_and_how_to_remove_it(self) -> None:
        self.failed_first_run()
        self.set_scenario(build={})
        p = self.run_full(*self.ARGS, "--preflight-only")
        flat = " ".join(self.out(p).split())
        self.assertIn("still installed", flat)
        self.assertIn(overrides(self.runner_deploy())["AuthorityExpires"], flat)
        self.assertIn("./setup.sh --teardown", flat)

    def test_status_and_doctor_say_it_too(self) -> None:
        self.failed_first_run()
        stamp = overrides(self.runner_deploy())["AuthorityExpires"]
        for command in ("--status", "--doctor"):
            with self.subTest(command=command):
                p = self.run_full(command, "--profile", "work", "--region", "us-east-1")
                flat = " ".join(self.out(p).split())
                self.assertIn("still installed", flat)
                self.assertIn(stamp, flat)

    def test_no_line_when_no_runner_is_installed(self) -> None:
        p = self.run_full("--full", "--profile", "work", "--region", "us-east-1", "--preflight-only")
        self.assertNotIn("still installed", self.out(p))


class AuthorityTextCase(FullBase):
    """What the screen and --help say is what the template grants (infra/setup-runner.yaml), including what the runner role does besides writing its logs."""

    def plan(self) -> str:
        p = self.run_full("--full", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        return " ".join(p.stdout.split())

    def test_the_plan_lists_what_the_runner_role_writes_besides_its_logs(self) -> None:
        flat = self.plan()
        self.assertNotIn("may write only its own logs", flat)
        for needle in ("the objects of the stage", "/hamh/direct/", "worker runtime endpoints", "image-build project", "scanner rules"):
            self.assertIn(needle, flat, needle)

    def test_the_plan_names_who_else_can_act_with_the_runner_and_until_when(self) -> None:
        flat = self.plan()
        for needle in ("codebuild:StartBuild", "12 hours", "only you and the runner role may write"):
            self.assertIn(needle, flat, needle)

    def test_the_plan_says_the_cdk_roles_stay_after_the_runner_is_removed(self) -> None:
        flat = self.plan()
        self.assertIn("stay in the account after the runner is removed", flat)
        self.assertIn("trust the whole account", flat)

    def test_help_says_the_same(self) -> None:
        p = self.run_full("--help")
        flat = " ".join(p.stdout.split())
        for needle in ("administrator", "AdministratorAccess", "codebuild:StartBuild", "stay in the account after the runner is removed", "12 hours", "HAMH_SETUP_AUTHORITY_HOURS"):
            self.assertIn(needle, flat, needle)
        self.assertNotIn("may write only its logs and its bucket", flat)
        self.assertNotIn("boundary limits the deployment and execution roles", flat)


class ClientAnchorCase(ReviewBase):
    """The client is installed and run with the user's credentials, so the values that decide whether to trust it cannot come from the bucket alone: the build
    records the SHA-256 of its result.json in CodeBuild (an exported variable), and setup.sh believes only a result that hashes to it."""

    def test_a_coordinated_rewrite_of_the_client_its_manifest_and_the_result_is_refused(self) -> None:
        self.set_scenario(build={"forge": "client"})
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertFalse(self.installed().exists())
        self.assertEqual(self.log_lines(), [], "the downloaded client never ran")
        self.assertIn("CodeBuild", p.stderr)

    def test_a_build_with_no_recorded_hash_is_refused(self) -> None:
        self.set_scenario(build={"anchor": "none"})
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertFalse(self.installed().exists())

    def test_a_wrong_recorded_hash_is_refused(self) -> None:
        self.set_scenario(build={"anchor": "wrong"})
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertFalse(self.installed().exists())
        self.assertEqual(self.log_lines(), [])

    def test_a_refused_result_releases_the_lease_so_the_next_run_builds_again(self) -> None:
        self.set_scenario(build={"anchor": "none"})
        self.assertEqual(self.run_full(*self.ARGS).returncode, 7)
        self.assertFalse(self.s3_exists("lease/direct.json"))
        self.set_scenario(build={})
        # the refused build did deploy the stage, so the way to a client is an update, which builds once more
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 2)
        self.assertTrue(self.installed().is_file())

    def test_the_hash_of_the_bytes_in_the_bucket_is_checked_before_the_client_is_downloaded(self) -> None:
        self.assertEqual(self.run_full(*self.ARGS).returncode, 0)
        calls = self.normalized_calls()
        looks = [i for i, c in enumerate(calls) if c[:2] == ["codebuild", "batch-get-builds"] and any("SETUP_RESULT_SHA256" in a for a in c)]
        downloads = [i for i, c in enumerate(calls) if c[:2] == ["s3", "cp"] and "/client/" in c[2]]
        self.assertTrue(looks, "no look at the exported variable of the build")
        self.assertTrue(downloads)
        self.assertLess(looks[0], downloads[0])

    def test_a_look_that_fails_is_exit_6_and_the_next_run_finishes_from_the_same_build(self) -> None:
        self.set_scenario(faults=[{"match": "exportedEnvironmentVariables", "exit": 255, "stderr": "Could not connect to the endpoint URL", "times": 1}])
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 6, self.out(p))
        self.assertFalse(self.installed().exists())
        self.assertIn("/hamh/direct/deploy-lock", self.world().get("params", {}), "the deploy lock stays, so the next run reads the finished build")
        q = self.run_full(*self.ARGS)
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1)

    def test_a_lease_naming_an_earlier_build_does_not_make_rewritten_files_believable(self) -> None:
        """The way a principal with write access to the bucket would go: a build of this checkout that already succeeded, its client, manifest and result
        rewritten, and a lease that names it, so that setup.sh reads the finished build at once."""
        commit = self.commit()
        bid = self.seed_finished_build(commit)
        w = self.open_world()
        setup_fake_aws.Build(w, bid).forge(self.bucket, "client", json.loads(self.s3_bytes(f"results/{bid}/result.json")))
        w.save()
        self.seed_object("lease/direct.json", self.lease_json(commit, bid))
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 0)
        self.assertFalse(self.installed().exists())
        self.assertEqual(self.log_lines(), [])


class EndpointCase(ReviewBase):
    """The endpoint the client is pointed at is the one the stack in the account has, and the checks of a URL do not let a look-alike host through."""

    OTHER = "https://zzz999.execute-api.us-east-1.amazonaws.com/direct"

    def test_a_result_that_names_another_endpoint_than_the_stack_is_refused(self) -> None:
        self.set_scenario(build={"result_extra": {"endpoint": self.OTHER}})
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertIn(self.OTHER, p.stderr)
        self.assertIn(self.ENDPOINT, p.stderr)
        self.assertFalse(self.installed().exists())
        self.assertNotIn("config set api_endpoint", "\n".join(self.log_lines()))

    def test_the_endpoint_is_read_from_the_control_stack_after_the_build(self) -> None:
        self.assertEqual(self.run_full(*self.ARGS).returncode, 0)
        self.assertGreaterEqual(self.count_calls("cloudformation", "describe-stacks", "--stack-name", "hamh-direct-control"), 1)

    def test_a_host_that_only_starts_like_this_machine_is_not_this_machine(self) -> None:
        art = self.make_artifact()
        for bad in ("http://localhost.attacker.example/direct", "http://127.0.0.1.attacker.example/direct", "http://localhost" + ":80" + "@attacker.example/direct",
                    "http://127.0.0.1:9x/direct", "http://localhost?x=1", "http://localhostx"):
            with self.subTest(bad=bad):
                p = self.run_setup("--from", str(art), "--endpoint", bad, "--yes")
                self.assertEqual(p.returncode, 2, self.out(p))
                self.assertIn("--endpoint", p.stderr)

    def test_this_machine_with_a_port_and_a_path_is_still_accepted(self) -> None:
        for good in ("http://localhost", "http://localhost:8080/direct", "http://127.0.0.1", "http://127.0.0.1:9000/direct", "https://abc.execute-api.us-east-1.amazonaws.com/direct"):
            with self.subTest(good=good):
                art = self.make_artifact(name="art-" + hashlib.sha1(good.encode()).hexdigest()[:8])  # nosec B324: a name for a directory
                p = self.run_setup("--from", str(art), "--endpoint", good, "--yes")
                self.assertEqual(p.returncode, 0, self.out(p))

    def test_a_deployed_stage_has_an_api_gateway_endpoint_of_amazonaws_com(self) -> None:
        for bad in ("https://attacker.example/direct", "https://abc.execute-api.us-east-1.amazonaws.com.attacker.example/direct", "https://user@abc.execute-api.us-east-1.amazonaws.com/direct"):
            with self.subTest(bad=bad):
                self.set_scenario(build={"result_extra": {"endpoint": bad}})
                self.state_reset_world()
                p = self.run_full(*self.ARGS)
                self.assertEqual(p.returncode, 5, self.out(p))
                self.assertFalse(self.installed().exists())

    def state_reset_world(self) -> None:
        if self.state.exists():
            shutil.rmtree(self.state)
        shutil.rmtree(self.home / ".config" / "ham" / "setup", ignore_errors=True)


class StageStateCase(ReviewBase):
    """What the state of the stage's stacks means for a run: a build of this checkout explains stacks that are in progress, a stack that rolled back is a state
    that setup builds on (or removes and makes again), and only a state nobody can build on stops the run."""

    def seed_stage(self, **statuses: str) -> None:
        w = self.open_world()
        for name, status in statuses.items():
            w.w["stacks"][f"hamh-direct-{name}"] = {"status": status, "outputs": {"V1ApiUrl": self.ENDPOINT} if name == "control" else {}}
        w.save()

    def seed_running_build(self) -> str:
        commit = self.commit()
        self.seed_runner()
        env = {"SETUP_SOURCE_KEY": f"source/{commit}.zip", "SETUP_CONFIG_KEY": f"config/accounts-{DIGEST[:12]}.json", "SETUP_STAGE": "direct", "SETUP_AZS": "1",
               "SETUP_BINARY_NAME": "ham"}
        self.seed_object(f"config/accounts-{DIGEST[:12]}.json", GOLDEN_EAST)
        bid = self.seed_build("IN_PROGRESS", env)
        self.seed_object("lease/direct.json", self.lease_json(commit, bid))
        return bid

    def test_a_build_of_this_checkout_explains_a_stack_that_is_in_progress(self) -> None:
        bid = self.seed_running_build()
        self.seed_stage(network="CREATE_COMPLETE", data="CREATE_IN_PROGRESS")
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 0)
        self.assertIn(bid, p.stdout)

    def test_an_update_attaches_the_same_way(self) -> None:
        bid = self.seed_running_build()
        self.seed_stage(network="UPDATE_COMPLETE", data="UPDATE_IN_PROGRESS", control="UPDATE_COMPLETE")
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 0)
        self.assertIn(bid, p.stdout)

    def test_a_stack_in_progress_that_no_build_explains_is_exit_7_and_nothing_is_changed(self) -> None:
        self.seed_stage(network="CREATE_COMPLETE", data="CREATE_IN_PROGRESS")
        for args in (self.ARGS, ("--update", "--profile", "work", "--region", "us-east-1", "--yes")):
            with self.subTest(args=args[0]):
                p = self.run_full(*args)
                self.assertEqual(p.returncode, 7, self.out(p))
                self.assertEqual(self.mutations(), [])
                self.assertIn("hamh-direct-data CREATE_IN_PROGRESS", p.stderr)
                self.assertNotIn("--teardown", p.stderr, "waiting is the advice for a stack that is still changing")

    def test_status_reports_a_stage_that_is_being_changed_as_running(self) -> None:
        self.seed_stage(network="CREATE_COMPLETE", data="CREATE_IN_PROGRESS")
        p = self.run_full("--status", "--profile", "work", "--region", "us-east-1")
        self.assertEqual(p.returncode, 6, self.out(p))
        self.assertIn("state=deploying", p.stdout)

    def test_a_stage_whose_update_rolled_back_is_a_stage_setup_builds_on(self) -> None:
        self.seed_stage(network="UPDATE_ROLLBACK_COMPLETE", data="UPDATE_COMPLETE", control="UPDATE_ROLLBACK_COMPLETE")
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1)

    def test_a_first_deploy_that_rolled_back_is_tried_again(self) -> None:
        self.seed_stage(network="CREATE_COMPLETE", control="ROLLBACK_COMPLETE")
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1)
        self.assertIn("ROLLBACK_COMPLETE", p.stdout, "the screen says that the failed stack is made again")

    def test_a_stack_that_rolled_back_next_to_a_complete_control_stack_means_the_stage_is_partial(self) -> None:
        self.seed_stage(network="CREATE_COMPLETE", control="CREATE_COMPLETE", data="ROLLBACK_COMPLETE")
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1)

    # D5 of the live rehearsal of the public setup: after the first pass of the two-pass deploy every stack of the stage exists (no worker image, a placeholder
    # runtime), so a stage with a complete control stack read as deployed and a retry of --full stopped with "already deployed" and no way forward. The deploy writes
    # /hamh/<stage>/deployment-digest after its last step; a stage whose stacks are all complete but that has none is partial.

    def unfinished_stage(self) -> None:
        self.set_scenario(stage_deployed=True, unfinished_deploy=True, bootstrap_present=True)
        self.ham_on_path()  # a client is on this machine, so a run that finds the stage deployed has one to configure

    def test_a_stage_with_every_stack_but_no_deploy_record_is_partial_and_the_full_setup_deploys_it_again(self) -> None:
        self.unfinished_stage()
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1, "the deploy runs again")
        self.assertNotIn("already deployed", p.stdout)
        self.assertIn("did not finish", p.stdout, "the plan says why the deploy runs on a stage that has its stacks")
        self.assertIn("/hamh/direct/deployment-digest", " ".join(" ".join(c) for c in self.normalized_calls() if c[:2] == ["ssm", "get-parameter"]))

    def test_the_deploy_that_follows_leaves_a_record_and_the_next_run_finds_the_stage_deployed(self) -> None:
        self.unfinished_stage()
        self.assertEqual(self.run_full(*self.ARGS).returncode, 0)
        builds = self.count_calls("codebuild", "start-build")
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), builds, "nothing is deployed again")
        self.assertIn("already deployed", p.stdout)

    def test_a_stage_with_every_stack_and_the_deploy_record_is_deployed(self) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True)
        self.ham_on_path()
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 0)
        self.assertIn("already deployed", p.stdout)

    def test_an_update_of_an_unfinished_stage_runs_the_deploy_too(self) -> None:
        self.unfinished_stage()
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1)

    def test_status_calls_an_unfinished_deploy_partial_and_says_why(self) -> None:
        self.unfinished_stage()
        p = self.run_full("--status", "--profile", "work", "--region", "us-east-1")
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertRegex(p.stdout, r"(?m)^STATUS .*state=partial")
        self.assertRegex(p.stdout, r"(?mi)^\s*stage:\s+partial .*did not finish")
        self.assertEqual(self.mutations(), [])

    def test_doctor_fails_the_stage_check_of_an_unfinished_deploy_and_names_the_command(self) -> None:
        self.unfinished_stage()
        p = self.run_full("--doctor", "--profile", "work", "--region", "us-east-1")
        self.assertEqual(p.returncode, 3, self.out(p))
        self.assertRegex(p.stdout, r"(?m)^FAIL stage direct is only partly deployed .*did not finish.*\./setup\.sh --full")
        self.assertNotRegex(p.stdout, r"(?m)^PASS stage\b")

    def test_a_deploy_record_that_cannot_be_read_stops_the_run_and_names_the_permission(self) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True,
                          faults=[{"match": "ssm get-parameter --name /hamh/direct/deployment-digest",
                                   "stderr": "An error occurred (AccessDeniedException) when calling the GetParameter operation: not allowed", "exit": 254}])
        self.ham_on_path()
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("could not read the deploy record of the stage", p.stderr)
        self.assertIn("ssm:GetParameter", p.stderr)
        self.assertEqual(self.mutations(), [])

    def test_states_nobody_can_build_on_still_stop_the_run_and_name_the_way_out(self) -> None:
        for bad in ("DELETE_FAILED", "UPDATE_ROLLBACK_FAILED", "ROLLBACK_FAILED", "CREATE_FAILED", "UPDATE_FAILED", "IMPORT_ROLLBACK_FAILED"):
            with self.subTest(status=bad):
                self.state_reset_world()
                self.seed_stage(network="CREATE_COMPLETE", control=bad)
                p = self.run_full(*self.ARGS)
                self.assertEqual(p.returncode, 7, self.out(p))
                self.assertEqual(self.mutations(), [])
                self.assertIn(bad, p.stderr)
                self.assertIn("--teardown", p.stderr)

    def state_reset_world(self) -> None:
        if self.state.exists():
            shutil.rmtree(self.state)

    def test_a_failed_update_can_be_run_again_as_the_message_says(self) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True, build={"final": "FAILED", "result": "fail", "leaves": {"hamh-direct-control": "UPDATE_ROLLBACK_COMPLETE"}})
        first = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--yes")
        self.assertEqual(first.returncode, 5, self.out(first))
        self.assertIn("./setup.sh --update", first.stderr + first.stdout, "the advice names the command that retries an update")
        self.set_scenario(build={})
        second = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--yes")
        self.assertEqual(second.returncode, 0, self.out(second))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 2)

    def test_a_failed_first_deploy_can_be_run_again_as_the_message_says(self) -> None:
        self.set_scenario(build={"final": "FAILED", "result": "fail", "leaves": {"hamh-direct-network": "CREATE_COMPLETE", "hamh-direct-control": "ROLLBACK_COMPLETE"}})
        first = self.run_full(*self.ARGS)
        self.assertEqual(first.returncode, 5, self.out(first))
        self.set_scenario(build={})
        second = self.run_full(*self.ARGS)
        self.assertEqual(second.returncode, 0, self.out(second))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 2)


class PreviewOfADeployedStageCase(FullBase):
    def writing_ham_lines(self) -> list:
        """The calls of the stand-in client except the reads of its settings (`config get`), which setup.sh makes to show what is set."""
        return [line for line in self.log_lines() if not line.startswith("config get")]

    def test_preflight_only_on_a_deployed_stage_installs_and_configures_nothing(self) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True)
        self.ham_on_path()
        p = self.run_full("--full", "--profile", "work", "--region", "us-east-1", "--yes", "--preflight-only")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.writing_ham_lines(), [], "no ham command ran but the reads of the settings: no config set, no rules init, no doctor")
        self.assertEqual(self.mutations(), [])
        self.assertFalse(self.config_file().exists())
        self.assertIn(self.ENDPOINT, p.stdout)
        self.assertIn("Preflight finished", p.stdout)

    def test_dry_run_on_a_deployed_stage_runs_no_ham_command_either(self) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True)
        self.ham_on_path()
        p = self.run_full("--full", "--profile", "work", "--region", "us-east-1", "--yes", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.writing_ham_lines(), [])
        self.assertEqual(self.mutations(), [])


class ClientBeforeRemovalCase(FullBase):
    """The client lives nowhere but the bucket the setup empties, so it is installed first; and a client that this machine checked earlier is used again."""

    def test_the_client_is_installed_before_the_bucket_is_emptied(self) -> None:
        self.set_scenario(probe_file=str(self.installed()))
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIs(self.world().get("probe"), True, "the runner was taken away before the client was in place")
        self.assertNotIn("hamh-setup-runner", self.world()["stacks"])

    def test_a_failed_install_removes_the_runner_and_keeps_the_checked_client(self) -> None:
        # Every exit after a build that succeeded removes the runner (or names it RETAINED); the checked client stays in the download folder of the receipt,
        # which the next run installs again (full_reuse_client)
        p = self.run_full(*self.ARGS, FAKE_MODELS_RC="3")
        self.assertNotEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("hamh-setup-runner", self.world()["stacks"], "the runner goes on this exit too")
        self.assertTrue((self.verified() / "ham").is_file(), "the checked client is kept outside the bucket")

    def verified(self):
        return self.receipt_path().parent / "download" / "verified"

    def seed_verified(self) -> None:
        d = self.verified()
        d.mkdir(parents=True)
        (d / "ham").write_text(STUB_HAM)
        (d / "ham").chmod(0o755)
        (d / "SHA256SUMS").write_text(f"{hashlib.sha256(STUB_HAM.encode()).hexdigest()}  ham\n")

    def test_a_deployed_stage_and_a_client_checked_earlier_installs_that_client(self) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True)
        self.seed_verified()
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.mutations(), [])
        self.assertEqual(self.installed().read_text(), STUB_HAM)
        self.assertIn("checked earlier", p.stdout)
        self.assertIn(f"config set api_endpoint {self.ENDPOINT}", self.log_lines())

    def test_a_deployed_stage_and_an_operator_tool_checked_earlier_installs_both(self) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True)
        self.seed_verified()
        (self.verified() / "ham-admin").write_text(STUB_ADMIN)
        (self.verified() / "ham-admin").chmod(0o755)
        with open(self.verified() / "SHA256SUMS", "a") as f:
            f.write(f"{hashlib.sha256(STUB_ADMIN.encode()).hexdigest()}  ham-admin\n")
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.installed().read_text(), STUB_HAM)
        self.assertEqual(self.installed_admin().read_text(), STUB_ADMIN)

    def test_an_operator_tool_that_no_longer_matches_its_manifest_is_not_used_and_the_client_is_not_installed_from_that_copy(self) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True)
        self.seed_verified()
        (self.verified() / "ham-admin").write_text(STUB_ADMIN + "# changed after the check\n")
        with open(self.verified() / "SHA256SUMS", "a") as f:
            f.write(f"{hashlib.sha256(STUB_ADMIN.encode()).hexdigest()}  ham-admin\n")
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertFalse(self.installed().exists())
        self.assertFalse(self.installed_admin().exists())

    def test_a_copy_that_no_longer_matches_its_manifest_is_not_used(self) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True)
        self.seed_verified()
        (self.verified() / "ham").write_text(STUB_HAM + "# changed after the check\n")
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertFalse(self.installed().exists())
        self.assertIn("--update", p.stderr)

    def test_a_client_that_is_already_installed_is_left_alone(self) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True)
        self.seed_verified()
        self.ham_on_path()
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertFalse(self.installed().exists(), "the ham on the PATH is used as it is")


class RunnerStackStateCase(FullBase):
    """A runner or bootstrap stack that an earlier run left in a state CloudFormation cannot update."""

    def seed_stack(self, name: str, **kw) -> None:
        w = self.open_world()
        w.w["stacks"][name] = {"outputs": {}, **kw}
        w.save()

    def test_a_runner_stack_that_rolled_back_is_deleted_and_made_again(self) -> None:
        self.seed_stack("hamh-setup-runner", status="ROLLBACK_COMPLETE")
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assert_order(["cloudformation", "delete-stack", "--stack-name", "hamh-setup-runner"], ["cloudformation", "wait", "stack-delete-complete"],
                          ["cloudformation", "deploy", "--stack-name", "hamh-setup-runner"], ["codebuild", "start-build"])
        self.assertIn("ROLLBACK_COMPLETE", p.stdout)

    def test_a_runner_stack_still_being_made_is_waited_for(self) -> None:
        self.seed_stack("hamh-setup-runner", status="CREATE_IN_PROGRESS", polls_left=2, then="CREATE_COMPLETE")
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertGreaterEqual(self.count_calls("cloudformation", "describe-stacks", "--stack-name", "hamh-setup-runner"), 3)
        self.assertEqual(self.count_calls("cloudformation", "delete-stack", "--stack-name", "hamh-setup-runner"), 1, "only the removal after the setup deletes it")

    def test_a_runner_stack_that_never_settles_is_exit_6_and_changes_nothing(self) -> None:
        self.seed_stack("hamh-setup-runner", status="UPDATE_IN_PROGRESS", polls_left=1000, then="UPDATE_COMPLETE")
        p = self.run_full(*self.ARGS, HAMH_SETUP_STACK_LOOKS="3")
        self.assertEqual(p.returncode, 6, self.out(p))
        self.assertIn("UPDATE_IN_PROGRESS", p.stderr)
        self.assertEqual(self.mutations(), [])

    def test_a_runner_stack_that_cannot_be_deleted_is_named_before_the_confirmation_with_its_way_out(self) -> None:
        # U18: found in preflight (exit 7, nothing created), never after the confirmation
        self.seed_stack("hamh-setup-runner", status="DELETE_FAILED")
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertIn("DELETE_FAILED", p.stderr)
        self.assertIn("aws cloudformation delete-stack --stack-name hamh-setup-runner --role-arn", " ".join(p.stderr.split()))
        self.assertEqual(self.mutations(), [])
        self.assertEqual(self.count_calls("codebuild", "start-build"), 0)

    def test_a_bootstrap_stack_that_rolled_back_is_deleted_and_made_again(self) -> None:
        self.set_scenario(bootstrap_present=False)
        self.seed_stack("CDKToolkit", status="ROLLBACK_COMPLETE")
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assert_order(["cloudformation", "delete-stack", "--stack-name", "CDKToolkit"], ["cloudformation", "deploy", "--stack-name", "CDKToolkit"])

    def test_the_message_for_a_stack_that_cannot_be_made_names_the_real_reason(self) -> None:
        self.set_scenario(faults=[{"match": "deploy --stack-name hamh-setup-runner", "exit": 254, "stderr": "An error occurred (AccessDenied) when calling the CreateStack operation: iam:CreateRole"}])
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertIn("iam:CreateRole", p.stderr)


class LibHelpersCase(HelperCase):
    """The two small helpers that the install uses: the principal patterns of the bucket policy and the expiry time."""

    def patterns(self, arn: str):
        return self.sh('setup_installer_patterns "$A"', "lib", A=arn)

    def test_a_user_the_root_user_and_a_federated_user_are_named_exactly(self) -> None:
        for arn in ("arn:aws:iam::222222222222:user/dev/me", "arn:aws:iam::222222222222:root", "arn:aws:sts::222222222222:federated-user/me@example.invalid"):
            with self.subTest(arn=arn):
                p = self.patterns(arn)
                self.assertEqual((p.returncode, p.stdout), (0, f"{arn} {arn}"), p.stderr)

    def test_a_role_session_is_named_by_its_role_with_and_without_a_path(self) -> None:
        p = self.patterns("arn:aws:sts::222222222222:assumed-role/Admin/session-1")
        self.assertEqual((p.returncode, p.stdout), (0, "arn:aws:iam::222222222222:role/Admin arn:aws:iam::222222222222:role/*/Admin"), p.stderr)

    def test_what_is_no_identity_a_policy_can_name_is_refused(self) -> None:
        for arn in ("", "arn:aws:iam::222222222222:group/x", "arn:aws-cn:iam::222222222222:root", "arn:aws:sts::222222222222:assumed-role/Admin",
                    "arn:aws:iam::222222222222:user/*", "arn:aws:sts::222222222222:assumed-role/a b/s", "*"):
            with self.subTest(arn=arn):
                p = self.patterns(arn)
                self.assertEqual((p.returncode, p.stdout), (1, ""), p.stderr)

    def test_the_expiry_is_a_utc_time_the_asked_number_of_seconds_ahead(self) -> None:
        p = self.sh("setup_at_plus 3600", "lib")
        self.assertEqual(p.returncode, 0, p.stderr)
        text = p.stdout.strip()
        self.assertRegex(text, r"^20\d\d-\d\d-\d\dT\d\d:\d\d:\d\dZ$")
        ahead = (dt.datetime.strptime(text, "%Y-%m-%dT%H:%M:%SZ") - dt.datetime.now(dt.timezone.utc).replace(tzinfo=None)).total_seconds()
        self.assertGreater(ahead, 3500)
        self.assertLess(ahead, 3700)


class LocalProfileCase(FullBase):
    """--build-on local hands the deploy engine the profile your credentials really come from."""

    TOOLS = ("cargo-zigbuild", "zig", "node", "docker", "python3")  # python3: the local route needs one of 3.11 or later
    DEPLOY = ('#!/bin/sh\nprintf "%s|%s|%s|%s\\n" "$*" "${HAMH_ACCOUNTS_FILE:-}" "${HAMH_AWS_PROFILE:-}" "${AWS_CONFIG_FILE:-}" >> "$STUB_LOG/deploy.log"\n'
              'cp "$HAMH_ACCOUNTS_FILE" "$STUB_LOG/accounts-used.json"\n'
              '[ -z "${AWS_CONFIG_FILE:-}" ] || cp "$AWS_CONFIG_FILE" "$STUB_LOG/aws-config-used"\n'
              'aws cloudformation deploy --stack-name hamh-direct-control --template-file infra/setup-runner.yaml --capabilities CAPABILITY_NAMED_IAM >/dev/null\n'
              'echo "DEPLOY-PASS stage=direct digest=aaaaaaaaaaaa sourceTree=bbbbbbbbbbbb"\n')
    ARGS_LOCAL = ("--full", "--build-on", "local", "--region", "us-east-1", "--yes", "--acknowledge-costs")

    def setUp(self) -> None:
        super().setUp()
        d = self.repo / "scripts"
        d.mkdir(exist_ok=True)
        (d / "deploy.sh").write_text(self.DEPLOY)
        (d / "deploy.sh").chmod(0o755)
        (self.repo / "Cargo.toml").write_text("[workspace]\n")
        (self.repo / "crates" / "ham").mkdir(parents=True)
        (self.repo / "crates" / "ham" / "Cargo.toml").write_text("[package]\nname = \"ham\"\n")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "local build files")
        for n in self.TOOLS:
            self.write_stub(n, "#!/bin/sh\nexit 0\n")
        self.write_stub("cargo", STUB_CARGO)

    def deploy_line(self) -> list:
        return self.log_lines("deploy.log")[0].split("|")

    def used_profile(self) -> str:
        return json.loads((self.logs / "accounts-used.json").read_text())["accounts"][0]["profile"]

    def test_a_profile_that_is_only_exported_in_the_shell_is_the_profile_of_the_deploy(self) -> None:
        (self.home / ".aws" / "config").write_text("[profile sandbox]\nregion = us-east-1\ncredential_process = /usr/bin/env helper print\n")
        p = self.run_full(*self.ARGS_LOCAL, AWS_PROFILE="sandbox", STUB_HAM_SOURCE=str(self.ham_source))
        self.assertEqual(p.returncode, 0, self.out(p))
        args, accounts_file, profile, config = self.deploy_line()
        self.assertEqual(profile, "sandbox")
        self.assertEqual(self.used_profile(), "sandbox")
        self.assertEqual(config, "", "the user's own AWS configuration is used as it is")

    def test_credentials_in_the_environment_get_a_profile_name_that_exists_for_the_deploy(self) -> None:
        p = self.run_full(*self.ARGS_LOCAL, AWS_ACCESS_KEY_ID="fake-id-" + "1", AWS_SECRET_ACCESS_KEY="fake-secret-" + "1", STUB_HAM_SOURCE=str(self.ham_source))
        self.assertEqual(p.returncode, 0, self.out(p))
        args, accounts_file, profile, config = self.deploy_line()
        self.assertEqual(profile, "hamh-setup")
        self.assertEqual(self.used_profile(), "hamh-setup")
        text = (self.logs / "aws-config-used").read_text()
        self.assertIn("[profile hamh-setup]", text)
        self.assertIn("region = us-east-1", text)
        self.assertNotIn("fake-", text, "no credential is written to the file")
        self.assertTrue(config.startswith(str(self.home)), "the generated file lives in the user's setup folder")

    def test_the_default_profile_is_used_when_nothing_else_is_named(self) -> None:
        p = self.run_full(*self.ARGS_LOCAL, STUB_HAM_SOURCE=str(self.ham_source))
        self.assertEqual(p.returncode, 0, self.out(p))
        args, accounts_file, profile, config = self.deploy_line()
        self.assertEqual((profile, config), ("default", ""))

    def test_a_named_profile_still_wins_over_the_environment(self) -> None:
        p = self.run_full(*self.ARGS_LOCAL, "--profile", "work", AWS_PROFILE="sandbox", STUB_HAM_SOURCE=str(self.ham_source))
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.deploy_line()[2], "work")


if __name__ == "__main__":
    unittest.main()
