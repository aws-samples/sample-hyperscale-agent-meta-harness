"""Tests for scripts/build-macos-driver.sh: the aarch64-apple-darwin `ham` for a Mac, cross-built with
cargo zigbuild. A fake `cargo` on PATH records its arguments and writes the binary with the header it
is told to (arm64 Mach-O unless a test says otherwise); fake `cargo-zigbuild`, `zig` and `rustc` stand in for the toolchain (a rustup proxy
under the test's temp HOME would install the rust-toolchain.toml channel there); the real
cargo-slot.sh runs with its lock directory under the test's temp root. No case builds or reaches the
network.
"""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess  # nosec B404: tests start the scripts under test and git with argument lists, never a shell
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
SCRIPT = REPO / "scripts" / "build-macos-driver.sh"
MACHO = bytes.fromhex("cffaedfe0c000001") + b"\0" * 56
ELF = b"\x7fELF" + b"\0" * 60

FAKE_CARGO = r'''#!/usr/bin/env python3
import json, os, sys
with open(os.environ["FAKE_CARGO_LOG"], "a") as f:
    f.write(json.dumps({"args": sys.argv[1:], "cwd": os.getcwd(), "target": os.environ.get("CARGO_TARGET_DIR")}) + "\n")
if os.environ.get("FAKE_CARGO_FAIL"):
    print("error[E0463]: can't find crate for `core`", file=sys.stderr); sys.exit(101)
d = os.path.join(os.environ["CARGO_TARGET_DIR"], "aarch64-apple-darwin", "release")
os.makedirs(d, exist_ok=True)
for b in ("ham",):
    head = bytes.fromhex(os.environ.get("FAKE_HEAD_" + b.replace("-", "_").upper(), "cffaedfe0c000001"))
    with open(os.path.join(d, b), "wb") as f:
        f.write(head + b.encode() * 8)
'''


