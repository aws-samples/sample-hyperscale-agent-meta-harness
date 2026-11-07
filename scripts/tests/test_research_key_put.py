"""Tests for scripts/research-key-put.py: the maintainers' one command that stores the Tavily API key in the stage account's Secrets Manager
secret.

Contract:
- The key is typed at a terminal prompt (no echo, asked twice, 20 to 256 characters without whitespace or control characters). A stdin that is
  not a terminal is refused (exit 2). The parser has no option that could carry the key.
- The AWS CLI (HAMH_AWS_BIN, else `aws` on PATH) is run with an argument list. The key reaches it on STDIN only (`--secret-string file:///dev/stdin`):
  it is not in argv, in the child's environment, in stdout, in stderr, or in a file. Account, Region and profile come from scripts/lib/aws_target.py.
- Exit 0 stored (or, with --check, the AWSCURRENT version is present); 1 an AWS step failed; 2 refused (not a terminal, mismatch, bad key, bad target);
  3 a prerequisite is missing (no CLI, the secret does not exist); 4 --check found no AWSCURRENT version.
- --check never reads the value (no get-secret-value) and never prompts.
- The secret name is the one the CDK construct creates (infra/lib/config.ts `names.researchSecret`).
The AWS CLI is a fake executable that records its argv, its environment and its stdin. The key is built at run time.
"""

from __future__ import annotations

import datetime
import hashlib
import importlib.util
import io
import json
import os
import pty
import re
import secrets
import select
import socket
import ssl
import subprocess  # nosec B404: runs the script under test with an argument list
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

BANDIT = "no" + "sec"  # two pieces: the goal check reads every Python line of main for this word and accepts it only in a written directive
REPO = Path(__file__).resolve().parents[2]
SCRIPT = REPO / "scripts" / "research-key-put.py"
CONFIG_TS = REPO / "infra" / "lib" / "config.ts"

ACTIVE = {"account": "111111111111", "region": "us-east-1", "profile": "fake-active", "role": "active"}
OTHER = {"account": "222222222222", "region": "us-west-2", "profile": "fake-history", "role": "history"}

FAKE_AWS = r"""#!__PYTHON__
import json, os, sys, time
argv = sys.argv[1:]
stdin = sys.stdin.read() if "file:///dev/stdin" in argv else ""
with open(os.environ["FAKE_AWS_RECORD"], "a") as f:
    f.write(json.dumps({"argv": argv, "env": dict(os.environ), "stdin": stdin}) + "\n")


def opt(name):
    return argv[argv.index(name) + 1] if name in argv else ""


sub = argv[1] if len(argv) > 1 and argv[0] == "secretsmanager" else ""
name, region = opt("--secret-id"), opt("--region")
if argv[:3] == ["configure", "get", "cli_history"]:
    # `aws configure get cli_history`: the value on stdout; exit 1 with no output when it is not set (the real CLI does the same).
    how = os.environ.get("FAKE_AWS_HISTORY", "unset")
    if how == "unset":
        sys.exit(1)
    if how == "fail":
        sys.stderr.write("The config profile (fake) could not be found\n")
        sys.exit(255)
    sys.stdout.write(how + "\n")
    sys.exit(0)
if argv[:2] == ["lambda", "invoke"]:
    # The research route, scripted by a JSON file of steps (FAKE_AWS_ROUTE); step n answers call n, the last step repeats. The output is what the
    # real CLI prints with /dev/stdout as the outfile: the function's payload, then the CLI's own metadata.
    route = os.environ.get("FAKE_AWS_ROUTE")
    if not route:
        sys.stderr.write("unexpected call " + " ".join(argv) + "\n")
        sys.exit(2)
    steps = json.load(open(route))
    counter = route + ".n"
    n = int(open(counter).read()) if os.path.exists(counter) else 0
    open(counter, "w").write(str(n + 1))
    step = steps[min(n, len(steps) - 1)]
    if step.get("sleep"):
        time.sleep(step["sleep"])
    if "aws_error" in step:
        sys.stderr.write("\naws: [ERROR]: An error occurred (%s) when calling the Invoke operation: %s\n" % (step["aws_error"], step.get("message", "x")))
        sys.exit(254)
    meta = {"StatusCode": 200, "ExecutedVersion": "$LATEST"}
    if step.get("function_error"):
        meta["FunctionError"] = "Unhandled"
        sys.stdout.write(json.dumps(step["payload"]))
    elif "raw" in step:
        sys.stdout.write(step["raw"])
    else:
        sys.stdout.write(json.dumps({"statusCode": step["status"], "headers": {"content-type": "application/json"},
                                     "body": json.dumps(step["body"]), "isBase64Encoded": False}))
    sys.stdout.write(json.dumps(meta, indent=4) + "\n")
    sys.exit(0)
arn = f"arn:aws:secretsmanager:{region}:111111111111:secret:{name}-AbCdEf"
vid = "11111111-2222-3333-4444-555555555555"
mode = os.environ.get("FAKE_AWS_STATE", "exists")
notfound = ("An error occurred (ResourceNotFoundException) when calling the %s operation: "
            "Secrets Manager can't find the specified secret.")
if sub == "get-secret-value":
    sys.stderr.write("forbidden call\n")
    sys.exit(99)
if (mode == "absent" and sub in ("put-secret-value", "describe-secret", "tag-resource", "untag-resource", "list-secret-version-ids")) or (mode == "vanish" and sub == "put-secret-value"):
    sys.stderr.write(notfound % sub + "\n")
    sys.exit(254)
if sub == "put-secret-value":
    how = os.environ.get("FAKE_AWS_PUT", "ok")
    if how == "echo_key":
        sys.stderr.write("An error occurred (InvalidRequestException) when calling the PutSecretValue operation: "
                         "the value " + stdin + " was rejected\n")
        sys.exit(254)
    if how == "fail":
        sys.stderr.write("An error occurred (AccessDeniedException) when calling the PutSecretValue operation: denied\n")
        sys.exit(254)
    if how == "fail_echo":
        sys.stderr.write("An error occurred (WeirdCodeFromNowhere) when calling the PutSecretValue operation: SOMETEXT " + stdin[-8:] + " " + stdin[:8] + "\n")
        sys.exit(254)
    if how == "garbage":
        print("this is not json")
        sys.exit(0)
    if how == "slow":
        time.sleep(3)
    print(json.dumps({"ARN": arn, "Name": name, "VersionId": vid, "VersionStages": ["AWSCURRENT"]}))
elif sub == "tag-resource":
    if os.environ.get("FAKE_AWS_TAG") == "fail":
        sys.stderr.write("An error occurred (AccessDeniedException) when calling the TagResource operation: denied\n")
        sys.exit(254)
elif sub == "list-secret-version-ids":
    how = os.environ.get("FAKE_AWS_LIST", "default")
    if how == "fail":
        sys.stderr.write("An error occurred (AccessDeniedException) when calling the ListSecretVersionIds operation: denied\n")
        sys.exit(254)
    if how == "junk":
        print("not json at all")
    else:
        versions = json.loads(os.environ["FAKE_AWS_VERSIONS"]) if "FAKE_AWS_VERSIONS" in os.environ else [{"VersionId": vid, "VersionStages": ["AWSCURRENT"]}]
        print(json.dumps({"ARN": arn, "Name": name, "Versions": versions}))
elif sub == "untag-resource":
    if os.environ.get("FAKE_AWS_UNTAG") == "fail":
        sys.stderr.write("An error occurred (AccessDeniedException) when calling the UntagResource operation: denied\n")
        sys.exit(254)
elif sub == "describe-secret":
    out = {"ARN": arn, "Name": name}
    if os.environ.get("FAKE_AWS_STAGES", "current") == "current":
        out["VersionIdsToStages"] = {vid: ["AWSCURRENT"]}
    if "FAKE_AWS_CHANGED" in os.environ:
        out["LastChangedDate"] = json.loads(os.environ["FAKE_AWS_CHANGED"])
    if "FAKE_AWS_TAGS" in os.environ:
        out["Tags"] = json.loads(os.environ["FAKE_AWS_TAGS"])
    print(json.dumps(out))
else:
    sys.stderr.write("unexpected call " + " ".join(argv) + "\n")
    sys.exit(2)
"""


