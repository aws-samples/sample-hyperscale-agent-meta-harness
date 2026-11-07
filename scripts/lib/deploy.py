"""scripts/deploy.sh: deploy the committed HEAD to stage `direct`, stack by stack.

Target: the entry of the deployed tree's accounts.json that HAMH_AWS_PROFILE names by
profile, else the active one (scripts/lib/aws_target.py); an unknown profile is a usage error (exit 2).
Its account, home Region and profile reach every AWS call (AWS_PROFILE is always its profile), bucket,
ECR URI, ARN and record; the deploy record's inputs.target names it.

The account file: HAMH_ACCOUNTS_FILE, an absolute path, replaces the tree's accounts.json for the whole deploy: this
program, every cargo build (harness-protocol's build script), every cdk call (scripts/node.sh mounts the file into the container) and every
CodeBuild image build (scripts/codebuild-run.sh puts it into the source bundle as a member of its own) read it. Unset or empty, the tree's
committed accounts.json is read and nothing below changes. The file in use is a separate input from the source: `sourceTree` is still the hash
of the committed deployable paths, and the deploy record and every image-build record carry `deploymentConfigDigest` beside it (`committed`, or
the SHA-256 of the file in use), also kept in SSM /hamh/<stage>/deployment-config-digest. A deploy whose digest differs from that earlier record
says so before it builds anything (`deploy: deployment config <new> differs from the earlier record <old> ...`); it reports, it does not stop.

Flow (every step prints `deploy: ...`; failures end with `DEPLOY-FAIL <step> <detail>`, exit 1):
1. Refuse bad flags and uncommitted tracked changes (exit 2) before any AWS call; the caller must be the
   target account (`account`) and, when the entry names an `operator`, that identity or the setup
   runner's role (`DEPLOY-FAIL caller <arn> is not <kind>/<name>...`, which names both; an entry without the field is checked
   by account alone, as it always was); the stage AZ name must map to its accounts.json AZ ID in this account
   (`azs`, DescribeAvailabilityZones); the stage's VPC CIDR (10.50.0.0/16) must overlap no other VPC of the account
   (`vpc-cidr`). Nothing is built before these pass.
1a. The owner of the stage (`settle_owner`, right after the AZ and VPC checks, before the first record of the stage is read): the `owner` tag of the network stack, else of the control stack (a stack that rolled back on its
   first create is ignored) is the owner of this deploy unless HAMH_OWNER or the entry's `owner` names one. A named owner that differs, or a tag that is no owner value, ends
   `DEPLOY-FAIL owner ...` (exit 1) before anything is built or changed, because a changed `owner` tag replaces the network stack's DNS query logging configuration and the
   update rolls back (AWS::Route53Resolver::ResolverQueryLoggingConfig, Tags: "Update requires: Replacement"). Without a named owner the stage's own is kept and
   `deploy: owner <value> kept from the stage` is printed; a stage that does not exist gets the derived or named value. The result reaches every cdk call as HAMH_OWNER, and the
   resources this deploy tags by hand.
2. Build the Lambdas (LAMBDAS: worker-api, control-api, run-session, research-api, research-job, plus the delivery package, one cargo lambda arm64 call)
   for the selection, in its own directories: CARGO_TARGET_DIR is
   <repo>/target/deploy/<account>-<region> and the bootstraps go to <that>/lambda/<stage> (`--lambda-dir`);
   HAMH_LAMBDA_DIR points there. The builds also get HAMH_AWS_PROFILE=<the selected profile>, which the Rust build
   compiles in (harness-protocol's build script), whatever the caller exported. Before anything is uploaded or handed to
   CDK, every build is checked (`deploy: build selection ok <sel>: lambda bytes=<binaries> outputs=<n>`): repo-gateway
   must embed the selected account id, agentd the selected Region, and the harness-protocol build script output in the
   target directory must name the selected entry; else `DEPLOY-FAIL build-selection <binary> built for <sel>, target is
   <sel>: ...` (exit 1) with nothing published. Install infra/ dependencies from the lockfile (`pnpm install
   --frozen-lockfile`), so a fresh checkout deploys.
3. Stack order: `cdk synth`, then cdk.out/manifest.json dependencies, ties broken by DEPLOY_PRIORITY.
3a. CDK bootstrap (`bootstrap`): SSM /cdk-bootstrap/hnb659fds/version at or above the assembly's highest
   requiresBootstrapStackVersion (MIN_BOOTSTRAP when none) is used unchanged; a lower one fails, since an
   existing CDKToolkit is never modified; none: `cdk bootstrap aws://<account>/<region>` with the tags
   project, stage=shared, owner and managed-by=deploy.sh, then the version is read again.
4. Worker image, tagged `in-<image key>` (the key covers the crate closure of agentd and the IMAGE_PATHS of lib/digests.py):
   reused when ECR has the tag. Otherwise: a fresh stage (no ECR repository) first gets a phase-1 pass of every stack without
   an image (the runtimes stack holds the image-build project). Then agentd is cross-built in one zigbuild through
   scripts/lib/agentd_build.py (its argv, the CARGO_HOME remap and the selection environment; the image-build record carries
   the recipe digest; CARGO_TARGET_DIR as in step 2: the binary is read from <target dir>/aarch64-unknown-linux-gnu/release),
   checked as in step 2 (agentd must embed the selected Region; `deploy: build selection ok <sel>: worker...`), uploaded to
   builds/out/agentd/<sha>/agentd, and scripts/codebuild-run.sh builds the image with its SHA-256 (AGENTD_SHA256) and
   LANG_FAMILY=direct.
   Before the first image that COPYs the Cline fork (CLINE_SHA256), the fork binary is settled by
   `--fork-source auto|build|copy` (default auto), and the image is built with the SHA-256 of the fork that was settled:
   - the lock's object: s3://<bucket>/engine/cline/<lock binarySha256>/cline with that SHA-256 (its sha256 metadata, or the hash of
     its bytes when it has none), used in auto and copy mode when present;
   - the pointer of a source build of this stage, s3://<bucket>/engine/cline/by-tree/<series tree>-<toolchain digest>.json, which names
     the fork a source build recorded for the same series tree and toolchain pins (auto and build mode); a pointer that names a missing
     or other object is fatal, and a deploy of a changed image reuses the recorded fork and never builds it again;
   - the engine/lock.toml `artifact`, copied with the profile of the accounts entry whose stage bucket holds it, in auto mode only when
     that profile is configured on this host (`aws configure list-profiles`) and in copy mode always; a copy that fails is fatal, never
     a build, and a copy with another SHA-256 is fatal;
   - a build on CodeBuild ARM from the pinned upstream source (images/codebuild/cline-fork-arm64.yml), in build mode, and in auto
     mode when nothing above answered. Builds are not bit-reproducible, so it is accepted on the evidence of the build itself, not on
     equality with the lock's SHA-256: the metadata and the record the build computed must show the locked commit, series tree and
     toolchain pins, an aarch64 ELF, telemetry off, leaks 0, a frozen lockfile and the suites run; the smoke test names the locked
     version; the CodeBuild build id equals the id this deploy started; and the SHA-256 the build computed equals the SHA-256 of the
     bytes downloaded. The binary is promoted to engine/cline/<its SHA-256>/cline with a create-only put and the pointer is written
     create-only; a lost pointer race uses the winner's fork. The deploy record names the lock's SHA-256 as a reference
     (`forkBinary.lockBinarySha256`, `matchesLock`) next to the SHA-256 that was built.
   The delivery image (images/delivery/Dockerfile: the three delivery binaries, git, gitleaks), tagged
   `dl-<delivery key>`: reused when ECR has the tag. Otherwise a stage without hamh-<stage>-delivery first
   deploys the delivery stack's phase 1 (repository and roles), then the bootstraps are uploaded to
   builds/out/delivery/<sha>/<bin> and scripts/codebuild-run.sh builds images/codebuild/delivery.yml.
5. One pass over every stack (`cdk deploy <stack> --exclusively`) with HAMH_WORKER_IMAGE and HAMH_DELIVERY_IMAGE,
   so each producer publishes the outputs its consumers read. A pass never omits a running worker image: until the new
   one exists, a pass carries the image the runtime serves.
6. The runtime's `live` endpoint version (runtimes stack outputs `<Name>RuntimeId`) must be READY,
   VPC mode, platform V1, requireMMDSV2 true and run the deployed image.
6a. Version endpoints. AgentCore's qualifier is an endpoint name,
   never a bare version, so a run pins `v<N>`, the endpoint on the worker version N its first dispatch
   found behind `live`. The worker runtime gets `v<N>` for the version `live` serves, tagged, and READY
   serving N before the digests (`deploy: endpoint v<N> on <runtime id> serves version <N>
   (created|present)`; otherwise `DEPLOY-FAIL version-endpoint <runtime id> v<N> <status>`). Older
   `v<M>` are deleted unless M is the highest version below N (the rollback target) or a RUNNING execution of hamh-<stage>-run
   is pinned to it (its Run item's `runtimeEndpoint`).
6b. Invocation logging: set is read and never overwritten; unset,
   a `read-only` entry (ham-acct-b02) records `unset-read-only` and writes nothing, and an
   `enable-and-restore` entry (acct-a01) first writes the prior state to SSM
   /hamh/<stage>/invocation-logging-prior (tagged, never overwritten), then enables it to
   /hamh/<stage>/bedrock-invocations; teardown.sh restores the recorded state.
7. Digests (scripts/lib/digests.py) to SSM /hamh/<stage>/{deployment-digest,deployed-source-tree,
   image-digest}, plus /hamh/<stage>/worker-revision and /hamh/<stage>/deployment-config-digest, tagged; the CDK asset objects to s3://<harness bucket>/deploy/<stage>/cdk-assets.json
   for teardown; an 11.6 evidence record to DEPLOY_RECORD_DIR.
7a. The Go module store. The packages stack creates the bucket hamh-<stage>-gomod-<account> empty and nothing in the deploy fills it, so a stage in a fresh
   account fails the events sample on 404s for every Go module. One `s3api list-objects-v2 --max-items 1` says whether the bucket holds an object and prints one line,
   always before the pass line: `GOMOD-STORE OK ...`, `GOMOD-STORE EMPTY ...` (with the fill command and the check command of scripts/gomod-store.sh) or
   `GOMOD-STORE UNKNOWN ...` (the bucket cannot be read). It is a report, never a failure, and it writes nothing; the deploy record carries it as
   `measurements.gomodStore` {bucket, state: filled|empty|unknown}. The release gate stops on an empty store before the events sample.
8. `DEPLOY-PASS stage=<s> digest=<12> sourceTree=<12>`.

HAMH_WORKER_REVISION (SSM /hamh/<stage>/worker-revision, default 0) reaches every cdk call and the
runtime's environment; --bump-worker-version adds 1, a CloudFormation environment change that
makes a new runtime version from the same source tree. A plain deploy keeps it.

HAMH_TOOLS_MODE is the stage setting of the tool layer. It is per deploy, like HAMH_UTILITY_MODEL, and it becomes the worker runtime's
AGENTD_TOOLS_MODE, always as a concrete off or on (infra/lib/config.ts `resolveToolsMode`, infra/lib/stacks/runtimes.ts). Any other word is a
usage error (exit 2) before any AWS call. Every cdk call gets the resolved word. The deploy says the mode on a `deploy: tools mode ...` line,
names the live runtime's AGENTD_TOOLS_MODE (before any build) and records `inputs.toolsMode` {setting, mode, previous, liveRuntime, change,
stageKind}. A mode that differs from the live runtime's (a missing variable differs) is a runtime environment change, so a new runtime version:
it meets the deploy guard like a bump does, before the Lambda build.

HAMH_COMPUTE_INSTANCES is the stage setting of the
Instances compute type. Off, the stage keeps the one microVM worker runtime. On, the runtimes stack also holds an Instances capacity provider and a
second worker runtime (`WorkerPythonInstances`) on the same image, and the security stack its operator role (infra/lib/config.ts
`resolveComputeInstances`, one table: infra/test/fixtures/compute-instances.json). Any other word is a usage error (exit 2) before any AWS call.
Every cdk call gets the resolved word. The deploy says the setting on a `deploy: compute instances ...` line, compares it with the live stage (the
runtimes stack has the capacity provider output or the Instances runtime's id output) and records `inputs.computeInstances`. Adding the part (the stage has
none and the setting is on) meets the deploy guard (below) before the Lambda build, like a bump. A deploy never removes the part: a stage that has it and a
setting that resolves to off (unset included) ends `DEPLOY-FAIL instances-off ...` before any cdk call, build or run listing, because removing it ends every
machine and session on the provider and cannot finish in CloudFormation (the security stack deletes the operator role first, then the runtimes update is
refused); removal is a separate, explicit step, for now the stage teardown. Each runtime must serve its own
isolation words (RUNTIME_WORDS: AGENTD_ENGINE_SANDBOX `uid` on the microVM runtime, `landlock` and an empty AGENTD_ENGINE_USER on the Instances runtime) and the
Instances runtime must run on the capacity provider, else the deploy fails (`runtime-flags`).

Deploy guard: a deploy
that makes a new worker runtime version (a bump, a worker image other than the one the live
endpoint serves, a tools mode other than the live runtime's, an isolation word the live runtime lacks or carries another way, adding the Instances part, or a
changed Instances machine configuration, which replaces the capacity provider: its name ends in a hash of the configuration, compared after the synth) first lists the RUNNING executions of hamh-<stage>-run; any is
`DEPLOY-FAIL guard <n> running run(s) on hamh-<stage>-run pin the worker runtime: <ids>` before a
build or a deploy (a bump is checked before the Lambda build). `--check-guard` checks the account
and the guard only: `DEPLOY-GUARD PASS stage=<s> running=0` (exit 0) or the same DEPLOY-FAIL (exit 1).

Previous names: the previous spellings of the variables this program reads (the earlier prefix and the same suffix) work
after the current ones, with one notice on stderr (lib/aws_target.py `env_get`), and the children get the selectors under the new names only (`fold_previous`). A VPC of the
account that carries the previous project tag and this stage is the old stage's: an overlap with it stops the deploy with the reason (`./setup.sh --teardown` removes the old
stage). The artifact of the engine pin may sit in a bucket of the retired live stage of the previous names (`artifact_owner`).

Test hooks: DEPLOY_ROOT, DEPLOY_AWS_BIN, DEPLOY_CARGO_BIN, DEPLOY_CARGO_SLOT (empty: no slot),
DEPLOY_NODE_BIN, DEPLOY_CODEBUILD_BIN, HAMH_ACCOUNTS_FILE, DEPLOY_RECORD_DIR, DEPLOY_CRITERIA, DEPLOY_ENDPOINT_POLL_S
(default 5) and DEPLOY_ENDPOINT_TIMEOUT_S (default 600), the wait for a new version endpoint. DEPLOY_AWS_CALL_LOG names a file that gets one
JSON line {argv, profile} for every call this program makes to the aws CLI (not the calls of codebuild-run.sh or cdk); the fresh-account
dry run reads it to show that no call named a bucket of another account.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import ipaddress
import json
import os
import re
import shlex
import shutil
import socket
import subprocess  # nosec B404: starts programs with argument lists, never through a shell
import sys
import tempfile
import time
import tomllib
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import agentd_build  # noqa: E402
import aws_target  # noqa: E402
import build_selection  # noqa: E402
import digests  # noqa: E402
import gomod_store  # noqa: E402

# The generations of names a stage can have been deployed under: the current project tag, then the frozen value of the
# previous names. This deploy only ever writes the current one; it reads the previous one to tell the old stage's VPC from a stranger's.
PROJECT = "hyperscale-agent-meta-harness"
LEGACY_PROJECT = "cline-agentcore-harness"  # COMPAT-CAH-STAGE: the project tag of a stage deployed under the previous names
AZS = {"direct": 1}
# The VPC CIDR of the stage (infra/lib/config.ts stageConfig `vpcCidr`; scripts/tests/test_deploy_direct.py pins the two together).
# A deploy refuses a CIDR that overlaps any other VPC of the account.
VPC_CIDRS = {"direct": "10.50.0.0/16"}
# The stage deploys five Lambdas: worker-api, control-api and run-session, and the research route's research-api and research-job
# (infra/lib/config.ts DIRECT_LAMBDAS); the delivery functions come from the delivery image. The cargo packages are four: the package
# `research-api` builds two binaries, so one `-p research-api` yields the bootstraps of both functions.
LAMBDA_PACKAGES = ("worker-api", "control-api", "run-session", "research-api")
LAMBDAS = ("worker-api", "control-api", "run-session", "research-api", "research-job")
# The endpoint a run pins on worker version N (run-session).
VERSION_ENDPOINT = re.compile(r"v([1-9][0-9]*)")
# The result line of scripts/codebuild-run.sh on success: `CODEBUILD PASS id=<project>:<uuid> sha=<sha7> seconds=<n>`.
CODEBUILD_RESULT = re.compile(r"^CODEBUILD PASS id=(\S+)")
DEPLOY_PRIORITY = ("network", "security", "data", "packages", "control", "runtimes", "orchestration", "delivery", "observability")
# The stacks whose `owner` tag is the owner of a stage that exists, in the order they are read: the network stack first (it holds the DNS query logging configuration,
# which CloudFormation replaces when its tags change, and the replacement fails), then the control stack. A stack in one of the states below holds nothing: its first create
# rolled back and the deploy makes it again.
OWNER_STACKS = ("network", "control")
EMPTY_STACK_STATUSES = ("ROLLBACK_COMPLETE", "CREATE_FAILED", "DELETE_COMPLETE", "REVIEW_IN_PROGRESS")
AGENTD_TARGET = agentd_build.TRIPLE
# The Rust binaries the worker image carries, with the build argument that pins each one.
WORKER_BINARIES = (("agentd", "AGENTD_SHA256"),)
# The delivery container-image functions (infra/lib/config.ts DELIVERY_FUNCTIONS): package `delivery`,
# one binary each, shipped in the hamh-<stage>-delivery image rather than as zip Lambdas.
DELIVERY_PACKAGE = "delivery"
DELIVERY_BINS = ("repo-gateway", "bundle-validator", "packager")
# Delivery image inputs besides the delivery crate closure.
DELIVERY_IMAGE_PATHS = ("Cargo.toml", "Cargo.lock", "accounts.json", "images/delivery", "images/codebuild/delivery.yml")


@dataclass(frozen=True)
class BinaryImage:
    """A runtime image built on CodeBuild ARM from workspace binaries cross-built in one zigbuild."""

    name: str
    """Record and log name: the record carries `<name>ImageTag` and `<name>Image`."""
    repo: str
    """ECR repository `hamh-<stage>-<repo>`."""
    tag_prefix: str
    binaries: tuple[tuple[str, str], ...]
    """(package and binary, the build argument carrying its SHA-256)."""
    paths: tuple[str, ...]
    """Tag inputs besides the binaries' crate closures."""
    buildspec: str
    runtime: str
    """The runtimes stack names the runtime's id in output `<runtime>RuntimeId`."""
    env: str | None
    """The CDK entry variable that carries the image (None: the worker's explicit argument)."""
    steps: tuple[str, str, str]
    """Failure step names: cross-build, upload, image build."""
    cline: bool = False
    """The build takes the promoted Cline fork (CLINE_SHA256): a worker image."""
    worker: bool = False
    """A worker runtime: runs pin its versions, so a new image is guarded."""


