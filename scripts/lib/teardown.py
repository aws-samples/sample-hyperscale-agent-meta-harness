"""scripts/teardown.sh: tear one stage down and prove what is left (within the stage limits).

  teardown.sh --stage direct [--e2e-only] [--dry-run] [--yes] [--names new|old|both] [--network-wait-minutes N|full]

Names: a stage can be deployed under the current names or under the names that releases before the rename used. A generation
owns a name only when the name prefix and the project tag are both its own: a stack with the prefix of one and the tag of the other is foreign, is named on a `kept:` line
and is never touched. `--names new|old|both` (default both) selects the generations a run covers. The previous generation is removed through CloudFormation (the CDK app
defines the current names only), its evidence and record carry `-previous-names` in their names, and `both` reads the previous generation once first: a clean one adds
no evidence file. The last line is the zero-resource line only when every selected generation reads zero; with `--names new` it says the previous names were not covered.

Anything but --dry-run needs --yes (exit 2 before any AWS call). --dry-run makes read calls only
and prints `teardown: plan ...` for every change a real run would make. The account must be the
selected accounts.json entry's (`TEARDOWN-FAIL account <id>`, exit 1, nothing changed).

Full, in order (a failed call is printed as `teardown: failed: <service> <operation> ...` and the
teardown goes on; the inventory shows what it left):
1. Read the CDK asset record s3://<harness bucket>/deploy/<stage>/cdk-assets.json first: the
   bucket that holds it is emptied later.
2. StopRuntimeSession for every Agent item in hamh-<stage>-harness whose status is not `stopped` and
   whose runtime is the stage's (`runtime/hamh_<stage>_`), at the endpoint its run is pinned to (the
   item's `runtimeEndpoint` when it is an endpoint name, else `live`).
3. Stop every RUNNING execution of the hamh-<stage>-* state machines.
4. `cdk destroy <stack> --exclusively --force` (through scripts/node.sh with HAMH_LAMBDA_DIR set to
   infra/test/fixtures/lambda) of every existing hamh-<stage>-* stack in reverse dependency order
   (from `cdk synth`). Before each, the stack's CloudTrail trails stop logging (CloudTrail keeps
   writing into its bucket until then), its S3 buckets (from list-stack-resources) lose every
   object version and delete marker (the Go module store's, under Object Lock governance, with the bypass flag; a status-0 answer
   that lists refused versions is a failure) and its ECR repositories every image, because CloudFormation
   cannot delete either while they hold data. The network stack is retried for up to `--network-wait-minutes` (0 to 480, or `full` for 480; TEARDOWN_NETWORK_TIMEOUT_S, in
   seconds, is the default and the test knob; 3,600 s without either), looking at the interfaces of the stage VPC every TEARDOWN_RETRY_SLEEP_S (60 s) while detached
   customer ENIs of the stage VPC are deleted; requester-managed (AgentCore) ENIs are waited for, never deleted.
   A hamh-<stage>-* stack the app
   no longer defines is deleted through CloudFormation. Before the runtimes stack, the `v<N>`
   endpoints deploy.sh made on the stage's runtimes are deleted (CloudFormation did not create them,
   and a runtime with them cannot be deleted).
   search provider: each target first, then the gateway once the last target is gone, because a gateway with a target cannot be
   deleted and a target goes asynchronously); the stage's tagged buckets (emptied, then
   deleted) and ECR repositories; log groups tagged for the stage in the three inference Regions
   and those under /aws/bedrock-agentcore/runtimes/hamh_<stage>, /aws/lambda/hamh-<stage>-
   and /aws/codebuild/hamh-<stage>- (the name is the ownership proof for groups a service created
   without tags); KMS keys tagged for the stage scheduled
   for deletion in 7 days (a longer schedule, such as CloudFormation's 30-day default, is cancelled
   and set to 7); tagged Secrets Manager secrets hamh/<stage>/* deleted without recovery (untagged
   ones are named and left); tagged SSM parameters /hamh/<stage>/* (deploy.sh's digests); every
   version of the recorded CDK asset objects and the recorded asset images; E2E repositories
   (below); the CodeBuild project hamh-<stage>-prcheck; EventBridge rules hamh-<stage>-* that no
   service manages (their targets first); the stage's tagged CloudTrail trails and CloudWatch
   alarms; CloudWatch dashboards hamh-<stage>-* and Logs Insights saved queries hamh-<stage>/*, which
   take no tags (the name is the ownership proof, as for service-made log groups).
6. Deferred retry: a stack still present is deleted again; when its failed resources are IAM
   roles, they are retained so the stack can go. Testacct's IAM deletion protection refuses an Testacct session the
   deletion of a role used in the last 24 hours: such a role is named with its last use (get-role RoleLastUsed) and
   "deletable after about <24 hours later>", kept in the record's measurements.kept, and stays in the inventory until a
   later run.

E2E repositories (full and --e2e-only): each CodeCommit repository hamh-e2e-* tagged for the stage
whose evidence copy is complete (TEARDOWN_EVIDENCE_DIR/e2e/codecommit/<repo>/ holding
pull-requests.json, approval-states.json, rule-evaluations.json, prcheck-builds.json and
branches.bundle, or a copy e2e-live.sh made before it replaced a fixed-name --new-repo target:
e2e-repos/<repo>-<utc>.json of kind `e2e-repo-copy` naming the repository, with this repository's
ID and creation date (`get-repository`; a repository made again under the same name has others), a
ref for every branch at its current head (`list-branches`, `get-branch`) and, when it has refs, the
bundle e2e-repos/<repo>-<utc>.bundle whose SHA-256 the record holds) has its open
PRs closed, its harness/* branches deleted and is then deleted (`Teardown.delete_e2e_repo`, which
e2e-live.sh calls for one repository); a repository without a complete copy, not named hamh-e2e-* or
not tagged for the stage is not touched.

--e2e-only: E2E runs are the Run items whose target is a hamh-e2e- CodeCommit repository and the
runs with a evidence copy TEARDOWN_EVIDENCE_DIR/e2e/runs/<runId>/. Their sessions are stopped and
their executions (named <runId> or <runId>-...) stopped, the E2E repositories handled as above, and
the runs/<runId>/ prefix of the harness bucket emptied once the run's evidence copy exists. No stack
is touched; the inventory covers the E2E repositories, run prefixes and executions.

Inventory (always): the Resource Groups Tagging API (project, stage) in the three Regions, with
KMS keys pending deletion counted apart; AgentCore runtimes and gateways by name; CodeArtifact
domains; Network Firewall firewalls; the stage VPC and its NAT gateways, addresses, VPC endpoints
and network interfaces (by tag and by VPC ID); log groups by the three prefixes;
the recorded CDK asset objects and images; IAM roles (path /hamh/<stage>/ and name hamh-<stage>-*,
tagged); Route 53 Resolver query log configs by name and their associations by VPC ID; CodeCommit
hamh-e2e-* repositories tagged for the stage; the prcheck project; EventBridge rules hamh-<stage>-*;
dashboards hamh-<stage>-* and saved queries hamh-<stage>/* by name (trails and alarms are tagged).
Each remaining resource prints `INVENTORY <region> <id>`; the last line is
`TEARDOWN-INVENTORY 0 resources stage=<stage> kms-pending=<n>` (exit 0) or
`TEARDOWN-INVENTORY <k> resources` (exit 1, or exit 9 when every remaining resource is a network resource that AgentCore's interface holds). The tagging index lags behind deletes: in a full run every ARN it lists is read
with its own service (EXISTENCE) and dropped only on that service's not-found answer (NOT_FOUND; recorded in
measurements.dropped); any other answer, and a type with no read, counts. A full run's inventory passes only after two
consecutive empty reads TEARDOWN_SETTLE_POLL_S apart, read for up to TEARDOWN_SETTLE_S while it is not empty (the last
read decides). A dry run and --e2e-only read once and check no ARN.

The invocation-logging configurations of the three Regions are read and recorded; the only write is the full teardown's restore: when SSM /hamh/<stage>/invocation-logging-prior holds the state deploy.sh found and the
home-Region setting still names /hamh/<stage>/bedrock-invocations, that state is put back (unset: the configuration is deleted) before any stack is destroyed. No Bedrock retention setting is touched.

Target: the accounts.json entry HAMH_AWS_PROFILE selects by profile, else the active one; its
account (the caller must be it), home Region and profile reach every call; an unknown profile is a usage
error (exit 2).

Test hooks: TEARDOWN_ROOT, TEARDOWN_AWS_BIN, TEARDOWN_NODE_BIN, TEARDOWN_EVIDENCE_DIR,
TEARDOWN_RETRY_SLEEP_S, TEARDOWN_NETWORK_TIMEOUT_S, TEARDOWN_SETTLE_S, TEARDOWN_SETTLE_POLL_S, TEARDOWN_CRITERIA.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import re
import math
import shlex
import socket
import subprocess  # nosec B404: starts programs with argument lists, never through a shell
import tempfile
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import aws_target  # noqa: E402

# The inventory Regions: criteria `inferenceRegions`.
REGIONS = aws_target.INFERENCE_REGIONS
PROJECT = "hyperscale-agent-meta-harness"
RECORDS = os.path.join(os.environ.get("XDG_STATE_HOME") or os.path.join(os.environ.get("HOME", "/nonexistent"), ".local", "state"), "ham")
QUALIFIER = "live"
# deploy.sh's version endpoints (dispatcher::endpoints): `v<N>`.
VERSION_ENDPOINT = re.compile(r"v[1-9][0-9]*")
# An AgentCore endpoint name; anything else (a bare version) is never a qualifier.
ENDPOINT_NAME = re.compile(r"[a-zA-Z][a-zA-Z0-9_]{0,47}")
E2E_REPO_PREFIX = "hamh-e2e-"
E2E_REPO_COPY = ("pull-requests.json", "approval-states.json", "rule-evaluations.json", "prcheck-builds.json",
                 "branches.bundle")
CDK_ASSET_BUCKET_PREFIX = "cdk-hnb659fds-assets-"
CDK_IMAGE_REPO_PREFIX = "cdk-hnb659fds-container-assets-"
KMS_WINDOW_DAYS = 7
# The invocation-logging window of a stage tags its role `hamh-<stage>-invwin-<region>` with the
# project tag and this marker, `<stage>/<region>`, and no stage tag.
WINDOW_MARKER = "hamh-invlog-window"
# The generations of names.
GENERATIONS = {
    "new": {"word": "hamh", "project": PROJECT, "e2e": E2E_REPO_PREFIX, "window": WINDOW_MARKER},
    "old": {"word": "cah", "project": "cline-agentcore-harness", "e2e": "cah-e2e-", "window": "cah-invlog-window"},  # COMPAT-CAH-STAGE
}
# how a message names a generation
GENERATION_WORDS = {"new": "current names", "old": "previous names"}
# deploy.sh's digests of the stage (/hamh/<stage>/<name>), read for the record's header before any delete.
DIGEST_PARAMS = ("deployment-digest", "deployed-source-tree", "image-digest")
# The files whose bytes the teardown runs (the record's inputs.sources; R4 compares each with code HEAD).
SOURCES = ("scripts/teardown.sh", "scripts/lib/teardown.py", "scripts/lib/aws_target.py", "scripts/lib/python.sh")
# AgentCore keeps one managed network interface (type agentic_ai) in the VPC of the stage after its runtime is deleted, and AWS releases it up to 8 hours later.
AGENTCORE_ENI_TYPE = "agentic_ai"
NETWORK_WAIT_MAX_MINUTES = 480
NETWORK_WAIT_DEFAULT_S = 3600.0
# While AgentCore's interface is listed the network stack is not tried, except on every 10th look: a listing that is stale must not keep the delete away for good.
NETWORK_ATTEMPT_EVERY = 10
# A resource of the stage's network: the EC2 resources of the VPC and the network stack that made them.
NETWORK_ARN = re.compile(r"arn:aws[a-z-]*:ec2:[^:]*:[0-9]*:(vpc|subnet|security-group|security-group-rule|route-table|internet-gateway|egress-only-internet-gateway|natgateway|network-interface|"
                         r"vpc-endpoint|elastic-ip|network-acl|dhcp-options|vpc-flow-log|prefix-list)/")
# Tie-break for the destroy order (the reverse of scripts/lib/deploy.py's DEPLOY_PRIORITY).
PRIORITY = ("network", "security", "data", "packages", "control", "runtimes", "orchestration", "delivery", "observability")


class Fail(Exception):
    pass


def say(msg: str) -> None:
    print(f"teardown: {msg}", flush=True)


def wait_minutes(text: str) -> int | None:
    """`--network-wait-minutes`: whole minutes 0 to 480, or `full` (480). None for anything else."""
    if text == "full":
        return NETWORK_WAIT_MAX_MINUTES
    if re.fullmatch(r"[0-9]{1,3}", text) and int(text) <= NETWORK_WAIT_MAX_MINUTES:
        return int(text)
    return None


def wait_seconds_from_env(value: str | None) -> float | None:
    """TEARDOWN_NETWORK_TIMEOUT_S: seconds, 0 to 28,800; the default when it is not set. None for a value that is no such number."""
    if value is None or value == "":
        return NETWORK_WAIT_DEFAULT_S
    try:
        seconds = float(value)
    except ValueError:
        return None
    return seconds if math.isfinite(seconds) and 0 <= seconds <= NETWORK_WAIT_MAX_MINUTES * 60 else None


def minutes_text(seconds: float) -> str:
    n = int(seconds // 60)
    return f"{n} minute{'' if n == 1 else 's'}"


def listed(answer: dict, member: str) -> list:
    """The list that a `list-*` answer carries under `member` (an empty list when it has none). The member is found by comparing names, not by a lookup
    with a literal key: CodeQL reads a key that holds the word secret as secret data and then reports every message that prints a name from the list
