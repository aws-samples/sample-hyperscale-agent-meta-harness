"""The failure block of `setup.sh` shows the lines of the build log that hold the cause (scripts/setup/runner.sh, `rn_failure_excerpt`).

When a runner build fails, `setup.sh` prints the failure sentence, where the log is, and some lines of the build log. The lines used to be the last 15 of the
run's log, which are CodeBuild's closing lines (`Phase complete: POST_BUILD`, `UPLOAD_ARTIFACTS`) and never the cause: the cause is printed before the build's
own failure line, and a stranger had to open CloudWatch to learn the name of the file that was missing. The fixture `fixtures/setup/runner-failed-build.txt` is
the end of the log of the build that failed that way.
"""

from __future__ import annotations

import re
import subprocess  # nosec B404: tests start bash with an argument list, never a shell string built from input
import unittest
from pathlib import Path

from setup_full_support import FullBase
from setup_waits import limit
from test_setup_script import BASH, REPO

FIXTURE = REPO / "fixtures" / "setup" / "runner-failed-build.txt"
HEAD_BEFORE = "Lines before the build's failure message follow."
HEAD_TAIL = "The last lines of the build log follow."
HEAD_RAW = "The local log contains no build output. Its last lines follow."
CAUSE = "Error: /codebuild/output/src545072180/src/../hamh-setup-work/account/accounts.json: ENOENT"
STAMP = "2026-01-01T00:00:00Z"


def run_log(messages: list[str], extra_before: tuple[str, ...] = ()) -> str:
    """Lines as `rn_show_log` writes them to the log of a run: `<time> build: <line>`, after some lines of the program's own."""
    return "".join(f"{STAMP} {x}\n" for x in extra_before) + "".join(f"{STAMP} build: {m}\n" for m in messages)


class ExcerptCase(FullBase):
    def excerpt(self, text: str, *args: str) -> list[str]:
        log = self.tmp / f"run-{len(list(self.tmp.glob('run-*.log')))}.log"
        log.write_text(text)
        body = f'. scripts/setup/names.sh\n. scripts/setup/lib.sh\n. scripts/setup/json.sh\n. scripts/setup/runner.sh\nrn_failure_excerpt "$1" {" ".join(args)}'
        proc = subprocess.run([BASH, "-c", "set -u\n" + body, "bash", str(log)], capture_output=True, text=True, env=self.env(), cwd=str(REPO), timeout=limit(60), stdin=subprocess.DEVNULL)  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        return proc.stdout.split("\n")[:-1]

    def fixture_lines(self) -> list[str]:
        return [ln for ln in FIXTURE.read_text().splitlines() if ln.strip()]  # the lines of a log as `rn_logs` hands them over: no empty ones

    def test_trigger_present_the_fixture_ends_in_codebuilds_closing_lines_after_the_failure_lines(self) -> None:
        lines = self.fixture_lines()
        last15 = lines[-15:]
        self.assertTrue(all(ln.startswith("[Container]") for ln in last15), "TRIGGER ABSENT: the last 15 lines of the fixture are no longer CodeBuild's own")
        self.assertFalse([ln for ln in last15 if "ENOENT" in ln])

    def test_the_cause_before_the_failure_line_is_shown_and_the_closing_lines_are_not(self) -> None:
        lines = self.fixture_lines()
        marker = next(i for i, ln in enumerate(lines) if ln.startswith("DEPLOY-FAIL"))
        got = self.excerpt(run_log(lines))
        self.assertEqual(got, lines[marker - 15:marker])
        self.assertIn(CAUSE, got)
        self.assertFalse([ln for ln in got if ln.startswith("[Container]") or "DEPLOY-FAIL" in ln or "SETUP-FAIL" in ln])

    def test_a_deep_stack_does_not_push_the_error_line_out(self) -> None:
        stack = [f"    at frame{n} (file{n}.js:{n}:1)" for n in range(40)]
        got = self.excerpt(run_log(["pnpm install done", "TypeError: boom", *stack, "SETUP-FAIL deploy something failed", "[Container] closing"]))
        self.assertEqual(len(got), 15)
        self.assertEqual(got[0], "TypeError: boom")
        self.assertEqual(got[1], "    at frame0 (file0.js:0:1)")

    def test_a_cause_that_is_in_the_last_lines_keeps_the_window_at_the_failure_line(self) -> None:
        before = [f"step {n} ok" for n in range(30)] + ["error: could not compile `x` (lib)", "note: see above"]
        got = self.excerpt(run_log([*before, "DEPLOY-FAIL lambda-build cargo lambda exit 101", "SETUP-FAIL deploy lambda-build cargo lambda exit 101"]))
        self.assertEqual(got, before[-15:])

    def test_the_first_failure_line_of_either_family_ends_the_window(self) -> None:
        for first in ("SETUP-FAIL inputs a thing", "DEPLOY-FAIL guard two runs"):
            with self.subTest(first=first):
                got = self.excerpt(run_log(["a", "b error c", first, "later line", "SETUP-FAIL deploy x"]))
                self.assertEqual(got, ["a", "b error c"])

    def test_a_build_with_no_failure_line_shows_the_last_lines_of_the_build_log(self) -> None:
        msgs = [f"line {n}" for n in range(30)]
        self.assertEqual(self.excerpt(run_log(msgs)), msgs[-15:])

    def test_a_failure_line_with_nothing_before_it_falls_back_to_the_last_lines(self) -> None:
        msgs = ["SETUP-FAIL inputs SETUP_STAGE must be direct", *[f"[Container] closing {n}" for n in range(20)]]
        self.assertEqual(self.excerpt(run_log(msgs)), msgs[-15:])

    def test_lines_the_program_wrote_for_itself_are_not_build_output(self) -> None:
        own = ("build hamh-setup-runner:aaaa: IN_PROGRESS BUILD", "looking at the build failed (1 of 3): timeout", "failed: hamh-setup-runner:aaaa phase=deploy x")
        got = self.excerpt(run_log(["x", "Error: y", "SETUP-FAIL deploy z"], extra_before=own))
        self.assertEqual(got, ["x", "Error: y"])

    def test_a_message_that_holds_the_word_build_with_a_colon_keeps_its_text(self) -> None:
        got = self.excerpt(run_log(["cargo build: error[E0432]: unresolved import", "SETUP-FAIL deploy x"]))
        self.assertEqual(got, ["cargo build: error[E0432]: unresolved import"])

    def test_the_number_of_lines_is_the_argument_and_a_line_is_cut_at_240_characters(self) -> None:
        msgs = [f"m{n}" for n in range(10)] + ["x" * 1000, "SETUP-FAIL deploy z"]
        self.assertEqual(self.excerpt(run_log(msgs), "5"), ["m6", "m7", "m8", "m9", "x" * 240])

    def test_control_characters_are_dropped_and_the_tab_stays(self) -> None:
        got = self.excerpt(run_log(["\x1b[31mred\x1b[0m\ttab\x07bell", "SETUP-FAIL deploy z"]))
        self.assertEqual(got, ["[31mred[0m\ttabbell"])

    def test_a_log_with_no_build_lines_shows_its_own_last_lines(self) -> None:
        text = "".join(f"{STAMP} own line {n}\n" for n in range(20))
        self.assertEqual(self.excerpt(text), text.splitlines()[-15:])

    def test_an_empty_log_shows_nothing(self) -> None:
        self.assertEqual(self.excerpt(""), [])


