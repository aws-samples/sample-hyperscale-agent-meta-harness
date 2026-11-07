#!/usr/bin/env python3
"""engine/plugins/lock.json: the SHA-256 of every shipped plugin file.

usage: engine/plugins/build/lock.py [--write]   no flag: check (exit 1 on a difference); --write: rewrite the observer entries of lock.json

The tree holds the harness's observer plugin (engine/plugins/harness-rounds) and this lock. Cline's goal plugin is not in the tree: `scripts/engine-fetch
build goal-plugin` fetches and bundles it and checks the five files against this lock, and `scripts/engine-fetch update` is the only writer of its
entries and of the `sources` record. This tool checks and rewrites the harness-rounds entries and checks that the goal entries are the five expected
names with a SHA-256 each. agentd checks the same file in the image at startup (a mismatch switches the goal loop off for the run) and
scripts/tests/test_goal_plugin.py holds it to the tree.
"""
from __future__ import annotations

import hashlib
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TRACKED_PLUGIN = "harness-rounds"
GOAL_FILES = ("LICENSE", "NOTICE", "PROVENANCE", "index.js", "package.json")


def tree_files() -> dict:
    files = {}
    for path in sorted((ROOT / TRACKED_PLUGIN).rglob("*")):
        if path.is_file():
            files[path.relative_to(ROOT).as_posix()] = hashlib.sha256(path.read_bytes()).hexdigest()
    return files


def render(lock: dict) -> str:
    return json.dumps(lock, indent=2, sort_keys=True) + "\n"


def problems(lock: dict) -> list:
    out = []
    files = lock.get("files", {})
    want = tree_files()
    have = {k: v for k, v in files.items() if k.startswith(TRACKED_PLUGIN + "/")}
    if have != want:
        out.append("the %s entries of lock.json differ from the tree" % TRACKED_PLUGIN)
    goal = {k for k in files if k.startswith("goal/")}
    if goal != {"goal/" + n for n in GOAL_FILES}:
        out.append("lock.json goal entries %s are not the five expected files" % sorted(goal))
    for k in sorted(goal):
        if not re.fullmatch(r"[0-9a-f]{64}", str(files[k])):
            out.append("%s is not a SHA-256" % k)
    other = sorted(k for k in files if not k.startswith(("goal/", TRACKED_PLUGIN + "/")))
    if other:
        out.append("lock.json lists files outside the two plugin folders: %s" % other)
    return out


def main(argv: list) -> int:
    path = ROOT / "lock.json"
    lock = json.loads(path.read_text())
    if argv[1:] == ["--write"]:
        lock["files"] = {**{k: v for k, v in lock["files"].items() if not k.startswith(TRACKED_PLUGIN + "/")}, **tree_files()}
        path.write_text(render(lock))
        print("GOAL-PLUGIN-LOCK WROTE %d observer files" % len(tree_files()))
        return 0
    if argv[1:]:
        print("usage: lock.py [--write]", file=sys.stderr)
        return 2
    found = problems(lock)
    if not found and path.read_text() == render(lock):
        print("GOAL-PLUGIN-LOCK PASS %d files" % len(lock["files"]))
        return 0
    for p in found:
        print("GOAL-PLUGIN-LOCK " + p)
    if not found:
        print("GOAL-PLUGIN-LOCK lock.json is not in canonical form")
    print("GOAL-PLUGIN-LOCK FAIL: run engine/plugins/build/lock.py --write (observer entries) or scripts/engine-fetch update goal-plugin (goal entries)")
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
