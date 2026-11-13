"""The self-check script of the lifecycle pack: lifecycle/pack/.agents/skills/lifecycle-check/scripts/lifecycle_check.py.

The script reads the records a run leaves in its repository (the goal card, research notes, the context index, red records, the run log) and
the git history, and prints one line per check, `LC-n <name> PASS|FAIL|NA: <value>`, then a last line `LIFECYCLE-CHECK PASS|FAIL ...`. It
exits 0 when no check failed, 1 when one did, 2 when the directory is not a git repository. It writes nothing. It is the agent's own aid;
the harness-side scorer reports the same eight checks and never runs the copy in a work tree.

The cases build a golden repository with git at run time (a Python project with two behaviors, built red first) and then one broken
variant per rule: each variant changes one thing and wants exit 1 and a FAIL line for the check that rule belongs to. The golden repository
wants exit 0, eight PASS lines and no change to the repository. Python standard library only; needs git and no network.
"""

from __future__ import annotations

import ast
import hashlib
import importlib.util
import itertools
import os
import re
import shutil
import subprocess  # nosec B404: tests start the scripts under test and git with argument lists, never a shell
import sys
import tempfile
import time
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
SCRIPT = REPO / "lifecycle" / "pack" / ".agents" / "skills" / "lifecycle-check" / "scripts" / "lifecycle_check.py"
STARTER = REPO / "lifecycle" / "pack" / ".agents" / "skills" / "lifecycle-delivery" / "references" / "dox-root.md"  # pack 1.0.8: the root AGENTS.md of a DOX project

GOAL = """SPECIFIC: Add add() and subtract() to calc.py in this repository, with tests.
MEASURABLE: the goal is met when every check below passes.
1. `python3 -m unittest discover -s tests` exits 0 and prints OK.
2. `python3 -c "import calc; print(calc.add(2, 3))"` prints 5.
ACHIEVABLE: Scope is calc.py and tests/. Nothing else changes.
TIME-BOUND: Stop when all checks pass, or when one check fails three times with three different fixes, or two cycles change nothing; then end with BLOCKED: and what is missing.
VERIFY: Run checks 1 and 2 last, after the final edit, and print each command, its exit code and its last output line.
"""
RESEARCH = """# 01 Arithmetic helpers

## Question
Does Python's int addition need a helper at all?

## Sources read
- calc.py (this repository), read 2026-10-16
- docs/spec.md (user-supplied), read 2026-10-16

## Findings
- verified: the spec asks for add and subtract on integers.
- unknown: whether floats are wanted; the spec does not say.

## Decision
Integers only; floats are an open item.
"""
SPEC = "The calculator adds and subtracts integers.\n"
RED = "command: python3 -m unittest discover -s tests\nexit: 1\nfailure: ImportError: cannot import name 'add' from 'calc' (test_add)\n"
TEST_ADD = "import unittest\nimport calc\n\n\nclass T(unittest.TestCase):\n    def test_add(self):\n        self.assertEqual(calc.add(2, 3), 5)\n"
TEST_BOTH = TEST_ADD + "\n    def test_subtract(self):\n        self.assertEqual(calc.subtract(5, 3), 2)\n"
CODE_ADD = "def add(a, b):\n    return a + b\n"
CODE_BOTH = CODE_ADD + "\n\ndef subtract(a, b):\n    return a - b\n"
AGENTS = "# Calc\n\nA small calculator library.\n\n## Build\n`python3 -m compileall calc.py`\n\n## Test\n`python3 -m unittest discover -s tests`\n\n## Layout\ncalc.py, tests/\n"
README = "# Calc\n\n## Run\n`python3 -c \"import calc; print(calc.add(1, 2))\"`\n"
LOG = "# Log\n\nRUN | python3 -m unittest discover -s tests | exit 0 | pass 2 fail 0 | OK printed, no warnings\n"
DELIVER_BODY = "Adds the project AGENTS.md and README and the run log.\nWhy: the hand-over needs the build and test commands.\nEvidence: 2 tests pass, see .ham/lifecycle/LOG.md.\n"


def context_index(spec_sha: str, licence: str = "MIT-0") -> str:
    return ("# Context index\n\n| Origin | Date read | sha256 | Licence | Why it matters |\n| --- | --- | --- | --- | --- |\n"
            "| docs/spec.md | 2026-10-16 | %s | %s | the behaviors asked for |\n" % (spec_sha, licence))


def sha(text: str) -> str:
    return hashlib.sha256(text.encode()).hexdigest()


def commits() -> list:
    """The golden history as a list of commit specs: subject, body, files written, files deleted."""
    return [
        {"name": "plan", "subject": "docs: goal card, research and context", "body": "",
         "files": {".ham/lifecycle/GOAL.md": GOAL, ".ham/research/01-arithmetic.md": RESEARCH, "docs/spec.md": SPEC,
                   ".ham/context/INDEX.md": context_index(sha(SPEC))}, "delete": []},
        {"name": "red1", "subject": "test: add (red first)", "body": "",
         "files": {"tests/test_calc.py": TEST_ADD, ".ham/lifecycle/red/01-add.txt": RED}, "delete": []},
        {"name": "green1", "subject": "calc: add", "body": "", "files": {"calc.py": CODE_ADD}, "delete": []},
        {"name": "red2", "subject": "test: subtract (red first)", "body": "",
         "files": {"tests/test_calc.py": TEST_BOTH, ".ham/lifecycle/red/02-subtract.txt": RED.replace("'add'", "'subtract'")}, "delete": []},
        {"name": "green2", "subject": "calc: subtract", "body": "", "files": {"calc.py": CODE_BOTH}, "delete": []},
        {"name": "deliver", "subject": "docs: hand over calc with build and test notes", "body": DELIVER_BODY,
         "files": {"AGENTS.md": AGENTS, "README.md": README, ".ham/lifecycle/LOG.md": LOG}, "delete": []},
    ]


BASE_CODE = "def legacy(a):\n    return a\n"
BASE_TEST = "import unittest\nimport calc\n\n\nclass L(unittest.TestCase):\n    def test_legacy(self):\n        self.assertEqual(calc.legacy(1), 1)\n        self.assertEqual(calc.legacy(2), 2)\n"


def on_existing_project(specs, name_the_base=True):
    """An existing project: a base commit that already holds code, tests and an old research note, then the run's commits on top of it.
    The goal card names the base commit ({BASE} becomes its hash) unless name_the_base is false."""
    specs.insert(0, {"name": "base", "subject": "Initial commit", "body": "", "files": {"calc.py": BASE_CODE, "tests/test_legacy.py": BASE_TEST,
                                                                                         ".ham/research/00-old.md": "# Old notes\n"}, "delete": []})
    spec_of(specs, "green1")["files"]["calc.py"] = BASE_CODE + "\n\n" + CODE_ADD
    spec_of(specs, "green2")["files"]["calc.py"] = BASE_CODE + "\n\n" + CODE_BOTH
    if name_the_base:
        goal = spec_of(specs, "plan")["files"][".ham/lifecycle/GOAL.md"]
        spec_of(specs, "plan")["files"][".ham/lifecycle/GOAL.md"] = goal.replace("Nothing else changes.", "Nothing else changes. Base commit: {BASE}")


def spec_of(specs: list, name: str) -> dict:
    return next(s for s in specs if s["name"] == name)


# Each variant changes the golden history in one way and names the check it must turn red. A mutation edits the commit specs
# (and may return a function that edits the working tree after the last commit).
def m_no_research(specs):
    del spec_of(specs, "plan")["files"][".ham/research/01-arithmetic.md"]


def m_research_without_sources(specs):
    spec_of(specs, "plan")["files"][".ham/research/01-arithmetic.md"] = "# Notes\n\nWe looked around and addition works.\n"


def m_research_without_labels(specs):
    spec_of(specs, "plan")["files"][".ham/research/01-arithmetic.md"] = RESEARCH.replace("verified", "checked").replace("unknown", "maybe")


def m_no_context_index(specs):
    del spec_of(specs, "plan")["files"][".ham/context/INDEX.md"]


def m_context_row_without_licence(specs):
    spec_of(specs, "plan")["files"][".ham/context/INDEX.md"] = context_index(sha(SPEC), licence="")


def m_context_hash_is_stale(specs):
    spec_of(specs, "plan")["files"][".ham/context/INDEX.md"] = context_index(sha("some other text\n"))


def m_context_item_too_big(specs):
    spec_of(specs, "plan")["files"]["docs/spec.md"] = "x" * (300 * 1024)
    spec_of(specs, "plan")["files"][".ham/context/INDEX.md"] = context_index(sha("x" * (300 * 1024)))


def m_no_red_records(specs):
    for name in ("red1", "red2"):
        spec_of(specs, name)["files"] = {k: v for k, v in spec_of(specs, name)["files"].items() if ".ham/lifecycle/red" not in k}


def m_red_record_with_exit_zero(specs):
    spec_of(specs, "red1")["files"][".ham/lifecycle/red/01-add.txt"] = RED.replace("exit: 1", "exit: 0")


def m_red_record_without_failure(specs):
    spec_of(specs, "red1")["files"][".ham/lifecycle/red/01-add.txt"] = "command: python3 -m unittest\nexit: 1\n"


def m_code_before_tests(specs):
    i, j = specs.index(spec_of(specs, "red1")), specs.index(spec_of(specs, "green1"))
    specs[i], specs[j] = specs[j], specs[i]


def m_test_file_deleted(specs):
    specs.insert(-1, {"name": "tidy", "subject": "chore: tidy up", "body": "", "files": {}, "delete": ["tests/test_calc.py"]})


def m_skip_added(specs):
    specs.insert(-1, {"name": "wip", "subject": "test: park a case", "body": "",
                      "files": {"tests/test_calc.py": TEST_BOTH.replace("    def test_subtract", "    @unittest.skip('later')\n    def test_subtract")},
                      "delete": []})


def m_assertion_dropped(specs):
    specs.insert(-1, {"name": "shrink", "subject": "test: simplify", "body": "",
                      "files": {"tests/test_calc.py": TEST_BOTH.replace("        self.assertEqual(calc.subtract(5, 3), 2)\n", "        pass\n")},
                      "delete": []})


def m_no_log(specs):
    del spec_of(specs, "deliver")["files"][".ham/lifecycle/LOG.md"]


def m_log_ends_red(specs):
    spec_of(specs, "deliver")["files"][".ham/lifecycle/LOG.md"] = LOG.replace("exit 0", "exit 1").replace("pass 2 fail 0", "pass 1 fail 1")


NEW_FAILURES_ROW = "exit 0 | pass 6285 fail 161 | NEW-FAILURES PASS (every failure is also a failure of the base commit)"


def log_ends_with(row: str):
    """The golden log with its one RUN row replaced by `row` (the part after the command)."""
    def mutation(specs):
        spec_of(specs, "deliver")["files"][".ham/lifecycle/LOG.md"] = LOG.replace("exit 0 | pass 2 fail 0 | OK printed, no warnings", row)
    return mutation


m_log_ends_with_failures_and_pass_in_its_note = log_ends_with(NEW_FAILURES_ROW)
m_log_ends_with_failures_and_a_note_that_denies_pass = log_ends_with("exit 0 | pass 2 fail 1 | NOT PASS: one failure left")
m_log_ends_with_failures_and_pass_beside_failed = log_ends_with("exit 0 | pass 2 fail 1 | PASS expected, 1 FAILED observed")
m_log_ends_with_a_non_zero_exit_and_pass_in_its_note = log_ends_with("exit 1 | pass 2 fail 0 | PASS")


def m_log_older_than_last_code(specs):
    log = spec_of(specs, "deliver")["files"].pop(".ham/lifecycle/LOG.md")
    spec_of(specs, "red2")["files"][".ham/lifecycle/LOG.md"] = log


def m_log_line_malformed(specs):
    spec_of(specs, "deliver")["files"][".ham/lifecycle/LOG.md"] = LOG + "RUN | pytest\n"


def m_goal_without_verify(specs):
    spec_of(specs, "plan")["files"][".ham/lifecycle/GOAL.md"] = GOAL.replace("VERIFY:", "NOTES:")


def m_goal_without_numbered_checks(specs):
    spec_of(specs, "plan")["files"][".ham/lifecycle/GOAL.md"] = re.sub(r"(?m)^\d\. .*\n", "", GOAL)


def m_goal_check_without_command(specs):
    spec_of(specs, "plan")["files"][".ham/lifecycle/GOAL.md"] = GOAL.replace("`python3 -m unittest discover -s tests` exits 0", "the tests pass")


def m_goal_labels_out_of_order(specs):
    lines = GOAL.split("\n")
    lines[0], lines[-2] = lines[-2], lines[0]
    spec_of(specs, "plan")["files"][".ham/lifecycle/GOAL.md"] = "\n".join(lines)


def m_blocked_without_missing_item(specs):
    spec_of(specs, "deliver")["files"][".ham/lifecycle/LOG.md"] = LOG.replace("exit 0", "exit 1").replace("pass 2 fail 0", "pass 1 fail 1") + "BLOCKED:\n"


def m_no_project_agents_md(specs):
    del spec_of(specs, "deliver")["files"]["AGENTS.md"]


def m_agents_md_without_test_command(specs):
    spec_of(specs, "deliver")["files"]["AGENTS.md"] = "# Calc\n\nA small calculator library.\n"


def m_no_readme(specs):
    del spec_of(specs, "deliver")["files"]["README.md"]


def m_last_commit_has_no_body(specs):
    spec_of(specs, "deliver")["body"] = ""


def m_last_commit_subject_too_long(specs):
    spec_of(specs, "deliver")["subject"] = "docs: " + "very long subject " * 6


def m_research_after_code(specs):
    plan = spec_of(specs, "plan")
    moved = {k: plan["files"].pop(k) for k in (".ham/research/01-arithmetic.md", ".ham/context/INDEX.md")}
    specs.insert(-1, {"name": "late", "subject": "docs: research notes", "body": "", "files": moved, "delete": []})


def m_secret_in_a_record(specs):
    key = "AK" + "IA" + "IOSFODNN7" + "EXAMPLE"
    spec_of(specs, "plan")["files"][".ham/research/01-arithmetic.md"] = RESEARCH + "- source-reported: key %s was in the sample\n" % key


