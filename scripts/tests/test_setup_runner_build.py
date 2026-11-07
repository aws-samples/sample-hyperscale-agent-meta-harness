"""Tests for scripts/setup/runner-build.sh and scripts/setup/runner_state.py, the script the setup runner's CodeBuild project runs.

The script runs on the runner, inside CodeBuild, from the source archive that `setup.sh` uploaded. These cases run it from a made-up source tree on this machine
with a fake `aws` (a directory stands in for the buckets and a JSON file for the account) and stand-ins for the scripts it calls (deploy.sh, gomod-store.sh,
the scanner-kit tool, node.sh, the toolchain installer and build-client.sh), each of which records its arguments and a few environment variables and fails when a
test asks. They hold: the nine phases in order, the arguments and environment of each call, the git repository made from the archive, the two ways the generated
account file reaches the build, every refusal of the first two phases, a failure in every phase (result.json and phases.jsonl written, the SETUP-FAIL line, the
exit code), the content of result.json, the skipping of phases that already completed for the same commit and configuration, and the absence of a secret in the output.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import stat
import subprocess  # nosec B404: tests start the scripts under test and git with argument lists, never a shell
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path
from typing import Any, Dict, List, Optional

import setup_runner_support as sup
from setup_waits import limit

REPO = sup.REPO
PHASES = ["inputs", "fetch", "toolchain", "account-file", "deploy", "gomod", "scanner-rules", "client", "outputs"]
ACCOUNT = "111122223333"
REGION = "us-east-1"
PROFILE = "setup-profile"
BUCKET = f"hamh-setup-{ACCOUNT}-{REGION}"
COMMIT = hashlib.sha1(b"made-up source").hexdigest()  # nosec B324: a made-up commit name, no security use
OTHER_COMMIT = hashlib.sha1(b"other made-up source").hexdigest()  # nosec B324: a made-up commit name, no security use
BUILD_ID = "hamh-setup-runner:11111111-2222-3333-4444-555555555555"
ENDPOINT = f"https://abc123.execute-api.{REGION}.amazonaws.com/direct"
DEPLOY_DIGEST = "0123456789abcdef" * 4
SECRET = "".join(["wJalrXUtn", "FEMI/K7MDENG/bPxRfiCY", "EXAMPLE"])  # built from pieces: a made-up key value that must never appear in any output

# The CDK bootstrap template the node.sh stand-in prints for `cdk bootstrap --show-template`, and the pin of the made-up tree.
TEMPLATE_BYTES = b"AWSTemplateFormatVersion: '2010-09-09'\nDescription: stand-in for the CDK bootstrap template\nResources: {}\n"
TEMPLATE_SHA = hashlib.sha256(TEMPLATE_BYTES).hexdigest()
TEMPLATE_KEY = f"bootstrap/cdk-bootstrap-{TEMPLATE_SHA[:12]}.yaml"

STACK_NAMES = ["network", "security", "data", "packages", "control", "orchestration", "runtimes", "delivery", "observability"]

FAKE_AWS = r'''#!__PY__
import hashlib, json, os, shutil, sys
args = sys.argv[1:]
root = os.environ["FAKE_S3_ROOT"]
state = json.load(open(os.environ["FAKE_STATE"]))
with open(os.environ["FAKE_AWS_LOG"], "a") as f:
    f.write(json.dumps({"argv": args, "profile": os.environ.get("AWS_PROFILE")}) + "\n")


def opt(name):
    return args[args.index(name) + 1] if name in args else None


def path_of(bucket, key):
    return os.path.join(root, bucket, key)


def split(uri):
    bucket, key = uri[5:].split("/", 1)
    return bucket, key


def fail(message, code=254):
    sys.stderr.write(message + "\n")
    sys.exit(code)


if args[:2] == ["sts", "get-caller-identity"]:
    if state.get("sts_fail"):
        fail("An error occurred (ExpiredToken) when calling the GetCallerIdentity operation: The security token included in the request is expired")
    print(json.dumps({"Account": state["account"], "Arn": "arn:aws:sts::%s:assumed-role/role/session" % state["account"], "UserId": "AROAEXAMPLE:session"}))
elif args[:2] == ["s3", "cp"]:
    src, dst = args[2], args[3]
    if src.startswith("s3://"):
        bucket, key = split(src)
        p = path_of(bucket, key)
        if not os.path.isfile(p):
            fail("download failed: %s to %s An error occurred (404) when calling the HeadObject operation: Key \"%s\" does not exist" % (src, dst, key), 1)
        if dst == "-":
            sys.stdout.buffer.write(open(p, "rb").read())
        else:
            shutil.copyfile(p, dst)
    else:
        bucket, key = split(dst)
        first = state.get("fail_first_put")
        marker = os.environ["FAKE_STATE"] + ".first-put"
        if first and key.startswith(first) and not os.path.exists(marker):
            open(marker, "w").write("x")
            fail("upload failed: %s to %s An error occurred (SlowDown) when calling the PutObject operation: Please reduce your request rate." % (src, dst), 1)
        for prefix in state.get("put_denied", []):
            if key.startswith(prefix):
                fail("upload failed: %s to %s An error occurred (AccessDenied) when calling the PutObject operation: Access Denied" % (src, dst), 1)
        p = path_of(bucket, key)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        shutil.copyfile(src, p)
elif args[:2] == ["s3api", "get-object"]:
    version = opt("--version-id")
    if version and version in state.get("missing_versions", []):
        fail("An error occurred (NoSuchVersion) when calling the GetObject operation: The specified version does not exist.", 254)
    p = path_of(opt("--bucket"), opt("--key"))
    if not os.path.isfile(p):
        fail("An error occurred (NoSuchKey) when calling the GetObject operation: The specified key does not exist.", 254)
    shutil.copyfile(p, args[-1])
    print(json.dumps({"VersionId": version or "null", "ContentLength": os.path.getsize(p)}))
elif args[:2] == ["s3api", "head-object"]:
    p = path_of(opt("--bucket"), opt("--key"))
    if not os.path.isfile(p):
        fail("An error occurred (404) when calling the HeadObject operation: Not Found", 254)
    print(json.dumps({"ContentLength": os.path.getsize(p)}))
elif args[:2] == ["s3api", "list-objects-v2"]:
    bucket, prefix = opt("--bucket"), opt("--prefix") or ""
    base = os.path.join(root, bucket)
    keys = []
    for d, _, files in os.walk(base):
        for name in files:
            key = os.path.relpath(os.path.join(d, name), base)
            if key.startswith(prefix):
                keys.append(key)
    print(json.dumps({"Contents": [{"Key": k} for k in sorted(keys)]} if keys else {}))
elif args[:2] == ["ssm", "get-parameter"]:
    name = opt("--name")
    if name not in state["ssm"]:
        fail("An error occurred (ParameterNotFound) when calling the GetParameter operation: ", 254)
    print(json.dumps({"Parameter": {"Name": name, "Value": state["ssm"][name]}}))
elif args[:2] == ["cloudformation", "describe-stacks"]:
    name = opt("--stack-name")
    if name not in state["stacks"]:
        fail("An error occurred (ValidationError) when calling the DescribeStacks operation: Stack with id %s does not exist" % name, 254)
    print(json.dumps({"Stacks": [{"StackName": name, "StackStatus": state.get("stack_statuses", {}).get(name) or state.get("stack_status", "CREATE_COMPLETE"), "Outputs": [{"OutputKey": k, "OutputValue": v} for k, v in state["stacks"][name].items()]}]}))
else:
    fail("fake aws: unsupported call %s" % " ".join(args), 252)
'''

STUB_COMMON = r'''
import json, os, sys
def record(name, extra=None):
    keys = ("HAMH_ACCOUNTS_FILE", "AWS_PROFILE", "HAMH_PYTHON", "NODE_SH_IMAGE_MIRROR", "DEPLOY_RECORD_DIR", "GOMOD_STORE_GO", "FAKE_TOOLCHAIN_ENV",
            "SETUP_BINARY_NAME", "HAMH_CLIENT_BIN", "SETUP_STAGE", "HAMH_TOOLS", "NODE_SH_TOOLS", "NODE_SH_TMP", "TMPDIR", "AWS_CONFIG_FILE", "CARGO_HOME", "TEARDOWN_EVIDENCE_DIR", "RUNNER_TOOLCHAIN_ONLY",
            "HAMH_AWS_PROFILE", "CAH_AWS_PROFILE", "CAH_ACCOUNTS_FILE")  # COMPAT-CAH: the earlier spellings of the two selectors
    doc = {"name": name, "argv": sys.argv[1:], "cwd": os.getcwd(), "env": {k: os.environ.get(k) for k in keys}}
    doc.update(extra or {})
    with open(os.environ["FAKE_CALLS"], "a") as f:
        f.write(json.dumps(doc) + "\n")
'''

STUBS = {
    "scripts/setup/runner-toolchain.sh": r'''#!/usr/bin/env bash
"${HAMH_PYTHON:-python3}" - "$@" <<'PY'
__COMMON__
record("toolchain")
PY
[ "${FAKE_TOOLCHAIN:-pass}" = fail ] && { echo "TOOLCHAIN FAIL rustup-init exit 1"; exit 1; }
mkdir -p "$1/bin"
printf 'export HAMH_TOOLS=%s\nexport FAKE_TOOLCHAIN_ENV=1\nexport PATH="%s/bin:$PATH"\n' "$1" "$1" > "$1/env.sh"
echo "TOOLCHAIN PASS rust=1.98.1"
''',
    "scripts/deploy.sh": r'''#!/usr/bin/env bash
"${HAMH_PYTHON:-python3}" - "$@" <<'PY'
__COMMON__
import subprocess
st = subprocess.run(["git", "status", "--porcelain", "--untracked-files=no"], capture_output=True, text=True).stdout
log = subprocess.run(["git", "log", "--format=%H"], capture_output=True, text=True).stdout.split()
acc = os.environ.get("HAMH_ACCOUNTS_FILE")
record("deploy", {"git_status": st, "commits": len(log), "accounts_json_in_tree": os.path.isfile("accounts.json"),
                  "accounts_file_text": open(acc).read() if acc and os.path.isfile(acc) else None})
heal = os.environ.get("FAKE_DEPLOY_HEAL")
if heal:
    path = os.environ["FAKE_STATE"]
    st_doc = json.load(open(path))
    st_doc.pop("stack_status", None)
    for stack in heal.split(","):
        st_doc["stacks"].setdefault(stack, {})
    with open(path, "w") as f:
        json.dump(st_doc, f)
PY
case "${FAKE_DEPLOY:-pass}" in
  pass) echo "deploy: stage=direct sha=abcdef0"; echo "DEPLOY-PASS stage=direct digest=__DIGEST12__ sourceTree=fedcba987654"; exit 0 ;;
  fail) echo "deploy: stack hamh-direct-network"; echo "DEPLOY-FAIL stack hamh-direct-network"; exit 1 ;;
  silent) echo "deploy: done"; exit 0 ;;
  passfail) echo "DEPLOY-PASS stage=direct digest=__DIGEST12__ sourceTree=fedcba987654"; exit 1 ;;
  failrc) echo "deploy: oops"; exit 3 ;;
  guard) echo "deploy: stage direct"; echo "DEPLOY-FAIL guard 2 running run(s) on hamh-direct-run pin the worker runtime: run-a, run-b"; exit 1 ;;
esac
''',
    "scripts/gomod-store.sh": r'''#!/usr/bin/env bash
"${HAMH_PYTHON:-python3}" - "$@" <<'PY'
__COMMON__
record("gomod")
PY
case "$1:${FAKE_GOMOD:-pass}" in
  fill:fill-fail) echo "GOMOD-FILL FAIL account 999999999999"; exit 1 ;;
  fill:current) echo "GOMOD-FILL CURRENT stage=direct roots=3 modules=9 objects=40 bucket=b manifest=m"; exit 0 ;;
  fill:*) touch "$FAKE_STATE.filled"; echo "GOMOD-FILL PASS stage=direct roots=3 modules=9 objects=40 put=40 exists=0 bucket=b manifest=m"; exit 0 ;;
  check:check-fail) echo "GOMOD-CHECK FAIL 2 objects missing"; exit 1 ;;
  check:store-empty) if [ -e "$FAKE_STATE.filled" ]; then echo "GOMOD-CHECK PASS stage=direct modules=9 objects=40 manifests=1"; exit 0; fi; echo "GOMOD-CHECK FAIL empty stage=direct bucket=b"; exit 1 ;;
  check:*) echo "GOMOD-CHECK PASS stage=direct modules=9 objects=40 manifests=1"; exit 0 ;;
esac
''',
    "scripts/scanner_kit_rules.py": r'''#!/usr/bin/env python3
__COMMON__
import pathlib
record("rules")
mode = os.environ.get("FAKE_RULES", "pass")
if sys.argv[1] == "build":
    if mode == "build-fail":
        sys.stderr.write("scanner_kit_rules.py: refused: the commit is not the pin\n")
        sys.exit(1)
    out = pathlib.Path(sys.argv[sys.argv.index("--out") + 1])
    out.mkdir(parents=True, exist_ok=True)
    (out / "hamh_semgrep_rules-1.0.0-py3-none-any.whl").write_bytes(b"wheel")
    print("SCANNER-KIT-RULES wheel hamh_semgrep_rules-1.0.0-py3-none-any.whl")
    print("SCANNER-KIT-RULES PASS")
else:
    if mode == "publish-fail":
        sys.stderr.write("scanner_kit_rules.py: refused: the repository answered 403\n")
        sys.exit(1)
    print("SCANNER-KIT-RULES PUBLISHED hamh-semgrep-rules 1.0.0 sha256=00")
''',
    "scripts/node.sh": r'''#!/usr/bin/env bash
"${HAMH_PYTHON:-python3}" - "$@" <<'PY'
__COMMON__
record("node")
PY
[ "${FAKE_NODE:-pass}" = fail ] && { echo "node.sh: docker pull node failed" >&2; exit 1; }
if [ "$*" = "cdk bootstrap --show-template" ]; then
  case "${FAKE_TEMPLATE:-ok}" in
    ok) cat "$(dirname "$0")/fake-template.yaml" ;;
    other) printf 'Description: another template\n' ;;
    huge) head -c 60000 /dev/zero | tr '\0' 'x' ;;
    empty) ;;
    fail) echo "cdk: Unable to resolve AWS account to use" >&2; exit 1 ;;
  esac
  exit 0
fi
echo v22.0.0
''',
    "scripts/teardown.sh": r'''#!/usr/bin/env bash
"${HAMH_PYTHON:-python3}" - "$@" <<'PY'
__COMMON__
import pathlib
record("teardown")
evidence = os.environ.get("TEARDOWN_EVIDENCE_DIR")
if evidence:
    pathlib.Path(evidence).mkdir(parents=True, exist_ok=True)
    (pathlib.Path(evidence) / "2026-10-29-teardown-direct.md").write_text("evidence of the teardown\n")
PY
case "${FAKE_TEARDOWN:-pass}" in
  pass) echo "teardown: stage direct"; echo "TEARDOWN-INVENTORY 0 resources stage=direct kms-pending=2"; exit 0 ;;
  fail) echo "teardown: 3 resources left"; echo "TEARDOWN-INVENTORY 3 resources"; exit 1 ;;
esac
''',
    "scripts/setup/build-client.sh": r'''#!/usr/bin/env bash
"${HAMH_PYTHON:-python3}" - "$@" <<'PY'
__COMMON__
record("client")
PY
mode="${FAKE_CLIENT:-pass}"
out="$1" bucket="$2" id="$3"
case "$mode" in
  missing) exit 99 ;;
  fail) echo "build-client: cargo zigbuild failed for darwin-arm64" >&2; exit 1 ;;
  badjson) echo "not json"; exit 0 ;;
esac
mkdir -p "$out"
printf 'x' > "$out/ham-linux-amd64"
printf 'operator tool' > "$out/ham-admin-linux-amd64"
sum=$(sha256sum "$out/ham-linux-amd64" | cut -d' ' -f1)
asum=$(sha256sum "$out/ham-admin-linux-amd64" | cut -d' ' -f1)
[ "$mode" = nosums ] || printf '%s  linux-amd64/ham\n%s  linux-amd64/ham-admin\n' "$sum" "$asum" > "$out/SHA256SUMS"
aws s3 cp "$out/ham-linux-amd64" "s3://$bucket/client/$id/linux-amd64/ham" --only-show-errors || exit 1
case "$mode" in
  adminmissing) ;;
  adminwrong) printf 'another file' > "$out/other-admin"; aws s3 cp "$out/other-admin" "s3://$bucket/client/$id/linux-amd64/ham-admin" --only-show-errors || exit 1 ;;
  *) aws s3 cp "$out/ham-admin-linux-amd64" "s3://$bucket/client/$id/linux-amd64/ham-admin" --only-show-errors || exit 1 ;;
esac
[ "$mode" = nosums ] || aws s3 cp "$out/SHA256SUMS" "s3://$bucket/client/$id/SHA256SUMS" --only-show-errors || exit 1
if [ "$mode" = noadmin ]; then
  printf '{"linux-amd64": {"key": "client/%s/linux-amd64/ham", "sha256": "%s", "bytes": 1}}\n' "$id" "$sum"
else
  printf '{"linux-amd64": {"key": "client/%s/linux-amd64/ham", "sha256": "%s", "bytes": 1, "admin": {"key": "client/%s/linux-amd64/ham-admin", "sha256": "%s", "bytes": 13}}}\n' "$id" "$sum" "$id" "$asum"
fi
''',
}

NAMES_SH = "# stand-in for scripts/setup/names.sh\nHAMH_CLIENT_BIN=${HAMH_CLIENT_BIN:-widget}\nHAMH_PRODUCT_NAME=Widget\nHAMH_CONFIG_DIR_NAME=widget\n"


def accounts_doc(profile: str = PROFILE, account: str = ACCOUNT, region: str = REGION, logging: str = "read-only") -> Dict[str, Any]:
    return {"schemaVersion": 1, "selector": "HAMH_AWS_PROFILE", "accounts": [{
        "account": account, "region": region, "profile": profile, "role": "active",
        "azs": {"live": [f"{region}a"], "drill": [f"{region}a"], "direct": [f"{region}a"]},
        "azIds": {"live": ["use1-az1"], "drill": ["use1-az1"], "direct": ["use1-az1"]},
        "invocationLogging": logging}]}


def sha12(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()[:12]


def source_tree_of(tree: Path) -> str:
    """The value deploy.sh records as /hamh/direct/deployed-source-tree for a tree, from the repository's own digests module."""
    sys.path.insert(0, str(REPO / "scripts" / "lib"))
    try:
        import digests
        return digests.source_tree(tree)
    finally:
        sys.path.pop(0)


