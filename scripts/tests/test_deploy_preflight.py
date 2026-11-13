"""Offline tests for the standalone read-only deployment preflight."""

from __future__ import annotations

import dataclasses
import io
import json
import os
import shutil
import subprocess  # nosec B404: tests start the script under test with argument lists, never a shell
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from types import MappingProxyType
from unittest import mock

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "scripts" / "lib"))
sys.path.insert(0, str(Path(__file__).resolve().parent))

import accounts_fixture  # noqa: E402
import aws_target  # noqa: E402
import deploy_fakes  # noqa: E402
import deploy_preflight as preflight  # noqa: E402

SCRIPT = REPO / "scripts" / "deploy-preflight.sh"
# A stand-in AWS CLI for the script test: it logs each call with its profile and answers the three reads from FAKE_AWS_ANSWERS ("<service> <operation>": JSON).
FAKE_AWS = """#!{python}
import json, os, sys
args = sys.argv[1:]
with open(os.environ["FAKE_AWS_LOG"], "a", encoding="utf-8") as f:
    f.write(json.dumps({{"argv": args, "profile": os.environ.get("AWS_PROFILE")}}) + "\\n")
answers = json.loads(os.environ["FAKE_AWS_ANSWERS"])
key = " ".join(args[:2])
if key not in answers:
    sys.stderr.write("unexpected call " + key + "\\n")
    sys.exit(97)
sys.stdout.write(json.dumps(answers[key]))
"""