WORKER_IMAGE = BinaryImage(
    name="worker", repo="worker-python", tag_prefix="in", binaries=WORKER_BINARIES, paths=digests.IMAGE_PATHS,
    buildspec="images/codebuild/worker-python.yml", runtime="WorkerPython", env=None,
    steps=("agentd-build", "agentd-upload", "image-build"))
# Every worker runtime a run pins, with the family whose Run item entry holds the pin.
WORKER_RUNTIMES = {WORKER_IMAGE.runtime: "python"}
IMAGE_ENV = ("HAMH_WORKER_IMAGE", "HAMH_DELIVERY_IMAGE")
# The tool layer's stage setting: infra/lib/config.ts TOOLS_MODES and DEFAULT_TOOLS_MODE, pinned together with the words of
# infra/test/fixtures/tools-mode.json by scripts/tests/test_deploy_direct.py. TOOLS_MODE_VAR is the worker runtime's environment variable.
TOOLS_MODES = ("off", "on")
DEFAULT_TOOLS_MODE = "off"
# The only characters HAMH_TOOLS_MODE is trimmed of (infra/lib/config.ts `TOOLS_SPACE`): Python's strip() and JavaScript's trim() differ on
# U+001C to U+001F, U+0085 and U+FEFF, so neither resolver uses them and both accept the same words.
TOOLS_SPACE = " \t\n\r\f\v"
TOOLS_MODE_VAR = "AGENTD_TOOLS_MODE"
# The Instances compute type's stage setting: infra/lib/config.ts COMPUTE_INSTANCES_MODES and DEFAULT_COMPUTE_INSTANCES, pinned together with the words of
# infra/test/fixtures/compute-instances.json by scripts/tests/test_deploy_direct.py. It trims the same ASCII whitespace as the tool layer's setting.
COMPUTE_INSTANCES_MODES = ("off", "on")
DEFAULT_COMPUTE_INSTANCES = "off"
# The second worker runtime (runtimes stack output `<name>RuntimeId`) and the stack output that says the capacity provider exists.
INSTANCES_RUNTIME = "WorkerPythonInstances"
INSTANCES_PROVIDER_OUTPUT = "InstancesCapacityProviderArn"
# `capacity-provider/<name>-<id>` (a name has no hyphen) and the type of the provider in a synthesized template.
PROVIDER_ARN_NAME = re.compile(r"capacity-provider/([^-/]+)-")
PROVIDER_TYPE = "AWS::BedrockAgentCore::CapacityProvider"
# The runtime environment words of the engine's isolation each worker runtime must serve: the microVM has a second user (`uid`), an Instances container
# a Landlock sandbox (`landlock`). In landlock mode agentd runs the engine as its own user and refuses to start it while AGENTD_ENGINE_USER names another one, so the Instances
# runtime sets the variable to the empty string (the worker image sets it to `agent`, which the microVM runtime keeps). infra/lib/stacks/runtimes.ts sets these words and
# scripts/tests/test_deploy_direct.py pins the two together. A live runtime that lacks a word or carries another is a runtime change (the deploy guard) before the deploy and an
# error after it (`check_runtimes`).
SANDBOX_VAR = "AGENTD_ENGINE_SANDBOX"
ENGINE_USER_VAR = "AGENTD_ENGINE_USER"
RUNTIME_WORDS = {WORKER_IMAGE.runtime: {SANDBOX_VAR: "uid"}, INSTANCES_RUNTIME: {SANDBOX_VAR: "landlock", ENGINE_USER_VAR: ""}}
SANDBOX_WORDS = {name: words[SANDBOX_VAR] for name, words in RUNTIME_WORDS.items()}
RECORDS = os.path.join(os.environ.get("XDG_STATE_HOME") or os.path.join(os.environ.get("HOME", "/nonexistent"), ".local", "state"), "ham")
# A fresh account.
BOOTSTRAP_PARAM = "/cdk-bootstrap/hnb659fds/version"
# The bootstrap version `cdk deploy` needs when the synthesized assembly names none.
MIN_BOOTSTRAP = 6
def bootstrap_tags(owner: str) -> tuple[str, ...]:
    """Tags of a bootstrap deploy.sh creates: shared by both stages, so no stage's teardown inventory holds it. `owner` is the owner of the entry (aws_target.owner_of)."""
    return (f"project={PROJECT}", "stage=shared", f"owner={owner}", "managed-by=deploy.sh")


def project_tags(owner: str, stage: str) -> dict[str, str]:
    """The four project tags of a resource deploy.sh creates in a stage. `owner` is the owner of the entry (aws_target.owner_of)."""
    return {"project": PROJECT, "stage": stage, "owner": owner, "managed-by": "deploy.sh"}


# The refusals of settle_owner: one line each, read by a person who runs setup.sh or scripts/deploy.sh.
OWNER_CHANGE_REFUSED = (
    "Stage {stage} already has the owner tag {have}; this run would set {want} through {named}. Changing the owner tag of a deployed stage replaces the network "
    "stack's DNS query logging configuration, and CloudFormation rolls the update back; nothing was changed. You can keep HAMH_OWNER={have} (or unset it), or remove "
    "the stage with ./setup.sh --teardown and deploy it again.")
OWNER_TAG_REFUSED = (
    'Stage {stage} has the owner tag "{have}", which is not a valid owner value (1 to 64 letters, digits or + = . @ _ -). A deploy cannot keep it; changing the tag '
    "of a deployed stage replaces the network stack's DNS query logging configuration, so nothing was changed. You can remove the stage with ./setup.sh --teardown "
    "and set it up again.")
FORK_SPEC = "images/codebuild/cline-fork-arm64.yml"
# --fork-source: where the Cline fork binary of a stage comes from.
FORK_SOURCES = ("auto", "build", "copy")
FORK_OUT = "builds/out/cline-fork"
FORK_POINTER_DIR = "engine/cline/by-tree"
S3_MISSING = ("(404)", "Not Found", "NoSuchKey", "does not exist")
S3_EXISTS = ("PreconditionFailed", "(412)")
HEX64 = re.compile(r"^[0-9a-f]{64}$")
# Where deploy.sh records the invocation-logging state it found before it enabled logging (teardown restores it).
PRIOR_LOGGING_PARAM = "/hamh/{stage}/invocation-logging-prior"


class Fail(Exception):
    def __init__(self, step: str, detail: str = "") -> None:
        super().__init__(f"{step} {detail}".strip())


class ToolsModeError(ValueError):
    """HAMH_TOOLS_MODE is neither off nor on: a usage error, raised before any AWS call."""


def resolve_tools_mode(setting: str | None) -> str:
    """The concrete mode HAMH_TOOLS_MODE names: unset, empty or blank is the default; `off` or `on` in any letter case, surrounding ASCII whitespace ignored.
    Any other word raises ToolsModeError. infra/lib/config.ts `resolveToolsMode` resolves the same words (one table: infra/test/fixtures/tools-mode.json)."""
    word = (setting or "").strip(TOOLS_SPACE).lower()
    if word == "":
        return DEFAULT_TOOLS_MODE
    if word in TOOLS_MODES:
        return word
    raise ToolsModeError(f'HAMH_TOOLS_MODE="{setting}" is not one of {", ".join(TOOLS_MODES)} (unset or empty means {DEFAULT_TOOLS_MODE})')


class ComputeInstancesError(ValueError):
    """HAMH_COMPUTE_INSTANCES is neither off nor on: a usage error, raised before any AWS call."""


def resolve_compute_instances(setting: str | None) -> str:
    """The concrete word HAMH_COMPUTE_INSTANCES names: unset, empty or blank is the default; `off` or `on` in any letter case, surrounding ASCII whitespace ignored.
    Any other word raises ComputeInstancesError. infra/lib/config.ts `resolveComputeInstances` resolves the same words (one table: infra/test/fixtures/compute-instances.json)."""
    word = (setting or "").strip(TOOLS_SPACE).lower()
    if word == "":
        return DEFAULT_COMPUTE_INSTANCES
    if word in COMPUTE_INSTANCES_MODES:
        return word
    raise ComputeInstancesError(
        f'HAMH_COMPUTE_INSTANCES="{setting}" is not one of {", ".join(COMPUTE_INSTANCES_MODES)} (unset or empty means {DEFAULT_COMPUTE_INSTANCES})')