def git(cwd: Path, *args: str) -> str:
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True, timeout=limit(60)).stdout.strip()  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do


class Rig:
    """One run of runner-build.sh: a made-up source tree, a directory for the buckets, a fake aws, and the stand-ins."""

    def __init__(self, test: unittest.TestCase, override_supported: bool = True, commit: str = COMMIT, pin: bool = True, override_text: Optional[str] = None) -> None:
        self.test = test
        self.tmp = Path(tempfile.mkdtemp(prefix="runner-build-"))
        test.addCleanup(shutil.rmtree, self.tmp, True)
        self.tree = self.tmp / "src"
        self.work = self.tmp / "work"
        self.bin = self.tmp / "bin"
        self.s3 = self.tmp / "s3"
        self.home = self.tmp / "home"
        for d in (self.tree, self.bin, self.s3, self.home):
            d.mkdir(parents=True)
        self.state_file = self.tmp / "state.json"
        self.aws_log = self.tmp / "aws.log"
        self.calls_file = self.tmp / "calls.log"
        self.commit = commit
        self.config = json.dumps(accounts_doc(), indent=1).encode()
        self.state: Dict[str, Any] = {
            "account": ACCOUNT, "ssm": {"/cdk-bootstrap/hnb659fds/version": "30", "/hamh/direct/deployment-digest": DEPLOY_DIGEST},
            "stacks": {**{f"hamh-direct-{n}": {} for n in STACK_NAMES},
                       "hamh-direct-control": {"V1ApiUrl": ENDPOINT, "InternalApiUrl": "https://internal.example.invalid/direct"}, "hamh-direct-runtimes": {"WorkerPythonRuntimeId": "rt-1"}}}
        self.env_extra: Dict[str, str] = {}
        self.override_supported = override_supported
        self.override_text = override_text
        self.build_id = BUILD_ID
        self.pin = pin
        self.make_tree()
        self.write_fake_aws()
        self.put_config()
        self.put_source()

    # ---- the made-up world ----
    def make_tree(self) -> None:
        for rel in ("scripts/setup/runner-build.sh", "scripts/setup/runner_state.py", "scripts/lib/python.sh", "scripts/lib/aws_target.py", "scripts/lib/aws-target.sh",
                    "scripts/lib/digests.py"):
            dst = self.tree / rel
            dst.parent.mkdir(parents=True, exist_ok=True)
            if (REPO / rel).is_file():
                shutil.copyfile(REPO / rel, dst)
                dst.chmod(0o755)
        for rel, body in STUBS.items():
            dst = self.tree / rel
            dst.parent.mkdir(parents=True, exist_ok=True)
            text = body.replace("__COMMON__", STUB_COMMON).replace("__DIGEST12__", DEPLOY_DIGEST[:12])
            dst.write_text(text)
            dst.chmod(0o755)
        (self.tree / "scripts/setup/names.sh").write_text(NAMES_SH)
        (self.tree / "scripts/fake-template.yaml").write_bytes(TEMPLATE_BYTES)
        if self.pin:
            (self.tree / "engine/policy").mkdir(parents=True, exist_ok=True)
            (self.tree / "engine/policy/cdk-bootstrap-template.pin").write_text(f"# made-up pin\nawsCdk=2.0.0\nsha256={TEMPLATE_SHA}\n")
        token = self.override_text or ("HAMH_ACCOUNTS_FILE" if self.override_supported else "SELECTION")
        for rel in ("infra/lib/config.ts", "crates/harness-protocol/build.rs"):
            p = self.tree / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(f"// reads {token}\n")
        (self.tree / "engine/rules/deps").mkdir(parents=True, exist_ok=True)
        (self.tree / "engine/rules/deps/go-approved-modules.txt").write_text("example.invalid/mod@v1.0.0\n")
        (self.tree / "scripts/gomod-tool-roots.txt").write_text("example.invalid/tool@v1.0.0\n")
        (self.tree / "tools.lock").write_text('schema = "1"\n')
        (self.tree / "README.md").write_text("made-up source\n")

    def write_fake_aws(self) -> None:
        p = self.bin / "aws"
        p.write_text(FAKE_AWS.replace("__PY__", sys.executable))
        p.chmod(0o755)

    def save_state(self) -> None:
        self.state_file.write_text(json.dumps(self.state))

    def put(self, key: str, data: bytes, bucket: str = BUCKET) -> None:
        p = self.s3 / bucket / key
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(data)

    def get(self, key: str, bucket: str = BUCKET) -> Optional[bytes]:
        p = self.s3 / bucket / key
        return p.read_bytes() if p.is_file() else None

    def put_config(self) -> None:
        self.config_key = f"config/accounts-{sha12(self.config)}.json"
        self.put(self.config_key, self.config)

    def put_source(self, comment: Optional[str] = None) -> None:
        zpath = self.tmp / "source.zip"
        with zipfile.ZipFile(zpath, "w") as zf:
            for p in sorted(self.tree.rglob("*")):
                if p.is_file() and ".git" not in p.parts:
                    zf.write(p, p.relative_to(self.tree).as_posix())
            zf.comment = (self.commit if comment is None else comment).encode()
        self.source_key = f"source/{self.commit}.zip"
        self.source_bytes = zpath.read_bytes()
        self.put(self.source_key, self.source_bytes)

    def mark_deployed(self) -> None:
        """What a real deploy.sh leaves in SSM about the tree and the account file it deployed (the stand-in deploy.sh writes nothing); the runner reads it to decide
        that a deploy can be skipped."""
        self.state["ssm"]["/hamh/direct/deployed-source-tree"] = source_tree_of(self.tree)
        self.state["ssm"]["/hamh/direct/deployment-config-digest"] = hashlib.sha256(self.config).hexdigest()

    # ---- a run ----
    def env(self) -> Dict[str, str]:
        env = {
            "PATH": f"{self.bin}:{os.environ.get('PATH', '/usr/bin:/bin')}",
            "HOME": str(self.home), "TMPDIR": str(self.tmp / "tmp"), "LANG": "C.UTF-8",
            "HAMH_PYTHON": sys.executable,
            "CODEBUILD_SRC_DIR": str(self.tree), "CODEBUILD_BUILD_ID": self.build_id, "AWS_REGION": REGION, "AWS_DEFAULT_REGION": REGION,
            "SETUP_SOURCE_KEY": self.source_key, "SETUP_CONFIG_KEY": self.config_key, "SETUP_STAGE": "direct", "SETUP_AZS": "1",
            "SETUP_SOURCE_SHA256": hashlib.sha256(self.source_bytes).hexdigest(), "SETUP_CONFIG_SHA256": hashlib.sha256(self.config).hexdigest(),
            "RUNNER_BUILD_WORK": str(self.work), "RUNNER_BUILD_UPLOAD_SLEEP": "0",
            "FAKE_S3_ROOT": str(self.s3), "FAKE_STATE": str(self.state_file), "FAKE_AWS_LOG": str(self.aws_log), "FAKE_CALLS": str(self.calls_file),
            "AWS_SECRET_ACCESS_KEY": SECRET, "AWS_SESSION_TOKEN": SECRET + "TOKEN",
        }
        (self.tmp / "tmp").mkdir(exist_ok=True)
        env.update(self.env_extra)
        return env

    def run(self, **extra: str) -> subprocess.CompletedProcess:
        self.save_state()
        env = self.env()
        env.update(extra)
        for key in [k for k, v in env.items() if v is None]:
            del env[key]
        return subprocess.run(["bash", str(self.tree / "scripts/setup/runner-build.sh")], cwd=self.tree, env=env, capture_output=True, text=True, timeout=limit(180))  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do

    # ---- what a run left ----
    def result(self, build_id: Optional[str] = None) -> Optional[Dict[str, Any]]:
        raw = self.get(f"results/{build_id or self.build_id}/result.json")
        return json.loads(raw) if raw else None

    def phase_lines(self, build_id: Optional[str] = None) -> List[Dict[str, Any]]:
        raw = self.get(f"results/{build_id or self.build_id}/phases.jsonl")
        return [json.loads(line) for line in raw.decode().splitlines()] if raw else []

    def calls(self, name: Optional[str] = None) -> List[Dict[str, Any]]:
        if not self.calls_file.is_file():
            return []
        out = [json.loads(line) for line in self.calls_file.read_text().splitlines()]
        return [c for c in out if name is None or c["name"] == name]

    def aws_calls(self) -> List[List[str]]:
        if not self.aws_log.is_file():
            return []
        return [json.loads(line)["argv"] for line in self.aws_log.read_text().splitlines()]


