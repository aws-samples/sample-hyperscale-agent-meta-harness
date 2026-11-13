"""The pin of the CDK bootstrap template: `engine/policy/cdk-bootstrap-template.pin`.

A fresh account has no CDK bootstrap stack, and the runner's role is not allowed to make one, so `setup.sh` makes it with the user's own credentials and the AWS CLI
alone, from a template. The template is the output of `cdk bootstrap --show-template` of the aws-cdk version `tools.lock` pins. It is not committed (the scanners of
the repository report 31 findings on it, and a third-party artifact is fetched, not copied): the runner prints it with the pinned CDK CLI in its own build
(`SETUP_ACTION=bootstrap-template`), puts it in the setup bucket, and `setup.sh` deploys it only when its SHA-256 is the one in the pin. The pin is the repository's
control over what the user's credentials deploy.

  format    the pin is `#` comment lines and two lines, `awsCdk=<version>` and `sha256=<64 hex>`, which `setup.sh` reads with grep and sed
  version   `awsCdk` is the aws-cdk version of `tools.lock` ([node] aws_cdk); a bump of the CDK has to prove the new hash (the live case below) in the same change
  live      opt-in (`HAMH_BUILD_TESTS=1`, a build host with Docker): the pinned CDK CLI, run through scripts/node.sh, prints a template of that SHA-256, under the 51,200
            bytes that `aws cloudformation deploy` accepts without a staging bucket

Run on a Linux machine with the tools pinned in `tools.lock`:
  $HAMH_PYTHON -m unittest discover -s scripts/tests -p test_cdk_bootstrap_pin.py
"""

from __future__ import annotations

import hashlib
import os
import re
import shutil
import stat
import subprocess  # nosec B404: tests start the scripts under test with argument lists, never a shell
import tempfile
import tomllib
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
PIN = REPO / "engine" / "policy" / "cdk-bootstrap-template.pin"
MAX_BYTES = 51200


def read_pin() -> dict:
    out = {}
    for line in PIN.read_text(encoding="utf-8").splitlines():
        if line.strip() and not line.startswith("#"):
            key, _, value = line.partition("=")
            out[key.strip()] = value.strip()
    return out


class PinFormatCase(unittest.TestCase):
    def setUp(self) -> None:
        if not PIN.is_file():
            self.fail("engine/policy/cdk-bootstrap-template.pin does not exist")

    def test_the_pin_is_comments_and_two_lines(self) -> None:
        lines = [ln for ln in PIN.read_text(encoding="utf-8").splitlines() if ln.strip() and not ln.startswith("#")]
        self.assertEqual(len(lines), 2, lines)
        self.assertEqual(sorted(read_pin()), ["awsCdk", "sha256"])

    def test_the_hash_is_64_lowercase_hex_digits(self) -> None:
        self.assertRegex(read_pin()["sha256"], r"^[0-9a-f]{64}$")

    def test_the_version_is_the_cdk_the_repository_pins(self) -> None:
        with open(REPO / "tools.lock", "rb") as f:
            locked = tomllib.load(f)["node"]["aws_cdk"]
        self.assertEqual(read_pin()["awsCdk"], locked, "a new aws-cdk in tools.lock needs the SHA-256 of its template here (run the live case with HAMH_BUILD_TESTS=1)")

    def test_setup_sh_reads_the_pin_with_tools_a_stock_mac_has(self) -> None:
        for name in ("grep", "sed", "cut"):
            self.assertIsNotNone(shutil.which(name), name)
        got = subprocess.run(["sed", "-n", "s/^sha256=//p", str(PIN)], capture_output=True, text=True, timeout=30)  # nosec B603: argument list, no shell; B607: sed looked up on PATH, the tool setup.sh reads the pin with
        self.assertEqual(got.stdout.strip(), read_pin()["sha256"])


@unittest.skipUnless(os.environ.get("HAMH_BUILD_TESTS") == "1" and shutil.which("docker"), "set HAMH_BUILD_TESTS=1 on a build host with Docker: it runs the pinned CDK CLI in its container")
class LiveCase(unittest.TestCase):
    def test_the_pinned_cdk_prints_the_pinned_template(self) -> None:
        with tempfile.TemporaryDirectory(prefix="bootstrap-pin-", dir=os.environ.get("TMPDIR")) as td:
            creds = Path(td) / "creds.sh"
            # scripts/node.sh wants a credentials command; `cdk bootstrap --show-template` makes no AWS call, so the values are placeholders
            creds.write_text('#!/bin/sh\nprintf \'%s\\n\' \'{"Version":1,"AccessKeyId":"placeholder","SecretAccessKey":"placeholder"}\'\n')
            creds.chmod(creds.stat().st_mode | stat.S_IXUSR)
            env = dict(os.environ, NODE_SH_CREDS_CMD=str(creds))
            done = subprocess.run(["bash", str(REPO / "scripts" / "node.sh"), "cdk", "bootstrap", "--show-template"], capture_output=True, env=env, timeout=1800, cwd=str(REPO))  # nosec B603: argument list, no shell, the repository's own wrapper; B607: bash looked up on PATH as the other script tests do
            self.assertEqual(done.returncode, 0, done.stderr.decode(errors="replace")[-2000:])
            self.assertLessEqual(len(done.stdout), MAX_BYTES, "aws cloudformation deploy takes no larger template without a staging bucket")
            self.assertGreater(len(done.stdout), 10000)
            self.assertEqual(hashlib.sha256(done.stdout).hexdigest(), read_pin()["sha256"], "the pinned CDK printed another template: prove it and update the pin")
            self.assertIsNotNone(re.search(rb"/cdk-bootstrap/\$\{Qualifier\}/version", done.stdout))


if __name__ == "__main__":
    unittest.main()
