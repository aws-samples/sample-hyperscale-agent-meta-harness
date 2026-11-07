#!/usr/bin/env python3
"""lock.py: pin the lifecycle pack by SHA-256 and keep the list of its earlier texts.

lifecycle/pack.lock.json holds the pack version and the SHA-256 of every file under lifecycle/pack/ (paths relative to lifecycle/pack/,
hidden folders such as .agents included, __pycache__ left out). A run records the version beside its rule digests.

lifecycle/pack.history.json sits beside it, outside pack/ (so the lock does not list it and a history change is no pack change). It holds, for
each path of the pack, the SHA-256 of every earlier text of the file and the last pack version whose lock held that text. `ham rules init`
reads it (crates/ham-core/src/pack.rs embeds it): an installed file equal to one of those texts was never edited, so the installer may
replace it with the current text; any other file is the user's and is kept.

  lock.py                      check: print one line per problem, then LIFECYCLE-LOCK PASS or LIFECYCLE-LOCK FAIL (exit 0 or 1)
                               (the lock against the files, the history against the lock)
  lock.py --write              rewrite pack.lock.json from the files; the version stays as it is, the history gains nothing
  lock.py --write --version X  rewrite it and set the version (X.Y.Z); bump it when the pack text changes. When the version changes, the old
                               lock's hash of every file whose text changed joins the history with the old version
  lock.py --write --version X --earlier PATH=FILE
                               also list the text in FILE as an earlier text of the pack path PATH, with the old version. The contract has
                               three texts (full, quick, auto) and the pack holds only auto, so the other two of the version being replaced
                               are named this way: --earlier AGENTS.md=<the old full text> --earlier AGENTS.md=<the old quick text>. It needs
                               a new version, and the files are read before anything is written
  lock.py --backfill-history   rebuild pack.history.json from the git history of pack.lock.json: every lock state older than the current
                               version (use it once, and after a merge that left the history in conflict; never edit the file by hand).
                               It knows only the lock states, so the texts named with --earlier are not in what it writes
  --root DIR                   the lifecycle directory (default: the folder of this script)

Every write drops from the history a hash that is the current text of its path, and a hash keeps the latest version that held it. Standard
library only (git for --backfill-history); Python 3.8 or later.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess  # nosec B404: runs git with an argument list, starts nothing else
import sys
from pathlib import Path

LOCK = "pack.lock.json"
HISTORY = "pack.history.json"
FIRST_VERSION = "1.0.0"
VERSION = re.compile(r"\d+\.\d+\.\d+")
DIGEST = re.compile(r"[0-9a-f]{64}")


def pack_files(pack: Path) -> dict:
    """{path relative to pack, posix: sha256} for every regular file under pack."""
    return {p.relative_to(pack).as_posix(): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in sorted(pack.rglob("*")) if p.is_file() and "__pycache__" not in p.parts}


def render(version: str, files: dict) -> str:
    return json.dumps({"version": version, "files": dict(sorted(files.items()))}, indent=2, sort_keys=True) + "\n"


def render_history(history: dict) -> str:
    """The canonical text of pack.history.json: {"files": {path: {sha256: version}}}, keys sorted, two-space indent, final newline."""
    files = {path: dict(sorted(hashes.items())) for path, hashes in sorted(history.items()) if hashes}
    return json.dumps({"files": files}, indent=2, sort_keys=True) + "\n"


def version_key(version: str) -> tuple:
    """1.0.10 is newer than 1.0.9: versions compare as numbers."""
    return tuple(int(n) for n in version.split("."))


def parse_lock(text: str):
    """(version, files) of a lock text, or None when it is not a lock."""
    try:
        recorded = json.loads(text)
        version, files = recorded["version"], recorded["files"]
    except (ValueError, KeyError, TypeError):
        return None
    if not isinstance(version, str) or not VERSION.fullmatch(version) or not isinstance(files, dict):
        return None
    return version, files


def read_lock(root: Path):
    path = root / LOCK
    return parse_lock(path.read_text(encoding="utf-8")) if path.is_file() else None


def parse_history(text: str):
    """(history, problems) of a history text: {path: {sha256: version}} and one line per fault; the history holds the valid entries only."""
    try:
        recorded = json.loads(text)
    except ValueError:
        return {}, ["%s is not JSON with `files`" % HISTORY]
    if not isinstance(recorded, dict) or sorted(recorded) != ["files"] or not isinstance(recorded["files"], dict):
        return {}, ["%s is not JSON with `files` and nothing else" % HISTORY]
    history, out = {}, []
    for path, hashes in recorded["files"].items():
        if not isinstance(hashes, dict):
            out.append("%s: %s lists %r, not a map of hash to version" % (HISTORY, path, hashes))
            continue
        for digest, version in hashes.items():
            if not DIGEST.fullmatch(digest):
                out.append("%s: %s: %r is not a lower-case SHA-256" % (HISTORY, path, digest))
            elif not isinstance(version, str) or not VERSION.fullmatch(version):
                out.append("%s: %s: %s names the version %r, not X.Y.Z" % (HISTORY, path, digest[:12], version))
            else:
                history.setdefault(path, {})[digest] = version
    return history, out


def read_history(root: Path):
    """(history, problems): an absent file is an empty history with no problem, so the first --write creates it."""
    path = root / HISTORY
    return parse_history(path.read_text(encoding="utf-8")) if path.is_file() else ({}, [])


def add_state(history: dict, version: str, files: dict, current: dict) -> None:
    """Add the texts of one lock state (version, files) that are no longer current. A hash keeps the latest version that held it."""
    for path, digest in files.items():
        if current.get(path) == digest or not isinstance(digest, str) or not DIGEST.fullmatch(digest):
            continue
        held = history.setdefault(path, {})
        if digest not in held or version_key(held[digest]) < version_key(version):
            held[digest] = version


def drop_current(history: dict, current: dict) -> None:
    """A text that is the current text of its path is not an earlier text."""
    for path in list(history):
        history[path].pop(current.get(path), None)
        if not history[path]:
            del history[path]


def history_problems(root: Path, version: str, listed: dict) -> list:
    path = root / HISTORY
    if not path.is_file():
        return ["%s not found (lock.py --backfill-history writes it)" % HISTORY]
    text = path.read_text(encoding="utf-8")
    history, out = parse_history(text)
    for rel in sorted(history):
        for digest, earlier in sorted(history[rel].items()):
            if listed.get(rel) == digest:
                out.append("%s: %s is the current text but is listed as earlier (pack %s)" % (rel, digest[:12], earlier))
            if version_key(earlier) >= version_key(version):
                out.append("%s: %s names pack %s, which is not older than the lock's %s" % (rel, digest[:12], earlier, version))
    if not out and text != render_history(history):
        out.append("%s is not in the canonical form that --write produces" % HISTORY)
    return out


def problems(root: Path) -> list:
    path = root / LOCK
    if not path.is_file():
        return ["%s not found" % LOCK]
    text = path.read_text(encoding="utf-8")
    try:
        recorded = json.loads(text)
        version, listed = recorded["version"], recorded["files"]
    except (ValueError, KeyError, TypeError):
        return ["%s is not JSON with `version` and `files`" % LOCK]
    out = []
    valid_version = isinstance(version, str) and VERSION.fullmatch(version) is not None
    if not valid_version:
        out.append("version %r is not X.Y.Z" % (version,))
    actual = pack_files(root / "pack")
    for rel in sorted(set(listed) | set(actual)):
        if rel not in actual:
            out.append("%s: in the lock but missing" % rel)
        elif rel not in listed:
            out.append("%s: not in the lock" % rel)
        elif listed[rel] != actual[rel]:
            out.append("%s: hash differs" % rel)
    if not out and text != render(version, listed):
        out.append("%s is not in the canonical form that --write produces" % LOCK)
    if valid_version:
        out.extend(history_problems(root, version, listed))
    return out


def git_lines(root: Path, *args: str) -> str:
    """The output of `git -C root args`; a failure raises RuntimeError with git's first line."""
    done = subprocess.run(["git", "-C", str(root), *args], capture_output=True)  # nosec B603: argument list, no shell, program is git; B607: bare name git looked up on PATH
    if done.returncode != 0:
        lines = done.stderr.decode("utf-8", "replace").strip().splitlines()
        raise RuntimeError(lines[0] if lines else "git %s failed" % args[0])
    return done.stdout.decode("utf-8", "replace")


