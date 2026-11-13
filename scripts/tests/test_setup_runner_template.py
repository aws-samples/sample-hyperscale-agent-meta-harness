"""Tests for infra/setup-runner.yaml, the CloudFormation template that `setup.sh` deploys as the stack `hamh-setup-runner`.

The template holds the account-local runner: an encrypted, versioned bucket for source, configuration and results, a CodeBuild project (Linux x86_64) that
runs scripts/setup/runner-build.sh, a log group, the runner role with the permissions boundary the runner cannot change, and four outputs. These cases hold
the invariants: the shape of the bucket, the trust policy of the runner role, no wildcard authority, no custom resource, project tags
and nothing that names the authors' own accounts, profiles or tags. The policy statements themselves are held against the checked-in policy file by
`test_setup_runner_policy.py`.
"""

from __future__ import annotations

import hashlib
import re
import unittest
from typing import Iterable, List, Set

import setup_runner_support as sup

TEMPLATE = sup.template() if sup.TEMPLATE_PATH.is_file() else {}
TEXT = sup.TEMPLATE_PATH.read_text(encoding="utf-8") if sup.TEMPLATE_PATH.is_file() else ""

# SHA-256 of the lower-case words that name the authors' environment (an account owner, an account alias, a profile, a cleanup service, its tag and the company
# domain). Only the hashes are here, so this file names nothing of the authors' environment itself.
DENIED_WORDS_SHA256 = {
    "36f41491abc99cc2f1990c9adea94fb974cea79666c638f7f47202f74ce53f12",
    "8758607d0bcaa8a6efc8f92aa63bf367a6f088001d8fd2f16d1583c356a1a23f",
    "5f8cd2e5d9508d3b5a2ecb527e7d7af3614f5c5ea8e4fec26dd7458387e14a8b",
    "dad256207b8b1652475f652bacc0f362bdf4bced02124b413641804257feefdd",
    "574a04fffd0557c12487e247c47286f40f0547e7941d22d945f512839659f74e",
    "e652f04833076f5084a6343511295bc24d7c8f6fa43925e336c3ec9d1c94f35f",
    "b19511d5c8085aea2d88c7ec5ecea136e3a36a58c0798197344be1aeb1667617",
}


def word_runs(text: str) -> Set[str]:
    """Every run of one to all words of a hyphen or dot joined token of the text, lower case, with the original separators."""
    out: Set[str] = set()
    for token in re.findall(r"[a-z0-9]+(?:[.\-][a-z0-9]+)*", text.lower()):
        pieces = re.split(r"([.\-])", token)
        words, seps = pieces[0::2], pieces[1::2]
        for i in range(len(words)):
            run = words[i]
            out.add(run)
            for j in range(i + 1, len(words)):
                run += seps[j - 1] + words[j]
                out.add(run)
    return out


def denied_in(text: str, hashes: Iterable[str]) -> List[str]:
    wanted = set(hashes)
    return sorted(run for run in word_runs(text) if hashlib.sha256(run.encode()).hexdigest() in wanted)


RESOURCE_TYPES = {
    "SetupBucket": "AWS::S3::Bucket",
    "SetupBucketPolicy": "AWS::S3::BucketPolicy",
    "RunnerKey": "AWS::KMS::Key",
    "RunnerKeyAlias": "AWS::KMS::Alias",
    "LogGroup": "AWS::Logs::LogGroup",
    "RunnerBoundary": "AWS::IAM::ManagedPolicy",
    "RunnerRole": "AWS::IAM::Role",
    "BuildProject": "AWS::CodeBuild::Project",
}
OUTPUTS = {
    "SetupBucket": {"Ref": "SetupBucket"},
    "BuildProject": {"Ref": "BuildProject"},
    "LogGroup": {"Ref": "LogGroup"},
    "RunnerRoleArn": {"Fn::GetAtt": ["RunnerRole", "Arn"]},
}
# The suppressions the template may carry: scanner, rule, resource. A new one needs a reason in the template and an entry here.
SUPPRESSIONS = {
    ("cfn_nag", "W35", "SetupBucket"),
    ("checkov", "CKV_AWS_18", "SetupBucket"),
    ("cfn_nag", "W11", "RunnerRole"),
    ("cfn_nag", "W28", "RunnerRole"),
    ("cfn_nag", "W76", "RunnerRole"),
    ("cfn_nag", "W13", "RunnerBoundary"),
    ("cfn_nag", "W28", "RunnerBoundary"),
    ("cfn_nag", "W32", "BuildProject"),
}
BUCKET_NAME = "hamh-setup-{account}-{region}"
PROJECT_NAME = "hamh-setup-runner"
LOG_GROUP_NAME = "/hamh/setup-runner"