def say(msg: str) -> None:
    print(f"deploy: {msg}", flush=True)


class Deployer:
    def __init__(self, stage: str, bump: bool = False, fork_source: str = "auto") -> None:
        self.stack_deps: dict[str, set[str]] = {}
        here = Path(__file__).resolve().parent
        self.root = Path(os.environ.get("DEPLOY_ROOT") or here.parents[1])
        # The accounts.json entry of the tree being deployed that HAMH_AWS_PROFILE selects
        # (else the active one); aws_target.TargetError is a usage error.
        # COMPAT-CAH: the previous spellings of the selectors are folded into the new ones, so the children (cargo, the CDK container) get one spelling of each.
        environ = aws_target.fold_previous(os.environ)
        env = dict(environ)
        env.setdefault(aws_target.FILE_HOOK, str(self.root / "accounts.json"))
        self.target = aws_target.select(env)
        self.account, self.region, self.profile = self.target.account, self.target.region, self.target.profile
        # The value of the `owner` tag of every resource this deploy creates by hand: the entry's, as the CDK app tags the stacks with it (infra/lib/owner.ts).
        self.owner = aws_target.owner_of(self.target)
        self.entries = aws_target.accounts(env)
        # `committed`, or the SHA-256 of the account file HAMH_ACCOUNTS_FILE names. Read from the caller's environment, not from `env`
        # above, which holds the tree's own accounts.json by default.
        self.config_digest = aws_target.config_digest(environ)
        self.accounts_file = environ.get(aws_target.FILE_HOOK, "")
        # The tool layer's stage setting, resolved once; a bad word is a usage error before any AWS call. The resolved word, not the
        # caller's spelling, reaches every cdk call, so the word compared with the live runtime is the word the CDK app writes into it.
        raw = aws_target.env_get("HAMH_TOOLS_MODE") or None
        self.tools_mode = resolve_tools_mode(raw)
        self.tools_setting = (raw or "").strip(TOOLS_SPACE) or None
        self.tools_state: dict = {}
        # The Instances compute type's stage setting, resolved once in the same way; the resolved word reaches every cdk call.
        raw_instances = aws_target.env_get("HAMH_COMPUTE_INSTANCES") or None
        self.compute_instances = resolve_compute_instances(raw_instances) == "on"
        self.instances_setting = (raw_instances or "").strip(TOOLS_SPACE) or None
        self.instances_state: dict = {}
        self.isolation: dict = {}
        self.stage = stage
        self.prefix = f"hamh-{stage}"
        self.bucket = f"{self.prefix}-harness-{self.account}"
        self.repo_name = f"{self.prefix}-{WORKER_IMAGE.repo}"
        self.delivery_repo = f"{self.prefix}-delivery"
        self.delivery_uri = f"{self.account}.dkr.ecr.{self.region}.amazonaws.com/{self.delivery_repo}"
        self.delivery_image: str | None = None
        self.aws_bin = os.environ.get("DEPLOY_AWS_BIN", "aws")
        self.cargo = shlex.split(os.environ.get("DEPLOY_CARGO_BIN", "cargo"))
        slot = os.environ.get("DEPLOY_CARGO_SLOT", str(self.root / "scripts" / "cargo-slot.sh"))
        self.slot = [slot] if slot else []
        self.node = os.environ.get("DEPLOY_NODE_BIN", str(self.root / "scripts" / "node.sh"))
        self.codebuild = os.environ.get("DEPLOY_CODEBUILD_BIN", str(self.root / "scripts" / "codebuild-run.sh"))
        # Builds go to this selection's own directories: the cargo
        # target directory keyed by account and Region, the Lambda bootstraps CDK zips keyed by stage as well.
        self.target_dir = build_selection.target_dir(self.root, self.target)
        self.lambda_dir = build_selection.lambda_dir(self.root, self.target, stage)
        # The variables this deploy sets itself for the CDK app (the image references, the Lambda folder, the worker revision, the owner, the two settings) are given
        # under their current names only; a previous spelling in the caller's environment is not handed on next to them.
        own = (*IMAGE_ENV, "HAMH_LAMBDA_DIR", "HAMH_WORKER_REVISION", "HAMH_TOOLS_MODE", "HAMH_COMPUTE_INSTANCES")
        self.env = {k: v for k, v in environ.items() if k not in (*IMAGE_ENV, "HAMH_LAMBDA_DIR") and k not in map(aws_target.previous_name, own)}
        # The deploy's cargo builds go ahead of helper builds waiting for a slot (cargo-slot.sh).
        self.env["CARGO_SLOT_PRIORITY"] = "high"
        self.env["HAMH_TOOLS_MODE"] = self.tools_mode
        self.env["HAMH_COMPUTE_INSTANCES"] = "on" if self.compute_instances else "off"
        # The selected entry's profile always: an inherited AWS_PROFILE never picks the account.
        self.env["AWS_PROFILE"] = self.profile
        # A cargo build compiles the selection in (harness-protocol's build script reads HAMH_AWS_PROFILE): the
        # selected profile and the selection's target directory, whatever the caller exported.
        self.build_env = dict(self.env, CARGO_TARGET_DIR=str(self.target_dir), HAMH_AWS_PROFILE=self.profile)
        # The fork binary's outcome, once per deploy, before the first image that COPYs it.
        self.fork: dict | None = None
        self.fork_source = fork_source
        self.assets: dict[str, set] = {"files": set(), "images": set()}
        self.bump = bump
        self.revision = 0
        # Binaries this deploy cross-built and uploaded: the package sets built, and each binary's
        # SHA-256 as last uploaded to builds/out/<binary>/<sha>/.
        self.built_sets: set[tuple[str, ...]] = set()
        self.uploaded: dict[str, str] = {}

    # ---- process helpers --------------------------------------------------------------------

    def run(self, cmd: list[str], cwd: Path | None = None, env: dict | None = None, stream: bool = False):
        log = os.environ.get("DEPLOY_AWS_CALL_LOG")
        if log and cmd and cmd[0] == self.aws_bin:
            with open(log, "a", encoding="utf-8") as f:
                f.write(json.dumps({"argv": cmd[1:], "profile": (env or self.env).get("AWS_PROFILE")}) + "\n")
        if stream:
            return subprocess.run(cmd, cwd=cwd or self.root, env=env or self.env, text=True)  # nosec B603: argument list built here, no shell, program is git, the interpreter or a configured binary  # nosemgrep: dangerous-subprocess-use-audit -- argument list built here from its own options and paths, no shell
        return subprocess.run(cmd, cwd=cwd or self.root, env=env or self.env, capture_output=True, text=True)  # nosec B603: argument list built here, no shell, program is git, the interpreter or a configured binary  # nosemgrep: dangerous-subprocess-use-audit -- argument list built here from its own options and paths, no shell

    def run_codebuild(self, cmd: list[str]) -> tuple[int, str | None]:
        """Runs scripts/codebuild-run.sh with its output on this console. Returns its exit code and the id of the CodeBuild build its
        success line names (`CODEBUILD PASS id=<project>:<uuid> ...`; the last such line), None when it printed none. The build id goes
        into the image-build record (R21 reads the build back). A byte of the build log that is no UTF-8 is replaced, not an error."""
        build_id = None
        with subprocess.Popen(cmd, cwd=self.root, env=self.env, text=True, encoding="utf-8", errors="replace",  # nosec B603: argument list built here, no shell, program is git, the interpreter or a configured binary  # nosemgrep: dangerous-subprocess-use-audit, python36-compatibility-Popen1, python36-compatibility-Popen2 -- argument list built here from its own options and paths, no shell; scripts run on Python 3.8 or newer as scripts/lib/python.sh picks it, so the errors and encoding arguments of Popen exist
                              stdout=subprocess.PIPE) as proc:
            for line in proc.stdout:  # type: ignore[union-attr]
                sys.stdout.write(line)
                sys.stdout.flush()
                m = CODEBUILD_RESULT.match(line)
                if m:
                    build_id = m.group(1)
        return proc.returncode, build_id

    def aws(self, *args: str, ok_missing: tuple[str, ...] = ()) -> dict | None:
        proc = self.run([self.aws_bin, *args, "--region", self.region, "--output", "json"])
        if proc.returncode != 0:
            if any(m in proc.stderr for m in ok_missing):
                return None
            raise Fail("aws", f"{' '.join(args[:2])}: {proc.stderr.strip().splitlines()[-1] if proc.stderr.strip() else proc.returncode}")
        return json.loads(proc.stdout) if proc.stdout.strip() else {}

    def git(self, *args: str) -> str:
        return subprocess.run(["git", "-C", str(self.root), *args], capture_output=True, text=True).stdout.strip()  # nosec B603: argument list built here, no shell, program is git, the interpreter or a configured binary; B607: bare name of a standard tool such as git, bash, tar or aws looked up on the PATH of the host running this tool

    def cdk(self, *args: str, image: str | None) -> int:
        env = dict(self.env, HAMH_LAMBDA_DIR=str(self.lambda_dir), HAMH_WORKER_REVISION=str(self.revision))
        if image:
            env["HAMH_WORKER_IMAGE"] = image
        if self.delivery_image:
            env["HAMH_DELIVERY_IMAGE"] = self.delivery_image
        rc = self.run([self.node, "cdk", *args, "-c", f"stage={self.stage}"], cwd=self.root / "infra", env=env, stream=True).returncode
        self.collect_assets()
        return rc

    # ---- steps ------------------------------------------------------------------------------

    def check_account(self) -> None:
        doc = self.aws("sts", "get-caller-identity") or {}
        got = doc.get("Account", "")
        if got != self.account:
            raise Fail("account", got or "none")
        op = self.target.operator
        if op is not None:
            # The entry names the identity that operates its stage; the stage trusts that identity, so a deploy made as another one would leave a stage that
            # refuses its operator. The setup runner of the account deploys with its own role and passes (aws_target.operator_may_deploy).
            arn = doc.get("Arn", "")
            if not aws_target.operator_may_deploy(self.account, op, arn):
                where = "the committed accounts.json" if self.config_digest == "committed" else f"HAMH_ACCOUNTS_FILE={self.accounts_file}"
                raise Fail("caller", f"{arn or 'no identity'} is not {op.label}, the operator that {where} names for account {self.account}. "
                                     f"Sign in as {op.label} (the account's setup runner role {aws_target.SETUP_RUNNER_ROLE} may deploy as well), or correct \"operator\" in that file")

    def stage_owner(self) -> str | None:
        """The value of the `owner` tag of the stage that exists in the account: the tag of the network stack, else of the control stack. None when the stage has no
        such stack (a stage that is new, or whose first create rolled back) or its stacks carry no `owner` tag."""
        for short in OWNER_STACKS:
            doc = self.aws("cloudformation", "describe-stacks", "--stack-name", f"{self.prefix}-{short}", ok_missing=("does not exist",))
            stack = ((doc or {}).get("Stacks") or [{}])[0]
            if not stack or stack.get("StackStatus") in EMPTY_STACK_STATUSES:
                continue
            for tag in stack.get("Tags", []):
                if tag.get("Key") == "owner":
                    return tag.get("Value", "")
        return None

    def settle_owner(self) -> None:
        """The owner of this deploy, before anything is built or changed. A change of the `owner` tag is a change of the tag of every resource of the stage, and
        CloudFormation replaces the network stack's DNS query logging configuration when its tags change (AWS::Route53Resolver::ResolverQueryLoggingConfig, Tags:
        "Update requires: Replacement"); the replacement fails and the stack rolls back. So a stage that exists keeps its owner: without a named owner (HAMH_OWNER or
        the entry's `owner`) the owner is the stage's own, and a named owner that differs ends the deploy (`DEPLOY-FAIL owner`). A stage that does not exist gets the
        derived or named owner. The value reaches the CDK app and the resources this deploy tags by hand as HAMH_OWNER, so the two cannot disagree."""
        have = self.stage_owner()
        if have is not None:
            try:
                aws_target.parse_owner(have, "owner tag")
            except aws_target.TargetError:
                raise Fail("owner", OWNER_TAG_REFUSED.format(stage=self.prefix, have=have)) from None
            if aws_target.owner_is_named(self.target):
                if self.owner != have:
                    named = "HAMH_OWNER" if aws_target.env_get(aws_target.OWNER_ENV) else "the owner of the account file"
                    raise Fail("owner", OWNER_CHANGE_REFUSED.format(stage=self.prefix, have=have, want=self.owner, named=named))
            else:
                self.owner = have
                say(f"owner {have} kept from the stage")
        self.env[aws_target.OWNER_ENV] = self.owner
        self.build_env[aws_target.OWNER_ENV] = self.owner

    def check_azs(self) -> dict[str, str]:
        """AZ names map to IDs per account, so each stage AZ must be, in this account,
        the AZ ID accounts.json names (the AgentCore VPC IDs of its Region)."""
        names, ids = self.target.azs[self.stage], self.target.az_ids[self.stage]
        doc = self.aws("ec2", "describe-availability-zones", "--zone-names", *names) or {}
        got = {z.get("ZoneName"): z.get("ZoneId") for z in doc.get("AvailabilityZones", [])}
        bad = [(n, i) for n, i in zip(names, ids) if got.get(n) != i]
        if bad:
            raise Fail("azs", " ".join(f"{n}={got.get(n) or 'missing'}" for n, _ in bad)
                       + f" (accounts.json: {' '.join(i for _, i in bad)})")
        say("azs " + " ".join(f"{n}={i}" for n, i in zip(names, ids)))
        return dict(zip(names, ids))

    def check_vpc_cidr(self) -> dict:
        """The stage's VPC CIDR (infra/lib/config.ts) must not overlap any VPC of the account except the stage's own (tagged
        project and stage). The stages' CIDRs are distinct by construction; this catches an account that holds another project's VPC
        in the range, before a stack fails halfway with a peering or routing surprise."""
        want = ipaddress.ip_network(VPC_CIDRS[self.stage])
        clashes, previous = [], []
        for vpc in (self.aws("ec2", "describe-vpcs") or {}).get("Vpcs", []):
            tags = {t.get("Key"): t.get("Value") for t in vpc.get("Tags", [])}
            if tags.get("project") == PROJECT and tags.get("stage") == self.stage:
                continue
            blocks = [vpc.get("CidrBlock"), *(a.get("CidrBlock") for a in vpc.get("CidrBlockAssociationSet", []))]
            for cidr in dict.fromkeys(b for b in blocks if b):
                if ipaddress.ip_network(cidr, strict=False).overlaps(want):
                    clashes.append(f"{vpc.get('VpcId')}={cidr}")
                    # COMPAT-CAH-STAGE: the VPC of this stage under the previous names (previous project tag, this stage) is the old stage's, not a stranger's
                    if tags.get("project") == LEGACY_PROJECT and tags.get("stage") == self.stage and vpc.get("VpcId") not in previous:
                        previous.append(vpc.get("VpcId"))
        if previous:
            raise Fail("vpc-cidr", f"{want} overlaps {' '.join(clashes)}: this account still has the stage under the previous names ({' '.join(previous)} carries the "
                                   "tag of the previous project). This version does not deploy over it. `./setup.sh --teardown` removes it before you deploy again.")
        if clashes:
            raise Fail("vpc-cidr", f"{want} overlaps {' '.join(clashes)}")
        say(f"vpc cidr {want} overlaps no other VPC of the account")
        return {"cidr": str(want), "overlaps": []}

    def required_bootstrap(self) -> int:
        manifest = json.loads((self.root / "infra" / "cdk.out" / "manifest.json").read_text())
        found = [(a.get("properties") or {}).get("requiresBootstrapStackVersion") for a in manifest.get("artifacts", {}).values()]
        return max((v for v in found if isinstance(v, int)), default=MIN_BOOTSTRAP)

    def bootstrap_version(self) -> int | None:
        doc = self.aws("ssm", "get-parameter", "--name", BOOTSTRAP_PARAM, ok_missing=("ParameterNotFound",))
        return int(doc["Parameter"]["Value"]) if doc else None

    def ensure_bootstrap(self) -> dict:
        """An existing CDKToolkit is used unchanged when it meets the
        synthesized assembly's version and never modified; only an account and Region with none get one,
        with the project tags."""
        need, have = self.required_bootstrap(), self.bootstrap_version()
        if have is not None:
            if have < need:
                raise Fail("bootstrap", f"version {have} is below the required {need}; the existing CDKToolkit is never modified")
            say(f"bootstrap version {have} used unchanged (required {need})")
            return {"outcome": "existing", "version": have, "required": need}
        say(f"bootstrap: none in {self.account} {self.region}; creating it with the project tags")
        tags = [a for tag in bootstrap_tags(self.owner) for a in ("--tags", tag)]
        if self.cdk("bootstrap", f"aws://{self.account}/{self.region}", *tags, image=None) != 0:
            raise Fail("bootstrap", "cdk bootstrap failed")
        have = self.bootstrap_version()
        if have is None or have < need:
            raise Fail("bootstrap", f"version {have} after cdk bootstrap (required {need})")
        say(f"bootstrap version {have} created")
        return {"outcome": "created", "version": have, "required": need}

    def artifact_owner(self, uri: str):
        """The accounts entry whose stage bucket holds `uri`: another account's, or this account's retired stage
        (the lock's artifact may sit in a `cah-live-harness-<account>` or `cah-drill-harness-<account>` bucket of the project's
        history). This stage's own bucket has nothing to copy. The stage buckets of the previous names are the first alternative of the pattern
        and the current direct bucket the second: the current names never had a live or drill stage (COMPAT-CAH-STAGE)."""
        m = re.fullmatch(r"s3://((?:cah-(?:live|drill|direct)|hamh-direct)-harness-([0-9]{12}))/.+", uri or "")  # COMPAT-CAH-STAGE: the alternative of the previous names
        if not m or m.group(1) == self.bucket:
            return None
        return next((t for t in self.entries if t.account == m.group(2)), None)

    # ---- the Cline fork binary -----------------------------------------------

    def cline_series(self) -> dict:
        """The series pin of engine/cline/lock.toml: what a source build must show, and what a recorded fork is keyed by (the series tree and
        the first 12 hex characters of the SHA-256 of the [toolchain] entries as `key=value` lines (no quotes, no spaces), sorted by key and joined
        by newlines, so a new toolchain pin is a new key)."""
        try:
            lock = tomllib.loads((self.root / "engine" / "cline" / "lock.toml").read_text())
            tree, commit, tag, tc = lock["series"]["tree"], lock["upstream"]["commit"], lock["upstream"]["tag"], dict(lock["toolchain"])
        except (OSError, KeyError, tomllib.TOMLDecodeError) as e:
            raise Fail("fork-binary", f"engine/cline/lock.toml cannot be read: {e}") from None
        digest = hashlib.sha256("\n".join(f"{k}={v}" for k, v in sorted(tc.items())).encode()).hexdigest()[:12]
        return {"tree": tree, "commit": commit, "tag": tag, "toolchain": tc, "toolchainDigest": digest, "pointerKey": f"{FORK_POINTER_DIR}/{tree}-{digest}.json"}

    def s3_head(self, key: str) -> dict | None:
        return self.aws("s3api", "head-object", "--bucket", self.bucket, "--key", key, ok_missing=S3_MISSING)

    def s3_text(self, uri: str) -> str | None:
        """The text of a small object of the stage bucket, None when it is not there."""
        proc = self.run([self.aws_bin, "s3", "cp", uri, "-", "--region", self.region, "--only-show-errors"])
        if proc.returncode != 0:
            if any(m in proc.stderr for m in S3_MISSING):
                return None
            raise Fail("fork-binary", f"{uri}: {proc.stderr.strip()[-200:]}")
        return proc.stdout

    def s3_download(self, uri: str, dest: Path) -> None:
        proc = self.run([self.aws_bin, "s3", "cp", uri, str(dest), "--region", self.region, "--only-show-errors"])
        if proc.returncode != 0 or not dest.is_file():
            raise Fail("fork-binary", f"{uri}: {proc.stderr.strip()[-200:]}")

    def s3_put_once(self, key: str, body: Path, sha256: str | None = None) -> bool:
        """A create-only put into the stage bucket: True when created, False when the key exists (the 412)."""
        cmd = [self.aws_bin, "s3api", "put-object", "--bucket", self.bucket, "--key", key, "--body", str(body), "--if-none-match", "*"]
        if sha256:
            cmd += ["--metadata", f"sha256={sha256}"]
        proc = self.run([*cmd, "--region", self.region, "--output", "json"])
        if proc.returncode == 0:
            return True
        if any(m in proc.stderr for m in S3_EXISTS):
            return False
        raise Fail("fork-binary", f"s3://{self.bucket}/{key}: {proc.stderr.strip()[-200:]}")

    def profile_configured(self, profile: str) -> bool:
        """Whether the AWS CLI of this host has the profile (`aws configure list-profiles`: local, no network)."""
        proc = self.run([self.aws_bin, "configure", "list-profiles"])
        return proc.returncode == 0 and profile in proc.stdout.split()

    def fork_present(self, want: str, work: Path) -> dict | None:
        """The lock's object in the stage bucket, verified by its sha256 metadata or else by its bytes (a promotion without metadata)."""
        key = f"engine/cline/{want}/cline"
        dest = f"s3://{self.bucket}/{key}"
        head = self.s3_head(key)
        if head is None:
            return None
        got = (head.get("Metadata") or {}).get("sha256")
        how = "sha256 metadata"
        if got is None:
            # A promotion without metadata (ham-acct-b02's c07af1c8 object): its bytes decide.
            local = work / "present"
            self.s3_download(dest, local)
            got, how = hashlib.sha256(local.read_bytes()).hexdigest(), "sha256 of its bytes"
        if got != want:
            raise Fail("fork-binary", f"{dest} has sha256 {got[:12]}, the lock pins {want[:12]}")
        say(f"fork binary {want[:12]} present at {dest} ({how})")
        return {"outcome": "present", "uri": dest, "sha256": want, "verifiedBy": how}

    def fork_from_doc(self, doc: dict, series: dict, uri: str, lock_sha: str, pointer_outcome: str) -> dict:
        """The fork a pointer document names, after the document is held to this series tree and toolchain and the object is found with its sha256."""
        want = doc.get("sha256") if isinstance(doc, dict) else None
        if (not isinstance(want, str) or not HEX64.match(want) or doc.get("schema") != 1 or doc.get("seriesTree") != series["tree"]
                or doc.get("toolchainDigest") != series["toolchainDigest"] or doc.get("upstreamCommit") != series["commit"]):
            raise Fail("fork-binary", f"pointer {uri} does not describe this series tree and toolchain")
        key = f"engine/cline/{want}/cline"
        head = self.s3_head(key)
        if head is None or (head.get("Metadata") or {}).get("sha256") != want:
            raise Fail("fork-binary", f"pointer {uri} names a missing or other object (s3://{self.bucket}/{key})")
        return {"outcome": doc.get("outcome") or "built-from-source", "uri": f"s3://{self.bucket}/{key}", "sha256": want, "lockBinarySha256": lock_sha,
                "matchesLock": want == lock_sha, "seriesTree": series["tree"], "upstreamCommit": series["commit"], "toolchainDigest": series["toolchainDigest"],
                "codebuildBuildId": doc.get("codebuildBuildId"), "pointer": uri, "pointerOutcome": pointer_outcome,
                "verifiedBy": "pointer of this series tree and toolchain, sha256 metadata of the object"}

    def fork_from_pointer(self, series: dict, lock_sha: str) -> dict | None:
        uri = f"s3://{self.bucket}/{series['pointerKey']}"
        text = self.s3_text(uri)
        if text is None:
            return None
        try:
            doc = json.loads(text)
        except ValueError:
            raise Fail("fork-binary", f"pointer {uri} is not JSON") from None
        found = self.fork_from_doc(doc, series, uri, lock_sha, "read")
        say(f"fork binary {found['sha256'][:12]} recorded by an earlier source build of this stage (series tree {series['tree'][:12]}, toolchain {series['toolchainDigest']})")
        return found

    def fork_copy(self, lock: dict, want: str, owner, work: Path, hint: str) -> dict:
        """The lock's artifact, copied with the profile of the account that owns it and checked against the lock's SHA-256."""
        src = lock.get("artifact") or ""
        if not owner:
            raise Fail("fork-binary", f"no copy source in reach for s3://{self.bucket}/engine/cline/{want}/cline: the object is not there and the lock's artifact "
                                      f"({src or 'none'}) is held by no account of accounts.json; {hint}")
        local = work / "cline"
        proc = self.run([self.aws_bin, "s3", "cp", src, str(local), "--profile", owner.profile, "--region", owner.region, "--only-show-errors"])
        if proc.returncode != 0 or not local.is_file():
            raise Fail("fork-binary", f"{src} unreadable with profile {owner.profile}: {proc.stderr.strip()[-200:]}; {hint}")
        got = hashlib.sha256(local.read_bytes()).hexdigest()
        if got != want:
            raise Fail("fork-binary", f"{src} has sha256 {got[:12]}, the lock pins {want[:12]}")
        say(f"fork binary {want[:12]} copied from {src} (profile {owner.profile})")
        dest = f"s3://{self.bucket}/engine/cline/{want}/cline"
        up = self.run([self.aws_bin, "s3", "cp", str(local), dest, "--metadata", f"sha256={want}", "--region", self.region, "--only-show-errors"])
        if up.returncode != 0:
            raise Fail("fork-binary", up.stderr.strip()[-200:])
        return {"outcome": "copied", "source": src, "sourceProfile": owner.profile, "verifiedBy": "sha256 of the copied bytes", "uri": dest, "sha256": want}

    def check_fork_build(self, series: dict, build_id: str, listed_id: str, docs: dict, got: str) -> None:
        """The evidence the build itself computed (build-cline-fork.sh's metadata and record, the smoke test of the build spec) against the lock and
        the bytes downloaded. A log line is not read. Every difference is fatal and nothing is promoted."""
        meta, rec, smoke = docs["build-metadata.json"], docs["build-record.json"], docs["smoke-acp-initialize.json"]
        tc = series["toolchain"]
        sect = lambda d, k: d.get(k) if isinstance(d.get(k), dict) else {}  # noqa: E731
        binary, up, ser, tool, gate = sect(meta, "binary"), sect(meta, "upstream"), sect(meta, "series"), sect(meta, "toolchain"), sect(meta, "gate")
        mrec = sect(rec, "measurements")
        version = str(series["tag"]).removeprefix("cli-v")
        checks = [
            (listed_id == build_id, f"build id {listed_id or 'missing'} in the build output is not the build id this deploy started ({build_id}): stale or other output"),
            (meta.get("mode") == "build" and meta.get("target") == "linux-arm64", "build metadata is not a linux-arm64 build (mode, target)"),
            (up.get("commit") == series["commit"], f"build metadata upstream.commit {str(up.get('commit'))[:12]} is not the lock's {series['commit'][:12]}"),
            (ser.get("tree") == series["tree"], f"build metadata series.tree {str(ser.get('tree'))[:12]} is not the lock's {series['tree'][:12]}"),
            (tool.get("bun_zip_sha256") == tc.get("bun_linux_aarch64_zip_sha256"), "build metadata toolchain.bun_zip_sha256 is not the lock's bun_linux_aarch64_zip_sha256"),
            (tool.get("bun_version") == tc.get("bun_version") and tool.get("build_image") == tc.get("build_image"), "build metadata toolchain bun_version or build_image is not the lock's"),
            ("--frozen-lockfile" in str(tool.get("build_command")), "build metadata toolchain.build_command lacks --frozen-lockfile (frozen lockfile)"),
            (gate.get("ran") is True, "build metadata gate.ran is not true: the suites did not run (gate)"),
            (meta.get("telemetry") == "off", f"build metadata telemetry is {meta.get('telemetry')!r}, not 'off' (telemetry)"),
            (binary.get("elf_machine") == "aarch64", f"build metadata binary.elf_machine is {binary.get('elf_machine')!r}, not 'aarch64' (ELF machine)"),
            (binary.get("sha256") == got, f"build metadata binary.sha256 {str(binary.get('sha256'))[:12]} is not the SHA-256 of the downloaded bytes {got[:12]}"),
            (rec.get("result") == "pass" and mrec.get("binarySha256") == got and mrec.get("seriesTree") == series["tree"],
             "build record is not a pass for these bytes and this series tree"),
            (mrec.get("telemetry") == "off" and mrec.get("leaks") == 0, f"build record measurements.leaks is {mrec.get('leaks')!r} and telemetry {mrec.get('telemetry')!r}, not 0 and 'off' (leaks)"),
            (str(mrec.get("lastLine", "")).startswith("FORK-BUILD PASS "), "build record has no FORK-BUILD PASS line"),
            (((smoke.get("result") or {}).get("agentInfo") or {}).get("version") == version,
             f"smoke initialize of the built binary does not report version {version} (smoke)"),
        ]
        for ok, message in checks:
            if not ok:
                raise Fail("fork-binary", message)

    def fork_build(self, sha: str, series: dict, lock_sha: str, work: Path) -> dict:
        """Builds the fork from the pinned upstream source on CodeBuild ARM and accepts it on the evidence of the build (`check_fork_build`): builds
        are not bit-reproducible, so equality with the lock's SHA-256 is no test. The binary is promoted under its own SHA-256 and recorded by a
        pointer, both create-only."""
        say(f"fork binary: building {series['tag']} with its series from the pinned source on CodeBuild ARM ({FORK_SPEC})")
        rc, build_id = self.run_codebuild([self.codebuild, "--buildspec", FORK_SPEC, "--stage", self.stage, "--env", f"HARNESS_BUCKET={self.bucket}"])
        if rc != 0:
            raise Fail("fork-binary", f"codebuild-run {FORK_SPEC} exit {rc}")
        if not build_id:
            raise Fail("fork-binary", "codebuild-run printed no build id")
        prefix = f"s3://{self.bucket}/{FORK_OUT}/{sha}/"
        docs: dict = {}
        for name in ("build-metadata.json", "build-record.json", "smoke-acp-initialize.json"):
            text = self.s3_text(prefix + name)
            if text is None:
                raise Fail("fork-binary", f"{prefix}{name}: the build left no such file")
            try:
                docs[name] = json.loads(text)
            except ValueError:
                raise Fail("fork-binary", f"{prefix}{name} is not JSON") from None
        listed = (self.s3_text(prefix + "build-id.txt") or "").strip()
        local = work / "cline"
        self.s3_download(prefix + "cline", local)
        got = hashlib.sha256(local.read_bytes()).hexdigest()
        self.check_fork_build(series, build_id, listed, docs, got)
        key = f"engine/cline/{got}/cline"
        dest = f"s3://{self.bucket}/{key}"
        if not self.s3_put_once(key, local, got):
            head = self.s3_head(key)
            if head is None or (head.get("Metadata") or {}).get("sha256") != got:
                raise Fail("fork-binary", f"{dest} exists and is not this build ({got[:12]})")
        pointer_uri = f"s3://{self.bucket}/{series['pointerKey']}"
        body = {"schema": 1, "sha256": got, "seriesTree": series["tree"], "upstreamCommit": series["commit"], "toolchainDigest": series["toolchainDigest"],
                "outcome": "built-from-source", "codebuildBuildId": build_id, "sourceSha": sha}
        pfile = work / "pointer.json"
        pfile.write_text(json.dumps(body, sort_keys=True, indent=1) + "\n")
        if self.s3_put_once(series["pointerKey"], pfile):
            found = self.fork_from_doc(body, series, pointer_uri, lock_sha, "created")
            found["verifiedBy"] = "evidence of the build (metadata, record, smoke test, build id) and the SHA-256 of the downloaded bytes"
            say(f"fork binary {got[:12]} built from source (series tree {series['tree'][:12]}, toolchain {series['toolchainDigest']}); "
                f"the lock's {lock_sha[:12]} is kept as a reference and {'equals' if got == lock_sha else 'differs from'} it -> {dest}")
            return found
        text = self.s3_text(pointer_uri)
        try:
            doc = json.loads(text or "")
        except ValueError:
            raise Fail("fork-binary", f"pointer {pointer_uri} is not JSON") from None
        found = self.fork_from_doc(doc, series, pointer_uri, lock_sha, "lost-race" if doc.get("sha256") != got else "read")
        say(f"another deploy recorded the fork first; using {found['sha256'][:12]} (this build's {got[:12]} stays as an unreferenced object)")
        return found

    def ensure_fork(self, sha: str) -> dict:
        """The fork binary every worker image COPYs, settled once per deploy by `--fork-source` (module docstring, step 4).
        The result names the SHA-256 the image is built with (`sha256`); for a fork built from source that is the SHA-256 of that build, with the
        lock's kept as a reference."""
        if self.fork is not None:
            return self.fork
        lock = tomllib.loads((self.root / "engine" / "lock.toml").read_text())["cline"]
        want = lock["binarySha256"]
        mode = self.fork_source
        if mode == "build":
            # said before the pointer lookup: a rerun that finds the pointer of an earlier source build makes no build and must still show the choice
            say("fork-source build (the lock's object and the copy source are not used)")
        work = Path(tempfile.mkdtemp(prefix="deploy-fork-"))
        try:
            found = self.fork_present(want, work) if mode in ("auto", "copy") else None
            series = None
            if found is None and mode in ("auto", "build"):
                series = self.cline_series()
                found = self.fork_from_pointer(series, want)
            if found is None:
                owner = self.artifact_owner(lock.get("artifact") or "")
                hint = "rerun with --fork-source build to build from the pinned source"
                if mode == "build":
                    use_copy = False
                elif mode == "copy":
                    use_copy = True
                elif owner and self.profile_configured(owner.profile):
                    say(f"fork-source auto -> copy (profile {owner.profile} of the account that holds the lock's artifact is configured here)")
                    use_copy = True
                else:
                    why = (f"profile {owner.profile} of the artifact account is not configured here" if owner
                           else "no account of accounts.json holds the lock's artifact")
                    say(f"fork-source auto -> build ({why})")
                    use_copy = False
                found = self.fork_copy(lock, want, owner, work, hint) if use_copy else self.fork_build(sha, series or self.cline_series(), want, work)
        finally:
            # This deploy's own mkdtemp directory.
            shutil.rmtree(work, ignore_errors=True)
        self.fork = found
        return self.fork

    def build_lambda(self) -> None:
        packages = [a for name in (*LAMBDA_PACKAGES, DELIVERY_PACKAGE) for a in ("-p", name)]
        # The target directory goes by CARGO_TARGET_DIR (self.build_env), never --target-dir: cargo lambda 1.9.2 passes
        # its arguments on to `cargo metadata`, which rejects it.
        proc = self.run([*self.slot, *self.cargo, "lambda", "build", "--release", "--arm64", "--lambda-dir", str(self.lambda_dir),
                         *packages], env=self.build_env, stream=True)
        if proc.returncode != 0:
            raise Fail("lambda-build", f"cargo lambda exit {proc.returncode}")
        for name in (*LAMBDAS, *DELIVERY_BINS):
            boot = self.lambda_dir / name / "bootstrap"
            if not boot.is_file():
                raise Fail("lambda-build", f"{name} has no bootstrap")
            say(f"lambda {name} bootstrap sha256={hashlib.sha256(boot.read_bytes()).hexdigest()[:12]}")
        self.verify_build("lambda", {name: self.lambda_dir / name / "bootstrap" for name in (*LAMBDAS, *DELIVERY_BINS)})

    def verify_build(self, what: str, files: dict[str, Path]) -> None:
        """Before anything built is uploaded or handed to CDK: each file must have been built for this deploy's
        selection (`DEPLOY-FAIL build-selection <binary> built for <sel>, target is <sel>: ...`). The binaries that
        embed their selection are read; the harness-protocol build output under the target directory, where every
        binary's selection comes from, is checked for all (lib/build_selection.py)."""
        checked = []
        try:
            for name, path in files.items():
                if build_selection.check_binary(path, name, self.target, self.entries):
                    checked.append(name)
            outputs = build_selection.check_build_output(self.target_dir, self.target, self.entries)
        except build_selection.Mismatch as e:
            raise Fail("build-selection", f"{e.binary} {e.detail}") from None
        say(f"build selection ok {build_selection.label(self.target)}: {what} bytes={','.join(checked) or 'none'} "
            f"outputs={len(outputs)}")

    def install_infra(self) -> None:
        # A fresh checkout has no infra/node_modules; the lockfile pins every package.
        rc = self.run([self.node, "pnpm", "install", "--frozen-lockfile"], cwd=self.root / "infra", stream=True).returncode
        if rc != 0:
            raise Fail("infra-install", f"pnpm install exit {rc}")

    def stack_order(self) -> list[str]:
        if self.cdk("synth", "--quiet", image=self.current_image) != 0:
            raise Fail("synth", "cdk synth failed")
        manifest = json.loads((self.root / "infra" / "cdk.out" / "manifest.json").read_text())
        arts = manifest.get("artifacts", {})
        stacks = {n: a for n, a in arts.items() if a.get("type") == "aws:cloudformation:stack" and n.startswith(self.prefix + "-")}
        deps = {n: {d for d in a.get("dependencies", []) if d in stacks} for n, a in stacks.items()}
        self.stack_deps = {n: set(d) for n, d in deps.items()}
        rank = {f"{self.prefix}-{n}": i for i, n in enumerate(DEPLOY_PRIORITY)}
        order: list[str] = []
        while deps:
            ready = sorted((n for n, d in deps.items() if not d), key=lambda n: (rank.get(n, len(rank)), n))
            if not ready:
                raise Fail("synth", "stack dependency cycle")
            order.append(ready[0])
            deps.pop(ready[0])
            for d in deps.values():
                d.discard(ready[0])
        return order

    def deploy_stack(self, name: str, image: str | None) -> None:
        delivery = f" delivery={self.delivery_image.rsplit(':', 1)[-1][:12]}" if self.delivery_image else ""
        say(f"stack {name}{' image=' + image.rsplit(':', 1)[-1][:12] if image else ' (no worker image)'}{delivery}")
        if self.cdk("deploy", name, "--exclusively", "--require-approval", "never", "--progress", "events", image=image) != 0:
            raise Fail("stack", name)

    def collect_assets(self) -> None:
        out = self.root / "infra" / "cdk.out"
        for f in out.glob(f"{self.prefix}-*.assets.json") if out.is_dir() else []:
            doc = json.loads(f.read_text())
            for asset in doc.get("files", {}).values():
                for d in asset.get("destinations", {}).values():
                    self.assets["files"].add((d["bucketName"], d["objectKey"]))
            for asset in doc.get("dockerImages", {}).values():
                for d in asset.get("destinations", {}).values():
                    self.assets["images"].add((d["repositoryName"], d["imageTag"]))

    def outputs(self, stack: str) -> dict[str, str]:
        doc = self.aws("cloudformation", "describe-stacks", "--stack-name", stack, ok_missing=("does not exist",))
        if not doc:
            return {}
        return {o["OutputKey"]: o["OutputValue"] for o in doc["Stacks"][0].get("Outputs", [])}

    def live_versions(self) -> list[dict]:
        """Every runtime's live endpoint version, as GetAgentRuntime returns it, with the output name
        (`<runtime>RuntimeId`) that identifies it."""
        out = []
        for key, rid in sorted(self.outputs(f"{self.prefix}-runtimes").items()):
            m = re.fullmatch(r"(\w+)RuntimeId", key)
            if not m:
                continue
            ep = self.aws("bedrock-agentcore-control", "get-agent-runtime-endpoint", "--agent-runtime-id", rid,
                          "--endpoint-name", "live", ok_missing=("ResourceNotFoundException",))
            if not ep or not ep.get("liveVersion"):
                out.append({"name": m.group(1), "runtimeId": rid, "endpoint": None})
                continue
            rt = self.aws("bedrock-agentcore-control", "get-agent-runtime", "--agent-runtime-id", rid,
                          "--agent-runtime-version", ep["liveVersion"]) or {}
            out.append({"name": m.group(1), "runtimeId": rid, "endpoint": ep, "runtime": rt})
        return out

    @property
    def current_runtimes(self) -> dict[str, dict]:
        """Each runtime's live version as GetAgentRuntime returns it, by runtime output name, before this deploy (a runtime with no live endpoint is absent)."""
        if not hasattr(self, "_current_runtimes"):
            self._current_runtimes = {v["name"]: v["runtime"] for v in self.live_versions() if v.get("endpoint")}
        return self._current_runtimes

    @property
    def current_images(self) -> dict[str, str]:
        """The image each runtime's live endpoint serves, by runtime output name, before this deploy."""
        out = {}
        for name, rt in self.current_runtimes.items():
            try:
                out[name] = rt["agentRuntimeArtifact"]["containerConfiguration"]["containerUri"]
            except (KeyError, TypeError):
                # The live read now happens before the Lambda build: an answer without an image ends the deploy by name, never in a traceback.
                raise Fail("live-runtime", f"{name}: GetAgentRuntime of the live version names no container image") from None
        return out

    @property
    def current_image(self) -> str | None:
        return self.current_images.get(WORKER_IMAGE.runtime)

    @property
    def current_tools_mode(self) -> str | None:
        """AGENTD_TOOLS_MODE of the worker runtime version the live endpoint serves (the version `current_image` reads); None when that runtime has
        no such variable, or when there is no runtime yet."""
        env = (self.current_runtimes.get(WORKER_IMAGE.runtime) or {}).get("environmentVariables") or {}
        return env.get(TOOLS_MODE_VAR)

    def tools_mode_state(self) -> dict:
        """The tool layer's stage setting against the live worker runtime, before anything is built. `change`: the mode differs from the live
        runtime's (a runtime without the variable differs), so this deploy makes a new runtime version. The deploy record keeps it as `inputs.toolsMode`."""
        live = self.current_image is not None
        previous = self.current_tools_mode
        return {"setting": self.tools_setting, "mode": self.tools_mode, "previous": previous, "liveRuntime": live,
                "change": live and previous != self.tools_mode,
                "stageKind": "official" if self.target.role == "active" else "test"}

    def say_tools_mode(self, st: dict) -> None:
        how = f"HAMH_TOOLS_MODE={st['setting']}" if st["setting"] else "HAMH_TOOLS_MODE not set, the default"
        if not st["liveRuntime"]:
            after = "the stage has no live runtime yet"
        else:
            had = f"{TOOLS_MODE_VAR}={st['previous']}" if st["previous"] else f"no {TOOLS_MODE_VAR}"
            after = f"the live runtime has {had}"
            if st["change"]:
                after += ", so this deploy CHANGES it and makes a new runtime version"
                if st["setting"] is None and st["previous"] == "on":
                    # A deploy that only forgot the setting says what it does to a stage that runs the layer.
                    after += "; HAMH_TOOLS_MODE is not set, so the layer goes from on to off (export HAMH_TOOLS_MODE=on to keep it)"
        say(f"tools mode {st['mode']} ({how}); {after}")
        if st["mode"] == "on" and st["stageKind"] == "official":
            say("tools mode is on at this stage; keep it on only when the sample projects pass with the layer on")

    def compute_instances_state(self) -> dict:
        """The Instances compute type's stage setting against the live stage, before anything is built. `live`: the runtimes stack already holds the capacity
        provider. `change`: the setting differs from the live stage, so this deploy adds or removes the provider and the second runtime. The deploy record
        keeps it as `inputs.computeInstances`."""
        outputs = self.outputs(f"{self.prefix}-runtimes")
        # The provider or the second runtime: a half-removed part still counts, and a deploy does not finish the removal either.
        live = INSTANCES_PROVIDER_OUTPUT in outputs or f"{INSTANCES_RUNTIME}RuntimeId" in outputs
        return {"setting": self.instances_setting, "mode": "on" if self.compute_instances else "off", "live": live,
                "change": live != self.compute_instances}

    def say_compute_instances(self, st: dict) -> None:
        how = f"HAMH_COMPUTE_INSTANCES={st['setting']}" if st["setting"] else "HAMH_COMPUTE_INSTANCES not set, the default"
        after = "the stage has the Instances part" if st["live"] else "the stage has no Instances capacity provider"
        if st["change"]:
            if st["mode"] == "on":
                after += ", so this deploy CREATES the capacity provider, its operator role and the second worker runtime"
            else:
                after += ", which a deploy does not remove"
        say(f"compute instances {st['mode']} ({how}); {after}")

    def isolation_state(self) -> dict:
        """The isolation words (RUNTIME_WORDS) against the live worker runtimes, before anything is built. `differs`: per runtime, each word the live version lacks (None)
        or carries another way, with the word it wants. A runtime with no live version has nothing to differ from. `change`: any differs, so this deploy makes a new
        runtime version (the environment is part of it) and meets the deploy guard. The deploy record keeps it as `inputs.isolationWords`."""
        differs: dict[str, dict] = {}
        for name, words in RUNTIME_WORDS.items():
            live = self.current_runtimes.get(name)
            if live is None:
                continue
            env = live.get("environmentVariables") or {}
            bad = {var: {"live": env.get(var), "want": want} for var, want in words.items() if env.get(var) != want}
            if bad:
                differs[name] = bad
        return {"change": bool(differs), "differs": differs}

    def say_isolation(self, st: dict) -> None:
        shown = lambda v: "''" if v == "" else str(v)  # noqa: E731
        for name, bad in sorted(st["differs"].items()):
            parts = [(f"no {var}" if d["live"] is None else f"{var}={shown(d['live'])}") + f" (wants {shown(d['want'])})" for var, d in sorted(bad.items())]
            say(f"isolation words: the live {name} runtime has {', '.join(parts)}, so this deploy changes its environment and makes a new runtime version")

    def provider_replacement(self) -> tuple[str, str] | None:
        """(live name, synthesized name) of the Instances capacity provider when they differ, else None. The provider's name ends in a hash of its configuration
        (infra/lib/config.ts `instancesProviderShape`), and AgentCore edits nothing of a provider but its description, so a different name is a replacement:
        CloudFormation creates the new provider, moves the runtime to it (a new runtime version) and deletes the old one with the machines on it. The synthesized
        name is read from the template of the runtimes stack that `stack_order` wrote, so this comes after the synth. A stage without the provider, or a synth without it, has nothing to compare."""
        m = PROVIDER_ARN_NAME.search(self.outputs(f"{self.prefix}-runtimes").get(INSTANCES_PROVIDER_OUTPUT, ""))
        manifest = json.loads((self.root / "infra" / "cdk.out" / "manifest.json").read_text())
        template_file = (manifest.get("artifacts", {}).get(f"{self.prefix}-runtimes", {}).get("properties") or {}).get("templateFile")
        if not m or not template_file:
            return None
        template = json.loads((self.root / "infra" / "cdk.out" / template_file).read_text())
        names = [r.get("Properties", {}).get("Name") for r in (template.get("Resources") or {}).values() if r.get("Type") == PROVIDER_TYPE]
        return (m.group(1), names[0]) if names and names[0] and names[0] != m.group(1) else None

    def refuse_instances_off(self, st: dict) -> None:
        """A stage that has the Instances part and a setting that resolves to off: removing the part ends every machine and session on the provider, and CloudFormation
        cannot finish it (the security stack deletes the operator role first, then the runtimes update fails). The removal is its own step, so the deploy stops here."""
        if not (st["live"] and not self.compute_instances):
            return
        word = "is not set" if st["setting"] is None else f"is {st['setting']}"
        raise Fail("instances-off", f"the stage has the Instances part (the capacity provider and the second worker runtime) and HAMH_COMPUTE_INSTANCES {word}, which is off. "
                                    "Removing the part ends every machine and session on the provider, so a deploy does not do it; removal is a separate, explicit step, "
                                    'and for now that step is the stage teardown. '
                                    "To deploy this stage as it is, export HAMH_COMPUTE_INSTANCES=on.")

    def find_image(self, tag: str, repo: str | None = None) -> str | None:
        repo = repo or self.repo_name
        doc = self.aws("ecr", "describe-images", "--repository-name", repo, "--image-ids", f"imageTag={tag}",
                       ok_missing=("ImageNotFoundException", "RepositoryNotFoundException"))
        if not doc or not doc.get("imageDetails"):
            return None
        return f"{self.account}.dkr.ecr.{self.region}.amazonaws.com/{repo}@{doc['imageDetails'][0]['imageDigest']}"

    def repo_exists(self, repo: str | None = None) -> bool:
        return self.aws("ecr", "describe-repositories", "--repository-names", repo or self.repo_name,
                        ok_missing=("RepositoryNotFoundException",)) is not None

    def image_tag(self, spec: BinaryImage, metadata: dict) -> str:
        """`<prefix>-<key>`: git tree hashes of the binaries' crate closures and the spec's paths."""
        crates = sorted({d for name, _ in spec.binaries for d in digests.local_crate_closure(metadata, name)})
        # Paths an image never reads (digests.IMAGE_EXCLUDED: engine/catalog, engine/policy) stay out
        # of its key, as in digests.image_key.
        paths = digests._without(self.root, spec.paths, digests.IMAGE_EXCLUDED)
        return f"{spec.tag_prefix}-" + digests._hash_paths(self.root, [*crates, *paths])

    def build_binary_image(self, spec: BinaryImage, sha: str, tag: str, extra: dict[str, str] | None = None) -> str:
        """Cross-builds the spec's binaries in one zigbuild, uploads each to builds/out/<binary>/<sha>/, and
        builds hamh-<stage>-<repo>:<tag> on CodeBuild ARM with their SHA-256s."""
        build, upload, image_step = spec.steps
        if extra and "CLINE_SHA256" in extra:
            # The image is built with the SHA-256 of the fork that was settled: the lock's for a present or copied one, the build's own for a source build.
            extra = {**extra, "CLINE_SHA256": self.ensure_fork(sha)["sha256"]}
        repo = f"{self.prefix}-{spec.repo}"
        envs = {"HARNESS_BUCKET": self.bucket, "REPO_URI": f"{self.account}.dkr.ecr.{self.region}.amazonaws.com/{repo}",
                **(extra or {}), "IMAGE_TAG": tag}
        names = tuple(name for name, _ in spec.binaries)
        if names in self.built_sets:
            # Another image of this deploy cross-built and uploaded these binaries already.
            say(f"{spec.name}: reusing {', '.join(names)} from this deploy's cross-build")
        else:
            # The recipe module owns the argv, the remap of CARGO_HOME and the variables that change the compiler's
            # output, so the fake-acp CI step builds the binary this deploy ships the same way.
            cross_env = agentd_build.build_env(self.build_env, target_dir=self.target_dir, profile=self.profile)
            proc = self.run([*self.slot, *self.cargo, *agentd_build.zigbuild_args(names)], env=cross_env, stream=True)
            release = self.target_dir / AGENTD_TARGET / "release"
            missing = [name for name in names if not (release / name).is_file()]
            if proc.returncode != 0 or missing:
                raise Fail(build, f"cargo zigbuild exit {proc.returncode} missing {','.join(missing) or 'none'}")
            self.verify_build(spec.name, {name: release / name for name in names})
            for name in names:
                binary = release / name
                digest = hashlib.sha256(binary.read_bytes()).hexdigest()
                say(f"{name} sha256={digest[:12]}")
                up = self.run([self.aws_bin, "s3", "cp", str(binary), f"s3://{self.bucket}/builds/out/{name}/{sha}/{name}",
                               "--region", self.region, "--only-show-errors"])
                if up.returncode != 0:
                    raise Fail(upload, up.stderr.strip()[-200:])
                self.uploaded[name] = digest
            self.built_sets.add(names)
        for name, env in spec.binaries:
            envs[env] = self.uploaded[name]
        cmd = [self.codebuild, "--buildspec", spec.buildspec, "--stage", self.stage]
        for k, v in envs.items():
            cmd += ["--env", f"{k}={v}"]
        rc, build_id = self.run_codebuild(cmd)
        if rc != 0:
            raise Fail(image_step, f"codebuild-run exit {rc}")
        if not build_id:
            raise Fail(image_step, "codebuild-run printed no build id")
        image = self.find_image(tag, repo)
        if not image:
            raise Fail(image_step, f"no image tagged {tag} after the build")
        self.write_image_record(spec.name, spec.buildspec, repo, tag, image, envs, build_id=build_id,
                                recipe=agentd_build.recipe_digest(self.root, self.profile))
        return image

    def cline_sha256(self) -> str:
        return tomllib.loads((self.root / "engine" / "lock.toml").read_text())["cline"]["binarySha256"]

    def build_image(self, sha: str, tag: str) -> str:
        # The stage's one worker runs any language a prompt names, so its image also carries Go, the JDK and Maven.
        extra = {"CLINE_SHA256": self.cline_sha256(), "LANG_FAMILY": "direct"}
        return self.build_binary_image(WORKER_IMAGE, sha, tag, extra)

    def build_delivery_image(self, sha: str, tag: str) -> str:
        """Uploads the three delivery bootstraps and builds hamh-<stage>-delivery:<tag> on CodeBuild ARM."""
        envs = {"HARNESS_BUCKET": self.bucket, "REPO_URI": self.delivery_uri, "IMAGE_TAG": tag}
        for b in DELIVERY_BINS:
            boot = self.lambda_dir / b / "bootstrap"
            envs[f"{b.upper().replace('-', '_')}_SHA256"] = hashlib.sha256(boot.read_bytes()).hexdigest()
            up = self.run([self.aws_bin, "s3", "cp", str(boot), f"s3://{self.bucket}/builds/out/delivery/{sha}/{b}",
                           "--region", self.region, "--only-show-errors"])
            if up.returncode != 0:
                raise Fail("delivery-upload", up.stderr.strip()[-200:])
        cmd = [self.codebuild, "--buildspec", "images/codebuild/delivery.yml", "--stage", self.stage]
        for k, v in envs.items():
            cmd += ["--env", f"{k}={v}"]
        rc, build_id = self.run_codebuild(cmd)
        if rc != 0:
            raise Fail("delivery-image-build", f"codebuild-run exit {rc}")
        if not build_id:
            raise Fail("delivery-image-build", "codebuild-run printed no build id")
        image = self.find_image(tag, self.delivery_repo)
        if not image:
            raise Fail("delivery-image-build", f"no image tagged {tag} after the build")
        self.write_image_record("delivery", "images/codebuild/delivery.yml", self.delivery_repo, tag, image, envs, build_id=build_id)
        return image

    def resolve_delivery_image(self, sha: str, tag: str, order: list[str], worker_image: str | None) -> str:
        found = self.find_image(tag, self.delivery_repo) if self.repo_exists(self.delivery_repo) else None
        if found:
            say(f"delivery image {tag[:15]} found {found.rsplit(':', 1)[-1][:12]}")
            return found
        stack = f"{self.prefix}-delivery"
        if stack in order and not self.repo_exists(self.delivery_repo):
            say("delivery phase-1: the stage has no delivery repository yet")
            self.deploy_stack(stack, worker_image)
        say(f"delivery image {tag[:15]} missing: building")
        return self.build_delivery_image(sha, tag)

    def expected_images(self, image: str | None) -> dict[str, str | None]:
        """The runtimes this deploy must leave serving `image`: the microVM worker runtime, and with HAMH_COMPUTE_INSTANCES=on the Instances one."""
        out: dict[str, str | None] = {WORKER_IMAGE.runtime: image}
        if self.compute_instances:
            out[INSTANCES_RUNTIME] = image
        return out

    def check_runtimes(self, images: dict[str, str | None]) -> list[dict]:
        """Every runtime's live version against its own deployed image; `images` maps each runtime
        output name (`WorkerPython`) to the image it must serve."""
        checks, bad = [], []
        versions = self.live_versions()
        seen = {v["name"] for v in versions}
        bad += [f"{name}:no-runtime" for name, image in sorted(images.items()) if image and name not in seen]
        for v in versions:
            rid = v["runtimeId"]
            if v["name"] not in images:
                bad.append(f"{rid}:no-deployed-image")
                continue
            if not v.get("endpoint"):
                bad.append(f"{rid}:no-live-endpoint")
                continue
            rt, ep = v["runtime"], v["endpoint"]
            on_instances = v["name"] == INSTANCES_RUNTIME
            c = {"runtime": v["name"], "runtimeId": rid, "liveVersion": ep["liveVersion"], "endpointStatus": ep.get("status"),
                 "status": rt.get("status"), "networkMode": rt.get("networkConfiguration", {}).get("networkMode"),
                 "platformVersion": rt.get("platformVersion"),
                 "requireMMDSV2": rt.get("metadataConfiguration", {}).get("requireMMDSV2"),
                 "image": rt.get("agentRuntimeArtifact", {}).get("containerConfiguration", {}).get("containerUri"),
                 # Where the runtime runs and which isolation word its environment carries (agentd refuses to start the engine without the right one).
                 "capacityProvider": (rt.get("capacityProviderConfiguration") or {}).get("capacityProviderArn"),
                 "sandbox": (rt.get("environmentVariables") or {}).get(SANDBOX_VAR),
                 "engineUser": (rt.get("environmentVariables") or {}).get(ENGINE_USER_VAR)}
            checks.append(c)
            # A microVM runtime is in VPC mode with a network of its own; an Instances runtime has no network configuration (its network is the provider's) and names the provider.
            want = {"endpointStatus": "READY", "status": "READY", "networkMode": None if on_instances else "VPC", "platformVersion": "V1",
                    "requireMMDSV2": True, "image": images[v["name"]], "sandbox": SANDBOX_WORDS.get(v["name"])}
            if ENGINE_USER_VAR in RUNTIME_WORDS.get(v["name"], {}):
                want["engineUser"] = RUNTIME_WORDS[v["name"]][ENGINE_USER_VAR]
            bad += [f"{rid}:{k}={c[k]}" for k, w in want.items() if c[k] != w]
            if on_instances and not c["capacityProvider"]:
                bad.append(f"{rid}:capacityProvider=None")
            if not on_instances and c["capacityProvider"]:
                bad.append(f"{rid}:capacityProvider={c['capacityProvider']}")
        if not checks and not bad:
            bad.append("no-runtimes")
        if bad:
            raise Fail("runtime-flags", " ".join(bad))
        for c in checks:
            where = "capacity-provider" if c["capacityProvider"] else "vpc"
            user = f" {ENGINE_USER_VAR}={c['engineUser']!r}" if ENGINE_USER_VAR in RUNTIME_WORDS.get(c["runtime"], {}) else ""
            say(f"runtime {c['runtimeId']} live={c['liveVersion']} {where}=ok platform=V1 requireMMDSV2=true {SANDBOX_VAR}={c['sandbox']}{user}")
        return checks

    def endpoint_state(self, rid: str, name: str) -> dict | None:
        return self.aws("bedrock-agentcore-control", "get-agent-runtime-endpoint", "--agent-runtime-id", rid,
                        "--endpoint-name", name, ok_missing=("ResourceNotFoundException",))

    def wait_endpoint(self, rid: str, name: str, version: str) -> None:
        """Until `name` is READY and serves `version`; a FAILED status or the timeout fails the deploy."""
        poll = float(os.environ.get("DEPLOY_ENDPOINT_POLL_S", "5"))
        deadline = time.monotonic() + float(os.environ.get("DEPLOY_ENDPOINT_TIMEOUT_S", "600"))
        while True:
            ep = self.endpoint_state(rid, name) or {}
            status = ep.get("status") or "absent"
            if status == "READY":
                if ep.get("liveVersion") != version:
                    raise Fail("version-endpoint", f"{rid} {name} serves {ep.get('liveVersion')}")
                return
            if status.endswith("FAILED") or status == "absent" or time.monotonic() >= deadline:
                raise Fail("version-endpoint", f"{rid} {name} {status}")
            time.sleep(poll)

    def pinned_endpoints(self) -> set[str]:
        """The endpoints the RUNNING runs of hamh-<stage>-run are pinned to on the worker runtime (their Run items' runtimeEndpoint)."""
        pins = set()
        for run in self.running_runs():
            key = json.dumps({"PK": {"S": f"RUN#{run}"}, "SK": {"S": "META"}})
            doc = self.aws("dynamodb", "get-item", "--table-name", f"{self.prefix}-harness", "--key", key,
                           "--consistent-read", "--projection-expression", "runtimeEndpoint") or {}
            pinned = doc.get("Item", {}).get("runtimeEndpoint", {}).get("S")
            if pinned:
                pins.add(pinned)
        return pins

    def version_endpoints(self, checks: list[dict]) -> list[dict]:
        """Step 6a: `v<N>` on the worker runtime for the version `live` serves; older `v<M>` pruned."""
        out = []
        for c in (c for c in checks if c["runtime"] in (*WORKER_RUNTIMES, INSTANCES_RUNTIME)):
            rid, n = c["runtimeId"], c["liveVersion"]
            name = f"v{n}"
            listed = self.aws("bedrock-agentcore-control", "list-agent-runtime-endpoints", "--agent-runtime-id", rid) or {}
            names = [e["name"] for e in listed.get("runtimeEndpoints", [])]
            created = name not in names
            if created:
                tags = project_tags(self.owner, self.stage)
                self.aws("bedrock-agentcore-control", "create-agent-runtime-endpoint", "--agent-runtime-id", rid,
                         "--name", name, "--agent-runtime-version", n, "--description",
                         f"{self.prefix} worker version {n}: runs pinned to it stay on it", "--tags", json.dumps(tags))
            self.wait_endpoint(rid, name, n)
            say(f"endpoint {name} on {rid} serves version {n} ({'created' if created else 'present'})")
            others = sorted(int(m.group(1)) for e in names if e != name and (m := VERSION_ENDPOINT.fullmatch(e)))
            below = [v for v in others if v < int(n)]
            keep = [name] + ([f"v{max(below)}"] if below else [])
            candidates = [f"v{v}" for v in others if f"v{v}" not in keep]
            pinned = self.pinned_endpoints() if candidates else set()
            kept = keep + sorted((e for e in candidates if e in pinned), key=lambda e: -int(e[1:]))
            deleted = [e for e in candidates if e not in pinned]
            for e in deleted:
                self.aws("bedrock-agentcore-control", "delete-agent-runtime-endpoint", "--agent-runtime-id", rid,
                         "--endpoint-name", e)
                say(f"endpoint {e} deleted (no running run pins it)")
            if kept[1:]:
                say("endpoints kept " + " ".join(kept))
            out.append({"runtime": c["runtime"], "runtimeId": rid, "endpoint": name, "version": n, "created": created,
                        "kept": kept, "deleted": deleted})
        return out

    def template_hashes(self) -> dict[str, str | None]:
        out = {}
        for n in digests.STACK_NAMES:
            doc = self.aws("cloudformation", "get-template", "--stack-name", f"{self.prefix}-{n}", "--template-stage", "Original",
                           ok_missing=("does not exist",))
            out[n] = digests.template_hash(doc["TemplateBody"]) if doc else None
        return out

    def asl_versions(self) -> dict[str, str]:
        return {k: v for k, v in self.outputs(f"{self.prefix}-orchestration").items() if k.endswith("StateMachineVersionArn")}

    def report_gomod_store(self) -> dict:
        """Say whether the stage's Go module store holds anything (see step 7a of the module docstring). A report, never a failure."""
        bucket = f"{self.prefix}-gomod-{self.account}"
        check = gomod_store.check_command(self.stage)
        try:
            doc = self.aws("s3api", "list-objects-v2", "--bucket", bucket, "--expected-bucket-owner", self.account, "--max-items", "1")
        except Fail as exc:
            print(f"GOMOD-STORE UNKNOWN stage={self.stage} bucket={bucket}: the store could not be read ({exc}); look at it with: {check}", flush=True)
            return {"bucket": bucket, "state": "unknown"}
        if (doc or {}).get("Contents"):
            print(f"GOMOD-STORE OK stage={self.stage} bucket={bucket} holds objects; audit it with: {check}", flush=True)
            return {"bucket": bucket, "state": "filled"}
        print(f"GOMOD-STORE EMPTY stage={self.stage} bucket={bucket}: The deploy leaves this S3 bucket for Go modules (libraries) empty. "
              f"While it is empty, every Go module download in a run fails with HTTP 404; runs in other languages do not use it. "
              f"On a machine with Go, you fill it once with {gomod_store.fill_command(self.stage)} and check it with {check}", flush=True)
        return {"bucket": bucket, "state": "empty"}

    def invocation_logging(self) -> dict:
        """Home-Region model invocation logging is enabled only for
        an `enable-and-restore` entry (acct-a01), only when it is unset, and only after the prior state is
        written to SSM /hamh/<stage>/invocation-logging-prior (never overwritten; teardown.sh restores it).
        A `read-only` entry (ham-acct-b02) is read and recorded, never written; a destination that is set is
        read and never overwritten. The outcome goes into the deploy record."""
        group = f"/hamh/{self.stage}/bedrock-invocations"
        cur = (self.aws("bedrock", "get-model-invocation-logging-configuration") or {}).get("loggingConfig")
        if cur:
            existing = (cur.get("cloudWatchConfig") or {}).get("logGroupName") or (cur.get("s3Config") or {}).get("bucketName")
            if existing == group:
                say(f"invocation logging already ours: {group}")
                return {"outcome": "ours", "logGroup": group}
            say(f"invocation logging points at {existing}; not overwritten")
            return {"outcome": "foreign", "logGroup": existing}
        if self.target.invocation_logging == "read-only":
            say(f"invocation logging unset; {self.profile} is read-only (never written)")
            return {"outcome": "unset-read-only", "logGroup": None}
        prior = PRIOR_LOGGING_PARAM.format(stage=self.stage)
        if self.aws("ssm", "get-parameter", "--name", prior, ok_missing=("ParameterNotFound",)):
            say(f"invocation logging prior state already recorded at {prior}")
        else:
            self.put_param(prior, json.dumps({"loggingConfig": None}), overwrite=False)
            say(f"invocation logging prior state (unset) recorded at {prior}")
        config = {
            "cloudWatchConfig": {"logGroupName": group, "roleArn": f"arn:aws:iam::{self.account}:role/{self.prefix}-bedrock-logging"},
            "textDataDeliveryEnabled": True, "imageDataDeliveryEnabled": False,
            "embeddingDataDeliveryEnabled": False, "videoDataDeliveryEnabled": False,
        }
        self.aws("bedrock", "put-model-invocation-logging-configuration", "--logging-config", json.dumps(config))
        got = (self.aws("bedrock", "get-model-invocation-logging-configuration") or {}).get("loggingConfig") or {}
        if (got.get("cloudWatchConfig") or {}).get("logGroupName") != group:
            raise Fail("invocation-logging", "the configuration did not take")
        say(f"invocation logging enabled: {group}")
        return {"outcome": "enabled", "logGroup": group, "prior": None}

    def running_runs(self) -> list[str]:
        """Names of the RUNNING executions of hamh-<stage>-run (one per run, named by the run id)."""
        arn = f"arn:aws:states:{self.region}:{self.account}:stateMachine:{self.prefix}-run"
        doc = self.aws("stepfunctions", "list-executions", "--state-machine-arn", arn, "--status-filter", "RUNNING",
                       ok_missing=("StateMachineDoesNotExist",))
        return sorted(e["name"] for e in (doc or {}).get("executions", []))

    def guard(self) -> None:
        """Every run pins the worker runtime version it started on; a new version waits."""
        runs = self.running_runs()
        if runs:
            shown = " ".join(runs[:20]) + (f" (+{len(runs) - 20} more)" if len(runs) > 20 else "")
            raise Fail("guard", f"{len(runs)} running run(s) on {self.prefix}-run pin the worker runtime: {shown}")

    def report_config_record(self) -> None:
        """Says when the account file in use is not the one the stage's earlier deploy recorded (SSM /hamh/<stage>/deployment-config-digest,
        `committed` or a SHA-256). A report, never a failure: a changed file can be intended, and nothing below depends on the earlier value."""
        name = f"/hamh/{self.stage}/deployment-config-digest"
        doc = self.aws("ssm", "get-parameter", "--name", name, ok_missing=("ParameterNotFound",))
        earlier = ((doc or {}).get("Parameter") or {}).get("Value") or None
        where = "the committed accounts.json" if self.config_digest == "committed" else f"HAMH_ACCOUNTS_FILE={aws_target.env_get(aws_target.FILE_HOOK)}"
        say(f"deployment config {self.config_digest[:12]} ({where})")
        if earlier and earlier != self.config_digest:
            say(f"deployment config {self.config_digest[:12]} differs from the earlier record {earlier[:12]} of stage {self.stage}: "
                f"the account file in use is not the one the last deploy recorded in {name}")

    def check_guard(self) -> str:
        self.check_account()
        self.guard()
        return f"DEPLOY-GUARD PASS stage={self.stage} running=0"

    def put_param(self, name: str, value: str, overwrite: bool = True) -> None:
        self.aws("ssm", "put-parameter", "--name", name, "--value", value, "--type", "String",
                 *(["--overwrite"] if overwrite else []))
        self.aws("ssm", "add-tags-to-resource", "--resource-type", "Parameter", "--resource-id", name, "--tags",
                 f"Key=project,Value={PROJECT}", f"Key=stage,Value={self.stage}",
                 f"Key=owner,Value={self.owner}", "Key=managed-by,Value=deploy.sh")

    def record_assets(self) -> None:
        key = f"s3://{self.bucket}/deploy/{self.stage}/cdk-assets.json"
        prev = self.run([self.aws_bin, "s3", "cp", key, "-", "--region", self.region])
        files, images = set(self.assets["files"]), set(self.assets["images"])
        if prev.returncode == 0 and prev.stdout.strip():
            old = json.loads(prev.stdout)
            files |= {(f["bucket"], f["key"]) for f in old.get("files", [])}
            images |= {(i["repository"], i["tag"]) for i in old.get("images", [])}
        doc = {"files": [{"bucket": b, "key": k} for b, k in sorted(files)],
               "images": [{"repository": r, "tag": t} for r, t in sorted(images)]}
        with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as f:
            json.dump(doc, f, indent=1)
        up = self.run([self.aws_bin, "s3", "cp", f.name, key, "--region", self.region, "--only-show-errors"])
        os.unlink(f.name)
        if up.returncode != 0:
            raise Fail("assets-record", up.stderr.strip()[-200:])
        say(f"recorded {len(doc['files'])} asset objects and {len(doc['images'])} asset images at {key}")

    def write_image_record(self, name: str, buildspec: str, repo: str, tag: str, image: str, envs: dict[str, str], *, build_id: str,
                           recipe: str | None = None) -> None:
        """The record of one image this deploy built on CodeBuild ARM. `build_id` is the id of the CodeBuild
        build (scripts/codebuild-run.sh prints it); R21 reads the build back to see it ran on an ARM environment. `recipe` is the
        digest of the binaries' build recipe (scripts/lib/agentd_build.py); a delivery image has none."""
        doc = self.aws("ecr", "describe-images", "--repository-name", repo, "--image-ids", f"imageTag={tag}",
                       ok_missing=("ImageNotFoundException",))
        size = ((doc or {}).get("imageDetails") or [{}])[0].get("imageSizeInBytes")
        crit = Path(os.environ.get("DEPLOY_CRITERIA", ""))
        now = dt.datetime.now(dt.timezone.utc)
        rec = {
            "schemaVersion": 1, "kind": "image-build", "createdAt": now.strftime("%Y-%m-%dT%H:%M:%SZ"),
            "host": socket.getfqdn(), "account": self.account, "region": self.region, "stage": self.stage,
            "codeSha": self.git("rev-parse", "HEAD"), "sourceTree": digests.source_tree(self.root),
            "deploymentConfigDigest": self.config_digest,
            # Known once every stack is up; the deploy record that follows names it.
            "deploymentDigest": None, "imageDigest": image.split("@", 1)[1],
            "criteriaSha256": hashlib.sha256(crit.read_bytes()).hexdigest() if crit.is_file() else None,
            "result": "pass",
            "inputs": {"image": name, "buildspec": buildspec, "repository": repo, "imageTag": tag,
                       "binaries": {k: v for k, v in sorted(envs.items()) if k.endswith("_SHA256")},
                       **({"recipeDigest": recipe} if recipe else {}),
                       **({"langFamily": envs["LANG_FAMILY"]} if "LANG_FAMILY" in envs else {})},
            "measurements": {"buildHost": "codebuild", "codebuildBuildId": build_id, "image": image, "sizeBytes": size},
            "runIds": [], "notes": "built by scripts/codebuild-run.sh on the stage's CodeBuild ARM project",
        }
        if "CLINE_SHA256" in envs:
            # R24: "the value every worker image asserted at image build". The build COPYs the promoted fork after
            # `sha256sum -c` of build/cline against CLINE_SHA256, and ensure_fork verified that value, the lock's binarySha256, against
            # the S3 object the build downloads. The images that carry no fork (verifier, evaluator, delivery) state none.
            fork = self.fork or {}
            if fork.get("sha256") != envs["CLINE_SHA256"]:
                raise Fail("image-record", f"{name}: the build asserted CLINE_SHA256 {envs['CLINE_SHA256'][:12]}, the verified fork binary "
                                           f"is {str(fork.get('sha256'))[:12]}")
            rec["measurements"]["clineSha256"] = fork["sha256"]
            rec["measurements"]["forkBinary"] = {k: v for k, v in fork.items() if k != "sha256"}
        d = Path(os.environ.get("DEPLOY_RECORD_DIR", f"{RECORDS}/deploy"))
        d.mkdir(parents=True, exist_ok=True)
        path = d / f"{now:%Y-%m-%d}-image-build-{self.stage}-{name}-{tag[-12:]}-{self.account}.json"
        path.write_text(json.dumps(rec, indent=1) + "\n")
        say(f"image record {path}")

    def write_record(self, rec: dict) -> None:
        d = Path(os.environ.get("DEPLOY_RECORD_DIR", f"{RECORDS}/deploy"))
        d.mkdir(parents=True, exist_ok=True)
        path = d / f"{dt.datetime.now(dt.timezone.utc):%Y-%m-%d}-deploy-{self.stage}-{rec['codeSha'][:7]}-{self.account}.json"
        path.write_text(json.dumps(rec, indent=1) + "\n")
        say(f"record {path}")

    # ---- main -------------------------------------------------------------------------------

    def deploy(self) -> str:
        start = dt.datetime.now(dt.timezone.utc)
        sha = self.git("rev-parse", "HEAD")
        tree = digests.source_tree(self.root)
        say(f"stage={self.stage} sha={sha[:7]} sourceTree={tree[:12]} profile={self.env['AWS_PROFILE']}")
        self.check_account()
        azs = self.check_azs()
        vpc = self.check_vpc_cidr()
        self.settle_owner()
        self.report_config_record()
        doc = self.aws("ssm", "get-parameter", "--name", f"/hamh/{self.stage}/worker-revision", ok_missing=("ParameterNotFound",))
        current = int(doc["Parameter"]["Value"]) if doc else 0
        self.revision = current + 1 if self.bump else current
        say(f"worker revision {self.revision}{' (bumped from %d)' % current if self.bump else ''}")
        # The Instances part is never removed by a deploy. Said and refused first, before the guard and before anything is built: waiting for the runs to end would not help.
        self.instances_state = self.compute_instances_state()
        self.say_compute_instances(self.instances_state)
        self.refuse_instances_off(self.instances_state)
        guarded = self.bump
        if self.bump:
            self.guard()
        # A tools mode other than the live runtime's is a runtime environment change, so a new runtime version. It is said and guarded
        # before the Lambda build, so a refusal costs seconds; a deploy that is also a bump or a new image asks the guard once.
        self.tools_state = self.tools_mode_state()
        self.say_tools_mode(self.tools_state)
        if self.tools_state["change"] and not guarded:
            self.guard()
            guarded = True
        # The isolation words are part of each runtime's environment. A live runtime that lacks one or carries another gets a new runtime version from this deploy
        # (the stage was deployed before the word existed, or by another tree), so it meets the same guard, before the Lambda build.
        self.isolation = self.isolation_state()
        self.say_isolation(self.isolation)
        if self.isolation["change"] and not guarded:
            self.guard()
            guarded = True
        # Adding the Instances part changes the runtimes, so it meets the same guard, before the Lambda build.
        if self.instances_state["change"] and not guarded:
            self.guard()
            guarded = True
        self.build_lambda()
        self.install_infra()
        meta = self.run([*self.cargo, "metadata", "--format-version", "1", "--locked"])
        if meta.returncode != 0:
            raise Fail("cargo-metadata", meta.stderr.strip()[-200:])
        doc = json.loads(meta.stdout)
        tag = self.image_tag(WORKER_IMAGE, doc)
        # Same hashing as the worker key (git tree hashes of the inputs), over the delivery inputs.
        delivery_tag = "dl-" + digests._hash_paths(
            self.root, [*digests.local_crate_closure(doc, DELIVERY_PACKAGE), *DELIVERY_IMAGE_PATHS])
        order = self.stack_order()
        say("stacks " + " ".join(order))
        # A changed provider configuration is a different provider name, which CloudFormation replaces (the machines on the old provider go): the same guard as a new runtime version.
        replaced = self.provider_replacement()
        if replaced:
            say(f"capacity provider: the live provider is {replaced[0]} and this deploy's configuration gives {replaced[1]}, so CloudFormation REPLACES it and deletes the old one with the machines on it")
            if not guarded:
                self.guard()
                guarded = True
        boot = self.ensure_bootstrap()
        image = self.find_image(tag) if self.repo_exists() else None
        if not guarded and self.current_image and image != self.current_image:
            # A new worker image is a new runtime version.
            self.guard()
        if image:
            say(f"image {tag[:15]} found {image.rsplit(':', 1)[-1][:12]}")
        else:
            if not self.repo_exists():
                say("phase-1 pass: the stage has no worker repository yet")
                for name in order:
                    self.deploy_stack(name, None)
            say(f"image {tag[:15]} missing: building")
            image = self.build_image(sha, tag)
        self.delivery_image = self.resolve_delivery_image(sha, delivery_tag, order, image)
        for name in order:
            self.deploy_stack(name, image)
        checks = self.check_runtimes(self.expected_images(image))
        endpoints = self.version_endpoints(checks)
        logging = self.invocation_logging()
        lock = hashlib.sha256((self.root / "engine" / "lock.toml").read_bytes()).hexdigest()
        served = sorted({c["image"].split("@", 1)[1] for c in checks})
        hashes = self.template_hashes()
        asl = self.asl_versions()
        digest = digests.deployment_digest(tree, served, lock, asl, hashes)
        img = digests.image_digest(served, lock)
        self.record_assets()
        for name, value in (("deployment-digest", digest), ("deployed-source-tree", tree), ("image-digest", img),
                            ("worker-revision", str(self.revision)), ("deployment-config-digest", self.config_digest)):
            self.put_param(f"/hamh/{self.stage}/{name}", value)
        gomod = self.report_gomod_store()
        crit = Path(os.environ.get("DEPLOY_CRITERIA", ""))
        self.write_record({
            "schemaVersion": 1, "kind": "deploy", "createdAt": start.strftime("%Y-%m-%dT%H:%M:%SZ"),
            "host": socket.getfqdn(), "account": self.account, "region": self.region, "stage": self.stage, "codeSha": sha,
            "sourceTree": tree, "deploymentConfigDigest": self.config_digest, "deploymentDigest": digest, "imageDigest": img,
            "criteriaSha256": hashlib.sha256(crit.read_bytes()).hexdigest() if crit.is_file() else None,
            "result": "pass", "inputs": {"target": self.target.core(), "stacks": order, "imageTag": tag, "image": image,
                                         "workerRevision": self.revision, "toolsMode": self.tools_state, "computeInstances": self.instances_state, "isolationWords": self.isolation,
                                         "deliveryImageTag": delivery_tag, "deliveryImage": self.delivery_image},
            "measurements": {"runtimes": checks, "versionEndpoints": endpoints, "templateHashes": hashes, "aslVersions": asl,
                             "invocationLogging": logging, "gomodStore": gomod, "azs": azs, "vpcCidr": vpc, "bootstrap": boot,
                             "forkBinary": self.fork or {"outcome": "not-needed"},
                             "seconds": int((dt.datetime.now(dt.timezone.utc) - start).total_seconds())},
            "runIds": [], "notes": "",
        })
        return f"DEPLOY-PASS stage={self.stage} digest={digest[:12]} sourceTree={tree[:12]}"