def last_line(proc: subprocess.CompletedProcess) -> str:
    return [ln for ln in proc.stdout.splitlines() if ln.strip()][-1]


class Base(unittest.TestCase):
    def setUp(self) -> None:
        if not sup.BUILD_SCRIPT.is_file():
            self.fail("scripts/setup/runner-build.sh does not exist")
        if not sup.STATE_HELPER.is_file():
            self.fail("scripts/setup/runner_state.py does not exist")

    def rig(self, **kw: Any) -> Rig:
        return Rig(self, **kw)

    def assertFails(self, proc: subprocess.CompletedProcess, rig: Rig, phase: str, needle: str) -> Dict[str, Any]:
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        line = last_line(proc)
        self.assertTrue(line.startswith(f"SETUP-FAIL {phase} "), line)
        self.assertIn(needle, line)
        return {"line": line}


class HappyPathCase(Base):
    def test_all_nine_phases_pass_in_order(self) -> None:
        rig = self.rig()
        proc = rig.run()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(last_line(proc), f"SETUP-PASS stage=direct digest={DEPLOY_DIGEST[:12]} endpoint={ENDPOINT}")
        lines = rig.phase_lines()
        self.assertEqual([ln["name"] for ln in lines], PHASES)
        self.assertEqual({ln["state"] for ln in lines}, {"ok"})
        for ln in lines:
            self.assertEqual((ln["buildId"], ln["stage"], ln["sourceCommit"], ln["configDigest"]), (BUILD_ID, "direct", COMMIT, sha12(rig.config)))
            self.assertIsInstance(ln["seconds"], int)

    def test_result_json(self) -> None:
        rig = self.rig()
        rig.run()
        res = rig.result()
        self.assertEqual(sorted(res), sorted([
            "schema", "state", "sourceCommit", "configDigest", "buildId", "stage", "region", "account", "endpoint", "stackOutputs", "client", "phases",
            "failedPhase", "detail", "deploymentDigest", "action"]))
        self.assertEqual((res["schema"], res["state"], res["action"]), ("1", "SETUP-PASS", "deploy"))
        self.assertEqual((res["sourceCommit"], res["configDigest"], res["buildId"]), (COMMIT, sha12(rig.config), BUILD_ID))
        self.assertEqual((res["stage"], res["region"], res["account"], res["endpoint"]), ("direct", REGION, ACCOUNT, ENDPOINT))
        self.assertEqual(res["deploymentDigest"], DEPLOY_DIGEST[:12])
        self.assertEqual(res["stackOutputs"]["hamh-direct-control.V1ApiUrl"], ENDPOINT)
        self.assertEqual(sorted(res["client"]), ["linux-amd64"])
        self.assertEqual(sorted(res["client"]["linux-amd64"]), ["admin", "bytes", "key", "sha256"])
        self.assertEqual(sorted(res["client"]["linux-amd64"]["admin"]), ["bytes", "key", "sha256"])
        self.assertEqual(res["client"]["linux-amd64"]["admin"]["key"], f"client/{BUILD_ID}/linux-amd64/ham-admin", "the operator tool is listed beside the client")
        self.assertEqual([p["name"] for p in res["phases"]], PHASES)
        self.assertEqual({tuple(sorted(p)) for p in res["phases"]}, {("name", "seconds", "state")})
        self.assertIsNone(res["failedPhase"])

    def test_the_hash_of_the_result_is_left_for_the_buildspec_to_export(self) -> None:
        """images/codebuild/setup-runner.yml sources <work>/anchor.env; CodeBuild keeps SETUP_RESULT_SHA256 with the build, and setup.sh believes a result.json only
        when its bytes hash to that value. The file is written after the upload, from the bytes that were uploaded."""
        rig = self.rig()
        proc = rig.run()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        uploaded = rig.get(f"results/{BUILD_ID}/result.json")
        self.assertEqual((rig.work / "anchor.env").read_text(), f"export SETUP_RESULT_SHA256={hashlib.sha256(uploaded).hexdigest()}\n")

    def test_a_build_that_failed_leaves_no_hash(self) -> None:
        rig = self.rig()
        proc = rig.run(FAKE_DEPLOY="fail")
        self.assertEqual(proc.returncode, 1)
        self.assertIsNotNone(rig.result(), "the failure is recorded in the bucket")
        self.assertFalse((rig.work / "anchor.env").exists(), "nothing for setup.sh to believe")

    def test_calls_carry_the_contract_arguments(self) -> None:
        rig = self.rig()
        rig.run()
        self.assertEqual(rig.calls("deploy")[0]["argv"], ["--stage", "direct", "--azs", "1", "--fork-source", "build"])
        fill, check = rig.calls("gomod")
        self.assertEqual(fill["argv"][:2], ["fill", "--stage"])
        self.assertIn("direct", fill["argv"])
        self.assertEqual(fill["argv"][fill["argv"].index("--modules") + 1], "engine/rules/deps/go-approved-modules.txt")
        self.assertEqual(fill["argv"][fill["argv"].index("--tools") + 1], "scripts/gomod-tool-roots.txt")
        self.assertEqual(check["argv"], ["check", "--stage", "direct"])
        rules = rig.calls("rules")
        self.assertEqual([c["argv"][0] for c in rules], ["build", "publish"])
        self.assertIn("--apply", rules[1]["argv"])
        wheel = rules[1]["argv"][rules[1]["argv"].index("--wheel") + 1]
        self.assertTrue(wheel.endswith("hamh_semgrep_rules-1.0.0-py3-none-any.whl"))
        client = rig.calls("client")[0]["argv"]
        self.assertEqual(client[1:], [BUCKET, BUILD_ID])
        self.assertTrue(client[0].startswith(str(rig.work)))

    def test_both_spellings_of_the_selectors_are_gone_when_the_deploy_starts(self) -> None:
        # a selector in the environment of the build (either spelling) must not reach the deploy: the generated account file is the only choice
        rig = self.rig()
        proc = rig.run(HAMH_AWS_PROFILE="stray-profile", CAH_AWS_PROFILE="stray-profile", CAH_ACCOUNTS_FILE="/nonexistent/accounts.json")  # COMPAT-CAH
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        deploy = rig.calls("deploy")[0]["env"]
        self.assertIsNone(deploy["HAMH_AWS_PROFILE"])
        self.assertIsNone(deploy["CAH_AWS_PROFILE"])  # COMPAT-CAH
        self.assertIsNone(deploy["CAH_ACCOUNTS_FILE"])  # COMPAT-CAH
        self.assertTrue(deploy["HAMH_ACCOUNTS_FILE"].startswith(str(rig.work)))

    def test_the_override_probe_reads_the_name_of_the_setting_under_either_prefix(self) -> None:
        # the archive's config.ts and build.rs may read the setting through a resolver that holds the name without one spelling of the prefix
        rig = self.rig(override_text="the setting ACCOUNTS_FILE under either prefix")
        proc = rig.run()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertFalse(rig.calls("deploy")[0]["accounts_json_in_tree"], "the file is not committed into the tree when the archive reads the setting")

    def test_environment_of_the_calls(self) -> None:
        rig = self.rig()
        rig.run()
        deploy = rig.calls("deploy")[0]["env"]
        self.assertTrue(deploy["AWS_CONFIG_FILE"].startswith(str(rig.work)), "a config file of the build holds the profile of the account file")
        self.assertEqual(deploy["FAKE_TOOLCHAIN_ENV"], "1", "the env.sh of the toolchain installer is read")
        self.assertEqual(deploy["NODE_SH_IMAGE_MIRROR"], "public.ecr.aws/docker/library")
        self.assertTrue(deploy["DEPLOY_RECORD_DIR"].startswith(str(rig.work)))
        self.assertTrue(deploy["HAMH_ACCOUNTS_FILE"].startswith(str(rig.work)), "the generated file lies outside the source tree")
        self.assertEqual(json.loads(Path(deploy["HAMH_ACCOUNTS_FILE"]).read_bytes()), accounts_doc())
        self.assertEqual(deploy["NODE_SH_TOOLS"], deploy["HAMH_TOOLS"])
        gomod = rig.calls("gomod")[0]["env"]
        self.assertTrue(gomod["GOMOD_STORE_GO"].endswith("/go/bin/go"))
        self.assertEqual(rig.calls("toolchain")[0]["argv"], [gomod["HAMH_TOOLS"]])

    def test_aws_config_has_a_profile_for_the_container_credentials(self) -> None:
        rig = self.rig()
        rig.run()
        config = Path(rig.calls("deploy")[0]["env"]["AWS_CONFIG_FILE"])
        text = config.read_text()
        self.assertIn(f"[profile {PROFILE}]", text)
        self.assertIn("credential_source = EcsContainer", text)
        self.assertIn(f"region = {REGION}", text)
        self.assertNotIn(SECRET, text)

    def test_the_binary_name_comes_from_names_sh(self) -> None:
        rig = self.rig()
        rig.run()
        self.assertEqual(rig.calls("client")[0]["env"]["SETUP_BINARY_NAME"], "widget")
        rig2 = self.rig()
        rig2.run(SETUP_BINARY_NAME="ham")
        self.assertEqual(rig2.calls("client")[0]["env"]["SETUP_BINARY_NAME"], "ham")
        # build-client.sh reads the client name from names.sh, which an exported HAMH_CLIENT_BIN overrides: the runner passes the name of the build on
        self.assertEqual(rig.calls("client")[0]["env"]["HAMH_CLIENT_BIN"], "widget")
        self.assertEqual(rig2.calls("client")[0]["env"]["HAMH_CLIENT_BIN"], "ham", "build-client.sh builds the binary the build was asked for")

    def test_the_script_names_the_client_binary_only_through_the_variable(self) -> None:
        text = sup.BUILD_SCRIPT.read_text()
        for n, line in enumerate(text.splitlines(), 1):
            code = line.split("#", 1)[0]
            self.assertIsNone(re.search(r"(^|[\s;&|(\"'])ham(\s|$|\"|')", code), f"line {n} names the client binary: {line}")

    def test_the_script_never_traces_or_prints_the_environment(self) -> None:
        text = sup.BUILD_SCRIPT.read_text()
        self.assertNotIn("set -x", text)
        self.assertNotRegex(text, r"(?m)^\s*(printenv|env|export -p|declare -x)\s*(\||$)")

    def test_nothing_secret_reaches_the_output_or_the_bucket(self) -> None:
        rig = self.rig()
        proc = rig.run()
        blob = proc.stdout + proc.stderr
        for p in (rig.s3).rglob("*"):
            if p.is_file():
                blob += p.read_bytes().decode("utf-8", "replace")
        self.assertNotIn(SECRET, blob)


