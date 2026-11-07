"""Shared support for the tests of the full setup.

`FullBase` extends `SetupBase` of test_setup_script.py with what the full path needs and the client-only path does not:

- a git checkout of the repository's own `setup.sh` and `scripts/setup/` (plus stand-in templates), because the full path archives `HEAD` and
  reads its helper files from the folder it runs from;
- the stand-in AWS CLI of setup_fake_aws.py first on PATH, a scenario file the tests rewrite between runs and a log of every call;
- a PATH that also holds the few extra system tools the full path uses (date, sleep).

Nothing here is a test. Standard library only.
"""

from __future__ import annotations

import json
import os
import platform
import shutil
import subprocess  # nosec B404: tests start git and the scripts under test with argument lists, never a shell
from pathlib import Path

import setup_fake_aws
from setup_waits import GroupProcess, limit
from test_setup_script import BASH, REPO, SCRIPT, STUB_HAM, SYSTEM_TOOLS, SetupBase

FULL_TOOLS = SYSTEM_TOOLS + ["date", "sleep", "uniq", "mktemp", "rm", "rmdir"]  # the last three are used by scripts/setup/empty-bucket.sh for its own temporary files
# The key of the client the runner builds for the machine the tests run on (cl_platform of scripts/setup/client.sh gives the same).
HOST_PLATFORM = {("Darwin", "arm64"): "darwin-arm64", ("Linux", "x86_64"): "linux-amd64", ("Linux", "aarch64"): "linux-arm64",
                 ("Linux", "arm64"): "linux-arm64"}.get((platform.system(), platform.machine()), "linux-amd64")
# The operator tool the runner builds beside the client: it logs its calls like the stand-in client and answers --version.
STUB_ADMIN = "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"${STUB_LOG:?}/ham-admin.log\"\necho 'ham-admin v9.99'\n"
TEMPLATE_RUNNER = "AWSTemplateFormatVersion: '2010-09-09'\nDescription: stand-in for infra/setup-runner.yaml\nResources: {}\n"

# The calls that change the account (the tests assert that a refused or cancelled setup made none).
MUTATING = {("cloudformation", "deploy"), ("cloudformation", "delete-stack"), ("s3api", "put-object"), ("s3api", "delete-objects"),
            ("codebuild", "start-build"), ("s3", "rm")}


def normalize(args: list) -> list:
    """The call without the global options --profile and --region (and their values)."""
    out, skip = [], False
    for a in args:
        if skip:
            skip = False
        elif a in ("--profile", "--region"):
            skip = True
        else:
            out.append(a)
    return out


def is_mutating(args: list) -> bool:
    n = normalize(args)
    if tuple(n[:2]) in MUTATING:
        return True
    if n[:2] == ["s3", "cp"] and len(n) >= 4 and n[3].startswith("s3://"):
        return True
    return False


def golden_accounts(region: str = "us-east-1", account: str | None = None, operator: tuple[str, str] | None = ("user", "setup-test")) -> bytes:
    """The account file the generator must write for the stand-in account (the fixture holds {account} so that no account id is committed).
    `operator` is the (kind, name) of the identity the stand-in's `sts get-caller-identity` answers with (the IAM user setup-test), which the file names as its
    operator; None is the file of a generator that was not given the caller (the entry has no `operator`)."""
    text = (REPO / "fixtures" / "setup" / "aws" / f"accounts-expected-{region}.json").read_text()
    if operator is not None:
        marker = '      "role": "active",\n'
        assert marker in text
        text = text.replace(marker, marker + f'      "operator": {{"kind": "{operator[0]}", "name": "{operator[1]}"}},\n', 1)
    return text.replace("{account}", account or setup_fake_aws.ACCOUNT).encode()


