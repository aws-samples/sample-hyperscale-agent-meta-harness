"""Tests for scripts/setup/build-client.sh, the client build and publish step of the in-account runner.

`build-client.sh <out-dir> <bucket> <build-id>` cross-builds the terminal client and the operator tool beside it for every platform of the table in
`scripts/setup/names.sh` with `cargo zigbuild --release --locked --target <target> -p <client> --bin <client> -p <admin> --bin <admin>`, checks every
binary (`client_check.py`), writes `<out-dir>/<platform>/<client>`, `<out-dir>/<platform>/<admin>` and `<out-dir>/SHA256SUMS`, uploads them with `aws s3 cp` to
`s3://<bucket>/client/<build-id>/...` and prints ONE JSON object on stdout, `{"<platform>": {"key": ..., "sha256": ..., "bytes": ..., "admin": {"key": ...,
"sha256": ..., "bytes": ...}}, ...}`. Everything else goes to stderr. It uploads only after every binary of every platform was built and passed its check,
and exits 0 only then. A fake `cargo` writes the valid fixture binaries of `client_fixtures.py`
(or the damaged one a case asks for), a fake `aws` copies each uploaded file into a folder that stands for the bucket, and the real
`cargo-slot.sh` runs with its lock folder under the test's temp root. No case builds, reaches the network or starts a toolchain.

Run on a Linux machine with the tools pinned in `tools.lock`:
  $HAMH_PYTHON -m unittest discover -s scripts/tests -p test_build_client.py
"""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess  # nosec B404: tests start the script under test and sha256sum with argument lists, never a shell
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Dict, List, Optional

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(Path(__file__).resolve().parent))
import client_fixtures as fx  # noqa: E402

SCRIPT = REPO / "scripts" / "setup" / "build-client.sh"

TARGETS = {
    "darwin-arm64": "aarch64-apple-darwin",
    "linux-amd64": "x86_64-unknown-linux-musl",
    "linux-arm64": "aarch64-unknown-linux-musl",
}

FAKE_CARGO = r'''#!/usr/bin/env python3
import json, os, shutil, sys
args = sys.argv[1:]
with open(os.environ["FAKE_CARGO_LOG"], "a") as f:
    f.write(json.dumps({"args": args, "cwd": os.getcwd(), "target_dir": os.environ.get("CARGO_TARGET_DIR"),
                        "accounts_file": os.environ.get("HAMH_ACCOUNTS_FILE")}) + "\n")
target = args[args.index("--target") + 1]
binaries = [args[i + 1] for i, a in enumerate(args) if a == "--bin"]
if os.environ.get("FAKE_CARGO_FAIL_TARGET") == target:
    print("error[E0463]: can't find crate for `core` (%s)" % target, file=sys.stderr)
    sys.exit(101)
if os.environ.get("FAKE_CARGO_NOFILE_TARGET") == target:
    sys.exit(0)
dest_dir = os.path.join(os.environ["CARGO_TARGET_DIR"], target, "release")
os.makedirs(dest_dir, exist_ok=True)
for binary in binaries:
    if binary == os.environ.get("FAKE_CARGO_NOFILE_BIN"):
        continue
    kind = "ADMIN_" if binary.endswith("-admin") else ""
    suffix = ".admin" if kind else ""
    src = os.environ.get("FAKE_" + kind + "BIN_" + target.replace("-", "_").upper()) or os.path.join(os.environ["FAKE_FIXTURES"], target + suffix)
    shutil.copyfile(src, os.path.join(dest_dir, binary))
'''

FAKE_AWS = r'''#!/usr/bin/env python3
import json, os, shutil, sys
args = sys.argv[1:]
with open(os.environ["FAKE_AWS_LOG"], "a") as f:
    f.write(json.dumps(args) + "\n")
match = os.environ.get("FAKE_AWS_FAIL_MATCH")
if match and any(match in a for a in args):
    print("upload failed: An error occurred (AccessDenied) when calling the PutObject operation", file=sys.stderr)
    sys.exit(1)
if args[:2] == ["s3", "cp"]:
    src, dest = [a for a in args[2:] if not a.startswith("--")][:2]
    assert dest.startswith("s3://"), dest
    out = os.path.join(os.environ["FAKE_BUCKET_DIR"], dest[len("s3://"):])
    os.makedirs(os.path.dirname(out), exist_ok=True)
    shutil.copyfile(src, out)
    sys.exit(0)
print("fake aws: unexpected call %r" % (args,), file=sys.stderr)
sys.exit(97)
'''


