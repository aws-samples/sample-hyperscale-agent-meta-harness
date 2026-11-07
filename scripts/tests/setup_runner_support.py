"""Shared helpers of the setup runner tests: the template, the policy file, the buildspec, the IAM wildcard matching and the scan of the scripts for AWS CLI calls.

Test support, not a test. The tests that use it: `test_setup_runner_template.py`, `test_setup_runner_policy.py`, `test_setup_runner_buildspec.py` and
`test_setup_runner_build.py`.

The scan (`scan_calls`) is how a script that starts to call an AWS API the runner role does not allow is found. It reads the text of one script and
returns the (service, operation) pairs of the AWS CLI calls in it. Python files are read for a list or a call whose first two strings are an AWS CLI
service and an operation (`self.aws("ssm", "get-parameter")`, `[self.aws_bin, "s3", "cp"]`, `aws.json(["codeartifact", "describe-package"])`), shell
files for `aws <service> <operation>` and `"$aws_bin" <service> <operation>`. A pair counts when its first word is in KNOWN_SERVICES, a list of AWS CLI
service names; a call to a service that is in neither the list nor the policy file's table is not found, so a new service name is added to the list
when the table gains an entry for it (`test_the_service_list_covers_the_table`).
"""

from __future__ import annotations

import fnmatch
import json
import re
import sys
from pathlib import Path
from typing import Any, Dict, Iterable, List, Set, Tuple

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
sys.path.insert(0, str(HERE))
import cfn_yaml  # noqa: E402

TEMPLATE_PATH = REPO / "infra" / "setup-runner.yaml"
POLICY_PATH = REPO / "infra" / "setup-runner-policy.json"
BUILDSPEC_PATH = REPO / "images" / "codebuild" / "setup-runner.yml"
BUILD_SCRIPT = REPO / "scripts" / "setup" / "runner-build.sh"
TOOLCHAIN_SCRIPT = REPO / "scripts" / "setup" / "runner-toolchain.sh"
STATE_HELPER = REPO / "scripts" / "setup" / "runner_state.py"

PROJECT_TAG = "hyperscale-agent-meta-harness"

# The scripts the runner role's credentials reach. A script the user runs with their own credentials (empty-bucket.sh) is not in the list.
RUNNER_SCRIPTS = (
    "scripts/lib/deploy.py",
    "scripts/codebuild-run.sh",
    "scripts/lib/gomod_store.py",
    "scripts/scanner_kit_rules.py",
    "scripts/node.sh",
    "scripts/setup/runner-build.sh",
    "scripts/setup/runner-toolchain.sh",
    "scripts/setup/build-client.sh",
)

KNOWN_SERVICES = frozenset("""
account accessanalyzer acm apigateway apigatewayv2 athena autoscaling bedrock bedrock-agent bedrock-agentcore bedrock-agentcore-control bedrock-runtime
budgets ce cloudformation cloudfront cloudtrail cloudwatch codeartifact codebuild codecommit codedeploy codepipeline cognito-identity cognito-idp
configure dynamodb ec2 ecr ecr-public ecs efs eks elb elbv2 events firehose glue health iam identitystore kinesis kms lambda logs
network-firewall organizations pricing rds resourcegroupstaggingapi route53 route53resolver s3 s3api s3control sagemaker secretsmanager servicequotas ses sesv2
sns sqs ssm sso sso-admin stepfunctions sts support trustedadvisor wafv2
""".split())

PY_PAIR = re.compile(r"""[\[(]\s*(?:self\.aws_bin,\s*)?"([a-z0-9-]+)",\s*"([a-z0-9-]+)\"""")
SH_PAIR = re.compile(r"""(?:^|[\s;&|(])(?:aws|"\$aws_bin"|\$aws_bin|"\$\{AWS_BIN\}"|\$\{AWS_BIN\})\s+([a-z][a-z0-9-]*)\s+([a-z][a-z0-9-]*)""", re.M)


def read(rel: str) -> str:
    return (REPO / rel).read_text(encoding="utf-8")