# Connection URLs a README shows as examples: the password part is a placeholder, not a password.
PLACEHOLDER_URLS = [
    "amqp://app:<pw>@host",
    "postgres://user:<password>@db:5432/app",
    "mysql://root:${MYSQL_PWD}@db/app",
    "redis://default:$REDIS_PASSWORD@cache:6379",
    "amqp://app:%(password)s@host",
    "amqp://app:%s@host",
    "https://user:{password}@example.test/x",
    "amqp://app:{{password}}@host",
    "ftp://u:*****@host",
]


def readme_with(url: str):
    def mutation(specs):
        spec_of(specs, "deliver")["files"]["README.md"] = README + "\n## Configure\n`%s`\n" % url
    return mutation


def m_readme_documents_placeholder_urls(specs):
    spec_of(specs, "deliver")["files"]["README.md"] = README + "\n## Configure\n" + "\n".join("`%s`" % u for u in PLACEHOLDER_URLS) + "\n"


def m_password_in_a_readme_url(specs):
    spec_of(specs, "deliver")["files"]["README.md"] = README + "\n## Configure\n`postgres://app:" + "hunter" + "2@db.example.test/x`\n"


def m_password_in_a_record_url(specs):
    spec_of(specs, "plan")["files"][".ham/research/01-arithmetic.md"] = RESEARCH + "- source-reported: amqp://app:" + "s3cr3t" + "pw@broker was used\n"


def m_uncommitted_code_change(specs):
    def post(repo):
        (repo / "calc.py").write_text(CODE_BOTH + "\n# edited after the last commit\n")
    return post


# A run that used the harness's research tool. `ham-research index --md` prints the table the run starts its
# .ham/context/INDEX.md from: the file below is that table for two stored items, a page whose address holds a pipe and a repository at a
# commit. crates/agentd/tests/direct_research_place.rs pins the file to the renderer's own output, so a change of the producer fails there
# before it reaches this check. The notes cite a stored page by its item id and a repository by its commit.
INDEX_GOLDEN = REPO / "fixtures" / "research" / "index-md.golden"
RESEARCH_STORED = RESEARCH.replace(
    "- calc.py (this repository), read 2026-10-16\n",
    "- calc.py (this repository), read 2026-10-16\n"
    "- https://docs.example.org/a%7Cb (stored by ham-research as item i-0001), read 2026-10-16\n"
    "- https://github.com/acme/widgets at 666df7c10035f7e26f27ec214fe5ae3173435f34 (stored as item i-0002), read 2026-10-16\n")


def producer_index() -> str:
    return INDEX_GOLDEN.read_text(encoding="utf-8")


def m_records_of_the_research_tool(specs):
    files = spec_of(specs, "plan")["files"]
    files[".ham/research/01-arithmetic.md"] = RESEARCH_STORED
    files[".ham/context/INDEX.md"] = producer_index()


def m_research_tool_index_with_cut_hashes(specs):
    files = spec_of(specs, "plan")["files"]
    files[".ham/research/01-arithmetic.md"] = RESEARCH_STORED
    files[".ham/context/INDEX.md"] = re.sub(r"\| ([0-9a-f]{12})[0-9a-f]{52} \|", r"| \1 |", producer_index())


# What the PRF1 bar leg (12 runs of GPT-6.1 Sol on the deployed stage, 2026-10-17) showed about the checks:
# - a model copies the runner's last line into the log as it is: indented, empty when the command printed nothing, or glued to the bar
#   (`|11 passed`). The line is a faithful record (command, exit code, counts); only the note is loose (LC-5);
# - a model commits its contract-check script with the red test as the test it is (`scripts/check-contract.sh`, LC-3);
# - the harness ends a run with its own `harness: turn N` commit for what the model left uncommitted; it is no delivery commit of the model (LC-7).
LOG_NOTE_SLIPS = (
    "# Log\n\n"
    "RUN | python3 -m compileall -q calc.py | exit 0 | pass unknown fail unknown |\n"
    "RUN | python3 -m compileall -q calc.py | exit 0 | pass unknown fail unknown | \n"
    "RUN | npm run build | exit 0 | pass unknown fail unknown |      Tests  4 passed (4)\n"
    "RUN | pytest -q | exit 0 | pass unknown fail unknown |2 passed in 0.8s\n"
    "RUN | python3 -m unittest discover -s tests | exit 0 | pass 2 fail 0 | OK\n"
)


def m_log_notes_that_are_empty_indented_or_glued_to_the_bar(specs):
    spec_of(specs, "deliver")["files"][".ham/lifecycle/LOG.md"] = LOG_NOTE_SLIPS


def m_run_line_without_the_counts(specs):
    spec_of(specs, "deliver")["files"][".ham/lifecycle/LOG.md"] = (
        LOG + "RUN | ./mvnw -B test | exit 0 | [INFO] Tests run: 5, Failures: 0 | unit tier\n" + LOG.split("\n", 2)[2])


def m_contract_check_script_with_the_first_test(specs):
    spec_of(specs, "red1")["files"]["scripts/check-contract.sh"] = "#!/bin/sh\ntest -f calc.py\n"


def m_production_script_with_the_first_test(specs):
    spec_of(specs, "red1")["files"]["deploy.sh"] = "#!/bin/sh\necho deploy\n"


# Pack 1.0.9: a script that runs the tests of the project is a test. A prompt asks for one ("add a script scripts/test.sh that runs them"), a model
# commits it with its first test, and because the name did not start with test-, check-, check_, verify- or verify_ the script counted as production code: a Mac run of
# 2026-10-20 ended with "FAIL ... LC-3 ... code was committed (276a727) before the first test-only commit (250393b)" over a finished, green project. A script whose name,
# without its extension (.sh, .bash or .py), is exactly test, tests, check, verify, run-tests or run_tests is a test. Every other script is still code.
RUNNER_STEMS = ["test", "tests", "check", "verify", "run-tests", "run_tests"]
RUNNER_EXTENSIONS = [".sh", ".bash", ".py"]
RUNNER = "#!/bin/sh\npython3 -m unittest discover -s tests\n"
SCRIPTS_THAT_ARE_CODE = ["scripts/build.sh", "scripts/deploy.sh", "scripts/testing.sh", "scripts/run-test.sh", "scripts/tester.py", "scripts/test.rb",
                         "scripts/test.old.sh", "scripts/start.sh"]


def extra_commit(name: str, subject: str, files: dict) -> dict:
    return {"name": name, "subject": subject, "body": "", "files": files, "delete": []}


def m_test_runner_script_with_the_first_test(specs):
    # The history of the Mac run: scripts/test.sh in the same commit as the first test and its red record.
    spec_of(specs, "red1")["files"]["scripts/test.sh"] = RUNNER


def m_test_runner_script_and_ignore_file_before_the_first_test(specs):
    specs.insert(1, extra_commit("runner", "chore: test runner and ignore file", {"scripts/test.sh": RUNNER, ".gitignore": "__pycache__/\n"}))


def m_every_test_runner_name_before_the_first_test(specs):
    files = {"scripts/%s%s" % (stem, ext): RUNNER for stem in RUNNER_STEMS for ext in RUNNER_EXTENSIONS}
    files.update({"tools/Run_Tests.PY": RUNNER, "ci/VERIFY.SH": RUNNER, "ci/Check.Bash": RUNNER, "Tests.sh": RUNNER})
    specs.insert(1, extra_commit("runners", "chore: every spelling of a test runner", files))


def m_script_committed_before_the_first_test(path: str):
    def mutation(specs):
        specs.insert(1, extra_commit("script", "chore: " + path, {path: "#!/bin/sh\necho hello\n"}))
    return mutation


def m_harness_commit_at_the_end(specs):
    specs.append({"name": "harness", "subject": "harness: turn 1", "body": "", "files": {}, "delete": []})


def m_delivery_commit_without_a_body_then_the_harness_commit(specs):
    spec_of(specs, "deliver")["body"] = ""
    specs.append({"name": "harness", "subject": "harness: turn 1", "body": "", "files": {}, "delete": []})


# A project that follows DOX has an AGENTS.md in its tests folder. That file is a document about the folder and no test: the
# words xfail and assertion in it are no skip and no assertion, and deleting it is no deleted test. A model writes both words there ("never mark a test
# xfail", "every assertion stays"). Any other document under a test folder is still a test.
TESTS_NOTES = "# tests\n\n## Local Contracts\n\nNever mark a test xfail or skip it.\nEvery assertion stays.\n"
TESTS_NOTES_ASSERT_ONLY = "# tests\n\n## Local Contracts\n\nEvery assertion stays.\n"
TESTS_NOTES_REWORDED = TESTS_NOTES_ASSERT_ONLY.replace("Every assertion stays.", "Every check stays.")


def notes_commit(name: str, files: dict, delete=()) -> dict:
    return {"name": name, "subject": "docs: " + name.replace("-", " "), "body": "", "files": files, "delete": list(delete)}


def notes_history(path: str, *versions: str, drop: bool = False):
    """A mutation: before the delivery commit, commit `path` with each text of `versions` in turn, then delete it when `drop`."""
    def mutation(specs):
        for number, text in enumerate(versions, start=1):
            specs.insert(-1, notes_commit("notes-%d" % number, {path: text}))
        if drop:
            specs.insert(-1, notes_commit("notes-dropped", {}, delete=[path]))
    return mutation


m_tests_agents_md_with_xfail_and_assert_words = notes_history("tests/AGENTS.md", TESTS_NOTES)
m_tests_agents_md_rewritten_without_the_assert_word = notes_history("tests/AGENTS.md", TESTS_NOTES_ASSERT_ONLY, TESTS_NOTES_REWORDED)
m_tests_agents_md_deleted = notes_history("tests/AGENTS.md", TESTS_NOTES, drop=True)

# A file named AGENTS.md in each folder name the check takes for a test folder, also one level down and with other capitals in the name.
AGENTS_MD_IN_TEST_FOLDERS = ["tests/AGENTS.md", "tests/sub/AGENTS.md", "test/AGENTS.md", "__tests__/AGENTS.md", "spec/AGENTS.md", "specs/AGENTS.md",
                             "e2e/agents.md", "fixtures/sample/AGENTS.md", "testdata/Agents.MD"]


def m_agents_md_in_every_kind_of_test_folder(specs):
    specs.insert(-1, notes_commit("test-folder-notes", {path: TESTS_NOTES for path in AGENTS_MD_IN_TEST_FOLDERS}))


def m_skip_added_beside_a_tests_agents_md(specs):
    skipped = TEST_BOTH.replace("    def test_subtract", "    @unittest.skip('later')\n    def test_subtract")
    specs.insert(-1, notes_commit("park-a-case", {"tests/AGENTS.md": TESTS_NOTES, "tests/test_calc.py": skipped}))


m_document_under_tests_deleted = notes_history("tests/golden/expected.md", "expected output\n", drop=True)

# Pack 1.0.8 (DOX): the delivery skill has the model write the root AGENTS.md from the starter `references/dox-root.md`. The starter's `# Project` paragraph holds no
# `build`, no `test` and no backtick, so a root that is only the starter must fail LC-7 (the model has to write the project part), and the same file with the
# paragraph filled in must pass it. A DOX tree (a root with an index, a child doc per folder, the notice) must leave the eight checks as they were.
PROJECT_PART = ("A small calculator library.\n\nBuild: `python3 -m compileall calc.py`. Test: `python3 -m unittest discover -s tests`.\n\n"
                "Layout: calc.py, tests/, api/ and docs/.\n")
CHILD_DOC = ("# %s\n\n## Purpose\n\n%s\n\n## Ownership\n\nOwns its files; the parent owns the layout.\n\n## Local Contracts\n\n"
             "Callers use calc.add and calc.subtract.\n\n## Work Guidance\n\n## Verification\n\n## Child DOX Index\n\nNone.\n")


def starter_text() -> str:
    return STARTER.read_text(encoding="utf-8")


def filled_starter(index: str = "") -> str:
    """The starter with its `# Project` paragraph replaced; with `index`, its `## Child DOX Index` sentence replaced as well."""
    text = re.sub(r"(?s)(\n# Project\n\n).*?(\n## Child DOX Index\n)", lambda m: m.group(1) + PROJECT_PART + m.group(2), starter_text(), count=1)
    if index:
        text = re.sub(r"(?s)(\n## Child DOX Index\n\n).*$", lambda m: m.group(1) + index, text, count=1)
    return text


def m_root_is_only_the_starter(specs):
    spec_of(specs, "deliver")["files"]["AGENTS.md"] = starter_text()


def m_root_is_the_starter_with_the_project_part(specs):
    spec_of(specs, "deliver")["files"]["AGENTS.md"] = filled_starter()


def m_a_dox_tree(specs):
    files = spec_of(specs, "deliver")["files"]
    files["AGENTS.md"] = filled_starter("- `api/AGENTS.md`: the command-line entry.\n- `docs/AGENTS.md`: the specification.\n")
    files["api/AGENTS.md"] = CHILD_DOC % ("api", "The command-line entry of the calculator.")
    files["docs/AGENTS.md"] = CHILD_DOC % ("docs", "The specification of the calculator.")
    files["THIRD-PARTY-NOTICES.md"] = (STARTER.parent / "dox-notice.md").read_text(encoding="utf-8")


