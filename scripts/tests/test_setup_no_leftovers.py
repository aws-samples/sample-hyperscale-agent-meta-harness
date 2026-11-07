"""`setup.sh` leaves no temporary file behind, and `aws_run` keeps the answer and the error text apart without one (reader finding L1).

`aws_run` of `scripts/setup/lib.sh` used to send the AWS CLI's error text to `${TMPDIR:-/tmp}/hamh-setup-err.<pid>`, a name anyone on the machine could predict, and never removed it.
It now takes the answer and the error text from the two output streams of the call itself. The helper files delete nothing (`test_setup_full_static.py`), so the fix is to make no file.

Cases:

  AwsRunCase       `aws_run` against a stand-in AWS CLI, compared with the earlier file-based version on the same calls: the answer, the error text and the exit code;
                   trailing newlines, an empty answer, an answer with no final newline, a warning on stderr beside an answer, a large answer, a failing call;
                   the call makes no file in the temporary folder.
  NoLeftoversCase  whole runs of the full setup with a folder of their own as TMPDIR: `--dry-run`, `--preflight-only`, a successful setup, a setup that fails, `--status`,
                   `--doctor`, and a setup that is interrupted with Ctrl-C and one that is ended with SIGTERM while it follows a build. The folder is empty afterwards.

Run with:
  $HAMH_PYTHON -m unittest discover -s scripts/tests -p test_setup_no_leftovers.py
"""

from __future__ import annotations

import os
import signal
import subprocess  # nosec B404: tests start bash and the script under test with argument lists, never a shell
import unittest
from pathlib import Path

from setup_full_support import FullBase
from setup_waits import limit
from test_setup_script import BASH, REPO

LIB = REPO / "scripts" / "setup" / "lib.sh"

# A stand-in AWS CLI whose answer, error text and exit code the test sets: STUB_OUT and STUB_ERR are printf formats (so \n and \000-free escapes work), STUB_RC the exit code.
STUB_AWS = r"""#!/bin/sh
[ -z "${STUB_OUT_FILE:-}" ] || cat "$STUB_OUT_FILE"
[ -z "${STUB_OUT:-}" ] || printf "$STUB_OUT"
[ -z "${STUB_ERR:-}" ] || printf "$STUB_ERR" >&2
exit "${STUB_RC:-0}"
"""

# The version of aws_run before the fix, kept here to compare behavior: the error text went through a file.
OLD_AWS_RUN = r"""
old_aws_run() {
  OLD_ERRF=$1; shift
  OLD_OUT=$(awsx "$@" 2>"$OLD_ERRF")
  OLD_RC=$?
  OLD_ERR=$(cat "$OLD_ERRF" 2>/dev/null)
  return "$OLD_RC"
}
"""


