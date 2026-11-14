"""The parts of `./setup.sh --update` that `ham update` relies on, pinned so that a change to setup that breaks them fails here first.

`ham update` fetches a commit into the client's download directory and runs that commit's setup as
`setup.sh --update --prefix <prefix> --yes` from the clone. It relies on four things setup does already: the three options exist and an update
needs only `--yes`; an update with a running run stops with exit 7 before anything is installed or built; the client is installed through a new
file in the install folder and one rename, after the new client was tried there, and the client it replaces is kept beside it as `ham.prev`
(ham renames that file back when the new client reports another version); and exit 6 means an operation is still running and safe to resume.
Since it also asks setup for its plan (`--dry-run`) when a run of the caller is active, and reads from it whether the update deploys nothing on the
stage: the option exists, the dry run prints the plan and ends before the question, and the plan follows setup's own decision. The words of the plan lines are
held by `crates/ham-core/tests/b843_core.rs`. Setup did all of that, so the test of the plan is a regression test: it passed when it was written,
against the setup of the release before, and holds what the client relies on from then on.
These are static checks of the script text: the behaviour itself is tested by the setup suites (test_setup_*.py).
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]


def read(rel: str) -> str:
    return (REPO / rel).read_text(encoding="utf-8")


class SetupUpdateInterface(unittest.TestCase):
    def test_b804_the_options_ham_update_passes_exist(self) -> None:
        setup = read("setup.sh")
        self.assertRegex(setup, r"\n\s*--update\) set_cmd update ;;")
        self.assertRegex(setup, r"\n\s*--prefix\) PREFIX=\$2 ;;")
        self.assertRegex(setup, r"\n\s*-y\|--yes\) YES=1 ;;")
        full = read("scripts/setup/full.sh")
        self.assertIn("# an update needs only --yes; the full setup needs the acknowledgement of the costs too", full)

    def test_b804_active_runs_stop_an_update_with_exit_7_before_anything_is_built(self) -> None:
        pre = read("scripts/setup/preflight.sh")
        body = pre[pre.index("pf_active_runs_check() {") :]
        body = body[: body.index("\n}\n")]
        self.assertIn('setup_exit 7 "the update is stopped:', body)
        full = read("scripts/setup/full.sh")
        self.assertRegex(full, r"elif \[ \"\$\{ALLOW_ACTIVE_RUNS:-0\}\" = 1 \]; then\n\s*pf_active_runs\n")
        self.assertIn("pf_active_runs_check", full)

    def test_b804_the_client_is_tried_then_renamed_into_place_and_the_earlier_one_kept(self) -> None:
        setup = read("setup.sh")
        tried = setup.index('[ "${FULL_RAN:-0}" != 1 ] || client_tried "$TMPBIN"')
        kept = setup.index('cp -p "$DEST" "$DEST.prev"')
        renamed = setup.index('if mv -f "$TMPBIN" "$DEST"; then')
        self.assertLess(tried, kept)
        self.assertLess(kept, renamed)
        self.assertIn("TMPBIN=$BINDIR/.$HAMH_CLIENT_BIN.setup-new", setup)

    def test_b841_a_dry_run_prints_the_plan_ham_update_reads_and_ends_before_the_question(self) -> None:
        setup = read("setup.sh")
        self.assertRegex(setup, r"\n\s*--dry-run\) DRY=1 ;;")
        self.assertRegex(setup, r"\n\s*--wait-for-runs\) WAIT_FOR_RUNS=\$2; WAIT_FOR_RUNS_SET=1 ;;")
        full = read("scripts/setup/full.sh")
        decided = full.index("        rn_same_deployment_check\n")
        plan = full.index('  full_plan "$action"\n  if [ "$DRY" = 1 ]; then\n')
        ends = full.index("    exit 0\n", plan)
        asks = full.index('full_confirm "Update stage $STAGE to commit $SRC_COMMIT? [y/N] " yes', plan)
        self.assertLess(decided, plan, "setup decides whether the update builds only the client before it prints the plan")
        self.assertLess(ends, asks, "a dry run ends after the plan and before the question")
        self.assertIn('say "  update:        $how"', full)

    def test_b804_exit_codes_ham_update_names(self) -> None:
        setup = read("setup.sh")
        header = setup[: setup.index("set -u")]
        for words in ("4 you cancelled", "6 an operation is still running and safe to resume", "7 a conflicting"):
            self.assertIn(words, re.sub(r"\s+#\s*", " ", header))


if __name__ == "__main__":
    unittest.main()
