"""Tests for scripts/lib/build_selection.py: which accounts.json entry a Rust build was made for.

The library keys every build directory by the selection and checks, before anything is published, what the built files say
they were built for. Fake binaries carry the constants the measured real ones do (selection_fixture.py); nothing here
builds Rust.

The contract:
- the cargo target directory is `<root>/target/deploy/<account>-<region>`, the Lambda output directory
  `<target dir>/lambda/<stage>`;
- `repo-gateway` must embed the selected entry's account id, `agentd` its
  Region; every other binary is left to the build-output check, because a real build of it carries neither
  in a form a byte scan tells apart (measured);
- a driver (`ham-e2e`, `hamh-dispatch`) must embed the selected account id and no other entry's;
- the harness-protocol build script output (`aws_target.rs`) in a target directory must name the selected entry;
  the newest one of a build root decides;
- a mismatch is a `Mismatch` that names the binary and both selections.
"""

from __future__ import annotations

import os
import subprocess  # nosec B404: tests start the scripts under test and git with argument lists, never a shell
import sys
import tempfile
import time
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "scripts" / "lib"))
sys.path.insert(0, str(Path(__file__).resolve().parent))
import build_selection as bs  # noqa: E402
import digests  # noqa: E402
import selection_fixture as sf  # noqa: E402

ACCT_B2 = sf.entry("history-profile")
ACCT_A1 = sf.entry("my-profile")
ENTRIES = sf.entries()
CLI = REPO / "scripts" / "lib" / "build_selection.py"


def git(cwd: Path, *args: str) -> str:
    env = dict(os.environ, GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@example.com",
               GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@example.com", GIT_CONFIG_GLOBAL="/dev/null",
               GIT_CONFIG_NOSYSTEM="1")
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True, env=env).stdout.strip()  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do


class Base(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="build-selection-"))

    def write(self, name: str, data: bytes) -> Path:
        p = self.tmp / name
        p.write_bytes(data)
        return p


class LayoutCase(Base):
    def test_the_target_directory_is_keyed_by_account_and_region_under_target_deploy(self) -> None:
        root = Path("/repo")
        self.assertEqual(bs.target_dir(root, ACCT_A1), root / "target" / "deploy" / "222222222222-us-east-1")
        self.assertEqual(bs.target_dir(root, ACCT_B2), root / "target" / "deploy" / "111111111111-us-west-2")
        self.assertEqual(bs.key(ACCT_A1), "222222222222-us-east-1")

    def test_the_lambda_directory_is_keyed_by_selection_and_stage(self) -> None:
        root = Path("/repo")
        self.assertEqual(bs.lambda_dir(root, ACCT_A1, "live"), bs.target_dir(root, ACCT_A1) / "lambda" / "live")
        self.assertNotEqual(bs.lambda_dir(root, ACCT_A1, "live"), bs.lambda_dir(root, ACCT_A1, "drill"))
        self.assertNotEqual(bs.lambda_dir(root, ACCT_A1, "live"), bs.lambda_dir(root, ACCT_B2, "live"))

    def test_the_label_names_profile_account_and_region(self) -> None:
        self.assertEqual(bs.label(ACCT_A1), "my-profile (222222222222 us-east-1)")

    def test_the_fixture_and_the_library_name_the_same_measured_binaries(self) -> None:
        self.assertEqual(set(bs.ACCOUNT_BINARIES), set(sf.ACCOUNT_BINARIES))
        self.assertEqual(bs.TRIPLE, sf.TRIPLE)

    def test_the_build_directories_are_ignored_by_git_and_leave_every_key_alone(self) -> None:
        # target/ is in .gitignore and the image key and sourceTree hash git tree objects of tracked paths.
        p = REPO / "target" / "deploy" / bs.key(ACCT_A1) / "lambda" / "live" / "run-tasks" / "bootstrap"
        self.assertEqual(subprocess.run(["git", "-C", str(REPO), "check-ignore", "-q", str(p)]).returncode, 0)  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
        root = self.tmp / "repo"
        (root / "crates" / "agentd").mkdir(parents=True)
        (root / "crates" / "agentd" / "main.rs").write_text("fn main() {}\n")
        (root / "accounts.json").write_text(sf.ACCOUNTS.read_text())
        (root / ".gitignore").write_text("target/\n")
        git(root, "init", "-q", "-b", "main")
        git(root, "add", ".")
        git(root, "commit", "-q", "-m", "base")
        before = (digests.source_tree(root), digests.image_key(root, ["crates/agentd"]))
        sf.write_tree("lambda", ["run-tasks"], bs.target_dir(root, ACCT_A1), bs.lambda_dir(root, ACCT_A1, "live"), "my-profile")
        sf.write_tree("zig", ["agentd"], bs.target_dir(root, ACCT_B2), None, "history-profile")
        self.assertEqual((digests.source_tree(root), digests.image_key(root, ["crates/agentd"])), before)
        self.assertEqual(git(root, "status", "--porcelain"), "")


