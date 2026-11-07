"""Pinned downloads that survive a bad transfer.

One bad transfer cost a whole deploy. `images/tools/fetch.sh` is the fix: `fetch_pinned` downloads to a `.part` file, checks the
pinned SHA-256 before the file takes its name, and runs the pair again (4 attempts, pauses of 5 s, 15 s and 45 s) when either
the transfer or the check fails. `retry_run` is the same loop for the other network steps of an installer (npm ci, uv pip
install, pip download, dnf install). The pinned checksum never changes, so a file that stays wrong still fails closed with the
same message.

The tests run the real scripts. A local HTTP server serves a bad file once and then the right one; a stub `sleep` records the
pauses; a stub `curl` fails with exit status 56 ("Failure in receiving network data") before it hands over to the real one.
No test waits and none leaves the machine.
"""

from __future__ import annotations

import hashlib
import io
import os
import platform
import re
import shutil
import stat
import subprocess  # nosec B404: tests start the scripts under test and git with argument lists, never a shell
import sys
import tarfile
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
TOOLS = ROOT / "images" / "tools"
FETCH = TOOLS / "fetch.sh"
LANG_INSTALL = TOOLS / "lang" / "install.sh"
GATE_INSTALL = TOOLS / "install.sh"
SERVICES_INSTALL = ROOT / "images" / "services" / "install.sh"
CODEBUILD = ROOT / "images" / "codebuild"

GOOD = b"the release asset, as published\n" * 40
BAD = b"<html>502 Bad Gateway from a proxy in the middle</html>\n"


def sha(b: bytes) -> str:
    return hashlib.sha256(b).hexdigest()


class Server:
    """A local HTTP server. `plan[path]` lists the bodies to answer in turn; the last one repeats."""

    def __init__(self, plan: dict[str, list[bytes]]):
        self.plan, self.hits = plan, {}
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self) -> None:  # noqa: N802 (the http.server name)
                bodies = outer.plan.get(self.path)
                if not bodies:
                    self.send_error(404)
                    return
                n = outer.hits.get(self.path, 0)
                outer.hits[self.path] = n + 1
                body = bodies[min(n, len(bodies) - 1)]
                self.send_response(200)
                self.send_header("Content-Type", "text/html" if body.startswith(b"<html") else "application/octet-stream")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *a) -> None:  # silence
                pass

        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)

    def __enter__(self) -> "Server":
        self.thread.start()
        return self

    def __exit__(self, *a) -> None:
        self.httpd.shutdown()
        self.httpd.server_close()

    def url(self, path: str) -> str:
        return f"http://127.0.0.1:{self.httpd.server_address[1]}{path}"


def write_exe(path: Path, text: str) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return path


class Base(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="fetch-retry-", dir=os.environ.get("TMPDIR")))
        self.addCleanup(lambda: shutil.rmtree(self.tmp, ignore_errors=True))
        self.stubs = self.tmp / "stubs"
        self.log = self.tmp / "sleeps.log"
        # sleep never waits; it writes its argument down.
        write_exe(self.stubs / "sleep", f'#!/bin/bash\necho "$1" >> "{self.log}"\n')

    def sleeps(self) -> list[str]:
        return self.log.read_text().split() if self.log.exists() else []

    def env(self, **extra: str) -> dict:
        env = dict(os.environ)
        for k in ("HAMH_RETRY_ATTEMPTS", "HAMH_RETRY_PAUSES"):
            env.pop(k, None)
        env.update(PATH=f"{self.stubs}:{os.environ['PATH']}", TMPDIR=str(self.tmp), LC_ALL="C")
        env.update(extra)
        return env

    def bash(self, script: str, **extra: str) -> subprocess.CompletedProcess:
        return subprocess.run(["bash", "-c", script], capture_output=True, text=True, env=self.env(**extra), timeout=120, cwd=self.tmp)  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do

    def flaky(self, fails: int, status: int = 7) -> Path:
        """A command that fails `fails` times with `status`, then succeeds; it counts its runs in a file."""
        counter = self.tmp / "runs"
        return write_exe(self.tmp / "flaky.sh", f'#!/bin/bash\nn=$(( $(cat "{counter}" 2>/dev/null || echo 0) + 1 ))\necho $n > "{counter}"\n'
                                                f'echo "run $n: $*"\n[ "$n" -gt {fails} ] || exit {status}\n')

    def runs(self) -> int:
        p = self.tmp / "runs"
        return int(p.read_text()) if p.exists() else 0