class PreflightCase(unittest.TestCase):
    def setUp(self) -> None:
        self.context = preflight.Context(
            stage="direct",
            region="us-east-1",
            profile="test-profile",
            expected_account=deploy_fakes.TEST_ACCOUNT,
            expected_azs=(("us-east-1a", "use1-az1"),),
            wanted_cidr="10.50.0.0/16",
            owned_tags=(("project", "hyperscale-agent-meta-harness"), ("stage", "direct")),
        )

    def registration(self, engine=()):
        return preflight.register(self.context, preflight.COMMON_READS, preflight.COMMON_CHECKS, engine=engine)

    def test_authorized_reads_have_exact_argv_environment_and_order(self) -> None:
        recorder = deploy_fakes.FakeAwsRecorder()
        registration = self.registration()
        parent = {
            "PATH": "/safe/bin",
            "AWS_REGION": "wrong",
            "AWS_DEFAULT_REGION": "wrong",
            "AWS_ENDPOINT_URL": "https://wrong.invalid",
            "AWS_ENDPOINT_URL_EC2": "https://wrong.invalid",
        }
        reader = preflight.AwsReader(self.context, registration, aws_bin="aws-safe", parent_environment=parent)
        with mock.patch.object(preflight.subprocess, "run", side_effect=recorder):
            results = preflight.run_checks(self.context, registration, reader)
        self.assertEqual([r.state for r in results], ["PASS", "PASS", "PASS"])
        self.assertEqual([c["argv"][1:3] for c in recorder.calls], [
            ["sts", "get-caller-identity"],
            ["ec2", "describe-availability-zones"],
            ["ec2", "describe-vpcs"],
        ])
        self.assertEqual(recorder.calls[1]["argv"], [
            "aws-safe", "ec2", "describe-availability-zones", "--zone-names", "us-east-1a",
            "--region", "us-east-1", "--output", "json",
        ])
        for call in recorder.calls:
            env = call["env"]
            self.assertEqual(env["AWS_PROFILE"], "test-profile")
            self.assertEqual(env["AWS_PAGER"], "")
            self.assertEqual(env["AWS_CLI_AUTO_PROMPT"], "off")
            self.assertEqual(env["AWS_IGNORE_CONFIGURED_ENDPOINT_URLS"], "true")
            self.assertNotIn("AWS_REGION", env)
            self.assertNotIn("AWS_DEFAULT_REGION", env)
            self.assertNotIn("AWS_ENDPOINT_URL", env)
            self.assertNotIn("AWS_ENDPOINT_URL_EC2", env)
            self.assertEqual(call["timeout"], 30)

    def test_account_nonpass_halts_ec2_reads(self) -> None:
        recorder = deploy_fakes.FakeAwsRecorder({
            ("sts", "get-caller-identity"): {"Account": deploy_fakes.TEST_OTHER_ACCOUNT},
        })
        registration = self.registration()
        reader = preflight.AwsReader(self.context, registration, aws_bin="aws", parent_environment={})
        with mock.patch.object(preflight.subprocess, "run", side_effect=recorder):
            results = preflight.run_checks(self.context, registration, reader)
        self.assertEqual([(r.check_id, r.state) for r in results], [("CALLER_ACCOUNT", "FAIL")])
        self.assertEqual(len(recorder.calls), 1)

    def test_complete_composition_is_rejected_before_launch(self) -> None:
        bad_read = preflight.ReadSpec("bad", "ec2", "describe-instances", lambda _: ())
        with mock.patch.object(preflight.subprocess, "run") as run:
            with self.assertRaises(ValueError):
                preflight.register(self.context, (*preflight.COMMON_READS, bad_read), preflight.COMMON_CHECKS)
        run.assert_not_called()
        duplicate = preflight.CheckSpec("CALLER_ACCOUNT", (), lambda c, r: preflight.CheckResult("x", "PASS", "x", {}))
        with self.assertRaises(ValueError):
            self.registration(engine=(duplicate,))
        missing = preflight.CheckSpec("extra", ("missing",), lambda c, r: preflight.CheckResult("extra", "PASS", "x", {}))
        with self.assertRaises(ValueError):
            self.registration(engine=(missing,))

    def test_registration_does_not_grant_operation_authority(self) -> None:
        mutating = preflight.ReadSpec("mutate", "ec2", "create-vpc", lambda _: ("--cidr-block", "10.0.0.0/16"))
        unlisted = preflight.ReadSpec("instances", "ec2", "describe-instances", lambda _: ())
        for spec in (mutating, unlisted):
            with self.subTest(spec=spec.read_id), self.assertRaises(ValueError):
                preflight.register(self.context, (spec,), ())

    def test_unknown_read_id_and_exact_argument_recheck_are_refused(self) -> None:
        registration = self.registration()
        reader = preflight.AwsReader(self.context, registration, aws_bin="aws", parent_environment={})
        with mock.patch.object(preflight.subprocess, "run") as run:
            with self.assertRaises(KeyError):
                reader.read("missing")
        run.assert_not_called()
        altered = preflight.BoundRead(registration.reads[1].spec, ("--zone-names", "us-east-1b"))
        changed = preflight.Registration((registration.reads[0], altered, registration.reads[2]), registration.checks, registration.common_count)
        reader = preflight.AwsReader(self.context, changed, aws_bin="aws", parent_environment={})
        with mock.patch.object(preflight.subprocess, "run") as run:
            with self.assertRaises(ValueError):
                reader.read("direct-azs")
        run.assert_not_called()

    def test_evaluators_return_immutable_results_and_reject_bad_shapes(self) -> None:
        account = preflight.evaluate_account({"Account": deploy_fakes.TEST_ACCOUNT}, deploy_fakes.TEST_ACCOUNT)
        self.assertEqual((account.state, account.reason_code), ("PASS", "ACCOUNT_MATCH"))
        self.assertIsInstance(account.facts, MappingProxyType)
        self.assertEqual(preflight.evaluate_account({"Account": 1}, deploy_fakes.TEST_ACCOUNT).state, "UNKNOWN")
        az = preflight.evaluate_az_mapping(
            {"AvailabilityZones": [{"ZoneName": "us-east-1a", "ZoneId": "use1-az1"}]},
            (("us-east-1a", "use1-az1"),),
        )
        self.assertEqual((az.state, az.reason_code), ("PASS", "AZ_MAPPING_MATCH"))
        self.assertEqual(az.facts["mapping"], (("us-east-1a", "use1-az1"),))
        duplicate = (("us-east-1a", "use1-az1"), ("us-east-1a", "use1-az1"))
        self.assertEqual(preflight.evaluate_az_mapping({"AvailabilityZones": []}, duplicate).state, "UNKNOWN")
        clear = preflight.evaluate_vpc_overlap({"Vpcs": []}, "10.50.0.0/16", {"project": "x", "stage": "direct"})
        self.assertEqual(clear.facts, {"cidr": "10.50.0.0/16", "overlaps": ()})
        overlap = preflight.evaluate_vpc_overlap(
            {"Vpcs": [{"VpcId": "vpc-x", "CidrBlock": "10.50.1.0/24", "Tags": []}]},
            "10.50.0.0/16", {"project": "x", "stage": "direct"},
        )
        self.assertEqual((overlap.state, overlap.reason_code), ("FAIL", "VPC_OVERLAP"))
        self.assertNotIn("vpc-x", repr(overlap.facts))

    def test_malformed_owned_vpcs_fail_closed(self) -> None:
        owned_tags = [
            {"Key": "project", "Value": "hyperscale-agent-meta-harness"},
            {"Key": "stage", "Value": "direct"},
        ]
        malformed = [
            {"VpcId": "vpc-owned", "Tags": owned_tags},
            {"VpcId": "vpc-owned", "CidrBlock": "not-a-cidr", "Tags": owned_tags},
            {
                "VpcId": "vpc-owned",
                "CidrBlock": "10.50.0.0/16",
                "Tags": owned_tags,
                "CidrBlockAssociationSet": [{"CidrBlock": 7}],
            },
        ]
        for vpc in malformed:
            with self.subTest(vpc=vpc):
                result = preflight.evaluate_vpc_overlap(
                    {"Vpcs": [vpc]}, self.context.wanted_cidr, dict(self.context.owned_tags),
                )
                self.assertEqual((result.state, result.reason_code), ("UNKNOWN", "MALFORMED_RESPONSE"))
                recorder = deploy_fakes.FakeAwsRecorder({("ec2", "describe-vpcs"): {"Vpcs": [vpc]}})
                registration = self.registration()
                reader = preflight.AwsReader(self.context, registration, aws_bin="aws", parent_environment={})
                output = io.StringIO()
                with mock.patch.object(preflight.subprocess, "run", side_effect=recorder), redirect_stdout(output):
                    results = preflight.run_checks(self.context, registration, reader)
                    rc = preflight._print_results(
                        self.context, results, expected_count=len(registration.checks),
                    )
                self.assertEqual(rc, 1)
                self.assertEqual(output.getvalue().splitlines()[-1], "DEPLOY-PREFLIGHT NOT-READY stage=direct")

    def test_reader_maps_transport_results_without_raw_output(self) -> None:
        cases = [
            (FileNotFoundError(), "CLI_MISSING"),
            (subprocess.TimeoutExpired(["secret-command"], 30, output="secret-output", stderr="secret-error"), "TIMEOUT"),
            (subprocess.CompletedProcess([], 254, "", "AccessDenied secret-error"), "ACCESS_DENIED"),
            (subprocess.CompletedProcess([], 3, "secret-output", "secret-error"), "AWS_ERROR"),
            (subprocess.CompletedProcess([], 0, "", ""), "MALFORMED_RESPONSE"),
            (subprocess.CompletedProcess([], 0, "{", ""), "MALFORMED_RESPONSE"),
        ]
        registration = self.registration()
        for response, reason in cases:
            with self.subTest(reason=reason):
                reader = preflight.AwsReader(self.context, registration, aws_bin="aws", parent_environment={})
                patch = (mock.patch.object(preflight.subprocess, "run", side_effect=response)
                         if isinstance(response, BaseException)
                         else mock.patch.object(preflight.subprocess, "run", return_value=response))
                with patch:
                    result = reader.read("caller-identity")
                self.assertEqual((result.state, result.reason_code), ("UNKNOWN", reason))
                self.assertNotIn("secret", repr(result))

    def test_reader_rejects_invalid_utf8_in_an_ignored_field(self) -> None:
        registration = self.registration()
        reader = preflight.AwsReader(self.context, registration, aws_bin="aws", parent_environment={})
        raw = b'{"Account":"' + deploy_fakes.TEST_ACCOUNT.encode("ascii") + b'","ignored":"\xff"}'

        def invalid_utf8(_argv, **kwargs):
            self.assertEqual(kwargs["errors"], "strict")
            raise UnicodeDecodeError("utf-8", raw, raw.index(b"\xff"), raw.index(b"\xff") + 1, "invalid byte")

        with mock.patch.object(preflight.subprocess, "run", side_effect=invalid_utf8):
            result = reader.read("caller-identity")
        self.assertEqual((result.state, result.reason_code), ("UNKNOWN", "MALFORMED_RESPONSE"))
        self.assertNotIn(deploy_fakes.TEST_ACCOUNT, repr(result))

    def test_reader_rejects_nonstandard_json_constants_in_ignored_fields(self) -> None:
        registration = self.registration()
        for constant in ("NaN", "Infinity", "-Infinity"):
            with self.subTest(constant=constant):
                response = subprocess.CompletedProcess(
                    [], 0, f'{{"Account":"{deploy_fakes.TEST_ACCOUNT}","ignored":{constant}}}', "",
                )
                reader = preflight.AwsReader(self.context, registration, aws_bin="aws", parent_environment={})
                output = io.StringIO()
                with mock.patch.object(preflight.subprocess, "run", return_value=response), redirect_stdout(output):
                    results = preflight.run_checks(self.context, registration, reader)
                    rc = preflight._print_results(
                        self.context, results, expected_count=len(registration.checks),
                    )
                self.assertEqual(rc, 1)
                self.assertEqual(
                    [(result.state, result.reason_code) for result in results],
                    [("UNKNOWN", "MALFORMED_RESPONSE")],
                )
                self.assertNotIn("READY stage=direct region=", output.getvalue())
                self.assertEqual(output.getvalue().splitlines()[-1], "DEPLOY-PREFLIGHT NOT-READY stage=direct")

    def test_cli_output_is_deterministic_and_redacted(self) -> None:
        target = aws_target.Target(
            account=deploy_fakes.TEST_ACCOUNT, region="us-east-1", profile="private-profile", role="active",
            azs={"direct": ["us-east-1a"]}, az_ids={"direct": ["use1-az1"]}, invocation_logging="read-only",
        )
        recorder = deploy_fakes.FakeAwsRecorder()
        out, err = io.StringIO(), io.StringIO()
        with mock.patch.object(preflight.aws_target, "select", return_value=target), \
             mock.patch.object(preflight.subprocess, "run", side_effect=recorder), \
             redirect_stdout(out), redirect_stderr(err):
            rc = preflight.main(["--stage", "direct"], environ={"SECRET_VALUE": "do-not-print"}, aws_bin="aws")
        self.assertEqual(rc, 0, err.getvalue())
        masked = "********" + deploy_fakes.TEST_ACCOUNT[-4:]
        self.assertEqual(out.getvalue().splitlines(), [
            f"DEPLOY-PREFLIGHT PASS CALLER_ACCOUNT ACCOUNT_MATCH account={masked}",
            "DEPLOY-PREFLIGHT PASS AZ_MAPPING AZ_MAPPING_MATCH",
            "DEPLOY-PREFLIGHT PASS VPC_OVERLAP VPC_CLEAR",
            f"DEPLOY-PREFLIGHT READY stage=direct region=us-east-1 account={masked}",
        ])
        text = out.getvalue() + err.getvalue()
        self.assertNotIn(deploy_fakes.TEST_ACCOUNT, text)
        self.assertNotIn("private-profile", text)
        self.assertNotIn("do-not-print", text)

    def test_engine_check_results_use_the_complete_registered_count(self) -> None:
        for state, expected_rc, final_word in (("PASS", 0, "READY"), ("FAIL", 1, "NOT-READY")):
            with self.subTest(state=state):
                engine = preflight.CheckSpec(
                    "ENGINE_SAMPLE",
                    (),
                    lambda _context, _reads, state=state: preflight.CheckResult(
                        "ENGINE_SAMPLE", state, "ENGINE_RESULT", MappingProxyType({}),
                    ),
                )
                registration = self.registration(engine=(engine,))
                reader = preflight.AwsReader(self.context, registration, aws_bin="aws", parent_environment={})
                recorder = deploy_fakes.FakeAwsRecorder()
                output = io.StringIO()
                with mock.patch.object(preflight.subprocess, "run", side_effect=recorder), redirect_stdout(output):
                    results = preflight.run_checks(self.context, registration, reader)
                    rc = preflight._print_results(
                        self.context, results, expected_count=len(registration.checks),
                    )
                self.assertEqual(rc, expected_rc)
                self.assertIn(f"DEPLOY-PREFLIGHT {state} ENGINE_SAMPLE ENGINE_RESULT", output.getvalue())
                self.assertTrue(output.getvalue().splitlines()[-1].startswith(f"DEPLOY-PREFLIGHT {final_word}"))

    def test_usage_and_target_errors_launch_nothing(self) -> None:
        for argv in ([], ["--stage", "direct", "extra"], ["--stage", "direct", "--stage", "direct"]):
            with self.subTest(argv=argv), mock.patch.object(preflight.subprocess, "run") as run, \
                 redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
                self.assertEqual(preflight.main(argv, environ={}, aws_bin="aws"), 2)
                run.assert_not_called()
        with mock.patch.object(preflight.aws_target, "select", side_effect=aws_target.TargetError("secret path")), \
             mock.patch.object(preflight.subprocess, "run") as run, redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
            self.assertEqual(preflight.main(["--stage", "direct"], environ={}, aws_bin="aws"), 2)
            run.assert_not_called()

    # ----: the caller check follows the deploy's -------------------------------------------------------------------------------------------------------

    def test_b835_a_caller_that_is_not_the_entrys_operator_is_not_ready(self) -> None:
        # The deploy refuses a caller of the right account who is neither the operator the entry names nor the setup runner role.
        # The preflight reports the deploy's own checks, so it must not say READY for that caller, and it stops
        # before the EC2 reads as the deploy does. It reads the ARN of the same caller-identity answer: no other AWS read.
        account = deploy_fakes.TEST_ACCOUNT
        context = dataclasses.replace(self.context, operator=aws_target.Operator("role", "StageOperator"))
        registration = preflight.register(context, preflight.COMMON_READS, preflight.COMMON_CHECKS)
        cases = (
            (f"arn:aws:sts::{account}:assumed-role/SomeoneElse/session", "FAIL", "CALLER_NOT_OPERATOR", 1),
            (None, "FAIL", "CALLER_NOT_OPERATOR", 1),
            (f"arn:aws:iam::{account}:user/StageOperator", "FAIL", "CALLER_NOT_OPERATOR", 1),
            (f"arn:aws:sts::{account}:assumed-role/StageOperator/session", "PASS", "ACCOUNT_MATCH", 3),
            (f"arn:aws:sts::{account}:assumed-role/{aws_target.SETUP_RUNNER_ROLE}/build", "PASS", "ACCOUNT_MATCH", 3),
        )
        for arn, state, reason, reads in cases:
            with self.subTest(arn=arn):
                identity = {"Account": account} if arn is None else {"Account": account, "Arn": arn}
                recorder = deploy_fakes.FakeAwsRecorder({("sts", "get-caller-identity"): identity})
                reader = preflight.AwsReader(context, registration, aws_bin="aws", parent_environment={})
                output = io.StringIO()
                with mock.patch.object(preflight.subprocess, "run", side_effect=recorder), redirect_stdout(output):
                    results = preflight.run_checks(context, registration, reader)
                    rc = preflight._print_results(context, results, expected_count=len(registration.checks))
                self.assertEqual((results[0].check_id, results[0].state, results[0].reason_code), ("CALLER_ACCOUNT", state, reason))
                self.assertEqual(len(recorder.calls), reads)
                self.assertEqual(rc, 0 if state == "PASS" else 1)
                self.assertNotIn(account, output.getvalue())
                self.assertNotIn("SomeoneElse", output.getvalue())
        # an entry without an operator is checked by account alone, as the deploy does
        recorder = deploy_fakes.FakeAwsRecorder({("sts", "get-caller-identity"): {"Account": account, "Arn": f"arn:aws:sts::{account}:assumed-role/Any/s"}})
        registration = self.registration()
        reader = preflight.AwsReader(self.context, registration, aws_bin="aws", parent_environment={})
        with mock.patch.object(preflight.subprocess, "run", side_effect=recorder):
            self.assertEqual([r.state for r in preflight.run_checks(self.context, registration, reader)], ["PASS", "PASS", "PASS"])

    def test_b835_main_checks_the_operator_of_the_selected_entry(self) -> None:
        account = deploy_fakes.TEST_ACCOUNT
        target = aws_target.Target(
            account=account, region="us-east-1", profile="private-profile", role="active",
            azs={"direct": ["us-east-1a"]}, az_ids={"direct": ["use1-az1"]}, invocation_logging="read-only",
            operator=aws_target.Operator("role", "StageOperator"),
        )
        recorder = deploy_fakes.FakeAwsRecorder({
            ("sts", "get-caller-identity"): {"Account": account, "Arn": f"arn:aws:sts::{account}:assumed-role/SomeoneElse/session"},
        })
        out, err = io.StringIO(), io.StringIO()
        with mock.patch.object(preflight.aws_target, "select", return_value=target), \
             mock.patch.object(preflight.subprocess, "run", side_effect=recorder), \
             redirect_stdout(out), redirect_stderr(err):
            rc = preflight.main(["--stage", "direct"], environ={}, aws_bin="aws")
        self.assertEqual(rc, 1, err.getvalue())
        masked = "********" + account[-4:]
        self.assertEqual(out.getvalue().splitlines(), [
            f"DEPLOY-PREFLIGHT FAIL CALLER_ACCOUNT CALLER_NOT_OPERATOR account={masked}",
            "DEPLOY-PREFLIGHT NOT-READY stage=direct",
        ])
        self.assertEqual(len(recorder.calls), 1)