def need(test: unittest.TestCase) -> None:
    if not sup.TEMPLATE_PATH.is_file():
        test.fail(f"{sup.TEMPLATE_PATH.relative_to(sup.REPO)} does not exist")


def prop(name: str) -> dict:
    return TEMPLATE["Resources"][name]["Properties"]


class ShapeCase(unittest.TestCase):
    def setUp(self) -> None:
        need(self)

    def test_top_level(self) -> None:
        self.assertEqual(TEMPLATE["AWSTemplateFormatVersion"], "2010-09-09")
        self.assertLessEqual(set(TEMPLATE), {"AWSTemplateFormatVersion", "Description", "Metadata", "Parameters", "Resources", "Outputs"})
        self.assertGreaterEqual(len(TEMPLATE["Description"]), 60)

    def test_resources_are_exactly_these(self) -> None:
        self.assertEqual({k: v["Type"] for k, v in TEMPLATE["Resources"].items()}, RESOURCE_TYPES)

    def test_no_custom_resource_and_no_code(self) -> None:
        for name, res in TEMPLATE["Resources"].items():
            self.assertFalse(res["Type"].startswith("Custom::") or res["Type"] == "AWS::CloudFormation::CustomResource", name)
            self.assertNotIn("Lambda", res["Type"], name)
        self.assertNotIn("Transform", TEMPLATE)
        self.assertNotIn("Conditions", TEMPLATE)

    def test_outputs_are_the_four_of_the_contract(self) -> None:
        self.assertEqual({k: v["Value"] for k, v in TEMPLATE["Outputs"].items()}, OUTPUTS)
        for name, out in TEMPLATE["Outputs"].items():
            self.assertNotIn("Export", out, f"{name}: an export would block the deletion of a stack that imports it")
            self.assertGreaterEqual(len(out["Description"]), 20, name)

    def test_parameters(self) -> None:
        params = TEMPLATE["Parameters"]
        self.assertEqual(sorted(params), ["AuthorityExpires", "BuildComputeType", "BuildTimeoutMinutes", "InstallerPrincipal", "InstallerPrincipalWithPath"])
        self.assertEqual(params["BuildComputeType"]["Default"], "BUILD_GENERAL1_LARGE")
        self.assertEqual(params["BuildComputeType"]["AllowedValues"], ["BUILD_GENERAL1_LARGE", "BUILD_GENERAL1_XLARGE", "BUILD_GENERAL1_2XLARGE"])
        self.assertEqual(params["BuildTimeoutMinutes"]["Type"], "Number")
        self.assertEqual((params["BuildTimeoutMinutes"]["MinValue"], params["BuildTimeoutMinutes"]["MaxValue"]), (30, 480))
        self.assertGreaterEqual(params["BuildTimeoutMinutes"]["Default"], 120)


