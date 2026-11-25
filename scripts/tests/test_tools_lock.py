"""Tests for scripts/lib/tools_lock.py, the tools.lock reader and installer behind

Installs use file:// fixtures built in a scratch directory; nothing is downloaded. The cases of a tool with several sources
also read from servers this file starts on 127.0.0.1.
"""

from __future__ import annotations

import collections
import contextlib
import hashlib
import http.server
import io
import json
import os
import re
import shutil
import subprocess  # nosec B404: tests start the scripts under test and git with argument lists, never a shell
import sys
import tarfile
import tempfile
import threading
import time
import unittest
import zipfile
from collections.abc import Callable
from pathlib import Path
from unittest import mock

REPO = Path(__file__).resolve().parents[2]
CLI = REPO / "scripts" / "lib" / "tools_lock.py"
LOCK = REPO / "tools.lock"

sys.path.insert(0, str(REPO / "scripts" / "lib"))

import wait_scale  # noqa: E402 (scripts/lib: the scale of a wait's ceiling on a busy host)

SCALE = wait_scale.from_environment()


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def tgz(member: str, content: bytes) -> bytes:
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as tar:
        info = tarfile.TarInfo(member)
        info.size = len(content)
        info.mode = 0o755
        tar.addfile(info, io.BytesIO(content))
    return buf.getvalue()


