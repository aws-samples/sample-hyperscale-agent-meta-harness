"""Lane A: `./setup.sh --update` keeps the settings of the stage. The deploy side: scripts/lib/deploy.py and scripts/lib/stage-settings.tsv.

What these cases hold:
- The registry (scripts/lib/stage-settings.tsv) lists every stage setting once: NAME|KEPT|CHANGE|PATTERN|NOTE. Every PATTERN gives the same
  answer in Python's re.fullmatch and in bash's `[[ $value =~ $pattern ]]`, accepts what the CDK app accepts (the models below take their
  ranges and words from infra/lib/config.ts, infra/lib/stacks/network.ts and the utility-model catalog), and repeats nothing more than
  255 times, the most that bash 3.2 on macOS compiles.
- The stage record, SSM /hamh/<stage>/settings: a deploy writes it through its tagged put, `status=deploying` before the first stack
  change and `status=deployed` at the end, with the commit, the source tree, VERSION, the account file digest, the operator and the zones,
  and one `set.` line for each kept setting a user chose. A code default is never written, but for the keep tag and the stage kind: the
  role of the account entry gives their default, the next deploy may come with an entry of another role, so the record always holds both.
- Resolution: an explicit value, else the record's `set.` line, else (only on a stage without a record) the live value, else the code
  default. A live value equal to today's default counts as the default. One DEPLOY-SETTING line per setting before any change, and every
  cdk call gets the resolved values.
- A refusal before the first stack change prints one line, `DEPLOY-REFUSED reason=<id> wait=<yes|no> stage=unchanged :: <action>`, and
  exits 3. A failure after the first stack change keeps DEPLOY-FAIL and exit 1.
- The deploy lock, SSM /hamh/<stage>/deploy-lock: with HAMH_DEPLOY_LOCK_TOKEN the deploy runs only under a lock with that token; without
  it the deploy takes the lock itself (create-only, tagged, route=manual), writes a new start time into it at least every ten minutes
  while it runs, and deletes it when it ends, after a failure too, unless another deploy's token is in it by then.
- Consent: a change of class replace, decrease or remove that no explicit value asks for stops before any stack change. A new release default
  that would need it, on a stage whose record follows the default, is not taken: the deploy keeps the stage's value and says so.

The deploys run the real scripts/deploy.sh against the fakes of test_deploy.py, through two wrappers. The wrapper of the CDK CLI logs every
HAMH_ variable of each cdk call, names the Instances capacity provider by a hash of its machine settings (as config.ts does), and writes into
the template of each stack it deploys the resources a live read looks at: the log groups (one that the stage names, in each stack that has
one, and the group a CDK construct makes for itself in the network and runtimes stacks), the run-session and research functions, the pull
request check, the S3 endpoint policy, the Tavily alarms and the capacity provider. The wrapper of the AWS CLI can fail one read on purpose.
A stage without a record is a stage an earlier release deployed: a case deploys it, then removes /hamh/direct/settings.
"""

from __future__ import annotations

import contextlib
import datetime as dt
import io
import json
import os
import re
import shutil
import subprocess  # nosec B404: tests start the script under test, bash and git with argument lists, never a shell string
import sys
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
sys.path.insert(0, str(REPO / "scripts" / "lib"))
sys.path.insert(0, str(HERE))
import test_deploy as td  # noqa: E402
from setup_waits import limit  # noqa: E402

REGISTRY = REPO / "scripts" / "lib" / "stage-settings.tsv"
CATALOG = REPO / "crates" / "ham-core" / "assets" / "utility-models.json"
SETTINGS = "/hamh/direct/settings"
LOCK = "/hamh/direct/deploy-lock"
# The default machine list of the Instances part, as deploy.py and the CDK app give it (test_b812_the_deploy_defaults_are_the_cdk_apps pins the two together).
DEFAULT_TYPES = "m7g.4xlarge,m6g.4xlarge"
SPACE = " \t\n\r\f\v"
HAIKU = "us.anthropic.claude-haiku-5-5"
PROVIDER_B = "hamh_direct_instances_bbbbbbbb"
# The log group that the AwsCustomResource construct of aws-cdk-lib makes for its provider function (the logical id both measured stages hold, in the
# network and runtimes templates): CDK names it by an intrinsic function and gives it 731 days, and the stage's retention setting never reaches it.
CONSTRUCT_GROUP = "AWS679f53fac002430cb0da5b7982bd2287LogGroup449FB7C2"
LOG_STACKS = ("network", "data", "control", "orchestration", "delivery", "runtimes")

# The rows the contract names, with their KEPT and CHANGE words.
CONTRACT_ROWS = {
    "HAMH_COMPUTE_INSTANCES": ("yes", "refuse-off"),
    "HAMH_INSTANCES_TYPES": ("yes", "replace"), "HAMH_INSTANCES_ROOT_FREE_GIB": ("yes", "replace"), "HAMH_INSTANCES_WORKSPACE_GIB": ("yes", "replace"),
    "HAMH_INSTANCES_LIFETIME_S": ("yes", "replace"), "HAMH_INSTANCES_IDLE_S": ("yes", "replace"),
    "HAMH_TOOLS_MODE": ("yes", "free"), "HAMH_UTILITY_MODEL": ("yes", "free"), "HAMH_UTILITY_PURPOSES": ("yes", "free"),
    "HAMH_RESEARCH_PROVIDER": ("yes", "free"), "HAMH_RESEARCH_ALARM_TOPIC_ARN": ("yes", "remove"), "HAMH_TAVILY_ALARMS": ("yes", "remove"),
    "HAMH_TAVILY_KEY_MAX_AGE_DAYS": ("yes", "free"), "HAMH_LOG_RETENTION_DAYS": ("yes", "decrease"),
    "HAMH_PR_CHECK_MINUTES": ("yes", "free"), "HAMH_PR_CHECK_COMPUTE": ("yes", "free"),
    "HAMH_AGENTCORE_STORAGE_ACCOUNTS": ("yes", "remove"), "HAMH_PROVIDER_SILENCE_S": ("yes", "free"), "HAMH_SLICE_DEADLINE_S": ("no", "free"),
    # Addendum 3 (A3.1): the keep tag and the kind of the stage, which the role of the account entry gave until a stage could keep them.
    "HAMH_KEEP_TAG": ("yes", "remove"), "HAMH_STAGE_KIND": ("yes", "free"),
}


def registry_rows() -> list[dict]:
    """The rows of the registry, parsed as its header says: the first three fields, the last one, and the pattern between them."""
    rows = []
    for line in REGISTRY.read_text(encoding="utf-8").splitlines():
        if not line.strip() or line.startswith("#"):
            continue
        parts = line.split("|")
        rows.append({"name": parts[0], "kept": parts[1], "change": parts[2], "pattern": "|".join(parts[3:-1]), "note": parts[-1]})
    return rows


def row(name: str) -> dict:
    return next(r for r in registry_rows() if r["name"] == name)


# ---- the wrappers of the fakes --------------------------------------------------------------------------------------------------------

NODE_WRAPPER = r'''#!/usr/bin/env python3
import hashlib, json, os, subprocess, sys, time
st = os.environ["FAKE_STATE"]
args = sys.argv[1:]
open(os.path.join(st, "node-env.log"), "a").write(json.dumps({"n": time.time_ns(), "args": args,
    "env": {k: v for k, v in os.environ.items() if k.startswith(("HAMH_", "CAH_"))}}) + "\n")  # COMPAT-CAH: the earlier spellings are logged to show the cdk calls get none
if args[:2] == ["cdk", "deploy"] and os.environ.get("FAKE_NODE_SLEEP_S"):
    time.sleep(float(os.environ["FAKE_NODE_SLEEP_S"]))  # a stack that takes a while: the deploy renews its lock meanwhile
SPACE = " \t\n\r\f\v"
REGION = os.environ.get("FAKE_STAGE_REGION", "us-west-2")
def word(name, default=""):
    return (os.environ.get(name) or "").strip(SPACE) or default
def num(name, default):
    return int(word(name, str(default)))
def role():
    """The role of the account entry the app selects (infra/lib/config.ts): HAMH_AWS_PROFILE names it, else the active entry of the account file."""
    try:
        doc = json.load(open(os.environ.get("HAMH_ACCOUNTS_FILE") or os.path.join(os.environ["DEPLOY_ROOT"], "accounts.json")))
        want = os.environ.get("HAMH_AWS_PROFILE") or ""
        return next(e["role"] for e in doc["accounts"] if (e["profile"] == want if want else e["role"] == "active"))
    except (OSError, KeyError, StopIteration, ValueError):
        return ""
ROLE = role()
# The keep tag and the stage kind as config.ts resolves them: the variable when it is set, else the role.
KEEP = word("HAMH_KEEP_TAG").lower() or ("on" if ROLE == "test" else "off")
KIND = word("HAMH_STAGE_KIND").lower() or ("official" if ROLE == "active" else "test")
shape = {"types": [t.strip(SPACE) for t in word("HAMH_INSTANCES_TYPES", "m7g.4xlarge,m6g.4xlarge").split(",")], "root": num("HAMH_INSTANCES_ROOT_FREE_GIB", 100),
         "workspace": num("HAMH_INSTANCES_WORKSPACE_GIB", 150), "lifetime": num("HAMH_INSTANCES_LIFETIME_S", 28800), "idle": num("HAMH_INSTANCES_IDLE_S", 1800)}
if KEEP == "on":
    # The provider's machines carry the stage's tags, and its name hashes them (config.ts instancesProviderShape): the keep tag is part of the name.
    shape["keep"] = "auto-delete=no"
env = dict(os.environ)
env.setdefault("FAKE_PROVIDER_NAME", "hamh_direct_instances_" + hashlib.sha256(json.dumps(shape, sort_keys=True).encode()).hexdigest()[:8])
rc = subprocess.run([sys.executable, os.environ["FAKE_NODE_REAL"], *args], env=env).returncode
if rc != 0 or args[:2] != ["cdk", "deploy"]:
    sys.exit(rc)
name = args[2]
short = name.split("hamh-direct-", 1)[1]
path = os.path.join(st, "stack-" + name)
body = json.load(open(path))
res = body.setdefault("template", {}).setdefault("Resources", {})
# The log groups as the CDK app synthesizes them. `Logs` is a group whose retention the stage sets: a CfnLogGroup with a literal name that carries the
# stage's name, here in each of the three forms the app uses. The network and runtimes stacks also hold the group that the AwsCustomResource construct
# makes for its provider function: its name is an intrinsic function and its 731 days are the construct's own, whatever the stage's setting is.
STAGE_GROUPS = {"network": "/hamh/direct/network/firewall-alert", "data": "/hamh/direct/bedrock-invocations", "control": "/aws/lambda/hamh-direct-control-api",
                "orchestration": "/aws/lambda/hamh-direct-run-session", "delivery": "/aws/codebuild/hamh-direct-prcheck", "runtimes": "/hamh/direct/codebuild/image-build"}
if short in STAGE_GROUPS:
    res["Logs"] = {"Type": "AWS::Logs::LogGroup", "Properties": {"LogGroupName": STAGE_GROUPS[short], "RetentionInDays": num("HAMH_LOG_RETENTION_DAYS", 30)}}
if short in ("network", "runtimes"):
    res["AWS679f53fac002430cb0da5b7982bd2287LogGroup449FB7C2"] = {"Type": "AWS::Logs::LogGroup", "Properties": {
        "LogGroupName": {"Fn::Join": ["", ["/aws/lambda/", {"Ref": "AWS679f53fac002430cb0da5b7982bd22872D164C4C"}]]}, "RetentionInDays": 731}}
provider = word("HAMH_RESEARCH_PROVIDER", "auto")
if short == "network":
    owners = {}
    for part in word("HAMH_AGENTCORE_STORAGE_ACCOUNTS").split(","):
        if part.strip():
            region, accounts = part.strip().split("=", 1)
            owners[region] = accounts.split("+")
    cond = {"aws:ResourceAccount": owners[REGION]} if REGION in owners else {"aws:PrincipalServiceName": "bedrock-agentcore.amazonaws.com"}
    bucket = "arn:aws:s3:::acr-storage-*-%s-an" % REGION
    res["S3Endpoint"] = {"Type": "AWS::EC2::VPCEndpoint", "Properties": {"PolicyDocument": {"Version": "2012-10-17", "Statement": [
        {"Sid": "HarnessBuckets", "Effect": "Allow", "Principal": "*", "Action": "s3:*", "Resource": ["arn:aws:s3:::hamh-direct-*"],
         "Condition": {"StringEquals": {"aws:ResourceAccount": "111111111111"}}},
        {"Sid": "AgentCoreSessionStorage", "Effect": "Allow", "Principal": "*", "Action": "s3:*", "Resource": [bucket, bucket + "/*"], "Condition": {"StringEquals": cond}}]}}}
if short == "control":
    res["ResearchApi"] = {"Type": "AWS::Lambda::Function", "Properties": {"FunctionName": "hamh-direct-research-api",
                          "Environment": {"Variables": {"RESEARCH_PROVIDER": provider, "HAMH_STAGE_KIND": KIND}}}}
    res["ResearchJob"] = {"Type": "AWS::Lambda::Function", "Properties": {"FunctionName": "hamh-direct-research-job",
                          "Environment": {"Variables": {"HAMH_STAGE_KIND": KIND}}}}
    if provider in ("tavily", "auto") and word("HAMH_TAVILY_ALARMS", "on").lower() != "off":
        topic = word("HAMH_RESEARCH_ALARM_TOPIC_ARN")
        for which in ("key-age", "key-refused", "rate-limited", "plan-limit"):
            props = {"AlarmName": "hamh-direct-research-tavily-" + which, "Threshold": num("HAMH_TAVILY_KEY_MAX_AGE_DAYS", 30) if which == "key-age" else 1}
            if topic:
                props.update(AlarmActions=[topic], OKActions=[topic])
            res["Alarm" + which.replace("-", "")] = {"Type": "AWS::CloudWatch::Alarm", "Properties": props}
if short == "orchestration":
    model = word("HAMH_UTILITY_MODEL").lower()
    if model == "":
        model = "%s" % ("us.anthropic.claude-haiku-5-5" if REGION == "us-east-1" else "off")
    elif model == "haiku":
        model = "us.anthropic.claude-haiku-5-5"
    purposes = word("HAMH_UTILITY_PURPOSES").lower()
    if model == "off":
        purposes = "off"
    elif purposes == "":
        purposes = "run-name,run-tags"
    else:
        given = {w.strip() for w in purposes.split(",")}
        purposes = ",".join(w for w in ("run-name", "run-tags") if w in given)
    variables = {"RESEARCH_PROVIDER": provider, "UTILITY_MODEL": model, "UTILITY_PURPOSES": purposes}
    for var in ("HAMH_PROVIDER_SILENCE_S", "HAMH_SLICE_DEADLINE_S"):
        if word(var):
            variables[var] = str(int(word(var)))
    res["RunSession"] = {"Type": "AWS::Lambda::Function", "Properties": {"FunctionName": "hamh-direct-run-session", "Environment": {"Variables": variables}}}
if short == "delivery":
    res["PrCheck"] = {"Type": "AWS::CodeBuild::Project", "Properties": {"Name": "hamh-direct-prcheck", "TimeoutInMinutes": num("HAMH_PR_CHECK_MINUTES", 60),
                      "Environment": {"Type": "ARM_CONTAINER", "ComputeType": "BUILD_GENERAL1_" + word("HAMH_PR_CHECK_COMPUTE", "small").upper()}}}
if short == "runtimes" and word("HAMH_COMPUTE_INSTANCES").lower() == "on":
    res["InstancesCapacityProvider"] = {"Type": "AWS::BedrockAgentCore::CapacityProvider", "Properties": {"Name": env["FAKE_PROVIDER_NAME"], "ComputeConfiguration": {
        "Ec2Configuration": {"LaunchTemplateSource": {"LaunchParameters": {"InstanceRequirements": {"AllowedInstanceTypes": shape["types"]}}},
                             "RootVolume": {"VolumeType": "gp3", "Encrypted": True, "FreeSpaceGiB": shape["root"]},
                             "Volumes": [{"EbsConfiguration": {"Name": "workspace", "SizeGiB": shape["workspace"]}}],
                             "LifecycleConfiguration": {"MaxLifetime": shape["lifetime"], "IdleInstanceTimeout": shape["idle"]}}}}}
# The stack tags CloudFormation holds after the deploy (DescribeStacks): the keep tag on every stack but the network stack (KEEP_TAG_EXCEPTIONS).
body.pop("tags", None)
if KEEP == "on" and short != "network":
    body["tags"] = {"auto-delete": "no"}
json.dump(body, open(path, "w"))
'''

AWS_WRAPPER = r'''#!/usr/bin/env python3
import json, os, sys, time
args = sys.argv[1:]
st = os.environ["FAKE_STATE"]
open(os.path.join(st, "aws-order.log"), "a").write(json.dumps({"n": time.time_ns(), "args": args}) + "\n")
def arg(name):
    return args[args.index(name) + 1] if name in args else None
if args[:2] == ["cloudformation", "get-template"] and arg("--stack-name") == os.environ.get("FAKE_GET_TEMPLATE_FAILS"):
    sys.stderr.write("An error occurred (AccessDenied) when calling the GetTemplate operation: not authorized to perform cloudformation:GetTemplate\n"); sys.exit(254)
if args[:2] == ["cloudformation", "describe-stacks"] and arg("--stack-name") == os.environ.get("FAKE_DESCRIBE_STACKS_FAILS"):
    sys.stderr.write("An error occurred (AccessDenied) when calling the DescribeStacks operation: not authorized to perform cloudformation:DescribeStacks\n"); sys.exit(254)
if args[:2] == ["ssm", "get-parameter"] and arg("--name") == os.environ.get("FAKE_GET_PARAMETER_FAILS"):
    sys.stderr.write("An error occurred (AccessDeniedException) when calling the GetParameter operation: not authorized to perform ssm:GetParameter\n"); sys.exit(254)
if args[:2] == ["ssm", "put-parameter"] and arg("--name") == os.environ.get("FAKE_LOCK_REPLACED_AT_PUT"):
    # Another deploy replaces the deploy lock at this moment, as setup does with a lock it takes for abandoned: FAKE_LOCK_OTHER is that deploy's lock.
    p = os.path.join(st, "ssm.json")
    d = json.load(open(p))
    d["/hamh/direct/deploy-lock"] = os.environ["FAKE_LOCK_OTHER"]
    with open(p, "w") as f:
        json.dump(d, f)
os.execv(sys.executable, [sys.executable, os.environ["FAKE_AWS_REAL"], *args])
'''

class FakeClock:
    """A clock that a case moves by hand. `now` is the deploy's clock; `wait` stands in for the wait of its lock keeper: it moves the clock on by the
    seconds asked for and says stop after `turns` waits."""

    def __init__(self, turns: int) -> None:
        self.at = dt.datetime(2026, 11, 10, 16, 0, 0, tzinfo=dt.timezone.utc)
        self.turns = turns
        self.waits: list[float] = []

    def now(self) -> dt.datetime:
        return self.at

    def wait(self, seconds: float) -> bool:
        self.waits.append(seconds)
        if len(self.waits) > self.turns:
            return True
        self.at += dt.timedelta(seconds=seconds)
        return False