class AwsRunCase(unittest.TestCase):
    def setUp(self) -> None:
        import tempfile

        td = tempfile.TemporaryDirectory(prefix="aws-run-", dir=os.environ.get("TMPDIR"))
        self.addCleanup(td.cleanup)
        self.tmp = Path(td.name).resolve()
        self.scratch = self.tmp / "scratch"
        self.scratch.mkdir()
        self.aws = self.tmp / "aws"
        self.aws.write_text(STUB_AWS)
        self.aws.chmod(0o755)

    def run_both(self, **stub: str) -> dict:
        script = f"""set -u
. "{LIB}"
{OLD_AWS_RUN}
REGION=us-east-1; PROFILE_EFF=""; AWS_CLI_PROG="{self.aws}"
old_aws_run "{self.tmp}/old-err" sts get-caller-identity; old_rc=$?
aws_run sts get-caller-identity; new_rc=$?
printf '%s' "$old_rc" > "{self.tmp}/old_rc"; printf '%s' "$OLD_OUT" > "{self.tmp}/old_out"; printf '%s' "$OLD_ERR" > "{self.tmp}/old_err"
printf '%s' "$new_rc" > "{self.tmp}/new_rc"; printf '%s' "$AWS_OUT" > "{self.tmp}/new_out"; printf '%s' "$AWS_ERR" > "{self.tmp}/new_err"; printf '%s' "$AWS_RC" > "{self.tmp}/new_aws_rc"
"""
        env = {"PATH": os.environ["PATH"], "HOME": str(self.tmp), "TMPDIR": str(self.scratch), "LC_ALL": "C", **stub}
        p = subprocess.run([BASH, "-c", script], capture_output=True, text=True, env=env, timeout=limit(60))  # nosec B603: argument list, no shell, the interpreter and a repository script
        self.assertEqual(p.returncode, 0, p.stderr)
        got = {name: (self.tmp / name).read_bytes().decode() for name in ("old_rc", "old_out", "old_err", "new_rc", "new_out", "new_err", "new_aws_rc")}
        got["scratch"] = sorted(os.listdir(self.scratch))
        return got

    def assert_same(self, got: dict, out: str, err: str, rc: str) -> None:
        self.assertEqual((got["old_out"], got["old_err"], got["old_rc"]), (out, err, rc), "the earlier version answers as the case says")
        self.assertEqual((got["new_out"], got["new_err"], got["new_rc"], got["new_aws_rc"]), (out, err, rc, rc))
        self.assertEqual(got["scratch"], [], "aws_run made a file in the temporary folder")

    def test_an_answer(self) -> None:
        self.assert_same(self.run_both(STUB_OUT="123456789012\\n"), "123456789012", "", "0")

    def test_an_error_text_and_the_exit_code(self) -> None:
        self.assert_same(self.run_both(STUB_ERR="An error occurred (ParameterNotFound) when calling the GetParameter operation: \\n", STUB_RC="254"), "",
                         "An error occurred (ParameterNotFound) when calling the GetParameter operation: ", "254")

    def test_a_warning_beside_an_answer_stays_out_of_the_answer(self) -> None:
        self.assert_same(self.run_both(STUB_OUT="one\\ttwo\\n", STUB_ERR="warning: a newer version exists\\n"), "one\ttwo", "warning: a newer version exists", "0")

    def test_trailing_newlines_go_and_inner_lines_stay(self) -> None:
        self.assert_same(self.run_both(STUB_OUT="a\\n\\nb\\n\\n\\n", STUB_ERR="x\\n\\ny\\n\\n"), "a\n\nb", "x\n\ny", "0")

    def test_no_output_at_all(self) -> None:
        self.assert_same(self.run_both(), "", "", "0")

    def test_an_answer_with_no_final_newline(self) -> None:
        self.assert_same(self.run_both(STUB_OUT="None"), "None", "", "0")

    def test_a_tab_separated_row_and_spaces_are_kept(self) -> None:
        self.assert_same(self.run_both(STUB_OUT="  lead\\tmid  \\ttail  \\n"), "  lead\tmid  \ttail  ", "", "0")

    def test_a_large_answer_and_a_large_error_text(self) -> None:
        big = self.tmp / "big"
        big.write_text("".join(f"line {i} of a long answer\n" for i in range(20000)))
        got = self.run_both(STUB_OUT_FILE=str(big), STUB_ERR="e\\n" * 5000, STUB_RC="1")
        self.assert_same(got, big.read_text().rstrip("\n"), ("e\n" * 5000).rstrip("\n"), "1")

    def test_non_ascii_text(self) -> None:
        self.assert_same(self.run_both(STUB_OUT="caf\\303\\251\\n", STUB_ERR="\\303\\274ber\\n"), "caf\u00e9", "\u00fcber", "0")

    def test_the_variables_of_an_earlier_call_do_not_leak_into_the_next(self) -> None:
        script = f"""set -u
. "{LIB}"
REGION=us-east-1; PROFILE_EFF=""; AWS_CLI_PROG="{self.aws}"
STUB_OUT='' STUB_ERR='bad\\n' STUB_RC=3 aws_run x || true
first="$AWS_OUT|$AWS_ERR|$AWS_RC"
STUB_OUT='fine\\n' STUB_ERR='' STUB_RC=0 aws_run x
printf '%s|%s|%s|%s' "$first" "$AWS_OUT" "$AWS_ERR" "$AWS_RC"
"""
        p = subprocess.run([BASH, "-c", script], capture_output=True, text=True, timeout=limit(60),  # nosec B603: argument list, no shell
                           env={"PATH": os.environ["PATH"], "HOME": str(self.tmp), "TMPDIR": str(self.scratch), "STUB_OUT": "", "STUB_ERR": ""})
        self.assertEqual((p.returncode, p.stdout), (0, "|bad|3|fine||0"), p.stderr)
        self.assertEqual(os.listdir(self.scratch), [])