class ToolsLockCase(unittest.TestCase):
    def setUp(self) -> None:
        import tools_lock  # noqa: PLC0415 (imported after sys.path setup)
        self.tl = tools_lock
        self.tmp = Path(tempfile.mkdtemp(prefix="tools-lock-"))
        self.dest = self.tmp / "tools"

    def write_lock(self, body: str) -> Path:
        p = self.tmp / "tools.lock"
        p.write_text('schema = "1"\n' + body)
        return p

    def run_cli(self, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run([sys.executable, str(CLI), *args], capture_output=True, text=True, timeout=120)  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool

    def test_repo_lock_is_valid(self) -> None:
        lock = self.tl.load(LOCK)
        names = {t["name"] for t in lock["tool"]}
        for required in ("rustup-init", "cargo-lambda", "cargo-zigbuild", "zig", "cargo-nextest",
                         "cargo-llvm-cov", "sccache", "awscli", "gh", "gitleaks", "jq", "docker-buildx", "uv"):
            self.assertIn(required, names)
        self.assertRegex(lock["gate_tools"]["python"], r"^3\.12\.\d+$")
        for tool in lock["gate_tools"]["tools"]:
            self.assertIn(tool, names)
        for tool in lock["tool"]:
            self.assertRegex(tool["sha256"], r"^[0-9a-f]{64}$", tool["name"])
            self.assertTrue(tool["url"].startswith("https://"), tool["name"])
            for mirror in tool.get("mirrors", []):
                self.assertTrue(mirror.startswith("https://"), tool["name"])
        self.assertRegex(lock["rust"]["toolchain"], r"^\d+\.\d+\.\d+$")
        for image in ("node", "binfmt", "hello_arm64"):
            self.assertRegex(lock["containers"][image], r"@sha256:[0-9a-f]{64}$", image)

    def test_b941_zig_comes_from_mirrors_of_its_own_list_and_from_its_origin_last(self) -> None:
        """ziglang.org promises no speed or uptime and asks automated downloads to use its community mirrors, with itself as the last source. The entry names at least
        three mirrors: each an https URL of the same archive on a host of its own, with the `source` parameter the mirror list asks for; `url` stays ziglang.org."""
        import urllib.parse  # noqa: PLC0415
        zig = {t["name"]: t for t in self.tl.load(LOCK)["tool"]}["zig"]
        origin = urllib.parse.urlsplit(zig["url"])
        self.assertEqual(origin.netloc, "ziglang.org", "the origin is the last source")
        archive = origin.path.rsplit("/", 1)[1]
        self.assertIn(zig["version"], archive)
        mirrors = [urllib.parse.urlsplit(m) for m in zig.get("mirrors", [])]
        self.assertGreaterEqual(len(mirrors), 3, "at least three mirrors stand before ziglang.org")
        hosts = [m.netloc for m in mirrors]
        self.assertEqual(len(set(hosts)), len(hosts), "each mirror is a host of its own")
        for mirror in mirrors:
            with self.subTest(mirror=mirror.netloc):
                self.assertEqual(mirror.scheme, "https")
                self.assertFalse(mirror.netloc == "ziglang.org" or mirror.netloc.endswith(".ziglang.org"), "a mirror is not the origin")
                self.assertEqual(mirror.path.rsplit("/", 1)[1], archive, "a mirror serves the archive under the origin's file name")
                self.assertRegex(mirror.query, r"^source=[a-z][a-z0-9-]+$", "the request says what makes it, as the mirror list asks")
        self.assertEqual(zig["size"], 55478392, "the archive's size in bytes, the ceiling of every further source")
        others = [t["name"] for t in self.tl.load(LOCK)["tool"] if t["name"] != "zig" and "mirrors" in t]
        self.assertEqual(others, [], "the other tools keep their one source")

    def test_b883_the_gate_tools_python_can_run_the_worker_image_helper(self) -> None:
        """Where the gate tool set holds the only managed CPython, the script tests run with the CPython of `[gate_tools].python`, and the
        tests of the worker image's helper `images/worker-python/ham-containers` start the helper with their own interpreter, which
        stands in for the image's python3.12. The helper holds a process by a handle (`os.pidfd_open`, `signal.pidfd_send_signal`).
        The managed CPython 3.12.11 has neither function and 3.12.14 has both, so the pin stays at 3.12.14 or later."""
        lock = self.tl.load(LOCK)
        version = tuple(int(part) for part in lock["gate_tools"]["python"].split("."))
        self.assertGreaterEqual(
            version, (3, 12, 14),
            "tools.lock [gate_tools] python is older than 3.12.14: the managed CPython 3.12.11 has no os.pidfd_open, so the tests of the "
            "worker image's helper (images/worker-python/ham-containers) fail where this interpreter runs the script tests")

    def test_repo_lock_pins_the_audit_scanners(self) -> None:
        # The advisory check runs `cargo audit --json` and `osv-scanner --format json -r <directory>` from the
        # tools folder, so both come from HAMH_TOOLS/bin.
        tools = {t["name"]: t for t in self.tl.load(LOCK)["tool"]}
        for name, host in (("cargo-audit", "https://github.com/rustsec/rustsec/releases/download/cargo-audit%2Fv"),
                           ("osv-scanner", "https://github.com/google/osv-scanner/releases/download/v")):
            with self.subTest(tool=name):
                self.assertIn(name, tools)
                self.assertTrue(tools[name]["url"].startswith(host), tools[name]["url"])
                self.assertIn(tools[name]["version"], tools[name]["url"])
                self.assertEqual([Path(b).name for b in tools[name]["bin"]], [name])

    def test_rejects_non_https_and_bad_digest(self) -> None:
        bad = self.write_lock('[[tool]]\nname = "x"\nversion = "1"\nurl = "http://example.com/x"\nsha256 = "abc"\nkind = "binary"\nbin = ["x"]\n')
        with self.assertRaises(self.tl.LockError):
            self.tl.load(bad, allow_file_urls=False)

    def test_rejects_duplicate_names(self) -> None:
        entry = '[[tool]]\nname = "x"\nversion = "1"\nurl = "https://e/x"\nsha256 = "' + "0" * 64 + '"\nkind = "binary"\nbin = ["x"]\n'
        with self.assertRaises(self.tl.LockError):
            self.tl.load(self.write_lock(entry + entry))

    def test_install_tarball_verifies_digest_and_links_bin(self) -> None:
        data = tgz("pkg/bin/hello", b"#!/bin/sh\necho hi\n")
        src = self.tmp / "hello.tgz"
        src.write_bytes(data)
        lock = self.write_lock(f'[[tool]]\nname = "hello"\nversion = "1.0"\nurl = "file://{src}"\nsha256 = "{sha(data)}"\nkind = "tar.gz"\nbin = ["pkg/bin/hello"]\n')
        proc = self.run_cli("install", "--lock", str(lock), "--dest", str(self.dest), "--allow-file-urls")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        link = self.dest / "bin" / "hello"
        self.assertTrue(link.exists())
        self.assertEqual(subprocess.run([str(link)], capture_output=True, text=True).stdout.strip(), "hi")  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool
        self.assertIn("TOOL OK hello 1.0", proc.stdout)

    def test_digest_mismatch_fails_and_installs_nothing(self) -> None:
        data = tgz("pkg/bin/hello", b"x")
        src = self.tmp / "hello.tgz"
        src.write_bytes(data)
        lock = self.write_lock(f'[[tool]]\nname = "hello"\nversion = "1.0"\nurl = "file://{src}"\nsha256 = "{"1" * 64}"\nkind = "tar.gz"\nbin = ["pkg/bin/hello"]\n')
        proc = self.run_cli("install", "--lock", str(lock), "--dest", str(self.dest), "--allow-file-urls")
        self.assertEqual(proc.returncode, 4)
        self.assertIn("TOOL FAIL hello sha256", proc.stdout)
        self.assertFalse((self.dest / "bin" / "hello").exists())

    def test_zip_and_plain_binary_kinds(self) -> None:
        buf = io.BytesIO()
        with zipfile.ZipFile(buf, "w") as zf:
            zf.writestr("z/tool", "#!/bin/sh\necho z\n")
        zdata = buf.getvalue()
        (self.tmp / "t.zip").write_bytes(zdata)
        bdata = b"#!/bin/sh\necho b\n"
        (self.tmp / "b").write_bytes(bdata)
        lock = self.write_lock(
            f'[[tool]]\nname = "z"\nversion = "2"\nurl = "file://{self.tmp}/t.zip"\nsha256 = "{sha(zdata)}"\nkind = "zip"\nbin = ["z/tool"]\n'
            f'[[tool]]\nname = "b"\nversion = "3"\nurl = "file://{self.tmp}/b"\nsha256 = "{sha(bdata)}"\nkind = "binary"\nbin = ["b"]\n')
        proc = self.run_cli("install", "--lock", str(lock), "--dest", str(self.dest), "--allow-file-urls")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(subprocess.run([str(self.dest / "bin" / "tool")], capture_output=True, text=True).stdout.strip(), "z")  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool
        self.assertEqual(subprocess.run([str(self.dest / "bin" / "b")], capture_output=True, text=True).stdout.strip(), "b")  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool

    def test_dest_outside_workspace_tools_root_is_refused(self) -> None:
        lock = self.write_lock("")
        proc = self.run_cli("install", "--lock", str(lock), "--dest", "/usr/local")
        self.assertEqual(proc.returncode, 2)
        self.assertIn("refusing", proc.stderr)

    # The roots of a destination come from where the command runs, not from a path written for one machine. With no
    # TOOLS_LOCK_ROOTS the roots are the working directory and TMPDIR, so a ham session (checkout in its workspace, TMPDIR beside it) and any other
    # machine need no path written for them. TOOLS_LOCK_ROOTS, when it is set, is the whole list as before; the callers that install outside both
    # name their own destination in it. The paths below that do not exist are
    # fine: allowed_dest compares real paths lexically.

    def allowed(self, path: str, cwd: "str | None" = None, **env: str) -> bool:
        base = {k: v for k, v in os.environ.items() if k not in ("TOOLS_LOCK_ROOTS", "TMPDIR")}
        base.update(env)
        before = os.getcwd()
        try:
            if cwd is not None:
                os.chdir(cwd)
            with mock.patch.dict(os.environ, base, clear=True):
                return self.tl.allowed_dest(Path(path))
        finally:
            os.chdir(before)

    def test_the_temp_directory_is_an_allowed_root_when_no_roots_are_set(self) -> None:
        tmp = "/ham-no-such-session/tmp"
        here = str(self.tmp)
        self.assertTrue(self.allowed(tmp, cwd=here, TMPDIR=tmp))
        self.assertTrue(self.allowed(tmp + "/x/tools", cwd=here, TMPDIR=tmp))
        self.assertFalse(self.allowed("/ham-no-such-session/other", cwd=here, TMPDIR=tmp))
        self.assertFalse(self.allowed("/usr/local", cwd=here, TMPDIR=tmp))
        self.assertFalse(self.allowed("/ham-no-such-session/tmp-sibling", cwd=here, TMPDIR=tmp))

    def test_the_working_directory_is_an_allowed_root_when_no_roots_are_set(self) -> None:
        repo = self.tmp / "session-workspace" / "repo"
        repo.mkdir(parents=True)
        self.assertTrue(self.allowed(str(repo / "tools"), cwd=str(repo)))
        self.assertTrue(self.allowed(str(repo), cwd=str(repo), TMPDIR="/ham-no-such-session/tmp"))
        self.assertFalse(self.allowed(str(repo.parent / "sibling"), cwd=str(repo)))
        self.assertFalse(self.allowed("/usr/local", cwd=str(repo), TMPDIR="/ham-no-such-session/tmp"))

    def test_no_path_of_the_authors_machine_is_a_default_root(self) -> None:
        self.assertFalse(hasattr(self.tl, "DEFAULT_ROOTS"), "the defaults must come from the working directory and TMPDIR, not from a list of paths")
        here = str(self.tmp)
        for author_path in ("/home/someone/work/tools/x", "/Users/you/scratch/x"):
            with self.subTest(path=author_path):
                self.assertFalse(self.allowed(author_path, cwd=here, TMPDIR="/ham-no-such-session/tmp"))

    def test_a_working_directory_that_would_allow_everything_is_ignored(self) -> None:
        self.assertFalse(self.allowed("/usr/local", cwd=os.sep))
        self.assertFalse(self.allowed("/ham-no-such-session/x", cwd=os.sep))

    def test_a_named_list_is_the_whole_list(self) -> None:
        tmp = "/ham-no-such-session/tmp"
        here = str(self.tmp)
        self.assertTrue(self.allowed("/ham-no-such-root/a", cwd=here, TOOLS_LOCK_ROOTS="/ham-no-such-root:/ham-other", TMPDIR=tmp))
        self.assertTrue(self.allowed("/ham-other/b", cwd=here, TOOLS_LOCK_ROOTS="/ham-no-such-root:/ham-other", TMPDIR=tmp))
        self.assertFalse(self.allowed(tmp + "/x", cwd=here, TOOLS_LOCK_ROOTS="/ham-no-such-root", TMPDIR=tmp))
        self.assertFalse(self.allowed(str(self.tmp / "x"), cwd=here, TOOLS_LOCK_ROOTS="/ham-no-such-root"))

    def test_a_temp_directory_that_would_allow_everything_or_is_not_absolute_is_ignored(self) -> None:
        here = str(self.tmp)
        for bad in ("/", "", "relative/tmp", "."):
            with self.subTest(tmpdir=bad):
                self.assertFalse(self.allowed("/usr/local", cwd=here, TMPDIR=bad))
                self.assertFalse(self.allowed("/ham-no-such-session/tmp", cwd=here, TMPDIR=bad))

    def test_the_command_line_accepts_a_destination_under_tmpdir(self) -> None:
        # The tests of this module put their destinations under TMPDIR: install into one with no TOOLS_LOCK_ROOTS in the environment.
        lock = self.write_lock("")
        env = {k: v for k, v in os.environ.items() if k != "TOOLS_LOCK_ROOTS"}
        env["TMPDIR"] = str(self.tmp)
        proc = subprocess.run([sys.executable, str(CLI), "install", "--lock", str(lock), "--dest", str(self.dest)], capture_output=True, text=True, env=env, cwd="/", timeout=120)  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script and scratch paths
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)

    def test_the_command_line_accepts_a_destination_under_the_working_directory_with_no_tmpdir(self) -> None:
        # A ham session started in its workspace: no TMPDIR guarantee, no TOOLS_LOCK_ROOTS, the destination inside the checkout.
        lock = self.write_lock("")
        env = {k: v for k, v in os.environ.items() if k not in ("TOOLS_LOCK_ROOTS", "TMPDIR")}
        proc = subprocess.run([sys.executable, str(CLI), "install", "--lock", str(lock), "--dest", str(self.dest)], capture_output=True, text=True, env=env, cwd=str(self.tmp), timeout=120)  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script and scratch paths
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)

    def test_second_install_is_idempotent(self) -> None:
        data = tgz("pkg/bin/hello", b"#!/bin/sh\necho hi\n")
        (self.tmp / "hello.tgz").write_bytes(data)
        lock = self.write_lock(f'[[tool]]\nname = "hello"\nversion = "1.0"\nurl = "file://{self.tmp}/hello.tgz"\nsha256 = "{sha(data)}"\nkind = "tar.gz"\nbin = ["pkg/bin/hello"]\n')
        self.run_cli("install", "--lock", str(lock), "--dest", str(self.dest), "--allow-file-urls")
        proc = self.run_cli("install", "--lock", str(lock), "--dest", str(self.dest), "--allow-file-urls")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn("TOOL OK hello 1.0 (present)", proc.stdout)

    def test_check_reports_installed_missing_and_stale_tools_and_downloads_nothing(self) -> None:
        # `check` is the read-only S11 form of `install` (bootstrap --check): it reads the state files
        # and bin links under dest and never fetches or unpacks.
        data = tgz("pkg/bin/hello", b"#!/bin/sh\necho hi\n")
        (self.tmp / "hello.tgz").write_bytes(data)
        entry = f'[[tool]]\nname = "hello"\nversion = "1.0"\nurl = "file://{self.tmp}/hello.tgz"\nsha256 = "{sha(data)}"\nkind = "tar.gz"\nbin = ["pkg/bin/hello"]\n'
        self.run_cli("install", "--lock", str(self.write_lock(entry)), "--dest", str(self.dest), "--allow-file-urls")
        missing = '[[tool]]\nname = "gone"\nversion = "2"\nurl = "https://example.invalid/gone"\nsha256 = "' + "2" * 64 + '"\nkind = "binary"\nbin = ["gone"]\n'
        proc = self.run_cli("check", "--lock", str(self.write_lock(entry + missing)), "--dest", str(self.dest), "--allow-file-urls")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertIn("TOOL OK hello 1.0 (present)", proc.stdout)
        self.assertIn("TOOL FAIL gone not installed", proc.stdout)
        self.assertFalse((self.dest / "pkgs" / "gone-2").exists())
        stale = entry.replace('version = "1.0"', 'version = "1.1"')
        proc = self.run_cli("check", "--lock", str(self.write_lock(stale)), "--dest", str(self.dest), "--allow-file-urls")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertIn("TOOL FAIL hello not installed", proc.stdout)
        proc = self.run_cli("check", "--lock", str(self.write_lock(entry)), "--dest", str(self.dest), "--allow-file-urls")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)



def entry(name: str, url: str, digest: str, mirrors: "list[str] | None" = None, size: "int | None" = None) -> str:
    """One [[tool]] table of a made-up lock: a tar.gz with the file pkg/bin/<name>; `mirrors` and `size` only when given."""
    text = f'[[tool]]\nname = "{name}"\nversion = "1.0"\nurl = "{url}"\n'
    if mirrors is not None:
        text += "mirrors = [" + ", ".join(f'"{m}"' for m in mirrors) + "]\n"
    if size is not None:
        text += f"size = {size}\n"
    return text + f'sha256 = "{digest}"\nkind = "tar.gz"\nbin = ["pkg/bin/{name}"]\n'