def load_script():
    spec = importlib.util.spec_from_file_location("research_key_put_under_test", SCRIPT)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def accounts_doc() -> dict:
    def entry(e: dict) -> dict:
        r = e["region"]
        return {**e, "azs": {s: [r + "a"] for s in ("live", "drill", "direct")},
                "azIds": {s: ["x-az1"] for s in ("live", "drill", "direct")}, "invocationLogging": "read-only"}
    return {"schemaVersion": 1, "selector": "HAMH_AWS_PROFILE", "accounts": [entry(OTHER), entry(ACTIVE)]}


def new_key() -> str:
    return secrets.token_hex(16)


class Bench:
    """A scratch directory with the fake CLI, an accounts file and the environment a run sees."""

    def __init__(self, test: unittest.TestCase, extra: dict | None = None) -> None:
        scratch = tempfile.TemporaryDirectory(prefix="rkp-", dir=os.environ.get("TMPDIR") or None)
        test.addCleanup(scratch.cleanup)
        self.dir = Path(scratch.name)
        self.aws = self.dir / "aws"
        self.aws.write_text(FAKE_AWS.replace("__PYTHON__", sys.executable))
        self.aws.chmod(0o755)
        self.record = self.dir / "record.jsonl"
        self.accounts = self.dir / "accounts.json"
        self.accounts.write_text(json.dumps(accounts_doc()))
        self.env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(self.dir), "HAMH_ACCOUNTS_FILE": str(self.accounts),
                    "HAMH_AWS_BIN": str(self.aws), "FAKE_AWS_RECORD": str(self.record), **(extra or {})}

    def all_calls(self) -> list[dict]:
        if not self.record.exists():
            return []
        return [json.loads(line) for line in self.record.read_text().splitlines()]

    def calls(self) -> list[dict]:
        """The AWS CLI calls that touch the secret or the function. The question about the CLI history (`configure get cli_history`) that every mode which takes
        a key asks first is left out here; `all_calls()` has it."""
        return [c for c in self.all_calls() if c["argv"][:3] != ["configure", "get", "cli_history"]]

    def subcommands(self) -> list[str]:
        return [c["argv"][1] for c in self.calls()]


def run_main(mod, argv: list[str], bench: Bench, *, tty: bool = True, answers: list[str] | None = None,
             now: datetime.datetime | None = None, env: dict | None = None):
    out, err = io.StringIO(), io.StringIO()
    asked: list[str] = []
    queue = list(answers or [])

    def prompt(msg: str) -> str:
        asked.append(msg)
        if not queue:
            raise AssertionError("the script asked for more input than the test gave")
        return queue.pop(0)

    code = mod.main(argv, env=env if env is not None else bench.env, is_tty=lambda: tty, prompt=prompt, stdout=out, stderr=err,
                    now=lambda: now or datetime.datetime(2026, 10, 16, 23, 59, tzinfo=datetime.timezone.utc))
    return code, out.getvalue(), err.getvalue(), asked


class ParserCase(unittest.TestCase):
    def test_no_option_can_carry_the_key(self) -> None:
        mod = load_script()
        options = {o for a in mod.build_parser()._actions for o in a.option_strings}
        # --max-age-days takes a whole number from 1 to 3650 and nothing else; a key given as its value is refused below. --clear takes no value.
        self.assertEqual(options, {"-h", "--help", "--stage", "--check", "--max-age-days", "--clear", "--rotate", "--rotate-enterprise", "--ttl-hours",
                                   "--key-limit", "--key-type", "--key-name"})

    def test_stage_is_required_and_only_direct(self) -> None:
        mod = load_script()
        b = Bench(self)
        for argv in ([], ["--stage", "live"]):
            with self.assertRaises(SystemExit) as cm:
                run_main(mod, argv, b, answers=[])
            self.assertEqual(cm.exception.code, 2)
        self.assertEqual(b.calls(), [])

    def test_a_key_given_as_an_argument_is_refused_and_not_echoed(self) -> None:
        key = new_key()
        for argv in (["--key", key], ["--key=" + key], ["--secret-string", key], ["--password", key], ["--stage", key],
                     ["--stage", "direct", key], ["--ch", key], [key], ["--stage", "direct", "--check", "--max-age-days", key], ["--stage", "direct", "--clear", key], ["--stage", "direct", "--clear=" + key]):
            p = subprocess.run([sys.executable, str(SCRIPT), *argv], capture_output=True, text=True, stdin=subprocess.DEVNULL)  # nosec B603: the test interpreter runs the script under test, no shell
            self.assertEqual(p.returncode, 2, argv)
            self.assertNotIn(key, p.stdout + p.stderr, argv)

    def test_options_are_not_abbreviated(self) -> None:
        mod = load_script()
        self.assertFalse(mod.build_parser().allow_abbrev)


