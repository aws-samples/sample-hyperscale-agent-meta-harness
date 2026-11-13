"""Tests for scripts/setup/client_check.py, the check that a built client binary will start on the machine it is built for.

`build-client.sh` runs it on every binary before anything is uploaded. It reads the file with the standard library only:

  macho-arm64   a thin 64-bit arm64 Mach-O executable that carries a code signature whose SHA-256 page hashes match the file. Apple Silicon
                refuses to run an arm64 binary without a valid signature (the kernel kills the process), and the zig linker writes an ad-hoc,
                linker-signed one; a binary changed after linking, or a linker that stops signing, fails here instead of on a user's Mac.
  elf-x86_64    a 64-bit little-endian ELF executable for x86-64 that is statically linked (no program interpreter), so it starts on any Linux.
  elf-aarch64   the same for AArch64.

Output: one line, `OK <kind> <facts>` (exit 0) or `FAIL <code>: <reason>` (exit 1); usage errors exit 2. The cases build small valid files with
`client_fixtures.py` and damage one property at a time.

Run on a Linux machine with the tools pinned in `tools.lock`:
  $HAMH_PYTHON -m unittest discover -s scripts/tests -p test_client_check.py
"""

from __future__ import annotations

import os
import subprocess  # nosec B404: tests start the checker with an argument list, never through a shell
import sys
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(Path(__file__).resolve().parent))
import client_fixtures as fx  # noqa: E402

CHECK = REPO / "scripts" / "setup" / "client_check.py"


class CheckCase(unittest.TestCase):
    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory(prefix="client-check-", dir=os.environ.get("TMPDIR"))
        self.tmp = Path(self._td.name)
        self.addCleanup(self._td.cleanup)

    def check(self, kind: str, data: bytes, name: str = "client") -> subprocess.CompletedProcess:
        f = self.tmp / name
        f.write_bytes(data)
        return subprocess.run([sys.executable, "-I", str(CHECK), kind, str(f)], capture_output=True, text=True, timeout=60)  # nosec B603: argument list, no shell, the interpreter running the tests

    def ok(self, kind: str, data: bytes) -> str:
        p = self.check(kind, data)
        self.assertEqual((p.returncode, p.stderr), (0, ""), p.stdout)
        self.assertEqual(len(p.stdout.splitlines()), 1)
        self.assertTrue(p.stdout.startswith(f"OK {kind} "), p.stdout)
        return p.stdout.strip()

    def fails(self, kind: str, data: bytes, code: str) -> str:
        p = self.check(kind, data)
        self.assertEqual(p.returncode, 1, p.stdout + p.stderr)
        self.assertEqual(len(p.stdout.splitlines()), 1, p.stdout)
        self.assertTrue(p.stdout.startswith(f"FAIL {code}: "), p.stdout)
        return p.stdout.strip()


class MachoCase(CheckCase):
    def test_a_signed_arm64_executable_passes_and_reports_the_signature_and_the_minimum_macos(self) -> None:
        line = self.ok("macho-arm64", fx.macho_arm64())
        self.assertIn("signature=adhoc", line)
        self.assertIn("pages=3", line)
        self.assertIn("minos=13.0.0", line)

    def test_the_page_count_follows_the_file(self) -> None:
        self.assertIn("pages=9", self.ok("macho-arm64", fx.macho_arm64(pages=9)))

    def test_a_binary_without_a_code_signature_is_refused_with_the_apple_silicon_reason(self) -> None:
        msg = self.fails("macho-arm64", fx.macho_arm64(sign=False), "no-code-signature")
        self.assertIn("Apple Silicon", msg)

    def test_one_changed_byte_after_signing_is_a_hash_mismatch(self) -> None:
        data = bytearray(fx.macho_arm64(pages=4))
        data[fx.PAGE * 2 + 100] ^= 0xFF  # inside the third page of code
        msg = self.fails("macho-arm64", bytes(data), "hash-mismatch")
        self.assertIn("page 2", msg)

    def test_a_zeroed_hash_slot_is_a_hash_mismatch(self) -> None:
        self.fails("macho-arm64", fx.macho_arm64(tamper_hash=True), "hash-mismatch")

    def test_a_cut_signature_is_refused(self) -> None:
        self.fails("macho-arm64", fx.macho_arm64(truncate_signature=True), "bad-signature")

    def test_a_signature_hash_type_other_than_sha256_is_refused(self) -> None:
        self.fails("macho-arm64", fx.macho_arm64(hash_type=1), "bad-signature")

    def test_an_x86_64_mach_o_is_the_wrong_cpu(self) -> None:
        self.fails("macho-arm64", fx.macho_arm64(cputype=0x01000007), "wrong-cpu")

    def test_a_library_is_not_an_executable(self) -> None:
        self.fails("macho-arm64", fx.macho_arm64(filetype=6), "wrong-filetype")

    def test_an_elf_file_is_not_a_mach_o(self) -> None:
        self.fails("macho-arm64", fx.elf64(fx.EM_AARCH64), "bad-magic")

    def test_a_universal_binary_is_refused_because_the_build_makes_a_thin_one(self) -> None:
        self.fails("macho-arm64", bytes.fromhex("cafebabe") + b"\x00" * 64, "bad-magic")


