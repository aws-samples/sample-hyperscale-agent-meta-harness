"""Tests for scripts/lib/arm64_sysroot.py: running the production aarch64 `agentd` on an x86_64 host.

A fake docker stands in for the engine here; nothing pulls an image.

The contract:
- the pinned image is the arm64 child digest tools.lock `[containers] al2023_arm64` names;
- an ELF's architecture is read from its header;
- the sysroot is copied out of a created (never started) container of the pinned image, so it needs no emulation itself;
  each file is checked against its pin, a wrong file is an error and a tampered cached file is extracted again;
- whether the host can run an aarch64 ELF is decided by running the sysroot's loader (`ld-linux-aarch64.so.1 --version`), not
  by reading /proc/sys/fs/binfmt_misc, which a build container may not mount although the kernel's registration applies;
  a host that cannot is registered through the pinned tonistiigi/binfmt image and probed again; a failed registration or a
  probe that still fails is an error, never a skip.
"""

from __future__ import annotations

import hashlib
import json
import os
import struct
import subprocess  # nosec B404: tests start the scripts under test and git with argument lists, never a shell
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "scripts" / "lib"))
import arm64_sysroot as sr  # noqa: E402

FAKE_DOCKER = r'''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
args = sys.argv[1:]
Path(os.environ["FAKE_DOCKER_LOG"]).open("a").write(json.dumps(args) + "\n")
mode = os.environ.get("FAKE_DOCKER_MODE", "ok")
if mode.startswith("fail-once-") and args[0] == mode[len("fail-once-"):] or mode == "fail-once-run" and "--privileged" in args:
    seen = Path(os.environ["FAKE_DOCKER_LOG"] + ".once")
    if not seen.exists():
        seen.write_text("x")
        print("TLS handshake timeout", file=sys.stderr)
        sys.exit(1)
if mode == "fail":
    print("boom", file=sys.stderr)
    sys.exit(125)
if args[0] == "run" and "--privileged" in args:          # the binfmt image registers the entry: the loader works afterwards
    if mode != "no-entry":
        Path(os.environ["FAKE_BINFMT_MARKER"]).write_text("registered")
    sys.exit(0)
if args[0] == "create":
    print("cid-0001")
    sys.exit(0)
if args[0] == "cp":
    src, dest = args[1], Path(args[2])
    rel = {"/usr/lib/ld-linux-aarch64.so.1": "lib/ld-linux-aarch64.so.1", "/usr/lib64/libc.so.6": "lib64/libc.so.6",
           "/usr/lib64/libm.so.6": "lib64/libm.so.6"}[src.split(":", 1)[1]]
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_bytes(b"WRONG" if mode == "wrong-content" else ("content of " + rel).encode())
    sys.exit(0)
if args[0] == "rm":
    sys.exit(0)
sys.exit(2)
'''

FAKE_LOADER = '''#!/bin/sh
# The aarch64 loader: runs only once the binfmt entry is "registered" (the marker exists), as an exec format error otherwise.
[ -e "$FAKE_BINFMT_MARKER" ] || { echo "cannot execute binary file: Exec format error" >&2; exit 126; }
echo "ld.so (GNU libc) stable release version 2.34."
'''


class Base(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="arm64-sysroot-"))
        self.docker = self.tmp / "docker"
        self.docker.write_text(FAKE_DOCKER)
        self.docker.chmod(0o755)
        self.log = self.tmp / "docker.log"
        self.binfmt = self.tmp / "qemu-aarch64"
        self.marker = self.tmp / "registered"
        self.loader = self.tmp / "loader"
        self.loader.write_text(FAKE_LOADER)
        self.loader.chmod(0o755)
        self.env = {"ARM64_SYSROOT_DOCKER": str(self.docker), "FAKE_DOCKER_LOG": str(self.log), "ARM64_BINFMT_PATH": str(self.binfmt),
                    "FAKE_BINFMT_MARKER": str(self.marker), "ARM64_SYSROOT_RETRY_SLEEP": "0"}
        self._files = dict(sr.FILES)
        sr.FILES = {rel: (src, hashlib.sha256(("content of " + rel).encode()).hexdigest()) for rel, (src, _) in self._files.items()}

    def tearDown(self) -> None:
        sr.FILES = self._files

    def calls(self) -> list[list[str]]:
        return [json.loads(ln) for ln in self.log.read_text().splitlines()] if self.log.is_file() else []