class BucketCase(unittest.TestCase):
    def setUp(self) -> None:
        need(self)
        self.bucket = prop("SetupBucket")

    def test_name(self) -> None:
        self.assertEqual(sup.normalise(self.bucket["BucketName"]), BUCKET_NAME)

    def test_encrypted(self) -> None:
        rules = self.bucket["BucketEncryption"]["ServerSideEncryptionConfiguration"]
        self.assertEqual(len(rules), 1)
        self.assertEqual(rules[0]["ServerSideEncryptionByDefault"]["SSEAlgorithm"], "AES256")

    def test_versioned(self) -> None:
        self.assertEqual(self.bucket["VersioningConfiguration"], {"Status": "Enabled"})

    def test_public_access_is_blocked_four_ways(self) -> None:
        self.assertEqual(self.bucket["PublicAccessBlockConfiguration"], {
            "BlockPublicAcls": True, "BlockPublicPolicy": True, "IgnorePublicAcls": True, "RestrictPublicBuckets": True})

    def test_owner_enforced(self) -> None:
        self.assertEqual(self.bucket["OwnershipControls"], {"Rules": [{"ObjectOwnership": "BucketOwnerEnforced"}]})

    def test_lifecycle_expires_each_prefix_and_old_versions(self) -> None:
        rules = {r["Id"]: r for r in self.bucket["LifecycleConfiguration"]["Rules"]}
        self.assertEqual(sorted(rules), ["abort-incomplete-uploads", "expire-client", "expire-config", "expire-noncurrent-versions", "expire-results", "expire-source"])
        for rid, rule in rules.items():
            self.assertEqual(rule["Status"], "Enabled", rid)
        self.assertEqual(rules["abort-incomplete-uploads"]["AbortIncompleteMultipartUpload"]["DaysAfterInitiation"], 1)
        self.assertGreaterEqual(rules["expire-noncurrent-versions"]["NoncurrentVersionExpiration"]["NoncurrentDays"], 1)
        for prefix in ("source", "config", "results", "client"):
            rule = rules[f"expire-{prefix}"]
            self.assertEqual(rule["Prefix"], f"{prefix}/")
            self.assertTrue(1 <= rule["ExpirationInDays"] <= 365, prefix)
        # nothing expires the lease: the object is small, rewritten by every setup, and removed with the bucket
        self.assertNotIn("lease", " ".join(str(r.get("Prefix")) for r in rules.values()))

    def test_stack_deletion_is_not_blocked(self) -> None:
        res = TEMPLATE["Resources"]["SetupBucket"]
        self.assertIn(res.get("DeletionPolicy", "Delete"), ("Delete",))
        self.assertIn(res.get("UpdateReplacePolicy", "Delete"), ("Delete",))

    def test_tags(self) -> None:
        self.assertIn({"Key": "project", "Value": sup.PROJECT_TAG}, self.bucket["Tags"])


