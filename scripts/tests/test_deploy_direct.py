"""scripts/deploy.sh --stage direct.

Stage `direct` is the one stage: it deploys the worker image, three Lambdas (worker-api, control-api, run-session) and the delivery image, in a
VPC of its own. Before it deploys anything, the CIDR it will create must overlap no other VPC of the account. These tests pin the data the
deploy keys on to infra/lib/config.ts, the build lists, the CIDR check and the usage; the stack-by-stack flow is the one test_deploy.py
exercises.
"""

from __future__ import annotations

import codecs
import json
import os
import re
import subprocess  # nosec B404: tests start the scripts under test and git with argument lists, never a shell
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "scripts" / "lib"))
import aws_target  # noqa: E402
import deploy  # noqa: E402

CONFIG_TS = (REPO / "infra" / "lib" / "config.ts").read_text()


def ts_stage_value(stage: str, key: str) -> str:
    """The `key: "value"` of the stageConfig case for `stage` in infra/lib/config.ts."""
    block = re.search(rf'case "{stage}":(.*?)(?:case "|default:)', CONFIG_TS, re.S)
    assert block, stage
    m = re.search(rf'{key}: "([^"]+)"', block.group(1))
    assert m, (stage, key)
    return m.group(1)


def ts_list(name: str) -> list[str]:
    m = re.search(rf"export const {name}(?::[^=]*)? = \[(.*?)\]", CONFIG_TS, re.S)
    assert m, name
    return re.findall(r'"([^"]+)"', m.group(1))


class StageData(unittest.TestCase):
    def test_the_vpc_cidr_is_the_one_infra_config_ts_creates(self) -> None:
        self.assertEqual(deploy.VPC_CIDRS, {"direct": ts_stage_value("direct", "vpcCidr")})

    def test_b835_the_preflight_checks_the_range_and_the_tags_the_deploy_checks(self) -> None:
        # deploy_preflight.py cannot import deploy.py (deploy.py imports it), so its copies of the stage range and the project tag are pinned here.
        self.assertEqual(deploy.deploy_preflight.VPC_CIDR, deploy.VPC_CIDRS["direct"])
        self.assertEqual(deploy.deploy_preflight.PROJECT, deploy.PROJECT)

    def test_the_az_count_follows_accounts_json(self) -> None:
        for t in aws_target.load(REPO / "accounts.json"):
            for stage, n in deploy.AZS.items():
                self.assertEqual(len(t.azs[stage]), n, f"{t.profile} {stage}")
        self.assertEqual(list(deploy.AZS), ["direct"])

    def test_the_lambda_lists_are_infra_config_ts_lists(self) -> None:
        self.assertEqual(list(deploy.LAMBDAS), ts_list("DIRECT_LAMBDAS"))
        # The research package builds two functions from two binaries: every other package is one binary of the same name.
        self.assertEqual(list(deploy.LAMBDA_PACKAGES), ["worker-api", "control-api", "run-session", "research-api"])
        self.assertEqual([n for n in deploy.LAMBDAS if n not in deploy.LAMBDA_PACKAGES], ["research-job"])

    def test_a_direct_stage_has_one_worker_image_and_no_other_runtime_images(self) -> None:
        d = deploy.Deployer("direct")
        for gone in ("runtime_images", "images", "direct", "fixtures", "catalog_bin", "policy_bin", "ladder_bin", "gomod_bin", "lane_b_script"):
            self.assertFalse(hasattr(d, gone), gone)
        for gone in ("RUNTIME_IMAGES", "family_images", "LANE_B_SCRIPT", "MODEL_LISTS", "RUNG_LIST"):
            self.assertFalse(hasattr(deploy, gone), gone)
        self.assertEqual(deploy.WORKER_BINARIES, (("agentd", "AGENTD_SHA256"),))
        self.assertEqual(deploy.IMAGE_ENV, ("HAMH_WORKER_IMAGE", "HAMH_DELIVERY_IMAGE"))

    def test_the_direct_stage_names(self) -> None:
        d = deploy.Deployer("direct")
        self.assertEqual(d.prefix, "hamh-direct")
        self.assertEqual(d.bucket, f"hamh-direct-harness-{d.account}")
        self.assertEqual(d.repo_name, "hamh-direct-worker-python")
        self.assertEqual(d.delivery_repo, "hamh-direct-delivery")

    def test_the_fork_artifact_is_copied_from_the_account_that_holds_it(self) -> None:
        d = deploy.Deployer("direct")
        # the engine pin names a bucket of the retired live stage, which had the previous names
        live = d.artifact_owner(f"s3://cah-live-harness-{d.account}/engine/cline/abc/cline")  # COMPAT-CAH-STAGE
        self.assertIsNotNone(live, "a direct deploy copies the promoted binary of the account's retired live stage")
        self.assertIsNone(d.artifact_owner(f"s3://hamh-direct-harness-{d.account}/engine/cline/abc/cline"), "its own bucket has nothing to copy")


