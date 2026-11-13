"""The variables of the earlier spelling in `scripts/setup/names.sh` (COMPAT-CAH, hand-edit item setup-05).
# COMPAT-CAH: this file names the earlier variable prefix on purpose, and ends with the shim it tests.

`hamh_env_compat` copies each variable of the earlier prefix to the one of the current prefix for the names setup reads (ENV_COMPAT_NAMES), with one line on stderr per name, and unsets the earlier variable, so that nothing
that setup starts sees two spellings of one setting. The current name wins when both are set; for the three selectors (AWS_PROFILE, ACCOUNTS_FILE, OWNER) two different
values are an error (return 2; setup.sh exits 2). HAMH_NO_DEPRECATION_NOTICES=1 silences the line. names.sh runs the helper first, so its own four name variables take the
earlier spelling too.
"""

from __future__ import annotations

import json
import os
import subprocess  # nosec B404: tests start bash and setup.sh with argument lists, never a shell string built from data
import unittest
from pathlib import Path

from setup_waits import limit

REPO = Path(__file__).resolve().parents[2]
NAMES = REPO / "scripts" / "setup" / "names.sh"
SETUP = REPO / "setup.sh"
EXPIRY_ENV = REPO / "env-names.manifest"
SELECTORS = ("AWS_PROFILE", "ACCOUNTS_FILE", "OWNER")
OLD = "CAH_"  # the earlier prefix, as a probe word


def bash(script: str, env: dict | None = None) -> subprocess.CompletedProcess:
    full = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LC_ALL": "C"}
    full.update(env or {})
    return subprocess.run(["bash", "-c", f'. "{NAMES}" || exit 9; ' + script], capture_output=True, text=True, env=full, timeout=limit(60), cwd=str(REPO))  # nosec B603: argument list, no shell string from data; B607: bash on PATH as the other tests do


class CopyAndUnset(unittest.TestCase):
    def test_an_earlier_variable_is_copied_exported_and_unset_with_one_notice(self) -> None:
        p = bash('printf "%s\\n" "$HAMH_OWNER"; env | grep -c "^HAMH_OWNER=legacy-owner$"; env | grep -c "^' + OLD + 'OWNER=" || true', {OLD + "OWNER": "legacy-owner"})
        self.assertEqual(p.returncode, 0, p.stderr)
        lines = p.stdout.split()
        self.assertEqual(lines[:2], ["legacy-owner", "1"], "copied and exported")
        self.assertEqual(lines[2], "0", "the earlier variable is gone from the environment of every child")
        self.assertEqual(p.stderr.count(OLD + "OWNER"), 1, p.stderr)
        self.assertIn("HAMH_OWNER", p.stderr)
        self.assertIn("previous name", p.stderr)
        self.assertIn("Set HAMH_NO_DEPRECATION_NOTICES=1 to silence this notice.", p.stderr)

    def test_the_helper_a_second_time_prints_nothing_more(self) -> None:
        p = bash("hamh_env_compat; hamh_env_compat", {OLD + "AWS_CLI": "/x/aws"})
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(p.stderr.count(OLD + "AWS_CLI"), 1, p.stderr)

    def test_one_notice_for_each_name(self) -> None:
        p = bash("true", {OLD + "AWS_CLI": "/x/aws", OLD + "SETUP_POLL_SECONDS": "3"})
        self.assertEqual(p.stderr.count("previous name"), 2, p.stderr)

    def test_the_switch_silences_the_notice_and_the_copy_still_happens(self) -> None:
        p = bash('printf "%s" "$HAMH_AWS_CLI"', {OLD + "AWS_CLI": "/x/aws", "HAMH_NO_DEPRECATION_NOTICES": "1"})
        self.assertEqual((p.returncode, p.stdout, p.stderr), (0, "/x/aws", ""))

    def test_the_current_name_wins_when_both_are_set(self) -> None:
        p = bash('printf "%s" "$HAMH_AWS_CLI"', {OLD + "AWS_CLI": "/old/aws", "HAMH_AWS_CLI": "/new/aws"})
        self.assertEqual((p.returncode, p.stdout), (0, "/new/aws"), p.stderr)
        self.assertIn("takes precedence", p.stderr)

    def test_the_same_value_under_both_names_is_silent(self) -> None:
        p = bash('printf "%s" "$HAMH_AWS_CLI"', {OLD + "AWS_CLI": "/x/aws", "HAMH_AWS_CLI": "/x/aws"})
        self.assertEqual((p.returncode, p.stdout, p.stderr), (0, "/x/aws", ""))

    def test_an_empty_current_name_counts_as_unset(self) -> None:
        p = bash('printf "%s" "$HAMH_AWS_CLI"', {OLD + "AWS_CLI": "/old/aws", "HAMH_AWS_CLI": ""})
        self.assertEqual((p.returncode, p.stdout), (0, "/old/aws"), p.stderr)

    def test_a_variable_that_is_not_in_the_list_is_left_alone(self) -> None:
        new = "HAM" + "H_"  # the current prefix, split so that a scan of the tree for variable names does not count the probe
        p = bash('env | grep -c "^' + OLD + 'SOMETHING_ELSE=1$"; env | grep -c "^' + new + 'SOMETHING_ELSE="; true', {OLD + "SOMETHING_ELSE": "1"})
        self.assertEqual(p.stdout.split(), ["1", "0"], p.stderr)
        self.assertEqual(p.stderr, "")

    def test_names_given_to_the_helper_are_the_only_ones_it_touches(self) -> None:
        # names.sh folds the names of its list when it is sourced; a name outside the list is folded only when the caller names it
        new = "HAM" + "H_"  # the current prefix, split so that a scan of the tree for variable names does not count the probes
        p = bash('printf "%s|" "${' + new + 'PROBE_ONE:-unset}" "${' + new + 'PROBE_TWO:-unset}"; hamh_env_compat PROBE_ONE; printf "%s|%s" "${' + new + 'PROBE_ONE:-unset}" "${' + new + 'PROBE_TWO:-unset}"',
                 {OLD + "PROBE_ONE": "one", OLD + "PROBE_TWO": "two"})
        self.assertEqual((p.returncode, p.stdout), (0, "unset|unset|one|unset"), p.stderr)

    def test_the_four_name_variables_take_the_earlier_spelling_when_the_file_is_sourced(self) -> None:
        p = bash('printf "%s|%s" "$HAMH_CLIENT_BIN" "$HAMH_CONFIG_DIR_NAME"', {OLD + "CLIENT_BIN": "probe-client", OLD + "CONFIG_DIR_NAME": "probe-config"})
        self.assertEqual((p.returncode, p.stdout), (0, "probe-client|probe-config"), p.stderr)

    def test_the_legacy_names_have_defaults(self) -> None:
        p = bash('printf "%s %s" "$LEGACY_CLIENT_BIN" "$HAMH_LEGACY_CONFIG_DIR_NAME"')
        self.assertEqual((p.returncode, p.stdout), (0, "cah cah"), p.stderr)


