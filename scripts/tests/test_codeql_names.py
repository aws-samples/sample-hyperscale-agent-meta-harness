"""Tests for the names that make CodeQL's sensitive-data heuristic fire on two scripts.

GitHub CodeQL (py/clear-text-logging-sensitive-data) treats three shapes as a source of secret data, whatever the value is: a string assigned to a name that
holds the word secret, password, token or credential (`SECRET_NAME = "hamh-direct-research-tavily-api-key"`), and a dictionary lookup with such a word in a
literal key (`doc.get("SecretList")`, `doc["password"]`), including a constant that holds the key. It then reports every message that prints the value.
`scripts/research-key-put.py` prints the NAME of a Secrets Manager secret and `scripts/lib/teardown.py` prints the names of the stage's secrets; neither reads
or prints a secret value. The two files avoid the three shapes, so the published sample's scan stays at zero for them. The detector is tested on samples first,
so the guard can fail.
"""

from __future__ import annotations

import ast
import re
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
GUARDED = ("scripts/research-key-put.py", "scripts/lib/teardown.py")
SENSITIVE = re.compile(r"(?i)secret|passw|token|credential")


def sensitive_shapes(source: str) -> list[str]:
    """The three CodeQL source shapes in `source`, as 'line: shape' texts."""
    found: list[str] = []

    def is_text(node: ast.AST) -> bool:
        return isinstance(node, ast.Constant) and isinstance(node.value, str)

    for node in ast.walk(ast.parse(source)):
        if isinstance(node, (ast.Assign, ast.AnnAssign)) and node.value is not None and is_text(node.value):
            for target in node.targets if isinstance(node, ast.Assign) else [node.target]:
                for part in ast.walk(target):
                    name = part.id if isinstance(part, ast.Name) else part.attr if isinstance(part, ast.Attribute) else ""
                    if SENSITIVE.search(name):
                        found.append(f"{node.lineno}: a string assigned to the name {name}")
        elif isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute) and node.func.attr == "get" and node.args:
            if is_text(node.args[0]) and SENSITIVE.search(node.args[0].value):
                found.append(f"{node.lineno}: a lookup .get({node.args[0].value!r})")
        elif isinstance(node, ast.Subscript) and is_text(node.slice) and SENSITIVE.search(node.slice.value):
            found.append(f"{node.lineno}: a lookup [{node.slice.value!r}]")
    return found


class DetectorCase(unittest.TestCase):
    def test_it_finds_each_shape(self) -> None:
        self.assertEqual(len(sensitive_shapes('SECRET_NAME = "hamh-direct-research-tavily-api-key"\n')), 1)
        self.assertEqual(len(sensitive_shapes('class A:\n    def f(self):\n        self.api_token = "x"\n')), 1)
        self.assertEqual(len(sensitive_shapes('for sec in doc.get("SecretList", []):\n    pass\n')), 1)
        self.assertEqual(len(sensitive_shapes('value = doc["password"]\n')), 1)
        self.assertEqual(len(sensitive_shapes('LISTED = "ok"\nx = a.get("Credentials")\ny = b["AuthToken"]\n')), 2)

    def test_it_leaves_neutral_code_alone(self) -> None:
        for clean in ('RESEARCH_STORE_NAME = "hamh-direct-research-tavily-api-key"\n',
                      'doc.get("Tags")\nsec["Name"]\nsec["ARN"]\n',
                      'member = [v for k, v in doc.items() if k == "SecretList"]\n',
                      'secret_id = lookup()\nx = args[0]\n',
                      'text = "the secret is not printed"\n'):
            self.assertEqual(sensitive_shapes(clean), [], clean)


class GuardedFilesCase(unittest.TestCase):
    def test_the_two_scripts_have_none_of_the_shapes(self) -> None:
        for rel in GUARDED:
            with self.subTest(file=rel):
                self.assertEqual(sensitive_shapes((REPO / rel).read_text()), [],
                                 f"{rel} has a shape that CodeQL reads as secret data: rename the constant or read the member without a literal key that holds the word")


if __name__ == "__main__":
    unittest.main()