class FullBase(SetupBase):
    """A fake home, a git checkout of setup.sh and the helpers, and the stand-in AWS CLI with its world."""

    ACCOUNT = setup_fake_aws.ACCOUNT
    ENDPOINT = setup_fake_aws.ENDPOINT
    REGION = "us-east-1"
    ARGS = ("--full", "--profile", "work", "--region", "us-east-1", "--yes", "--acknowledge-costs")

    def link_system_tools(self, without: tuple = ()) -> None:
        self.sysbin.mkdir(exist_ok=True)
        for name in FULL_TOOLS:
            link = self.sysbin / name
            if name in without:
                if link.is_symlink():
                    link.unlink()
                continue
            real = shutil.which(name)
            if real and not link.exists():
                link.symlink_to(real)

    def setUp(self) -> None:
        super().setUp()
        real_git = shutil.which("git")
        self.assertTrue(real_git, "the full-setup tests need a real git")
        (self.stub / "git").unlink()
        (self.stub / "git").symlink_to(real_git)
        setup_fake_aws.install(self.stub)
        self.state = self.tmp / "awsstate"
        self.scenario_path = self.tmp / "scenario.json"
        self.client_file = self.tmp / "client-body"
        self.client_file.write_text(STUB_HAM)
        self.admin_file = self.tmp / "admin-body"
        self.admin_file.write_text(STUB_ADMIN)
        self.scenario: dict = {}
        self.set_scenario(client_file=str(self.client_file), admin_file=str(self.admin_file), bootstrap_present=True)
        self.repo = self.make_checkout()
        self.bucket = f"hamh-setup-{self.ACCOUNT}-{self.REGION}"

    # ---- the checkout ------------------------------------------------------------------------------
    def git(self, *args: str, cwd: Path | None = None) -> str:
        env = dict(os.environ, GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1", GIT_AUTHOR_NAME="Setup Test", GIT_AUTHOR_EMAIL="setup@test.invalid",
                   GIT_COMMITTER_NAME="Setup Test", GIT_COMMITTER_EMAIL="setup@test.invalid")
        p = subprocess.run(["git", *args], cwd=str(cwd or self.repo), capture_output=True, text=True, env=env, timeout=limit(60))  # nosec B603: argument list, no shell; B607: bare name of git looked up on PATH as the scripts under test do
        self.assertEqual(p.returncode, 0, p.stderr)
        return p.stdout.strip()

    def make_checkout(self) -> Path:
        repo = self.tmp / "checkout"
        (repo / "scripts").mkdir(parents=True)
        (repo / "infra").mkdir()
        shutil.copy2(SCRIPT, repo / "setup.sh")
        helpers = REPO / "scripts" / "setup"
        if helpers.is_dir():
            shutil.copytree(helpers, repo / "scripts" / "setup")
        (repo / "infra" / "setup-runner.yaml").write_text(TEMPLATE_RUNNER)
        (repo / "engine" / "policy").mkdir(parents=True)
        (repo / "engine" / "policy" / "cdk-bootstrap-template.pin").write_text(f"# stand-in pin\nawsCdk=2.0.0\nsha256={setup_fake_aws.BOOTSTRAP_SHA}\n")
        (repo / "README.md").write_text("stand-in repository\n")
        self.git("init", "-q", cwd=repo)
        self.git("add", "-A", cwd=repo)
        self.git("commit", "-q", "-m", "stand-in checkout", cwd=repo)
        return repo

    def commit_more(self, name: str = "CHANGES.md") -> str:
        (self.repo / name).write_text("another commit\n")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", f"add {name}")
        return self.commit()

    def commit(self) -> str:
        return self.git("rev-parse", "HEAD")

    # ---- the stand-in AWS CLI ----------------------------------------------------------------------
    def set_scenario(self, **kw) -> None:
        self.scenario.update(kw)
        self.scenario_path.write_text(json.dumps(self.scenario))

    def env(self, **extra: str) -> dict:
        e = super().env(FAKE_AWS_STATE=str(self.state), FAKE_AWS_SCENARIO=str(self.scenario_path), HAMH_SETUP_POLL_SECONDS="0",
                        HAMH_SETUP_POLL_RETRIES="3", HAMH_SETUP_RETRY_SECONDS="0", GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1")
        e.update(extra)
        return e

    def run_full(self, *args: str, **env: str) -> subprocess.CompletedProcess:
        return self.run_setup(*args, script=self.repo / "setup.sh", **env)

    def popen(self, *args: str, **env: str) -> GroupProcess:
        """`setup.sh` with `args` in a process group of its own. A thread reads its output, and the group is ended when the test ends, whatever the outcome."""
        return GroupProcess([BASH, str(self.repo / "setup.sh"), *args], env=self.env(**env), cwd=str(self.tmp), case=self, name="setup.sh")

    def follow_started(self, proc: GroupProcess, seconds: float = 40.0) -> float:
        """Wait until setup.sh has asked CodeBuild about the build it started: from then on its Ctrl-C handler is in place. If setup.sh ended first, or the limit runs out,
        the test fails and the group is ended; a signal is never sent to a setup.sh that was not ready for it."""

        def looked() -> bool:
            try:
                return any(c[:2] == ["codebuild", "batch-get-builds"] for c in self.normalized_calls())
            except ValueError:  # the stand-in was writing its log line while it was read
                return False

        return proc.wait_ready(looked, seconds, "setup.sh to look at the build it started (its first codebuild batch-get-builds call)")

    def world(self) -> dict:
        p = self.state / "world.json"
        return json.loads(p.read_text()) if p.is_file() else {}

    def open_world(self) -> setup_fake_aws.World:
        self.state.mkdir(parents=True, exist_ok=True)
        return setup_fake_aws.World(self.state, self.scenario_path)

    def s3_exists(self, key: str, bucket: str | None = None) -> bool:
        vs = self.world().get("versions", {}).get(bucket or self.bucket, {}).get(key)
        return bool(vs) and not vs[-1]["marker"]

    def s3_bytes(self, key: str, bucket: str | None = None) -> bytes:
        return (self.state / "s3" / (bucket or self.bucket) / key).read_bytes()

    def seed_object(self, key: str, body: bytes, bucket: str | None = None) -> None:
        w = self.open_world()
        w.put(bucket or self.bucket, key, body)
        w.save()

    def seed_build(self, status: str, env: dict, build_id: str | None = None) -> str:
        w = self.open_world()
        w.w["n_builds"] += 1
        bid = build_id or f"hamh-setup-runner-build:00000000-0000-4000-8000-a{w.w['n_builds']:011d}"
        w.w["builds"][bid] = {"status": status, "phase": "BUILD" if status == "IN_PROGRESS" else "COMPLETED", "polls": 0, "env": env, "log": []}
        w.save()
        return bid

    def calls(self) -> list:
        p = self.state / "calls.jsonl"
        if not p.is_file():
            return []
        return [json.loads(line)["args"] for line in p.read_text().splitlines() if line.strip()]

    def normalized_calls(self) -> list:
        return [normalize(c) for c in self.calls()]

    def mutations(self) -> list:
        return [c for c in self.calls() if is_mutating(c)]

    def count_calls(self, *words: str) -> int:
        return sum(1 for c in self.normalized_calls() if c[: len(words)] == list(words))

    def assert_order(self, *wanted: list) -> None:
        """Each list of words matches the start of a call, in this order (other calls may lie between them)."""
        calls = self.normalized_calls()
        at = 0
        for words in wanted:
            while at < len(calls) and calls[at][: len(words)] != list(words):
                at += 1
            self.assertLess(at, len(calls), f"no call {words} after position {at}; calls so far:\n" + "\n".join(" ".join(c) for c in calls))
            at += 1

    def start_build_env(self, n: int = 0) -> dict:
        """The environment overrides of the nth start-build call as a dict."""
        starts = [c for c in self.normalized_calls() if c[:2] == ["codebuild", "start-build"]]
        self.assertGreater(len(starts), n, "no start-build call")
        c = starts[n]
        out = {}
        for item in c[c.index("--environment-variables-override") + 1:]:
            if item.startswith("--"):
                break
            parts = dict(p.split("=", 1) for p in item.split(","))
            out[parts["name"]] = parts["value"]
        return out

    # ---- what a run leaves ---------------------------------------------------------------------------
    def receipt_path(self, account: str | None = None, region: str | None = None) -> Path:
        return self.home / ".config" / "ham" / "setup" / (account or self.ACCOUNT) / (region or self.REGION) / "direct.json"

    def receipt(self) -> dict:
        return json.loads(self.receipt_path().read_text())

    def installed(self) -> Path:
        return self.prefix / "bin" / "ham"

    def installed_admin(self) -> Path:
        """The operator tool, in the folder of the client."""
        return self.prefix / "bin" / "ham-admin"