SETTING_LINE = re.compile(r"^DEPLOY-SETTING (HAMH_[A-Z_]+)=(\S+) source=(explicit|kept|live|default)(?: change=(\S+) -> (\S+) \(([a-z-]+)\))?(?: new-default=\S+ \([a-z-]+\) :: .+)?$")
# The line of a setting whose new release default the deploy does not take, because taking it needs consent: the value kept, the default and the
# sentence that names the export.
HELD_LINE = re.compile(r"^DEPLOY-SETTING (HAMH_[A-Z_]+)=(\S+) source=kept new-default=(\S+) \(([a-z-]+)\) :: (.+)$")
REFUSED_LINE = re.compile(r"^DEPLOY-REFUSED reason=([a-z-]+) wait=(yes|no) stage=unchanged :: (.+)$")


def held(proc: subprocess.CompletedProcess) -> dict:
    """The settings whose new default the run did not take because taking it needs consent, by name: the value kept, the default, the class and the sentence."""
    out = {}
    for line in proc.stdout.splitlines():
        m = HELD_LINE.match(line)
        if m:
            out[m.group(1)] = {"value": m.group(2), "default": m.group(3), "class": m.group(4), "sentence": m.group(5)}
    return out


class SettingsBase(td.DeployCase):
    """test_deploy.DeployCase with the two wrappers, a VERSION file and the utility-model catalog in the tree it deploys."""

    def setUp(self) -> None:
        super().setUp()
        self.real = dict(self.bins)
        for name, body in (("node", NODE_WRAPPER), ("aws", AWS_WRAPPER)):
            p = self.tmp / f"{name}-wrapper"
            p.write_text(body.replace("/usr/bin/env python3", sys.executable, 1))
            p.chmod(0o755)
            self.bins[name] = str(p)
        (self.root / "VERSION").write_text("5.13\n")
        cat = self.root / "crates" / "ham-core" / "assets" / "utility-models.json"
        cat.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(CATALOG, cat)
        td.git(self.root, "add", ".")
        td.git(self.root, "commit", "-q", "-m", "version and catalog")

    def run_deploy(self, *args: str, **env: str) -> subprocess.CompletedProcess:
        names = [r["name"] for r in registry_rows()] + ["HAMH_DEPLOY_LOCK_TOKEN", "HAMH_SOURCE_COMMIT"]
        drop = {n for name in names for n in (name, "CAH_" + name[len("HAMH_"):])}  # COMPAT-CAH: an earlier spelling of the host's environment is no explicit value
        clean = {k: v for k, v in os.environ.items() if k not in drop}
        with mock.patch.dict(os.environ, clean, clear=True):
            return super().run_deploy(*args, FAKE_NODE_REAL=self.real["node"], FAKE_AWS_REAL=self.real["aws"], **env)

    def deployer(self):
        """scripts/lib/deploy.py as a module and a Deployer of this process on the fakes, for the cases that drive its clock by hand."""
        import deploy as dp
        names = [r["name"] for r in registry_rows()] + ["HAMH_DEPLOY_LOCK_TOKEN", "HAMH_SOURCE_COMMIT"]
        drop = {n for name in names for n in (name, "CAH_" + name[len("HAMH_"):])} | {"AWS_PROFILE"}  # COMPAT-CAH: as run_deploy drops them
        env = {k: v for k, v in os.environ.items() if k not in drop}
        env.update(HAMH_AWS_PROFILE="history-profile", DEPLOY_AWS_BIN=self.bins["aws"], DEPLOY_ROOT=str(self.root), FAKE_STATE=str(self.state),
                   FAKE_AWS_REAL=self.real["aws"])
        patch = mock.patch.dict(os.environ, env, clear=True)
        patch.start()
        self.addCleanup(patch.stop)
        d = dp.Deployer("direct")
        d.sha = d.commit = td.git(self.root, "rev-parse", "HEAD")
        d.caller_arn = f"arn:aws:sts::{td.ACCOUNT}:assumed-role/Admin/alice"
        return dp, d

    def deploy(self, *args: str, acct_two: bool = False, **env: str) -> subprocess.CompletedProcess:
        if acct_two:
            env = {"HAMH_AWS_PROFILE": "", "FAKE_ACCOUNT": td.ACCT_TWO, "FAKE_STAGE_REGION": "us-east-1", **env}
        return self.run_deploy("--stage", "direct", *args, **env)

    def ok(self, *args: str, **env) -> subprocess.CompletedProcess:
        proc = self.deploy(*args, **env)
        self.assertEqual(proc.returncode, 0, f"{env}: {proc.stdout}{proc.stderr}")
        return proc

    def seed_lock(self, token: str = "a" * 32) -> str:
        text = "\n".join((f"token={token}", "holder=ci-host:4242", f"identity=arn:aws:sts::{td.ACCOUNT}:assumed-role/hamh-setup-runner-role/b",
                          "route=runner", "started=2026-11-10T16:00:00Z", "commit=" + "c" * 40, "inputs=" + "d" * 64, "build=hamh-setup-runner:1"))
        d = self.ssm()
        d[LOCK] = text
        (self.state / "ssm.json").write_text(json.dumps(d))
        return text


    # ---- reading what happened ----

    def settings(self, proc: subprocess.CompletedProcess) -> dict:
        """The DEPLOY-SETTING lines of a run, by name: value, source and, for a change, old, new and class."""
        out = {}
        for line in proc.stdout.splitlines():
            m = SETTING_LINE.match(line)
            if m:
                out[m.group(1)] = {"value": m.group(2), "source": m.group(3), "old": m.group(4), "new": m.group(5), "class": m.group(6)}
        return out

    def env_log(self) -> list[dict]:
        p = self.state / "node-env.log"
        return [json.loads(x) for x in p.read_text().splitlines()] if p.exists() else []

    def cdk_calls(self, since: int = 0) -> list[dict]:
        """The environment of every synth and deploy of the CDK app after mark `since`."""
        return [c["env"] for c in self.env_log()[since:] if c["args"][:2] in (["cdk", "synth"], ["cdk", "deploy"])]

    def mark(self) -> int:
        return len(self.env_log())

    def record(self) -> dict | None:
        text = self.ssm().get(SETTINGS)
        if text is None:
            return None
        return dict(line.split("=", 1) for line in text.splitlines() if line)

    def drop_record(self) -> None:
        """The stage as a release left it: deployed, with no settings record."""
        d = self.ssm()
        d.pop(SETTINGS, None)
        (self.state / "ssm.json").write_text(json.dumps(d))

    def edit_record(self, **changes: str | None) -> None:
        d = self.ssm()
        rec = dict(line.split("=", 1) for line in d[SETTINGS].splitlines() if line)
        for k, v in changes.items():
            if v is None:
                rec.pop(k, None)
            else:
                rec[k] = v
        d[SETTINGS] = "\n".join(f"{k}={v}" for k, v in rec.items())
        (self.state / "ssm.json").write_text(json.dumps(d))

    def template(self, short: str) -> dict:
        return json.loads((self.state / f"stack-hamh-direct-{short}").read_text())["template"]

    def resource(self, short: str, logical: str) -> dict | None:
        return (self.template(short).get("Resources") or {}).get(logical)

    def edit_stack(self, short: str, edit) -> None:
        p = self.state / f"stack-hamh-direct-{short}"
        body = json.loads(p.read_text())
        edit(body)
        p.write_text(json.dumps(body))

    def retentions(self) -> dict:
        """The days of the log group the stage names in each stack that has one."""
        return {short: self.resource(short, "Logs")["Properties"]["RetentionInDays"] for short in LOG_STACKS}

    def provider(self) -> str:
        arn = json.loads((self.state / "stack-hamh-direct-runtimes").read_text())["outputs"]["InstancesCapacityProviderArn"]
        return re.search(r"capacity-provider/([^-/]+)-", arn).group(1)

    def stack_deploys(self) -> int:
        return len([c for c in self.env_log() if c["args"][:2] == ["cdk", "deploy"]])

    def aws_calls(self, since: int = 0) -> list[list[str]]:
        return [c["args"] for c in self.log("aws")[since:]]

    def puts(self, name: str, since: int = 0) -> list[list[str]]:
        return [a for a in self.aws_calls(since) if a[:2] == ["ssm", "put-parameter"] and a[a.index("--name") + 1] == name]

    def refused(self, proc: subprocess.CompletedProcess) -> tuple[str, str, str]:
        """reason, wait and text of the run's one DEPLOY-REFUSED line, which is its last line; exit 3 and no DEPLOY-FAIL line."""
        self.assertEqual(proc.returncode, 3, proc.stdout + proc.stderr)
        lines = [ln for ln in proc.stdout.splitlines() if ln.startswith("DEPLOY-REFUSED")]
        self.assertEqual(len(lines), 1, proc.stdout)
        self.assertEqual(self.last(proc), lines[0])
        self.assertNotIn("DEPLOY-FAIL", proc.stdout)
        m = REFUSED_LINE.match(lines[0])
        self.assertIsNotNone(m, lines[0])
        return m.group(1), m.group(2), m.group(3)

    def assert_kept(self, proc: subprocess.CompletedProcess, values: dict, source: str, since: int) -> None:
        """Each setting of `values` resolved to its value from `source`, with no change, and reached every cdk call of the run."""
        rows = self.settings(proc)
        for name, value in values.items():
            self.assertEqual(rows[name], {"value": value, "source": source, "old": None, "new": None, "class": None}, f"{name}: {proc.stdout}")
        calls = self.cdk_calls(since)
        self.assertTrue(calls)
        for call in calls:
            for name, value in values.items():
                self.assertEqual(call.get(name), value, name)
        rec = self.record()
        for name, value in values.items():
            self.assertEqual(rec.get(f"set.{name}"), value, f"{name} is recorded as the stage's choice")


# SettingsBase borrows the fakes and helpers of test_deploy.DeployCase, not its cases: test_deploy.py runs those.
for _name in [n for n in dir(td.DeployCase) if n.startswith("test")]:
    setattr(SettingsBase, _name, None)


# ---- the registry ------------------------------------------------------------------------------------------------------------------------

def ts(path: str) -> str:
    return (REPO / path).read_text(encoding="utf-8")


def ts_const(name: str, src: str) -> str:
    m = re.search(r"export const %s\b[^=]*=\s*([^;]+);" % re.escape(name), src)
    assert m, name
    return m.group(1)


def ts_ints(text: str) -> list[int]:
    return [int(x.replace("_", "")) for x in re.findall(r"[0-9][0-9_]*", text)]


def ts_words(text: str) -> list[str]:
    return re.findall(r'"([^"]+)"', text)


def ts_regex(name: str, src: str) -> re.Pattern:
    """A regular expression literal of the TypeScript source, compiled by Python (the literals used here mean the same in both)."""
    m = re.search(r"const %s = /(.+)/;" % re.escape(name), src)
    assert m, name
    return re.compile(m.group(1))