class RetryRunCase(Base):
    def lib(self, body: str, **extra: str) -> subprocess.CompletedProcess:
        return self.bash(f'set -euo pipefail\n. "{FETCH}"\n{body}', **extra)

    def test_a_command_that_fails_twice_is_run_a_third_time_after_pauses_of_5_and_15_seconds(self) -> None:
        p = self.lib(f'retry_run "step" "{self.flaky(2)}" a "b c"\necho DONE')
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual((self.runs(), self.sleeps()), (3, ["5", "15"]))
        self.assertIn("DONE", p.stdout)
        self.assertIn("retry: step attempt 1 of 4 failed (exit status 7); retrying in 5s", p.stderr)
        self.assertIn("retry: step attempt 2 of 4 failed (exit status 7); retrying in 15s", p.stderr)

    def test_a_command_that_never_works_is_run_four_times_with_pauses_of_5_15_and_45_and_returns_its_last_status(self) -> None:
        p = self.lib(f'rc=0; retry_run "step" "{self.flaky(99, status=9)}" || rc=$?\necho "rc=$rc"')
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertIn("rc=9", p.stdout)
        self.assertEqual((self.runs(), self.sleeps()), (4, ["5", "15", "45"]))
        self.assertIn("retry: step failed after 4 attempts (last exit status 9)", p.stderr)

    def test_the_last_pause_repeats_when_more_attempts_are_asked_for(self) -> None:
        p = self.lib(f'retry_run "step" "{self.flaky(99)}" || true', HAMH_RETRY_ATTEMPTS="6")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual((self.runs(), self.sleeps()), (6, ["5", "15", "45", "45", "45"]))

    def test_attempts_and_pauses_come_from_the_environment(self) -> None:
        p = self.lib(f'retry_run "step" "{self.flaky(99)}" || true', HAMH_RETRY_ATTEMPTS="3", HAMH_RETRY_PAUSES="1 2.5")
        self.assertEqual((p.returncode, self.runs(), self.sleeps()), (0, 3, ["1", "2.5"]), p.stderr)

    def test_one_attempt_runs_the_command_once_and_never_sleeps(self) -> None:
        p = self.lib(f'retry_run "step" "{self.flaky(99)}" || true', HAMH_RETRY_ATTEMPTS="1")
        self.assertEqual((self.runs(), self.sleeps()), (1, []), p.stderr)

    def test_a_command_that_works_at_once_runs_once_and_says_nothing(self) -> None:
        p = self.lib(f'retry_run "step" "{self.flaky(0)}" x')
        self.assertEqual((p.returncode, self.runs(), self.sleeps(), p.stderr), (0, 1, [], ""))
        self.assertEqual(p.stdout.strip(), "run 1: x", "the command's own output passes through")

    def test_set_e_still_stops_the_caller_after_the_last_failure(self) -> None:
        p = self.lib(f'retry_run "step" "{self.flaky(99, status=3)}"\necho AFTER')
        self.assertNotIn("AFTER", p.stdout)
        self.assertEqual(p.returncode, 3, p.stderr)
        self.assertEqual(self.runs(), 4)

    def test_a_bad_setting_is_a_usage_error_before_the_command_runs(self) -> None:
        for env, why in (({"HAMH_RETRY_ATTEMPTS": "0"}, "attempts"), ({"HAMH_RETRY_ATTEMPTS": "x"}, "attempts"),
                         ({"HAMH_RETRY_ATTEMPTS": "2", "HAMH_RETRY_PAUSES": "5 soon"}, "pauses"),
                         ({"HAMH_RETRY_ATTEMPTS": "2", "HAMH_RETRY_PAUSES": ""}, "pauses")):
            p = self.lib(f'rc=0; retry_run "step" "{self.flaky(0)}" || rc=$?\necho "rc=$rc"', **env)
            self.assertIn("rc=2", p.stdout, (env, p.stderr))
            self.assertIn(f"retry: HAMH_RETRY_{why.upper()}", p.stderr)
            self.assertEqual(self.runs(), 0)

    def test_a_call_without_a_command_is_a_usage_error(self) -> None:
        for call in ('retry_run', 'retry_run "only a label"'):
            p = self.lib(f'rc=0; {call} || rc=$?\necho "rc=$rc"')
            self.assertIn("rc=2", p.stdout, call)
            self.assertIn("usage: retry_run <label> <command> [<arg>...]", p.stderr)

    def test_the_callers_field_separator_does_not_change_the_pauses(self) -> None:
        p = self.lib(f'IFS=,; retry_run "step" "{self.flaky(99)}" || true')
        self.assertEqual((p.returncode, self.runs(), self.sleeps()), (0, 4, ["5", "15", "45"]), p.stderr)

    def test_sourcing_the_file_runs_nothing_and_leaves_the_callers_options_alone(self) -> None:
        p = self.bash(f'. "{FETCH}"; echo "opts=$-"; type retry_run fetch_pinned >/dev/null && echo FUNCS')
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertIn("FUNCS", p.stdout)
        self.assertNotRegex(p.stdout, r"opts=\w*[eu]")


