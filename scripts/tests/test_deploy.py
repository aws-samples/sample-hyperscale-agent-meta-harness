"""Tests for scripts/deploy.sh: stack-by-stack deploy of HEAD to stage `direct`.

Fakes stand in for every external tool: aws (DEPLOY_AWS_BIN, a state directory models ECR, S3,
CloudFormation, AgentCore, EC2 and SSM), cargo (DEPLOY_CARGO_BIN), scripts/node.sh (DEPLOY_NODE_BIN,
the CDK CLI) and scripts/codebuild-run.sh (DEPLOY_CODEBUILD_BIN). The contract:
- stage `direct` is the only stage: any other `--stage`, an `--azs` other than 1 and a retired flag
  (`--ladder-deferred`) are usage errors, as is a dirty tree (exit 2), all before any AWS call;
- a fresh stage gets a phase-1 pass of its 9 stacks (no worker image), then the image build, then a full pass
  with the image; a stage whose image already exists gets one pass and no build (idempotent);
- every cdk call deploys one stack (`--exclusively`) in dependency order with HAMH_LAMBDA_DIR set,
  and the image pass sets HAMH_WORKER_IMAGE for every stack, so producers publish their outputs; the image
  variables of the caller's environment never reach cdk;
- the Lambdas (worker-api, control-api, run-session, the research package's two binaries and the delivery package's three binaries) are built in
  one cargo lambda call; a missing bootstrap fails before any cdk call;
- the worker image is keyed on its inputs (tag `in-<key>`, over the crate closure of agentd and the
  IMAGE_PATHS of lib/digests.py), so a rerun at the same inputs reuses it; agentd is the one cross-built
  binary (one zigbuild), uploaded, and its SHA-256 passed to the image build, which also asks for the
  `direct` language family (LANG_FAMILY=direct);
- the VPC CIDR of the stage must overlap no other VPC of the account (the stage's own tagged VPC excepted)
  before anything is built;
- the runtime check covers the worker runtime: its `live` endpoint version must be READY, VPC mode,
  platform V1 and requireMMDSV2 true, running the deployed image; a runtime the deploy has no image for
  fails; otherwise DEPLOY-FAIL and no SSM digest write;
- SSM /hamh/direct/deployment-digest, /deployed-source-tree, /image-digest and /worker-revision are
  written and tagged; --bump-worker-version adds 1 to the revision
  and a plain deploy keeps it;
- the delivery image (repo-gateway, bundle-validator and packager bootstraps, git, gitleaks) is
  keyed on its inputs (tag `dl-<key>`), built by CodeBuild from bootstraps uploaded to
  builds/out/delivery/<sha>/, reused when ECR has the tag, and reaches every cdk call of the image
  pass as HAMH_DELIVERY_IMAGE; a stage without the delivery repository gets the delivery stack's
  phase 1 first;
- the CDK asset objects the stage uploaded are recorded for teardown;
- the last line is `DEPLOY-PASS stage=<s> digest=<12> sourceTree=<12>`;
- the deploy guard: a deploy that makes a new worker runtime version (a bump, or a worker
  image other than the one the live endpoint serves) while runs of `hamh-direct-run` are RUNNING
  ends `DEPLOY-FAIL guard ...` before it builds or deploys anything; `--check-guard` answers the
  same question and deploys nothing;
- version endpoints: the worker runtime gets a tagged `v<N>` endpoint for the version `live` serves,
  READY before the digests; older ones are deleted unless they are the rollback target or a running run pins them;
- The target is the accounts.json entry HAMH_AWS_PROFILE selects (else the active
  active-entry entry): its account, Region and profile reach every call, name and record; before any build
  the caller must be that account and the stage AZ must map to its accounts.json AZ ID;
- a fresh account: an existing CDK bootstrap at or above the synthesized requirement is used unchanged, a
  lower one fails, none is created with the project tags; the Cline fork binary
  is used from the stage bucket under the lock's SHA-256, else the fork a source build of this
  stage recorded for the same series tree and toolchain (the pointer `engine/cline/by-tree/<tree>-<toolchain>.json`),
  else copied from the account the lock's artifact names when that entry's profile is configured here (a copy that
  fails is fatal, never a build; its SHA-256 is checked), else built on CodeBuild ARM from the pinned upstream source.
  `build` skips the lock's object and the copy (the dry run of a second account). A source build is accepted on the
  evidence of the build itself (metadata and record computed by the build: commit, series tree, toolchain pins,
  ELF machine, telemetry off, leaks 0, frozen lockfile, the CodeBuild build id, the SHA-256 of the output equal to
  the SHA-256 of the downloaded bytes), promoted under its own SHA-256 with a create-only put, and recorded as a
  reference next to the lock's; the image build gets that SHA-256. Invocation logging is enabled only for an
  `enable-and-restore` entry, after its prior state is written to SSM, and a `read-only` entry is
  read and never written;
- 2026-10-01: every build goes to the directory of its account and Region selection, and what was built is
  checked (repo-gateway embeds the account, agentd the Region, the harness-protocol build output names the
  entry) before anything is uploaded or handed to CDK.
"""

from __future__ import annotations

import contextlib
import hashlib
import io
import json
import os
import shutil
import subprocess  # nosec B404: tests start the scripts under test and git with argument lists, never a shell
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

REPO = Path(__file__).resolve().parents[2]
SCRIPT = REPO / "scripts" / "deploy.sh"
sys.path.insert(0, str(REPO / "scripts" / "lib"))
sys.path.insert(0, str(Path(__file__).resolve().parent))
import aws_target  # noqa: E402
import deploy  # noqa: E402
import digests  # noqa: E402
import selection_fixture as sf  # noqa: E402

ACCOUNT = "1" * 12
ACCT_TWO = "2" * 12
ACCT_A1 = ACCT_TWO
ZERO_ACCOUNT = "0" * 12
OUTSIDE_ACCOUNT = "4" * 12  # an account that no entry of the accounts file names
EXTRA_ACCOUNT = "5555" + "6666" + "7777"
# The profiles of the two accounts, read from the placeholder accounts file of the fakes (selection_fixture.ACCOUNTS), which the tree of each case holds as its accounts.json.
PROFILE_OF = {a["account"]: a["profile"] for a in json.loads(sf.ACCOUNTS.read_text())["accounts"]}
ACCT_B2_PROFILE, ACCT_TWO_PROFILE = PROFILE_OF[ACCOUNT], PROFILE_OF[ACCT_TWO]
# The `owner` tag of each account's stage, by the rule of the account file (aws_target.owner_of): the cases below name no owner of their own.
OWNER_OF = {t.account: aws_target.owner_of(t) for t in aws_target.load(sf.ACCOUNTS)}
CLINE_SHA = "d" * 64
# The series pin of the test repository (engine/cline/lock.toml): the values of the real lock; the fake fork build writes them into its metadata.
SERIES_TREE = "f8990b6745402aec9bf0458611c763a1a7e710f8"
UPSTREAM_COMMIT = "1a5c3f84e0933268a9dba14a55a6383a6578414d"
TOOLCHAIN = {
    "bun_version": "1.4.2",
    "bun_linux_x64_zip_sha256": "36368faef7527875d5ffa52e53cd48021741f2a83eb6208a8dd64068d422a913",
    "bun_linux_aarch64_zip_sha256": "54328bbc2d9c8e0c9f892c544d66c57a83b84139e34909e5ee81758f1ac8fda7",
    "build_image": "node:22-bookworm@sha256:8a34c4ab3ea2c5cd194f07e317b2a8f09461d3c8b05c4e34c8ccd56d56024c4d",
}
CLINE_LOCK = ('schema = "1"\n\n[upstream]\nurl = "https://github.com/cline/cline.git"\ntag = "cli-v3.0.71"\ncommit = "%s"\n\n[series]\ntree = "%s"\n\n[toolchain]\n'
              % (UPSTREAM_COMMIT, SERIES_TREE) + "".join('%s = "%s"\n' % kv for kv in TOOLCHAIN.items()))
# The pointer key of a source-built fork: the series tree and the first 12 hex of the SHA-256 of the sorted toolchain lines.
TOOLCHAIN_DIGEST = hashlib.sha256("\n".join("%s=%s" % kv for kv in sorted(TOOLCHAIN.items())).encode()).hexdigest()[:12]
POINTER_KEY = f"engine/cline/by-tree/{SERIES_TREE}-{TOOLCHAIN_DIGEST}.json"

FAKE_AWS = r'''#!/usr/bin/env python3
import hashlib, json, os, sys, time
sys.dont_write_bytecode = True
# The folder of deploy_fakes.py: FAKE_FIXTURE_DIR when the caller sets it, else the tests folder that DeployCase.setUp writes in (a Deployer that a test
# drives in its own process runs this fake without the variable).
sys.path.insert(0, os.environ.get("FAKE_FIXTURE_DIR") or "__FIXTURE_DIR__")
from deploy_fakes import apply_fake_aws_control, write_control_endpoint
st = os.environ["FAKE_STATE"]
args = sys.argv[1:]
with open(os.path.join(st, "aws.log"), "a") as f:
    f.write(json.dumps({"args": args, "profile": os.environ.get("AWS_PROFILE")}) + "\n")
def arg(name):
    return args[args.index(name) + 1] if name in args else None
def has(name):
    return os.path.exists(os.path.join(st, name))
def read(name):
    return open(os.path.join(st, name)).read()
def out(obj):
    print(json.dumps(obj)); sys.exit(0)
def fail(msg, rc=254):
    sys.stderr.write(msg + "\n"); sys.exit(rc)
svc, op = (args + ["", ""])[:2]
controlled = apply_fake_aws_control((svc, op), args)
if controlled is not None:
    sys.stdout.write(controlled.stdout)
    sys.stderr.write(controlled.stderr)
    sys.exit(controlled.returncode)
if (svc, op) == ("sts", "get-caller-identity"):
    out({"Account": os.environ.get("FAKE_ACCOUNT", "__TEST_ACCOUNT__"), "Arn": os.environ.get("FAKE_CALLER_ARN", "")})
# AZ names map to IDs per account (active-entry us-east-1b = use1-az1); FAKE_AZ_MAP overrides.
if (svc, op) == ("ec2", "describe-availability-zones"):
    m = {"us-west-2a": "usw2-az1", "us-west-2b": "usw2-az2", "us-west-2c": "usw2-az3", "us-east-1a": "use1-az6",
         "us-east-1b": "use1-az1", "us-east-1c": "use1-az2", "us-east-1d": "use1-az4"}
    m.update(json.loads(os.environ.get("FAKE_AZ_MAP", "{}")))
    names = args[args.index("--zone-names") + 1:]
    names = names[:next((i for i, a in enumerate(names) if a.startswith("--")), len(names))]
    out({"AvailabilityZones": [{"ZoneName": n, "ZoneId": m[n], "State": "available"} for n in names if n in m]})
# The VPCs of the account (FAKE_VPCS: the list DescribeVpcs returns; none by default).
if (svc, op) == ("ec2", "describe-vpcs"):
    out({"Vpcs": json.loads(os.environ.get("FAKE_VPCS", "[]"))})
if (svc, op) == ("s3api", "head-object"):
    p = os.path.join(st, "s3", f"s3://{arg('--bucket')}/{arg('--key')}".replace("/", "|"))
    if not os.path.exists(p):
        fail("An error occurred (404) when calling the HeadObject operation: Not Found")
    meta = json.load(open(p + ".meta")) if os.path.exists(p + ".meta") else {}
    out({"ContentLength": os.path.getsize(p), "Metadata": meta})
if (svc, op) == ("configure", "list-profiles"):
    # The profiles configured on this host (FAKE_PROFILES, comma separated; run_deploy sets the profiles of accounts.json by default).
    print("\n".join(os.environ.get("FAKE_PROFILES", "").split(","))); sys.exit(0)
if (svc, op) == ("s3api", "put-object"):
    # A put of the stage bucket. With --if-none-match '*' a second put of the key is the 412 of the real service. FAKE_POINTER_RACE (a JSON
    # document {"pointer": <body>, "object": <content>, "sha256": <hex>}) makes another deploy win the pointer first.
    store = os.path.join(st, "s3"); os.makedirs(store, exist_ok=True)
    key = f"s3://{arg('--bucket')}/{arg('--key')}"
    p = os.path.join(store, key.replace("/", "|"))
    race = os.environ.get("FAKE_POINTER_RACE")
    if race and "/by-tree/" in key and not os.path.exists(p):
        r = json.loads(race)
        bucket = arg("--bucket")
        o = os.path.join(store, f"s3://{bucket}/engine/cline/{r['sha256']}/cline".replace("/", "|"))
        open(o, "w").write(r["object"]); json.dump({"sha256": r["sha256"]}, open(o + ".meta", "w"))
        open(p, "w").write(json.dumps(r["pointer"]))
    if arg("--if-none-match") == "*" and os.path.exists(p):
        fail("An error occurred (PreconditionFailed) when calling the PutObject operation: At least one of the pre-conditions you specified did not hold")
    open(p, "wb").write(open(arg("--body"), "rb").read())
    if "--metadata" in args:
        json.dump(dict(kv.split("=", 1) for kv in arg("--metadata").split(",")), open(p + ".meta", "w"))
    out({"ETag": "\"fake\""})
if (svc, op) == ("ecr", "describe-repositories"):
    name = arg("--repository-names")
    if has("repo-" + name):
        out({"repositories": [{"repositoryName": name, "repositoryUri": f"__TEST_ACCOUNT__.dkr.ecr.us-west-2.amazonaws.com/{name}"}]})
    fail("An error occurred (RepositoryNotFoundException) when calling the DescribeRepositories operation")
if (svc, op) == ("ecr", "describe-images"):
    tag = arg("--image-ids").split("=", 1)[1]
    if has("image-" + tag):
        out({"imageDetails": [{"imageDigest": read("image-" + tag), "imageTags": [tag]}]})
    fail("An error occurred (ImageNotFoundException) when calling the DescribeImages operation")
if (svc, op) == ("s3", "cp"):
    src, dst = args[2], args[3]
    store = os.path.join(st, "s3"); os.makedirs(store, exist_ok=True)
    if dst == "-":
        p = os.path.join(store, src.replace("/", "|"))
        if not os.path.exists(p): fail("fatal error: An error occurred (404) when calling the HeadObject operation", 1)
        sys.stdout.write(open(p).read()); sys.exit(0)
    if src.startswith("s3://"):
        # A download: only a reader in the account that owns the bucket (FAKE_S3_OWNERS: bucket -> profile).
        p = os.path.join(store, src.replace("/", "|"))
        owner = json.loads(os.environ.get("FAKE_S3_OWNERS", "{}")).get(src[5:].split("/", 1)[0])
        who = arg("--profile") or os.environ.get("AWS_PROFILE")
        if not os.path.exists(p) or (owner and who != owner):
            fail("fatal error: An error occurred (404) when calling the HeadObject operation: Key does not exist", 1)
        open(dst, "wb").write(open(p, "rb").read()); sys.exit(0)
    open(os.path.join(store, dst.replace("/", "|")), "wb").write(open(src, "rb").read())
    if "--metadata" in args:
        json.dump(dict(kv.split("=", 1) for kv in arg("--metadata").split(",")),
                  open(os.path.join(store, dst.replace("/", "|")) + ".meta", "w"))
    sys.exit(0)
if (svc, op) == ("cloudformation", "describe-stacks"):
    name = arg("--stack-name")
    if not has("stack-" + name): fail(f"An error occurred (ValidationError) when calling the DescribeStacks operation: Stack with id {name} does not exist")
    body = json.loads(read("stack-" + name))
    if name == "hamh-direct-control":
        write_control_endpoint(body, arg("--region"))
    outputs = [{"OutputKey": k, "OutputValue": v} for k, v in body.get("outputs", {}).items()]
    # A test that models a stack as CloudFormation holds it writes "status" and "tags" (a dict) into the stack file.
    out({"Stacks": [{"StackName": name, "StackStatus": body.get("status", "UPDATE_COMPLETE"), "Outputs": outputs,
                     "Tags": [{"Key": k, "Value": v} for k, v in body.get("tags", {}).items()]}]})
if (svc, op) == ("cloudformation", "get-template"):
    name = arg("--stack-name")
    if not has("stack-" + name): fail(f"An error occurred (ValidationError) when calling the GetTemplate operation: Stack with id {name} does not exist")
    out({"TemplateBody": json.loads(read("stack-" + name)).get("template", {})})
# Endpoints per runtime (state endpoints-<id>.json): live and DEFAULT serve version 3 unless seeded;
# a created endpoint is CREATING for two reads, then FAKE_ENDPOINT_ENDS (default READY).
def endpoints(rid):
    p = os.path.join(st, "endpoints-" + rid + ".json")
    if os.path.exists(p):
        return json.load(open(p))
    return {"live": {"liveVersion": "3", "status": "READY"}, "DEFAULT": {"liveVersion": "3", "status": "READY"}}
def save_endpoints(rid, eps):
    json.dump(eps, open(os.path.join(st, "endpoints-" + rid + ".json"), "w"))
if (svc, op) == ("bedrock-agentcore-control", "get-agent-runtime-endpoint"):
    rid, name = arg("--agent-runtime-id"), arg("--endpoint-name")
    if not has("runtime-" + rid):
        fail("An error occurred (ResourceNotFoundException) when calling the GetAgentRuntimeEndpoint operation")
    eps = endpoints(rid)
    if name not in eps:
        fail(f"An error occurred (ResourceNotFoundException) when calling the GetAgentRuntimeEndpoint operation: "
             f"Agent endpoint with name {name} not found for agent {rid}")
    e = eps[name]
    if e["status"] == "CREATING":
        e["reads"] = e.get("reads", 0) + 1
        if e["reads"] >= 2:
            e["status"] = os.environ.get("FAKE_ENDPOINT_ENDS", "READY")
            if e["status"] == "READY":
                e["liveVersion"] = e["targetVersion"]
        save_endpoints(rid, eps)
    out({"name": name, "liveVersion": e.get("liveVersion"), "targetVersion": e.get("targetVersion", e.get("liveVersion")),
         "status": e["status"]})
if (svc, op) == ("bedrock-agentcore-control", "list-agent-runtime-endpoints"):
    eps = endpoints(arg("--agent-runtime-id"))
    out({"runtimeEndpoints": [{"name": n, "id": n, "liveVersion": e.get("liveVersion"), "targetVersion": e.get("targetVersion"),
                               "status": e["status"]} for n, e in eps.items()]})
if (svc, op) == ("bedrock-agentcore-control", "create-agent-runtime-endpoint"):
    rid, name = arg("--agent-runtime-id"), arg("--name")
    eps = endpoints(rid)
    if name in eps:
        fail(f"An error occurred (ConflictException) when calling the CreateAgentRuntimeEndpoint operation: {name} exists")
    eps[name] = {"targetVersion": arg("--agent-runtime-version"), "status": "CREATING", "tags": json.loads(arg("--tags"))}
    save_endpoints(rid, eps)
    out({"targetVersion": arg("--agent-runtime-version"), "status": "CREATING", "agentRuntimeEndpointArn":
         f"arn:aws:bedrock-agentcore:us-west-2:__TEST_ACCOUNT__:runtime/{rid}/runtime-endpoint/{name}"})
if (svc, op) == ("bedrock-agentcore-control", "delete-agent-runtime-endpoint"):
    rid, name = arg("--agent-runtime-id"), arg("--endpoint-name")
    eps = endpoints(rid)
    eps.pop(name)
    save_endpoints(rid, eps)
    out({"status": "DELETING"})
if (svc, op) == ("dynamodb", "get-item"):
    # pins.json: the endpoint each run's Run item is pinned to (its `runtimeEndpoint`).
    pins = json.loads(read("pins.json")) if has("pins.json") else {}
    key = json.loads(arg("--key"))
    run = key["PK"]["S"].split("#", 1)[1]
    item = dict(key)
    if pins.get(run):
        item["runtimeEndpoint"] = {"S": pins[run]}
    out({"Item": item})
if (svc, op) == ("bedrock-agentcore-control", "get-agent-runtime"):
    rid = arg("--agent-runtime-id")
    state = json.loads(read("runtime-" + rid))
    image = state["image"]
    env = state.get("environmentVariables") or {}
    # FAKE_STALE_RUNTIME: a runtime (by id prefix) whose live version serves another image.
    if os.environ.get("FAKE_STALE_RUNTIME") and rid.startswith(os.environ["FAKE_STALE_RUNTIME"]):
        image = image.split("@", 1)[0] + "@sha256:" + "0" * 64
    out({"agentRuntimeId": rid, "agentRuntimeVersion": arg("--agent-runtime-version"), "status": "READY",
         **({} if os.environ.get("FAKE_RUNTIME_NO_ARTIFACT") else {"agentRuntimeArtifact": {"containerConfiguration": {"containerUri": image}}}),
         **({"capacityProviderConfiguration": {"capacityProviderArn": state["capacityProviderArn"]}} if state.get("capacityProviderArn")
            else {"networkConfiguration": {"networkMode": os.environ.get("FAKE_NETWORK_MODE", "VPC")}}),
         "platformVersion": "V1", "metadataConfiguration": {"requireMMDSV2": os.environ.get("FAKE_MMDS", "true") == "true"},
         **({"environmentVariables": env} if env else {})})
def ssm_edit(change):
    # One change of the parameter store at a time, and the file is replaced whole, so a reader never sees half of it: the deploy renews its lock from a
    # thread while it writes its records, and the service changes a parameter in one step. `change` edits the store or returns an error text.
    import fcntl
    p = os.path.join(st, "ssm.json")
    with open(os.path.join(st, "ssm-guard.lock"), "w") as guard:
        fcntl.flock(guard, fcntl.LOCK_EX)
        d = json.load(open(p)) if os.path.exists(p) else {}
        err = change(d)
        if err is None:
            tmp = "%s.%d.tmp" % (p, os.getpid())
            with open(tmp, "w") as f:
                json.dump(d, f)
            os.replace(tmp, p)
    if err is not None: fail(err)
    sys.exit(0)
if (svc, op) == ("ssm", "put-parameter"):
    # As the service: without --overwrite a name that exists is refused.
    def put(d):
        if arg("--name") in d and "--overwrite" not in args:
            return ("An error occurred (ParameterAlreadyExists) when calling the PutParameter operation: The parameter already exists. To overwrite this value, "
                    "set the overwrite option in the request to true.")
        d[arg("--name")] = arg("--value")
    ssm_edit(put)
if (svc, op) == ("ssm", "delete-parameter"):
    def delete(d):
        if arg("--name") not in d: return "An error occurred (ParameterNotFound) when calling the DeleteParameter operation: "
        d.pop(arg("--name"))
    ssm_edit(delete)
if (svc, op) == ("ssm", "get-parameter") and arg("--name") == "/cdk-bootstrap/hnb659fds/version":
    # The account's CDK bootstrap: FAKE_BOOTSTRAP_VERSION (default 32; `none`: never bootstrapped), or the
    # version a `cdk bootstrap` of this test wrote.
    v = read("bootstrap-version") if has("bootstrap-version") else os.environ.get("FAKE_BOOTSTRAP_VERSION", "32")
    if v == "none": fail("An error occurred (ParameterNotFound) when calling the GetParameter operation")
    out({"Parameter": {"Name": arg("--name"), "Value": v}})
if (svc, op) == ("ssm", "get-parameter"):
    p = os.path.join(st, "ssm.json")
    d = json.load(open(p)) if os.path.exists(p) else {}
    if arg("--name") not in d: fail("An error occurred (ParameterNotFound) when calling the GetParameter operation")
    out({"Parameter": {"Name": arg("--name"), "Value": d[arg("--name")]}})
if (svc, op) == ("ssm", "add-tags-to-resource"):
    # FAKE_TAG_FAIL: the tag call on that parameter fails, as a throttled call does right after a create
    if arg("--resource-id") == os.environ.get("FAKE_TAG_FAIL"):
        fail("An error occurred (ThrottlingException) when calling the AddTagsToResource operation: Rate exceeded")
    sys.exit(0)
if (svc, op) == ("stepfunctions", "list-executions"):
    # The state file running.json: the run names RUNNING on the stage's run machine.
    names = json.loads(read("running.json")) if has("running.json") else []
    arn = arg("--state-machine-arn")
    # order.log: when the guard looked, against when the Lambda build ran (the fake cargo writes the other half).
    open(os.path.join(st, "order.log"), "a").write(json.dumps({"n": time.time_ns(), "what": "list-executions"}) + "\n")
    out({"executions": [{"executionArn": f"{arn.replace(':stateMachine:', ':execution:')}:{n}", "name": n,
                         "stateMachineArn": arn, "status": "RUNNING"} for n in names]})
if (svc, op) == ("bedrock", "get-model-invocation-logging-configuration"):
    # Unset prints nothing, as the CLI does.
    if has("logging.json"):
        out({"loggingConfig": json.loads(read("logging.json"))})
    sys.exit(0)
if (svc, op) == ("bedrock", "put-model-invocation-logging-configuration"):
    open(os.path.join(st, "logging.json"), "w").write(arg("--logging-config")); sys.exit(0)
if (svc, op) == ("s3api", "list-objects-v2"):
    # The Go module store. FAKE_GOMOD: `objects` (default), `empty` (the CLI prints nothing for an empty bucket) or `denied`.
    mode = os.environ.get("FAKE_GOMOD", "objects")
    if mode == "denied": fail("An error occurred (AccessDenied) when calling the ListObjectsV2 operation: Access Denied")
    if mode == "empty": sys.exit(0)
    out({"Contents": [{"Key": "_manifests/2026-10-14T00:00:00Z-x.json", "Size": 1}]})
fail("fake aws: unexpected " + " ".join(args), 3)
'''

