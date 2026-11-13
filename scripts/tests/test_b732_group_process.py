"""The helper that runs a program in a process group of its own, for the tests that wait for `setup.sh` and send it Ctrl-C.

`GroupProcess` (scripts/tests/setup_waits.py) replaces a bare `Popen` plus `communicate(timeout=30)`. The old form left a hung `setup.sh` polling for the rest of the
run when its wait ran out, so one slow test added load to every test after it. The helper waits for the event it names, signals the whole group when the test ends
whatever the outcome, and says which stage ran out (ready, signal, exit or eof) with the output and the processes of the group.

The cases start small stand-ins with `bash -c` (or Python, where a process must leave its group). A wait that must fail uses a limit of a second or less on a free
host; scale 1 to 8 only makes it later.

Run with:
  $HAMH_PYTHON -B -m unittest discover -s scripts/tests -p test_b732_group_process.py
"""

from __future__ import annotations

import os
import signal
import sys
import time
import unittest
from pathlib import Path

import setup_waits
from setup_waits import GroupProcess
from test_setup_script import BASH

TESTS_DIR = Path(__file__).resolve().parent


def start(case: unittest.TestCase, script: str, cls=GroupProcess) -> GroupProcess:
    return cls([BASH, "-c", script], env=dict(os.environ, LC_ALL="C"), cwd=os.getcwd(), case=case, name="the stand-in")


def alive(group: int) -> list:
    return setup_waits.running(group)


class LateReader(GroupProcess):
    """A program whose reader thread starts late, as one does on a busy host: the program has ended before its output was taken from the pipe."""

    def _pump(self) -> None:
        time.sleep(0.8)
        super()._pump()


class BrokenReader(GroupProcess):
    """A reader thread that stops with an error instead of reaching end-of-file."""

    def _pump(self) -> None:
        self._reader_error = OSError("the pipe could not be read")
        self._stopped.set()