class ScanCase(Base):
    def test_a_needle_is_found_anywhere_in_the_file(self) -> None:
        data = b"\x7fELF" + os.urandom(5000) + b"222222222222" + os.urandom(5000)
        p = self.write("bin", data)
        self.assertEqual(bs.scan(p, [b"222222222222", b"111111111111"]), {b"222222222222"})

    def test_a_needle_across_a_chunk_boundary_is_found(self) -> None:
        for offset in range(0, 12):
            data = b"x" * (64 - offset) + b"222222222222" + b"y" * 200
            p = self.write("bin", data)
            self.assertEqual(bs.scan(p, [b"222222222222"], chunk=64), {b"222222222222"}, offset)

    def test_digits_around_a_needle_do_not_hide_it(self) -> None:
        # Rust string literals sit in read-only data with no separator: "us-east-1" + the account id.
        p = self.write("bin", b"us-east-1222222222222us-west-2")
        self.assertEqual(bs.scan(p, [b"222222222222"]), {b"222222222222"})

    def test_a_file_without_the_needles_finds_none(self) -> None:
        self.assertEqual(bs.scan(self.write("bin", os.urandom(4096)), [b"222222222222"]), set())


class BinaryCase(Base):
    def binary(self, name: str, built_for: str, foreign: bool = False) -> Path:
        return self.write(f"{name}-{built_for}", sf.binary_bytes(name, built_for, foreign=foreign))

    def test_an_account_binary_built_for_the_selection_passes_for_each_entry(self) -> None:
        for sel in (ACCT_A1, ACCT_B2):
            for name in sorted(bs.ACCOUNT_BINARIES):
                self.assertEqual(bs.check_binary(self.binary(name, sel.profile), name, sel, ENTRIES), "account", (name, sel))

    def test_an_account_binary_built_for_another_entry_names_the_binary_and_both_selections(self) -> None:
        for name in sorted(bs.ACCOUNT_BINARIES):
            p = self.binary(name, "history-profile")
            with self.assertRaises(bs.Mismatch) as cm:
                bs.check_binary(p, name, ACCT_A1, ENTRIES)
            e = cm.exception
            self.assertEqual(e.binary, name)
            self.assertIn(bs.label(ACCT_B2), e.detail)
            self.assertIn(bs.label(ACCT_A1), e.detail)
            self.assertIn(str(p), e.detail)
            self.assertIn("111111111111", e.detail)
            self.assertIn("222222222222", e.detail)

    def test_an_account_binary_may_carry_the_other_entrys_id_in_a_price_list_string(self) -> None:
        # control-api and dispatch-turn embed history-profile's id in prices.json's source line whatever they are
        # built for (measured 2026-10-01): the selected id being present is what counts.
        p = self.binary("repo-gateway", "my-profile", foreign=True)
        self.assertIn(b"111111111111", p.read_bytes())
        self.assertEqual(bs.check_binary(p, "repo-gateway", ACCT_A1, ENTRIES), "account")

    def test_an_account_binary_with_no_account_id_at_all_is_refused(self) -> None:
        p = self.write("repo-gateway", b"\x7fELF" + os.urandom(2048))
        with self.assertRaises(bs.Mismatch) as cm:
            bs.check_binary(p, "repo-gateway", ACCT_A1, ENTRIES)
        self.assertEqual(cm.exception.binary, "repo-gateway")
        self.assertIn("no account id", cm.exception.detail)
        self.assertIn(bs.label(ACCT_A1), cm.exception.detail)

    def test_every_other_binary_is_left_to_the_build_output_check(self) -> None:
        # A real agentd carries neither id and its Region strings are drowned by SDK tables (measured): a byte
        # scan can say nothing about it, so it never fails here, whatever it embeds.
        for name in ("agentd", "control-api", "run-session"):
            p = self.write(name, sf.binary_bytes(name, "history-profile") + b"111111111111 us-west-2")
            self.assertIsNone(bs.check_binary(p, name, ACCT_A1, ENTRIES), name)

    def test_an_unreadable_binary_is_a_mismatch_that_names_it(self) -> None:
        with self.assertRaises(bs.Mismatch) as cm:
            bs.check_binary(self.tmp / "missing", "repo-gateway", ACCT_A1, ENTRIES)
        self.assertEqual(cm.exception.binary, "repo-gateway")
        self.assertIn("cannot read", cm.exception.detail)