FAKE_CARGO = r'''#!/usr/bin/env python3
import json, os, sys, time
st = os.environ["FAKE_STATE"]
args = sys.argv[1:]
open(os.path.join(st, "cargo.log"), "a").write(json.dumps(args) + "\n")
open(os.path.join(st, "cargo-priority.log"), "a").write(json.dumps([args[0], os.environ.get("CARGO_SLOT_PRIORITY")]) + "\n")
open(os.path.join(st, "cargo-env.log"), "a").write(json.dumps({"call": args[:2], "CARGO_TARGET_DIR": os.environ.get("CARGO_TARGET_DIR"),
    "HAMH_AWS_PROFILE": os.environ.get("HAMH_AWS_PROFILE"), "HAMH_ACCOUNTS_FILE": os.environ.get("HAMH_ACCOUNTS_FILE"), "cwd": os.getcwd()}) + "\n")
root = os.getcwd()
flag = lambda n: args[args.index(n) + 1] if n in args else None
# Where cargo puts things: --target-dir, else CARGO_TARGET_DIR, else ./target; cargo lambda's --lambda-dir, else <target>/lambda.
tdir = flag("--target-dir") or os.environ.get("CARGO_TARGET_DIR") or "target"
if args[0] == "metadata":
    names = ("agentd", "harness-protocol", "worker-api", "harness-core", "delivery")
    ids = {n: f"{n} 0.1.0 (path+file://{root}/crates/{n})" for n in names}
    dep = lambda n: {"pkg": ids[n], "dep_kinds": [{"kind": None}]}
    print(json.dumps({"workspace_root": root,
        "packages": [{"id": i, "name": n, "source": None, "manifest_path": f"{root}/crates/{n}/Cargo.toml"} for n, i in ids.items()],
        "resolve": {"nodes": [
            {"id": ids["agentd"], "deps": [dep("harness-protocol")]},
            {"id": ids["harness-protocol"], "deps": []},
            {"id": ids["worker-api"], "deps": [dep("harness-protocol")]},
            {"id": ids["harness-core"], "deps": []},
            {"id": ids["delivery"], "deps": [dep("harness-protocol"), dep("harness-core")]}]}}))
    sys.exit(0)
# A build writes what a real one does: the binaries, with the selection they were built for (HAMH_AWS_PROFILE)
# compiled in, and the harness-protocol build script's output (selection_fixture.py).
sys.dont_write_bytecode = True  # a child writes no .pyc beside selection_fixture.py
sys.path.insert(0, os.environ["FAKE_FIXTURE_DIR"])
import selection_fixture as sf
if args[:2] == ["lambda", "build"]:
    open(os.path.join(st, "order.log"), "a").write(json.dumps({"n": time.time_ns(), "what": "lambda-build"}) + "\n")
    skip = os.environ.get("FAKE_LAMBDA_SKIP", "")
    bins = {"delivery": ["repo-gateway", "bundle-validator", "packager"], "research-api": ["research-api", "research-job"]}
    names = []
    for i, a in enumerate(args):
        if a != "-p" or args[i + 1] == skip:
            continue
        # The delivery package builds one bootstrap per binary.
        names += [b for b in bins.get(args[i + 1], [args[i + 1]]) if b != skip]
    sf.write_build("lambda", names, tdir, flag("--lambda-dir") or os.path.join(tdir, "lambda"), os.environ)
    sys.exit(int(os.environ.get("FAKE_CARGO_RC", "0")))
if args[0] == "zigbuild":
    names = [args[i + 1] for i, a in enumerate(args) if a == "-p" and args[i + 1] != os.environ.get("FAKE_ZIG_SKIP")]
    sf.write_build("zig", names, tdir, None, os.environ)
    sys.exit(0)
sys.exit(3)
'''

# The CDK CLI: synth writes the manifest; deploy records the call and models the stack in the state.
FAKE_NODE = r'''#!/usr/bin/env python3
import hashlib, json, os, sys
st = os.environ["FAKE_STATE"]
args = sys.argv[1:]
image = os.environ.get("HAMH_WORKER_IMAGE")
delivery = os.environ.get("HAMH_DELIVERY_IMAGE")
rev = os.environ.get("HAMH_WORKER_REVISION")
open(os.path.join(st, "node.log"), "a").write(json.dumps({"args": args, "cwd": os.getcwd(),
    "HAMH_LAMBDA_DIR": os.environ.get("HAMH_LAMBDA_DIR"), "HAMH_WORKER_IMAGE": image, "HAMH_WORKER_REVISION": rev,
    "HAMH_DELIVERY_IMAGE": delivery, "HAMH_TOOLS_MODE": os.environ.get("HAMH_TOOLS_MODE"),
    "HAMH_COMPUTE_INSTANCES": os.environ.get("HAMH_COMPUTE_INSTANCES"), "HAMH_ACCOUNTS_FILE": os.environ.get("HAMH_ACCOUNTS_FILE"),
    "HAMH_OWNER": os.environ.get("HAMH_OWNER")}) + "\n")
stage = args[args.index("-c") + 1].split("=", 1)[1] if "-c" in args else "direct"
p = f"hamh-{stage}"
os.makedirs("cdk.out", exist_ok=True)
# The app's 9 stacks and their edges (infra/lib/app.ts).
stacks = {"network": [], "security": ["network"], "data": ["security"], "packages": ["security", "network"],
          "control": ["network", "security", "data"], "runtimes": ["network", "security", "packages"],
          "orchestration": ["security", "data", "packages", "control"], "delivery": ["security", "data"],
          "observability": ["data"]}
art = {f"{p}-{n}": {"type": "aws:cloudformation:stack", "dependencies": [f"{p}-{d}" for d in deps] + [f"{p}-{n}.assets"]}
       for n, deps in stacks.items()}
art.update({f"{p}-{n}.assets": {"type": "cdk:asset-manifest"} for n in stacks})
if os.environ.get("FAKE_REQUIRED_BOOTSTRAP"):
    for n in stacks:
        art[f"{p}-{n}"]["properties"] = {"requiresBootstrapStackVersion": int(os.environ["FAKE_REQUIRED_BOOTSTRAP"])}
# The runtimes template holds the capacity provider, whose name ends in a hash of its configuration. FAKE_PROVIDER_NAME is another name (a changed configuration).
provider_name = os.environ.get("FAKE_PROVIDER_NAME", f"{p.replace('-', '_')}_instances_aaaaaaaa")
if os.environ.get("HAMH_COMPUTE_INSTANCES") == "on":
    art[f"{p}-runtimes"]["properties"] = {**art[f"{p}-runtimes"].get("properties", {}), "templateFile": f"{p}-runtimes.template.json"}
    json.dump({"Resources": {"InstancesCapacityProvider": {"Type": "AWS::BedrockAgentCore::CapacityProvider", "Properties": {"Name": provider_name}}}},
              open(f"cdk.out/{p}-runtimes.template.json", "w"))
json.dump({"version": "48.0.0", "artifacts": art}, open("cdk.out/manifest.json", "w"))
json.dump({"version": "48.0.0", "files": {"abc": {"destinations": {"d": {
    "bucketName": "cdk-hnb659fds-assets-__TEST_ACCOUNT__-us-west-2", "objectKey": "abc.zip"}}}}, "dockerImages": {}},
    open(f"cdk.out/{p}-control.assets.json", "w"))
if args[:2] == ["cdk", "deploy"]:
    name = args[2]
    if name == os.environ.get("FAKE_FAIL_STACK"):
        sys.stderr.write(f"{name} failed to deploy\n"); sys.exit(1)
    body = {"template": {"Resources": {"R": {"Type": "x", "Properties": {"image": image, "revision": rev, "delivery": delivery,
                                                                "toolsMode": os.environ.get("HAMH_TOOLS_MODE")}}}},
            "outputs": {}}
    if name == f"{p}-delivery":
        open(os.path.join(st, f"repo-{p}-delivery"), "w").write("1")
    if name == f"{p}-runtimes":
        # Phase 1 creates the repository; the runtime exists only with its image.
        open(os.path.join(st, f"repo-{p}-worker-python"), "w").write("1")
        body["outputs"]["WorkerPythonRepositoryUri"] = f"__TEST_ACCOUNT__.dkr.ecr.us-west-2.amazonaws.com/{p}-worker-python"
        if image:
            rid = f"{p.replace('-', '_')}_harness_worker_python-abc"
            body["outputs"]["WorkerPythonRuntimeId"] = rid
            # The CDK app puts the mode into the runtime's environment. The
            # isolation word is part of every runtime environment (FAKE_SANDBOX changes it).
            # FAKE_RUNTIME_ENV=nosandbox: a stage deployed before the isolation word existed.
            env = {} if os.environ.get("FAKE_RUNTIME_ENV") == "nosandbox" else {"AGENTD_ENGINE_SANDBOX": os.environ.get("FAKE_SANDBOX", "uid")}
            if os.environ.get("FAKE_RUNTIME_ENV") != "legacy":
                env["AGENTD_TOOLS_MODE"] = os.environ.get("HAMH_TOOLS_MODE") or "off"
            json.dump({"image": image, "environmentVariables": env}, open(os.path.join(st, "runtime-" + rid), "w"))
    if name == f"{p}-runtimes" and os.environ.get("HAMH_COMPUTE_INSTANCES") == "on":
        # The capacity provider exists in both phases; the second runtime only with the image, on the provider, with no network configuration.
        provider = f"arn:aws:bedrock-agentcore:us-west-2:__ZERO_ACCOUNT__:capacity-provider/{provider_name}-cp1abcdefg"
        body["outputs"]["InstancesCapacityProviderArn"] = provider
        if image:
            irid = f"{p.replace('-', '_')}_harness_worker_python_instances-xyz"
            body["outputs"]["WorkerPythonInstancesRuntimeId"] = irid
            ienv = {"AGENTD_ENGINE_SANDBOX": os.environ.get("FAKE_INSTANCES_SANDBOX", "landlock"), "AGENTD_TOOLS_MODE": os.environ.get("HAMH_TOOLS_MODE") or "off"}
            # Landlock runs the engine as agentd's own user, so the CDK app sets AGENTD_ENGINE_USER to the empty string.
            # FAKE_INSTANCES_ENGINE_USER=<user> sets another one, `absent` leaves the variable out (the image's "agent" would apply).
            if os.environ.get("FAKE_INSTANCES_ENGINE_USER", "") != "absent":
                ienv["AGENTD_ENGINE_USER"] = os.environ.get("FAKE_INSTANCES_ENGINE_USER", "")
            json.dump({"image": image, "environmentVariables": ienv,
                       **({} if os.environ.get("FAKE_INSTANCES_NO_PROVIDER") else {"capacityProviderArn": provider})},
                      open(os.path.join(st, "runtime-" + irid), "w"))
    if name == f"{p}-runtimes" and os.environ.get("FAKE_EXTRA_RUNTIME"):
        key, rid = os.environ["FAKE_EXTRA_RUNTIME"].split("=", 1)
        body["outputs"][f"{key}RuntimeId"] = rid
    json.dump(body, open(os.path.join(st, f"stack-{name}"), "w"))
    sys.exit(0)
if args[:2] == ["cdk", "synth"]:
    sys.exit(0)
if args[:2] == ["cdk", "bootstrap"]:
    open(os.path.join(st, "bootstrap-version"), "w").write("30")
    sys.exit(0)
if args[:2] == ["pnpm", "install"]:
    sys.exit(int(os.environ.get("FAKE_PNPM_RC", "0")))
sys.exit(3)
'''

FAKE_CODEBUILD = r'''#!/usr/bin/env python3
import hashlib, json, os, sys
st = os.environ["FAKE_STATE"]
args = sys.argv[1:]
open(os.path.join(st, "codebuild.log"), "a").write(json.dumps(args) + "\n")
open(os.path.join(st, "codebuild-env.log"), "a").write(json.dumps({"HAMH_ACCOUNTS_FILE": os.environ.get("HAMH_ACCOUNTS_FILE")}) + "\n")
envs = dict(args[i + 1].split("=", 1) for i, a in enumerate(args) if a == "--env")
spec = args[args.index("--buildspec") + 1]
if os.environ.get("FAKE_CODEBUILD_RC", "0") != "0" or spec == os.environ.get("FAKE_CODEBUILD_FAIL_SPEC"):
    print("CODEBUILD FAIL id=b:1 status=FAILED"); sys.exit(1)
if spec == "images/codebuild/cline-fork-arm64.yml":
    import subprocess, tomllib, uuid
    head = subprocess.run(["git", "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
    lock = tomllib.load(open("engine/cline/lock.toml", "rb"))
    stage = args[args.index("--stage") + 1]
    bid = "hamh-%s-image-build:%s" % (stage, uuid.UUID(hashlib.sha256((spec + "|" + head).encode()).hexdigest()[:32]))
    content = os.environ.get("FAKE_FORK_CONTENT", "rebuilt-fork").encode()
    bin_sha = hashlib.sha256(content).hexdigest()
    bad = os.environ.get("FAKE_FORK_BAD", "")
    tc = lock["toolchain"]
    meta = {"schema": "1", "mode": "build", "upstream": {"url": lock["upstream"]["url"], "tag": lock["upstream"]["tag"], "commit": lock["upstream"]["commit"]},
            "series": {"tree": lock["series"]["tree"], "patch_count": 15}, "telemetry": "off", "target": "linux-arm64",
            "toolchain": {"bun_version": tc["bun_version"], "bun_zip": "bun-linux-aarch64.zip", "bun_zip_sha256": tc["bun_linux_aarch64_zip_sha256"],
                          "build_image": tc["build_image"], "build_command": "bun install --frozen-lockfile && cd apps/cli && bun script/build.ts --single"},
            "binary": {"file": "cline", "size_bytes": len(content), "sha256": bin_sha, "elf_machine": "aarch64", "version": "3.0.71", "expected_version": "3.0.71"},
            "gate": {"ran": True}}
    record = {"kind": "cline-fork-build", "result": "pass", "measurements": {"telemetry": "off", "leaks": 0, "binarySha256": bin_sha,
              "seriesTree": lock["series"]["tree"], "lastLine": "FORK-BUILD PASS series=15 tree=%s bin=%s telemetry=off leaks=0" % (lock["series"]["tree"][:12], bin_sha[:12])}}
    smoke = {"jsonrpc": "2.0", "id": 1, "result": {"agentInfo": {"name": "cline", "version": "3.0.71"}}}
    out_id = bid
    if bad == "tree": meta["series"]["tree"] = "0" * 40
    if bad == "commit": meta["upstream"]["commit"] = "1" * 40
    if bad == "sha": meta["binary"]["sha256"] = "2" * 64
    if bad == "buildid": out_id = "hamh-%s-image-build:" % stage + "-".join(("00000000", "0000", "0000", "0000", "0" * 12))
    if bad == "elf": meta["binary"]["elf_machine"] = "x86-64"
    if bad == "leaks": record["measurements"]["leaks"] = 1
    if bad == "smoke": smoke["result"]["agentInfo"]["version"] = "9.9.9"
    if bad == "toolchain": meta["toolchain"]["bun_zip_sha256"] = "3" * 64
    if bad == "frozen": meta["toolchain"]["build_command"] = "bun install && cd apps/cli && bun script/build.ts --single"
    if bad == "nogate": meta["gate"]["ran"] = False
    if bad == "telemetry": meta["telemetry"] = "on"
    store = os.path.join(st, "s3"); os.makedirs(store, exist_ok=True)
    base = f"s3://{envs['HARNESS_BUCKET']}/builds/out/cline-fork/{head}/"
    for name, data in (("cline", content), ("build-metadata.json", json.dumps(meta).encode()), ("build-record.json", json.dumps(record).encode()),
                       ("build-id.txt", (out_id + "\n").encode()), ("smoke-acp-initialize.json", json.dumps(smoke).encode())):
        open(os.path.join(store, (base + name).replace("/", "|")), "wb").write(data)
    print("CODEBUILD PASS id=%s sha=abcdef0 seconds=1" % bid); sys.exit(0)
tag = envs["IMAGE_TAG"]
open(os.path.join(st, "image-" + tag), "w").write("sha256:" + hashlib.sha256(tag.encode()).hexdigest())
# scripts/codebuild-run.sh prints the id of the build in its result line: `<project>:<uuid>`, the project `hamh-<stage>-image-build`.
import uuid
stage = args[args.index("--stage") + 1]
bid = "hamh-%s-image-build:%s" % (stage, uuid.UUID(hashlib.sha256((spec + "|" + tag).encode()).hexdigest()[:32]))
if os.environ.get("FAKE_CODEBUILD_BAD_BYTES") == "1":
    sys.stdout.flush(); sys.stdout.buffer.write(b"build log line with a byte that is no UTF-8: \xff\xfe\n"); sys.stdout.buffer.flush()
if os.environ.get("FAKE_CODEBUILD_NO_ID") == "1":
    print("CODEBUILD PASS sha=abcdef0 seconds=1")
else:
    print("CODEBUILD PASS id=%s sha=abcdef0 seconds=1" % bid)
'''

STACKS = ["network", "security", "data", "packages", "control", "runtimes", "orchestration", "delivery", "observability"]


def fake_build_id(stage: str, spec: str, tag: str) -> str:
    """The build id FAKE_CODEBUILD prints for an image (the shape of scripts/codebuild-run.sh's `CODEBUILD PASS id=`)."""
    import uuid
    return f"hamh-{stage}-image-build:{uuid.UUID(hashlib.sha256((spec + '|' + tag).encode()).hexdigest()[:32])}"


def git(cwd: Path, *args: str) -> str:
    env = dict(os.environ, GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@example.com",
               GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@example.com")
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True, env=env).stdout.strip()  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do


ENDPOINT_FAKE_AWS = r'''#!{python}
import json
import os
import sys
from pathlib import Path

from deploy_fakes import apply_fake_aws_control, write_control_endpoint

arguments = sys.argv[1:]
pair = tuple(arguments[:2])
pid_file = os.environ.get("FAKE_AWS_PID_FILE")
if pid_file:
    Path(pid_file).write_text(str(os.getpid()), encoding="utf-8")
result = apply_fake_aws_control(pair, arguments)
if result is None:
    if pair != ("cloudformation", "describe-stacks"):
        raise SystemExit(97)
    region = arguments[arguments.index("--region") + 1]
    body = {{}}
    write_control_endpoint(body, region)
    rows = [{{"OutputKey": key, "OutputValue": value}} for key, value in body.get("outputs", {{}}).items()]
    result = type("Result", (), {{
        "returncode": 0,
        "stdout": json.dumps({{"Stacks": [{{"Outputs": rows}}]}}),
        "stderr": "",
    }})()
sys.stdout.write(result.stdout)
sys.stderr.write(result.stderr)
raise SystemExit(result.returncode)
'''


