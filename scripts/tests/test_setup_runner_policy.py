"""Tests for infra/setup-runner-policy.json, the checked-in list of what the setup runner may call, and for its agreement with the scripts and the template.

The policy file holds the statements of the runner role (and of its permissions boundary) and a table of every AWS CLI call that the scripts the
runner role's credentials reach make (`calls`). Three things are held here: the statements are narrow (no wildcard action, no IAM, every wildcard
resource listed and explained), the table matches the scripts both ways (a script that starts to call an API the table lacks fails, and so does a
table entry for a call that is gone), and every runner action in the table is allowed by the statements and by the boundary.
"""

from __future__ import annotations

import re
import unittest

import setup_runner_support as sup

POLICY = sup.policy() if sup.POLICY_PATH.is_file() else {}
TEMPLATE = sup.template() if sup.TEMPLATE_PATH.is_file() else {}


def need(test: unittest.TestCase) -> None:
    if not sup.POLICY_PATH.is_file():
        test.fail(f"{sup.POLICY_PATH.relative_to(sup.REPO)} does not exist")


class ShapeCase(unittest.TestCase):
    def setUp(self) -> None:
        need(self)

    def test_top_level_keys(self) -> None:
        self.assertEqual(sorted(POLICY), ["boundary", "calls", "note", "platform", "schema", "statements", "unscopable"])
        self.assertEqual(POLICY["schema"], "1")

    def test_statements_have_unique_sids_and_the_iam_fields(self) -> None:
        for name in ("statements", "boundary"):
            sids = [s["Sid"] for s in POLICY[name]]
            self.assertEqual(len(sids), len(set(sids)), f"{name}: a Sid twice")
            for st in POLICY[name]:
                self.assertIn(st["Effect"], ("Allow", "Deny"), st["Sid"])
                self.assertTrue(sup.as_list(st.get("Action")), f"{name}/{st['Sid']}: no Action")
                self.assertTrue(sup.as_list(st.get("Resource")), f"{name}/{st['Sid']}: no Resource")
                self.assertNotIn("NotAction", st, st["Sid"])
                self.assertNotIn("NotResource", st, st["Sid"])
                self.assertRegex(st["Sid"], r"^[A-Za-z0-9]+$")

    def test_tokens_are_the_three_pseudo_parameters_and_the_expiry(self) -> None:
        text = sup.POLICY_PATH.read_text(encoding="utf-8")
        self.assertEqual(sorted(set(re.findall(r"\{([A-Za-z]+)\}", text))), ["account", "authorityExpires", "partition", "region"])