class BuildMacosDriverCase(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="macos-driver-", dir=os.environ.get("TMPDIR")))
        self.bin = self.tmp / "bin"
        self.bin.mkdir()
        (self.bin / "cargo").write_text(FAKE_CARGO)
        (self.bin / "cargo-zigbuild").write_text("#!/bin/sh\nexit 0\n")
        (self.bin / "zig").write_text("#!/bin/sh\necho 0.16.0\n")
        (self.bin / "rustc").write_text("#!/bin/sh\necho 'rustc 1.98.1-fake (0000000 2026-01-01)'\n")
        for p in self.bin.iterdir():
            p.chmod(0o755)
        self.out = self.tmp / "out"

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)

    def run_script(self, *args: str, path: str | None = None, **env: str) -> subprocess.CompletedProcess:
        e = {"PATH": path or f"{self.bin}:{os.environ['PATH']}", "HOME": str(self.tmp),
             "TMPDIR": str(self.tmp), "CARGO_TARGET_DIR": str(self.tmp / "target"),
             "CARGO_SLOT_DIR": str(self.tmp / "locks"), "FAKE_CARGO_LOG": str(self.tmp / "cargo.log"), **env}
        return subprocess.run(["bash", str(SCRIPT), *args], capture_output=True, text=True, env=e, timeout=120)  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do

    def cargo_calls(self) -> list[dict]:
        log = self.tmp / "cargo.log"
        return [json.loads(ln) for ln in log.read_text().splitlines()] if log.is_file() else []

    def test_the_binary_is_cross_built_checked_copied_and_summed(self) -> None:
        p = self.run_script(str(self.out))
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        calls = self.cargo_calls()
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0]["args"], ["zigbuild", "--release", "--locked", "--target", "aarch64-apple-darwin",
                                            "-p", "ham", "--bin", "ham"])
        self.assertEqual(calls[0]["cwd"], str(REPO))
        self.assertEqual(calls[0]["target"], str(self.tmp / "target"))
        sums = {}
        for b in ("ham",):
            data = (self.out / b).read_bytes()
            self.assertEqual(data[:8], bytes.fromhex("cffaedfe0c000001"))
            self.assertTrue(os.access(self.out / b, os.X_OK))
            sums[b] = hashlib.sha256(data).hexdigest()
        self.assertEqual((self.out / "SHA256SUMS").read_text(), f"{sums['ham']}  ham\n")
        doc = json.loads((self.out / "build.json").read_text())
        self.assertEqual(doc["target"], "aarch64-apple-darwin")
        self.assertEqual(doc["sha256"], sums)
        self.assertEqual(doc["zig"], "0.16.0")
        self.assertIn(doc["dirty"], (True, False))
        commit = subprocess.run(["git", "-C", str(REPO), "rev-parse", "--short=7", "HEAD"], capture_output=True,  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
                                text=True).stdout.strip()
        self.assertEqual(doc["commit"], commit)
        last = p.stdout.strip().splitlines()[-1]
        self.assertEqual(last, f"MACOS-DRIVER PASS dir={self.out} ham={sums['ham'][:12]} commit={commit}")
        self.assertTrue((self.out / "build.log").is_file())
        # The SHA256SUMS file checks with the sha256sum the laptop's shasum -a 256 -c mirrors.
        chk = subprocess.run(["sha256sum", "-c", "SHA256SUMS"], cwd=self.out, capture_output=True, text=True)  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
        self.assertEqual(chk.returncode, 0, chk.stdout + chk.stderr)

    def test_no_run_installs_a_toolchain_and_build_json_names_the_rustc_on_path(self) -> None:
        # The test's HOME is its temp dir. A rustup proxy reached there would sync the
        # rust-toolchain.toml channel into <HOME>/.rustup (1.5 GB, about half a minute) on every run.
        p = self.run_script(str(self.out))
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        self.assertFalse((self.tmp / ".rustup").exists(), "a rustup proxy synced a channel into the test HOME")
        self.assertFalse((self.tmp / ".cargo").exists(), "a rustup proxy wrote the test HOME's cargo home")
        self.assertEqual(json.loads((self.out / "build.json").read_text())["rust"], "1.98.1-fake")

    def test_a_binary_that_is_not_arm64_macho_fails_and_is_not_copied(self) -> None:
        p = self.run_script(str(self.out), FAKE_HEAD_HAM=ELF[:8].hex())
        self.assertEqual(p.returncode, 1, p.stdout + p.stderr)
        self.assertEqual(p.stdout.strip().splitlines()[-1], f"MACOS-DRIVER FAIL ham not-arm64-macho:{ELF[:8].hex()}")
        self.assertFalse((self.out / "ham").exists())
        self.assertFalse((self.out / "SHA256SUMS").exists())

    def test_a_failed_build_names_its_exit_and_log(self) -> None:
        p = self.run_script(str(self.out), FAKE_CARGO_FAIL="1")
        self.assertEqual(p.returncode, 1, p.stdout + p.stderr)
        self.assertEqual(p.stdout.strip(), f"MACOS-DRIVER FAIL build exit=101 (see {self.out}/build.log)")
        self.assertIn("can't find crate", (self.out / "build.log").read_text())
        self.assertIn("error[E0463]", p.stderr)

    def test_a_missing_tool_fails_before_any_build(self) -> None:
        (self.bin / "cargo-zigbuild").unlink()
        # Only the fake bin directory and the base system tools: no other cargo-zigbuild on PATH.
        base = self.tmp / "base"
        base.mkdir()
        for tool in ("bash", "od", "tr", "sha256sum", "cut", "git", "date", "mkdir", "rm", "cp", "chmod", "grep",
                     "head", "sed", "awk", "dirname", "flock", "python3", "env"):
            found = shutil.which(tool)
            if found:
                (base / tool).symlink_to(found)
        p = self.run_script(str(self.out), path=f"{self.bin}:{base}")
        self.assertEqual((p.returncode, p.stdout.strip()), (1, "MACOS-DRIVER FAIL missing-cargo-zigbuild"))
        self.assertEqual(self.cargo_calls(), [])

    def test_usage_errors_exit_2_before_any_build(self) -> None:
        for args in ([], ["rel/out"], [str(self.out), "extra"]):
            p = self.run_script(*args)
            self.assertEqual(p.returncode, 2, (args, p.stdout, p.stderr))
            self.assertIn("usage", p.stderr)
        self.assertEqual(self.cargo_calls(), [])


if __name__ == "__main__":
    unittest.main()
