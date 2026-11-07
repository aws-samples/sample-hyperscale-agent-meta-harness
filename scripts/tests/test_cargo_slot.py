"""Tests for scripts/cargo-slot.sh: runs a command while holding one of CARGO_SLOTS (default 2)
flock slots, so at most that many cargo builds run at once on one machine. COMPAT-CAH: two cases cover the old tools variable name.
"""

from __future__ import annotations

import os
import shutil
import subprocess  # nosec B404: tests start the scripts under test and git with argument lists, never a shell
import sys
import tempfile
import time
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
SCRIPT = REPO / "scripts" / "cargo-slot.sh"
BASH = shutil.which("bash") or "/bin/bash"  # the full path of the shell that starts the script: no partial executable name in a process start

HOLD = ("import os, sys, time; f = open(sys.argv[1], 'a'); f.write(f'S {time.monotonic()}\\n'); f.flush(); "
        "time.sleep(1.0); f.write(f'E {time.monotonic()}\\n'); f.close()")


def max_overlap(log: Path) -> int:
    events = sorted((float(t), k) for k, t in (line.split() for line in log.read_text().splitlines()))
    cur = best = 0
    for _, kind in events:
        cur += 1 if kind == "S" else -1
        best = max(best, cur)
    return best


class CargoSlotCase(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="cargo-slot-"))
        self.log = self.tmp / "events.log"

    def launch(self, n: int, **env: str) -> list[subprocess.Popen]:
        # The caller's CARGO_SLOTS never leaks in: the default case tests the script's default.
        full = {k: v for k, v in os.environ.items() if k != "CARGO_SLOTS"}
        full.update(CARGO_SLOT_DIR=str(self.tmp / "locks"), **env)
        return [subprocess.Popen(["bash", str(SCRIPT), sys.executable, "-c", HOLD, str(self.log)], env=full)  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
                for _ in range(n)]

    def test_at_most_two_run_at_once_by_default(self) -> None:
        procs = self.launch(4)
        self.assertEqual([p.wait(timeout=60) for p in procs], [0, 0, 0, 0])
        self.assertEqual(max_overlap(self.log), 2)

    def test_one_slot_serializes(self) -> None:
        procs = self.launch(3, CARGO_SLOTS="1")
        self.assertEqual([p.wait(timeout=60) for p in procs], [0, 0, 0])
        self.assertEqual(max_overlap(self.log), 1)

    def test_exit_code_propagates(self) -> None:
        proc = subprocess.run(["bash", str(SCRIPT), "bash", "-c", "exit 7"], capture_output=True, text=True,  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
                              env=dict(os.environ, CARGO_SLOT_DIR=str(self.tmp / "locks")), timeout=60)
        self.assertEqual(proc.returncode, 7)

    # The default lock folder is under the build host's tools folder. A ham session cannot make it; the slots then live under
    # $TMPDIR/cargo-slots (a session has no other cargo user on its machine). A folder named with CARGO_SLOT_DIR is the caller's choice and is not replaced.

    def test_a_default_lock_folder_that_cannot_be_made_falls_back_to_tmpdir(self) -> None:
        closed = self.tmp / "tools-is-a-file"
        closed.write_text("closed", encoding="utf-8")
        tmpdir = self.tmp / "tmpdir"
        tmpdir.mkdir()
        env = {k: v for k, v in os.environ.items() if k not in ("CARGO_SLOTS", "CARGO_SLOT_DIR", "CARGO_SLOT_PRIORITY")}
        env.update(HAMH_TOOLS=str(closed), TMPDIR=str(tmpdir))
        proc = subprocess.run(["bash", str(SCRIPT), "bash", "-c", "exit 7"], capture_output=True, text=True, env=env, timeout=60)  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
        self.assertEqual(proc.returncode, 7, proc.stdout + proc.stderr)
        self.assertTrue((tmpdir / "cargo-slots" / "priority").is_dir())
        self.assertTrue((tmpdir / "cargo-slots" / "cargo-0.lock").exists())

    # COMPAT-CAH (ends at VERSION 5.50): a host whose env.sh still exports CAH_TOOLS keeps one slot folder: the old name is read when the new one is unset.

    @unittest.skipUnless("CAH_" + "TOOLS" in SCRIPT.read_text(encoding="utf-8"), "the public copy of the script carries no shim for the earlier host variable")
    def test_the_old_tools_name_chooses_the_default_lock_folder_when_the_new_name_is_unset(self) -> None:
        tools = self.tmp / "old-tools"
        env = {k: v for k, v in os.environ.items() if k not in ("CARGO_SLOTS", "CARGO_SLOT_DIR", "CARGO_SLOT_PRIORITY", "HAMH_TOOLS", "CAH_TOOLS")}
        env.update(CAH_TOOLS=str(tools))
        proc = subprocess.run([BASH, str(SCRIPT), "bash", "-c", "exit 0"], capture_output=True, text=True, env=env, timeout=60)  # nosec B603: argument list, no shell, program under test is a repository script
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertTrue((tools / "locks" / "cargo-0.lock").exists())

    def test_the_new_tools_name_wins_over_the_old_one(self) -> None:
        new, old = self.tmp / "new-tools", self.tmp / "old-tools"
        env = {k: v for k, v in os.environ.items() if k not in ("CARGO_SLOTS", "CARGO_SLOT_DIR", "CARGO_SLOT_PRIORITY")}
        env.update(HAMH_TOOLS=str(new), CAH_TOOLS=str(old))
        proc = subprocess.run([BASH, str(SCRIPT), "bash", "-c", "exit 0"], capture_output=True, text=True, env=env, timeout=60)  # nosec B603: argument list, no shell, program under test is a repository script
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertTrue((new / "locks" / "cargo-0.lock").exists())
        self.assertFalse(old.exists())

    def test_a_named_lock_folder_that_cannot_be_made_is_an_error_not_a_fallback(self) -> None:
        closed = self.tmp / "named-is-a-file"
        closed.write_text("closed", encoding="utf-8")
        tmpdir = self.tmp / "tmpdir2"
        tmpdir.mkdir()
        env = {k: v for k, v in os.environ.items() if k not in ("CARGO_SLOTS", "CARGO_SLOT_PRIORITY")}
        env.update(CARGO_SLOT_DIR=str(closed / "locks"), TMPDIR=str(tmpdir))
        proc = subprocess.run(["bash", str(SCRIPT), "bash", "-c", "exit 7"], capture_output=True, text=True, env=env, timeout=60)  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
        self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
        self.assertIn("cargo-slot.sh: cannot create", proc.stderr)
        self.assertFalse((tmpdir / "cargo-slots").exists())

    # Priority: a waiter
    # with CARGO_SLOT_PRIORITY=high registers <dir>/priority/<pid> while it waits, and a normal job
    # takes no slot while a live registration exists. The slot count stays CARGO_SLOTS.

    def slot_env(self, **env: str) -> dict:
        full = {k: v for k, v in os.environ.items() if k not in ("CARGO_SLOTS", "CARGO_SLOT_PRIORITY")}
        full.update(CARGO_SLOT_DIR=str(self.tmp / "locks"), **env)
        return full

    def test_a_normal_job_takes_no_free_slot_while_a_high_priority_waiter_is_registered(self) -> None:
        waiter = subprocess.Popen(["sleep", "30"])  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
        self.addCleanup(waiter.kill)
        prio = self.tmp / "locks" / "priority"
        prio.mkdir(parents=True)
        (prio / str(waiter.pid)).write_text(f"{waiter.pid}\n")
        done = self.tmp / "done"
        job = subprocess.Popen(["/bin/bash", str(SCRIPT), "/bin/bash", "-c", f"echo ran >> {done}"], env=self.slot_env())  # nosec B603: a fixed test command, no shell
        time.sleep(1.5)
        self.assertFalse(done.exists(), "a normal job ran while a high-priority waiter was registered")
        (prio / str(waiter.pid)).unlink()
        self.assertEqual(job.wait(timeout=30), 0)
        self.assertTrue(done.exists())

    def test_a_dead_waiters_registration_is_ignored_and_removed(self) -> None:
        dead = subprocess.Popen(["true"])  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
        dead.wait()
        prio = self.tmp / "locks" / "priority"
        prio.mkdir(parents=True)
        (prio / str(dead.pid)).write_text(f"{dead.pid}\n")
        proc = subprocess.run(["bash", str(SCRIPT), "true"], env=self.slot_env(), timeout=10)  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
        self.assertEqual(proc.returncode, 0)
        self.assertEqual(list(prio.iterdir()), [])

    def test_a_high_priority_waiter_runs_before_normal_waiters_and_clears_its_registration(self) -> None:
        order = self.tmp / "order.log"
        env = self.slot_env(CARGO_SLOTS="1")
        holder = subprocess.Popen(["bash", str(SCRIPT), "sleep", "2"], env=env)  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
        time.sleep(0.5)
        normal = [subprocess.Popen(["bash", str(SCRIPT), "bash", "-c", f"echo normal >> {order}"], env=env)  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
                  for _ in range(3)]
        time.sleep(0.5)
        high = subprocess.Popen(["bash", str(SCRIPT), "bash", "-c", f"echo high >> {order}"],  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
                                env=dict(env, CARGO_SLOT_PRIORITY="high"))
        time.sleep(0.5)
        prio = self.tmp / "locks" / "priority"
        self.assertEqual([f.name for f in prio.iterdir()], [str(high.pid)], "registered while waiting")
        self.assertEqual([p.wait(timeout=60) for p in (holder, high, *normal)], [0] * 5)
        self.assertEqual(order.read_text().split()[0], "high")
        self.assertEqual(list(prio.iterdir()), [], "registration removed")

    # D3 of the live rehearsal of the public setup: a stock Mac has no flock. The script took a failing flock for "slot busy" and polled for ever, one error line per
    # poll (283,867 lines in 9 minutes), and never started the build. Without flock the command now runs at once with one explicit line, and a flock that fails for
    # any reason other than a busy slot is an error that names it.

    def tool_path(self, *, extra: Path | None = None) -> str:
        """A PATH of the base tools the script uses and no flock, with `extra` (a directory holding a stand-in flock) in front."""
        base = self.tmp / "base-bin"
        base.mkdir(exist_ok=True)
        for tool in ("bash", "mkdir", "rm", "sleep", "dirname", "true", "cat", "echo", "sh"):
            found = shutil.which(tool)
            if found and not (base / tool).exists():
                (base / tool).symlink_to(found)
        return f"{extra}:{base}" if extra else str(base)

    def stand_in_flock(self, body: str) -> Path:
        d = self.tmp / "fake-flock-bin"
        d.mkdir(exist_ok=True)
        f = d / "flock"
        f.write_text("#!/bin/sh\n" + body)
        f.chmod(0o755)
        return d

    def run_with_path(self, path: str, *cmd: str, timeout: int = 20) -> subprocess.CompletedProcess:
        env = {k: v for k, v in os.environ.items() if k not in ("CARGO_SLOTS", "CARGO_SLOT_PRIORITY")}
        env.update(CARGO_SLOT_DIR=str(self.tmp / "locks"), PATH=path)
        return subprocess.run(["bash", str(SCRIPT), *cmd], capture_output=True, text=True, env=env, timeout=timeout)  # nosec B603: argument list, no shell, program under test; B607: bash from the PATH built for the test

    def test_without_flock_the_command_runs_once_after_one_explicit_line(self) -> None:
        out = self.tmp / "ran.txt"
        proc = self.run_with_path(self.tool_path(), "bash", "-c", f"echo ran >> {out}")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(out.read_text().split(), ["ran"])
        lines = proc.stderr.strip().splitlines()
        self.assertEqual(len(lines), 1, proc.stderr)
        self.assertIn("cargo-slot.sh: flock", lines[0])
        self.assertIn("without a build slot", lines[0])

    def test_the_exit_code_of_the_command_is_kept_without_flock(self) -> None:
        proc = self.run_with_path(self.tool_path(), "bash", "-c", "exit 7")
        self.assertEqual(proc.returncode, 7, proc.stderr)

    def test_a_flock_that_fails_with_a_message_is_an_error_that_names_it_and_not_a_busy_slot(self) -> None:
        for body, code in (("echo 'flock: unrecognized option --bogus' >&2\nexit 64\n", 64), ("exit 127\n", 127), ("exit 126\n", 126)):
            with self.subTest(exit=code):
                out = self.tmp / f"ran-{code}.txt"
                proc = self.run_with_path(self.tool_path(extra=self.stand_in_flock(body)), "bash", "-c", f"echo ran >> {out}", timeout=15)
                self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
                self.assertIn(f"cargo-slot.sh: flock failed (exit {code})", proc.stderr)
                self.assertFalse(out.exists(), "the command did not run")
                self.assertLess(len(proc.stderr.splitlines()), 5, "one message, not one line per poll")

    def test_a_flock_that_says_nothing_and_exits_1_is_a_busy_slot_that_is_waited_for(self) -> None:
        count = self.tmp / "polls"
        body = f'n=$(cat {count} 2>/dev/null || echo 0)\necho $((n + 1)) > {count}\n[ "$n" -ge 3 ] && exit 0\nexit 1\n'
        out = self.tmp / "ran-busy.txt"
        proc = self.run_with_path(self.tool_path(extra=self.stand_in_flock(body)), "bash", "-c", f"echo ran >> {out}", timeout=30)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(out.read_text().split(), ["ran"])
        self.assertGreaterEqual(int(count.read_text()), 4)
        self.assertEqual(proc.stderr, "", "a busy slot is silent")

    def test_an_unknown_priority_is_a_usage_error(self) -> None:
        proc = subprocess.run(["bash", str(SCRIPT), "true"], capture_output=True, text=True, timeout=10,  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
                              env=self.slot_env(CARGO_SLOT_PRIORITY="urgent"))
        self.assertEqual(proc.returncode, 2)
        self.assertIn("CARGO_SLOT_PRIORITY", proc.stderr)

    def test_no_command_is_usage_error(self) -> None:
        proc = subprocess.run(["bash", str(SCRIPT)], capture_output=True, text=True, timeout=60)  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
        self.assertEqual(proc.returncode, 2)
        self.assertIn("usage", proc.stderr)


if __name__ == "__main__":
    unittest.main()