class CodeModel:
    """What the CDK app accepts for each setting, written from its readers (after the trim of the six ASCII whitespace characters that
    deploy.py and config.ts `trimSetting` apply). Lists use plain spaces around commas here; a list that names an item twice is left out of
    the samples, because the pattern cannot see a repeat (the app refuses it at synth, before any stack change)."""

    def __init__(self) -> None:
        cfg, net = ts("infra/lib/config.ts"), ts("infra/lib/stacks/network.ts")
        self.log_days = ts_ints(ts_const("LOG_RETENTION_DAYS_ALLOWED", cfg).split("]")[0])
        self.pr = (ts_ints(ts_const("MIN_PR_CHECK_MINUTES", cfg))[0], ts_ints(ts_const("MAX_PR_CHECK_MINUTES", cfg))[0])
        self.compute = ts_words(ts_const("PR_CHECK_COMPUTE_WORDS", cfg))
        self.providers = ts_words(ts_const("RESEARCH_PROVIDERS", cfg))
        self.disk = (1, ts_ints(ts_const("MAX_INSTANCES_DISK_GIB", cfg))[0])
        self.lifetime = (ts_ints(ts_const("MIN_INSTANCES_LIFETIME_S", cfg))[0], ts_ints(ts_const("MAX_INSTANCES_LIFETIME_S", cfg))[0])
        self.idle = (ts_ints(ts_const("MIN_INSTANCES_IDLE_S", cfg))[0], self.lifetime[1])
        m = re.search(r'"HAMH_PROVIDER_SILENCE_S", ([0-9_]+), ([0-9_]+)', cfg)
        self.silence = (ts_ints(m.group(1))[0], ts_ints(m.group(2))[0])
        m = re.search(r"n < ([0-9_]+) \|\| n > ([0-9_]+)", cfg)
        self.slice = (ts_ints(m.group(1))[0], ts_ints(m.group(2))[0])
        m = re.search(r"count >= ([0-9]+) && count <= ([0-9]+)", cfg)
        self.max_age = (int(m.group(1)), int(m.group(2)))
        sizes = re.search(r"const SIZE_VCPUS[^{]*\{([^}]+)\}", cfg).group(1)
        self.sizes = re.findall(r'"?([0-9]*x?[a-z]+)"?\s*:', sizes)
        self.graviton = ts_regex("GRAVITON_TYPE", cfg)
        self.sns = ts_regex("SNS_TOPIC_ARN", cfg)
        self.regions = ts_words(ts_const("DEPLOYABLE_REGIONS", net))
        self.storage = re.compile(re.search(r"const m = /(.+)/\.exec\(part\)", net).group(1))
        self.keep_words = ts_words(ts_const("KEEP_TAG_WORDS", cfg))
        self.kinds = ts_words(ts_const("STAGE_KINDS", cfg))
        doc = json.loads(CATALOG.read_text())
        self.model_ids = [m["id"] for m in doc["models"]]
        self.model_aliases = [m["alias"] for m in doc["models"]]

    @staticmethod
    def number(v: str, lo: int, hi: int, shape: str = r"[1-9][0-9]{0,9}") -> bool:
        return re.fullmatch(shape, v) is not None and lo <= int(v) <= hi

    def accepts(self, name: str, v: str) -> bool:
        if name in ("HAMH_COMPUTE_INSTANCES", "HAMH_TOOLS_MODE", "HAMH_TAVILY_ALARMS"):
            return v.lower() in ("on", "off")
        if name == "HAMH_INSTANCES_TYPES":
            for t in (p.strip(SPACE) for p in v.split(",")):
                m = self.graviton.fullmatch(t)
                if not m or m.group(1) not in self.sizes:
                    return False
            return True
        if name in ("HAMH_INSTANCES_ROOT_FREE_GIB", "HAMH_INSTANCES_WORKSPACE_GIB"):
            return self.number(v, *self.disk)
        if name == "HAMH_INSTANCES_LIFETIME_S":
            return self.number(v, *self.lifetime)
        if name == "HAMH_INSTANCES_IDLE_S":
            return self.number(v, *self.idle)
        if name == "HAMH_UTILITY_MODEL":
            return v.lower() == "off" or v in self.model_ids or v.lower() in self.model_aliases
        if name == "HAMH_UTILITY_PURPOSES":
            return all(w.strip().lower() in ("run-name", "run-tags") for w in v.split(","))
        if name == "HAMH_RESEARCH_PROVIDER":
            return v in self.providers
        if name == "HAMH_RESEARCH_ALARM_TOPIC_ARN":
            return self.sns.fullmatch(v) is not None
        if name == "HAMH_TAVILY_KEY_MAX_AGE_DAYS":
            return self.number(v, *self.max_age, shape=r"[0-9]{1,3}")
        if name == "HAMH_LOG_RETENTION_DAYS":
            return re.fullmatch(r"[1-9][0-9]{0,4}", v) is not None and int(v) in self.log_days
        if name == "HAMH_PR_CHECK_MINUTES":
            return self.number(v, *self.pr)
        if name == "HAMH_PR_CHECK_COMPUTE":
            return v.lower() in self.compute
        if name == "HAMH_AGENTCORE_STORAGE_ACCOUNTS":
            parts = [p.strip() for p in v.split(",") if p.strip()]
            return bool(parts) and all((m := self.storage.fullmatch(p)) is not None and m.group(1) in self.regions for p in parts)
        if name == "HAMH_PROVIDER_SILENCE_S":
            return self.number(v, *self.silence)
        if name == "HAMH_SLICE_DEADLINE_S":
            return self.number(v, *self.slice, shape=r"[0-9]+")
        if name == "HAMH_KEEP_TAG":
            return v.lower() in self.keep_words
        if name == "HAMH_STAGE_KIND":
            return v.lower() in self.kinds
        raise AssertionError(name)

    def samples(self, name: str) -> list[str]:
        """Values near every edge of what the setting takes, and values beside them."""
        bad = ["", "default ", "x", "0", "-1", "+5", "5.0", "1e3", "on off", "on\n", "ON;", "1 2"]
        def nums(lo: int, hi: int) -> list[str]:
            pts = {lo - 2, lo - 1, lo, lo + 1, hi - 1, hi, hi + 1, hi + 2, (lo + hi) // 2, 10 * hi, hi * 10 + 7}
            pts |= {d for d in range(max(0, lo - 30), min(hi, lo + 300))}
            pts |= {d for d in range(max(0, hi - 300), hi + 30)}
            out = {str(p) for p in pts if p >= 0}
            return sorted(out | {"0" + str(lo), "00" + str(lo + 1), str(lo) + " "})
        if name in ("HAMH_COMPUTE_INSTANCES", "HAMH_TOOLS_MODE", "HAMH_TAVILY_ALARMS"):
            return bad + ["on", "off", "ON", "Off", "oN", "of", "onn", "o n", "true", "auto", "0n"]
        if name == "HAMH_INSTANCES_TYPES":
            good = [f"m7g.{s}" for s in self.sizes] + ["c7gn.4xlarge", "r8g.16xlarge", "x2gd.2xlarge", "im4gn.large", "m7g.4xlarge,m7g.8xlarge",
                                                       "m7g.4xlarge , c7g.8xlarge", "m7g.4xlarge,r7g.4xlarge,c7g.4xlarge"]
            return bad + good + ["m7i.4xlarge", "m7g.5xlarge", "m7g.metal", "M7g.4xlarge", "m7g.4xlarge,", ",m7g.4xlarge", "m7g.4xlarge,,c7g.large",
                                 "abcdefghijklmg.large", "m7g4xlarge", "m7gxx.large", "t4g.nano"]
        if name in ("HAMH_INSTANCES_ROOT_FREE_GIB", "HAMH_INSTANCES_WORKSPACE_GIB"):
            return bad + nums(*self.disk)
        if name == "HAMH_INSTANCES_LIFETIME_S":
            return bad + nums(*self.lifetime)
        if name == "HAMH_INSTANCES_IDLE_S":
            return bad + nums(*self.idle)
        if name == "HAMH_UTILITY_MODEL":
            return bad + ["off", "OFF", "haiku", "HAIKU", "Haiku", HAIKU, HAIKU.upper(), "sonnet", "us.anthropic.claude-haiku-5-55", "haikux"]
        if name == "HAMH_UTILITY_PURPOSES":
            return bad + ["run-name", "run-tags", "RUN-NAME", "run-name,run-tags", "run-tags, run-name", "run-name , run-name", "run-name,",
                          ",run-tags", "run_name", "run-name,,run-tags", "run-names"]
        if name == "HAMH_RESEARCH_PROVIDER":
            return bad + self.providers + ["AUTO", "Tavily", "tavily-keyles", "web"]
        if name == "HAMH_RESEARCH_ALARM_TOPIC_ARN":
            base = "arn:aws:sns:us-east-1:123456789012:"
            return bad + [base + "alarms", base + "alarms.fifo", base + "a" * 256, base + "a" * 257, base + "a" * 255 + ".fifo",
                          "arn:aws-us-gov:sns:us-gov-west-1:123456789012:t", "arn:aws:sns:us-east-1:12345678901:t", "arn:aws:sqs:us-east-1:123456789012:t",
                          base, base + "a.b", base + "a b", "arn:aws:sns:US-EAST-1:123456789012:t"]
        if name == "HAMH_TAVILY_KEY_MAX_AGE_DAYS":
            return bad + nums(*self.max_age) + ["007", "0007", "000", "00", "0365", "366"]
        if name == "HAMH_LOG_RETENTION_DAYS":
            return bad + [str(d) for d in range(0, 4000)] + ["030", "365 "]
        if name == "HAMH_PR_CHECK_MINUTES":
            return bad + nums(*self.pr)
        if name == "HAMH_PR_CHECK_COMPUTE":
            return bad + self.compute + ["SMALL", "Large", "2XLARGE", "3xlarge", "tiny", "x large"]
        if name == "HAMH_AGENTCORE_STORAGE_ACCOUNTS":
            a, b = "123456789012", "210987654321"
            return bad + [f"us-east-1={a}", f"us-west-2={a}+{b}", f"us-east-1={a},us-west-2={b}", f" us-east-1={a} , us-west-2={b} ", f"us-east-1={a},",
                          f",,us-east-1={a}", f"us-east-1={a},,us-west-2={b}", f"eu-west-1={a}", f"us-east-1={a}+", f"us-east-1={a[:-1]}", f"us-east-1 = {a}",
                          f"us-east-1={a} us-west-2={b}", f"us-east-2={a}", ",", " , "]
        if name == "HAMH_PROVIDER_SILENCE_S":
            return bad + nums(*self.silence)
        if name == "HAMH_SLICE_DEADLINE_S":
            return bad + nums(*self.slice) + ["0060", "000000300", "028800", "028801"]
        if name == "HAMH_KEEP_TAG":
            return bad + ["on", "off", "ON", "Off", "oN", "of", "onn", "o n", "true", "yes", "no", "auto-delete", "0n"]
        if name == "HAMH_STAGE_KIND":
            return bad + ["official", "test", "OFFICIAL", "Test", "tEsT", "officia", "tests", "prod", "staging", "active", "history", "official test", "off"]
        raise AssertionError(name)


BASH_MATCH = r'''re=$1
while IFS= read -r v; do
  if [[ $v =~ $re ]]; then echo 1; else echo 0; fi
done'''

BASH_PARSE = r'''while IFS= read -r line; do
  case $line in ''|'#'*) continue ;; esac
  IFS='|' read -r name kept change rest <<<"$line"
  pattern=${rest%|*}
  note=${rest##*|}
  printf '%s\t%s\t%s\t%s\t%s\n' "$name" "$kept" "$change" "$pattern" "$note"
done'''


def bash(script: str, *args: str, stdin: str = "") -> str:
    proc = subprocess.run(["bash", "-c", script, "bash", *args], input=stdin, capture_output=True, text=True, timeout=limit(120))  # nosec B603: argument list, no shell string, bash runs a fixed script of this file; B607: bash looked up on PATH as the scripts under test do
    assert proc.returncode == 0, proc.stderr
    return proc.stdout


class RegistryCase(unittest.TestCase):
    def test_b812_the_registry_lists_every_stage_setting_once_with_the_contract_classes(self) -> None:
        rows = registry_rows()
        self.assertEqual([r["name"] for r in rows], list(CONTRACT_ROWS), "one row per setting of the contract, in its order")
        for r in rows:
            self.assertEqual((r["kept"], r["change"]), CONTRACT_ROWS[r["name"]], r["name"])
            self.assertTrue(r["pattern"].startswith("^") and r["pattern"].endswith("$"), r["name"])
            self.assertTrue(r["note"].endswith(".") and "|" not in r["note"] and len(r["note"]) < 200, r["name"])
        # Every name is a variable the CDK app reads as a stage setting (infra/lib/env.ts VARIABLES), with its earlier spelling.
        env_ts = ts("infra/lib/env.ts")
        for r in rows:
            self.assertRegex(env_ts, r"\n  %s: setting," % r["name"][len("HAMH_"):], r["name"])

    def test_b812_bash_reads_the_registry_as_python_does(self) -> None:
        got = [dict(zip(("name", "kept", "change", "pattern", "note"), line.split("\t"))) for line in bash(BASH_PARSE, stdin=REGISTRY.read_text()).splitlines()]
        self.assertEqual(got, registry_rows())

    def test_b812_no_pattern_repeats_more_than_255_times(self) -> None:
        # bash 3.2 on macOS compiles its patterns with the BSD regex library, whose limit is 255: `^a{1,256}$` is no pattern there.
        for r in registry_rows():
            for lo, hi in re.findall(r"\{([0-9]+)(?:,([0-9]*))?\}", r["pattern"]):
                self.assertLessEqual(max(int(lo), int(hi or 0)), 255, r["name"])
            self.assertNotRegex(r["pattern"], r"\\[dswDSWbB]|\(\?", r["name"])

    def test_b812_every_pattern_gives_the_same_answer_in_python_and_bash_and_accepts_what_the_cdk_app_accepts(self) -> None:
        model = CodeModel()
        for r in registry_rows():
            with self.subTest(name=r["name"]):
                samples = [s for s in model.samples(r["name"]) if "\n" not in s]
                py = ["1" if re.fullmatch(r["pattern"], s) else "0" for s in samples]
                sh = bash(BASH_MATCH, r["pattern"], stdin="".join(s + "\n" for s in samples)).split()
                self.assertEqual(len(sh), len(samples))
                for s, a, b in zip(samples, py, sh):
                    self.assertEqual(a, b, f"python and bash differ on {s!r}")
                    self.assertEqual(a == "1", model.accepts(r["name"], s), f"the pattern and the CDK app differ on {s!r}")
                self.assertIn("1", py, "some sample passes")
                self.assertIn("0", py, "some sample fails")
                self.assertIsNone(re.fullmatch(r["pattern"], "default"), "`default` is the deploy's word, not a value of the app")

    def test_b815_the_default_machine_list_holds_more_than_one_type(self) -> None:
        """One machine type in one zone ran out of EC2 capacity; the default lets AgentCore pick among types of one size and architecture."""
        import deploy  # noqa: PLC0415
        types = deploy.SETTING_DEFAULTS["HAMH_INSTANCES_TYPES"].split(",")
        self.assertGreater(len(types), 1, types)
        self.assertEqual(DEFAULT_TYPES, deploy.SETTING_DEFAULTS["HAMH_INSTANCES_TYPES"], "the tests' copy of the default")
        self.assertEqual(types[0], "m7g.4xlarge", "the type the spike proved stays first")
        for t in types:
            self.assertRegex(t, r"^m[0-9]+g\.4xlarge$", "general purpose Graviton (arm64), 16 vCPU and 64 GiB each")
        self.assertTrue(re.fullmatch(row("HAMH_INSTANCES_TYPES")["pattern"], deploy.SETTING_DEFAULTS["HAMH_INSTANCES_TYPES"]), "the registry takes the default")
        self.assertIn('word("HAMH_INSTANCES_TYPES", "%s")' % deploy.SETTING_DEFAULTS["HAMH_INSTANCES_TYPES"], NODE_WRAPPER, "the stand-in CDK app names the provider from the same default")

    def test_b812_the_deploy_defaults_are_the_cdk_apps(self) -> None:
        import deploy  # noqa: PLC0415
        cfg = ts("infra/lib/config.ts")
        want = {
            "HAMH_COMPUTE_INSTANCES": ts_words(ts_const("DEFAULT_COMPUTE_INSTANCES", cfg))[0], "HAMH_TOOLS_MODE": ts_words(ts_const("DEFAULT_TOOLS_MODE", cfg))[0],
            "HAMH_RESEARCH_PROVIDER": ts_words(ts_const("DEFAULT_RESEARCH_PROVIDER", cfg))[0], "HAMH_RESEARCH_ALARM_TOPIC_ARN": "", "HAMH_TAVILY_ALARMS": "on",
            "HAMH_TAVILY_KEY_MAX_AGE_DAYS": ts_const("DEFAULT_TAVILY_KEY_MAX_AGE_DAYS", cfg), "HAMH_LOG_RETENTION_DAYS": ts_const("DEFAULT_LOG_RETENTION_DAYS", cfg),
            "HAMH_PR_CHECK_MINUTES": ts_const("DEFAULT_PR_CHECK_MINUTES", cfg), "HAMH_PR_CHECK_COMPUTE": ts_words(ts_const("DEFAULT_PR_CHECK_COMPUTE", cfg))[0],
            "HAMH_AGENTCORE_STORAGE_ACCOUNTS": "", "HAMH_PROVIDER_SILENCE_S": "", "HAMH_SLICE_DEADLINE_S": "",
        }
        inst = re.search(r"instances: \{\s*instanceTypes: \[\.\.\.DEFAULT_INSTANCE_TYPES\], vcpus: [0-9]+, rootFreeGiB: ([0-9_]+), workspaceGiB: ([0-9_]+),[^}]*?"
                         r"maxLifetimeS: ([0-9_]+), idleInstanceTimeoutS: ([0-9_]+)", cfg)
        types = re.search(r"export const DEFAULT_INSTANCE_TYPES = \[([^\]]*)\] as const;", cfg)
        want.update({"HAMH_INSTANCES_TYPES": ",".join(ts_words(types.group(1))), "HAMH_INSTANCES_ROOT_FREE_GIB": str(ts_ints(inst.group(1))[0]),
                     "HAMH_INSTANCES_WORKSPACE_GIB": str(ts_ints(inst.group(2))[0]), "HAMH_INSTANCES_LIFETIME_S": str(ts_ints(inst.group(3))[0]),
                     "HAMH_INSTANCES_IDLE_S": str(ts_ints(inst.group(4))[0])})
        self.assertEqual({k: v for k, v in deploy.SETTING_DEFAULTS.items() if k in want}, want)
        self.assertEqual(set(deploy.SETTING_DEFAULTS), set(CONTRACT_ROWS) - {"HAMH_UTILITY_MODEL", "HAMH_UTILITY_PURPOSES", "HAMH_KEEP_TAG", "HAMH_STAGE_KIND"},
                         "the utility model's default comes from the catalog and the stage Region, the keep tag's and the stage kind's from the role")
        self.assertEqual([r.name for r in deploy.load_settings()], list(CONTRACT_ROWS))

    def test_b812_the_cdk_app_names_every_log_group_whose_retention_it_sets_after_the_stage(self) -> None:
        """A4.2: the live reader of HAMH_LOG_RETENTION_DAYS reads the log groups whose name is a plain string that carries the stage's name (deploy.py
        `stage_log_group`). Held to the CDK app: every group that gets cfg.logRetentionDays is a CfnLogGroup with such a literal name, and no stack of the
        stage sets a retention another way."""
        stacks = [p for p in sorted((REPO / "infra" / "lib" / "stacks").glob("*.ts")) if not p.name.startswith("dev-builder")]
        call = re.compile(r"new logs\.CfnLogGroup\(\s*(?:this|stack),\s*[^,]+,\s*\{(.*?)\}\s*\)", re.S)
        config = ts("infra/lib/config.ts")
        found = 0
        for path in stacks:
            src = path.read_text(encoding="utf-8")
            self.assertNotIn("logs.LogGroup(", src, f"{path.name}: a stage log group is a CfnLogGroup with a name of its own")
            groups = call.findall(src)
            self.assertEqual(len(groups), src.count("new logs.CfnLogGroup("), f"{path.name}: every log group is read")
            self.assertEqual(src.count("retentionInDays"), len(groups), f"{path.name}: a retention is set on the stage's log groups only")
            for props in groups:
                found += 1
                self.assertIn("retentionInDays: cfg.logRetentionDays", props, path.name)
                name = re.search(r"logGroupName:\s*(`[^`]*`|\w+)", props)
                self.assertIsNotNone(name, f"{path.name}: {props}")
                text = name.group(1)
                if not text.startswith("`"):
                    # An identifier: the invocation log group, named by `names.bedrockInvocationLogGroup` of config.ts.
                    self.assertRegex(src, rf"const {text} = names\.bedrockInvocationLogGroup\(cfg\);", path.name)
                    made = re.search(r"bedrockInvocationLogGroup: \(cfg: StageConfig\) => (`[^`]*`)", config)
                    self.assertIsNotNone(made)
                    text = made.group(1)
                text = text[1:-1]
                if text.startswith("/hamh/${cfg.stage}/"):
                    continue
                m = re.fullmatch(r"/aws/(?:lambda|codebuild)/\$\{(\w+)\}(.*)", text)
                self.assertIsNotNone(m, f"{path.name}: {text} carries the stage's name in no form the reader knows")
                if m.group(1) == "p":
                    self.assertTrue(m.group(2).startswith("-"), text)
                else:
                    self.assertRegex(src, rf"const {m.group(1)} = `\$\{{p\}}-", f"{path.name}: {text}")
                self.assertRegex(src, r"const p = (?:props\.)?cfg\.prefix;", path.name)
        self.assertGreaterEqual(found, 10, "the stage's log groups were found")
        # The names the fake CDK app writes are of these forms, and the reader takes them and not the construct's group.
        import deploy as dp
        d = dp.Deployer.__new__(dp.Deployer)
        d.stage, d.prefix = "direct", "hamh-direct"
        for name in ("/hamh/direct/network/firewall-alert", "/hamh/direct/bedrock-invocations", "/aws/lambda/hamh-direct-control-api",
                     "/aws/codebuild/hamh-direct-prcheck", "/hamh/direct/codebuild/image-build"):
            self.assertTrue(d.stage_log_group({"LogGroupName": name}), name)
        for name in ({"Fn::Join": ["", ["/aws/lambda/", {"Ref": "AWS679f53fac002430cb0da5b7982bd22872D164C4C"}]]}, None, "/aws/lambda/other-function",
                     "/hamh/directly/x", "/aws/lambda/hamh-direct", "/hamh/other/network/firewall-alert"):
            self.assertFalse(d.stage_log_group({"LogGroupName": name}), name)

    def test_b812_the_keep_tag_and_the_stage_kind_default_to_the_role_as_in_the_cdk_app(self) -> None:
        # Addendum 3 (A3.1): unset, the role of the account entry gives both words (infra/lib/config.ts KEEP_TAG_OF_ROLE and STAGE_KIND_OF_ROLE).
        import deploy  # noqa: PLC0415
        cfg, keep = ts("infra/lib/config.ts"), ts("infra/lib/keep-tag.ts")
        tables = {"HAMH_KEEP_TAG": "KEEP_TAG_OF_ROLE", "HAMH_STAGE_KIND": "STAGE_KIND_OF_ROLE"}
        self.assertEqual(set(deploy.ROLE_DEFAULTS), set(tables))
        for name, const in tables.items():
            self.assertEqual(deploy.ROLE_DEFAULTS[name], dict(re.findall(r'([a-z]+): "([a-z]+)"', ts_const(const, cfg))), name)
            self.assertEqual(set(deploy.ROLE_DEFAULTS[name]), {"active", "history", "test"}, name)
        self.assertEqual(deploy.ROLE_DEFAULTS["HAMH_KEEP_TAG"], {"active": "off", "history": "off", "test": "on"})
        self.assertEqual(deploy.ROLE_DEFAULTS["HAMH_STAGE_KIND"], {"active": "official", "history": "test", "test": "test"})
        # The tag the live reader looks for, and the stacks that never carry it, are the app's.
        self.assertEqual(deploy.KEEP_TAG_KEY, ts_words(ts_const("KEEP_TAG_KEY", keep))[0])
        self.assertEqual(deploy.KEEP_TAG_VALUE, ts_words(ts_const("KEEP_TAG_VALUE", keep))[0])
        self.assertEqual(list(deploy.KEEP_TAG_EXCEPTION_STACKS), ts_words(re.search(r"stacks: (\[[^\]]*\])", ts_const("KEEP_TAG_EXCEPTIONS", cfg)).group(1)))


# ---- the rows of the update matrix --------------------------------------------------------------------------------------------------

class KeepCase(SettingsBase):
    def test_b812_update_keeps_the_instances_machine_settings_and_does_not_replace_the_provider(self) -> None:
        shape = dict(HAMH_INSTANCES_TYPES="m7g.8xlarge", HAMH_INSTANCES_ROOT_FREE_GIB="200", HAMH_INSTANCES_WORKSPACE_GIB="400",
                     HAMH_INSTANCES_LIFETIME_S="172800", HAMH_INSTANCES_IDLE_S="3600")
        self.ok(HAMH_COMPUTE_INSTANCES="on", **shape)
        name = self.provider()
        self.assertNotEqual(name, "hamh_direct_instances_aaaaaaaa")
        for with_record in (True, False):
            with self.subTest(record=with_record):
                if not with_record:
                    self.drop_record()
                mark = self.mark()
                # The update carries only the Instances flag, as the local route does today.
                again = self.ok(HAMH_COMPUTE_INSTANCES="on")
                self.assert_kept(again, shape, "kept" if with_record else "live", mark)
                self.assertEqual(self.provider(), name, "the provider keeps its name, so CloudFormation replaces nothing")
                self.assertFalse([ln for ln in again.stdout.splitlines() if "REPLACES" in ln], again.stdout)
        # An update that carries nothing keeps the part and its machines as well.
        mark = self.mark()
        nothing = self.ok()
        self.assert_kept(nothing, {"HAMH_COMPUTE_INSTANCES": "on", **shape}, "kept", mark)
        self.assertEqual(self.provider(), name)
        # A new machine shape happens only on an explicit value, and the deploy names the change and its class.
        change = self.ok(HAMH_INSTANCES_TYPES="m7g.16xlarge")
        self.assertEqual(self.settings(change)["HAMH_INSTANCES_TYPES"], {"value": "m7g.16xlarge", "source": "explicit", "old": "m7g.8xlarge",
                                                                          "new": "m7g.16xlarge", "class": "replace"})
        self.assertNotEqual(self.provider(), name)
        self.assertTrue([ln for ln in change.stdout.splitlines() if "REPLACES" in ln], change.stdout)
        self.assertEqual(self.record()["set.HAMH_INSTANCES_TYPES"], "m7g.16xlarge")
        # `default` returns to the code default: the variable leaves the cdk calls and the record.
        mark = self.mark()
        back = self.ok(HAMH_INSTANCES_TYPES="default")
        self.assertEqual(self.settings(back)["HAMH_INSTANCES_TYPES"], {"value": "default", "source": "explicit", "old": "m7g.16xlarge",
                                                                        "new": DEFAULT_TYPES, "class": "replace"})
        self.assertTrue(all("HAMH_INSTANCES_TYPES" not in c for c in self.cdk_calls(mark)))
        self.assertNotIn("set.HAMH_INSTANCES_TYPES", self.record())
        self.assertEqual(self.record()["set.HAMH_INSTANCES_ROOT_FREE_GIB"], "200", "the other machine settings stay")

    def test_b812_update_keeps_tools_mode_on(self) -> None:
        self.ok(HAMH_TOOLS_MODE="on")
        for with_record in (True, False):
            with self.subTest(record=with_record):
                if not with_record:
                    self.drop_record()
                self.running("01K5RUNA")
                listed, mark = len(self.listed()), self.mark()
                again = self.ok()
                self.assert_kept(again, {"HAMH_TOOLS_MODE": "on"}, "kept" if with_record else "live", mark)
                self.assertEqual(len(self.listed()), listed, "no new runtime version, so the running run does not stop the update")
                self.assertNotIn("CHANGES", "\n".join(ln for ln in again.stdout.splitlines() if ln.startswith("deploy: tools mode")))
                self.assertEqual(self.live_mode(), "on")
                self.running()
        off = self.ok(HAMH_TOOLS_MODE="off")
        self.assertEqual(self.settings(off)["HAMH_TOOLS_MODE"], {"value": "off", "source": "explicit", "old": "on", "new": "off", "class": "free"})
        self.assertEqual(self.live_mode(), "off")
        self.assertEqual(self.record()["set.HAMH_TOOLS_MODE"], "off", "an explicit value is the user's choice, even when it equals the default")
        self.ok(HAMH_TOOLS_MODE="on")
        mark = self.mark()
        back = self.ok(HAMH_TOOLS_MODE="default")
        self.assertEqual(self.settings(back)["HAMH_TOOLS_MODE"], {"value": "default", "source": "explicit", "old": "on", "new": "off", "class": "free"})
        self.assertEqual({c["HAMH_TOOLS_MODE"] for c in self.cdk_calls(mark)}, {"off"}, "the tool layer's word is always concrete")
        self.assertNotIn("set.HAMH_TOOLS_MODE", self.record())

    def test_b812_update_keeps_the_utility_model_switched_off(self) -> None:
        # A us-east-1 stage, where the catalog's default model is smoked: unset would turn run names on again.
        self.ok(acct_two=True, HAMH_UTILITY_MODEL="off")
        self.assertEqual(self.resource("orchestration", "RunSession")["Properties"]["Environment"]["Variables"]["UTILITY_MODEL"], "off")
        for with_record in (True, False):
            with self.subTest(record=with_record):
                if not with_record:
                    self.drop_record()
                mark = self.mark()
                again = self.ok(acct_two=True)
                self.assert_kept(again, {"HAMH_UTILITY_MODEL": "off"}, "kept" if with_record else "live", mark)
                self.assertEqual(self.resource("orchestration", "RunSession")["Properties"]["Environment"]["Variables"]["UTILITY_MODEL"], "off")
        # The model on with run names only: the limited purposes are kept the same way.
        on = self.ok(acct_two=True, HAMH_UTILITY_MODEL="default", HAMH_UTILITY_PURPOSES="run-name")
        self.assertEqual(self.settings(on)["HAMH_UTILITY_MODEL"], {"value": "default", "source": "explicit", "old": "off", "new": HAIKU, "class": "free"})
        for with_record in (True, False):
            with self.subTest(purposes=True, record=with_record):
                if not with_record:
                    self.drop_record()
                mark = self.mark()
                again = self.ok(acct_two=True)
                self.assert_kept(again, {"HAMH_UTILITY_PURPOSES": "run-name"}, "kept" if with_record else "live", mark)
                self.assertEqual(self.settings(again)["HAMH_UTILITY_MODEL"]["source"], "default", "a live value equal to today's default is the default")
                self.assertNotIn("set.HAMH_UTILITY_MODEL", self.record())
        both = self.ok(acct_two=True, HAMH_UTILITY_PURPOSES="run-name, run-tags")
        self.assertEqual(self.settings(both)["HAMH_UTILITY_PURPOSES"], {"value": "run-name,run-tags", "source": "explicit", "old": "run-name",
                                                                        "new": "run-name,run-tags", "class": "free"})
        mark = self.mark()
        back = self.ok(acct_two=True, HAMH_UTILITY_PURPOSES="default")
        self.assertEqual(self.settings(back)["HAMH_UTILITY_PURPOSES"]["source"], "explicit")
        self.assertTrue(all("HAMH_UTILITY_PURPOSES" not in c for c in self.cdk_calls(mark)))

    def test_b812_update_keeps_the_research_provider_and_alarm_settings(self) -> None:
        topic = "arn:aws:sns:us-west-2:111111111111:hamh-alarms"
        kept = dict(HAMH_RESEARCH_PROVIDER="tavily", HAMH_RESEARCH_ALARM_TOPIC_ARN=topic, HAMH_TAVILY_KEY_MAX_AGE_DAYS="60")
        self.ok(**kept)
        for with_record in (True, False):
            with self.subTest(record=with_record):
                if not with_record:
                    self.drop_record()
                mark = self.mark()
                again = self.ok()
                self.assert_kept(again, kept, "kept" if with_record else "live", mark)
                age = self.resource("control", "Alarmkeyage")["Properties"]
                self.assertEqual((age["Threshold"], age["AlarmActions"]), (60, [topic]))
        off = self.ok(HAMH_TAVILY_ALARMS="off")
        self.assertEqual(self.settings(off)["HAMH_TAVILY_ALARMS"], {"value": "off", "source": "explicit", "old": "on", "new": "off", "class": "remove"})
        self.assertIsNone(self.resource("control", "Alarmkeyage"))
        for with_record in (True, False):
            with self.subTest(alarms="off", record=with_record):
                if not with_record:
                    self.drop_record()
                mark = self.mark()
                self.assert_kept(self.ok(), {"HAMH_RESEARCH_PROVIDER": "tavily", "HAMH_TAVILY_ALARMS": "off"}, "kept" if with_record else "live", mark)
                self.assertIsNone(self.resource("control", "Alarmkeyage"), "the alarms stay away")
        inside = self.ok(HAMH_RESEARCH_PROVIDER="agentcore")
        self.assertEqual(self.settings(inside)["HAMH_RESEARCH_PROVIDER"], {"value": "agentcore", "source": "explicit", "old": "tavily",
                                                                           "new": "agentcore", "class": "free"})
        for with_record in (True, False):
            with self.subTest(provider="agentcore", record=with_record):
                if not with_record:
                    self.drop_record()
                mark = self.mark()
                self.assert_kept(self.ok(), {"HAMH_RESEARCH_PROVIDER": "agentcore"}, "kept" if with_record else "live", mark)
                self.assertEqual(self.resource("orchestration", "RunSession")["Properties"]["Environment"]["Variables"]["RESEARCH_PROVIDER"], "agentcore",
                                 "research queries stay inside AWS")
        mark = self.mark()
        back = self.ok(HAMH_RESEARCH_PROVIDER="default")
        self.assertEqual(self.settings(back)["HAMH_RESEARCH_PROVIDER"], {"value": "default", "source": "explicit", "old": "agentcore", "new": "auto",
                                                                         "class": "free"})
        self.assertTrue(all("HAMH_RESEARCH_PROVIDER" not in c for c in self.cdk_calls(mark)))

    def test_b812_update_keeps_the_log_retention(self) -> None:
        self.ok(HAMH_LOG_RETENTION_DAYS="365")
        for with_record in (True, False):
            with self.subTest(record=with_record):
                if not with_record:
                    self.drop_record()
                mark = self.mark()
                again = self.ok()
                self.assert_kept(again, {"HAMH_LOG_RETENTION_DAYS": "365"}, "kept" if with_record else "live", mark)
                for short in ("network", "data", "control", "orchestration", "delivery", "runtimes"):
                    self.assertEqual(self.resource(short, "Logs")["Properties"]["RetentionInDays"], 365, short)
        lower = self.ok(HAMH_LOG_RETENTION_DAYS="90")
        self.assertEqual(self.settings(lower)["HAMH_LOG_RETENTION_DAYS"], {"value": "90", "source": "explicit", "old": "365", "new": "90", "class": "decrease"})
        mark = self.mark()
        back = self.ok(HAMH_LOG_RETENTION_DAYS="default")
        self.assertEqual(self.settings(back)["HAMH_LOG_RETENTION_DAYS"], {"value": "default", "source": "explicit", "old": "90", "new": "30", "class": "decrease"})
        self.assertTrue(all("HAMH_LOG_RETENTION_DAYS" not in c for c in self.cdk_calls(mark)))
        self.assertNotIn("set.HAMH_LOG_RETENTION_DAYS", self.record())

    def test_b812_update_keeps_the_pr_check_time_and_compute(self) -> None:
        kept = dict(HAMH_PR_CHECK_MINUTES="120", HAMH_PR_CHECK_COMPUTE="large")
        self.ok(**kept)
        for with_record in (True, False):
            with self.subTest(record=with_record):
                if not with_record:
                    self.drop_record()
                mark = self.mark()
                self.assert_kept(self.ok(), kept, "kept" if with_record else "live", mark)
                project = self.resource("delivery", "PrCheck")["Properties"]
                self.assertEqual((project["TimeoutInMinutes"], project["Environment"]["ComputeType"]), (120, "BUILD_GENERAL1_LARGE"))
        change = self.ok(HAMH_PR_CHECK_MINUTES="240", HAMH_PR_CHECK_COMPUTE="XLarge")
        rows = self.settings(change)
        self.assertEqual(rows["HAMH_PR_CHECK_MINUTES"], {"value": "240", "source": "explicit", "old": "120", "new": "240", "class": "free"})
        self.assertEqual(rows["HAMH_PR_CHECK_COMPUTE"], {"value": "xlarge", "source": "explicit", "old": "large", "new": "xlarge", "class": "free"})
        mark = self.mark()
        back = self.ok(HAMH_PR_CHECK_MINUTES="default", HAMH_PR_CHECK_COMPUTE="default")
        self.assertEqual(self.settings(back)["HAMH_PR_CHECK_MINUTES"], {"value": "default", "source": "explicit", "old": "240", "new": "60", "class": "free"})
        self.assertTrue(all("HAMH_PR_CHECK_MINUTES" not in c and "HAMH_PR_CHECK_COMPUTE" not in c for c in self.cdk_calls(mark)))

    def test_b812_update_keeps_the_agentcore_storage_accounts(self) -> None:
        pin = "us-west-2=123456789012+210987654321"
        self.ok(HAMH_AGENTCORE_STORAGE_ACCOUNTS=pin)
        for with_record in (True, False):
            with self.subTest(record=with_record):
                if not with_record:
                    self.drop_record()
                mark = self.mark()
                self.assert_kept(self.ok(), {"HAMH_AGENTCORE_STORAGE_ACCOUNTS": pin}, "kept" if with_record else "live", mark)
                statement = self.resource("network", "S3Endpoint")["Properties"]["PolicyDocument"]["Statement"][1]
                self.assertEqual(statement["Condition"]["StringEquals"], {"aws:ResourceAccount": ["123456789012", "210987654321"]})
        change = self.ok(HAMH_AGENTCORE_STORAGE_ACCOUNTS="us-west-2=123456789012")
        self.assertEqual(self.settings(change)["HAMH_AGENTCORE_STORAGE_ACCOUNTS"], {"value": "us-west-2=123456789012", "source": "explicit", "old": pin,
                                                                                    "new": "us-west-2=123456789012", "class": "remove"})
        mark = self.mark()
        back = self.ok(HAMH_AGENTCORE_STORAGE_ACCOUNTS="default")
        self.assertEqual(self.settings(back)["HAMH_AGENTCORE_STORAGE_ACCOUNTS"], {"value": "default", "source": "explicit", "old": "us-west-2=123456789012",
                                                                                  "new": "none", "class": "remove"})
        self.assertTrue(all("HAMH_AGENTCORE_STORAGE_ACCOUNTS" not in c for c in self.cdk_calls(mark)))
        statement = self.resource("network", "S3Endpoint")["Properties"]["PolicyDocument"]["Statement"][1]
        self.assertEqual(statement["Condition"]["StringEquals"], {"aws:PrincipalServiceName": "bedrock-agentcore.amazonaws.com"})

    def test_b812_update_keeps_provider_silence_and_names_the_slice_deadline_reset(self) -> None:
        self.ok(HAMH_PROVIDER_SILENCE_S="1800")
        for with_record in (True, False):
            with self.subTest(record=with_record):
                self.ok(HAMH_SLICE_DEADLINE_S="300")
                self.assertNotIn("set.HAMH_SLICE_DEADLINE_S", self.record(), "a setting that is not kept is never recorded")
                if not with_record:
                    self.drop_record()
                mark = self.mark()
                again = self.ok()
                self.assert_kept(again, {"HAMH_PROVIDER_SILENCE_S": "1800"}, "kept" if with_record else "live", mark)
                # The slice deadline override returns to the full slice by design, and the deploy says so on its line.
                self.assertEqual(self.settings(again)["HAMH_SLICE_DEADLINE_S"], {"value": "default", "source": "default", "old": "300", "new": "none", "class": "free"})
                self.assertTrue(all("HAMH_SLICE_DEADLINE_S" not in c for c in self.cdk_calls(mark)))
                self.assertNotIn("HAMH_SLICE_DEADLINE_S", self.resource("orchestration", "RunSession")["Properties"]["Environment"]["Variables"])
        change = self.ok(HAMH_PROVIDER_SILENCE_S="900")
        self.assertEqual(self.settings(change)["HAMH_PROVIDER_SILENCE_S"], {"value": "900", "source": "explicit", "old": "1800", "new": "900", "class": "free"})
        mark = self.mark()
        back = self.ok(HAMH_PROVIDER_SILENCE_S="default")
        self.assertEqual(self.settings(back)["HAMH_PROVIDER_SILENCE_S"], {"value": "default", "source": "explicit", "old": "900", "new": "none", "class": "free"})
        self.assertTrue(all("HAMH_PROVIDER_SILENCE_S" not in c for c in self.cdk_calls(mark)))

    def test_b812_a_release_that_replaces_the_capacity_provider_stops_for_consent(self) -> None:
        self.ok(HAMH_COMPUTE_INSTANCES="on")
        name, deploys, rec = self.provider(), self.stack_deploys(), self.ssm()[SETTINGS]
        # A later release builds the provider from another shape; nobody asked for it.
        proc = self.deploy(FAKE_PROVIDER_NAME=PROVIDER_B)
        reason, wait, text = self.refused(proc)
        self.assertEqual((reason, wait), ("consent", "no"))
        for needle in (name, PROVIDER_B, "deletes the machines", "HAMH_INSTANCES_TYPES="):
            self.assertIn(needle, text)
        self.assertEqual(self.stack_deploys(), deploys, "no stack changed")
        self.assertEqual((self.provider(), self.ssm()[SETTINGS]), (name, rec), "the provider and the record are as they were")
        self.assertNotIn(LOCK, self.ssm())
        # An explicit value is the consent.
        ok = self.ok(FAKE_PROVIDER_NAME=PROVIDER_B, HAMH_INSTANCES_TYPES="default")
        self.assertTrue([ln for ln in ok.stdout.splitlines() if "REPLACES" in ln])
        self.assertEqual(self.provider(), PROVIDER_B)
        # A release whose default machine is another one replaces nothing either: the deploy keeps the machine.


class ConsentCase(SettingsBase):
    def test_b815_an_update_of_a_stage_on_the_old_default_machine_keeps_it(self) -> None:
        """The default machine list changed from m7g.4xlarge alone. A stage that took the old default has no line for the types in its record, and its
        capacity provider allows m7g.4xlarge. Its update replaces no machine on its own: the deploy keeps the machine (B833DefaultChangeCase holds the
        rest). A stage without a record keeps the machine it has; an exported value is the consent."""
        self.ok(HAMH_COMPUTE_INSTANCES="on", HAMH_INSTANCES_TYPES="m7g.4xlarge")
        self.edit_record(**{"set.HAMH_INSTANCES_TYPES": None})
        name = self.provider()
        # Keeping the machine: no replacement.
        kept = self.ok()
        self.assertFalse([ln for ln in kept.stdout.splitlines() if "REPLACES" in ln], kept.stdout)
        self.assertEqual(self.provider(), name)
        # A stage that an earlier release deployed (no record) keeps the machine it has as its own choice: no consent asked, no replacement.
        self.drop_record()
        mark = self.mark()
        self.assert_kept(self.ok(), {"HAMH_INSTANCES_TYPES": "m7g.4xlarge"}, "live", mark)
        self.assertEqual(self.provider(), name)
        # Taking the new default is an exported value: the consent. The provider is replaced.
        took = self.ok(HAMH_INSTANCES_TYPES="default")
        self.assertTrue([ln for ln in took.stdout.splitlines() if "REPLACES" in ln], took.stdout)
        self.assertNotEqual(self.provider(), name)

    def test_b833_a_lower_default_retention_keeps_the_stage_retention(self) -> None:
        """The stage keeps 365 days and its record has no line for them (the default of an earlier release, as far as the record goes): a default of 30 would delete
        the older events, so a plain update keeps 365 and names the export that takes 30."""
        self.ok(HAMH_LOG_RETENTION_DAYS="365")
        self.edit_record(**{"set.HAMH_LOG_RETENTION_DAYS": None})
        proc = self.ok()
        self.assertEqual(held(proc), {"HAMH_LOG_RETENTION_DAYS": {"value": "365", "default": "30", "class": "decrease", "sentence":
                                      "the stage keeps 365; this release's default is 30, and taking it deletes what is older than the new value: "
                                      "export HAMH_LOG_RETENTION_DAYS=default to take it"}})
        self.assertEqual(set(self.retentions().values()), {365})
        self.assertEqual(self.record()["set.HAMH_LOG_RETENTION_DAYS"], "365")
        self.ok(HAMH_LOG_RETENTION_DAYS="default")
        self.assertEqual(self.resource("data", "Logs")["Properties"]["RetentionInDays"], 30)

    def test_b833_a_removed_default_alarm_topic_or_storage_pin_is_kept_and_a_free_setting_is_taken(self) -> None:
        topic = "arn:aws:sns:us-west-2:111111111111:hamh-alarms"
        self.ok(HAMH_RESEARCH_ALARM_TOPIC_ARN=topic, HAMH_AGENTCORE_STORAGE_ACCOUNTS="us-west-2=123456789012", HAMH_PR_CHECK_MINUTES="120")
        self.edit_record(**{"set.HAMH_RESEARCH_ALARM_TOPIC_ARN": None, "set.HAMH_AGENTCORE_STORAGE_ACCOUNTS": None, "set.HAMH_PR_CHECK_MINUTES": None})
        proc = self.ok()
        got = held(proc)
        self.assertEqual(sorted(got), ["HAMH_AGENTCORE_STORAGE_ACCOUNTS", "HAMH_RESEARCH_ALARM_TOPIC_ARN"])
        self.assertEqual(got["HAMH_RESEARCH_ALARM_TOPIC_ARN"]["sentence"], f"the stage keeps {topic}; this release's default is none, and taking it removes what the "
                                                                         "setting holds now: export HAMH_RESEARCH_ALARM_TOPIC_ARN=default to take it")
        self.assertEqual(got["HAMH_AGENTCORE_STORAGE_ACCOUNTS"]["value"], "us-west-2=123456789012")
        # a free setting returns to its default without a question, as before
        self.assertEqual(self.settings(proc)["HAMH_PR_CHECK_MINUTES"], {"value": "default", "source": "default", "old": "120", "new": "60", "class": "free"})
        rec = self.record()
        self.assertEqual((rec["set.HAMH_RESEARCH_ALARM_TOPIC_ARN"], rec["set.HAMH_AGENTCORE_STORAGE_ACCOUNTS"]), (topic, "us-west-2=123456789012"))
        self.assertNotIn("set.HAMH_PR_CHECK_MINUTES", rec)

    def test_b812_the_consent_refusal_names_what_each_export_gives(self) -> None:
        # Addendum 4, A4.12 (finding F15). The record holds 7 days; a deploy of an earlier release, which reads no record, put the live stage back to 30,
        # which is today's default as well. Before, the line offered `default` "to accept the new value" and 30 "to keep it": both gave 30, and the 7 days
        # of the record were offered nowhere.
        self.ok()
        self.edit_record(**{"set.HAMH_LOG_RETENTION_DAYS": "7"})
        deploys = self.stack_deploys()
        reason, _, text = self.refused(self.deploy())
        self.assertEqual(reason, "consent")
        self.assertIn("HAMH_LOG_RETENTION_DAYS goes from 30 to 7 (decrease)", text)
        self.assertTrue(text.endswith("export HAMH_LOG_RETENTION_DAYS=7 to keep what you chose, or HAMH_LOG_RETENTION_DAYS=30 to keep what the stage has now"), text)
        self.assertNotIn("=default", text, "default gives 30, which is what the stage has now: it is not named twice")
        self.assertNotIn("to accept the new value", text)
        self.assertEqual(self.stack_deploys(), deploys)
        # Each export gives what the line says.
        self.ok(HAMH_LOG_RETENTION_DAYS="30")
        self.assertEqual(set(self.retentions().values()), {30})
        self.edit_record(**{"set.HAMH_LOG_RETENTION_DAYS": "7"})
        chosen = self.ok(HAMH_LOG_RETENTION_DAYS="7")
        self.assertEqual(self.settings(chosen)["HAMH_LOG_RETENTION_DAYS"], {"value": "7", "source": "explicit", "old": "30", "new": "7", "class": "decrease"})
        self.assertEqual(set(self.retentions().values()), {7})
        # Three different values: the record's 90, the live 365 and the default 30 are all named.
        self.ok(HAMH_LOG_RETENTION_DAYS="365")
        self.edit_record(**{"set.HAMH_LOG_RETENTION_DAYS": "90"})
        reason, _, text = self.refused(self.deploy("--print-settings"))
        self.assertEqual(reason, "consent")
        self.assertIn("HAMH_LOG_RETENTION_DAYS goes from 365 to 90 (decrease)", text)
        self.assertTrue(text.endswith("export HAMH_LOG_RETENTION_DAYS=90 to keep what you chose, HAMH_LOG_RETENTION_DAYS=365 to keep what the stage has now, "
                                      "or HAMH_LOG_RETENTION_DAYS=default for 30"), text)
        # A record value that is the default: `default` would give the same as the record's value and is left out.
        self.edit_record(**{"set.HAMH_LOG_RETENTION_DAYS": "30"})
        _, _, text = self.refused(self.deploy("--print-settings"))
        self.assertTrue(text.endswith("export HAMH_LOG_RETENTION_DAYS=30 to keep what you chose, or HAMH_LOG_RETENTION_DAYS=365 to keep what the stage has now"), text)


class LogRetentionReaderCase(SettingsBase):
    """Addendum 4, A4.2 (finding F1): the live reader of HAMH_LOG_RETENTION_DAYS reads the log groups whose retention the stage sets, and a stage that has a
    record is never refused for a live value the deploy cannot tell."""

    def test_b812_the_log_group_a_cdk_construct_makes_for_itself_does_not_decide_the_stage_retention(self) -> None:
        # Both measured stages: every group the stage names keeps 30 days, the construct's group of the network and runtimes templates 731, and no record.
        self.ok()
        self.drop_record()
        for short in ("network", "runtimes"):
            group = self.resource(short, CONSTRUCT_GROUP)["Properties"]
            self.assertEqual(group["RetentionInDays"], 731, short)
            self.assertIsInstance(group["LogGroupName"], dict, "CDK names the construct's group by an intrinsic function")
        self.assertEqual(set(self.retentions().values()), {30})
        default = {"value": "default", "source": "default", "old": None, "new": None, "class": None}
        printed = self.deploy("--print-settings")
        self.assertEqual(printed.returncode, 0, printed.stdout + printed.stderr)
        self.assertNotIn("DEPLOY-REFUSED", printed.stdout)
        self.assertEqual(self.settings(printed)["HAMH_LOG_RETENTION_DAYS"], default)
        mark = self.mark()
        proc = self.ok()
        self.assertNotIn("DEPLOY-REFUSED", proc.stdout)
        self.assertEqual(self.settings(proc)["HAMH_LOG_RETENTION_DAYS"], default)
        self.assertTrue(all("HAMH_LOG_RETENTION_DAYS" not in c for c in self.cdk_calls(mark)))
        self.assertNotIn("set.HAMH_LOG_RETENTION_DAYS", self.record())
        self.assertEqual(set(self.retentions().values()), {30})
        # A stage whose own groups keep 365 days: 365 is the live value, whatever the construct's group holds.
        self.ok(HAMH_LOG_RETENTION_DAYS="365")
        self.drop_record()
        mark = self.mark()
        self.assert_kept(self.ok(), {"HAMH_LOG_RETENTION_DAYS": "365"}, "live", mark)
        self.assertEqual(set(self.retentions().values()), {365})
        self.assertEqual(self.resource("network", CONSTRUCT_GROUP)["Properties"]["RetentionInDays"], 731)
        # The stage's own groups at two values are no single answer for a stage without a record; the construct's days are not among them.
        self.drop_record()
        self.edit_stack("data", lambda body: body["template"]["Resources"]["Logs"]["Properties"].update(RetentionInDays=90))
        deploys = self.stack_deploys()
        reason, wait, text = self.refused(self.deploy())
        self.assertEqual((reason, wait), ("settings-read", "no"))
        self.assertIn("(90, 365)", text)
        self.assertNotIn("731", text)
        self.assertIn("export HAMH_LOG_RETENTION_DAYS=<value>, or HAMH_LOG_RETENTION_DAYS=default", text)
        self.assertEqual(self.stack_deploys(), deploys)

    def test_b812_a_stage_with_a_record_is_not_refused_for_a_live_value_the_deploy_cannot_tell(self) -> None:
        # A deploy that raised the retention and failed half way: the stacks before the failure keep 365 days, the others 30, and the record holds the intent.
        self.ok()
        failed = self.deploy(HAMH_LOG_RETENTION_DAYS="365", FAKE_FAIL_STACK="hamh-direct-control")
        self.assertEqual(failed.returncode, 1, failed.stdout + failed.stderr)
        self.assertEqual((self.record()["status"], self.record()["set.HAMH_LOG_RETENTION_DAYS"]), ("deploying", "365"))
        self.assertEqual(set(self.retentions().values()), {30, 365})
        # The rerun with nothing exported applies the record's value: the record decides, the live stage is only compared with it.
        mark = self.mark()
        again = self.ok()
        self.assertNotIn("DEPLOY-REFUSED", again.stdout)
        kept = {"value": "365", "source": "kept", "old": None, "new": None, "class": None}
        self.assertEqual(self.settings(again)["HAMH_LOG_RETENTION_DAYS"], kept)
        note = [ln for ln in again.stdout.splitlines() if ln.startswith("deploy: the live value of HAMH_LOG_RETENTION_DAYS is not known")]
        self.assertEqual(len(note), 1, again.stdout)
        self.assertIn("(30, 365)", note[0])
        self.assertIn("the settings record decides", note[0])
        self.assertTrue(all(c.get("HAMH_LOG_RETENTION_DAYS") == "365" for c in self.cdk_calls(mark)))
        self.assertEqual(set(self.retentions().values()), {365})
        self.assertEqual((self.record()["status"], self.record()["set.HAMH_LOG_RETENTION_DAYS"]), ("deployed", "365"))
        # A template that cannot be read is no refusal either while the record holds the value (the resolution alone: a whole deploy reads every template
        # again for its digests).
        proc = self.deploy("--print-settings", FAKE_GET_TEMPLATE_FAILS="hamh-direct-data")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.settings(proc)["HAMH_LOG_RETENTION_DAYS"], kept)
        self.assertIn("deploy: the live value of HAMH_LOG_RETENTION_DAYS is not known", proc.stdout)
        # A record that holds no line for the setting decides too: the default, with no refusal. Without a record the same stage is refused.
        self.edit_record(**{"set.HAMH_LOG_RETENTION_DAYS": None})
        proc = self.deploy("--print-settings", FAKE_GET_TEMPLATE_FAILS="hamh-direct-data")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.settings(proc)["HAMH_LOG_RETENTION_DAYS"], {"value": "default", "source": "default", "old": None, "new": None, "class": None})
        self.drop_record()
        reason, _, _ = self.refused(self.deploy("--print-settings", FAKE_GET_TEMPLATE_FAILS="hamh-direct-data"))
        self.assertEqual(reason, "settings-read")