class VpcCidr(unittest.TestCase):
    def check(self, vpcs: list[dict]):
        d = deploy.Deployer("direct")
        with mock.patch.object(deploy.Deployer, "aws", return_value={"Vpcs": vpcs}) as aws:
            out = d.check_vpc_cidr()
        aws.assert_called_once_with("ec2", "describe-vpcs")
        return out

    def vpc(self, cidr: str, vpc_id: str = "vpc-1", tags: dict | None = None, extra: list[str] | None = None) -> dict:
        v = {"VpcId": vpc_id, "CidrBlock": cidr,
             "Tags": [{"Key": k, "Value": v} for k, v in (tags or {}).items()]}
        if extra:
            v["CidrBlockAssociationSet"] = [{"CidrBlock": cidr}, *({"CidrBlock": c} for c in extra)]
        return v

    def test_an_account_with_no_overlap_passes(self) -> None:
        self.assertEqual(self.check([]), {"cidr": "10.50.0.0/16", "overlaps": []})
        self.check([self.vpc("172.31.0.0/16", "vpc-default")])

    def test_the_vpcs_of_the_retired_stages_do_not_stop_a_direct_deploy(self) -> None:
        project = {"project": deploy.PROJECT}
        self.check([self.vpc("10.40.0.0/16", "vpc-live", {**project, "stage": "live"}),
                    self.vpc("10.41.0.0/16", "vpc-drill", {**project, "stage": "drill"})])

    def test_the_stages_own_vpc_is_not_an_overlap(self) -> None:
        self.check([self.vpc("10.50.0.0/16", "vpc-own", {"project": deploy.PROJECT, "stage": "direct"})])

    def test_another_vpc_in_the_range_is_refused_without_vpc_details(self) -> None:
        for cidr in ("10.50.0.0/16", "10.50.4.0/24", "10.0.0.0/8", "10.48.0.0/14"):
            document = {"Vpcs": [self.vpc(cidr, "vpc-other")]}
            result = deploy.deploy_preflight.evaluate_vpc_overlap(
                document,
                deploy.VPC_CIDRS["direct"],
                {"project": deploy.PROJECT, "stage": "direct"},
            )
            self.assertEqual((result.state, result.reason_code), ("FAIL", "VPC_OVERLAP"))
            self.assertNotIn("vpc-other", str(dict(result.facts)))
            with self.assertRaises(deploy.Fail, msg=cidr) as cm:
                self.check(document["Vpcs"])
            self.assertEqual(str(cm.exception), f"vpc-cidr 10.50.0.0/16 overlaps vpc-other={cidr}")

    def test_a_secondary_cidr_block_counts(self) -> None:
        with self.assertRaises(deploy.Fail) as cm:
            self.check([self.vpc("172.31.0.0/16", "vpc-two", extra=["10.50.128.0/17"])])
        self.assertEqual(str(cm.exception), "vpc-cidr 10.50.0.0/16 overlaps vpc-two=10.50.128.0/17")

    def test_malformed_owned_vpcs_fail_before_the_owned_stage_exemption(self) -> None:
        tags = {"project": deploy.PROJECT, "stage": "direct"}
        for vpc in (
            {"VpcId": "vpc-owned", "Tags": [{"Key": k, "Value": v} for k, v in tags.items()]},
            self.vpc("not-a-cidr", "vpc-owned", tags),
            self.vpc("10.50.0.0/16", "vpc-owned", tags, extra=[7]),
        ):
            with self.subTest(vpc=vpc), self.assertRaisesRegex(deploy.Fail, "^vpc-cidr unavailable$"):
                self.check([vpc])

    def test_the_previous_stage_is_refused_without_vpc_details(self) -> None:
        tags = {"project": deploy.LEGACY_PROJECT, "stage": "direct"}
        document = {"Vpcs": [self.vpc("10.50.0.0/16", "vpc-old", tags)]}
        result = deploy.deploy_preflight.evaluate_vpc_overlap(
            document,
            deploy.VPC_CIDRS["direct"],
            {"project": deploy.PROJECT, "stage": "direct"},
        )
        self.assertEqual((result.state, result.reason_code), ("FAIL", "VPC_OVERLAP"))
        self.assertNotIn("vpc-old", str(dict(result.facts)))
        with self.assertRaises(deploy.Fail) as raised:
            self.check(document["Vpcs"])
        text = str(raised.exception)
        self.assertIn("stage under the previous names", text)
        self.assertIn("vpc-old", text)
        self.assertIn("10.50.0.0/16", text)

    def test_a_vpc_of_the_project_but_another_stage_is_not_excused_when_it_overlaps(self) -> None:
        with self.assertRaises(deploy.Fail):
            self.check([self.vpc("10.50.0.0/16", "vpc-x", {"project": deploy.PROJECT, "stage": "live"})])

    def test_a_tag_without_the_project_does_not_excuse(self) -> None:
        with self.assertRaises(deploy.Fail):
            self.check([self.vpc("10.50.0.0/16", "vpc-x", {"stage": "direct"})])

    def test_b835_a_refusal_names_the_other_vpcs_and_never_the_stages_own(self) -> None:
        # The stage's own tagged VPC is never an overlap, so the refusal that another VPC causes names that VPC alone, as the
        # deploy did before the readiness preflight. Naming the stage's own VPC would ask the reader to free a range that the stage itself holds.
        own = self.vpc("10.50.0.0/16", "vpc-own", {"project": deploy.PROJECT, "stage": "direct"})
        with self.assertRaises(deploy.Fail) as cm:
            self.check([own, self.vpc("10.50.4.0/24", "vpc-other")])
        self.assertEqual(str(cm.exception), "vpc-cidr 10.50.0.0/16 overlaps vpc-other=10.50.4.0/24")
        old = self.vpc("10.50.0.0/16", "vpc-old", {"project": deploy.LEGACY_PROJECT, "stage": "direct"})
        with self.assertRaises(deploy.Fail) as cm:
            self.check([own, old])
        self.assertIn("overlaps vpc-old=10.50.0.0/16: this account still has the stage under the previous names", str(cm.exception))
        self.assertNotIn("vpc-own", str(cm.exception))

    def test_b835_a_vpc_answer_the_check_cannot_read_is_refused_with_a_sentence_and_an_action(self) -> None:
        # The detail `unavailable` alone made the line `DEPLOY-REFUSED reason=vpc-cidr ... :: unavailable`, which says neither what was found
        # nor what to do. The detail stays `unavailable`; the refusal line says what was read and what to do.
        for vpcs in ([{"VpcId": "vpc-x"}], [self.vpc("not-a-cidr", "vpc-x")]):
            with self.subTest(vpcs=vpcs):
                with self.assertRaises(deploy.Fail) as cm:
                    self.check(vpcs)
                self.assertEqual(str(cm.exception), "vpc-cidr unavailable")
                self.assertEqual(deploy.refusal_line(cm.exception),
                                 "DEPLOY-REFUSED reason=vpc-cidr wait=no stage=unchanged :: the answer of `aws ec2 describe-vpcs` has a shape this deploy "
                                 "cannot check. Check that the AWS CLI of this machine answers `aws ec2 describe-vpcs` as the EC2 API documents it, "
                                 "then deploy again.")