class NarrowCase(unittest.TestCase):
    def setUp(self) -> None:
        need(self)

    def test_no_allow_statement_has_a_wildcard_action(self) -> None:
        for st in (s for s in POLICY["statements"] if s["Effect"] == "Allow"):
            for action in sup.as_list(st["Action"]):
                self.assertNotEqual(action, "*", st["Sid"])
                self.assertFalse(action.endswith(":*"), f"{st['Sid']}: {action}")

    def test_iam_organizations_and_account_are_denied_and_never_allowed(self) -> None:
        for st in (s for s in POLICY["statements"] if s["Effect"] == "Allow"):
            for action in sup.as_list(st["Action"]):
                self.assertFalse(re.match(r"(iam|organizations|account):", action), f"{st['Sid']} allows {action}")
        denied = [s for s in POLICY["statements"] if s["Effect"] == "Deny"]
        for service in ("iam", "organizations", "account"):
            self.assertTrue(any(f"{service}:*" in sup.as_list(s["Action"]) for s in denied), f"{service}:* is not denied")

    def test_a_wildcard_resource_is_listed_and_explained(self) -> None:
        listed = POLICY["unscopable"]
        used = set()
        for st in POLICY["statements"]:
            if st["Effect"] == "Allow" and "*" in sup.as_list(st["Resource"]):
                self.assertEqual(sup.as_list(st["Resource"]), ["*"], st["Sid"])
                used.update(sup.as_list(st["Action"]))
        self.assertEqual(sorted(used), sorted(listed), "the actions allowed on every resource and the unscopable list differ")
        for action, reason in listed.items():
            self.assertGreaterEqual(len(reason), 40, f"{action}: say why it cannot name a resource")

    def test_a_wildcard_resource_statement_is_held_by_a_condition(self) -> None:
        for st in POLICY["statements"]:
            if st["Effect"] == "Allow" and sup.as_list(st["Resource"]) == ["*"]:
                self.assertIn("Condition", st, f"{st['Sid']} allows every resource with no condition")

    def test_s3_writes_name_a_prefix_of_a_bucket(self) -> None:
        for st in POLICY["statements"]:
            if st["Effect"] == "Allow" and any(a in ("s3:PutObject", "s3:DeleteObject") for a in sup.as_list(st["Action"])):
                for res in sup.as_list(st["Resource"]):
                    self.assertRegex(res, r"^arn:\{partition\}:s3:::[a-z0-9{}-]+/[a-z][a-z0-9/-]*/\*$", f"{st['Sid']}: {res}")

    def test_the_runner_writes_the_setup_bucket_under_exactly_three_prefixes(self) -> None:
        """results/ (result.json, phases.jsonl, records), client/ (the built client) and bootstrap/ (the CDK bootstrap template of a bootstrap-template build). Never source/,
        config/ or lease/: the runner reads what setup.sh uploads and does not change it."""
        prefixes = sorted(res.rsplit("/", 2)[-2] for st in POLICY["statements"] if st["Effect"] == "Allow" and "s3:PutObject" in sup.as_list(st["Action"])
                          for res in sup.as_list(st["Resource"]) if "hamh-setup-" in res)
        self.assertEqual(prefixes, ["bootstrap", "client", "results"])

    def test_s3_never_allows_delete(self) -> None:
        for st in POLICY["statements"]:
            for action in sup.as_list(st["Action"]):
                if st["Effect"] == "Allow":
                    self.assertFalse(action.startswith("s3:Delete"), f"{st['Sid']}: {action}")

    def test_assume_role_names_exact_roles_of_the_account(self) -> None:
        for st in POLICY["statements"]:
            if "sts:AssumeRole" in sup.as_list(st["Action"]):
                for res in sup.as_list(st["Resource"]):
                    self.assertRegex(res, r"^arn:\{partition\}:iam::\{account\}:role/(cdk-hnb659fds-[a-z-]+-role-\{account\}-\{region\}|hamh-direct-gomod-filler)$")

    def test_the_statements_that_assume_a_role_end_at_the_expiry(self) -> None:
        """AuthorityExpires: a runner that is left behind cannot assume the CDK roles (and with them act as an administrator) after the time setup.sh wrote at its
        last install. Only these two statements assume a role, and both carry the condition."""
        assume = sorted(s["Sid"] for s in POLICY["statements"] if "sts:AssumeRole" in sup.as_list(s["Action"]))
        self.assertEqual(assume, ["AssumeCdkBootstrapRoles", "AssumeGomodFiller"])
        for st in (s for s in POLICY["statements"] if s["Sid"] in assume):
            self.assertEqual(st["Condition"], {"DateLessThan": {"aws:CurrentTime": "{authorityExpires}"}}, st["Sid"])

    def test_every_resource_names_this_project_or_the_cdk_bootstrap(self) -> None:
        for st in POLICY["statements"]:
            aliases = (st.get("Condition") or {}).get("ForAnyValue:StringEquals", {}).get("kms:ResourceAliases")
            for res in sup.as_list(st["Resource"]):
                if res == "*":
                    continue
                if aliases:
                    # A KMS key ARN holds no name; the alias condition names the stage's data key.
                    self.assertEqual(sup.as_list(aliases), ["alias/hamh-direct-data"], st["Sid"])
                    continue
                self.assertRegex(res, r"hamh-setup-|hamh-direct|hamh_direct_|/hamh/|cdk-hnb659fds|cdk-bootstrap/hnb659fds", f"{st['Sid']}: {res}")


