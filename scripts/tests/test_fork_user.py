"""Tests for scripts/lib/fork_user.sh: which uid:gid the Cline fork build runs its containers as.

Upstream suites assume a non-root user (plugin-uninstall tests expect permission errors), so a
root host (CodeBuild) runs the containers as a fixed non-root uid; other hosts keep their own
uid so files in the run dir stay theirs.
"""

from __future__ import annotations

import subprocess  # nosec B404: tests start the scripts under test and git with argument lists, never a shell
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
LIB = REPO / "scripts" / "lib" / "fork_user.sh"


def call(*args: str, env: str = "") -> subprocess.CompletedProcess:
    prefix = f"export {env}; " if env else ""
    script = f'{prefix}. "{LIB}"; {" ".join(args)}'
    return subprocess.run(["bash", "-c", script], capture_output=True, text=True, timeout=30)  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do


class ForkUserCase(unittest.TestCase):
    def test_a_non_root_host_keeps_its_uid_and_gid(self) -> None:
        self.assertEqual(call("container_user", "501", "20").stdout.strip(), "501:20")
        self.assertEqual(call("container_user", "13653916", "100").stdout.strip(), "13653916:100")

    def test_a_root_host_runs_containers_as_the_non_root_build_uid(self) -> None:
        self.assertEqual(call("container_user", "0", "0").stdout.strip(), "1000:1000")

    def test_the_build_uid_is_configurable_but_never_root(self) -> None:
        self.assertEqual(call("container_user", "0", "0", env="FORK_BUILD_UID=2000").stdout.strip(), "2000:2000")
        proc = call("container_user", "0", "0", env="FORK_BUILD_UID=0")
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("non-root", proc.stderr)

    def test_needs_handover_only_when_the_host_is_root(self) -> None:
        self.assertEqual(call("needs_handover", "0").returncode, 0)
        self.assertEqual(call("needs_handover", "501").returncode, 1)

    def test_the_build_script_uses_the_helper_for_every_container(self) -> None:
        text = (REPO / "scripts" / "build-cline-fork.sh").read_text()
        self.assertIn('. "$HERE/lib/fork_user.sh"', text)
        self.assertNotIn('--user "$(id -u):$(id -g)"', text)
        self.assertGreaterEqual(text.count('--user "$CUSER"'), 2)


if __name__ == "__main__":
    unittest.main()