class AzCheck(unittest.TestCase):
    def test_b835_an_az_answer_the_check_cannot_read_is_refused_with_a_sentence_and_an_action(self) -> None:
        # An answer that names every zone with its ID and is still not one row per zone (a zone twice) is no mismatch the
        # refusal can name; it left the detail ` (accounts.json:)`. It is refused as an answer this deploy cannot check, with what to do.
        d = deploy.Deployer("direct")
        rows = [{"ZoneName": n, "ZoneId": i} for n, i in zip(d.target.azs["direct"], d.target.az_ids["direct"])]
        with mock.patch.object(deploy.Deployer, "aws", return_value={"AvailabilityZones": rows + rows}):
            with self.assertRaises(deploy.Fail) as cm:
                d.check_azs()
        self.assertEqual(str(cm.exception), "azs unavailable")
        self.assertEqual(deploy.refusal_line(cm.exception),
                         "DEPLOY-REFUSED reason=azs wait=no stage=unchanged :: the answer of `aws ec2 describe-availability-zones` has a shape this "
                         "deploy cannot check. Check that the AWS CLI of this machine answers `aws ec2 describe-availability-zones` as the EC2 API "
                         "documents it, then deploy again.")

    def test_b835_a_zone_that_maps_to_another_id_still_names_the_zone_and_both_ids(self) -> None:
        # The mismatch refusal of a well-formed answer is the deploy's own and stays as it was (regression of the fit above).
        d = deploy.Deployer("direct")
        names, ids = d.target.azs["direct"], d.target.az_ids["direct"]
        rows = [{"ZoneName": n, "ZoneId": "xxx-az9"} for n in names]
        with mock.patch.object(deploy.Deployer, "aws", return_value={"AvailabilityZones": rows}):
            with self.assertRaises(deploy.Fail) as cm:
                d.check_azs()
        self.assertEqual(str(cm.exception), "azs " + " ".join(f"{n}=xxx-az9" for n in names) + f" (accounts.json: {' '.join(ids)})")


class DeployRunsTheCidrCheck(unittest.TestCase):
    """review S5 (py_vpc_check_off): the check was tested alone; `deploy()` must call it before it reads or builds anything."""

    class Stop(Exception):
        pass

    def run_deploy(self, vpc_effect=None):
        d = deploy.Deployer("direct")
        order: list[str] = []
        stop = self.Stop()

        def step(name, value=None, effect=None):
            def f(*a, **k):
                order.append(name)
                if effect is not None:
                    raise effect
                return value
            return f

        patches = [
            mock.patch.object(deploy.Deployer, "git", return_value="a" * 40),
            mock.patch.object(deploy.digests, "source_tree", return_value="b" * 40),
            mock.patch.object(deploy.Deployer, "check_account", side_effect=step("account")),
            mock.patch.object(deploy.Deployer, "check_azs", side_effect=step("azs", [])),
            mock.patch.object(deploy.Deployer, "check_vpc_cidr", side_effect=step("vpc", {"cidr": deploy.VPC_CIDRS["direct"]}, vpc_effect)),
            mock.patch.object(deploy.Deployer, "aws", side_effect=step("ssm", effect=stop)),
        ]
        for p in patches:
            p.start()
            self.addCleanup(p.stop)
        try:
            d.deploy()
        except (self.Stop, deploy.Fail) as e:
            return order, e
        raise AssertionError("deploy() went past the first reads")

    def test_a_deploy_checks_the_cidr_after_the_account_and_the_azs_and_before_anything_is_read(self) -> None:
        order, err = self.run_deploy()
        self.assertEqual(order, ["account", "azs", "vpc", "ssm"])
        self.assertIsInstance(err, self.Stop)

    def test_an_overlapping_vpc_ends_a_deploy_before_the_first_read(self) -> None:
        order, err = self.run_deploy(deploy.Fail("vpc-cidr 10.50.0.0/16 overlaps vpc-other"))
        self.assertEqual(order, ["account", "azs", "vpc"])
        self.assertTrue(str(err).startswith("vpc-cidr "), str(err))