class TableCase(unittest.TestCase):
    def setUp(self) -> None:
        need(self)

    def test_entries_have_the_fields_and_a_script_that_exists(self) -> None:
        for c in POLICY["calls"]:
            self.assertIn(c["script"], sup.RUNNER_SCRIPTS)
            self.assertIn(c["service"], sup.KNOWN_SERVICES)
            self.assertRegex(c["operation"], r"^[a-z][a-z0-9-]*$")
            self.assertIsInstance(c["actions"], list)
            ways = [k for k in ("local", "open", "role", "guarded") if k in c]
            self.assertLessEqual(len(ways), 1, f"{c['script']} {c['service']} {c['operation']}: {ways}")
            if ways:
                self.assertGreaterEqual(len(c[ways[0]]) if isinstance(c[ways[0]], str) else 1, 1)

    def test_no_entry_twice(self) -> None:
        keys = [(c["script"], c["service"], c["operation"]) for c in POLICY["calls"]]
        self.assertEqual(len(keys), len(set(keys)))

    def test_every_call_of_every_script_is_in_the_table(self) -> None:
        for rel in sup.RUNNER_SCRIPTS:
            if not (sup.REPO / rel).is_file():
                continue
            missing = sorted(sup.scan_calls(rel) - sup.table_pairs(POLICY, rel))
            self.assertEqual(missing, [], f"{rel} calls the AWS API {missing}: add it to the calls of infra/setup-runner-policy.json with the IAM actions it needs, and to the statements")

    def test_the_table_lists_no_call_that_is_gone(self) -> None:
        for rel in sup.RUNNER_SCRIPTS:
            if not (sup.REPO / rel).is_file():
                continue
            stale = sorted(sup.table_pairs(POLICY, rel) - sup.scan_calls(rel))
            self.assertEqual(stale, [], f"{rel} no longer makes the call(s) {stale}: remove them from infra/setup-runner-policy.json")

    def test_a_script_that_gains_a_call_fails_the_check(self) -> None:
        rel = "scripts/lib/deploy.py"
        text = sup.read(rel) + '\n        self.aws("lambda", "invoke", "--function-name", "x")\n'
        added = sup.scan_calls(rel, text) - sup.table_pairs(POLICY, rel)
        self.assertEqual(added, {("lambda", "invoke")})
        rel = "scripts/codebuild-run.sh"
        text = sup.read(rel) + '\n"$aws_bin" iam list-roles --region "$region"\n'
        self.assertEqual(sup.scan_calls(rel, text) - sup.table_pairs(POLICY, rel), {("iam", "list-roles")})
        text = sup.read("scripts/setup/runner-build.sh") if (sup.REPO / "scripts/setup/runner-build.sh").is_file() else ""
        added = sup.scan_calls("scripts/setup/runner-build.sh", text + "\naws secretsmanager get-secret-value --secret-id x\n") - sup.table_pairs(POLICY, "scripts/setup/runner-build.sh")
        self.assertEqual(added, {("secretsmanager", "get-secret-value")})

    def test_the_scan_finds_the_known_calls(self) -> None:
        deploy = sup.scan_calls("scripts/lib/deploy.py")
        for pair in (("sts", "get-caller-identity"), ("ssm", "get-parameter"), ("s3", "cp"), ("s3api", "put-object"), ("ec2", "describe-vpcs"),
                     ("bedrock-agentcore-control", "create-agent-runtime-endpoint"), ("stepfunctions", "list-executions"), ("dynamodb", "get-item"),
                     ("bedrock", "put-model-invocation-logging-configuration"), ("cloudformation", "describe-stacks"), ("ecr", "describe-images")):
            self.assertIn(pair, deploy)
        self.assertEqual(sup.scan_calls("scripts/codebuild-run.sh"),
                         {("s3", "cp"), ("codebuild", "start-build"), ("codebuild", "batch-get-builds"), ("logs", "get-log-events")})
        self.assertIn(("codeartifact", "get-authorization-token"), sup.scan_calls("scripts/scanner_kit_rules.py"))
        self.assertIn(("sts", "assume-role"), sup.scan_calls("scripts/lib/gomod_store.py"))
        self.assertIn(("s3api", "put-object"), sup.scan_calls("scripts/lib/gomod_store.py"))

    def test_the_service_list_covers_the_table(self) -> None:
        for c in POLICY["calls"]:
            self.assertIn(c["service"], sup.KNOWN_SERVICES)


class ActionsCase(unittest.TestCase):
    def setUp(self) -> None:
        need(self)

    def runner_calls(self):
        return [c for c in POLICY["calls"] if not {"local", "open", "role", "guarded"} & set(c)]

    def test_every_action_a_runner_call_needs_is_allowed(self) -> None:
        for c in self.runner_calls():
            self.assertTrue(c["actions"], f"{c['script']} {c['service']} {c['operation']}: name the IAM actions it needs")
            for action in c["actions"]:
                self.assertTrue(sup.is_allowed(POLICY["statements"], action), f"{c['script']} {c['service']} {c['operation']} needs {action}, which no statement allows")

    def test_a_local_call_or_one_made_under_another_role_needs_no_runner_action(self) -> None:
        for c in POLICY["calls"]:
            if {"local", "open", "role"} & set(c):
                self.assertEqual(c["actions"], [], f"{c['script']} {c['service']} {c['operation']}")

    def test_a_guarded_call_is_explained_and_not_allowed(self) -> None:
        guarded = [c for c in POLICY["calls"] if "guarded" in c]
        self.assertTrue(guarded)
        for c in guarded:
            self.assertGreaterEqual(len(c["guarded"]), 40, "say which check stops the call")
            self.assertEqual(c["actions"], [])
            self.assertTrue(c.get("denied"), "name the IAM action the runner is not given")
            for action in c["denied"]:
                self.assertFalse(sup.is_allowed(POLICY["statements"], action), f"{action} is allowed")

    def test_the_gomod_filler_calls_name_the_role(self) -> None:
        roles = {c["role"] for c in POLICY["calls"] if "role" in c}
        self.assertEqual(roles, {"hamh-direct-gomod-filler"})

    def test_the_platform_actions_are_explained(self) -> None:
        for action, reason in POLICY["platform"].items():
            self.assertGreaterEqual(len(reason), 30, f"{action}: say what CodeBuild itself does with it")
            self.assertTrue(sup.is_allowed(POLICY["statements"], action), f"{action} is not allowed")

    def test_every_allowed_action_is_used_by_a_call_or_by_codebuild(self) -> None:
        needed = {a for c in self.runner_calls() for a in c["actions"]} | set(POLICY["platform"])
        for st in POLICY["statements"]:
            if st["Effect"] != "Allow":
                continue
            for action in sup.as_list(st["Action"]):
                self.assertIn(action, needed, f"{st['Sid']} allows {action}, which no call of the table needs")