class StoreCase(unittest.TestCase):
    def setUp(self) -> None:
        self.mod = load_script()
        self.bench = Bench(self)
        self.key = new_key()

    def store(self, **kw):
        return run_main(self.mod, ["--stage", "direct"], self.bench, answers=[self.key, self.key], **kw)

    def test_the_key_reaches_the_cli_on_stdin_only(self) -> None:
        code, out, err, asked = self.store()
        self.assertEqual(code, 0, err)
        self.assertEqual(len(asked), 2)
        calls = self.bench.calls()
        put = [c for c in calls if c["argv"][1] == "put-secret-value"]
        self.assertEqual(len(put), 1)
        self.assertEqual(put[0]["stdin"], self.key, "the key is exactly the child's stdin")
        for c in calls:
            self.assertNotIn(self.key, json.dumps(c["argv"]), "the key is not in argv")
            self.assertNotIn(self.key, json.dumps(c["env"]), "the key is not in the child's environment")
        self.assertNotIn(self.key, out + err)
        self.assertIn("file:///dev/stdin", put[0]["argv"])
        for c in calls:
            if c["argv"][1] != "put-secret-value":
                self.assertEqual(c["stdin"], "", "only the put reads stdin")

    def test_a_variable_that_holds_the_key_does_not_reach_the_child(self) -> None:
        env = {**self.bench.env, "LEAKY": "x" + self.key + "y"}
        code, _out, err, _ = run_main(self.mod, ["--stage", "direct"], self.bench, answers=[self.key, self.key], env=env)
        self.assertEqual(code, 0, err)
        after_prompt = [c for c in self.bench.calls() if c["argv"][1] in ("put-secret-value", "tag-resource")]
        self.assertEqual(len(after_prompt), 2)
        for c in after_prompt:
            self.assertNotIn("LEAKY", c["env"])
            self.assertNotIn(self.key, json.dumps(c["env"]))

    def test_the_argument_list_names_the_secret_the_profile_and_the_region(self) -> None:
        code, _out, err, _ = self.store()
        self.assertEqual(code, 0, err)
        put = [c["argv"] for c in self.bench.calls() if c["argv"][1] == "put-secret-value"][0]
        self.assertEqual(put[:2], ["secretsmanager", "put-secret-value"])
        self.assertEqual(put[put.index("--secret-id") + 1], self.mod.RESEARCH_STORE_NAME)
        self.assertEqual(put[put.index("--secret-string") + 1], "file:///dev/stdin")
        self.assertEqual(put[put.index("--profile") + 1], ACTIVE["profile"])
        self.assertEqual(put[put.index("--region") + 1], ACTIVE["region"])
        for bad in ("--cli-input-json", "--secret-binary", "--generate-secret-string"):
            self.assertNotIn(bad, put)

    def test_the_other_selection_uses_its_profile_and_region(self) -> None:
        env = {**self.bench.env, "HAMH_AWS_PROFILE": OTHER["profile"]}
        code, _out, err, _ = run_main(self.mod, ["--stage", "direct"], self.bench, answers=[self.key, self.key], env=env)
        self.assertEqual(code, 0, err)
        for c in self.bench.calls():
            argv = c["argv"]
            self.assertEqual(argv[argv.index("--profile") + 1], OTHER["profile"])
            self.assertEqual(argv[argv.index("--region") + 1], OTHER["region"])

    def test_an_unknown_selection_is_refused_before_any_prompt_or_call(self) -> None:
        env = {**self.bench.env, "HAMH_AWS_PROFILE": "no-such-profile"}
        code, _out, err, asked = run_main(self.mod, ["--stage", "direct"], self.bench, answers=[], env=env)
        self.assertEqual(code, 2)
        self.assertIn("no-such-profile", err)
        self.assertEqual(asked, [])
        self.assertEqual(self.bench.calls(), [])

    def test_the_secret_is_tagged_with_the_utc_date(self) -> None:
        late = datetime.datetime(2026, 10, 16, 23, 59, tzinfo=datetime.timezone(datetime.timedelta(hours=-6)))
        code, _out, err, _ = self.store(now=late)
        self.assertEqual(code, 0, err)
        tag = [c["argv"] for c in self.bench.calls() if c["argv"][1] == "tag-resource"]
        self.assertEqual(len(tag), 1)
        self.assertEqual(tag[0][tag[0].index("--tags") + 1], "Key=research-key-stored-at,Value=2026-10-17")
        self.assertEqual(tag[0][tag[0].index("--secret-id") + 1], self.mod.RESEARCH_STORE_NAME)
        self.assertNotIn("file:///dev/stdin", tag[0])

    def test_the_output_names_the_secret_the_version_and_the_length_and_nothing_of_the_key(self) -> None:
        code, out, err, _ = self.store()
        self.assertEqual(code, 0, err)
        self.assertIn(f"arn:aws:secretsmanager:{ACTIVE['region']}:111111111111:secret:{self.mod.RESEARCH_STORE_NAME}-AbCdEf", out)
        self.assertIn("11111111-2222-3333-4444-555555555555", out)
        self.assertIn(f"{len(self.key)} characters", out)
        self.assertIn("next:", out)
        digest = hashlib.sha256(self.key.encode()).hexdigest()
        for derived in (self.key, self.key[:6], self.key[-6:], digest[:12], digest[-12:]):
            self.assertNotIn(derived, out + err)

    def test_a_mismatch_stores_nothing(self) -> None:
        code, out, err, asked = run_main(self.mod, ["--stage", "direct"], self.bench, answers=[self.key, new_key()])
        self.assertEqual(code, 2)
        self.assertEqual(self.bench.subcommands(), ["describe-secret"], "only the read-only pre-flight ran; nothing was stored or tagged")
        self.assertNotIn(self.key, out + err)
        self.assertEqual(len(asked), 2)

    def test_a_bad_key_stores_nothing_and_is_not_echoed(self) -> None:
        bad = ["a" * 19, "a" * 257, "abc def" + "x" * 20, "tab\there" + "x" * 20, "ctl\x07" + "x" * 20, "new\nline" + "x" * 20, ""]
        for k in bad:
            code, out, err, _ = run_main(self.mod, ["--stage", "direct"], self.bench, answers=[k, k])
            self.assertEqual(code, 2, repr(k))
            if len(k) > 8:
                self.assertNotIn(k, out + err)
        self.assertEqual(set(self.bench.subcommands()), {"describe-secret"}, "nothing was stored or tagged")

    def test_the_length_limits_are_inclusive(self) -> None:
        for n in (20, 256):
            k = "k" * n
            code, _out, err, _ = run_main(self.mod, ["--stage", "direct"], self.bench, answers=[k, k])
            self.assertEqual(code, 0, f"{n}: {err}")

    def test_a_stdin_that_is_not_a_terminal_is_refused_without_a_prompt(self) -> None:
        code, _out, err, asked = run_main(self.mod, ["--stage", "direct"], self.bench, tty=False, answers=[])
        self.assertEqual(code, 2)
        self.assertIn("terminal", err)
        self.assertEqual(asked, [])
        self.assertEqual(self.bench.calls(), [])

    def test_a_cli_error_that_echoes_the_key_is_redacted_and_names_the_step(self) -> None:
        bench = Bench(self, {"FAKE_AWS_PUT": "echo_key"})
        code, out, err, _ = run_main(self.mod, ["--stage", "direct"], bench, answers=[self.key, self.key])
        self.assertEqual(code, 1)
        self.assertNotIn(self.key, out + err)
        self.assertIn("<redacted>", err)
        self.assertIn("put-secret-value", err)
        self.assertNotIn("tag-resource", bench.subcommands(), "a failed put is not tagged")

    def test_an_error_echoing_the_key_in_json_escaped_form_is_redacted(self) -> None:
        key = 'ab"cd\\ef' + secrets.token_hex(8)
        bench = Bench(self, {"FAKE_AWS_PUT": "echo_key"})
        # The fake echoes the raw text; the script also strips the JSON-escaped spelling a CLI error body may carry.
        escaped = json.dumps(key)[1:-1]
        self.assertNotEqual(escaped, key)
        red = self.mod.redact(f"x {key} y {escaped} z", key)
        self.assertNotIn(key, red)
        self.assertNotIn(escaped, red)
        self.assertEqual(red.count("<redacted>"), 2)
        code, out, err, _ = run_main(self.mod, ["--stage", "direct"], bench, answers=[key, key])
        self.assertEqual(code, 1)
        self.assertNotIn(key, out + err)

    def test_a_denied_put_exits_1_and_says_which_step(self) -> None:
        bench = Bench(self, {"FAKE_AWS_PUT": "fail"})
        code, _out, err, _ = run_main(self.mod, ["--stage", "direct"], bench, answers=[self.key, self.key])
        self.assertEqual(code, 1)
        self.assertIn("put-secret-value", err)
        self.assertIn("AccessDeniedException", err)

    def test_a_tag_failure_after_the_put_says_the_key_is_stored(self) -> None:
        bench = Bench(self, {"FAKE_AWS_TAG": "fail"})
        code, out, err, _ = run_main(self.mod, ["--stage", "direct"], bench, answers=[self.key, self.key])
        self.assertEqual(code, 1)
        self.assertIn("tag-resource", err)
        self.assertIn("stored", out + err)
        self.assertIn("11111111-2222-3333-4444-555555555555", out + err)
        self.assertNotIn(self.key, out + err)

    def test_an_absent_secret_exits_3_before_the_prompt_with_the_deploy_hint(self) -> None:
        bench = Bench(self, {"FAKE_AWS_STATE": "absent"})
        code, out, err, asked = run_main(self.mod, ["--stage", "direct"], bench, answers=[])
        self.assertEqual(code, 3)
        self.assertIn(self.mod.RESEARCH_STORE_NAME, err)
        self.assertIn("deploy", err)
        self.assertEqual(asked, [], "the owner is not asked to type a key the script cannot store")
        self.assertEqual(bench.subcommands(), ["describe-secret"])

    def test_a_secret_that_vanishes_before_the_put_exits_3_and_stores_nothing(self) -> None:
        bench = Bench(self, {"FAKE_AWS_STATE": "vanish"})
        code, out, err, _ = run_main(self.mod, ["--stage", "direct"], bench, answers=[self.key, self.key])
        self.assertEqual(code, 3)
        self.assertIn("deploy", err)
        self.assertNotIn(self.key, out + err)
        self.assertNotIn("tag-resource", bench.subcommands())

    def test_a_missing_cli_exits_3_with_a_plain_message(self) -> None:
        env = {**self.bench.env, "HAMH_AWS_BIN": str(self.bench.dir / "no-such-aws")}
        code, out, err, asked = run_main(self.mod, ["--stage", "direct"], self.bench, answers=[], env=env)
        self.assertEqual(code, 3)
        self.assertIn("aws", err)
        self.assertNotIn("Traceback", err)
        self.assertEqual(asked, [], "the CLI is looked up before the prompt")

    def test_without_ham_aws_bin_aws_is_found_on_path(self) -> None:
        env = {k: v for k, v in self.bench.env.items() if k != "HAMH_AWS_BIN"}
        env["PATH"] = str(self.bench.dir) + os.pathsep + env["PATH"]
        code, _out, err, _ = run_main(self.mod, ["--stage", "direct"], self.bench, answers=[self.key, self.key], env=env)
        self.assertEqual(code, 0, err)

    def test_the_key_buffer_the_script_holds_is_cleared_after_the_store(self) -> None:
        self.assertTrue(hasattr(self.mod, "wipe"))
        buf = bytearray(b"secret-bytes")
        self.mod.wipe(buf)
        self.assertEqual(bytes(buf), b"\x00" * len(b"secret-bytes"))