class FetchPinnedCase(Base):
    def setUp(self) -> None:
        super().setUp()
        self.dest = self.tmp / "dl" / "asset.tgz"
        self.dest.parent.mkdir()

    def fetch(self, url: str, digest: str, **extra: str) -> subprocess.CompletedProcess:
        return self.bash(f'set -euo pipefail\n. "{FETCH}"\nfetch_pinned "{url}" "{digest}" "{self.dest}"\necho FETCHED', **extra)

    def stub_curl(self, fails: int, status: int = 56) -> Path:
        """A curl that fails `fails` times with `status` and then runs the real one; it logs its arguments."""
        real = shutil.which("curl")
        counter, argv = self.tmp / "curl.n", self.tmp / "curl.argv"
        return write_exe(self.stubs / "curl", f'#!/bin/bash\necho "$*" >> "{argv}"\nn=$(( $(cat "{counter}" 2>/dev/null || echo 0) + 1 ))\n'
                                              f'echo $n > "{counter}"\n[ "$n" -gt {fails} ] || exit {status}\nexec "{real}" "$@"\n')

    def test_a_bad_download_is_fetched_again_and_the_good_one_takes_its_name(self) -> None:
        # The deploy B2 case: the first answer is not the asset, the second is.
        with Server({"/gosec.tgz": [BAD, GOOD]}) as srv:
            p = self.fetch(srv.url("/gosec.tgz"), sha(GOOD))
            hits = srv.hits["/gosec.tgz"]
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertIn("FETCHED", p.stdout)
        self.assertEqual(hits, 2)
        self.assertEqual(self.dest.read_bytes(), GOOD)
        self.assertEqual(self.sleeps(), ["5"])
        self.assertIn(f"fetch: SHA-256 of http://127.0.0.1:", p.stderr)
        self.assertIn(f"is {sha(BAD)}, wanted {sha(GOOD)}", p.stderr)
        self.assertIn("retrying in 5s", p.stderr)

    def test_a_download_that_stays_wrong_fails_with_the_checksum_status_after_four_attempts_and_names_nothing(self) -> None:
        with Server({"/x.tgz": [BAD]}) as srv:
            p = self.bash(f'. "{FETCH}"; rc=0; fetch_pinned "{srv.url("/x.tgz")}" "{sha(GOOD)}" "{self.dest}" || rc=$?; echo "rc=$rc"')
            hits = srv.hits["/x.tgz"]
        self.assertIn("rc=65", p.stdout)
        self.assertEqual((hits, self.sleeps()), (4, ["5", "15", "45"]))
        self.assertFalse(self.dest.exists(), "bytes that fail the pin never take the final name")
        self.assertEqual(list(self.dest.parent.iterdir()), [], "no staging file is left behind")
        self.assertIn("failed after 4 attempts", p.stderr)
        # The mismatch says what came back, so a proxy's error page is recognisable.
        self.assertIn(f"(HTTP 200, {len(BAD)} bytes, text/html)", p.stderr)

    def test_a_good_download_is_fetched_once_with_no_pause_and_no_noise(self) -> None:
        with Server({"/x.tgz": [GOOD]}) as srv:
            p = self.fetch(srv.url("/x.tgz"), sha(GOOD))
            hits = srv.hits["/x.tgz"]
        self.assertEqual((p.returncode, hits, self.sleeps(), p.stderr), (0, 1, [], ""))
        self.assertEqual(self.dest.read_bytes(), GOOD)
        self.assertEqual(sorted(x.name for x in self.dest.parent.iterdir()), ["asset.tgz"], "the staging file became the file")
        self.assertEqual(self.dest.stat().st_mode & 0o777, 0o644)

    def test_a_dropped_transfer_is_tried_again_through_the_same_pair(self) -> None:
        # Deploy afdbc0f: curl exit 56, "Failure in receiving network data".
        self.stub_curl(fails=2)
        with Server({"/x.tgz": [GOOD]}) as srv:
            p = self.fetch(srv.url("/x.tgz"), sha(GOOD))
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(self.sleeps(), ["5", "15"])
        self.assertIn("fetch: transfer of http://127.0.0.1:", p.stderr)
        self.assertIn("(curl exit status 56)", p.stderr)

    def test_a_transfer_that_never_works_fails_with_curls_own_status(self) -> None:
        self.stub_curl(fails=99, status=56)
        p = self.bash(f'. "{FETCH}"; rc=0; fetch_pinned "http://127.0.0.1:9/x" "{sha(GOOD)}" "{self.dest}" || rc=$?; echo "rc=$rc"')
        self.assertIn("rc=56", p.stdout)
        self.assertEqual(len((self.tmp / "curl.argv").read_text().splitlines()), 4)

    def test_curl_keeps_its_own_retry_flags_and_writes_a_staging_file_beside_the_destination(self) -> None:
        self.stub_curl(fails=0)
        with Server({"/x.tgz": [GOOD]}) as srv:
            self.fetch(srv.url("/x.tgz"), sha(GOOD))
        argv = (self.tmp / "curl.argv").read_text()
        self.assertIn("-fsSL --retry 5 --retry-all-errors", argv)
        self.assertRegex(argv, rf"-o {re.escape(str(self.dest))}\.part\.\w{{6}}\b")

    def test_each_attempt_stages_in_a_file_of_its_own(self) -> None:
        # Two installers that share a download cache never write one staging file.
        self.stub_curl(fails=0)
        with Server({"/x.tgz": [BAD, GOOD]}) as srv:
            p = self.fetch(srv.url("/x.tgz"), sha(GOOD))
        self.assertEqual(p.returncode, 0, p.stderr)
        staged = re.findall(r"-o (\S+)", (self.tmp / "curl.argv").read_text())
        self.assertEqual(len(staged), 2)
        self.assertEqual(len(set(staged)), 2, staged)
        self.assertEqual(sorted(x.name for x in self.dest.parent.iterdir()), ["asset.tgz"])

    def test_a_good_file_already_in_place_is_not_replaced_by_a_bad_download(self) -> None:
        self.dest.write_bytes(GOOD)
        with Server({"/x.tgz": [BAD]}) as srv:
            p = self.bash(f'. "{FETCH}"; fetch_pinned "{srv.url("/x.tgz")}" "{sha(GOOD)}" "{self.dest}" || true')
        self.assertEqual(self.dest.read_bytes(), GOOD, p.stderr)

    def test_an_uppercase_pin_is_the_same_pin(self) -> None:
        with Server({"/x.tgz": [GOOD]}) as srv:
            p = self.fetch(srv.url("/x.tgz"), sha(GOOD).upper())
        self.assertEqual(p.returncode, 0, p.stderr)

    def test_a_malformed_call_is_a_usage_error_and_downloads_nothing(self) -> None:
        self.stub_curl(fails=0)
        for args, why in ((["http://x/y"], "usage: fetch.sh <url> <sha256> <dest>"),
                          (["http://x/y", sha(GOOD), str(self.dest), "extra"], "usage: fetch.sh <url> <sha256> <dest>"),
                          (["http://x/y", "abc", str(self.dest)], "fetch: not a SHA-256: 'abc'")):
            p = subprocess.run(["bash", str(FETCH), *args], capture_output=True, text=True, env=self.env(), timeout=30)  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
            self.assertEqual(p.returncode, 2, (args, p.stderr))
            self.assertIn(why, p.stderr)
        self.assertFalse((self.tmp / "curl.argv").exists(), "curl was never run")

    def test_run_as_a_command_it_fetches_and_exits_with_the_status(self) -> None:
        with Server({"/ok.tgz": [BAD, GOOD], "/bad.tgz": [BAD]}) as srv:
            ok = subprocess.run(["bash", str(FETCH), srv.url("/ok.tgz"), sha(GOOD), str(self.dest)], capture_output=True, text=True,  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
                                env=self.env(), timeout=60)
            bad = subprocess.run(["bash", str(FETCH), srv.url("/bad.tgz"), sha(GOOD), str(self.tmp / "other")], capture_output=True,  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
                                 text=True, env=self.env(), timeout=60)
        self.assertEqual((ok.returncode, self.dest.read_bytes()), (0, GOOD), ok.stderr)
        self.assertEqual(bad.returncode, 65, bad.stderr)