class SourceVersionCase(Base):
    VERSION = "3HL4kqtJlcpXroDTDmJ.xUZgSJn2lQz"

    def test_the_archive_is_read_by_the_version_codebuild_extracted(self) -> None:
        rig = self.rig()
        proc = rig.run(CODEBUILD_SOURCE_VERSION=self.VERSION)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        calls = [c for c in rig.aws_calls() if c[:2] == ["s3api", "get-object"]]
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0][calls[0].index("--version-id") + 1], self.VERSION)
        self.assertEqual(calls[0][calls[0].index("--key") + 1], rig.source_key)

    def test_a_version_that_is_gone_stops_the_build(self) -> None:
        rig = self.rig()
        rig.state["missing_versions"] = [self.VERSION]
        proc = rig.run(CODEBUILD_SOURCE_VERSION=self.VERSION)
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertTrue(last_line(proc).startswith("SETUP-FAIL fetch "), proc.stdout)
        self.assertIn(self.VERSION, last_line(proc))

    def test_a_value_that_is_no_version_of_the_object_stops_the_build(self) -> None:
        rig = self.rig()
        rig.state["missing_versions"] = ["main"]
        proc = rig.run(CODEBUILD_SOURCE_VERSION="main")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertTrue(last_line(proc).startswith("SETUP-FAIL fetch "), proc.stdout)
        self.assertIn("main", last_line(proc))

    def test_without_a_version_the_plain_copy_is_used(self) -> None:
        for extra in ({}, {"CODEBUILD_SOURCE_VERSION": ""}, {"CODEBUILD_SOURCE_VERSION": "null"}):
            rig = self.rig()
            proc = rig.run(**extra)
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertEqual([c for c in rig.aws_calls() if c[:2] == ["s3api", "get-object"]], [], extra)


class SourceTreeCase(Base):
    def test_git_repository_is_made_from_the_archive_and_the_tree_is_clean(self) -> None:
        rig = self.rig()
        rig.run()
        deploy = rig.calls("deploy")[0]
        self.assertEqual(deploy["git_status"], "")
        self.assertEqual(os.path.realpath(deploy["cwd"]), os.path.realpath(str(rig.tree)))
        self.assertTrue((rig.tree / ".git").is_dir())

    def test_override_supported_leaves_the_tree_as_the_commit_has_it(self) -> None:
        rig = self.rig(override_supported=True)
        rig.run()
        deploy = rig.calls("deploy")[0]
        self.assertEqual(deploy["commits"], 1)
        self.assertFalse(deploy["accounts_json_in_tree"])
        self.assertEqual(json.loads(deploy["accounts_file_text"]), accounts_doc())

    def test_without_override_support_the_file_is_copied_in_and_committed(self) -> None:
        rig = self.rig(override_supported=False)
        proc = rig.run()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        deploy = rig.calls("deploy")[0]
        self.assertEqual(deploy["commits"], 2)
        self.assertTrue(deploy["accounts_json_in_tree"])
        self.assertEqual(deploy["git_status"], "")
        self.assertEqual(json.loads((rig.tree / "accounts.json").read_bytes()), accounts_doc())
        detail = [ln for ln in rig.phase_lines() if ln["name"] == "account-file"][0]["detail"]
        self.assertNotIn("interim", detail)
        self.assertNotIn("yet", detail.split())
        self.assertIn("does not read HAMH_ACCOUNTS_FILE", detail)


class InputsCase(Base):
    def test_each_input_is_checked(self) -> None:
        cases = [
            ({"SETUP_SOURCE_KEY": None}, "SETUP_SOURCE_KEY"),
            ({"SETUP_SOURCE_KEY": "source/not-a-commit.zip"}, "SETUP_SOURCE_KEY"),
            ({"SETUP_CONFIG_KEY": None}, "SETUP_CONFIG_KEY"),
            ({"SETUP_CONFIG_KEY": "config/accounts-XYZ.json"}, "SETUP_CONFIG_KEY"),
            ({"SETUP_STAGE": "live"}, "SETUP_STAGE"),
            ({"SETUP_AZS": "2"}, "SETUP_AZS"),
            ({"SETUP_BINARY_NAME": "Bad Name"}, "SETUP_BINARY_NAME"),
            ({"SETUP_SOURCE_SHA256": None}, "SETUP_SOURCE_SHA256"),
            ({"SETUP_SOURCE_SHA256": "abc"}, "SETUP_SOURCE_SHA256"),
            ({"SETUP_SOURCE_SHA256": "G" * 64}, "SETUP_SOURCE_SHA256"),
            ({"SETUP_CONFIG_SHA256": None}, "SETUP_CONFIG_SHA256"),
            ({"SETUP_CONFIG_SHA256": "f" * 63}, "SETUP_CONFIG_SHA256"),
            ({"SETUP_CONFIG_SHA256": "0" * 64}, "SETUP_CONFIG_KEY"),
        ]
        for extra, needle in cases:
            rig = self.rig()
            proc = rig.run(**extra)
            self.assertFails(proc, rig, "inputs", needle)
            self.assertEqual(rig.calls("deploy"), [], needle)
            res = rig.result()
            self.assertEqual(res["state"], "SETUP-FAIL", needle)
            self.assertEqual(res["failedPhase"], "inputs")

    def test_a_missing_build_id_or_region_cannot_be_recorded_and_still_fails(self) -> None:
        for extra, needle in (({"CODEBUILD_BUILD_ID": ""}, "CODEBUILD_BUILD_ID"), ({"AWS_REGION": None}, "AWS_REGION")):
            rig = self.rig()
            proc = rig.run(**extra)
            self.assertEqual(proc.returncode, 1)
            self.assertTrue(last_line(proc).startswith("SETUP-FAIL inputs "), proc.stdout)
            self.assertIn(needle, last_line(proc))
            self.assertEqual([p for p in rig.s3.rglob("result.json")], [])

    def test_no_cdk_bootstrap(self) -> None:
        rig = self.rig()
        del rig.state["ssm"]["/cdk-bootstrap/hnb659fds/version"]
        proc = rig.run()
        self.assertFails(proc, rig, "inputs", "no CDK bootstrap")
        self.assertIn("setup.sh creates it", last_line(proc))
        self.assertEqual(rig.calls("deploy"), [])

    def test_no_credentials(self) -> None:
        rig = self.rig()
        rig.state["sts_fail"] = True
        proc = rig.run()
        self.assertFails(proc, rig, "inputs", "ExpiredToken")
        self.assertEqual([p for p in rig.s3.rglob("result.json")], [], "without an account there is no bucket to write to")

    def test_no_python_of_the_floor(self) -> None:
        rig = self.rig()
        fake = rig.tmp / "oldpython"
        fake.write_text("#!/bin/sh\nexit 1\n")
        fake.chmod(0o755)
        proc = rig.run(HAMH_PYTHON=str(fake))
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertIn("Python 3.11", last_line(proc))


