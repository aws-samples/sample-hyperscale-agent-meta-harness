"""Tests for scripts/lib/tools_lock.py, the tools.lock reader and installer behind

Installs use file:// fixtures built in a scratch directory; nothing is downloaded.
"""

from __future__ import annotations

import hashlib
import io
import os
import subprocess  # nosec B404: tests start the scripts under test and git with argument lists, never a shell
import sys
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path
from unittest import mock

REPO = Path(__file__).resolve().parents[2]
CLI = REPO / "scripts" / "lib" / "tools_lock.py"
LOCK = REPO / "tools.lock"

sys.path.insert(0, str(REPO / "scripts" / "lib"))


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
        self.assertRegex(lock["rust"]["toolchain"], r"^\d+\.\d+\.\d+$")
        for image in ("node", "binfmt", "hello_arm64"):
            self.assertRegex(lock["containers"][image], r"@sha256:[0-9a-f]{64}$", image)

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