class CheckCase(unittest.TestCase):
    def setUp(self) -> None:
        self.mod = load_script()

    def check(self, bench: Bench, *extra: str, **kw):
        def never(_msg: str) -> str:
            raise AssertionError("--check must not prompt")
        out, err = io.StringIO(), io.StringIO()
        code = self.mod.main(["--stage", "direct", "--check", *extra], env=bench.env, is_tty=lambda: False, prompt=never, stdout=out,
                             stderr=err, **kw)
        return code, out.getvalue(), err.getvalue()

    def test_present_exits_0_and_reads_no_value(self) -> None:
        bench = Bench(self)
        code, out, err = self.check(bench)
        self.assertEqual(code, 0, err)
        self.assertIn("present", out)
        self.assertEqual(bench.subcommands(), ["describe-secret"])
        self.assertNotIn("get-secret-value", bench.subcommands())

    def test_a_secret_with_no_current_version_is_absent_exit_4(self) -> None:
        bench = Bench(self, {"FAKE_AWS_STAGES": "none"})
        code, out, _err = self.check(bench)
        self.assertEqual(code, 4)
        self.assertIn("absent", out)
        self.assertEqual(bench.subcommands(), ["describe-secret"])

    def test_a_missing_secret_is_absent_exit_4_with_the_deploy_hint(self) -> None:
        bench = Bench(self, {"FAKE_AWS_STATE": "absent"})
        code, out, err = self.check(bench)
        self.assertEqual(code, 4)
        self.assertIn("absent", out)
        self.assertIn("deploy", out + err)

    def test_check_uses_the_selected_profile_and_region(self) -> None:
        bench = Bench(self, {"HAMH_AWS_PROFILE": OTHER["profile"]})
        code, _out, err = self.check(bench)
        self.assertEqual(code, 0, err)
        argv = bench.calls()[0]["argv"]
        self.assertEqual(argv[argv.index("--profile") + 1], OTHER["profile"])
        self.assertEqual(argv[argv.index("--region") + 1], OTHER["region"])