class ElfCase(CheckCase):
    def test_a_static_x86_64_executable_passes(self) -> None:
        self.assertIn("static", self.ok("elf-x86_64", fx.elf64(fx.EM_X86_64)))

    def test_a_static_pie_executable_passes(self) -> None:
        self.ok("elf-x86_64", fx.elf64(fx.EM_X86_64, etype=3))

    def test_a_static_aarch64_executable_passes(self) -> None:
        self.assertIn("static", self.ok("elf-aarch64", fx.elf64(fx.EM_AARCH64)))

    def test_the_other_architecture_is_the_wrong_machine(self) -> None:
        self.fails("elf-x86_64", fx.elf64(fx.EM_AARCH64), "wrong-machine")
        self.fails("elf-aarch64", fx.elf64(fx.EM_X86_64), "wrong-machine")

    def test_a_program_interpreter_means_a_dynamic_binary_that_may_not_start_on_every_linux(self) -> None:
        msg = self.fails("elf-x86_64", fx.elf64(fx.EM_X86_64, interp=True), "dynamic-interpreter")
        self.assertIn("/lib/ld-musl-x86_64.so.1", msg)

    def test_a_32_bit_or_big_endian_file_is_refused(self) -> None:
        self.fails("elf-x86_64", fx.elf64(fx.EM_X86_64, ident_class=1), "bad-class")
        self.fails("elf-x86_64", fx.elf64(fx.EM_X86_64, ident_data=2), "bad-class")

    def test_a_relocatable_object_is_not_an_executable(self) -> None:
        self.fails("elf-x86_64", fx.elf64(fx.EM_X86_64, etype=1), "wrong-filetype")

    def test_a_mach_o_file_is_not_an_elf(self) -> None:
        self.fails("elf-x86_64", fx.macho_arm64(), "bad-magic")

    def test_program_headers_that_run_past_the_end_of_the_file_are_refused(self) -> None:
        data = fx.elf64(fx.EM_X86_64)
        self.fails("elf-x86_64", data[:70], "too-short")


class InputCase(CheckCase):
    def test_empty_and_tiny_files_are_too_short(self) -> None:
        for kind in ("macho-arm64", "elf-x86_64", "elf-aarch64"):
            for data in (b"", b"\x7fELF", bytes.fromhex("cffaedfe")):
                with self.subTest(kind=kind, size=len(data)):
                    p = self.check(kind, data)
                    self.assertEqual(p.returncode, 1, p.stdout)
                    self.assertTrue(p.stdout.startswith(("FAIL too-short: ", "FAIL bad-magic: ")), p.stdout)

    def test_a_text_file_is_bad_magic(self) -> None:
        self.fails("elf-x86_64", b"#!/bin/sh\necho hello\n" * 10, "bad-magic")

    def test_a_missing_file_is_unreadable(self) -> None:
        p = subprocess.run([sys.executable, "-I", str(CHECK), "elf-x86_64", str(self.tmp / "nothing")], capture_output=True, text=True, timeout=60)  # nosec B603: argument list, no shell
        self.assertEqual((p.returncode, p.stdout.startswith("FAIL unreadable: ")), (1, True), p.stdout)

    def test_usage_errors_exit_2(self) -> None:
        for args in ([], ["macho-arm64"], ["macho-x86", "x"], ["elf-x86_64", "a", "b"]):
            with self.subTest(args=args):
                p = subprocess.run([sys.executable, "-I", str(CHECK), *args], capture_output=True, text=True, timeout=60)  # nosec B603: argument list, no shell
                self.assertEqual(p.returncode, 2, p.stdout)
                self.assertIn("usage", p.stderr)

    def test_the_checker_reads_nothing_but_the_file_and_writes_nothing(self) -> None:
        f = self.tmp / "client"
        f.write_bytes(fx.macho_arm64())
        before = sorted(os.listdir(self.tmp))
        p = subprocess.run([sys.executable, "-I", str(CHECK), "macho-arm64", str(f)], capture_output=True, text=True, timeout=60, cwd=self.tmp)  # nosec B603: argument list, no shell
        self.assertEqual(p.returncode, 0, p.stdout)
        self.assertEqual(sorted(os.listdir(self.tmp)), before)


if __name__ == "__main__":
    unittest.main()