class FetchCase(Base):
    def test_config_with_another_digest(self) -> None:
        rig = self.rig()
        rig.put(rig.config_key, rig.config + b" ")
        proc = rig.run()
        self.assertFails(proc, rig, "fetch", "config digest")
        self.assertEqual(rig.calls("toolchain"), [])

    def test_missing_config_and_missing_source(self) -> None:
        for key, needle in (("config", "SETUP_CONFIG_KEY"), ("source", "SETUP_SOURCE_KEY")):
            rig = self.rig()
            target = rig.config_key if key == "config" else rig.source_key
            (rig.s3 / BUCKET / target).unlink()
            proc = rig.run()
            self.assertFails(proc, rig, "fetch", needle)

    def test_source_whose_archive_names_another_commit(self) -> None:
        rig = self.rig()
        rig.put_source(comment=OTHER_COMMIT)
        proc = rig.run()
        self.assertFails(proc, rig, "fetch", "source commit")
        self.assertIn(OTHER_COMMIT, last_line(proc))

    def test_source_with_no_commit_in_the_archive(self) -> None:
        rig = self.rig()
        rig.put_source(comment="")
        proc = rig.run()
        self.assertFails(proc, rig, "fetch", "source commit")

    def test_an_archive_with_other_bytes_than_setup_sh_uploaded_is_not_run(self) -> None:
        """The zip comment is free text: an archive another principal of the account wrote under the key can name the right commit. What setup.sh sent in the build
        environment is the SHA-256 of the bytes it uploaded, and nothing of an archive with other bytes is used."""
        rig = self.rig()
        rig.put(rig.source_key, rig.source_bytes + b"another principal wrote these bytes after the comment")
        proc = rig.run()
        self.assertFails(proc, rig, "fetch", "SETUP_SOURCE_SHA256")
        self.assertEqual(rig.calls("toolchain"), [])
        self.assertEqual(rig.calls("deploy"), [])
        self.assertFalse((rig.work / "anchor.env").exists())

    def test_the_archive_read_by_a_version_is_hashed_too(self) -> None:
        rig = self.rig()
        proc = rig.run(CODEBUILD_SOURCE_VERSION="3HL4kqtJlcpXroDTDmJ.xUZgSJn2lQz", SETUP_SOURCE_SHA256="a" * 64)
        self.assertFails(proc, rig, "fetch", "SETUP_SOURCE_SHA256")

    def test_a_config_with_the_digest_of_the_key_and_other_bytes_is_not_used(self) -> None:
        rig = self.rig()
        proc = rig.run(SETUP_CONFIG_SHA256=sha12(rig.config) + "0" * 52)
        self.assertFails(proc, rig, "fetch", "SETUP_CONFIG_SHA256")
        self.assertEqual(rig.calls("toolchain"), [])


class AccountFileCase(Base):
    def run_with(self, doc: Dict[str, Any], **kw: Any):
        rig = self.rig()
        rig.config = json.dumps(doc, indent=1).encode()
        rig.put_config()
        return rig, rig.run(**kw)

    def test_account_that_is_not_the_callers(self) -> None:
        rig, proc = self.run_with(accounts_doc(account="999988887777"))
        self.assertFails(proc, rig, "account-file", "does not match")
        self.assertEqual(rig.calls("deploy"), [])

    def test_region_that_is_not_the_builds(self) -> None:
        rig, proc = self.run_with(accounts_doc(region="us-west-2"))
        self.assertFails(proc, rig, "account-file", "Region")

    def test_logging_mode_the_runner_cannot_write(self) -> None:
        rig, proc = self.run_with(accounts_doc(logging="enable-and-restore"))
        self.assertFails(proc, rig, "account-file", "invocationLogging")
        self.assertIn("read-only", last_line(proc))

    def test_a_file_the_repository_does_not_accept(self) -> None:
        doc = accounts_doc()
        doc["accounts"][0]["azs"].pop("drill")
        rig, proc = self.run_with(doc)
        self.assertFails(proc, rig, "account-file", "accounts file")

    def test_an_operator_the_repository_does_not_accept(self) -> None:
        # The optional `operator` of the entry is checked by the same loader as every other field, before anything is deployed.
        for bad in ({"kind": "role", "name": "Admin/other"}, {"kind": "group", "name": "admins"}, "role/Admin", {"kind": "user", "name": "alice", "arn": "x"}):
            with self.subTest(bad=bad):
                doc = accounts_doc()
                doc["accounts"][0]["operator"] = bad
                rig, proc = self.run_with(doc)
                self.assertFails(proc, rig, "account-file", "operator")
                self.assertEqual(rig.calls("deploy"), [])

    def test_a_file_that_names_an_operator_reaches_the_deploy_unchanged(self) -> None:
        doc = accounts_doc()
        doc["accounts"][0]["operator"] = {"kind": "role", "name": "AWSReservedSSO_AdministratorAccess_0123456789abcdef"}
        rig, proc = self.run_with(doc)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        deploy = rig.calls("deploy")[0]["env"]
        self.assertEqual(json.loads(Path(deploy["HAMH_ACCOUNTS_FILE"]).read_bytes()), doc)

    def test_node_smoke_failure(self) -> None:
        rig = self.rig()
        proc = rig.run(FAKE_NODE="fail")
        self.assertFails(proc, rig, "account-file", "node.sh")


class WorkFolderPathCase(Base):
    """The work folder is made a clean, physical path once, before any path is made from it.

    The buildspec names it `$CODEBUILD_SRC_DIR/../hamh-setup-work`, one level above the checkout. Every path the script derives from it (the account file, the profile
    config, the records, the temp and tool folders) reaches scripts/node.sh and the programs it starts, and a `..` in them made `node.sh` take the account file for a
    file of the checkout it mounts. The first build of a stranger's account failed there; the cases above name a work folder without a `..` and did not see it."""

    PATH_KEYS = ("HAMH_ACCOUNTS_FILE", "AWS_CONFIG_FILE", "DEPLOY_RECORD_DIR", "NODE_SH_TMP", "NODE_SH_TOOLS", "TMPDIR", "HAMH_TOOLS", "GOMOD_STORE_GO")

    def assert_paths_under(self, rig: Rig, clean: Path) -> None:
        deploy = rig.calls("deploy")[0]["env"]
        node = rig.calls("node")[0]["env"]
        gomod = rig.calls("gomod")[0]["env"]
        self.assertEqual(deploy["HAMH_ACCOUNTS_FILE"], f"{clean}/account/accounts.json")
        self.assertEqual(deploy["AWS_CONFIG_FILE"], f"{clean}/aws/config")
        self.assertEqual(deploy["DEPLOY_RECORD_DIR"], f"{clean}/records")
        self.assertEqual(node["NODE_SH_TMP"], f"{clean}/node-tmp")
        self.assertEqual(node["HAMH_ACCOUNTS_FILE"], f"{clean}/account/accounts.json")
        for env in (deploy, node, gomod):
            for key in self.PATH_KEYS:
                value = env.get(key)
                if value:
                    self.assertNotIn("/../", value + "/", f"{key}={value}")
                    self.assertNotIn("/./", value, f"{key}={value}")
        self.assertEqual(json.loads(Path(deploy["HAMH_ACCOUNTS_FILE"]).read_bytes()), accounts_doc())

    def test_trigger_present_the_buildspec_names_the_work_folder_with_a_dotdot(self) -> None:
        self.assertIn("RUNNER_BUILD_WORK=$CODEBUILD_SRC_DIR/../hamh-setup-work", sup.BUILDSPEC_PATH.read_text(), "TRIGGER ABSENT: the buildspec no longer names a work folder with a ..")

    def test_a_work_folder_named_with_a_dotdot_is_cleaned_before_any_path_is_made_from_it(self) -> None:
        rig = self.rig()
        named = f"{rig.tree}/../hamh-setup-work"
        proc = rig.run(RUNNER_BUILD_WORK=named)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        clean = (rig.tmp / "hamh-setup-work").resolve()
        self.assert_paths_under(rig, clean)
        # the buildspec reads the anchor through the name it gave: the same file
        self.assertTrue(Path(named, "anchor.env").is_file())
        self.assertEqual(Path(named, "anchor.env").read_bytes(), (clean / "anchor.env").read_bytes())

    def test_a_work_folder_that_is_a_link_is_the_folder_it_leads_to(self) -> None:
        rig = self.rig()
        (rig.tmp / "real-work").mkdir()
        (rig.tmp / "work-link").symlink_to(rig.tmp / "real-work")
        proc = rig.run(RUNNER_BUILD_WORK=str(rig.tmp / "work-link"))
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assert_paths_under(rig, (rig.tmp / "real-work").resolve())

    def test_a_relative_work_folder_becomes_an_absolute_one(self) -> None:
        rig = self.rig()
        proc = rig.run(RUNNER_BUILD_WORK="../relative-work")  # the run starts in the source folder
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assert_paths_under(rig, (rig.tmp / "relative-work").resolve())

    def test_the_real_node_sh_mounts_the_account_file_the_runner_lays_beside_the_checkout(self) -> None:
        """The two halves together, with the real scripts/node.sh in the made-up tree and a fake docker: the buildspec's work folder name, the account-file phase's
        smoke run of the CDK, and the mounts that docker is given. The container must be able to open the file HAMH_ACCOUNTS_FILE names."""
        rig = self.rig()
        for rel in ("scripts/node.sh", "scripts/lib/path-canon.sh", "scripts/lib/tools_lock.py", "tools.lock"):
            dst = rig.tree / rel
            dst.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(REPO / rel, dst)
            dst.chmod(0o755)
        rig.put_source()  # the archive the build extracts is made from the tree as it is now
        docker = rig.tmp / "docker"
        docker.write_text('#!/bin/bash\nfor a in "$@"; do printf "%s\\n" "$a"; done > "$FAKE_DOCKER_ARGS"\nprintf "%s\\n" "${HAMH_ACCOUNTS_FILE:-unset}" > "$FAKE_DOCKER_ARGS.account"\nexit 0\n')
        docker.chmod(0o755)
        creds = rig.tmp / "creds"
        creds.write_text('#!/bin/bash\nprintf \'{"Version":1,"AccessKeyId":"x","SecretAccessKey":"x","SessionToken":"x","Expiration":"2099-01-01T00:00:00Z"}\\n\'\n')
        creds.chmod(0o755)
        args_file = rig.tmp / "docker-args.txt"
        named = f"{rig.tree}/../hamh-setup-work"
        proc = rig.run(RUNNER_BUILD_WORK=named, NODE_SH_DOCKER_BIN=str(docker), NODE_SH_CREDS_CMD=str(creds), FAKE_DOCKER_ARGS=str(args_file))
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        clean = (rig.tmp / "hamh-setup-work").resolve()
        account = f"{clean}/account/accounts.json"
        args = args_file.read_text().splitlines()
        mounts = [args[i + 1] for i, a in enumerate(args) if a == "-v"]
        self.assertIn(f"{account}:{account}:ro", mounts, "the container is given the account file at its cleaned path")
        self.assertEqual(args_file.with_name(args_file.name + ".account").read_text().strip(), account, "the CDK app is told the cleaned path")
        self.assertIn(f"{rig.tree.resolve()}:{rig.tree.resolve()}", mounts)
        # TRIGGER PRESENT: the name the buildspec gives begins with the checkout's path, which is what fooled the string test
        self.assertTrue(named.startswith(str(rig.tree) + "/"))

    def test_a_work_folder_that_cannot_be_made_stops_in_inputs(self) -> None:
        rig = self.rig()
        (rig.tmp / "a-file").write_text("not a folder")
        proc = rig.run(RUNNER_BUILD_WORK=str(rig.tmp / "a-file" / "work"))
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertTrue(last_line(proc).startswith("SETUP-FAIL inputs cannot make the work folder"), last_line(proc))