class EndpointReady(unittest.TestCase):
    def setUp(self) -> None:
        self.deployer = deploy.Deployer("direct")
        self.endpoint = f"https://abc123.execute-api.{self.deployer.region}.amazonaws.com/direct"
        # A lookup that cannot be read is tried again after a wait; the cases record the waits and do not sleep.
        patcher = mock.patch.object(deploy.time, "sleep")
        self.sleep = patcher.start()
        self.addCleanup(patcher.stop)

    def test_production_endpoint_lookup_timeout_is_30_seconds(self) -> None:
        self.assertEqual(deploy.ENDPOINT_LOOKUP_TIMEOUT_S, 30)

    def test_run_threads_optional_timeout_to_capture_and_stream_calls(self) -> None:
        completed = subprocess.CompletedProcess([], 0, "", "")
        for stream in (False, True):
            with self.subTest(stream=stream), mock.patch.object(deploy.subprocess, "run", return_value=completed) as run:
                self.deployer.run(["tool"], stream=stream, timeout=1.25)
                self.assertEqual(run.call_args.kwargs["timeout"], 1.25)
                self.assertEqual(run.call_args.kwargs["encoding"], "utf-8")
                self.assertEqual(run.call_args.kwargs["errors"], "strict")

    def test_endpoint_validator_accepts_only_the_stage_execute_api_url(self) -> None:
        rejected = [
            "http" + self.endpoint[5:],
            self.endpoint + "?query=1",
            self.endpoint + "#fragment",
            self.endpoint.replace("/direct", "/other"),
            self.endpoint.replace("abc123", "ABC123"),
            self.endpoint.replace("amazonaws.com", "amazonaws.com.evil.invalid"),
            self.endpoint.replace("abc123", "user@abc123"),
            self.endpoint.replace("/direct", ":443/direct"),
            self.endpoint + "\n",
        ]
        with mock.patch.object(self.deployer, "outputs", return_value={"V1ApiUrl": self.endpoint}):
            self.assertTrue(self.deployer.endpoint_ready())
        for value in rejected:
            with self.subTest(value=value), mock.patch.object(self.deployer, "outputs", return_value={"V1ApiUrl": value}), \
                    self.assertRaisesRegex(deploy.Fail, "^endpoint invalid$"):
                self.deployer.endpoint_ready()

    def test_b835_an_endpoint_of_another_region_is_invalid(self) -> None:
        # The host must name the stage's own Region. A mutation that loosened the Region part of the host pattern passed the MR's tests.
        region = self.deployer.region
        for other in ("us-west-2", "eu-west-1", "us-east-1"):
            if other == region:
                continue
            value = self.endpoint.replace(f".{region}.", f".{other}.")
            with self.subTest(value=value), mock.patch.object(self.deployer, "outputs", return_value={"V1ApiUrl": value}), \
                    self.assertRaisesRegex(deploy.Fail, "^endpoint invalid$"):
                self.deployer.endpoint_ready()
        value = self.endpoint.replace(f".{region}.", ".")
        with mock.patch.object(self.deployer, "outputs", return_value={"V1ApiUrl": value}), self.assertRaisesRegex(deploy.Fail, "^endpoint invalid$"):
            self.deployer.endpoint_ready()

    def test_b835_the_lookup_is_tried_three_times_five_and_ten_seconds_apart(self) -> None:
        self.assertEqual((deploy.ENDPOINT_LOOKUP_ATTEMPTS, deploy.ENDPOINT_LOOKUP_WAIT_S), (3, 5))

    def test_b835_a_lookup_that_cannot_be_read_is_tried_again(self) -> None:
        # The read follows the stack changes; a slow or throttled describe-stacks must not fail a deploy whose stacks are all in place.
        answers = [deploy.Fail("aws", "timeout"), deploy.Fail("aws", "cloudformation describe-stacks: Rate exceeded"), {"V1ApiUrl": self.endpoint}]
        with mock.patch.object(self.deployer, "outputs", side_effect=answers) as outputs:
            self.assertTrue(self.deployer.endpoint_ready())
        self.assertEqual(outputs.call_count, 3)
        self.assertEqual([c.args[0] for c in self.sleep.call_args_list], [5, 10])
        for c in outputs.call_args_list:
            self.assertEqual(c.kwargs["timeout"], deploy.ENDPOINT_LOOKUP_TIMEOUT_S)

    def test_b835_a_lookup_that_stays_unreadable_fails_after_the_last_try(self) -> None:
        with mock.patch.object(self.deployer, "outputs", side_effect=OSError("down")) as outputs:
            with self.assertRaisesRegex(deploy.Fail, "^endpoint unavailable$"):
                self.deployer.endpoint_ready()
        self.assertEqual(outputs.call_count, 3)
        self.assertEqual(self.sleep.call_count, 2)

    def test_b835_an_answer_without_a_valid_endpoint_is_final(self) -> None:
        # A stack that names no endpoint, or a malformed one, answers the same way the next time: no second read.
        for answer, detail in (({}, "missing"), ({"V1ApiUrl": "not-a-url"}, "invalid"), (ValueError("shape"), "invalid")):
            with self.subTest(detail=detail):
                with mock.patch.object(self.deployer, "outputs", side_effect=[answer]) as outputs:
                    with self.assertRaisesRegex(deploy.Fail, f"^endpoint {detail}$"):
                        self.deployer.endpoint_ready()
                self.assertEqual(outputs.call_count, 1)
        self.sleep.assert_not_called()

    def test_outputs_reject_malformed_collections_and_duplicate_keys(self) -> None:
        row = {"OutputKey": "V1ApiUrl", "OutputValue": self.endpoint}
        malformed = (
            {"Stacks": []},
            {"Stacks": [{"Outputs": []}, {"Outputs": []}]},
            {"Stacks": [{"Outputs": {}}]},
            {"Stacks": [{"Outputs": [7]}]},
            {"Stacks": [{"Outputs": [{"OutputKey": 7, "OutputValue": self.endpoint}]}]},
            {"Stacks": [{"Outputs": [row, dict(row)]}]},
        )
        for document in malformed:
            with self.subTest(document=document), mock.patch.object(self.deployer, "aws", return_value=document), \
                    self.assertRaises(ValueError):
                self.deployer.outputs("hamh-direct-control")

    def test_endpoint_transport_and_json_failures_are_redacted(self) -> None:
        cases = (
            (subprocess.TimeoutExpired(["aws"], 30), "unavailable"),
            (UnicodeDecodeError("utf-8", b"x", 0, 1, "bad"), "unavailable"),
            (subprocess.CompletedProcess([], 0, "{", ""), "invalid"),
            (subprocess.CompletedProcess([], 0, f'{{"Stacks":[{{"Outputs":[]}}],"ignored":NaN}}', ""), "invalid"),
            (subprocess.CompletedProcess([], 0, f'{{"Stacks":[{{"Outputs":[]}}],"ignored":1e999}}', ""), "invalid"),
            (subprocess.CompletedProcess([], 0, f'{{"Stacks":[{{"Outputs":[]}}],"ignored":-1e999}}', ""), "invalid"),
        )
        for result, detail in cases:
            with self.subTest(detail=detail):
                patch = (
                    mock.patch.object(self.deployer, "run", side_effect=result)
                    if isinstance(result, BaseException)
                    else mock.patch.object(self.deployer, "run", return_value=result)
                )
                with patch, self.assertRaisesRegex(deploy.Fail, f"^endpoint {detail}$") as raised:
                    self.deployer.endpoint_ready()
                self.assertNotIn(self.endpoint, str(raised.exception))


