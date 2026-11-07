"""Tests for scripts/codebuild-run.sh: run a CodeBuild ARM build of the committed HEAD.

The script zips `git archive HEAD`, uploads it to builds/source/<sha>.zip in the stage bucket,
starts the image-build project with a buildspec override and SOURCE_SHA, polls to completion and
prints `CODEBUILD PASS id=<id> sha=<sha7> seconds=<n>` or `CODEBUILD FAIL id=<id> status=<status>`.
A fake aws CLI (CODEBUILD_AWS_BIN) records each call and answers from a script of statuses.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess  # nosec B404: tests start the scripts under test and git with argument lists, never a shell
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "scripts" / "tests"))
import accounts_fixture as fx  # noqa: E402

# The script runs from a copy of what it reads (itself, lib/, accounts.json) in which accounts.json is the placeholder file of accounts_fixture.py:
# the committed file is the maintainers' choice and these cases select their entries by name.
SCRIPT = REPO / "scripts" / "codebuild-run.sh"
_MIRROR = None


def setUpModule() -> None:
    global SCRIPT, _MIRROR
    _MIRROR = tempfile.TemporaryDirectory(prefix="codebuild-run-scripts-")
    tree = Path(_MIRROR.name)
    (tree / "scripts" / "lib").mkdir(parents=True)
    for rel in ("scripts/codebuild-run.sh", "scripts/lib/python.sh", "scripts/lib/aws-target.sh", "scripts/lib/aws_target.py", "scripts/lib/source_bundle.py"):
        shutil.copy2(REPO / rel, tree / rel)
    fx.write_accounts(tree)
    SCRIPT = tree / "scripts" / "codebuild-run.sh"


def tearDownModule() -> None:
    if _MIRROR is not None:
        _MIRROR.cleanup()


FAKE_AWS = r'''#!/usr/bin/env python3
import json, os, sys
log = os.environ["FAKE_AWS_LOG"]
args = sys.argv[1:]
with open(log, "a") as f:
    f.write(json.dumps(args) + "\n")
with open(log + ".profiles", "a") as f:
    f.write(os.environ.get("AWS_PROFILE", "<unset>") + "\n")
if args[:2] == ["s3", "cp"]:
    src = args[2]
    os.makedirs(os.environ["FAKE_S3_DIR"], exist_ok=True)
    import shutil
    shutil.copy(src, os.path.join(os.environ["FAKE_S3_DIR"], os.path.basename(args[3])))
    sys.exit(0)
if args[:2] == ["codebuild", "start-build"]:
    print(os.environ.get("FAKE_START_BODY") or json.dumps({"build": {"id": "hamh-direct-image-build:0001", "buildStatus": "IN_PROGRESS"}}))
    sys.exit(0)
if args[:2] == ["codebuild", "batch-get-builds"]:
    if os.environ.get("FAKE_POLL_FAIL"):
        sys.stderr.write("An error occurred (AccessDeniedException) when calling the BatchGetBuilds operation: not allowed\n")
        sys.exit(254)
    if os.environ.get("FAKE_POLL_FAIL_FIRST") and not os.path.exists(os.environ["FAKE_STATE"] + ".failed"):
        open(os.environ["FAKE_STATE"] + ".failed", "w").write("1")
        sys.stderr.write("Could not connect to the endpoint URL\n")
        sys.exit(255)
    seq = os.environ["FAKE_STATUSES"].split(",")
    state = os.environ["FAKE_STATE"]
    n = int(open(state).read()) if os.path.exists(state) else 0
    open(state, "w").write(str(n + 1))
    status = seq[min(n, len(seq) - 1)]
    print(json.dumps({"builds": [{"id": "hamh-direct-image-build:0001", "buildStatus": status, "currentPhase": "BUILD",
        "logs": {"groupName": "/hamh/direct/codebuild/image-build", "streamName": "0001"}}]}))
    sys.exit(0)
if args[:2] == ["logs", "get-log-events"]:
    print(json.dumps({"events": [{"message": "step failed: exit 9\n"}]}))
    sys.exit(0)
sys.exit(3)
'''


def git(cwd: Path, *args: str) -> str:
    env = dict(os.environ, GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@example.com",
               GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@example.com")
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True, env=env).stdout.strip()  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do


class CodebuildRunBase(unittest.TestCase):
    """The temporary repository, the fake aws CLI and the runner of the script; the cases below subclass it."""

    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="codebuild-run-"))
        self.repo = self.tmp / "repo"
        self.repo.mkdir()
        git(self.repo, "init", "-q", "-b", "main")
        (self.repo / "images").mkdir()
        (self.repo / "images" / "build.yml").write_text("version: 0.2\n")
        (self.repo / "README.md").write_text("x\n")
        git(self.repo, "add", ".")
        git(self.repo, "commit", "-q", "-m", "base")
        self.sha = git(self.repo, "rev-parse", "HEAD")
        self.aws = self.tmp / "aws"
        self.aws.write_text(FAKE_AWS)
        self.aws.chmod(0o755)
        self.log = self.tmp / "aws.log"
        self.profile = "history-profile"

    def run_cb(self, *args: str, statuses: str = "IN_PROGRESS,SUCCEEDED", env_extra: dict | None = None) -> subprocess.CompletedProcess:
        # history-profile unless a test selects another accounts.json entry. The caller's HAMH_ACCOUNTS_FILE never reaches the
        # script unless a test passes it in env_extra.
        env = {k: v for k, v in os.environ.items() if k != "HAMH_ACCOUNTS_FILE"}
        env.update(HAMH_AWS_PROFILE=self.profile, CODEBUILD_AWS_BIN=str(self.aws), FAKE_AWS_LOG=str(self.log),
                   FAKE_S3_DIR=str(self.tmp / "s3"),
                   FAKE_STATUSES=statuses, FAKE_STATE=str(self.tmp / "state"), CODEBUILD_POLL_S="0", CODEBUILD_ROOT=str(self.repo),
                   CODEBUILD_TMP=str(self.tmp / "work"))
        env.update(env_extra or {})
        return subprocess.run(["bash", str(SCRIPT), *args], capture_output=True, text=True, env=env, timeout=120)  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do

    def calls(self) -> list[list[str]]:
        return [json.loads(line) for line in self.log.read_text().splitlines()]


class CodebuildRunCase(CodebuildRunBase):
    def test_uploads_head_and_starts_the_build_with_overrides(self) -> None:
        proc = self.run_cb("--buildspec", "images/build.yml", "--env", "TARGET=linux-arm64")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        last = proc.stdout.strip().splitlines()[-1]
        self.assertRegex(last, rf"^CODEBUILD PASS id=hamh-direct-image-build:0001 sha={self.sha[:7]} seconds=\d+$")
        calls = self.calls()
        cp = next(c for c in calls if c[:2] == ["s3", "cp"])
        self.assertEqual(cp[3], f"s3://hamh-direct-harness-111111111111/builds/source/{self.sha}.zip")
        with zipfile.ZipFile(self.tmp / "s3" / f"{self.sha}.zip") as z:
            self.assertIn("images/build.yml", z.namelist())
        start = next(c for c in calls if c[:2] == ["codebuild", "start-build"])
        joined = " ".join(start)
        self.assertIn("--project-name hamh-direct-image-build", joined)
        self.assertIn("--source-type-override S3", joined)
        self.assertIn(f"--source-location-override hamh-direct-harness-111111111111/builds/source/{self.sha}.zip", joined)
        self.assertIn("--buildspec-override images/build.yml", joined)
        self.assertIn(f"name=SOURCE_SHA,value={self.sha},type=PLAINTEXT", joined)
        self.assertIn("name=TARGET,value=linux-arm64,type=PLAINTEXT", joined)
        self.assertEqual(sum(1 for c in calls if c[:2] == ["codebuild", "batch-get-builds"]), 2)

    def test_a_failed_build_prints_the_log_tail_and_exits_1(self) -> None:
        proc = self.run_cb("--buildspec", "images/build.yml", statuses="IN_PROGRESS,FAILED")
        self.assertEqual(proc.returncode, 1)
        self.assertIn("step failed: exit 9", proc.stdout)
        self.assertEqual(proc.stdout.strip().splitlines()[-1], "CODEBUILD FAIL id=hamh-direct-image-build:0001 status=FAILED")

    def test_a_dirty_tracked_tree_is_refused(self) -> None:
        (self.repo / "README.md").write_text("changed\n")
        proc = self.run_cb("--buildspec", "images/build.yml")
        self.assertEqual(proc.returncode, 2)
        self.assertIn("uncommitted", proc.stderr)
        self.assertFalse(self.log.exists())

    def test_a_missing_buildspec_or_bad_flag_is_usage(self) -> None:
        self.assertEqual(self.run_cb().returncode, 2)
        self.assertEqual(self.run_cb("--buildspec", "images/nope.yml").returncode, 2)
        self.assertEqual(self.run_cb("--bogus").returncode, 2)

    def test_the_active_entry_is_the_default_account_region_and_profile(self) -> None:
        # Unset HAMH_AWS_PROFILE is active-entry 222222222222 us-east-1.
        self.profile = ""
        proc = self.run_cb("--buildspec", "images/build.yml")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        joined = " ".join(" ".join(c) for c in self.calls())
        self.assertIn(f"s3://hamh-direct-harness-222222222222/builds/source/{self.sha}.zip", joined)
        self.assertEqual({c[c.index("--region") + 1] for c in self.calls() if "--region" in c}, {"us-east-1"})
        self.assertEqual(set(Path(str(self.log) + ".profiles").read_text().split()), {"my-profile"})

    def test_an_unknown_profile_is_a_usage_error_before_any_call(self) -> None:
        self.profile = "nope"
        proc = self.run_cb("--buildspec", "images/build.yml")
        self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
        self.assertIn("HAMH_AWS_PROFILE=nope", proc.stderr)
        self.assertFalse(self.log.exists(), "no AWS call")

    def test_every_aws_call_uses_the_selected_profile_and_never_an_inherited_one(self) -> None:
        env_profile = os.environ.get("AWS_PROFILE")
        os.environ["AWS_PROFILE"] = "someone-else"
        try:
            proc = self.run_cb("--buildspec", "images/build.yml")
        finally:
            if env_profile is None:
                os.environ.pop("AWS_PROFILE")
            else:
                os.environ["AWS_PROFILE"] = env_profile
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        profiles = set(Path(str(self.log) + ".profiles").read_text().split())
        self.assertEqual(profiles, {"history-profile"})

    def test_stage_direct_builds_in_its_own_project_and_bucket(self) -> None:
        # review S3: stage direct has no lanes stack; its runtimes stack holds hamh-direct-image-build
        proc = self.run_cb("--buildspec", "images/build.yml", "--stage", "direct")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        calls = self.calls()
        cp = next(c for c in calls if c[:2] == ["s3", "cp"])
        self.assertEqual(cp[3], f"s3://hamh-direct-harness-111111111111/builds/source/{self.sha}.zip")
        start = " ".join(next(c for c in calls if c[:2] == ["codebuild", "start-build"]))
        self.assertIn("--project-name hamh-direct-image-build", start)
        self.assertIn(f"--source-location-override hamh-direct-harness-111111111111/builds/source/{self.sha}.zip", start)
        # the fake CLI answers every build with an id of its own; what the script itself addresses is the direct stage's
        sent = " ".join(" ".join(c) for c in calls if c[:2] in (["s3", "cp"], ["codebuild", "start-build"]))
        self.assertNotIn("hamh-live", sent)
        self.assertNotIn("hamh-drill", sent)

    def test_a_stage_that_is_not_direct_is_usage_before_any_call(self) -> None:
        for stage in ("prod", "", "lanes", "live", "drill"):
            proc = self.run_cb("--buildspec", "images/build.yml", "--stage", stage)
            self.assertEqual(proc.returncode, 2, stage)
            self.assertIn("--stage direct", proc.stderr)
        self.assertFalse(self.log.exists(), "no AWS call")


class PythonAndPolling(CodebuildRunBase):
    """D4 of the live rehearsal of the public setup: the script reads the answers of the AWS CLI with the interpreter it picked (`$py`, HAMH_PYTHON), so a
    machine whose only Python is python3.12 works; a start that names no build ends the run at once; and the wait for a build, and the wait through a
    poll that keeps failing, are bounded."""

    def stranger_path(self) -> str:
        """A PATH with the tools the script and its libraries use and no python3, python or python3.x."""
        base = self.tmp / "stranger-bin"
        base.mkdir(exist_ok=True)
        for tool in ("bash", "git", "dirname", "basename", "mkdir", "date", "sleep", "cat", "sed", "tr", "head", "tail", "cut", "grep", "awk", "env"):
            found = shutil.which(tool)
            if found and not (base / tool).exists():
                (base / tool).symlink_to(found)
        return str(base)

    def run_stranger(self, *args: str, statuses: str = "IN_PROGRESS,SUCCEEDED", env_extra: dict | None = None, timeout: int = 60) -> subprocess.CompletedProcess:
        aws = self.tmp / "aws-abs"
        aws.write_text(FAKE_AWS.replace("#!/usr/bin/env python3", f"#!{sys.executable}", 1))
        aws.chmod(0o755)
        env = {k: v for k, v in os.environ.items() if k != "HAMH_ACCOUNTS_FILE"}
        env.update(HAMH_AWS_PROFILE=self.profile, CODEBUILD_AWS_BIN=str(aws), FAKE_AWS_LOG=str(self.log), FAKE_S3_DIR=str(self.tmp / "s3"),
                   FAKE_STATUSES=statuses, FAKE_STATE=str(self.tmp / "state"), CODEBUILD_POLL_S="0", CODEBUILD_ROOT=str(self.repo),
                   CODEBUILD_TMP=str(self.tmp / "work"), PATH=self.stranger_path(), HAMH_PYTHON=sys.executable)
        env.update(env_extra or {})
        return subprocess.run(["bash", str(SCRIPT), *args], capture_output=True, text=True, env=env, timeout=timeout)  # nosec B603: argument list, no shell, program under test; B607: bash from the PATH built for the test

    def test_a_machine_with_only_the_interpreter_of_ham_python_runs_the_build(self) -> None:
        proc = self.run_stranger("--buildspec", "images/build.yml")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertNotIn("command not found", proc.stderr)
        self.assertRegex(proc.stdout.strip().splitlines()[-1], rf"^CODEBUILD PASS id=hamh-direct-image-build:0001 sha={self.sha[:7]} seconds=\d+$")
        self.assertIn("codebuild-run: started hamh-direct-image-build:0001 for", proc.stdout)

    def test_a_failed_build_prints_its_log_tail_without_a_bare_python3(self) -> None:
        proc = self.run_stranger("--buildspec", "images/build.yml", statuses="IN_PROGRESS,FAILED")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertIn("step failed: exit 9", proc.stdout)
        self.assertEqual(proc.stdout.strip().splitlines()[-1], "CODEBUILD FAIL id=hamh-direct-image-build:0001 status=FAILED")

    def test_a_start_that_names_no_build_stops_at_once_and_polls_nothing(self) -> None:
        for body in ('{"build": {}}', "{}", "not json"):
            with self.subTest(body=body):
                if self.log.exists():
                    self.log.write_text("")
                proc = self.run_stranger("--buildspec", "images/build.yml", env_extra={"FAKE_START_BODY": body}, timeout=30)
                self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
                self.assertIn("codebuild-run: start-build answered without a build id", proc.stderr)
                self.assertEqual([c for c in self.calls() if c[:2] == ["codebuild", "batch-get-builds"]], [])
                self.assertNotIn("started  for", proc.stdout)

    def test_a_build_that_never_ends_is_given_up_on_with_a_message_that_names_the_stop_command(self) -> None:
        proc = self.run_stranger("--buildspec", "images/build.yml", statuses="IN_PROGRESS", env_extra={"CODEBUILD_MAX_WAIT_S": "1"}, timeout=30)
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertIn("codebuild-run: build hamh-direct-image-build:0001 was still IN_PROGRESS after", proc.stderr)
        self.assertIn("aws codebuild stop-build --id hamh-direct-image-build:0001", proc.stderr)

    def test_a_poll_that_keeps_failing_ends_the_run_with_the_error_it_got(self) -> None:
        proc = self.run_stranger("--buildspec", "images/build.yml", env_extra={"FAKE_POLL_FAIL": "1", "CODEBUILD_MAX_ERRORS": "3"}, timeout=30)
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertIn("codebuild-run: batch-get-builds failed 3 times in a row", proc.stderr)
        self.assertIn("AccessDeniedException", proc.stderr)
        self.assertEqual(sum(1 for c in self.calls() if c[:2] == ["codebuild", "batch-get-builds"]), 3)

    def test_one_failed_poll_before_a_good_one_is_not_the_end(self) -> None:
        # The limit counts failures in a row: the fake fails its first poll only, and the build then succeeds.
        proc = self.run_stranger("--buildspec", "images/build.yml", env_extra={"FAKE_POLL_FAIL_FIRST": "1", "CODEBUILD_MAX_ERRORS": "3"}, timeout=30)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertRegex(proc.stdout.strip().splitlines()[-1], r"^CODEBUILD PASS ")


EXAMPLE = REPO / "fixtures" / "setup" / "accounts-generated.json"
MEMBER = ".hamh-build/accounts.json"


class GeneratedAccountFile(CodebuildRunBase):
    """A generated account file (HAMH_ACCOUNTS_FILE) is a separate input of the build. It travels in the source bundle as
    one member the archive does not have, under a key that names its digest, and the build is told the digest so that a child build can
    check it and export HAMH_ACCOUNTS_FILE (lib/child-build-env.sh)."""

    def generated(self, name: str = "generated.json", old: str = "us-east-1a", new: str = "us-east-1a") -> Path:
        p = self.tmp / name
        p.write_text(EXAMPLE.read_text().replace(old, new))
        return p

    def head_names(self) -> list[str]:
        return sorted(git(self.repo, "ls-tree", "-r", "--name-only", "HEAD").splitlines())

    def start_args(self) -> str:
        return " ".join(next(c for c in self.calls() if c[:2] == ["codebuild", "start-build"]))

    def test_without_the_override_the_bundle_is_the_archive_alone_under_the_plain_key(self) -> None:
        proc = self.run_cb("--buildspec", "images/build.yml")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        with zipfile.ZipFile(self.tmp / "s3" / f"{self.sha}.zip") as z:
            self.assertEqual(sorted(n for n in z.namelist() if not n.endswith("/")), self.head_names())
        self.assertNotIn("HAMH_ACCOUNTS_FILE", self.start_args())
        self.assertNotIn(MEMBER, "".join(" ".join(c) for c in self.calls()))

    def test_an_empty_override_is_the_same_as_none(self) -> None:
        proc = self.run_cb("--buildspec", "images/build.yml", env_extra={"HAMH_ACCOUNTS_FILE": ""})
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertTrue((self.tmp / "s3" / f"{self.sha}.zip").is_file())
        self.assertNotIn("HAMH_ACCOUNTS_FILE", self.start_args())

    def test_the_generated_file_is_one_extra_member_and_the_key_names_its_digest(self) -> None:
        import hashlib
        self.profile = ""
        f = self.generated()
        digest = hashlib.sha256(f.read_bytes()).hexdigest()
        proc = self.run_cb("--buildspec", "images/build.yml", env_extra={"HAMH_ACCOUNTS_FILE": str(f)})
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertRegex(proc.stdout.strip().splitlines()[-1], rf"^CODEBUILD PASS id=\S+ sha={self.sha[:7]} seconds=\d+$")
        key = f"builds/source/{self.sha}-{digest[:12]}.zip"
        cp = next(c for c in self.calls() if c[:2] == ["s3", "cp"])
        self.assertEqual(cp[3], f"s3://hamh-direct-harness-111122223333/{key}")
        with zipfile.ZipFile(self.tmp / "s3" / f"{self.sha}-{digest[:12]}.zip") as z:
            names = sorted(n for n in z.namelist() if not n.endswith("/"))
            self.assertEqual(names, sorted([*self.head_names(), MEMBER]))
            self.assertEqual(z.read(MEMBER), f.read_bytes())
            self.assertEqual(z.read("README.md"), b"x\n", "the archive's own files are untouched")
        joined = self.start_args()
        self.assertIn(f"--source-location-override hamh-direct-harness-111122223333/{key}", joined)
        self.assertIn(f"name=HAMH_ACCOUNTS_FILE_SHA256,value={digest},type=PLAINTEXT", joined)
        self.assertIn(f"name=SOURCE_SHA,value={self.sha},type=PLAINTEXT", joined)
        self.assertEqual({c[c.index("--region") + 1] for c in self.calls() if "--region" in c}, {"us-east-1"})
        self.assertEqual(set(Path(str(self.log) + ".profiles").read_text().split()), {"your-aws-profile"})

    def test_another_file_gets_another_key_and_neither_overwrites_the_plain_bundle(self) -> None:
        self.profile = ""
        a, b = self.generated("a.json"), self.generated("b.json", new="us-east-1b")
        for f in (a, b):
            proc = self.run_cb("--buildspec", "images/build.yml", env_extra={"HAMH_ACCOUNTS_FILE": str(f)})
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        keys = [c[3] for c in self.calls() if c[:2] == ["s3", "cp"]]
        self.assertEqual(len(keys), 2)
        self.assertEqual(len(set(keys)), 2)
        self.assertTrue(all(k.startswith(f"s3://hamh-direct-harness-111122223333/builds/source/{self.sha}-") for k in keys))

    def test_the_committed_file_in_the_archive_is_not_the_one_the_build_is_told_to_use(self) -> None:
        # The bundle's own accounts.json (if tracked) stays what the commit holds; the generated file is a separate member.
        (self.repo / "accounts.json").write_text('{"committed": true}\n')
        git(self.repo, "add", "accounts.json")
        git(self.repo, "commit", "-q", "-m", "accounts")
        sha = git(self.repo, "rev-parse", "HEAD")
        self.profile = ""
        f = self.generated()
        proc = self.run_cb("--buildspec", "images/build.yml", env_extra={"HAMH_ACCOUNTS_FILE": str(f)})
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        zips = list((self.tmp / "s3").glob(f"{sha}-*.zip"))
        self.assertEqual(len(zips), 1)
        with zipfile.ZipFile(zips[0]) as z:
            self.assertEqual(z.read("accounts.json"), b'{"committed": true}\n')
            self.assertEqual(z.read(MEMBER), f.read_bytes())

    def test_a_relative_or_missing_override_is_usage_before_any_call(self) -> None:
        self.profile = ""
        for value, msg in (("accounts.json", "absolute"), (str(self.tmp / "gone.json"), "gone.json")):
            with self.subTest(value=value):
                proc = self.run_cb("--buildspec", "images/build.yml", env_extra={"HAMH_ACCOUNTS_FILE": value})
                self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
                self.assertIn(msg, proc.stderr)
                self.assertFalse(self.log.exists(), "no AWS call")


class ChildBuildEnv(unittest.TestCase):
    """lib/child-build-env.sh is the first command of every buildspec under images/codebuild. It runs in the shell CodeBuild uses (POSIX sh
    semantics) and, when the bundle carries a generated account file, checks its digest and exports HAMH_ACCOUNTS_FILE for the commands after it."""

    HELPER = REPO / "scripts" / "lib" / "child-build-env.sh"

    def setUp(self) -> None:
        self.src = Path(tempfile.mkdtemp(prefix="child-build-"))
        (self.src / MEMBER).parent.mkdir(parents=True)
        (self.src / MEMBER).write_bytes(EXAMPLE.read_bytes())
        import hashlib
        self.digest = hashlib.sha256(EXAMPLE.read_bytes()).hexdigest()

    def source(self, shell: str, **env: str) -> subprocess.CompletedProcess:
        base = {k: v for k, v in os.environ.items() if not k.startswith("HAMH_ACCOUNTS_FILE")}
        base.update(CODEBUILD_SRC_DIR=str(self.src), **env)
        script = f'. "{self.HELPER}" && printf "%s\\n" "${{HAMH_ACCOUNTS_FILE:-<unset>}}"'
        return subprocess.run([shell, "-c", script], capture_output=True, text=True, env=base, cwd=self.src, timeout=60)  # nosec B603: argument list, shell is a standard interpreter, the script text is a constant with a path; B607: sh and bash from PATH as CodeBuild does

    def test_the_file_is_exported_by_its_absolute_path_when_the_digest_matches(self) -> None:
        for shell in ("sh", "bash"):
            with self.subTest(interpreter=shell):
                r = self.source(shell, HAMH_ACCOUNTS_FILE_SHA256=self.digest)
                self.assertEqual((r.returncode, r.stdout, r.stderr), (0, f"{self.src}/{MEMBER}\n", ""))

    def test_without_a_digest_the_environment_is_left_alone(self) -> None:
        for shell in ("sh", "bash"):
            with self.subTest(interpreter=shell):
                r = self.source(shell)
                self.assertEqual((r.returncode, r.stdout), (0, "<unset>\n"))

    def test_a_digest_that_does_not_match_stops_the_build(self) -> None:
        (self.src / MEMBER).write_text("{}")
        for shell in ("sh", "bash"):
            with self.subTest(interpreter=shell):
                r = self.source(shell, HAMH_ACCOUNTS_FILE_SHA256=self.digest)
                self.assertNotEqual(r.returncode, 0)
                self.assertEqual(r.stdout, "")
                self.assertIn("child-build-env: ", r.stderr)
                self.assertIn("sha256", r.stderr)

    def test_a_digest_without_the_member_stops_the_build(self) -> None:
        (self.src / MEMBER).unlink()
        r = self.source("sh", HAMH_ACCOUNTS_FILE_SHA256=self.digest)
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("child-build-env: ", r.stderr)
        self.assertIn(MEMBER, r.stderr)

    def test_a_malformed_digest_stops_the_build(self) -> None:
        r = self.source("sh", HAMH_ACCOUNTS_FILE_SHA256="not-a-digest")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("child-build-env: ", r.stderr)

    def test_the_member_path_is_the_one_the_bundle_writer_uses(self) -> None:
        import sys
        sys.path.insert(0, str(REPO / "scripts" / "lib"))
        import source_bundle
        self.assertEqual(source_bundle.MEMBER, MEMBER)
        self.assertIn(MEMBER, self.HELPER.read_text())


class BuildspecHook(unittest.TestCase):
    """Every buildspec under images/codebuild starts with `. scripts/lib/child-build-env.sh`, so a nested build that reads the selection reads the
    generated file the deploy used."""

    def test_the_hook_is_the_first_command_of_the_first_phase_of_every_buildspec(self) -> None:
        specs = sorted((REPO / "images" / "codebuild").glob("*.yml"))
        self.assertGreaterEqual(len(specs), 5, "the glob finds the buildspecs: 7 in the repository, 5 in the public tree")
        for spec in specs:
            with self.subTest(spec=spec.name):
                lines = spec.read_text().splitlines()
                at = next(i for i, ln in enumerate(lines) if ln == "phases:")
                cmd = next(i for i in range(at, len(lines)) if lines[i].strip() == "commands:")
                self.assertEqual(lines[cmd + 1].strip(), "- . scripts/lib/child-build-env.sh", spec.name)


if __name__ == "__main__":
    unittest.main()