class BucketPolicyCase(unittest.TestCase):
    def setUp(self) -> None:
        need(self)
        self.policy = prop("SetupBucketPolicy")

    def test_names_the_bucket(self) -> None:
        self.assertEqual(self.policy["Bucket"], {"Ref": "SetupBucket"})

    def test_tls_only(self) -> None:
        doc = sup.normalise(self.policy["PolicyDocument"])
        denies = [s for s in doc["Statement"] if s["Effect"] == "Deny"]
        tls = [s for s in denies if s.get("Condition") == {"Bool": {"aws:SecureTransport": "false"}}]
        self.assertEqual(len(tls), 1)
        self.assertEqual(tls[0]["Principal"], "*")
        self.assertEqual(sup.as_list(tls[0]["Action"]), ["s3:*"])
        self.assertEqual(sorted(sup.as_list(tls[0]["Resource"])), sorted([f"arn:{{partition}}:s3:::{BUCKET_NAME}", f"arn:{{partition}}:s3:::{BUCKET_NAME}/*"]))

    def test_no_allow_statement(self) -> None:
        for st in self.policy["PolicyDocument"]["Statement"]:
            self.assertEqual(st["Effect"], "Deny", st.get("Sid"))

    def test_only_the_runner_role_and_the_installer_write_or_delete(self) -> None:
        """Any principal of the account that may call s3:PutObject could put its own archive under source/<commit>.zip, and the runner runs that archive with deployment
        authority. The bucket policy refuses writes and deletes to everyone but the runner role and the identity that ran setup.sh (the two stack parameters)."""
        doc = sup.normalise(self.policy["PolicyDocument"])
        writes = [s for s in doc["Statement"] if s.get("Sid") == "OnlyTheRunnerAndTheInstallerWrite"]
        self.assertEqual(len(writes), 1)
        st = writes[0]
        self.assertEqual((st["Effect"], st["Principal"]), ("Deny", "*"))
        self.assertEqual(sorted(sup.as_list(st["Action"])), ["s3:DeleteObject", "s3:DeleteObjectVersion", "s3:PutObject"])
        self.assertEqual(sup.as_list(st["Resource"]), [f"arn:{{partition}}:s3:::{BUCKET_NAME}/*"])
        self.assertEqual(sorted(st["Condition"]), ["ArnNotLike"])
        self.assertEqual(sup.as_list(st["Condition"]["ArnNotLike"]["aws:PrincipalArn"]),
                         ["arn:{partition}:iam::{account}:role/hamh-setup-runner-role", {"Ref": "InstallerPrincipal"}, {"Ref": "InstallerPrincipalWithPath"}])

    def test_the_runner_role_the_policy_names_is_the_role_the_template_makes(self) -> None:
        self.assertEqual(prop("RunnerRole")["RoleName"], "hamh-setup-runner-role")

    def test_the_stack_parameters_are_checked_before_they_reach_a_policy(self) -> None:
        params = TEMPLATE["Parameters"]
        arn = re.compile(params["InstallerPrincipal"]["AllowedPattern"])
        for good in ("arn:aws:iam::{a}:user/setup-test", "arn:aws:iam::{a}:root", "arn:aws:iam::{a}:role/AWSReservedSSO_Admin_0123456789abcdef", "arn:aws:iam::{a}:role/*/Admin",
                     "arn:aws:sts::{a}:federated-user/me@example.invalid"):
            self.assertRegex(good.format(a="2" * 12), arn)
        for bad in ("*", "arn:aws:iam::{a}:*", "arn:aws:iam::{a}:role/*", "arn:aws:iam::{a}:user/*", "arn:aws:iam::{a}:role/*/*", "arn:aws:iam::{a}:role/Ad*", "arn:aws-cn:iam::{a}:root",
                    "arn:aws:iam::123:root", "arn:aws:iam::{a}:role/x y", "arn:aws:iam::{a}:role/x\\nDeny", "arn:aws:s3:::bucket", "arn:aws:iam::{a}:group/x"):
            self.assertNotRegex(bad.format(a="2" * 12), arn)
        self.assertEqual(params["InstallerPrincipalWithPath"]["AllowedPattern"], params["InstallerPrincipal"]["AllowedPattern"])
        when = re.compile(params["AuthorityExpires"]["AllowedPattern"])
        for good in ("2026-10-29T17:00:00Z", "2099-12-31T23:59:59Z"):
            self.assertRegex(good, when)
        for bad in ("2026-10-29 17:00:00", "2026-13-01T00:00:00Z", "2026-10-29T24:00:00Z", "tomorrow", "2026-10-29T17:00:00+00:00", "1999-01-01T00:00:00Z"):
            self.assertNotRegex(bad, when)


class LogGroupCase(unittest.TestCase):
    def setUp(self) -> None:
        need(self)
        self.group = prop("LogGroup")

    def test_name_retention_and_tags(self) -> None:
        self.assertEqual(self.group["LogGroupName"], LOG_GROUP_NAME)
        self.assertIn(self.group["RetentionInDays"], (14, 30, 60, 90))
        self.assertIn({"Key": "project", "Value": sup.PROJECT_TAG}, self.group["Tags"])

    def test_encrypted_with_the_runner_key(self) -> None:
        self.assertEqual(self.group["KmsKeyId"], {"Fn::GetAtt": ["RunnerKey", "Arn"]})

    def test_deleted_with_the_stack(self) -> None:
        res = TEMPLATE["Resources"]["LogGroup"]
        self.assertIn(res.get("DeletionPolicy", "Delete"), ("Delete",))