class DriverCase(Base):
    def driver(self, built_for: str, extra: bytes = b"") -> Path:
        name = "ham-e2e"
        return self.write(f"{name}-{built_for}", b"\x7fELF" + os.urandom(1024) + sf.binary_bytes("repo-gateway", built_for) + extra)

    def test_a_driver_built_for_the_selection_passes_for_each_entry(self) -> None:
        for sel in (ACCT_A1, ACCT_B2):
            bs.check_driver(self.driver(sel.profile), sel, ENTRIES)

    def test_a_driver_built_for_another_entry_is_refused_with_both_selections(self) -> None:
        with self.assertRaises(bs.Mismatch) as cm:
            bs.check_driver(self.driver("my-profile"), ACCT_B2, ENTRIES)
        e = cm.exception
        self.assertEqual(e.built_for, ACCT_A1)
        self.assertIn(bs.label(ACCT_A1), e.detail)
        self.assertIn(bs.label(ACCT_B2), e.detail)
        self.assertEqual(e.binary, "ham-e2e-my-profile")

    def test_a_driver_that_embeds_both_ids_is_refused(self) -> None:
        with self.assertRaises(bs.Mismatch) as cm:
            bs.check_driver(self.driver("history-profile", extra=b" 222222222222 "), ACCT_B2, ENTRIES)
        self.assertIn("222222222222", cm.exception.detail)

    def test_a_driver_with_no_account_id_cannot_be_told_apart_and_is_refused(self) -> None:
        with self.assertRaises(bs.Mismatch) as cm:
            bs.check_driver(self.write("hamh-dispatch", b"#!/bin/sh\nexit 0\n"), ACCT_B2, ENTRIES)
        self.assertIn("no account id", cm.exception.detail)
        self.assertIsNone(cm.exception.built_for)

    def test_a_missing_driver_is_a_mismatch(self) -> None:
        with self.assertRaises(bs.Mismatch):
            bs.check_driver(self.tmp / "nope", ACCT_B2, ENTRIES)

    def test_the_command_line_check_exits_1_with_the_reason_and_0_in_silence(self) -> None:
        env = dict(os.environ, HAMH_AWS_PROFILE="history-profile", HAMH_ACCOUNTS_FILE=str(sf.ACCOUNTS))
        ok = subprocess.run([sys.executable, str(CLI), "check-driver", str(self.driver("history-profile"))],  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool
                            capture_output=True, text=True, env=env)
        self.assertEqual((ok.returncode, ok.stdout, ok.stderr), (0, "", ""))
        bad = subprocess.run([sys.executable, str(CLI), "check-driver", str(self.driver("my-profile"))],  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool
                             capture_output=True, text=True, env=env)
        self.assertEqual(bad.returncode, 1, bad.stdout + bad.stderr)
        self.assertIn(bs.label(ACCT_A1), bad.stderr)
        self.assertIn(bs.label(ACCT_B2), bad.stderr)
        usage = subprocess.run([sys.executable, str(CLI)], capture_output=True, text=True, env=env)  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool
        self.assertEqual(usage.returncode, 2)