class GroupProcessCase(unittest.TestCase):
    def test_b732_a_program_that_catches_ctrl_c_ends_with_its_status_and_its_output(self) -> None:
        proc = start(self, 'trap "echo caught; exit 6" INT; echo ready; while :; do sleep 0.2; done')
        proc.wait_ready(lambda: "ready" in proc.text(), 10, "the stand-in to print ready")
        done = proc.interrupt(signal.SIGINT)
        self.assertEqual(done.returncode, 6)
        self.assertIn("caught", done.output)
        self.assertLess(done.to_exit, setup_waits.limit(10))
        self.assertGreaterEqual(done.to_eof, done.to_exit)

    def test_b732_a_wait_for_readiness_that_runs_out_fails_names_the_stage_and_ends_the_group(self) -> None:
        proc = start(self, "echo started; sleep 60")
        with self.assertRaises(AssertionError) as caught:
            proc.wait_ready(lambda: False, 0.5, "the stand-in to be ready")
        text = str(caught.exception)
        self.assertIn("stage=ready", text)
        self.assertIn("the stand-in to be ready did not happen within", text)
        self.assertIn("started", text, "the output so far is in the failure")
        self.assertIn(f"pid={proc.group}", text, "the processes of the group are in the failure")
        self.assertIn("sleep 60", text)
        self.assertEqual(alive(proc.group), [], "the group was left running")

    def test_b732_output_still_in_the_pipe_when_the_program_ends_counts_as_ready(self) -> None:
        proc = start(self, "echo ready", cls=LateReader)
        proc.wait_ready(lambda: "ready" in proc.text(), 10, "the stand-in to print ready")
        self.assertEqual(proc.returncode, 0)

    def test_b732_a_program_that_ends_before_it_is_ready_fails_with_its_status_and_output(self) -> None:
        proc = start(self, "echo hello; exit 3")
        with self.assertRaises(AssertionError) as caught:
            proc.wait_ready(lambda: False, 10, "the stand-in to be ready")
        text = str(caught.exception)
        self.assertIn("stage=ready", text)
        self.assertIn("ended with exit status 3", text)
        self.assertIn("hello", text)

    def test_b732_a_program_that_ended_before_the_signal_fails_at_the_signal_stage(self) -> None:
        proc = start(self, "echo ready; exit 6")
        proc.wait_ready(lambda: "ready" in proc.text(), 10, "the stand-in to print ready")
        setup_waits.wait_for(lambda: proc.returncode is not None, 10, "the stand-in to end")
        with self.assertRaises(AssertionError) as caught:
            proc.interrupt(signal.SIGINT)
        text = str(caught.exception)
        self.assertIn("stage=signal", text)
        self.assertIn("had already ended with exit status 6 before SIGINT was sent", text)

    def test_b732_a_program_that_ignores_ctrl_c_fails_at_the_exit_stage_and_is_ended(self) -> None:
        proc = start(self, 'trap "" INT; echo ready; while :; do sleep 0.2; done')
        proc.wait_ready(lambda: "ready" in proc.text(), 10, "the stand-in to print ready")
        began = time.monotonic()
        with self.assertRaises(AssertionError) as caught:
            proc.interrupt(signal.SIGINT, exit_seconds=1.0)
        text = str(caught.exception)
        self.assertIn("stage=exit", text)
        self.assertIn("to end after SIGINT did not happen within", text)
        self.assertIn("still running", text)
        self.assertIn(f"pid={proc.group}", text)
        self.assertGreaterEqual(time.monotonic() - began, 1.0, "the wait ended before its limit")
        self.assertEqual(alive(proc.group), [], "the hung program was left running")

    def test_b732_a_process_that_keeps_the_pipe_open_after_the_program_ends_fails_at_the_eof_stage(self) -> None:
        proc = start(self, 'trap "exit 6" INT; echo ready; (trap "" INT; sleep 60) & while :; do sleep 0.2; done')
        proc.wait_ready(lambda: "ready" in proc.text(), 10, "the stand-in to print ready")
        with self.assertRaises(AssertionError) as caught:
            proc.interrupt(signal.SIGINT, exit_seconds=5.0, eof_seconds=0.5)
        text = str(caught.exception)
        self.assertIn("stage=eof", text)
        self.assertIn("may still hold the pipe", text)
        self.assertIn("ended with exit status 6", text)
        self.assertIn("sleep 60", text)
        self.assertEqual(alive(proc.group), [], "the process that held the pipe was left running")

    def test_b732_a_reader_that_failed_is_no_end_of_output(self) -> None:
        proc = start(self, 'trap "exit 6" INT; while :; do sleep 0.2; done', cls=BrokenReader)
        proc.wait_ready(lambda: True, 10, "the stand-in to start")
        with self.assertRaises(AssertionError) as caught:
            proc.interrupt(signal.SIGINT)
        text = str(caught.exception)
        self.assertIn("stage=eof", text)
        self.assertIn("the thread that reads the output of the stand-in failed", text)
        self.assertIn("the pipe could not be read", text)

    def test_b732_the_group_is_ended_when_the_test_ends_even_if_nothing_failed(self) -> None:
        proc = start(self, "echo ready; sleep 60")
        proc.wait_ready(lambda: "ready" in proc.text(), 10, "the stand-in to print ready")
        self.assertNotEqual(alive(proc.group), [])
        self.doCleanups()
        self.assertEqual(alive(proc.group), [], "the cleanup of the test left the group running")

    def test_b732_a_process_that_left_the_group_but_stayed_in_the_session_is_ended_too(self) -> None:
        script = ("import os, subprocess, time\n"
                  "subprocess.Popen(['sleep', '60'], preexec_fn=os.setpgrp)\n"  # own process group, same session
                  "print('ready', flush=True)\n"
                  "time.sleep(60)\n")
        proc = GroupProcess([sys.executable, "-B", "-c", script], env=dict(os.environ), cwd=os.getcwd(), case=self, name="the stand-in")
        proc.wait_ready(lambda: "ready" in proc.text(), 10, "the stand-in to print ready")
        left = [r for r in setup_waits.process_table(proc.group) if r.pgrp != proc.group and r.session == proc.group]
        self.assertTrue(left, "the stand-in did not start a process of its own group")
        self.doCleanups()
        self.assertEqual(alive(proc.group), [], "the process of the other group was left running")

    def test_b732_closing_twice_is_harmless(self) -> None:
        proc = start(self, "echo ready; sleep 60")
        proc.wait_ready(lambda: "ready" in proc.text(), 10, "the stand-in to print ready")
        proc.close()
        proc.close()
        self.assertEqual(alive(proc.group), [])

    def test_b732_the_report_carries_the_load_of_the_host_where_the_host_offers_it(self) -> None:
        proc = start(self, "echo ready; sleep 60")
        proc.wait_ready(lambda: "ready" in proc.text(), 10, "the stand-in to print ready")
        report = "\n".join(proc.diagnostics())
        if os.path.exists("/proc/loadavg"):
            self.assertIn("loadavg:", report)
        self.assertIn("limits scaled by", report)

    def test_b732_a_test_process_that_was_started_with_ctrl_c_ignored_still_gives_the_program_the_default(self) -> None:
        """A runner started in the background hands its children SIGINT as ignored. Importing setup_waits handles the signal, so the programs it starts get the default."""
        inner = (f"import os, signal, sys, unittest\nsys.path.insert(0, {str(TESTS_DIR)!r})\nimport setup_waits\n"
                 "class T(unittest.TestCase):\n"
                 "    def runTest(self):\n"
                 "        p = setup_waits.GroupProcess(['bash', '-c', 'trap \"echo caught; exit 6\" INT; echo ready; while :; do sleep 0.2; done'], env=dict(os.environ), cwd=os.getcwd(), case=self, name='inner')\n"
                 "        p.wait_ready(lambda: 'ready' in p.text(), 10, 'the inner program to print ready')\n"
                 "        print('RESULT', p.interrupt(signal.SIGINT).returncode, flush=True)\n"
                 "r = unittest.TextTestRunner(stream=open(os.devnull, 'w')).run(T())\n"
                 "sys.exit(0 if r.wasSuccessful() else 1)\n")
        outer = GroupProcess([BASH, "-c", 'trap "" INT; exec "$@"', "x", sys.executable, "-B", "-c", inner], env=dict(os.environ), cwd=os.getcwd(), case=self, name="the test process")
        outer.wait_ready(lambda: "RESULT" in outer.text(), 60, "the inner test to report")
        self.assertIn("RESULT 6", outer.text())


if __name__ == "__main__":
    unittest.main()