class KeyCase(unittest.TestCase):
    def setUp(self) -> None:
        need(self)
        self.key = prop("RunnerKey")

    def test_rotation_short_deletion_window_and_tags(self) -> None:
        self.assertIs(self.key["EnableKeyRotation"], True)
        self.assertEqual(self.key["PendingWindowInDays"], 7)
        self.assertIn({"Key": "project", "Value": sup.PROJECT_TAG}, self.key["Tags"])
        self.assertEqual(TEMPLATE["Resources"]["RunnerKey"].get("DeletionPolicy", "Delete"), "Delete")

    def test_policy_is_the_account_and_cloudwatch_logs_for_this_group(self) -> None:
        doc = sup.normalise(self.key["KeyPolicy"])
        by_sid = {s["Sid"]: s for s in doc["Statement"]}
        self.assertEqual(sorted(by_sid), ["AccountAdministersThroughIam", "CloudWatchLogsDescribesTheKey", "CloudWatchLogsForTheGroup"])
        self.assertEqual(by_sid["AccountAdministersThroughIam"]["Principal"], {"AWS": "arn:{partition}:iam::{account}:root"})
        logs = by_sid["CloudWatchLogsForTheGroup"]
        self.assertEqual(logs["Principal"], {"Service": "logs.{region}.amazonaws.com"})
        self.assertEqual(logs["Condition"], {"StringEquals": {"kms:EncryptionContext:aws:logs:arn": "arn:{partition}:logs:{region}:{account}:log-group:" + LOG_GROUP_NAME}})
        self.assertEqual(by_sid["CloudWatchLogsDescribesTheKey"]["Action"], "kms:DescribeKey")
        for st in doc["Statement"]:
            self.assertNotEqual(st["Principal"], "*", st["Sid"])

    def test_alias(self) -> None:
        alias = prop("RunnerKeyAlias")
        self.assertEqual(alias, {"AliasName": "alias/hamh-setup-runner", "TargetKeyId": {"Ref": "RunnerKey"}})


class RunnerRoleCase(unittest.TestCase):
    def setUp(self) -> None:
        need(self)
        self.role = prop("RunnerRole")

    def test_trust_is_codebuild_of_this_project_in_this_account(self) -> None:
        doc = sup.normalise(self.role["AssumeRolePolicyDocument"])
        self.assertEqual(doc["Version"], "2012-10-17")
        self.assertEqual(len(doc["Statement"]), 1)
        st = doc["Statement"][0]
        self.assertEqual(st["Effect"], "Allow")
        self.assertEqual(st["Principal"], {"Service": "codebuild.amazonaws.com"})
        self.assertEqual(st["Action"], "sts:AssumeRole")
        self.assertEqual(st["Condition"], {
            "StringEquals": {"aws:SourceAccount": "{account}"},
            "ArnEquals": {"aws:SourceArn": f"arn:{{partition}}:codebuild:{{region}}:{{account}}:project/{PROJECT_NAME}"}})

    def test_the_boundary_is_attached_and_the_role_has_no_managed_policy(self) -> None:
        self.assertEqual(self.role["PermissionsBoundary"], {"Ref": "RunnerBoundary"})
        self.assertNotIn("ManagedPolicyArns", self.role)

    def test_one_inline_policy_and_no_wildcard_authority(self) -> None:
        policies = self.role["Policies"]
        self.assertEqual(len(policies), 1)
        text = str(policies)
        self.assertNotIn("AdministratorAccess", text)
        for st in policies[0]["PolicyDocument"]["Statement"]:
            for action in sup.as_list(st["Action"]):
                if st["Effect"] == "Allow":
                    self.assertNotIn(action, ("*",), st["Sid"])
                    self.assertFalse(action.endswith(":*"), f"{st['Sid']}: {action}")

    def test_role_name_and_tags(self) -> None:
        self.assertEqual(self.role["RoleName"], "hamh-setup-runner-role")
        self.assertIn({"Key": "project", "Value": sup.PROJECT_TAG}, self.role["Tags"])

    def test_the_trust_policy_does_not_name_the_project_by_reference(self) -> None:
        # A Ref or GetAtt of BuildProject in the trust policy would make the role and the project depend on each other.
        self.assertNotIn("BuildProject", str(self.role["AssumeRolePolicyDocument"]))