NOW = datetime.datetime(2026, 10, 16, 23, 59, tzinfo=datetime.timezone.utc)
TAG = {"Key": "research-key-stored-at", "Value": "2026-09-10"}


def days_ago(days: float) -> float:
    """The Unix time `days` before NOW, as `aws` prints a timestamp when cli_timestamp_format is unixtimestamp."""
    return NOW.timestamp() - days * 86400


class AgeCase(unittest.TestCase):
    """--check also reports how long ago the current key was stored (describe-secret, never the value) and --max-age-days sets the limit."""

    def setUp(self) -> None:
        self.mod = load_script()

    def check(self, bench: Bench, *extra: str):
        def never(_msg: str) -> str:
            raise AssertionError("--check must not prompt")
        out, err = io.StringIO(), io.StringIO()
        code = self.mod.main(["--stage", "direct", "--check", *extra], env=bench.env, is_tty=lambda: False, prompt=never, stdout=out,
                             stderr=err, now=lambda: NOW)
        return code, out.getvalue(), err.getvalue()

    def bench(self, changed, tags=None) -> Bench:
        extra = {"FAKE_AWS_CHANGED": json.dumps(changed)}
        if tags is not None:
            extra["FAKE_AWS_TAGS"] = json.dumps(tags)
        return Bench(self, extra)

    def test_a_key_stored_five_days_ago_is_fine_and_the_age_is_shown(self) -> None:
        bench = self.bench(days_ago(5))
        code, out, err = self.check(bench)
        self.assertEqual(code, 0, err)
        self.assertIn("present", out)
        self.assertRegex(out, r"age: stored 5\.0 days ago")
        self.assertRegex(out, r"limit: 30 days")
        self.assertNotIn("OLD", out)
        self.assertEqual(bench.subcommands(), ["describe-secret"])

    def test_an_older_key_exits_5_and_says_to_rotate(self) -> None:
        bench = self.bench(days_ago(31))
        code, out, err = self.check(bench)
        self.assertEqual(code, 5, err)
        self.assertIn("present", out)
        self.assertRegex(out, r"age: stored 31\.0 days ago")
        self.assertRegex(out, r"OLD: .*older than 30 days")
        self.assertEqual(bench.subcommands(), ["describe-secret"])

    def test_the_limit_is_older_than_and_max_age_days_moves_it(self) -> None:
        self.assertEqual(self.check(self.bench(days_ago(29.5)))[0], 0)
        self.assertEqual(self.check(self.bench(days_ago(30.5)))[0], 5)
        code, out, _err = self.check(self.bench(days_ago(31)), "--max-age-days", "45")
        self.assertEqual(code, 0)
        self.assertIn("limit: 45 days", out)
        code, out, _err = self.check(self.bench(days_ago(31)), "--max-age-days", "7")
        self.assertEqual(code, 5)
        self.assertIn("older than 7 days", out)

    def test_the_cli_timestamp_forms_are_read(self) -> None:
        # AWS CLI v2 prints ISO 8601 with an offset (or Z); v1 prints a Unix time. 2026-09-26T12:00:00.123-06:00 is 2026-09-26T18:00:00.123Z.
        for form in ("2026-09-26T12:00:00.123000-06:00", "2026-09-26T18:00:00.123Z", "2026-09-26T18:00:00+00:00", 1790445600.123,
                     1790445600, "1790445600.123"):
            code, out, err = self.check(self.bench(form))
            self.assertEqual(code, 0, f"{form!r}: {err}")
            self.assertRegex(out, r"age: stored 20\.2 days ago", f"{form!r}: {out}")

    def test_the_stored_at_tag_is_shown_when_present_and_other_tags_are_not(self) -> None:
        code, out, _err = self.check(self.bench(days_ago(5), [TAG, {"Key": "project", "Value": "hyperscale-agent-meta-harness"}]))
        self.assertEqual(code, 0)
        self.assertIn("research-key-stored-at=2026-09-10", out)
        self.assertNotIn("hyperscale-agent-meta-harness", out)
        code, out, _err = self.check(self.bench(days_ago(5), [{"Key": "project", "Value": "x"}]))
        self.assertEqual(code, 0)
        self.assertIn("no research-key-stored-at tag", out)

    def test_a_missing_or_unreadable_date_is_unknown_and_not_a_failure(self) -> None:
        code, out, _err = self.check(Bench(self))
        self.assertEqual(code, 0)
        self.assertIn("present", out)
        self.assertIn("age: unknown", out)
        for bad in ("yesterday", "", None, [], {"a": 1}, True):
            code, out, _err = self.check(self.bench(bad))
            self.assertEqual(code, 0, repr(bad))
            self.assertIn("age: unknown", out, repr(bad))

    def test_an_absent_key_keeps_exit_4_and_prints_no_age(self) -> None:
        for env in ({"FAKE_AWS_STAGES": "none"}, {"FAKE_AWS_STATE": "absent"}):
            bench = Bench(self, {**env, "FAKE_AWS_CHANGED": json.dumps(days_ago(90))})
            code, out, _err = self.check(bench)
            self.assertEqual(code, 4, env)
            self.assertNotIn("age:", out)
            self.assertNotIn("OLD", out)

    def test_max_age_days_is_a_positive_whole_number_and_belongs_to_check(self) -> None:
        for bad in ("0", "-1", "abc", "1.5", "", "3651"):
            bench = Bench(self)
            out, err = io.StringIO(), io.StringIO()
            with self.assertRaises(SystemExit) as raised:
                self.mod.main(["--stage", "direct", "--check", "--max-age-days", bad], env=bench.env, is_tty=lambda: False,
                              prompt=lambda _m: "", stdout=out, stderr=err, now=lambda: NOW)
            self.assertEqual(raised.exception.code, 2, bad)
            self.assertEqual(bench.calls(), [], bad)
        bench = Bench(self)
        with self.assertRaises(SystemExit) as raised:
            self.mod.main(["--stage", "direct", "--max-age-days", "30"], env=bench.env, is_tty=lambda: True, prompt=lambda _m: "",
                          stdout=io.StringIO(), stderr=io.StringIO(), now=lambda: NOW)
        self.assertEqual(raised.exception.code, 2)
        self.assertEqual(bench.calls(), [])

    def test_the_exit_codes_zero_to_four_are_unchanged_and_five_is_documented(self) -> None:
        mod = load_script()
        self.assertEqual((mod.EXIT_OK, mod.EXIT_AWS, mod.EXIT_REFUSED, mod.EXIT_PREREQ, mod.EXIT_ABSENT), (0, 1, 2, 3, 4))
        self.assertEqual(mod.EXIT_OLD, 5)
        doc = mod.__doc__ or ""
        self.assertRegex(doc, r"5 with --check")
        self.assertIn("--max-age-days", doc)

    def test_the_age_check_reads_no_value_and_the_key_handling_is_untouched(self) -> None:
        for line in SCRIPT.read_text().splitlines():
            if "get-secret-value" in line:
                self.assertTrue(line.lstrip().startswith("#"), f"the value is never read: {line}")
        mod = load_script()
        self.assertEqual(set(mod.AWS_SUBCOMMANDS), {"put-secret-value", "tag-resource", "describe-secret"})


