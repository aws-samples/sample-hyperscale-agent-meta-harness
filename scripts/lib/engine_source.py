#!/usr/bin/env python3
"""engine_source.py: the engine source files of `engine/sources/`.

One small TOML file per engine or plugin declares where its source comes from and what the fetch must find there. `scripts/engine-fetch`
reads these files; nothing else decides a pin. A source file declares its kind, URL, pin (a full commit or a SHA-256, never a branch or the word
"head"), content hashes, the build recipe id and the SPDX licence id.

Kinds (the fetch adapters in engine_fetch.py):
  git-tag           a git repository at a tag: url, tag, tag_object, commit; optionally `patches_lock`, the existing lock that lists the patch
                    series and the tree it must give (engine/cline/lock.toml). The file holds the Cline pin once; `check` holds the older
                    locks to it.
  git-path-bundle   named paths of a git repository at one commit: url, commit and one [[files]] table per path (path, blob, sha256, as);
                    [[inputs]] tables take a path from another source (`from`) at that source's own pin.
  release-asset     a published download per platform: [assets.<platform>] with url, sha256 and optionally the archive member and its sha256.

The reader accepts a strict subset of TOML, the same subset the other locks of this repository use plus arrays of tables: full-line and trailing
comments, [table] and [[array table]] headers, `key = "string"`, `key = 123`, `key = true|false` and one-line arrays of strings. A repeated key, a
repeated table, an unknown escape or any other syntax is an error: a partial parse never passes. `rewrite` changes one value in place and keeps every
comment and every other line byte for byte (the only writer `engine-fetch update` uses).

Python 3.8+, standard library only (no tomllib).
"""

from __future__ import annotations

import os
import re
import sys
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

KINDS = ("git-tag", "git-path-bundle", "release-asset")
RECIPES = ("cline-fork", "esbuild-bundle", "none")
PLATFORMS = ("any", "linux-x64", "linux-arm64", "darwin-x64", "darwin-arm64")

HEX40 = re.compile(r"^[0-9a-f]{40}$")
HEX64 = re.compile(r"^[0-9a-f]{64}$")
NAME = re.compile(r"^[a-z][a-z0-9-]*$")
TAG = re.compile(r"^[A-Za-z0-9._/-]+$")
REL_PATH = re.compile(r"^[A-Za-z0-9._@-]+(?:/[A-Za-z0-9._@-]+)*$")
# An SPDX licence expression, reduced to ids joined by AND, OR and WITH.
SPDX = re.compile(r"^[A-Za-z0-9.+-]+(?: (?:AND|OR|WITH) [A-Za-z0-9.+-]+)*$")
URL = re.compile(r"^https://[A-Za-z0-9._~:/?#@!$&'()*+,;=%-]+$")


class SourceError(Exception):
    """A source file that is not valid. `problems` lists every reason."""

    def __init__(self, message: str, problems: Optional[List[str]] = None):
        super().__init__(message)
        self.problems = problems or [message]


_HEADER = re.compile(r"^\[(\[)?([A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)*)(?(1)\])\]\s*(?:#.*)?$")
_KEY = re.compile(r"^([A-Za-z0-9_-]+)\s*=\s*(.+?)\s*$")
_STRING = re.compile(r'^"((?:[^"\\]|\\["\\])*)"')
_INT = re.compile(r"^-?[0-9]+")


def _unescape(s: str) -> str:
    return s.replace('\\"', '"').replace("\\\\", "\\")