def usage(msg: str) -> int:
    print(f"deploy.sh: {msg}\nusage: scripts/deploy.sh --stage direct [--azs 1] [--bump-worker-version] [--check-guard] [--fork-source auto|build|copy]", file=sys.stderr)
    return 2


def main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(add_help=False)
    p.add_argument("--stage", choices=("direct",))
    p.add_argument("--azs", type=int, choices=(1,))
    p.add_argument("--bump-worker-version", action="store_true")
    p.add_argument("--check-guard", action="store_true")
    p.add_argument("--fork-source", choices=FORK_SOURCES, default="auto")
    try:
        args, extra = p.parse_known_args(argv)
    except SystemExit:
        return usage("bad arguments")
    if extra or not args.stage:
        return usage("bad arguments" if extra else "--stage is required")
    if args.azs is not None and args.azs != AZS[args.stage]:
        return usage(f"stage {args.stage} has {AZS[args.stage]} AZs (infra/lib/config.ts)")
    try:
        d = Deployer(args.stage, bump=args.bump_worker_version, fork_source=args.fork_source)
    except (aws_target.TargetError, ToolsModeError, ComputeInstancesError) as e:
        return usage(str(e))
    if args.check_guard:
        try:
            line = d.check_guard()
        except Fail as e:
            print(f"DEPLOY-FAIL {e}", flush=True)
            return 1
        print(line, flush=True)
        return 0
    if d.git("status", "--porcelain", "--untracked-files=no"):
        print("deploy.sh: uncommitted changes to tracked files; commit first so the deploy matches a commit", file=sys.stderr)
        return 2
    try:
        line = d.deploy()
    except Fail as e:
        print(f"DEPLOY-FAIL {e}", flush=True)
        return 1
    print(line, flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
