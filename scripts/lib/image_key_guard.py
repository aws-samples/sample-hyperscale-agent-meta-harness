"""The worker image key guard: implementation of scripts/image-key-guard.sh.

The worker image is tagged `in-<key>`; the key hashes agentd's crate closure, the Cargo files,
accounts.json, engine/ outside engine/catalog and engine/policy, and the image sources
(digests.worker_image_key, the same value deploy.py tags the image with). The guard compares the
key of a tree with the pin in engine/policy/image-key.pin (outside the key, so updating the pin
never moves it) and fails when they differ.

usage: image_key_guard.py [--root DIR] [--staged] [--update]

  (no flag)   the key of HEAD against the pin committed at HEAD
  --staged    the key of the index tree against the pin in the index
  --update    --staged, then rewrite the pin file in the working tree when it differs; the commit that
              changes the key on purpose stages the change, runs this, stages the pin and commits

Prints one line: `IMAGE-KEY PASS <key>`, `IMAGE-KEY UPDATED <old|none> -> <new>` or
`IMAGE-KEY FAIL computed=<key> pin=<pin|none|malformed> ...` (exit 1). Usage errors exit 2.
Test hook: CI_CARGO_BIN (default `cargo`) runs `metadata --format-version 1 --locked` in the root.
Python 3.8+, standard library only.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess  # nosec B404: starts programs with argument lists, never through a shell
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import digests  # noqa: E402

PIN = "engine/policy/image-key.pin"
KEY = re.compile(r"[0-9a-f]{64}")
HEADER = (
    "# Worker image key pin. The image-key check compares it with the key\n"
    "# computed from the committed tree. A commit that changes the key on purpose also runs\n"
    "# `scripts/image-key-guard.sh --update`. Never edit by hand.\n"
)
HOWTO = ("a commit that changes the worker image inputs also updates {pin}: stage the change, run "
         "scripts/image-key-guard.sh --update, stage the pin (scripts/AGENTS.md)").format(pin=PIN)


class GuardError(Exception):
    """Reported as `IMAGE-KEY FAIL <message>`."""


def git(root: Path, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run(["git", "-C", str(root), *args], capture_output=True, text=True)  # nosec B603: argument list built here, no shell, program is git, the interpreter or a configured binary; B607: bare name of a standard tool such as git, bash, tar or aws looked up on the PATH of the host running this tool


def cargo_metadata(root: Path) -> dict:
    cargo = os.environ.get("CI_CARGO_BIN", "cargo")
    try:
        proc = subprocess.run([cargo, "metadata", "--format-version", "1", "--locked"], cwd=root,  # nosec B603: argument list built here, no shell, program is git, the interpreter or a configured binary  # nosemgrep: dangerous-subprocess-use-audit, dangerous-subprocess-use-tainted-env-args -- argument list built here from its own options and paths, no shell; cargo program from the documented test hook CI_CARGO_BIN, argument list, no shell
                              capture_output=True, text=True)
    except OSError as e:
        raise GuardError(f"cargo-metadata: {e}")
    if proc.returncode != 0:
        raise GuardError("cargo-metadata: " + " ".join(proc.stderr.split())[-200:])
    try:
        return json.loads(proc.stdout)
    except ValueError as e:
        raise GuardError(f"cargo-metadata: unreadable output ({e})")


def tree_of(root: Path, staged: bool) -> str:
    if not staged:
        return "HEAD"
    proc = git(root, "write-tree")
    if proc.returncode != 0:
        raise GuardError("git-write-tree: " + " ".join(proc.stderr.split())[-200:])
    return proc.stdout.strip()


def parse_pin(text: str) -> str:
    """The one key line of a pin file; comment lines and blank lines are ignored."""
    lines = [ln.strip() for ln in text.splitlines() if ln.strip() and not ln.lstrip().startswith("#")]
    if len(lines) != 1 or not KEY.fullmatch(lines[0]):
        raise ValueError("expected one line of 64 lowercase hex characters")
    return lines[0]


def committed_pin(root: Path, staged: bool) -> tuple:
    """(pin, problem): the pin at HEAD or in the index; `pin` is None when absent or malformed."""
    proc = git(root, "show", (":" if staged else "HEAD:") + PIN)
    if proc.returncode != 0:
        return None, "no pin"
    try:
        return parse_pin(proc.stdout), None
    except ValueError as e:
        return None, f"malformed pin: {e}"


def run(root: Path, staged: bool, update: bool) -> str:
    key = digests.worker_image_key(root, cargo_metadata(root), tree_of(root, staged or update))
    if update:
        path = root / PIN
        old = None
        if path.is_file():
            try:
                old = parse_pin(path.read_text())
            except ValueError:
                old = None
        if old == key:
            return f"IMAGE-KEY PASS {key} (pin already current)"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(f"{HEADER}{key}\n")
        return f"IMAGE-KEY UPDATED {old or 'none'} -> {key}"
    pin, problem = committed_pin(root, staged)
    if pin == key:
        return f"IMAGE-KEY PASS {key}"
    if pin is None:
        raise GuardError(f"computed={key} pin={'none' if problem == 'no pin' else 'malformed'} - {problem}; {HOWTO}")
    raise GuardError(f"computed={key} pin={pin} - the worker image inputs changed; {HOWTO}")


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(prog="image-key-guard.sh", add_help=True,
                                 usage="image-key-guard.sh [--root DIR] [--staged] [--update]")
    ap.add_argument("--root", default=str(Path(__file__).resolve().parents[2]))
    ap.add_argument("--staged", action="store_true")
    ap.add_argument("--update", action="store_true")
    args = ap.parse_args(argv)
    try:
        print(run(Path(args.root).resolve(), args.staged, args.update))
        return 0
    except GuardError as e:
        print(f"IMAGE-KEY FAIL {e}")
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