. The `list-secrets` answer has the secrets' names, ARNs and tags and never a value."""
    return next((value for key, value in answer.items() if key == member), [])


def instant(v) -> float | None:
    """A CLI timestamp (epoch seconds, or ISO 8601 as CLI v2 prints by default) as epoch seconds."""
    if isinstance(v, bool) or v is None:
        return None
    if isinstance(v, (int, float)):
        return float(v)
    try:
        return float(v)
    except (TypeError, ValueError):
        pass
    try:
        t = dt.datetime.fromisoformat(str(v).replace("Z", "+00:00"))
    except ValueError:
        return None
    return (t if t.tzinfo else t.replace(tzinfo=dt.timezone.utc)).timestamp()


def same_instant(a, b) -> bool:
    x, y = instant(a), instant(b)
    return x is not None and y is not None and abs(x - y) < 0.001


def tag_map(tags) -> dict[str, str]:
    if isinstance(tags, dict):
        return dict(tags)
    return {t.get("Key", t.get("key")): t.get("Value", t.get("value")) for t in tags or []}


def chunks(items: list, n: int):
    for i in range(0, len(items), n):
        yield items[i:i + n]


def iso(seconds: float) -> str:
    return dt.datetime.fromtimestamp(seconds, dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _absent_log_group(doc: dict, m) -> bool:
    return m.group(1) not in [g.get("logGroupName") for g in doc.get("logGroups", [])]


def _ec2(kind: str) -> list[str]:
    op, flag = {"vpc": ("describe-vpcs", "--vpc-ids"), "subnet": ("describe-subnets", "--subnet-ids"),
                "security-group": ("describe-security-groups", "--group-ids"),
                "network-interface": ("describe-network-interfaces", "--network-interface-ids"),
                "vpc-endpoint": ("describe-vpc-endpoints", "--vpc-endpoint-ids"), "route-table": ("describe-route-tables", "--route-table-ids"),
                "internet-gateway": ("describe-internet-gateways", "--internet-gateway-ids"),
                "natgateway": ("describe-nat-gateways", "--nat-gateway-ids"), "elastic-ip": ("describe-addresses", "--allocation-ids"),
                # The machines and disks of an Instances capacity provider (propagated tags put the project tags on them).
                "instance": ("describe-instances", "--instance-ids"), "volume": ("describe-volumes", "--volume-ids")}[kind]
    # An AgentCore-managed machine is hidden from describe-instances unless the caller asks for managed resources.
    return ["ec2", op, *(["--include-managed-resources"] if kind == "instance" else []), flag]


# The existence read of each ARN type the Tagging API lists for a stage: (ARN pattern, the read call, the error codes that
# mean "does not exist", a test of a successful answer that means the same). The inventory drops a tagged ARN only on one
# of these answers (the index lags behind deletes); any other answer, and a type with no row here, counts.
EXISTENCE = (
    (re.compile(r"^arn:aws:s3:::([^/]+)$"), lambda m, a: ["s3api", "head-bucket", "--bucket", m.group(1)],  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
     ("(404)", "Not Found", "NoSuchBucket"), None),
    # The AgentCore Gateway of the agentcore search provider; its id is the last part of the ARN.
    (re.compile(r"^arn:aws:bedrock-agentcore:[^:]+:[0-9]+:gateway/([^/]+)$"),
     lambda m, a: ["bedrock-agentcore-control", "get-gateway", "--gateway-identifier", m.group(1)], ("ResourceNotFoundException",), None),  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
    (re.compile(r"^arn:aws:ssm:[^:]+:[0-9]+:parameter(/.+)$"), lambda m, a: ["ssm", "get-parameter", "--name", m.group(1)],  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
     ("ParameterNotFound",), None),
    (re.compile(r"^arn:aws:logs:[^:]+:[0-9]+:log-group:([^:]+)(?::\*)?$"),
     lambda m, a: ["logs", "describe-log-groups", "--log-group-name-prefix", m.group(1)], (), _absent_log_group),  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
    (re.compile(r"^arn:aws:lambda:[^:]+:[0-9]+:function:[^:]+$"), lambda m, a: ["lambda", "get-function", "--function-name", a],  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
     ("ResourceNotFoundException",), None),
    (re.compile(r"^arn:aws:dynamodb:[^:]+:[0-9]+:table/([^/]+)$"), lambda m, a: ["dynamodb", "describe-table", "--table-name", m.group(1)],  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
     ("ResourceNotFoundException",), None),
    # A table's stream has no read of its own; the table's absence means the stream is gone (the Tagging index keeps
    # `table/<name>/stream/<created>` after the table is deleted). A table that answers leaves the stream counted.
    (re.compile(r"^arn:aws:dynamodb:[^:]+:[0-9]+:table/([^/]+)/stream/[^/]+$"),
     lambda m, a: ["dynamodb", "describe-table", "--table-name", m.group(1)], ("ResourceNotFoundException",), None),  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
    (re.compile(r"^arn:aws:states:[^:]+:[0-9]+:stateMachine:"), lambda m, a: ["stepfunctions", "describe-state-machine",  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
                                                                            "--state-machine-arn", a], ("StateMachineDoesNotExist",), None),
    (re.compile(r"^arn:aws:ecr:[^:]+:[0-9]+:repository/(.+)$"), lambda m, a: ["ecr", "describe-repositories", "--repository-names",  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
                                                                             m.group(1)], ("RepositoryNotFoundException",), None),
    (re.compile(r"^arn:aws:events:[^:]+:[0-9]+:rule/([^/]+)$"), lambda m, a: ["events", "describe-rule", "--name", m.group(1)],  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
     ("ResourceNotFoundException",), None),
    (re.compile(r"^arn:aws:secretsmanager:"), lambda m, a: ["secretsmanager", "describe-secret", "--secret-id", a],  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
     ("ResourceNotFoundException",), None),
    (re.compile(r"^arn:aws:cloudformation:[^:]+:[0-9]+:stack/"), lambda m, a: ["cloudformation", "describe-stacks", "--stack-name", a],  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
     ("does not exist",), lambda d, m: all(x.get("StackStatus") == "DELETE_COMPLETE" for x in d.get("Stacks", []))),  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
    (re.compile(r"^arn:aws:cloudwatch:[^:]+:[0-9]+:alarm:(.+)$"), lambda m, a: ["cloudwatch", "describe-alarms", "--alarm-names",  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
                                                                               m.group(1)], (),
     lambda d, m: not d.get("MetricAlarms") and not d.get("CompositeAlarms")),  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
    (re.compile(r"^arn:aws:codebuild:[^:]+:[0-9]+:project/(.+)$"), lambda m, a: ["codebuild", "batch-get-projects", "--names", m.group(1)],  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
     (), lambda d, m: not d.get("projects")),  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
    (re.compile(r"^arn:aws:cloudtrail:[^:]+:[0-9]+:trail/"), lambda m, a: ["cloudtrail", "get-trail", "--name", a],  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
     ("TrailNotFoundException",), None),
    (re.compile(r"^arn:aws:sqs:[^:]+:[0-9]+:([^:/]+)$"), lambda m, a: ["sqs", "get-queue-url", "--queue-name", m.group(1)],  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
     ("NonExistentQueue", "QueueDoesNotExist"), None),
    (re.compile(r"^arn:aws:sns:[^:]+:[0-9]+:([^:]+)$"), lambda m, a: ["sns", "get-topic-attributes", "--topic-arn", a], ("NotFound",), None),  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
    (re.compile(r"^arn:aws:ec2:[^:]+:[0-9]+:(vpc|subnet|security-group|network-interface|vpc-endpoint|route-table|internet-gateway|"
                r"natgateway|elastic-ip)/(.+)$"), lambda m, a: [*_ec2(m.group(1)), m.group(2)],  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
     ("InvalidVpcID.NotFound", "InvalidSubnetID.NotFound", "InvalidGroup.NotFound", "InvalidNetworkInterfaceID.NotFound",
      "InvalidVpcEndpointId.NotFound", "InvalidRouteTableID.NotFound", "InvalidInternetGatewayID.NotFound", "NatGatewayNotFound",
      "InvalidAllocationID.NotFound"),
     lambda d, m: m.group(1) == "natgateway" and all(n.get("State") == "deleted" for n in d.get("NatGateways", []))),  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
    # A capacity provider, and the machines and disks it launched. A terminated machine stays visible for about an hour: terminated is gone.
    (re.compile(r"^arn:aws:bedrock-agentcore:[^:]+:[0-9]+:capacity-provider/([^/]+)$"),
     lambda m, a: ["bedrock-agentcore-control", "get-capacity-provider", "--capacity-provider-id", m.group(1)], ("ResourceNotFoundException",), None),  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
    (re.compile(r"^arn:aws:ec2:[^:]+:[0-9]+:instance/(i-[0-9a-f]+)$"), lambda m, a: _ec2("instance") + [m.group(1)],  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
     ("InvalidInstanceID.NotFound",),
     lambda d, m: all(i.get("State", {}).get("Name") == "terminated" for r in d.get("Reservations", []) for i in r.get("Instances", []))),  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
    (re.compile(r"^arn:aws:ec2:[^:]+:[0-9]+:volume/(vol-[0-9a-f]+)$"), lambda m, a: _ec2("volume") + [m.group(1)],  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
     ("InvalidVolume.NotFound",), None),  # nosemgrep: return-not-in-function -- the return is the body of a lambda, an anonymous function, not a return outside a function
)
# Every answer the inventory drops an ARN on.
NOT_FOUND = tuple(sorted({c for _, _, codes, _ in EXISTENCE for c in codes} | {"absent"}))


class Teardown:
    def __init__(self, stage: str, e2e_only: bool, dry_run: bool, names: str = "new", network_wait_s: float | None = None, invocation: str = "") -> None:
        here = Path(__file__).resolve().parent
        self.root = Path(os.environ.get("TEARDOWN_ROOT") or here.parents[1])
        self.stage = stage
        self.names = names
        gen = GENERATIONS[names]
        self.word, self.project, self.e2e_prefix, self.window_marker = gen["word"], gen["project"], gen["e2e"], gen["window"]
        self.p = f"{self.word}-{stage}"
        self.u = f"{self.word}_{stage}"
        self.ssm = f"/{self.word}/{stage}"
        self.secret_root = f"{self.word}/{stage}/"
        self.foreign: list[str] = []
        # The accounts.json entry HAMH_AWS_PROFILE selects, else the active one. COMPAT-CAH: the previous spellings of the selectors are folded in first.
        environ = aws_target.fold_previous(os.environ)
        env = dict(environ)
        env.setdefault(aws_target.FILE_HOOK, str(self.root / "accounts.json"))
        self.target = aws_target.select(env)
        self.account, self.home = self.target.account, self.target.region
        self.bucket = f"{self.p}-harness-{self.account}"
        self.table = f"{self.p}-harness"
        self.e2e_only = e2e_only
        self.dry = dry_run
        self.aws_bin = os.environ.get("TEARDOWN_AWS_BIN", "aws")
        self.node = os.environ.get("TEARDOWN_NODE_BIN", str(Path(__file__).resolve().parents[1] / "node.sh"))
        self.evidence = Path(os.environ.get("TEARDOWN_EVIDENCE_DIR", f"{RECORDS}/evidence"))
        self.sleep_s = float(os.environ.get("TEARDOWN_RETRY_SLEEP_S", "60"))
        # The flag, else TEARDOWN_NETWORK_TIMEOUT_S (seconds), else one hour. main has checked both.
        self.network_timeout_s = network_wait_s if network_wait_s is not None else (wait_seconds_from_env(os.environ.get("TEARDOWN_NETWORK_TIMEOUT_S")) or 0.0)
        self.invocation = invocation
        self.network_hold: dict | None = None
        # A full run's inventory passes after two consecutive empty reads TEARDOWN_SETTLE_POLL_S apart, read for up to
        # TEARDOWN_SETTLE_S while it is not empty (resources still going, the tagging index lagging behind).
        self.settle_s = float(os.environ.get("TEARDOWN_SETTLE_S", "600"))
        self.poll_s = float(os.environ.get("TEARDOWN_SETTLE_POLL_S", "30"))
        self.criteria = Path(os.environ.get("TEARDOWN_CRITERIA", ""))
        # The selected entry's profile always: an inherited AWS_PROFILE never picks the account.
        self.env = dict(environ, AWS_PROFILE=self.target.profile)
        self.profile_selector = environ.get(aws_target.SELECTOR) or None
        self.commands: list[str] = []
        self.notes: list[str] = []
        self.record: dict = {"files": [], "images": []}
        self.vpcs: list[str] = []
        self.order: list[str] = []
        self.pre: dict[str, str | None] = {}
        self.kept: list[dict] = []
        self.dropped: list[dict] = []
        self.reads = 0

    # ---- AWS ----------------------------------------------------------------------------------

    def call(self, args: list[str], region: str | None = None) -> tuple[bool, str, str]:
        region = region or self.home
        proc = subprocess.run([self.aws_bin, *args, "--region", region, "--output", "json"], capture_output=True,  # nosec B603: argument list built here, no shell, program is git, the interpreter or a configured binary  # nosemgrep: dangerous-subprocess-use-audit -- argument list built here from its own options and paths, no shell
                              text=True, env=self.env)
        return proc.returncode == 0, proc.stdout, proc.stderr.strip()

    def read(self, *args: str, region: str | None = None, missing: tuple[str, ...] = ()) -> dict | None:
        """A read call's JSON; None when its error names one of `missing`. Other errors abort."""
        ok, out, err = self.call(list(args), region)
        if not ok:
            if any(m in err for m in missing):
                return None
            raise Fail(f"read {args[0]} {args[1]}: {err.splitlines()[-1] if err else 'failed'}")
        return json.loads(out) if out.strip() else {}

    def act(self, what: str, *args: str, region: str | None = None, ok_if: tuple[str, ...] = (), delete_errors: bool = False,
            retry_if: tuple[str, ...] = ()) -> bool:
        """A change: planned under --dry-run, else run; a failure is reported and returns False. `delete_errors`: the call is
        s3api delete-objects, which answers status 0 and lists the versions it refused under `Errors`; any such entry is a failure.
        `retry_if`: the call is repeated, every TEARDOWN_SETTLE_POLL_S for up to TEARDOWN_SETTLE_S, while its error text names one of these
        words (a refusal that goes away by itself, such as a delete the service refuses while something it depends on is still going);
        only the last answer is reported."""
        region = region or self.home
        line = f"{what} ({region})" if region != self.home else what
        if self.dry:
            self.commands.append(f"plan {line}")
            say(f"plan {line}")
            return True
        self.commands.append(f"aws {' '.join(args)} --region {region}")
        ok, out, err = self.call(list(args), region)
        if retry_if:
            deadline = time.monotonic() + self.settle_s
            while not ok and any(m in err for m in retry_if) and not any(m in err for m in ok_if) and time.monotonic() < deadline:
                time.sleep(self.poll_s)
                self.commands.append(f"aws {' '.join(args)} --region {region}")
                ok, out, err = self.call(list(args), region)
        if ok and delete_errors:
            try:
                refused = (json.loads(out) if out.strip() else {}).get("Errors") or []
            except ValueError:
                refused = []
            if refused:
                ok, err = False, f"{len(refused)} object version(s) refused: {refused[0].get('Code')} {refused[0].get('Key')}: {refused[0].get('Message', '')}"
        if ok or any(m in err for m in ok_if):
            say(line)
            return True
        detail = err.splitlines()[-1] if err else "failed"
        say(f"failed: {args[0]} {args[1]} {' '.join(args[2:4])}: {detail}")
        self.notes.append(f"failed: {args[0]} {args[1]} {' '.join(args[2:4])}: {detail}")
        return False

    def tagged(self, region: str) -> list[dict]:
        doc = self.read("resourcegroupstaggingapi", "get-resources", "--tag-filters",
                        f"Key=project,Values={self.project}", f"Key=stage,Values={self.stage}", region=region) or {}
        return [{"arn": m["ResourceARN"], "tags": tag_map(m.get("Tags"))} for m in doc.get("ResourceTagMappingList", [])]

    def ours(self, tags) -> bool:
        t = tag_map(tags)
        return t.get("project") == self.project and t.get("stage") == self.stage

    def guard(self, name: str, fn):
        """Runs one step; a failed read ends that step only, and the inventory shows what it left."""
        try:
            return fn()
        except Fail as e:
            msg = f"step {name} failed: {e}"
            say(msg)
            self.notes.append(msg)
            return None

    # ---- node / cdk ---------------------------------------------------------------------------

    def node_run(self, *args: str) -> int:
        env = dict(self.env, HAMH_LAMBDA_DIR=str(self.root / "infra" / "test" / "fixtures" / "lambda"))
        return subprocess.run([self.node, *args], cwd=self.root / "infra", env=env).returncode  # nosec B603: argument list built here, no shell, program is git, the interpreter or a configured binary  # nosemgrep: dangerous-subprocess-use-audit -- argument list built here from its own options and paths, no shell

    def stack_order(self) -> list[str]:
        """The app's hamh-<stage>-* stacks, dependencies first (from cdk synth's manifest)."""
        if self.node_run("pnpm", "install", "--frozen-lockfile") != 0:
            raise Fail("infra-install pnpm install failed")
        if self.node_run("cdk", "synth", "--quiet", "-c", f"stage={self.stage}") != 0:
            raise Fail("synth cdk synth failed")
        manifest = json.loads((self.root / "infra" / "cdk.out" / "manifest.json").read_text())
        arts = manifest.get("artifacts", {})
        stacks = {n: a for n, a in arts.items() if a.get("type") == "aws:cloudformation:stack" and n.startswith(self.p + "-")}
        deps = {n: {d for d in a.get("dependencies", []) if d in stacks} for n, a in stacks.items()}
        rank = {f"{self.p}-{n}": i for i, n in enumerate(PRIORITY)}
        order: list[str] = []
        while deps:
            ready = sorted((n for n, d in deps.items() if not d), key=lambda n: (rank.get(n, len(rank)), n))
            if not ready:
                raise Fail("synth stack dependency cycle")
            order.append(ready[0])
            deps.pop(ready[0])
            for d in deps.values():
                d.discard(ready[0])
        return order

    def cfn_stacks(self) -> dict[str, str]:
        """The stage's stacks of this generation: the name has its prefix AND the stack carries its project tag and the stage tag. A stack with the prefix and other tags
        (the tag of the other generation, or none) is foreign: `kept:` line, never touched."""
        doc = self.read("cloudformation", "list-stacks") or {}
        out = {}
        for s in doc.get("StackSummaries", []):
            name = s["StackName"]
            if not name.startswith(self.p + "-") or s["StackStatus"] == "DELETE_COMPLETE":
                continue
            described = self.read("cloudformation", "describe-stacks", "--stack-name", name, missing=("does not exist",)) or {}
            tags = ((described.get("Stacks") or [{}])[0]).get("Tags")
            if self.ours(tags):
                out[name] = s["StackStatus"]
            else:
                msg = f"kept: {name} has the prefix {self.word}- but lacks project={self.project} or stage={self.stage}; teardown does not touch it."
                if msg not in self.foreign:
                    self.foreign.append(msg)
                    say(msg)
                    self.notes.append(msg)
        return out

    # ---- storage ------------------------------------------------------------------------------

    def versions(self, bucket: str, prefix: str = "") -> list[dict] | None:
        args = ["s3api", "list-object-versions", "--bucket", bucket] + (["--prefix", prefix] if prefix else [])
        doc = self.read(*args, missing=("NoSuchBucket",))
        if doc is None:
            return None
        return [{"Key": v["Key"], "VersionId": v["VersionId"]} for v in (doc.get("Versions") or []) + (doc.get("DeleteMarkers") or [])]

    def empty_bucket(self, bucket: str, prefix: str = "", keys: set[str] | None = None) -> bool:
        """Deletes every object version and delete marker (under `prefix`, or of exactly `keys`)."""
        doc = self.read("s3api", "list-object-versions", "--bucket", bucket, *(["--prefix", prefix] if prefix else []),
                        missing=("NoSuchBucket",))
        if doc is None:
            return True
        vs = [{"Key": v["Key"], "VersionId": v["VersionId"]} for v in doc.get("Versions") or []]
        ms = [{"Key": m["Key"], "VersionId": m["VersionId"]} for m in doc.get("DeleteMarkers") or []]
        if keys is not None:
            vs = [v for v in vs if v["Key"] in keys]
            ms = [m for m in ms if m["Key"] in keys]
        if not vs and not ms:
            return True
        target = f"s3://{bucket}/{prefix}" if prefix else f"s3://{bucket}"
        if self.dry:
            return self.act(f"empty {target} ({len(vs)} versions, {len(ms)} delete markers)")
        # The Go module store keeps Object Lock in GOVERNANCE mode so that this step can remove it (invariant 7); its
        # bucket policy lets the deploying principal alone delete and bypass (the IAM role Admin, or the operator the accounts file names, and
        # the setup runner's role). No other bucket is sent the flag.
        bypass = ["--bypass-governance-retention"] if bucket == f"{self.p}-gomod-{self.account}" else []
        ok = True
        for batch in chunks(vs + ms, 1000):
            # 1000 long keys as one argument exceed the kernel's limit for a single argument (131072 bytes, E2BIG), so the spec goes in a
            # file the CLI reads (`--delete file://...`); the temporary file is removed when it closes.
            with tempfile.NamedTemporaryFile("w", prefix="teardown-delete-", suffix=".json") as fh:
                json.dump({"Objects": batch, "Quiet": True}, fh)
                fh.flush()
                ok &= self.act(f"empty {target}: {len(batch)} versions and delete markers", "s3api", "delete-objects",
                               "--bucket", bucket, "--delete", f"file://{fh.name}", *bypass, delete_errors=True)
        return ok

    def ecr_images(self, repo: str, only_tags: set[str] | None = None, region: str | None = None) -> None:
        region = region or self.home
        doc = self.read("ecr", "list-images", "--repository-name", repo, region=region, missing=("RepositoryNotFoundException",))
        ids = (doc or {}).get("imageIds", [])
        if only_tags is not None:
            ids = [{"imageTag": i["imageTag"]} for i in ids if i.get("imageTag") in only_tags]
        else:
            ids = list({i["imageDigest"]: {"imageDigest": i["imageDigest"]} for i in ids}.values())
        for batch in chunks(ids, 100):
            self.act(f"batch-delete-image {repo} ({len(batch)} images)", "ecr", "batch-delete-image", "--repository-name", repo,
                     "--image-ids", json.dumps(batch), region=region)

    # ---- steps --------------------------------------------------------------------------------

    def check_account(self) -> None:
        got = (self.read("sts", "get-caller-identity") or {}).get("Account", "")
        if got != self.account:
            raise Fail(f"account {got or 'none'}")

    def read_record(self) -> None:
        ok, out, err = self.call(["s3", "cp", f"s3://{self.bucket}/deploy/{self.stage}/cdk-assets.json", "-"])
        if ok and out.strip():
            self.record = json.loads(out)
            say(f"asset record: {len(self.record.get('files', []))} objects, {len(self.record.get('images', []))} images")
        else:
            self.notes.append("no CDK asset record at s3://" + self.bucket + f"/deploy/{self.stage}/cdk-assets.json")

    def items(self) -> list[dict]:
        doc = self.read("dynamodb", "scan", "--table-name", self.table, missing=("ResourceNotFoundException",))
        return (doc or {}).get("Items", [])

    def e2e_runs(self, items: list[dict]) -> set[str]:
        runs = set()
        for it in items:
            if it.get("SK", {}).get("S") != "META":
                continue
            repo = it.get("target", {}).get("M", {}).get("repo", {}).get("S", "")
            if repo.startswith("cc:" + self.e2e_prefix):
                runs.add(it["runId"]["S"])
        copies = self.evidence / "e2e" / "runs"
        if copies.is_dir():
            runs |= {d.name for d in copies.iterdir() if d.is_dir()}
        return runs

    def capacity_provider_id(self) -> str | None:
        """The id of the stage's Instances capacity provider (the runtimes stack publishes it as an SSM parameter); None when the stage has none.
        DeleteCapacityProviderSession takes the id, not the ARN."""
        if not hasattr(self, "_provider_id"):
            doc = self.read("ssm", "get-parameter", "--name", f"{self.ssm}/instances-capacity-provider-id", missing=("ParameterNotFound",))
            self._provider_id = ((doc or {}).get("Parameter") or {}).get("Value") or None
        return self._provider_id

    def stop_sessions(self, items: list[dict], runs: set[str] | None) -> None:
        for it in items:
            if not it.get("SK", {}).get("S", "").startswith("AGENT#"):
                continue
            rid = it.get("runId", {}).get("S", "")
            arn = it.get("runtimeArn", {}).get("S", "")
            sid = it.get("runtimeSessionId", {}).get("S", "")
            if runs is not None and rid not in runs:
                continue
            if not arn or not sid or it.get("status", {}).get("S") == "stopped":
                continue
            if f":runtime/{self.u}_" not in arn:
                self.notes.append(f"session {rid}/{it.get('agentId', {}).get('S', '')} is on a runtime that is not the stage's; not stopped")
                continue
            pinned = it.get("runtimeEndpoint", {}).get("S", "")
            qualifier = pinned if ENDPOINT_NAME.fullmatch(pinned) else QUALIFIER
            self.act(f"stop-runtime-session {rid}/{it.get('agentId', {}).get('S', '')}", "bedrock-agentcore",
                     "stop-runtime-session", "--agent-runtime-arn", arn, "--runtime-session-id", sid, "--qualifier", qualifier,
                     ok_if=("ResourceNotFoundException",))
            # A session on the Instances runtime is a machine of the account. StopRuntimeSession stops the agent in it and the machine idles out later (and bills);
            # DeleteCapacityProviderSession removes the machine, its network interface and its disks.
            if f":runtime/{self.u}_harness_worker_python_instances-" in arn:
                provider = self.capacity_provider_id()
                if provider is None:
                    self.notes.append(f"session {rid}/{it.get('agentId', {}).get('S', '')} is on the Instances runtime and the stage publishes no capacity provider id; its machine is not deleted")
                    continue
                self.act(f"delete-capacity-provider-session {rid}/{it.get('agentId', {}).get('S', '')}", "bedrock-agentcore",
                         "delete-capacity-provider-session", "--capacity-provider-id", provider, "--session-id", sid,
                         ok_if=("ResourceNotFoundException",))

    def running_executions(self, runs: set[str] | None) -> list[dict]:
        doc = self.read("stepfunctions", "list-state-machines") or {}
        out = []
        for sm in doc.get("stateMachines", []):
            if not sm["name"].startswith(self.p + "-"):
                continue
            ex = self.read("stepfunctions", "list-executions", "--state-machine-arn", sm["stateMachineArn"],
                           "--status-filter", "RUNNING") or {}
            for e in ex.get("executions", []):
                if runs is None or any(e["name"] == r or e["name"].startswith(r + "-") for r in runs):
                    out.append(e)
        return out

    def stop_executions(self, runs: set[str] | None) -> None:
        for e in self.running_executions(runs):
            self.act(f"stop-execution {e['name']}", "stepfunctions", "stop-execution", "--execution-arn", e["executionArn"],
                     "--cause", "scripts/teardown.sh")

    def e2e_repos(self) -> list[tuple[str, str]]:
        """(name, arn) of the CodeCommit hamh-e2e-* repositories (of this generation's prefix) tagged for the stage."""
        doc = self.read("codecommit", "list-repositories") or {}
        out = []
        for r in doc.get("repositories", []):
            name = r["repositoryName"]
            if not name.startswith(self.e2e_prefix):
                continue
            arn = f"arn:aws:codecommit:{self.home}:{self.account}:{name}"
            tags = (self.read("codecommit", "list-tags-for-resource", "--resource-arn", arn,
                              missing=("RepositoryDoesNotExistException",)) or {}).get("tags", {})
            if self.ours(tags):
                out.append((name, arn))
        return out

    def repo_identity(self, name: str) -> dict | None:
        """The repository as it is now: its ID, creation date and branch heads (None when absent)."""
        meta = (self.read("codecommit", "get-repository", "--repository-name", name,
                          missing=("RepositoryDoesNotExistException",)) or {}).get("repositoryMetadata")
        if meta is None:
            return None
        heads = {}
        for b in (self.read("codecommit", "list-branches", "--repository-name", name) or {}).get("branches", []):
            br = (self.read("codecommit", "get-branch", "--repository-name", name, "--branch-name", b,
                            missing=("BranchDoesNotExistException",)) or {}).get("branch")
            if br is not None:
                heads[f"refs/heads/{b}"] = br.get("commitId", "")
        return {"repositoryId": meta.get("repositoryId"), "creationDate": meta.get("creationDate"), "heads": heads}

    def repos_copy_ok(self, name: str) -> bool:
        """A complete e2e-repos/<repo>-<utc> copy of this repository: the record names it, carries
        its repository ID and creation date (a repository made again under the same name has others)
        and a ref for every branch at its current head, and the bundle it names hashes to its SHA-256
        (no bundle only for a repository with no refs)."""
        recs = sorted((self.evidence / "e2e-repos").glob(f"{name}-*.json"))
        if not recs:
            return False
        live = self.repo_identity(name)
        if live is None:
            return False
        for rec in recs:
            try:
                doc = json.loads(rec.read_text())
            except (OSError, ValueError):
                continue
            refs = doc.get("refs")
            if doc.get("kind") != "e2e-repo-copy" or doc.get("repository") != name or not isinstance(refs, dict):
                continue
            if not live["repositoryId"] or doc.get("repositoryId") != live["repositoryId"] or \
                    not same_instant(doc.get("creationDate"), live["creationDate"]):
                continue
            if any(refs.get(ref) != sha for ref, sha in live["heads"].items()):
                continue
            if not refs and not live["heads"] and doc.get("bundle") is None:
                return True
            bundle = rec.parent / str(doc.get("bundle") or "")
            if refs and doc.get("bundle") and bundle.is_file() and \
                    hashlib.sha256(bundle.read_bytes()).hexdigest() == doc.get("sha256"):
                return True
        return False

    def repo_copy_missing(self, name: str) -> list[str]:
        d = self.evidence / "e2e" / "codecommit" / name
        missing = [f for f in E2E_REPO_COPY if not (d / f).is_file()]
        return [] if missing and self.repos_copy_ok(name) else missing

    def delete_e2e_repo(self, name: str) -> bool:
        """One E2E repository: named hamh-e2e-*, tagged for the stage and with a complete evidence copy,
        its open PRs closed, its harness/* branches deleted, then the repository. False when kept
        (the reason is the last note)."""
        def keep(why: str) -> bool:
            msg = f"{name}: {why}; repository kept"
            say(msg)
            self.notes.append(msg)
            return False
        if not name.startswith(self.e2e_prefix):
            return keep(f"not a {self.e2e_prefix}* repository")
        arn = f"arn:aws:codecommit:{self.home}:{self.account}:{name}"
        doc = self.read("codecommit", "list-tags-for-resource", "--resource-arn", arn,
                        missing=("RepositoryDoesNotExistException",))
        if doc is None:
            return True
        if not self.ours(doc.get("tags", {})):
            return keep(f"not tagged for stage {self.stage}")
        missing = self.repo_copy_missing(name)
        if missing:
            return keep(f"evidence copy incomplete ({', '.join(missing)})")
        prs = self.read("codecommit", "list-pull-requests", "--repository-name", name, "--pull-request-status", "OPEN") or {}
        for pr in prs.get("pullRequestIds", []):
            self.act(f"close pull request {pr} in {name}", "codecommit", "update-pull-request-status", "--pull-request-id", pr,
                     "--pull-request-status", "CLOSED")
        for b in (self.read("codecommit", "list-branches", "--repository-name", name) or {}).get("branches", []):
            if b.startswith("harness/"):
                self.act(f"delete-branch {name} {b}", "codecommit", "delete-branch", "--repository-name", name, "--branch-name", b)
        return self.act(f"delete-repository {name}", "codecommit", "delete-repository", "--repository-name", name)

    def delete_e2e_repos(self) -> None:
        for name, _ in self.e2e_repos():
            self.delete_e2e_repo(name)

    def empty_run_prefixes(self, runs: set[str]) -> None:
        for rid in sorted(runs):
            copy = self.evidence / "e2e" / "runs" / rid
            if not (copy.is_dir() and any(copy.iterdir())):
                if self.versions(self.bucket, f"runs/{rid}/"):
                    msg = f"runs/{rid}/: evidence copy missing; prefix kept"
                    say(msg)
                    self.notes.append(msg)
                continue
            self.empty_bucket(self.bucket, f"runs/{rid}/")

    def stack_resources(self, stack: str) -> list[dict]:
        """list-stack-resources (paged; describe-stack-resources stops at 100)."""
        doc = self.read("cloudformation", "list-stack-resources", "--stack-name", stack, missing=("does not exist",)) or {}
        return doc.get("StackResourceSummaries", [])

    def stack_storage(self, stack: str) -> None:
        resources = self.stack_resources(stack)
        # CloudTrail delivers into its bucket until the trail stops, so an emptied bucket would fill again.
        for r in resources:
            if r["ResourceType"] == "AWS::CloudTrail::Trail" and r.get("PhysicalResourceId"):
                self.act(f"stop-logging {r['PhysicalResourceId']}", "cloudtrail", "stop-logging", "--name", r["PhysicalResourceId"],
                         ok_if=("TrailNotFoundException",))
        for r in resources:
            pid = r.get("PhysicalResourceId", "")
            if r["ResourceType"] == "AWS::S3::Bucket" and pid.startswith(self.p + "-"):
                self.empty_bucket(pid)
            elif r["ResourceType"] == "AWS::ECR::Repository" and pid.startswith(self.p + "-"):
                self.ecr_images(pid)

    def stack_destroy(self, stack: str) -> bool:
        """A stack of the app goes through `cdk destroy`; any other (the previous generation, which the CDK app no longer defines) through CloudFormation."""
        if stack in self.order:
            return self.cdk_destroy(stack)
        ok = self.act(f"delete-stack {stack} (not in the app)", "cloudformation", "delete-stack", "--stack-name", stack)
        if ok and not self.dry:
            ok = self.call(["cloudformation", "wait", "stack-delete-complete", "--stack-name", stack])[0]
        return ok

    def cdk_destroy(self, stack: str) -> bool:
        if self.dry:
            return self.act(f"cdk destroy {stack}")
        self.commands.append(f"scripts/node.sh cdk destroy {stack} --exclusively --force -c stage={self.stage}")
        say(f"cdk destroy {stack}")
        return self.node_run("cdk", "destroy", stack, "--exclusively", "--force", "-c", f"stage={self.stage}") == 0

    def stage_vpcs(self) -> list[str]:
        doc = self.read("ec2", "describe-vpcs", "--filters", f"Name=tag:project,Values={self.project}",
                        f"Name=tag:stage,Values={self.stage}") or {}
        return [v["VpcId"] for v in doc.get("Vpcs", [])]

    def remaining_enis(self) -> list[dict]:
        """Deletes detached customer ENIs of the stage VPC; returns the others (in use, or managed by a service) as {id, type, vpc}."""
        rows: list[dict] = []
        for vpc in self.vpcs:
            doc = self.read("ec2", "describe-network-interfaces", "--filters", f"Name=vpc-id,Values={vpc}") or {}
            for e in doc.get("NetworkInterfaces", []):
                if e.get("Status") == "available" and not e.get("RequesterManaged"):
                    self.act(f"delete-network-interface {e['NetworkInterfaceId']} (detached, {vpc})", "ec2",
                             "delete-network-interface", "--network-interface-id", e["NetworkInterfaceId"])
                else:
                    rows.append({"id": e["NetworkInterfaceId"], "type": e.get("InterfaceType") or "", "vpc": vpc})
        return rows

    def delete_detached_enis(self) -> int:
        """Deletes detached customer ENIs of the stage VPC; returns how many service-managed ones remain."""
        return len(self.remaining_enis())

    @staticmethod
    def agentcore_enis(rows: list[dict]) -> list[dict]:
        return [r for r in rows if r["type"] == AGENTCORE_ENI_TYPE]

    def wait_text(self) -> str:
        minutes = int(self.network_timeout_s // 60)
        return f"{minutes} minutes" if self.network_timeout_s >= 60 else f"{int(self.network_timeout_s)} seconds"

    def destroy_network(self, stack: str) -> bool:
        """The network stack did not go. Detached customer interfaces are deleted; AgentCore's own interface (type agentic_ai) is only waited for: AWS releases it up to
        8 hours after the runtime delete. While it is listed the stack is not tried, except on every 10th look; when it is gone the stack is deleted once more. The wait ends at
        the limit (`--network-wait-minutes`, else TEARDOWN_NETWORK_TIMEOUT_S, else one hour)."""
        start = time.monotonic()
        polls = 0
        rows: list[dict] | None = None
        while True:
            if rows is None:
                rows = self.remaining_enis()
            held = self.agentcore_enis(rows)
            waited = time.monotonic() - start
            # the hold is what the last look found: a released interface leaves no hold behind, and other interfaces in use keep the run from ending with exit 9
            self.network_hold = ({"stack": stack, "enis": held, "waited_s": waited, "others": [r["id"] for r in rows if r["type"] != AGENTCORE_ENI_TYPE]}
                                 if held else None)
            if waited >= self.network_timeout_s:
                if held:
                    msg = (f"{stack} still present after {minutes_text(waited)}: AgentCore holds {len(held)} network interface(s) ({', '.join(h['id'] for h in held)}) "
                           f"in {held[0]['vpc']}")
                else:
                    msg = f"{stack} still present after {int(self.network_timeout_s)} s ({len(rows)} network interfaces in use or service-managed)"
                say(msg)
                self.notes.append(msg)
                return False
            if held:
                if polls == 0:
                    say(f"{stack}: AgentCore holds {len(held)} network interface(s) in the VPC ({', '.join(h['id'] for h in held)}); waiting for AWS to release the "
                        f"AgentCore-managed network interface for up to {self.wait_text()}, looking every {int(self.sleep_s)} s")
                elif polls % 3 == 0:
                    say(f"{stack}: still waiting for AWS to release the AgentCore-managed network interface ({minutes_text(waited)} of up to {self.wait_text()})")
            else:
                say(f"{stack}: retrying in {int(self.sleep_s)} s ({len(rows)} network interfaces still in the VPC)")
            time.sleep(self.sleep_s)
            polls += 1
            attempt = True
            rows = None
            if held:
                rows = self.remaining_enis()
                if self.agentcore_enis(rows):
                    attempt = polls % NETWORK_ATTEMPT_EVERY == 0
                else:
                    say("AWS released the AgentCore-managed network interface; the network stack is deleted again")
            if attempt:
                rows = None
                if self.stack_destroy(stack):
                    self.network_hold = None
                    return True

    def network_only(self, items: list[tuple[str, str]]) -> bool:
        """Every remaining resource is a network resource of the stage: an EC2 resource of the VPC, or the network stack."""
        for region, rid in items:
            if region == "unknown":
                return False
            if NETWORK_ARN.match(rid) or (rid.startswith("arn:aws:cloudformation:") and f":stack/{self.p}-network/" in rid):
                continue
            return False
        return True

    def rerun_command(self) -> str:
        parts = ["scripts/teardown.sh", "--stage", self.stage, "--yes"]
        if self.invocation:
            parts += shlex.split(self.invocation)
        prefix = f"HAMH_AWS_PROFILE={shlex.quote(self.profile_selector)} " if self.profile_selector else ""
        return prefix + " ".join(shlex.quote(x) for x in parts)

    def say_hold(self, only: bool) -> None:
        """What the leftover network resources wait for, the interface, and the command to run again. `only`: nothing but those network resources remains (exit 9)."""
        hold = self.network_hold or {}
        enis = hold.get("enis") or []
        others = hold.get("others") or []
        ids = ", ".join(f"{e['id']} (in {e['vpc']})" for e in enis)
        say(f"AgentCore-managed network interface still held: {ids}. It keeps the subnet, the security group and the VPC of stack {hold.get('stack')} in use, "
            f"and teardown does not detach or delete it.")
        say("AWS releases such an interface up to 8 hours after the runtime is deleted (measured: about 3 hours in one account, 30 to 60 minutes in another). "
            f"This run waited {minutes_text(hold.get('waited_s', 0.0))}.")
        say("Run the teardown again after the release; it continues from what the account shows:")
        print(f"  {self.rerun_command()}", flush=True)
        say("To wait for the whole period in one run, add --network-wait-minutes full (up to 480 minutes).")
        if others:
            say(f"Other network interfaces in the VPC are in use as well ({', '.join(others)}); the exit code is 1.")
        else:
            say("Nothing but these network resources remains: the exit code is 9." if only else "Other resources of the stage remain as well (listed below): the exit code is 1.")

    def version_endpoints(self) -> None:
        """deploy.sh's `v<N>` endpoints on the stage's runtimes: CloudFormation did not create them."""
        doc = self.read("bedrock-agentcore-control", "list-agent-runtimes") or {}
        for rt in doc.get("agentRuntimes", []):
            if not rt["agentRuntimeName"].startswith(self.u + "_"):
                continue
            rid = rt["agentRuntimeId"]
            eps = self.read("bedrock-agentcore-control", "list-agent-runtime-endpoints", "--agent-runtime-id", rid) or {}
            for ep in eps.get("runtimeEndpoints", []):
                if VERSION_ENDPOINT.fullmatch(ep["name"]):
                    self.act(f"delete-agent-runtime-endpoint {rid} {ep['name']}", "bedrock-agentcore-control",
                             "delete-agent-runtime-endpoint", "--agent-runtime-id", rid, "--endpoint-name", ep["name"])

    def plan_network_wait(self) -> None:
        """A dry run: the detached interfaces are planned for deletion, and the wait for AgentCore's interface is planned when one is there and the limit allows a wait."""
        held = self.agentcore_enis(self.remaining_enis())
        if held and self.network_timeout_s > 0:
            line = (f"plan wait up to {int(self.network_timeout_s // 60)} minutes for AWS to release the AgentCore-managed network interface "
                    f"({', '.join(h['id'] for h in held)}) before the network stack is deleted again")
            self.commands.append(line)
            say(line)

    def destroy_stacks(self) -> list[str]:
        existing = self.cfn_stacks()
        failed = []
        for stack in reversed(self.order):
            if stack not in existing:
                continue
            if stack == f"{self.p}-runtimes":
                self.version_endpoints()
            self.stack_storage(stack)
            if self.dry and stack == f"{self.p}-network":
                self.plan_network_wait()
            if self.cdk_destroy(stack):
                continue
            if stack == f"{self.p}-network" and self.destroy_network(stack):
                continue
            failed.append(stack)
        # not in the app: a stack the app no longer defines, and every stack of the previous generation; unknown names first, then the reverse of the deploy order
        rank = {f"{self.p}-{n}": i for i, n in enumerate(PRIORITY)}
        for stack in sorted(set(existing) - set(self.order), key=lambda n: (n in rank, -rank.get(n, 0), n)):
            if stack == f"{self.p}-runtimes":
                self.version_endpoints()
            self.stack_storage(stack)
            if self.dry and stack == f"{self.p}-network":
                self.plan_network_wait()
            if self.stack_destroy(stack):
                continue
            if stack == f"{self.p}-network" and self.destroy_network(stack):
                continue
            failed.append(stack)
        return failed

    def agentcore(self) -> None:
        doc = self.read("bedrock-agentcore-control", "list-agent-runtimes") or {}
        for rt in doc.get("agentRuntimes", []):
            if not rt["agentRuntimeName"].startswith(self.u + "_"):
                continue
            rid = rt["agentRuntimeId"]
            eps = self.read("bedrock-agentcore-control", "list-agent-runtime-endpoints", "--agent-runtime-id", rid) or {}
            for ep in eps.get("runtimeEndpoints", []):
                if ep["name"] != "DEFAULT":
                    self.act(f"delete-agent-runtime-endpoint {rid} {ep['name']}", "bedrock-agentcore-control",
                             "delete-agent-runtime-endpoint", "--agent-runtime-id", rid, "--endpoint-name", ep["name"])
            self.act(f"delete-agent-runtime {rid}", "bedrock-agentcore-control", "delete-agent-runtime", "--agent-runtime-id", rid)
        # An Instances capacity provider the stack delete left (a stack that failed, or one made outside CloudFormation). It goes after the runtimes that name it.
        # DeleteAgentRuntime is asynchronous, and DeleteCapacityProvider fails with a ValidationException while any runtime or runtime version still names the provider (the
        # versions detach from it asynchronously), so: read the stage's runtimes until none is left, repeat the delete while the service refuses it, read the provider until it is gone.
        providers = [cp for cp in (self.read("bedrock-agentcore-control", "list-capacity-providers") or {}).get("capacityProviders", [])
                     if cp.get("name", "").startswith(self.u + "_")]
        if providers:
            self.wait_runtimes_gone()
        for cp in providers:
            cid = cp["capacityProviderId"]
            if self.act(f"delete-capacity-provider {cid}", "bedrock-agentcore-control", "delete-capacity-provider", "--capacity-provider-id", cid,
                        ok_if=("ResourceNotFoundException",), retry_if=("ValidationException", "RetryableConflictException")):
                self.wait_provider_gone(cid)

    def wait_runtimes_gone(self) -> None:
        """Until no runtime of the stage is listed (a deleted runtime is DELETING for a while), for up to TEARDOWN_SETTLE_S. A dry run plans the calls and waits for nothing."""
        if self.dry:
            return
        deadline = time.monotonic() + self.settle_s
        while True:
            left = [rt for rt in (self.read("bedrock-agentcore-control", "list-agent-runtimes") or {}).get("agentRuntimes", [])
                    if rt["agentRuntimeName"].startswith(self.u + "_")]
            if not left:
                return
            if time.monotonic() >= deadline:
                self.notes.append(f"{len(left)} runtime(s) of the stage still deleting after {self.settle_s:.0f} s; the capacity provider is deleted anyway and may refuse")
                return
            time.sleep(self.poll_s)

    def wait_provider_gone(self, capacity_provider_id: str) -> None:
        """Until the capacity provider answers ResourceNotFoundException (its delete is asynchronous too), for up to TEARDOWN_SETTLE_S."""
        if self.dry:
            return
        deadline = time.monotonic() + self.settle_s
        while True:
            ok, _, err = self.call(["bedrock-agentcore-control", "get-capacity-provider", "--capacity-provider-id", capacity_provider_id])
            if not ok and "ResourceNotFoundException" in err:
                return
            if time.monotonic() >= deadline:
                self.notes.append(f"capacity provider {capacity_provider_id} still present after {self.settle_s:.0f} s")
                return
            time.sleep(self.poll_s)

    def gateway_arn(self, gateway_id: str) -> str:
        return f"arn:aws:bedrock-agentcore:{self.home}:{self.account}:gateway/{gateway_id}"

    def stage_gateways(self) -> list[dict]:
        """The AgentCore Gateways named hamh-<stage>-*: the name is the ownership proof, as for the service-made log groups."""
        doc = self.read("bedrock-agentcore-control", "list-gateways") or {}
        return [g for g in doc.get("items", []) if g["name"].startswith(self.p + "-")]

    def gateway_targets(self, gateway_id: str) -> list[dict]:
        doc = self.read("bedrock-agentcore-control", "list-gateway-targets", "--gateway-identifier", gateway_id,
                        missing=("ResourceNotFoundException",)) or {}
        return doc.get("items", [])

    def gateways(self) -> None:
        """Gateways of the stage that CloudFormation did not delete (the stack failed, or a target was made outside it). A gateway with
        a target cannot be deleted and a target goes asynchronously: delete each target, read the list again until it is empty (for up
        to TEARDOWN_SETTLE_S, every TEARDOWN_SETTLE_POLL_S), then delete the gateway. A dry run plans the calls and waits for nothing."""
        for gw in self.stage_gateways():
            gid = gw["gatewayId"]
            for t in self.gateway_targets(gid):
                if t.get("status") != "DELETING":
                    self.act(f"delete-gateway-target {gid} {t['name']}", "bedrock-agentcore-control", "delete-gateway-target",
                             "--gateway-identifier", gid, "--target-id", t["targetId"], ok_if=("ResourceNotFoundException",))
            if not self.dry:
                deadline = time.monotonic() + self.settle_s
                while True:
                    left = self.gateway_targets(gid)
                    if not left:
                        break
                    if time.monotonic() >= deadline:
                        self.notes.append(f"gateway {gid}: {len(left)} target(s) still deleting after {self.settle_s:.0f} s")
                        break
                    time.sleep(self.poll_s)
            self.act(f"delete-gateway {gid}", "bedrock-agentcore-control", "delete-gateway", "--gateway-identifier", gid,
                     ok_if=("ResourceNotFoundException",))

    def leftover_storage(self) -> None:
        for r in self.tagged(self.home):
            if r["arn"].startswith("arn:aws:s3:::"):
                name = r["arn"].split(":::", 1)[1]
                if name.startswith(self.p + "-") and self.empty_bucket(name):
                    self.act(f"delete-bucket {name}", "s3api", "delete-bucket", "--bucket", name, ok_if=("NoSuchBucket",))
        doc = self.read("ecr", "describe-repositories") or {}
        for repo in doc.get("repositories", []):
            if not repo["repositoryName"].startswith(self.p + "-"):
                continue
            tags = (self.read("ecr", "list-tags-for-resource", "--resource-arn", repo["repositoryArn"]) or {}).get("tags", [])
            if self.ours(tags):
                self.act(f"delete-repository {repo['repositoryName']} (with its images)", "ecr", "delete-repository",
                         "--repository-name", repo["repositoryName"], "--force")

    def log_group_prefixes(self) -> tuple[str, ...]:
        return (f"/aws/bedrock-agentcore/runtimes/{self.u}", f"/aws/lambda/{self.p}-", f"/aws/codebuild/{self.p}-")

    def log_groups(self) -> list[tuple[str, str]]:
        """(region, name) of the stage's log groups: tagged in every Region, or under the three prefixes."""
        found: dict[tuple[str, str], None] = {}
        for region in REGIONS:
            for r in self.tagged(region):
                if ":log-group:" in r["arn"]:
                    found[(region, r["arn"].split(":log-group:", 1)[1].removesuffix(":*"))] = None
        for prefix in self.log_group_prefixes():
            doc = self.read("logs", "describe-log-groups", "--log-group-name-prefix", prefix) or {}
            for g in doc.get("logGroups", []):
                found[(self.home, g["logGroupName"])] = None
        return list(found)

    def delete_log_groups(self) -> None:
        for region, name in self.log_groups():
            self.act(f"delete-log-group {name}", "logs", "delete-log-group", "--log-group-name", name, region=region,
                     ok_if=("ResourceNotFoundException",))

    def kms(self) -> None:
        soon = dt.datetime.now(dt.timezone.utc) + dt.timedelta(days=KMS_WINDOW_DAYS + 1)
        for region in REGIONS:
            for r in self.tagged(region):
                if ":key/" not in r["arn"] or not r["arn"].startswith("arn:aws:kms:"):
                    continue
                md = (self.read("kms", "describe-key", "--key-id", r["arn"], region=region, missing=("NotFoundException",)) or {}).get("KeyMetadata")
                if not md or md.get("KeyManager") == "AWS":
                    continue
                if md["KeyState"] == "PendingDeletion":
                    when = dt.datetime.fromisoformat(str(md.get("DeletionDate")).replace("Z", "+00:00"))
                    if when <= soon:
                        continue
                    if not self.act(f"cancel-key-deletion {r['arn']} (scheduled {when:%Y-%m-%d}; rescheduling at {KMS_WINDOW_DAYS} days)",
                                    "kms", "cancel-key-deletion", "--key-id", r["arn"], region=region):
                        continue
                self.act(f"schedule-key-deletion {r['arn']} ({KMS_WINDOW_DAYS} days)", "kms", "schedule-key-deletion", "--key-id", r["arn"],
                         "--pending-window-in-days", str(KMS_WINDOW_DAYS), region=region)

    def secrets(self) -> None:
        seen = set()
        doc = self.read("secretsmanager", "list-secrets", "--filters", f"Key=name,Values={self.secret_root}") or {}
        for sec in listed(doc, "SecretList"):
            if not sec["Name"].startswith(self.secret_root):
                continue
            seen.add(sec["ARN"])
            if not self.ours(sec.get("Tags")):
                msg = f"{sec['Name']}: no project tag, left for the owner"
                say(msg)
                self.notes.append(msg)
                continue
            self.act(f"delete-secret {sec['Name']} (no recovery)", "secretsmanager", "delete-secret", "--secret-id", sec["ARN"],
                     "--force-delete-without-recovery")
        for region in REGIONS:
            for r in self.tagged(region):
                if r["arn"].startswith("arn:aws:secretsmanager:") and r["arn"] not in seen:
                    self.act(f"delete-secret {r['arn']} (no recovery)", "secretsmanager", "delete-secret", "--secret-id", r["arn"],
                             "--force-delete-without-recovery", region=region)

    def ssm_parameters(self) -> None:
        """The stage's tagged parameters under /hamh/<stage>/ (deploy.sh's digests outlive the stacks)."""
        for region in REGIONS:
            for r in self.tagged(region):
                if not r["arn"].startswith("arn:aws:ssm:") or ":parameter/" not in r["arn"]:
                    continue
                name = "/" + r["arn"].split(":parameter/", 1)[1]
                if name.startswith(f"{self.ssm}/"):
                    self.act(f"delete-parameter {name}", "ssm", "delete-parameter", "--name", name, region=region,
                             ok_if=("ParameterNotFound",))

    def cdk_assets(self) -> None:
        by_bucket: dict[str, set[str]] = {}
        for f in self.record.get("files", []):
            if f["bucket"].startswith(CDK_ASSET_BUCKET_PREFIX):
                by_bucket.setdefault(f["bucket"], set()).add(f["key"])
        for bucket, keys in sorted(by_bucket.items()):
            self.empty_bucket(bucket, keys=keys)
        by_repo: dict[str, set[str]] = {}
        for i in self.record.get("images", []):
            if i["repository"].startswith(CDK_IMAGE_REPO_PREFIX):
                by_repo.setdefault(i["repository"], set()).add(i["tag"])
        for repo, tags in sorted(by_repo.items()):
            self.ecr_images(repo, only_tags=tags)

    def prcheck(self) -> None:
        doc = self.read("codebuild", "batch-get-projects", "--names", f"{self.p}-prcheck") or {}
        for proj in doc.get("projects", []):
            self.act(f"delete-project {proj['name']}", "codebuild", "delete-project", "--name", proj["name"])

    def rules(self) -> list[dict]:
        doc = self.read("events", "list-rules", "--name-prefix", self.p + "-") or {}
        return [r for r in doc.get("Rules", []) if r["Name"].startswith(self.p + "-") and not r.get("ManagedBy")]

    def delete_rules(self) -> None:
        for rule in self.rules():
            targets = (self.read("events", "list-targets-by-rule", "--rule", rule["Name"]) or {}).get("Targets", [])
            if targets:
                self.act(f"remove-targets {rule['Name']} ({len(targets)})", "events", "remove-targets", "--rule", rule["Name"],
                         "--ids", *[t["Id"] for t in targets])
            self.act(f"delete-rule {rule['Name']}", "events", "delete-rule", "--name", rule["Name"])

    def trails(self) -> list[str]:
        """ARNs of the stage's tagged CloudTrail trails in the home Region."""
        return [r["arn"] for r in self.tagged(self.home) if r["arn"].startswith("arn:aws:cloudtrail:") and ":trail/" in r["arn"]]

    def delete_trails(self) -> None:
        for arn in self.trails():
            self.act(f"delete-trail {arn.rsplit('/', 1)[-1]}", "cloudtrail", "delete-trail", "--name", arn, ok_if=("TrailNotFoundException",))

    def alarms(self) -> list[str]:
        """Names of the stage's tagged CloudWatch alarms in the home Region."""
        return sorted(r["arn"].split(":alarm:", 1)[1] for r in self.tagged(self.home)
                      if r["arn"].startswith("arn:aws:cloudwatch:") and ":alarm:" in r["arn"])

    def delete_alarms(self) -> None:
        for batch in chunks(self.alarms(), 100):
            self.act(f"delete-alarms {' '.join(batch)}", "cloudwatch", "delete-alarms", "--alarm-names", *batch)

    def dashboards(self) -> list[dict]:
        """Dashboards named hamh-<stage>-* (dashboards take no tags)."""
        doc = self.read("cloudwatch", "list-dashboards", "--dashboard-name-prefix", self.p + "-") or {}
        return [d for d in doc.get("DashboardEntries", []) if d["DashboardName"].startswith(self.p + "-")]

    def delete_dashboards(self) -> None:
        for batch in chunks(sorted(d["DashboardName"] for d in self.dashboards()), 100):
            self.act(f"delete-dashboards {' '.join(batch)}", "cloudwatch", "delete-dashboards", "--dashboard-names", *batch)

    def query_definitions(self) -> list[dict]:
        """Logs Insights saved queries named hamh-<stage>/* (they take no tags)."""
        # The API refuses a prefix that ends with "/" (InvalidParameterException), so ask for the stage prefix and keep the `<stage>/` names.
        doc = self.read("logs", "describe-query-definitions", "--query-definition-name-prefix", self.p) or {}
        return [q for q in doc.get("queryDefinitions", []) if q["name"].startswith(f"{self.p}/")]

    def delete_query_definitions(self) -> None:
        for q in self.query_definitions():
            self.act(f"delete-query-definition {q['name']}", "logs", "delete-query-definition", "--query-definition-id",
                     q["queryDefinitionId"], ok_if=("ResourceNotFoundException",))

    def window_role(self, name: str, tags) -> bool:
        """The invocation-logging window role of this stage: `hamh-<stage>-invwin-<region>` with the project tag and the
        marker `hamh-invlog-window=<stage>/<region>` of that same region (it carries no stage tag)."""
        pre = f"{self.p}-invwin-"
        if not name.startswith(pre):
            return False
        t = tag_map(tags)
        return t.get("project") == self.project and t.get(self.window_marker) == f"{self.stage}/{name[len(pre):]}"

    def roles(self) -> list[dict]:
        """The stage's IAM roles: path /hamh/<stage>/ or name hamh-<stage>-*, with the stage's tags, and the stage's
        invocation-logging window roles (`window_role`)."""
        found: dict[str, dict] = {}
        for args in (("--path-prefix", f"{self.ssm}/"), ()):
            for r in (self.read("iam", "list-roles", *args) or {}).get("Roles", []):
                if r["Path"].startswith(f"{self.ssm}/") or r["RoleName"].startswith(self.p + "-"):
                    found[r["RoleName"]] = r
        out = []
        for name, r in sorted(found.items()):
            tags = (self.read("iam", "list-role-tags", "--role-name", name, missing=("NoSuchEntity",)) or {}).get("Tags", [])
            if self.ours(tags) or self.window_role(name, tags):
                out.append(r)
        return out

    def delete_roles(self) -> None:
        for r in self.roles():
            name = r["RoleName"]
            for pol in (self.read("iam", "list-attached-role-policies", "--role-name", name) or {}).get("AttachedPolicies", []):
                self.act(f"detach-role-policy {name} {pol['PolicyArn']}", "iam", "detach-role-policy", "--role-name", name,
                         "--policy-arn", pol["PolicyArn"])
            for pol in (self.read("iam", "list-role-policies", "--role-name", name) or {}).get("PolicyNames", []):
                self.act(f"delete-role-policy {name} {pol}", "iam", "delete-role-policy", "--role-name", name, "--policy-name", pol)
            for ip in (self.read("iam", "list-instance-profiles-for-role", "--role-name", name) or {}).get("InstanceProfiles", []):
                self.act(f"remove-role-from-instance-profile {name} {ip['InstanceProfileName']}", "iam",
                         "remove-role-from-instance-profile", "--role-name", name, "--instance-profile-name", ip["InstanceProfileName"])
            if not self.act(f"delete-role {name}", "iam", "delete-role", "--role-name", name, ok_if=("NoSuchEntity",)):
                if "AccessDenied" not in (self.notes[-1] if self.notes else ""):
                    continue
                self.keep_role(name)

    def keep_role(self, name: str) -> None:
        """Testacct's IAM entity deletion protection refuses an Testacct session the deletion of a role used in the last 24
        hours: the role stays in the inventory, and the record names it with its last use and when it can go (about 24 hours
        later; IAM reports the last use with a lag, so a rerun after that time retries rather than trusting it)."""
        role = (self.read("iam", "get-role", "--role-name", name, missing=("NoSuchEntity",)) or {}).get("Role") or {}
        t = instant((role.get("RoleLastUsed") or {}).get("LastUsedDate"))
        last, after = (iso(t), iso(t + 86400)) if t is not None else (None, None)
        self.kept.append({"role": name, "lastUsed": last, "deletableAfter": after})
        msg = (f"role {name} remains: IAM deletion protection refuses deleting a role used in the last 24 hours (last used "
               f"{last or 'not reported'}); deletable after about {after or '24 hours after its last use'}; rerun the teardown then")
        say(msg)
        self.notes.append(msg)

    def deferred(self, failed: list[str]) -> None:
        still = self.cfn_stacks()
        for stack in [s for s in failed if s in still]:
            roles = [r["LogicalResourceId"] for r in self.stack_resources(stack)
                     if r["ResourceType"] == "AWS::IAM::Role" and r.get("ResourceStatus") == "DELETE_FAILED"]
            if roles:
                self.act(f"delete-stack {stack} retaining roles {', '.join(roles)} (the role pass deletes them)", "cloudformation",
                         "delete-stack", "--stack-name", stack, "--retain-resources", *roles)
                self.call(["cloudformation", "wait", "stack-delete-complete", "--stack-name", stack])
            else:
                self.stack_storage(stack)
                self.cdk_destroy(stack)
        self.delete_roles()

    def restore_invocation_logging(self) -> None:
        """deploy.sh wrote the state it found to SSM /hamh/<stage>/invocation-logging-prior
        before it enabled home-Region invocation logging. While the setting still names the stage's log group,
        that state is put back (unset: the configuration is deleted), before the log group's stack goes.
        Without the record, or when the setting names another destination, nothing is written."""
        name = f"{self.ssm}/invocation-logging-prior"
        doc = self.read("ssm", "get-parameter", "--name", name, missing=("ParameterNotFound",))
        if not doc:
            return
        prior = json.loads(doc["Parameter"]["Value"]).get("loggingConfig")
        cur = (self.read("bedrock", "get-model-invocation-logging-configuration") or {}).get("loggingConfig") or {}
        dest = (cur.get("cloudWatchConfig") or {}).get("logGroupName") or (cur.get("s3Config") or {}).get("bucketName")
        group = f"{self.ssm}/bedrock-invocations"
        if dest != group:
            self.notes.append(f"{self.home} invocation logging: {dest or 'unset'}; the prior state recorded at {name} was "
                              f"not restored (the setting does not name {group})")
            return
        if prior is None:
            what = "unset"
            ok = self.act(f"restore {self.home} invocation logging to unset", "bedrock",
                          "delete-model-invocation-logging-configuration")
        else:
            what = (prior.get("cloudWatchConfig") or {}).get("logGroupName") or (prior.get("s3Config") or {}).get("bucketName") or "its prior setting"
            ok = self.act(f"restore {self.home} invocation logging to {what}", "bedrock", "put-model-invocation-logging-configuration",
                          "--logging-config", json.dumps(prior))
        if ok and not self.dry:
            self.notes.append(f"{self.home} invocation logging: restored to {what} (recorded at {name})")

    def invocation_logging(self) -> None:
        for region in REGIONS:
            cfg = (self.read("bedrock", "get-model-invocation-logging-configuration", region=region) or {}).get("loggingConfig")
            dest = ((cfg or {}).get("cloudWatchConfig") or {}).get("logGroupName") or ((cfg or {}).get("s3Config") or {}).get("bucketName")
            if not dest:
                self.notes.append(f"{region} invocation logging: unset")
            elif dest == f"{self.ssm}/bedrock-invocations":
                self.notes.append(f"{region} invocation logging: {dest} (the stage's log group; the setting is never written, "
                                  "clear it by hand once the group is gone)")
            else:
                self.notes.append(f"{region} invocation logging: {dest} (not the stage's; never written)")

    # ---- inventory ----------------------------------------------------------------------------

    def inventory(self) -> tuple[list[tuple[str, str]], int]:
        """What is left, as (region, id); a source that cannot be read counts as a remaining item."""
        items: dict[tuple[str, str], None] = {}
        count = {"pending": 0}

        def add(region: str, rid: str) -> None:
            items.setdefault((region, rid), None)

        def source(label: str, fn) -> None:
            try:
                fn()
            except Fail as e:
                add("unknown", f"unreadable {label}: {e}")

        dropped: list[dict] = []
        self.reads += 1

        def tagging(region: str) -> None:
            for r in self.tagged(region):
                arn = r["arn"]
                if arn.startswith("arn:aws:kms:") and ":key/" in arn:
                    md = (self.read("kms", "describe-key", "--key-id", arn, region=region,
                                    missing=("NotFoundException",)) or {}).get("KeyMetadata")
                    if not md:
                        continue
                    if md.get("KeyState") == "PendingDeletion":
                        count["pending"] += 1
                        continue
                # A full run checks each listed ARN with its own service: the index lags behind the deletes it just made.
                gone = None if self.dry or self.e2e_only else self.gone(arn, region)
                if gone:
                    dropped.append({"arn": arn, "api": gone[0], "error": gone[1]})
                    continue
                add(region, arn)

        tagged: dict[tuple[str, str], None] = {}
        for region in REGIONS:
            source(f"tagging {region}", lambda region=region: tagging(region))
        tagged.update(items)
        self.dropped = dropped
        if self.e2e_only:
            items.clear()
            for region, rid in tagged:
                if region == "unknown":
                    add(region, rid)
            source("E2E", lambda: [add(self.home, rid) for rid in self.e2e_inventory()])
            return list(items), count["pending"]

        w = f"{self.home}:{self.account}"
        vpcs: list[str] = []

        def agentcore() -> None:
            for rt in (self.read("bedrock-agentcore-control", "list-agent-runtimes") or {}).get("agentRuntimes", []):
                if rt["agentRuntimeName"].startswith(self.u + "_"):
                    add(self.home, rt["agentRuntimeArn"])
            for gw in self.stage_gateways():
                add(self.home, self.gateway_arn(gw["gatewayId"]))
            for cp in (self.read("bedrock-agentcore-control", "list-capacity-providers") or {}).get("capacityProviders", []):
                if cp.get("name", "").startswith(self.u + "_"):
                    add(self.home, cp["capacityProviderArn"])

        def codeartifact() -> None:
            for d in (self.read("codeartifact", "list-domains") or {}).get("domains", []):
                if d["name"] == self.p or d["name"].startswith(self.p + "-"):
                    add(self.home, d["arn"])

        def firewalls() -> None:
            for fw in (self.read("network-firewall", "list-firewalls") or {}).get("Firewalls", []):
                if fw["FirewallName"].startswith(self.p + "-"):
                    add(self.home, fw["FirewallArn"])

        def network() -> None:
            current = self.stage_vpcs()
            vpcs.extend(dict.fromkeys([*current, *self.vpcs]))
            for v in current:
                add(self.home, f"arn:aws:ec2:{w}:vpc/{v}")
            tag_filters = [f"Name=tag:project,Values={self.project}", f"Name=tag:stage,Values={self.stage}"]
            for fs in [tag_filters, *([f"Name=vpc-id,Values={v}"] for v in vpcs)]:
                for n in (self.read("ec2", "describe-nat-gateways", "--filter", *fs) or {}).get("NatGateways", []):
                    if n.get("State") != "deleted":
                        add(self.home, f"arn:aws:ec2:{w}:natgateway/{n['NatGatewayId']}")
                for e in (self.read("ec2", "describe-vpc-endpoints", "--filters", *fs) or {}).get("VpcEndpoints", []):
                    if e.get("State", "").lower() != "deleted":
                        add(self.home, f"arn:aws:ec2:{w}:vpc-endpoint/{e['VpcEndpointId']}")
                for e in (self.read("ec2", "describe-network-interfaces", "--filters", *fs) or {}).get("NetworkInterfaces", []):
                    add(self.home, f"arn:aws:ec2:{w}:network-interface/{e['NetworkInterfaceId']}")
            for a in (self.read("ec2", "describe-addresses", "--filters", *tag_filters) or {}).get("Addresses", []):
                add(self.home, f"arn:aws:ec2:{w}:elastic-ip/{a['AllocationId']}")

        def log_groups() -> None:
            for prefix in self.log_group_prefixes():
                for g in (self.read("logs", "describe-log-groups", "--log-group-name-prefix", prefix) or {}).get("logGroups", []):
                    add(self.home, f"arn:aws:logs:{w}:log-group:{g['logGroupName']}")

        def assets() -> None:
            for f in self.record.get("files", []):
                if any(v["Key"] == f["key"] for v in self.versions(f["bucket"], f["key"]) or []):
                    add(self.home, f"s3://{f['bucket']}/{f['key']}")
            for i in self.record.get("images", []):
                if self.read("ecr", "describe-images", "--repository-name", i["repository"], "--image-ids", f"imageTag={i['tag']}",
                             missing=("ImageNotFoundException", "RepositoryNotFoundException")):
                    add(self.home, f"ecr://{i['repository']}:{i['tag']}")

        def resolver() -> None:
            for c in (self.read("route53resolver", "list-resolver-query-log-configs") or {}).get("ResolverQueryLogConfigs", []):
                if c["Name"].startswith(self.p + "-"):
                    add(self.home, c["Arn"])
            doc = self.read("route53resolver", "list-resolver-query-log-config-associations") or {}
            for a in doc.get("ResolverQueryLogConfigAssociations", []):
                if a["ResourceId"] in vpcs:
                    add(self.home, f"arn:aws:route53resolver:{w}:resolver-query-log-config-association/{a['Id']}")

        def prcheck() -> None:
            for proj in (self.read("codebuild", "batch-get-projects", "--names", f"{self.p}-prcheck") or {}).get("projects", []):
                add(self.home, proj["arn"])

        for label, fn in (("AgentCore", agentcore), ("CodeArtifact", codeartifact), ("Network Firewall", firewalls),
                          ("EC2", network), ("log groups", log_groups),
                          ("CDK assets", assets), ("IAM", lambda: [add("global", r["Arn"]) for r in self.roles()]),
                          ("Route 53 Resolver", resolver), ("CodeCommit", lambda: [add(self.home, arn) for _, arn in self.e2e_repos()]),
                          ("CodeBuild project", prcheck), ("EventBridge", lambda: [add(self.home, r["Arn"]) for r in self.rules()]),
                          ("CloudWatch dashboards", lambda: [add(self.home, d["DashboardArn"]) for d in self.dashboards()]),
                          ("saved queries", lambda: [add(self.home, f"arn:aws:logs:{w}:query-definition/{q['queryDefinitionId']} ({q['name']})")
                                                     for q in self.query_definitions()])):
            source(label, fn)
        return list(items), count["pending"]

    def gone(self, arn: str, region: str) -> tuple[str, str] | None:
        """(read, answer) when the ARN's own service answers that it does not exist (EXISTENCE); None when it exists, when
        the read answers anything else, or when the type has no existence read."""
        for rx, build, codes, absent in EXISTENCE:
            m = rx.match(arn)
            if not m:
                continue
            args = build(m, arn)
            ok, out, err = self.call(args, region)
            api = f"{args[0]} {args[1]}"
            if ok:
                try:
                    doc = json.loads(out) if out.strip() else {}
                except ValueError:
                    return None
                return (api, "absent") if absent is not None and absent(doc, m) else None
            hit = next((c for c in codes if c in err), None)
            return (api, hit) if hit else None
        return None

    def settled_inventory(self) -> tuple[list[tuple[str, str]], int]:
        """The inventory. A full run reads it again every TEARDOWN_SETTLE_POLL_S until two consecutive reads are empty or
        TEARDOWN_SETTLE_S has passed (then the last read decides): one empty read can be the tagging index omitting a
        resource it has not indexed yet, and a non-empty one can be a resource still going."""
        cur = self.inventory()
        if self.dry or self.e2e_only:
            return cur
        deadline = time.monotonic() + self.settle_s
        prev = None
        while True:
            if prev is not None and not prev[0] and not cur[0]:
                return cur
            if prev is not None and time.monotonic() >= deadline:
                if prev[0] != cur[0]:
                    self.notes.append(f"inventory unsettled after {self.settle_s:.0f} s: the last two reads differ")
                return cur
            time.sleep(self.poll_s)
            prev, cur = cur, self.inventory()

    def e2e_inventory(self) -> list[str]:
        """The E2E repositories, run prefixes and running executions still present."""
        out = [arn for _, arn in self.e2e_repos()]
        runs = self.e2e_runs(self.items())
        out += [f"s3://{self.bucket}/runs/{rid}/" for rid in sorted(runs) if self.versions(self.bucket, f"runs/{rid}/")]
        out += [e["executionArn"] for e in self.running_executions(runs)]
        return out

    # ---- main ---------------------------------------------------------------------------------

    def full(self) -> None:
        self.guard("asset record", self.read_record)
        items = self.guard("harness table", self.items) or []
        self.guard("sessions", lambda: self.stop_sessions(items, None))
        self.guard("executions", lambda: self.stop_executions(None))
        self.guard("invocation logging restore", self.restore_invocation_logging)
        self.vpcs = self.guard("stage VPC", self.stage_vpcs) or []
        # Without the app's dependency order no stack of the app is destroyed: stop here. The previous generation has no app to synthesize.
        self.order = self.stack_order() if self.names == "new" else []
        failed = self.guard("stacks", self.destroy_stacks) or []
        for name, fn in (("AgentCore", self.agentcore), ("AgentCore gateways", self.gateways), ("buckets and repositories", self.leftover_storage),
                         ("log groups", self.delete_log_groups), ("KMS", self.kms),
                         ("secrets", self.secrets), ("SSM parameters", self.ssm_parameters), ("CDK assets", self.cdk_assets),
                         ("E2E repositories", self.delete_e2e_repos),
                         ("prcheck project", self.prcheck), ("EventBridge rules", self.delete_rules), ("trails", self.delete_trails),
                         ("alarms", self.delete_alarms), ("dashboards", self.delete_dashboards),
                         ("saved queries", self.delete_query_definitions)):
            self.guard(name, fn)
        self.guard("deferred retry", lambda: self.deferred(failed))

    def e2e(self) -> None:
        items = self.guard("harness table", self.items) or []
        runs = self.e2e_runs(items)
        say(f"E2E runs: {', '.join(sorted(runs)) or 'none'}")
        self.guard("sessions", lambda: self.stop_sessions(items, runs))
        self.guard("executions", lambda: self.stop_executions(runs))
        self.guard("E2E repositories", self.delete_e2e_repos)
        self.guard("run prefixes", lambda: self.empty_run_prefixes(runs))

    def read_digests(self) -> None:
        """deploy.sh's digests of the stage, for the record's header: read before anything is changed (the teardown deletes
        the tagged /hamh/<stage>/* parameters). An absent or unreadable one is null and said in the notes."""
        for name in DIGEST_PARAMS:
            full = f"{self.ssm}/{name}"
            ok, out, err = self.call(["ssm", "get-parameter", "--name", full])
            value = None
            if ok:
                try:
                    value = (json.loads(out).get("Parameter") or {}).get("Value") or None
                except ValueError:
                    value = None
            if value is None:
                why = "absent" if (not ok and "ParameterNotFound" in err) or ok else f"unreadable ({err.splitlines()[-1] if err else 'failed'})"
                self.notes.append(f"{full}: {why} before the teardown")
            self.pre[name] = value

    def suffix(self) -> str:
        """The mode in a file name: -e2e, -dry-run, and -previous-names for the previous generation (so its evidence never takes the name of the current one's)."""
        return ("-e2e" if self.e2e_only else "") + ("-dry-run" if self.dry else "") + ("-previous-names" if self.names == "old" else "")

    def record_path(self) -> Path:
        """<UTC date>-<HHMMSS>-teardown-<stage><suffix>.json: the .md's suffixes, so a full run that is not a dry run
        is `*-teardown-direct.json`; a new name per run (a run never overwrites an earlier record)."""
        suffix = self.suffix()
        while True:
            p = self.evidence / f"{dt.datetime.now(dt.timezone.utc):%Y-%m-%d-%H%M%S}-teardown-{self.stage}{suffix}.json"
            if not p.exists():
                return p
            time.sleep(1)  # nosemgrep: arbitrary-sleep -- bounded wait between polls, retries or settle checks in tooling, nothing depends on the delay

    def write_record(self, items: list[tuple[str, str]], pending: int, lines: list[str], md: Path) -> Path:
        """The record of this run."""
        here = Path(__file__).resolve()
        files = {"scripts/teardown.sh": here.parents[1] / "teardown.sh", "scripts/lib/teardown.py": here,
                 "scripts/lib/aws_target.py": Path(aws_target.__file__).resolve(), "scripts/lib/python.sh": here.parent / "python.sh"}
        sha = lambda p: hashlib.sha256(p.read_bytes()).hexdigest() if p.is_file() else None  # noqa: E731
        sources = {rel: sha(files[rel]) for rel in SOURCES}
        code = subprocess.run(["git", "-C", str(here.parents[2]), "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()  # nosec B603: argument list built here, no shell, program is git, the interpreter or a configured binary; B607: bare name of a standard tool such as git, bash, tar or aws looked up on the PATH of the host running this tool
        path = self.record_path()
        created = dt.datetime.strptime(path.name[:17], "%Y-%m-%d-%H%M%S").strftime("%Y-%m-%dT%H:%M:%SZ")
        rec = {
            "schemaVersion": 1, "kind": f"teardown-{self.stage}", "createdAt": created, "host": socket.getfqdn(),
            "account": self.account, "region": self.home, "stage": self.stage, "codeSha": code or None,
            "sourceTree": self.pre.get("deployed-source-tree"), "deploymentDigest": self.pre.get("deployment-digest"),
            "imageDigest": self.pre.get("image-digest"), "criteriaSha256": sha(self.criteria),
            "result": "fail" if items else "pass",
            "inputs": {"teardownSha256": sources["scripts/teardown.sh"], "teardownPySha256": sources["scripts/lib/teardown.py"],
                       "sources": sources, "stage": self.stage, "names": self.names, "mode": "e2e-only" if self.e2e_only else "full", "dryRun": self.dry,
                       "profile": self.env["AWS_PROFILE"], "regions": list(REGIONS), "evidenceMd": md.name},
            "measurements": {"inventory": len(items), "kmsPending": pending, "items": [{"region": r, "id": i} for r, i in items],
                             "kept": self.kept, "dropped": self.dropped, "inventoryLine": lines[-1], "inventoryReads": self.reads,
                             "commands": len(self.commands)},
            "runIds": [], "notes": "; ".join(self.notes) or "none",
        }
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_name(path.name + ".tmp")
        tmp.write_text(json.dumps(rec, indent=1) + "\n")
        os.replace(tmp, path)
        return path

    def evidence_path(self) -> Path:
        suffix = self.suffix()
        return self.evidence / f"{dt.datetime.now(dt.timezone.utc):%Y-%m-%d}-teardown-{self.stage}{suffix}.md"

    def write_evidence(self, lines: list[str], started: dt.datetime) -> Path:
        here = Path(__file__).resolve()
        sha = lambda p: hashlib.sha256(p.read_bytes()).hexdigest() if p.is_file() else "missing"  # noqa: E731
        code = subprocess.run(["git", "-C", str(here.parents[2]), "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()  # nosec B603: argument list built here, no shell, program is git, the interpreter or a configured binary; B607: bare name of a standard tool such as git, bash, tar or aws looked up on the PATH of the host running this tool
        mode = ("e2e-only" if self.e2e_only else "full") + (", dry run" if self.dry else "") + (", previous names" if self.names == "old" else "")
        body = [
            f"# Teardown of stage {self.stage} ({mode})", "",
            f"- started: {started:%Y-%m-%dT%H:%M:%SZ}; finished: {dt.datetime.now(dt.timezone.utc):%Y-%m-%dT%H:%M:%SZ}",
            f"- account: {self.account}; Regions: {', '.join(REGIONS)}; profile: {self.env['AWS_PROFILE']}",
            f"- code: {code or 'unknown'}",
            f"- scripts/teardown.sh sha256: {sha(here.parents[1] / 'teardown.sh')}",
            f"- scripts/lib/teardown.py sha256: {sha(here)}", "",
            "## Commands", "", *([f"- `{c}`" for c in self.commands] or ["- none"]), "",
            "## Notes", "", *([f"- {n}" for n in self.notes] or ["- none"]), "",
            "## Inventory", "", "```", *lines, "```", "",
        ]
        path = self.evidence_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("\n".join(body))
        return path

    def run(self, probe: bool = False, announce: bool = True) -> tuple[list[tuple[str, str]], int, bool]:
        """One generation's teardown. Returns (what is left as (region, id), KMS keys pending deletion, whether it ran). `probe` (the previous generation of a run that
        covers both): one read of the inventory decides, and a clean generation adds no evidence file. `announce` prints this generation's TEARDOWN-INVENTORY line;
        a run of several generations prints one line for all of them (main)."""
        started = dt.datetime.now(dt.timezone.utc)
        self.check_account()
        if probe:
            found, pending = self.inventory()
            if not found:
                say(f"{GENERATION_WORDS[self.names]}: nothing found in the previous-names pass.")
                return [], pending, False
        self.read_digests()
        say(f"stage={self.stage} mode={'e2e-only' if self.e2e_only else 'full'}{' dry-run' if self.dry else ''}{' names=previous' if self.names == 'old' else ''}")
        if self.e2e_only:
            self.e2e()
        else:
            self.full()
        self.guard("invocation logging", self.invocation_logging)
        items, pending = self.settled_inventory()
        for d in self.dropped:
            msg = f"tagging index lists {d['arn']}; {d['api']} says {d['error']}: not counted"
            say(msg)
            self.notes.append(msg)
        lines = [f"INVENTORY {region} {rid}" for region, rid in items]
        if items and self.network_hold:
            self.say_hold(self.network_only(items))
        if items:
            lines.append(f"TEARDOWN-INVENTORY {len(items)} resources")
        else:
            lines.append(f"TEARDOWN-INVENTORY 0 resources stage={self.stage} kms-pending={pending}")
        path = self.write_evidence(lines, started)
        say(f"evidence {path}")
        say(f"record {self.write_record(items, pending, lines, path)}")
        for line in lines:
            if announce or line.startswith("INVENTORY "):
                print(line, flush=True)
        return items, pending, True


def usage(msg: str) -> int:
    print(f"teardown.sh: {msg}\nusage: scripts/teardown.sh --stage direct [--e2e-only] [--dry-run] [--yes] [--names new|old|both] [--network-wait-minutes N|full]", file=sys.stderr)
    return 2


def main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(add_help=False)
    p.add_argument("--stage", choices=("direct",))
    p.add_argument("--e2e-only", action="store_true")
    p.add_argument("--dry-run", action="store_true")
    p.add_argument("--yes", action="store_true")
    p.add_argument("--names", choices=("new", "old", "both"), default="both")
    p.add_argument("--network-wait-minutes", action="append", default=[])
    try:
        args, extra = p.parse_known_args(argv)
    except SystemExit:
        return usage("bad arguments")
    if extra or not args.stage:
        return usage("bad arguments" if extra else "--stage is required")
    # How long to wait for AWS to release AgentCore's network interface. The flag, else TEARDOWN_NETWORK_TIMEOUT_S (seconds), else one hour; both are checked before any AWS call.
    wait_s: float | None = None
    if len(args.network_wait_minutes) > 1:
        return usage("--network-wait-minutes is given once")
    if args.network_wait_minutes:
        minutes = wait_minutes(args.network_wait_minutes[0])
        if minutes is None:
            return usage(f"--network-wait-minutes must be a whole number of minutes from 0 to {NETWORK_WAIT_MAX_MINUTES}, or full (got '{args.network_wait_minutes[0]}')")
        wait_s = minutes * 60.0
    elif wait_seconds_from_env(os.environ.get("TEARDOWN_NETWORK_TIMEOUT_S")) is None:
        return usage(f"TEARDOWN_NETWORK_TIMEOUT_S must be a number of seconds from 0 to {NETWORK_WAIT_MAX_MINUTES * 60} (got '{os.environ.get('TEARDOWN_NETWORK_TIMEOUT_S')}')")
    typed = []
    if "--names" in argv or any(a.startswith("--names=") for a in argv):
        typed += ["--names", args.names]
    if args.e2e_only:
        typed += ["--e2e-only"]
    if args.network_wait_minutes:
        typed += ["--network-wait-minutes", args.network_wait_minutes[0]]
    invocation = " ".join(shlex.quote(x) for x in typed)
    if not args.dry_run and not args.yes:
        print("teardown.sh: refusing to change anything without --yes (or pass --dry-run)", file=sys.stderr)
        return 2
    gens = ["new", "old"] if args.names == "both" else [args.names]
    if len(gens) == 1:
        other = GENERATION_WORDS["old" if args.names == "new" else "new"]
        say(f"--names {args.names}: {other} were not covered; this run does not read or remove any stage under the {other}.")
    left: list[tuple[str, str]] = []
    pending_total = 0
    failures: list[str] = []
    deferred = True  # every generation that left something left only network resources that AgentCore's interface holds
    for g in gens:
        try:
            t = Teardown(args.stage, args.e2e_only, args.dry_run, g, network_wait_s=wait_s, invocation=invocation)
        except aws_target.TargetError as e:
            return usage(str(e))
        try:
            items, pending, _ran = t.run(probe=(g == "old" and len(gens) > 1), announce=len(gens) == 1)
        except Fail as e:
            failures.append(str(e))
            continue
        if items and not (t.network_hold and not t.network_hold.get("others") and t.network_only(items)):
            deferred = False
        left += items
        pending_total += pending
    if failures:
        print(f"TEARDOWN-FAIL {'; '.join(dict.fromkeys(failures))}", flush=True)
        return 1
    if len(gens) > 1:
        if left:
            print(f"TEARDOWN-INVENTORY {len(left)} resources", flush=True)
        else:
            print(f"TEARDOWN-INVENTORY 0 resources stage={args.stage} kms-pending={pending_total}", flush=True)
    if left and deferred:
        return 9
    return 1 if left else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
