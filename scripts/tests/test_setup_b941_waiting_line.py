"""The waiting line of `setup.sh` names the phase of the runner that is working (scripts/setup/runner.sh, `rn_heartbeat`).

While setup follows a build, a line comes every two minutes in which nothing else was said. It used to name only CodeBuild's own phase, which is `BUILD` for
almost the whole build, so a slow step of the runner showed as minutes of `still running: CodeBuild phase BUILD`. The runner says `setup-runner: phase <name>`
in the build log when one of its phases starts and `setup-runner: phase <name>: <how it ended>` when it ends, and setup reads those lines anyway (it copies the
build log to the log of the run). So the line can name the working phase without a call of its own. Between two phases, and for a runner that says no phase,
the line is the one it was.
"""

from __future__ import annotations

import subprocess  # nosec B404: tests start bash with an argument list, never a shell string built from input
import unittest

from setup_full_support import FullBase
from setup_waits import limit
from test_setup_script import BASH, REPO

PLAIN = "  still running: CodeBuild phase BUILD, 5 minutes since you started following it"
SOURCES = ". scripts/setup/names.sh\n. scripts/setup/lib.sh\n. scripts/setup/json.sh\n. scripts/setup/runner.sh\n"
# the clock stands 300 s after the start, and nothing was said for longer than the two minutes of the waiting line
STUBS = 'say() { printf "%s\\n" "$*"; }\nsetup_epoch() { echo 1300; }\nRUN_LOG=""\nVERBOSE=0\nRN_STARTED_AT=1000\n'
BEAT = "RN_LAST_SAID=0\nrn_heartbeat BUILD\n"


class WaitingLineCase(FullBase):
    def lines(self, body: str) -> list[str]:
        proc = subprocess.run([BASH, "-c", "set -u\n" + SOURCES + STUBS + body], capture_output=True, text=True, env=self.env(), cwd=str(REPO), timeout=limit(60), stdin=subprocess.DEVNULL)  # nosec B603: argument list, no shell, program under test that is bash with the repository's own scripts
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        return proc.stdout.split("\n")[:-1]

    def test_b941_without_a_phase_line_of_the_runner_the_waiting_line_is_the_one_it_was(self) -> None:
        self.assertEqual(self.lines(BEAT), [PLAIN])
        self.assertEqual(self.lines('rn_show_log "Downloading the source archive"\nrn_show_log "[Container] Entering phase BUILD"\n' + BEAT), [PLAIN])

    def test_b941_the_waiting_line_names_the_phase_of_the_runner_that_is_working(self) -> None:
        self.assertEqual(self.lines('rn_show_log "setup-runner: phase toolchain"\n' + BEAT),
                         ["  still running: CodeBuild phase BUILD, the runner's phase toolchain, 5 minutes since you started following it"])
        self.assertEqual(self.lines('rn_show_log "setup-runner: phase toolchain"\nrn_show_log "TOOL GET zig 0.16.0"\nrn_show_log "setup-runner: phase toolchain: ok"\n'
                                    'rn_show_log "setup-runner: phase account-file"\n' + BEAT),
                         ["  still running: CodeBuild phase BUILD, the runner's phase account-file, 5 minutes since you started following it"])

    def test_b941_a_phase_that_has_ended_is_not_named(self) -> None:
        for ended in ("setup-runner: phase toolchain: ok", "setup-runner: phase deploy: skipped, finished by build hamh-setup-runner:1234"):
            with self.subTest(line=ended):
                started = ended.split(": ")[0] + ": " + ended.split(": ")[1]
                self.assertEqual(self.lines(f'rn_show_log "{started}"\nrn_show_log "{ended}"\n' + BEAT), [PLAIN])

    def test_b941_a_line_that_only_looks_like_a_phase_line_names_nothing(self) -> None:
        for line in ("setup-runner: phase", "setup-runner: phase ", "setup-runner: phase two words", "setup-runner: phases are listed below", "note: setup-runner: phase toolchain",
                     "setup-runner: phase TOOLCHAIN"):
            with self.subTest(line=line):
                self.assertEqual(self.lines(f'rn_show_log "{line}"\n' + BEAT), [PLAIN])

    def test_b941_a_new_build_starts_without_a_phase(self) -> None:
        text = (REPO / "scripts/setup/runner.sh").read_text(encoding="utf-8")
        follow = text[text.index("\nrn_follow() {"):]
        self.assertIn('RN_RUNNER_PHASE=""', follow[: follow.index("while :; do")], "rn_follow forgets the phase of the build it followed before")

    def test_b941_the_runner_says_its_phases_in_the_words_setup_reads(self) -> None:
        build = (REPO / "scripts/setup/runner-build.sh").read_text(encoding="utf-8")
        self.assertIn("say() { printf 'setup-runner: %s\\n' \"$*\"; }", build)
        for said in ('say "phase $name"', 'say "phase $name: ok"', 'say "phase $name: skipped, finished by build $SKIP_FROM"'):
            self.assertIn(said, build)


if __name__ == "__main__":
    unittest.main()