# ----: a new release default that would need consent ----------------------------------------------------------------------------------

class B833DefaultChangeCase(SettingsBase):
    """Release 5.15 changed the default of HAMH_INSTANCES_TYPES from m7g.4xlarge to m7g.4xlarge,m6g.4xlarge. A
    stage that 5.14 deployed with the Instances part followed the old default: its record has no line for the types and its capacity provider allows m7g.4xlarge.
    A plain update was refused for consent after the runner was installed and the build had run. The update keeps every setting the stage has: a new default that
    would need consent (replace, a lower number, a removal) is not taken; the deploy keeps the stage's value, says so in its DEPLOY-SETTING line with the export
    that takes the default, and the record then holds the value. A default that needs no consent is taken, a new stage gets the new default, and an exported
    value (the word default too) is the consent, as before."""

    OLD = "m7g.4xlarge"

    def new_default(self) -> str:
        import deploy  # noqa: PLC0415
        return deploy.SETTING_DEFAULTS["HAMH_INSTANCES_TYPES"]

    def sentence(self) -> str:
        return (f"the stage keeps {self.OLD}; this release's default is {self.new_default()}, and taking it replaces the machines of the Instances part: "
                "export HAMH_INSTANCES_TYPES=default to take it")

    def stage_on_the_old_default(self, **more: str) -> str:
        """A stage as 5.14 left it: the Instances part on the 5.14 default machine (its provider allows m7g.4xlarge) and a record with no line for the types."""
        self.ok(HAMH_COMPUTE_INSTANCES="on", HAMH_INSTANCES_TYPES=self.OLD, **more)
        self.edit_record(**{"set.HAMH_INSTANCES_TYPES": None, "version": "5.14", **{f"set.{k}": None for k in more}})
        self.assertEqual(self.record().get("set.HAMH_COMPUTE_INSTANCES"), "on")
        return self.provider()

    def replaced(self, proc: subprocess.CompletedProcess) -> bool:
        return bool([ln for ln in proc.stdout.splitlines() if "REPLACES" in ln])

    def test_b833_a_plain_update_keeps_the_machine_of_a_stage_on_the_old_default_and_replaces_nothing(self) -> None:
        name = self.stage_on_the_old_default()
        self.assertNotEqual(self.OLD, self.new_default(), "TRIGGER PRESENT: the release's default differs from the machine the stage runs")
        # The resolution alone shows the same line, exits 0 and writes nothing.
        before = PrintSettingsCase.snapshot(self)
        printed = self.deploy("--print-settings")
        self.assertEqual(printed.returncode, 0, printed.stdout + printed.stderr)
        self.assertEqual(held(printed)["HAMH_INSTANCES_TYPES"]["sentence"], self.sentence())
        self.assertEqual(PrintSettingsCase.snapshot(self), before)
        # The plain update deploys and keeps the machine.
        mark, deploys = self.mark(), self.stack_deploys()
        proc = self.deploy()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertNotIn("DEPLOY-REFUSED", proc.stdout)
        self.assertEqual(held(proc), {"HAMH_INSTANCES_TYPES": {"value": self.OLD, "default": self.new_default(), "class": "replace", "sentence": self.sentence()}})
        self.assertEqual(self.settings(proc)["HAMH_INSTANCES_TYPES"], {"value": self.OLD, "source": "kept", "old": None, "new": None, "class": None})
        self.assertGreater(self.stack_deploys(), deploys, "the stage got the new revision")
        self.assertTrue(all(c.get("HAMH_INSTANCES_TYPES") == self.OLD for c in self.cdk_calls(mark)), "every cdk call builds the machine the stage has")
        self.assertEqual(self.provider(), name, "the capacity provider keeps its name: CloudFormation replaces nothing")
        self.assertFalse(self.replaced(proc), proc.stdout)

    def test_b833_the_record_then_holds_the_kept_machine_and_the_next_update_keeps_it(self) -> None:
        name = self.stage_on_the_old_default()
        self.ok()
        rec = self.record()
        self.assertEqual((rec["status"], rec["set.HAMH_INSTANCES_TYPES"]), ("deployed", self.OLD), "the kept value is the stage's own value now")
        mark = self.mark()
        again = self.ok()
        self.assertEqual(held(again), {}, "the second update has nothing new to say")
        self.assert_kept(again, {"HAMH_INSTANCES_TYPES": self.OLD}, "kept", mark)
        self.assertEqual(self.provider(), name)

    def test_b833_after_a_kept_update_an_exported_default_takes_the_new_machine_list(self) -> None:
        name = self.stage_on_the_old_default()
        self.ok()
        mark = self.mark()
        took = self.ok(HAMH_INSTANCES_TYPES="default")
        self.assertEqual(self.settings(took)["HAMH_INSTANCES_TYPES"], {"value": "default", "source": "explicit", "old": self.OLD, "new": self.new_default(),
                                                                        "class": "replace"})
        self.assertEqual(held(took), {})
        self.assertTrue(self.replaced(took), took.stdout)
        self.assertNotEqual(self.provider(), name)
        self.assertTrue(all("HAMH_INSTANCES_TYPES" not in c for c in self.cdk_calls(mark)))
        self.assertNotIn("set.HAMH_INSTANCES_TYPES", self.record())
        # The stage follows the default again: a plain update has nothing to keep.
        again = self.ok()
        self.assertEqual(self.settings(again)["HAMH_INSTANCES_TYPES"], {"value": "default", "source": "default", "old": None, "new": None, "class": None})
        # An exported machine is the consent as well.
        other = self.ok(HAMH_INSTANCES_TYPES="m7g.8xlarge")
        self.assertEqual(self.settings(other)["HAMH_INSTANCES_TYPES"]["source"], "explicit")
        self.assertEqual(self.record()["set.HAMH_INSTANCES_TYPES"], "m7g.8xlarge")

    def test_b833_one_update_keeps_what_needs_consent_and_takes_the_defaults_that_do_not(self) -> None:
        # The machine (replace), a retention under today's default (decrease class, but 14 -> 30 deletes nothing) and a free setting all follow the record's default.
        name = self.stage_on_the_old_default(HAMH_LOG_RETENTION_DAYS="14", HAMH_PR_CHECK_MINUTES="120")
        proc = self.ok()
        self.assertEqual(sorted(held(proc)), ["HAMH_INSTANCES_TYPES"])
        rows = self.settings(proc)
        self.assertEqual(rows["HAMH_LOG_RETENTION_DAYS"], {"value": "default", "source": "default", "old": "14", "new": "30", "class": "decrease"})
        self.assertEqual(rows["HAMH_PR_CHECK_MINUTES"], {"value": "default", "source": "default", "old": "120", "new": "60", "class": "free"})
        self.assertEqual(set(self.retentions().values()), {30})
        self.assertEqual(self.provider(), name)
        rec = self.record()
        self.assertNotIn("set.HAMH_LOG_RETENTION_DAYS", rec)
        self.assertNotIn("set.HAMH_PR_CHECK_MINUTES", rec)

    def test_b833_a_new_stage_and_a_stage_without_the_part_get_the_new_default(self) -> None:
        # A new stage: the default, no line in the record, and the provider allows every type of it.
        mark = self.mark()
        first = self.ok(HAMH_COMPUTE_INSTANCES="on")
        self.assertEqual(self.settings(first)["HAMH_INSTANCES_TYPES"], {"value": "default", "source": "default", "old": None, "new": None, "class": None})
        self.assertEqual(held(first), {})
        self.assertTrue(all("HAMH_INSTANCES_TYPES" not in c for c in self.cdk_calls(mark)))
        self.assertNotIn("set.HAMH_INSTANCES_TYPES", self.record())
        types = self.template("runtimes")["Resources"]["InstancesCapacityProvider"]["Properties"]["ComputeConfiguration"]["Ec2Configuration"][
            "LaunchTemplateSource"]["LaunchParameters"]["InstanceRequirements"]["AllowedInstanceTypes"]
        self.assertEqual(types, self.new_default().split(","))
        # A 5.14 stage without the Instances part has no machine to keep: its update keeps nothing, and the part it adds later gets today's default.
        self.setUp()
        self.ok()
        self.edit_record(version="5.14")
        plain = self.ok()
        self.assertEqual(held(plain), {})
        self.assertNotIn("set.HAMH_INSTANCES_TYPES", self.record())
        mark = self.mark()
        on = self.ok(HAMH_COMPUTE_INSTANCES="on")
        self.assertEqual(held(on), {})
        self.assertTrue(all("HAMH_INSTANCES_TYPES" not in c for c in self.cdk_calls(mark)))

    def test_b833_a_kept_value_of_an_unfinished_deploy_is_consent_only_when_that_deploy_exported_it(self) -> None:
        """A record whose deploy did not finish (status=deploying) carries the names that deploy got from its environment (`explicit=`). Its kept value is the
        earlier explicit intent only for those names. Before, any kept value of such a record was consent, so a value the deploy kept by itself would have
        let a later release replace the capacity provider on a retry."""
        name = self.stage_on_the_old_default()
        self.ok()
        # A deploy that kept the machine and then failed; the next try runs a release whose provider has another shape.
        self.edit_record(status="deploying", explicit="")
        deploys = self.stack_deploys()
        reason, wait, text = self.refused(self.deploy(FAKE_PROVIDER_NAME=PROVIDER_B))
        self.assertEqual((reason, wait), ("consent", "no"))
        self.assertIn(PROVIDER_B, text)
        self.assertEqual((self.stack_deploys(), self.provider()), (deploys, name), "nothing changed")
        # The same record whose deploy exported the machine: that was the consent, and the retry carries it.
        self.edit_record(explicit="HAMH_INSTANCES_TYPES")
        retry = self.ok(FAKE_PROVIDER_NAME=PROVIDER_B)
        self.assertTrue(self.replaced(retry), retry.stdout)
        self.assertEqual(self.provider(), PROVIDER_B)

    def test_b833_an_exported_default_of_an_unfinished_deploy_is_taken_by_the_retry(self) -> None:
        """A deploy that took the new default (exported `default`, which leaves no set. line) and failed before the runtimes stack: the retry with nothing exported
        carries that intent. It keeps no old machine, and its record names the setting it was given."""
        name = self.stage_on_the_old_default()
        failed = self.deploy(HAMH_INSTANCES_TYPES="default", FAKE_FAIL_STACK="hamh-direct-runtimes")
        self.assertEqual(failed.returncode, 1, failed.stdout + failed.stderr)
        rec = self.record()
        self.assertEqual(rec["status"], "deploying")
        self.assertNotIn("set.HAMH_INSTANCES_TYPES", rec)
        self.assertIn("HAMH_INSTANCES_TYPES", rec.get("explicit", "").split(","), "the record names what the deploy was given")
        self.assertEqual(self.provider(), name, "TRIGGER PRESENT: the provider was not replaced yet")
        retry = self.ok()
        self.assertEqual(held(retry), {}, "the retry does not keep the machine the user asked to replace")
        self.assertTrue(self.replaced(retry), retry.stdout)
        self.assertNotEqual(self.provider(), name)
        self.assertEqual(self.record()["status"], "deployed")

    def test_b833_a_retry_of_a_record_without_the_explicit_line_applies_the_value_it_holds(self) -> None:
        """Review finding 1a: a release before the `explicit=` line (5.14, 5.15) wrote the record of an update that exported HAMH_INSTANCES_TYPES=m7g.8xlarge and
        failed before the runtimes stack: status=deploying and set.HAMH_INSTANCES_TYPES=m7g.8xlarge. The retry with nothing exported keeps the rule of that release
        (C2: a rerun after a failure re-applies the newer intent): each value such a record holds is the consent, and the provider is replaced."""
        self.ok(HAMH_COMPUTE_INSTANCES="on", HAMH_INSTANCES_TYPES=self.OLD)
        name = self.provider()
        failed = self.deploy(HAMH_INSTANCES_TYPES="m7g.8xlarge", FAKE_FAIL_STACK="hamh-direct-runtimes")
        self.assertEqual(failed.returncode, 1, failed.stdout + failed.stderr)
        self.edit_record(explicit=None, version="5.15")
        rec = self.record()
        self.assertEqual((rec["status"], rec["set.HAMH_INSTANCES_TYPES"]), ("deploying", "m7g.8xlarge"))
        self.assertNotIn("explicit", rec, "TRIGGER PRESENT: the record is one a release before the explicit= line wrote")
        self.assertEqual(self.provider(), name, "the provider was not replaced yet")
        retry = self.deploy()
        self.assertEqual(retry.returncode, 0, retry.stdout + retry.stderr)
        self.assertNotIn("DEPLOY-REFUSED", retry.stdout)
        self.assertEqual(self.settings(retry)["HAMH_INSTANCES_TYPES"], {"value": "m7g.8xlarge", "source": "kept", "old": self.OLD, "new": "m7g.8xlarge",
                                                                         "class": "replace"})
        self.assertTrue(self.replaced(retry), retry.stdout)
        self.assertNotEqual(self.provider(), name)
        rec = self.record()
        self.assertEqual((rec["status"], rec["set.HAMH_INSTANCES_TYPES"]), ("deployed", "m7g.8xlarge"))

    def test_b833_a_retry_of_a_record_without_the_explicit_line_keeps_no_value_by_itself(self) -> None:
        """Review finding 1b: the stage chose m7g.8xlarge; an update of such a release exported HAMH_INSTANCES_TYPES=default and failed before the runtimes stack,
        so its record has neither a line for the types nor an `explicit=` line. It cannot say whether that update was given `default`, so the retry keeps no value
        by itself: it asks for consent before any stack change, as that release did, says why, and leaves the record as it was. An export answers it. A 5.14 update
        that stopped part way on a stage that follows the default reads the same and is asked the same."""
        self.ok(HAMH_COMPUTE_INSTANCES="on", HAMH_INSTANCES_TYPES="m7g.8xlarge")
        name = self.provider()
        failed = self.deploy(HAMH_INSTANCES_TYPES="default", FAKE_FAIL_STACK="hamh-direct-runtimes")
        self.assertEqual(failed.returncode, 1, failed.stdout + failed.stderr)
        self.edit_record(explicit=None, version="5.15")
        self.assertNotIn("set.HAMH_INSTANCES_TYPES", self.record())
        self.assertEqual(self.provider(), name, "TRIGGER PRESENT: the provider was not replaced yet")
        rec, deploys = self.ssm()[SETTINGS], self.stack_deploys()
        retry = self.deploy()
        reason, wait, text = self.refused(retry)
        self.assertEqual((reason, wait), ("consent", "no"))
        self.assertIn(f"HAMH_INSTANCES_TYPES goes from m7g.8xlarge to {self.new_default()} (replace), and nothing you set asks for it.", text)
        self.assertTrue(text.endswith(f"export HAMH_INSTANCES_TYPES=m7g.8xlarge to keep what the stage has now, or HAMH_INSTANCES_TYPES=default for "
                                      f"{self.new_default()}"), text)
        self.assertEqual(held(retry), {}, "no value is kept by itself")
        note = [ln for ln in retry.stdout.splitlines() if ln.startswith("deploy: HAMH_INSTANCES_TYPES: the settings record is of a deploy that did not finish")]
        self.assertEqual(len(note), 1, retry.stdout)
        self.assertIn("does not say whether that deploy was given HAMH_INSTANCES_TYPES=default", note[0])
        self.assertEqual((self.stack_deploys(), self.provider(), self.ssm()[SETTINGS]), (deploys, name, rec), "nothing changed")
        # The export of the earlier update answers it: the default replaces the provider.
        took = self.ok(HAMH_INSTANCES_TYPES="default")
        self.assertTrue(self.replaced(took), took.stdout)
        # A 5.14 update that stopped part way, on a stage that follows the default.
        self.setUp()
        name = self.stage_on_the_old_default()
        self.edit_record(status="deploying", explicit=None)
        reason, _, text = self.refused(self.deploy())
        self.assertEqual(reason, "consent")
        self.assertTrue(text.endswith(f"export HAMH_INSTANCES_TYPES={self.OLD} to keep what the stage has now, or HAMH_INSTANCES_TYPES=default for "
                                      f"{self.new_default()}"), text)
        kept = self.ok(HAMH_INSTANCES_TYPES=self.OLD)
        self.assertFalse(self.replaced(kept), kept.stdout)
        self.assertEqual(self.provider(), name)

    def test_b833_the_defaults_file_holds_the_deploy_defaults_one_row_each(self) -> None:
        import deploy  # noqa: PLC0415
        path = REPO / "scripts" / "lib" / "stage-defaults.tsv"
        rows = [ln for ln in path.read_text(encoding="utf-8").splitlines() if ln.strip() and not ln.startswith("#")]
        names = [ln.split("|", 1)[0] for ln in rows]
        self.assertEqual(len(names), len(set(names)), "one row per setting")
        self.assertEqual(dict(ln.split("|", 1) for ln in rows), deploy.SETTING_DEFAULTS, "the deploy reads its defaults from the file")
        registry = {r["name"]: r for r in registry_rows()}
        self.assertEqual(set(names), set(registry) - set(deploy.ROLE_DEFAULTS) - {"HAMH_UTILITY_MODEL", "HAMH_UTILITY_PURPOSES"})
        for name, value in deploy.SETTING_DEFAULTS.items():
            self.assertTrue(value == "" or re.fullmatch(registry[name]["pattern"], value), f"{name}={value} is a value its setting takes")

    def test_b833_the_sentence_of_a_kept_value_is_true_for_every_setting_of_its_class(self) -> None:
        import deploy  # noqa: PLC0415
        # "replaces the machines of the Instances part": every setting of the class replace is one of the Instances machine settings.
        self.assertEqual({r["name"] for r in registry_rows() if r["change"] == "replace"}, set(deploy.INSTANCES_SETTINGS))
        self.assertEqual(set(deploy.HELD_COST), set(deploy.CONSENT_CLASSES))


