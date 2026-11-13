"""What the product code says about itself: its comments, doc comments and deployed descriptions state what the code does, in the present tense.

Each file below once carried such words; the table pins the fragments that were removed so that they do not return.

  TEXT_PINS      a file and the fragments it must not contain.
  SETUP_SCRIPTS  every shell script of `scripts/setup/` and `setup.sh` outside comment lines: no sentence that tells a person to delete a stage
                 that may be working, and no word that marks a feature as unfinished.
                 repository does not contain.

An example value (the time `2030-01-01T00:00:00Z` in a usage message, a protocol version) is data and is not matched: the fragments are phrases.

Run it from the repository root:
  python3 -m unittest discover -s scripts/tests -p test_shipped_comment_words.py
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]

TEXT_PINS = {
    # deployed descriptions: a stranger reads them in the CloudFormation console and in the AgentCore console
    "infra/lib/stacks/packages.ts": ("Lane B", "verifiers", "Measured live", "acct-"),
    "infra/lib/stacks/runtimes.ts": ("OpenCode)",),
    # comments
    "infra/lib/stacks/orchestration-direct.ts": ("multi-agent", "Lane B", "run-tasks", "(2026-"),
    "infra/lib/stacks/observability.ts": ("us-east-1 holds five",),
    "infra/lib/stacks/control.ts": ("run-tasks",),
    "infra/lib/config.ts": ("real gateway, 2026-", "first deploy, 2026-"),
    "scripts/lib/build_selection.py": ("acct-", "dispatch-turn", "hamh-dispatch", "run-tasks", "On 2026-", "builds of 2026-", "(measured"),
    "scripts/gomod-store.sh": ("Lane B", "verifiers"),
    "scripts/setup/teardown.sh": ("keep costing",),
    "scripts/seed-lang-dl.sh": ("On 2026-", "the fleet's"),
    "scripts/research-key-put.py": ("fetched 2026-", "2026-09-26"),
    "scripts/lib/teardown.py": ("live 2026-", "hamh-drill-harness"),
    "scripts/lib/arm64_sysroot.py": ("measured 2026-",),
    "scripts/lib/gomod_store.py": ("live finding of",),
    "crates/ham/src/lib.rs": ("A7", "ham-e2e`, `tui-probe`", "worker image key"),
    "crates/ham/src/ws.rs": ("A7", "ham-e2e"),
    "crates/ham/src/watch.rs": ("live test drivers",),
    "crates/ham/src/shell.rs": ("A7", "multi-agent", "ham-e2e"),
    "crates/ham/src/cli.rs": ("hamh-dispatch", "K-suite", "set apiEndpoint (the /v1 stage URL)"),
    "crates/ham-core/src/command.rs": ("hamh-dispatch", "K-suite", "before 2026-"),
    "crates/run-session/src/wiring.rs": ("(2026-",),
    "crates/run-session/src/plan_delivery.rs": ("live row", "A8,"),
    "crates/research-api/src/agentcore.rs": ("(measured 2026-",),
    "crates/dispatcher/src/policy.rs": ("STS on 2026-",),
    "crates/dispatcher/src/aws.rs": ("measured\n/// 2026-",),
    "crates/delivery/src/validator.rs": ("the multi-agent line",),
    "engine/catalog/models.json": ("live pass",),
}
# A message the file must contain (the text a person reads when no stage URL is set).
REQUIRED = {
    "crates/ham/src/cli.rs": "No stage URL is set. You set api_endpoint to your stage URL with `ham config set api_endpoint <url>`",
}

SETUP_SCRIPTS = ["setup.sh", *sorted(str(p.relative_to(REPO)) for p in (REPO / "scripts" / "setup").glob("*.sh"))]
SETUP_FORBIDDEN = ("to remove everything", "interim", "oding agents")

RUSTDOC_FORBIDDEN = re.compile(r"\bA\d{1,2}\b|ham-e2e|multi-agent")


def read(rel: str) -> str:
    return (REPO / rel).read_text(encoding="utf-8")


class TextPinCase(unittest.TestCase):
    def test_no_pinned_fragment_returns(self) -> None:
        for rel, fragments in TEXT_PINS.items():
            text = read(rel)
            for fragment in fragments:
                with self.subTest(file=rel, fragment=fragment):
                    self.assertFalse(fragment in text, f"{rel} carries {fragment!r} again")

    def test_the_required_messages_are_there(self) -> None:
        for rel, message in REQUIRED.items():
            with self.subTest(file=rel):
                self.assertTrue(message in read(rel), f"{rel} lacks its message")


def code_lines(text: str):
    """The lines of a shell script that are not comments."""
    for n, line in enumerate(text.splitlines(), 1):
        if not line.lstrip().startswith("#"):
            yield n, line


class SetupScriptCase(unittest.TestCase):
    def test_no_message_invites_a_person_to_delete_a_working_stage_or_marks_a_feature_unfinished(self) -> None:
        for rel in SETUP_SCRIPTS:
            for n, line in code_lines(read(rel)):
                for word in SETUP_FORBIDDEN:
                    with self.subTest(file=rel, line=n, word=word):
                        self.assertNotIn(word, line, f"{rel}:{n} says {word!r}")


class RustdocCase(unittest.TestCase):
    def test_the_crate_documentation_of_ham_names_no_acceptance_row_and_no_missing_crate(self) -> None:
        for path in sorted((REPO / "crates" / "ham" / "src").glob("*.rs")):
            for n, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
                if line.startswith("//!"):
                    with self.subTest(file=path.name, line=n):
                        self.assertIsNone(RUSTDOC_FORBIDDEN.search(line), f"{path.name}:{n}: {line}")


if __name__ == "__main__":
    unittest.main()