class FailureBlockCase(FullBase):
    """`setup.sh --full` on the stand-in AWS CLI with the log of the build that failed: the block names the cause."""

    def fail_with(self, log_lines: list[str]):
        self.set_scenario(build={"final": "FAILED", "result": "fail", "failed_phase": "deploy", "detail": "synth cdk synth failed", "log_lines": log_lines})
        return self.run_full(*self.ARGS)

    def block(self, p: subprocess.CompletedProcess, heading: str) -> list[str]:
        """The lines under `heading` up to the line that says the runner stays installed."""
        lines = [re.sub(r"^\s+", "", ln) for ln in self.out(p).splitlines()]
        start = next(i for i, ln in enumerate(lines) if ln.startswith(heading))
        end = next(i for i, ln in enumerate(lines) if i > start and ln.startswith("The runner stays installed"))
        return lines[start + 1:end]

    def test_the_block_of_the_failed_build_names_the_missing_file_and_leaves_out_codebuilds_closing_lines(self) -> None:
        raw = [ln for ln in FIXTURE.read_text().splitlines() if ln.strip()]
        p = self.fail_with(raw)
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertIn(HEAD_BEFORE, self.out(p))
        shown = self.block(p, HEAD_BEFORE)
        self.assertIn(CAUSE, shown)
        self.assertLessEqual(len(shown), 15)
        self.assertFalse([ln for ln in shown if "POST_BUILD" in ln or "UPLOAD_ARTIFACTS" in ln or ln.startswith("[Container]")], shown)
        self.assertNotIn("The last lines of this run's log", self.out(p))

    def test_the_heading_names_the_file_that_holds_the_whole_log(self) -> None:
        p = self.fail_with([ln for ln in FIXTURE.read_text().splitlines() if ln.strip()])
        heading = next(ln.strip() for ln in self.out(p).splitlines() if HEAD_BEFORE in ln)
        path = re.search(r"\((/[^)]+\.log)\)", heading)
        self.assertIsNotNone(path, heading)
        self.assertTrue(Path(path.group(1)).is_file(), heading)
        self.assertIn(CAUSE, Path(path.group(1)).read_text())

    def test_a_build_that_printed_no_failure_line_shows_its_last_lines(self) -> None:
        msgs = [f"compiling crate {n}" for n in range(30)]
        p = self.fail_with(msgs)
        self.assertEqual(p.returncode, 5, self.out(p))
        shown = self.block(p, HEAD_TAIL)
        self.assertEqual(shown, msgs[-15:])


if __name__ == "__main__":
    unittest.main()