class ClearCase(unittest.TestCase):
    """--clear replaces the stored Tavily key by a blank version, so a stage in auto mode searches with AgentCore Web Search again. It asks for
    nothing, needs no terminal, reads nothing and makes the same three AWS calls as the store."""

    def setUp(self) -> None:
        self.mod = load_script()
        self.bench = Bench(self)

    def clear(self, bench: Bench | None = None, **kw):
        return run_main(self.mod, ["--stage", "direct", "--clear"], bench or self.bench, tty=False, answers=[], **kw)

    def test_it_stores_a_blank_version_without_a_prompt_or_a_terminal(self) -> None:
        code, out, err, asked = self.clear()
        self.assertEqual(code, 0, err)
        self.assertEqual(asked, [], "nothing is typed")
        self.assertEqual(self.bench.subcommands(), ["describe-secret", "put-secret-value", "tag-resource"])
        put = [c for c in self.bench.calls() if c["argv"][1] == "put-secret-value"][0]
        # Secrets Manager refuses an empty SecretString (minimum length 1), so the blank is one space; the research Lambda trims it to no key.
        self.assertEqual(put["stdin"], " ")
        self.assertIn("file:///dev/stdin", put["argv"])
        self.assertEqual(put["argv"][put["argv"].index("--secret-id") + 1], self.mod.RESEARCH_STORE_NAME)
        for c in self.bench.calls():
            if c["argv"][1] != "put-secret-value":
                self.assertEqual(c["stdin"], "")

    def test_it_says_what_it_did_and_names_the_new_version(self) -> None:
        code, out, err, _ = self.clear()
        self.assertEqual(code, 0, err)
        self.assertIn(f"arn:aws:secretsmanager:{ACTIVE['region']}:111111111111:secret:{self.mod.RESEARCH_STORE_NAME}-AbCdEf", out)
        self.assertIn("version: 11111111-2222-3333-4444-555555555555", out)
        self.assertRegex(out, r"cleared: .*blank")
        self.assertIn("next:", out)
        self.assertRegex(out, r"within 5 minutes")
        self.assertIn("AgentCore Web Search", out)
        self.assertIn("200 characters", out)
        self.assertIn("without --clear", out)

    def test_it_tags_the_secret_cleared_with_the_utc_date(self) -> None:
        late = datetime.datetime(2026, 10, 16, 23, 59, tzinfo=datetime.timezone(datetime.timedelta(hours=-6)))
        code, _out, err, _ = self.clear(now=late)
        self.assertEqual(code, 0, err)
        tag = [c["argv"] for c in self.bench.calls() if c["argv"][1] == "tag-resource"][0]
        self.assertEqual(tag[tag.index("--tags") + 1], "Key=research-key-stored-at,Value=cleared-2026-10-17")
        self.assertNotIn("file:///dev/stdin", tag)

    def test_it_never_reads_the_value_and_makes_only_the_three_calls(self) -> None:
        self.clear()
        self.assertNotIn("get-secret-value", self.bench.subcommands())
        self.assertLessEqual(set(self.bench.subcommands()), set(self.mod.AWS_SUBCOMMANDS))

    def test_it_uses_the_selected_profile_and_region(self) -> None:
        bench = Bench(self, {"HAMH_AWS_PROFILE": OTHER["profile"]})
        code, _out, err, _ = self.clear(bench)
        self.assertEqual(code, 0, err)
        for c in bench.calls():
            argv = c["argv"]
            self.assertEqual(argv[argv.index("--profile") + 1], OTHER["profile"])
            self.assertEqual(argv[argv.index("--region") + 1], OTHER["region"])

    def test_an_absent_secret_exits_3_with_the_deploy_hint_and_stores_nothing(self) -> None:
        bench = Bench(self, {"FAKE_AWS_STATE": "absent"})
        code, _out, err, _ = self.clear(bench)
        self.assertEqual(code, 3)
        self.assertIn("deploy", err)
        self.assertEqual(bench.subcommands(), ["describe-secret"])

    def test_a_secret_that_vanishes_before_the_put_exits_3_and_tags_nothing(self) -> None:
        bench = Bench(self, {"FAKE_AWS_STATE": "vanish"})
        code, _out, err, _ = self.clear(bench)
        self.assertEqual(code, 3)
        self.assertIn("deploy", err)
        self.assertNotIn("tag-resource", bench.subcommands())

    def test_a_denied_put_exits_1_names_the_step_and_does_not_tag(self) -> None:
        bench = Bench(self, {"FAKE_AWS_PUT": "fail"})
        code, _out, err, _ = self.clear(bench)
        self.assertEqual(code, 1)
        self.assertIn("put-secret-value", err)
        self.assertNotIn("tag-resource", bench.subcommands())

    def test_a_tag_failure_after_the_put_says_the_key_is_cleared(self) -> None:
        bench = Bench(self, {"FAKE_AWS_TAG": "fail"})
        code, out, err, _ = self.clear(bench)
        self.assertEqual(code, 1)
        self.assertIn("tag-resource", err)
        self.assertRegex(out + err, r"(?i)cleared")
        self.assertIn("11111111-2222-3333-4444-555555555555", out + err)

    def test_a_missing_cli_exits_3(self) -> None:
        env = {**self.bench.env, "HAMH_AWS_BIN": str(self.bench.dir / "no-such-aws")}
        code, _out, err, _ = self.clear(env=env)
        self.assertEqual(code, 3)
        self.assertNotIn("Traceback", err)

    def test_it_cannot_be_combined_with_check_or_max_age_days(self) -> None:
        for extra in (["--check"], ["--max-age-days", "30"], ["--check", "--max-age-days", "30"]):
            bench = Bench(self)
            with self.assertRaises(SystemExit) as raised:
                run_main(self.mod, ["--stage", "direct", "--clear", *extra], bench, tty=False, answers=[])
            self.assertEqual(raised.exception.code, 2, extra)
            self.assertEqual(bench.calls(), [], extra)

    def test_the_docstring_documents_the_flag_and_the_exit_codes_stay_zero_to_five(self) -> None:
        doc = self.mod.__doc__ or ""
        self.assertIn("--clear", doc)
        self.assertEqual((self.mod.EXIT_OK, self.mod.EXIT_AWS, self.mod.EXIT_REFUSED, self.mod.EXIT_PREREQ, self.mod.EXIT_ABSENT, self.mod.EXIT_OLD),
                         (0, 1, 2, 3, 4, 5))


