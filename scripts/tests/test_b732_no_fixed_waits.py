"""No fixed wait in the setup tests.

Every limit on a wait in `test_setup_*.py` and the helpers `setup_*.py` goes through `setup_waits.limit(...)`, so that `HAMH_TEST_WAIT_SCALE` reaches all of it. This test reads
those files with `ast` and lists what it finds in the forms the files use:

  a `timeout=` keyword whose value is a number, a constant that is not a `limit(...)`, or any name other than the parameter of the function it is in
  a parameter named `timeout` whose default is a number
  `subprocess.run`, `check_output`, `check_call` or `call` with no `timeout=` (a call that can wait for ever is a hang that never fails)
  `.communicate(...)` with no `timeout=`, or a number as the first argument of `.communicate`, `.wait` or `.join`
  `time.monotonic() + <number>` and `time.time() + <number>` (a deadline written by hand)
  a module constant named like a wait (`..._WAIT`, `..._TIMEOUT`, `..._SECONDS`) that is a number
  `limit(...)` inside the seconds of `wait_for`, `wait_ready`, `interrupt` or `follow_started`, which scale their seconds themselves (a limit scaled twice)
  a file that calls `limit(...)` without importing it from `setup_waits`

It does not prove that every wait is covered: a wait written in another form is not seen. The cases at the end show what each rule catches. A new wait gets its limit from
`limit(...)` or, if it has to be a different kind of wait, an entry in ALLOWED below with its reason.

Run with:
  $HAMH_PYTHON -B -m unittest discover -s scripts/tests -p test_b732_no_fixed_waits.py
"""

from __future__ import annotations

import ast
import re
import unittest
from pathlib import Path
from typing import List

TESTS = Path(__file__).resolve().parent
HELPER = "setup_waits.py"
SUBPROCESS_CALLS = {"run", "check_output", "check_call", "call"}
WAIT_NAME = re.compile(r"(_WAIT|_TIMEOUT|_SECONDS|_SECS)$")
CONSTANTS_FROM_LIMIT = {"PROMPT_WAIT", "FINISH_WAIT"}
SCALE_THEMSELVES = {"wait_for", "wait_ready", "interrupt", "follow_started"}  # take the limit of a free host and scale it

# (file, line text fragment) of a wait that is not a limit on a program or an event, with the reason. None today.
ALLOWED: dict = {}


def is_limit_call(node: ast.AST) -> bool:
    return isinstance(node, ast.Call) and isinstance(node.func, ast.Name) and node.func.id == "limit"


def is_number(node: ast.AST) -> bool:
    return isinstance(node, ast.Constant) and isinstance(node.value, (int, float)) and not isinstance(node.value, bool)


class Finder(ast.NodeVisitor):
    def __init__(self) -> None:
        self.found: List[str] = []
        self.params: List[set] = []

    def add(self, node: ast.AST, message: str) -> None:
        self.found.append(f"line {node.lineno}: {message}")

    def visit_FunctionDef(self, node: ast.FunctionDef) -> None:
        names = {a.arg for a in node.args.args + node.args.kwonlyargs}
        pairs = list(zip(reversed(node.args.args), reversed(node.args.defaults))) + [(a, d) for a, d in zip(node.args.kwonlyargs, node.args.kw_defaults) if d is not None]
        for arg, default in pairs:
            if arg.arg == "timeout" and not (is_limit_call(default) or (isinstance(default, ast.Name) and default.id in CONSTANTS_FROM_LIMIT) or (isinstance(default, ast.Constant) and default.value is None)):
                self.add(default, f"parameter timeout of {node.name} has the default {ast.unparse(default)}, not limit(...)")
        self.params.append(names)
        self.generic_visit(node)
        self.params.pop()

    visit_AsyncFunctionDef = visit_FunctionDef

    def visit_Assign(self, node: ast.Assign) -> None:
        for target in node.targets:
            if isinstance(target, ast.Name) and WAIT_NAME.search(target.id) and not is_limit_call(node.value):
                self.add(node, f"{target.id} = {ast.unparse(node.value)[:40]} is not limit(...)")
        self.generic_visit(node)

    def visit_BinOp(self, node: ast.BinOp) -> None:
        if isinstance(node.op, ast.Add) and is_number(node.right) and re.search(r"time\.(monotonic|time)\(\)", ast.unparse(node.left)):
            self.add(node, f"a deadline written by hand: {ast.unparse(node)}")
        self.generic_visit(node)

    def visit_Call(self, node: ast.Call) -> None:
        func = node.func
        name = func.attr if isinstance(func, ast.Attribute) else (func.id if isinstance(func, ast.Name) else "")
        keywords = {k.arg: k.value for k in node.keywords if k.arg}
        if "timeout" in keywords:
            value = keywords["timeout"]
            in_params = any("timeout" in p for p in self.params[-1:])
            ok = is_limit_call(value) or (isinstance(value, ast.Name) and ((value.id == "timeout" and in_params) or value.id in CONSTANTS_FROM_LIMIT))
            if not ok:
                self.add(node, f"{name}(timeout={ast.unparse(value)}) is not limit(...)")
        elif isinstance(func, ast.Attribute) and isinstance(func.value, ast.Name) and func.value.id == "subprocess" and name in SUBPROCESS_CALLS:
            self.add(node, f"subprocess.{name} has no timeout")
        elif name == "communicate" and isinstance(func, ast.Attribute):
            self.add(node, "communicate() has no timeout")
        if name in {"communicate", "wait", "join"} and isinstance(func, ast.Attribute) and node.args and is_number(node.args[0]):
            self.add(node, f"{name}({ast.unparse(node.args[0])}) is a fixed wait")
        if name in SCALE_THEMSELVES:
            for part in list(node.args) + [k.value for k in node.keywords]:
                if any(is_limit_call(inner) for inner in ast.walk(part)):
                    self.add(node, f"{name}(... limit(...) ...) scales its seconds twice: pass the limit of a free host")
        self.generic_visit(node)