class FailureCase(Base):
    def assertRecorded(self, rig: Rig, proc: subprocess.CompletedProcess, phase: str, needle: str) -> None:
        self.assertFails(proc, rig, phase, needle)
        res = rig.result()
        self.assertEqual((res["schema"], res["state"], res["failedPhase"]), ("1", "SETUP-FAIL", phase))
        self.assertIn(needle, res["detail"])
        self.assertEqual((res["sourceCommit"], res["stage"], res["buildId"]), (COMMIT, "direct", BUILD_ID))
        names = [p["name"] for p in res["phases"]]
        self.assertEqual(names, PHASES[:PHASES.index(phase) + 1])
        self.assertEqual(res["phases"][-1]["state"], "failed")
        self.assertEqual({p["state"] for p in res["phases"][:-1]}, {"ok"} if len(names) > 1 else set())
        lines = rig.phase_lines()
        self.assertEqual([ln["name"] for ln in lines], names)
        self.assertEqual(lines[-1]["state"], "failed")
        self.assertEqual(lines[-1]["detail"], res["detail"])

    def test_toolchain(self) -> None:
        rig = self.rig()
        self.assertRecorded(rig, rig.run(FAKE_TOOLCHAIN="fail"), "toolchain", "TOOLCHAIN FAIL rustup-init exit 1")
        self.assertEqual(rig.calls("deploy"), [])

    def test_deploy_reports_its_own_fail_line(self) -> None:
        rig = self.rig()
        self.assertRecorded(rig, rig.run(FAKE_DEPLOY="fail"), "deploy", "stack hamh-direct-network")
        self.assertEqual(rig.calls("gomod"), [])

    def test_deploy_that_ends_without_a_pass_line(self) -> None:
        rig = self.rig()
        self.assertRecorded(rig, rig.run(FAKE_DEPLOY="silent"), "deploy", "no DEPLOY-PASS line")
        rig = self.rig()
        self.assertRecorded(rig, rig.run(FAKE_DEPLOY="failrc"), "deploy", "exit 3")

    def test_a_pass_line_does_not_make_up_for_a_failing_exit_code(self) -> None:
        rig = self.rig()
        self.assertRecorded(rig, rig.run(FAKE_DEPLOY="passfail"), "deploy", "exit 1")
        self.assertEqual(rig.calls("gomod"), [])

    def test_gomod_fill_and_check(self) -> None:
        rig = self.rig()
        self.assertRecorded(rig, rig.run(FAKE_GOMOD="fill-fail"), "gomod", "GOMOD-FILL FAIL account")
        self.assertEqual(rig.calls("rules"), [])
        rig = self.rig()
        self.assertRecorded(rig, rig.run(FAKE_GOMOD="check-fail"), "gomod", "GOMOD-CHECK FAIL 2 objects missing")

    def test_the_fill_may_reuse_a_store_that_holds_the_same_modules(self) -> None:
        rig = self.rig()
        rig.run()
        fill = rig.calls("gomod")[0]["argv"]
        self.assertIn("--reuse-current", fill)

    def test_a_store_that_is_current_needs_neither_a_resolve_nor_the_full_check(self) -> None:
        rig = self.rig()
        proc = rig.run(FAKE_GOMOD="current")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual([c["argv"][0] for c in rig.calls("gomod")], ["fill"], "no check follows a fill that found the store current")
        phase = [p for p in rig.phase_lines() if p["name"] == "gomod"][0]
        self.assertEqual(phase["state"], "ok")
        self.assertIn("GOMOD-FILL CURRENT", phase["detail"])

    def test_a_fill_that_filled_is_still_followed_by_the_full_check(self) -> None:
        rig = self.rig()
        rig.run()
        self.assertEqual([c["argv"][0] for c in rig.calls("gomod")], ["fill", "check"])

    def test_scanner_rules_build_and_publish(self) -> None:
        rig = self.rig()
        self.assertRecorded(rig, rig.run(FAKE_RULES="build-fail"), "scanner-rules", "the commit is not the pin")
        self.assertEqual(rig.calls("client"), [])
        rig = self.rig()
        self.assertRecorded(rig, rig.run(FAKE_RULES="publish-fail"), "scanner-rules", "403")

    def test_client(self) -> None:
        rig = self.rig()
        self.assertRecorded(rig, rig.run(FAKE_CLIENT="fail"), "client", "cargo zigbuild failed")
        rig = self.rig()
        self.assertRecorded(rig, rig.run(FAKE_CLIENT="badjson"), "client", "JSON")
        rig = self.rig()
        self.assertRecorded(rig, rig.run(FAKE_CLIENT="nosums"), "client", "SHA256SUMS")
        rig = self.rig()
        self.assertRecorded(rig, rig.run(FAKE_CLIENT="missing"), "client", "exit 99")

    def test_the_operator_tool_is_checked_in_the_bucket_like_the_client(self) -> None:
        rig = self.rig()
        self.assertRecorded(rig, rig.run(FAKE_CLIENT="adminmissing"), "client", "ham-admin")
        rig = self.rig()
        self.assertRecorded(rig, rig.run(FAKE_CLIENT="adminwrong"), "client", "ham-admin")
        rig = self.rig()
        self.assertRecorded(rig, rig.run(FAKE_CLIENT="noadmin"), "client", "no valid client section")

    def test_the_client_phase_names_the_operator_tool_in_its_detail(self) -> None:
        rig = self.rig()
        self.assertEqual(rig.run().returncode, 0)
        detail = {p["name"]: p.get("detail", "") for p in rig.phase_lines()}["client"]
        self.assertIn("operator tool", detail)

    def test_client_script_that_is_not_there(self) -> None:
        rig = self.rig()
        (rig.tree / "scripts/setup/build-client.sh").unlink()
        rig.put_source()
        self.assertRecorded(rig, rig.run(), "client", "build-client.sh is missing")

    def test_outputs_without_the_control_stack(self) -> None:
        rig = self.rig()
        del rig.state["stacks"]["hamh-direct-control"]
        self.assertRecorded(rig, rig.run(), "outputs", "hamh-direct-control")

    def test_outputs_without_any_one_of_the_nine_stacks(self) -> None:
        for stack in ("network", "observability"):
            rig = self.rig()
            del rig.state["stacks"][f"hamh-direct-{stack}"]
            self.assertRecorded(rig, rig.run(), "outputs", f"hamh-direct-{stack}")

    def test_outputs_with_a_stack_that_is_not_complete(self) -> None:
        rig = self.rig()
        rig.state["stack_status"] = "ROLLBACK_COMPLETE"
        self.assertRecorded(rig, rig.run(), "outputs", "ROLLBACK_COMPLETE")

    def test_outputs_accepts_a_stack_whose_last_update_rolled_back(self) -> None:
        """A failed update leaves a stack in UPDATE_ROLLBACK_COMPLETE, and a later deploy that has nothing to change in that stack keeps the status (the stack holds the
        state before the failed update, which is the template). The outputs phase runs only after the deploy passed, so it reads that stack like any other."""
        rig = self.rig()
        rig.state["stack_statuses"] = {"hamh-direct-network": "UPDATE_ROLLBACK_COMPLETE"}
        proc = rig.run()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(last_line(proc), f"SETUP-PASS stage=direct digest={DEPLOY_DIGEST[:12]} endpoint={ENDPOINT}")
        self.assertEqual({ln["name"]: ln["state"] for ln in rig.phase_lines()}["outputs"], "ok")

    def test_outputs_refuses_every_state_that_is_not_complete_and_names_the_stack_and_the_state(self) -> None:
        for status in ("UPDATE_ROLLBACK_FAILED", "ROLLBACK_COMPLETE", "ROLLBACK_FAILED", "ROLLBACK_IN_PROGRESS", "CREATE_FAILED", "CREATE_IN_PROGRESS", "UPDATE_FAILED",
                       "UPDATE_IN_PROGRESS", "UPDATE_COMPLETE_CLEANUP_IN_PROGRESS", "UPDATE_ROLLBACK_IN_PROGRESS", "UPDATE_ROLLBACK_COMPLETE_CLEANUP_IN_PROGRESS",
                       "DELETE_IN_PROGRESS", "DELETE_FAILED", "DELETE_COMPLETE", "REVIEW_IN_PROGRESS", "IMPORT_IN_PROGRESS", "IMPORT_ROLLBACK_IN_PROGRESS",
                       "IMPORT_ROLLBACK_FAILED", "IMPORT_ROLLBACK_COMPLETE"):
            with self.subTest(status=status):
                rig = self.rig()
                rig.state["stack_statuses"] = {"hamh-direct-data": status}
                self.assertRecorded(rig, rig.run(), "outputs", f"the stack hamh-direct-data is {status}, not ")

    def test_outputs_without_a_v1_api_url(self) -> None:
        rig = self.rig()
        rig.state["stacks"]["hamh-direct-control"] = {"InternalApiUrl": "https://x.invalid"}
        self.assertRecorded(rig, rig.run(), "outputs", "V1ApiUrl")

    def test_one_failed_write_is_tried_again(self) -> None:
        rig = self.rig()
        # the fake aws refuses the first write under results/ (a throttle) and takes the next
        rig.state["fail_first_put"] = "results/"
        proc = rig.run()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        phase_puts = [c for c in rig.aws_calls() if c[:2] == ["s3", "cp"] and c[3] == f"s3://{BUCKET}/results/{BUILD_ID}/phases.jsonl"]
        self.assertEqual(len(phase_puts), len(PHASES) + 1, "one write per phase and one more for the retry")

    def test_a_result_that_cannot_be_written(self) -> None:
        rig = self.rig()
        rig.state["put_denied"] = ["results/"]
        proc = rig.run()
        self.assertEqual(proc.returncode, 1)
        self.assertTrue(last_line(proc).startswith("SETUP-FAIL "), proc.stdout)
        self.assertIn("results", last_line(proc))


class ActionCase(Base):
    """SETUP_ACTION: the runner builds and deploys (`deploy`, the default) or prints the CDK bootstrap template (`bootstrap-template`). It does not tear a stage down:
    `setup.sh --teardown` runs on the user's machine with the user's own credentials, because the runner role holds none of the permissions a teardown needs and a
    runner that could delete the stage would be one more way to act as the administrator. A build that is told to tear down stops where it reads its inputs and runs
    nothing, so it can never deploy the stage the user asked to remove."""

    def test_a_deploy_names_its_action_in_the_result(self) -> None:
        for extra in ({}, {"SETUP_ACTION": "deploy"}, {"SETUP_ACTION": ""}):
            with self.subTest(extra=extra):
                rig = self.rig()
                proc = rig.run(**extra)
                self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
                self.assertEqual(rig.result()["action"], "deploy")
                self.assertEqual(rig.calls("teardown"), [])

    def test_a_teardown_is_not_an_action_of_the_runner(self) -> None:
        rig = self.rig()
        proc = rig.run(SETUP_ACTION="teardown")
        self.assertFails(proc, rig, "inputs", "SETUP_ACTION must be deploy or bootstrap-template, not 'teardown'")
        for name in ("deploy", "gomod", "rules", "client", "node", "teardown", "toolchain"):
            self.assertEqual(rig.calls(name), [], name)

    def test_a_template_build_leaves_its_hash_too(self) -> None:
        rig = self.rig()
        self.assertEqual(rig.run(SETUP_ACTION="bootstrap-template").returncode, 0)
        self.assertEqual((rig.work / "anchor.env").read_text(), f"export SETUP_RESULT_SHA256={hashlib.sha256(rig.get(f'results/{BUILD_ID}/result.json')).hexdigest()}\n")

    def test_an_action_that_is_neither_deploy_nor_the_template_stops_in_inputs(self) -> None:
        for value in ("wipe", "Teardown", "teardown", "deploy teardown"):
            with self.subTest(value=value):
                rig = self.rig()
                proc = rig.run(SETUP_ACTION=value)
                self.assertFails(proc, rig, "inputs", "SETUP_ACTION must be deploy or bootstrap-template")
                for name in ("deploy", "teardown", "toolchain"):
                    self.assertEqual(rig.calls(name), [], name)