class DeployCase(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="deploy-"))
        self.root = self.tmp / "repo"
        files = {
            "Cargo.toml": "[workspace]\n", "Cargo.lock": "# lock\n",
            "crates/agentd/src/main.rs": "fn main() {}\n", "crates/harness-protocol/src/lib.rs": "\n",
            "crates/harness-core/src/lib.rs": "\n", "crates/worker-api/src/main.rs": "fn main() {}\n",
            "crates/delivery/src/lib.rs": "\n", "infra/lib/app.ts": "// app\n",
            "images/worker-python/Dockerfile": "FROM x\n", "images/codebuild/worker-python.yml": "version: 0.2\n",
            "images/delivery/Dockerfile": "FROM y\n", "images/codebuild/delivery.yml": "version: 0.2\n",
            "engine/lock.toml": f'schema = "1"\n\n[cline]\nbinarySha256 = "{CLINE_SHA}"\n',
            "engine/cline/lock.toml": CLINE_LOCK,
            "accounts.json": sf.ACCOUNTS.read_text(),
            "README.md": "x\n",
        }
        for rel, body in files.items():
            p = self.root / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(body)
        (self.root / ".gitignore").write_text("target/\ncdk.out/\n")
        git(self.root, "init", "-q", "-b", "main")
        git(self.root, "add", ".")
        git(self.root, "commit", "-q", "-m", "base")
        self.state = self.tmp / "state"
        self.state.mkdir()
        self.bins = {}
        for name, body in (("aws", FAKE_AWS), ("cargo", FAKE_CARGO), ("node", FAKE_NODE), ("codebuild", FAKE_CODEBUILD)):
            p = self.tmp / name
            rendered = body.replace("__TEST_ACCOUNT__", ACCOUNT).replace("__ZERO_ACCOUNT__", ZERO_ACCOUNT)
            rendered = rendered.replace("__FIXTURE_DIR__", str(Path(__file__).resolve().parent))
            p.write_text(rendered.replace("/usr/bin/env python3", sys.executable, 1))
            p.chmod(0o755)
            self.bins[name] = str(p)
        self.records = self.tmp / "records"
        # The promoted Cline fork binary is already in every stage bucket, unless a test removes it.
        for acct in (ACCOUNT, ACCT_TWO):
            self.seed_s3(f"s3://hamh-direct-harness-{acct}/engine/cline/{CLINE_SHA}/cline", "fork", {"sha256": CLINE_SHA})

    def seed_s3(self, uri: str, body: str, meta: dict | None = None) -> Path:
        store = self.state / "s3"
        store.mkdir(exist_ok=True)
        p = store / uri.replace("/", "|")
        p.write_text(body)
        if meta is not None:
            Path(str(p) + ".meta").write_text(json.dumps(meta))
        return p

    def run_deploy(self, *args: str, **env: str) -> subprocess.CompletedProcess:
        full = {k: v for k, v in os.environ.items() if k not in ("AWS_PROFILE", "HAMH_WORKER_IMAGE", "HAMH_LAMBDA_DIR", "HAMH_TOOLS_MODE", "HAMH_COMPUTE_INSTANCES")}
        # history-profile unless a test selects another entry: these cases were written against it.
        full.update(HAMH_AWS_PROFILE="history-profile",
                    DEPLOY_AWS_BIN=self.bins["aws"], DEPLOY_CARGO_BIN=self.bins["cargo"], DEPLOY_CARGO_SLOT="",
                    DEPLOY_NODE_BIN=self.bins["node"], DEPLOY_CODEBUILD_BIN=self.bins["codebuild"],
                    DEPLOY_ROOT=str(self.root), DEPLOY_RECORD_DIR=str(self.records), FAKE_STATE=str(self.state),
                    FAKE_FIXTURE_DIR=str(Path(__file__).resolve().parent), DEPLOY_ENDPOINT_POLL_S="0",
                    FAKE_PROFILES=",".join(PROFILE_OF.values()))
        full.update(env)
        full = {k: v for k, v in full.items() if v is not None}  # a case that passes None for a variable runs without it, as the setup runner does without DEPLOY_RECORD_DIR
        return subprocess.run(["bash", str(SCRIPT), *args], capture_output=True, text=True, env=full, timeout=300)  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do

    def log(self, name: str) -> list:
        p = self.state / f"{name}.log"
        return [json.loads(line) for line in p.read_text().splitlines()] if p.exists() else []

    def deploys(self) -> list[dict]:
        return [c for c in self.log("node") if c["args"][:2] == ["cdk", "deploy"]]

    def builds(self, spec: str) -> list:
        return [b for b in self.log("codebuild") if b[b.index("--buildspec") + 1] == spec]

    def worker_builds(self) -> list:
        return self.builds("images/codebuild/worker-python.yml")

    def delivery_builds(self) -> list:
        return self.builds("images/codebuild/delivery.yml")

    def zigbuilds(self) -> list[list[str]]:
        """The `-p` packages of every cargo zigbuild call."""
        calls = [c for c in self.log("cargo") if c[0] == "zigbuild"]
        return [[c[i + 1] for i, a in enumerate(c) if a == "-p"] for c in calls]

    def sha(self, name: str, profile: str = "history-profile") -> str:
        """SHA-256 of binary `name` as the fake build writes it for a selection (selection_fixture.binary_bytes)."""
        return hashlib.sha256(sf.binary_bytes(name, profile)).hexdigest()

    def keyed(self, profile: str = "history-profile") -> Path:
        """The per-selection cargo target directory of this deploy's checkout: target/deploy/<account>-<region>."""
        return self.root / "target" / "deploy" / sf.key(profile)

    def ssm(self) -> dict:
        p = self.state / "ssm.json"
        return json.loads(p.read_text()) if p.exists() else {}

    def last(self, proc: subprocess.CompletedProcess) -> str:
        return proc.stdout.strip().splitlines()[-1] if proc.stdout.strip() else ""

    def assert_refused(self, proc: subprocess.CompletedProcess, reason: str, detail: str = "") -> str:
        """A refusal before the first stack change is one DEPLOY-REFUSED line, the last one, and exit 3; `detail` is the start of its text."""
        self.assertEqual(proc.returncode, 3, proc.stdout + proc.stderr)
        line = self.last(proc)
        head = f"DEPLOY-REFUSED reason={reason} wait={'yes' if reason in ('guard', 'lock') else 'no'} stage=unchanged :: "
        self.assertTrue(line.startswith(head + detail), line)
        self.assertNotIn("DEPLOY-FAIL", proc.stdout)
        return line

    def digests_written(self) -> dict:
        """The SSM parameters of the stage but its settings record, which a deploy writes with status=deploying before its first stack change."""
        return {k: v for k, v in self.ssm().items() if k != "/hamh/direct/settings"}

    def test_a_fresh_stage_gets_phase_1_then_the_image_build_then_the_image_pass(self) -> None:
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        st = digests.source_tree(self.root)
        self.assertRegex(self.last(proc), rf"^DEPLOY-PASS stage=direct digest=[0-9a-f]{{12}} sourceTree={st[:12]}$")
        calls = self.deploys()
        names = [c["args"][2] for c in calls]
        self.assertEqual(names, [f"hamh-direct-{n}" for n in STACKS] * 2)
        for c in calls:
            self.assertIn("--exclusively", c["args"])
            self.assertIn("--require-approval", c["args"])
            self.assertIn("stage=direct", c["args"])
            self.assertEqual(c["HAMH_LAMBDA_DIR"], str(self.keyed() / "lambda" / "direct"))
            self.assertEqual(c["cwd"], str(self.root / "infra"))
        self.assertEqual({c["HAMH_WORKER_IMAGE"] for c in calls[:9]}, {None})
        image = calls[9]["HAMH_WORKER_IMAGE"]
        self.assertRegex(image, rf"^{ACCOUNT}\.dkr\.ecr\.us-west-2\.amazonaws\.com/hamh-direct-worker-python@sha256:[0-9a-f]{{64}}$")
        self.assertEqual({c["HAMH_WORKER_IMAGE"] for c in calls[9:]}, {image})
        builds = self.worker_builds()
        self.assertEqual(len(builds), 1)
        joined = " ".join(builds[0])
        key = digests.image_key(self.root, ["crates/agentd", "crates/harness-protocol"])
        for needle in ("--buildspec images/codebuild/worker-python.yml", "--stage direct",
                       f"HARNESS_BUCKET=hamh-direct-harness-{ACCOUNT}", f"CLINE_SHA256={CLINE_SHA}", f"IMAGE_TAG=in-{key}",
                       f"REPO_URI={ACCOUNT}.dkr.ecr.us-west-2.amazonaws.com/hamh-direct-worker-python",
                       "AGENTD_SHA256=" + self.sha("agentd"), "LANG_FAMILY=direct"):
            self.assertIn(needle, joined)
        self.assertNotIn("GATES_SHA256", joined)
        sha = git(self.root, "rev-parse", "HEAD")
        uploads = [c["args"] for c in self.log("aws") if c["args"][:2] == ["s3", "cp"] and c["args"][3] != "-"]
        self.assertIn(f"s3://hamh-direct-harness-{ACCOUNT}/builds/out/agentd/{sha}/agentd", [u[3] for u in uploads])
        self.assertEqual([z for z in self.zigbuilds() if "agentd" in z], [["agentd"]], "agentd is the one cross-built binary")

    def test_each_image_the_deploy_builds_writes_an_image_build_record_and_a_reuse_writes_none(self) -> None:
        # R21: every image-build record names CodeBuild as its build host and the account.
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        recs = sorted(self.records.glob("*-image-build-*.json"))
        docs = [json.loads(p.read_text()) for p in recs]
        self.assertEqual(sorted(d["inputs"]["buildspec"] for d in docs),
                         ["images/codebuild/delivery.yml", "images/codebuild/worker-python.yml"])
        sha = git(self.root, "rev-parse", "HEAD")
        for d in docs:
            for key in ("schemaVersion", "kind", "createdAt", "host", "account", "region", "stage", "codeSha",
                        "sourceTree", "deploymentDigest", "imageDigest", "criteriaSha256", "result", "inputs",
                        "measurements", "runIds", "notes"):
                self.assertIn(key, d)
            self.assertEqual((d["kind"], d["account"], d["region"], d["stage"], d["codeSha"], d["result"]),
                             ("image-build", ACCOUNT, "us-west-2", "direct", sha, "pass"))
            self.assertEqual(d["measurements"]["buildHost"], "codebuild")
            self.assertRegex(d["measurements"]["image"], r"@sha256:[0-9a-f]{64}$")
            self.assertTrue(d["inputs"]["imageTag"])
        again = self.run_deploy("--stage", "direct")
        self.assertEqual(again.returncode, 0, again.stdout + again.stderr)
        self.assertEqual(sorted(self.records.glob("*-image-build-*.json")), recs)

    def test_each_image_build_record_names_the_codebuild_build_that_made_the_image(self) -> None:
        # R21: the record carries the CodeBuild build id, the id scripts/codebuild-run.sh
        # prints in its result line, so the row can read the build back and see that it ran on an ARM environment.
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        docs = [json.loads(p.read_text()) for p in sorted(self.records.glob("*-image-build-*.json"))]
        self.assertEqual(len(docs), 2)
        for d in docs:
            with self.subTest(image=d["inputs"]["image"]):
                want = fake_build_id("direct", d["inputs"]["buildspec"], d["inputs"]["imageTag"])
                self.assertEqual(d["measurements"]["codebuildBuildId"], want)
        self.assertEqual(len({d["measurements"]["codebuildBuildId"] for d in docs}), 2, "one build per image")

    def test_a_codebuild_run_that_prints_no_build_id_fails_the_deploy_and_writes_no_record(self) -> None:
        proc = self.run_deploy("--stage", "direct", FAKE_CODEBUILD_NO_ID="1")
        self.assertEqual(proc.returncode, 1)
        self.assertRegex(self.last(proc), r"^DEPLOY-FAIL [a-z-]+ codebuild-run printed no build id$")
        self.assertEqual(sorted(self.records.glob("*-image-build-*.json")), [])

    def test_a_build_log_line_that_is_no_utf8_does_not_stop_the_deploy_or_lose_the_id(self) -> None:
        proc = self.run_deploy("--stage", "direct", FAKE_CODEBUILD_BAD_BYTES="1")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        docs = [json.loads(p.read_text()) for p in sorted(self.records.glob("*-image-build-*.json"))]
        self.assertEqual(len(docs), 2)
        self.assertTrue(all(d["measurements"]["codebuildBuildId"].startswith("hamh-direct-image-build:") for d in docs))

    def test_worker_image_build_records_assert_the_cline_sha256_and_other_images_do_not(self) -> None:
        # R24: "the value every worker image asserted at image build". The worker image COPYs the promoted fork, and
        # its record states the SHA-256 of the binary the build COPYed: the lock's binarySha256, verified against the S3 object the
        # build downloads (metadata, else bytes). The delivery image carries no fork: no field.
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        docs = {d["inputs"]["buildspec"]: d for d in (json.loads(p.read_text()) for p in sorted(self.records.glob("*-image-build-*.json")))}
        workers = ["images/codebuild/worker-python.yml"]
        for spec in workers:
            with self.subTest(image=spec):
                m = docs[spec]["measurements"]
                self.assertEqual(m.get("clineSha256"), CLINE_SHA)
                self.assertEqual(docs[spec]["inputs"]["binaries"]["CLINE_SHA256"], m["clineSha256"])
                self.assertEqual(m.get("forkBinary", {}).get("outcome"), "present")
                self.assertEqual(docs[spec]["inputs"]["langFamily"], "direct")
        self.assertEqual(sorted(docs), ["images/codebuild/delivery.yml", *workers])
        for spec, d in docs.items():
            if spec not in workers:
                with self.subTest(image=spec):
                    self.assertNotIn("clineSha256", d["measurements"])
                    self.assertNotIn("forkBinary", d["measurements"])

    def test_a_worker_image_record_names_the_sha256_of_a_copied_or_built_fork_binary(self) -> None:
        sha = self.lock_with(None, b"rebuilt-fork")
        proc = self.acct_two()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        docs = [json.loads(p.read_text()) for p in sorted(self.records.glob("*-image-build-*.json"))]
        workers = [d for d in docs if d["inputs"]["image"].startswith("worker")]
        self.assertEqual(len(workers), 1)
        for d in workers:
            self.assertEqual(d["measurements"]["clineSha256"], sha)
            self.assertEqual(d["measurements"]["forkBinary"]["outcome"], "built-from-source")
            self.assertTrue(d["measurements"]["forkBinary"]["matchesLock"])

    def test_records_of_one_commit_deployed_to_two_accounts_do_not_overwrite_each_other(self) -> None:
        acct_b2 = self.run_deploy("--stage", "direct")
        self.assertEqual(acct_b2.returncode, 0, acct_b2.stdout + acct_b2.stderr)
        acct_two = self.run_deploy("--stage", "direct", HAMH_AWS_PROFILE="", FAKE_ACCOUNT=ACCT_TWO)
        self.assertEqual(acct_two.returncode, 0, acct_two.stdout + acct_two.stderr)
        deploys = sorted(self.records.glob("*-deploy-direct-*.json"))
        self.assertEqual(sorted(json.loads(p.read_text())["account"] for p in deploys), sorted([ACCOUNT, ACCT_TWO]))
        recs = sorted(self.records.glob("*.json"))
        images = [p for p in recs if "-image-build-" in p.name]
        self.assertTrue(images)
        for p in recs:
            self.assertTrue(p.name.endswith(f"-{json.loads(p.read_text())['account']}.json"), p.name)
        for p in images:
            self.assertIn("-image-build-direct-", p.name, "the record name carries the stage")

    def test_every_lambda_is_built_in_one_cargo_lambda_call(self) -> None:
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        builds = [c for c in self.log("cargo") if c[:2] == ["lambda", "build"]]
        self.assertEqual(len(builds), 1, builds)
        self.assertEqual([builds[0][i + 1] for i, a in enumerate(builds[0]) if a == "-p"],
                         ["worker-api", "control-api", "run-session", "research-api", "delivery"])
        for name in ("worker-api", "control-api", "run-session", "research-api", "research-job", "repo-gateway", "bundle-validator", "packager"):
            self.assertIn(f"lambda {name} bootstrap sha256=", proc.stdout)

    def test_every_build_the_deploy_makes_asks_for_cargo_slot_priority(self) -> None:
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        calls = [json.loads(line) for line in (self.state / "cargo-priority.log").read_text().splitlines()]
        builds = [c for c in calls if c[0] in ("lambda", "zigbuild")]
        self.assertGreaterEqual(len(builds), 2, calls)
        self.assertEqual({c[1] for c in builds}, {"high"}, builds)

    def test_a_lambda_without_its_bootstrap_fails_the_deploy_before_any_cdk_call(self) -> None:
        for name in ("worker-api", "control-api", "run-session", "research-api", "research-job"):
            with self.subTest(name):
                # A fresh checkout and state each time: the bootstraps of the previous build must not stand in for the missing one.
                self.setUp()
                proc = self.run_deploy("--stage", "direct", FAKE_LAMBDA_SKIP=name)
                self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
                self.assertRegex(self.last(proc), rf"^DEPLOY-FAIL lambda-build {name}")
                self.assertEqual(self.deploys(), [])

    def test_a_fresh_checkout_installs_the_infra_dependencies_before_any_cdk_call(self) -> None:
        # A clean worktree has no infra/node_modules: synth failed with "tsx: not found".
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        first = self.log("node")[0]
        self.assertEqual(first["args"], ["pnpm", "install", "--frozen-lockfile"])
        self.assertEqual(first["cwd"], str(self.root / "infra"))

    def test_a_failed_dependency_install_fails_before_any_cdk_call(self) -> None:
        proc = self.run_deploy("--stage", "direct", FAKE_PNPM_RC="1")
        self.assertEqual(proc.returncode, 1)
        self.assertEqual(self.last(proc), "DEPLOY-FAIL infra-install pnpm install exit 1")
        self.assertEqual([c for c in self.log("node") if c["args"][0] == "cdk"], [])

    def test_a_rerun_reuses_the_image_deploys_once_and_prints_the_same_digest(self) -> None:
        first = self.run_deploy("--stage", "direct")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        n_deploys, n_builds = len(self.deploys()), len(self.log("codebuild"))
        self.assertEqual(n_builds, 2, "one worker build and one delivery build")
        second = self.run_deploy("--stage", "direct")
        self.assertEqual(second.returncode, 0, second.stdout + second.stderr)
        self.assertEqual(self.last(second), self.last(first))
        self.assertEqual(len(self.log("codebuild")), n_builds)
        again = self.deploys()[n_deploys:]
        self.assertEqual(len(again), 9)
        self.assertEqual({c["HAMH_WORKER_IMAGE"] for c in again}, {self.deploys()[-1]["HAMH_WORKER_IMAGE"]})
        self.assertEqual({c["HAMH_DELIVERY_IMAGE"] for c in again}, {self.deploys()[n_deploys - 1]["HAMH_DELIVERY_IMAGE"]})
        self.assertEqual(self.zigbuilds().count(["agentd"]), 1, "the rerun cross-builds nothing")

    def test_an_infra_only_commit_reuses_the_image_and_changes_the_source_tree(self) -> None:
        first = self.run_deploy("--stage", "direct")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        (self.root / "infra" / "lib" / "app.ts").write_text("// changed\n")
        git(self.root, "commit", "-q", "-am", "infra change")
        second = self.run_deploy("--stage", "direct")
        self.assertEqual(second.returncode, 0, second.stdout + second.stderr)
        self.assertEqual(len(self.worker_builds()), 1)
        self.assertEqual(len(self.delivery_builds()), 1)
        self.assertNotEqual(self.last(second).split()[-1], self.last(first).split()[-1])

    def test_an_agentd_change_rebuilds_the_worker_image_alone_and_deploys_one_pass(self) -> None:
        # The converted case of the old line's "image input change": the repositories exist, so there is no pre-build stack update.
        first = self.run_deploy("--stage", "direct")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        n = len(self.deploys())
        (self.root / "crates" / "agentd" / "src" / "main.rs").write_text("fn main() { }\n")
        git(self.root, "commit", "-q", "-am", "agentd change")
        second = self.run_deploy("--stage", "direct")
        self.assertEqual(second.returncode, 0, second.stdout + second.stderr)
        again = self.deploys()[n:]
        self.assertEqual([c["args"][2] for c in again], [f"hamh-direct-{s}" for s in STACKS])
        self.assertEqual(len(self.worker_builds()), 2)
        self.assertEqual(len(self.delivery_builds()), 1, "an agentd change leaves the delivery image alone")
        self.assertEqual(self.zigbuilds().count(["agentd"]), 2)
        new = again[-1]["HAMH_WORKER_IMAGE"]
        self.assertNotEqual(new, self.deploys()[n - 1]["HAMH_WORKER_IMAGE"])
        self.assertEqual({c["HAMH_WORKER_IMAGE"] for c in again}, {new})

    def test_a_missing_agentd_binary_fails_before_the_image_build(self) -> None:
        proc = self.run_deploy("--stage", "direct", FAKE_ZIG_SKIP="agentd")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertRegex(self.last(proc), r"^DEPLOY-FAIL agentd-build .*missing agentd")
        self.assertEqual(self.log("codebuild"), [])
        self.assertEqual(self.digests_written(), {})

    def test_ssm_holds_the_digests_the_pass_line_names(self) -> None:
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        params = self.ssm()
        digest = params["/hamh/direct/deployment-digest"]
        self.assertRegex(digest, r"^[0-9a-f]{64}$")
        self.assertIn(f"digest={digest[:12]}", self.last(proc))
        self.assertEqual(params["/hamh/direct/deployed-source-tree"], digests.source_tree(self.root))
        served = [self.deploys()[-1]["HAMH_WORKER_IMAGE"].split("@", 1)[1]]
        lock = hashlib.sha256((self.root / "engine" / "lock.toml").read_bytes()).hexdigest()
        self.assertEqual(params["/hamh/direct/image-digest"], digests.image_digest(served, lock))
        self.assertEqual(params["/hamh/direct/worker-revision"], "0")
        self.assertEqual(params["/hamh/direct/deployment-config-digest"], "committed")
        digest_names = [f"/hamh/direct/{n}" for n in ("deployment-digest", "deployed-source-tree", "image-digest", "worker-revision", "deployment-config-digest")]
        tags = [c["args"] for c in self.log("aws") if c["args"][:2] == ["ssm", "add-tags-to-resource"] and c["args"][c["args"].index("--resource-id") + 1] in digest_names]
        self.assertEqual(len(tags), 5)
        self.assertTrue(all("Key=project,Value=hyperscale-agent-meta-harness" in t for t in tags))

    def test_bump_worker_version_changes_the_runtime_environment_and_a_plain_rerun_keeps_it(self) -> None:
        # S2 and: a CloudFormation environment change makes a new runtime version.
        first = self.run_deploy("--stage", "direct")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        self.assertEqual({c["HAMH_WORKER_REVISION"] for c in self.deploys()}, {"0"})
        n = len(self.deploys())
        bump = self.run_deploy("--stage", "direct", "--bump-worker-version")
        self.assertEqual(bump.returncode, 0, bump.stdout + bump.stderr)
        self.assertEqual({c["HAMH_WORKER_REVISION"] for c in self.deploys()[n:]}, {"1"})
        self.assertEqual(self.ssm()["/hamh/direct/worker-revision"], "1")
        self.assertNotEqual(self.last(bump), self.last(first))
        self.assertEqual(len(self.worker_builds()), 1, "a bump reuses the image")
        m = len(self.deploys())
        again = self.run_deploy("--stage", "direct")
        self.assertEqual(again.returncode, 0, again.stdout + again.stderr)
        self.assertEqual({c["HAMH_WORKER_REVISION"] for c in self.deploys()[m:]}, {"1"})
        self.assertEqual(self.last(again), self.last(bump))

    # ----: no runtime version update while pinned runs are active -------------------------

    def running(self, *names: str) -> None:
        (self.state / "running.json").write_text(json.dumps(list(names)))

    def listed(self) -> list:
        return [c["args"] for c in self.log("aws") if c["args"][:2] == ["stepfunctions", "list-executions"]]

    def test_a_bump_while_runs_are_active_is_refused_before_it_builds_or_deploys(self) -> None:
        first = self.run_deploy("--stage", "direct", "--azs", "1")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        self.running("01K5RUNA", "01K5RUNB")
        n, builds = len(self.deploys()), len([c for c in self.log("cargo") if c[:2] == ["lambda", "build"]])
        proc = self.run_deploy("--stage", "direct", "--bump-worker-version")
        self.assertEqual(self.assert_refused(proc, "guard"),
                         "DEPLOY-REFUSED reason=guard wait=yes stage=unchanged :: 2 running run(s) on hamh-direct-run pin the worker runtime: 01K5RUNA 01K5RUNB. "
                         "Wait until they end, then deploy again.")
        self.assertEqual(len(self.deploys()), n, "no stack deploys")
        self.assertEqual(len([c for c in self.log("cargo") if c[:2] == ["lambda", "build"]]), builds, "no builds")
        self.assertEqual(self.ssm()["/hamh/direct/worker-revision"], "0", "the revision is not bumped")
        arn = f"arn:aws:states:us-west-2:{ACCOUNT}:stateMachine:hamh-direct-run"
        self.assertIn(["stepfunctions", "list-executions", "--state-machine-arn", arn, "--status-filter", "RUNNING",
                       "--region", "us-west-2", "--output", "json"], self.listed())
        # Once the runs end, the same bump goes through.
        self.running()
        ok = self.run_deploy("--stage", "direct", "--bump-worker-version")
        self.assertEqual(ok.returncode, 0, ok.stdout + ok.stderr)
        self.assertEqual(self.ssm()["/hamh/direct/worker-revision"], "1")

    def test_a_new_worker_image_while_runs_are_active_is_refused_before_the_image_build(self) -> None:
        first = self.run_deploy("--stage", "direct")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        (self.root / "crates/agentd/src/main.rs").write_text("fn main() { let _ = 1; }\n")
        git(self.root, "commit", "-q", "-am", "agentd change")
        self.running("01K5RUNA")
        n = len(self.deploys())
        proc = self.run_deploy("--stage", "direct")
        self.assert_refused(proc, "guard", "1 running run(s) on hamh-direct-run ")
        self.assertEqual(len(self.worker_builds()), 1, "the new image is not built")
        self.assertEqual(len(self.deploys()), n)

    def test_a_redeploy_that_keeps_the_worker_version_is_not_guarded(self) -> None:
        first = self.run_deploy("--stage", "direct")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        self.running("01K5RUNA")
        # An infra-only change keeps the worker image and the revision: no new runtime version.
        (self.root / "infra/lib/app.ts").write_text("// app v2\n")
        git(self.root, "commit", "-q", "-am", "infra change")
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.listed(), [], "the guard is not consulted")

    def test_a_fresh_stage_has_no_runtime_to_guard(self) -> None:
        self.running("01K5RUNA")
        proc = self.run_deploy("--stage", "direct", "--azs", "1")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.listed(), [])

    def test_check_guard_answers_without_building_or_deploying(self) -> None:
        proc = self.run_deploy("--stage", "direct", "--bump-worker-version", "--check-guard")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.last(proc), "DEPLOY-GUARD PASS stage=direct running=0")
        self.running("01K5RUNA")
        proc = self.run_deploy("--stage", "direct", "--check-guard")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertEqual(self.last(proc), "DEPLOY-FAIL guard 1 running run(s) on hamh-direct-run pin the worker runtime: 01K5RUNA")
        self.assertEqual(self.log("cargo"), [])
        self.assertEqual(self.log("node"), [])
        self.assertEqual(self.log("codebuild"), [])
        self.assertEqual({tuple(c["args"][:2]) for c in self.log("aws")},
                         {("sts", "get-caller-identity"), ("stepfunctions", "list-executions")})
        # A dirty tree does not matter to a read-only check.
        (self.root / "README.md").write_text("dirty\n")
        self.assertEqual(self.run_deploy("--stage", "direct", "--check-guard").returncode, 1)

    # ----: the tool layer's stage setting HAMH_TOOLS_MODE ------------------------------------
    # A per-deploy setting (like HAMH_UTILITY_MODEL) that defaults to off, reaches every cdk call as a concrete off or on and becomes the worker
    # runtime's AGENTD_TOOLS_MODE. A change of that variable makes a new runtime version, so it meets the guard as a bump does.

    # ----: the Instances part (HAMH_COMPUTE_INSTANCES=on): a second runtime on the capacity provider, checked with its own isolation word.
    IRID = "hamh_direct_harness_worker_python_instances-xyz"

    def cdk_instances(self) -> list:
        """HAMH_COMPUTE_INSTANCES as every cdk call (synth and deploy) of the run saw it."""
        return [c["HAMH_COMPUTE_INSTANCES"] for c in self.log("node") if c["args"][:2] in (["cdk", "synth"], ["cdk", "deploy"])]

    def test_off_is_the_default_the_stage_has_one_runtime_and_cdk_hears_off(self) -> None:
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(set(self.cdk_instances()), {"off"})
        self.assertIn("deploy: compute instances off (HAMH_COMPUTE_INSTANCES not set, the default); the stage has no Instances capacity provider", proc.stdout)
        self.assertEqual([c["runtime"] for c in self.deploy_record()["measurements"]["runtimes"]], ["WorkerPython"])

    def test_on_deploys_the_second_runtime_and_checks_it_on_the_provider_with_the_landlock_word(self) -> None:
        proc = self.run_deploy("--stage", "direct", HAMH_COMPUTE_INSTANCES="on")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(set(self.cdk_instances()), {"on"})
        self.assertIn("CREATES the capacity provider", proc.stdout)
        self.assertIn(f"deploy: runtime {self.IRID} live=3 capacity-provider=ok platform=V1 requireMMDSV2=true AGENTD_ENGINE_SANDBOX=landlock", proc.stdout)
        self.assertIn(f"deploy: runtime {self.WORKER_RID} live=3 vpc=ok platform=V1 requireMMDSV2=true AGENTD_ENGINE_SANDBOX=uid", proc.stdout)
        rec = self.deploy_record()
        self.assertEqual(sorted(c["runtime"] for c in rec["measurements"]["runtimes"]), ["WorkerPython", "WorkerPythonInstances"])
        self.assertEqual(rec["inputs"]["computeInstances"], {"setting": "on", "mode": "on", "live": False, "change": True})
        # Both runtimes get their `v<N>` endpoint for the version `live` serves (a run pins one).
        created = [c for c in self.log("aws") if c["args"][:2] == ["bedrock-agentcore-control", "create-agent-runtime-endpoint"]]
        self.assertEqual(sorted(c["args"][c["args"].index("--agent-runtime-id") + 1] for c in created), sorted([self.WORKER_RID, self.IRID]))

    def test_on_a_runtime_that_does_not_run_on_the_provider_fails(self) -> None:
        proc = self.run_deploy("--stage", "direct", HAMH_COMPUTE_INSTANCES="on", FAKE_INSTANCES_NO_PROVIDER="1")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertRegex(self.last(proc), rf"^DEPLOY-FAIL runtime-flags {self.IRID}:networkMode=VPC {self.IRID}:capacityProvider=None$")
        self.assertEqual(self.digests_written(), {})

    def test_a_runtime_that_serves_the_wrong_isolation_word_fails_on_either_side(self) -> None:
        for env, rid, got in ((dict(FAKE_INSTANCES_SANDBOX="uid"), self.IRID, "uid"), (dict(FAKE_SANDBOX="landlock"), self.WORKER_RID, "landlock")):
            with self.subTest(env=env):
                proc = self.run_deploy("--stage", "direct", HAMH_COMPUTE_INSTANCES="on", **env)
                self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
                self.assertEqual(self.last(proc), f"DEPLOY-FAIL runtime-flags {rid}:sandbox={got}")
                self.assertEqual(self.digests_written(), {})

    # infra-iam-2, infra-lifecycle-1, infra-defaults-3: turning the setting from on to off cannot complete in CloudFormation (the security stack update deletes the operator
    # role first, then the runtimes update fails), and it would end every machine and session on the provider. A deploy does not remove the Instances part: it refuses
    # before it builds or deploys anything, and the removal is the stage teardown.
    def drop_output(self, stack: str, key: str) -> None:
        path = self.state / f"stack-hamh-direct-{stack}"
        doc = json.loads(path.read_text())
        doc["outputs"].pop(key)
        path.write_text(json.dumps(doc))

    def lambda_builds(self) -> int:
        return len([c for c in self.log("cargo") if c[:2] == ["lambda", "build"]])

    def test_off_after_on_is_refused_before_anything_is_built_or_deployed_and_names_the_way_out(self) -> None:
        first = self.run_deploy("--stage", "direct", HAMH_COMPUTE_INSTANCES="on")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        calls, builds, listed = len(self.log("node")), self.lambda_builds(), len(self.listed())
        # default, off and OFF with spaces all resolve to off; none removes the Instances part.
        for env in ({"HAMH_COMPUTE_INSTANCES": "default"}, {"HAMH_COMPUTE_INSTANCES": "off"}, {"HAMH_COMPUTE_INSTANCES": " OFF "}):
            again = self.run_deploy("--stage", "direct", **env)
            line = self.assert_refused(again, "instances-off")
            for needle in ("capacity provider", "ends every machine and session", "separate, explicit step", "stage teardown", "HAMH_COMPUTE_INSTANCES=on"):
                self.assertIn(needle, line, env)
            self.assertNotIn("DEPLOY-PASS", again.stdout)
        self.assertEqual(len(self.log("node")), calls, "no cdk call: not a synth, not a deploy")
        self.assertEqual(self.lambda_builds(), builds, "no build")
        self.assertEqual(len(self.listed()), listed, "the guard is not even asked: waiting for the runs to end would not help")
        self.assertIn("WorkerPythonInstancesRuntimeId", json.loads((self.state / "stack-hamh-direct-runtimes").read_text())["outputs"], "the stage is as it was")

    def test_the_refusal_comes_first_whether_or_not_runs_are_active(self) -> None:
        first = self.run_deploy("--stage", "direct", HAMH_COMPUTE_INSTANCES="on")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        listed = len(self.listed())  # adding the part asked the guard once
        self.running("01K5RUNA", "01K5RUNB")
        again = self.run_deploy("--stage", "direct", HAMH_COMPUTE_INSTANCES="off")
        self.assert_refused(again, "instances-off")
        self.assertEqual(len(self.listed()), listed, "not the guard's refusal")
        # With a bump as well.
        bump = self.run_deploy("--stage", "direct", "--bump-worker-version", HAMH_COMPUTE_INSTANCES="off")
        self.assert_refused(bump, "instances-off")
        self.assertEqual(self.ssm()["/hamh/direct/worker-revision"], "0", "the revision is not bumped")

    def test_the_refusal_holds_when_only_the_runtime_or_only_the_provider_is_left(self) -> None:
        for gone in ("InstancesCapacityProviderArn", "WorkerPythonInstancesRuntimeId"):
            with self.subTest(gone=gone):
                self.setUp()
                self.assertEqual(self.run_deploy("--stage", "direct", HAMH_COMPUTE_INSTANCES="on").returncode, 0)
                self.drop_output("runtimes", gone)
                again = self.run_deploy("--stage", "direct", HAMH_COMPUTE_INSTANCES="off")
                self.assert_refused(again, "instances-off")

    def test_on_after_on_and_off_after_off_are_not_refused_and_check_guard_ignores_the_setting(self) -> None:
        self.assertEqual(self.run_deploy("--stage", "direct", HAMH_COMPUTE_INSTANCES="on").returncode, 0)
        again = self.run_deploy("--stage", "direct", HAMH_COMPUTE_INSTANCES="on")
        self.assertEqual(again.returncode, 0, again.stdout + again.stderr)
        self.assertNotIn("instances-off", again.stdout)
        check = self.run_deploy("--stage", "direct", "--check-guard")
        self.assertEqual(self.last(check), "DEPLOY-GUARD PASS stage=direct running=0")
        self.setUp()
        self.assertEqual(self.run_deploy("--stage", "direct").returncode, 0)
        self.assertEqual(self.run_deploy("--stage", "direct").returncode, 0)

    # infra-defaults-4: adding the Instances part meets the guard, before the Lambda build, and a refusal builds and deploys nothing. (Removing it is refused above, before the guard.)
    def order(self) -> list[str]:
        p = self.state / "order.log"
        rows = [json.loads(line) for line in p.read_text().splitlines()] if p.exists() else []
        return [r["what"] for r in sorted(rows, key=lambda r: r["n"])]

    def test_adding_the_instances_part_while_runs_are_active_is_refused_by_the_guard_before_the_lambda_build(self) -> None:
        first = self.run_deploy("--stage", "direct")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        self.running("01K5RUNA")
        calls, builds = len(self.log("node")), self.lambda_builds()
        proc = self.run_deploy("--stage", "direct", HAMH_COMPUTE_INSTANCES="on")
        self.assert_refused(proc, "guard", "1 running run(s) on hamh-direct-run pin the worker runtime: 01K5RUNA.")
        self.assertIn("CREATES the capacity provider", proc.stdout, "the deploy says what it would do before the guard refuses")
        self.assertEqual(self.lambda_builds(), builds, "no build")
        self.assertEqual(len(self.log("node")), calls, "no synth, no deploy")
        self.assertEqual(self.ssm()["/hamh/direct/worker-revision"], "0")
        # Once the runs end, the same deploy goes through.
        self.running()
        ok = self.run_deploy("--stage", "direct", HAMH_COMPUTE_INSTANCES="on")
        self.assertEqual(ok.returncode, 0, ok.stdout + ok.stderr)

    def test_adding_the_instances_part_and_a_bump_or_a_new_tools_mode_together_consult_the_guard_once(self) -> None:
        for extra, env in ((("--bump-worker-version",), {}), ((), {"HAMH_TOOLS_MODE": "on"})):
            with self.subTest(extra=extra, env=env):
                self.setUp()
                first = self.run_deploy("--stage", "direct")
                self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
                before = len(self.listed())
                ok = self.run_deploy("--stage", "direct", *extra, HAMH_COMPUTE_INSTANCES="on", **env)
                self.assertEqual(ok.returncode, 0, ok.stdout + ok.stderr)
                self.assertEqual(len(self.listed()) - before, 1)

    def test_the_guard_is_asked_before_the_lambda_build_and_once(self) -> None:
        first = self.run_deploy("--stage", "direct")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        mark = len(self.order())
        ok = self.run_deploy("--stage", "direct", HAMH_COMPUTE_INSTANCES="on")
        self.assertEqual(ok.returncode, 0, ok.stdout + ok.stderr)
        second = self.order()[mark:]
        self.assertEqual(second.count("list-executions"), 1, second)
        self.assertEqual(second.count("lambda-build"), 1, second)
        self.assertLess(second.index("list-executions"), second.index("lambda-build"))

    # infra-iam-4, infra-lifecycle-2, infra-defaults-1: the isolation words are part of each worker runtime's environment, and agentd refuses to start the engine
    # without the right ones. A live runtime that lacks a word or carries another one gets a new runtime version from this deploy, so it meets the guard like a
    # bump does, before the Lambda build. The microVM runtime needs AGENTD_ENGINE_SANDBOX=uid; the Instances runtime landlock and AGENTD_ENGINE_USER set to the empty string.
    def live_env(self, rid: str, **changes: str | None) -> None:
        """Rewrites the environment of the live version of runtime `rid` in the fake account (None removes a variable): a stage as an older deploy left it."""
        path = self.state / f"runtime-{rid}"
        doc = json.loads(path.read_text())
        env = doc.setdefault("environmentVariables", {})
        for k, v in changes.items():
            if v is None:
                env.pop(k, None)
            else:
                env[k] = v
        path.write_text(json.dumps(doc))

    def isolation_lines(self, proc: subprocess.CompletedProcess) -> list[str]:
        return [ln for ln in proc.stdout.splitlines() if ln.startswith("deploy: isolation words")]

    def test_a_live_microvm_runtime_without_the_isolation_word_meets_the_guard_before_the_lambda_build(self) -> None:
        first = self.run_deploy("--stage", "direct")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        self.live_env(self.WORKER_RID, AGENTD_ENGINE_SANDBOX=None)
        self.running("01K5RUNA")
        calls, builds = len(self.log("node")), self.lambda_builds()
        proc = self.run_deploy("--stage", "direct")
        self.assert_refused(proc, "guard", "1 running run(s) on hamh-direct-run ")
        self.assertEqual(self.lambda_builds(), builds, "no build")
        self.assertEqual(len(self.log("node")), calls, "no synth, no deploy")
        line = self.isolation_lines(proc)[0]
        self.assertIn("WorkerPython", line)
        self.assertIn("no AGENTD_ENGINE_SANDBOX", line)
        self.assertIn("wants uid", line)
        self.running()
        ok = self.run_deploy("--stage", "direct")
        self.assertEqual(ok.returncode, 0, ok.stdout + ok.stderr)
        rec = self.deploy_record()["inputs"]["isolationWords"]
        self.assertTrue(rec["change"], rec)
        self.assertEqual(rec["differs"], {"WorkerPython": {"AGENTD_ENGINE_SANDBOX": {"live": None, "want": "uid"}}})

    def test_a_live_runtime_that_carries_another_word_meets_the_guard_too(self) -> None:
        for rid, changes in ((self.WORKER_RID, {"AGENTD_ENGINE_SANDBOX": "landlock"}), (self.IRID, {"AGENTD_ENGINE_SANDBOX": "uid"}),
                             (self.IRID, {"AGENTD_ENGINE_USER": "agent"}), (self.IRID, {"AGENTD_ENGINE_USER": None})):
            with self.subTest(rid=rid, changes=changes):
                self.setUp()
                self.assertEqual(self.run_deploy("--stage", "direct", HAMH_COMPUTE_INSTANCES="on").returncode, 0)
                self.live_env(rid, **changes)
                self.running("01K5RUNA")
                builds = self.lambda_builds()
                proc = self.run_deploy("--stage", "direct", HAMH_COMPUTE_INSTANCES="on")
                self.assert_refused(proc, "guard", "1 running run(s)")
                self.assertEqual(self.lambda_builds(), builds, "no build")
                self.assertTrue(self.isolation_lines(proc), proc.stdout)

    def test_runtimes_with_the_right_words_are_not_guarded_and_the_record_says_no_change(self) -> None:
        for env in ({}, {"HAMH_COMPUTE_INSTANCES": "on"}):
            with self.subTest(env=env):
                self.setUp()
                self.assertEqual(self.run_deploy("--stage", "direct", **env).returncode, 0)
                # A fresh stage has no live runtime, so no word differs.
                self.assertEqual(self.deploy_record()["inputs"]["isolationWords"], {"change": False, "differs": {}})
                listed = len(self.listed())  # adding the Instances part to a fresh stage asked the guard once
                self.running("01K5RUNA")
                (self.root / "infra/lib/app.ts").write_text(f"// app {env}\n")
                git(self.root, "commit", "-q", "-am", "infra change")
                proc = self.run_deploy("--stage", "direct", **env)
                self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
                self.assertEqual(len(self.listed()), listed, "the guard is not consulted")
                self.assertEqual(self.isolation_lines(proc), [])
                self.assertEqual(self.deploy_record()["inputs"]["isolationWords"], {"change": False, "differs": {}})

    def test_the_instances_runtime_must_serve_the_empty_engine_user(self) -> None:
        for user, got in (("agent", "agent"), ("absent", "None")):
            with self.subTest(user=user):
                self.setUp()
                proc = self.run_deploy("--stage", "direct", HAMH_COMPUTE_INSTANCES="on", FAKE_INSTANCES_ENGINE_USER=user)
                self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
                self.assertEqual(self.last(proc), f"DEPLOY-FAIL runtime-flags {self.IRID}:engineUser={got}")
                self.assertEqual(self.digests_written(), {})
        self.setUp()
        ok = self.run_deploy("--stage", "direct", HAMH_COMPUTE_INSTANCES="on")
        self.assertEqual(ok.returncode, 0, ok.stdout + ok.stderr)
        self.assertIn("AGENTD_ENGINE_USER=''", ok.stdout)

    # infra-iam-3: a changed machine configuration is a new capacity provider (its name ends in a hash of the configuration), which CloudFormation creates next to the old one and
    # then deletes the old one, with the machines on it. That is a runtime change like a bump, so the deploy compares the live provider's name with the synthesized one and asks the
    # guard before any stack is deployed (the synth that makes the comparison needs the Lambda build, so this guard cannot come before it).
    PROVIDER_B = "hamh_direct_instances_bbbbbbbb"

    def provider_lines(self, proc: subprocess.CompletedProcess) -> list[str]:
        return [ln for ln in proc.stdout.splitlines() if ln.startswith("deploy: capacity provider")]

    def test_a_changed_provider_configuration_replaces_the_provider_and_meets_the_guard_before_any_stack_is_deployed(self) -> None:
        first = self.run_deploy("--stage", "direct", HAMH_COMPUTE_INSTANCES="on")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        self.running("01K5RUNA")
        deploys = len(self.deploys())
        proc = self.run_deploy("--stage", "direct", HAMH_COMPUTE_INSTANCES="on", HAMH_INSTANCES_TYPES="default", FAKE_PROVIDER_NAME=self.PROVIDER_B)
        self.assert_refused(proc, "guard", "1 running run(s) on hamh-direct-run pin the worker runtime: 01K5RUNA.")
        line = self.provider_lines(proc)[0]
        for needle in ("hamh_direct_instances_aaaaaaaa", self.PROVIDER_B, "REPLACES", "the machines on it"):
            self.assertIn(needle, line)
        self.assertEqual(len(self.deploys()), deploys, "no stack is deployed")
        self.running()
        ok = self.run_deploy("--stage", "direct", HAMH_COMPUTE_INSTANCES="on", HAMH_INSTANCES_TYPES="default", FAKE_PROVIDER_NAME=self.PROVIDER_B)
        self.assertEqual(ok.returncode, 0, ok.stdout + ok.stderr)

    def test_the_same_provider_name_again_is_not_guarded_for_this_reason(self) -> None:
        first = self.run_deploy("--stage", "direct", HAMH_COMPUTE_INSTANCES="on")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        listed = len(self.listed())
        self.running("01K5RUNA")
        (self.root / "infra/lib/app.ts").write_text("// app v2\n")
        git(self.root, "commit", "-q", "-am", "infra change")
        proc = self.run_deploy("--stage", "direct", HAMH_COMPUTE_INSTANCES="on")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(len(self.listed()), listed)
        self.assertEqual(self.provider_lines(proc), [])

    def test_a_replacement_together_with_another_cause_asks_the_guard_once(self) -> None:
        first = self.run_deploy("--stage", "direct", HAMH_COMPUTE_INSTANCES="on")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        before = len(self.listed())
        ok = self.run_deploy("--stage", "direct", "--bump-worker-version", HAMH_COMPUTE_INSTANCES="on", HAMH_INSTANCES_TYPES="default",
                             FAKE_PROVIDER_NAME=self.PROVIDER_B)
        self.assertEqual(ok.returncode, 0, ok.stdout + ok.stderr)
        self.assertEqual(len(self.listed()) - before, 1)

    def test_a_compute_instances_word_that_is_neither_off_nor_on_is_a_usage_error_before_any_aws_call(self) -> None:
        for bad in ("auto", "true", "instances"):
            proc = self.run_deploy("--stage", "direct", HAMH_COMPUTE_INSTANCES=bad)
            self.assertEqual(proc.returncode, 2, f"{bad}: {proc.stdout}{proc.stderr}")
            self.assertIn("HAMH_COMPUTE_INSTANCES", proc.stderr)
        self.assertEqual(self.log("aws"), [])
        self.assertEqual(self.log("node"), [])

    def cdk_modes(self) -> list:
        """HAMH_TOOLS_MODE as every cdk call (synth and deploy) of the run saw it."""
        return [c["HAMH_TOOLS_MODE"] for c in self.log("node") if c["args"][:2] in (["cdk", "synth"], ["cdk", "deploy"])]

    def tools_lines(self, proc: subprocess.CompletedProcess) -> list[str]:
        return [ln for ln in proc.stdout.splitlines() if ln.startswith("deploy: tools mode")]

    def live_mode(self) -> str | None:
        """AGENTD_TOOLS_MODE of the worker runtime in the fake account (what GetAgentRuntime would return)."""
        path = self.state / f"runtime-{self.WORKER_RID}"
        if not path.exists():
            return None
        return (json.loads(path.read_text()).get("environmentVariables") or {}).get("AGENTD_TOOLS_MODE")

    def test_a_tools_mode_that_is_neither_off_nor_on_is_a_usage_error_before_any_aws_call(self) -> None:
        for bad in ("auto", "true", "1", "enabled"):
            proc = self.run_deploy("--stage", "direct", HAMH_TOOLS_MODE=bad)
            self.assertEqual(proc.returncode, 2, f"{bad}: {proc.stdout}{proc.stderr}")
            self.assertIn("HAMH_TOOLS_MODE", proc.stderr)
            self.assertIn("off, on", proc.stderr)
        self.assertEqual(self.log("aws"), [])
        self.assertEqual(self.log("cargo"), [])
        self.assertEqual(self.log("node"), [])

    def test_every_cdk_call_gets_the_resolved_mode_whatever_the_caller_wrote(self) -> None:
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        modes = self.cdk_modes()
        self.assertTrue(modes)
        self.assertEqual(set(modes), {"off"}, "an unset setting reaches cdk as off, never as nothing")
        self.setUp()
        proc = self.run_deploy("--stage", "direct", HAMH_TOOLS_MODE=" ON ")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(set(self.cdk_modes()), {"on"}, "the word the script compared is the word cdk gets, normalized")
        self.assertEqual(self.live_mode(), "on")

    def test_the_deploy_says_the_mode_and_the_record_keeps_it(self) -> None:
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(len(self.tools_lines(proc)), 1, proc.stdout)
        line = self.tools_lines(proc)[0]
        for needle in ("tools mode off", "HAMH_TOOLS_MODE not set", "no live runtime yet"):
            self.assertIn(needle, line)
        self.assertEqual(self.deploy_record()["inputs"]["toolsMode"], {
            "setting": None, "mode": "off", "previous": None, "liveRuntime": False, "change": False, "stageKind": "test"})
        self.setUp()
        proc = self.run_deploy("--stage", "direct", HAMH_TOOLS_MODE="on")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        line = self.tools_lines(proc)[0]
        for needle in ("tools mode on", "HAMH_TOOLS_MODE=on", "no live runtime yet"):
            self.assertIn(needle, line)
        self.assertEqual(self.deploy_record()["inputs"]["toolsMode"], {
            "setting": "on", "mode": "on", "previous": None, "liveRuntime": False, "change": False, "stageKind": "test"})
        self.assertEqual(self.last(proc).split()[0], "DEPLOY-PASS", "the pass line keeps its shape")

    def test_the_official_stage_with_the_layer_on_says_so_and_is_allowed(self) -> None:
        proc = self.acct_two(HAMH_TOOLS_MODE="on")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertTrue(any("keep it on only when the sample projects pass" in ln for ln in self.tools_lines(proc)), proc.stdout)
        self.assertEqual(self.deploy_record()["inputs"]["toolsMode"]["stageKind"], "official")
        self.assertEqual(self.live_mode(), "on")
        self.setUp()
        proc = self.acct_two()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertFalse(any("keep it on only when the sample projects pass" in ln for ln in self.tools_lines(proc)), "off is the quiet default")
        self.assertEqual(self.live_mode(), "off")

    def test_a_first_deploy_has_no_runtime_to_compare_so_the_mode_is_not_guarded(self) -> None:
        self.running("01K5RUNA")
        proc = self.run_deploy("--stage", "direct", HAMH_TOOLS_MODE="on")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.listed(), [])

    def test_turning_the_layer_on_while_runs_are_active_is_refused_before_it_builds_or_deploys(self) -> None:
        first = self.run_deploy("--stage", "direct")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        self.assertEqual(self.live_mode(), "off")
        self.running("01K5RUNA")
        n, builds = len(self.deploys()), len([c for c in self.log("cargo") if c[:2] == ["lambda", "build"]])
        records = len(list(self.records.glob("*-deploy-direct-*.json")))
        proc = self.run_deploy("--stage", "direct", HAMH_TOOLS_MODE="on")
        self.assert_refused(proc, "guard", "1 running run(s) on hamh-direct-run pin the worker runtime: 01K5RUNA.")
        self.assertEqual(len(self.deploys()), n, "no stack deploys")
        self.assertEqual(len([c for c in self.log("cargo") if c[:2] == ["lambda", "build"]]), builds, "no build starts: the refusal costs seconds")
        self.assertEqual(len(list(self.records.glob("*-deploy-direct-*.json"))), records)
        self.assertEqual(self.live_mode(), "off")
        self.assertIn("CHANGES", self.tools_lines(proc)[0], "the deploy says it would change the live runtime before it refuses")
        self.running()
        ok = self.run_deploy("--stage", "direct", HAMH_TOOLS_MODE="on")
        self.assertEqual(ok.returncode, 0, ok.stdout + ok.stderr)
        self.assertEqual(self.live_mode(), "on")
        self.assertEqual(self.deploy_record()["inputs"]["toolsMode"], {
            "setting": "on", "mode": "on", "previous": "off", "liveRuntime": True, "change": True, "stageKind": "test"})

    def test_turning_the_layer_off_is_guarded_too_and_a_deploy_that_forgets_the_setting_keeps_it(self) -> None:
        first = self.run_deploy("--stage", "direct", HAMH_TOOLS_MODE="on")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        self.running("01K5RUNA", "01K5RUNB")
        for env in ({"HAMH_TOOLS_MODE": "off"}, {"HAMH_TOOLS_MODE": "default"}):
            proc = self.run_deploy("--stage", "direct", **env)
            self.assert_refused(proc, "guard", "2 running run(s) on hamh-direct-run ")
            line = self.tools_lines(proc)[0]
            self.assertRegex(line, r"tools mode off .*live runtime has AGENTD_TOOLS_MODE=on.*CHANGES")
        self.assertEqual(self.live_mode(), "on", "a refused deploy leaves the layer as it was")
        # A deploy that forgets the setting keeps the stage's mode (its settings record), so it changes no runtime and the running runs do not stop it.
        kept = self.run_deploy("--stage", "direct")
        self.assertEqual(kept.returncode, 0, kept.stdout + kept.stderr)
        self.assertNotIn("CHANGES", self.tools_lines(kept)[0])
        self.assertEqual(self.live_mode(), "on")
        self.running()
        ok = self.run_deploy("--stage", "direct", HAMH_TOOLS_MODE="off")
        self.assertEqual(ok.returncode, 0, ok.stdout + ok.stderr)
        self.assertEqual(self.live_mode(), "off")
        self.assertEqual(self.deploy_record()["inputs"]["toolsMode"], {
            "setting": "off", "mode": "off", "previous": "on", "liveRuntime": True, "change": True, "stageKind": "test"})

    def test_the_same_mode_again_is_not_guarded(self) -> None:
        for env in ({}, {"HAMH_TOOLS_MODE": "on"}):
            self.setUp()
            first = self.run_deploy("--stage", "direct", **env)
            self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
            self.running("01K5RUNA")
            (self.root / "infra/lib/app.ts").write_text(f"// app {env}\n")
            git(self.root, "commit", "-q", "-am", "infra change")
            proc = self.run_deploy("--stage", "direct", **env)
            self.assertEqual(proc.returncode, 0, f"{env}: {proc.stdout}{proc.stderr}")
            self.assertEqual(self.listed(), [], f"{env}: the guard is not consulted")
            self.assertNotIn("CHANGES", self.tools_lines(proc)[0])
            self.assertFalse(self.deploy_record()["inputs"]["toolsMode"]["change"])

    def test_a_runtime_that_has_no_mode_yet_counts_as_a_different_one(self) -> None:
        # A stage deployed before the setting existed: the first deploy after it adds the variable, which is a new runtime version.
        first = self.run_deploy("--stage", "direct", FAKE_RUNTIME_ENV="legacy")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        self.assertIsNone(self.live_mode())
        self.running("01K5RUNA")
        proc = self.run_deploy("--stage", "direct")
        self.assert_refused(proc, "guard", "1 running run(s)")
        self.assertIn("no AGENTD_TOOLS_MODE", self.tools_lines(proc)[0])
        self.running()
        ok = self.run_deploy("--stage", "direct")
        self.assertEqual(ok.returncode, 0, ok.stdout + ok.stderr)
        self.assertEqual(self.live_mode(), "off")
        self.assertEqual(self.deploy_record()["inputs"]["toolsMode"], {
            "setting": None, "mode": "off", "previous": None, "liveRuntime": True, "change": True, "stageKind": "test"})

    def test_a_new_image_and_a_new_mode_together_consult_the_guard_once_and_before_the_image_build(self) -> None:
        first = self.run_deploy("--stage", "direct")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        (self.root / "crates/agentd/src/main.rs").write_text("fn main() { let _ = 2; }\n")
        git(self.root, "commit", "-q", "-am", "agentd change")
        self.running("01K5RUNA")
        proc = self.run_deploy("--stage", "direct", HAMH_TOOLS_MODE="on")
        self.assert_refused(proc, "guard")
        self.assertEqual(len(self.listed()), 1)
        self.assertEqual(len(self.worker_builds()), 1, "the new image is not built")
        self.running()
        before = len(self.listed())
        ok = self.run_deploy("--stage", "direct", HAMH_TOOLS_MODE="on")
        self.assertEqual(ok.returncode, 0, ok.stdout + ok.stderr)
        self.assertEqual(len(self.listed()) - before, 1, "one deploy asks the guard once, for both causes")
        self.assertEqual(len(self.worker_builds()), 2)

    def test_a_bump_and_a_new_mode_together_consult_the_guard_once(self) -> None:
        first = self.run_deploy("--stage", "direct")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        before = len(self.listed())
        ok = self.run_deploy("--stage", "direct", "--bump-worker-version", HAMH_TOOLS_MODE="on")
        self.assertEqual(ok.returncode, 0, ok.stdout + ok.stderr)
        self.assertEqual(len(self.listed()) - before, 1)

    def test_a_live_runtime_that_returns_no_image_fails_the_deploy_by_name_before_any_build(self) -> None:
        # The live read now happens before the Lambda build, so a GetAgentRuntime answer without an image must end the deploy as a named
        # failure and never as a Python traceback.
        first = self.run_deploy("--stage", "direct")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        builds = len([c for c in self.log("cargo") if c[:2] == ["lambda", "build"]])
        proc = self.run_deploy("--stage", "direct", FAKE_RUNTIME_NO_ARTIFACT="1")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertRegex(self.last(proc), r"^DEPLOY-FAIL live-runtime ")
        self.assertNotIn("Traceback", proc.stdout + proc.stderr)
        self.assertEqual(len([c for c in self.log("cargo") if c[:2] == ["lambda", "build"]]), builds, "no build starts")

    def test_the_mode_moves_the_deployment_digest_and_the_same_mode_keeps_it(self) -> None:
        # The digest hashes the deployed templates, and the runtimes template carries the mode (the fake CDK models that).
        off = self.run_deploy("--stage", "direct")
        self.assertEqual(off.returncode, 0, off.stdout + off.stderr)
        self.setUp()
        on = self.run_deploy("--stage", "direct", HAMH_TOOLS_MODE="on")
        self.assertEqual(on.returncode, 0, on.stdout + on.stderr)
        self.setUp()
        again = self.run_deploy("--stage", "direct", HAMH_TOOLS_MODE="off")
        self.assertEqual(again.returncode, 0, again.stdout + again.stderr)
        self.assertNotEqual(self.last(off), self.last(on))
        self.assertEqual(self.last(off), self.last(again))

    def test_check_guard_ignores_the_mode(self) -> None:
        proc = self.run_deploy("--stage", "direct", "--check-guard", HAMH_TOOLS_MODE="on")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.last(proc), "DEPLOY-GUARD PASS stage=direct running=0")
        self.assertEqual(self.log("node"), [])

    # ---- version endpoints -----------------------------------

    WORKER_RID = "hamh_direct_harness_worker_python-abc"

    def endpoints(self, rid: str = WORKER_RID) -> dict:
        p = self.state / f"endpoints-{rid}.json"
        return json.loads(p.read_text()) if p.exists() else {}

    def seed_endpoints(self, live: str, versions: list[int], rid: str = WORKER_RID) -> None:
        eps = {"live": {"liveVersion": live, "status": "READY"}, "DEFAULT": {"liveVersion": live, "status": "READY"}}
        for v in versions:
            eps[f"v{v}"] = {"liveVersion": str(v), "targetVersion": str(v), "status": "READY"}
        (self.state / f"endpoints-{rid}.json").write_text(json.dumps(eps))

    def endpoint_calls(self, op: str) -> list[list[str]]:
        return [c["args"] for c in self.log("aws") if c["args"][:2] == ["bedrock-agentcore-control", op]]

    def test_a_deploy_creates_the_endpoint_of_the_live_worker_version_tagged_and_waits_until_it_is_ready(self) -> None:
        # AgentCore's qualifier is an endpoint name; runs pin `v<N>`, so it must exist and be READY.
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        creates = self.endpoint_calls("create-agent-runtime-endpoint")
        # The stage has one worker runtime, and it is the one a run pins.
        self.assertEqual(len(creates), 1, "one worker runtime")
        c = creates[0]
        arg = lambda n: c[c.index(n) + 1]  # noqa: E731
        self.assertEqual((arg("--agent-runtime-id"), arg("--name"), arg("--agent-runtime-version")), (self.WORKER_RID, "v3", "3"))
        self.assertEqual(json.loads(arg("--tags")), {"project": "hyperscale-agent-meta-harness", "stage": "direct", "owner": OWNER_OF[ACCOUNT],
                                                     "managed-by": "deploy.sh"})
        self.assertEqual((self.endpoints()["v3"]["status"], self.endpoints()["v3"]["liveVersion"]), ("READY", "3"))
        self.assertIn(f"deploy: endpoint v3 on {self.WORKER_RID} serves version 3 (created)", proc.stdout)
        self.assertEqual(self.deploy_record()["measurements"]["versionEndpoints"],
                         [{"runtime": "WorkerPython", "runtimeId": self.WORKER_RID, "endpoint": "v3", "version": "3",
                           "created": True, "kept": ["v3"], "deleted": []}])
        self.assertRegex(self.last(proc), r"^DEPLOY-PASS stage=direct ")
        # A rerun finds it and creates nothing.
        again = self.run_deploy("--stage", "direct")
        self.assertEqual(again.returncode, 0, again.stdout + again.stderr)
        self.assertEqual(len(self.endpoint_calls("create-agent-runtime-endpoint")), 1)
        self.assertIn(f"deploy: endpoint v3 on {self.WORKER_RID} serves version 3 (present)", again.stdout)

    def test_old_version_endpoints_go_but_the_rollback_target_and_those_running_runs_pin_stay(self) -> None:
        first = self.run_deploy("--stage", "direct")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        self.seed_endpoints("5", [1, 2, 3, 4, 5])
        # Two runs are in flight: one pinned to v1, one that fell back to live.
        self.running("01K5RUNOLD", "01K5RUNLIVE")
        (self.state / "pins.json").write_text(json.dumps({"01K5RUNOLD": "v1", "01K5RUNLIVE": "live"}))
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(sorted(self.endpoints()), ["DEFAULT", "live", "v1", "v4", "v5"])
        deleted = [c[c.index("--endpoint-name") + 1] for c in self.endpoint_calls("delete-agent-runtime-endpoint")]
        self.assertEqual(deleted, ["v2", "v3"])
        self.assertIn("deploy: endpoint v2 deleted (no running run pins it)", proc.stdout)
        self.assertEqual(self.deploy_record()["measurements"]["versionEndpoints"][0]["kept"], ["v5", "v4", "v1"])
        reads = [c["args"] for c in self.log("aws") if c["args"][:2] == ["dynamodb", "get-item"]]
        # Only the two running runs' Run items are read.
        self.assertEqual({json.loads(r[r.index("--key") + 1])["PK"]["S"] for r in reads}, {"RUN#01K5RUNOLD", "RUN#01K5RUNLIVE"})
        self.assertTrue(all(r[r.index("--table-name") + 1] == "hamh-direct-harness" and "--consistent-read" in r for r in reads))

    def test_an_endpoint_that_does_not_become_ready_fails_the_deploy_before_the_digests(self) -> None:
        proc = self.run_deploy("--stage", "direct", FAKE_ENDPOINT_ENDS="CREATE_FAILED")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertEqual(self.last(proc), f"DEPLOY-FAIL version-endpoint {self.WORKER_RID} v3 CREATE_FAILED")
        self.assertNotIn("/hamh/direct/deployment-digest", self.ssm())

    def test_every_aws_call_uses_the_history_entry_profile(self) -> None:
        self.run_deploy("--stage", "direct")
        self.assertEqual({c["profile"] for c in self.log("aws")}, {"history-profile"})

    def test_a_runtime_without_requireMMDSV2_fails_and_writes_no_digest(self) -> None:
        proc = self.run_deploy("--stage", "direct", FAKE_MMDS="false")
        self.assertEqual(proc.returncode, 1)
        self.assertRegex(self.last(proc), r"^DEPLOY-FAIL runtime-flags .*requireMMDSV2")
        self.assertEqual(self.digests_written(), {})

    def test_a_runtime_outside_vpc_mode_fails(self) -> None:
        proc = self.run_deploy("--stage", "direct", FAKE_NETWORK_MODE="PUBLIC")
        self.assertEqual(proc.returncode, 1)
        self.assertRegex(self.last(proc), r"^DEPLOY-FAIL runtime-flags .*networkMode")

    def test_a_worker_runtime_on_another_image_fails_and_writes_no_digest(self) -> None:
        # The runtime check compares the live version's image with the image the deploy built (the converted case of the
        # old line's evaluator and family runtime checks: the stage has the one worker runtime).
        proc = self.run_deploy("--stage", "direct", FAKE_STALE_RUNTIME="hamh_direct_harness_worker_python")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertRegex(self.last(proc), r"^DEPLOY-FAIL runtime-flags hamh_direct_harness_worker_python-abc:image=")
        self.assertEqual(self.digests_written(), {})

    def test_a_runtime_the_deploy_has_no_image_for_fails(self) -> None:
        # A runtime of the runtimes stack that the deploy has no image for serves an image of its own.
        (self.state / "runtime-hamh_direct_harness_extra-jkl").write_text(json.dumps({"image": "x@sha256:" + "1" * 64}))
        proc = self.run_deploy("--stage", "direct", FAKE_EXTRA_RUNTIME="Extra=hamh_direct_harness_extra-jkl")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertEqual(self.last(proc), "DEPLOY-FAIL runtime-flags hamh_direct_harness_extra-jkl:no-deployed-image")
        self.assertEqual(self.digests_written(), {})

    def test_image_variables_from_the_callers_environment_never_reach_cdk(self) -> None:
        stale = f"{ACCOUNT}.dkr.ecr.us-west-2.amazonaws.com/hamh-direct-delivery@sha256:" + "f" * 64
        proc = self.run_deploy("--stage", "direct", HAMH_WORKER_IMAGE=stale, HAMH_DELIVERY_IMAGE=stale)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        for c in self.log("node"):
            self.assertNotEqual(c.get("HAMH_WORKER_IMAGE"), stale, c["args"])
            self.assertNotEqual(c.get("HAMH_DELIVERY_IMAGE"), stale, c["args"])
        phase_1 = self.deploys()[:9]
        self.assertEqual({c["HAMH_WORKER_IMAGE"] for c in phase_1}, {None})
        self.assertEqual({c["HAMH_DELIVERY_IMAGE"] for c in phase_1}, {None})

    def test_a_failed_stack_stops_the_deploy_and_names_the_stack(self) -> None:
        proc = self.run_deploy("--stage", "direct", FAKE_FAIL_STACK="hamh-direct-data")
        self.assertEqual(proc.returncode, 1)
        self.assertEqual(self.last(proc), "DEPLOY-FAIL stack hamh-direct-data")
        self.assertEqual([c["args"][2] for c in self.deploys()], ["hamh-direct-network", "hamh-direct-security", "hamh-direct-data"])
        self.assertEqual(self.digests_written(), {})

    def test_a_failed_image_build_fails_the_deploy(self) -> None:
        proc = self.run_deploy("--stage", "direct", FAKE_CODEBUILD_RC="1")
        self.assertEqual(proc.returncode, 1)
        self.assertEqual(self.last(proc), "DEPLOY-FAIL image-build codebuild-run exit 1")

    def test_a_first_pass_that_deployed_every_stack_and_an_image_build_that_fails_leave_no_deployment_digest(self) -> None:
        # setup.sh (scripts/setup/preflight.sh, pf_stage) reads /hamh/<stage>/deployment-digest as the end of the deploy: a stage whose stacks all exist but that has
        # none is partial, and --full deploys it again. The deploy writes the digest after the last stack, the runtime checks and the endpoints, never after the first pass.
        proc = self.run_deploy("--stage", "direct", FAKE_CODEBUILD_RC="1")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertEqual([c["args"][2] for c in self.deploys()], [f"hamh-direct-{s}" for s in STACKS], "the first pass deployed every stack")
        self.assertNotIn("/hamh/direct/deployment-digest", self.ssm())
        done = self.run_deploy("--stage", "direct")
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertIn("/hamh/direct/deployment-digest", self.ssm())

    def test_the_wrong_account_fails_before_any_deploy(self) -> None:
        proc = self.run_deploy("--stage", "direct", FAKE_ACCOUNT=OUTSIDE_ACCOUNT)
        self.assertEqual(self.assert_refused(proc, "account"), f"DEPLOY-REFUSED reason=account wait=no stage=unchanged :: the caller is in account {OUTSIDE_ACCOUNT}, "
                         f"not in account {ACCOUNT} of the selected entry (history-profile). Sign in to that account, then deploy again.")
        self.assertEqual(self.deploys(), [])

    def test_the_asset_objects_are_recorded_for_teardown(self) -> None:
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        key = f"s3://hamh-direct-harness-{ACCOUNT}/deploy/direct/cdk-assets.json".replace("/", "|")
        rec = json.loads((self.state / "s3" / key).read_text())
        self.assertIn({"bucket": f"cdk-hnb659fds-assets-{ACCOUNT}-us-west-2", "key": "abc.zip"}, rec["files"])

    def test_an_evidence_record_carries_the_header_and_the_checks(self) -> None:
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        files = sorted(self.records.glob("*-deploy-direct-*.json"))
        self.assertEqual(len(files), 1)
        rec = json.loads(files[0].read_text())
        for field in ("schemaVersion", "kind", "createdAt", "host", "account", "region", "stage", "codeSha", "sourceTree",
                      "deploymentDigest", "imageDigest", "criteriaSha256", "result", "inputs", "measurements", "runIds", "notes"):
            self.assertIn(field, rec)
        self.assertEqual((rec["kind"], rec["stage"], rec["result"]), ("deploy", "direct", "pass"))
        self.assertEqual(rec["deploymentDigest"], self.ssm()["/hamh/direct/deployment-digest"])
        self.assertEqual(rec["inputs"]["stacks"], [f"hamh-direct-{n}" for n in STACKS])
        self.assertEqual(rec["measurements"]["runtimes"][0]["requireMMDSV2"], True)
        self.assertEqual(len(rec["measurements"]["runtimes"]), 1, "the one worker runtime")
        self.assertEqual(rec["measurements"]["azs"], {"us-west-2a": "usw2-az1"})
        self.assertEqual(rec["measurements"]["vpcCidr"], {"cidr": "10.50.0.0/16", "overlaps": []})

    def invocation_logging_puts(self) -> list[dict]:
        return [json.loads(c["args"][c["args"].index("--logging-config") + 1]) for c in self.log("aws")
                if c["args"][:2] == ["bedrock", "put-model-invocation-logging-configuration"]]

    def deploy_record(self) -> dict:
        return json.loads(sorted(self.records.glob("*-deploy-*.json"))[-1].read_text())

    def test_invocation_logging_is_enabled_when_unset_and_recorded(self) -> None:
        proc = self.run_deploy("--stage", "direct", HAMH_AWS_PROFILE="", FAKE_ACCOUNT=ACCT_TWO)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        puts = self.invocation_logging_puts()
        self.assertEqual(puts, [{
            "cloudWatchConfig": {"logGroupName": "/hamh/direct/bedrock-invocations",
                                 "roleArn": f"arn:aws:iam::{ACCT_TWO}:role/hamh-direct-bedrock-logging"},
            "textDataDeliveryEnabled": True, "imageDataDeliveryEnabled": False,
            "embeddingDataDeliveryEnabled": False, "videoDataDeliveryEnabled": False,
        }])
        self.assertEqual(self.deploy_record()["measurements"]["invocationLogging"],
                         {"outcome": "enabled", "logGroup": "/hamh/direct/bedrock-invocations", "prior": None})
        self.assertIn("invocation logging enabled", proc.stdout)
        self.assertEqual(json.loads(self.ssm()["/hamh/direct/invocation-logging-prior"]), {"loggingConfig": None})
        calls = [c["args"][:2] + [c["args"][c["args"].index("--name") + 1] if "--name" in c["args"] else ""] for c in self.log("aws")]
        prior = calls.index(["ssm", "put-parameter", "/hamh/direct/invocation-logging-prior"])
        self.assertLess(prior, calls.index(["bedrock", "put-model-invocation-logging-configuration", ""]))
        tags = [c for c in self.log("aws") if c["args"][:2] == ["ssm", "add-tags-to-resource"]
                and "/hamh/direct/invocation-logging-prior" in c["args"]]
        self.assertEqual(len(tags), 1)
        self.assertIn("Key=project,Value=hyperscale-agent-meta-harness", tags[0]["args"])

    def test_a_recorded_prior_state_is_never_overwritten(self) -> None:
        (self.state / "ssm.json").write_text(json.dumps({"/hamh/direct/invocation-logging-prior": '{"loggingConfig": null}'}))
        proc = self.run_deploy("--stage", "direct", HAMH_AWS_PROFILE="", FAKE_ACCOUNT=ACCT_TWO)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        writes = [c for c in self.log("aws") if c["args"][:2] == ["ssm", "put-parameter"]
                  and "/hamh/direct/invocation-logging-prior" in c["args"]]
        self.assertEqual(writes, [])
        self.assertIn("prior state already recorded", proc.stdout)

    def test_a_read_only_entry_never_writes_invocation_logging(self) -> None:
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.invocation_logging_puts(), [])
        self.assertNotIn("/hamh/direct/invocation-logging-prior", self.ssm())
        self.assertEqual(self.deploy_record()["measurements"]["invocationLogging"],
                         {"outcome": "unset-read-only", "logGroup": None})
        self.assertIn("invocation logging unset; history-profile is read-only (never written)", proc.stdout)

    def test_invocation_logging_that_points_elsewhere_is_read_and_never_overwritten(self) -> None:
        foreign = {"cloudWatchConfig": {"logGroupName": "/aws/bedrock/other/model-invocations",
                                        "roleArn": f"arn:aws:iam::{ACCOUNT}:role/other-logging"},
                   "textDataDeliveryEnabled": True}
        (self.state / "logging.json").write_text(json.dumps(foreign))
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.invocation_logging_puts(), [])
        self.assertEqual(json.loads((self.state / "logging.json").read_text()), foreign)
        self.assertEqual(self.deploy_record()["measurements"]["invocationLogging"],
                         {"outcome": "foreign", "logGroup": "/aws/bedrock/other/model-invocations"})

    def test_invocation_logging_that_is_already_ours_is_not_rewritten(self) -> None:
        ours = {"cloudWatchConfig": {"logGroupName": "/hamh/direct/bedrock-invocations",
                                     "roleArn": f"arn:aws:iam::{ACCOUNT}:role/hamh-direct-bedrock-logging"},
                "textDataDeliveryEnabled": True}
        (self.state / "logging.json").write_text(json.dumps(ours))
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.invocation_logging_puts(), [])
        self.assertEqual(self.deploy_record()["measurements"]["invocationLogging"]["outcome"], "ours")

    # ----: the target and a fresh account --------------------------------------------------

    def acct_two(self, **env: str) -> subprocess.CompletedProcess:
        """A deploy of the default target, the active active-entry entry."""
        return self.run_deploy("--stage", "direct", **{"HAMH_AWS_PROFILE": "", "FAKE_ACCOUNT": ACCT_TWO, **env})

    def test_the_active_entry_is_the_default_target(self) -> None:
        proc = self.acct_two()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn("profile=my-profile", proc.stdout)
        aws = self.log("aws")
        self.assertEqual({c["profile"] for c in aws}, {"my-profile"})
        regions = {c["args"][c["args"].index("--region") + 1] for c in aws if "--region" in c["args"]
                   and "--profile" not in c["args"]}
        self.assertEqual(regions, {"us-east-1"})
        joined = " ".join(self.worker_builds()[0])
        self.assertIn(f"HARNESS_BUCKET=hamh-direct-harness-{ACCT_TWO}", joined)
        self.assertIn(f"REPO_URI={ACCT_TWO}.dkr.ecr.us-east-1.amazonaws.com/hamh-direct-worker-python", joined)
        self.assertRegex(self.deploys()[-1]["HAMH_WORKER_IMAGE"], rf"^{ACCT_TWO}\.dkr\.ecr\.us-east-1\.amazonaws\.com/")
        rec = self.deploy_record()
        self.assertEqual((rec["account"], rec["region"]), (ACCT_TWO, "us-east-1"))
        self.assertEqual(rec["inputs"]["target"], {"account": ACCT_TWO, "region": "us-east-1", "profile": "my-profile", "role": "active"})

    def test_a_caller_outside_the_selected_entry_is_refused_before_any_build(self) -> None:
        proc = self.run_deploy("--stage", "direct", HAMH_AWS_PROFILE="")
        self.assert_refused(proc, "account", f"the caller is in account {ACCOUNT}, ")
        self.assertEqual(self.log("cargo"), [])

    def test_an_unknown_profile_is_a_usage_error_before_any_aws_call(self) -> None:
        proc = self.run_deploy("--stage", "direct", HAMH_AWS_PROFILE="default")
        self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
        self.assertIn("HAMH_AWS_PROFILE=default", proc.stderr)
        self.assertEqual(self.log("aws"), [])

    def test_each_stage_az_must_map_to_its_accounts_json_id(self) -> None:
        proc = self.acct_two()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn("deploy: azs us-east-1b=use1-az1\n", proc.stdout)
        self.setUp()
        proc = self.acct_two(FAKE_AZ_MAP=json.dumps({"us-east-1b": "use1-az6"}))
        self.assert_refused(proc, "azs", "us-east-1b=use1-az6 (accounts.json: use1-az1). ")
        self.assertEqual(self.log("cargo"), [])
        self.assertEqual(self.deploys(), [])

    def test_another_vpc_in_the_stage_range_fails_the_deploy_before_any_build_and_the_stages_own_does_not(self) -> None:
        # The VPC CIDR check, through the script and the fake EC2: an unrelated VPC in
        # 10.50.0.0/16 ends the deploy before a build; the stage's own tagged VPC (a redeploy) is not an overlap.
        other = [{"VpcId": "vpc-other", "CidrBlock": "10.50.4.0/24"}]
        proc = self.run_deploy("--stage", "direct", FAKE_VPCS=json.dumps(other))
        self.assert_refused(proc, "vpc-cidr", "10.50.0.0/16 overlaps vpc-other=10.50.4.0/24. ")
        self.assertEqual(self.log("cargo"), [])
        self.assertEqual(self.deploys(), [])
        self.setUp()
        own = [{"VpcId": "vpc-own", "CidrBlock": "10.50.0.0/16",
                "Tags": [{"Key": "project", "Value": "hyperscale-agent-meta-harness"}, {"Key": "stage", "Value": "direct"}]},
               {"VpcId": "vpc-default", "CidrBlock": "172.31.0.0/16"}]
        proc = self.run_deploy("--stage", "direct", FAKE_VPCS=json.dumps(own))
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn("deploy: vpc cidr 10.50.0.0/16 overlaps no other VPC of the account", proc.stdout)

    def test_an_existing_bootstrap_is_used_unchanged(self) -> None:
        proc = self.acct_two(FAKE_BOOTSTRAP_VERSION="18")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn("deploy: bootstrap version 18 used unchanged (required 6)", proc.stdout)
        self.assertEqual([c for c in self.log("node") if c["args"][:2] == ["cdk", "bootstrap"]], [])
        self.assertEqual(self.deploy_record()["measurements"]["bootstrap"], {"outcome": "existing", "version": 18, "required": 6})

    def test_a_bootstrap_below_the_requirement_fails_before_any_stack(self) -> None:
        proc = self.acct_two(FAKE_BOOTSTRAP_VERSION="18", FAKE_REQUIRED_BOOTSTRAP="21")
        self.assert_refused(proc, "bootstrap", "version 18 is below the required 21; the existing CDKToolkit is never modified. Upgrade the CDK bootstrap of "
                                                f"aws://{ACCT_TWO}/us-east-1 to version 21 or later")
        self.assertEqual(self.deploys(), [])

    def test_an_account_without_a_bootstrap_gets_one_with_the_project_tags(self) -> None:
        proc = self.acct_two(FAKE_BOOTSTRAP_VERSION="none")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        node = self.log("node")
        boots = [c for c in node if c["args"][:2] == ["cdk", "bootstrap"]]
        self.assertEqual(len(boots), 1)
        self.assertEqual(boots[0]["args"][:3], ["cdk", "bootstrap", f"aws://{ACCT_TWO}/us-east-1"])
        for tag in ("project=hyperscale-agent-meta-harness", "stage=shared", f"owner={OWNER_OF[ACCT_TWO]}", "managed-by=deploy.sh"):
            self.assertIn(tag, boots[0]["args"])
        self.assertLess(node.index(boots[0]), node.index(self.deploys()[0]))
        self.assertEqual(self.deploy_record()["measurements"]["bootstrap"], {"outcome": "created", "version": 30, "required": 6})

    def lock_with(self, artifact: str | None, content: bytes) -> str:
        sha = hashlib.sha256(content).hexdigest()
        lock = f'schema = "1"\n\n[cline]\nbinarySha256 = "{sha}"\n' + (f'artifact = "{artifact}"\n' if artifact else "")
        (self.root / "engine" / "lock.toml").write_text(lock)
        git(self.root, "commit", "-q", "-am", "lock")
        return sha

    def test_the_fork_binary_is_copied_from_the_account_that_holds_it(self) -> None:
        sha = self.lock_with(f"s3://hamh-direct-harness-{ACCOUNT}/engine/cline/X/cline", b"the-fork")
        self.seed_s3(f"s3://hamh-direct-harness-{ACCOUNT}/engine/cline/X/cline", "the-fork")
        proc = self.acct_two(FAKE_S3_OWNERS=json.dumps({f"hamh-direct-harness-{ACCOUNT}": "history-profile"}))
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        aws = self.log("aws")
        down = [c for c in aws if c["args"][:3] == ["s3", "cp", f"s3://hamh-direct-harness-{ACCOUNT}/engine/cline/X/cline"]]
        self.assertEqual(len(down), 1)
        self.assertEqual(down[0]["args"][down[0]["args"].index("--profile") + 1], "history-profile")
        self.assertEqual(down[0]["args"][down[0]["args"].index("--region") + 1], "us-west-2")
        up = [c for c in aws if c["args"][:2] == ["s3", "cp"] and len(c["args"]) > 3
              and c["args"][3] == f"s3://hamh-direct-harness-{ACCT_TWO}/engine/cline/{sha}/cline"]
        self.assertEqual(len(up), 1)
        self.assertIn(f"sha256={sha}", up[0]["args"])
        self.assertLess(aws.index(up[0]), len(aws))
        self.assertIn(f"deploy: fork binary {sha[:12]} copied from s3://hamh-direct-harness-{ACCOUNT}/engine/cline/X/cline", proc.stdout)
        self.assertEqual(self.deploy_record()["measurements"]["forkBinary"]["outcome"], "copied")
        builds = [" ".join(b) for b in self.log("codebuild")]
        self.assertTrue(any(f"CLINE_SHA256={sha}" in b for b in builds))
        self.assertEqual([b for b in builds if "cline-fork-arm64.yml" in b], [])

    def test_a_direct_stage_copies_the_fork_binary_from_the_retired_stage_of_the_same_account(self) -> None:
        # 2026-09-30: history-profile's drill stage, deployed after its live stage, rebuilt the fork on CodeBuild ARM
        # (other bytes: DEPLOY-FAIL fork-binary) because only another account's artifact was copied. The same holds for
        # stage `direct` in an account whose retired live stage bucket holds the promoted binary.
        sha = self.lock_with(f"s3://cah-live-harness-{ACCOUNT}/engine/cline/X/cline", b"the-fork")
        self.seed_s3(f"s3://cah-live-harness-{ACCOUNT}/engine/cline/X/cline", "the-fork")
        proc = self.run_deploy("--stage", "direct", "--azs", "1",
                               FAKE_S3_OWNERS=json.dumps({f"cah-live-harness-{ACCOUNT}": "history-profile"}))
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        aws = self.log("aws")
        down = [c for c in aws if c["args"][:3] == ["s3", "cp", f"s3://cah-live-harness-{ACCOUNT}/engine/cline/X/cline"]]
        self.assertEqual(len(down), 1)
        self.assertEqual(down[0]["args"][down[0]["args"].index("--profile") + 1], "history-profile")
        up = [c for c in aws if c["args"][:2] == ["s3", "cp"] and len(c["args"]) > 3
              and c["args"][3] == f"s3://hamh-direct-harness-{ACCOUNT}/engine/cline/{sha}/cline"]
        self.assertEqual(len(up), 1)
        self.assertIn(f"sha256={sha}", up[0]["args"])
        self.assertIn(f"deploy: fork binary {sha[:12]} copied from s3://cah-live-harness-{ACCOUNT}/engine/cline/X/cline "
                      "(profile history-profile)", proc.stdout)
        self.assertEqual([b for b in self.log("codebuild") if "cline-fork-arm64.yml" in b], [])

    def test_a_present_fork_binary_without_sha256_metadata_is_verified_by_its_bytes(self) -> None:
        # history-profile's promoted object (engine/cline/c07af1c8.../cline) carries no metadata; its bytes decide.
        sha = self.lock_with(None, b"promoted-bytes")
        self.seed_s3(f"s3://hamh-direct-harness-{ACCOUNT}/engine/cline/{sha}/cline", "promoted-bytes")
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn(f"deploy: fork binary {sha[:12]} present at s3://hamh-direct-harness-{ACCOUNT}/engine/cline/{sha}/cline "
                      "(sha256 of its bytes)", proc.stdout)
        self.assertEqual([b for b in self.log("codebuild") if "cline-fork-arm64.yml" in b], [])
        self.setUp()
        sha = self.lock_with(None, b"promoted-bytes")
        self.seed_s3(f"s3://hamh-direct-harness-{ACCOUNT}/engine/cline/{sha}/cline", "other-bytes")
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertRegex(self.last(proc), rf"^DEPLOY-FAIL fork-binary s3://hamh-direct-harness-{ACCOUNT}/engine/cline/{sha}/cline "
                                          r"has sha256 [0-9a-f]{12}, the lock pins [0-9a-f]{12}$")
        self.assertEqual(self.worker_builds(), [])

    def test_a_copied_fork_binary_with_another_sha256_fails_before_the_image_build(self) -> None:
        self.lock_with(f"s3://hamh-direct-harness-{ACCOUNT}/engine/cline/X/cline", b"the-fork")
        self.seed_s3(f"s3://hamh-direct-harness-{ACCOUNT}/engine/cline/X/cline", "tampered")
        proc = self.acct_two()
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertRegex(self.last(proc), rf"^DEPLOY-FAIL fork-binary s3://hamh-direct-harness-{ACCOUNT}/engine/cline/X/cline "
                                          r"has sha256 [0-9a-f]{12}, the lock pins [0-9a-f]{12}$")
        self.assertEqual(self.worker_builds(), [])

    # ----: the Cline fork of a fresh account is built from the pinned upstream source -------------

    BUILT = "rebuilt-fork"

    def built_sha(self, content: str = "rebuilt-fork") -> str:
        return hashlib.sha256(content.encode()).hexdigest()

    def fork_builds(self) -> list:
        return self.builds("images/codebuild/cline-fork-arm64.yml")

    def fork(self, mode: str | None = "build", acct: str = ACCT_TWO, **env: str) -> subprocess.CompletedProcess:
        """A deploy of the active entry with `--fork-source <mode>` (None: the default, auto)."""
        args = ["--stage", "direct"] + (["--fork-source", mode] if mode else [])
        return self.run_deploy(*args, **{"HAMH_AWS_PROFILE": "", "FAKE_ACCOUNT": acct, **env})

    def s3_object(self, uri: str) -> Path:
        return self.state / "s3" / uri.replace("/", "|")

    def stage_uri(self, key: str, acct: str = ACCT_TWO) -> str:
        return f"s3://hamh-direct-harness-{acct}/{key}"

    def aws_calls(self) -> list:
        return [c["args"] for c in self.log("aws")]

    def worker_cline_sha(self) -> str:
        env = next(a for a in self.worker_builds()[0] if a.startswith("CLINE_SHA256="))
        return env.split("=", 1)[1]

    def test_a_fork_built_from_source_is_accepted_on_the_evidence_of_the_build_promoted_under_its_own_sha256_and_given_to_the_image(self) -> None:
        # The lock's object is in the bucket (setUp), but --fork-source build never uses it: the dry run of a second account.
        proc = self.fork("build")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        built = self.built_sha()
        self.assertNotEqual(built, CLINE_SHA)
        self.assertEqual(len(self.fork_builds()), 1)
        self.assertEqual(self.worker_cline_sha(), built, "the image is built with the SHA-256 of the fork that was built, not the lock's")
        obj = self.s3_object(self.stage_uri(f"engine/cline/{built}/cline"))
        self.assertTrue(obj.exists())
        self.assertEqual(json.loads(Path(str(obj) + ".meta").read_text()), {"sha256": built})
        puts = [a for a in self.aws_calls() if a[:2] == ["s3api", "put-object"]]
        self.assertEqual(len(puts), 2, "the binary and the pointer")
        for put in puts:
            self.assertEqual(put[put.index("--if-none-match") + 1], "*", "create-only")
        pointer = json.loads(self.s3_object(self.stage_uri(POINTER_KEY)).read_text())
        self.assertEqual((pointer["sha256"], pointer["seriesTree"], pointer["upstreamCommit"], pointer["toolchainDigest"]),
                         (built, SERIES_TREE, UPSTREAM_COMMIT, TOOLCHAIN_DIGEST))
        rec = self.deploy_record()["measurements"]["forkBinary"]
        self.assertEqual((rec["outcome"], rec["sha256"], rec["lockBinarySha256"], rec["matchesLock"]), ("built-from-source", built, CLINE_SHA, False))
        self.assertEqual((rec["seriesTree"], rec["upstreamCommit"], rec["pointerOutcome"]), (SERIES_TREE, UPSTREAM_COMMIT, "created"))
        self.assertRegex(rec["codebuildBuildId"], r"^hamh-direct-image-build:[0-9a-f-]{36}$")
        self.assertIn(f"deploy: fork binary {built[:12]} built from source", proc.stdout)
        self.assertIn("deploy: fork-source build", proc.stdout)
        image = [json.loads(p.read_text()) for p in self.records.glob("*-image-build-*.json") if '"worker' in p.read_text()][0]
        self.assertEqual(image["measurements"]["clineSha256"], built)

    def test_a_build_with_nothing_to_copy_and_no_object_builds_in_auto_and_says_why(self) -> None:
        for acct in (ACCOUNT, ACCT_TWO):
            self.s3_object(self.stage_uri(f"engine/cline/{CLINE_SHA}/cline", acct)).unlink()
            Path(str(self.s3_object(self.stage_uri(f"engine/cline/{CLINE_SHA}/cline", acct))) + ".meta").unlink()
        proc = self.fork(None)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(len(self.fork_builds()), 1)
        self.assertIn("deploy: fork-source auto -> build", proc.stdout)
        self.assertEqual(self.worker_cline_sha(), self.built_sha())

    def test_auto_builds_when_the_profile_of_the_copy_source_is_not_configured_here(self) -> None:
        self.lock_with(f"s3://cah-live-harness-{ACCOUNT}/engine/cline/X/cline", b"the-fork")
        self.seed_s3(f"s3://cah-live-harness-{ACCOUNT}/engine/cline/X/cline", "the-fork")
        for acct in (ACCT_TWO,):
            for suffix in ("", ".meta"):
                f = Path(str(self.s3_object(self.stage_uri(f"engine/cline/{self.sha_of(b'the-fork')}/cline", acct))) + suffix)
                if f.exists():
                    f.unlink()
        proc = self.fork(None, FAKE_PROFILES=ACCT_TWO_PROFILE, FAKE_FORK_CONTENT="source-built")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn(f"deploy: fork-source auto -> build (profile {ACCT_B2_PROFILE} of the artifact account is not configured here)", proc.stdout)
        self.assertEqual(len(self.fork_builds()), 1)
        down = [a for a in self.aws_calls() if a[:2] == ["s3", "cp"] and f"cah-live-harness-{ACCOUNT}" in " ".join(a)]
        self.assertEqual(down, [], "the artifact bucket of another account is not read")

    def sha_of(self, data: bytes) -> str:
        return hashlib.sha256(data).hexdigest()

    def test_auto_copies_when_the_profile_is_configured_and_a_failed_copy_is_fatal_never_a_build(self) -> None:
        sha = self.lock_with(f"s3://cah-live-harness-{ACCOUNT}/engine/cline/X/cline", b"the-fork")
        for acct in (ACCOUNT, ACCT_TWO):
            for suffix in ("", ".meta"):
                f = Path(str(self.s3_object(self.stage_uri(f"engine/cline/{CLINE_SHA}/cline", acct))) + suffix)
                if f.exists():
                    f.unlink()
        # the object is not there (or the read is refused): an AccessDenied-like failure of a configured profile
        proc = self.fork(None)
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertRegex(self.last(proc), r"^DEPLOY-FAIL fork-binary .*rerun with --fork-source build")
        self.assertEqual(self.fork_builds(), [], "a failed copy never turns into a 30 minute build with another SHA-256")
        self.assertEqual(self.worker_builds(), [])
        self.assertNotEqual(sha, "")

    def test_copy_mode_never_builds_and_says_how_to_build(self) -> None:
        self.lock_with(None, b"the-fork")
        proc = self.fork("copy")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertRegex(self.last(proc), r"^DEPLOY-FAIL fork-binary no copy source in reach .*--fork-source build")
        self.assertEqual(self.fork_builds(), [])

    def test_copy_mode_uses_the_lock_object_that_is_present(self) -> None:
        proc = self.fork("copy")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.fork_builds(), [])
        self.assertEqual(self.deploy_record()["measurements"]["forkBinary"]["outcome"], "present")

    def test_an_unknown_fork_source_is_a_usage_error_before_any_aws_call(self) -> None:
        proc = self.fork("somewhere")
        self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
        self.assertIn("--fork-source", proc.stderr)
        self.assertEqual(self.log("aws"), [])

    def test_the_second_deploy_of_a_changed_image_reuses_the_recorded_fork_and_builds_it_again_never(self) -> None:
        first = self.fork("build")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        built = self.built_sha()
        self.assertEqual(len(self.fork_builds()), 1)
        (self.root / "crates" / "agentd" / "src" / "main.rs").write_text("fn main() { /* changed */ }\n")
        git(self.root, "commit", "-q", "-am", "agentd change: a new image key, the fork unchanged")
        (self.state / "codebuild.log").unlink()
        second = self.fork("build", FAKE_FORK_CONTENT="would-be-another-binary")
        self.assertEqual(second.returncode, 0, second.stdout + second.stderr)
        self.assertEqual(self.fork_builds(), [], "the pointer of this series tree and toolchain answers")
        self.assertEqual(self.worker_cline_sha(), built, "the image carries the recorded fork")
        newest = max(self.records.glob("*-deploy-*.json"), key=lambda p: p.stat().st_mtime_ns)
        rec = json.loads(newest.read_text())["measurements"]["forkBinary"]
        self.assertEqual((rec["outcome"], rec["sha256"], rec["pointerOutcome"]), ("built-from-source", built, "read"))

    def test_a_changed_toolchain_pin_is_another_pointer_and_builds_again(self) -> None:
        first = self.fork("build")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        lock = (self.root / "engine" / "cline" / "lock.toml").read_text().replace(TOOLCHAIN["bun_version"], "1.4.3")
        (self.root / "engine" / "cline" / "lock.toml").write_text(lock)
        git(self.root, "commit", "-q", "-am", "toolchain")
        (self.state / "codebuild.log").unlink()
        second = self.fork("build", FAKE_FORK_CONTENT="the-new-toolchain-binary")
        self.assertEqual(second.returncode, 0, second.stdout + second.stderr)
        self.assertEqual(len(self.fork_builds()), 1)
        self.assertEqual(self.worker_cline_sha(), self.built_sha("the-new-toolchain-binary"))

    def test_a_pointer_that_names_a_missing_object_is_fatal_and_builds_nothing(self) -> None:
        body = {"schema": 1, "sha256": "e" * 64, "seriesTree": SERIES_TREE, "upstreamCommit": UPSTREAM_COMMIT, "toolchainDigest": TOOLCHAIN_DIGEST}
        self.seed_s3(self.stage_uri(POINTER_KEY), json.dumps(body))
        proc = self.fork("build")
        self.assertRegex(self.assert_refused(proc, "fork-pointer", "pointer "), r" names a missing or other object .* Delete the pointer ")
        self.assertEqual(self.fork_builds(), [])
        self.assertEqual(self.deploys(), [], "refused before the first stack change")

    def test_a_pointer_for_another_tree_is_ignored_as_a_fatal_mismatch(self) -> None:
        body = {"schema": 1, "sha256": self.built_sha(), "seriesTree": "9" * 40, "upstreamCommit": UPSTREAM_COMMIT, "toolchainDigest": TOOLCHAIN_DIGEST}
        self.seed_s3(self.stage_uri(POINTER_KEY), json.dumps(body))
        proc = self.fork("build")
        self.assert_refused(proc, "fork-pointer", "pointer ")

    def test_a_lost_pointer_race_uses_the_winners_fork_and_leaves_ours_as_an_unreferenced_object(self) -> None:
        rival = "the-winners-binary"
        race = {"pointer": {"schema": 1, "sha256": self.built_sha(rival), "seriesTree": SERIES_TREE, "upstreamCommit": UPSTREAM_COMMIT,
                            "toolchainDigest": TOOLCHAIN_DIGEST, "outcome": "built-from-source"},
                "object": rival, "sha256": self.built_sha(rival)}
        proc = self.fork("build", FAKE_POINTER_RACE=json.dumps(race))
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.worker_cline_sha(), self.built_sha(rival))
        self.assertIn("another deploy recorded the fork first", proc.stdout)
        rec = self.deploy_record()["measurements"]["forkBinary"]
        self.assertEqual((rec["sha256"], rec["pointerOutcome"]), (self.built_sha(rival), "lost-race"))

    def test_a_build_the_evidence_of_which_is_wrong_is_refused_before_anything_is_promoted(self) -> None:
        cases = {
            "tree": "series.tree",
            "commit": "upstream.commit",
            "sha": "binary.sha256",
            "buildid": "build id",
            "elf": "ELF machine",
            "leaks": "leaks",
            "smoke": "smoke",
            "toolchain": "bun_zip_sha256",
            "frozen": "frozen lockfile",
            "nogate": "gate",
            "telemetry": "telemetry",
        }
        for bad, needle in cases.items():
            with self.subTest(bad=bad):
                self.setUp()
                proc = self.fork("build", FAKE_FORK_BAD=bad)
                self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
                self.assertRegex(self.last(proc), r"^DEPLOY-FAIL fork-binary ")
                self.assertIn(needle, self.last(proc))
                self.assertEqual([a for a in self.aws_calls() if a[:2] == ["s3api", "put-object"]], [], "nothing is promoted and no pointer is written")
                self.assertEqual(self.worker_builds(), [])

    def test_the_fork_build_spec_leaves_the_build_id_and_the_build_record_beside_the_binary_for_the_deploy_to_read(self) -> None:
        spec = (REPO / "images" / "codebuild" / "cline-fork-arm64.yml").read_text()
        self.assertIn('printf \'%s\\n\' "$CODEBUILD_BUILD_ID" > "$OUT/build-id.txt"', spec)
        self.assertIn('cp "$OUT"/*-cline-fork-build-arm64-*.json "$OUT/build-record.json"', spec)
        lines = [ln.strip() for ln in spec.splitlines()]
        pass_check = next(i for i, ln in enumerate(lines) if "FORK-BUILD PASS series=15" in ln)
        self.assertLess(pass_check, next(i for i, ln in enumerate(lines) if "build-id.txt" in ln), "the files are written after the build passed")
        self.assertLess(next(i for i, ln in enumerate(lines) if "build-record.json" in ln), next(i for i, ln in enumerate(lines) if ln.startswith("finally:")))

    def test_a_codebuild_that_fails_is_fatal(self) -> None:
        proc = self.fork("build", FAKE_CODEBUILD_FAIL_SPEC="images/codebuild/cline-fork-arm64.yml")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertRegex(self.last(proc), r"^DEPLOY-FAIL fork-binary codebuild-run images/codebuild/cline-fork-arm64.yml exit 1")

    def test_a_present_or_copied_object_with_another_sha256_stays_fatal_in_every_mode(self) -> None:
        self.seed_s3(self.stage_uri(f"engine/cline/{CLINE_SHA}/cline"), "tampered", {"sha256": "f" * 64})
        proc = self.fork("copy")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertRegex(self.last(proc), r"^DEPLOY-FAIL fork-binary .* has sha256 [0-9a-f]{12}, the lock pins [0-9a-f]{12}$")

    def test_every_aws_call_is_logged_when_asked_and_a_source_build_names_no_foreign_bucket(self) -> None:
        # The dry run of a second account: the lock's artifact lives in another account's bucket; in build
        # mode no call names it, in copy mode the negative control does.
        foreign = f"cah-live-harness-{ACCOUNT}"
        self.lock_with(f"s3://{foreign}/engine/cline/X/cline", b"the-fork")
        log = self.tmp / "aws-calls.jsonl"
        proc = self.fork("build", DEPLOY_AWS_CALL_LOG=str(log))
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        calls = [json.loads(line) for line in log.read_text().splitlines()]
        self.assertGreater(len(calls), 20)
        self.assertEqual({tuple(c["argv"][:1]) for c in calls} - {("s3",), ("s3api",), ("ecr",), ("ssm",), ("sts",), ("ec2",), ("cloudformation",), ("bedrock",),
                                                              ("bedrock-agentcore-control",), ("dynamodb",), ("stepfunctions",), ("configure",)}, set())
        for c in calls:
            self.assertEqual(c["profile"], ACCT_TWO_PROFILE)
            self.assertNotIn(foreign, " ".join(c["argv"]))
        self.setUp()
        self.lock_with(f"s3://{foreign}/engine/cline/X/cline", b"the-fork")
        self.seed_s3(f"s3://{foreign}/engine/cline/X/cline", "the-fork")
        log2 = self.tmp / "aws-calls-copy.jsonl"
        copy = self.fork("copy", DEPLOY_AWS_CALL_LOG=str(log2), FAKE_S3_OWNERS=json.dumps({foreign: ACCT_B2_PROFILE}))
        self.assertEqual(copy.returncode, 0, copy.stdout + copy.stderr)
        self.assertTrue(any(foreign in " ".join(json.loads(line)["argv"]) for line in log2.read_text().splitlines()), "the guard sees a foreign read")

    def test_an_accounts_json_change_retags_every_image(self) -> None:
        # Every binary carries the selection it was built with, so accounts.json is an input of each image.
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        first = {b[b.index("--buildspec") + 1]: next(a for a in b if a.startswith("IMAGE_TAG=")) for b in self.log("codebuild")}
        doc = json.loads((self.root / "accounts.json").read_text())
        doc["accounts"][0]["azs"]["direct"] = ["us-west-2b"]
        doc["accounts"][0]["azIds"]["direct"] = ["usw2-az2"]
        (self.root / "accounts.json").write_text(json.dumps(doc, indent=2) + "\n")
        git(self.root, "commit", "-q", "-am", "accounts")
        (self.state / "codebuild.log").unlink()
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        again = {b[b.index("--buildspec") + 1]: next(a for a in b if a.startswith("IMAGE_TAG=")) for b in self.log("codebuild")}
        self.assertEqual(sorted(again), sorted(first), "every image is rebuilt")
        self.assertFalse(set(again.values()) & set(first.values()), "under new tags")

    # ---- the delivery image ---------------------------------------------------------------------

    def test_the_delivery_image_is_built_from_the_three_bootstraps_and_reaches_the_image_pass(self) -> None:
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        builds = self.delivery_builds()
        self.assertEqual(len(builds), 1)
        joined = " ".join(builds[0])
        meta = json.loads(subprocess.run([self.bins["cargo"], "metadata"], cwd=self.root, capture_output=True, text=True,  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool
                                         env=dict(os.environ, FAKE_STATE=str(self.state))).stdout)
        closure = digests.local_crate_closure(meta, "delivery")
        self.assertEqual(closure, ["crates/delivery", "crates/harness-core", "crates/harness-protocol"])
        key = digests._hash_paths(self.root, [*closure, "Cargo.toml", "Cargo.lock", "accounts.json", "images/delivery",
                                              "images/codebuild/delivery.yml"])
        for needle in ("--buildspec images/codebuild/delivery.yml", "--stage direct", f"HARNESS_BUCKET=hamh-direct-harness-{ACCOUNT}",
                       f"REPO_URI={ACCOUNT}.dkr.ecr.us-west-2.amazonaws.com/hamh-direct-delivery", f"IMAGE_TAG=dl-{key}",
                       "REPO_GATEWAY_SHA256=" + self.sha("repo-gateway"),
                       "BUNDLE_VALIDATOR_SHA256=" + self.sha("bundle-validator"),
                       "PACKAGER_SHA256=" + self.sha("packager")):
            self.assertIn(needle, joined)
        sha = git(self.root, "rev-parse", "HEAD")
        uploads = [c["args"][3] for c in self.log("aws") if c["args"][:2] == ["s3", "cp"] and c["args"][3] != "-"]
        for b in ("repo-gateway", "bundle-validator", "packager"):
            self.assertIn(f"s3://hamh-direct-harness-{ACCOUNT}/builds/out/delivery/{sha}/{b}", uploads)
        calls = self.deploys()
        self.assertEqual({c["HAMH_DELIVERY_IMAGE"] for c in calls[:9]}, {None}, "phase 1 has no delivery image")
        delivery = calls[9]["HAMH_DELIVERY_IMAGE"]
        self.assertRegex(delivery, rf"^{ACCOUNT}\.dkr\.ecr\.us-west-2\.amazonaws\.com/hamh-direct-delivery@sha256:[0-9a-f]{{64}}$")
        self.assertEqual({c["HAMH_DELIVERY_IMAGE"] for c in calls[9:]}, {delivery})
        rec = json.loads(sorted(self.records.glob("*-deploy-direct-*.json"))[-1].read_text())
        self.assertEqual(rec["inputs"]["deliveryImage"], delivery)
        self.assertEqual(rec["inputs"]["deliveryImageTag"], f"dl-{key}")

    def test_a_missing_delivery_bootstrap_fails_before_any_cdk_call(self) -> None:
        proc = self.run_deploy("--stage", "direct", FAKE_LAMBDA_SKIP="packager")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertRegex(self.last(proc), r"^DEPLOY-FAIL lambda-build packager")
        self.assertEqual(self.deploys(), [])

    def test_a_delivery_change_rebuilds_only_the_delivery_image(self) -> None:
        first = self.run_deploy("--stage", "direct")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        old = self.deploys()[-1]["HAMH_DELIVERY_IMAGE"]
        (self.root / "crates" / "delivery" / "src" / "lib.rs").write_text("// changed\n")
        git(self.root, "commit", "-q", "-am", "delivery change")
        second = self.run_deploy("--stage", "direct")
        self.assertEqual(second.returncode, 0, second.stdout + second.stderr)
        self.assertEqual(len(self.worker_builds()), 1)
        self.assertEqual(len(self.delivery_builds()), 2)
        self.assertNotEqual(self.deploys()[-1]["HAMH_DELIVERY_IMAGE"], old)

    def test_a_stage_without_the_delivery_repository_gets_the_delivery_phase_1_first(self) -> None:
        # A stage deployed before the delivery stack existed: worker repository and image in place.
        first = self.run_deploy("--stage", "direct")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        (self.state / "repo-hamh-direct-delivery").unlink()
        for f in self.state.glob("image-dl-*"):
            f.unlink()
        n = len(self.deploys())
        second = self.run_deploy("--stage", "direct")
        self.assertEqual(second.returncode, 0, second.stdout + second.stderr)
        again = self.deploys()[n:]
        self.assertEqual(again[0]["args"][2], "hamh-direct-delivery")
        self.assertIsNone(again[0]["HAMH_DELIVERY_IMAGE"])
        self.assertIsNotNone(again[0]["HAMH_WORKER_IMAGE"], "the running worker image stays in every synth")
        self.assertEqual(len(again), 10)
        self.assertEqual(len(self.delivery_builds()), 2)
        self.assertTrue(all(c["HAMH_DELIVERY_IMAGE"] for c in again[1:]))

    def test_a_failed_delivery_image_build_fails_the_deploy(self) -> None:
        proc = self.run_deploy("--stage", "direct", FAKE_CODEBUILD_FAIL_SPEC="images/codebuild/delivery.yml")
        self.assertEqual(proc.returncode, 1)
        self.assertEqual(self.last(proc), "DEPLOY-FAIL delivery-image-build codebuild-run exit 1")
        self.assertEqual(self.digests_written(), {})

    # ---- usage ----------------------------------------------------------------------------------

    def test_a_dirty_tree_is_refused_before_any_aws_call(self) -> None:
        (self.root / "README.md").write_text("changed\n")
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 2)
        self.assertIn("uncommitted", proc.stderr)
        self.assertEqual(self.log("aws"), [])

    def test_bad_flags_are_usage_errors(self) -> None:
        for args in ((), ("--stage", "prod"), ("--stage", "live"), ("--stage", "drill"), ("--stage", "direct", "--azs", "3"),
                     ("--stage", "direct", "--ladder-deferred"), ("--stage", "direct", "--bogus")):
            proc = self.run_deploy(*args)
            self.assertEqual(proc.returncode, 2, args)
        self.assertEqual(self.log("aws"), [])

    # ---- 2026-10-01: a deploy publishes only what was built for its own selection ----------------
    # The Rust binaries carry the accounts.json entry they were built for. Deploys of active-entry live,
    # history-profile live and the active-entry drill stage overlapped in one checkout and shared target/, so the
    # active-entry run-tasks Lambda shipped with history-profile's Region compiled in. Every build now goes to a
    # directory keyed by the selection (scripts/lib/build_selection.py), and what it wrote is checked before
    # anything is uploaded or handed to CDK.

    def cargo_env(self, verb: str) -> list[dict]:
        return [c for c in self.log("cargo-env") if c["call"][0] == verb]

    def uploaded(self, uri: str) -> bytes:
        return (self.state / "s3" / uri.replace("/", "|")).read_bytes()

    def tree_hashes(self, base: Path) -> dict[str, str]:
        return {str(p.relative_to(base)): hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(base.rglob("*")) if p.is_file()}

    def check_layout(self, proc: subprocess.CompletedProcess, profile: str, stage: str = "direct") -> None:
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        key = self.keyed(profile)
        lam = self.cargo_env("lambda")
        zig = self.cargo_env("zigbuild")
        self.assertEqual(len(lam), 1)
        self.assertTrue(zig)
        for c in (*lam, *zig):
            self.assertEqual((c["CARGO_TARGET_DIR"], c["HAMH_AWS_PROFILE"]), (str(key), profile), c)
        # The target directory goes by CARGO_TARGET_DIR only: cargo lambda 1.9.2 passes --target-dir on to
        # `cargo metadata`, which rejects it. The Lambda directory is a flag.
        build = [c for c in self.log("cargo") if c[:2] == ["lambda", "build"]][0]
        self.assertEqual(build[build.index("--lambda-dir") + 1], str(key / "lambda" / stage))
        for c in (c for c in self.log("cargo") if c[0] in ("lambda", "zigbuild")):
            self.assertNotIn("--target-dir", c)
        for c in self.deploys():
            self.assertEqual(c["HAMH_LAMBDA_DIR"], str(key / "lambda" / stage))
        self.assertTrue((key / "lambda" / stage / "run-session" / "bootstrap").is_file())
        self.assertTrue((key / sf.TRIPLE / "release" / "agentd").is_file())
        # Nothing lands in the shared target/ any more.
        self.assertFalse((self.root / "target" / "lambda").exists())
        self.assertFalse((self.root / "target" / sf.TRIPLE).exists())

    def test_builds_go_to_the_target_directory_keyed_by_the_history_entry_selection(self) -> None:
        self.check_layout(self.run_deploy("--stage", "direct"), "history-profile")

    def test_builds_go_to_the_target_directory_keyed_by_the_active_entry_selection(self) -> None:
        self.check_layout(self.acct_two(), "my-profile")

    def test_the_build_selection_comes_from_the_selected_entry_whatever_the_caller_exported(self) -> None:
        # The caller's CARGO_TARGET_DIR never decides where a deploy builds, and an unset HAMH_AWS_PROFILE (the
        # active entry) still builds for the entry the deploy targets.
        proc = self.acct_two(CARGO_TARGET_DIR=str(self.tmp / "elsewhere"))
        self.check_layout(proc, "my-profile")
        self.assertFalse((self.tmp / "elsewhere").exists())

    def test_two_deploys_of_different_selections_never_share_artifacts(self) -> None:
        acct_b2 = self.run_deploy("--stage", "direct")
        self.assertEqual(acct_b2.returncode, 0, acct_b2.stdout + acct_b2.stderr)
        before = self.tree_hashes(self.keyed("history-profile"))
        acct_two = self.acct_two()
        self.assertEqual(acct_two.returncode, 0, acct_two.stdout + acct_two.stderr)
        self.assertEqual(self.tree_hashes(self.keyed("history-profile")), before, "the active-entry deploy changed history-profile's files")
        for profile in ("history-profile", "my-profile"):
            lam = self.keyed(profile) / "lambda" / "direct"
            self.assertEqual((lam / "repo-gateway" / "bootstrap").read_bytes(), sf.binary_bytes("repo-gateway", profile))
        # The fake ECR keeps one image per tag, so the second deploy reuses the first one's worker image and cross-builds
        # nothing: agentd exists in the first selection's directory only.
        self.assertEqual((self.keyed("history-profile") / sf.TRIPLE / "release" / "agentd").read_bytes(),
                         sf.binary_bytes("agentd", "history-profile"))
        self.assertNotEqual(self.keyed("history-profile"), self.keyed("my-profile"))

    def test_a_build_of_another_selection_at_the_same_time_cannot_replace_what_a_deploy_publishes(self) -> None:
        # The incident, replayed: while this history-profile deploy builds, an active-entry deploy from the same checkout
        # writes its binaries over the shared target/ outputs (and into its own keyed directory).
        proc = self.run_deploy("--stage", "direct", FAKE_CLOBBER_FOR="my-profile")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        key = self.keyed("history-profile")
        self.assertTrue(self.deploys())
        for c in self.deploys():
            self.assertEqual(c["HAMH_LAMBDA_DIR"], str(key / "lambda" / "direct"))
        for name in ("run-session", "control-api", "worker-api", "repo-gateway"):
            self.assertEqual((key / "lambda" / "direct" / name / "bootstrap").read_bytes(), sf.binary_bytes(name, "history-profile"), name)
        sha = git(self.root, "rev-parse", "HEAD")
        got = self.uploaded(f"s3://hamh-direct-harness-{ACCOUNT}/builds/out/agentd/{sha}/agentd")
        self.assertEqual(got, sf.binary_bytes("agentd", "history-profile"))
        joined = " ".join(self.worker_builds()[0])
        self.assertIn("AGENTD_SHA256=" + self.sha("agentd"), joined)
        for name in ("repo-gateway", "bundle-validator", "packager"):
            got = self.uploaded(f"s3://hamh-direct-harness-{ACCOUNT}/builds/out/delivery/{sha}/{name}")
            self.assertEqual(got, sf.binary_bytes(name, "history-profile"), name)

    def test_the_lambda_files_of_a_retired_stage_of_the_same_selection_are_never_this_deploys(self) -> None:
        # active-entry's retired live stage shares the cargo cache with `direct`, never the files CDK zips: a build that rewrites the
        # `live` lambda directory while the `direct` deploy publishes from its own leaves the published files whole.
        proc = self.acct_two(FAKE_CLOBBER_STAGE="live")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        key = self.keyed("my-profile")
        for c in self.deploys():
            self.assertEqual(c["HAMH_LAMBDA_DIR"], str(key / "lambda" / "direct"))
        self.assertEqual((key / "lambda" / "direct" / "run-session" / "bootstrap").read_bytes(), sf.binary_bytes("run-session", "my-profile"))
        self.assertEqual((key / "lambda" / "live" / "run-session" / "bootstrap").read_bytes(), b"")

    def test_the_deploy_says_what_the_build_check_covered(self) -> None:
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn(f"deploy: build selection ok history-profile ({ACCOUNT} us-west-2): lambda bytes=repo-gateway outputs=1", proc.stdout)
        self.assertIn(f"deploy: build selection ok history-profile ({ACCOUNT} us-west-2): worker bytes=none outputs=1", proc.stdout)

    def no_upload_of_binaries(self) -> None:
        uploads = [c["args"][3] for c in self.log("aws") if c["args"][:2] == ["s3", "cp"] and len(c["args"]) > 3]
        self.assertEqual([u for u in uploads if "/builds/out/" in u], [], uploads)

    def test_a_lambda_built_for_another_selection_fails_the_deploy_before_anything_is_published(self) -> None:
        for name in ("repo-gateway",):
            with self.subTest(name):
                proc = self.run_deploy("--stage", "direct", FAKE_BUILT_FOR="my-profile", FAKE_BUILT_FOR_BINARIES=name)
                self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
                self.assertRegex(self.last(proc), rf"^DEPLOY-FAIL build-selection {name} built for my-profile "
                                 rf"\({ACCT_A1} us-east-1\), target is history-profile \({ACCOUNT} us-west-2\): ")
                self.assertIn(f"/lambda/direct/{name}/bootstrap embeds {ACCT_A1}, not {ACCOUNT}", self.last(proc))
                self.assertEqual(self.log("node"), [], "no pnpm install, no cdk call")
                self.no_upload_of_binaries()

    def test_an_active_entry_deploy_refuses_lambdas_built_for_history_entry_and_names_both(self) -> None:
        proc = self.acct_two(FAKE_BUILT_FOR="history-profile", FAKE_BUILT_FOR_BINARIES="repo-gateway")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertRegex(self.last(proc), rf"^DEPLOY-FAIL build-selection repo-gateway built for history-profile \({ACCOUNT} us-west-2\), "
                         rf"target is my-profile \({ACCT_A1} us-east-1\): ")
        self.assertEqual(self.deploys(), [])

    def test_build_output_naming_another_selection_fails_the_lambda_build_step(self) -> None:
        # run-session carries neither account id: the harness-protocol build script output in the target
        # directory is what tells which selection it was built for.
        proc = self.run_deploy("--stage", "direct", FAKE_OUT_FOR="my-profile", FAKE_OUT_KINDS="lambda")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertRegex(self.last(proc), rf"^DEPLOY-FAIL build-selection harness-protocol built for my-profile "
                         rf"\({ACCT_A1} us-east-1\), target is history-profile \({ACCOUNT} us-west-2\): ")
        self.assertIn("aws_target.rs", self.last(proc))
        self.assertEqual(self.log("node"), [])

    def test_build_output_naming_another_selection_fails_the_cross_build_before_the_upload(self) -> None:
        proc = self.run_deploy("--stage", "direct", FAKE_OUT_FOR="my-profile", FAKE_OUT_KINDS="zig")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertRegex(self.last(proc), r"^DEPLOY-FAIL build-selection harness-protocol built for my-profile ")
        self.no_upload_of_binaries()
        self.assertEqual(self.worker_builds(), [])

    def test_a_build_with_no_script_output_cannot_confirm_its_selection(self) -> None:
        proc = self.run_deploy("--stage", "direct", FAKE_NO_OUT="1")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertRegex(self.last(proc), r"^DEPLOY-FAIL build-selection harness-protocol has no build output ")
        self.assertEqual(self.log("node"), [])

    def test_the_other_entrys_id_in_a_price_list_string_is_not_a_wrong_build(self) -> None:
        # control-api embedded history-profile's id in prices.json's source line whatever it was built for (measured on the
        # 2026-10-01 release builds): an active-entry deploy must pass with it.
        proc = self.acct_two(FAKE_FOREIGN_IN="control-api")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        boot = (self.keyed("my-profile") / "lambda" / "direct" / "control-api" / "bootstrap").read_bytes()
        self.assertIn(ACCOUNT.encode(), boot)
    def _run_endpoint_deploy(self, endpoint_env: dict[str, str] | None = None, endpoint_timeout: float | None = None):
        with tempfile.TemporaryDirectory(prefix="deploy-endpoint-") as raw:
            fake = Path(raw) / "aws"
            fake.write_text(ENDPOINT_FAKE_AWS.format(python=sys.executable), encoding="utf-8")
            fake.chmod(0o755)

            deployer = deploy.Deployer("direct")
            deployer.aws_bin = str(fake)
            deployer._current_runtimes = {}
            deployer.env.update(endpoint_env or {})
            tests_path = str(REPO / "scripts" / "tests")
            deployer.env["PYTHONPATH"] = tests_path + (os.pathsep + deployer.env["PYTHONPATH"] if deployer.env.get("PYTHONPATH") else "")
            image = f"example.invalid/worker@sha256:{'a' * 64}"
            order: list[str] = []
            launched: list[list[str]] = []
            real_aws = deployer.aws
            real_run = deployer.run

            def aws(*args: str, **kwargs: object):
                if args[:2] == ("ssm", "get-parameter"):
                    return None
                if args[:2] == ("cloudformation", "describe-stacks"):
                    order.append("endpoint")
                    return real_aws(*args, **kwargs)
                raise AssertionError(args)

            metadata = subprocess.CompletedProcess([], 0, json.dumps({"packages": [], "resolve": {"nodes": []}}), "")

            def run(cmd: list[str], *args: object, **kwargs: object):
                if cmd and cmd[0] == str(fake):
                    launched.append(list(cmd))
                    return real_run(cmd, *args, **kwargs)
                return metadata

            output = io.StringIO()
            record = mock.Mock(side_effect=lambda _document: order.append("record"))
            error: deploy.Fail | None = None
            result: str | None = None
            with contextlib.ExitStack() as stack:
                replacements = {
                    "git": "b" * 40,
                    "check_account": None,
                    "check_azs": {},
                    "check_vpc_cidr": {"cidr": "10.50.0.0/16", "overlaps": []},
                    "settle_owner": None,
                    "deploy_lock": contextlib.nullcontext(),
                    "report_config_record": None,
                    "settle_settings": {},
                    "require_consent": None,
                    "compute_instances_state": {"change": False},
                    "say_compute_instances": None,
                    "refuse_instances_off": None,
                    "tools_mode_state": {"change": False},
                    "say_tools_mode": None,
                    "isolation_state": {"change": False},
                    "say_isolation": None,
                    "build_lambda": None,
                    "install_infra": None,
                    "image_tag": "in-test",
                    "stack_order": [],
                    "provider_replacement": None,
                    "check_bootstrap": {"outcome": "existing", "version": 21, "required": 21},
                    "repo_exists": True,
                    "find_image": image,
                    "resolve_delivery_image": f"example.invalid/delivery@sha256:{'c' * 64}",
                    "version_endpoints": [],
                    "invocation_logging": {},
                    "template_hashes": {},
                    "asl_versions": {},
                    "record_assets": None,
                    "guard": None,
                }
                for name, value in replacements.items():
                    stack.enter_context(mock.patch.object(deployer, name, return_value=value))
                # The first write of the SSM digests is recorded as "digests", so a case sees whether the endpoint was checked before it.
                stack.enter_context(mock.patch.object(
                    deployer, "put_param", side_effect=lambda _name, _value: None if "digests" in order else order.append("digests")))
                stack.enter_context(mock.patch.object(deploy, "ENDPOINT_LOOKUP_WAIT_S", 0, create=True))
                stack.enter_context(mock.patch.object(deployer, "run", side_effect=run))
                stack.enter_context(mock.patch.object(deployer, "aws", side_effect=aws))
                stack.enter_context(mock.patch.object(
                    deployer,
                    "check_runtimes",
                    side_effect=lambda _expected: order.append("runtimes") or [{"image": image}],
                ))
                stack.enter_context(mock.patch.object(
                    deployer,
                    "report_gomod_store",
                    side_effect=lambda: order.append("gomod") or {"state": "filled"},
                ))
                stack.enter_context(mock.patch.object(deployer, "write_record", record))
                stack.enter_context(mock.patch.object(
                    deployer,
                    "write_settings_record",
                    side_effect=lambda _status: order.append("settings-record"),
                ))
                stack.enter_context(mock.patch.object(deploy.digests, "source_tree", return_value="d" * 64))
                stack.enter_context(mock.patch.object(deploy.digests, "local_crate_closure", return_value=[]))
                stack.enter_context(mock.patch.object(deploy.digests, "_hash_paths", return_value="e" * 64))
                stack.enter_context(mock.patch.object(deploy.digests, "deployment_digest", return_value="f" * 64))
                stack.enter_context(mock.patch.object(deploy.digests, "image_digest", return_value="1" * 64))
                if endpoint_timeout is not None:
                    stack.enter_context(mock.patch.object(deploy, "ENDPOINT_LOOKUP_TIMEOUT_S", endpoint_timeout))
                with contextlib.redirect_stdout(output):
                    try:
                        result = deployer.deploy()
                        print(result)
                    except deploy.Fail as exc:
                        error = exc

            return deployer, output.getvalue(), order, record, error, result, launched

    def test_deploy_prints_the_endpoint(self) -> None:
        deployer, output, order, record, error, result, launched = self._run_endpoint_deploy()

        self.assertIsNone(error)
        self.assertIsNotNone(result)
        self.assertEqual(order, ["runtimes", "endpoint", "digests", "gomod", "record", "settings-record"])
        record.assert_called_once()
        self.assertEqual(len(launched), 1)
        self.assertTrue(all(call[0].endswith("/aws") for call in launched), launched)
        lines = output.splitlines()
        ready = f"deploy: endpoint V1ApiUrl ready stage=direct region={deployer.region}. Read it from stack hamh-direct-control."
        self.assertEqual(lines[-2], ready)
        self.assertTrue(lines[-1].startswith("DEPLOY-PASS stage=direct "))
        self.assertNotIn("https://", output)

    def test_endpoint_failures_use_the_fake_process_and_write_no_pass_record(self) -> None:
        cases = (
            ({"FAKE_NO_V1_API_URL": "1"}, "missing"),
            ({"FAKE_MALFORMED_V1_API_URL": "1"}, "invalid"),
            ({"FAKE_AWS_RESPONSE_MODE": "denied"}, "unavailable"),
            ({"FAKE_AWS_RESPONSE_MODE": "nonzero"}, "unavailable"),
            ({"FAKE_AWS_RESPONSE_MODE": "empty"}, "missing"),
            ({"FAKE_AWS_RESPONSE_MODE": "malformed-json"}, "invalid"),
            ({"FAKE_AWS_RESPONSE_MODE": "malformed-shape"}, "invalid"),
            ({"FAKE_AWS_RESPONSE_MODE": "multiple-stacks"}, "invalid"),
            ({"FAKE_AWS_RESPONSE_MODE": "duplicate-output-keys"}, "invalid"),
        )
        for settings, detail in cases:
            with self.subTest(settings=settings):
                controls = {
                    "FAKE_AWS_RESPONSE_PAIR": "cloudformation describe-stacks",
                    "FAKE_AWS_RESPONSE_STACK": "hamh-direct-control",
                    "FAKE_AWS_STDOUT": "do-not-print-response-detail",
                    "FAKE_AWS_STDERR": "do-not-print-transport-detail",
                    **settings,
                }
                _deployer, output, order, record, error, result, launched = self._run_endpoint_deploy(controls)
                # A stack that cannot be read is read three times; an answer is final. Nothing records the deployment.
                tries = 3 if detail == "unavailable" else 1
                self.assertIsNone(result)
                self.assertEqual(str(error), f"endpoint {detail}")
                self.assertEqual(order, ["runtimes"] + ["endpoint"] * tries)
                record.assert_not_called()
                self.assertEqual(len(launched), tries)
                self.assertNotIn("endpoint V1ApiUrl ready", output)
                self.assertNotIn("DEPLOY-PASS", output)
                self.assertNotIn("do-not-print", output + str(error))
                self.assertNotIn("https://", output + str(error))

    def test_delayed_endpoint_process_is_terminated_without_a_pass_record(self) -> None:
        with tempfile.TemporaryDirectory(prefix="deploy-endpoint-pid-") as raw:
            pid_file = Path(raw) / "pid"
            controls = {
                "FAKE_AWS_DELAY_PAIR": "cloudformation describe-stacks",
                "FAKE_AWS_DELAY_S": "5",
                "FAKE_AWS_PID_FILE": str(pid_file),
            }
            started = time.monotonic()
            _deployer, output, order, record, error, result, launched = self._run_endpoint_deploy(controls, endpoint_timeout=0.5)
            elapsed = time.monotonic() - started
            pid = int(pid_file.read_text(encoding="utf-8"))

        # Each of the three tries is ended at the time limit.
        self.assertLess(elapsed, 3 * 0.5 + 1.5)
        self.assertIsNone(result)
        self.assertEqual(str(error), "endpoint unavailable")
        self.assertEqual(order, ["runtimes", "endpoint", "endpoint", "endpoint"])
        record.assert_not_called()
        self.assertEqual(len(launched), 3)
        self.assertNotIn("endpoint V1ApiUrl ready", output)
        self.assertNotIn("DEPLOY-PASS", output)
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)


    def test_b835_the_endpoint_is_checked_before_the_deployment_is_recorded(self) -> None:
        # The endpoint is checked with the runtimes. A deploy that cannot confirm it writes no SSM digest, so the stage's parameters still name
        # the last deploy that finished, as after any other failed check of the new stacks.
        _deployer, _output, order, _record, error, _result, _launched = self._run_endpoint_deploy()
        self.assertIsNone(error)
        self.assertEqual(order.index("endpoint"), order.index("runtimes") + 1)
        self.assertLess(order.index("endpoint"), order.index("digests"))
        _deployer, _output, order, record, error, _result, _launched = self._run_endpoint_deploy({"FAKE_NO_V1_API_URL": "1"})
        self.assertEqual(str(error), "endpoint missing")
        self.assertEqual(order, ["runtimes", "endpoint"])
        record.assert_not_called()

    def test_b835_a_deploy_that_cannot_confirm_its_endpoint_writes_no_digest(self) -> None:
        proc = self.run_deploy("--stage", "direct", FAKE_NO_V1_API_URL="1")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertEqual(self.last(proc), "DEPLOY-FAIL endpoint missing")
        ssm = self.ssm()
        for name in ("deployment-digest", "deployed-source-tree", "image-digest", "worker-revision", "deployment-config-digest"):
            self.assertNotIn(f"/hamh/direct/{name}", ssm)
        self.assertNotIn("https://", proc.stdout + proc.stderr)

    # ----: the Go module store is said at the end of the deploy -------------------------------------------------------------------------

    FILL = ("scripts/gomod-store.sh fill --stage direct --modules engine/rules/deps/go-approved-modules.txt "
            "--tools scripts/gomod-tool-roots.txt --jobs 8")

    def test_an_empty_go_module_store_is_said_before_the_pass_line_with_the_fill_command(self) -> None:
        proc = self.run_deploy("--stage", "direct", FAKE_GOMOD="empty")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        lines = proc.stdout.splitlines()
        hits = [i for i, ln in enumerate(lines) if ln.startswith("GOMOD-STORE EMPTY ")]
        self.assertEqual(len(hits), 1, proc.stdout)
        line = lines[hits[0]]
        self.assertIn(f"stage=direct bucket=hamh-direct-gomod-{ACCOUNT}", line)
        self.assertIn(self.FILL, line)
        self.assertIn("scripts/gomod-store.sh check --stage direct", line)
        self.assertTrue(lines[-1].startswith("DEPLOY-PASS "), "the pass line stays the last line")
        self.assertLess(hits[0], len(lines) - 1)
        self.assertEqual(self.deploy_record()["measurements"]["gomodStore"], {"bucket": f"hamh-direct-gomod-{ACCOUNT}", "state": "empty"})

    def test_the_empty_store_line_explains_the_store_to_a_person_who_has_never_seen_the_release_gate(self) -> None:
        # O5 of the live rehearsal of the public setup: the line pointed at "the release gate" and "the events sample", names nobody outside the team has met. It says
        # what the store is, who is affected and what to run, in words of the product.
        proc = self.run_deploy("--stage", "direct", FAKE_GOMOD="empty")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        line = next(ln for ln in proc.stdout.splitlines() if ln.startswith("GOMOD-STORE EMPTY "))
        for word in ("release gate", "events sample", "gate"):
            self.assertNotIn(word, line)
        for fact in ("Go modules", "HTTP 404", "other languages", "empty"):
            self.assertIn(fact, line)
        self.assertLess(line.index(self.FILL), line.index("scripts/gomod-store.sh check --stage direct"), "the fill command comes first, then the check")
        self.assertEqual(line.count(self.FILL), 1)

    def test_a_go_module_store_that_holds_objects_is_reported_ok(self) -> None:
        proc = self.run_deploy("--stage", "direct", FAKE_GOMOD="objects")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        ok = [ln for ln in proc.stdout.splitlines() if ln.startswith("GOMOD-STORE OK ")]
        self.assertEqual(len(ok), 1, proc.stdout)
        self.assertIn(f"bucket=hamh-direct-gomod-{ACCOUNT}", ok[0])
        self.assertNotIn("GOMOD-STORE EMPTY", proc.stdout)
        self.assertEqual(self.deploy_record()["measurements"]["gomodStore"], {"bucket": f"hamh-direct-gomod-{ACCOUNT}", "state": "filled"})

    def test_a_go_module_store_that_cannot_be_read_is_a_warning_and_the_deploy_still_passes(self) -> None:
        proc = self.run_deploy("--stage", "direct", FAKE_GOMOD="denied")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        warn = [ln for ln in proc.stdout.splitlines() if ln.startswith("GOMOD-STORE UNKNOWN ")]
        self.assertEqual(len(warn), 1, proc.stdout)
        self.assertIn("AccessDenied", warn[0])
        self.assertIn("scripts/gomod-store.sh check --stage direct", warn[0])
        self.assertTrue(self.last(proc).startswith("DEPLOY-PASS "))
        self.assertEqual(self.deploy_record()["measurements"]["gomodStore"]["state"], "unknown")

    def test_the_go_module_store_look_reads_one_object_of_the_stage_bucket_and_writes_nothing(self) -> None:
        self.assertEqual(self.run_deploy("--stage", "direct", FAKE_GOMOD="empty").returncode, 0)
        calls = [c["args"] for c in self.log("aws")]
        listing = [a for a in calls if a[:2] == ["s3api", "list-objects-v2"]]
        self.assertEqual(len(listing), 1)
        self.assertEqual(listing[0][listing[0].index("--bucket") + 1], f"hamh-direct-gomod-{ACCOUNT}")
        self.assertEqual(listing[0][listing[0].index("--max-items") + 1], "1")
        self.assertEqual(listing[0][listing[0].index("--expected-bucket-owner") + 1], ACCOUNT)
        gomod_writes = [a for a in calls if "gomod" in " ".join(a) and a[:2] in (["s3api", "put-object"], ["s3", "cp"], ["s3api", "delete-object"])]
        self.assertEqual(gomod_writes, [])

    def test_a_failed_deploy_does_not_look_at_the_store(self) -> None:
        proc = self.run_deploy("--stage", "direct", FAKE_ACCOUNT=ZERO_ACCOUNT)
        self.assert_refused(proc, "account")
        self.assertNotIn("GOMOD-STORE", proc.stdout)


    # ----: the account file in use is a separate, recorded input ----------------------------------------------

    def generated_config(self, tag: str = "a") -> Path:
        """A generated account file: the committed entries plus one more, so its bytes differ and the entry the run selects stays."""
        doc = json.loads(sf.ACCOUNTS.read_text())
        doc["accounts"].append({**doc["accounts"][0], "profile": f"extra-{tag}", "account": EXTRA_ACCOUNT, "role": "test"})
        p = self.tmp / f"generated-{tag}.json"
        p.write_text(json.dumps(doc, indent=1) + "\n")
        return p

    def config_lines(self, proc: subprocess.CompletedProcess) -> list[str]:
        return [ln for ln in proc.stdout.splitlines() if ln.startswith("deploy: deployment config")]

    def test_without_the_override_the_record_says_committed_and_the_children_see_no_override(self) -> None:
        proc = self.run_deploy("--stage", "direct")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        st = digests.source_tree(self.root)
        self.assertRegex(self.last(proc), rf"^DEPLOY-PASS stage=direct digest=[0-9a-f]{{12}} sourceTree={st[:12]}$")
        rec = self.deploy_record()
        self.assertEqual(rec["deploymentConfigDigest"], "committed")
        self.assertEqual(rec["sourceTree"], st)
        for p in self.records.glob("*-image-build-*.json"):
            self.assertEqual(json.loads(p.read_text())["deploymentConfigDigest"], "committed", p.name)
        self.assertEqual(self.ssm()["/hamh/direct/deployment-config-digest"], "committed")
        self.assertTrue(all(c["HAMH_ACCOUNTS_FILE"] is None for c in self.log("node")))
        self.assertTrue(all(c["HAMH_ACCOUNTS_FILE"] is None for c in self.log("cargo-env")))
        self.assertTrue(all(c["HAMH_ACCOUNTS_FILE"] is None for c in self.log("codebuild-env")))
        self.assertEqual([ln for ln in self.config_lines(proc) if "differs" in ln], [])

    def test_with_the_override_the_record_names_its_digest_beside_an_unchanged_source_tree(self) -> None:
        f = self.generated_config()
        digest = hashlib.sha256(f.read_bytes()).hexdigest()
        proc = self.run_deploy("--stage", "direct", HAMH_ACCOUNTS_FILE=str(f))
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        st = digests.source_tree(self.root)
        self.assertRegex(self.last(proc), rf"^DEPLOY-PASS stage=direct digest=[0-9a-f]{{12}} sourceTree={st[:12]}$")
        rec = self.deploy_record()
        self.assertEqual((rec["deploymentConfigDigest"], rec["sourceTree"]), (digest, st))
        for p in self.records.glob("*-image-build-*.json"):
            d = json.loads(p.read_text())
            self.assertEqual((d["deploymentConfigDigest"], d["sourceTree"]), (digest, st), p.name)
        self.assertEqual(self.ssm()["/hamh/direct/deployment-config-digest"], digest)
        self.assertTrue(any(f"deployment config {digest[:12]}" in ln for ln in self.config_lines(proc)), proc.stdout)

    def test_with_the_override_cargo_cdk_and_the_image_builds_all_see_the_same_file(self) -> None:
        f = self.generated_config()
        proc = self.run_deploy("--stage", "direct", HAMH_ACCOUNTS_FILE=str(f))
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        for name in ("node", "cargo-env", "codebuild-env"):
            seen = {c["HAMH_ACCOUNTS_FILE"] for c in self.log(name)}
            self.assertEqual(seen, {str(f)}, name)
        self.assertTrue(self.log("codebuild-env"))
        self.assertTrue(self.cargo_env("zigbuild") and self.cargo_env("lambda"))

    def test_the_selection_and_the_build_directory_follow_the_override_entry(self) -> None:
        f = self.generated_config()
        proc = self.run_deploy("--stage", "direct", HAMH_ACCOUNTS_FILE=str(f))
        self.check_layout(proc, PROFILE_OF[ACCOUNT])
        self.assertEqual(self.deploy_record()["inputs"]["target"]["account"], ACCOUNT)

    def test_an_override_that_names_no_entry_of_the_profile_is_a_usage_error_before_any_call(self) -> None:
        f = self.generated_config()
        proc = self.run_deploy("--stage", "direct", HAMH_ACCOUNTS_FILE=str(f), HAMH_AWS_PROFILE="no-such-profile")
        self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
        self.assertIn("HAMH_AWS_PROFILE=no-such-profile names no entry of " + str(f), proc.stderr)
        self.assertEqual(self.log("aws"), [])

    def test_a_relative_override_is_a_usage_error_before_any_call(self) -> None:
        proc = self.run_deploy("--stage", "direct", HAMH_ACCOUNTS_FILE="accounts.json")
        self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
        self.assertIn("absolute path", proc.stderr)
        self.assertEqual(self.log("aws"), [])

    def test_a_deploy_with_another_file_than_the_earlier_record_says_so(self) -> None:
        a, b = self.generated_config("a"), self.generated_config("b")
        da, db = (hashlib.sha256(x.read_bytes()).hexdigest() for x in (a, b))
        first = self.run_deploy("--stage", "direct", HAMH_ACCOUNTS_FILE=str(a))
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        self.assertEqual([ln for ln in self.config_lines(first) if "differs" in ln], [], "the first deploy has no earlier record")
        same = self.run_deploy("--stage", "direct", HAMH_ACCOUNTS_FILE=str(a))
        self.assertEqual([ln for ln in self.config_lines(same) if "differs" in ln], [])
        other = self.run_deploy("--stage", "direct", HAMH_ACCOUNTS_FILE=str(b))
        self.assertEqual(other.returncode, 0, other.stdout + other.stderr)
        lines = [ln for ln in self.config_lines(other) if "differs" in ln]
        self.assertEqual(len(lines), 1, other.stdout)
        self.assertIn(db[:12], lines[0])
        self.assertIn(da[:12], lines[0])
        self.assertEqual(self.ssm()["/hamh/direct/deployment-config-digest"], db)
        back = self.run_deploy("--stage", "direct")
        self.assertEqual(back.returncode, 0, back.stdout + back.stderr)
        lines = [ln for ln in self.config_lines(back) if "differs" in ln]
        self.assertEqual(len(lines), 1, back.stdout)
        self.assertIn("committed", lines[0])
        self.assertIn(db[:12], lines[0])

    def test_a_mismatch_is_reported_not_fatal_and_comes_before_the_first_stack(self) -> None:
        a, b = self.generated_config("a"), self.generated_config("b")
        self.assertEqual(self.run_deploy("--stage", "direct", HAMH_ACCOUNTS_FILE=str(a)).returncode, 0)
        proc = self.run_deploy("--stage", "direct", HAMH_ACCOUNTS_FILE=str(b))
        self.assertEqual(proc.returncode, 0, "a different file is reported, it does not stop the deploy")
        out = proc.stdout.splitlines()
        differs = next(i for i, ln in enumerate(out) if "differs" in ln)
        first_stack = next(i for i, ln in enumerate(out) if ln.startswith("deploy: ") and "hamh-direct-" in ln and "differs" not in ln)
        self.assertLess(differs, first_stack)


    # The owner tag of a stage that exists (blocker of the second update test of the public setup): the update changed `owner` from the value the stage was
    # deployed with to the name of the operator, which changes the tag of every resource, and CloudFormation REPLACES the network stack's DNS query logging
    # configuration when its tags change (AWS::Route53Resolver::ResolverQueryLoggingConfig, Tags: "Update requires: Replacement"); the replacement failed and the
    # stack rolled back. A deploy therefore keeps the owner of the stage it finds, and stops before it changes anything when it is told another one.
    def seed_stack(self, stack: str, owner: str | None, status: str = "UPDATE_COMPLETE") -> None:
        """A stack of the stage as CloudFormation holds it: its status and its tags (the CDK app tags every stack of the stage with the owner)."""
        tags = {"project": "hyperscale-agent-meta-harness", "stage": "direct", "managed-by": "cdk"}
        if owner is not None:
            tags["owner"] = owner
        (self.state / f"stack-hamh-direct-{stack}").write_text(json.dumps({"template": {}, "outputs": {}, "status": status, "tags": tags}))

    def cdk_owners(self) -> list:
        """The HAMH_OWNER each cdk call (synth and deploy) of the run got."""
        return [c["HAMH_OWNER"] for c in self.log("node") if c["args"][:1] == ["cdk"]]

    def ssm_owner_tags(self) -> set:
        """The owner value of every `ssm add-tags-to-resource` call of the run."""
        found = set()
        for c in self.log("aws"):
            if c["args"][:2] == ["ssm", "add-tags-to-resource"]:
                found.update(a.split("Value=", 1)[1] for a in c["args"] if a.startswith("Key=owner,Value="))
        return found

    def assert_refused_before_any_change(self, proc: subprocess.CompletedProcess, *needles: str) -> None:
        line = self.assert_refused(proc, "owner")
        for needle in needles:
            self.assertIn(needle, line)
        self.assertNotIn("DEPLOY-PASS", proc.stdout)
        self.assertEqual(self.log("node"), [], "no cdk call: not a synth, not a deploy")
        self.assertEqual(self.lambda_builds(), 0, "no build")
        self.assertEqual(self.ssm(), {}, "no record of the stage was written")
        self.assertFalse((self.state / "bootstrap-version").exists(), "no bootstrap")

    def test_a_stage_that_exists_keeps_its_owner_when_none_is_named(self) -> None:
        self.seed_stack("network", "keepme")
        self.seed_stack("control", "keepme")
        proc = self.run_deploy("--stage", "direct", HAMH_OWNER="")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertNotEqual(OWNER_OF[ACCOUNT], "keepme")
        self.assertIn("deploy: owner keepme kept from the stage", proc.stdout.splitlines())
        owners = self.cdk_owners()
        self.assertTrue(owners)
        self.assertEqual(set(owners), {"keepme"}, "the CDK app tags the stacks with the owner the stage already has")
        self.assertEqual(self.ssm_owner_tags(), {"keepme"}, "so do the parameters this deploy tags by hand")
        endpoint_tags = [json.loads(c["args"][c["args"].index("--tags") + 1]) for c in self.log("aws")
                         if c["args"][:2] == ["bedrock-agentcore-control", "create-agent-runtime-endpoint"]]
        self.assertTrue(endpoint_tags)
        self.assertEqual({t["owner"] for t in endpoint_tags}, {"keepme"})

    def test_the_owner_the_stage_has_wins_over_the_owner_the_account_file_derives(self) -> None:
        # The default entry names no owner and no operator, so its derived owner is `hamh-user`; a file that names the operator derives that name (the
        # file `setup.sh` writes does). The stage keeps what it has in both cases.
        doc = json.loads(sf.ACCOUNTS.read_text())
        for entry in doc["accounts"]:
            entry["operator"] = {"kind": "role", "name": "SomeoneElse"}
        accounts = self.tmp / "accounts-operator.json"
        accounts.write_text(json.dumps(doc, indent=1) + "\n")
        self.seed_stack("network", "keepme")
        proc = self.run_deploy("--stage", "direct", HAMH_OWNER="", HAMH_ACCOUNTS_FILE=str(accounts),
                              FAKE_CALLER_ARN=f"arn:aws:sts::{ACCOUNT}:assumed-role/SomeoneElse/session")
        self.assertEqual(set(self.cdk_owners()), {"keepme"}, proc.stdout + proc.stderr)

    def test_an_owner_named_the_same_as_the_stage_has_deploys(self) -> None:
        self.seed_stack("network", "keepme")
        proc = self.run_deploy("--stage", "direct", HAMH_OWNER="keepme")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(set(self.cdk_owners()), {"keepme"})

    def test_an_owner_in_the_environment_that_differs_stops_before_anything_changes(self) -> None:
        self.seed_stack("network", "keepme")
        self.seed_stack("control", "keepme")
        proc = self.run_deploy("--stage", "direct", HAMH_OWNER="someone-else")
        self.assert_refused_before_any_change(proc, "keepme", "someone-else", "HAMH_OWNER=keepme", "owner tag", "DNS query logging configuration", "teardown")

    def test_an_owner_in_the_account_file_that_differs_stops_before_anything_changes(self) -> None:
        doc = json.loads(sf.ACCOUNTS.read_text())
        for entry in doc["accounts"]:
            entry["owner"] = "entry-owner"
        accounts = self.tmp / "accounts-owner.json"
        accounts.write_text(json.dumps(doc, indent=1) + "\n")
        self.seed_stack("network", "keepme")
        proc = self.run_deploy("--stage", "direct", HAMH_OWNER="", HAMH_ACCOUNTS_FILE=str(accounts))
        self.assert_refused_before_any_change(proc, "keepme", "entry-owner")

    def test_a_fresh_stage_gets_the_owner_the_account_file_derives(self) -> None:
        proc = self.run_deploy("--stage", "direct", HAMH_OWNER="")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(set(self.cdk_owners()), {OWNER_OF[ACCOUNT]})
        self.assertFalse([ln for ln in proc.stdout.splitlines() if "kept from the stage" in ln])

    def test_a_fresh_stage_gets_the_owner_the_environment_names(self) -> None:
        proc = self.run_deploy("--stage", "direct", HAMH_OWNER="team-new")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(set(self.cdk_owners()), {"team-new"})
        self.assertEqual(self.ssm_owner_tags(), {"team-new"})

    def test_a_stack_that_rolled_back_on_its_first_create_holds_nothing_and_decides_nothing(self) -> None:
        self.seed_stack("network", "old-attempt", status="ROLLBACK_COMPLETE")
        proc = self.run_deploy("--stage", "direct", HAMH_OWNER="team-new")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(set(self.cdk_owners()), {"team-new"})

    def test_the_control_stack_decides_when_the_network_stack_has_no_owner_tag(self) -> None:
        self.seed_stack("network", None)
        self.seed_stack("control", "keepme")
        proc = self.run_deploy("--stage", "direct", HAMH_OWNER="")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(set(self.cdk_owners()), {"keepme"})

    def test_a_stage_without_any_owner_tag_takes_the_derived_or_named_owner(self) -> None:
        self.seed_stack("network", None)
        self.seed_stack("control", None)
        proc = self.run_deploy("--stage", "direct", HAMH_OWNER="team-new")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(set(self.cdk_owners()), {"team-new"})

    def test_an_owner_tag_that_is_no_owner_value_stops_before_anything_changes(self) -> None:
        self.seed_stack("network", "has a space")
        proc = self.run_deploy("--stage", "direct", HAMH_OWNER="")
        self.assert_refused_before_any_change(proc, "has a space")

    def test_the_guard_check_reads_no_owner(self) -> None:
        self.seed_stack("network", "keepme")
        proc = self.run_deploy("--stage", "direct", "--check-guard", HAMH_OWNER="someone-else")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)