class BuildOutputCase(Base):
    def out(self, root: Path, profile: str, hash_: str = "aaaa", triple: str = bs.TRIPLE, age: float = 0.0) -> Path:
        p = root / triple / "release" / "build" / f"harness-protocol-{hash_}" / "out" / "aws_target.rs"
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(sf.aws_target_rs(profile))
        t = time.time() - age
        os.utime(p, (t, t))
        return p

    def test_output_naming_the_selection_passes_and_is_returned(self) -> None:
        root = self.tmp / "t"
        f = self.out(root, "my-profile")
        self.assertEqual(bs.check_build_output(root, ACCT_A1, ENTRIES), [f])

    def test_output_naming_another_entry_names_harness_protocol_and_both_selections(self) -> None:
        root = self.tmp / "t"
        f = self.out(root, "history-profile")
        with self.assertRaises(bs.Mismatch) as cm:
            bs.check_build_output(root, ACCT_A1, ENTRIES)
        e = cm.exception
        self.assertEqual(e.binary, "harness-protocol")
        self.assertIn(bs.label(ACCT_B2), e.detail)
        self.assertIn(bs.label(ACCT_A1), e.detail)
        self.assertIn(str(f), e.detail)

    def test_no_output_at_all_is_refused_because_nothing_confirms_the_selection(self) -> None:
        root = self.tmp / "t"
        root.mkdir()
        with self.assertRaises(bs.Mismatch) as cm:
            bs.check_build_output(root, ACCT_A1, ENTRIES)
        self.assertEqual(cm.exception.binary, "harness-protocol")
        self.assertIn(str(root), cm.exception.detail)

    def test_the_newest_output_of_a_build_root_decides(self) -> None:
        root = self.tmp / "t"
        # A stale directory from an older dependency set names history-profile; the current one names active-entry.
        self.out(root, "history-profile", "stale", age=3600)
        new = self.out(root, "my-profile", "fresh")
        self.assertEqual(bs.check_build_output(root, ACCT_A1, ENTRIES), [new])
        # The other way round, the newest build script run was for history-profile: refused.
        root2 = self.tmp / "t2"
        self.out(root2, "my-profile", "stale", age=3600)
        self.out(root2, "history-profile", "fresh")
        with self.assertRaises(bs.Mismatch):
            bs.check_build_output(root2, ACCT_A1, ENTRIES)

    def test_every_build_root_is_checked(self) -> None:
        root = self.tmp / "t"
        self.out(root, "my-profile", triple=bs.TRIPLE)
        self.out(root, "history-profile", triple="x86_64-unknown-linux-gnu")
        with self.assertRaises(bs.Mismatch):
            bs.check_build_output(root, ACCT_A1, ENTRIES)

    def test_a_file_without_the_constants_is_refused(self) -> None:
        root = self.tmp / "t"
        p = self.out(root, "my-profile")
        p.write_text("// empty\n")
        with self.assertRaises(bs.Mismatch) as cm:
            bs.check_build_output(root, ACCT_A1, ENTRIES)
        self.assertIn("E2E_ACCOUNT", cm.exception.detail)


if __name__ == "__main__":
    unittest.main()