# ---- the record, the refusals and the lock ----------------------------------------------------------------------------------------

class RecordCase(SettingsBase):
    def test_b812_the_settings_record_carries_the_stage_and_only_the_users_choices(self) -> None:
        self.ok(HAMH_LOG_RETENTION_DAYS="365", HAMH_TOOLS_MODE=" ON ", HAMH_SLICE_DEADLINE_S="300", HAMH_PR_CHECK_COMPUTE="default")
        rec = self.record()
        sets = {k: v for k, v in rec.items() if k.startswith("set.")}
        # The user's choices, and the keep tag and the kind that the stage runs (A4.7: always recorded; the entry of these cases has role history).
        self.assertEqual(sets, {"set.HAMH_LOG_RETENTION_DAYS": "365", "set.HAMH_TOOLS_MODE": "on", "set.HAMH_KEEP_TAG": "off", "set.HAMH_STAGE_KIND": "test"})
        self.assertEqual(rec["schema"], "1")
        self.assertEqual(rec["status"], "deployed")
        self.assertEqual(rec["commit"], td.git(self.root, "rev-parse", "HEAD"))
        self.assertEqual(rec["tree"], self.ssm()["/hamh/direct/deployed-source-tree"])
        self.assertEqual(rec["version"], "5.13")
        self.assertEqual(rec["accounts"], self.ssm()["/hamh/direct/deployment-config-digest"])
        self.assertEqual(rec["operator"], "", "the entry names no operator")
        self.assertEqual(rec["azs"], "usw2-az1")
        self.assertRegex(rec["updated"], r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$")
        self.assertEqual(list(rec)[:9], ["schema", "status", "commit", "tree", "version", "accounts", "operator", "azs", "updated"])
        puts = self.puts(SETTINGS)
        self.assertEqual([p[p.index("--value") + 1].splitlines()[1] for p in puts], ["status=deploying", "status=deployed"])
        self.assertTrue(all("--overwrite" in p for p in puts))
        tags = [a for a in self.aws_calls() if a[:2] == ["ssm", "add-tags-to-resource"] and SETTINGS in a]
        self.assertEqual(len(tags), 2)
        self.assertTrue(all("Key=project,Value=hyperscale-agent-meta-harness" in t and "Key=stage,Value=direct" in t for t in tags))

    def test_b812_the_record_names_the_commit_the_user_asked_to_deploy(self) -> None:
        # Addendum 4, A4.1 (findings F5 and F17): the setup runner deploys from a source archive, in a repository with one commit of its own that no clone
        # of the user holds, and exports the commit the user asked for as HAMH_SOURCE_COMMIT. The record names that commit, so the next update can compare it.
        head = td.git(self.root, "rev-parse", "HEAD")
        asked = "0123456789abcdef0123456789abcdef01234567"
        proc = self.ok(HAMH_SOURCE_COMMIT=asked)
        self.assertEqual(self.record()["commit"], asked)
        commits = [dict(line.split("=", 1) for line in p[p.index("--value") + 1].splitlines())["commit"] for p in self.puts(SETTINGS)]
        self.assertEqual(commits, [asked, asked], "the deploying record and the deployed record")
        self.assertIn(f"deploy: source commit {asked} (HAMH_SOURCE_COMMIT); the tree deployed is at {head[:7]}", proc.stdout)
        (lock,) = self.puts(LOCK)
        self.assertIn(f"commit={asked}", lock[lock.index("--value") + 1].splitlines(), "the lock of a manual deploy names the same commit")
        # The deploy record keeps the commit of the tree that was built, as before.
        self.assertEqual(self.deploy_record()["codeSha"], head)
        self.assertIn(f"sha={head[:7]}", proc.stdout)
        # Hex digits in upper case are the same commit, written the way git prints it.
        self.ok(HAMH_SOURCE_COMMIT=asked.upper())
        self.assertEqual(self.record()["commit"], asked)
        # Anything that is no 40 hex digits names no commit: the record takes HEAD, as a manual deploy and the local route always do.
        for other in ("main", asked[:39], asked + "0", "g" * 40):
            with self.subTest(value=other):
                again = self.ok(HAMH_SOURCE_COMMIT=other)
                self.assertEqual(self.record()["commit"], head)
                self.assertNotIn("deploy: source commit", again.stdout)

    def test_b812_the_record_names_the_operator_of_the_account_entry(self) -> None:
        doc = json.loads(td.sf.ACCOUNTS.read_text())
        doc["accounts"][0]["operator"] = {"kind": "role", "name": "Admin"}
        accounts = self.tmp / "accounts-operator.json"
        accounts.write_text(json.dumps(doc))
        self.ok(HAMH_ACCOUNTS_FILE=str(accounts), FAKE_CALLER_ARN=f"arn:aws:sts::{td.ACCOUNT}:assumed-role/Admin/alice")
        rec = self.record()
        self.assertEqual(rec["operator"], f"arn:aws:iam::{td.ACCOUNT}:role/Admin")
        self.assertEqual(rec["accounts"], self.ssm()["/hamh/direct/deployment-config-digest"])
        self.assertNotEqual(rec["accounts"], "committed")

    def test_b812_the_deploying_record_comes_after_every_check_that_can_refuse_and_before_the_first_stack_change(self) -> None:
        self.ok()
        self.running()
        mark_aws, mark_node = len(self.aws_order()), self.mark()
        self.ok("--bump-worker-version")
        calls = self.aws_order()[mark_aws:]
        deploying = next(c["n"] for c in calls if c["args"][:2] == ["ssm", "put-parameter"] and SETTINGS in c["args"] and "status=deploying" in " ".join(c["args"]))
        reads = [c["n"] for c in calls if c["args"][:2] in (["cloudformation", "get-template"], ["stepfunctions", "list-executions"])
                 or (c["args"][:2] == ["ssm", "get-parameter"] and ("/cdk-bootstrap/hnb659fds/version" in c["args"] or SETTINGS in c["args"] or LOCK in c["args"]))]
        first_deploy = next(c["n"] for c in self.env_log()[mark_node:] if c["args"][:2] == ["cdk", "deploy"])
        last_deploy = [c["n"] for c in self.env_log()[mark_node:] if c["args"][:2] == ["cdk", "deploy"]][-1]
        deployed = next(c["n"] for c in calls if c["args"][:2] == ["ssm", "put-parameter"] and SETTINGS in c["args"] and "status=deployed" in " ".join(c["args"]))
        reads_before_change = [n for n in reads if n < first_deploy]
        self.assertTrue(reads_before_change)
        self.assertLess(max(reads_before_change), deploying, "every read that can refuse comes first")
        self.assertLess(deploying, first_deploy)
        self.assertLess(last_deploy, deployed)

    def aws_order(self) -> list[dict]:
        p = self.state / "aws-order.log"
        return [json.loads(x) for x in p.read_text().splitlines()] if p.exists() else []

    def test_b812_a_failure_after_the_first_stack_change_keeps_deploy_fail_and_the_deploying_record(self) -> None:
        proc = self.deploy(HAMH_LOG_RETENTION_DAYS="365", FAKE_FAIL_STACK="hamh-direct-data")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertEqual(self.last(proc), "DEPLOY-FAIL stack hamh-direct-data")
        self.assertNotIn("DEPLOY-REFUSED", proc.stdout)
        self.assertEqual(self.record()["status"], "deploying")
        self.assertEqual(self.record()["set.HAMH_LOG_RETENTION_DAYS"], "365", "a rerun after the failure applies the newer intent")
        self.assertNotIn(LOCK, self.ssm(), "the lock is released after a failure")
        rerun = self.ok()
        self.assertEqual(self.settings(rerun)["HAMH_LOG_RETENTION_DAYS"]["source"], "kept")
        self.assertEqual(self.record()["status"], "deployed")

    def test_b812_a_bad_explicit_value_is_a_usage_error_before_any_aws_call_and_default_is_taken_by_every_setting(self) -> None:
        for name, bad in (("HAMH_LOG_RETENTION_DAYS", "45"), ("HAMH_INSTANCES_TYPES", "m7i.4xlarge"), ("HAMH_RESEARCH_PROVIDER", "web"),
                          ("HAMH_SLICE_DEADLINE_S", "30")):
            with self.subTest(name=name):
                proc = self.deploy(**{name: bad})
                self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
                self.assertIn(name, proc.stderr)
                self.assertIn(row(name)["note"], proc.stderr)
                self.assertEqual(self.log("aws"), [])
        everything = {r["name"]: "default" for r in registry_rows()}
        mark = self.mark()
        proc = self.ok(**everything)
        rows = self.settings(proc)
        self.assertEqual(set(rows), set(everything))
        self.assertTrue(all(r["value"] == "default" and r["source"] == "explicit" for r in rows.values()), rows)
        for call in self.cdk_calls(mark):
            self.assertEqual({k for k in call if k in everything}, {"HAMH_TOOLS_MODE", "HAMH_COMPUTE_INSTANCES"})
            self.assertEqual((call["HAMH_TOOLS_MODE"], call["HAMH_COMPUTE_INSTANCES"]), ("off", "off"))
        self.assertEqual({k: v for k, v in self.record().items() if k.startswith("set.")}, {"set.HAMH_KEEP_TAG": "off", "set.HAMH_STAGE_KIND": "test"},
                         "no choice is recorded; the keep tag and the kind always are, with the words the role gave")

    def test_b812_an_exported_empty_value_is_unset_and_keeps_the_stage_value(self) -> None:
        # Addendum 2, A2.1: only the word `default` returns to the code default.
        self.ok(HAMH_LOG_RETENTION_DAYS="365", HAMH_TOOLS_MODE="on")
        mark = self.mark()
        again = self.ok(HAMH_LOG_RETENTION_DAYS="", HAMH_TOOLS_MODE="  ")
        self.assert_kept(again, {"HAMH_LOG_RETENTION_DAYS": "365", "HAMH_TOOLS_MODE": "on"}, "kept", mark)

    def test_b812_compute_instances_default_on_an_instances_stage_is_refused_like_off(self) -> None:
        # Addendum 2, A2.2: the stage teardown is the only removal of the Instances part.
        self.ok(HAMH_COMPUTE_INSTANCES="on")
        for word in ("off", "default"):
            with self.subTest(word=word):
                deploys = self.stack_deploys()
                reason, wait, text = self.refused(self.deploy(HAMH_COMPUTE_INSTANCES=word))
                self.assertEqual((reason, wait), ("instances-off", "no"))
                self.assertIn("HAMH_COMPUTE_INSTANCES=on", text)
                self.assertEqual(self.stack_deploys(), deploys)

    def test_b812_the_minimum_bootstrap_version_lives_in_one_file(self) -> None:
        # Addendum 2, A2.6: one committed integer line that deploy.py reads and setup's preflight compares with the account's bootstrap.
        path = REPO / "scripts" / "lib" / "bootstrap-min-version.txt"
        text = path.read_text(encoding="utf-8")
        self.assertRegex(text, r"^[1-9][0-9]*\n$")
        import deploy  # noqa: PLC0415
        self.assertEqual(deploy.MIN_BOOTSTRAP, int(text))
        self.assertEqual(deploy.read_min_bootstrap(path), int(text))
        bad = self.tmp / "bootstrap-min-version.txt"
        for body in ("", "6 7\n", "six\n", "0\n", "6\n7\n"):
            bad.write_text(body)
            with self.assertRaises(ValueError, msg=repr(body)):
                deploy.read_min_bootstrap(bad)
        proc = self.ok(acct_two=True, FAKE_BOOTSTRAP_VERSION="18")
        self.assertIn(f"deploy: bootstrap version 18 used unchanged (required {int(text)})", proc.stdout)

    def test_b812_the_previous_spelling_of_a_setting_is_explicit_too(self) -> None:
        mark = self.mark()
        proc = self.ok(CAH_LOG_RETENTION_DAYS="365")  # COMPAT-CAH: the earlier spelling counts as explicit
        self.assertEqual(self.settings(proc)["HAMH_LOG_RETENTION_DAYS"]["source"], "explicit")
        self.assertTrue(all(c.get("HAMH_LOG_RETENTION_DAYS") == "365" and "CAH_LOG_RETENTION_DAYS" not in c for c in self.cdk_calls(mark)))  # COMPAT-CAH


class PrintSettingsCase(SettingsBase):
    # The read calls the mode may make: the caller, the record, the templates, the outputs of the runtimes stack and the live runtime of the tool layer.
    READS = {("sts", "get-caller-identity"), ("ssm", "get-parameter"), ("cloudformation", "get-template"), ("cloudformation", "describe-stacks"),
             ("bedrock-agentcore-control", "get-agent-runtime-endpoint"), ("bedrock-agentcore-control", "get-agent-runtime")}

    def snapshot(self) -> dict:
        """Everything a deploy could write in the fake account and on this machine."""
        return {p.name: p.read_text() for p in sorted(self.state.iterdir()) if p.is_file() and p.name.startswith(("stack-", "runtime-", "ssm.json", "repo-", "image-",
                                                                                                                    "bootstrap-version"))} | {
            "records": sorted(p.name for p in self.records.glob("*.json")) if self.records.is_dir() else [], "cdk": self.mark(), "cargo": len(self.log("cargo")),
            "codebuild": len(self.log("codebuild"))}

    def print_settings(self, **env: str) -> tuple[subprocess.CompletedProcess, set]:
        log = self.tmp / f"print-calls-{len(list(self.tmp.glob('print-calls-*')))}.jsonl"
        proc = self.deploy("--print-settings", DEPLOY_AWS_CALL_LOG=str(log), **env)
        calls = {tuple(json.loads(line)["argv"][:2]) for line in log.read_text().splitlines()} if log.exists() else set()
        return proc, calls

    def test_b812_print_settings_resolves_and_writes_nothing(self) -> None:
        self.ok(HAMH_COMPUTE_INSTANCES="on", HAMH_LOG_RETENTION_DAYS="365", HAMH_TOOLS_MODE="on")
        self.drop_record()  # as the stages an earlier release deployed: no record yet
        (self.root / "README.md").write_text("a dirty tree does not matter to a read\n")
        before = self.snapshot()
        proc, calls = self.print_settings()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        rows = self.settings(proc)
        self.assertEqual(list(rows), [r["name"] for r in registry_rows()], "one line per setting, in the registry's order")
        for name, value in (("HAMH_COMPUTE_INSTANCES", "on"), ("HAMH_LOG_RETENTION_DAYS", "365"), ("HAMH_TOOLS_MODE", "on")):
            self.assertEqual(rows[name], {"value": value, "source": "live", "old": None, "new": None, "class": None}, name)
        self.assertEqual(rows["HAMH_PR_CHECK_MINUTES"]["source"], "default")
        self.assertFalse([ln for ln in proc.stdout.splitlines() if ln.startswith(("DEPLOY-PASS", "DEPLOY-FAIL", "DEPLOY-REFUSED"))], proc.stdout)
        self.assertTrue(calls, "the mode reads the stage")
        self.assertLessEqual(calls, self.READS, "read calls only")
        self.assertEqual(self.snapshot(), before, "no record, no lock, no stack call, no build, no deploy record")
        td.git(self.root, "checkout", "--", "README.md")
        # With a record the kept values; an explicit value is shown with its change.
        self.ok()
        before = self.snapshot()
        proc, calls = self.print_settings(HAMH_LOG_RETENTION_DAYS="90")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        rows = self.settings(proc)
        self.assertEqual(rows["HAMH_TOOLS_MODE"]["source"], "kept")
        self.assertEqual(rows["HAMH_LOG_RETENTION_DAYS"], {"value": "90", "source": "explicit", "old": "365", "new": "90", "class": "decrease"})
        self.assertLessEqual(calls, self.READS)
        self.assertEqual(self.snapshot(), before)
        # A reader that fails, and a change that needs consent, print the refusal and exit 3, still writing nothing.
        self.drop_record()
        before = self.snapshot()
        proc, calls = self.print_settings(FAKE_GET_TEMPLATE_FAILS="hamh-direct-data")
        reason, wait, text = self.refused(proc)
        self.assertEqual((reason, wait), ("settings-read", "no"))
        self.assertIn("export HAMH_LOG_RETENTION_DAYS=<value>, or HAMH_LOG_RETENTION_DAYS=default", text)
        self.assertEqual(self.snapshot(), before)
        self.ok()
        self.edit_record(**{"set.HAMH_LOG_RETENTION_DAYS": "90"})
        before = self.snapshot()
        proc, calls = self.print_settings()
        reason, wait, text = self.refused(proc)
        self.assertEqual((reason, wait), ("consent", "no"))
        self.assertTrue(text.endswith("export HAMH_LOG_RETENTION_DAYS=90 to keep what you chose, HAMH_LOG_RETENTION_DAYS=365 to keep what the stage has now, "
                                      "or HAMH_LOG_RETENTION_DAYS=default for 30"), text)
        self.assertLessEqual(calls, self.READS)
        self.assertEqual(self.snapshot(), before)
        # Explicit off on a stage with the Instances part is the deploy's refusal too; a bad value is a usage error before any call.
        reason, _, _ = self.refused(self.print_settings(HAMH_COMPUTE_INSTANCES="off", HAMH_LOG_RETENTION_DAYS="365")[0])
        self.assertEqual(reason, "instances-off")
        proc, calls = self.print_settings(HAMH_LOG_RETENTION_DAYS="45")
        self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
        self.assertEqual(calls, set())
        self.assertEqual(self.snapshot(), before)


class RefusalCase(SettingsBase):
    def assert_unchanged(self, proc: subprocess.CompletedProcess, reason: str, wait: str, deploys: int, record: str | None, *needles: str) -> str:
        got_reason, got_wait, text = self.refused(proc)
        self.assertEqual((got_reason, got_wait), (reason, wait), text)
        for needle in needles:
            self.assertIn(needle, text)
        self.assertEqual(self.stack_deploys(), deploys, "no stack changed")
        self.assertFalse([c for c in self.env_log() if c["args"][:2] == ["cdk", "bootstrap"]], "no bootstrap")
        self.assertEqual(self.ssm().get(SETTINGS), record, "the record is as it was")
        return text

    def test_b812_each_refusal_before_any_stack_change_prints_one_line_and_exits_3(self) -> None:
        cases = {
            "guard": (lambda: (self.ok(), self.running("01K5RUNA")), ("--bump-worker-version",), {}, "yes", ("01K5RUNA", "deploy again")),
            "instances-off": (lambda: self.ok(HAMH_COMPUTE_INSTANCES="on"), (), {"HAMH_COMPUTE_INSTANCES": "off"}, "no", ("HAMH_COMPUTE_INSTANCES=on",)),
            "owner": (lambda: self.seed_stack("network", "keepme"), (), {"HAMH_OWNER": "someone-else"}, "no", ("keepme", "someone-else")),
            "account": (lambda: None, (), {"FAKE_ACCOUNT": "999999999999"}, "no", ("999999999999",)),
            "azs": (lambda: None, (), {"FAKE_AZ_MAP": json.dumps({"us-west-2a": "usw2-az9"})}, "no", ("usw2-az9",)),
            "vpc-cidr": (lambda: None, (), {"FAKE_VPCS": json.dumps([{"VpcId": "vpc-other", "CidrBlock": "10.50.4.0/24", "Tags": []}])}, "no", ("vpc-other",)),
            "bootstrap": (lambda: None, (), {"FAKE_BOOTSTRAP_VERSION": "18", "FAKE_REQUIRED_BOOTSTRAP": "21"}, "no", ("18", "21")),
            "record-downgrade": (lambda: (self.ok(), self.edit_record(schema="2")), (), {}, "no", ("git pull",)),
            "settings-read": (lambda: (self.ok(HAMH_LOG_RETENTION_DAYS="365"), self.drop_record()), (), {"FAKE_GET_TEMPLATE_FAILS": "hamh-direct-data"}, "no",
                              ("export HAMH_LOG_RETENTION_DAYS=<value>, or HAMH_LOG_RETENTION_DAYS=default",)),
            "lock": (lambda: self.seed_lock(), (), {}, "yes", ("ci-host:4242", "route runner")),
        }
        for reason, (prepare, args, env, wait, needles) in cases.items():
            with self.subTest(reason=reason):
                self.setUp()
                prepare()
                deploys, record, lock = self.stack_deploys(), self.ssm().get(SETTINGS), self.ssm().get(LOCK)
                proc = self.deploy(*args, **env)
                self.assert_unchanged(proc, reason, wait, deploys, record, *needles)
                self.assertEqual(self.ssm().get(LOCK), lock, "a lock this deploy took is gone again; another holder's lock is left alone")

    def test_b812_no_text_of_the_deploy_calls_a_refusal_a_deploy_fail(self) -> None:
        # Addendum 4, A4.16 (finding F22): the checks of REFUSAL_STEPS end a deploy with DEPLOY-REFUSED and exit 3, and the texts say so. The one place
        # where such a step still prints DEPLOY-FAIL is the mode --check-guard, which keeps its two answers.
        import deploy as dp
        for rel in ("scripts/lib/deploy.py", "scripts/lib/AGENTS.md"):
            lines = (REPO / rel).read_text(encoding="utf-8").splitlines()
            wrong = [f"{rel}:{n}: {line.strip()[:160]}" for n, line in enumerate(lines, 1) for step in dp.REFUSAL_STEPS
                     if f"DEPLOY-FAIL {step}" in line and "--check-guard" not in line]
            self.assertEqual(wrong, [])
        # What the texts say is what happens: a guard refusal of a deploy, and the answer of --check-guard to the same runs.
        self.ok()
        self.running("01K5RUNA")
        reason, wait, _ = self.refused(self.deploy("--bump-worker-version"))
        self.assertEqual((reason, wait), ("guard", "yes"))
        check = self.deploy("--check-guard")
        self.assertEqual(check.returncode, 1, check.stdout + check.stderr)
        self.assertTrue(self.last(check).startswith("DEPLOY-FAIL guard 1 running run(s) on hamh-direct-run"), check.stdout)

    def test_b812_a_settings_record_that_cannot_be_read_refuses(self) -> None:
        self.ok()
        deploys, record = self.stack_deploys(), self.ssm().get(SETTINGS)
        self.assert_unchanged(self.deploy(FAKE_GET_PARAMETER_FAILS=SETTINGS), "settings-read", "no", deploys, record, SETTINGS)

    def test_b812_a_caller_that_is_not_the_operator_is_refused_by_name(self) -> None:
        doc = json.loads(td.sf.ACCOUNTS.read_text())
        doc["accounts"][0]["operator"] = {"kind": "role", "name": "Admin"}
        accounts = self.tmp / "accounts-operator.json"
        accounts.write_text(json.dumps(doc))
        proc = self.deploy(HAMH_ACCOUNTS_FILE=str(accounts), FAKE_CALLER_ARN=f"arn:aws:sts::{td.ACCOUNT}:assumed-role/Other/bob")
        self.assert_unchanged(proc, "caller", "no", 0, None, "role/Admin", "assumed-role/Other/bob")

    def test_b812_a_pointer_to_a_missing_fork_is_refused_before_any_stack_change(self) -> None:
        body = {"schema": 1, "sha256": "e" * 64, "seriesTree": td.SERIES_TREE, "upstreamCommit": td.UPSTREAM_COMMIT, "toolchainDigest": td.TOOLCHAIN_DIGEST}
        pointer = f"s3://hamh-direct-harness-{td.ACCOUNT}/{td.POINTER_KEY}"
        self.seed_s3(pointer, json.dumps(body))
        text = self.assert_unchanged(self.deploy("--fork-source", "build"), "fork-pointer", "no", 0, None, pointer)
        self.assertIn("delete", text.lower())

    def test_b812_a_build_failure_before_the_first_stack_change_keeps_deploy_fail(self) -> None:
        # Addendum 2, A2.3: the failure keeps its DEPLOY-FAIL line (the last line) and exit 1, and the line before it says the stage is unchanged.
        self.ok()
        record = self.ssm()[SETTINGS]
        (self.root / "crates/agentd/src/main.rs").write_text("fn main() { let _ = 3; }\n")
        td.git(self.root, "commit", "-q", "-am", "agentd change: the update builds a new worker image")
        for env, step in (({"FAKE_CARGO_RC": "1"}, "lambda-build"), ({"FAKE_PNPM_RC": "1"}, "infra-install"),
                          ({"FAKE_CODEBUILD_FAIL_SPEC": "images/codebuild/worker-python.yml"}, "image-build")):
            with self.subTest(step=step):
                deploys = self.stack_deploys()
                proc = self.deploy(**env)
                self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
                lines = proc.stdout.strip().splitlines()
                self.assertRegex(lines[-1], rf"^DEPLOY-FAIL {step} ")
                self.assertEqual(lines[-2], "DEPLOY-STAGE unchanged")
                self.assertEqual(proc.stdout.count("DEPLOY-STAGE"), 1)
                self.assertNotIn("DEPLOY-REFUSED", proc.stdout)
                self.assertEqual(self.stack_deploys(), deploys, "no stack changed")
                self.assertEqual(self.ssm()[SETTINGS], record, "the record is as it was")
                self.assertNotIn(LOCK, self.ssm())
        # After the first stack change a failure is today's DEPLOY-FAIL alone: the stage may hold partial changes.
        proc = self.deploy(FAKE_FAIL_STACK="hamh-direct-data")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertEqual(self.last(proc), "DEPLOY-FAIL stack hamh-direct-data")
        self.assertNotIn("DEPLOY-STAGE", proc.stdout)


class LockCase(SettingsBase):
    def lock_puts(self) -> list[list[str]]:
        return self.puts(LOCK)

    def test_b812_a_manual_deploy_takes_the_lock_create_only_and_releases_it(self) -> None:
        self.ok()
        (put,) = self.lock_puts()
        self.assertNotIn("--overwrite", put, "create-only: a lock someone holds is never overwritten")
        value = dict(line.split("=", 1) for line in put[put.index("--value") + 1].splitlines())
        self.assertEqual(list(value), ["token", "holder", "identity", "route", "started", "commit", "inputs", "build"])
        self.assertRegex(value["token"], r"^[0-9a-f]{32}$")
        self.assertEqual(value["route"], "manual")
        self.assertEqual(value["commit"], td.git(self.root, "rev-parse", "HEAD"))
        self.assertRegex(value["holder"], r"^\S+:\d+$")
        tags = [a for a in self.aws_calls() if a[:2] == ["ssm", "add-tags-to-resource"] and LOCK in a]
        self.assertEqual(len(tags), 1)
        self.assertIn("Key=project,Value=hyperscale-agent-meta-harness", tags[0])
        deletes = [a for a in self.aws_calls() if a[:2] == ["ssm", "delete-parameter"]]
        self.assertEqual([a[a.index("--name") + 1] for a in deletes], [LOCK])
        self.assertNotIn(LOCK, self.ssm())
        # Released after a failure and after a refusal as well.
        self.assertEqual(self.deploy(FAKE_FAIL_STACK="hamh-direct-control").returncode, 1)
        self.assertNotIn(LOCK, self.ssm())
        self.running("01K5RUNA")
        self.assertEqual(self.deploy("--bump-worker-version").returncode, 3)
        self.assertNotIn(LOCK, self.ssm())

    def lock_values(self) -> list[dict]:
        """The lines of every value the deploy wrote into its lock, in order."""
        return [dict(line.split("=", 1) for line in p[p.index("--value") + 1].splitlines()) for p in self.lock_puts()]

    def test_b812_the_manual_lock_is_renewed_at_least_every_ten_minutes_while_the_deploy_runs(self) -> None:
        # Addendum 4, A4.3 (findings F7 and F18): setup and teardown take a lock that names no build for abandoned 30 minutes after its start time, and a
        # manual deploy that builds the worker image or the Cline fork runs longer. The deploy therefore keeps the start time of its own lock fresh.
        dp, d = self.deployer()
        self.assertLessEqual(dp.LOCK_RENEW_S, 600, "at least every ten minutes")
        clock = FakeClock(turns=9)  # 45 minutes of a deploy at the default interval
        d.clock = clock.now
        out = io.StringIO()
        with contextlib.redirect_stdout(out), mock.patch.object(dp.Deployer, "start_lock_keeper"):  # the keeper is driven by hand below, not from a thread
            d.take_lock()
            d.keep_lock(clock.wait)
        puts, values = self.lock_puts(), self.lock_values()
        self.assertEqual(len(puts), 1 + 9, "the create-only put, then one renewal per interval")
        self.assertNotIn("--overwrite", puts[0])
        self.assertTrue(all("--overwrite" in p for p in puts[1:]), "a renewal writes over the deploy's own lock")
        self.assertEqual(values[0]["started"], "2026-11-10T16:00:00Z")
        for value in values[1:]:
            self.assertEqual({k: v for k, v in value.items() if k != "started"}, {k: v for k, v in values[0].items() if k != "started"}, "only the start time moves")
        started = [dt.datetime.strptime(v["started"], "%Y-%m-%dT%H:%M:%SZ") for v in values]
        gaps = [(b - a).total_seconds() for a, b in zip(started, started[1:])]
        self.assertTrue(all(0 < gap <= 600 for gap in gaps), gaps)
        self.assertEqual(clock.waits, [dp.LOCK_RENEW_S] * 10)
        self.assertEqual(values[-1]["started"], f"{clock.at:%Y-%m-%dT%H:%M:%SZ}", "after 45 minutes the lock is as fresh as the last interval")
        self.assertEqual((started[-1] - started[0]).total_seconds(), 9 * dp.LOCK_RENEW_S)
        # A lock that was deleted under the running deploy is taken again, create-only and tagged, with the same token.
        store = self.ssm()
        store.pop(LOCK)
        (self.state / "ssm.json").write_text(json.dumps(store))
        with contextlib.redirect_stdout(out):
            self.assertTrue(d.renew_lock())
        self.assertNotIn("--overwrite", self.lock_puts()[-1])
        self.assertEqual(self.lock_values()[-1]["token"], values[0]["token"])
        self.assertEqual(len([a for a in self.aws_calls() if a[:2] == ["ssm", "add-tags-to-resource"] and LOCK in a]), 2)
        # A lock that another deploy replaced is never written over: the renewals stop, and the release leaves it alone and says so.
        other = self.seed_lock("b" * 32)
        calls = len(self.aws_calls())
        late = FakeClock(turns=3)
        with contextlib.redirect_stdout(out):
            d.keep_lock(late.wait)
            d.release_lock()
        self.assertEqual(len(late.waits), 1, "the keeper stops at the first renewal that finds another deploy's lock")
        self.assertEqual(self.ssm()[LOCK], other)
        self.assertEqual([a for a in self.aws_calls(calls) if a[:2] in (["ssm", "put-parameter"], ["ssm", "delete-parameter"])], [])
        said = [ln for ln in out.getvalue().splitlines() if "left alone" in ln]
        self.assertEqual(len(said), 2, out.getvalue())
        self.assertTrue(all("ci-host:4242" in ln and "route runner" in ln and LOCK in ln for ln in said), said)

    def test_b812_a_manual_deploy_renews_its_lock_from_a_thread_and_stops_before_it_releases_it(self) -> None:
        proc = self.ok(DEPLOY_LOCK_RENEW_S="0.2", FAKE_NODE_SLEEP_S="0.3")
        puts, values = self.lock_puts(), self.lock_values()
        self.assertNotIn("--overwrite", puts[0])
        self.assertGreaterEqual(len(puts) - 1, 3, "a renewal per interval while the nine stacks deploy")
        self.assertTrue(all("--overwrite" in p for p in puts[1:]))
        self.assertEqual({v["token"] for v in values}, {values[0]["token"]})
        self.assertEqual({v["route"] for v in values}, {"manual"})
        self.assertEqual(sorted(v["started"] for v in values), [v["started"] for v in values], "the start time only moves on")
        self.assertGreater(values[-1]["started"], values[0]["started"])
        calls = self.aws_calls()
        deleted = [i for i, a in enumerate(calls) if a[:2] == ["ssm", "delete-parameter"]]
        self.assertEqual(len(deleted), 1)
        self.assertFalse([a for a in calls[deleted[0]:] if a[:2] == ["ssm", "put-parameter"] and LOCK in a], "the keeper has stopped before the lock is deleted")
        self.assertNotIn(LOCK, self.ssm())
        # The deploy's own records are whole: the keeper's writes went beside them.
        self.assertEqual(self.record()["status"], "deployed")
        self.assertEqual(self.last(proc)[:11], "DEPLOY-PASS")
        self.assertLessEqual({"/hamh/direct/deployment-digest", "/hamh/direct/deployed-source-tree", "/hamh/direct/image-digest", "/hamh/direct/worker-revision",
                              "/hamh/direct/deployment-config-digest", SETTINGS}, set(self.ssm()))

    def test_b812_a_lock_that_another_deploy_replaced_is_left_alone_when_the_deploy_ends(self) -> None:
        # The lock of a manual deploy is replaced while it runs (here at the moment it writes its digests). The deploy must not delete the new holder's lock.
        other = "\n".join(("token=" + "b" * 32, "holder=other-host:77", f"identity=arn:aws:sts::{td.ACCOUNT}:assumed-role/Admin/bob", "route=local",
                           "started=2026-11-10T16:40:00Z", "commit=" + "e" * 40, "inputs=" + "f" * 64, "build="))
        proc = self.ok(FAKE_LOCK_REPLACED_AT_PUT="/hamh/direct/deployed-source-tree", FAKE_LOCK_OTHER=other)
        self.assertEqual(self.ssm().get(LOCK), other, "the lock of the deploy that replaced it stays")
        self.assertEqual([a for a in self.aws_calls() if a[:2] == ["ssm", "delete-parameter"]], [])
        said = [ln for ln in proc.stdout.splitlines() if ln.startswith("deploy: the deploy lock") and "left alone" in ln]
        self.assertEqual(len(said), 1, proc.stdout)
        for needle in (LOCK, "other-host:77", "route local", "2026-11-10T16:40:00Z"):
            self.assertIn(needle, said[0])
        self.assertEqual(self.last(proc)[:11], "DEPLOY-PASS")

    def test_b816_a_failed_tag_call_right_after_the_lock_is_taken_leaves_no_lock(self) -> None:
        # take_lock ran outside the try block of deploy_lock, so a failure after the create-only put (here its tag call) left the lock behind.
        proc = self.deploy(FAKE_TAG_FAIL=LOCK)
        self.assertNotEqual(proc.returncode, 0, proc.stdout)
        self.assertEqual(len(self.lock_puts()), 1, "TRIGGER PRESENT: the create-only put of the lock went through before its tag call failed")
        self.assertNotIn(LOCK, self.ssm(), "the lock this deploy created is deleted")
        self.ok()

    def test_b812_a_held_lock_is_left_alone_and_names_its_holder(self) -> None:
        text = self.seed_lock()
        proc = self.deploy()
        reason, wait, line = self.refused(proc)
        self.assertEqual((reason, wait), ("lock", "yes"))
        for needle in ("ci-host:4242", "2026-11-10T16:00:00Z", "c" * 40, "route runner", LOCK):
            self.assertIn(needle, line)
        self.assertEqual(self.ssm()[LOCK], text)
        self.assertEqual([a for a in self.aws_calls() if a[:2] == ["ssm", "delete-parameter"]], [])
        self.assertEqual(self.log("cargo"), [], "nothing is built")

    def test_b812_a_matching_token_deploys_under_the_callers_lock_and_leaves_it(self) -> None:
        text = self.seed_lock("b" * 32)
        self.ok(HAMH_DEPLOY_LOCK_TOKEN="b" * 32)
        self.assertEqual(self.lock_puts(), [])
        self.assertEqual([a for a in self.aws_calls() if a[:2] == ["ssm", "delete-parameter"]], [])
        self.assertEqual(self.ssm()[LOCK], text, "the holder releases its own lock")

    def test_b812_a_token_that_does_not_match_or_finds_no_lock_refuses(self) -> None:
        text = self.seed_lock("b" * 32)
        reason, wait, _ = self.refused(self.deploy(HAMH_DEPLOY_LOCK_TOKEN="c" * 32))
        self.assertEqual((reason, wait), ("lock", "yes"))
        self.assertEqual(self.ssm()[LOCK], text)
        self.setUp()
        reason, wait, line = self.refused(self.deploy(HAMH_DEPLOY_LOCK_TOKEN="c" * 32))
        self.assertEqual((reason, wait), ("lock", "yes"))
        self.assertIn("no deploy lock", line)
        self.assertNotIn(LOCK, self.ssm())


# ---- Addendum 3 (A3.1): the keep tag and the kind of the stage ------------------------------------------------------------------------------

THIRD = "333333333333"  # the account of the fixture's `test` entry: test-profile, us-east-1, zone us-east-1a with the id use1-az1


class StageShapeBase(SettingsBase):
    """A stage that the fixture's `test` entry deployed, and the account file setup generates for its update: the same account, Region and zone in one entry
    with role active and the caller as operator (scripts/setup/accounts.sh). The entry keeps the fixture's profile name, where setup writes hamh-setup: the fake
    cargo builds binaries for the fixture's profiles only, and no deploy rule reads the name. `as_test=True` deploys with the test entry."""

    CALLER = f"arn:aws:sts::{THIRD}:assumed-role/Admin/alice"

    def setUp(self) -> None:
        super().setUp()
        self.seed_s3(f"s3://hamh-direct-harness-{THIRD}/engine/cline/{td.CLINE_SHA}/cline", "fork", {"sha256": td.CLINE_SHA})
        doc = json.loads(td.sf.ACCOUNTS.read_text())
        entry = next(e for e in doc["accounts"] if e["role"] == "test")
        self.generated = self.tmp / "accounts-generated.json"
        self.generated.write_text(json.dumps({**doc, "accounts": [{**entry, "role": "active", "operator": {"kind": "role", "name": "Admin"}}]}))

    def deploy(self, *args: str, as_test: bool = False, **env: str) -> subprocess.CompletedProcess:
        where = {"FAKE_ACCOUNT": THIRD, "FAKE_STAGE_REGION": "us-east-1", "FAKE_AZ_MAP": json.dumps({"us-east-1a": "use1-az1"}), "FAKE_CALLER_ARN": self.CALLER}
        where.update({"HAMH_AWS_PROFILE": "test-profile"} if as_test else {"HAMH_ACCOUNTS_FILE": str(self.generated), "HAMH_AWS_PROFILE": ""})
        return super().deploy(*args, **{**where, **env})

    def stack_tags(self) -> dict:
        """The tags CloudFormation holds for each stack of the stage, by its short name."""
        out = {}
        for p in sorted(self.state.glob("stack-hamh-direct-*")):
            out[p.name[len("stack-hamh-direct-"):]] = json.loads(p.read_text()).get("tags", {})
        return out

    def research_kinds(self) -> dict:
        res = self.template("control").get("Resources") or {}
        return {r["Properties"]["FunctionName"]: r["Properties"]["Environment"]["Variables"].get("HAMH_STAGE_KIND") for r in res.values()
                if r.get("Type") == "AWS::Lambda::Function" and r["Properties"]["FunctionName"].startswith("hamh-direct-research-")}

    def assert_test_stage(self) -> None:
        """The keep tag on every stack but the network stack, and both research functions of kind test."""
        tags = self.stack_tags()
        self.assertEqual(len(tags), 9)
        for short, got in tags.items():
            self.assertEqual(got.get("auto-delete"), None if short == "network" else "no", short)
        self.assertEqual(self.research_kinds(), {"hamh-direct-research-api": "test", "hamh-direct-research-job": "test"})

    def deploy_record(self) -> dict:
        found = sorted(self.records.glob("*-deploy-direct-*.json"), key=lambda p: p.stat().st_mtime_ns)
        self.assertTrue(found)
        return json.loads(found[-1].read_text())


class MeasuredStageCase(StageShapeBase):
    """U13 for a stage without a record: the shapes of the two stages measured on 2026-11-10 (read-only). Both carry the stack tag auto-delete=no, both have
    research functions of kind test and no /hamh/direct/settings record, one has the Instances part with one machine type; the account file setup generates
    for their update has one entry with role active."""

    def update_keeps(self, instances: bool) -> None:
        first = dict(HAMH_COMPUTE_INSTANCES="on", HAMH_INSTANCES_TYPES="m7g.8xlarge") if instances else {}
        self.ok(as_test=True, **first)
        self.assert_test_stage()
        name = self.provider() if instances else None
        self.drop_record()  # as an earlier release left the stage
        # --print-settings shows both rows from the live stage and writes nothing.
        printed = self.deploy("--print-settings")
        self.assertEqual(printed.returncode, 0, printed.stdout + printed.stderr)
        self.assertIsNone(self.record())
        rows = self.settings(printed)
        self.assertEqual(list(rows), [r["name"] for r in registry_rows()])
        for got in (rows, None):
            if got is None:
                mark = self.mark()
                proc = self.ok()
                self.assertNotIn("DEPLOY-REFUSED", proc.stdout)
                got = self.settings(proc)
            self.assertEqual(got["HAMH_KEEP_TAG"], {"value": "on", "source": "live", "old": None, "new": None, "class": None}, got)
            self.assertEqual(got["HAMH_STAGE_KIND"], {"value": "test", "source": "live", "old": None, "new": None, "class": None}, got)
        kept = {"HAMH_KEEP_TAG": "on", "HAMH_STAGE_KIND": "test"}
        if instances:
            kept.update(HAMH_COMPUTE_INSTANCES="on", HAMH_INSTANCES_TYPES="m7g.8xlarge")
        self.assert_kept(proc, kept, "live", mark)
        # The live machine settings that equal today's defaults stay defaults, and nothing changes class.
        for n in ("HAMH_INSTANCES_ROOT_FREE_GIB", "HAMH_INSTANCES_WORKSPACE_GIB", "HAMH_INSTANCES_LIFETIME_S", "HAMH_INSTANCES_IDLE_S"):
            self.assertEqual(got[n]["source"], "default", n)
        self.assertFalse([n for n, r in got.items() if r["old"] is not None], "no setting changes")
        if instances:
            self.assertEqual(self.provider(), name, "the capacity provider keeps its name: CloudFormation replaces nothing and deletes no machine")
            self.assertFalse([ln for ln in proc.stdout.splitlines() if "REPLACES" in ln], proc.stdout)
        else:
            self.assertEqual(got["HAMH_COMPUTE_INSTANCES"], {"value": "default", "source": "default", "old": None, "new": None, "class": None})
            self.assertTrue(all(c["HAMH_COMPUTE_INSTANCES"] == "off" for c in self.cdk_calls(mark)))
        self.assert_test_stage()
        sets = {k: v for k, v in self.record().items() if k.startswith("set.")}
        self.assertEqual(sets, {f"set.{k}": v for k, v in kept.items()}, "the record holds the user's choices the live stage showed, and nothing else")
        self.assertEqual(self.record()["status"], "deployed")
        self.assertEqual(self.deploy_record()["inputs"]["toolsMode"]["stageKind"], "test", "the tool layer's record follows the resolved kind")
        # The next update reads the record.
        mark = self.mark()
        again = self.ok()
        self.assert_kept(again, kept, "kept", mark)
        self.assert_test_stage()
        if instances:
            self.assertEqual(self.provider(), name)

    def test_b812_update_of_a_test_stage_with_the_instances_part_keeps_its_keep_tag_kind_and_capacity_provider(self) -> None:
        self.update_keeps(instances=True)

    def test_b812_update_of_a_test_stage_without_the_instances_part_keeps_its_keep_tag_and_kind(self) -> None:
        self.update_keeps(instances=False)


class KeepTagKindCase(StageShapeBase):
    def test_b812_a_record_written_under_an_entry_of_one_role_keeps_the_keep_tag_and_kind_under_an_entry_of_another(self) -> None:
        # Addendum 4, A4.7 (findings F13 and F21): a maintainer's test stage gets its first record from scripts/deploy.sh with the committed entry of role
        # test; the next update comes through setup, whose generated account file always says role active. The stage keeps on and test, with no question.
        self.ok(as_test=True)
        both = {"HAMH_KEEP_TAG": "on", "HAMH_STAGE_KIND": "test"}
        self.assertEqual({k: v for k, v in self.record().items() if k.startswith("set.")}, {f"set.{k}": v for k, v in both.items()},
                         "the record holds both words, though the role of the entry gave them")
        printed = self.deploy("--print-settings")
        self.assertEqual(printed.returncode, 0, printed.stdout + printed.stderr)
        mark, deploys = self.mark(), self.stack_deploys()
        proc = self.ok()
        self.assertNotIn("DEPLOY-REFUSED", proc.stdout)
        self.assertGreater(self.stack_deploys(), deploys, "the update deployed")
        for rows in (self.settings(printed), self.settings(proc)):
            for name, word in both.items():
                self.assertEqual(rows[name], {"value": word, "source": "kept", "old": None, "new": None, "class": None}, name)
        self.assert_kept(proc, both, "kept", mark)
        self.assert_test_stage()
        self.assertEqual(self.deploy_record()["inputs"]["toolsMode"]["stageKind"], "test")
        # The other way round: the record of a deploy with an entry of role active keeps off and official under the entry of role test.
        self.setUp()
        self.ok()
        other = {"HAMH_KEEP_TAG": "off", "HAMH_STAGE_KIND": "official"}
        self.assertEqual({k: v for k, v in self.record().items() if k.startswith("set.")}, {f"set.{k}": v for k, v in other.items()})
        mark = self.mark()
        self.assert_kept(self.ok(as_test=True), other, "kept", mark)
        self.assertEqual({short: tags.get("auto-delete") for short, tags in self.stack_tags().items()}, {short: None for short in self.stack_tags()})
        self.assertEqual(self.research_kinds(), {"hamh-direct-research-api": "official", "hamh-direct-research-job": "official"})

    def test_b812_turning_the_keep_tag_off_needs_an_explicit_value(self) -> None:
        # A record that holds no line for the two words (one edited by hand: a deploy always writes both), on a stage that the test entry deployed; the
        # account file of the update gives role active, whose words are off and official.
        self.ok(as_test=True)
        self.edit_record(**{"set.HAMH_KEEP_TAG": None, "set.HAMH_STAGE_KIND": None})
        # The default of the role (off) would take the tag off, so a plain update keeps on and names the export that takes the default; the kind is a
        # free change. The resolution alone shows it and writes nothing.
        deploys, record = self.stack_deploys(), self.ssm()[SETTINGS]
        printed = self.deploy("--print-settings")
        self.assertEqual(printed.returncode, 0, printed.stdout + printed.stderr)
        self.assertEqual(held(printed), {"HAMH_KEEP_TAG": {"value": "on", "default": "off", "class": "remove", "sentence":
                                         "the stage keeps on; this release's default is off, and taking it removes what the setting holds now: "
                                         "export HAMH_KEEP_TAG=default to take it"}})
        self.assertEqual(self.settings(printed)["HAMH_STAGE_KIND"], {"value": "default", "source": "default", "old": "test", "new": "official", "class": "free"})
        self.assertEqual((self.stack_deploys(), self.ssm()[SETTINGS]), (deploys, record), "nothing changed")
        self.assert_test_stage()
        # An explicit word is the consent: on keeps the tag, off takes it off every stack.
        mark = self.mark()
        on = self.ok(HAMH_KEEP_TAG="on")
        self.assertEqual(self.settings(on)["HAMH_KEEP_TAG"], {"value": "on", "source": "explicit", "old": None, "new": None, "class": None})
        self.assertEqual(self.settings(on)["HAMH_STAGE_KIND"], {"value": "default", "source": "default", "old": "test", "new": "official", "class": "free"})
        self.assertTrue(all(c.get("HAMH_KEEP_TAG") == "on" and "HAMH_STAGE_KIND" not in c for c in self.cdk_calls(mark)))
        self.assertEqual(self.record()["set.HAMH_KEEP_TAG"], "on")
        self.assertEqual(self.research_kinds(), {"hamh-direct-research-api": "official", "hamh-direct-research-job": "official"})
        self.assertEqual(self.deploy_record()["inputs"]["toolsMode"]["stageKind"], "official")
        off = self.ok(HAMH_KEEP_TAG="off")
        self.assertEqual(self.settings(off)["HAMH_KEEP_TAG"], {"value": "off", "source": "explicit", "old": "on", "new": "off", "class": "remove"})
        self.assertEqual({short: t.get("auto-delete") for short, t in self.stack_tags().items()}, {short: None for short in self.stack_tags()})
        # On a stage without a record, `default` is the consent too.
        self.ok(HAMH_KEEP_TAG="on", HAMH_STAGE_KIND="test")
        self.drop_record()
        back = self.ok(HAMH_KEEP_TAG="default")
        self.assertEqual(self.settings(back)["HAMH_KEEP_TAG"], {"value": "default", "source": "explicit", "old": "on", "new": "off", "class": "remove"})
        self.assertEqual(self.settings(back)["HAMH_STAGE_KIND"]["source"], "live")
        self.assertEqual(self.record()["set.HAMH_KEEP_TAG"], "off", "the record holds the word the stage runs now")

    def test_b812_the_kind_is_a_free_change_and_reaches_the_research_functions(self) -> None:
        self.ok(as_test=True)
        self.drop_record()
        proc = self.ok(HAMH_STAGE_KIND="Official ")
        self.assertEqual(self.settings(proc)["HAMH_STAGE_KIND"], {"value": "official", "source": "explicit", "old": "test", "new": "official", "class": "free"})
        self.assertEqual(self.settings(proc)["HAMH_KEEP_TAG"]["source"], "live")
        self.assertEqual(self.research_kinds(), {"hamh-direct-research-api": "official", "hamh-direct-research-job": "official"})
        self.assertEqual(self.record()["set.HAMH_STAGE_KIND"], "official", "an explicit value is the user's choice, even when it equals the role's word")
        self.assertEqual(self.deploy_record()["inputs"]["toolsMode"]["stageKind"], "official")

    def test_b812_a_bad_keep_tag_or_kind_is_a_usage_error_before_any_aws_call(self) -> None:
        for name, bad in (("HAMH_KEEP_TAG", "yes"), ("HAMH_STAGE_KIND", "prod"), ("HAMH_KEEP_TAG", "maybe")):
            with self.subTest(name=name, bad=bad):
                proc = self.deploy(**{name: bad})
                self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
                self.assertIn(f'{name}="{bad}"', proc.stderr)
                self.assertIn(row(name)["note"], proc.stderr)
                self.assertEqual(self.log("aws"), [])

    def test_b812_a_live_keep_tag_or_kind_the_stage_does_not_answer_with_one_value_refuses(self) -> None:
        def no_tag(body: dict) -> None:
            body["tags"] = {}

        def other_kind(body: dict) -> None:
            body["template"]["Resources"]["ResearchJob"]["Properties"]["Environment"]["Variables"]["HAMH_STAGE_KIND"] = "official"

        def no_kind(body: dict) -> None:
            del body["template"]["Resources"]["ResearchJob"]["Properties"]["Environment"]["Variables"]["HAMH_STAGE_KIND"]

        cases = (("HAMH_KEEP_TAG", lambda: self.edit_stack("data", no_tag), {}, "the stacks of the stage do not agree"),
                 ("HAMH_KEEP_TAG", lambda: None, {"FAKE_DESCRIBE_STACKS_FAILS": "hamh-direct-data"}, "DescribeStacks"),
                 ("HAMH_STAGE_KIND", lambda: self.edit_stack("control", other_kind), {}, "official"),
                 ("HAMH_STAGE_KIND", lambda: self.edit_stack("control", no_kind), {}, "hamh-direct-research-job"))
        for name, spoil, env, why in cases:
            with self.subTest(name=name, why=why):
                self.setUp()
                self.ok(as_test=True)
                self.drop_record()
                spoil()
                deploys = self.stack_deploys()
                reason, wait, text = self.refused(self.deploy(**env))
                self.assertEqual((reason, wait), ("settings-read", "no"))
                self.assertIn(f"export {name}=<value>, or {name}=default", text)
                self.assertIn(why, text)
                self.assertEqual(self.stack_deploys(), deploys)
                self.assertIsNone(self.record())
                # An explicit word needs no live read.
                self.ok(**{name: "default" if name == "HAMH_STAGE_KIND" else "on"}, **env)

    def test_b812_a_stage_without_research_functions_takes_the_kind_of_the_role(self) -> None:
        self.ok(as_test=True)
        self.drop_record()

        def no_research(body: dict) -> None:
            for logical in ("ResearchApi", "ResearchJob"):
                del body["template"]["Resources"][logical]
        self.edit_stack("control", no_research)
        proc = self.ok(HAMH_KEEP_TAG="on")
        self.assertEqual(self.settings(proc)["HAMH_STAGE_KIND"], {"value": "default", "source": "default", "old": None, "new": None, "class": None})
        self.assertEqual(self.deploy_record()["inputs"]["toolsMode"]["stageKind"], "official")


if __name__ == "__main__":
    unittest.main()