class PinsCase(unittest.TestCase):
    def test_the_image_is_the_arm64_child_digest_of_tools_lock(self) -> None:
        lock = tomllib.loads((REPO / "tools.lock").read_text())
        self.assertEqual(sr.IMAGE, lock["containers"]["al2023_arm64"])
        self.assertRegex(sr.IMAGE, r"^public\.ecr\.aws/amazonlinux/amazonlinux@sha256:[0-9a-f]{64}$")

    def test_the_worker_image_builds_from_the_index_of_that_arm64_child(self) -> None:
        text = (REPO / "images" / "worker-python" / "Dockerfile").read_text()
        self.assertIn("FROM public.ecr.aws/amazonlinux/amazonlinux:2023@sha256:", text)

    def test_the_sysroot_holds_the_needed_set_of_agentd(self) -> None:
        self.assertEqual(sorted(sr.FILES), ["lib/ld-linux-aarch64.so.1", "lib64/libc.so.6", "lib64/libm.so.6"])
        for _, sha in sr.FILES.values():
            self.assertRegex(sha, r"^[0-9a-f]{64}$")


class ElfCase(unittest.TestCase):
    def test_architecture_from_the_header(self) -> None:
        d = Path(tempfile.mkdtemp(prefix="elf-"))
        for name, machine, want in (("a", 183, "aarch64"), ("x", 62, "x86_64"), ("r", 243, None)):
            p = d / name
            p.write_bytes(b"\x7fELF" + b"\x02\x01\x01" + b"\0" * 9 + struct.pack("<HH", 3, machine) + b"\0" * 8)
            self.assertEqual(sr.elf_arch(p), want, name)
        (d / "t").write_text("#!/bin/sh\n")
        self.assertIsNone(sr.elf_arch(d / "t"))


class BinfmtCase(Base):
    def test_a_host_that_runs_the_loader_needs_no_registration(self) -> None:
        self.marker.write_text("x")
        self.binfmt.write_text("enabled\ninterpreter /usr/bin/qemu-aarch64\nflags: POCF\noffset 0\n")
        b = sr.ensure_binfmt(self.env, self.loader)
        self.assertEqual((b["how"], b["interpreter"], b["flags"]), ("present", "/usr/bin/qemu-aarch64", "POCF"))
        self.assertEqual(self.calls(), [])

    def test_the_decision_is_the_probe_not_the_binfmt_file(self) -> None:
        # A build container may not mount binfmt_misc although the kernel's registration applies: no file, loader works.
        self.marker.write_text("x")
        b = sr.ensure_binfmt(self.env, self.loader)
        self.assertEqual(b["how"], "present")
        self.assertEqual(b["interpreter"], "unreadable")
        self.assertEqual(self.calls(), [])

    def test_a_disabled_entry_counts_as_absent_in_the_report(self) -> None:
        self.binfmt.write_text("disabled\ninterpreter /usr/bin/qemu-aarch64\nflags: POCF\n")
        self.assertIsNone(sr.binfmt_entry(self.env))

    def test_a_host_that_cannot_run_the_loader_is_registered_through_the_pinned_binfmt_image_then_probed(self) -> None:
        b = sr.ensure_binfmt(self.env, self.loader)
        self.assertEqual(b["how"], "registered-now")
        lock = tomllib.loads((REPO / "tools.lock").read_text())
        self.assertEqual(self.calls(), [["run", "--privileged", "--rm", lock["containers"]["binfmt"], "--install", "arm64"]])

    def test_a_failed_registration_is_an_error(self) -> None:
        with self.assertRaises(sr.SysrootError) as cm:
            sr.ensure_binfmt({**self.env, "FAKE_DOCKER_MODE": "fail"}, self.loader)
        self.assertIn("binfmt registration failed", str(cm.exception))

    def test_a_registration_after_which_the_loader_still_fails_is_an_error(self) -> None:
        with self.assertRaises(sr.SysrootError) as cm:
            sr.ensure_binfmt({**self.env, "FAKE_DOCKER_MODE": "no-entry"}, self.loader)
        self.assertIn("still cannot run", str(cm.exception))