class ImageBuild(unittest.TestCase):
    """review S3: a first deploy builds its images in the stage's own project (infra/lib/stacks/image-build.ts, held by the runtimes
    stack), through scripts/codebuild-run.sh --stage direct."""

    def test_the_delivery_image_is_built_with_the_stage_flag_of_the_deployed_stage(self) -> None:
        d = deploy.Deployer("direct")
        with tempfile.TemporaryDirectory() as tmp:
            d.lambda_dir = Path(tmp)
            for b in deploy.DELIVERY_BINS:
                (Path(tmp) / b).mkdir()
                (Path(tmp) / b / "bootstrap").write_bytes(b"x")
            cmds: list[list[str]] = []

            def run_codebuild(cmd, _cmds=cmds):
                _cmds.append(cmd)
                return 0, "hamh-direct-image-build:0001"

            ok = subprocess.CompletedProcess([], 0, "", "")
            with mock.patch.object(deploy.Deployer, "run", return_value=ok), \
                    mock.patch.object(deploy.Deployer, "run_codebuild", side_effect=run_codebuild), \
                    mock.patch.object(deploy.Deployer, "find_image", return_value=f"img@sha256:{'a' * 64}"), \
                    mock.patch.object(deploy.Deployer, "write_image_record"):
                image = d.build_delivery_image("c" * 40, "dl-tag")
        self.assertTrue(image.startswith("img@"))
        self.assertEqual(len(cmds), 1)
        cmd = cmds[0]
        self.assertEqual(cmd[cmd.index("--stage") + 1], "direct")
        self.assertEqual(cmd[cmd.index("--buildspec") + 1], "images/codebuild/delivery.yml")
        self.assertIn(f"REPO_URI={d.delivery_uri}", cmd)
        self.assertIn("hamh-direct-delivery", d.delivery_uri)

    def test_the_script_the_deployer_calls_knows_the_stage_it_deploys(self) -> None:
        text = (REPO / "scripts" / "codebuild-run.sh").read_text()
        self.assertIn("direct", re.search(r"--stage\) .*", text).group(0))
        self.assertNotIn("live", re.search(r"--stage\) .*", text).group(0))
        usage = deploy.Deployer("direct").codebuild
        self.assertTrue(usage.endswith("codebuild-run.sh"), usage)

    def test_the_project_the_script_names_is_the_one_the_runtimes_stack_creates(self) -> None:
        # `hamh-$stage-image-build` in the script, `${p}-image-build` in infra/lib/stacks/image-build.ts
        script = (REPO / "scripts" / "codebuild-run.sh").read_text()
        self.assertIn('project="hamh-$stage-image-build"', script)
        ts = (REPO / "infra" / "lib" / "stacks" / "image-build.ts").read_text()
        self.assertIn("const projectName = `${p}-image-build`;", ts)
        runtimes = (REPO / "infra" / "lib" / "stacks" / "runtimes.ts").read_text()
        self.assertRegex(runtimes, r"addImageBuild\(this, cfg, props\.security")