class TemplateActionCase(Base):
    """SETUP_ACTION=bootstrap-template: a fresh account has no CDK bootstrap, the runner role cannot make one, and the user's desktop has no Node. The runner prints the
    template with the pinned CDK CLI (`cdk bootstrap --show-template` through scripts/node.sh), checks it against the pin of the repository, and puts it in the setup bucket;
    setup.sh deploys it with the user's credentials."""

    PHASES = ["inputs", "fetch", "toolchain", "account-file", "template"]

    def test_the_pinned_template_goes_to_the_bucket_and_into_the_result(self) -> None:
        rig = self.rig()
        proc = rig.run(SETUP_ACTION="bootstrap-template")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(last_line(proc), f"SETUP-PASS stage=direct action=bootstrap-template template={TEMPLATE_SHA[:12]}")
        self.assertEqual(rig.get(TEMPLATE_KEY), TEMPLATE_BYTES)
        res = rig.result()
        self.assertEqual((res["schema"], res["state"], res["action"]), ("1", "SETUP-PASS", "bootstrap-template"))
        self.assertEqual(res["bootstrapTemplate"], {"key": TEMPLATE_KEY, "sha256": TEMPLATE_SHA, "bytes": len(TEMPLATE_BYTES)})
        self.assertEqual((res["endpoint"], res["stackOutputs"], res["client"], res["failedPhase"], res["deploymentDigest"]), ("", {}, {}, None, ""))
        self.assertEqual([p["name"] for p in res["phases"]], self.PHASES)
        self.assertEqual([ln["name"] for ln in rig.phase_lines()], self.PHASES)

    def test_only_the_cdk_runs_and_it_runs_once(self) -> None:
        rig = self.rig()
        self.assertEqual(rig.run(SETUP_ACTION="bootstrap-template").returncode, 0)
        nodes = rig.calls("node")
        self.assertEqual([c["argv"] for c in nodes], [["cdk", "bootstrap", "--show-template"]], "the smoke run of the CDK is for a deploy; this build prints the template")
        for name in ("deploy", "gomod", "rules", "client", "teardown"):
            self.assertEqual(rig.calls(name), [], name)

    def test_the_toolchain_is_the_template_one_and_the_account_file_is_laid(self) -> None:
        rig = self.rig()
        self.assertEqual(rig.run(SETUP_ACTION="bootstrap-template").returncode, 0)
        self.assertEqual(rig.calls("toolchain")[0]["env"]["RUNNER_TOOLCHAIN_ONLY"], "template")
        node = rig.calls("node")[0]
        self.assertEqual(node["env"]["HAMH_ACCOUNTS_FILE"], str(rig.work / "account" / "accounts.json"))
        self.assertEqual(node["env"]["AWS_CONFIG_FILE"], str(rig.work / "aws" / "config"))

    def test_a_deploy_keeps_the_full_toolchain(self) -> None:
        rig = self.rig()
        self.assertEqual(rig.run().returncode, 0)
        self.assertIsNone(rig.calls("toolchain")[0]["env"]["RUNNER_TOOLCHAIN_ONLY"])

    def test_it_needs_no_cdk_bootstrap(self) -> None:
        rig = self.rig()
        del rig.state["ssm"]["/cdk-bootstrap/hnb659fds/version"]
        proc = rig.run(SETUP_ACTION="bootstrap-template")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)

    def test_a_template_that_is_not_the_pinned_one_fails_and_is_not_uploaded(self) -> None:
        rig = self.rig()
        proc = rig.run(SETUP_ACTION="bootstrap-template", FAKE_TEMPLATE="other")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        line = last_line(proc)
        self.assertTrue(line.startswith("SETUP-FAIL template "), line)
        self.assertIn("SHA-256", line)
        self.assertIn(TEMPLATE_SHA, line, "the line names the SHA-256 the repository pins")
        self.assertFalse((rig.s3 / BUCKET / "bootstrap").exists(), "a template that fails the check is not put in the bucket")
        res = rig.result()
        self.assertEqual((res["state"], res["action"], res["failedPhase"]), ("SETUP-FAIL", "bootstrap-template", "template"))
        self.assertNotIn("bootstrapTemplate", res)

    def test_a_template_over_the_deploy_limit_fails_before_the_hash_is_compared(self) -> None:
        rig = self.rig()
        proc = rig.run(SETUP_ACTION="bootstrap-template", FAKE_TEMPLATE="huge")
        self.assertEqual(proc.returncode, 1)
        self.assertTrue(last_line(proc).startswith("SETUP-FAIL template "), last_line(proc))
        self.assertIn("51,200", last_line(proc))

    def test_an_empty_template_fails(self) -> None:
        rig = self.rig()
        proc = rig.run(SETUP_ACTION="bootstrap-template", FAKE_TEMPLATE="empty")
        self.assertEqual(proc.returncode, 1)
        self.assertTrue(last_line(proc).startswith("SETUP-FAIL template "), last_line(proc))

    def test_the_cdk_failing_fails_the_phase_with_its_last_line(self) -> None:
        rig = self.rig()
        proc = rig.run(SETUP_ACTION="bootstrap-template", FAKE_TEMPLATE="fail")
        self.assertEqual(proc.returncode, 1)
        self.assertTrue(last_line(proc).startswith("SETUP-FAIL template "), last_line(proc))
        self.assertIn("Unable to resolve AWS account", last_line(proc))
        self.assertFalse((rig.s3 / BUCKET / "bootstrap").exists())

    def test_a_clone_without_the_pin_stops_before_the_cdk_runs(self) -> None:
        rig = self.rig(pin=False)
        proc = rig.run(SETUP_ACTION="bootstrap-template")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertTrue(last_line(proc).startswith("SETUP-FAIL template "), last_line(proc))
        self.assertIn("cdk-bootstrap-template.pin", last_line(proc))
        self.assertEqual(rig.calls("node"), [])

    def test_it_is_never_skipped_and_a_second_build_makes_the_same_object(self) -> None:
        rig = self.rig()
        self.assertEqual(rig.run(SETUP_ACTION="bootstrap-template").returncode, 0)
        other = "hamh-setup-runner:99999999-2222-3333-4444-555555555555"
        self.assertEqual(rig.run(SETUP_ACTION="bootstrap-template", CODEBUILD_BUILD_ID=other).returncode, 0)
        self.assertEqual(len(rig.calls("node")), 2)
        self.assertEqual(rig.get(TEMPLATE_KEY), TEMPLATE_BYTES)


class GuardCase(Base):
    """The deploy guard (no runtime update while runs are active) ends the deploy with `DEPLOY-FAIL guard ...`; setup.sh reads failedPhase `guard` as exit 7."""

    def test_the_guard_is_its_own_failed_phase(self) -> None:
        rig = self.rig()
        proc = rig.run(FAKE_DEPLOY="guard")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertEqual(last_line(proc), "SETUP-FAIL guard 2 running run(s) on hamh-direct-run pin the worker runtime: run-a, run-b")
        res = rig.result()
        self.assertEqual((res["state"], res["failedPhase"], res["action"]), ("SETUP-FAIL", "guard", "deploy"))
        self.assertIn("2 running run(s)", res["detail"])
        names = [p["name"] for p in res["phases"]]
        self.assertEqual(names, ["inputs", "fetch", "toolchain", "account-file", "guard"])
        self.assertEqual(res["phases"][-1]["state"], "failed")
        self.assertEqual(rig.calls("gomod"), [])

    def test_a_deploy_failure_that_is_not_the_guard_stays_a_deploy_failure(self) -> None:
        rig = self.rig()
        proc = rig.run(FAKE_DEPLOY="fail")
        self.assertEqual(rig.result()["failedPhase"], "deploy", proc.stdout)