class SourcesBase(unittest.TestCase):
    def setUp(self) -> None:
        import tools_lock  # noqa: PLC0415 (imported after sys.path setup)
        self.tl = tools_lock
        self.tmp = Path(tempfile.mkdtemp(prefix="tools-lock-sources-"))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.dest = self.tmp / "tools"
        self.data = tgz("pkg/bin/hello", b"#!/bin/sh\necho hi\n")
        self.good = self.tmp / "hello.tgz"
        self.good.write_bytes(self.data)
        self.other = self.tmp / "other.tgz"
        self.other.write_bytes(tgz("pkg/bin/hello", b"#!/bin/sh\necho other\n"))
        self.missing = self.tmp / "no-such.tgz"

    def write_lock(self, body: str) -> Path:
        p = self.tmp / "tools.lock"
        p.write_text('schema = "1"\n' + body)
        return p

    def install(self, body: str, *flags: str) -> subprocess.CompletedProcess:
        return subprocess.run([sys.executable, str(CLI), "install", "--lock", str(self.write_lock(body)), "--dest", str(self.dest), *(flags or ("--allow-file-urls",))],  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script and scratch paths
                              capture_output=True, text=True, timeout=120)

    @staticmethod
    def end_lines(proc: subprocess.CompletedProcess) -> "list[str]":
        """The lines that end a tool: its TOOL OK or its TOOL FAIL."""
        return [ln for ln in proc.stdout.splitlines() if ln.startswith(("TOOL OK", "TOOL FAIL"))]

    def hello_runs(self) -> str:
        link = self.dest / "bin" / "hello"
        return subprocess.run([str(link)], capture_output=True, text=True).stdout.strip() if link.exists() else ""  # nosec B603: argument list, no shell, the file this test installed


class OneSourceCase(SourcesBase):
    """What a tool with one source prints and returns. A build log, the setup runner and the fleet's toolchain script read these lines and exit codes,
    and they stay when a tool gets further sources: its `url` is then its last source and fails in the same words."""

    def test_b941_a_download_that_fails_is_one_tool_fail_line_and_exit_5(self) -> None:
        proc = self.install(entry("hello", f"file://{self.missing}", sha(self.data)))
        self.assertEqual(proc.returncode, 5, proc.stdout + proc.stderr)
        lines = self.end_lines(proc)
        self.assertEqual(len(lines), 1, proc.stdout)
        self.assertTrue(lines[0].startswith("TOOL FAIL hello download "), lines[0])
        self.assertIn("No such file or directory", lines[0])
        self.assertEqual(self.hello_runs(), "")

    def test_b941_other_bytes_are_one_tool_fail_line_with_both_digests_and_exit_4(self) -> None:
        proc = self.install(entry("hello", f"file://{self.other}", sha(self.data)))
        self.assertEqual(proc.returncode, 4, proc.stdout + proc.stderr)
        self.assertEqual(self.end_lines(proc), [f"TOOL FAIL hello sha256 want={sha(self.data)} got={sha(self.other.read_bytes())}"])
        self.assertEqual(self.hello_runs(), "")

    def test_b941_the_exit_code_of_several_tools(self) -> None:
        gone = entry("gone", f"file://{self.missing}", sha(self.data))
        wrong = entry("wrong", f"file://{self.other}", sha(self.data))
        fine = entry("hello", f"file://{self.good}", sha(self.data))
        for body, code, starts in ((gone + wrong, 4, ("TOOL FAIL gone download ", "TOOL FAIL wrong sha256 ")),
                                   (wrong + gone, 4, ("TOOL FAIL wrong sha256 ", "TOOL FAIL gone download ")),
                                   (gone + fine, 5, ("TOOL FAIL gone download ", "TOOL OK hello 1.0"))):
            with self.subTest(code=code, starts=starts):
                proc = self.install(body)
                self.assertEqual(proc.returncode, code, proc.stdout + proc.stderr)
                final = self.end_lines(proc)
                self.assertEqual(len(final), 2, proc.stdout)
                for line, start in zip(final, starts):
                    self.assertTrue(line.startswith(start), line)

    def test_b941_one_ok_line_for_an_installed_tool_and_the_present_line_for_the_next_run(self) -> None:
        body = entry("hello", f"file://{self.good}", sha(self.data))
        proc = self.install(body)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        final = self.end_lines(proc)
        self.assertEqual(len(final), 1, proc.stdout)
        self.assertTrue(final[0].startswith("TOOL OK hello 1.0"), final[0])
        self.assertEqual(self.hello_runs(), "hi")
        again = self.install(body)
        self.assertEqual((again.returncode, again.stdout), (0, "TOOL OK hello 1.0 (present)\n"), again.stderr)


GIVEN_UP = r"^TOOL SOURCE hello %s given up after \d+\.\ds: %s"


def in_order(mirrors: "list[str]") -> "list[str]":
    """The draw a test fixes: the further sources as the lock names them (install's parameter `draw`; the product draws at random)."""
    return list(mirrors)


def turned(mirrors: "list[str]") -> "list[str]":
    """Another fixed draw: the further sources from the last to the first."""
    return list(reversed(mirrors))