def template() -> Dict[str, Any]:
    return cfn_yaml.load(TEMPLATE_PATH.read_text(encoding="utf-8"))


def policy() -> Dict[str, Any]:
    return json.loads(POLICY_PATH.read_text(encoding="utf-8"))


def buildspec() -> Dict[str, Any]:
    return cfn_yaml.load(BUILDSPEC_PATH.read_text(encoding="utf-8"))


# ---- the template's intrinsic functions, in the policy file's notation ----

PSEUDO = {"AWS::Partition": "{partition}", "AWS::Region": "{region}", "AWS::AccountId": "{account}", "AuthorityExpires": "{authorityExpires}"}


def sub_string(text: str) -> str:
    """A Fn::Sub string with its pseudo parameters written as the tokens the policy file uses."""
    for name, token in PSEUDO.items():
        text = text.replace("${" + name + "}", token)
    return text


def normalise(value: Any) -> Any:
    """The value with every Ref to a pseudo parameter and every Fn::Sub over pseudo parameters turned into the policy file's tokens; any other
    intrinsic stays as a one-key dictionary so a comparison with the file fails on it."""
    if isinstance(value, dict):
        if set(value) == {"Fn::Sub"} and isinstance(value["Fn::Sub"], str):
            return sub_string(value["Fn::Sub"])
        if set(value) == {"Ref"} and value["Ref"] in PSEUDO:
            return PSEUDO[value["Ref"]]
        return {k: normalise(v) for k, v in value.items()}
    if isinstance(value, list):
        return [normalise(v) for v in value]
    return value


def as_list(value: Any) -> List[Any]:
    if value is None:
        return []
    return value if isinstance(value, list) else [value]


def canonical_statement(st: Dict[str, Any]) -> Dict[str, Any]:
    """A statement with lists sorted, for a comparison that ignores the order of actions and resources."""
    out: Dict[str, Any] = {}
    for key, val in st.items():
        if key in ("Action", "Resource", "NotAction", "NotResource"):
            out[key] = sorted(as_list(val))
        elif key == "Condition":
            out[key] = {op: {k: (sorted(as_list(v)) if isinstance(v, list) else v) for k, v in cond.items()} for op, cond in val.items()}
        else:
            out[key] = val
    return out


def canonical_statements(statements: Iterable[Dict[str, Any]]) -> Dict[str, Dict[str, Any]]:
    return {st["Sid"]: canonical_statement(normalise(st)) for st in statements}


# ---- IAM matching, without conditions ----

def pattern_matches(pattern: str, name: str) -> bool:
    return fnmatch.fnmatchcase(name.lower(), pattern.lower())


def statement_allows(statement: Dict[str, Any], action: str) -> bool:
    return statement.get("Effect") == "Allow" and any(pattern_matches(p, action) for p in as_list(statement.get("Action")))


def statement_denies(statement: Dict[str, Any], action: str) -> bool:
    return statement.get("Effect") == "Deny" and any(pattern_matches(p, action) for p in as_list(statement.get("Action")))


def is_allowed(statements: Iterable[Dict[str, Any]], action: str) -> bool:
    sts = list(statements)
    return any(statement_allows(s, action) for s in sts) and not any(statement_denies(s, action) for s in sts)


# ---- the scan of the scripts ----

def scan_calls(rel: str, text: str | None = None) -> Set[Tuple[str, str]]:
    """The (service, operation) pairs of the AWS CLI calls in the script `rel` (or in `text` standing for it)."""
    body = read(rel) if text is None else text
    pairs: Set[Tuple[str, str]] = set()
    if rel.endswith(".py"):
        for m in PY_PAIR.finditer(body):
            if m.group(1) in KNOWN_SERVICES:
                pairs.add((m.group(1), m.group(2)))
    else:
        for m in SH_PAIR.finditer(body):
            if m.group(1) in KNOWN_SERVICES:
                pairs.add((m.group(1), m.group(2)))
    return pairs


def table_pairs(doc: Dict[str, Any], rel: str) -> Set[Tuple[str, str]]:
    return {(c["service"], c["operation"]) for c in doc["calls"] if c["script"] == rel}