class BoundaryCase(unittest.TestCase):
    def setUp(self) -> None:
        need(self)
        self.boundary = prop("RunnerBoundary")

    def test_name_and_no_attachment(self) -> None:
        self.assertEqual(self.boundary["ManagedPolicyName"], "hamh-setup-runner-boundary")
        for key in ("Roles", "Users", "Groups"):
            self.assertNotIn(key, self.boundary)

    def test_denies_iam_and_leaves_it_out_of_the_allow(self) -> None:
        statements = self.boundary["PolicyDocument"]["Statement"]
        self.assertTrue(any(s["Effect"] == "Deny" and "iam:*" in sup.as_list(s["Action"]) for s in statements))
        for s in statements:
            if s["Effect"] == "Allow":
                self.assertFalse(any(a.startswith("iam:") for a in sup.as_list(s["Action"])), s["Sid"])


class ProjectCase(unittest.TestCase):
    def setUp(self) -> None:
        need(self)
        self.project = prop("BuildProject")

    def test_name_and_role(self) -> None:
        self.assertEqual(self.project["Name"], PROJECT_NAME)
        self.assertEqual(self.project["ServiceRole"], {"Fn::GetAtt": ["RunnerRole", "Arn"]})

    def test_environment_is_linux_x86_64_standard_5(self) -> None:
        env = self.project["Environment"]
        self.assertEqual(env["Type"], "LINUX_CONTAINER")
        self.assertEqual(env["Image"], "aws/codebuild/amazonlinux-x86_64-standard:5.0")
        self.assertEqual(env["ComputeType"], {"Ref": "BuildComputeType"})
        # scripts/node.sh runs Node, pnpm and the CDK in a container, so the build needs the Docker daemon, which needs privileged mode.
        self.assertIs(env["PrivilegedMode"], True)
        self.assertNotIn("EnvironmentVariables", env, "setup.sh passes the inputs at start-build; the project holds none")

    def test_source_is_an_s3_object_with_the_buildspec_of_the_archive(self) -> None:
        src = self.project["Source"]
        self.assertEqual(src["Type"], "S3")
        self.assertEqual(src["BuildSpec"], "images/codebuild/setup-runner.yml")
        self.assertEqual(sup.normalise(src["Location"]), f"{BUCKET_NAME}/source/placeholder.zip")

    def test_no_artifacts_one_build_at_a_time_and_a_timeout(self) -> None:
        self.assertEqual(self.project["Artifacts"], {"Type": "NO_ARTIFACTS"})
        self.assertEqual(self.project["ConcurrentBuildLimit"], 1)
        self.assertEqual(self.project["TimeoutInMinutes"], {"Ref": "BuildTimeoutMinutes"})
        self.assertLessEqual(self.project["QueuedTimeoutInMinutes"], 60)

    def test_logs_go_to_the_log_group(self) -> None:
        self.assertEqual(self.project["LogsConfig"], {"CloudWatchLogs": {"Status": "ENABLED", "GroupName": {"Ref": "LogGroup"}}})

    def test_tags(self) -> None:
        self.assertIn({"Key": "project", "Value": sup.PROJECT_TAG}, self.project["Tags"])