class WorkerImageToolchains(unittest.TestCase):
    """The stage's one worker image is the polyglot one (Go, JDK 21 and Maven beside Python and Node): the deploy asks its build
    for the `direct` language family."""

    def test_a_deploy_builds_the_worker_with_the_direct_language_family(self) -> None:
        d = deploy.Deployer("direct")
        calls: list[tuple] = []
        with mock.patch.object(deploy.Deployer, "build_binary_image", side_effect=lambda *a, **k: calls.append((a, k)) or "img"), \
                mock.patch.object(deploy.Deployer, "cline_sha256", return_value="f" * 64):
            self.assertEqual(d.build_image("c" * 40, "in-tag"), "img")
        self.assertEqual(len(calls), 1)
        spec, _sha, _tag, extra = calls[0][0]
        self.assertEqual(spec, deploy.WORKER_IMAGE)
        self.assertEqual(extra, {"CLINE_SHA256": "f" * 64, "LANG_FAMILY": "direct"})

    def test_the_image_record_names_the_family_it_built(self) -> None:
        d = deploy.Deployer("direct")
        with tempfile.TemporaryDirectory() as tmp, \
                mock.patch.dict(os.environ, {"DEPLOY_RECORD_DIR": tmp, "DEPLOY_CRITERIA": "/nonexistent"}), \
                mock.patch.object(deploy.Deployer, "aws", return_value={"imageDetails": [{"imageSizeInBytes": 1}]}), \
                mock.patch.object(deploy.Deployer, "git", return_value="c" * 40), \
                mock.patch.object(deploy.digests, "source_tree", return_value="t" * 64):
            d.fork = {"sha256": "f" * 64}
            d.write_image_record("worker", "images/codebuild/worker-python.yml", "hamh-direct-worker-python", "in-abc", f"img@sha256:{'a' * 64}",
                                 {"CLINE_SHA256": "f" * 64, "LANG_FAMILY": "direct"}, build_id="hamh-direct-image-build:1")
            rec = json.loads(next(Path(tmp).iterdir()).read_text())
        self.assertEqual(rec["inputs"]["langFamily"], "direct")

    def test_the_worker_image_is_built_from_agentd_alone(self) -> None:
        # The harness-gates binary left the image with the crate: no GATES_SHA256, one cross-built binary.
        self.assertEqual([name for name, _ in deploy.WORKER_IMAGE.binaries], ["agentd"])
        spec = (REPO / "images" / "codebuild" / "worker-python.yml").read_text()
        self.assertNotIn("GATES_SHA256", spec)
        self.assertNotIn("harness-gates", spec.replace("crates/harness-gates", ""))


class Build(unittest.TestCase):
    def test_a_stage_builds_four_lambda_packages_and_the_delivery_ones(self) -> None:
        d = deploy.Deployer("direct")
        tmp = Path(tempfile.mkdtemp(prefix="deploy-direct-"))
        d.lambda_dir = tmp / "lambda"
        cmds: list[list[str]] = []

        def fake_run(cmd, *a, **k):
            cmds.append(list(cmd))
            for name in (*deploy.LAMBDAS, *deploy.DELIVERY_BINS):
                (d.lambda_dir / name).mkdir(parents=True, exist_ok=True)
                (d.lambda_dir / name / "bootstrap").write_bytes(b"x")
            return subprocess.CompletedProcess(cmd, 0, "", "")

        with mock.patch.object(deploy.Deployer, "run", side_effect=fake_run), mock.patch.object(deploy.Deployer, "verify_build") as vb:
            d.build_lambda()
        packages = [cmds[0][i + 1] for i, a in enumerate(cmds[0]) if a == "-p"]
        verified = sorted(vb.call_args.args[1])
        self.assertEqual(packages, ["worker-api", "control-api", "run-session", "research-api", "delivery"])
        self.assertEqual(verified, sorted(["worker-api", "control-api", "run-session", "research-api", "research-job", *deploy.DELIVERY_BINS]))