class RerunCase(Base):
    def passed_once(self, **kw: Any) -> Rig:
        rig = self.rig(**kw)
        proc = rig.run()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        rig.mark_deployed()  # the deploy of the first run left its tree and its account file in SSM, as deploy.sh does
        return rig

    def second(self, rig: Rig, **extra: str) -> subprocess.CompletedProcess:
        rig.build_id = "hamh-setup-runner:99999999-2222-3333-4444-555555555555"
        rig.calls_file.unlink()
        return rig.run(CODEBUILD_BUILD_ID=rig.build_id, **extra)

    def test_a_second_run_skips_what_is_done_and_says_so(self) -> None:
        rig = self.passed_once()
        first_id = BUILD_ID
        proc = self.second(rig)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(rig.calls("deploy"), [], "deploy ran again")
        self.assertEqual([c["argv"][0] for c in rig.calls("rules")], ["build", "publish"], "the rules are built and published again: a version that is there is left alone by the tool")
        self.assertEqual([c["argv"] for c in rig.calls("gomod")], [["check", "--stage", "direct"]], "only the full check of the store, to see it is still filled")
        self.assertEqual(len(rig.calls("toolchain")), 1, "the toolchain is installed in every build")
        self.assertEqual(len(rig.calls("client")), 1, "the client is built in every build: it is the file that is installed and run on the user's machine")
        lines = {ln["name"]: ln for ln in rig.phase_lines(rig.build_id)}
        self.assertEqual([ln["state"] for ln in lines.values()], ["ok", "ok", "ok", "ok", "skipped", "skipped", "ok", "ok", "ok"])
        self.assertEqual(lines["deploy"]["from"], first_id)
        res = rig.result(rig.build_id)
        self.assertEqual(res["state"], "SETUP-PASS")
        self.assertEqual(res["client"]["linux-amd64"]["key"], f"client/{rig.build_id}/linux-amd64/ham", "the client is the one this build made")
        self.assertEqual(res["endpoint"], ENDPOINT)
        self.assertEqual(last_line(proc), f"SETUP-PASS stage=direct digest={DEPLOY_DIGEST[:12]} endpoint={ENDPOINT}")

    def test_a_record_in_the_bucket_alone_never_skips_the_deploy(self) -> None:
        """Another principal of the account could write results/<id>/phases.jsonl with a deploy `ok` for this commit, this account file and the digest the account
        holds. The account must also say it runs this source tree and this account file."""
        for what, key, value in (("no tree recorded", "/hamh/direct/deployed-source-tree", None), ("another tree", "/hamh/direct/deployed-source-tree", "0" * 64),
                                 ("no account file recorded", "/hamh/direct/deployment-config-digest", None), ("another account file", "/hamh/direct/deployment-config-digest", "1" * 64),
                                 ("the committed file", "/hamh/direct/deployment-config-digest", "committed")):
            with self.subTest(what=what):
                rig = self.passed_once()
                if value is None:
                    del rig.state["ssm"][key]
                else:
                    rig.state["ssm"][key] = value
                proc = self.second(rig)
                self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
                self.assertEqual(len(rig.calls("deploy")), 1, what)

    def test_the_deploy_is_skipped_only_when_the_account_says_the_same(self) -> None:
        rig = self.passed_once()
        proc = self.second(rig)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(rig.calls("deploy"), [])
        reads = [c[c.index("--name") + 1] for c in rig.aws_calls() if c[:2] == ["ssm", "get-parameter"]]
        for name in ("deployment-digest", "deployed-source-tree", "deployment-config-digest"):
            self.assertIn(f"/hamh/direct/{name}", reads)

    def test_the_client_is_never_skipped_whatever_an_earlier_record_says(self) -> None:
        rig = self.passed_once()
        first = rig.get(f"results/{BUILD_ID}/phases.jsonl").decode().splitlines()
        forged = [json.loads(line) for line in first if json.loads(line)["name"] == "deploy"]
        forged[0].update({"name": "client", "state": "ok", "binary": "widget"})
        rig.put("results/forged/phases.jsonl", ("\n".join(json.dumps(x) for x in forged) + "\n").encode())
        proc = self.second(rig)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(len(rig.calls("client")), 1)
        self.assertEqual({ln["name"]: ln["state"] for ln in rig.phase_lines(rig.build_id)}["client"], "ok")

    def test_a_run_after_a_failure_resumes_at_the_failed_phase(self) -> None:
        rig = self.rig()
        proc = rig.run(FAKE_CLIENT="fail")
        self.assertEqual(proc.returncode, 1)
        rig.mark_deployed()
        proc = self.second(rig)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(rig.calls("deploy"), [])
        self.assertEqual([c["argv"][0] for c in rig.calls("gomod")], ["check"])
        self.assertEqual(len(rig.calls("client")), 1)

    def test_a_failed_deploy_is_run_again(self) -> None:
        rig = self.rig()
        rig.run(FAKE_DEPLOY="fail")
        proc = self.second(rig)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(len(rig.calls("deploy")), 1)

    def test_another_commit_or_configuration_skips_nothing(self) -> None:
        rig = self.passed_once()
        rig.config = json.dumps(accounts_doc(profile="another-profile"), indent=1).encode()
        rig.put_config()
        proc = self.second(rig)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(len(rig.calls("deploy")), 1)
        rig2 = self.passed_once()
        rig2.commit = OTHER_COMMIT
        rig2.put_source()
        proc = self.second(rig2)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(len(rig2.calls("deploy")), 1)

    def test_a_deployment_that_is_gone_is_deployed_again(self) -> None:
        rig = self.passed_once()
        rig.state["ssm"]["/hamh/direct/deployment-digest"] = "ffffffffffff" + DEPLOY_DIGEST[12:]
        proc = self.second(rig)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(len(rig.calls("deploy")), 1, "the digest the account holds is not the one the earlier build recorded")

    def test_an_empty_go_module_store_is_filled_again(self) -> None:
        rig = self.passed_once()
        Path(str(rig.state_file) + ".filled").unlink()  # the stand-in store is empty again
        proc = self.second(rig, FAKE_GOMOD="store-empty")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(rig.calls("deploy"), [])
        self.assertEqual([c["argv"][0] for c in rig.calls("gomod")], ["check", "fill", "check"])

    def test_a_deploy_whose_stack_is_not_complete_is_deployed_again(self) -> None:
        rig = self.passed_once()
        rig.state["stack_status"] = "UPDATE_ROLLBACK_COMPLETE"
        proc = self.second(rig, FAKE_DEPLOY_HEAL="hamh-direct-control")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(len(rig.calls("deploy")), 1)

    def test_a_retry_after_a_rolled_back_update_deploys_again_and_reads_the_outputs(self) -> None:
        """The update that rolled back is not the deploy the record names, so the deploy runs again; it has nothing to change in the stack, which keeps its status, and the
        outputs phase of the same build then reads it."""
        rig = self.passed_once()
        rig.state["stack_statuses"] = {"hamh-direct-network": "UPDATE_ROLLBACK_COMPLETE"}
        proc = self.second(rig)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(len(rig.calls("deploy")), 1, "a stack that rolled back since the recorded deploy is a deploy that cannot be skipped")
        self.assertEqual(last_line(proc), f"SETUP-PASS stage=direct digest={DEPLOY_DIGEST[:12]} endpoint={ENDPOINT}")

    def test_a_deploy_with_one_of_the_nine_stacks_gone_is_deployed_again(self) -> None:
        rig = self.passed_once()
        del rig.state["stacks"]["hamh-direct-observability"]
        proc = self.second(rig, FAKE_DEPLOY_HEAL="hamh-direct-observability")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(len(rig.calls("deploy")), 1, "the control stack alone being complete is not a finished deploy")

    def test_a_phases_file_under_a_key_with_a_space_is_read(self) -> None:
        rig = self.passed_once()
        data = rig.get(f"results/{BUILD_ID}/phases.jsonl")
        (rig.s3 / BUCKET / "results" / BUILD_ID / "phases.jsonl").unlink()
        rig.put("results/a copy/phases.jsonl", data)
        proc = self.second(rig)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(rig.calls("deploy"), [], "the record is read whatever the key looks like")

    def test_another_binary_name_builds_the_client_again(self) -> None:
        rig = self.passed_once()
        proc = self.second(rig, SETUP_BINARY_NAME="ham")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(rig.calls("deploy"), [])
        self.assertEqual(len(rig.calls("client")), 1)
        self.assertEqual(rig.calls("client")[0]["env"]["SETUP_BINARY_NAME"], "ham")

    def test_skipping_can_be_turned_off(self) -> None:
        rig = self.passed_once()
        proc = self.second(rig, RUNNER_BUILD_NO_SKIP="1")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        for name in ("deploy", "client"):
            self.assertEqual(len(rig.calls(name)), 1)


class StateHelperCase(Base):
    def run_helper(self, *args: str, stdin: str = "") -> subprocess.CompletedProcess:
        return subprocess.run([sys.executable, "-I", "-B", str(sup.STATE_HELPER), *args], input=stdin, capture_output=True, text=True, timeout=limit(60))  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do

    def test_json_get(self) -> None:
        proc = self.run_helper("json-get", "Parameter.Value", stdin='{"Parameter": {"Value": "30"}}')
        self.assertEqual((proc.returncode, proc.stdout), (0, "30\n"))
        proc = self.run_helper("json-get", "Stacks.0.Outputs", stdin='{"Stacks": [{"Outputs": []}]}')
        self.assertEqual((proc.returncode, proc.stdout), (0, "[]\n"))
        proc = self.run_helper("json-get", "Parameter.Missing", stdin='{"Parameter": {}}')
        self.assertEqual(proc.returncode, 1)

    def test_phase_line_is_one_json_line_and_cleans_the_detail(self) -> None:
        proc = self.run_helper("phase-line", "--build-id", "b", "--stage", "direct", "--commit", "c" * 40, "--digest", "d" * 12, "--name", "deploy",
                               "--state", "failed", "--seconds", "7", "--detail", "line one\nline two\x07 " + "x" * 700)
        self.assertEqual(proc.returncode, 0)
        self.assertEqual(proc.stdout.count("\n"), 1)
        doc = json.loads(proc.stdout)
        self.assertEqual((doc["name"], doc["state"], doc["seconds"], doc["schema"]), ("deploy", "failed", 7, "1"))
        self.assertLessEqual(len(doc["detail"]), 500)
        self.assertNotIn("\x07", doc["detail"])
        self.assertNotIn("\n", doc["detail"])

    def test_completed_lists_the_phases_of_earlier_builds_for_this_commit_and_configuration(self) -> None:
        def line(build, name, state, commit="c" * 40, digest="d" * 12, **extra):
            return json.dumps(dict(schema="1", buildId=build, stage="direct", sourceCommit=commit, configDigest=digest, name=name, state=state, seconds=1, detail="", **extra))
        d = Path(tempfile.mkdtemp(prefix="runner-state-"))
        self.addCleanup(shutil.rmtree, d, True)
        (d / "a.jsonl").write_text("\n".join([line("a", "deploy", "ok", deploymentDigest="1" * 12), line("a", "gomod", "failed")]) + "\n")
        (d / "b.jsonl").write_text("\n".join([line("b", "deploy", "ok", commit="e" * 40), line("b", "client", "ok", binary="ham")]) + "\n")
        (d / "c.jsonl").write_text(line("c", "deploy", "skipped", **{"from": "a"}) + "\n")
        proc = self.run_helper("completed", "--stage", "direct", "--commit", "c" * 40, "--digest", "d" * 12, str(d / "a.jsonl"), str(d / "b.jsonl"), str(d / "c.jsonl"))
        self.assertEqual(proc.returncode, 0, proc.stderr)
        rows = [r.split("\t") for r in proc.stdout.splitlines()]
        self.assertEqual(sorted(rows), [["client", "b", "", "ham"], ["deploy", "a", "1" * 12, ""]],
                         "a failed phase, another commit and a skipped record do not count; the build that did the work and its deployment digest are named")

    def test_client_check_prints_platform_key_bytes_and_sha256(self) -> None:
        d = Path(tempfile.mkdtemp(prefix="runner-state-"))
        self.addCleanup(shutil.rmtree, d, True)
        doc = {"linux-amd64": {"key": "client/b/linux-amd64/ham", "sha256": "a" * 64, "bytes": 7,
                               "admin": {"key": "client/b/linux-amd64/ham-admin", "sha256": "c" * 64, "bytes": 3}},
               "darwin-arm64": {"key": "client/b/darwin-arm64/ham", "sha256": "b" * 64, "bytes": 9,
                                "admin": {"key": "client/b/darwin-arm64/ham-admin", "sha256": "d" * 64, "bytes": 5}}}
        (d / "c.json").write_text(json.dumps(doc))
        proc = self.run_helper("client-check", str(d / "c.json"))
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(proc.stdout.splitlines(), ["darwin-arm64\tclient/b/darwin-arm64/ham\t9\t" + "b" * 64, "linux-amd64\tclient/b/linux-amd64/ham\t7\t" + "a" * 64])
        admin = self.run_helper("client-check", "--part", "admin", str(d / "c.json"))
        self.assertEqual(admin.returncode, 0, admin.stderr)
        self.assertEqual(admin.stdout.splitlines(), ["darwin-arm64\tclient/b/darwin-arm64/ham-admin\t5\t" + "d" * 64, "linux-amd64\tclient/b/linux-amd64/ham-admin\t3\t" + "c" * 64])

    def test_client_check_refuses_a_platform_without_the_operator_tool_or_with_a_bad_one(self) -> None:
        td = tempfile.TemporaryDirectory(prefix="runner-state-")
        self.addCleanup(td.cleanup)
        d = Path(td.name)
        good = {"key": "client/b/linux-amd64/ham", "sha256": "a" * 64, "bytes": 7, "admin": {"key": "client/b/linux-amd64/ham-admin", "sha256": "c" * 64, "bytes": 3}}
        cases = {
            "no admin": {k: v for k, v in good.items() if k != "admin"},
            "admin sha256 too short": {**good, "admin": {**good["admin"], "sha256": "c" * 63}},
            "admin key outside client/": {**good, "admin": {**good["admin"], "key": "results/b/ham-admin"}},
            "admin bytes not a number": {**good, "admin": {**good["admin"], "bytes": "3"}},
            "admin with an extra member": {**good, "admin": {**good["admin"], "mode": "0755"}},
            "admin not an object": {**good, "admin": "client/b/linux-amd64/ham-admin"},
            "another extra member": {**good, "extra": 1},
        }
        for name, entry in cases.items():
            with self.subTest(name):
                (d / "c.json").write_text(json.dumps({"linux-amd64": entry}))
                for part in ([], ["--part", "admin"]):
                    proc = self.run_helper("client-check", *part, str(d / "c.json"))
                    self.assertEqual((proc.returncode, proc.stdout), (1, ""), proc.stderr)
                    self.assertIn("linux-amd64", proc.stderr)

    def test_a_line_that_is_not_json_is_ignored(self) -> None:
        d = Path(tempfile.mkdtemp(prefix="runner-state-"))
        self.addCleanup(shutil.rmtree, d, True)
        (d / "a.jsonl").write_text("not json\n")
        proc = self.run_helper("completed", "--stage", "direct", "--commit", "c" * 40, "--digest", "d" * 12, str(d / "a.jsonl"))
        self.assertEqual((proc.returncode, proc.stdout), (0, ""))


if __name__ == "__main__":
    unittest.main()