def _own_aws_variable(name: str) -> bool:
    """Whether the variable `name` is one the readiness preflight reads from its caller: the AWS CLI's own, the deploy's, and the accounts-file and AWS-profile selectors of the product."""
    return name.startswith(("AWS_", "DEPLOY_")) or name.split("_")[:2] in (["HAMH", "AWS"], ["HAMH", "ACCOUNTS"])


class ScriptCase(unittest.TestCase):
    """The user path end to end: scripts/deploy-preflight.sh with a placeholder accounts file (HAMH_ACCOUNTS_FILE) and a stand-in AWS CLI
    (DEPLOY_AWS_BIN). No AWS call is made."""

    def run_script(self, tmp: Path, answers: dict) -> subprocess.CompletedProcess:
        fake = tmp / "aws"
        if not fake.exists():
            fake.write_text(FAKE_AWS.format(python=sys.executable), encoding="utf-8")
            fake.chmod(0o755)
        env = {k: v for k, v in os.environ.items() if not _own_aws_variable(k)}
        env.update(HAMH_ACCOUNTS_FILE=str(accounts_fixture.write_accounts(tmp)), HAMH_AWS_PROFILE=accounts_fixture.ACTIVE["profile"],
                   DEPLOY_AWS_BIN=str(fake), FAKE_AWS_LOG=str(tmp / "aws.log"), FAKE_AWS_ANSWERS=json.dumps(answers), PYTHONDONTWRITEBYTECODE="1")
        return subprocess.run(["bash", str(SCRIPT), "--stage", "direct"], capture_output=True, text=True, env=env, timeout=120)  # nosec B603: the script under test with an argument list, no shell; B607: bash looked up on PATH, as the scripts under test do

    def calls(self, tmp: Path) -> list[dict]:
        log = tmp / "aws.log"
        return [json.loads(line) for line in log.read_text(encoding="utf-8").splitlines()] if log.exists() else []

    def test_b835_the_script_reads_the_selected_entry_and_says_ready(self) -> None:
        active = accounts_fixture.ACTIVE
        answers = {
            "sts get-caller-identity": {"Account": active["account"], "Arn": f"arn:aws:sts::{active['account']}:assumed-role/Any/session"},
            "ec2 describe-availability-zones": {"AvailabilityZones": [{"ZoneName": "us-east-1b", "ZoneId": "use1-az1"}]},
            "ec2 describe-vpcs": {"Vpcs": [{"VpcId": "vpc-default", "CidrBlock": "172.31.0.0/16"}]},
        }
        with tempfile.TemporaryDirectory(prefix="preflight-script-") as raw:
            tmp = Path(raw)
            proc = self.run_script(tmp, answers)
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertEqual(proc.stdout.splitlines()[-1],
                             f"DEPLOY-PREFLIGHT READY stage=direct region=us-east-1 account=********{active['account'][-4:]}")
            calls = self.calls(tmp)
        self.assertEqual([c["argv"] for c in calls], [
            ["sts", "get-caller-identity", "--region", "us-east-1", "--output", "json"],
            ["ec2", "describe-availability-zones", "--zone-names", "us-east-1b", "--region", "us-east-1", "--output", "json"],
            ["ec2", "describe-vpcs", "--region", "us-east-1", "--output", "json"],
        ])
        self.assertEqual({c["profile"] for c in calls}, {active["profile"]})
        self.assertNotIn(active["account"], proc.stdout + proc.stderr)

    def test_b835_the_script_stops_after_another_account_and_exits_1(self) -> None:
        answers = {"sts get-caller-identity": {"Account": accounts_fixture.TEST["account"]}}
        with tempfile.TemporaryDirectory(prefix="preflight-script-") as raw:
            tmp = Path(raw)
            proc = self.run_script(tmp, answers)
            self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
            self.assertEqual(proc.stdout.splitlines(), [
                "DEPLOY-PREFLIGHT FAIL CALLER_ACCOUNT ACCOUNT_MISMATCH account=<unavailable>",
                "DEPLOY-PREFLIGHT NOT-READY stage=direct",
            ])
            self.assertEqual(len(self.calls(tmp)), 1)

    def test_b835_the_script_refuses_other_arguments_before_any_read(self) -> None:
        with tempfile.TemporaryDirectory(prefix="preflight-script-") as raw:
            tmp = Path(raw)
            env_answers: dict = {}
            fake = tmp / "aws"
            fake.write_text(FAKE_AWS.format(python=sys.executable), encoding="utf-8")
            fake.chmod(0o755)
            env = {k: v for k, v in os.environ.items() if not _own_aws_variable(k)}
            env.update(DEPLOY_AWS_BIN=str(fake), FAKE_AWS_LOG=str(tmp / "aws.log"), FAKE_AWS_ANSWERS=json.dumps(env_answers), PYTHONDONTWRITEBYTECODE="1")
            proc = subprocess.run(["bash", str(SCRIPT), "--stage", "live"], capture_output=True, text=True, env=env, timeout=120)  # nosec B603: the script under test with an argument list, no shell; B607: bash looked up on PATH, as the scripts under test do
            self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
            # The USAGE line is the last line of standard output, as the script's header says.
            self.assertEqual(proc.stdout.splitlines()[-1:], ["DEPLOY-PREFLIGHT USAGE arguments"])
            self.assertEqual(self.calls(tmp), [])

    def test_b835_a_machine_without_python_3_11_ends_with_the_usage_line(self) -> None:
        # No interpreter at all, and only a Python 3.9 (the macOS system python3): exit 2 with the USAGE line, and the reason on standard error.
        bodies = (("no-python", "exit 1"), ("python-3.9", 'case "$2" in *"(3, 8)"*) exit 0 ;; *) exit 1 ;; esac'))
        for name, body in bodies:
            with self.subTest(name=name), tempfile.TemporaryDirectory(prefix="preflight-python-") as raw:
                tmp = Path(raw)
                bin_dir = tmp / "bin"
                bin_dir.mkdir()
                (bin_dir / "dirname").symlink_to(shutil.which("dirname"))
                fake = bin_dir / "python3"
                fake.write_text("#!/bin/sh\n" + body + "\n", encoding="utf-8")
                fake.chmod(0o755)
                env = {"PATH": str(bin_dir), "HOME": str(tmp), "HAMH_NO_DEPRECATION_NOTICES": "1"}
                proc = subprocess.run([shutil.which("bash"), str(SCRIPT), "--stage", "direct"], capture_output=True, text=True, env=env, timeout=120)  # nosec B603: the script under test with an argument list, no shell
                self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
                self.assertEqual(proc.stdout.splitlines(), ["DEPLOY-PREFLIGHT USAGE python"])
                self.assertIn("no Python 3.", proc.stderr)


if __name__ == "__main__":
    unittest.main()