# Pack 1.0.9: the user can ask for no AGENTS.md at all ("Do not write AGENTS.md docs."). Step 1 of the contract has the goal card carry the request and step 7
# allows the exception, so LC-7 does the same: when the card holds an explicit prohibition of AGENTS.md it asks for the README and the last commit message only, and it fails when the run wrote
# an AGENTS.md anywhere (the user's ban was broken). "DOX off", "no DOX", "no framework" and "no docs" alone are no such prohibition: the root file is still required. A prohibition that is not in
# the goal card changes nothing.
BANS = [
    "Do not write AGENTS.md docs.",
    "do not write AGENTS.md",
    "DO NOT WRITE AN AGENTS.MD FILE.",
    "Don't create any AGENTS.md.",
    "Never add a root AGENTS.md.",
    "No AGENTS.md.",
    "Write no AGENTS.md docs.",
    "Build it without AGENTS.md.",
    "Done without an AGENTS.md file.",
    "User requested no AGENTS.md; the delivery step is set aside.",
    "The user asked for no AGENTS.md.",
    "Empty repository; no AGENTS.md (the request overrides the delivery step).",
    'The request says "no AGENTS.md".',
    "- no agents.md\n",
]
NOT_BANS = [
    "DOX off.",
    "No DOX.",
    "No framework.",
    "No docs.",
    "Write no docs.",
    "Do not write DOX docs.",
    "Do not write child AGENTS.md files.",
    "Write no child AGENTS.md.",
    "The repository has no AGENTS.md yet.",
    "There is no AGENTS.md at the Base commit.",
    "Leave the user's AGENTS.md as it is.",
    "AGENTS.md must not change.",
    "Do not touch AGENTS.md.",
    "Do not write the README or AGENTS to the docs folder.",
    "test ! -e AGENTS.md",
    "",
]


def card_says(*phrases):
    """A mutation: the ACHIEVABLE line of the goal card gets these sentences."""
    def edit(specs):
        goal = spec_of(specs, "plan")["files"][".ham/lifecycle/GOAL.md"]
        spec_of(specs, "plan")["files"][".ham/lifecycle/GOAL.md"] = goal.replace("Nothing else changes.", "Nothing else changes. " + " ".join(phrases))
    return edit


def card_says_and_the_run_writes(phrase: str, root: bool = False, child: bool = False, readme: bool = True):
    """A mutation: the card holds `phrase`; the delivery commit keeps the root AGENTS.md only when `root`, adds api/AGENTS.md when `child`, drops the README unless `readme`."""
    def mutation(specs):
        card_says(phrase)(specs)
        files = spec_of(specs, "deliver")["files"]
        if not root:
            del files["AGENTS.md"]
        if child:
            files["api/AGENTS.md"] = CHILD_DOC % ("api", "The command-line entry of the calculator.")
        if not readme:
            del files["README.md"]
    return mutation


def m_ban_in_the_readme_and_no_goal_card(specs):
    """A prohibition that is not in the goal card (there is no card) is the same as no prohibition: the root AGENTS.md is required."""
    del spec_of(specs, "plan")["files"][".ham/lifecycle/GOAL.md"]
    del spec_of(specs, "deliver")["files"]["AGENTS.md"]
    spec_of(specs, "deliver")["files"]["README.md"] = README + "\nThe request said: Do not write AGENTS.md docs.\n"


def m_ban_in_the_log_not_in_the_card(specs):
    del spec_of(specs, "deliver")["files"]["AGENTS.md"]
    spec_of(specs, "deliver")["files"][".ham/lifecycle/LOG.md"] = LOG + "NOT RUN | AGENTS.md | the request said: no AGENTS.md\n"


def m_ban_and_the_users_own_agents_md_at_the_base(specs):
    """The user's own AGENTS.md is in the base commit and the run does not touch it: the run wrote none, so the ban holds."""
    on_existing_project(specs)
    spec_of(specs, "base")["files"]["AGENTS.md"] = "# Mine\n\nNo build or test commands here, on purpose.\n"
    card_says("Do not write AGENTS.md docs.")(specs)
    del spec_of(specs, "deliver")["files"]["AGENTS.md"]


def m_ban_and_the_run_rewrites_the_users_own_agents_md(specs):
    on_existing_project(specs)
    spec_of(specs, "base")["files"]["AGENTS.md"] = "# Mine\n\nNo build or test commands here, on purpose.\n"
    card_says("Do not write AGENTS.md docs.")(specs)
    spec_of(specs, "deliver")["files"]["AGENTS.md"] = AGENTS


def m_ban_and_an_agents_md_nobody_committed(specs):
    card_says("Do not write AGENTS.md docs.")(specs)
    del spec_of(specs, "deliver")["files"]["AGENTS.md"]

    def post(repo):
        (repo / "notes").mkdir()
        (repo / "notes" / "AGENTS.md").write_text("# notes\n")
    return post


# name -> (mutation, the check that must fail)
BROKEN = {
    "no research note": (m_no_research, "LC-1"),
    "research note without sources": (m_research_without_sources, "LC-1"),
    "research findings without labels": (m_research_without_labels, "LC-1"),
    "no context index": (m_no_context_index, "LC-2"),
    "context row without licence": (m_context_row_without_licence, "LC-2"),
    "context hash is stale": (m_context_hash_is_stale, "LC-2"),
    "context item over 256 KiB": (m_context_item_too_big, "LC-2"),
    "research tool index with cut hashes": (m_research_tool_index_with_cut_hashes, "LC-2"),
    "no red records": (m_no_red_records, "LC-3"),
    "red record with exit 0": (m_red_record_with_exit_zero, "LC-3"),
    "red record without a failure": (m_red_record_without_failure, "LC-3"),
    "code committed before any test": (m_code_before_tests, "LC-3"),
    "test file deleted": (m_test_file_deleted, "LC-4"),
    "skip added to a test": (m_skip_added, "LC-4"),
    "assertions dropped": (m_assertion_dropped, "LC-4"),
    "no run log": (m_no_log, "LC-5"),
    "run log ends red": (m_log_ends_red, "LC-5"),
    "run log ends with failed tests and a note that denies PASS": (m_log_ends_with_failures_and_a_note_that_denies_pass, "LC-5"),
    "run log ends with failed tests and a note with PASS beside FAILED": (m_log_ends_with_failures_and_pass_beside_failed, "LC-5"),
    "run log ends with a non-zero exit and PASS in its note": (m_log_ends_with_a_non_zero_exit_and_pass_in_its_note, "LC-5"),
    "run log older than the last code commit": (m_log_older_than_last_code, "LC-5"),
    "run log line malformed": (m_log_line_malformed, "LC-5"),
    "uncommitted code change": (m_uncommitted_code_change, "LC-5"),
    "goal card without VERIFY": (m_goal_without_verify, "LC-6"),
    "goal card without numbered checks": (m_goal_without_numbered_checks, "LC-6"),
    "goal check without a command": (m_goal_check_without_command, "LC-6"),
    "goal labels out of order": (m_goal_labels_out_of_order, "LC-6"),
    "BLOCKED without the missing item": (m_blocked_without_missing_item, "LC-6"),
    "no project AGENTS.md": (m_no_project_agents_md, "LC-7"),
    "project AGENTS.md without a test command": (m_agents_md_without_test_command, "LC-7"),
    "no README": (m_no_readme, "LC-7"),
    "last commit without a body": (m_last_commit_has_no_body, "LC-7"),
    "last commit subject too long": (m_last_commit_subject_too_long, "LC-7"),
    "research written after the code": (m_research_after_code, "LC-8"),
    "secret-shaped string in a record": (m_secret_in_a_record, "LC-8"),
    "password in a connection URL of the README": (m_password_in_a_readme_url, "LC-8"),
    "password in a connection URL of a research note": (m_password_in_a_record_url, "LC-8"),
    "run line without the pass and fail counts": (m_run_line_without_the_counts, "LC-5"),
    "production script committed with the first test": (m_production_script_with_the_first_test, "LC-3"),
    "delivery commit without a body before the harness commit": (m_delivery_commit_without_a_body_then_the_harness_commit, "LC-7"),
    "skip added to a test beside a tests AGENTS.md": (m_skip_added_beside_a_tests_agents_md, "LC-4"),
    "document under tests deleted": (m_document_under_tests_deleted, "LC-4"),
    "goal card bans AGENTS.md and the run wrote the root one": (card_says_and_the_run_writes(BANS[0], root=True), "LC-7"),
    "goal card bans AGENTS.md and the run wrote a child doc only": (card_says_and_the_run_writes(BANS[0], child=True), "LC-7"),
    "goal card bans AGENTS.md and there is no README": (card_says_and_the_run_writes(BANS[0], readme=False), "LC-7"),
    "goal card bans AGENTS.md and the run rewrote the users own": (m_ban_and_the_run_rewrites_the_users_own_agents_md, "LC-7"),
    "goal card bans AGENTS.md and an AGENTS.md was never committed": (m_ban_and_an_agents_md_nobody_committed, "LC-7"),
    "goal card says DOX off, no DOX, no framework and no docs, no root AGENTS.md": (card_says_and_the_run_writes(" ".join(NOT_BANS[:6])), "LC-7"),
    "prohibition in the README and no goal card, no root AGENTS.md": (m_ban_in_the_readme_and_no_goal_card, "LC-7"),
    "prohibition in the run log and not in the goal card, no root AGENTS.md": (m_ban_in_the_log_not_in_the_card, "LC-7"),
}
for _path in SCRIPTS_THAT_ARE_CODE:
    BROKEN["%s committed before the first test" % _path] = (m_script_committed_before_the_first_test(_path), "LC-3")

# Variants that stay green: documents that show a connection URL with a placeholder where the password goes, the records a run
# leaves when it used the harness's research tool, and an AGENTS.md in a test folder (a document, no test).
HARMLESS = {
    "README with placeholder connection URLs": m_readme_documents_placeholder_urls,
    "run log notes that are empty, indented or glued to the bar": m_log_notes_that_are_empty_indented_or_glued_to_the_bar,
    "run log ends with failed tests and PASS in its note (a wrapper that compares with the failures of the base commit)": m_log_ends_with_failures_and_pass_in_its_note,
    "contract check script committed with the first test": m_contract_check_script_with_the_first_test,
    "harness commit after the delivery commit": m_harness_commit_at_the_end,
    "notes and context index written with the research tool": m_records_of_the_research_tool,
    "tests folder AGENTS.md with the words xfail and assertion": m_tests_agents_md_with_xfail_and_assert_words,
    "tests folder AGENTS.md reworded without the assert word": m_tests_agents_md_rewritten_without_the_assert_word,
    "tests folder AGENTS.md deleted": m_tests_agents_md_deleted,
    "AGENTS.md in every kind of test folder": m_agents_md_in_every_kind_of_test_folder,
    "test runner script committed with the first test (the history of the Mac run)": m_test_runner_script_with_the_first_test,
    "test runner script and ignore file committed before the first test": m_test_runner_script_and_ignore_file_before_the_first_test,
    "every spelling of a test runner committed before the first test": m_every_test_runner_name_before_the_first_test,
    "goal card bans AGENTS.md and the user's own AGENTS.md at the base is not touched": m_ban_and_the_users_own_agents_md_at_the_base,
}
for _n, _phrase in enumerate(BANS):
    HARMLESS["goal card bans AGENTS.md (form %d) and the run wrote none" % _n] = card_says_and_the_run_writes(_phrase)
for _n, _phrase in enumerate(NOT_BANS):
    if _phrase:
        HARMLESS["goal card says something else about AGENTS.md or DOX (text %d) and the run wrote the root AGENTS.md" % _n] = card_says_and_the_run_writes(_phrase, root=True)

# A long run of article words, and a long run of spaces, between the word and the file name (the pattern was rewritten for the semgrep rule regex_dos, see NoAgentsMdPatternCase).
LONG_ARTICLES = "a the root any an " * 12
HARMLESS["goal card bans AGENTS.md after a long run of article words and the run wrote none"] = card_says_and_the_run_writes("Do not write " + LONG_ARTICLES + "AGENTS.md.")
HARMLESS["goal card bans AGENTS.md after a long run of spaces and the run wrote none"] = card_says_and_the_run_writes("No" + " " * 500 + "AGENTS.md.")
BROKEN["goal card bans AGENTS.md after a long run of article words and the run wrote the root one"] = (
    card_says_and_the_run_writes("Do not write " + LONG_ARTICLES + "AGENTS.md.", root=True), "LC-7")


def git_env(home: str) -> dict:
    env = dict(os.environ)
    env.update({"HOME": home, "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull, "GIT_TERMINAL_PROMPT": "0",
                "GIT_AUTHOR_NAME": "Test", "GIT_AUTHOR_EMAIL": "test@example.invalid",
                "GIT_COMMITTER_NAME": "Test", "GIT_COMMITTER_EMAIL": "test@example.invalid"})
    return env


def git(repo: Path, env: dict, *args: str) -> str:
    done = subprocess.run(["git", "-c", "init.defaultBranch=main", "-c", "commit.gpgsign=false", *args], cwd=str(repo), env=env,  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
                          capture_output=True, text=True)
    if done.returncode != 0:
        raise AssertionError("git %s failed: %s" % (" ".join(args), done.stderr))
    return done.stdout


def build(root: Path, mutation=None) -> Path:
    """Build the golden history in root/repo (with `mutation` applied) and return the repository path."""
    home = root / "home"
    home.mkdir()
    env = git_env(str(home))
    repo = root / "repo"
    repo.mkdir()
    git(repo, env, "init", "-q")
    specs = commits()
    base_sha = ""
    post = mutation(specs) if mutation else None
    for spec in specs:
        for rel in spec["delete"]:
            git(repo, env, "rm", "-q", rel)
        for rel, text in spec["files"].items():
            path = repo / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text.replace("{BASE}", base_sha))
        git(repo, env, "add", "-A")
        message = spec["subject"] + ("\n\n" + spec["body"] if spec["body"] else "")
        git(repo, env, "commit", "-q", "--allow-empty", "-m", message)
        if spec["name"] == "base":
            base_sha = git(repo, env, "rev-parse", "HEAD").strip()
    if post:
        post(repo)
    return repo


def run(repo: Path, *extra: str) -> tuple:
    done = subprocess.run([sys.executable, "-I", "-B", str(SCRIPT), "--repo", str(repo), *extra], capture_output=True, text=True)  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool
    return done.returncode, done.stdout, done.stderr


def verdicts(out: str) -> dict:
    return {m.group(1): m.group(3) for m in re.finditer(r"(?m)^(LC-\d) (\S+) (PASS|FAIL|NA)", out)}