def _value(text: str, where: str) -> Any:
    """One value and nothing after it but an optional comment."""
    m = _STRING.match(text)
    if m:
        rest = text[m.end():].strip()
        if rest and not rest.startswith("#"):
            raise SourceError("%s: text after the string: %s" % (where, rest[:40]))
        return _unescape(m.group(1))
    if text.startswith("["):
        items = []  # type: List[str]
        i = 1
        while True:
            while i < len(text) and text[i] in " \t":
                i += 1
            if i < len(text) and text[i] == "]":
                i += 1
                break
            m = _STRING.match(text[i:])
            if not m:
                raise SourceError("%s: an array holds strings on one line" % where)
            items.append(_unescape(m.group(1)))
            i += m.end()
            while i < len(text) and text[i] in " \t":
                i += 1
            if i < len(text) and text[i] == ",":
                i += 1
            elif i < len(text) and text[i] == "]":
                i += 1
                break
            else:
                raise SourceError("%s: an array needs commas" % where)
        rest = text[i:].strip()
        if rest and not rest.startswith("#"):
            raise SourceError("%s: text after the array" % where)
        return items
    for word, val in (("true", True), ("false", False)):
        if text == word or text.startswith(word + " ") or text.startswith(word + "\t") or text.startswith(word + "#"):
            rest = text[len(word):].strip()
            if rest and not rest.startswith("#"):
                raise SourceError("%s: text after %s" % (where, word))
            return val
    m = _INT.match(text)
    if m:
        rest = text[m.end():].strip()
        if rest and not rest.startswith("#"):
            raise SourceError("%s: text after the number" % where)
        return int(m.group(0))
    raise SourceError("%s: unsupported value: %s" % (where, text[:40]))