def find_fixed_waits(source: str) -> List[str]:
    tree = ast.parse(source)
    finder = Finder()
    finder.visit(tree)
    uses_limit = any(is_limit_call(n) for n in ast.walk(tree))
    imported = any(isinstance(n, ast.ImportFrom) and n.module == "setup_waits" and any(a.name == "limit" for a in n.names) for n in tree.body)
    if uses_limit and not imported:
        finder.found.append("line 1: limit(...) is used without `from setup_waits import limit`")
    return finder.found


def family() -> List[Path]:
    files = [p for p in sorted(TESTS.glob("test_setup_*.py")) + sorted(TESTS.glob("setup_*.py")) if p.name != HELPER]
    return files


class FamilyCase(unittest.TestCase):
    def test_b732_the_family_is_found(self) -> None:
        names = {p.name for p in family()}
        for must in ("test_setup_wizard.py", "test_setup_full_state.py", "test_setup_no_leftovers.py", "setup_full_support.py", "setup_fake_aws.py"):
            self.assertIn(must, names)

    def test_b732_no_setup_test_has_a_fixed_wait(self) -> None:
        problems = []
        for path in family():
            for line in find_fixed_waits(path.read_text(encoding="utf-8")):
                if (path.name, line.split(": ", 1)[1]) not in ALLOWED:
                    problems.append(f"{path.name} {line}")
        self.assertEqual(problems, [], "put these limits through setup_waits.limit(...) (HAMH_TEST_WAIT_SCALE):\n" + "\n".join(problems))


class FinderCase(unittest.TestCase):
    """What each rule catches, so that the scan above cannot go quiet by accident."""

    def found(self, source: str) -> List[str]:
        return find_fixed_waits(source)

    IMPORT = "from setup_waits import limit\n"

    def test_b732_a_number_as_timeout_is_found(self) -> None:
        self.assertEqual(len(self.found("import subprocess\nsubprocess.run(['x'], timeout=60)\n")), 1)
        self.assertEqual(len(self.found("import subprocess\nsubprocess.run(['x'], timeout=None)\n")), 1)

    def test_b732_limit_is_accepted(self) -> None:
        self.assertEqual(self.found(self.IMPORT + "import subprocess\nsubprocess.run(['x'], timeout=limit(60))\nproc.communicate(timeout=limit(30))\n"), [])

    def test_b732_a_limit_scaled_twice_is_found(self) -> None:
        self.assertEqual(len(self.found(self.IMPORT + "proc.wait_ready(ready, limit(40), 'x')\n")), 1)
        self.assertEqual(len(self.found(self.IMPORT + "wait_for(ready, seconds=limit(40), what='x')\n")), 1)
        self.assertEqual(self.found(self.IMPORT + "proc.wait_ready(ready, 40, 'x')\n"), [])

    def test_b732_a_limit_that_is_not_the_helper_is_found(self) -> None:
        self.assertEqual(len(self.found("def limit(x):\n    return x\nsubprocess.run(['x'], timeout=limit(60))\n")), 1)

    def test_b732_a_parameter_passed_on_is_accepted_only_inside_a_function_that_has_it(self) -> None:
        ok = "def f(proc, timeout):\n    return proc.wait(timeout=timeout)\n"
        self.assertEqual(self.found(ok), [])
        self.assertEqual(len(self.found("def g(proc):\n    return proc.wait(timeout=timeout)\n")), 1)

    def test_b732_a_constant_is_accepted_only_when_it_comes_from_limit(self) -> None:
        self.assertEqual(self.found(self.IMPORT + "PROMPT_WAIT = limit(60.0)\ndef f(r):\n    return r.run(timeout=PROMPT_WAIT)\n"), [])
        self.assertEqual(len(self.found("PROMPT_WAIT = 60.0\n")), 1)
        self.assertEqual(len(self.found("def f(timeout=60.0):\n    pass\n")), 1)
        self.assertEqual(self.found(self.IMPORT + "def f(timeout=None):\n    pass\ndef h(timeout=limit(30.0)):\n    pass\n"), [])

    def test_b732_a_call_that_can_wait_for_ever_is_found(self) -> None:
        self.assertEqual(len(self.found("import subprocess\nsubprocess.check_output(['x'])\n")), 1)
        self.assertEqual(len(self.found("proc.communicate()\n")), 1)
        self.assertEqual(self.found(self.IMPORT + "proc.communicate('input', timeout=limit(5))\n"), [])

    def test_b732_a_fixed_positional_wait_is_found(self) -> None:
        for call in ("proc.communicate(30)", "proc.wait(30)", "thread.join(5)"):
            with self.subTest(call=call):
                self.assertTrue(self.found(call + "\n"))

    def test_b732_a_deadline_written_by_hand_is_found(self) -> None:
        self.assertEqual(len(self.found("import time\ndeadline = time.monotonic() + 40\n")), 1)
        self.assertEqual(self.found(self.IMPORT + "import time\ndeadline = time.monotonic() + limit(40)\n"), [])


if __name__ == "__main__":
    unittest.main()
