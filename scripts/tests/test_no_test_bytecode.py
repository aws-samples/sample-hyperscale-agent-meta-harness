"""The child processes that the shipped tests start write no bytecode into the repository.

A test that puts a stand-in program first on PATH (a fake `aws`, a fake `cargo`) starts it as a child process. A child that imports a module of this
repository (`setup_fake_aws`, `s3_versions_model`, `runner_state`, `selection_fixture`) writes `__pycache__/<module>.cpython-3xx.pyc` beside that module
unless it is told not to. The environment of the test run does not tell it: the stand-in launcher starts with `python -I`, which ignores
`PYTHONDONTWRITEBYTECODE`, and the tests build the environment of a child from a short list that leaves the variable out. A person who runs the shipped
tests in an exported tree then finds `.pyc` files in it, and the public scan reports them.

Each case below breaks that way of writing a file:

  StandInLauncherCase   installs the launcher of the AWS CLI stand-in in a made-up copy of the tree, runs it with an environment that holds no `PYTHON*`
                        variable, and requires that no `.pyc` file and no `__pycache__` folder exists afterwards.
  ChildScriptTextCase   reads every test module and requires that the text of a stand-in program that adds a folder to `sys.path` (and so imports a module
                        beside the tests) switches bytecode off before its first import.

Run it from the repository root:
  python3 -m unittest discover -s scripts/tests -p test_no_test_bytecode.py
"""

from __future__ import annotations

import ast
import os
import shutil
import subprocess  # nosec B404: runs the stand-in launcher with an argument list, never through a shell
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
sys.path.insert(0, str(HERE))
import setup_fake_aws  # noqa: E402  its LAUNCHER text is the program under test


def child_environment(home: Path) -> dict:
    """What a test gives a child: a short list, with no variable that tells Python to leave bytecode out."""
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(home), "TMPDIR": str(home)}
    for key in env:
        assert not key.startswith("PYTHON"), key
    return env


class StandInLauncherCase(unittest.TestCase):
    def test_the_aws_stand_in_launcher_writes_no_bytecode(self) -> None:
        with tempfile.TemporaryDirectory(dir=os.environ.get("TMPDIR")) as td:
            root = Path(td) / "tree"
            tests, setup = root / "scripts" / "tests", root / "scripts" / "setup"
            tests.mkdir(parents=True)
            setup.mkdir(parents=True)
            for name in ("setup_fake_aws.py", "s3_versions_model.py"):
                shutil.copy(HERE / name, tests / name)
            shutil.copy(REPO / "scripts" / "setup" / "runner_state.py", setup / "runner_state.py")
            launcher = Path(td) / "aws"
            launcher.write_text(setup_fake_aws.LAUNCHER.format(python=sys.executable, here=str(tests)))
            launcher.chmod(0o755)
            home = Path(td) / "home"
            home.mkdir()
            run = subprocess.run([str(launcher), "--version"], capture_output=True, text=True, timeout=120, env=child_environment(home), cwd=td, check=False)  # nosec B603: the launcher of this test, argument list, no shell
            self.assertNotIn("ModuleNotFoundError", run.stderr, run.stderr)
            written = sorted(str(p.relative_to(root)) for p in list(root.rglob("*.pyc")) + list(root.rglob("__pycache__")))
            self.assertEqual(written, [], "the stand-in launcher wrote bytecode into the tree")


def string_constants(path: Path):
    tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    for node in ast.walk(tree):
        if isinstance(node, ast.Constant) and isinstance(node.value, str):
            yield node.lineno, node.value


class ChildScriptTextCase(unittest.TestCase):
    def test_a_stand_in_program_that_imports_a_sibling_module_switches_bytecode_off_first(self) -> None:
        offenders = []
        for path in sorted(HERE.glob("*.py")):
            for line, text in string_constants(path):
                if not text.startswith("#!") or "sys.path.insert" not in text:
                    continue
                before_import = text.split("sys.path.insert", 1)[0]
                if "dont_write_bytecode" not in before_import and " -B" not in text.splitlines()[0]:
                    offenders.append(f"{path.name}:{line}")
        self.assertEqual(offenders, [], "these stand-in programs import a module beside the tests without switching bytecode off first: " + ", ".join(offenders))


if __name__ == "__main__":
    unittest.main()
