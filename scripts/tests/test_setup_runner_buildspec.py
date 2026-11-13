"""Tests for images/codebuild/setup-runner.yml, the buildspec of the setup runner's CodeBuild project.

The project (infra/setup-runner.yaml) reads this file from the source archive that `setup.sh` uploads, so the buildspec of a run is the one of the commit that
is built. It holds one command, scripts/setup/runner-build.sh, and nothing else of the logic. The rules of the other buildspecs in images/codebuild apply as
well (infra/test/buildspecs.test.ts reads every file of the folder): under 22,000 bytes, no Region and no account number.
"""

from __future__ import annotations

import re
import unittest

import setup_runner_support as sup

SPEC = sup.buildspec() if sup.BUILDSPEC_PATH.is_file() else {}
TEXT = sup.BUILDSPEC_PATH.read_text(encoding="utf-8") if sup.BUILDSPEC_PATH.is_file() else ""


class BuildspecCase(unittest.TestCase):
    def setUp(self) -> None:
        if not sup.BUILDSPEC_PATH.is_file():
            self.fail("images/codebuild/setup-runner.yml does not exist")

    def test_version_and_shell(self) -> None:
        self.assertEqual(SPEC["version"], 0.2)
        self.assertEqual(SPEC["env"]["shell"], "bash")

    def test_python_runtime_for_the_scripts(self) -> None:
        # deploy.py reads TOML with tomllib, which Python 3.11 introduced.
        self.assertEqual(SPEC["phases"]["install"]["runtime-versions"], {"python": "3.12"})

    def test_the_runner_script_is_the_only_work_and_the_hook_of_the_other_buildspecs_comes_first(self) -> None:
        self.assertEqual(sorted(SPEC["phases"]), ["build", "install"])
        # Every buildspec of the folder starts with the hook that reads a generated account file from a bundle (scripts/lib/child-build-env.sh); a run of
        # this project gets its account file from the bucket instead, so the hook has nothing to do here and changes no variable.
        # ... and the build ends by reading the record that only this build makes: the SHA-256 of its result.json (see test_the_hash_of_the_result_is_exported_to_codebuild).
        self.assertEqual(SPEC["phases"]["build"]["commands"], [
            ". scripts/lib/child-build-env.sh",
            "export SETUP_RESULT_SHA256=none RUNNER_BUILD_WORK=$CODEBUILD_SRC_DIR/../hamh-setup-work",
            "bash scripts/setup/runner-build.sh",
            ". $RUNNER_BUILD_WORK/anchor.env"])
        self.assertNotIn("commands", SPEC["phases"]["install"], "the hook must be the first command of the first phase that has commands")
        self.assertTrue(sup.BUILD_SCRIPT.is_file())
        lines = TEXT.splitlines()
        at = lines.index("phases:")
        first = next(i for i in range(at, len(lines)) if lines[i].strip() == "commands:")
        self.assertEqual(lines[first + 1].strip(), "- . scripts/lib/child-build-env.sh")

    def test_no_artifacts_cache_or_reports(self) -> None:
        for key in ("artifacts", "cache", "reports", "secrets-manager", "parameter-store"):
            self.assertNotIn(key, SPEC)
            self.assertNotIn(key, SPEC.get("env", {}))

    def test_no_environment_variables_are_set(self) -> None:
        self.assertEqual(sorted(SPEC["env"]), ["exported-variables", "shell"], "no variables: block, no parameter-store, no secrets-manager")

    def test_the_hash_of_the_result_is_exported_to_codebuild(self) -> None:
        """setup.sh believes a result.json only when it hashes to the exported variable, which CodeBuild keeps with the build (batch-get-builds). runner-build.sh writes
        <work>/anchor.env after it uploaded the result; the buildspec sources it from the same folder, and the variable starts as `none` so that it exists on every path."""
        self.assertEqual(SPEC["env"]["exported-variables"], ["SETUP_RESULT_SHA256"])
        commands = SPEC["phases"]["build"]["commands"]
        start = next(c for c in commands if c.startswith("export "))
        self.assertIn("SETUP_RESULT_SHA256=none", start)
        self.assertIn("RUNNER_BUILD_WORK=", start)
        self.assertLess(commands.index(start), commands.index("bash scripts/setup/runner-build.sh"))
        self.assertEqual(commands[-1], ". $RUNNER_BUILD_WORK/anchor.env")
        script = sup.BUILD_SCRIPT.read_text(encoding="utf-8")
        self.assertIn("anchor.env", script)
        self.assertIn("export SETUP_RESULT_SHA256=", script)

    def test_size_and_neutrality_like_the_other_buildspecs(self) -> None:
        self.assertLess(len(TEXT.encode("utf-8")), 22_000)
        self.assertIsNone(re.search(r"\b(us|eu|ap|ca|sa)-(east|west|north|south|central|northeast|southeast)-[0-9]\b", TEXT))
        self.assertIsNone(re.search(r"\b[0-9]{12}\b", TEXT))

    def test_no_trace_mode_that_would_print_the_environment(self) -> None:
        self.assertNotIn("set -x", TEXT)
        self.assertNotIn("printenv", TEXT)
        self.assertNotRegex(TEXT, r"(?m)^\s*-\s*env\s*$")


if __name__ == "__main__":
    unittest.main()