class ToolsMode(unittest.TestCase):
    """The stage setting `HAMH_TOOLS_MODE` is resolved here and in infra/lib/config.ts (`resolveToolsMode`) by one table, so a word means the
    same to the script that compares it with the live runtime and to the CDK app that writes it into the runtime environment."""

    TABLE = json.loads((REPO / "infra" / "test" / "fixtures" / "tools-mode.json").read_text())

    def test_the_words_are_the_words_infra_config_ts_has(self) -> None:
        self.assertEqual(list(deploy.TOOLS_MODES), ts_list("TOOLS_MODES"))
        self.assertEqual(list(deploy.TOOLS_MODES), ["off", "on"])
        self.assertEqual(deploy.DEFAULT_TOOLS_MODE, self.TABLE["default"])
        m = re.search(r'export const DEFAULT_TOOLS_MODE(?::[^=]*)? = "([a-z]+)"', CONFIG_TS)
        self.assertIsNotNone(m)
        self.assertEqual(m.group(1), deploy.DEFAULT_TOOLS_MODE)

    def test_the_trimmed_characters_are_the_ones_infra_config_ts_trims(self) -> None:
        m = re.search(r'const TOOLS_SPACE = "([^"]*)";', CONFIG_TS)
        self.assertIsNotNone(m)
        self.assertEqual(codecs.decode(m.group(1), "unicode_escape"), deploy.TOOLS_SPACE)
        self.assertEqual(deploy.TOOLS_SPACE, " \t\n\r\f\v", "ASCII whitespace only: strip() and trim() differ on U+001C to U+001F, U+0085 and U+FEFF")

    def test_every_accepted_setting_of_the_shared_table_resolves_to_its_mode(self) -> None:
        for case in self.TABLE["accept"]:
            with self.subTest(setting=case["setting"]):
                self.assertEqual(deploy.resolve_tools_mode(case["setting"]), case["mode"])

    def test_every_rejected_setting_of_the_shared_table_raises_and_names_the_setting_and_the_words(self) -> None:
        for word in self.TABLE["reject"]:
            with self.subTest(setting=word):
                with self.assertRaises(deploy.ToolsModeError) as cm:
                    deploy.resolve_tools_mode(word)
                self.assertIn("HAMH_TOOLS_MODE", str(cm.exception))
                self.assertIn("off, on", str(cm.exception))

    def test_a_deployer_reads_the_variable_and_a_bad_word_is_a_usage_error_from_main(self) -> None:
        with mock.patch.dict(os.environ, {"HAMH_TOOLS_MODE": " On "}):
            self.assertEqual(deploy.Deployer("direct").tools_mode, "on")
            self.assertEqual(deploy.Deployer("direct").tools_setting, "On")
        with mock.patch.dict(os.environ):
            os.environ.pop("HAMH_TOOLS_MODE", None)
            d = deploy.Deployer("direct")
            self.assertEqual((d.tools_mode, d.tools_setting), ("off", None))
        with mock.patch.dict(os.environ, {"HAMH_TOOLS_MODE": "auto"}), mock.patch("sys.stderr") as err:
            self.assertEqual(deploy.main(["--stage", "direct"]), 2)
            self.assertIn("HAMH_TOOLS_MODE", "".join(str(c.args[0]) for c in err.write.call_args_list))