# ---- the installers use it ---------------------------------------------------------------------------------------

def lang_tree(tmp: Path, srv: Server, node_tar: bytes, pin: bytes | None = None) -> Path:
    """A copy of tools/ with a lang.lock whose node row points at the local server and a stub Node in the archive."""
    tools = tmp / "tools"
    (tools / "lang" / "js").mkdir(parents=True)
    shutil.copy(FETCH, tools / "fetch.sh")
    shutil.copy(LANG_INSTALL, tools / "lang" / "install.sh")
    (tools / "lang" / "js" / "package.json").write_text("{}\n")
    (tools / "lang" / "js" / "package-lock.json").write_text("{}\n")
    arch = platform.machine()
    (tools / "lang" / "lang.lock").write_text(f"node 0.0.0 {arch} {sha(pin or node_tar)} {srv.url('/node.tar')}\n")
    return tools


def stub_node_archive() -> bytes:
    """A tar with one top directory holding bin/{node,npm,npx} stubs: node prints a version for any arguments."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w") as t:
        for name, body in (("bin/node", b'#!/bin/bash\necho 0.0.0\n'), ("bin/npm", b"#!/bin/bash\nexit 0\n"), ("bin/npx", b"#!/bin/bash\nexit 0\n"),
                           ("include/x.h", b"x\n"), ("share/y", b"y\n"), ("README.md", b"r\n"), ("CHANGELOG.md", b"c\n")):
            info = tarfile.TarInfo(f"node-v0/{name}")
            info.size, info.mode = len(body), 0o755
            t.addfile(info, io.BytesIO(body))
    return buf.getvalue()


class LangInstallerCase(Base):
    def run_install(self, tools: Path, dest: Path, **extra: str) -> subprocess.CompletedProcess:
        return subprocess.run(["bash", str(tools / "lang" / "install.sh"), str(dest), "python", str(self.tmp / "cache")],  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
                              capture_output=True, text=True, env=self.env(**extra), timeout=120)

    def test_a_bad_first_download_of_a_locked_tool_is_fetched_again_and_the_install_passes(self) -> None:
        node = stub_node_archive()
        with Server({"/node.tar": [BAD, node]}) as srv:
            tools = lang_tree(self.tmp, srv, node)
            p = self.run_install(tools, self.tmp / "out")
            hits = srv.hits["/node.tar"]
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        self.assertIn("LANG-TOOLS PASS family=python", p.stdout)
        self.assertEqual((hits, self.sleeps()), (2, ["5"]))
        self.assertIn("retrying in 5s", p.stderr)

    def test_a_persistent_mismatch_still_fails_with_the_installers_own_message(self) -> None:
        node = stub_node_archive()
        with Server({"/node.tar": [BAD]}) as srv:
            tools = lang_tree(self.tmp, srv, node)
            p = self.run_install(tools, self.tmp / "out")
            hits, url = srv.hits["/node.tar"], srv.url("/node.tar")
        self.assertNotEqual(p.returncode, 0)
        self.assertIn(f"install.sh: SHA-256 mismatch for {url}", p.stderr)
        self.assertNotIn("LANG-TOOLS PASS", p.stdout)
        self.assertEqual(hits, 4)

    def test_a_dropped_transfer_is_fetched_again_too(self) -> None:
        node = stub_node_archive()
        real = shutil.which("curl")
        write_exe(self.stubs / "curl", f'#!/bin/bash\nf="{self.tmp}/curl.n"\nn=$(( $(cat "$f" 2>/dev/null || echo 0) + 1 ))\necho $n > "$f"\n'
                                       f'[ "$n" -gt 1 ] || exit 56\nexec "{real}" "$@"\n')
        with Server({"/node.tar": [node]}) as srv:
            tools = lang_tree(self.tmp, srv, node)
            p = self.run_install(tools, self.tmp / "out")
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        self.assertEqual(self.sleeps(), ["5"])

    def test_a_cached_file_that_matches_is_not_fetched_at_all(self) -> None:
        node = stub_node_archive()
        (self.tmp / "cache").mkdir()
        (self.tmp / "cache" / "node.tar").write_bytes(node)
        with Server({"/node.tar": [BAD]}) as srv:
            tools = lang_tree(self.tmp, srv, node)
            p = self.run_install(tools, self.tmp / "out")
            hits = srv.hits.get("/node.tar", 0)
        self.assertEqual((p.returncode, hits), (0, 0), p.stdout + p.stderr)

    def test_npm_ci_is_run_again_when_it_fails(self) -> None:
        # An npm that fails once, counted in a file: the stub goes into the archive the lock points at.
        buf = io.BytesIO()
        counter = self.tmp / "npm.n"
        npm = (f'#!/bin/bash\nn=$(( $(cat "{counter}" 2>/dev/null || echo 0) + 1 ))\necho $n > "{counter}"\n'
               '[ "$n" -gt 1 ] || { echo "npm ERR! EINTEGRITY" >&2; exit 1; }\nexit 0\n').encode()
        with tarfile.open(fileobj=buf, mode="w") as t:
            for name, body in (("bin/node", b"#!/bin/bash\necho 0.0.0\n"), ("bin/npm", npm), ("bin/npx", b"#!/bin/bash\nexit 0\n")):
                info = tarfile.TarInfo(f"node-v0/{name}")
                info.size, info.mode = len(body), 0o755
                t.addfile(info, io.BytesIO(body))
        node = buf.getvalue()
        with Server({"/node.tar": [node]}) as srv:
            tools = lang_tree(self.tmp, srv, node)
            p = self.run_install(tools, self.tmp / "out")
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        self.assertEqual(counter.read_text().strip(), "2")
        self.assertIn("retry: npm ci attempt 1 of 4 failed", p.stderr)


class GateInstallerCase(Base):
    """images/tools/install.sh with a fake uv and a python whose pip downloads fail once."""

    def tree(self) -> tuple[Path, Path, Path, Path]:
        tools = self.tmp / "tools"
        tools.mkdir()
        shutil.copy(FETCH, tools / "fetch.sh")
        shutil.copy(GATE_INSTALL, tools / "install.sh")
        (tools / "tools.txt").write_text("ruff==0.0.0 --hash=sha256:00\n")
        (tools / "runtime.txt").write_text("pytest==0.0.0 --hash=sha256:00\n")
        py = Path(sys.executable)
        state = self.tmp / "state"
        state.mkdir()
        stub = lambda v: f'#!/bin/bash\necho "{v}"\n'  # noqa: E731
        venv_python = (f'#!/bin/bash\nif [ "$1" = -m ] && [ "$2" = pip ]; then\n  echo "$*" >> "{state}/pip.argv"\n'
                       f'  n=$(( $(cat "{state}/pip.n" 2>/dev/null || echo 0) + 1 )); echo $n > "{state}/pip.n"\n'
                       '  [ "$n" -gt 1 ] || { echo "ERROR: THESE PACKAGES DO NOT MATCH THE HASHES FROM THE REQUIREMENTS FILE" >&2; exit 1; }\n'
                       '  while [ $# -gt 0 ]; do [ "$1" = -d ] && d=$2; shift; done\n  mkdir -p "$d" && : > "$d/pytest-0.0.0-py3-none-any.whl"\n  exit 0\nfi\n'
                       f'exec "{py}" "$@"\n')
        uv = (f'#!/bin/bash\necho "$*" >> "{state}/uv.argv"\ncase "$1" in\n  --version) echo "uv 0.0.0";;\n  venv)\n'
              '    for a in "$@"; do v=$a; done\n    mkdir -p "$v/bin"\n'
              f'    cp "{self.tmp}/venv-python" "$v/bin/python"\n    for t in ruff mypy bandit pip-audit diff-cover; do cp "{self.tmp}/tool-stub" "$v/bin/$t"; done;;\n'
              f'  pip)\n    n=$(( $(cat "{state}/uv.n" 2>/dev/null || echo 0) + 1 )); echo $n > "{state}/uv.n"\n'
              '    [ "$n" -gt 1 ] || { echo "error: hash mismatch" >&2; exit 2; };;\nesac\n')
        write_exe(self.tmp / "venv-python", venv_python)
        write_exe(self.tmp / "tool-stub", stub("tool 0.0.0"))
        return tools, write_exe(self.tmp / "uv", uv), write_exe(self.tmp / "gitleaks", '#!/bin/bash\necho "8.0.0"\n'), state

    def test_a_failed_hash_checked_install_and_a_failed_wheel_download_are_each_run_again(self) -> None:
        tools, uv, gitleaks, state = self.tree()
        p = subprocess.run(["bash", str(tools / "install.sh"), str(self.tmp / "out"), str(Path(sys.executable)), str(uv), str(gitleaks)],  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
                           capture_output=True, text=True, env=self.env(), timeout=120)
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        self.assertIn(f"GATE-TOOLS PASS {self.tmp / 'out'}", p.stdout)
        self.assertEqual((state / "uv.n").read_text().strip(), "2", "uv pip install ran twice")
        self.assertEqual((state / "pip.n").read_text().strip(), "2", "pip download ran twice")
        self.assertEqual(self.sleeps(), ["5", "5"])
        self.assertIn("retry: uv pip install attempt 1 of 4 failed", p.stderr)
        self.assertIn("retry: pip download attempt 1 of 4 failed", p.stderr)
        # Both still run with the pinned hashes required.
        self.assertIn("--require-hashes", (state / "uv.argv").read_text())
        self.assertIn("--require-hashes", (state / "pip.argv").read_text())

    def test_an_install_that_never_works_fails_the_script(self) -> None:
        tools, uv, gitleaks, state = self.tree()
        write_exe(self.tmp / "uv", (self.tmp / "uv").read_text().replace('[ "$n" -gt 1 ]', '[ "$n" -gt 99 ]'))
        p = subprocess.run(["bash", str(tools / "install.sh"), str(self.tmp / "out"), str(Path(sys.executable)), str(uv), str(gitleaks)],  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
                           capture_output=True, text=True, env=self.env(), timeout=120)
        self.assertNotEqual(p.returncode, 0)
        self.assertNotIn("GATE-TOOLS PASS", p.stdout)
        self.assertEqual((state / "uv.n").read_text().strip(), "4")
        self.assertFalse((state / "pip.n").exists(), "the script stopped at the failed step")


# ---- every network step of the image-build installers goes through the helper -----------------------------------------

def commands(path: Path) -> list[tuple[int, str]]:
    """The non-comment commands of a script with the number of their first line; a backslash continuation joins its lines."""
    out: list[tuple[int, str]] = []
    pending: tuple[int, str] | None = None
    for n, l in enumerate(path.read_text().splitlines(), 1):
        if pending is None and (not l.strip() or l.lstrip().startswith("#")):
            continue
        start, text = pending if pending else (n, "")
        text = f"{text} {l.strip()}".strip()
        if text.endswith("\\"):
            pending = (start, text[:-1])
        else:
            pending = None
            out.append((start, text))
    return out


def block(path: Path) -> str:
    text = path.read_text()
    m = re.search(r"^# >>> retry-lib.*?\n(.*?)^# <<< retry-lib", text, re.M | re.S)
    assert m, f"{path} has no retry-lib block"
    return m.group(1)


class CoverageCase(unittest.TestCase):
    def test_the_block_markers_appear_exactly_once_in_each_file(self) -> None:
        for path in (FETCH, SERVICES_INSTALL):
            text = path.read_text()
            self.assertEqual(len(re.findall(r"^# >>> retry-lib", text, re.M)), 1, path)
            self.assertEqual(len(re.findall(r"^# <<< retry-lib", text, re.M)), 1, path)

    def test_the_services_installer_carries_the_same_helper_as_fetch_sh(self) -> None:
        # images/services is bind-mounted alone (and as the named context `services` of the language test container), so it cannot
        # source ../crates/...; it holds a copy of the block, and this test keeps the two the same.
        self.assertEqual(block(SERVICES_INSTALL), block(FETCH))

    def test_no_installer_runs_a_network_command_outside_the_helper(self) -> None:
        net = re.compile(r"\bcurl\b|\bwget\b|pip download|\bpip\b.*\binstall\b|\bnpm\b.*\bci\b|\bdnf\b.*\binstall\b")
        for path in (LANG_INSTALL, GATE_INSTALL, SERVICES_INSTALL):
            text = path.read_text()
            inside = block(path) if "retry-lib" in text else ""
            for n, line in commands(path):
                if not net.search(line) or line in inside or line.lstrip().startswith("echo "):
                    continue
                self.assertRegex(line, r"\bretry_run\b|\bfetch_pinned\b", f"{path.relative_to(ROOT)}:{n}: {line.strip()}")

    def test_every_buildspec_download_is_one_fetch_sh_call_with_its_url_and_its_archive_pin(self) -> None:
        calls = 0
        for path in sorted(CODEBUILD.glob("*.yml")):
            for n, line in commands(path):
                where = f"{path.relative_to(ROOT)}:{n}"
                self.assertNotRegex(line, r"\bcurl\b.*\s-o\s", where)
                m = re.search(r"bash images/tools/fetch\.sh (\S+) (\S+) (\S+)\s*$", line)
                if not m:
                    continue
                calls += 1
                name = re.match(r'"\$(\w+)_URL"$', m.group(1))
                self.assertIsNotNone(name, where)
                self.assertEqual(m.group(2), f'"${name.group(1)}_ARCHIVE_SHA256"', where)
                self.assertRegex(m.group(3), r"^build/[a-z]+\.tgz$", where)
        # gitleaks is no release download any more: images/tools/build-gitleaks.sh builds it and fetches its Go toolchain with fetch_pinned.
        self.assertEqual(calls, 3, "the 3 release downloads of the image buildspecs (worker-python: 3, delivery: 0)")

    def test_the_helper_is_where_every_consumer_can_see_it(self) -> None:
        # The Dockerfiles bind-mount images/tools whole; the language tools container mounts it whole too.
        for p in (ROOT / "images").glob("*/Dockerfile"):
            text = p.read_text()
            if "gate-src/lang/install.sh" in text or "gate-src/install.sh" in text:
                self.assertIn("source=images/tools,target=/gate-src", text, str(p))
        script = (ROOT / "scripts" / "gates-lang-tools.sh").read_text()
        self.assertIn('-v "$tools:$tools:ro"', script)
        self.assertNotIn('-v "$lang:$lang:ro"', script)


if __name__ == "__main__":
    unittest.main()