class Selectors(unittest.TestCase):
    def test_two_different_values_are_an_error_for_each_selector(self) -> None:
        for name in SELECTORS:
            with self.subTest(selector=name):
                script = f'. "{NAMES}"; echo "returned $?"'
                p = subprocess.run(["bash", "-c", script], capture_output=True, text=True, timeout=limit(60), cwd=str(REPO),  # nosec B603: argument list; B607: bash on PATH
                                   env={"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LC_ALL": "C", "HAMH_" + name: "new", OLD + name: "old"})
                self.assertIn("returned 2", p.stdout)
                self.assertIn("HAMH_" + name, p.stderr)
                self.assertIn(OLD + name, p.stderr)
                self.assertIn("never ignored", p.stderr)

    def test_the_same_value_is_fine_for_a_selector(self) -> None:
        p = bash('printf "%s" "$HAMH_OWNER"', {"HAMH_OWNER": "same", OLD + "OWNER": "same"})
        self.assertEqual((p.returncode, p.stdout, p.stderr), (0, "same", ""))

    def test_setup_exits_2_and_changes_nothing(self) -> None:
        for args in (["--help"], ["--status"], ["--dry-run"]):
            with self.subTest(args=args):
                p = subprocess.run(["bash", str(SETUP), *args], capture_output=True, text=True, timeout=limit(60), cwd=str(REPO),  # nosec B603: argument list; B607: bash on PATH
                                   env={"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LC_ALL": "C", "HOME": "/nonexistent-home", "HAMH_AWS_PROFILE": "a", OLD + "AWS_PROFILE": "b"})
                self.assertEqual(p.returncode, 2, p.stdout + p.stderr)
                self.assertEqual(p.stdout, "")
                self.assertIn("HAMH_AWS_PROFILE", p.stderr)
                self.assertIn(OLD + "AWS_PROFILE", p.stderr)


class ListAgreesWithTheTable(unittest.TestCase):
    def names(self) -> list:
        p = bash('printf "%s" "$ENV_COMPAT_NAMES"')
        return p.stdout.split()

    def test_the_list_holds_the_three_selectors_and_the_four_name_variables(self) -> None:
        got = self.names()
        for want in (*SELECTORS, "CLIENT_BIN", "ADMIN_BIN", "PRODUCT_NAME", "CONFIG_DIR_NAME"):
            self.assertIn(want, got)
        self.assertEqual(len(got), len(set(got)), "no name twice")

    def test_every_name_in_the_list_is_a_tier_1_variable_of_the_table(self) -> None:
        if not EXPIRY_ENV.is_file():
            self.skipTest("the manifest of the variables with an earlier name is not in this tree")
        tier1 = {n["suffix"] for n in json.loads(EXPIRY_ENV.read_text())["names"] if n.get("tier") == 1}
        self.assertEqual(sorted(set(self.names()) - tier1), [], "names setup copies that the table does not list as Tier 1")


if __name__ == "__main__":
    unittest.main()