class BoundaryCase(unittest.TestCase):
    def setUp(self) -> None:
        need(self)

    def test_the_boundary_covers_every_action_of_the_statements(self) -> None:
        for st in POLICY["statements"]:
            if st["Effect"] != "Allow":
                continue
            for action in sup.as_list(st["Action"]):
                self.assertTrue(sup.is_allowed(POLICY["boundary"], action), f"{st['Sid']}: the boundary does not allow {action}")

    def test_the_boundary_covers_every_resource_of_the_statements_and_names_resources(self) -> None:
        boundary = [s for s in POLICY["boundary"] if s["Effect"] == "Allow"]
        for st in POLICY["statements"]:
            if st["Effect"] != "Allow":
                continue
            for action in sup.as_list(st["Action"]):
                patterns = [r for b in boundary if sup.statement_allows(b, action) for r in sup.as_list(b["Resource"])]
                for res in sup.as_list(st["Resource"]):
                    self.assertTrue(any(sup.pattern_matches(p, res) for p in patterns), f"{st['Sid']}: no boundary statement allows {action} on {res}")
        for b in boundary:
            if sup.as_list(b["Resource"]) == ["*"]:
                self.assertIn("Condition", b, f"{b['Sid']} allows every resource with no condition")
                self.assertTrue(set(sup.as_list(b["Action"])) <= set(POLICY["unscopable"]), b["Sid"])

    def test_the_boundary_is_a_ceiling_that_leaves_out_iam(self) -> None:
        for action in ("iam:CreateRole", "iam:PutRolePolicy", "iam:CreatePolicyVersion", "iam:DeleteRolePermissionsBoundary", "iam:AttachRolePolicy",
                       "organizations:DescribeOrganization", "account:GetContactInformation", "lambda:InvokeFunction", "ec2:RunInstances",
                       "s3:DeleteObject", "s3:PutBucketPolicy", "cloudformation:CreateStack", "cloudformation:DeleteStack"):
            self.assertFalse(sup.is_allowed(POLICY["boundary"], action), f"the boundary allows {action}")

    def test_the_boundary_denies_iam_organizations_and_account(self) -> None:
        for service in ("iam", "organizations", "account"):
            self.assertTrue(any(s["Effect"] == "Deny" and f"{service}:*" in sup.as_list(s["Action"]) for s in POLICY["boundary"]), service)


class TemplateAgreementCase(unittest.TestCase):
    def setUp(self) -> None:
        need(self)
        if not sup.TEMPLATE_PATH.is_file():
            self.fail("infra/setup-runner.yaml does not exist")

    def runner_policy_statements(self):
        role = TEMPLATE["Resources"]["RunnerRole"]["Properties"]
        policies = role["Policies"]
        self.assertEqual(len(policies), 1)
        return policies[0]["PolicyDocument"]["Statement"]

    def boundary_statements(self):
        return TEMPLATE["Resources"]["RunnerBoundary"]["Properties"]["PolicyDocument"]["Statement"]

    def test_the_role_policy_is_the_policy_file(self) -> None:
        self.assertEqual(sup.canonical_statements(self.runner_policy_statements()), sup.canonical_statements(POLICY["statements"]))

    def test_the_boundary_is_the_boundary_of_the_file(self) -> None:
        self.assertEqual(sup.canonical_statements(self.boundary_statements()), sup.canonical_statements(POLICY["boundary"]))


if __name__ == "__main__":
    unittest.main()