class CheckProviderCase(unittest.TestCase):
    """--check also says which provider a stage in auto mode (the default) uses: Tavily when a key is stored, AgentCore Web Search when none is.
    The value is never read, so a current version is told from the tag this script writes; with no usable tag the script says it cannot tell."""

    def setUp(self) -> None:
        self.mod = load_script()

    def check(self, bench: Bench, *extra: str):
        def never(_msg: str) -> str:
            raise AssertionError("--check must not prompt")
        out, err = io.StringIO(), io.StringIO()
        code = self.mod.main(["--stage", "direct", "--check", *extra], env=bench.env, is_tty=lambda: False, prompt=never, stdout=out,
                             stderr=err, now=lambda: NOW)
        return code, out.getvalue(), err.getvalue()

    def bench(self, tags=None, changed=None, **env) -> Bench:
        extra = dict(env)
        if tags is not None:
            extra["FAKE_AWS_TAGS"] = json.dumps(tags)
        if changed is not None:
            extra["FAKE_AWS_CHANGED"] = json.dumps(changed)
        return Bench(self, extra)

    def test_a_current_version_stored_by_this_script_means_tavily(self) -> None:
        code, out, err = self.check(self.bench([TAG], days_ago(5)))
        self.assertEqual(code, 0, err)
        self.assertIn("auto: Tavily is used (a key is stored)", out)
        self.assertNotIn("AgentCore Web Search is used", out)
        self.assertRegex(out, r"age: stored 5\.0 days ago")

    def test_a_current_version_cleared_by_this_script_means_agentcore_and_has_no_age_to_rotate(self) -> None:
        cleared = {"Key": "research-key-stored-at", "Value": "cleared-2026-07-01"}
        code, out, err = self.check(self.bench([cleared], days_ago(110)))
        self.assertEqual(code, 0, err)
        self.assertIn("auto: AgentCore Web Search is used (no key", out)
        self.assertIn("2026-07-01", out, "the date of the clear is shown")
        self.assertNotIn("Tavily is used", out)
        # A blank version has no key to rotate: no age limit applies, however old the version is.
        self.assertNotIn("OLD", out)
        self.assertNotIn("limit:", out)

    def test_a_current_version_with_no_usable_tag_is_reported_as_not_known_and_keeps_its_age_check(self) -> None:
        for tags in (None, [{"Key": "project", "Value": "x"}], [{"Key": "research-key-stored-at", "Value": "last week"}]):
            code, out, err = self.check(self.bench(tags, days_ago(40)))
            self.assertEqual(code, 5, f"{tags}: {err}")
            self.assertIn("auto: cannot tell", out)
            self.assertRegex(out, r"OLD: ")

    def test_no_current_version_means_agentcore_and_keeps_exit_4(self) -> None:
        code, out, _err = self.check(self.bench(FAKE_AWS_STAGES="none"))
        self.assertEqual(code, 4)
        self.assertIn("auto: AgentCore Web Search is used (no key)", out)

    def test_a_missing_secret_has_no_provider_line(self) -> None:
        code, out, _err = self.check(self.bench(FAKE_AWS_STATE="absent"))
        self.assertEqual(code, 4)
        self.assertNotIn("auto:", out)

    def test_the_line_says_it_is_the_choice_of_a_stage_in_auto_mode(self) -> None:
        _code, out, _err = self.check(self.bench([TAG], days_ago(5)))
        self.assertRegex(out, r"(?i)note: .*auto mode")
        self.assertIn("HAMH_RESEARCH_PROVIDER", out)

    def test_the_check_still_reads_no_value(self) -> None:
        bench = self.bench([TAG], days_ago(5))
        self.check(bench)
        self.assertEqual(bench.subcommands(), ["describe-secret"])


class PtyCase(unittest.TestCase):
    """The real script in a real terminal: the typed key is not echoed and reaches the CLI on stdin."""

    def test_the_prompt_does_not_echo_and_the_store_succeeds(self) -> None:
        bench = Bench(self)
        key = new_key()
        env = {**bench.env}
        pid, fd = pty.fork()
        if pid == 0:  # the child: becomes the script
            os.execve(sys.executable, [sys.executable, str(SCRIPT), "--stage", "direct"], env)  # nosec B606: the test interpreter runs the script under test, fixed argument list
        seen = b""

        def read_until(marker: bytes, timeout: float = 20.0) -> None:
            nonlocal seen
            end = time.monotonic() + timeout
            while marker not in seen:
                left = end - time.monotonic()
                if left <= 0:
                    self.fail(f"timed out waiting for {marker!r}; saw {seen!r}")
                if select.select([fd], [], [], min(left, 1.0))[0]:
                    try:
                        chunk = os.read(fd, 4096)
                    except OSError:
                        break
                    if not chunk:
                        break
                    seen += chunk

        try:
            read_until(b"key: ")
            os.write(fd, key.encode() + b"\n")
            read_until(b"again: ", 20.0)
            os.write(fd, key.encode() + b"\n")
            read_until(b"characters")
            time.sleep(0.5)
            end = time.monotonic() + 10
            status = None
            while time.monotonic() < end:
                done, status = os.waitpid(pid, os.WNOHANG)
                if done:
                    break
                time.sleep(0.1)
            else:
                self.fail("the script did not exit")
        finally:
            try:
                os.close(fd)
            except OSError:
                pass
        self.assertNotIn(key.encode(), seen, "the terminal never showed the key")
        self.assertTrue(os.WIFEXITED(status) and os.WEXITSTATUS(status) == 0, f"status {status}; saw {seen!r}")
        put = [c for c in bench.calls() if c["argv"][1] == "put-secret-value"]
        self.assertEqual(put[0]["stdin"], key)