class SelfCheckCase(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory(prefix="lifecycle-check-")
        self.root = Path(self.tmp.name)

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def test_the_golden_repository_passes_every_check(self) -> None:
        repo = build(self.root)
        code, out, err = run(repo)
        self.assertEqual((code, err), (0, ""), out)
        self.assertEqual(verdicts(out), {"LC-%d" % n: "PASS" for n in range(1, 9)}, out)
        self.assertEqual(out.strip().split("\n")[-1], "LIFECYCLE-CHECK PASS pass=8 na=0 fail=0")

    def test_each_line_names_the_check_and_carries_a_measured_value(self) -> None:
        repo = build(self.root)
        _, out, _ = run(repo)
        lines = out.strip().split("\n")
        # The first line names the flow the checks are for.
        self.assertEqual(len(lines), 10)
        self.assertEqual(lines[0], "FLOW full (no Flow line in the last commit: the eight steps are checked)")
        names = ["research", "context", "red-first", "tests-kept", "build-test", "loop", "delivery", "order"]
        for number, (line, name) in enumerate(zip(lines[1:], names), start=1):
            self.assertRegex(line, r"^LC-%d %s PASS: \S" % (number, re.escape(name)))

    def test_the_run_changes_nothing_in_the_repository(self) -> None:
        repo = build(self.root)
        env = git_env(str(self.root / "home"))
        before = git(repo, env, "status", "--porcelain", "--ignored")
        run(repo)
        self.assertEqual(git(repo, env, "status", "--porcelain", "--ignored"), before)
        self.assertEqual(before, "")

    def test_each_broken_repository_fails_the_check_its_rule_belongs_to(self) -> None:
        for name, (mutation, check) in BROKEN.items():
            with self.subTest(variant=name), tempfile.TemporaryDirectory(prefix="lifecycle-check-", dir=str(self.root)) as tmp:
                repo = build(Path(tmp), mutation)
                code, out, err = run(repo)
                self.assertEqual(code, 1, out)
                self.assertEqual(err, "")
                self.assertEqual(verdicts(out).get(check), "FAIL", out)
                self.assertRegex(out.strip().split("\n")[-1], r"^LIFECYCLE-CHECK FAIL .*%s" % check)

    def test_a_connection_url_with_a_placeholder_for_the_password_is_not_a_finding(self) -> None:
        for name, mutation in HARMLESS.items():
            with self.subTest(variant=name), tempfile.TemporaryDirectory(prefix="lifecycle-check-", dir=str(self.root)) as tmp:
                code, out, err = run(build(Path(tmp), mutation))
                self.assertEqual((code, err), (0, ""), out)
                self.assertEqual(verdicts(out), {"LC-%d" % n: "PASS" for n in range(1, 9)}, out)

    def test_each_placeholder_form_alone_passes_and_the_same_url_with_a_value_fails(self) -> None:
        for url in PLACEHOLDER_URLS:
            with self.subTest(url=url), tempfile.TemporaryDirectory(prefix="lifecycle-check-", dir=str(self.root)) as tmp:
                code, out, _ = run(build(Path(tmp), readme_with(url)))
                self.assertEqual(verdicts(out).get("LC-8"), "PASS", out)
                self.assertEqual(code, 0, out)
        for real in ["amqp://app:" + "hunter" + "2@host", "postgres://user:" + "p4ss" + "word@db:5432/app", "redis://default:" + "abc" + "123@cache",
                     "mysql://root:" + "<" + "@db", "amqp://app:" + "pa$" + "$w0rd@host", "https://user:" + "{" + "}x@example.test/x"]:
            with self.subTest(url=real), tempfile.TemporaryDirectory(prefix="lifecycle-check-", dir=str(self.root)) as tmp:
                code, out, _ = run(build(Path(tmp), readme_with(real)))
                self.assertEqual(verdicts(out).get("LC-8"), "FAIL", out)
                self.assertEqual(code, 1, out)

    def test_a_placeholder_url_beside_a_real_password_still_fails(self) -> None:
        def both(specs):
            spec_of(specs, "deliver")["files"]["README.md"] = README + "\n`amqp://app:<pw>@host`\n`amqp://app:" + "hunter" + "2@host`\n"
        with tempfile.TemporaryDirectory(prefix="lifecycle-check-", dir=str(self.root)) as tmp:
            code, out, _ = run(build(Path(tmp), both))
            self.assertEqual((code, verdicts(out).get("LC-8")), (1, "FAIL"), out)

    def test_a_broken_variant_changes_only_what_it_should_in_the_golden_verdicts(self) -> None:
        # Control: the variants above turn exactly one rule red each, so a PASS elsewhere is real. Sample three.
        for name in ("no research note", "no run log", "no README"):
            with self.subTest(variant=name), tempfile.TemporaryDirectory(prefix="lifecycle-check-", dir=str(self.root)) as tmp:
                mutation, check = BROKEN[name]
                _, out, _ = run(build(Path(tmp), mutation))
                failing = sorted(k for k, v in verdicts(out).items() if v == "FAIL")
                self.assertEqual(failing, [check], out)

    def test_ending_blocked_with_the_missing_item_named_is_a_consistent_record(self) -> None:
        def m(specs):
            log = LOG.replace("exit 0", "exit 1").replace("pass 2 fail 0", "pass 1 fail 1")
            spec_of(specs, "deliver")["files"][".ham/lifecycle/LOG.md"] = log + "BLOCKED: need the staging API key, which this session does not have\n"
        code, out, _ = run(build(self.root, m))
        self.assertEqual(code, 0, out)
        self.assertEqual(verdicts(out)["LC-5"], "NA", out)
        self.assertEqual(verdicts(out)["LC-6"], "PASS", out)

    def test_a_run_line_may_report_a_count_as_unknown(self) -> None:
        # A linter or a build prints no pass count; the log says unknown rather than inventing a number.
        def m(specs):
            spec_of(specs, "deliver")["files"][".ham/lifecycle/LOG.md"] = LOG.replace("pass 2 fail 0", "pass unknown fail unknown")
        code, out, _ = run(build(self.root, m))
        self.assertEqual(code, 0, out)
        self.assertEqual(verdicts(out)["LC-5"], "PASS", out)

    def test_a_failed_count_above_zero_is_red_even_with_exit_code_zero(self) -> None:
        def m(specs):
            spec_of(specs, "deliver")["files"][".ham/lifecycle/LOG.md"] = LOG.replace("pass 2 fail 0", "pass 1 fail 1")
        code, out, _ = run(build(self.root, m))
        self.assertEqual(code, 1, out)
        self.assertEqual(verdicts(out)["LC-5"], "FAIL", out)

    def test_a_count_that_is_neither_a_number_nor_unknown_is_a_malformed_line(self) -> None:
        def m(specs):
            spec_of(specs, "deliver")["files"][".ham/lifecycle/LOG.md"] = LOG.replace("pass 2 fail 0", "pass many fail none")
        code, out, _ = run(build(self.root, m))
        self.assertEqual(code, 1, out)
        self.assertEqual(verdicts(out)["LC-5"], "FAIL", out)

    def test_work_on_an_existing_project_is_judged_from_the_base_commit_the_goal_card_names(self) -> None:
        # The base commit holds code, tests and an old research note; the goal card says "Base commit: <hash>".
        code, out, _ = run(build(self.root, on_existing_project))
        self.assertEqual(code, 0, out)
        self.assertEqual(verdicts(out), {"LC-%d" % n: "PASS" for n in range(1, 9)}, out)

    def test_without_a_named_base_the_whole_history_is_judged(self) -> None:
        # Strict by default: code that came before the first test counts, so the agent must name where it started.
        code, out, _ = run(build(self.root, lambda specs: on_existing_project(specs, name_the_base=False)))
        self.assertEqual(code, 1, out)
        self.assertEqual(verdicts(out)["LC-3"], "FAIL", out)

    def test_removing_the_assertions_of_the_base_tests_is_caught(self) -> None:
        def m(specs):
            on_existing_project(specs)
            specs.insert(-1, {"name": "shrink", "subject": "test: simplify", "body": "", "delete": [],
                              "files": {"tests/test_legacy.py": BASE_TEST.replace("        self.assertEqual(calc.legacy(1), 1)\n", "").replace(
                                  "        self.assertEqual(calc.legacy(2), 2)\n", "        pass\n")}})
        code, out, _ = run(build(self.root, m))
        self.assertEqual(code, 1, out)
        self.assertEqual(verdicts(out)["LC-4"], "FAIL", out)

    def test_deleting_a_base_test_file_is_caught(self) -> None:
        def m(specs):
            on_existing_project(specs)
            specs.insert(-1, {"name": "tidy", "subject": "chore: tidy up", "body": "", "files": {}, "delete": ["tests/test_legacy.py"]})
        code, out, _ = run(build(self.root, m))
        self.assertEqual(code, 1, out)
        self.assertEqual(verdicts(out)["LC-4"], "FAIL", out)

    def test_the_base_option_names_the_commit_the_work_started_from(self) -> None:
        repo = build(self.root, lambda specs: on_existing_project(specs, name_the_base=False))
        env = git_env(str(self.root / "home"))
        first = git(repo, env, "rev-list", "--max-parents=0", "HEAD").strip()
        code, out, err = run(repo, "--base", first)
        self.assertEqual((code, err), (0, ""), out)
        self.assertEqual(verdicts(out), {"LC-%d" % n: "PASS" for n in range(1, 9)}, out)

    def test_a_base_that_is_not_a_commit_exits_2(self) -> None:
        repo = build(self.root)
        code, out, _ = run(repo, "--base", "no-such-ref")
        self.assertEqual(code, 2)
        self.assertIn("LIFECYCLE-CHECK ERROR", out)

    def test_a_base_commit_in_the_goal_card_that_is_not_in_the_repository_exits_2(self) -> None:
        def m(specs):
            goal = spec_of(specs, "plan")["files"][".ham/lifecycle/GOAL.md"]
            spec_of(specs, "plan")["files"][".ham/lifecycle/GOAL.md"] = goal.replace("Nothing else changes.", "Nothing else changes. Base commit: 0123456789abcdef0123456789abcdef01234567")
        code, out, _ = run(build(self.root, m))
        self.assertEqual(code, 2)
        self.assertIn("LIFECYCLE-CHECK ERROR", out)

    def test_a_task_with_no_code_marks_the_code_checks_not_applicable(self) -> None:
        def m(specs):
            keep = [s for s in specs if s["name"] in ("plan", "deliver")]
            del specs[:]
            specs.extend(keep)
            spec_of(specs, "deliver")["files"][".ham/lifecycle/LOG.md"] = "# Log\n\nNOT RUN | build and tests | this task changes documents only\n"
        code, out, _ = run(build(self.root, m))
        self.assertEqual(code, 0, out)
        got = verdicts(out)
        self.assertEqual((got["LC-3"], got["LC-4"], got["LC-8"]), ("NA", "NA", "NA"), out)
        self.assertEqual(got["LC-5"], "PASS", out)

    def test_an_empty_repository_fails_without_a_traceback(self) -> None:
        home = self.root / "home"
        home.mkdir()
        repo = self.root / "empty"
        repo.mkdir()
        git(repo, git_env(str(home)), "init", "-q")
        code, out, err = run(repo)
        self.assertEqual(code, 1, out)
        self.assertNotIn("Traceback", out + err)
        self.assertEqual(sorted(verdicts(out)), ["LC-%d" % n for n in range(1, 9)])

    def test_a_directory_that_is_not_a_git_repository_exits_2(self) -> None:
        code, out, err = run(self.root)
        self.assertEqual(code, 2)
        self.assertIn("LIFECYCLE-CHECK ERROR", out)
        self.assertNotIn("Traceback", out + err)
        code, out, _ = run(self.root / "missing")
        self.assertEqual(code, 2)

    def test_the_repo_option_defaults_to_the_current_directory(self) -> None:
        repo = build(self.root)
        done = subprocess.run([sys.executable, "-I", "-B", str(SCRIPT)], cwd=str(repo), capture_output=True, text=True)  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool
        self.assertEqual(done.returncode, 0, done.stdout)

    def test_a_root_that_is_only_the_starter_fails_lc_7(self) -> None:
        self.assertTrue(STARTER.is_file(), "the pack ships the DOX starter (references/dox-root.md of lifecycle-delivery)")
        code, out, err = run(build(self.root, m_root_is_only_the_starter))
        self.assertEqual((code, err), (1, ""), out)
        self.assertEqual(verdicts(out).get("LC-7"), "FAIL", out)
        self.assertIn("does not give a build command and a test command", out)

    def test_the_starter_with_a_filled_project_paragraph_passes_lc_7(self) -> None:
        self.assertTrue(STARTER.is_file(), "the pack ships the DOX starter (references/dox-root.md of lifecycle-delivery)")
        code, out, err = run(build(self.root, m_root_is_the_starter_with_the_project_part))
        self.assertEqual((code, err), (0, ""), out)
        self.assertEqual(verdicts(out).get("LC-7"), "PASS", out)

    def test_a_dox_tree_leaves_the_eight_checks_at_pass(self) -> None:
        self.assertTrue(STARTER.is_file(), "the pack ships the DOX starter (references/dox-root.md of lifecycle-delivery)")
        code, out, err = run(build(self.root, m_a_dox_tree))
        self.assertEqual((code, err), (0, ""), out)
        self.assertEqual(verdicts(out), {"LC-%d" % n: "PASS" for n in range(1, 9)}, out)
        self.assertEqual(out.strip().split("\n")[-1], "LIFECYCLE-CHECK PASS pass=8 na=0 fail=0")


def script_module():
    """The script as a module, for its path classifiers alone (the rest of this file runs it as a program). It leaves no bytecode beside the pack file."""
    spec = importlib.util.spec_from_file_location("lifecycle_check_under_test", str(SCRIPT))
    module = importlib.util.module_from_spec(spec)
    keep, sys.dont_write_bytecode = sys.dont_write_bytecode, True
    try:
        spec.loader.exec_module(module)
    finally:
        sys.dont_write_bytecode = keep
    return module


def lc4_line(out: str) -> str:
    match = re.search(r"(?m)^LC-4 tests-kept .*$", out)
    return match.group(0) if match else ""


class AgentsMdIsNoTestCase(unittest.TestCase):
    """Package of the DOX work (pack 1.0.8): a file named AGENTS.md is a document about its folder and never a test, so LC-4 does not read it.

    A DOX project has an AGENTS.md in its tests folder, and a model writes sentences there such as "never mark a test xfail" and "every assertion
    stays". Read as a test file, the first counted as a skip added and the second moved the assertion count, and deleting the file counted as a deleted
    test. Each case also runs the same history on another document of that folder (`tests/NOTES.md`), which still is a test: the control that shows the
    case could fail. The Rust scorer follows the same rule.
    """

    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory(prefix="lifecycle-check-")
        self.root = Path(self.tmp.name)
        self.made = 0

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def check(self, mutation) -> tuple:
        """(exit code, output) of the script on the golden repository with `mutation` applied, each call in a folder of its own."""
        self.made += 1
        folder = self.root / ("case%d" % self.made)
        folder.mkdir()
        code, out, err = run(build(folder, mutation))
        self.assertEqual(err, "", out)
        return code, out

    def test_a_tests_folder_agents_md_with_the_word_xfail_does_not_fail_lc_4(self) -> None:
        code, out = self.check(m_tests_agents_md_with_xfail_and_assert_words)
        self.assertEqual(code, 0, out)
        self.assertEqual(lc4_line(out), "LC-4 tests-kept PASS: 2 assertion(s) now; no test file deleted; no skip added", out)
        self.assertEqual(verdicts(out), {"LC-%d" % n: "PASS" for n in range(1, 9)}, out)
        code, out = self.check(notes_history("tests/NOTES.md", TESTS_NOTES))  # the trigger: the same two lines in another document of the folder
        self.assertEqual(code, 1, out)
        self.assertIn("skip or xfail added: tests/NOTES.md", lc4_line(out))

    def test_removing_an_assert_word_from_a_tests_folder_agents_md_does_not_lower_the_assertion_count(self) -> None:
        code, out = self.check(m_tests_agents_md_rewritten_without_the_assert_word)
        self.assertEqual(code, 0, out)
        self.assertEqual(lc4_line(out), "LC-4 tests-kept PASS: 2 assertion(s) now; no test file deleted; no skip added", out)
        code, out = self.check(notes_history("tests/NOTES.md", TESTS_NOTES_ASSERT_ONLY, TESTS_NOTES_REWORDED))
        self.assertEqual(code, 1, out)
        self.assertIn("assertions fell from 3 to 2", lc4_line(out))

    def test_deleting_a_tests_folder_agents_md_is_not_a_deleted_test(self) -> None:
        code, out = self.check(m_tests_agents_md_deleted)
        self.assertEqual(code, 0, out)
        self.assertEqual(verdicts(out)["LC-4"], "PASS", out)
        code, out = self.check(notes_history("tests/NOTES.md", TESTS_NOTES_ASSERT_ONLY, drop=True))
        self.assertEqual(code, 1, out)
        self.assertIn("test file deleted: tests/NOTES.md", lc4_line(out))

    def test_a_real_skip_in_a_test_file_is_still_found_beside_a_tests_agents_md(self) -> None:
        code, out = self.check(m_skip_added_beside_a_tests_agents_md)
        self.assertEqual(code, 1, out)
        self.assertEqual(verdicts(out)["LC-4"], "FAIL", out)
        self.assertIn("skip or xfail added:", lc4_line(out))
        self.assertIn("tests/test_calc.py", lc4_line(out))

    def test_a_skip_beside_a_tests_agents_md_is_blamed_on_the_test_file_only(self) -> None:
        code, out = self.check(m_skip_added_beside_a_tests_agents_md)
        self.assertEqual(code, 1, out)
        self.assertNotIn("tests/AGENTS.md", lc4_line(out))

    def test_other_documents_under_tests_still_count_as_tests(self) -> None:
        code, out = self.check(m_document_under_tests_deleted)
        self.assertEqual(code, 1, out)
        self.assertIn("test file deleted: tests/golden/expected.md", lc4_line(out))

    def test_an_agents_md_is_no_test_in_any_test_folder_or_spelling(self) -> None:
        code, out = self.check(m_agents_md_in_every_kind_of_test_folder)
        self.assertEqual(code, 0, out)
        self.assertEqual(verdicts(out), {"LC-%d" % n: "PASS" for n in range(1, 9)}, out)

    def test_the_path_classifier_says_the_same(self) -> None:
        module = script_module()
        for path in ["AGENTS.md", "Tests/AGENTS.md"] + AGENTS_MD_IN_TEST_FOLDERS:
            with self.subTest(path=path):
                self.assertFalse(module.is_test(path))
                self.assertTrue(module.is_doc(path))
                self.assertFalse(module.is_code(path))
        for path in ["tests/test_calc.py", "tests/expected.md", "tests/golden/expected.md", "tests/README.md", "tests/AGENTS.md.bak", "tests/MY_AGENTS.md",
                     "tests/test_agents.md", "e2e/run.sh", "calc_test.go", "src/app.spec.js", "conftest.py"]:
            with self.subTest(path=path):
                self.assertTrue(module.is_test(path))


class CaseInAFolder(unittest.TestCase):
    """Cases that build the golden repository with a mutation and read the script's verdict, each in a folder of its own."""

    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory(prefix="lifecycle-check-")
        self.root = Path(self.tmp.name)
        self.made = 0

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def check(self, mutation) -> tuple:
        """(exit code, output) of the script on the golden repository with `mutation` applied."""
        self.made += 1
        folder = self.root / ("case%d" % self.made)
        folder.mkdir()
        code, out, err = run(build(folder, mutation))
        self.assertEqual(err, "", out)
        return code, out


def line_of(out: str, check: str) -> str:
    match = re.search(r"(?m)^%s .*$" % re.escape(check), out)
    return match.group(0) if match else ""


class TestRunnerScriptCase(CaseInAFolder):
    """Package DOX-FIX-B (pack 1.0.9), LC-3: a script that runs the tests is a test. The Mac run of 2026-10-20 committed the `scripts/test.sh` its
    prompt asked for together with its first test and read, in its own closing message, "FAIL ... LC-3 ... code was committed (276a727) before the first test-only commit (250393b)".
    The script is a test when its name without the extension (.sh, .bash or .py) is exactly test, tests, check, verify, run-tests or run_tests; any other script is still code."""

    def test_the_history_of_the_mac_run_passes_lc_3(self) -> None:
        code, out = self.check(m_test_runner_script_with_the_first_test)
        self.assertEqual(code, 0, out)
        self.assertEqual(verdicts(out), {"LC-%d" % n: "PASS" for n in range(1, 9)}, out)
        self.assertRegex(line_of(out, "LC-3"), r"^LC-3 red-first PASS: 2 red record\(s\); test-only commit [0-9a-f]{7} before first code commit [0-9a-f]{7}; 0 mixed commit\(s\)$")

    def test_the_same_history_with_a_production_script_instead_still_fails_with_the_sentence_the_mac_run_showed(self) -> None:
        code, out = self.check(m_production_script_with_the_first_test)  # the trigger: deploy.sh where test.sh stood
        self.assertEqual(code, 1, out)
        self.assertRegex(line_of(out, "LC-3"), r"^LC-3 red-first FAIL: code was committed \([0-9a-f]{7}\) before the first test-only commit \([0-9a-f]{7}\)")

    def test_a_runner_script_and_an_ignore_file_in_a_commit_before_the_first_test_pass_lc_3(self) -> None:
        code, out = self.check(m_test_runner_script_and_ignore_file_before_the_first_test)
        self.assertEqual(code, 0, out)
        self.assertEqual(verdicts(out)["LC-3"], "PASS", out)

    def test_every_spelling_of_a_test_runner_committed_first_passes_lc_3(self) -> None:
        code, out = self.check(m_every_test_runner_name_before_the_first_test)
        self.assertEqual(code, 0, out)
        self.assertEqual(verdicts(out)["LC-3"], "PASS", out)

    def test_each_other_script_committed_first_fails_lc_3(self) -> None:
        for path in SCRIPTS_THAT_ARE_CODE:
            with self.subTest(path=path):
                code, out = self.check(m_script_committed_before_the_first_test(path))
                self.assertEqual(code, 1, out)
                self.assertIn("code was committed", line_of(out, "LC-3"))

    def test_the_path_classifier_says_the_same(self) -> None:
        module = script_module()
        for stem in RUNNER_STEMS:
            for ext in RUNNER_EXTENSIONS:
                for path in ("%s%s" % (stem, ext), "scripts/%s%s" % (stem, ext), "ci/tools/%s%s" % (stem.upper(), ext.upper()), "Scripts/%s%s" % (stem.title(), ext)):
                    with self.subTest(path=path):
                        self.assertTrue(module.is_test(path))
                        self.assertFalse(module.is_code(path))
                        self.assertFalse(module.is_doc(path))
        for path in SCRIPTS_THAT_ARE_CODE + ["build.sh", "scripts/test.js", "scripts/test.go"]:
            with self.subTest(path=path):
                self.assertFalse(module.is_test(path))
                self.assertTrue(module.is_code(path))
        for path in ["scripts/test", "scripts/test.sh.txt", "scripts/test.sh.bak", "docs/test.md"]:
            with self.subTest(path=path):
                self.assertFalse(module.is_code(path))
        for path in ["scripts/test", "scripts/test.sh.bak"]:
            with self.subTest(path=path):
                self.assertFalse(module.is_test(path))
        for path in ["scripts/check-contract.sh", "scripts/test-unit.sh", "scripts/verify_all.py", "scripts/check_build.bash"]:  # the prefixes of pack 1.0.4 stay tests
            with self.subTest(path=path):
                self.assertTrue(module.is_test(path))


def card_with(phrase: str) -> str:
    return GOAL.replace("Nothing else changes.", "Nothing else changes. " + phrase)


class NoAgentsMdCase(CaseInAFolder):
    """Package DOX-FIX-B (pack 1.0.9), LC-7: a goal card that holds an explicit prohibition of AGENTS.md makes LC-7 ask for the README and the last commit only, and fail when the run wrote an
    AGENTS.md anywhere. Contract step 7 allows the exception ("unless the user says to write no AGENTS.md"); the card is where step 1 has the request written down. The list of phrases is
    short and pinned here; "DOX off", "no DOX", "no framework" and "no docs" alone are no prohibition, and a prohibition that is not in the goal card changes nothing."""

    def test_each_phrase_of_the_list_is_a_prohibition_in_a_card_and_alone(self) -> None:
        module = script_module()
        for phrase in BANS:
            with self.subTest(phrase=phrase):
                self.assertTrue(module.bans_agents_md(phrase))
                self.assertTrue(module.bans_agents_md(card_with(phrase)))

    def test_nothing_else_is_a_prohibition(self) -> None:
        module = script_module()
        self.assertFalse(module.bans_agents_md(GOAL))
        for phrase in NOT_BANS:
            with self.subTest(phrase=phrase):
                self.assertFalse(module.bans_agents_md(phrase))
                self.assertFalse(module.bans_agents_md(card_with(phrase)))

    def test_a_card_with_a_prohibition_and_no_AGENTS_md_in_the_tree_passes_lc_7_and_says_why(self) -> None:
        code, out = self.check(card_says_and_the_run_writes(BANS[0]))
        self.assertEqual(code, 0, out)
        self.assertEqual(verdicts(out), {"LC-%d" % n: "PASS" for n in range(1, 9)}, out)
        self.assertEqual(line_of(out, "LC-7"), "LC-7 delivery PASS: no AGENTS.md written, as the goal card says; README.md with run, last commit with subject and body")

    def test_without_a_prohibition_the_pass_line_of_lc_7_is_as_it_was(self) -> None:
        code, out = self.check(None)
        self.assertEqual(code, 0, out)
        self.assertEqual(line_of(out, "LC-7"), "LC-7 delivery PASS: AGENTS.md with build and test, README.md with run, last commit with subject and body")

    def test_the_run_that_wrote_the_root_file_against_the_prohibition_fails_lc_7_and_names_the_file(self) -> None:
        code, out = self.check(card_says_and_the_run_writes(BANS[0], root=True))
        self.assertEqual(code, 1, out)
        self.assertEqual(line_of(out, "LC-7"), "LC-7 delivery FAIL: the goal card says to write no AGENTS.md, and the run wrote AGENTS.md")
        self.assertEqual(sorted(k for k, v in verdicts(out).items() if v == "FAIL"), ["LC-7"], out)

    def test_a_child_doc_against_the_prohibition_fails_lc_7_and_names_the_file(self) -> None:
        code, out = self.check(card_says_and_the_run_writes(BANS[0], child=True))
        self.assertEqual(code, 1, out)
        self.assertEqual(line_of(out, "LC-7"), "LC-7 delivery FAIL: the goal card says to write no AGENTS.md, and the run wrote api/AGENTS.md")

    def test_the_prohibition_does_not_excuse_the_readme(self) -> None:
        code, out = self.check(card_says_and_the_run_writes(BANS[0], readme=False))
        self.assertEqual(code, 1, out)
        self.assertEqual(line_of(out, "LC-7"), "LC-7 delivery FAIL: no README.md")

    def test_an_AGENTS_md_nobody_committed_is_still_the_runs_file(self) -> None:
        code, out = self.check(m_ban_and_an_agents_md_nobody_committed)
        self.assertEqual(code, 1, out)
        self.assertEqual(line_of(out, "LC-7"), "LC-7 delivery FAIL: the goal card says to write no AGENTS.md, and the run wrote notes/AGENTS.md")

    def test_the_users_own_AGENTS_md_is_not_the_runs_file_unless_the_run_touches_it(self) -> None:
        code, out = self.check(m_ban_and_the_users_own_agents_md_at_the_base)
        self.assertEqual(code, 0, out)
        self.assertEqual(line_of(out, "LC-7"), "LC-7 delivery PASS: no AGENTS.md written, as the goal card says; README.md with run, last commit with subject and body")
        code, out = self.check(m_ban_and_the_run_rewrites_the_users_own_agents_md)
        self.assertEqual(code, 1, out)
        self.assertEqual(line_of(out, "LC-7"), "LC-7 delivery FAIL: the goal card says to write no AGENTS.md, and the run wrote AGENTS.md")

    def test_dox_off_and_the_other_opt_outs_still_need_the_root_file(self) -> None:
        for phrase in NOT_BANS:
            if not phrase:
                continue
            with self.subTest(phrase=phrase):
                code, out = self.check(card_says_and_the_run_writes(phrase))
                self.assertEqual(code, 1, out)
                self.assertEqual(line_of(out, "LC-7"), "LC-7 delivery FAIL: no root AGENTS.md")

    def test_a_prohibition_outside_the_goal_card_changes_nothing(self) -> None:
        for mutation in (m_ban_in_the_readme_and_no_goal_card, m_ban_in_the_log_not_in_the_card):
            with self.subTest(mutation=mutation.__name__):
                code, out = self.check(mutation)
                self.assertEqual(code, 1, out)
                self.assertEqual(line_of(out, "LC-7"), "LC-7 delivery FAIL: no root AGENTS.md")


# The first form of NO_AGENTS_MD (pack 1.0.9 as first written), kept as the oracle of NoAgentsMdPatternCase. It stands in two parts, joined when the test runs: the pinned semgrep
# rule regex_dos reads a pattern that is a string literal given to the compile call, and this one is the form it flagged (a star over a group that holds \s+).
FIRST_FORM_LEAD = (r"(?mi)(?:\b(?:do not|don't|never)\s+(?:write|create|add|make|generate|include)"
                   r"|\b(?:write|create|add|make|generate|include|requested|asked for|wants?)\s+no"
                   r"|\bwithout"
                   r"|(?:^|[.;:!?\n(\"'`*-])\s*no)")
FIRST_FORM_TAIL = r"\s+(?:(?:a|an|any|the|root)\s+)*agents\.md\b"


class Sequence:
    """A small deterministic generator (a linear congruential one), so that the corpus is the same on every run and the test needs nothing from the random module."""

    def __init__(self, seed: int) -> None:
        self.state = seed

    def below(self, limit: int) -> int:
        self.state = (self.state * 6364136223846793005 + 1442695040888963407) % (1 << 64)
        return (self.state >> 33) % limit

    def choice(self, items: list):
        return items[self.below(len(items))]

    def between(self, low: int, high: int) -> int:
        return low + self.below(high - low + 1)


def pattern_corpus() -> list:
    """Texts for the oracle comparison: every lead, separator, run of up to two words and file name in turn, long runs of the article words, and seeded random strings."""
    leads = ["no", "No", "NO", "write no", "Write no", "without", "Without", "do not write", "Don't create", "never add", "requested no", "asked for no", "wants no", "want no",
             "(no", '"no', "`no", "*no", "-no", "x no", "has no", "there is no", "with no", "do not touch", "do not", "write", ""]
    words = ["a", "an", "any", "the", "root", "A", "The", "ROOT", "child", "an'", "a-", "x", "agents"]
    seps = [" ", "  ", "\t", "\n", " \n ", "\u00a0", "", "\r\n"]
    names = ["agents.md", "AGENTS.MD", "Agents.Md", "agents.mdx", "agents.md.bak", "agentsmd", "agents.md.", "xagents.md", "_agents.md", "agents_md"]
    texts = []
    for lead in leads:
        for sep in seps:
            for count in (0, 1, 2):
                for combo in itertools.product(words, repeat=count):
                    for name in names:
                        texts.append(lead + sep + sep.join(combo + (name,)))
    rng = Sequence(20261020)
    articles = ["a", "an", "any", "the", "root"]
    for lead in ("no", "Do not write", "without", "x no"):
        for sep in (" ", "  ", "\n"):
            for count in (3, 4, 5, 9, 20, 100):
                for name in ("agents.md", "agents.mdx"):
                    texts.append(lead + sep + sep.join([rng.choice(articles) for _ in range(count)] + [name]))
    tokens = ["no", "do", "not", "write", "without", "a", "an", "any", "the", "root", "agents.md", "AGENTS.md", ".", ";", ":", "x", "-", "(", "`", '"']
    for _ in range(30000):
        texts.append("".join(rng.choice(tokens) + rng.choice([" ", "  ", "\n", "\t", ""]) for _ in range(rng.between(1, 9))))
    return texts


class NoAgentsMdPatternCase(unittest.TestCase):
    """NO_AGENTS_MD was rewritten so that the pinned semgrep rule regex_dos stays quiet. The optional words
    before the file name (a, an, any, the, root) were a star over a group that held \\s+; they are now one whitespace character followed by a star over a single whitespace character or
    one of the words with a word boundary on each side. The language is the same, so the verdict of every text is the same. The first form stays here as the oracle, and the cases
    state the forms the two must treat alike: the corpus, a long run of article words, a long run of spaces, and inputs that make a backtracking matcher slow."""

    @classmethod
    def setUpClass(cls) -> None:
        cls.module = script_module()
        cls.first = re.compile(FIRST_FORM_LEAD + FIRST_FORM_TAIL)

    def test_the_pattern_gives_the_verdict_of_its_first_form_on_a_corpus(self) -> None:
        corpus = pattern_corpus()
        self.assertGreater(len(corpus), 300000)
        verdicts_of_first = [self.first.search(text) is not None for text in corpus]
        yes = sum(verdicts_of_first)
        self.assertGreater(yes, 5000, "the corpus holds prohibitions")
        self.assertGreater(len(corpus) - yes, 100000, "and texts that are none")
        different = [text for text, want in zip(corpus, verdicts_of_first) if self.module.bans_agents_md(text) != want]
        self.assertEqual(different[:5], [])

    def test_a_long_run_of_article_words_is_still_skipped_over(self) -> None:
        for lead in ("no", "Do not write", "Write no", "without", "The request says: no"):
            for count in (1, 2, 3, 4, 9, 50, 500, 5000):
                text = lead + " " + " ".join(["a"] * count) + " agents.md"
                with self.subTest(lead=lead, count=count):
                    self.assertTrue(self.module.bans_agents_md(text))
                    self.assertIsNotNone(self.first.search(text))
        text = "no " + " ".join(["a", "an", "any", "the", "root"] * 400) + " AGENTS.md"
        self.assertTrue(self.module.bans_agents_md(text))
        self.assertIsNotNone(self.first.search(text))
        for tail in (" x", " agentsmd", " agents.mdx", ""):
            text = "no " + " ".join(["a"] * 3000) + tail
            with self.subTest(tail=tail):
                self.assertFalse(self.module.bans_agents_md(text))
                self.assertIsNone(self.first.search(text))

    def test_a_long_run_of_spaces_is_still_skipped_over(self) -> None:
        for count in (1, 2, 10, 1000, 100000):
            for text in ("no" + " " * count + "agents.md", "no a" + " " * count + "agents.md", "no" + "\n" * count + "the" + "\t" * count + "AGENTS.md"):
                with self.subTest(count=count, text=text[:12]):
                    self.assertTrue(self.module.bans_agents_md(text))
                    self.assertIsNotNone(self.first.search(text))
        for text in ("no" + " " * 100000 + "x", "no a" + " " * 100000 + "agentsmd", "no" + "\u00a0" * 5000 + "a" + "\u00a0" * 5000):
            with self.subTest(text=text[:12]):
                self.assertFalse(self.module.bans_agents_md(text))
                self.assertIsNone(self.first.search(text))

    def test_inputs_that_slow_a_backtracking_matcher_are_answered_quickly(self) -> None:
        hard = {
            "article words and no file": "no " + "a " * 40000 + "x",
            "spaces and no file": "no" + " " * 200000 + "x",
            "many leads": "no " * 40000,
            "many refusals": "do not " * 30000,
            "punctuation and spaces": ". " * 40000 + "no",
            "a word with no space": "no " + "a" * 200000,
            "article words, one space short": "no " + "a " * 40000 + "agents.md".replace("a", "b", 1),
        }
        for label, text in hard.items():
            with self.subTest(label=label):
                started = time.monotonic()
                self.module.bans_agents_md(text)
                self.assertLess(time.monotonic() - started, 5.0)


PACK = REPO / "lifecycle" / "pack"
STEPS =["Goal card", "Research", "Context", "Test first", "Build and test", "Loop", "Deliver", "Self-check"]


def installed_pack(parent: Path) -> Path:
    """What `ham lifecycle init` leaves in an empty folder: the contract and the skills folder, byte for byte; no git, no records."""
    dest = parent / "installed"
    shutil.copytree(PACK, dest)
    return dest


def structure(repo: Path, *extra: str) -> tuple:
    return run(repo, "--structure", *extra)


def st_verdicts(out: str) -> dict:
    return {m.group(1): m.group(3) for m in re.finditer(r"(?m)^(ST-\d) (\S+) (PASS|FAIL)", out)}


class StructureModeCase(unittest.TestCase):
    """`ham lifecycle init` into an empty folder passes the mode of the self-check that needs no records (`--structure`).

    The mode checks the installed pack and nothing the run wrote: the contract with its eight steps, the seven skills with their frontmatter, and the script
    itself. It needs no git, prints its own last line (`LIFECYCLE-STRUCTURE`, never `LIFECYCLE-CHECK`: the harness reads that line as the self-check) and
    leaves the default mode alone.
    """

    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory(prefix="lifecycle-structure-")
        self.root = Path(self.tmp.name)
        self.pack = installed_pack(self.root)

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def skill(self, name: str, pack: "Path | None" = None) -> Path:
        return (pack or self.pack) / ".agents" / "skills" / ("lifecycle-" + name) / "SKILL.md"

    def edit(self, path: Path, old: str, new: str) -> None:
        text = path.read_text(encoding="utf-8")
        self.assertIn(old, text)
        path.write_text(text.replace(old, new, 1), encoding="utf-8")

    def test_an_installed_pack_passes_without_git_and_without_records(self) -> None:
        self.assertFalse((self.pack / ".git").exists())
        code, out, err = structure(self.pack)
        self.assertEqual((code, err), (0, ""), out)
        self.assertEqual(st_verdicts(out), {"ST-1": "PASS", "ST-2": "PASS", "ST-3": "PASS"}, out)
        self.assertEqual(out.strip().split("\n")[-1], "LIFECYCLE-STRUCTURE PASS pass=3 fail=0")
        self.assertNotIn("LIFECYCLE-CHECK", out)
        self.assertNotIn("LC-", out)

    def test_the_mode_writes_nothing(self) -> None:
        before = sorted((p.relative_to(self.pack).as_posix(), p.stat().st_size) for p in self.pack.rglob("*") if p.is_file())
        structure(self.pack)
        after = sorted((p.relative_to(self.pack).as_posix(), p.stat().st_size) for p in self.pack.rglob("*") if p.is_file())
        self.assertEqual(before, after)

    def test_the_default_mode_on_the_same_folder_is_unchanged_a_refusal_for_a_folder_that_is_no_repository(self) -> None:
        code, out, _ = run(self.pack)
        self.assertEqual(code, 2)
        self.assertIn("LIFECYCLE-CHECK ERROR", out)

    def test_a_project_part_after_the_contract_is_fine(self) -> None:
        agents = self.pack / "AGENTS.md"
        agents.write_text(agents.read_text(encoding="utf-8") + "\n# Project\n\n1. Goal card. Nothing here.\nBuild: `make`. Test: `make test`.\n", encoding="utf-8")
        code, out, _ = structure(self.pack)
        self.assertEqual(code, 0, out)

    def test_each_broken_pack_fails_the_check_its_rule_belongs_to(self) -> None:
        def own_agents_md(pack: Path) -> None:
            # A project's own AGENTS.md with its own numbered list: eight numbered lines are not the contract.
            (pack / "AGENTS.md").write_text("# My project\n\n" + "\n".join("%d. %s. text" % (i + 1, n) for i, n in enumerate(STEPS)) + "\n", encoding="utf-8")

        def steps_out_of_order(pack: Path) -> None:
            # The numbers stay right and step 8 moves in front of step 7.
            text = (pack / "AGENTS.md").read_text(encoding="utf-8")
            block = re.search(r"(?ms)^8\. Self-check.*?\n\n", text)
            self.assertIsNotNone(block)
            moved = text.replace(block.group(0), "", 1)
            moved = moved.replace("7. Deliver", block.group(0) + "7. Deliver", 1)
            (pack / "AGENTS.md").write_text(moved, encoding="utf-8")

        def drop_description(pack: Path) -> None:
            text = self.skill("context", pack).read_text(encoding="utf-8")
            self.skill("context", pack).write_text(re.sub(r"(?m)^description:.*\n", "", text, count=1), encoding="utf-8")

        def long_description(pack: Path) -> None:
            text = self.skill("research", pack).read_text(encoding="utf-8")
            self.skill("research", pack).write_text(re.sub(r"(?m)^description: .*$", "description: " + "x" * 1025, text, count=1), encoding="utf-8")

        def drop_frontmatter(pack: Path) -> None:
            text = self.skill("delivery", pack).read_text(encoding="utf-8")
            self.skill("delivery", pack).write_text(text.split("---\n", 2)[2], encoding="utf-8")

        def script_becomes_a_directory(pack: Path) -> None:
            path = pack / ".agents/skills/lifecycle-check/scripts/lifecycle_check.py"
            path.unlink()
            path.mkdir()

        def empty_description(pack: Path) -> None:
            text = self.skill("tdd", pack).read_text(encoding="utf-8")
            self.skill("tdd", pack).write_text(re.sub(r"(?m)^description: .*$", 'description: ""', text, count=1), encoding="utf-8")

        def block_scalar_description(pack: Path) -> None:
            text = self.skill("loop", pack).read_text(encoding="utf-8")
            self.skill("loop", pack).write_text(re.sub(r"(?m)^description: .*$", "description: >", text, count=1), encoding="utf-8")

        def empty_script(pack: Path) -> None:
            (pack / ".agents/skills/lifecycle-check/scripts/lifecycle_check.py").write_text("", encoding="utf-8")

        def script_is_a_link_to_a_file_outside(pack: Path) -> None:
            outside = self.root / "outside.py"
            outside.write_text("print('x')\n", encoding="utf-8")
            path = pack / ".agents/skills/lifecycle-check/scripts/lifecycle_check.py"
            path.unlink()
            path.symlink_to(outside)

        def skills_folder_is_a_link_to_a_pack_outside(pack: Path) -> None:
            elsewhere = self.root / "elsewhere"
            shutil.copytree(pack / ".agents", elsewhere)
            shutil.rmtree(pack / ".agents")
            (pack / ".agents").symlink_to(elsewhere)

        def contract_is_a_link_to_a_file_outside(pack: Path) -> None:
            outside = self.root / "outside.md"
            outside.write_text((pack / "AGENTS.md").read_text(encoding="utf-8"), encoding="utf-8")
            (pack / "AGENTS.md").unlink()
            (pack / "AGENTS.md").symlink_to(outside)

        cases = [
            ("no AGENTS.md", lambda pack: (pack / "AGENTS.md").unlink(), "ST-1"),
            ("an AGENTS.md of the project with a numbered list of its own", own_agents_md, "ST-1"),
            ("a step of the contract missing", lambda pack: self.edit(pack / "AGENTS.md", "5. Build and test.", "5. Run it."), "ST-1"),
            ("steps out of order", steps_out_of_order, "ST-1"),
            ("a skill folder missing", lambda pack: shutil.rmtree(self.skill("loop", pack).parent), "ST-2"),
            ("a skill whose name is not its folder", lambda pack: self.edit(self.skill("tdd", pack), "name: lifecycle-tdd", "name: lifecycle-tests"), "ST-2"),
            ("a skill with no description", drop_description, "ST-2"),
            ("a skill description over 1,024 characters", long_description, "ST-2"),
            ("a skill with no frontmatter", drop_frontmatter, "ST-2"),
            ("a skill of more than 60 lines", lambda pack: self.skill("build-test", pack).write_text(self.skill("build-test", pack).read_text(encoding="utf-8") + "more\n" * 60, encoding="utf-8"), "ST-2"),
            ("no script", lambda pack: (pack / ".agents/skills/lifecycle-check/scripts/lifecycle_check.py").unlink(), "ST-3"),
            ("a directory where the script should be", script_becomes_a_directory, "ST-3"),
            ("a skill with an empty description", empty_description, "ST-2"),
            ("a skill whose description is a block scalar marker", block_scalar_description, "ST-2"),
            ("an empty script", empty_script, "ST-3"),
            ("a script that is a link to a file outside the folder", script_is_a_link_to_a_file_outside, "ST-3"),
            ("a skills folder that is a link to a pack outside the folder", skills_folder_is_a_link_to_a_pack_outside, ("ST-2", "ST-3")),
            ("a contract that is a link to a file outside the folder", contract_is_a_link_to_a_file_outside, "ST-1"),
        ]
        for i, (name, mutate, check) in enumerate(cases):
            with self.subTest(variant=name):
                pack = installed_pack(self.root / ("case%d" % i))
                mutate(pack)
                code, out, err = structure(pack)
                self.assertEqual((code, err), (1, ""), out)
                got = st_verdicts(out)
                expected = [check] if isinstance(check, str) else list(check)
                self.assertEqual([k for k, v in got.items() if v == "FAIL"], expected, out)
                self.assertTrue(out.strip().split("\n")[-1].startswith("LIFECYCLE-STRUCTURE FAIL %s " % ",".join(expected)), out)

    def test_a_byte_order_mark_before_the_frontmatter_is_read_through(self) -> None:
        path = self.skill("check")
        path.write_bytes(b"\xef\xbb\xbf" + path.read_bytes())
        code, out, _ = structure(self.pack)
        self.assertEqual(code, 0, out)

    def test_an_abbreviation_of_the_flag_is_not_the_flag(self) -> None:
        # The scorer refuses a title that shows `--structure`; an abbreviation would run the mode and slip past it, so the script takes the whole word only.
        for flag in ("--struct", "--str", "--s"):
            with self.subTest(flag=flag):
                code, out, err = run(self.pack, flag)
                self.assertEqual(code, 2, out + err)
                self.assertNotIn("LIFECYCLE-STRUCTURE", out)

    def test_a_folder_that_does_not_exist_exits_2_and_the_mode_refuses_a_base(self) -> None:
        code, out, _ = structure(self.root / "missing")
        self.assertEqual(code, 2)
        self.assertIn("LIFECYCLE-STRUCTURE ERROR", out)
        code, out, _ = structure(self.pack, "--base", "HEAD")
        self.assertEqual(code, 2)
        self.assertIn("LIFECYCLE-STRUCTURE ERROR", out)

    def test_the_check_runs_in_a_git_repository_that_holds_the_pack_too(self) -> None:
        env = git_env(str(self.root))
        git(self.pack, env, "init", "-q")
        git(self.pack, env, "add", "-A")
        git(self.pack, env, "commit", "-q", "-m", "Install the lifecycle pack")
        code, out, _ = structure(self.pack)
        self.assertEqual(code, 0, out)
        # ... and the default mode still answers the same pack-only repository the way the installer's test pins it: a verdict, exit 1, LC-1 failing.
        code, out, _ = run(self.pack)
        self.assertEqual(code, 1)
        self.assertEqual(verdicts(out)["LC-1"], "FAIL")


# The repository may hold `.ham/records`: its first line is `off` or a folder that takes the place of .ham (the card, LOG.md and red/ go to <folder>/lifecycle, the notes to
# <folder>/research, the index to <folder>/context). Without the file the records sit below the hidden folder .ham, where no docs gate looks; before pack 1.2.0 they sat in docs/, and a gate
# that rejects unknown files under docs/ took them for a defect. The checks read the folder the file names. agentd reads the same file the same way (crates/agentd/src/direct/records.rs); the
# cases both readers must agree on are in fixtures/lifecycle/records-locator.json.
LOCATOR_CASES = REPO / "fixtures" / "lifecycle" / "records-locator.json"
DOCS_ROOT = "docs"          # the layout of before pack 1.2.0, now an explicit choice
OTHER_ROOT = "run-records"  # any other folder
MOVED_ROOT = DOCS_ROOT


def moved_path(path: str, root: str) -> str:
    """The folder takes the place of .ham for the three record folders (docs/spec.md, the user's own document, stays)."""
    for kind in ("lifecycle", "research", "context"):
        if path.startswith(".ham/%s/" % kind):
            return root + path[len(".ham"):]
    return path


def m_records_moved(root: str = MOVED_ROOT):
    """The golden history with every record under `root` and the locator in the first commit."""
    def mutation(specs):
        for spec in specs:
            spec["files"] = {moved_path(p, root): text for p, text in spec["files"].items()}
        spec_of(specs, "plan")["files"][".ham/records"] = "# the records of runs\n%s\n" % root
    return mutation


def m_records_in_docs_without_a_locator(specs):
    """The records of before pack 1.2.0: in docs/ and no `.ham/records`, as a run under an older rules file leaves them."""
    for spec in specs:
        spec["files"] = {moved_path(p, DOCS_ROOT): text for p, text in spec["files"].items()}


def m_records_off(specs):
    """No goal card, research, context, red records or log, and a locator that says off."""
    for spec in specs:
        spec["files"] = {p: t for p, t in spec["files"].items() if not p.startswith((".ham/lifecycle/", ".ham/research/", ".ham/context/"))}
    spec_of(specs, "plan")["files"][".ham/records"] = "off\n"


# A RUN row whose note says PASS for itself is not a red run, as agentd's observation reads it (crates/agentd/src/direct/observe.rs, note_says_pass). The cases both readers and the
# scorer must agree on are in fixtures/lifecycle/pass-notes.json.
PASS_NOTES = REPO / "fixtures" / "lifecycle" / "pass-notes.json"


class PassNoteCase(CaseInAFolder):
    def test_the_script_reads_every_shared_note_as_agentd_and_the_scorer_do(self) -> None:
        import json
        module = script_module()
        cases = json.loads(PASS_NOTES.read_text(encoding="utf-8"))["cases"]
        self.assertGreaterEqual(len(cases), 40)
        self.assertTrue(any(c["says_pass"] for c in cases) and any(not c["says_pass"] for c in cases))
        for case in cases:
            with self.subTest(note=case["note"]):
                self.assertIs(module.note_says_pass(case["note"]), case["says_pass"])

    def test_a_pass_row_with_failures_passes_check_5(self) -> None:
        code, out = self.check(m_log_ends_with_failures_and_pass_in_its_note)
        self.assertEqual(code, 0, out)
        self.assertEqual(verdicts(out)["LC-5"], "PASS", out)

    def test_failures_with_a_note_that_does_not_say_pass_and_a_non_zero_exit_still_fail_check_5(self) -> None:
        for label, mutation in (("a note that denies PASS", m_log_ends_with_failures_and_a_note_that_denies_pass),
                                ("PASS beside FAILED", m_log_ends_with_failures_and_pass_beside_failed),
                                ("a non-zero exit", m_log_ends_with_a_non_zero_exit_and_pass_in_its_note)):
            with self.subTest(label):
                code, out = self.check(mutation)
                self.assertEqual(code, 1, out)
                self.assertEqual(verdicts(out)["LC-5"], "FAIL", out)
                self.assertIn("the last run is red", line_of(out, "LC-5"))


class RecordsLocatorCase(CaseInAFolder):
    def test_the_script_reads_every_shared_locator_case_as_agentd_does(self) -> None:
        import json
        module = script_module()
        data = json.loads(LOCATOR_CASES.read_text(encoding="utf-8"))
        cases = data["cases"]
        self.assertGreaterEqual(len(cases), 50)
        self.assertEqual(module.DEFAULT_BASE, data["defaultBase"])
        self.assertEqual(module.Layout().records, data["defaultBase"] + "/lifecycle")
        for case in cases:
            kind, value = module.parse_locator(case["text"])
            self.assertEqual(kind, case["kind"], repr(case["text"]))
            if case["kind"] == "moved":
                self.assertEqual(value, case["dir"], repr(case["text"]))
            if case["kind"] == "invalid":
                self.assertTrue(value, repr(case["text"]))

    def test_the_default_places_are_the_places_of_the_value_dot_ham(self) -> None:
        module = script_module()
        default, named = module.Layout(), module.Layout("moved", ".ham")
        self.assertEqual((default.goal, default.log, default.red_dir, default.research_dir, default.context_index),
                         (named.goal, named.log, named.red_dir, named.research_dir, named.context_index))
        self.assertEqual(module.Layout("moved", "docs").goal, "docs/lifecycle/GOAL.md")

    def test_a_file_over_the_read_limit_is_invalid_as_it_is_for_agentd(self) -> None:
        """agentd answers `invalid` for a locator of 2 MiB (records.rs), whatever its file reader does with a big file; the script answers the same for any file over 4,096 bytes."""
        module = script_module()
        for size in (4097, 2 << 20):
            with self.subTest(size=size):
                root = self.root / ("big%d" % size)
                (root / ".ham").mkdir(parents=True)
                (root / ".ham" / "records").write_bytes(b"off\n" + b"#" * size)
                self.assertEqual(module.read_locator(root), ("invalid", "the file is longer than 4096 bytes"))

    def test_the_default_layout_prints_no_layout_line_and_is_as_it_was(self) -> None:
        code, out = self.check(None)
        self.assertEqual(code, 0, out)
        self.assertNotIn("LAYOUT", out)
        self.assertEqual(verdicts(out), {"LC-%d" % n: "PASS" for n in range(1, 9)}, out)

    def test_a_run_on_a_repository_without_a_locator_writes_nothing_under_docs_but_the_users_own_document(self) -> None:
        """The stray files of a self-run (docs/lifecycle, docs/research, docs/context, 9 and 6 files) are not written when the repository says nothing."""
        repo = build(self.root, None)
        tracked = subprocess.run(["git", "-C", str(repo), "ls-files"], capture_output=True, text=True).stdout.split()  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
        self.assertEqual([p for p in tracked if p.startswith("docs/")], ["docs/spec.md"], tracked)
        self.assertFalse((repo / ".ham" / "records").exists())
        for card in (".ham/lifecycle/GOAL.md", ".ham/lifecycle/LOG.md", ".ham/lifecycle/red/01-add.txt", ".ham/research/01-arithmetic.md", ".ham/context/INDEX.md"):
            self.assertIn(card, tracked)
        history = subprocess.run(["git", "-C", str(repo), "log", "--name-only", "--format="], capture_output=True, text=True).stdout.split()  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
        self.assertEqual([p for p in history if re.match(r"docs/(?:lifecycle|research|context)/", p)], [])
        code, out, err = run(repo)
        self.assertEqual((code, err), (0, ""), out)
        self.assertEqual(verdicts(out), {"LC-%d" % n: "PASS" for n in range(1, 9)}, out)

    def test_records_in_the_folder_named_docs_pass_every_check_and_the_layout_is_named(self) -> None:
        code, out = self.check(m_records_moved(DOCS_ROOT))
        self.assertEqual(code, 0, out)
        self.assertEqual(verdicts(out), {"LC-%d" % n: "PASS" for n in range(1, 9)}, out)
        self.assertIn("LAYOUT records in docs (from .ham/records)", out)
        self.assertEqual(out.strip().split("\n")[-1], "LIFECYCLE-CHECK PASS pass=8 na=0 fail=0")

    def test_records_in_another_folder_pass_every_check_and_the_layout_is_named(self) -> None:
        code, out = self.check(m_records_moved(OTHER_ROOT))
        self.assertEqual(code, 0, out)
        self.assertEqual(verdicts(out), {"LC-%d" % n: "PASS" for n in range(1, 9)}, out)
        self.assertIn("LAYOUT records in %s (from .ham/records)" % OTHER_ROOT, out)

    def test_the_value_dot_ham_is_the_default_places_and_is_named_in_the_first_line(self) -> None:
        code, out = self.check(m_records_moved(".ham"))
        self.assertEqual(code, 0, out)
        self.assertEqual(verdicts(out), {"LC-%d" % n: "PASS" for n in range(1, 9)}, out)
        self.assertIn("LAYOUT records in .ham (from .ham/records)", out)

    def test_records_in_docs_are_not_read_when_the_repository_has_no_locator(self) -> None:
        """No second place: a run that wrote its records to docs/ under an older rules file and left no `.ham/records` has no records where the pack looks."""
        code, out = self.check(m_records_in_docs_without_a_locator)
        self.assertEqual(code, 1, out)
        got = verdicts(out)
        for check in ("LC-1", "LC-2", "LC-3", "LC-5", "LC-6"):
            self.assertEqual(got[check], "FAIL", (check, out))
        self.assertIn(".ham/lifecycle/GOAL.md not found", line_of(out, "LC-6"))

    def test_a_repository_whose_docs_gate_rejects_unknown_files_is_green_by_default_and_red_with_the_records_in_docs(self) -> None:
        # This stand-in knows one document.
        def gate(repo: Path) -> list:
            tracked = subprocess.run(["git", "-C", str(repo), "ls-files", "docs"], capture_output=True, text=True).stdout.split()  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
            return [p for p in tracked if p not in ("docs/spec.md",)]
        (self.root / "a").mkdir()
        (self.root / "b").mkdir()
        # TRIGGER PRESENT: with the records in docs/ the gate is red
        self.assertGreaterEqual(len(gate(build(self.root / "a", m_records_moved(DOCS_ROOT)))), 5)
        repo = build(self.root / "b", None)
        self.assertEqual(gate(repo), [])
        code, out, err = run(repo)
        self.assertEqual((code, err), (0, ""), out)
        self.assertEqual(verdicts(out), {"LC-%d" % n: "PASS" for n in range(1, 9)}, out)

    def test_each_rule_still_fails_in_the_other_layouts(self) -> None:
        for root in (DOCS_ROOT, OTHER_ROOT):
            for name, (mutation, check) in BROKEN.items():
                if check not in ("LC-1", "LC-2", "LC-3", "LC-5", "LC-6"):
                    continue
                with self.subTest(root=root, name=name):
                    def both(specs, mutation=mutation, root=root):
                        post = mutation(specs)
                        m_records_moved(root)(specs)
                        return post
                    code, out = self.check(both)
                    self.assertEqual(code, 1, (name, out))
                    self.assertEqual(verdicts(out).get(check), "FAIL", (name, out))
                    line = line_of(out, check)
                    self.assertNotRegex(line, r"\.ham/(?:lifecycle|research|context)", (name, line))

    def test_records_left_in_the_default_folders_are_not_read_when_the_locator_names_another(self) -> None:
        def stale(specs):
            m_records_moved()(specs)
            spec_of(specs, "deliver")["files"][".ham/lifecycle/LOG.md"] = LOG.replace("exit 0", "exit 1").replace("pass 2 fail 0", "pass 1 fail 1")
        code, out = self.check(stale)
        self.assertEqual(code, 0, out)
        self.assertEqual(verdicts(out)["LC-5"], "PASS", out)

    def test_a_script_in_the_records_folder_is_a_document_like_one_under_docs(self) -> None:
        for root in (None, DOCS_ROOT, OTHER_ROOT):
            with self.subTest(root=root):
                def baseline_script(specs, root=root):
                    if root:
                        m_records_moved(root)(specs)
                    specs.insert(1, extra_commit("baseline", "docs: baseline of known failures", {(root or ".ham") + "/lifecycle/verify-suite.sh": "#!/bin/sh\necho baseline\n"}))
                code, out = self.check(baseline_script)
                self.assertEqual(code, 0, out)
                self.assertEqual(verdicts(out)["LC-3"], "PASS", out)

    def test_a_secret_shaped_string_in_a_note_of_the_records_folder_fails_check_8_in_every_layout(self) -> None:
        for root in (None, DOCS_ROOT, OTHER_ROOT):
            with self.subTest(root=root):
                def leak(specs, root=root):
                    if root:
                        m_records_moved(root)(specs)
                    key = "AKIA" + "ABCDEFGHIJKLMNOP"
                    files = spec_of(specs, "plan")["files"]
                    files[(root or ".ham") + "/research/01-arithmetic.md"] = RESEARCH + "- source-reported: key %s was in the sample\n" % key
                code, out = self.check(leak)
                self.assertEqual(code, 1, out)
                self.assertEqual(verdicts(out)["LC-8"], "FAIL", out)
                self.assertIn("secret-shaped string in %s/research/01-arithmetic.md" % (root or ".ham"), line_of(out, "LC-8"))

    def test_records_off_marks_the_record_checks_not_applicable_and_runs_the_others(self) -> None:
        code, out = self.check(m_records_off)
        self.assertEqual(code, 0, out)
        got = verdicts(out)
        self.assertEqual({k: got[k] for k in ("LC-1", "LC-2", "LC-3", "LC-5", "LC-6", "LC-8")}, {k: "NA" for k in ("LC-1", "LC-2", "LC-3", "LC-5", "LC-6", "LC-8")}, out)
        self.assertEqual({k: got[k] for k in ("LC-4", "LC-7")}, {"LC-4": "PASS", "LC-7": "PASS"}, out)
        self.assertIn("LAYOUT records off (from .ham/records)", out)
        for line in out.split("\n"):
            if line.startswith(("LC-1 ", "LC-2 ", "LC-3 ", "LC-5 ", "LC-6 ")):
                self.assertIn("records off", line)
        self.assertEqual(out.strip().split("\n")[-1], "LIFECYCLE-CHECK PASS pass=2 na=6 fail=0")

    def test_records_off_still_fails_a_deleted_test_a_missing_readme_and_a_secret(self) -> None:
        for mutation, check in ((m_test_file_deleted, "LC-4"), (m_no_readme, "LC-7"), (m_secret_in_a_readme_when_off, "LC-8")):
            def both(specs, mutation=mutation):
                post = mutation(specs)
                m_records_off(specs)
                return post
            code, out = self.check(both)
            self.assertEqual(code, 1, (check, out))
            self.assertEqual(verdicts(out).get(check), "FAIL", (check, out))

    def test_an_invalid_locator_is_an_error_and_not_a_silent_default(self) -> None:
        for text in ("../outside\n", "/etc\n", "a b\n", ".git/x\n"):
            def bad(specs, text=text):
                spec_of(specs, "plan")["files"][".ham/records"] = text
            code, out = self.check(bad)
            self.assertEqual(code, 2, (text, out))
            self.assertRegex(out, r"(?m)^LIFECYCLE-CHECK ERROR \.ham/records ", out)
            self.assertNotRegex(out, r"(?m)^LC-\d ")

    def test_a_locator_that_is_a_link_is_not_followed(self) -> None:
        def linked(specs):
            def post(repo):
                (repo / ".ham").mkdir(exist_ok=True)
                target = repo.parent / "elsewhere"
                target.write_text("off\n")
                os.symlink(target, repo / ".ham" / "records")
            return post
        code, out = self.check(linked)
        self.assertEqual(code, 0, out)
        self.assertNotIn("LAYOUT", out)
        self.assertEqual(verdicts(out), {"LC-%d" % n: "PASS" for n in range(1, 9)}, out)


def m_secret_in_a_readme_when_off(specs):
    spec_of(specs, "deliver")["files"]["README.md"] = README + "\nkey AKIA" + "ABCDEFGHIJKLMNOP\n"


# Compatibility with the rename: the folder of the records is chosen once per run: `.ham`, or the folder of the runs made before the rename when only that one holds state, and a refusal when both do
# (the expiry manifest in compat/ ends the fallback). agentd (crates/agentd/src/direct/records.rs) and the scorer read the same cases
# (fixtures/lifecycle/records-locator.json, `baseCases`). This file names the earlier folder only through that fixture.
def base_data() -> dict:
    import json
    return json.loads(LOCATOR_CASES.read_text(encoding="utf-8"))


def m_records_in_the_earlier_folder(specs):
    """The golden history with every `.ham/` path under the folder of the earlier runs, and nothing else changed."""
    earlier = base_data()["baseFolders"]["legacy"]
    for spec in specs:
        spec["files"] = {(earlier + p[len(".ham"):] if p.startswith(".ham/") else p): text for p, text in spec["files"].items()}


def m_records_in_the_earlier_folder_moved_to(root: str):
    """The same, with the earlier folder's own `records` file naming `root` as the folder that takes the place of the base."""
    earlier = base_data()["baseFolders"]["legacy"]

    def mutation(specs):
        m_records_moved(root)(specs)
        for spec in specs:
            spec["files"] = {(earlier + p[len(".ham"):] if p.startswith(".ham/records") else p): text for p, text in spec["files"].items()}
    return mutation


def m_both_folders_hold_records(specs):
    m_records_in_the_earlier_folder(specs)
    spec_of(specs, "plan")["files"][".ham/lifecycle/GOAL.md"] = "SPECIFIC: a second card.\n"


def build_base_case(case: dict, root: Path) -> None:
    """The tree of one shared case in an empty directory."""
    for entry in case.get("files", []):
        rel, text = (entry, "") if isinstance(entry, str) else (entry["path"], entry["text"])
        (root / rel).parent.mkdir(parents=True, exist_ok=True)
        (root / rel).write_text(text, encoding="utf-8")
    for rel in case.get("emptyDirs", []):
        (root / rel).mkdir(parents=True, exist_ok=True)
    for link in case.get("links", []):
        (root / link["path"]).parent.mkdir(parents=True, exist_ok=True)
        (root / link["path"]).symlink_to(link["target"])


class BaseFolderCase(CaseInAFolder):
    def test_the_script_chooses_the_base_of_every_shared_case_as_agentd_does(self) -> None:
        module = script_module()
        data = base_data()
        self.assertGreaterEqual(len(data["baseCases"]), 20)
        self.assertEqual((module.DEFAULT_BASE, module.EARLIER_BASE), (data["baseFolders"]["current"], data["baseFolders"]["legacy"]))
        for number, case in enumerate(data["baseCases"]):
            with self.subTest(case=case["name"]):
                root = self.root / ("base%d" % number)
                root.mkdir()
                build_base_case(case, root)
                if case["expect"] == "refuse":
                    with self.assertRaises(module.RecordsFolderError) as raised:
                        module.choose_base(root)
                    for folder in data["baseFolders"].values():
                        self.assertIn(folder, str(raised.exception))
                else:
                    self.assertEqual(module.choose_base(root), data["baseFolders"][case["expect"]])

    def test_records_in_the_earlier_folder_pass_every_check_and_the_layout_is_named(self) -> None:
        earlier = base_data()["baseFolders"]["legacy"]
        code, out = self.check(m_records_in_the_earlier_folder)
        self.assertEqual(code, 0, out)
        self.assertEqual(verdicts(out), {"LC-%d" % n: "PASS" for n in range(1, 9)}, out)
        self.assertEqual(out.split("\n")[0], "LAYOUT records in %s (the folder of earlier runs; .ham holds none)" % earlier)

    def test_a_locator_in_the_earlier_folder_is_read_there_and_the_layout_says_so(self) -> None:
        earlier = base_data()["baseFolders"]["legacy"]
        code, out = self.check(m_records_in_the_earlier_folder_moved_to(DOCS_ROOT))
        self.assertEqual(code, 0, out)
        self.assertEqual(verdicts(out), {"LC-%d" % n: "PASS" for n in range(1, 9)}, out)
        self.assertIn("LAYOUT records in docs (from %s/records)" % earlier, out)

    def test_records_in_both_folders_end_the_run_with_a_usage_error_that_names_both(self) -> None:
        earlier = base_data()["baseFolders"]["legacy"]
        self.made += 1
        folder = self.root / ("case%d" % self.made)
        folder.mkdir()
        code, out, _ = run(build(folder, m_both_folders_hold_records))
        self.assertEqual(code, 2, out)
        self.assertRegex(out, r"(?m)^LIFECYCLE-CHECK ERROR records: both \.ham and %s hold records" % re.escape(earlier))
        self.assertEqual(verdicts(out), {}, out)

    def test_an_empty_current_folder_does_not_hide_the_earlier_one(self) -> None:
        self.made += 1
        folder = self.root / ("case%d" % self.made)
        folder.mkdir()
        repo = build(folder, m_records_in_the_earlier_folder)
        (repo / ".ham").mkdir()
        code, out, err = run(repo)
        self.assertEqual((code, err), (0, ""), out)
        self.assertEqual(verdicts(out), {"LC-%d" % n: "PASS" for n in range(1, 9)}, out)

    def test_the_earlier_folder_is_not_read_when_the_current_one_holds_the_records(self) -> None:
        code, out = self.check(None)
        self.assertEqual(code, 0, out)
        self.assertNotIn("LAYOUT", out)


class ScriptShapeCase(unittest.TestCase):
    def test_the_script_imports_only_the_standard_library(self) -> None:
        tree = ast.parse(SCRIPT.read_text(encoding="utf-8"))
        names = set()
        for node in ast.walk(tree):
            if isinstance(node, ast.Import):
                names |= {a.name.split(".")[0] for a in node.names}
            elif isinstance(node, ast.ImportFrom) and node.level == 0 and node.module:
                names.add(node.module.split(".")[0])
        stdlib = getattr(sys, "stdlib_module_names", None)
        if stdlib is None:
            stdlib = {"__future__", "argparse", "hashlib", "json", "os", "re", "subprocess", "sys", "pathlib", "typing"}
        self.assertEqual(sorted(n for n in names if n not in stdlib), [])

    def test_the_script_runs_under_python_3_8_syntax(self) -> None:
        text = SCRIPT.read_text(encoding="utf-8")
        ast.parse(text, feature_version=(3, 8))
        self.assertNotRegex(text, r"(?m)^\s*match\s+\S+:\s*$")

    def test_the_script_has_a_docstring_that_says_what_it_reads_and_what_it_prints(self) -> None:
        doc = ast.get_docstring(ast.parse(SCRIPT.read_text(encoding="utf-8"))) or ""
        for word in ("LC-1", "LC-8", "exit", "LIFECYCLE-CHECK"):
            self.assertIn(word, doc)

    def test_the_docstring_names_the_structure_mode_and_says_it_is_no_self_check(self) -> None:
        doc = ast.get_docstring(ast.parse(SCRIPT.read_text(encoding="utf-8"))) or ""
        for word in ("--structure", "LIFECYCLE-STRUCTURE", "ST-1", "not the self-check", "present", "not that"):
            self.assertIn(word, doc)


if __name__ == "__main__":
    unittest.main()