class BuildClientCase(unittest.TestCase):
    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory(prefix="build-client-", dir=os.environ.get("TMPDIR"))
        self.addCleanup(self._td.cleanup)
        self.tmp = Path(self._td.name)
        self.bin = self.tmp / "bin"
        self.bin.mkdir()
        (self.bin / "cargo").write_text(FAKE_CARGO)
        (self.bin / "aws").write_text(FAKE_AWS)
        (self.bin / "cargo-zigbuild").write_text("#!/bin/sh\nexit 0\n")
        (self.bin / "zig").write_text("#!/bin/sh\necho 0.16.0\n")
        (self.bin / "rustc").write_text("#!/bin/sh\necho 'rustc 1.98.1-fake (0000000 2026-01-01)'\n")
        for p in self.bin.iterdir():
            p.chmod(0o755)
        self.fixtures = self.tmp / "fixtures"
        self.fixtures.mkdir()
        (self.fixtures / TARGETS["darwin-arm64"]).write_bytes(fx.macho_arm64(pages=5))
        (self.fixtures / TARGETS["linux-amd64"]).write_bytes(fx.elf64(fx.EM_X86_64, payload=b"amd64" * 50))
        (self.fixtures / TARGETS["linux-arm64"]).write_bytes(fx.elf64(fx.EM_AARCH64, payload=b"arm64" * 70))
        # the operator tool: other bytes than the client, so a mix-up of the two files shows
        (self.fixtures / (TARGETS["darwin-arm64"] + ".admin")).write_bytes(fx.macho_arm64(pages=3))
        (self.fixtures / (TARGETS["linux-amd64"] + ".admin")).write_bytes(fx.elf64(fx.EM_X86_64, payload=b"admin-amd64" * 40))
        (self.fixtures / (TARGETS["linux-arm64"] + ".admin")).write_bytes(fx.elf64(fx.EM_AARCH64, payload=b"admin-arm64" * 60))
        self.out = self.tmp / "out"
        self.bucket_dir = self.tmp / "bucket"
        self.bucket = "hamh-setup-123456789012-us-east-1"
        self.build_id = "hamh-setup-runner:0a1b2c3d-1111-2222-3333-444455556666"

    def run_script(self, *args: str, path: Optional[str] = None, drop: tuple = (), **env: str) -> subprocess.CompletedProcess:
        e = {
            "PATH": path or f"{self.bin}:{os.environ['PATH']}",
            "HOME": str(self.tmp),
            "TMPDIR": str(self.tmp),
            "CARGO_TARGET_DIR": str(self.tmp / "target"),
            "CARGO_SLOT_DIR": str(self.tmp / "locks"),
            "HAMH_PYTHON": sys.executable,
            "FAKE_CARGO_LOG": str(self.tmp / "cargo.log"),
            "FAKE_AWS_LOG": str(self.tmp / "aws.log"),
            "FAKE_FIXTURES": str(self.fixtures),
            "FAKE_BUCKET_DIR": str(self.bucket_dir),
            **env,
        }
        for name in drop:
            e.pop(name, None)
        return subprocess.run(["bash", str(SCRIPT), *args], capture_output=True, text=True, env=e, timeout=180)  # nosec B603: argument list, no shell, the script under test; B607: bash looked up on PATH as the other script tests do

    def run_default(self, **env: str) -> subprocess.CompletedProcess:
        return self.run_script(str(self.out), self.bucket, self.build_id, **env)

    def cargo_calls(self) -> List[dict]:
        log = self.tmp / "cargo.log"
        return [json.loads(ln) for ln in log.read_text().splitlines()] if log.is_file() else []

    def aws_calls(self) -> List[List[str]]:
        log = self.tmp / "aws.log"
        return [json.loads(ln) for ln in log.read_text().splitlines()] if log.is_file() else []

    def uploaded(self, key: str) -> Path:
        return self.bucket_dir / self.bucket / key

    def report(self, p: subprocess.CompletedProcess) -> Dict[str, dict]:
        self.assertEqual(len(p.stdout.splitlines()), 1, f"stdout must be one JSON line: {p.stdout!r}")
        return json.loads(p.stdout)

    # ---- the successful run

    def test_every_platform_is_built_checked_copied_summed_uploaded_and_reported(self) -> None:
        p = self.run_default()
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        doc = self.report(p)
        self.assertEqual(list(doc), list(TARGETS))
        for platform, target in TARGETS.items():
            data = (self.fixtures / target).read_bytes()
            f = self.out / platform / "ham"
            self.assertEqual(f.read_bytes(), data)
            self.assertTrue(os.access(f, os.X_OK))
            key = f"client/{self.build_id}/{platform}/ham"
            admin_data = (self.fixtures / (target + ".admin")).read_bytes()
            admin_key = f"client/{self.build_id}/{platform}/ham-admin"
            self.assertEqual(doc[platform], {"key": key, "sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data),
                                             "admin": {"key": admin_key, "sha256": hashlib.sha256(admin_data).hexdigest(), "bytes": len(admin_data)}})
            self.assertEqual(self.uploaded(key).read_bytes(), data)
            self.assertEqual((self.out / platform / "ham-admin").read_bytes(), admin_data)
            self.assertTrue(os.access(self.out / platform / "ham-admin", os.X_OK))
            self.assertEqual(self.uploaded(admin_key).read_bytes(), admin_data, "the operator tool is published beside the client")
        sums = (self.out / "SHA256SUMS").read_text()
        self.assertEqual(sums, "".join(f"{doc[pl]['sha256']}  {pl}/ham\n{doc[pl]['admin']['sha256']}  {pl}/ham-admin\n" for pl in TARGETS))
        self.assertEqual(self.uploaded(f"client/{self.build_id}/SHA256SUMS").read_text(), sums)
        chk = subprocess.run(["sha256sum", "-c", "SHA256SUMS"], cwd=self.out, capture_output=True, text=True)  # nosec B603: argument list, no shell; B607: sha256sum is the tool the user's checksum step mirrors, looked up on PATH
        self.assertEqual(chk.returncode, 0, chk.stdout + chk.stderr)
        self.assertEqual(p.stderr.strip().splitlines()[-1], f"BUILD-CLIENT PASS platforms=3 bucket={self.bucket} build={self.build_id}")

    def test_cargo_is_called_once_per_target_for_the_client_and_the_operator_tool_together(self) -> None:
        p = self.run_default()
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        calls = self.cargo_calls()
        self.assertEqual([c["args"] for c in calls],
                         [["zigbuild", "--release", "--locked", "--target", t, "-p", "ham", "--bin", "ham", "-p", "ham-admin", "--bin", "ham-admin"] for t in TARGETS.values()])
        for c in calls:
            self.assertEqual(c["cwd"], str(REPO))
            self.assertEqual(c["target_dir"], str(self.tmp / "target"))

    def test_the_environment_reaches_cargo_unchanged_so_a_generated_account_file_selects_the_account(self) -> None:
        accounts = self.tmp / "accounts-generated.json"
        accounts.write_text("{}")
        p = self.run_default(HAMH_ACCOUNTS_FILE=str(accounts))
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        self.assertEqual({c["accounts_file"] for c in self.cargo_calls()}, {str(accounts)})

    def test_the_uploads_are_s3_cp_calls_and_nothing_else_touches_aws(self) -> None:
        p = self.run_default()
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        calls = self.aws_calls()
        self.assertTrue(all(c[:2] == ["s3", "cp"] for c in calls), calls)
        dests = sorted(c[3] for c in calls)
        want = sorted([f"s3://{self.bucket}/client/{self.build_id}/{pl}/{name}" for pl in TARGETS for name in ("ham", "ham-admin")]
                      + [f"s3://{self.bucket}/client/{self.build_id}/SHA256SUMS"])
        self.assertEqual(dests, want)

    def test_progress_goes_to_stderr_and_stdout_is_the_json_alone(self) -> None:
        p = self.run_default()
        self.assertEqual(p.returncode, 0)
        json.loads(p.stdout)
        self.assertEqual(p.stdout.count("\n"), 1)
        for platform, target in TARGETS.items():
            self.assertRegex(p.stderr, rf"build-client: platform={platform} target={target} cargo zigbuild exit=0 seconds=\d+ log=")
            self.assertTrue((self.out / f"{platform}.build.log").is_file())

    def test_a_codebuild_build_id_with_a_colon_and_a_one_platform_table_work(self) -> None:
        table = "linux-arm64 aarch64-unknown-linux-musl elf-aarch64\n"
        p = self.run_default(HAMH_CLIENT_PLATFORM_TABLE=table)
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        self.assertEqual(list(self.report(p)), ["linux-arm64"])
        self.assertEqual(len(self.cargo_calls()), 1)
        self.assertEqual(len(self.aws_calls()), 3, "the client, the operator tool and the sums")

    def test_with_no_slot_folder_and_no_tools_folder_the_slots_live_beside_the_output_not_in_the_users_home(self) -> None:
        """cargo-slot.sh would otherwise make its lock folder below $HOME in the layout of the authors' build hosts."""
        p = self.run_default(drop=("CARGO_SLOT_DIR",))
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        self.assertTrue((self.out / ".cargo-slots").is_dir())
        self.assertEqual(sorted(x.name for x in self.tmp.iterdir() if x.name in ("workplace", ".cargo-slots")), [], "nothing is made below the home folder")

    def test_a_tools_folder_keeps_the_slots_where_cargo_slot_puts_them(self) -> None:
        tools = self.tmp / "tools"
        p = self.run_default(drop=("CARGO_SLOT_DIR",), HAMH_TOOLS=str(tools))
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        self.assertTrue((tools / "locks").is_dir())
        self.assertFalse((self.out / ".cargo-slots").exists())

    def test_the_client_name_comes_from_names_sh_so_the_rename_is_one_change(self) -> None:
        # a probe name that is not the default: the build uses the name names.sh gives, and nothing is built or written under the default name
        p = self.run_default(HAMH_CLIENT_BIN="widget")
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        for c in self.cargo_calls():
            self.assertEqual(c["args"][-8:], ["-p", "widget", "--bin", "widget", "-p", "widget-admin", "--bin", "widget-admin"])
        doc = self.report(p)
        for platform in TARGETS:
            self.assertEqual(doc[platform]["key"], f"client/{self.build_id}/{platform}/widget")
            self.assertEqual(doc[platform]["admin"]["key"], f"client/{self.build_id}/{platform}/widget-admin", "the operator tool is named after the client")
            self.assertTrue((self.out / platform / "widget").is_file())
            self.assertTrue((self.out / platform / "widget-admin").is_file())
            self.assertFalse((self.out / platform / "ham").exists())
            self.assertFalse((self.out / platform / "ham-admin").exists())
        self.assertIn("  darwin-arm64/widget\n", (self.out / "SHA256SUMS").read_text())
        self.assertIn("  darwin-arm64/widget-admin\n", (self.out / "SHA256SUMS").read_text())

    def test_the_operator_tool_name_can_be_set_apart_from_the_client_name(self) -> None:
        p = self.run_default(HAMH_ADMIN_BIN="ops")
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        for c in self.cargo_calls():
            self.assertEqual(c["args"][-8:], ["-p", "ham", "--bin", "ham", "-p", "ops", "--bin", "ops"])
        self.assertEqual(self.report(p)["linux-amd64"]["admin"]["key"], f"client/{self.build_id}/linux-amd64/ops")

    def test_an_earlier_run_in_the_same_out_dir_is_replaced_not_merged(self) -> None:
        (self.out / "darwin-arm64").mkdir(parents=True)
        (self.out / "darwin-arm64" / "ham").write_bytes(b"old")
        (self.out / "SHA256SUMS").write_text("stale\n")
        p = self.run_default()
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        self.assertEqual((self.out / "darwin-arm64" / "ham").read_bytes(), (self.fixtures / TARGETS["darwin-arm64"]).read_bytes())
        self.assertNotIn("stale", (self.out / "SHA256SUMS").read_text())

    # ---- failures: nothing is published, stdout stays empty

    def assert_failed_before_publishing(self, p: subprocess.CompletedProcess, platform: str, reason: str) -> None:
        self.assertEqual(p.returncode, 1, p.stdout + p.stderr)
        self.assertEqual(p.stdout, "", "a failed run prints no JSON")
        self.assertEqual(self.aws_calls(), [], "nothing is uploaded unless every platform built and passed its check")
        last = p.stderr.strip().splitlines()[-1]
        self.assertTrue(last.startswith(f"BUILD-CLIENT FAIL {platform} {reason}"), last)

    def test_a_failed_build_names_the_platform_the_exit_and_the_log(self) -> None:
        p = self.run_default(FAKE_CARGO_FAIL_TARGET=TARGETS["linux-arm64"])
        self.assert_failed_before_publishing(p, "linux-arm64", "build exit=101")
        self.assertIn(f"(see {self.out}/linux-arm64.build.log)", p.stderr)
        self.assertIn("can't find crate", (self.out / "linux-arm64.build.log").read_text())
        self.assertIn("error[E0463]", p.stderr)

    def test_a_first_platform_failure_stops_before_the_others_are_built(self) -> None:
        p = self.run_default(FAKE_CARGO_FAIL_TARGET=TARGETS["darwin-arm64"])
        self.assert_failed_before_publishing(p, "darwin-arm64", "build exit=101")
        self.assertEqual(len(self.cargo_calls()), 1)

    def test_a_darwin_binary_without_a_code_signature_stops_the_run(self) -> None:
        bad = self.tmp / "unsigned"
        bad.write_bytes(fx.macho_arm64(sign=False))
        p = self.run_default(FAKE_BIN_AARCH64_APPLE_DARWIN=str(bad))
        self.assert_failed_before_publishing(p, "darwin-arm64", "no-code-signature")
        self.assertIn("Apple Silicon", p.stderr)

    def test_a_linux_binary_for_the_wrong_machine_stops_the_run(self) -> None:
        bad = self.tmp / "wrongarch"
        bad.write_bytes(fx.elf64(fx.EM_AARCH64))
        p = self.run_default(FAKE_BIN_X86_64_UNKNOWN_LINUX_MUSL=str(bad))
        self.assert_failed_before_publishing(p, "linux-amd64", "wrong-machine")

    def test_a_dynamic_linux_binary_stops_the_run(self) -> None:
        bad = self.tmp / "dynamic"
        bad.write_bytes(fx.elf64(fx.EM_X86_64, interp=True))
        p = self.run_default(FAKE_BIN_X86_64_UNKNOWN_LINUX_MUSL=str(bad))
        self.assert_failed_before_publishing(p, "linux-amd64", "dynamic-interpreter")

    def test_a_darwin_operator_tool_without_a_code_signature_stops_the_run_like_the_client(self) -> None:
        bad = self.tmp / "unsigned-admin"
        bad.write_bytes(fx.macho_arm64(sign=False))
        p = self.run_default(FAKE_ADMIN_BIN_AARCH64_APPLE_DARWIN=str(bad))
        self.assert_failed_before_publishing(p, "darwin-arm64", "ham-admin no-code-signature")
        self.assertIn("Apple Silicon", p.stderr)

    def test_a_dynamic_linux_operator_tool_stops_the_run_like_the_client(self) -> None:
        bad = self.tmp / "dynamic-admin"
        bad.write_bytes(fx.elf64(fx.EM_AARCH64, interp=True))
        p = self.run_default(FAKE_ADMIN_BIN_AARCH64_UNKNOWN_LINUX_MUSL=str(bad))
        self.assert_failed_before_publishing(p, "linux-arm64", "ham-admin dynamic-interpreter")

    def test_an_operator_tool_for_the_wrong_machine_stops_the_run(self) -> None:
        bad = self.tmp / "wrongarch-admin"
        bad.write_bytes(fx.elf64(fx.EM_AARCH64))
        p = self.run_default(FAKE_ADMIN_BIN_X86_64_UNKNOWN_LINUX_MUSL=str(bad))
        self.assert_failed_before_publishing(p, "linux-amd64", "ham-admin wrong-machine")

    def test_a_cargo_that_leaves_out_the_operator_tool_is_a_failure(self) -> None:
        p = self.run_default(FAKE_CARGO_NOFILE_BIN="ham-admin")
        self.assert_failed_before_publishing(p, "darwin-arm64", "ham-admin missing-binary")

    def test_a_cargo_that_succeeds_without_writing_the_binary_is_a_failure(self) -> None:
        p = self.run_default(FAKE_CARGO_NOFILE_TARGET=TARGETS["linux-amd64"])
        self.assert_failed_before_publishing(p, "linux-amd64", "missing-binary")

    def test_an_upload_failure_names_the_key_prints_no_json_and_fails(self) -> None:
        p = self.run_default(FAKE_AWS_FAIL_MATCH="linux-arm64")
        self.assertEqual(p.returncode, 1, p.stdout + p.stderr)
        self.assertEqual(p.stdout, "")
        last = p.stderr.strip().splitlines()[-1]
        self.assertTrue(last.startswith(f"BUILD-CLIENT FAIL upload client/{self.build_id}/linux-arm64/ham"), last)
        self.assertIn("AccessDenied", p.stderr)

    def test_an_upload_failure_of_the_operator_tool_names_its_key_and_fails(self) -> None:
        p = self.run_default(FAKE_AWS_FAIL_MATCH="linux-amd64/ham-admin")
        self.assertEqual((p.returncode, p.stdout), (1, ""), p.stderr)
        last = p.stderr.strip().splitlines()[-1]
        self.assertTrue(last.startswith(f"BUILD-CLIENT FAIL upload client/{self.build_id}/linux-amd64/ham-admin"), last)

    def test_a_failed_sums_upload_fails_the_run(self) -> None:
        p = self.run_default(FAKE_AWS_FAIL_MATCH="SHA256SUMS")
        self.assertEqual((p.returncode, p.stdout), (1, ""), p.stderr)
        self.assertIn("BUILD-CLIENT FAIL upload client/", p.stderr.strip().splitlines()[-1])

    # ---- tools and usage

    def base_path(self, drop: str) -> str:
        """A PATH of the fake tools and the base system tools, without the tool `drop`."""
        base = self.tmp / "base"
        base.mkdir(exist_ok=True)
        for tool in ("bash", "od", "tr", "sha256sum", "shasum", "cut", "git", "date", "mkdir", "rm", "cp", "mv", "chmod", "grep", "head", "tail", "sed",
                     "awk", "dirname", "basename", "flock", "env", "wc", "cat", "uname", "touch", "sort", "printf", "ls", "readlink", "pwd", "python3", "true"):
            found = shutil.which(tool)
            if found and tool != drop:
                link = base / tool
                if not link.exists():
                    link.symlink_to(found)
        return f"{self.bin}:{base}"

    def test_a_missing_tool_fails_before_any_build(self) -> None:
        for tool in ("cargo-zigbuild", "zig", "aws", "cargo"):
            with self.subTest(tool=tool):
                hidden = self.bin / tool
                saved = hidden.read_text()
                hidden.unlink()
                try:
                    p = self.run_default(path=self.base_path(tool))
                finally:
                    hidden.write_text(saved)
                    hidden.chmod(0o755)
                self.assertEqual(p.returncode, 1, p.stdout + p.stderr)
                self.assertEqual(p.stdout, "")
                self.assertEqual(p.stderr.strip().splitlines()[-1], f"BUILD-CLIENT FAIL missing-{tool}")
                self.assertEqual(self.cargo_calls(), [])
                self.assertEqual(self.aws_calls(), [])

    def test_a_checker_that_dies_without_output_is_a_failure_that_says_so(self) -> None:
        crash = self.tmp / "crashing-python"
        crash.write_text('#!/bin/sh\nif [ "$1" = "-c" ]; then exit 0; fi\nexit 3\n')  # passes the interpreter probe, dies on the checker
        crash.chmod(0o755)
        p = self.run_default(HAMH_PYTHON=str(crash))
        self.assert_failed_before_publishing(p, "darwin-arm64", "check-error: client_check.py exited 3")

    def test_no_python_is_a_missing_tool_before_any_build(self) -> None:
        p = self.run_default(path=self.base_path("python3"), HAMH_PYTHON="/nonexistent/python")
        self.assertEqual((p.returncode, p.stdout), (1, ""), p.stderr)
        self.assertEqual(p.stderr.strip().splitlines()[-1], "BUILD-CLIENT FAIL missing-python")
        self.assertEqual(self.cargo_calls(), [])

    def test_usage_errors_exit_2_before_any_build_or_upload(self) -> None:
        good = [str(self.out), self.bucket, self.build_id]
        cases = [
            [],
            good[:2],
            good + ["extra"],
            ["relative/out", self.bucket, self.build_id],
            [str(self.out), "Bad_Bucket", self.build_id],
            [str(self.out), "ab", self.build_id],
            [str(self.out), "bucket/with/slash", self.build_id],
            [str(self.out), "bucket name", self.build_id],
            [str(self.out), self.bucket, ""],
            [str(self.out), self.bucket, "a/b"],
            [str(self.out), self.bucket, "has space"],
            [str(self.out), self.bucket, ".."],
            [str(self.out), self.bucket, "x;y"],
        ]
        for args in cases:
            with self.subTest(args=args):
                p = self.run_script(*args)
                self.assertEqual(p.returncode, 2, (args, p.stdout, p.stderr))
                self.assertEqual(p.stdout, "")
                self.assertIn("usage", p.stderr)
        self.assertEqual(self.cargo_calls(), [])
        self.assertEqual(self.aws_calls(), [])

    def test_an_unknown_platform_in_a_replaced_table_is_a_usage_error(self) -> None:
        p = self.run_default(HAMH_CLIENT_PLATFORM_TABLE="linux-amd64 x86_64-unknown-linux-musl\n")
        self.assertEqual(p.returncode, 2, p.stdout + p.stderr)
        self.assertEqual(self.cargo_calls(), [])

    def test_the_script_runs_the_same_from_any_directory(self) -> None:
        other = self.tmp / "elsewhere"
        other.mkdir()
        e = {"PATH": f"{self.bin}:{os.environ['PATH']}", "HOME": str(self.tmp), "TMPDIR": str(self.tmp), "CARGO_TARGET_DIR": str(self.tmp / "target"),
             "CARGO_SLOT_DIR": str(self.tmp / "locks"), "HAMH_PYTHON": sys.executable, "FAKE_CARGO_LOG": str(self.tmp / "cargo.log"),
             "FAKE_AWS_LOG": str(self.tmp / "aws.log"), "FAKE_FIXTURES": str(self.fixtures), "FAKE_BUCKET_DIR": str(self.bucket_dir)}
        p = subprocess.run(["bash", str(SCRIPT), str(self.out), self.bucket, self.build_id], capture_output=True, text=True, env=e, cwd=other, timeout=180)  # nosec B603: argument list, no shell; B607: bash looked up on PATH as the other script tests do
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        self.assertEqual({c["cwd"] for c in self.cargo_calls()}, {str(REPO)})


class StaticCase(unittest.TestCase):
    TEXT = SCRIPT.read_text(encoding="utf-8") if SCRIPT.is_file() else ""

    def test_the_script_exists_is_executable_and_documents_its_usage_and_exit_codes(self) -> None:
        self.assertTrue(SCRIPT.is_file(), "scripts/setup/build-client.sh is missing")
        self.assertTrue(os.access(SCRIPT, os.X_OK))
        head = "\n".join(self.TEXT.splitlines()[:60])
        for needle in ("usage:", "stdout", "exit", "BUILD-CLIENT PASS", "BUILD-CLIENT FAIL", "SHA256SUMS", "client/<build-id>"):
            self.assertIn(needle, head)

    def test_the_script_sources_the_names_and_runs_the_slot_wrapper(self) -> None:
        self.assertRegex(self.TEXT, r'(?m)^\s*\.\s+"\$here/names\.sh"')
        self.assertIn("cargo-slot.sh", self.TEXT)


if __name__ == "__main__":
    unittest.main()