def lock_states(root: Path) -> list:
    """[(version, files)] for every distinct committed state of pack.lock.json in the history of HEAD, side branches included."""
    prefix = git_lines(root, "rev-parse", "--show-prefix").strip()
    # --full-history: the default history of a file hides the commits of a merged side branch whose merge kept the other side.
    commits = git_lines(root, "log", "--full-history", "--format=%H", "--", LOCK).split()
    request = "".join("%s:%s%s\n" % (c, prefix, LOCK) for c in commits)
    done = subprocess.run(["git", "-C", str(root), "cat-file", "--batch"], input=request.encode(), capture_output=True)  # nosec B603: argument list, no shell, program is git; B607: bare name git looked up on PATH
    if done.returncode != 0:
        raise RuntimeError("git cat-file failed")
    data, pos, seen, states = done.stdout, 0, set(), []
    while pos < len(data):
        end = data.index(b"\n", pos)
        head = data[pos:end].decode("utf-8", "replace").split()
        pos = end + 1
        if len(head) == 3 and head[1] == "blob":
            size = int(head[2])
            body, pos = data[pos:pos + size], pos + size + 1
            if head[0] not in seen:
                seen.add(head[0])
                state = parse_lock(body.decode("utf-8", "replace"))
                if state is not None:
                    states.append(state)
    return states