class FurtherSourcesCase(SourcesBase):
    """A tool may name further sources of its archive in `mirrors`. At most two of them are tried before `url`, in an order drawn for each install, every one against the pinned SHA-256, and a
    source that is given up is named with why. The `url` stays the last source and fails as the one source of a tool does."""

    def test_b941_a_mirror_that_is_missing_is_given_up_and_the_url_installs(self) -> None:
        proc = self.install(entry("hello", f"file://{self.good}", sha(self.data), mirrors=[f"file://{self.missing}"], size=len(self.data)))
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertRegex(proc.stdout, re.compile(GIVEN_UP % ("file", ".*No such file or directory"), re.M))
        self.assertNotIn("TOOL FAIL", proc.stdout)
        self.assertLess(proc.stdout.index("TOOL SOURCE hello"), proc.stdout.index("TOOL OK hello 1.0"), "the source that was given up is named before the tool is done")
        self.assertEqual(self.hello_runs(), "hi")

    def test_b941_a_mirror_with_other_bytes_is_given_up_and_never_installed(self) -> None:
        # other bytes of the archive's own size: the pinned SHA-256 refuses them
        flipped = self.tmp / "flipped.tgz"
        flipped.write_bytes(bytes([self.data[0] ^ 0xFF]) + self.data[1:])
        proc = self.install(entry("hello", f"file://{self.good}", sha(self.data), mirrors=[f"file://{flipped}"], size=len(self.data)))
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertRegex(proc.stdout, re.compile(GIVEN_UP % ("file", f"sha256 want={sha(self.data)} got={sha(flipped.read_bytes())}$"), re.M))
        self.assertNotIn("TOOL FAIL", proc.stdout)
        self.assertEqual(self.hello_runs(), "hi", "what is installed is the pinned archive, whatever a source sent")
        # another archive, longer than the pinned one: the size of the lock refuses it at the first byte too many
        shutil.rmtree(self.dest, True)
        self.assertGreater(len(self.other.read_bytes()), len(self.data), "TRIGGER PRESENT: the other archive is longer than the pinned one")
        proc = self.install(entry("hello", f"file://{self.good}", sha(self.data), mirrors=[f"file://{self.other}"], size=len(self.data)))
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertRegex(proc.stdout, re.compile(GIVEN_UP % ("file", f"more than the archive's {len(self.data)} bytes$"), re.M))
        self.assertNotIn("TOOL FAIL", proc.stdout)
        self.assertEqual(self.hello_runs(), "hi", "what is installed is the pinned archive, whatever a source sent")
        # the pinned archive and one byte more: the ceiling is the archive's size to the byte
        shutil.rmtree(self.dest, True)
        longer = self.tmp / "one-more.tgz"
        longer.write_bytes(self.data + b"\0")
        proc = self.install(entry("hello", f"file://{self.good}", sha(self.data), mirrors=[f"file://{longer}"], size=len(self.data)))
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertRegex(proc.stdout, re.compile(GIVEN_UP % ("file", f"more than the archive's {len(self.data)} bytes$"), re.M))
        self.assertNotIn("TOOL FAIL", proc.stdout)
        self.assertEqual(self.hello_runs(), "hi", "what is installed is the pinned archive, whatever a source sent")

    def test_b941_the_mirrors_are_tried_in_the_drawn_order_and_the_first_good_one_ends_the_search(self) -> None:
        # the url does not exist: a run that reached it would fail, so exit 0 shows that a mirror ended the search
        mirrors = [f"file://{self.missing}", f"file://{self.good}"]
        tool = {"name": "hello", "version": "1.0", "url": f"file://{self.tmp}/never-asked.tgz", "mirrors": mirrors, "size": len(self.data), "sha256": sha(self.data),
                "kind": "tar.gz", "bin": ["pkg/bin/hello"]}
        for draw, given_up in ((in_order, 1), (turned, 0)):
            with self.subTest(draw=draw.__name__):
                shutil.rmtree(self.dest, True)
                out = io.StringIO()
                with contextlib.redirect_stdout(out):
                    rc = self.tl.install({"schema": "1", "tool": [tool]}, self.dest, None, draw=draw)
                self.assertEqual(rc, 0, out.getvalue())
                self.assertEqual(len(re.findall(r"^TOOL SOURCE hello ", out.getvalue(), re.M)), given_up, "the good mirror ends the search where the draw puts it")
                self.assertNotIn("TOOL FAIL", out.getvalue())
                self.assertEqual(self.hello_runs(), "hi")
        # the command line draws the order itself: whichever mirror comes first, the good one ends the search before the url
        shutil.rmtree(self.dest, True)
        proc = self.install(entry("hello", f"file://{self.tmp}/never-asked.tgz", sha(self.data), mirrors=mirrors, size=len(self.data)))
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertLessEqual(len(re.findall(r"^TOOL SOURCE hello ", proc.stdout, re.M)), 1, proc.stdout)
        self.assertNotIn("TOOL FAIL", proc.stdout)
        self.assertEqual(self.hello_runs(), "hi")

    def test_b941_installers_that_are_processes_of_their_own_do_not_all_ask_the_same_source_first(self) -> None:
        """The order is drawn anew by every installer, so that no one source carries every build. 12 runs of the command line, each a process of its own, with a
        lock of four further sources that are all missing; the source a run asked first is read from the path in its first TOOL SOURCE line. Under a fair draw
        one source comes first in all 12 runs once in 4 to the power of 11 times."""
        mirrors = [f"file://{self.tmp}/mirror-{n}.tgz" for n in range(4)]
        body = entry("hello", f"file://{self.good}", sha(self.data), mirrors=mirrors, size=len(self.data))
        first: "collections.Counter[str]" = collections.Counter()
        for _ in range(12):
            shutil.rmtree(self.dest, True)
            proc = self.install(body)
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            asked = re.findall(r"^TOOL SOURCE hello file given up after \d+\.\ds: .*No such file or directory: '[^']*/(mirror-\d)\.tgz'", proc.stdout, re.M)
            self.assertEqual(len(asked), 2, f"TRIGGER PRESENT: a run asks two further sources and names each by its path: {proc.stdout}")
            self.assertEqual(len(set(asked)), 2, "no source is asked twice")
            self.assertEqual(self.hello_runs(), "hi")
            first[asked[0]] += 1
        self.assertEqual(sum(first.values()), 12)
        self.assertGreater(len(first), 1, f"12 installers, each a process of its own, all asked the same further source first: {dict(first)}")

    def test_b941_when_every_source_is_bad_the_url_fails_the_tool_in_todays_words(self) -> None:
        mirrors = [f"file://{self.missing}", f"file://{self.other}"]
        gone = self.install(entry("hello", f"file://{self.tmp}/also-gone.tgz", sha(self.data), mirrors=mirrors, size=len(self.data)))
        self.assertEqual(gone.returncode, 5, gone.stdout + gone.stderr)
        self.assertEqual(len(re.findall(r"^TOOL SOURCE hello file given up", gone.stdout, re.M)), 2, gone.stdout)
        last = gone.stdout.splitlines()[-1]
        self.assertTrue(last.startswith("TOOL FAIL hello download ") and "also-gone.tgz" in last, last)
        self.assertEqual(self.hello_runs(), "")
        wrong = self.install(entry("hello", f"file://{self.other}", sha(self.data), mirrors=[f"file://{self.missing}"], size=len(self.data)))
        self.assertEqual(wrong.returncode, 4, wrong.stdout + wrong.stderr)
        self.assertEqual(wrong.stdout.splitlines()[-1], f"TOOL FAIL hello sha256 want={sha(self.data)} got={sha(self.other.read_bytes())}")
        self.assertEqual(self.hello_runs(), "")

    def test_b941_mirrors_are_https_urls_and_file_urls_only_with_the_flag_of_the_tests(self) -> None:
        digest = sha(self.data)
        for mirrors, why in ((["http://example.com/x"], "plain http"), (["ftp://example.com/x"], "another scheme"), (["https://example.com/x", ""], "an empty entry"),
                             ([f"file://{self.good}"], "a file URL without the flag")):
            with self.subTest(why=why):
                with self.assertRaises(self.tl.LockError) as caught:
                    self.tl.load(self.write_lock(entry("hello", "https://example.com/hello.tgz", digest, mirrors=mirrors, size=len(self.data))))
                self.assertIn("tool hello: mirrors", str(caught.exception))
        for raw, why in (('mirrors = "https://example.com/x"\n', "a string"), ("mirrors = [1]\n", "a number in the list")):
            with self.subTest(why=why):
                body = entry("hello", "https://example.com/hello.tgz", digest).replace("sha256 =", raw + "sha256 =")
                with self.assertRaises(self.tl.LockError):
                    self.tl.load(self.write_lock(body))
        lock = self.tl.load(self.write_lock(entry("hello", "https://example.com/hello.tgz", digest, mirrors=["https://mirror.example/hello.tgz"], size=len(self.data))))
        self.assertEqual(lock["tool"][0]["mirrors"], ["https://mirror.example/hello.tgz"])
        self.tl.load(self.write_lock(entry("hello", f"file://{self.good}", digest, mirrors=[f"file://{self.good}"], size=len(self.data))), allow_file_urls=True)
        refused = self.install(entry("hello", "https://example.com/hello.tgz", digest, mirrors=[f"file://{self.good}"], size=len(self.data)), "--only", "hello")
        self.assertEqual(refused.returncode, 3, refused.stdout + refused.stderr)
        self.assertIn("invalid lock: tool hello: mirrors", refused.stderr)

    def test_b941_a_tool_with_further_sources_names_the_size_of_its_archive(self) -> None:
        """The archive's size is the ceiling of a further source, so the lock check refuses `mirrors` without it: exit 3 and a line that says so."""
        digest, mirrors = sha(self.data), [f"file://{self.missing}"]
        refused = self.install(entry("hello", f"file://{self.good}", digest, mirrors=mirrors))
        self.assertEqual((refused.returncode, refused.stdout), (3, ""), refused.stderr)
        self.assertEqual(refused.stderr.strip(), "invalid lock: tool hello: mirrors need size, the archive's size in bytes: a further source is given up when it sends more")
        self.assertEqual(self.hello_runs(), "", "nothing is installed from a lock that is refused")
        with self.assertRaises(self.tl.LockError):
            self.tl.load(self.write_lock(entry("hello", "https://example.com/hello.tgz", digest, mirrors=["https://mirror.example/hello.tgz"])))
        for raw, why in (('"150"', "a string"), ("0", "zero"), ("-1", "below zero"), ("1.5", "a fraction"), ("true", "a truth value"), ("[150]", "a list")):
            with self.subTest(why=why):
                body = entry("hello", f"file://{self.good}", digest, mirrors=mirrors).replace("sha256 =", f"size = {raw}\nsha256 =")
                with self.assertRaises(self.tl.LockError) as caught:
                    self.tl.load(self.write_lock(body), allow_file_urls=True)
                self.assertIn("tool hello: size must be the archive's size in bytes", str(caught.exception))
        # A tool without further sources needs no size: it behaves as it always did. An empty list names no further source.
        for body in (entry("hello", f"file://{self.good}", digest), entry("hello", f"file://{self.good}", digest, mirrors=[])):
            proc = self.install(body)
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            shutil.rmtree(self.dest, True)
        proc = self.install(entry("hello", f"file://{self.good}", digest, mirrors=mirrors, size=len(self.data)))
        self.assertEqual((proc.returncode, self.hello_runs()), (0, "hi"), proc.stdout + proc.stderr)

    def test_b941_a_size_on_a_tool_without_further_sources_is_not_read(self) -> None:
        """A tool without further sources is read as it was before the key `size` existed: the key is the ceiling of a further source and is checked only where
        `mirrors` names one. A word, a zero and a list as `size` each install with exit 0, without the key `mirrors` and with an empty list."""
        digest = sha(self.data)
        for raw, why in (('"big"', "a word"), ("0", "zero"), ("[150]", "a list")):
            for mirrors, form in ((None, "no key mirrors"), ([], "an empty list")):
                with self.subTest(size=why, mirrors=form):
                    shutil.rmtree(self.dest, True)
                    proc = self.install(entry("hello", f"file://{self.good}", digest, mirrors=mirrors).replace("sha256 =", f"size = {raw}\nsha256 ="))
                    self.assertEqual((proc.returncode, proc.stderr), (0, ""), proc.stdout)
                    lines = proc.stdout.splitlines()
                    self.assertEqual(len(lines), 2, proc.stdout)
                    self.assertEqual(lines[0], "TOOL GET hello 1.0")
                    self.assertRegex(lines[1], r"^TOOL OK hello 1\.0 \d+\.\ds from file$")
                    self.assertEqual(self.hello_runs(), "hi")
        # the same word beside a further source is refused: the key is checked where it is used
        shutil.rmtree(self.dest, True)
        refused = self.install(entry("hello", f"file://{self.good}", digest, mirrors=[f"file://{self.missing}"]).replace("sha256 =", 'size = "big"\nsha256 ='))
        self.assertEqual((refused.returncode, refused.stdout), (3, ""), refused.stderr)
        self.assertEqual(refused.stderr.strip(), "invalid lock: tool hello: size must be the archive's size in bytes, a whole number above 0")
        self.assertEqual(self.hello_runs(), "")

    def test_b941_the_download_of_a_further_source_runs_in_a_daemon_thread(self) -> None:
        """A source that was given up may hold its connection for as long as it likes. Only a daemon thread lets the installer end then, so the flag of the thread that
        the installer starts for a further source is read here, where it is made."""
        made: "list[threading.Thread]" = []
        real = threading.Thread

        class Recorded(real):  # type: ignore[misc, valid-type]
            def __init__(self, *args: object, **kwargs: object) -> None:
                super().__init__(*args, **kwargs)
                made.append(self)

        tool = {"name": "hello", "version": "1.0", "url": f"file://{self.good}", "mirrors": [f"file://{self.missing}"], "size": len(self.data), "sha256": sha(self.data),
                "kind": "tar.gz", "bin": ["pkg/bin/hello"]}
        out = io.StringIO()
        with mock.patch.object(self.tl.threading, "Thread", Recorded), contextlib.redirect_stdout(out):
            rc = self.tl.install({"schema": "1", "tool": [tool]}, self.dest, None)
        self.assertEqual(rc, 0, out.getvalue())
        self.assertEqual(len(made), 1, "TRIGGER PRESENT: the installer started one thread, for the download of the further source")
        self.assertIs(made[0].daemon, True, "the download of a further source must run in a daemon thread")


