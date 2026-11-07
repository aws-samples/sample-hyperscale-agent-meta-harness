"""A stand-in AWS CLI for the tests of the setup wizard.

The tests put a one-line launcher named `aws` first on PATH (`install`); it calls `main` of this module. The stand-in keeps a small world in a
state directory (`FAKE_AWS_STATE`): CloudFormation stacks, an S3 store that keeps object versions, CodeBuild builds, the log of every call
(`calls.jsonl`) and a scenario file (`FAKE_AWS_SCENARIO`, JSON) that a test rewrites between two runs to change what the next call does.

It answers the calls the wizard makes and nothing else: an argument or a query it does not know is an error (exit 252), so a script that
drifts from the calls the tests pin fails loudly. It speaks the AWS CLI's `--query` (a JMESPath subset: fields, indexes, `[]` projections,
multi-select lists, filters with `==` and `starts_with`) and its `--output text` and `--output json` forms. Canned bodies come from
fixtures/setup/aws/. Standard library only, Python 3.8 or later.

What the runner would do in the account is played here as well: each `codebuild batch-get-builds` poll moves the build one step, writes
`results/<buildId>/phases.jsonl` as it goes and, at the end, `result.json` and the client files (see `Build.advance`).
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
FIXTURES = REPO / "fixtures" / "setup" / "aws"
sys.path.insert(0, str(REPO / "scripts" / "setup"))
import runner_state  # noqa: E402  the runner's own writer of phases.jsonl and result.json: the stand-in plays the runner in the real formats
sys.path.insert(0, str(Path(__file__).resolve().parent))
import s3_versions_model as s3model  # noqa: E402  the rules of S3 listing and multi-object delete, and of the AWS CLI over them

ACCOUNT = "210987654321"
ENDPOINT = "https://abc123.execute-api.us-east-1.amazonaws.com/direct"
PLATFORMS = ("darwin-arm64", "linux-amd64", "linux-arm64")
CLIENT_BYTES = b"#!/bin/sh\necho 'ham v9.99 from the runner'\n"
ADMIN_BYTES = b"#!/bin/sh\necho 'ham-admin v9.99 from the runner'\n"
STAGE_STACKS = ("hamh-direct-network", "hamh-direct-data", "hamh-direct-control")
PROJECT_TAGS = {"project": "hyperscale-agent-meta-harness", "stage": "direct"}
# The previous generation of names (the stages that releases before the rename deployed): the same world under the first word and the project tag of that generation.
# lib.sh of scripts/setup/ holds the frozen row; these two lines are the stand-in's copy of it, and everything else is derived from the current names by leg().
LEGACY_WORD = "cah"  # COMPAT-CAH-STAGE: the first word of the stage names of the previous generation
LEGACY_PROJECT = "cline-agentcore-harness"  # COMPAT-CAH-STAGE: the project tag value of the previous generation
GEN_TAGS = {"current": PROJECT_TAGS, "legacy": {"project": LEGACY_PROJECT, "stage": "direct"}}
# Ids that two stages of one account must not share (a VPC, a key, a runtime id suffix, a secret suffix, a REST API id): the previous generation gets its own.
_LEGACY_IDS = (("key-runner-0001", "key-runner-leg0001"), ("0abc1234", "0leg1234"), ("key-0001", "key-leg0001"), ("abcde12345", "abcde54321"), ("fghij67890", "fghij09876"),
               ("AbCdEf", "GhIjKl"), ("abc123", "leg123"))
# The id of the stage's AgentCore runtime (the name, a hyphen and ten characters), the name of the research secret (infra/lib/config.ts) and the AgentCore log group
# that its DEFAULT endpoint writes (`/aws/bedrock-agentcore/runtimes/<runtime id>-<endpoint>`).
RUNTIME_ID = "hamh_direct_worker_python-abcde12345"
INSTANCES_RUNTIME_ID = "hamh_direct_harness_worker_python_instances-fghij67890"
RESEARCH_KEY_NAME = "hamh-direct-research-tavily-api-key"
RUNNER_KEY_ID = "key-runner-0001"
# The id of every stack the stand-in lists (the fixture's), and the id of an earlier stack of the same name: a deleted stack and a new one with its name are two stacks.
STACK_UUID = "aaaaaaaa-0000-4000-8000-a00000000000"
OLD_STACK_UUID = "bbbbbbbb-0000-4000-8000-b00000000000"


def leg(value):
    """A name, a list or a dict of the current generation as the previous generation spells it: the first word and the project tag, and the ids of _LEGACY_IDS. A tuple stays a tuple."""
    if isinstance(value, str):
        for a, b in _LEGACY_IDS:
            value = value.replace(a, b)
        return value.replace("hamh", LEGACY_WORD).replace(PROJECT_TAGS["project"], LEGACY_PROJECT)
    if isinstance(value, tuple):
        return tuple(leg(v) for v in value)
    if isinstance(value, list):
        return [leg(v) for v in value]
    if isinstance(value, dict):
        return {leg(k): leg(v) for k, v in value.items()}
    return value


def gen_text(value, gen: str = "current"):
    """`value` in the names of the generation (current is unchanged)."""
    return leg(value) if gen == "legacy" else value


def exec_role_arn(acct: str, region: str) -> str:
    """The CloudFormation execution role of the default CDK bootstrap."""
    return f"arn:aws:iam::{acct}:role/cdk-hnb659fds-cfn-exec-role-{acct}-{region}"
# The phases scripts/setup/runner-build.sh runs for a deploy, in order.
DEPLOY_PHASES = ["inputs", "fetch", "toolchain", "account-file", "deploy", "gomod", "scanner-rules", "client", "outputs"]
# ... and for the build that prints the CDK bootstrap template (SETUP_ACTION=bootstrap-template).
TEMPLATE_PHASES = ["inputs", "fetch", "toolchain", "account-file", "template"]
# The CDK bootstrap template of the stand-in world and its SHA-256, which FullBase writes into the pin of the stand-in checkout.
BOOTSTRAP_TEMPLATE = b"AWSTemplateFormatVersion: '2010-09-09'\nDescription: stand-in for the CDK bootstrap template\nParameters:\n  CloudFormationExecutionPolicies:\n    Type: CommaDelimitedList\nResources: {}\n"
BOOTSTRAP_SHA = hashlib.sha256(BOOTSTRAP_TEMPLATE).hexdigest()


def stage_model(nine: bool, acct: str, region: str = "us-east-1", gen: str = "current") -> dict:
    """The stage of generation `gen` (current or legacy), in the names of that generation. The stacks of the stand-in stage, deploy order: the resources of each as (logical id, type, physical id), the exports it makes (output key to export name) and
    the exports it imports. Three stacks by default, the nine of the product with `nine_stacks`; the cross-stack exports are what CloudFormation checks before it
    deletes a stack. Physical ids are the forms CloudFormation lists: an ARN for a state machine, a secret and an AgentCore runtime, a name for a rule, a function, a
    table and a CodeBuild project."""
    return gen_text(_stage_model(nine, acct, region), gen)


def _stage_model(nine: bool, acct: str, region: str) -> dict:
    """stage_model in the current names."""
    def exp(stack: str, key: str) -> str:
        return f"hamh-direct-{stack}:{key}"
    runtime_arn = f"arn:aws:bedrock-agentcore:{region}:{acct}:runtime/{RUNTIME_ID}"
    instances_arn = f"arn:aws:bedrock-agentcore:{region}:{acct}:runtime/{INSTANCES_RUNTIME_ID}"
    machine_arn = f"arn:aws:states:{region}:{acct}:stateMachine:hamh-direct-run"
    ended_arn = f"arn:aws:states:{region}:{acct}:stateMachine:hamh-direct-run-ended"
    network = {"resources": [("Vpc", "AWS::EC2::VPC", "vpc-0abc1234"), ("PrivateSubnet", "AWS::EC2::Subnet", "subnet-0abc1234"),
                             ("WorkerSecurityGroup", "AWS::EC2::SecurityGroup", "sg-0abc1234")],
               "exports": {"VpcId": exp("network", "VpcId")}}
    table = ("HarnessTable", "AWS::DynamoDB::Table", "hamh-direct-harness")
    function = ("ControlApiFunction", "AWS::Lambda::Function", "hamh-direct-control-api")
    if not nine:
        return {
            "network": network,
            "data": {"resources": [("HarnessBucket", "AWS::S3::Bucket", f"hamh-direct-harness-{acct}"), table], "exports": {"BucketName": exp("data", "BucketName")}},
            "control": {"resources": [("Api", "AWS::ApiGateway::RestApi", "abc123"), function], "imports": [exp("network", "VpcId"), exp("data", "BucketName")]},
        }
    return {
        "network": network,
        "security": {"resources": [("DataKey", "AWS::KMS::Key", "key-0001")], "exports": {"KeyArn": exp("security", "KeyArn")}},
        "data": {"resources": [("HarnessBucket", "AWS::S3::Bucket", f"hamh-direct-harness-{acct}"), table], "exports": {"BucketName": exp("data", "BucketName")},
                 "imports": [exp("security", "KeyArn")]},
        "packages": {"resources": [("GoModuleStore", "AWS::S3::Bucket", f"hamh-direct-gomod-{acct}")], "exports": {"GomodBucket": exp("packages", "GomodBucket")},
                     "imports": [exp("network", "VpcId")]},
        # the resource policy of the secret is a resource of its own in the product's stack (ResearchTavilyApiKeyPolicy...); CloudFormation lists it by the ARN of the secret
        "control": {"resources": [("Api", "AWS::ApiGateway::RestApi", "abc123"), function,
                                  ("TavilyApiKey", "AWS::SecretsManager::Secret", f"arn:aws:secretsmanager:{region}:{acct}:secret:{RESEARCH_KEY_NAME}-AbCdEf"),
                                  ("ResearchTavilyApiKeyPolicy39AE7A77", "AWS::SecretsManager::ResourcePolicy", f"arn:aws:secretsmanager:{region}:{acct}:secret:{RESEARCH_KEY_NAME}-AbCdEf")],
                    "exports": {"ApiId": exp("control", "ApiId")}, "imports": [exp("data", "BucketName"), exp("packages", "GomodBucket"), exp("security", "KeyArn")]},
        "runtimes": {"resources": [("WorkerPythonRepo", "AWS::ECR::Repository", "hamh-direct-worker-python"), ("WorkerRuntime", "AWS::BedrockAgentCore::Runtime", runtime_arn),
                                   ("InstancesRuntime", "AWS::BedrockAgentCore::Runtime", instances_arn)],
                     "exports": {"RuntimeArn": exp("runtimes", "RuntimeArn")}, "imports": [exp("network", "VpcId"), exp("control", "ApiId")]},
        "orchestration": {"resources": [("RunMachine", "AWS::StepFunctions::StateMachine", machine_arn), ("EndedMachine", "AWS::StepFunctions::StateMachine", ended_arn),
                                        ("RunEnded", "AWS::Events::Rule", "hamh-direct-run-ended")],
                          "exports": {"MachineArn": exp("orchestration", "MachineArn")}, "imports": [exp("runtimes", "RuntimeArn"), exp("control", "ApiId")]},
        "delivery": {"resources": [("DeliveryRepo", "AWS::ECR::Repository", "hamh-direct-delivery"), ("ImageBuild", "AWS::CodeBuild::Project", "hamh-direct-image-build")],
                     "imports": [exp("orchestration", "MachineArn")]},
        "observability": {"resources": [("RunTokensQuery", "AWS::Logs::QueryDefinition", "hamh-direct/run-tokens")], "imports": [exp("control", "ApiId")]},
    }


def runner_state_text(*argv: str) -> str:
    """What `python3 scripts/setup/runner_state.py <argv>` prints (the runner's own code writes the files the stand-in plays back)."""
    import contextlib
    import io
    buf = io.StringIO()
    with contextlib.redirect_stdout(buf):
        rc = runner_state.main(list(argv))
    if rc != 0:
        raise Refuse(f"runner_state.py {' '.join(argv)} exited {rc}")
    return buf.getvalue()


_PLACEHOLDER = re.compile(r"\{([A-Za-z_]+)\}")


def render(node, values: dict):
    """A fixture with its {placeholders} filled: a string that is only a placeholder becomes the value itself (a list, a number), any other string
    gets the text of the values put in."""
    if isinstance(node, str):
        whole = _PLACEHOLDER.fullmatch(node)
        if whole and whole.group(1) in values:
            return values[whole.group(1)]
        return _PLACEHOLDER.sub(lambda m: str(values[m.group(1)]) if m.group(1) in values else m.group(0), node)
    if isinstance(node, list):
        return [render(x, values) for x in node]
    if isinstance(node, dict):
        return {k: render(v, values) for k, v in node.items()}
    return node


def fx(fixture: str, /, **values):
    """A canned answer of fixtures/setup/aws/ with the values of this call put in."""
    return render(json.loads((FIXTURES / fixture).read_text()), values)


class Refuse(Exception):
    """The stand-in does not know this call."""


class AwsError(Exception):
    def __init__(self, message: str, code: int = 254):
        super().__init__(message)
        self.code = code


# ---- a JMESPath subset -----------------------------------------------------------------------------------

_TOKEN = re.compile(r"""\s*(?:
    (?P<flatten>\[\])|(?P<filter>\[\?)|(?P<lb>\[)|(?P<rb>\])|(?P<lc>\{)|(?P<rc>\})|(?P<lp>\()|(?P<rp>\))|
    (?P<comma>,)|(?P<colon>:)|(?P<dot>\.)|(?P<eq>==)|(?P<ne>!=)|(?P<star>\*)|
    '(?P<raw>(?:[^'\\]|\\.)*)'|"(?P<quoted>(?:[^"\\]|\\.)*)"|(?P<num>-?\d+)|(?P<ident>[A-Za-z_][A-Za-z0-9_\-]*)
)""", re.X)


def _tokens(expr: str):
    pos, out = 0, []
    while pos < len(expr):
        m = _TOKEN.match(expr, pos)
        if not m or m.end() == pos:
            if expr[pos:].strip() == "":
                break
            raise Refuse(f"query: cannot read {expr[pos:]!r} in {expr!r}")
        pos = m.end()
        kind = m.lastgroup
        out.append((kind, m.group(kind)))
    return out


class _Query:
    def __init__(self, expr: str):
        self.expr, self.toks, self.i = expr, _tokens(expr), 0

    def peek(self):
        return self.toks[self.i][0] if self.i < len(self.toks) else None

    def take(self, kind=None):
        if self.i >= len(self.toks) or (kind and self.toks[self.i][0] != kind):
            raise Refuse(f"query: expected {kind} in {self.expr!r}")
        tok = self.toks[self.i]
        self.i += 1
        return tok[1]

    def parse(self):
        node = self.chain()
        if self.i != len(self.toks):
            raise Refuse(f"query: trailing input in {self.expr!r}")
        return node

    # chain := primary ( '.' step | index | flatten | filter )*   -- a projection takes the rest of the chain as its right side
    def chain(self):
        node = self.primary()
        while True:
            kind = self.peek()
            if kind == "dot":
                self.take()
                node = ("sub", node, self.step())
            elif kind == "flatten":
                self.take()
                node = ("project", ("flatten", node), self.rest())
            elif kind == "filter":
                self.take()
                cond = self.condition()
                self.take("rb")
                node = ("project", ("filter", node, cond), self.rest())
            elif kind == "lb":
                self.take()
                n = int(self.take("num"))
                self.take("rb")
                node = ("index", node, n)
            else:
                return node

    def rest(self):
        """The right side of a projection: what follows, up to the end or a closing bracket, comma or brace."""
        if self.peek() in (None, "rb", "rc", "comma", "rp"):
            return ("current",)
        if self.peek() == "dot":
            self.take()
            node = self.step()
        else:
            node = ("current",)
        while self.peek() in ("dot", "lb"):
            if self.peek() == "dot":
                self.take()
                node = ("sub", node, self.step())
            else:
                self.take()
                n = int(self.take("num"))
                self.take("rb")
                node = ("index", node, n)
        return node

    def step(self):
        kind = self.peek()
        if kind == "lb":
            return self.multilist()
        if kind == "lc":
            return self.multihash()
        return self.primary()

    def primary(self):
        kind = self.peek()
        if kind == "ident":
            name = self.take()
            if self.peek() == "lp":
                self.take()
                args = [self.chain()]
                while self.peek() == "comma":
                    self.take()
                    args.append(self.chain())
                self.take("rp")
                return ("call", name, args)
            return ("field", name)
        if kind == "quoted":
            return ("field", self.take().replace('\\"', '"'))
        if kind == "raw":
            return ("literal", self.take())
        if kind == "lb":
            return self.multilist()
        if kind == "lc":
            return self.multihash()
        raise Refuse(f"query: unexpected {kind} in {self.expr!r}")

    def multilist(self):
        self.take("lb")
        items = [self.chain()]
        while self.peek() == "comma":
            self.take()
            items.append(self.chain())
        self.take("rb")
        return ("multilist", items)

    def multihash(self):
        self.take("lc")
        items = []
        while True:
            key = self.take("ident")
            self.take("colon")
            items.append((key, self.chain()))
            if self.peek() != "comma":
                break
            self.take()
        self.take("rc")
        return ("multihash", items)

    def condition(self):
        left = self.chain()
        kind = self.peek()
        if kind in ("eq", "ne"):
            self.take()
            return (kind, left, self.chain())
        return ("truthy", left)


def _eval(node, data):
    kind = node[0]
    if kind == "current":
        return data
    if kind == "field":
        return data.get(node[1]) if isinstance(data, dict) else None
    if kind == "literal":
        return node[1]
    if kind == "sub":
        return _eval(node[2], _eval(node[1], data))
    if kind == "index":
        value = _eval(node[1], data)
        if not isinstance(value, list):
            return None
        try:
            return value[node[2]]
        except IndexError:
            return None
    if kind == "flatten":
        value = _eval(node[1], data)
        if not isinstance(value, list):
            return None
        out = []
        for item in value:
            out.extend(item if isinstance(item, list) else [item])
        return out
    if kind == "filter":
        value = _eval(node[1], data)
        if not isinstance(value, list):
            return None
        return [item for item in value if _cond(node[2], item)]
    if kind == "project":
        value = _eval(node[1], data)
        if not isinstance(value, list):
            return None
        out = [_eval(node[2], item) for item in value]
        return [x for x in out if x is not None]
    if kind == "multilist":
        if data is None:
            return None
        return [_eval(item, data) for item in node[1]]
    if kind == "multihash":
        if data is None:
            return None
        return {key: _eval(item, data) for key, item in node[1]}
    if kind == "call":
        args = [_eval(a, data) for a in node[2]]
        if node[1] == "starts_with":
            return isinstance(args[0], str) and args[0].startswith(args[1])
        raise Refuse(f"query: function {node[1]}")
    raise Refuse(f"query: node {kind}")


def _cond(cond, item) -> bool:
    kind = cond[0]
    if kind == "eq":
        return _eval(cond[1], item) == _eval(cond[2], item)
    if kind == "ne":
        return _eval(cond[1], item) != _eval(cond[2], item)
    return bool(_eval(cond[1], item))


def jmespath(expr: str, data):
    return _eval(_Query(expr).parse(), data)


def _scalar_text(value) -> str:
    if value is None:
        return "None"
    if isinstance(value, bool):
        return "True" if value else "False"
    return str(value)


def as_text(value) -> str:
    """The AWS CLI's --output text for the shapes the wizard asks for: a scalar, a list of scalars or a list of lists."""
    if isinstance(value, list):
        if all(not isinstance(x, (list, dict)) for x in value):
            return "\t".join(_scalar_text(x) for x in value) + ("\n" if value else "")
        return "".join(as_text(x) if isinstance(x, list) else as_text([x]) for x in value)
    if isinstance(value, dict):
        raise Refuse("output text of a dictionary")
    return _scalar_text(value) + "\n"


# ---- the world -------------------------------------------------------------------------------------------

class World:
    def __init__(self, state: Path, scenario_path: Path):
        self.dir = state
        self.scenario = json.loads(scenario_path.read_text()) if scenario_path.is_file() else {}
        self.path = state / "world.json"
        self.w = json.loads(self.path.read_text()) if self.path.is_file() else {
            "stacks": {}, "builds": {}, "n_builds": 0, "versions": {}, "n_version": 0, "faults_seen": {}, "bootstrap": None}
        if self.w["bootstrap"] is None:
            self.w["bootstrap"] = bool(self.scenario.get("bootstrap_present"))
            if self.w["bootstrap"]:
                self.w["stacks"]["CDKToolkit"] = {"status": "CREATE_COMPLETE", "outputs": {}}
            for name, st in (self.scenario.get("stacks") or {}).items():
                self.w["stacks"].setdefault(name, st)
            if self.scenario.get("stage_deployed"):
                # `unfinished_deploy`: every stack of the stage exists but the deploy stopped before its end (the first pass of the two-pass deploy, a failed image
                # build): scripts/lib/deploy.py writes /hamh/<stage>/deployment-digest last, so the stage has none.
                self.deploy_stage(record=not self.scenario.get("unfinished_deploy"))
            if self.scenario.get("runner_present"):
                self.make_runner()
            # the previous generation of names: `legacy_stage_deployed` (with `legacy_unfinished_deploy`), `legacy_runner_present`, and `foreign_stacks`: stacks that carry
            # the name of one generation and the project tag of the other, or another tag set of their own ({"name", "tags": "current"|"legacy"|{...}, "status"})
            if self.scenario.get("legacy_stage_deployed"):
                self.deploy_stage(record=not self.scenario.get("legacy_unfinished_deploy"), gen="legacy")
            if self.scenario.get("legacy_runner_present"):
                self.make_runner("legacy")
            for fs in self.scenario.get("foreign_stacks") or []:
                tags = GEN_TAGS[fs["tags"]] if isinstance(fs.get("tags"), str) else dict(fs.get("tags") or {})
                self.w["stacks"].setdefault(fs["name"], {"status": fs.get("status", "CREATE_COMPLETE"), "outputs": {}, "exports": {}, "imports": [], "tags": dict(tags),
                                                         "resources": []})

    def save(self) -> None:
        self.path.write_text(json.dumps(self.w))

    # -- S3: a store with versions and delete markers
    def obj_dir(self, bucket: str) -> Path:
        d = self.dir / "s3" / bucket
        d.mkdir(parents=True, exist_ok=True)
        return d

    def put(self, bucket: str, key: str, body: bytes) -> str:
        p = self.obj_dir(bucket) / key
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(body)
        self.w["n_version"] += 1
        vid = f"v{self.w['n_version']}"
        self.w["versions"].setdefault(bucket, {}).setdefault(key, []).append({"id": vid, "marker": False})
        return vid

    def get(self, bucket: str, key: str):
        vs = self.w["versions"].get(bucket, {}).get(key)
        if not vs or vs[-1]["marker"]:
            return None
        p = self.obj_dir(bucket) / key
        return p.read_bytes() if p.is_file() else None

    def mark_deleted(self, bucket: str, key: str) -> None:
        self.w["n_version"] += 1
        self.w["versions"].setdefault(bucket, {}).setdefault(key, []).append({"id": f"v{self.w['n_version']}", "marker": True})

    def seed_entries(self, bucket: str, versions: int, markers: int, layout: str = "markers-first") -> None:
        """Fill a bucket with that many object versions and delete markers (no bodies), in a layout of s3_versions_model.shape."""
        self.w.setdefault("buckets", {})[bucket] = True
        for e in s3model.shape(versions, markers, layout):
            self.w["n_version"] += 1
            self.w["versions"].setdefault(bucket, {}).setdefault(e["Key"], []).append({"id": e["VersionId"], "marker": e["marker"]})

    def bucket_exists(self, bucket: str) -> bool:
        return bucket in self.w["versions"] or bool(self.w.get("buckets", {}).get(bucket))

    # -- CloudFormation
    def runner_names(self, gen: str = "current") -> dict:
        acct, region = self.account(), self.region()
        return gen_text({"SetupBucket": f"hamh-setup-{acct}-{region}", "BuildProject": "hamh-setup-runner-build",
                         "LogGroup": "/aws/codebuild/hamh-setup-runner", "RunnerRoleArn": f"arn:aws:iam::{acct}:role/hamh-setup-runner-role"}, gen)

    @staticmethod
    def runner_stack_gen(name: str):
        """The generation whose runner stack this is (current or legacy), None for any other name."""
        return next((g for g in ("current", "legacy") if name == gen_text("hamh-setup-runner", g)), None)

    @staticmethod
    def stage_stack_gen(name: str):
        """The generation whose stage-stack prefix the name has (current or legacy), None for any other name."""
        return next((g for g in ("current", "legacy") if name.startswith(gen_text("hamh-direct-", g))), None)

    @staticmethod
    def gomod_bucket_gen(bucket: str):
        """The generation of a Go module store bucket name, None for any other name."""
        return next((g for g in ("current", "legacy") if bucket.startswith(gen_text("hamh-direct-gomod-", g))), None)

    def account(self) -> str:
        return self.scenario.get("account", ACCOUNT)

    def stack_id(self, name: str, uuid: str = STACK_UUID) -> str:
        """The id CloudFormation gives a stack (the fixture's); a stack with the name of an earlier one has another uuid."""
        return f"arn:aws:cloudformation:{self.region()}:{self.account()}:stack/{name}/{uuid}"

    def region(self) -> str:
        return self.scenario.get("region", "us-east-1")

    def make_runner(self, gen: str = "current") -> None:
        names = self.runner_names(gen)
        key = gen_text(RUNNER_KEY_ID, gen)
        # the resources of the runner stack that a teardown reads: its encrypted bucket, its key (7-day deletion window) and its role
        self.w["stacks"][gen_text("hamh-setup-runner", gen)] = {"status": "CREATE_COMPLETE", "outputs": names, "tags": {"project": GEN_TAGS[gen]["project"], "managed-by": "setup.sh"},
                                                                "resources": [{"logical": "SetupBucket", "type": "AWS::S3::Bucket", "id": names["SetupBucket"]},
                                                                              {"logical": "RunnerKey", "type": "AWS::KMS::Key", "id": key},
                                                                              {"logical": "RunnerRole", "type": "AWS::IAM::Role", "id": gen_text("hamh-setup-runner-role", gen)}]}
        self.w.setdefault("kms", {}).setdefault(key, {"state": "Enabled", "deletion": None})
        self.w.setdefault("buckets", {})[names["SetupBucket"]] = True
        self.obj_dir(names["SetupBucket"])

    def record_failure(self, name: str, rows: list, reason: str) -> None:
        """The events of one delete that failed, newest first (the stack's DELETE_FAILED, each resource's DELETE_FAILED, the stack's DELETE_IN_PROGRESS), put before the events of
        the earlier tries: what describe-stack-events lists for the stack."""
        st = self.w["stacks"][name]
        sid = self.stack_id(name, st.get("uuid") or STACK_UUID)
        attempt = []
        if rows:
            attempt.append({"StackId": sid, "StackName": name, "LogicalResourceId": name, "PhysicalResourceId": sid, "ResourceType": "AWS::CloudFormation::Stack",
                            "ResourceStatus": "DELETE_FAILED", "ResourceStatusReason": "The following resource(s) failed to delete: [" + ", ".join(r[0] for r in rows) + "]. "})
        for logical, rtype, rid in rows:
            attempt.append({"StackId": sid, "StackName": name, "LogicalResourceId": logical, "PhysicalResourceId": rid, "ResourceType": rtype,
                            "ResourceStatus": "DELETE_FAILED", "ResourceStatusReason": reason})
        attempt.append({"StackId": sid, "StackName": name, "LogicalResourceId": name, "PhysicalResourceId": sid, "ResourceType": "AWS::CloudFormation::Stack",
                        "ResourceStatus": "DELETE_IN_PROGRESS", "ResourceStatusReason": "User Initiated"})
        st["events"] = attempt + st.get("events", [])

    def tick(self) -> None:
        """Time passes between two calls: a secret that was deleted without a recovery window is still described for a while ("marked for deletion") and then gone."""
        gone = []
        for rid, sec in self.w.get("secrets", {}).items():
            if sec.get("vanish") is not None:
                sec["vanish"] -= 1
                if sec["vanish"] <= 0:
                    gone.append(rid)
        for rid in gone:
            del self.w["secrets"][rid]

    def secret_marked(self, rid: str) -> bool:
        """True while the secret at this ARN was force-deleted and is not gone yet: CloudFormation cannot delete the resource policy of such a secret."""
        sec = self.w.get("secrets", {}).get(rid)
        return sec is not None and sec.get("vanish") is not None

    def deploy_stage(self, status: str = "CREATE_COMPLETE", record: bool = True, gen: str = "current") -> None:
        """The stage of the stand-in: three stacks, or the nine of the product with the scenario flag nine_stacks. Each stack has the resources and the cross-stack
        exports of stage_model, and the things those resources stand for (objects, images, runtimes, keys, log groups, parameters) are put in the world. Every stack
        carries the project and stage tags that the CDK app puts on each stack of the stage, and the `owner` tag when the scenario names a `stage_owner`."""
        model = stage_model(bool(self.scenario.get("nine_stacks")), self.account(), self.region(), gen)
        for short, spec in model.items():
            name = gen_text(f"hamh-direct-{short}", gen)
            self.w["stacks"][name] = {"status": status, "outputs": {k: f"value-of-{k}" for k in spec.get("exports", {})}, "exports": dict(spec.get("exports", {})),
                                      "imports": list(spec.get("imports", [])), "resources": [{"logical": a, "type": b, "id": c} for a, b, c in spec["resources"]],
                                      "tags": {**GEN_TAGS[gen], **({"owner": self.scenario["stage_owner"]} if self.scenario.get("stage_owner") else {})}}
        endpoint = self.scenario.get("endpoint", ENDPOINT) if gen == "current" else self.scenario.get("legacy_endpoint", leg(ENDPOINT))
        self.w["stacks"][gen_text("hamh-direct-control", gen)]["outputs"]["V1ApiUrl"] = endpoint
        self.seed_stage_things(model, record=record, gen=gen)

    # ---- the things the resources of the stage stand for ---------------------------------------------------------------
    def seed_stage_things(self, model: dict, record: bool = True, gen: str = "current") -> None:
        """Idempotent: a second deploy (an update) finds the things there and leaves them as they are. `record` False leaves out the parameters that
        scripts/lib/deploy.py writes after its last step (the deployment digest and its two companions). `model` is the stage of generation `gen` (stage_model gives it
        in that generation's names); the things it stands for carry that generation's tags."""
        w = self.w
        tags = dict(GEN_TAGS[gen])
        names_of = lambda text: gen_text(text, gen)  # noqa: E731  a name written in the current generation, in this generation's words
        for short, spec in model.items():
            for logical, rtype, rid in spec["resources"]:
                if rtype == "AWS::S3::Bucket":
                    if rid in w.setdefault("buckets", {}):
                        continue
                    w["buckets"][rid] = True
                    w.setdefault("bucket_tags", {})[rid] = dict(tags)
                    self.obj_dir(rid)
                    for key in ("runs/r1/summary.json", "runs/r1/events.jsonl"):
                        self.put(rid, key, b"first version\n")
                        self.put(rid, key, b"second version\n")
                    self.put(rid, "runs/r2/gone.json", b"deleted later\n")
                    self.mark_deleted(rid, "runs/r2/gone.json")
                elif rtype == "AWS::ECR::Repository":
                    w.setdefault("ecr", {}).setdefault(rid, [{"digest": f"sha256:{n:064x}", "tags": [tag]} for n, tag in ((1, "latest"), (2, "v1"), (3, None))])
                elif rtype == "AWS::BedrockAgentCore::Runtime":
                    runtime_id = rid.rsplit("/", 1)[-1]
                    # the microVM runtime has the endpoints of two deploys (v1, v2) besides its own; the Instances runtime only its own
                    w.setdefault("runtimes", {}).setdefault(runtime_id, {"name": runtime_id.rsplit("-", 1)[0], "arn": rid,
                                                                         "endpoints": ["DEFAULT", "live"] + ([] if "instances" in runtime_id else ["v1", "v2"]),
                                                                         "tags": dict(tags)})
                elif rtype == "AWS::KMS::Key":
                    w.setdefault("kms", {}).setdefault(rid, {"state": "Enabled", "deletion": None})
                elif rtype == "AWS::SecretsManager::Secret":
                    name = rid.split(":secret:", 1)[1].rsplit("-", 1)[0]
                    held = [r for r, sec in w.setdefault("secrets", {}).items() if sec["name"] == name and sec["deleted"]]
                    if held:
                        raise AwsError("An error occurred (InvalidRequestException) when calling the CreateSecret operation: You can't create this secret because a secret "
                                       f"with this name is already scheduled for deletion ({name}); it can be created again once the recovery window has passed.", 254)
                    w["secrets"].setdefault(rid, {"name": name, "deleted": None})
                elif rtype == "AWS::DynamoDB::Table":
                    w.setdefault("tables", {})[rid] = True
                elif rtype == "AWS::Lambda::Function":
                    if rid not in w.setdefault("lambdas", []):
                        w["lambdas"].append(rid)
                elif rtype == "AWS::CodeBuild::Project":
                    if rid not in w.setdefault("projects", []):
                        w["projects"].append(rid)
                elif rtype == "AWS::Events::Rule":
                    if rid not in w.setdefault("rules", []):
                        w["rules"].append(rid)
                elif rtype == "AWS::StepFunctions::StateMachine":
                    w.setdefault("machines", {})[rid.rsplit(":", 1)[-1]] = rid
        w.setdefault("log_groups", [])
        names = {g["name"] for g in w["log_groups"]}
        for group, owner in ((names_of("/aws/lambda/hamh-direct-control-api"), names_of("hamh-direct-control")),
                             (names_of(f"/aws/bedrock-agentcore/runtimes/{RUNTIME_ID}-DEFAULT"), None),
                             (names_of("/aws/codebuild/hamh-direct-image-build"), None)):
            if group not in names and (owner is None or owner in w["stacks"]) and (not group.startswith("/aws/codebuild/") or names_of("hamh-direct-delivery") in w["stacks"]):
                w["log_groups"].append({"name": group, "stack": owner})
        if "network" in model:
            w.setdefault("ec2", {"vpcs": {}})["vpcs"].setdefault(names_of("vpc-0abc1234"), {"subnets": [names_of("subnet-0abc1234")], "sgs": [names_of("sg-0abc1234")],
                                                                                           "tags": {**tags, "aws:cloudformation:stack-name": names_of("hamh-direct-network"),
                                                                                                    "aws:cloudformation:stack-id": self.stack_id(names_of("hamh-direct-network"))}})
        for n in ("deployment-digest", "deployed-source-tree", "image-digest") if record else ():
            if names_of(f"/hamh/direct/{n}") not in w.setdefault("params", {}):
                w["params"][names_of(f"/hamh/direct/{n}")] = "0123456789ab"
                w.setdefault("param_tags", {})[names_of(f"/hamh/direct/{n}")] = dict(tags)
        # `logging_record`: the state a deploy in the mode enable-and-restore recorded before it turned Bedrock invocation logging on (a dict is written as JSON, a string as it is);
        # `logging_record_tagged: false` leaves the stage tags off the parameter; `bedrock_logging`: the account's setting in the home Region at the start (absent: unset).
        # (`legacy_logging_record` and `legacy_logging_record_tagged` are the same for the previous generation's parameter)
        key = "logging_record" if gen == "current" else "legacy_logging_record"
        record = self.scenario.get(key, "absent")
        record_name = names_of("/hamh/direct/invocation-logging-prior")
        if record != "absent" and record_name not in w.setdefault("params", {}):
            w["params"][record_name] = record if isinstance(record, str) else json.dumps(record)
            w.setdefault("param_tags", {})[record_name] = dict(tags) if self.scenario.get(key + "_tagged", True) else {}
        if "bedrock_logging" in self.scenario and "bedrock_logging" not in w:
            w["bedrock_logging"] = self.scenario["bedrock_logging"]
        if self.scenario.get("nine_stacks"):
            acct, region = self.account(), self.region()
            w.setdefault("machines", {names_of("hamh-direct-run"): f"arn:aws:states:{region}:{acct}:stateMachine:{names_of('hamh-direct-run')}",
                                      names_of("hamh-direct-run-ended"): f"arn:aws:states:{region}:{acct}:stateMachine:{names_of('hamh-direct-run-ended')}"})
            w.setdefault("rules", [names_of("hamh-direct-run-ended-rule")])

    def gomod_delete_patterns(self, gen: str = "current") -> list:
        """The aws:PrincipalArn patterns the Go module store's bucket policy lets delete: the stage operator (scenario `operator`, a kind and a name; the IAM user
        setup-test by default, which is who the stand-in's get-caller-identity answers) and the setup runner's role."""
        acct = self.account()
        kind, name = self.scenario.get("operator", ["user", "setup-test"])
        return [f"arn:aws:iam::{acct}:{kind}/{name}", f"arn:aws:iam::{acct}:{kind}/*/{name}", gen_text(f"arn:aws:iam::{acct}:role/hamh-setup-runner-role", gen)]

    def caller_may_delete_gomod(self, gen: str = "current") -> bool:
        """Whether the caller of the stand-in (scenario `arn`, else the IAM user setup-test) matches those patterns, as IAM compares them: a role session by its role."""
        import fnmatch
        acct = self.account()
        arn = self.scenario.get("arn", f"arn:aws:iam::{acct}:user/setup-test")
        m = re.fullmatch(r"arn:aws:sts::(\d+):assumed-role/([^/]+)/[^/]+", arn)
        principal = f"arn:aws:iam::{m.group(1)}:role/{m.group(2)}" if m else arn
        return any(fnmatch.fnmatchcase(principal, pat) for pat in self.gomod_delete_patterns(gen))

    def stage_blockers(self, name: str) -> list:
        """The resources of a stack that CloudFormation cannot delete now, as (logical id, type, physical id): a bucket that holds an object version, a repository that
        holds an image, a runtime that has an endpoint the stack did not make, and the subnet and security group that AgentCore's network interfaces hold."""
        w, out = self.w, []
        for r in w["stacks"][name].get("resources", []):
            rtype, rid = r["type"], r["id"]
            if r.get("status") == "DELETE_COMPLETE":
                continue  # a resource that was deleted earlier holds nothing up, and its name may belong to somebody else now
            if rtype == "AWS::S3::Bucket" and any(vs for vs in w["versions"].get(rid, {}).values()):
                out.append([r["logical"], rtype, rid])
            elif rtype == "AWS::ECR::Repository" and w.get("ecr", {}).get(rid):
                out.append([r["logical"], rtype, rid])
            elif rtype == "AWS::BedrockAgentCore::Runtime" and [e for e in w.get("runtimes", {}).get(rid.rsplit("/", 1)[-1], {}).get("endpoints", []) if e not in ("DEFAULT", "live")]:
                out.append([r["logical"], rtype, rid])
            elif rtype in ("AWS::EC2::Subnet", "AWS::EC2::SecurityGroup") and self.scenario.get("eni_stuck"):
                out.append([r["logical"], rtype, rid])
            elif rtype == "AWS::SecretsManager::ResourcePolicy" and self.secret_marked(rid):
                # live: "You can't perform this operation on the secret because it was marked for deletion" while the secret waits to be gone (about 30 s after a force delete)
                out.append([r["logical"], rtype, rid])
        return out

    def drop_stack_things(self, name: str, kept: list) -> None:
        """What a stack delete removes: every resource of the stack except the rows that failed (kept), and what each stood for."""
        w = self.w
        keep = {row[2] for row in kept}
        for r in w["stacks"][name].get("resources", []):
            rtype, rid = r["type"], r["id"]
            if rid in keep or r.get("status") == "DELETE_COMPLETE":
                continue
            if rtype == "AWS::S3::Bucket":
                w.get("buckets", {}).pop(rid, None)
                w.get("bucket_tags", {}).pop(rid, None)
                w["versions"].pop(rid, None)
            elif rtype == "AWS::ECR::Repository":
                w.get("ecr", {}).pop(rid, None)
            elif rtype == "AWS::BedrockAgentCore::Runtime":
                w.get("runtimes", {}).pop(rid.rsplit("/", 1)[-1], None)
            elif rtype == "AWS::KMS::Key" and rid in w.get("kms", {}):
                if not self.scenario.get("retain_keys"):
                    w["kms"][rid] = {"state": "PendingDeletion", "deletion": "2026-11-29T17:00:00+00:00"}
            elif rtype == "AWS::SecretsManager::Secret" and rid in w.get("secrets", {}):
                # the secret stays under its name for the 30 days of its recovery window unless it was deleted without one before
                if not (self.scenario.get("retain_secrets") and "research-tavily" not in rid):
                    w["secrets"][rid]["deleted"] = "2026-11-29T17:00:00+00:00"
            elif rtype == "AWS::DynamoDB::Table":
                w.get("tables", {}).pop(rid, None)
            elif rtype == "AWS::Lambda::Function" and rid in w.get("lambdas", []):
                w["lambdas"].remove(rid)
            elif rtype == "AWS::CodeBuild::Project" and rid in w.get("projects", []):
                w["projects"].remove(rid)
            elif rtype == "AWS::Events::Rule" and rid in w.get("rules", []):
                w["rules"].remove(rid)
            elif rtype == "AWS::StepFunctions::StateMachine":
                w.get("machines", {}).pop(rid.rsplit(":", 1)[-1], None)
            elif rtype in ("AWS::EC2::Subnet", "AWS::EC2::SecurityGroup"):
                for vpc in w.get("ec2", {}).get("vpcs", {}).values():
                    for kind in ("subnets", "sgs"):
                        if rid in vpc[kind]:
                            vpc[kind].remove(rid)
        w["log_groups"] = [g for g in w.get("log_groups", []) if g["stack"] != name]
        if self.stage_stack_gen(name) and name.endswith("-network") and not kept:
            w.get("ec2", {}).get("vpcs", {}).pop(gen_text("vpc-0abc1234", self.stage_stack_gen(name)), None)


def _build_cfg(world: World, action: str = "deploy") -> dict:
    """The scenario of a build: `build` for a deploy, `template_build` for the build that prints the bootstrap template."""
    cfg = {"polls_until_done": 2, "final": "SUCCEEDED", "result": "pass", "client": "ok", "admin": "ok", "failed_phase": "deploy",
           "detail": "DEPLOY-FAIL deploy cdk deploy failed", "phases": None}
    if action == "bootstrap-template":
        cfg.update({"failed_phase": "template", "detail": "the CDK could not print the bootstrap template"})
        cfg.update(world.scenario.get("template_build") or {})
    else:
        cfg.update(world.scenario.get("build") or {})
    return cfg


class Build:
    """One CodeBuild build and what the runner does in it."""

    def __init__(self, world: World, build_id: str):
        self.world, self.bid = world, build_id
        self.rec = world.w["builds"][build_id]

    @property
    def env(self) -> dict:
        return self.rec["env"]

    def advance(self) -> None:
        """One poll: the runner gets one step further."""
        w = self.world
        cfg = _build_cfg(w, self.action())
        rec = self.rec
        if rec["status"] != "IN_PROGRESS":
            return
        rec["polls"] += 1
        problem = self.source_problem()
        if problem:
            # CodeBuild's first step: a source it cannot find ends the build there, with no result of its own
            rec["phase"], rec["status"] = "DOWNLOAD_SOURCE", "FAILED"
            rec["log"].append(f"[Container] DOWNLOAD_SOURCE failed: {problem}")
            return
        phases = self.phase_names(cfg)
        shown = min(rec["polls"], len(phases))
        bucket = w.runner_names()["SetupBucket"]
        w.put(bucket, f"results/{self.bid}/phases.jsonl", self.phase_lines(phases[:shown]).encode())
        rec["log"].append(f"== phase {phases[shown - 1]} ok")
        if cfg.get("hang") or rec["polls"] < cfg["polls_until_done"]:
            rec["phase"] = "BUILD"
            return
        rec["phase"] = "COMPLETED"
        rec["status"] = cfg["final"]
        self.finish(cfg, bucket)

    def source_problem(self) -> str:
        """Why CodeBuild could not read the source, or "". Only a build started through start-build carries a source; the builds the tests seed do not."""
        src = self.rec.get("source")
        if src is None:
            return ""
        w = self.world
        bucket = w.runner_names()["SetupBucket"]
        key = self.env.get("SETUP_SOURCE_KEY", "")
        want = f"{bucket}/{key}"
        if src.get("location") != want:
            return f"the source location of the project is {src.get('location')!r}, the archive is at {want!r}; start-build must name it with --source-location-override"
        versions = [v["id"] for v in w.w["versions"].get(bucket, {}).get(key, []) if not v["marker"]]
        if not versions or w.get(bucket, key) is None:
            return f"s3://{want} does not exist"
        if src.get("version") and src["version"] not in versions:
            return f"s3://{want} has no version {src['version']}"
        return ""

    # -- what the runner writes, in the formats of scripts/setup/runner_state.py
    def action(self) -> str:
        return self.env.get("SETUP_ACTION", "deploy")

    def commit(self) -> str:
        return self.env["SETUP_SOURCE_KEY"].split("/")[-1].rsplit(".", 1)[0]

    def phase_names(self, cfg: dict) -> list:
        """The phases of this build in order; a failure ends the list at the failed phase (the deploy guard is a phase of its own after account-file)."""
        default = {"bootstrap-template": TEMPLATE_PHASES}.get(self.action(), DEPLOY_PHASES)
        names = list(cfg.get("phases") or default)
        if cfg["result"] == "fail" or self.rec["status"] == "FAILED" or cfg["final"] != "SUCCEEDED":
            failed = cfg["failed_phase"]
            names = names[: names.index(failed) + 1] if failed in names else [n for n in names if n in ("inputs", "fetch", "toolchain", "account-file")] + [failed]
        return names

    def phase_lines(self, names: list, failed: bool = False, detail: str = "") -> str:
        env = self.env
        digest12 = env["SETUP_CONFIG_KEY"].rsplit("-", 1)[-1].split(".")[0][:12]
        out = ""
        for i, name in enumerate(names):
            last_failed = failed and i == len(names) - 1
            out += runner_state_text("phase-line", "--build-id", self.bid, "--stage", env.get("SETUP_STAGE", "direct"), "--commit", self.commit(), "--digest", digest12,
                                     "--name", name, "--state", "failed" if last_failed else "ok", "--seconds", str(3 + i), "--detail", detail if last_failed else "")
        return out

    def result_text(self, cfg: dict, state: str, endpoint: str = "", outputs: dict | None = None, client: dict | None = None, failed: str = "", detail: str = "",
                    template: tuple | None = None) -> str:
        """result.json as the runner's own code writes it (the phases come from the phases.jsonl the build left)."""
        w, env = self.world, self.env
        bucket = w.runner_names()["SetupBucket"]
        work = w.dir / "runner-work"
        work.mkdir(exist_ok=True)
        phases_file = work / f"{self.bid.split(':')[-1]}.phases.jsonl"
        phases_file.write_bytes(w.get(bucket, f"results/{self.bid}/phases.jsonl") or b"")
        args = ["result", "--state", state, "--build-id", self.bid, "--stage", env.get("SETUP_STAGE", "direct"), "--commit", self.commit(),
                "--digest", env["SETUP_CONFIG_KEY"].rsplit("-", 1)[-1].split(".")[0][:12], "--region", w.region(), "--account", w.account(), "--endpoint", endpoint,
                "--deployment-digest", "" if self.action() == "bootstrap-template" else "0123456789ab", "--phases", str(phases_file), "--failed-phase", failed, "--detail", detail,
                "--action", self.action()]
        if outputs:
            out_file = work / f"{self.bid.split(':')[-1]}.outputs.json"
            out_file.write_text(json.dumps(outputs))
            args += ["--outputs", str(out_file)]
        if client:
            client_file = work / f"{self.bid.split(':')[-1]}.client.json"
            client_file.write_text(json.dumps(client))
            args += ["--client", str(client_file)]
        if template:
            args += ["--bootstrap-template", template[0], template[1], str(template[2])]
        return runner_state_text(*args)

    def finish(self, cfg: dict, bucket: str) -> None:
        w, env = self.world, self.env
        failed_run = self.rec["status"] != "SUCCEEDED" or cfg["result"] == "fail"
        if not failed_run:
            names = self.phase_names(cfg)
            w.put(bucket, f"results/{self.bid}/phases.jsonl", self.phase_lines(names).encode())
            self.rec["log"] += [line for line in (f"== phase {n} ok" for n in names) if line not in self.rec["log"]]
        if failed_run:
            phase, detail = cfg["failed_phase"], cfg["detail"]
            names = self.phase_names(cfg)
            w.put(bucket, f"results/{self.bid}/phases.jsonl", self.phase_lines(names, failed=True, detail=detail).encode())
            # `log_lines` is the whole tail of the build log, failure lines and CodeBuild's closing lines included (fixtures/setup/runner-failed-build.txt)
            if cfg.get("log_lines"):
                self.rec["log"] += list(cfg["log_lines"])
            else:
                self.rec["log"].append(f"SETUP-FAIL {phase} {detail}")
            for stack, status in (cfg.get("leaves") or {}).items():
                # what the failed deploy leaves behind in CloudFormation (a rolled-back update, a first create that rolled back)
                w.w["stacks"][stack] = {"status": status, "outputs": {}}
            if cfg["result"] == "none":
                return
            result = json.loads(self.result_text(cfg, "SETUP-FAIL", failed=phase, detail=detail))
        elif self.action() == "bootstrap-template":
            mode = w.scenario.get("bootstrap_template", "ok")
            body = BOOTSTRAP_TEMPLATE + (b"# another template\n" if mode == "other" else b"")
            sha = hashlib.sha256(body).hexdigest()
            key = f"bootstrap/cdk-bootstrap-{sha[:12]}.yaml"
            w.put(bucket, key, body + (b"# changed after the build\n" if mode == "tampered" else b""))
            size = 60000 if mode == "too_big" else len(body)
            result = json.loads(self.result_text(cfg, "SETUP-PASS", template=(key, sha, size), detail=f"CDK bootstrap template of {size} bytes, SHA-256 {sha[:12]}"))
            self.rec["log"].append(f"SETUP-PASS stage={env.get('SETUP_STAGE', 'direct')} action=bootstrap-template template={sha[:12]}")
        else:
            endpoint = w.scenario.get("endpoint", ENDPOINT)
            w.deploy_stage()
            client = {}
            sums = ""
            binary = env.get("SETUP_BINARY_NAME", "ham")
            admin_name = f"{binary}-admin"
            for plat in PLATFORMS:
                body = Path(w.scenario["client_file"]).read_bytes() if w.scenario.get("client_file") else CLIENT_BYTES
                sha = hashlib.sha256(body).hexdigest()
                key = f"client/{self.bid}/{plat}/{binary}"
                stored = body
                if cfg["client"] == "tamper":
                    stored = body + b"# tampered\n"
                if cfg["client"] != "missing":
                    w.put(bucket, key, stored)
                client[plat] = {"key": key, "sha256": sha, "bytes": len(body)}
                listed = "f" * 64 if cfg["client"] == "sums-mismatch" else sha
                sums += f"{listed}  {plat}/{binary}\n"
                if cfg["admin"] == "none":
                    continue  # the build lists no operator tool for the platform
                abody = Path(w.scenario["admin_file"]).read_bytes() if w.scenario.get("admin_file") else ADMIN_BYTES
                asha = hashlib.sha256(abody).hexdigest()
                akey = f"client/{self.bid}/{plat}/{admin_name}"
                if cfg["admin"] != "missing":
                    w.put(bucket, akey, abody + b"# tampered\n" if cfg["admin"] == "tamper" else abody)
                client[plat]["admin"] = {"key": akey, "sha256": asha, "bytes": len(abody)}
                sums += f"{'f' * 64 if cfg['admin'] == 'sums-mismatch' else asha}  {plat}/{admin_name}\n"
            w.put(bucket, f"client/{self.bid}/SHA256SUMS", sums.encode())
            result = json.loads(self.result_text(cfg, "SETUP-PASS", endpoint=endpoint, outputs={"hamh-direct-control.V1ApiUrl": endpoint}, client=client,
                                                 detail=f"{len(DEPLOY_PHASES)} phases"))
            self.rec["log"].append(f"SETUP-PASS stage={env.get('SETUP_STAGE', 'direct')} digest=0123456789ab endpoint={endpoint}")
        result.update(cfg.get("result_extra") or {})
        text = json.dumps(result, indent=2).encode()
        w.put(bucket, f"results/{self.bid}/result.json", text)
        if failed_run:
            return
        # The build's own record in CodeBuild: the runner exports the SHA-256 of the result.json it wrote (images/codebuild/setup-runner.yml), which no principal
        # that can write the bucket can change. "anchor": "none" leaves it out, "wrong" exports another value.
        anchor = cfg.get("anchor", "ok")
        if anchor != "none":
            self.rec["exported"] = {"SETUP_RESULT_SHA256": "e" * 64 if anchor == "wrong" else hashlib.sha256(text).hexdigest()}
        if cfg.get("forge"):
            self.forge(bucket, cfg["forge"], result)

    def forge(self, bucket: str, kind: str, result: dict) -> None:
        """What a principal of the account that can write the bucket does after the build: a coordinated rewrite of the client, its manifest and result.json
        (`client`), or a result.json that names another endpoint (`endpoint`). The build's exported record is left as the build made it."""
        w = self.world
        if kind == "client":
            forged = b"#!/bin/sh\necho forged client\n"
            sha = hashlib.sha256(forged).hexdigest()
            sums = ""
            for plat, info in (result.get("client") or {}).items():
                w.put(bucket, info["key"], forged)
                info["sha256"], info["bytes"] = sha, len(forged)
                sums += f"{sha}  {plat}/{self.env.get('SETUP_BINARY_NAME', 'ham')}\n"
                if info.get("admin"):
                    sums += f"{info['admin']['sha256']}  {plat}/{self.env.get('SETUP_BINARY_NAME', 'ham')}-admin\n"
            w.put(bucket, f"client/{self.bid}/SHA256SUMS", sums.encode())
            w.put(bucket, f"results/{self.bid}/result.json", json.dumps(result, indent=2).encode())
        elif kind == "endpoint":
            result["endpoint"] = "https://attacker.example/direct"
            w.put(bucket, f"results/{self.bid}/result.json", json.dumps(result, indent=2).encode())


# ---- the CLI ---------------------------------------------------------------------------------------------

class Cli:
    def __init__(self, argv: list):
        self.argv = list(argv)
        self.state = Path(os.environ["FAKE_AWS_STATE"])
        self.state.mkdir(parents=True, exist_ok=True)
        self.world = World(self.state, Path(os.environ.get("FAKE_AWS_SCENARIO", "")))
        self.query = None
        self.output = "json"
        self.region = None
        self.profile = None
        self.opts: dict = {}
        self.out = sys.stdout
        self.positional: list = []

    # -- argument parsing: global options first, then service, operation and the operation's own options
    VALUE = {"--query", "--output", "--region", "--profile", "--stack-name", "--template-file", "--capabilities", "--tags", "--name",
             "--bucket", "--key", "--body", "--delete", "--project-name", "--ids", "--environment-variables-override", "--log-group-name",
             "--log-stream-name", "--start-time", "--next-token", "--filters", "--stack-status-filter", "--retain-resources", "--if-none-match",
             "--max-items", "--sort-order", "--parameter-overrides", "--endpoint-url", "--version-id", "--cli-read-timeout", "--cli-connect-timeout",
             "--sse", "--expected-size", "--limit", "--query-compat", "--source-location-override", "--source-version", "--expected-bucket-owner", "--checksum-algorithm",
             "--repository-name", "--agent-runtime-id", "--endpoint-name", "--agent-runtime-arn", "--runtime-session-id", "--qualifier", "--state-machine-arn",
             "--status-filter", "--execution-arn", "--cause", "--table-name", "--filter-expression", "--expression-attribute-values", "--name-prefix",
             "--log-group-name-prefix", "--path", "--key-id", "--secret-id", "--export-name", "--subnet-id", "--group-id", "--vpc-id", "--network-interface-id",
             "--max-keys", "--page-size", "--starting-token", "--prefix", "--resource-arn", "--association-id", "--function-name", "--recovery-window-in-days",
             "--capacity-provider-id", "--session-id", "--event-bus-name", "--route-table-id", "--internet-gateway-id", "--function-version", "--role-arn",
             "--logging-config"}
    FLAGS = {"--no-fail-on-empty-changeset", "--no-cli-pager", "--start-from-head", "--no-paginate", "--no-sign-request", "--recursive", "--quiet", "--only-show-errors",
             "--with-decryption", "--no-verify-ssl", "--debug", "--no-start-from-head", "--bypass-governance-retention", "--include-planned-deletion", "--generate-cli-skeleton", "--force-delete-without-recovery",
             "--include-managed-resources", "--no-include-managed-resources"}
    MULTI = {"--capabilities", "--tags", "--environment-variables-override", "--retain-resources", "--stack-status-filter", "--filters", "--parameter-overrides", "--ids",
             "--image-ids", "--names", "--tag-filters", "--parameter-filters", "--vpc-ids", "--vpc-endpoint-ids", "--route-table-ids"}

    def parse(self) -> None:
        a = self.argv
        i = 0
        rest = []
        while i < len(a):
            tok = a[i]
            if not tok.startswith("--"):
                rest.append(tok)
                i += 1
                continue
            i += 1
            if tok in self.FLAGS:
                self.opts[tok] = True
            elif tok in self.MULTI:
                vals = []
                while i < len(a) and not a[i].startswith("--"):
                    vals.append(a[i])
                    i += 1
                if not vals:
                    raise Refuse(f"{tok} needs a value")
                self.opts[tok] = vals
            elif tok in self.VALUE:
                if i >= len(a):
                    raise Refuse(f"{tok} needs a value")
                self.opts[tok] = a[i]
                i += 1
            else:
                raise Refuse(f"unknown option {tok}")
        self.query = self.opts.get("--query")
        self.output = self.opts.get("--output", "json")
        self.region = self.opts.get("--region")
        self.profile = self.opts.get("--profile")
        self.positional = rest

    def log_call(self) -> None:
        with open(self.state / "calls.jsonl", "a") as fh:
            fh.write(json.dumps({"args": self.argv, "env": {k: os.environ[k] for k in ("AWS_PROFILE", "AWS_PAGER") if k in os.environ}}) + "\n")

    def emit(self, data) -> None:
        if self.query:
            before = data
            data = jmespath(self.query, data)
            if self.world.scenario.get("query_log"):
                # `query_log`: a file that records every query with the document it ran on and the stand-in's result, so that a test can check them against the real JMESPath
                with open(self.world.scenario["query_log"], "a") as fh:
                    fh.write(json.dumps({"query": self.query, "document": before, "result": data}) + "\n")
        if self.output == "text":
            self.out.write(as_text(data))
        else:
            self.out.write(json.dumps(data, indent=4) + "\n")

    # -- faults named in the scenario: {"match": "<words the call starts with>", "exit": 254, "stderr": "...", "times": 1, "after": 0}
    def check_faults(self) -> None:
        for n, f in enumerate(self.world.scenario.get("faults") or []):
            if f["match"] in " ".join(self.argv):
                seen = self.world.w["faults_seen"].get(str(n), 0)
                self.world.w["faults_seen"][str(n)] = seen + 1
                if seen >= f.get("after", 0) and seen < f.get("after", 0) + f.get("times", 10 ** 6):
                    raise AwsError(f.get("stderr", "An error occurred (InternalFailure): fake fault"), f.get("exit", 254))

    def check_events(self) -> None:
        """Scenario `on_call`: [{"match": "<words of a call>", "params": {name: tags}, "log_groups": [name], "bucket": name}]: once, when a call that contains the words is
        made, these things appear in the account (what another person creates while a run goes on)."""
        for n, ev in enumerate(self.world.scenario.get("on_call") or []):
            key = f"event-{n}"
            if ev["match"] in " ".join(self.argv) and not self.world.w["faults_seen"].get(key):
                self.world.w["faults_seen"][key] = 1
                for name, tags in (ev.get("params") or {}).items():
                    self.world.w.setdefault("params", {})[name] = "x"
                    self.world.w.setdefault("param_tags", {})[name] = tags
                for name in ev.get("log_groups") or []:
                    self.world.w.setdefault("log_groups", []).append({"name": name, "stack": None})
                if ev.get("bucket"):
                    self.world.seed_entries(ev["bucket"], 1, 0)
                    self.world.w.setdefault("bucket_tags", {})[ev["bucket"]] = dict(GEN_TAGS[ev.get("gen", "current")])
                if ev.get("stack"):
                    self.world.w["stacks"][ev["stack"]] = {"status": "DELETE_FAILED", "outputs": {}, "exports": {}, "imports": [], "tags": dict(GEN_TAGS[ev.get("gen", "current")]),
                                                           "resources": [{"logical": "Subnet", "type": "AWS::EC2::Subnet", "id": "subnet-appeared"}],
                                                           "failed": [["Subnet", "AWS::EC2::Subnet", "subnet-appeared"]]}

    def run(self) -> int:
        self.log_call()
        try:
            if self.argv[:1] == ["--version"]:
                line = os.environ.get("FAKE_AWS_VERSION", "aws-cli/2.37.2 Python/3.13.9 Linux/6.1.0 exe/x86_64")
                print(line)
                return 0
            self.parse()
            if len(self.positional) < 2:
                raise Refuse("expected a service and an operation")
            self.check_faults()
            self.check_events()
            self.world.tick()
            service, op = self.positional[0], self.positional[1]
            handler = getattr(self, f"{service.replace('-', '_')}__{op.replace('-', '_')}", None)
            if self.world.scenario.get("cli_without_agentcore") and service in ("bedrock-agentcore", "bedrock-agentcore-control"):
                # an AWS CLI that predates the AgentCore commands
                raise AwsError("aws: error: argument command: Invalid choice, valid choices are:\n\nbedrock\nbedrock-runtime", 252)
            if self.opts.get("--generate-cli-skeleton"):
                # the CLI prints the skeleton of an operation it knows without calling anything
                if f"{service} {op}" in (self.world.scenario.get("cli_lacks") or []):
                    raise AwsError(f"aws: error: argument operation: Invalid choice, valid choices are:\n\n{op}-x", 252)
                if handler is None:
                    raise Refuse(f"call {service} {op}")
                print("{}")
                self.world.save()
                return 0
            if handler is None:
                raise Refuse(f"call {service} {op}")
            handler(*self.positional[2:])
            self.world.save()
            return 0
        except AwsError as exc:
            self.world.save()
            sys.stderr.write(str(exc) + "\n")
            return exc.code
        except Refuse as exc:
            sys.stderr.write(f"fake aws: not supported: {exc}\n")
            return 252

    # -- sts
    def sts__get_caller_identity(self) -> None:
        sc = self.world.scenario
        if sc.get("identity_error"):
            raise AwsError(sc["identity_error"], 255)
        acct = self.world.account()
        arn = sc.get("arn", f"arn:aws:iam::{acct}:user/setup-test")
        # `arn_later`: the credentials that every call after the first one resolves to (a step that runs as another identity than the plan's)
        n = self.world.w.get("sts_calls", 0) + 1
        self.world.w["sts_calls"] = n
        if n > 1 and sc.get("arn_later"):
            arn = sc["arn_later"]
        self.emit(fx("sts-get-caller-identity.json", account=acct, arn=arn))

    # -- ec2
    def ec2__describe_availability_zones(self) -> None:
        region = self.region or self.world.region()
        if self.world.scenario.get("azs") is not None:
            rows = self.world.scenario["azs"]
            self.emit({"AvailabilityZones": [{"State": "available", "RegionName": region, "ZoneName": n, "ZoneId": i, "ZoneType": "availability-zone"} for n, i in rows]})
            return
        path = FIXTURES / f"ec2-describe-availability-zones-{region}.json"
        if not path.is_file():
            raise Refuse(f"no fixture for Region {region}")
        self.emit(json.loads(path.read_text()))

    # -- ssm
    def ssm__get_parameter(self) -> None:
        name = self.opts.get("--name")
        if name != "/cdk-bootstrap/hnb659fds/version":
            value = self.world.w.get("params", {}).get(name)
            if value is None:
                raise AwsError("An error occurred (ParameterNotFound) when calling the GetParameter operation: ", 254)
            self.emit({"Parameter": {"Name": name, "Type": "String", "Value": value, "Version": 1}})
            return
        if not self.world.w["bootstrap"]:
            raise AwsError("An error occurred (ParameterNotFound) when calling the GetParameter operation: ", 254)
        self.emit(fx("ssm-get-parameter.json", version="21"))

    def ssm__get_parameters_by_path(self) -> None:
        path = self.opts.get("--path", "")
        if not self.opts.get("--recursive"):
            raise Refuse("get-parameters-by-path without --recursive")
        if not path.startswith("/") or path.endswith("/"):
            raise AwsError("An error occurred (ValidationException) when calling the GetParametersByPath operation: the path starts with a slash and does not end with one", 254)
        self.emit({"Parameters": [{"Name": n, "Type": "String", "Value": v, "Version": 1} for n, v in sorted(self.world.w.get("params", {}).items()) if n.startswith(path + "/")]})

    def ssm__describe_parameters(self) -> None:
        """DescribeParameters with ParameterFilters: Key=Path (Option=Recursive) and Key=tag:<name> are the two kinds the wizard sends."""
        path, tags = None, {}
        for item in self.opts.get("--parameter-filters") or []:
            parts = dict(p.split("=", 1) for p in item.split(","))
            key = parts.get("Key", "")
            if key == "Path":
                if parts.get("Option") != "Recursive" or not parts.get("Values", "").startswith("/") or parts["Values"].endswith("/"):
                    raise AwsError("An error occurred (ValidationException) when calling the DescribeParameters operation: the Path filter needs Option=Recursive and a path that starts with a slash and does not end with one", 254)
                path = parts["Values"]
            elif key.startswith("tag:"):
                tags[key[4:]] = parts.get("Values", "")
            else:
                raise Refuse(f"parameter filter {item!r}")
        ptags = self.world.w.get("param_tags", {})
        rows = [{"Name": n, "Type": "String", "Version": 1} for n in sorted(self.world.w.get("params", {}))
                if (path is None or n.startswith(path + "/")) and all((ptags.get(n) or {}).get(k) == v for k, v in tags.items())]
        self.emit({"Parameters": rows})

    def ssm__delete_parameters(self) -> None:
        names = self.opts.get("--names") or []
        if not names or len(names) > 10:
            raise AwsError("An error occurred (ValidationException) when calling the DeleteParameters operation: 1 to 10 names", 254)
        params = self.world.w.setdefault("params", {})
        gone = [n for n in names if params.pop(n, None) is not None]
        for n in gone:
            self.world.w.get("param_tags", {}).pop(n, None)
        self.emit({"DeletedParameters": gone, "InvalidParameters": [n for n in names if n not in gone]})

    # -- bedrock: the account's model invocation logging setting of the Region (one document, absent while unset)
    def bedrock__get_model_invocation_logging_configuration(self) -> None:
        cfg = self.world.w.get("bedrock_logging")
        self.emit({"loggingConfig": cfg} if cfg else {})

    def bedrock__put_model_invocation_logging_configuration(self) -> None:
        try:
            cfg = json.loads(self.opts.get("--logging-config") or "")
        except ValueError:
            raise AwsError("Error parsing parameter '--logging-config': Invalid JSON", 252) from None
        if not isinstance(cfg, dict) or not (cfg.get("cloudWatchConfig") or cfg.get("s3Config")):
            raise AwsError("An error occurred (ValidationException) when calling the PutModelInvocationLoggingConfiguration operation: the configuration names no destination", 254)
        self.world.w["bedrock_logging"] = cfg
        self.emit({})

    def bedrock__delete_model_invocation_logging_configuration(self) -> None:
        self.world.w["bedrock_logging"] = None
        self.emit({})

    # -- cloudformation
    def stack(self, name: str) -> dict:
        st = self.world.w["stacks"].get(name)
        if st is not None and st["status"] == "DELETE_COMPLETE_RETAINED":
            st = None  # a deleted stack is not found by its name
        if st is None:
            raise AwsError(f"An error occurred (ValidationError) when calling the DescribeStacks operation: Stack with id {name} does not exist", 254)
        return st

    def cloudformation__describe_stacks(self) -> None:
        name = self.opts.get("--stack-name")
        st = self.stack(name)
        left = st.get("polls_left")
        if left is not None:
            # a stack that is still changing (seeded as {"status": "CREATE_IN_PROGRESS", "polls_left": 2, "then": "CREATE_COMPLETE"}) settles after that many looks
            if left <= 0:
                st["status"] = st.pop("then", "CREATE_COMPLETE")
                st.pop("polls_left")
            else:
                st["polls_left"] = left - 1
        outputs = [{"OutputKey": k, "OutputValue": v, **({"ExportName": st["exports"][k]} if k in st.get("exports", {}) else {})} for k, v in st.get("outputs", {}).items()]
        doc = fx("cloudformation-describe-stacks.json", name=name, status=st["status"], outputs=outputs, region=self.world.region(), account=self.world.account())
        if st.get("params"):
            doc["Stacks"][0]["Parameters"] = [{"ParameterKey": k, "ParameterValue": v} for k, v in st["params"].items()]
        doc["Stacks"][0]["Tags"] = [{"Key": k, "Value": v} for k, v in (st.get("tags") or {}).items()]
        if st.get("uuid"):
            doc["Stacks"][0]["StackId"] = self.world.stack_id(name, st["uuid"])
        self.emit(doc)

    def cloudformation__describe_stack_events(self) -> None:
        """The events of a stack, newest first: what a DELETE_FAILED stack failed to delete and why (the reason CloudFormation gives)."""
        name = self.opts.get("--stack-name")
        st = self.stack(name)
        if st.get("events"):
            self.emit({"StackEvents": [dict(e, EventId=f"e{i}", Timestamp="2026-10-30T18:00:00+00:00") for i, e in enumerate(st["events"])]})
            return
        sid = self.world.stack_id(name, st.get("uuid") or STACK_UUID)
        events = []
        failed = st.get("failed", [])
        if failed:
            events.append({"StackId": sid, "EventId": "e3", "StackName": name, "LogicalResourceId": name, "PhysicalResourceId": sid, "ResourceType": "AWS::CloudFormation::Stack",
                           "Timestamp": "2026-10-30T18:00:03+00:00", "ResourceStatus": "DELETE_FAILED",
                           "ResourceStatusReason": "The following resource(s) failed to delete: [" + ", ".join(row[0] for row in failed) + "]. "})
        for logical, rtype, rid in failed:
            events.append({"StackId": sid, "EventId": "e2-" + logical, "StackName": name, "LogicalResourceId": logical, "PhysicalResourceId": rid, "ResourceType": rtype,
                           "Timestamp": "2026-10-30T18:00:02+00:00", "ResourceStatus": "DELETE_FAILED", "ResourceStatusReason": st.get("fail_reason") or "fake delete failure"})
        events.append({"StackId": sid, "EventId": "e1", "StackName": name, "LogicalResourceId": name, "PhysicalResourceId": sid, "ResourceType": "AWS::CloudFormation::Stack",
                       "Timestamp": "2026-10-30T18:00:01+00:00", "ResourceStatus": "DELETE_IN_PROGRESS", "ResourceStatusReason": "User Initiated"})
        self.emit({"StackEvents": events})

    def cloudformation__deploy(self) -> None:
        name = self.opts.get("--stack-name")
        tpl = self.opts.get("--template-file")
        if not tpl or not Path(tpl).is_file():
            raise AwsError(f"[Errno 2] No such file or directory: '{tpl}'", 255)
        if "CAPABILITY_NAMED_IAM" not in (self.opts.get("--capabilities") or []):
            raise AwsError("An error occurred (InsufficientCapabilitiesException): Requires capabilities : [CAPABILITY_NAMED_IAM]", 254)
        existing = self.world.w["stacks"].get(name)
        if existing and (existing["status"] in ("ROLLBACK_COMPLETE", "ROLLBACK_FAILED", "CREATE_FAILED", "DELETE_FAILED") or existing["status"].endswith("_IN_PROGRESS")):
            # CloudFormation changes a stack only from a settled state
            raise AwsError("An error occurred (ValidationError) when calling the CreateChangeSet operation: Stack:arn:aws:cloudformation:"
                           f"{self.world.region()}:{self.world.account()}:stack/{name}/aaaaaaaa-0000-4000-8000-a00000000000 is in {existing['status']} state and can not be updated.", 254)
        params = {}
        for item in self.opts.get("--parameter-overrides") or []:
            key, _, value = item.partition("=")
            params[key] = value
        if name == "CDKToolkit":
            if not any(o.startswith("CloudFormationExecutionPolicies=") for o in self.opts.get("--parameter-overrides") or []):
                raise AwsError("An error occurred (ValidationError) when calling the CreateChangeSet operation: the CloudFormation execution role of the bootstrap would have no policy", 254)
            self.world.w["bootstrap_template_sha"] = hashlib.sha256(Path(tpl).read_bytes()).hexdigest()
            self.world.w["stacks"][name] = {"status": "CREATE_COMPLETE", "outputs": {}}
            self.world.w["bootstrap"] = True
        elif name == "hamh-setup-runner":
            self.world.make_runner()
            self.world.w["stacks"][name]["params"] = params
        elif name == "hamh-direct-control":  # the CDK deploy that a stand-in scripts/deploy.sh stands for
            self.world.deploy_stage()
        else:
            raise Refuse(f"deploy of stack {name}")
        print(f"Successfully created/updated stack - {name}")

    def cloudformation__delete_stack(self) -> None:
        name = self.opts.get("--stack-name")
        sc = self.world.scenario
        st = self.world.w["stacks"].get(name)
        if st is None:
            return
        retain = self.opts.get("--retain-resources")
        runner_gen = self.world.runner_stack_gen(name)
        if runner_gen:
            bucket = self.world.runner_names(runner_gen)["SetupBucket"]
            role_name = gen_text("hamh-setup-runner-role", runner_gen)
            key_id = gen_text(RUNNER_KEY_ID, runner_gen)
            if sc.get("runner_delete_fails"):
                raise AwsError("An error occurred (ValidationError) when calling the DeleteStack operation: fake delete failure", 254)
            left = [k for k, vs in self.world.w["versions"].get(bucket, {}).items() if vs]
            if left:
                st["status"] = "DELETE_FAILED"
                return
            # `runner_role_denied`: "caller" = the session of the caller may not delete the role that has been used (live: an explicit deny in a session policy) and the CDK
            # CloudFormation execution role may; "always" = the execution role cannot be used either (it does not exist, or CloudFormation cannot assume it)
            denied = sc.get("runner_role_denied")
            role = self.opts.get("--role-arn")
            if sc.get("runner_fails_otherwise") and not role:
                # a delete that fails for a reason that is no denial of the caller (a dependency of the stack)
                st["status"] = "DELETE_FAILED"
                st["failed"] = [["SetupBucketPolicy", "AWS::S3::BucketPolicy", gen_text("hamh-setup-runner-policy", runner_gen)]]
                st["fail_reason"] = "The bucket policy is in use by another resource (HandlerErrorCode: ResourceConflict)"
                st["resources"] = [{"logical": "SetupBucketPolicy", "type": "AWS::S3::BucketPolicy", "id": gen_text("hamh-setup-runner-policy", runner_gen)}]
                self.world.record_failure(name, st["failed"], st["fail_reason"])
                return
            exec_role = exec_role_arn(self.world.account(), self.world.region())
            if denied == "always" and role:
                raise AwsError(f"An error occurred (ValidationError) when calling the DeleteStack operation: Role {role} is invalid or cannot be assumed", 254)
            if denied == "bucket_policy" and not role:
                # a refusal that is no IAM action of the caller: the policy of a bucket denies the delete
                st["status"] = "DELETE_FAILED"
                st["failed"] = [["SetupBucket", "AWS::S3::Bucket", bucket]]
                st["fail_reason"] = "Resource handler returned message: \"Access Denied (Service: S3, Status Code: 403)\" (HandlerErrorCode: AccessDenied)"
                st["resources"] = [{"logical": "SetupBucket", "type": "AWS::S3::Bucket", "id": bucket}]
                self.world.record_failure(name, st["failed"], st["fail_reason"])
                return
            if denied and role != exec_role:
                caller = sc.get("arn", f"arn:aws:iam::{self.world.account()}:user/setup-test")
                if denied == "other_principal":
                    # the refused principal is another role, one that CloudFormation used for an earlier operation: changing the role of this delete would not be the remedy
                    caller = f"arn:aws:sts::{self.world.account()}:assumed-role/OldCfnRole/session-1"
                st["status"] = "DELETE_FAILED"
                st["failed"] = [["RunnerRole", "AWS::IAM::Role", role_name]]
                filler = "Resource handler returned message: \"" + "x" * 500 + " " if denied == "long_caller" else ""
                st["fail_reason"] = (f"{filler}User: {caller} is not authorized to perform: iam:DeleteRole on resource: role {role_name} with an explicit deny in a session policy "
                                     "(Service: Iam, Status Code: 403)")
                st["resources"] = [{"logical": "RunnerRole", "type": "AWS::IAM::Role", "id": role_name}]
                self.world.record_failure(name, st["failed"], st["fail_reason"])
                self.world.w.get("buckets", {}).pop(bucket, None)  # the bucket went before the role
                self.world.w["versions"].pop(bucket, None)
                self.world.w.get("kms", {})[key_id] = {"state": "PendingDeletion", "deletion": "2026-11-06T17:00:00+00:00"}
                return
            del self.world.w["stacks"][name]
            self.world.w.get("buckets", {}).pop(bucket, None)
            self.world.w.get("kms", {})[key_id] = {"state": "PendingDeletion", "deletion": "2026-11-06T17:00:00+00:00"}
            return
        if st["status"] == "DELETE_FAILED" and retain:
            failed = {row[0] for row in st.get("failed", [])}
            known = {r["logical"] for r in st.get("resources", [])} | failed
            if not set(retain) <= known:
                raise AwsError("An error occurred (ValidationError) when calling the DeleteStack operation: Specified resource(s) to retain are not in the stack: " + ", ".join(sorted(set(retain) - known)), 254)
            if sc.get("retain_only_failed") and not set(retain) <= failed:
                # a CloudFormation that accepts only the resources that failed (the stand-in's other model, for the program's fallback)
                raise AwsError("An error occurred (ValidationError) when calling the DeleteStack operation: retain list names a resource that did not fail", 254)
            rows = [[r["logical"], r["type"], r["id"]] for r in st.get("resources", []) if r["logical"] in retain]
            st["retained"] = rows
            self.world.drop_stack_things(name, rows)
            vpcs = [r for r in st.get("resources", []) if r["type"] == "AWS::EC2::VPC" and r["logical"] not in retain]
            if vpcs and any(row[1] in ("AWS::EC2::Subnet", "AWS::EC2::SecurityGroup") for row in rows):
                # the VPC is deleted next and cannot go while the subnet is there (live: DependencyViolation after about 16 minutes)
                st["status"] = "DELETE_FAILED"
                st["failed"] = [[r["logical"], r["type"], r["id"]] for r in vpcs]
                st["fail_reason"] = "The vpc has dependencies and cannot be deleted. (HandlerErrorCode: GeneralServiceException)"
                st["resources"] = vpcs
                return
            st["status"] = "DELETE_COMPLETE_RETAINED"
            st.pop("failed", None)
            return
        if st["status"].endswith("_IN_PROGRESS") and st["status"] != "DELETE_IN_PROGRESS":
            raise AwsError(f"An error occurred (ValidationError) when calling the DeleteStack operation: Stack [{name}] cannot be deleted while in status {st['status']}", 254)
        for exported in st.get("exports", {}).values():
            users = sorted(other for other, o in self.world.w["stacks"].items() if other != name and exported in o.get("imports", []) and o["status"] != "DELETE_COMPLETE_RETAINED")
            if users:
                raise AwsError(f"An error occurred (ValidationError) when calling the DeleteStack operation: Export {exported} cannot be deleted as it is in use by {users[0]}", 254)
        if name in (sc.get("fail_once") or []) and name not in self.world.w.setdefault("failed_once", []):
            # an asynchronous dependency that is gone a moment later (a capacity provider while runtime versions detach): the first delete ends DELETE_FAILED, the next works
            self.world.w["failed_once"].append(name)
            first = st["resources"][0]
            st["status"], st["failed"] = "DELETE_FAILED", [[first["logical"], first["type"], first["id"]]]
            return
        if st["status"] == "DELETE_FAILED" and not retain:
            st.pop("failed", None)
        blockers = self.world.stage_blockers(name) if self.world.stage_stack_gen(name) else []
        if blockers:
            st["status"], st["failed"] = "DELETE_FAILED", blockers
            st["fail_reason"] = {"AWS::SecretsManager::ResourcePolicy": "You can't perform this operation on the secret because it was marked for deletion. (HandlerErrorCode: InvalidRequest)",
                                 "AWS::EC2::Subnet": "The subnet has dependencies and cannot be deleted. (HandlerErrorCode: GeneralServiceException)",
                                 "AWS::EC2::SecurityGroup": "resource has a dependent object (HandlerErrorCode: GeneralServiceException)"}.get(blockers[0][1], "fake delete failure")
            self.world.record_failure(name, blockers, st["fail_reason"])
            self.world.drop_stack_things(name, blockers)
            keep_ids = {b[2] for b in blockers}
            if any(b[1] in ("AWS::EC2::Subnet", "AWS::EC2::SecurityGroup") for b in blockers):
                # the VPC cannot go while its subnet stays: CloudFormation leaves it undeleted (CREATE_COMPLETE), not failed
                keep_ids |= {r["id"] for r in st.get("resources", []) if r["type"] == "AWS::EC2::VPC"}
            st["resources"] = [r for r in st.get("resources", []) if r["id"] in keep_ids]
            return
        if self.world.stage_stack_gen(name):
            self.world.drop_stack_things(name, [])
        del self.world.w["stacks"][name]

    def cloudformation__wait(self, which: str = "") -> None:
        if which != "stack-delete-complete":
            raise Refuse(f"wait {which}")
        name = self.opts.get("--stack-name")
        slow = self.world.scenario.get("wait_seconds") or {}  # {stack name: seconds the waiter takes}: the program prints a line while it waits
        if slow.get(name):
            import time
            time.sleep(float(slow[name]))
        st = self.world.w["stacks"].get(name)
        if st is not None and st["status"] != "DELETE_COMPLETE_RETAINED":
            raise AwsError("Waiter StackDeleteComplete failed: Waiter encountered a terminal failure state: For expression \"Stacks[].StackStatus\" "
                           f"we matched expected path: \"DELETE_FAILED\" at least once", 255)

    def cloudformation__list_stacks(self) -> None:
        summaries = [{"StackName": k, "StackStatus": v["status"]} for k, v in sorted(self.world.w["stacks"].items())
                     if v["status"] != "DELETE_COMPLETE_RETAINED"]
        self.emit(fx("cloudformation-list-stacks.json", summaries=summaries))

    def cloudformation__list_stack_resources(self) -> None:
        st = self.stack(self.opts.get("--stack-name"))
        failed = {row[2] for row in st.get("failed", [])}
        rows = [{"LogicalResourceId": r["logical"], "ResourceType": r["type"], "PhysicalResourceId": r["id"],
                 "ResourceStatus": "DELETE_FAILED" if r["id"] in failed else r.get("status", "CREATE_COMPLETE")} for r in st.get("resources", [])]
        known = {r["id"] for r in st.get("resources", [])}
        rows += [{"LogicalResourceId": a, "ResourceType": b, "PhysicalResourceId": c, "ResourceStatus": "DELETE_FAILED"} for a, b, c in st.get("failed", []) if c not in known]
        self.emit(fx("cloudformation-list-stack-resources.json", rows=rows))

    def cloudformation__list_imports(self) -> None:
        export = self.opts.get("--export-name")
        users = sorted(name for name, o in self.world.w["stacks"].items() if export in o.get("imports", []) and o["status"] != "DELETE_COMPLETE_RETAINED")
        if not users:
            raise AwsError(f"An error occurred (ValidationError) when calling the ListImports operation: Export '{export}' is not imported by any stack.", 254)
        self.emit({"Imports": users})

    # -- s3
    @staticmethod
    def split_uri(uri: str):
        m = re.fullmatch(r"s3://([^/]+)/(.+)", uri)
        if not m:
            raise Refuse(f"s3 uri {uri}")
        return m.group(1), m.group(2)

    def s3__cp(self, src: str = "", dst: str = "") -> None:
        if not src or not dst:
            raise Refuse("s3 cp needs two arguments")
        if dst.startswith("s3://") and not src.startswith("s3://"):
            bucket, key = self.split_uri(dst)
            if bucket not in self.world.w.get("buckets", {}) and not self.world.bucket_exists(bucket):
                raise AwsError(f"upload failed: An error occurred (NoSuchBucket) when calling the PutObject operation: The specified bucket does not exist", 1)
            body = sys.stdin.buffer.read() if src == "-" else Path(src).read_bytes()
            self.world.put(bucket, key, body)
            return
        if src.startswith("s3://") and not dst.startswith("s3://"):
            bucket, key = self.split_uri(src)
            body = self.world.get(bucket, key)
            if body is None:
                raise AwsError(f'fatal error: An error occurred (404) when calling the HeadObject operation: Key "{key}" does not exist', 1)
            if dst == "-":
                sys.stdout.buffer.write(body)
                sys.stdout.buffer.flush()
            else:
                Path(dst).parent.mkdir(parents=True, exist_ok=True)
                Path(dst).write_bytes(body)
            return
        raise Refuse("s3 cp between these two places")

    def s3api__delete_object(self) -> None:
        bucket, key = self.opts.get("--bucket"), self.opts.get("--key")
        if self.world.get(bucket, key) is not None:
            self.world.mark_deleted(bucket, key)

    def s3api__head_bucket(self) -> None:
        bucket = self.opts.get("--bucket")
        if not self.world.bucket_exists(bucket):
            raise AwsError("An error occurred (404) when calling the HeadBucket operation: Not Found", 254)

    def s3api__head_object(self) -> None:
        bucket, key = self.opts.get("--bucket"), self.opts.get("--key")
        body = self.world.get(bucket, key)
        if body is None:
            raise AwsError("An error occurred (404) when calling the HeadObject operation: Not Found", 254)
        version = self.world.w["versions"][bucket][key][-1]["id"]
        self.emit(fx("s3api-head-object.json", length=len(body), etag=hashlib.sha256(body).hexdigest()[:32], version=version))

    def s3api__put_object(self) -> None:
        bucket, key = self.opts.get("--bucket"), self.opts.get("--key")
        if not self.world.bucket_exists(bucket):
            raise AwsError("An error occurred (NoSuchBucket) when calling the PutObject operation: The specified bucket does not exist", 254)
        if self.opts.get("--if-none-match") == "*" and self.world.get(bucket, key) is not None:
            raise AwsError("An error occurred (PreconditionFailed) when calling the PutObject operation: At least one of the pre-conditions you specified did not hold", 254)
        body = Path(self.opts["--body"]).read_bytes() if "--body" in self.opts else b""
        vid = self.world.put(bucket, key, body)
        doc = fx("s3api-put-object.json", etag="d41d8cd98f00b204e9800998ecf8427e", version=vid)
        if self.world.scenario.get("no_version_ids"):
            del doc["VersionId"]  # a bucket whose versioning was suspended answers with none
        self.emit(doc)

    def s3api__list_object_versions(self) -> None:
        bucket = self.opts.get("--bucket")
        if self.world.scenario.get("probe_file"):
            # where the client is when the runner starts to be taken away (the first call of scripts/setup/empty-bucket.sh)
            self.world.w.setdefault("probe", Path(self.world.scenario["probe_file"]).exists())
        if not self.world.bucket_exists(bucket):
            raise AwsError("An error occurred (NoSuchBucket) when calling the ListObjectVersions operation: The specified bucket does not exist", 254)
        entries = s3model.entries_from_versions(self.world.w["versions"].get(bucket, {}))

        def number(name):
            return int(self.opts[name]) if name in self.opts else None
        try:
            data = s3model.cli_list(entries, bucket=bucket, no_paginate=bool(self.opts.get("--no-paginate")), max_keys=number("--max-keys"), page_size=number("--page-size"),
                                    max_items=number("--max-items"), starting_token=self.opts.get("--starting-token"))
        except s3model.UsageError as exc:
            raise AwsError("\n" + str(exc) + "\n", 252)
        except s3model.ServiceError as exc:
            raise AwsError(exc.text(), exc.exit_code)
        self.emit(data)

    def s3api__delete_objects(self) -> None:
        bucket = self.opts.get("--bucket")
        spec = self.opts.get("--delete", "")
        quiet = False
        if spec.startswith("file://"):
            payload = json.loads(Path(spec[len("file://"):]).read_text())
            pairs = [(o["Key"], o["VersionId"]) for o in payload["Objects"]]
            quiet = bool(payload.get("Quiet"))
        else:
            pairs = re.findall(r"\{Key=([^,}]+),VersionId=([^,}]+)\}", spec)
        if not pairs:
            raise Refuse(f"delete-objects spec {spec!r}")
        if self.world.scenario.get("empty_bucket_fails"):
            raise AwsError("An error occurred (AccessDenied) when calling the DeleteObjects operation: Access Denied", 254)
        vs = self.world.w["versions"].get(bucket, {})
        gomod_gen = self.world.gomod_bucket_gen(bucket)
        gomod = gomod_gen is not None
        # the bucket policy of the Go module store lets only the operator (and the runner's role) delete: another identity is refused per version
        denied = gomod and not self.world.caller_may_delete_gomod(gomod_gen)

        def locked(key, vid):
            return gomod and not any(v["marker"] for v in vs.get(key, []) if v["id"] == vid)

        def remove(key, vid):
            vs[key] = [v for v in vs.get(key, []) if v["id"] != vid]
            if not vs[key]:
                del vs[key]
                p = self.world.obj_dir(bucket) / key
                if p.is_file():
                    p.unlink()
        objects = [{"Key": k, "VersionId": v} for k, v in pairs]
        self.world.w.setdefault("delete_sizes", []).append(len(objects))
        try:
            if denied:
                s3model.delete_objects(objects, quiet=quiet)  # only to apply the limit of 1,000 keys; nothing is removed
                doc = {"Errors": [{"Key": k, "VersionId": v, "Code": "AccessDenied", "Message": "User is not authorized: explicit deny in a resource-based policy"} for k, v in pairs]}
            else:
                doc = s3model.delete_objects(objects, quiet=quiet, bypass=bool(self.opts.get("--bypass-governance-retention")), protected=locked, remove=remove)
        except s3model.ServiceError as exc:
            raise AwsError(exc.text(), exc.exit_code)
        self.emit(doc)

    def s3api__get_bucket_tagging(self) -> None:
        bucket = self.opts.get("--bucket")
        if not self.world.bucket_exists(bucket):
            raise AwsError("An error occurred (NoSuchBucket) when calling the GetBucketTagging operation: The specified bucket does not exist", 254)
        tags = self.world.w.get("bucket_tags", {}).get(bucket)
        if not tags:
            raise AwsError("An error occurred (NoSuchTagSet) when calling the GetBucketTagging operation: The TagSet does not exist", 254)
        self.emit({"TagSet": [{"Key": k, "Value": v} for k, v in tags.items()]})

    def s3api__delete_bucket(self) -> None:
        bucket = self.opts.get("--bucket")
        if not self.world.bucket_exists(bucket):
            raise AwsError("An error occurred (NoSuchBucket) when calling the DeleteBucket operation: The specified bucket does not exist", 254)
        if any(vs for vs in self.world.w["versions"].get(bucket, {}).values()):
            raise AwsError("An error occurred (BucketNotEmpty) when calling the DeleteBucket operation: The bucket you tried to delete is not empty. You must delete all versions in the bucket.", 254)
        self.world.w.get("buckets", {}).pop(bucket, None)
        self.world.w.get("bucket_tags", {}).pop(bucket, None)
        self.world.w["versions"].pop(bucket, None)

    def s3api__get_bucket_policy(self) -> None:
        bucket = self.opts.get("--bucket")
        if self.world.gomod_bucket_gen(bucket) is None or not self.world.bucket_exists(bucket):
            raise AwsError("An error occurred (NoSuchBucketPolicy) when calling the GetBucketPolicy operation: The bucket policy does not exist", 254)
        patterns = self.world.gomod_delete_patterns(self.world.gomod_bucket_gen(bucket))
        policy = {"Version": "2012-10-17", "Statement": [{"Sid": "DenyDeletesFromOtherPrincipals", "Effect": "Deny", "Principal": "*",
                                                          "Action": ["s3:DeleteObject", "s3:DeleteObjectVersion", "s3:BypassGovernanceRetention"],
                                                          "Resource": f"arn:aws:s3:::{bucket}/*", "Condition": {"ArnNotLike": {"aws:PrincipalArn": patterns}}}]}
        self.emit({"Policy": json.dumps(policy)})

    # -- the services a teardown touches outside CloudFormation (scripts/setup/teardown.sh)
    def ecr__list_images(self) -> None:
        repo = self.opts.get("--repository-name")
        images = self.world.w.get("ecr", {}).get(repo)
        if images is None:
            raise AwsError("An error occurred (RepositoryNotFoundException) when calling the ListImages operation: The repository with name '" + str(repo) + "' does not exist", 254)
        ids = []
        for img in images:
            for tag in img["tags"] or [None]:
                ids.append({"imageDigest": img["digest"], **({"imageTag": tag} if tag else {})})
        self.emit({"imageIds": ids[: int(self.opts.get("--max-items", 1000))]})

    def ecr__batch_delete_image(self) -> None:
        repo = self.opts.get("--repository-name")
        images = self.world.w.get("ecr", {}).get(repo)
        if images is None:
            raise AwsError("An error occurred (RepositoryNotFoundException) when calling the BatchDeleteImage operation: The repository does not exist", 254)
        wanted = []
        for item in self.opts.get("--image-ids") or []:
            m = re.fullmatch(r"imageDigest=(sha256:[0-9a-f]{64})", item)
            if not m:
                raise Refuse(f"batch-delete-image id {item!r}")
            wanted.append(m.group(1))
        if not wanted or len(wanted) > 100:
            raise AwsError("An error occurred (InvalidParameterException) when calling the BatchDeleteImage operation: 1 to 100 image ids", 254)
        failures = []
        if self.world.scenario.get("ecr_delete_fails"):
            failures = [{"imageId": {"imageDigest": d}, "failureCode": "ImageReferencedByManifestList", "failureReason": "Requested image referenced by manifest list"} for d in wanted]
        else:
            # a child image of a manifest list cannot be deleted while the list is there (the list may be in the same batch: it goes first, its children in the next call)
            present = {i["digest"] for i in images}
            refused = [d for d in wanted if next((i.get("child_of") for i in images if i["digest"] == d), None) in present]
            failures = [{"imageId": {"imageDigest": d}, "failureCode": "ImageReferencedByManifestList", "failureReason": "Requested image referenced by manifest list"} for d in refused]
            self.world.w["ecr"][repo] = [i for i in images if i["digest"] not in wanted or i["digest"] in refused]
        self.emit({"imageIds": [{"imageDigest": d} for d in wanted if d not in {f["imageId"]["imageDigest"] for f in failures}], "failures": failures})

    def bedrock_agentcore_control__list_agent_runtimes(self) -> None:
        rts = self.world.w.get("runtimes", {})
        self.emit({"agentRuntimes": [{"agentRuntimeId": rid, "agentRuntimeArn": r.get("arn", f"arn:aws:bedrock-agentcore:{self.world.region()}:{self.world.account()}:runtime/{rid}"),
                                      "agentRuntimeName": r["name"], "status": "READY"} for rid, r in sorted(rts.items())]})

    def bedrock_agentcore_control__list_tags_for_resource(self) -> None:
        arn = self.opts.get("--resource-arn", "")
        for r in self.world.w.get("runtimes", {}).values():
            if r.get("arn") == arn:
                self.emit({"tags": dict(r.get("tags") or {})})
                return
        raise AwsError("An error occurred (ResourceNotFoundException) when calling the ListTagsForResource operation: resource not found", 254)

    def bedrock_agentcore__delete_capacity_provider_session(self) -> None:
        if not self.opts.get("--capacity-provider-id") or not self.opts.get("--session-id"):
            raise AwsError("An error occurred (ValidationException) when calling the DeleteCapacityProviderSession operation: id and session id are required", 254)
        self.emit({})

    def bedrock_agentcore_control__list_agent_runtime_endpoints(self) -> None:
        rid = self.opts.get("--agent-runtime-id")
        rt = self.world.w.get("runtimes", {}).get(rid)
        if rt is None:
            raise AwsError("An error occurred (ResourceNotFoundException) when calling the ListAgentRuntimeEndpoints operation: runtime not found", 254)
        # scenario `endpoint_deleting_looks`: a deleted endpoint is still listed, as DELETING, for that many listings (AgentCore deletes an endpoint asynchronously; live it took over 6 minutes)
        deleting = rt.setdefault("deleting", {})
        rows = []
        for n in list(rt["endpoints"]):
            if n in deleting and deleting[n] <= 0:
                rt["endpoints"].remove(n)
                del deleting[n]
            elif n in deleting:
                deleting[n] -= 1
                rows.append({"name": n, "status": "DELETING"})
            else:
                rows.append({"name": n, "status": "READY"})
        self.emit({"runtimeEndpoints": rows})

    def bedrock_agentcore_control__delete_agent_runtime_endpoint(self) -> None:
        rid, name = self.opts.get("--agent-runtime-id"), self.opts.get("--endpoint-name")
        rt = self.world.w.get("runtimes", {}).get(rid)
        if rt is None or name not in rt["endpoints"]:
            raise AwsError("An error occurred (ResourceNotFoundException) when calling the DeleteAgentRuntimeEndpoint operation: endpoint not found", 254)
        if self.world.scenario.get("endpoint_delete_fails"):
            raise AwsError("An error occurred (AccessDeniedException) when calling the DeleteAgentRuntimeEndpoint operation: not authorized", 254)
        looks = int(self.world.scenario.get("endpoint_deleting_looks", 0))
        if looks > 0:
            rt.setdefault("deleting", {})[name] = looks
        else:
            rt["endpoints"].remove(name)
        self.emit({"status": "DELETING", "agentRuntimeEndpointArn": f"arn:aws:bedrock-agentcore:{self.world.region()}:{self.world.account()}:runtime/{rid}/runtime-endpoint/{name}"})

    def bedrock_agentcore__stop_runtime_session(self) -> None:
        arn, sid = self.opts.get("--agent-runtime-arn"), self.opts.get("--runtime-session-id")
        if not arn or not sid or not self.opts.get("--qualifier"):
            raise AwsError("An error occurred (ValidationException) when calling the StopRuntimeSession operation: arn, session id and qualifier are required", 254)
        if self.world.scenario.get("stopped_sessions_missing"):
            raise AwsError("An error occurred (ResourceNotFoundException) when calling the StopRuntimeSession operation: session not found", 254)
        self.emit({})

    def dynamodb__scan(self) -> None:
        table = self.opts.get("--table-name")
        if table not in (gen_text("hamh-direct-harness", "current"), gen_text("hamh-direct-harness", "legacy")):
            raise Refuse("scan of another table")
        if not self.world.w.get("tables", {}).get(table):
            raise AwsError("An error occurred (ResourceNotFoundException) when calling the Scan operation: Requested resource not found", 254)
        items = []
        for sess in self.world.scenario.get("sessions") or []:
            items.append({"SK": {"S": "AGENT#" + sess["agent"]}, "runId": {"S": sess["run"]}, "runtimeArn": {"S": sess["arn"]}, "runtimeSessionId": {"S": sess["session"]},
                          "status": {"S": sess.get("status", "running")}, **({"runtimeEndpoint": {"S": sess["endpoint"]}} if sess.get("endpoint") else {})})
        self.emit({"Items": items, "Count": len(items)})

    def stepfunctions__list_state_machines(self) -> None:
        self.emit({"stateMachines": [{"name": n, "stateMachineArn": a} for n, a in sorted(self.world.w.get("machines", {}).items())]})

    def stepfunctions__list_executions(self) -> None:
        arn = self.opts.get("--state-machine-arn", "")
        name = arn.rsplit(":", 1)[-1]
        running = (self.world.scenario.get("executions") or {}).get(name, [])
        stopped = self.world.w.setdefault("stopped_executions", [])
        self.emit({"executions": [{"name": e, "executionArn": f"arn:aws:states:{self.world.region()}:{self.world.account()}:execution:{name}:{e}", "status": "RUNNING"}
                                  for e in running if e not in stopped]})

    def stepfunctions__stop_execution(self) -> None:
        self.world.w.setdefault("stopped_executions", []).append(self.opts.get("--execution-arn", "").rsplit(":", 1)[-1])
        self.emit({"stopDate": "2026-10-30T00:00:00+00:00"})

    def events__list_rules(self) -> None:
        prefix = self.opts.get("--name-prefix", "")
        self.emit({"Rules": [{"Name": n, "State": "DISABLED" if n in self.world.w.setdefault("disabled_rules", []) else "ENABLED"} for n in self.world.w.get("rules", [])
                             if n.startswith(prefix)]})

    def events__disable_rule(self) -> None:
        self.world.w.setdefault("disabled_rules", []).append(self.opts.get("--name"))

    def lambda__list_functions(self) -> None:
        # a function writes to /aws/lambda/<its name> unless its LoggingConfig names another group (`lambda_logs` in the world: function name to group); with
        # --function-version ALL the answer also holds one entry for each published version (`lambda_versions`: function name to the groups of its versions)
        custom = self.world.w.get("lambda_logs", {})
        rows = [dict({"FunctionName": n}, **({"LoggingConfig": {"LogGroup": custom[n]}} if n in custom else {})) for n in sorted(self.world.w.get("lambdas", []))]
        if self.opts.get("--function-version") == "ALL":
            for n, groups in sorted(self.world.w.get("lambda_versions", {}).items()):
                rows += [{"FunctionName": n, "Version": str(i + 1), "LoggingConfig": {"LogGroup": g}} for i, g in enumerate(groups)]
        self.emit({"Functions": rows})

    def codebuild__list_projects(self) -> None:
        self.emit({"projects": sorted(self.world.w.get("projects", []))})

    def codebuild__batch_get_projects(self) -> None:
        # a project writes to /aws/codebuild/<its name> unless its logs configuration names another group (`project_logs` in the world: project name to group)
        names = self.opts.get("--names") or []
        if not names or len(names) > 100:
            raise AwsError("An error occurred (InvalidInputException) when calling the BatchGetProjects operation: 1 to 100 names", 254)
        known = self.world.w.get("projects", [])
        custom = self.world.w.get("project_logs", {})
        rows = []
        for n in names:
            if n in known:
                cw = {"status": "ENABLED"}
                if n in custom:
                    cw["groupName"] = custom[n]
                rows.append({"name": n, "logsConfig": {"cloudWatchLogs": cw}})
        self.emit({"projects": rows, "projectsNotFound": [n for n in names if n not in known]})

    def logs__describe_log_groups(self) -> None:
        prefix = self.opts.get("--log-group-name-prefix", "")
        self.emit({"logGroups": [{"logGroupName": g["name"]} for g in self.world.w.get("log_groups", []) if g["name"].startswith(prefix)]})

    def logs__delete_log_group(self) -> None:
        name = self.opts.get("--log-group-name")
        groups = self.world.w.get("log_groups", [])
        if not any(g["name"] == name for g in groups):
            raise AwsError("An error occurred (ResourceNotFoundException) when calling the DeleteLogGroup operation: The specified log group does not exist.", 254)
        self.world.w["log_groups"] = [g for g in groups if g["name"] != name]

    def kms__describe_key(self) -> None:
        key = self.world.w.get("kms", {}).get(self.opts.get("--key-id"))
        if key is None:
            raise AwsError("An error occurred (NotFoundException) when calling the DescribeKey operation: Key not found", 254)
        meta = {"KeyId": self.opts.get("--key-id"), "KeyState": key["state"], "KeyManager": "CUSTOMER"}
        if key["deletion"]:
            meta["DeletionDate"] = key["deletion"].replace("+00:00", ".865000+00:00")  # the AWS CLI prints microseconds: 2026-11-30T03:14:47.865000+00:00
        self.emit({"KeyMetadata": meta})

    def secretsmanager__delete_secret(self) -> None:
        """DeleteSecret: with --force-delete-without-recovery the secret and its name are gone at once; without it the secret waits out a recovery window (30 days by default) and
        its name cannot be used for a new secret until the window has passed."""
        key = self.opts.get("--secret-id")
        force = bool(self.opts.get("--force-delete-without-recovery"))
        if force and self.opts.get("--recovery-window-in-days"):
            raise AwsError("An error occurred (InvalidParameterException) when calling the DeleteSecret operation: You can't use ForceDeleteWithoutRecovery in conjunction with RecoveryWindowInDays.", 254)
        secrets = self.world.w.get("secrets", {})
        found = key if key in secrets else next((r for r, sec in secrets.items() if sec["name"] == key), None)
        if found is None:
            raise AwsError("An error occurred (ResourceNotFoundException) when calling the DeleteSecret operation: Secrets Manager can't find the specified secret.", 254)
        name = secrets[found]["name"]
        if force and self.world.scenario.get("secret_force_ignored"):
            secrets[found]["deleted"] = None  # the call is accepted and the secret is not marked (it was restored in the meantime)
            secrets[found].pop("vanish", None)
        elif force:
            # live: accepted also for a secret that is scheduled already (DeletedDate = now), and the secret is NotFound about 30 seconds later: `vanish` counts the calls until then
            secrets[found]["deleted"] = "2026-10-30T00:00:00+00:00"
            secrets[found]["vanish"] = int(self.world.scenario.get("secret_vanish_calls", 40))
        else:
            secrets[found]["deleted"] = "2026-11-29T17:00:00+00:00"
        self.emit({"ARN": found, "Name": name, "DeletionDate": "2026-10-30T00:00:00+00:00"})

    def secretsmanager__describe_secret(self) -> None:
        sec = self.world.w.get("secrets", {}).get(self.opts.get("--secret-id"))
        if sec is None:
            raise AwsError("An error occurred (ResourceNotFoundException) when calling the DescribeSecret operation: Secrets Manager can't find the specified secret.", 254)
        doc = {"ARN": self.opts.get("--secret-id"), "Name": sec["name"]}
        if sec["deleted"]:
            doc["DeletedDate"] = sec["deleted"].replace("+00:00", ".865000+00:00")
        self.emit(doc)

    def resourcegroupstaggingapi__get_resources(self) -> None:
        # the index answers for the project tag value of the call: an entry of the scenario `tag_index` is an ARN of the current generation, or {"arn": ..., "gen": "legacy"}
        project = None
        for item in self.opts.get("--tag-filters") or []:
            parts = dict(p.split("=", 1) for p in item.split(","))
            if parts.get("Key") == "project":
                project = parts.get("Values")
        entries = [(a, "current") if isinstance(a, str) else (a["arn"], a.get("gen", "current")) for a in self.world.scenario.get("tag_index") or []]
        for kid, key in self.world.w.get("kms", {}).items():
            if key["state"] == "PendingDeletion":
                entries.append((f"arn:aws:kms:{self.world.region()}:{self.world.account()}:key/{kid}", "legacy" if "leg" in kid else "current"))
        arns = [a for a, g in entries if project is None or GEN_TAGS[g]["project"] == project]
        self.emit({"ResourceTagMappingList": [{"ResourceARN": a, "Tags": []} for a in arns]})

    # -- ec2: the VPC of the stage
    def tag_filters(self) -> dict:
        out = {}
        for item in self.opts.get("--filters") or []:
            m = re.fullmatch(r"Name=([^,]+),Values=(.+)", item)
            if not m:
                raise Refuse(f"filter {item!r}")
            out[m.group(1)] = m.group(2).split(",")
        return out

    def stage_vpcs(self) -> dict:
        return self.world.w.get("ec2", {}).get("vpcs", {})

    @staticmethod
    def tag_match(vpc: dict, rid: str, f: dict) -> bool:
        """Whether a resource of the VPC passes the tag filters of the call. A resource named in vpc["untagged"] carries no tags (the default security group and the main route
        table never do)."""
        wants = {k[4:]: v for k, v in f.items() if k.startswith("tag:")}
        if not wants:
            return True
        if rid in vpc.get("untagged", []) or rid == "sg-default0":
            return False
        return all(vpc["tags"].get(k) in vals for k, vals in wants.items())

    def ec2__describe_vpcs(self) -> None:
        f = self.tag_filters()
        ids = self.opts.get("--vpc-ids")
        rows = [{"VpcId": vid, "Tags": [{"Key": k, "Value": v} for k, v in vpc["tags"].items()]} for vid, vpc in sorted(self.stage_vpcs().items())
                if all(vpc["tags"].get(name.split(":", 1)[1]) in vals for name, vals in f.items() if name.startswith("tag:"))
                and ("vpc-id" not in f or vid in f["vpc-id"]) and (not ids or vid in ids)]
        if ids and not rows:
            raise AwsError("An error occurred (InvalidVpcID.NotFound) when calling the DescribeVpcs operation: The vpc ID '" + ids[0] + "' does not exist", 254)
        self.emit({"Vpcs": rows})

    def ec2__describe_subnets(self) -> None:
        f = self.tag_filters()
        want = f.get("vpc-id", [])
        self.emit({"Subnets": [{"SubnetId": sid, "VpcId": vid} for vid, v in sorted(self.stage_vpcs().items()) if vid in want for sid in v["subnets"] if self.tag_match(v, sid, f)]})

    def ec2__describe_security_groups(self) -> None:
        f = self.tag_filters()
        want = f.get("vpc-id", [])
        rows = [{"GroupId": gid, "GroupName": "worker", "VpcId": vid} for vid, v in sorted(self.stage_vpcs().items()) if vid in want for gid in v["sgs"] if self.tag_match(v, gid, f)]
        rows += [{"GroupId": "sg-default0", "GroupName": "default", "VpcId": vid} for vid, v in sorted(self.stage_vpcs().items()) if vid in want and self.tag_match(v, "sg-default0", f)]
        self.emit({"SecurityGroups": rows})

    def ec2__describe_network_interfaces(self) -> None:
        sc = self.world.scenario
        flag = bool(self.opts.get("--include-managed-resources"))
        if flag and sc.get("old_cli_no_managed"):
            raise AwsError("Unknown options: --include-managed-resources", 252)
        want = self.tag_filters().get("vpc-id", [])
        rows = []
        for vid, v in sorted(self.stage_vpcs().items()):
            if vid not in want:
                continue
            if sc.get("eni_stuck") and (v["subnets"] or v["sgs"]):
                rows.append({"NetworkInterfaceId": "eni-0agentcore1", "VpcId": vid, "Status": "in-use", "RequesterManaged": True, "InterfaceType": "agentic_ai"})
            # an interface of the list may name its type ("nat_gateway", "vpc_endpoint": the interfaces the network stack makes itself, which are requester-managed)
            rows += [{"NetworkInterfaceId": e["id"], "VpcId": vid, "Status": e.get("status", "available"), "RequesterManaged": bool(e.get("managed")),
                      **({"InterfaceType": e["type"]} if e.get("type") else {})} for e in v.get("enis", [])]
        if sc.get("hide_managed_enis") and not flag:
            # a managed resource is left out of the answer unless the call asks for it
            rows = [r for r in rows if not r["RequesterManaged"]]
        self.emit({"NetworkInterfaces": rows})

    def ec2__describe_route_tables(self) -> None:
        f = self.tag_filters()
        ids = self.opts.get("--route-table-ids")
        want = f.get("vpc-id", [])
        rows = []
        for vid, v in sorted(self.stage_vpcs().items()):
            for t in v.get("route_tables", []):
                if (ids and t["id"] in ids) or (not ids and vid in want and self.tag_match(v, t["id"], f)):
                    rows.append({"RouteTableId": t["id"], "VpcId": vid, "Associations": [{"RouteTableAssociationId": a["id"], "Main": bool(a.get("main")),
                                                                                         **({"GatewayId": a["gateway"]} if a.get("gateway") else {"SubnetId": a.get("subnet", "subnet-x")})}
                                                                                        for a in t.get("assoc", [])]})
        self.emit({"RouteTables": rows})

    def ec2__disassociate_route_table(self) -> None:
        aid = self.opts.get("--association-id")
        for v in self.stage_vpcs().values():
            for t in v.get("route_tables", []):
                for a in t.get("assoc", []):
                    if a["id"] == aid:
                        t["assoc"].remove(a)
                        return
        raise AwsError("An error occurred (InvalidAssociationID.NotFound) when calling the DisassociateRouteTable operation: The association ID does not exist", 254)

    def ec2__delete_route_table(self) -> None:
        rid = self.opts.get("--route-table-id")
        for v in self.stage_vpcs().values():
            for t in v.get("route_tables", []):
                if t["id"] == rid:
                    if t.get("assoc"):
                        raise AwsError(f"An error occurred (DependencyViolation) when calling the DeleteRouteTable operation: The routeTable '{rid}' has dependencies and cannot be deleted.", 254)
                    v["route_tables"].remove(t)
                    return
        raise AwsError("An error occurred (InvalidRouteTableID.NotFound) when calling the DeleteRouteTable operation: The routeTable ID does not exist", 254)

    def ec2__describe_internet_gateways(self) -> None:
        f = self.tag_filters()
        want = f.get("attachment.vpc-id", [])
        self.emit({"InternetGateways": [{"InternetGatewayId": g, "Attachments": [{"VpcId": vid, "State": "available"}]} for vid, v in sorted(self.stage_vpcs().items()) if vid in want
                                        for g in v.get("igws", []) if self.tag_match(v, g, f)]})

    def ec2__detach_internet_gateway(self) -> None:
        gid, vid = self.opts.get("--internet-gateway-id"), self.opts.get("--vpc-id")
        v = self.stage_vpcs().get(vid)
        if v is None or gid not in v.get("igws", []):
            raise AwsError("An error occurred (Gateway.NotAttached) when calling the DetachInternetGateway operation: resource is not attached", 254)
        v.setdefault("detached", []).append(gid)

    def ec2__delete_internet_gateway(self) -> None:
        gid = self.opts.get("--internet-gateway-id")
        for v in self.stage_vpcs().values():
            if gid in v.get("igws", []):
                v["igws"].remove(gid)
                return
        raise AwsError("An error occurred (InvalidInternetGatewayID.NotFound) when calling the DeleteInternetGateway operation: The internetGateway ID does not exist", 254)

    def ec2__describe_vpc_endpoints(self) -> None:
        f = self.tag_filters()
        want = f.get("vpc-id", [])
        self.emit({"VpcEndpoints": [{"VpcEndpointId": e, "VpcId": vid, "State": "available"} for vid, v in sorted(self.stage_vpcs().items()) if vid in want
                                    for e in v.get("endpoints", []) if self.tag_match(v, e, f)]})

    def ec2__delete_vpc_endpoints(self) -> None:
        ids = self.opts.get("--vpc-endpoint-ids") or []
        if self.world.scenario.get("endpoint_unsuccessful"):
            self.emit({"Unsuccessful": [{"ResourceId": i, "Error": {"Code": "InvalidParameter", "Message": "the endpoint cannot be deleted now"}} for i in ids]})
            return
        for v in self.stage_vpcs().values():
            v["endpoints"] = [e for e in v.get("endpoints", []) if e not in ids]
        self.emit({"Unsuccessful": []})

    def ec2__delete_subnet(self) -> None:
        sid = self.opts.get("--subnet-id")
        for v in self.stage_vpcs().values():
            if sid in v["subnets"]:
                if self.world.scenario.get("eni_stuck"):
                    raise AwsError(f"An error occurred (DependencyViolation) when calling the DeleteSubnet operation: The subnet '{sid}' has dependencies and cannot be deleted.", 254)
                v["subnets"].remove(sid)
                for t in v.get("route_tables", []):
                    t["assoc"] = [a for a in t.get("assoc", []) if a.get("subnet") != sid]
                return
        raise AwsError("An error occurred (InvalidSubnetID.NotFound) when calling the DeleteSubnet operation: The subnet ID does not exist", 254)

    def ec2__delete_security_group(self) -> None:
        gid = self.opts.get("--group-id")
        for v in self.stage_vpcs().values():
            if gid in v["sgs"]:
                if self.world.scenario.get("eni_stuck"):
                    raise AwsError(f"An error occurred (DependencyViolation) when calling the DeleteSecurityGroup operation: resource {gid} has a dependent object", 254)
                v["sgs"].remove(gid)
                return
        raise AwsError("An error occurred (InvalidGroup.NotFound) when calling the DeleteSecurityGroup operation: The security group does not exist", 254)

    def ec2__delete_vpc(self) -> None:
        vid = self.opts.get("--vpc-id")
        vpc = self.stage_vpcs().get(vid)
        if vpc is None:
            raise AwsError("An error occurred (InvalidVpcID.NotFound) when calling the DeleteVpc operation: The vpc ID does not exist", 254)
        if vpc["subnets"] or vpc["sgs"] or vpc.get("route_tables") or vpc.get("igws") or vpc.get("endpoints"):
            raise AwsError(f"An error occurred (DependencyViolation) when calling the DeleteVpc operation: The vpc '{vid}' has dependencies and cannot be deleted.", 254)
        del self.stage_vpcs()[vid]

    def ec2__delete_network_interface(self) -> None:
        raise AwsError("An error occurred (InvalidParameterValue) when calling the DeleteNetworkInterface operation: Network interface is currently in use and is not supported", 254)

    # -- codebuild
    def codebuild__start_build(self) -> None:
        w = self.world
        env = {}
        for item in self.opts.get("--environment-variables-override") or []:
            parts = dict(p.split("=", 1) for p in item.split(","))
            env[parts["name"]] = parts["value"]
        for need in ("SETUP_SOURCE_KEY", "SETUP_CONFIG_KEY", "SETUP_STAGE", "SETUP_AZS", "SETUP_BINARY_NAME"):
            if need not in env:
                raise AwsError(f"An error occurred (InvalidInputException) when calling the StartBuild operation: missing {need}", 254)
        if self.opts.get("--project-name") != w.runner_names()["BuildProject"]:
            raise AwsError("An error occurred (ResourceNotFoundException) when calling the StartBuild operation: Project cannot be found", 254)
        w.w["n_builds"] += 1
        bid = f"{w.runner_names()['BuildProject']}:00000000-0000-4000-8000-a{w.w['n_builds']:011d}"
        w.w["builds"][bid] = {"status": "IN_PROGRESS", "phase": "QUEUED", "polls": 0, "env": env, "log": [],
                              "source": {"location": self.opts.get("--source-location-override"), "version": self.opts.get("--source-version")}}
        names = w.runner_names()
        self.emit(fx("codebuild-start-build.json", id=bid, number=w.w["n_builds"], project=names["BuildProject"], region=w.region(), account=w.account(),
                     bucket=names["SetupBucket"], source=env["SETUP_SOURCE_KEY"], group=names["LogGroup"], stream=bid.split(":", 1)[1],
                     environment=[{"name": k, "value": v, "type": "PLAINTEXT"} for k, v in env.items()]))

    def codebuild__batch_get_builds(self) -> None:
        w = self.world
        if w.scenario.get("connection_lost"):
            raise AwsError('Could not connect to the endpoint URL: "https://codebuild.us-east-1.amazonaws.com/"', 255)
        ids = self.opts.get("--ids") or []
        builds = []
        for bid in ids:
            if bid not in w.w["builds"]:
                continue
            b = Build(w, bid)
            b.advance()
            rec = w.w["builds"][bid]
            uuid_part = bid.split(":", 1)[1]
            doc = fx("codebuild-build.json", id=bid, number=1, status=rec["status"], phase=rec["phase"], project=w.runner_names()["BuildProject"],
                     region=w.region(), account=w.account(), group=w.runner_names()["LogGroup"], stream=uuid_part,
                     environment=[{"name": k, "value": v, "type": "PLAINTEXT"} for k, v in rec["env"].items()])
            if rec.get("exported"):
                doc["exportedEnvironmentVariables"] = [{"name": k, "value": v} for k, v in rec["exported"].items()]
            builds.append(doc)
        self.emit(fx("codebuild-batch-get-builds.json", builds=builds, missing=[i for i in ids if i not in w.w["builds"]]))

    def codebuild__list_builds_for_project(self) -> None:
        self.emit({"ids": sorted(self.world.w["builds"], reverse=True)})

    # -- logs
    def logs__get_log_events(self) -> None:
        stream = self.opts.get("--log-stream-name", "")
        start = int(self.opts.get("--start-time", 0) or 0)
        events = []
        for bid, rec in self.world.w["builds"].items():
            if bid.split(":", 1)[1] == stream:
                for n, line in enumerate(rec["log"]):
                    ts = 1700000000000 + n * 1000
                    if ts >= start:
                        events.append({"timestamp": ts, "message": line + "\n", "ingestionTime": ts})
        self.emit(fx("logs-get-log-events.json", events=events))


def main(argv: list) -> int:
    return Cli(argv).run()


LAUNCHER = """#!{python} -I
import sys
sys.dont_write_bytecode = True
sys.path.insert(0, {here!r})
import setup_fake_aws
sys.exit(setup_fake_aws.main(sys.argv[1:]))
"""


def install(directory: Path, name: str = "aws") -> Path:
    """Write the launcher that makes `aws` the stand-in, with this interpreter and this module."""
    path = Path(directory) / name
    path.write_text(LAUNCHER.format(python=sys.executable, here=str(Path(__file__).resolve().parent)))
    path.chmod(0o755)
    return path


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