class ComputeInstances(unittest.TestCase):
    """The stage setting `HAMH_COMPUTE_INSTANCES` is resolved here and in infra/lib/config.ts (`resolveComputeInstances`) by one table, so a word means the
    same to the script that compares it with the live stage and to the CDK app that creates (or leaves out) the Instances capacity provider and its runtime."""

    TABLE = json.loads((REPO / "infra" / "test" / "fixtures" / "compute-instances.json").read_text())
    RUNTIMES_TS = (REPO / "infra" / "lib" / "stacks" / "runtimes.ts").read_text()

    def test_the_words_are_the_words_infra_config_ts_has(self) -> None:
        self.assertEqual(list(deploy.COMPUTE_INSTANCES_MODES), ts_list("COMPUTE_INSTANCES_MODES"))
        self.assertEqual(list(deploy.COMPUTE_INSTANCES_MODES), ["off", "on"])
        self.assertEqual(deploy.DEFAULT_COMPUTE_INSTANCES, self.TABLE["default"])
        m = re.search(r'export const DEFAULT_COMPUTE_INSTANCES(?::[^=]*)? = "([a-z]+)"', CONFIG_TS)
        self.assertIsNotNone(m)
        self.assertEqual(m.group(1), deploy.DEFAULT_COMPUTE_INSTANCES)

    def test_every_accepted_setting_of_the_shared_table_resolves_to_its_mode(self) -> None:
        for case in self.TABLE["accept"]:
            with self.subTest(setting=case["setting"]):
                self.assertEqual(deploy.resolve_compute_instances(case["setting"]), case["mode"])

    def test_every_rejected_setting_of_the_shared_table_raises_and_names_the_setting_and_the_words(self) -> None:
        for word in self.TABLE["reject"]:
            with self.subTest(setting=word):
                with self.assertRaises(deploy.ComputeInstancesError) as cm:
                    deploy.resolve_compute_instances(word)
                self.assertIn("HAMH_COMPUTE_INSTANCES", str(cm.exception))
                self.assertIn("off, on", str(cm.exception))

    def test_a_deployer_reads_the_variable_and_hands_cdk_the_resolved_word(self) -> None:
        with mock.patch.dict(os.environ, {"HAMH_COMPUTE_INSTANCES": " On "}):
            d = deploy.Deployer("direct")
            self.assertEqual((d.compute_instances, d.instances_setting, d.env["HAMH_COMPUTE_INSTANCES"]), (True, "On", "on"))
        with mock.patch.dict(os.environ):
            os.environ.pop("HAMH_COMPUTE_INSTANCES", None)
            d = deploy.Deployer("direct")
            self.assertEqual((d.compute_instances, d.instances_setting, d.env["HAMH_COMPUTE_INSTANCES"]), (False, None, "off"))
        with mock.patch.dict(os.environ, {"HAMH_COMPUTE_INSTANCES": "auto"}), mock.patch("sys.stderr") as err:
            self.assertEqual(deploy.main(["--stage", "direct"]), 2)
            self.assertIn("HAMH_COMPUTE_INSTANCES", "".join(str(c.args[0]) for c in err.write.call_args_list))

    def test_the_names_the_deploy_keys_on_are_the_ones_the_runtimes_stack_creates(self) -> None:
        # The second runtime's construct id gives its `<id>RuntimeId` output, which live_versions() reads; the provider output says the Instances part exists.
        self.assertEqual(deploy.INSTANCES_RUNTIME, "WorkerPythonInstances")
        self.assertIn(f'id: "{deploy.INSTANCES_RUNTIME}"', self.RUNTIMES_TS)
        self.assertIn(f'new CfnOutput(this, "{deploy.INSTANCES_PROVIDER_OUTPUT}"', self.RUNTIMES_TS)

    def test_the_isolation_words_are_the_ones_the_runtimes_stack_sets(self) -> None:
        # Agentd refuses to start the engine without the word, so the deploy checks each runtime for its own.
        self.assertEqual(deploy.SANDBOX_VAR, "AGENTD_ENGINE_SANDBOX")
        self.assertEqual(deploy.SANDBOX_WORDS, {"WorkerPython": "uid", "WorkerPythonInstances": "landlock"})
        self.assertIn(f'{deploy.SANDBOX_VAR}: "uid"', self.RUNTIMES_TS)
        self.assertIn(f'{deploy.SANDBOX_VAR}: "landlock"', self.RUNTIMES_TS)

    def test_the_words_the_deploy_compares_with_a_live_runtime_are_the_ones_the_runtimes_stack_sets(self) -> None:
        # infra-iam-4 and infra-defaults-1: a live runtime that lacks one of these, or carries another, is a runtime change (the guard) and, after the deploy, an error.
        # In landlock mode agentd refuses to start the engine while AGENTD_ENGINE_USER names another user, so the Instances runtime overrides the image's "agent" with "".
        self.assertEqual(deploy.ENGINE_USER_VAR, "AGENTD_ENGINE_USER")
        self.assertEqual(deploy.RUNTIME_WORDS, {"WorkerPython": {"AGENTD_ENGINE_SANDBOX": "uid"},
                                                "WorkerPythonInstances": {"AGENTD_ENGINE_SANDBOX": "landlock", "AGENTD_ENGINE_USER": ""}})
        self.assertIn(f'{deploy.ENGINE_USER_VAR}: ""', self.RUNTIMES_TS)
        self.assertEqual(deploy.SANDBOX_WORDS, {name: words[deploy.SANDBOX_VAR] for name, words in deploy.RUNTIME_WORDS.items()})


class Usage(unittest.TestCase):
    def test_direct_has_one_az_and_is_the_only_stage(self) -> None:
        with mock.patch("sys.stderr"):
            self.assertEqual(deploy.main(["--stage", "direct", "--azs", "3"]), 2)
            self.assertEqual(deploy.main(["--stage", "prod"]), 2)
            self.assertEqual(deploy.main(["--stage", "live"]), 2)
            self.assertEqual(deploy.main(["--stage", "drill"]), 2)
            self.assertEqual(deploy.main(["--stage", "direct", "--ladder-deferred"]), 2)

    def test_the_usage_line_names_the_stage(self) -> None:
        with tempfile.TemporaryFile("w+") as err:
            with mock.patch("sys.stderr", err):
                deploy.usage("x")
            err.seek(0)
            self.assertIn("--stage direct", err.read())

    def test_check_guard_accepts_the_direct_stage(self) -> None:
        # The guard reads the run machine of the stage: here it is asked through a fake aws that knows no run.
        fake = Path(tempfile.mkdtemp(prefix="deploy-direct-aws-")) / "aws"
        fake.write_text("#!/bin/sh\necho '{\"executions\": [], \"Account\": \"%s\"}'\n" % aws_target.select({}).account)
        fake.chmod(0o755)
        env = {**os.environ, "DEPLOY_AWS_BIN": str(fake)}
        proc = subprocess.run([sys.executable, str(REPO / "scripts" / "lib" / "deploy.py"), "--stage", "direct", "--check-guard"],  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool
                              capture_output=True, text=True, env=env, timeout=120)
        self.assertNotEqual(proc.returncode, 2, proc.stderr)
        self.assertNotIn("usage", proc.stderr)


if __name__ == "__main__":
    unittest.main()