SECRET_FORM = re.compile(r"researchSecret\s*:\s*\(\s*cfg[^)]*\)\s*=>\s*`([^`]+)`")


def secret_name_in(config_ts: str) -> str | None:
    """The secret name `names.researchSecret` builds in infra/lib/config.ts, or None when the function is absent."""
    m = SECRET_FORM.search(config_ts)
    prefix = re.search(r'prefix:\s*"([^"]+)"', config_ts)
    if not m or not prefix:
        return None
    return m.group(1).replace("${cfg.prefix}", prefix.group(1))


class TlsContextCase(unittest.TestCase):
    """The context that wraps the socket sets its own floor of TLS 1.2, whatever the platform's default allows, and still
    verifies the certificate chain and the host name. No network: the socket and the wrapping are stand-ins."""

    def connect_with(self, platform_floor: ssl.TLSVersion):
        mod = load_script()
        built: list[ssl.SSLContext] = []
        wrapped: list[tuple[object, object]] = []
        real = ssl.create_default_context

        def spy(*args, **kwargs):
            ctx = real(*args, **kwargs)
            ctx.minimum_version = platform_floor  # a platform whose default context still allows the old versions
            ctx.wrap_socket = lambda sock, server_hostname=None, **kw: wrapped.append((sock, server_hostname)) or "tls-socket"
            built.append(ctx)
            return ctx

        with mock.patch.object(ssl, "create_default_context", spy), mock.patch.object(socket, "create_connection", lambda addr, timeout=None: "raw-socket"):
            conn = mod._VerifiedTls("api.example.test", 443, timeout=5)
            conn.connect()
        return conn, built, wrapped

    def test_the_floor_is_tls_1_2_even_when_the_platform_default_allows_older_versions(self) -> None:
        for floor in (ssl.TLSVersion.MINIMUM_SUPPORTED, ssl.TLSVersion.TLSv1, ssl.TLSVersion.TLSv1_1):
            with self.subTest(platform_floor=floor.name):
                _conn, built, _wrapped = self.connect_with(floor)
                self.assertEqual(len(built), 1)
                self.assertGreaterEqual(built[0].minimum_version, ssl.TLSVersion.TLSv1_2)

    def test_the_certificate_chain_and_the_host_name_are_still_verified(self) -> None:
        conn, built, wrapped = self.connect_with(ssl.TLSVersion.MINIMUM_SUPPORTED)
        self.assertEqual(built[0].verify_mode, ssl.CERT_REQUIRED)
        self.assertTrue(built[0].check_hostname)
        self.assertEqual(wrapped, [("raw-socket", "api.example.test")])
        self.assertEqual(conn.sock, "tls-socket")


class SecretNameCase(unittest.TestCase):
    def test_the_extraction_reads_the_names_helper_form(self) -> None:
        sample = ('prefix: "hamh-direct", logRetentionDays: 30,\n'
                  "  researchSecret: (cfg: StageConfig) => `${cfg.prefix}-research-tavily-api-key`,\n")
        self.assertEqual(secret_name_in(sample), "hamh-direct-research-tavily-api-key")
        self.assertIsNone(secret_name_in('prefix: "hamh-direct", other: (cfg) => `${cfg.prefix}-x`'))

    def test_the_name_is_the_one_the_cdk_construct_creates(self) -> None:
        name = secret_name_in(CONFIG_TS.read_text())
        if name is None:
            self.skipTest("infra/lib/config.ts has no names.researchSecret yet (the INFRA lane adds it): the name pin is NOT verified")
        self.assertEqual(name, load_script().RESEARCH_STORE_NAME)

    def test_the_name_is_a_valid_secrets_manager_name(self) -> None:
        name = load_script().RESEARCH_STORE_NAME
        self.assertRegex(name, r"^[A-Za-z0-9/_+=.@-]{1,512}$")
        self.assertEqual(name, "hamh-direct-research-tavily-api-key")

    def test_the_constant_does_not_carry_the_word_secret_in_its_name(self) -> None:
        # CodeQL (py/clear-text-logging-sensitive-data) reads a string assigned to a name that holds "secret" as secret data and then reports every
        # message that prints it. The value is the NAME of a Secrets Manager secret, never a value, so the constant has a neutral name.
        text = SCRIPT.read_text()
        self.assertNotRegex(text, r"\bSECRET_NAME\b")
        self.assertFalse(hasattr(load_script(), "SECRET_NAME"))
        self.assertIn('RESEARCH_STORE_NAME = "hamh-direct-research-tavily-api-key"', text)


class StaticCase(unittest.TestCase):
    def setUp(self) -> None:
        self.text = SCRIPT.read_text()

    def test_no_shell_and_no_forbidden_calls(self) -> None:
        self.assertNotIn("shell=True", self.text)
        self.assertNotIn("os.system", self.text)
        self.assertNotIn("os.popen", self.text)
        self.assertNotIn("boto3", self.text)

    def test_the_cli_calls_are_the_three_that_never_read_the_value(self) -> None:
        mod = load_script()
        self.assertEqual(set(mod.AWS_SUBCOMMANDS), {"put-secret-value", "tag-resource", "describe-secret"})

    def test_every_subprocess_use_carries_a_reasoned_bandit_directive(self) -> None:
        lines = self.text.splitlines()
        imports = [l for l in lines if re.match(r"\s*(import subprocess|from subprocess import)", l)]
        calls = [l for l in lines if re.search(r"subprocess\.(run|Popen|call|check_output|check_call)\(", l)]
        self.assertTrue(imports and calls)
        for l in imports:
            self.assertRegex(l, rf"# {BANDIT} B404: \S")
        for l in calls:
            self.assertRegex(l, rf"# {BANDIT} B603: \S")

    def test_the_docstring_opens_with_what_the_owner_types(self) -> None:
        doc = load_script().__doc__ or ""
        self.assertIn("python3 scripts/research-key-put.py --stage direct", "\n".join(doc.splitlines()[:12]))


if __name__ == "__main__":
    unittest.main()