class NoLeftoversCase(FullBase):
    def setUp(self) -> None:
        super().setUp()
        self.scratch = self.tmp / "own-tmp"
        self.scratch.mkdir()

    def left(self) -> list:
        return sorted(os.listdir(self.scratch))

    def assert_clean(self, label: str) -> None:
        self.assertEqual(self.left(), [], f"{label}: files left in TMPDIR")

    def run_clean(self, *args: str, **env: str) -> subprocess.CompletedProcess:
        return self.run_full(*args, TMPDIR=str(self.scratch), **env)

    def test_a_dry_run(self) -> None:
        p = self.run_clean("--full", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertGreaterEqual(self.count_calls("sts", "get-caller-identity"), 1, "TRIGGER PRESENT: the run called the AWS CLI")
        self.assert_clean("--dry-run")

    def test_preflight_only(self) -> None:
        p = self.run_clean("--full", "--profile", "work", "--region", "us-east-1", "--preflight-only")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertGreaterEqual(self.count_calls("sts", "get-caller-identity"), 1)
        self.assert_clean("--preflight-only")

    def test_a_successful_setup(self) -> None:
        p = self.run_clean(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertGreaterEqual(self.count_calls("codebuild", "start-build"), 1)
        self.assert_clean("a successful setup")

    def test_a_setup_that_fails(self) -> None:
        self.set_scenario(build={"final": "FAILED", "result": "fail", "failed_phase": "deploy", "detail": "DEPLOY-FAIL deploy cdk deploy hamh-direct-network failed"})
        p = self.run_clean(*self.ARGS)
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assert_clean("a failed setup")

    def test_a_refused_setup(self) -> None:
        self.set_scenario(faults=[{"match": "deploy --stack-name hamh-setup-runner", "exit": 254, "stderr": "An error occurred (AccessDenied) when calling the CreateStack operation: iam:CreateRole"}])
        p = self.run_clean(*self.ARGS)
        self.assertNotEqual(p.returncode, 0, self.out(p))
        self.assert_clean("a setup the account refuses")

    def test_status_and_doctor(self) -> None:
        self.assertEqual(self.run_clean(*self.ARGS).returncode, 0)
        for cmd in ("--status", "--doctor"):
            p = self.run_clean(cmd, "--profile", "work", "--region", "us-east-1")
            self.assertIn(p.returncode, (0, 3), self.out(p))
            self.assert_clean(cmd)

    def follow_and_signal(self, sig: signal.Signals) -> None:
        self.set_scenario(build={"hang": True})
        proc = self.popen(*self.ARGS, TMPDIR=str(self.scratch), HAMH_SETUP_POLL_SECONDS="1")
        self.follow_started(proc)
        done = proc.interrupt(sig)
        self.assertNotEqual(done.returncode, 0, done.output)
        self.assert_clean(f"setup ended with {sig.name}")

    def test_ctrl_c_while_following_a_build(self) -> None:
        self.follow_and_signal(signal.SIGINT)

    def test_sigterm_while_following_a_build(self) -> None:
        self.follow_and_signal(signal.SIGTERM)


if __name__ == "__main__":
    unittest.main()