def parse(text: str, where: str = "source") -> Dict[str, Any]:
    """The strict subset as nested dicts; an array of tables is a list of dicts."""
    root = {}  # type: Dict[str, Any]
    cur = root
    seen_tables = set()  # type: set
    for n, raw in enumerate(text.splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        loc = "%s:%d" % (where, n)
        h = _HEADER.match(line)
        if h:
            is_array, name = bool(h.group(1)), h.group(2)
            parts = name.split(".")
            parent = root
            for p in parts[:-1]:
                nxt = parent.setdefault(p, {})
                if isinstance(nxt, list):
                    nxt = nxt[-1]
                if not isinstance(nxt, dict):
                    raise SourceError("%s: %s is not a table" % (loc, p))
                parent = nxt
            last = parts[-1]
            if is_array:
                arr = parent.setdefault(last, [])
                if not isinstance(arr, list):
                    raise SourceError("%s: %s is not an array of tables" % (loc, name))
                cur = {}
                arr.append(cur)
            else:
                if name in seen_tables or last in parent:
                    raise SourceError("%s: repeated table [%s]" % (loc, name))
                seen_tables.add(name)
                cur = {}
                parent[last] = cur
            continue
        k = _KEY.match(line)
        if not k:
            raise SourceError("%s: unsupported syntax: %s" % (loc, line[:60]))
        key = k.group(1)
        if key in cur:
            raise SourceError("%s: repeated key %s" % (loc, key))
        cur[key] = _value(k.group(2), loc)
    return root


def rewrite(text: str, table: str, key: str, new: str, index: Optional[int] = None) -> str:
    """`text` with the string value of `key` in `table` replaced by `new`. `table` is "" for the top level, "source" for [source], "files" with
    `index` for the index-th [[files]]. Everything else, comments included, stays byte for byte; a trailing comment on the line stays too.
    Raises SourceError when the key is not there."""
    lines = text.splitlines(keepends=True)
    cur = ""
    counts = {}  # type: Dict[str, int]
    for i, raw in enumerate(lines):
        line = raw.strip()
        h = _HEADER.match(line)
        if h:
            cur = h.group(2)
            if h.group(1):
                counts[cur] = counts.get(cur, -1) + 1
            continue
        if cur != table or (index is not None and counts.get(cur, -1) != index):
            continue
        m = re.match(r"^(\s*" + re.escape(key) + r'\s*=\s*)"((?:[^"\\]|\\["\\])*)"(.*)$', raw.rstrip("\n"))
        if m:
            nl = "\n" if raw.endswith("\n") else ""
            lines[i] = m.group(1) + '"' + new.replace("\\", "\\\\").replace('"', '\\"') + '"' + m.group(3) + nl
            return "".join(lines)
    raise SourceError("rewrite: no %s in [%s]%s" % (key, table, "" if index is None else "#%d" % index))


class Source:
    """A parsed and validated source file."""

    def __init__(self, path: Path, raw: Dict[str, Any]):
        self.path = path
        self.raw = raw
        self.name = raw.get("name", "")
        self.kind = raw.get("kind", "")
        self.recipe = raw.get("recipe", "none")
        self.license = raw.get("license", "")

    def table(self, name: str) -> Dict[str, Any]:
        v = self.raw.get(name)
        return v if isinstance(v, dict) else {}

    def tables(self, name: str) -> List[Dict[str, Any]]:
        v = self.raw.get(name)
        return v if isinstance(v, list) else []

    def asset(self, platform: str) -> Dict[str, Any]:
        """The [assets.<platform>] table, else [assets.any]."""
        a = self.table("assets")
        v = a.get(platform) or a.get("any")
        return v if isinstance(v, dict) else {}


def _need(problems: List[str], where: str, table: Dict[str, Any], key: str, rx: "re.Pattern[str]", what: str) -> None:
    v = table.get(key)
    if not isinstance(v, str) or not v:
        problems.append("%s: %s is missing" % (where, key))
    elif not rx.match(v):
        problems.append("%s: %s must be %s (got %r)" % (where, key, what, v))


def _unknown(problems: List[str], where: str, table: Dict[str, Any], allowed: Tuple[str, ...]) -> None:
    for k in table:
        if k not in allowed:
            problems.append("%s: unknown key %s" % (where, k))


def validate(raw: Dict[str, Any], stem: str, allow_local_url: bool = False) -> List[str]:
    """Every problem of a parsed source file. `stem` is the file name without .toml; `name` must equal it.
    `allow_local_url` accepts a file path or file:// URL (the test fixtures), never in the repository."""
    problems = []  # type: List[str]
    url = URL if not allow_local_url else re.compile(r"^(?:https://[^\s]+|file://[^\s]+|/[^\s]+)$")
    if raw.get("schema") != "1":
        problems.append('schema must be "1"')
    if raw.get("name") != stem:
        problems.append("name must be the file name %r (got %r)" % (stem, raw.get("name")))
    kind = raw.get("kind")
    if kind not in KINDS:
        problems.append("kind must be one of %s (got %r)" % (", ".join(KINDS), kind))
    if not isinstance(raw.get("license"), str) or not SPDX.match(raw.get("license") or ""):
        problems.append("license must be an SPDX id such as Apache-2.0 (got %r)" % (raw.get("license"),))
    if raw.get("recipe") not in RECIPES:
        problems.append("recipe must be one of %s (got %r)" % (", ".join(RECIPES), raw.get("recipe")))
    common = ("schema", "name", "kind", "license", "recipe", "description")
    if kind == "git-tag":
        _unknown(problems, "top level", {k: v for k, v in raw.items() if not isinstance(v, (dict, list))}, common)
        src = raw.get("source") if isinstance(raw.get("source"), dict) else {}
        _need(problems, "[source]", src, "url", url, "an https URL")
        _need(problems, "[source]", src, "tag", TAG, "a tag name")
        _need(problems, "[source]", src, "tag_object", HEX40, "a 40-hex tag object id")
        _need(problems, "[source]", src, "commit", HEX40, "a full 40-hex commit")
        _unknown(problems, "[source]", src, ("url", "tag", "tag_object", "commit", "patches_lock"))
        pl = src.get("patches_lock")
        if pl is not None and (not isinstance(pl, str) or not REL_PATH.match(pl)):
            problems.append("[source]: patches_lock must be a repository path")
    elif kind == "git-path-bundle":
        _unknown(problems, "top level", {k: v for k, v in raw.items() if not isinstance(v, (dict, list))}, common)
        src = raw.get("source") if isinstance(raw.get("source"), dict) else {}
        _need(problems, "[source]", src, "url", url, "an https URL")
        _need(problems, "[source]", src, "commit", HEX40, "a full 40-hex commit")
        _unknown(problems, "[source]", src, ("url", "commit", "resolved_from"))
        files = raw.get("files") if isinstance(raw.get("files"), list) else []
        inputs = raw.get("inputs") if isinstance(raw.get("inputs"), list) else []
        if not files:
            problems.append("a git-path-bundle lists at least one [[files]]")
        seen = set()  # type: set
        for label, group, extra in (("files", files, ()), ("inputs", inputs, ("from",))):
            for i, t in enumerate(group):
                where = "[[%s]]#%d" % (label, i + 1)
                _need(problems, where, t, "path", REL_PATH, "a repository path")
                _need(problems, where, t, "blob", HEX40, "a 40-hex git blob id")
                _need(problems, where, t, "sha256", HEX64, "a SHA-256")
                _need(problems, where, t, "as", REL_PATH, "a relative layout path")
                if label == "inputs":
                    _need(problems, where, t, "from", NAME, "the name of another source")
                _unknown(problems, where, t, ("path", "blob", "sha256", "as") + extra)
                dest = t.get("as")
                if dest in seen:
                    problems.append("%s: layout path %s is used twice" % (where, dest))
                seen.add(dest)
                if isinstance(dest, str) and (dest.startswith("/") or ".." in dest.split("/")):
                    problems.append("%s: as must stay inside the layout" % where)
    elif kind == "release-asset":
        _unknown(problems, "top level", {k: v for k, v in raw.items() if not isinstance(v, (dict, list))}, common + ("version",))
        assets = raw.get("assets") if isinstance(raw.get("assets"), dict) else {}
        if not assets:
            problems.append("a release-asset has at least one [assets.<platform>] table")
        for plat, t in assets.items():
            where = "[assets.%s]" % plat
            if plat not in PLATFORMS:
                problems.append("%s: platform must be one of %s" % (where, ", ".join(PLATFORMS)))
            if not isinstance(t, dict):
                problems.append("%s: not a table" % where)
                continue
            _need(problems, where, t, "url", url, "an https URL")
            _need(problems, where, t, "sha256", HEX64, "a SHA-256")
            if "member" in t:
                _need(problems, where, t, "member", REL_PATH, "an archive member path")
                _need(problems, where, t, "member_sha256", HEX64, "a SHA-256")
            elif "member_sha256" in t:
                problems.append("%s: member_sha256 without member" % where)
            _unknown(problems, where, t, ("url", "sha256", "member", "member_sha256"))
    return problems


def load(path: Path, allow_local_url: bool = False) -> Source:
    """Reads and validates one source file; raises SourceError listing every problem."""
    path = Path(path)
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as e:
        raise SourceError("%s: %s" % (path, e))
    raw = parse(text, str(path))
    problems = validate(raw, path.stem, allow_local_url)
    if problems:
        raise SourceError("%s: %d problem(s)" % (path, len(problems)), ["%s: %s" % (path, p) for p in problems])
    return Source(path, raw)


def load_all(directory: Path, allow_local_url: bool = False) -> Dict[str, Source]:
    """Every *.toml of `directory` by name; the problems of all files are raised together."""
    out = {}  # type: Dict[str, Source]
    problems = []  # type: List[str]
    for p in sorted(Path(directory).glob("*.toml")):
        try:
            out[p.stem] = load(p, allow_local_url)
        except SourceError as e:
            problems.extend(e.problems)
    if problems:
        raise SourceError("%d problem(s) in %s" % (len(problems), directory), problems)
    return out


def main(argv: List[str]) -> int:
    """`engine_source.py [DIR]`: validates every source file of DIR (default engine/sources of this repository)."""
    root = Path(__file__).resolve().parents[2]
    d = Path(argv[1]) if len(argv) > 1 else root / "engine" / "sources"
    try:
        found = load_all(d)
    except SourceError as e:
        for p in e.problems:
            print("SOURCE-LINT %s" % p)
        print("SOURCE-LINT FAIL")
        return 1
    print("SOURCE-LINT PASS %d source file(s): %s" % (len(found), ", ".join(sorted(found))))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