class ToolLinesCase(SourcesBase):
    """The installer's lines come as they happen, so a log that follows it holds a time for each tool: a line when a tool's download starts, and the tool's
    OK line with its seconds and the source that delivered the archive."""

    def test_b941_a_download_has_a_start_line_and_an_ok_line_with_seconds_and_source(self) -> None:
        body = entry("hello", f"file://{self.good}", sha(self.data))
        proc = self.install(body)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        lines = proc.stdout.splitlines()
        self.assertEqual(len(lines), 2, proc.stdout)
        self.assertEqual(lines[0], "TOOL GET hello 1.0")
        self.assertRegex(lines[1], r"^TOOL OK hello 1\.0 \d+\.\ds from file$")
        again = self.install(body)
        self.assertEqual(again.stdout, "TOOL OK hello 1.0 (present)\n", "a tool that is present downloads nothing and has no start line")

    def test_b941_a_tool_that_fails_has_its_start_line_too(self) -> None:
        proc = self.install(entry("hello", f"file://{self.missing}", sha(self.data)))
        lines = proc.stdout.splitlines()
        self.assertEqual(len(lines), 2, proc.stdout)
        self.assertEqual(lines[0], "TOOL GET hello 1.0")
        self.assertTrue(lines[1].startswith("TOOL FAIL hello download "), lines[1])

    def test_b941_the_lines_come_while_the_installer_still_runs(self) -> None:
        # The second tool's source is a named pipe: the installer waits in that download until this test writes the archive into the pipe.
        pipe = self.tmp / "later.tgz"
        os.mkfifo(pipe)
        later = tgz("pkg/bin/later", b"#!/bin/sh\necho later\n")
        body = entry("hello", f"file://{self.good}", sha(self.data)) + entry("later", f"file://{pipe}", sha(later))
        proc = subprocess.Popen([sys.executable, str(CLI), "install", "--lock", str(self.write_lock(body)), "--dest", str(self.dest), "--allow-file-urls"],  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script and scratch paths
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        self.addCleanup(proc.wait)
        self.addCleanup(proc.kill)
        seen: "list[str]" = []

        def read() -> None:
            for line in proc.stdout:
                seen.append(line.rstrip("\n"))

        reader = threading.Thread(target=read, daemon=True)
        reader.start()

        def both_shown() -> bool:
            return any(ln.startswith("TOOL OK hello 1.0 ") for ln in seen) and "TOOL GET later 1.0" in seen

        ceiling = 20 * SCALE
        end = time.monotonic() + ceiling
        while not both_shown() and time.monotonic() < end:
            time.sleep(0.05)
        self.assertTrue(both_shown(), f"{ceiling:g} s into the second download the installer had shown {seen!r}: the first tool's line and the second tool's start are missing")
        self.assertIsNone(proc.poll(), "TRIGGER PRESENT: the installer is still in its second download")
        with open(pipe, "wb") as fh:
            fh.write(later)
        self.assertEqual(proc.wait(timeout=60 * SCALE), 0, seen)
        reader.join(10 * SCALE)
        self.assertRegex(seen[-1], r"^TOOL OK later 1\.0 \d+\.\ds from file$")


class CommandLineCase(SourcesBase):
    """What the command line's own path hands to the downloads of a tool. `main` runs in this process with the arguments of the program, and every
    download is recorded where the installer starts it."""

    def run_main(self, lock: Path, *flags: str, command: str = "install") -> "tuple[int, list[tuple], str]":
        """`main` with the command line `<command> --lock <lock> --dest <dest> <flags>`. A further source's download is recorded as (url, limit, ceiling) and given
        up with the test's reason. The last source's is recorded as (url,): a file is then downloaded as always, any other source fails without a request."""
        asked: "list[tuple]" = []
        real_fetch = self.tl.fetch

        def further(url: str, out: Path, limit_s: float, *more: object) -> str:
            asked.append((url, limit_s, *more))
            return "recorded by the test"

        def last(url: str, out: Path) -> None:
            asked.append((url,))
            if not url.startswith("file://"):
                raise OSError("recorded by the test")
            real_fetch(url, out)

        argv = [str(CLI), command, "--lock", str(lock), "--dest", str(self.dest), *flags]
        shown = io.StringIO()
        with mock.patch.object(self.tl, "fetch_within", further), mock.patch.object(self.tl, "fetch", last), mock.patch.object(sys, "argv", argv), \
                mock.patch.dict(os.environ, {"TOOLS_LOCK_ROOTS": str(self.tmp)}), contextlib.redirect_stdout(shown):
            rc = self.tl.main()
        return rc, asked, shown.getvalue()

    def test_b941_the_command_line_gives_a_further_source_60_seconds(self) -> None:
        mirrors = [f"file://{self.tmp}/further-{n}.tgz" for n in range(4)]
        lock = self.write_lock(entry("hello", f"file://{self.good}", sha(self.data), mirrors=mirrors, size=len(self.data)))
        rc, asked, shown = self.run_main(lock, "--allow-file-urls")
        self.assertEqual(rc, 0, shown)
        further = asked[:-1]
        self.assertEqual(len(further), 2, "TRIGGER PRESENT: the command line's path asked further sources before the last one")
        self.assertEqual(asked[-1], (f"file://{self.good}",))
        for url, limit_s, *more in further:
            self.assertIn(url, mirrors)
            self.assertEqual(limit_s, 60, f"the command line's path gave {url} {limit_s:g} s")
            self.assertEqual(limit_s, self.tl.SOURCE_LIMIT_S)
            self.assertEqual(more, [len(self.data)], "the ceiling of a further source is the archive's size of the lock")
        self.assertEqual(len(re.findall(r"^TOOL SOURCE hello file given up after \d+\.\ds: recorded by the test$", shown, re.M)), 2, shown)
        self.assertEqual(self.hello_runs(), "hi")

    def test_b941_the_gate_tools_command_gives_a_further_source_60_seconds_too(self) -> None:
        """`gate-tools` installs its tools through the same installer. No gate tool of the repository's lock names further sources today; one that does gets the
        limit and the ceiling that `install` gives. A uv that makes a Python (FAKE_UV) and an installer that only records its arguments let the command run to its end."""
        uv, gitleaks = tgz("pkg/bin/uv", FAKE_UV.encode()), tgz("pkg/bin/gitleaks", b"#!/bin/sh\necho 8.0.0\n")
        (self.tmp / "uv.tgz").write_bytes(uv)
        (self.tmp / "gitleaks.tgz").write_bytes(gitleaks)
        mirrors = [f"file://{self.tmp}/further-{n}.tgz" for n in range(4)]
        lock = self.write_lock(entry("uv", f"file://{self.tmp}/uv.tgz", sha(uv), mirrors=mirrors, size=len(uv)) + entry("gitleaks", f"file://{self.tmp}/gitleaks.tgz", sha(gitleaks))
                               + '[gate_tools]\npython = "3.12.99"\ntools = ["uv", "gitleaks"]\n')
        installer = self.tmp / "install.sh"
        installer.write_text('#!/usr/bin/env bash\nprintf \'%s\\n\' "$@" > "$FAKE_INSTALLER_LOG"\n')
        installer.chmod(0o755)
        with mock.patch.dict(os.environ, {"FAKE_UV_LOG": str(self.tmp / "uv.log"), "FAKE_INSTALLER_LOG": str(self.tmp / "installer.log")}):
            rc, asked, shown = self.run_main(lock, "--installer", str(installer), "--allow-file-urls", command="gate-tools")
        self.assertEqual(rc, 0, shown)
        further = [a for a in asked if len(a) > 1]
        self.assertEqual(len(further), 2, "TRIGGER PRESENT: the gate-tools command's path asked further sources of a gate tool")
        for url, limit_s, *more in further:
            self.assertIn(url, mirrors)
            self.assertEqual(limit_s, 60, f"the gate-tools command's path gave {url} {limit_s:g} s")
            self.assertEqual(limit_s, self.tl.SOURCE_LIMIT_S)
            self.assertEqual(more, [len(uv)], "the ceiling of a further source is the archive's size of the lock")
        self.assertEqual([a for a in asked if len(a) == 1], [(f"file://{self.tmp}/uv.tgz",), (f"file://{self.tmp}/gitleaks.tgz",)], "each tool's last source, asked last")
        self.assertEqual(len(re.findall(r"^TOOL SOURCE uv file given up after \d+\.\ds: recorded by the test$", shown, re.M)), 2, shown)
        self.assertEqual((self.tmp / "installer.log").read_text().splitlines()[0], str(self.dest / "gate-tools"), "the command ran to its end: it started the installer")

    def test_b941_over_many_draws_every_source_comes_first_and_never_more_than_two_are_asked(self) -> None:
        """The order of a tool's further sources is drawn anew for each install, so that no one source carries every build, and at most two of them are asked
        before the last source. 300 installs of zig from the repository's lock through the command line's own path, with every download recorded and none made."""
        zig = {t["name"]: t for t in self.tl.load(LOCK)["tool"]}["zig"]
        mirrors = zig["mirrors"]
        self.assertGreater(len(mirrors), 2, "TRIGGER PRESENT: the lock names more further sources than an install asks")
        first: "collections.Counter[str]" = collections.Counter()
        for _ in range(300):
            rc, asked, shown = self.run_main(LOCK, "--only", "zig")
            self.assertEqual(rc, 5, shown)
            further = [a[0] for a in asked[:-1]]
            self.assertEqual(len(further), 2, f"never more than two further sources are asked, and no fewer while they fail: {asked}")
            self.assertEqual(len(set(further)), 2, "no source is asked twice")
            self.assertLessEqual(set(further), set(mirrors))
            self.assertEqual(asked[-1], (zig["url"],), "the last source is always last")
            self.assertEqual({limit_s for _, limit_s, *_ in asked[:-1]}, {self.tl.SOURCE_LIMIT_S})
            first[further[0]] += 1
        self.assertEqual(set(first), set(mirrors), f"every source of the lock came first at least once in 300 draws: {dict(first)}")
        self.assertEqual(self.tl.SOURCE_TRIES, 2)


# Where the endless source of the tests stops by itself: far above every archive of the tests, and small enough for a scratch folder.
ENDLESS_STOP = 64 << 20


class SourceServer(http.server.BaseHTTPRequestHandler):
    """What a source of a tool can send, by path: /ok the archive, /page a web page with status 200, /gone status 404, /cut the first half of the archive under the
    length of the whole, /trickle the archive in pieces with a pause before each, /stall the headers and then nothing, /loop a redirect to itself whose status
    line carries the server's `reason`, /endless a body without a length that goes on until the reader leaves (and stops at ENDLESS_STOP bytes, so that an
    installer without a ceiling cannot fill a disk). A trickle and a stall end when the test does.
    A server with a `delay` waits that many seconds before every answer, as a crowded machine would. Each request also records in `stood` which files of
    sources that were given up stand in the installer's download folder at that moment."""

    def log_message(self, *args: object) -> None:  # noqa: D102 (no request log on stderr)
        pass

    def do_GET(self) -> None:  # noqa: N802 (the name the base class calls)
        srv = self.server
        path = self.path.split("?")[0]
        srv.asked.append(path)
        srv.stood.append(sorted(p.name for p in srv.watch.glob(".dl-*/download-*")))
        if srv.delay and srv.release.wait(srv.delay):
            return
        try:
            if path == "/gone":
                self.send_error(404)
                return
            if path == "/loop":
                self.send_response(302, srv.reason)
                self.send_header("Location", self.path)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            if path == "/endless":
                self.send_response(200)
                self.send_header("Content-Type", "application/octet-stream")
                self.end_headers()
                block = bytes(1 << 16)
                while srv.sent < ENDLESS_STOP and not srv.release.is_set():
                    self.wfile.write(block)
                    srv.sent += len(block)
                return
            body = b"<html><body>Please sign in to continue.</body></html>" if path == "/page" else srv.archive
            self.send_response(200)
            self.send_header("Content-Type", "text/html" if path == "/page" else "application/octet-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            if path == "/cut":
                self.wfile.write(body[: len(body) // 2])
            elif path == "/stall":
                self.wfile.flush()
                srv.release.wait()
            elif path == "/trickle":
                for at in range(0, len(body), srv.piece):
                    if srv.release.wait(srv.pause):
                        return
                    self.wfile.write(body[at:at + srv.piece])
                    self.wfile.flush()
            else:
                self.wfile.write(body)
        except OSError:
            pass  # the reader left


# The limit of a source that is not the last, for a case whose subject is something else than that limit: so far away that a crowded machine cannot reach it.
# A case about the limit passes a short one of its own.
NO_HURRY_S = 600.0


class SourceServerCase(SourcesBase):
    """Against servers on 127.0.0.1: what a slow, broken or wrong source does to an install. `install` is called in this process, because the lock file
    itself takes https URLs only. A case about the limit of a source that is not the last passes a short one (the parameter `limit_s`; the product's value is
    SOURCE_LIMIT_S); every other case leaves the limit far away (NO_HURRY_S), so that only what the source sends decides."""

    def server(self, piece: int = 1, pause: float = 1.0, delay: float = 0.0) -> str:
        srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), SourceServer)
        srv.daemon_threads = True
        srv.archive, srv.piece, srv.pause, srv.delay, srv.release, srv.asked = self.data, piece, pause, delay, threading.Event(), []
        srv.watch, srv.stood, srv.reason, srv.sent = self.dest, [], "Found", 0
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        self.addCleanup(srv.server_close)
        self.addCleanup(srv.shutdown)
        self.addCleanup(srv.release.set)
        self.servers = getattr(self, "servers", []) + [srv]
        return f"http://127.0.0.1:{srv.server_address[1]}"

    def run_install(self, url: str, mirrors: "list[str]", limit_s: float = NO_HURRY_S, draw: Callable = in_order) -> "tuple[int, str, float]":
        tool = {"name": "hello", "version": "1.0", "url": url, "mirrors": mirrors, "size": len(self.data), "sha256": sha(self.data), "kind": "tar.gz", "bin": ["pkg/bin/hello"]}
        self.dest.mkdir(parents=True, exist_ok=True)
        out = io.StringIO()
        started = time.monotonic()
        with contextlib.redirect_stdout(out):
            rc = self.tl.install({"schema": "1", "tool": [tool]}, self.dest, None, limit_s=limit_s, draw=draw)
        return rc, out.getvalue(), time.monotonic() - started

    def test_b941_the_products_limit_for_a_source_that_is_not_the_last_is_60_seconds(self) -> None:
        self.assertEqual(self.tl.SOURCE_LIMIT_S, 60)

    def test_b941_a_missing_file_a_web_page_with_status_200_and_a_cut_off_body_are_given_up(self) -> None:
        bad, good = self.server(), self.server()
        host = bad.split("//")[1]
        for path, why in (("/gone", "HTTP Error 404"), ("/page", f"sha256 want={sha(self.data)} got=[0-9a-f]{{64}}$"), ("/cut", ".+")):
            with self.subTest(source=path):
                shutil.rmtree(self.dest, True)
                rc, out, _ = self.run_install(good + "/ok", [bad + path])
                self.assertEqual(rc, 0, out)
                self.assertRegex(out, re.compile(GIVEN_UP % (re.escape(host), why), re.M))
                self.assertNotIn("TOOL FAIL", out)
                self.assertEqual(self.hello_runs(), "hi")
        self.assertEqual(self.servers[0].asked, ["/gone", "/page", "/cut"], "each bad source was asked once")
        self.assertEqual(self.servers[1].asked, ["/ok"] * 3, "the next source delivered the archive each time")
        self.assertEqual(self.servers[1].stood, [[]] * 3, "no file of a source that was given up stood when the next source was asked")

    def test_b941_at_most_two_further_sources_are_asked_before_the_last_one(self) -> None:
        """However many further sources a tool names, an install asks two of them, the first two of its draw, and then the last source."""
        bad = [self.server() for _ in range(4)]
        good = self.server()
        for draw, asked in ((in_order, [["/gone"], ["/gone"], [], [], ["/ok"]]), (turned, [[], [], ["/gone"], ["/gone"], ["/ok"]])):
            with self.subTest(draw=draw.__name__):
                shutil.rmtree(self.dest, True)
                for srv in self.servers:
                    srv.asked.clear()
                    srv.stood.clear()
                rc, out, _ = self.run_install(good + "/ok", [b + "/gone" for b in bad], draw=draw)
                self.assertEqual(rc, 0, out)
                self.assertEqual([srv.asked for srv in self.servers], asked, "the first two of the draw, then the last source")
                self.assertEqual(self.servers[-1].stood, [[]], "no file of the two sources that were given up stood when the last source was asked")
                self.assertEqual(len(re.findall(r"^TOOL SOURCE hello ", out, re.M)), 2, out)
                self.assertRegex(out.splitlines()[-1], r"^TOOL OK hello 1\.0 \d+\.\ds from %s$" % re.escape(good.split("//")[1]))
        self.assertEqual(self.tl.SOURCE_TRIES, 2)

    def test_b941_an_endless_source_is_given_up_at_once_and_leaves_no_file(self) -> None:
        """A further source may send no more than the archive's size of the lock. One that goes on is given up at the first byte too many, long before its
        limit, and its file is gone before the next source is asked."""
        endless, good = self.server(), self.server()
        self.assertGreater(ENDLESS_STOP, 1000 * len(self.data), "TRIGGER PRESENT: the source sends far more than the archive's size")
        rc, out, seconds = self.run_install(good + "/ok", [endless + "/endless"])
        self.assertEqual(rc, 0, out)
        self.assertRegex(out, re.compile(GIVEN_UP % (re.escape(endless.split("//")[1]), f"more than the archive's {len(self.data)} bytes$"), re.M))
        self.assertLess(self.servers[0].sent, ENDLESS_STOP, "the installer left the source before the source ended by itself")
        self.assertEqual(self.servers[1].stood, [[]], "no file of the endless source stood when the next source was asked")
        self.assertNotIn("TOOL FAIL", out)
        self.assertEqual(self.hello_runs(), "hi")

    def test_b941_a_source_that_redirects_to_itself_cannot_write_a_line_of_its_own(self) -> None:
        """The reason of a failed download can carry the source's own words: a redirect to itself ends in an error whose text holds the reason text of the
        source's status line after two line breaks. Whatever the source sends, the installer's output holds TOOL lines only, and the reason is one line."""
        looping, good = self.server(), self.server()
        host = re.escape(looping.split("//")[1])
        for reason, why in (("BUILD NOTE a line the source chose", "a text of the source's choice"), ("note\ta\x07b " + "x" * 2000, "a long text with a tab and a bell")):
            with self.subTest(why=why):
                self.servers[0].reason = reason
                shutil.rmtree(self.dest, True)
                rc, out, _ = self.run_install(good + "/ok", [looping + "/loop"])
                self.assertEqual(rc, 0, out)
                lines = out.splitlines()
                self.assertEqual([ln for ln in lines if not ln.startswith("TOOL ")], [], "every line of the installer begins with TOOL")
                self.assertEqual(len(lines), 3, out)
                given_up = re.match(r"^TOOL SOURCE hello %s given up after \d+\.\ds: (.*)$" % host, lines[1])
                self.assertIsNotNone(given_up, lines[1])
                said = given_up.group(1)
                self.assertIn("infinite loop", said, "TRIGGER PRESENT: the reason is the redirect error that carries the source's text")
                self.assertLessEqual(len(said), 300)
                self.assertTrue(said.isprintable(), repr(said))
                self.assertIn(reason.split()[0], said, "the source's words are kept, on the line of the source")
                self.assertEqual(self.hello_runs(), "hi")

    def test_b941_one_line_makes_any_text_one_printable_line_of_at_most_300_characters(self) -> None:
        one_line = self.tl.one_line
        self.assertEqual(self.tl.REASON_MOST, 300)
        self.assertEqual(one_line("HTTP Error 404: Not Found"), "HTTP Error 404: Not Found", "a short reason of one line stays as it is")
        self.assertEqual(one_line("first\nsecond\r\nthird\x0b\x0c\x1c\x1d\x1e\x85\u2028\u2029fourth"), "first second third fourth")
        self.assertEqual(one_line("a\tb\x00c\x1b[2Kd   e\n"), "a b c [2Kd e")
        self.assertEqual(one_line(""), "")
        self.assertEqual(one_line("x" * 300), "x" * 300)
        cut = one_line("y" * 301)
        self.assertEqual((len(cut), cut), (300, "y" * 297 + "..."))
        for text in ("line\n" * 500, "\n".join("TOOL OK zig 0.16.0 0.1s from nowhere" for _ in range(40))):
            said = one_line(text)
            self.assertEqual(len(said.splitlines()), 1, repr(said))
            self.assertLessEqual(len(said), 300)

    def test_b941_a_reason_with_a_byte_above_127_does_not_fail_an_install_on_an_output_that_takes_ascii_only(self) -> None:
        """A source's own words can hold any byte, and an output may take ASCII only. The reason of a source that was given up keeps printable ASCII only, so
        printing it cannot fail the install: the installer runs as a program of its own with such an output, gives the source up and installs from the url."""
        looping, good = self.server(), self.server()
        self.servers[0].reason = "Caf\xe9 ferm\xe9"
        sent = self.tl.fetch_within(looping + "/loop", self.tmp / "probe", NO_HURRY_S, len(self.data))
        self.assertIn("Caf\xe9 ferm\xe9", sent, "TRIGGER PRESENT: the source's answer puts a character above 127 into the reason")
        self.assertFalse(sent.isascii())
        tool = {"name": "hello", "version": "1.0", "url": good + "/ok", "mirrors": [looping + "/loop"], "size": len(self.data), "sha256": sha(self.data), "kind": "tar.gz",
                "bin": ["pkg/bin/hello"]}
        self.dest.mkdir(parents=True)
        code = ("import json, sys\nfrom pathlib import Path\nsys.path.insert(0, sys.argv[1])\nimport tools_lock\n"
                "sys.exit(tools_lock.install({'schema': '1', 'tool': [json.loads(sys.argv[2])]}, Path(sys.argv[3]), None, limit_s=%r))\n" % NO_HURRY_S)
        proc = subprocess.run([sys.executable, "-B", "-c", code, str(REPO / "scripts" / "lib"), json.dumps(tool), str(self.dest)], capture_output=True, text=True, timeout=120,  # nosec B603: argument list, no shell, the interpreter with the module under test and scratch paths
                              env=dict(os.environ, PYTHONIOENCODING="ascii"))
        self.assertEqual((proc.returncode, proc.stderr), (0, ""), proc.stdout)
        lines = proc.stdout.splitlines()
        self.assertEqual(len(lines), 3, proc.stdout)
        given_up = re.match(r"^TOOL SOURCE hello %s given up after \d+\.\ds: (.*)$" % re.escape(looping.split("//")[1]), lines[1])
        self.assertIsNotNone(given_up, lines[1])
        said = given_up.group(1)
        self.assertIn("infinite loop", said, "the reason is the redirect error that carries the source's text")
        self.assertIn("Caf ferm", said, "the source's words are kept where they are printable ASCII")
        self.assertTrue(said.isascii() and said.isprintable(), repr(said))
        self.assertRegex(lines[2], r"^TOOL OK hello 1\.0 \d+\.\ds from %s$" % re.escape(good.split("//")[1]))
        self.assertEqual(self.hello_runs(), "hi", "the url's archive is installed")
        one_line = self.tl.one_line
        self.assertEqual(one_line("Caf\xe9 ferm\xe9"), "Caf ferm")
        self.assertEqual(one_line("zig \u65e5\u672c\u8a9e 0.16.0 \U0001f600\x7f~ end"), "zig 0.16.0 ~ end")
        self.assertEqual(one_line("".join(chr(n) for n in range(32, 127))), "".join(chr(n) for n in range(32, 127)).strip(), "every printable ASCII character stays")

    def test_b941_a_source_that_answers_late_is_judged_by_what_it_sends(self) -> None:
        # The stand-in for a crowded machine: every answer of the late server comes after a second. The cases that pass no limit of their own (the one above,
        # the two on the OK line and on every source bad) must not depend on a quick answer, so the limit they get stands far above that second.
        late, good = self.server(delay=1.0), self.server()
        host = re.escape(late.split("//")[1])
        rc, out, seconds = self.run_install(good + "/ok", [late + "/gone"])
        self.assertGreater(seconds, 0.9, "TRIGGER PRESENT: the further source answered late")
        self.assertEqual(rc, 0, out)
        self.assertRegex(out, re.compile(GIVEN_UP % (host, "HTTP Error 404"), re.M))
        shutil.rmtree(self.dest, True)
        rc, out, _ = self.run_install("https://example.invalid/never-asked.tgz", [late + "/ok"])
        self.assertEqual(rc, 0, out)
        self.assertRegex(out.splitlines()[-1], r"^TOOL OK hello 1\.0 \d+\.\ds from %s$" % host)
        self.assertEqual(self.hello_runs(), "hi")

    def test_b941_a_source_that_trickles_past_its_limit_is_given_up_at_the_limit(self) -> None:
        slow, good = self.server(piece=1, pause=1.0), self.server()   # one byte a second: the archive would take minutes
        self.assertGreater(len(self.data), 100, "TRIGGER PRESENT: the trickle outlasts every wait of this test")
        rc, out, seconds = self.run_install(good + "/ok", [slow + "/trickle"], limit_s=1.5)
        self.assertEqual(rc, 0, out)
        self.assertRegex(out, re.compile(GIVEN_UP % (re.escape(slow.split("//")[1]), r"not complete after 1\.5 s$"), re.M))
        self.assertLess(seconds, 20 * SCALE, f"the install took {seconds:.1f} s: the slow source was not left at its limit")
        self.assertEqual(self.servers[1].asked, ["/ok"], "the next source was used")
        self.assertEqual(self.servers[1].stood, [[]], "no file of the source that was given up stood when the next source was asked")
        self.assertEqual(self.hello_runs(), "hi")

    def test_b941_a_source_that_sends_nothing_is_given_up_at_the_limit(self) -> None:
        silent, good = self.server(), self.server()
        rc, out, seconds = self.run_install(good + "/ok", [silent + "/stall"], limit_s=0.5)
        self.assertEqual(rc, 0, out)
        self.assertRegex(out, re.compile(GIVEN_UP % (re.escape(silent.split("//")[1]), r"not complete after 0\.5 s$"), re.M))
        self.assertLess(seconds, 20 * SCALE, f"the install took {seconds:.1f} s: the silent source was not left at its limit")
        self.assertEqual(self.servers[1].stood, [[]], "no file of the source that was given up stood when the next source was asked")
        self.assertEqual(self.hello_runs(), "hi")

    def test_b941_a_download_that_its_own_timeout_ends_first_is_given_up_in_the_same_words(self) -> None:
        """Two clocks of the same length watch a further source: the installer's wait for the download, and the timeout of the download's own connection. On a
        crowded machine the connection's timeout can end the download before the installer is back from its wait. The source was not complete at its limit
        either way, and the reason says so in the same words. The stand-ins fix that order: the connection times out when the limit is over, and the
        installer's wait comes back only when the download has ended.

        Only a failure when the limit is over reads so. The two ends beside it keep what they were: a download that is complete when the limit is over is
        still taken, and a failure that comes before the limit keeps its own words. For these two the clock the installer reads is a stand-in as well, and
        only the connection's answer moves it, so no machine's speed decides them."""
        real = threading.Thread

        class BackLate(real):  # type: ignore[misc, valid-type]
            def join(self, timeout: "float | None" = None) -> None:
                super().join()

        def times_out(req: object, timeout: float) -> None:
            time.sleep(timeout)
            raise TimeoutError("timed out")

        started = time.monotonic()
        with mock.patch.object(self.tl.threading, "Thread", BackLate), mock.patch.object(self.tl.urllib.request, "urlopen", times_out):
            why = self.tl.fetch_within("https://example.invalid/hello.tgz", self.tmp / "probe", 0.2, len(self.data))
        self.assertGreaterEqual(time.monotonic() - started, 0.2, "TRIGGER PRESENT: the download ended by its own timeout, when the limit was over")
        self.assertEqual(why, "not complete after 0.2 s")

        class Clock:  # stands in for the module `time` in the installer: the one clock it reads
            now = 0.0

            def monotonic(self) -> float:
                return self.now

        class Answer:  # a connection that delivers its body in one piece and then its end
            def __init__(self, body: bytes) -> None:
                self.parts = [body, b""]

            def __enter__(self) -> "Answer":
                return self

            def __exit__(self, *exc: object) -> bool:
                return False

            def read1(self, n: int) -> bytes:
                return self.parts.pop(0)

        def ends(after: float, limit: float, outcome: "bytes | str", out: Path) -> "tuple[str, float]":
            # What the installer says of a download that ends `after` seconds after its start, with a body or with a failure's words, and how long the
            # download took on the installer's clock.
            clock = Clock()

            def answers(req: object, timeout: float) -> Answer:
                clock.now += after
                if isinstance(outcome, bytes):
                    return Answer(outcome)
                raise OSError(outcome)

            with mock.patch.object(self.tl.threading, "Thread", BackLate), mock.patch.object(self.tl.urllib.request, "urlopen", answers), mock.patch.object(self.tl, "time", clock):
                return self.tl.fetch_within("https://example.invalid/hello.tgz", out, limit, len(self.data)), clock.now

        late = self.tmp / "complete-late"
        why, took = ends(0.4, 0.2, self.data, late)
        self.assertGreaterEqual(took, 0.2, "TRIGGER PRESENT: the download came to its end when the limit was over")
        self.assertEqual(why, "", "a download that is complete when the limit is over is still taken")
        self.assertEqual(late.read_bytes(), self.data, "the file holds the whole body")
        why, took = ends(0.6, 1.0, "connection reset by the source", self.tmp / "failed-early")
        self.assertTrue(0.5 < took < 1.0, f"TRIGGER PRESENT: the download failed late in its limit and before its end, after {took} s of 1 s")
        self.assertEqual(why, "connection reset by the source", "a failure that comes before the limit keeps its own words")

    def test_b941_the_installer_ends_cleanly_while_a_source_it_gave_up_still_holds_its_connection(self) -> None:
        # A program of its own, as the command line is: it installs from a source that sends nothing and from a good one, and ends at once.
        silent, good = self.server(), self.server()
        tool = {"name": "hello", "version": "1.0", "url": good + "/ok", "mirrors": [silent + "/stall"], "size": len(self.data), "sha256": sha(self.data), "kind": "tar.gz",
                "bin": ["pkg/bin/hello"]}
        self.dest.mkdir(parents=True)
        code = ("import json, sys\nfrom pathlib import Path\nsys.path.insert(0, sys.argv[1])\nimport tools_lock\n"
                "sys.exit(tools_lock.install({'schema': '1', 'tool': [json.loads(sys.argv[2])]}, Path(sys.argv[3]), None, limit_s=0.5))\n")
        proc = subprocess.run([sys.executable, "-B", "-c", code, str(REPO / "scripts" / "lib"), json.dumps(tool), str(self.dest)], capture_output=True, text=True, timeout=120)  # nosec B603: argument list, no shell, the interpreter with the module under test and scratch paths
        self.assertEqual((proc.returncode, proc.stderr), (0, ""), proc.stdout)
        self.assertRegex(proc.stdout, re.compile(GIVEN_UP % (re.escape(silent.split("//")[1]), r"not complete after 0\.5 s$"), re.M))
        self.assertEqual(self.hello_runs(), "hi")

    def test_b941_the_last_source_has_no_limit(self) -> None:
        # five pieces, 0.3 s before each: 1.5 s for the archive, five times the limit of the sources before it
        slow = self.server(piece=len(self.data) // 5 + 1, pause=0.3)
        gone = self.server()
        rc, out, seconds = self.run_install(slow + "/trickle", [gone + "/gone"], limit_s=0.3)
        self.assertGreater(seconds, 1.2, "TRIGGER PRESENT: the last source took several times the limit")
        self.assertEqual(rc, 0, out)
        self.assertNotIn("TOOL FAIL", out)
        self.assertEqual(self.hello_runs(), "hi")
        shutil.rmtree(self.dest, True)
        rc, out, seconds = self.run_install(slow + "/trickle", [], limit_s=0.3)
        self.assertEqual((rc, self.hello_runs()), (0, "hi"), out)
        self.assertNotIn("TOOL SOURCE", out, "a tool with one source gives nothing up")

    def test_b941_the_ok_line_names_the_source_that_delivered_the_archive(self) -> None:
        bad, good = self.server(), self.server()
        rc, out, _ = self.run_install("https://example.invalid/never-asked.tgz", [bad + "/gone", good + "/ok"])
        self.assertEqual(rc, 0, out)
        lines = out.splitlines()
        self.assertEqual(len(lines), 3, out)
        self.assertEqual(lines[0], "TOOL GET hello 1.0")
        self.assertRegex(lines[1], GIVEN_UP % (re.escape(bad.split("//")[1]), "HTTP Error 404"))
        self.assertRegex(lines[2], r"^TOOL OK hello 1\.0 \d+\.\ds from %s$" % re.escape(good.split("//")[1]))

    def test_b941_every_source_bad_ends_with_the_last_sources_failure(self) -> None:
        a, b = self.server(), self.server()
        rc, out, _ = self.run_install(b + "/gone", [a + "/page", a + "/gone"])
        self.assertEqual(rc, 5, out)
        self.assertEqual(len(re.findall(r"^TOOL SOURCE hello ", out, re.M)), 2, out)
        self.assertEqual(out.splitlines()[-1], "TOOL FAIL hello download HTTP Error 404: Not Found")
        rc, out, _ = self.run_install(b + "/page", [a + "/gone"])
        self.assertEqual(rc, 4, out)
        self.assertRegex(out.splitlines()[-1], f"^TOOL FAIL hello sha256 want={sha(self.data)} got=[0-9a-f]{{64}}$")
        self.assertEqual(self.hello_runs(), "")


FAKE_UV = """#!/bin/sh
# Fake uv for the gate-tools tests: records its arguments; `python install` makes a fake interpreter
# under UV_PYTHON_INSTALL_DIR and `python find` prints its path.
echo "$*" >> "$FAKE_UV_LOG"
py="$UV_PYTHON_INSTALL_DIR/cpython-fake/bin/python3.12"
case "$1 $2" in
  "python install") mkdir -p "$(dirname "$py")"; printf '#!/bin/sh\\necho Python 3.12.99\\n' > "$py"; chmod +x "$py" ;;
  "python find") [ -z "${FAKE_UV_NO_PYTHON:-}" ] && [ -x "$py" ] && echo "$py" || exit 2 ;;
esac
"""

FAKE_INSTALLER = """#!/usr/bin/env bash
printf '%s\\n' "$@" > "$FAKE_INSTALLER_LOG"
[ -n "${FAKE_INSTALLER_FAIL:-}" ] && { echo "GATE-TOOLS FAIL fake"; exit 1; }
mkdir -p "$1/bin" && echo "GATE-TOOLS PASS $1"
"""


class GateToolsCase(unittest.TestCase):
    """`tools_lock.py gate-tools`: the harness-gates tool set (images/tools/install.sh)."""

    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="gate-tools-"))
        self.dest = self.tmp / "tools"

    def write_lock(self, body: str) -> Path:
        p = self.tmp / "tools.lock"
        p.write_text('schema = "1"\n' + body)
        return p

    def gate_lock(self, python: str = "3.12.99", table: bool = True) -> Path:
        uv = tgz("uv-fake/uv", FAKE_UV.encode())
        (self.tmp / "uv.tgz").write_bytes(uv)
        gl = b"#!/bin/sh\necho 8.0.0\n"
        (self.tmp / "gitleaks").write_bytes(gl)
        body = (f'[[tool]]\nname = "uv"\nversion = "0.0.1"\nurl = "file://{self.tmp}/uv.tgz"\nsha256 = "{sha(uv)}"\n'
                'kind = "tar.gz"\nbin = ["uv-fake/uv"]\n'
                f'[[tool]]\nname = "gitleaks"\nversion = "8.0.0"\nurl = "file://{self.tmp}/gitleaks"\nsha256 = "{sha(gl)}"\n'
                'kind = "binary"\nbin = ["gitleaks"]\n')
        if table:
            body += f'[gate_tools]\npython = "{python}"\ntools = ["uv", "gitleaks"]\n'
        return self.write_lock(body)

    def run_gate(self, lock: Path, **env: str) -> subprocess.CompletedProcess:
        installer = self.tmp / "install.sh"
        installer.write_text(FAKE_INSTALLER)
        installer.chmod(0o755)
        full = dict(os.environ, FAKE_UV_LOG=str(self.tmp / "uv.log"), FAKE_INSTALLER_LOG=str(self.tmp / "installer.log"))
        full.update(env)
        return subprocess.run([sys.executable, str(CLI), "gate-tools", "--lock", str(lock), "--dest", str(self.dest),  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool
                               "--installer", str(installer), "--allow-file-urls"],
                              capture_output=True, text=True, timeout=120, env=full)

    def test_gate_tools_installs_the_pinned_python_then_runs_the_installer(self) -> None:
        proc = self.run_gate(self.gate_lock())
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        uv_log = (self.tmp / "uv.log").read_text().splitlines()
        self.assertIn("python install --no-bin 3.12.99", uv_log)
        self.assertIn("python find --managed-python --no-project 3.12.99", uv_log)
        args = (self.tmp / "installer.log").read_text().splitlines()
        self.assertEqual(args, [str(self.dest / "gate-tools"),
                                str(self.dest / "python" / "cpython-fake" / "bin" / "python3.12"),
                                str(self.dest / "bin" / "uv"), str(self.dest / "bin" / "gitleaks")])
        self.assertIn("TOOL OK uv 0.0.1", proc.stdout)
        self.assertIn(f"GATE-TOOLS PASS {self.dest / 'gate-tools'}", proc.stdout)

    def test_gate_tools_without_the_table_is_an_invalid_lock(self) -> None:
        proc = self.run_gate(self.gate_lock(table=False))
        self.assertEqual(proc.returncode, 3)
        self.assertIn("gate_tools", proc.stderr)
        self.assertFalse((self.tmp / "installer.log").exists())

    def test_gate_tools_installer_failure_exits_5(self) -> None:
        proc = self.run_gate(self.gate_lock(), FAKE_INSTALLER_FAIL="1")
        self.assertEqual(proc.returncode, 5)
        self.assertIn("GATE-TOOLS FAIL installer exit 1", proc.stdout)

    def test_gate_tools_python_not_found_exits_5(self) -> None:
        proc = self.run_gate(self.gate_lock(), FAKE_UV_NO_PYTHON="1")
        self.assertEqual(proc.returncode, 5, proc.stdout + proc.stderr)
        self.assertIn("GATE-TOOLS FAIL python 3.12.99", proc.stdout)


if __name__ == "__main__":
    unittest.main()