class NeutralCase(unittest.TestCase):
    def setUp(self) -> None:
        need(self)

    def test_no_account_number_region_or_profile(self) -> None:
        self.assertIsNone(re.search(r"\b[0-9]{12}\b", TEXT))
        self.assertIsNone(re.search(r"\b(us|eu|ap|ca|sa|me|af)-(east|west|north|south|central|northeast|southeast|northwest|southwest)-[0-9]\b", TEXT))

    def test_no_author_names_and_no_author_tags(self) -> None:
        self.assertEqual(denied_in(TEXT, DENIED_WORDS_SHA256), [], "the template names the authors' environment")
        for res in TEMPLATE["Resources"].values():
            keys = {t["Key"] for t in res.get("Properties", {}).get("Tags", [])}
            self.assertLessEqual(keys, {"project"}, "the template carries the project tag and nothing of the authors")

    def test_the_word_check_finds_a_denied_word_in_any_token_position(self) -> None:
        wanted = {hashlib.sha256(b"example-host").hexdigest(), hashlib.sha256(b"corp.invalid").hexdigest()}
        self.assertEqual(denied_in("see sub.example-host.invalid, then Example-Host again", wanted), ["example-host"])
        self.assertEqual(denied_in("mail me@team.corp.invalid", wanted), ["corp.invalid"])
        self.assertEqual(denied_in("nothing of the kind: example, host, corp", wanted), [])
        self.assertEqual(word_runs("a.b-c"), {"a", "b", "c", "a.b", "b-c", "a.b-c"})

    def test_the_hashes_are_the_ones_of_the_words_the_template_may_not_use(self) -> None:
        self.assertEqual(len(DENIED_WORDS_SHA256), 7)
        self.assertTrue(all(re.fullmatch(r"[0-9a-f]{64}", h) for h in DENIED_WORDS_SHA256))

    def test_every_taggable_resource_has_the_project_tag(self) -> None:
        for name, res in TEMPLATE["Resources"].items():
            if res["Type"] in ("AWS::S3::BucketPolicy", "AWS::IAM::ManagedPolicy", "AWS::KMS::Alias"):
                continue
            self.assertIn({"Key": "project", "Value": sup.PROJECT_TAG}, res["Properties"].get("Tags", []), name)


class SuppressionCase(unittest.TestCase):
    def setUp(self) -> None:
        need(self)

    def test_only_the_listed_suppressions_with_a_reason(self) -> None:
        found = set()
        for name, res in TEMPLATE["Resources"].items():
            meta = res.get("Metadata") or {}
            for rule in (meta.get("cfn_nag") or {}).get("rules_to_suppress", []):
                found.add(("cfn_nag", rule["id"], name))
                self.assertGreaterEqual(len(rule["reason"]), 60, f"{name} {rule['id']}")
            for rule in (meta.get("checkov") or {}).get("skip", []):
                found.add(("checkov", rule["id"], name))
                self.assertGreaterEqual(len(rule["comment"]), 60, f"{name} {rule['id']}")
                self.assertEqual(rule.get("reason"), rule["comment"], f"{name} {rule['id']}")
        self.assertEqual(found, SUPPRESSIONS)


class HeaderCase(unittest.TestCase):
    """The comment block at the top of the template says what the stack is allowed to do, what it is not, and how it is removed."""

    def setUp(self) -> None:
        need(self)
        self.head = "\n".join(line for line in TEXT.splitlines()[:140] if line.startswith("#"))

    def test_says_the_cdk_roles_are_not_bounded_here(self) -> None:
        self.assertIn("AdministratorAccess", self.head)
        self.assertIn("cdk-hnb659fds-cfn-exec-role", self.head)
        self.assertIn("KMS key", self.head)
        self.assertIn("not bounded by this template", self.head)

    def test_lists_every_unscopable_action(self) -> None:
        for action in sup.policy()["unscopable"]:
            self.assertIn(action, self.head, f"the header does not list {action}")

    def test_says_how_the_bucket_is_emptied_before_the_stack_is_deleted(self) -> None:
        self.assertIn("list-object-versions", self.head)
        self.assertIn("delete-objects", self.head)
        self.assertIn("scripts/setup/empty-bucket.sh", self.head)

    def test_says_how_a_build_is_started(self) -> None:
        for word in ("start-build", "SETUP_SOURCE_KEY", "SETUP_CONFIG_KEY", "--source-location-override"):
            self.assertIn(word, self.head)


if __name__ == "__main__":
    unittest.main()