class Retries(Base):
    """A pull from Docker Hub or public ECR can fail for a second; a step that fails on it costs a whole CI run."""

    def test_a_registration_that_fails_once_is_retried(self) -> None:
        b = sr.ensure_binfmt({**self.env, "FAKE_DOCKER_MODE": "fail-once-run"}, self.loader)
        self.assertEqual(b["how"], "registered-now")
        self.assertEqual(len([c for c in self.calls() if "--privileged" in c]), 2)

    def test_a_registration_that_never_works_stops_after_three_attempts(self) -> None:
        with self.assertRaises(sr.SysrootError):
            sr.ensure_binfmt({**self.env, "FAKE_DOCKER_MODE": "fail"}, self.loader)
        self.assertEqual(len([c for c in self.calls() if "--privileged" in c]), 3)

    def test_a_create_that_fails_once_is_retried(self) -> None:
        sr.ensure_sysroot(self.tmp / "sysroot", {**self.env, "FAKE_DOCKER_MODE": "fail-once-create"})
        self.assertEqual(len([c for c in self.calls() if c[0] == "create"]), 2)
        self.assertEqual(sr.verify(self.tmp / "sysroot"), [])

    def test_a_create_that_never_works_stops_after_three_attempts(self) -> None:
        with self.assertRaises(sr.SysrootError):
            sr.ensure_sysroot(self.tmp / "sysroot", {**self.env, "FAKE_DOCKER_MODE": "fail"})
        self.assertEqual(len([c for c in self.calls() if c[0] == "create"]), 3)


class SysrootCase(Base):
    def test_extraction_copies_out_of_a_created_container_of_the_pinned_arm64_image_and_checks_each_file(self) -> None:
        dest = self.tmp / "sysroot"
        info = sr.ensure_sysroot(dest, self.env)
        calls = self.calls()
        self.assertEqual(calls[0], ["create", "--platform", "linux/arm64", sr.IMAGE])
        self.assertEqual(sorted(c[1] for c in calls if c[0] == "cp"),
                         sorted(f"cid-0001:{src}" for src, _ in sr.FILES.values()))
        self.assertEqual(calls[-1], ["rm", "cid-0001"])
        self.assertNotIn("run", [c[0] for c in calls], "no container is started: the extraction needs no emulation")
        self.assertEqual(sr.verify(dest), [])
        self.assertEqual(info["image"], sr.IMAGE)
        self.assertIn("libc.so.6=", sr.describe(info))

    def test_a_good_cached_sysroot_is_not_extracted_again(self) -> None:
        dest = self.tmp / "sysroot"
        sr.ensure_sysroot(dest, self.env)
        n = len(self.calls())
        sr.ensure_sysroot(dest, self.env)
        self.assertEqual(len(self.calls()), n)

    def test_a_tampered_file_is_extracted_again(self) -> None:
        dest = self.tmp / "sysroot"
        sr.ensure_sysroot(dest, self.env)
        (dest / "lib64/libc.so.6").write_bytes(b"tampered")
        self.assertEqual(len(sr.verify(dest)), 1)
        n = len(self.calls())
        sr.ensure_sysroot(dest, self.env)
        self.assertGreater(len(self.calls()), n)
        self.assertEqual(sr.verify(dest), [])

    def test_content_that_differs_from_the_pins_is_an_error(self) -> None:
        with self.assertRaises(sr.SysrootError) as cm:
            sr.ensure_sysroot(self.tmp / "sysroot", {**self.env, "FAKE_DOCKER_MODE": "wrong-content"})
        self.assertIn("differs from its pins", str(cm.exception))

    def test_a_failed_docker_run_is_an_error(self) -> None:
        with self.assertRaises(sr.SysrootError) as cm:
            sr.ensure_sysroot(self.tmp / "sysroot", {**self.env, "FAKE_DOCKER_MODE": "fail"})
        self.assertIn("extraction failed", str(cm.exception))


class CliCase(Base):
    def run_cli(self, *args: str, mode: str = "ok") -> subprocess.CompletedProcess:
        env = {**os.environ, **self.env, "FAKE_DOCKER_MODE": mode}
        return subprocess.run([sys.executable, str(REPO / "scripts/lib/arm64_sysroot.py"), *args], capture_output=True, text=True, env=env)  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool

    def test_ensure_fails_on_content_that_differs_from_the_real_pins(self) -> None:
        # The CLI checks the real pins, so a fake that writes other bytes fails it: that failure is the line's FAIL form.
        p = self.run_cli("ensure", "--dest", str(self.tmp / "s"))
        self.assertEqual(p.returncode, 1, p.stdout)
        self.assertRegex(p.stdout, r"^ARM64-SYSROOT FAIL sysroot differs from its pins")


if __name__ == "__main__":
    unittest.main()