class DeployWrapperCase(unittest.TestCase):
    def test_python_wrapper_requires_3_11_for_deploy(self) -> None:
        for mode, expected_rc, reached in (("old", 2, False), ("new", 0, True), ("missing", 2, False)):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory(prefix="deploy-wrapper-") as raw:
                root = Path(raw)
                scripts = root / "scripts"
                lib = scripts / "lib"
                bindir = root / "bin"
                lib.mkdir(parents=True)
                bindir.mkdir()
                shutil.copy2(SCRIPT, scripts / "deploy.sh")
                shutil.copy2(REPO / "scripts" / "lib" / "python.sh", lib / "python.sh")
                (lib / "deploy.py").write_text('print("WRAPPER-SENTINEL")\n', encoding="utf-8")
                dirname = shutil.which("dirname")
                self.assertIsNotNone(dirname)
                (bindir / "dirname").symlink_to(dirname)
                environment = {
                    "PATH": str(bindir),
                    "HOME": str(root / "home"),
                    "UV_PYTHON_INSTALL_DIR": str(root / "uv"),
                    "UV_PYTHON_PREFERENCE": "only-managed",
                    "HAMH_PYTHON": "",
                }
                if mode == "old":
                    interpreter = root / "old-python"
                    interpreter.write_text("#!/bin/sh\nexit 1\n", encoding="utf-8")
                    interpreter.chmod(0o755)
                    environment["HAMH_PYTHON"] = str(interpreter)
                elif mode == "new":
                    environment["HAMH_PYTHON"] = sys.executable
                result = subprocess.run(
                    ["/bin/bash", str(scripts / "deploy.sh"), "--stage", "direct"],
                    capture_output=True,
                    text=True,
                    env=environment,
                    timeout=60,
                )  # nosec B603: isolated wrapper and local fixed-answer interpreter stand-in under test
                self.assertEqual(result.returncode, expected_rc, result.stdout + result.stderr)
                self.assertEqual("WRAPPER-SENTINEL" in result.stdout, reached)


if __name__ == "__main__":
    unittest.main()