def backfill(root: Path) -> int:
    """Rebuild pack.history.json from the git history of the lock: every state older than the current lock's version."""
    current = read_lock(root)
    if current is None:
        print("LIFECYCLE-LOCK %s is not found or is not a lock; run --write first" % LOCK)
        return 2
    version, files = current
    try:
        states = lock_states(root)
    except (RuntimeError, OSError) as e:
        print("LIFECYCLE-LOCK cannot read the git history of %s: %s" % (LOCK, e))
        return 2
    history, used = {}, 0
    for state_version, state_files in states:
        if version_key(state_version) < version_key(version):
            add_state(history, state_version, state_files, files)
            used += 1
    drop_current(history, files)
    (root / HISTORY).write_text(render_history(history), encoding="utf-8")
    print("LIFECYCLE-LOCK BACKFILLED %s from %d older of %d lock states: %d hashes of %d files" %
          (HISTORY, used, len(states), sum(len(h) for h in history.values()), len(history)))
    return 0


def main(argv: list) -> int:
    parser = argparse.ArgumentParser(description="Pin the lifecycle pack by SHA-256 and keep the list of its earlier texts.")
    parser.add_argument("--root", default=str(Path(__file__).resolve().parent))
    parser.add_argument("--write", action="store_true", help="rewrite pack.lock.json from the files")
    parser.add_argument("--version", help="with --write, set the pack version (X.Y.Z)")
    parser.add_argument("--backfill-history", action="store_true", help="rebuild pack.history.json from the git history of pack.lock.json")
    parser.add_argument("--earlier", action="append", default=[], metavar="PATH=FILE",
                        help="with --write and a new --version, list the text in FILE as an earlier text of the pack path PATH (repeatable)")
    args = parser.parse_args(argv)
    root = Path(args.root)
    if args.version and not args.write:
        parser.error("--version needs --write")
    if args.earlier and not args.write:
        parser.error("--earlier needs --write")
    earlier = []
    for spec in args.earlier:
        rel, _, name = spec.partition("=")
        if not rel or not name:
            parser.error("--earlier takes PATH=FILE, got %r" % (spec,))
        earlier.append((rel, name))
    if args.backfill_history and (args.write or args.version):
        parser.error("--backfill-history stands alone")
    if args.backfill_history:
        return backfill(root)
    if args.write:
        version = args.version
        try:
            old = read_lock(root)
        except (OSError, ValueError):
            old = None
        if version is None:
            version = old[0] if old else FIRST_VERSION
        if not VERSION.fullmatch(str(version)):
            parser.error("the version must be X.Y.Z, got %r" % (version,))
        history, bad = read_history(root)
        if bad:
            for line in bad:
                print("LIFECYCLE-LOCK %s" % line)
            print("LIFECYCLE-LOCK nothing written; fix %s or rebuild it with --backfill-history" % HISTORY)
            return 2
        files = pack_files(root / "pack")
        if earlier and (old is None or old[0] == version):
            print("LIFECYCLE-LOCK --earlier needs a new --version, so that the texts name the version they replace; nothing written")
            return 2
        texts = {}
        for rel, name in earlier:
            try:
                texts[(rel, name)] = hashlib.sha256(Path(name).read_bytes()).hexdigest()
            except OSError as e:
                print("LIFECYCLE-LOCK cannot read %s for --earlier %s: %s; nothing written" % (name, rel, e.strerror or e))
                return 2
        if old is not None and old[0] != version:
            add_state(history, old[0], old[1], files)
            for (rel, _), digest in texts.items():
                add_state(history, old[0], {rel: digest}, files)
        drop_current(history, files)
        (root / LOCK).write_text(render(version, files), encoding="utf-8")
        (root / HISTORY).write_text(render_history(history), encoding="utf-8")
        print("LIFECYCLE-LOCK WROTE version %s, %d files" % (version, len(files)))
        return 0
    found = problems(root)
    for line in found:
        print("LIFECYCLE-LOCK %s" % line)
    print("LIFECYCLE-LOCK %s" % ("FAIL" if found else "PASS"))
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
