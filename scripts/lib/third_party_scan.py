#!/usr/bin/env python3
"""third_party_scan.py: no third-party engine or plugin source in the tracked files.

The rule: coding engines, harnesses and plugins are fetched at setup from a pinned source and never committed. The repository keeps the pin
(URL, tag or commit, SHA-256 or tree hash), our patches and the fetch component (scripts/engine-fetch, engine/sources/). This module is the check
that holds the tree to the rule. It reads `git ls-files -s` (modes and paths of the tracked files) and the bytes of those files from the work tree.

Hard rules (each finding names its rule):
                    source, the upstream licence texts). A rename or a move does not hide a copy.
  licence-file      a file named LICENSE, LICENCE, COPYING, NOTICE or PROVENANCE (any dot suffix), or, in any case, license, licence, copying,
                    copyright, notice, provenance, unlicense or third-party-notices with no suffix, a text suffix (.txt .md .rst) or the name of a
                    licence (LICENSE-MIT, LICENSE_APACHE), other than the two of the repository root. A copied licence text is the sign of copied code.
  gitlink           a submodule entry (mode 160000) or a .gitmodules file: the fetched code would sit in the tree by reference.
  symlink           a symbolic link other than the listed ones.
  vendor-dir        a directory named node_modules, vendor, vendored, third_party, third-party, thirdparty, 3rdparty, deps or external, in any
                    case.
  engine-layout     a tracked file under engine/ that is none of the kinds the layout allows (ENGINE_ALLOWED): AGENTS.md, locks, profiles, the
                    catalog, the patch series, the source declarations, the authored recipe files and our own observer plugin.
  engine-size       a file under engine/ over 64 KiB that is not a patch, or with a NUL byte in it.
  bundle-comment    a JavaScript or TypeScript file whose first line is the path comment esbuild puts above each bundled module.
  long-line         a JavaScript file with a line over 1,000 characters (a minified or bundled file).
  binary-file       an archive, a compiled object (also a versioned shared object such as libfoo.so.1), a source map or a Git LFS pointer.
  rules-copy        a path with a folder named hamh_semgrep_rules or a YAML file that carries the registry url
                    of a semgrep rule (semgrep.dev/r/): the rules are third-party content under the Semgrep Rules License, built from a pinned commit by
                    scripts/scanner_kit_rules.py and published by the operator; the repository keeps the pin (security/scanner-kit.toml) and the tool.

Diagnostics (printed by `engine-fetch check`, never a failure): `header_signals` lists source files whose first lines carry another project's
licence or copyright header. A reviewer reads them; a header can be stripped, so only the rules above block.

Python 3.8+, standard library only.
"""

from __future__ import annotations

import hashlib
import os
import re
import subprocess  # nosec B404: starts git with an argument list, never through a shell
import sys
from pathlib import Path
from typing import Callable, Dict, Iterable, List, NamedTuple, Optional, Tuple

FORBIDDEN_PATHS = {
    "engine/plugins/goal/index.js": "the goal plugin bundle is fetched and bundled at image build",
    "engine/plugins/build/sdk-stub.mjs": "the SDK stand-in is taken from the fetched Cline tag at build",
    "engine/plugins/goal/LICENSE": "the licence text is fetched with the plugin",
}

# SHA-256 of known third-party files; a copy under any name is a finding.
FORBIDDEN_HASHES = {
    "c9b7cae3d805a899fb582970fadf397000bec896a06f946f272d405c5d0b261e": "the old committed goal plugin bundle (cline/plugins plugins/goal, bundled)",
    "802df90480f9a2a745f25fb32483c4244c3bd7c1e075c9ea5a570793b9b6e6be": "the old hand-copied createTool stand-in",
    "8fa09f470ff4b8e1b86472213d0b84abaf07b09e4c6f2da6d914dbacce09dd30": "cline/plugins plugins/goal/index.ts at 96bde661",
    "237584785cc1b8627548897b4268b0c6af88ede8d708842e4d3e71fd1c93b6cb": "the Apache-2.0 text of the old goal plugin folder",
    "f704446a5f1271608805598b557e4288cf8580477ea038c9c3d8b361f693f6b8": "LICENSE of cline/plugins at 96bde661",
    "5ff090d12d8c828446e692e4bed9581f8e69f3bff6be7216df74fffceedd7487": "sdk/packages/shared/src/tools/create.ts of cline/cline at cli-v3.0.71",
    "1a6466ae5f1c68631bbd13e3f1aa8f380d84219853c08f0456b853fccfacee1d": "LICENSE of semgrep/semgrep-rules at 12f2eb5b (the Semgrep Rules License)",
}

# The scanner kit's rules: the package folder, and the registry url that every rule of the semgrep-rules repository carries in its metadata.
RULES_PACKAGE_DIR = "hamh_semgrep_rules"
RULES_REGISTRY_URL = b"semgrep.dev/r/"
RULE_SUFFIXES = {".yaml", ".yml"}

# A licence, notice or provenance file of another project. Two spellings are read: the upper-case names with any dot suffix (LICENSE.html, NOTICE.json),
# and the names in any case when the rest is empty, a text suffix or the name of a licence (license.txt, License, LICENSE-MIT, LICENSE_APACHE, COPYRIGHT,
# UNLICENSE, THIRD-PARTY-NOTICES.md). A code or data file that only carries the word (licence.rs, notice.json) is neither.
LICENCE_NAME = re.compile(r"^(LICENSE|LICENCE|COPYING|NOTICE|PROVENANCE)(\.[A-Za-z0-9_-]+)?$")
LICENCE_STEM = r"(?:licen[cs]e|copying|copyright|notice|provenance|unlicense|third[-_]?party[-_]?(?:notices?|licen[cs]es?))"
LICENCE_KIND = (r"(?:mit(?:[-_.]?0)?|apache(?:[-_.]?2(?:\.0)?)?|bsd(?:[-_.]?[0-9])?(?:[-_.]clause)?|l?gpl(?:[-_.]?v?[0-9](?:\.[0-9])?)?|agpl(?:[-_.]?v?[0-9](?:\.[0-9])?)?"
                r"|mpl(?:[-_.]?2(?:\.0)?)?|isc|cc0|zlib|boost|epl|cddl)")
LICENCE_NAME_ANY_CASE = re.compile(r"^" + LICENCE_STEM + r"(?:[-_.]" + LICENCE_KIND + r")?(?:\.(?:txt|md|rst))?$", re.I)
LICENCE_ALLOWED = {"LICENSE", "NOTICE"}  # the repository root only

SYMLINK_ALLOWED = {"CLAUDE.md"}  # a link to AGENTS.md

VENDOR_DIR_NAMES = {"node_modules", "vendor", "vendored", "third_party", "third-party", "thirdparty", "3rdparty", "deps", "external"}  # compared in lower case
# Directories with such a name that hold the maintainers' own tools, with the reason.
VENDOR_DIR_ALLOWED = {
    "engine/rules/deps": "the approved dependency versions of the n-tier samples: our own data lists, no code (engine/AGENTS.md)",
}

# What engine/ may hold. Everything else under engine/ is third-party until a person decides otherwise and adds the path here.
ENGINE_ALLOWED = [
    r"engine/(?:[\w.-]+/)*AGENTS\.md",
    r"engine/lock\.toml",
    r"engine/cline/(?:lock\.toml|rebase\.sh)",
    r"engine/cline/patches/(?:tdd/)?[0-9]{4}-[\w.-]+\.patch",
    r"engine/catalog/[\w.-]+\.json",
    r"engine/policy/(?:image-key|cdk-bootstrap-template)\.pin",
    r"engine/profiles/[\w.-]+\.toml",
    r"engine/profiles/templates/[\w.-]+\.json",
    r"engine/rules/deps/(?:jvm\.json|go-approved-modules\.txt)",
    r"engine/plugins/lock\.json",
    r"engine/plugins/harness-rounds/(?:index\.js|package\.json)",
    r"engine/plugins/build/(?:lock\.py|observer-unit\.mjs|plugin-unit\.mjs)",
    r"engine/sources/[\w.-]+\.toml",
    r"engine/sources/recipes/[\w.-]+/[\w.-]+",
]
ENGINE_SIZE_LIMIT = 64 * 1024

BINARY_SUFFIXES = {
    ".zip", ".tgz", ".tar", ".gz", ".xz", ".bz2", ".zst", ".7z", ".jar", ".war", ".so", ".dylib", ".dll", ".exe", ".wasm", ".node",
    ".whl", ".rpm", ".deb", ".bin", ".o", ".a", ".pyc", ".map",
}
SHARED_OBJECT = re.compile(r"\.so(?:\.[0-9]+)*$")  # libfoo.so.1.2.3
BUNDLE_SUFFIXES = {".js", ".mjs", ".cjs", ".ts", ".tsx", ".jsx"}
JS_SUFFIXES = {".js", ".mjs", ".cjs"}
ESBUILD_COMMENT = re.compile(r"^// (?:[\w@.-]+/)*[\w@.-]+\.(?:ts|tsx|js|mjs|cjs|jsx)\s*$")
LONG_LINE = 1000
LFS_POINTER = b"version https://git-lfs.github.com/spec/v1"

SIGNAL_SUFFIXES = BUNDLE_SUFFIXES | {".py", ".rs", ".sh", ".go", ".java", ".kt"}
SIGNAL = re.compile(r"^\s*(?://|#|/\*|\*)\s.*(licensed under|copyright \(c\)|copyright\s+[0-9]{4}|spdx-license-identifier)", re.I)


class Entry(NamedTuple):
    """A tracked file: its path, git mode (100644, 100755, 120000 for a link, 160000 for a gitlink)."""

    path: str
    mode: str


class Finding(NamedTuple):
    rule: str
    path: str
    detail: str

    def line(self) -> str:
        return f"{self.rule} {self.path}: {self.detail}"


def tracked_entries(root: Path) -> List[Entry]:
    """The entries of `git ls-files -s` in `root`. Raises RuntimeError when `root` is not a git work tree or git cannot be started: a scan of nothing must not pass."""
    try:
        proc = subprocess.run(["git", "-C", str(root), "ls-files", "-s", "-z"], capture_output=True)  # nosec B603: argument list built here, no shell, program is git; B607: bare name of a standard tool such as git looked up on the PATH of the host running this tool  # nosemgrep: dangerous-subprocess-use-audit, dangerous-subprocess-use-tainted-env-args -- argument list built here, no shell; the program is git
    except OSError as e:
        raise RuntimeError("git is not installed or cannot be started: %s" % (e.strerror or e))
    if proc.returncode != 0:
        raise RuntimeError("git ls-files failed in %s: %s" % (root, proc.stderr.decode("utf-8", "replace").strip()))
    out = []  # type: List[Entry]
    for rec in proc.stdout.split(b"\0"):
        if not rec:
            continue
        meta, _, path = rec.partition(b"\t")
        out.append(Entry(path.decode("utf-8", "surrogateescape"), meta.split(b" ")[0].decode("ascii")))
    return out


def _read(root: Path, path: str, limit: Optional[int] = None) -> Optional[bytes]:
    p = root / path
    if p.is_symlink() or not p.is_file():
        return None
    with open(p, "rb") as f:
        return f.read() if limit is None else f.read(limit)


def _engine_allowed(path: str) -> bool:
    return any(re.fullmatch(rx, path) for rx in ENGINE_ALLOWED)


def scan(entries: Iterable[Entry], read: Callable[[str], Optional[bytes]]) -> List[Finding]:
    """Applies the hard rules. `read(path)` returns the bytes of a tracked file (None when it is not a regular file)."""
    findings = []  # type: List[Finding]
    for e in entries:
        path = e.path
        base = path.rsplit("/", 1)[-1]
        suffix = os.path.splitext(base)[1].lower()
        parts = path.split("/")
        if path in FORBIDDEN_PATHS:
            findings.append(Finding("forbidden-path", path, FORBIDDEN_PATHS[path]))
        if e.mode == "160000" or path == ".gitmodules" or base == ".gitmodules":
            findings.append(Finding("gitlink", path, "a submodule is a third-party tree kept by reference"))
        if e.mode == "120000" and path not in SYMLINK_ALLOWED:
            findings.append(Finding("symlink", path, "a symbolic link; only %s may be one" % ", ".join(sorted(SYMLINK_ALLOWED))))
        if (LICENCE_NAME.match(base) or LICENCE_NAME_ANY_CASE.match(base)) and not ("/" not in path and path in LICENCE_ALLOWED):
            findings.append(Finding("licence-file", path, "a licence, notice or provenance file of another project; only the root LICENSE and NOTICE are ours"))
        for i, name in enumerate(parts[:-1]):
            prefix = "/".join(parts[: i + 1])
            if name.lower() in VENDOR_DIR_NAMES and not any(prefix == a or prefix.startswith(a + "/") for a in VENDOR_DIR_ALLOWED):
                findings.append(Finding("vendor-dir", path, "a %s directory holds copied code" % name))
                break
        if RULES_PACKAGE_DIR in parts[:-1] or (parts[-1] == RULES_PACKAGE_DIR):
            findings.append(Finding("rules-copy", path, "the package folder of the scanner kit's rules; they are fetched from a pinned commit and published by the operator"))
        if suffix in BINARY_SUFFIXES or SHARED_OBJECT.search(base.lower()):
            findings.append(Finding("binary-file", path, "an archive, compiled object or source map"))
        if parts[0] == "engine" and not _engine_allowed(path):
            findings.append(Finding("engine-layout", path, "engine/ holds declarations, locks, patches and our own files only; add the path to ENGINE_ALLOWED if it is ours"))
        if e.mode in ("120000", "160000"):
            continue
        data = read(path)
        if data is None:
            continue
        digest = hashlib.sha256(data).hexdigest()
        if digest in FORBIDDEN_HASHES:
            findings.append(Finding("forbidden-hash", path, "a copy of " + FORBIDDEN_HASHES[digest]))
        if suffix in RULE_SUFFIXES and RULES_REGISTRY_URL in data:
            findings.append(Finding("rules-copy", path, "a YAML file that carries the registry url of a semgrep rule; the rules are fetched, never committed"))
        if data.startswith(LFS_POINTER):
            findings.append(Finding("binary-file", path, "a Git LFS pointer"))
        if parts[0] == "engine" and not path.endswith(".patch"):
            if len(data) > ENGINE_SIZE_LIMIT:
                findings.append(Finding("engine-size", path, "%d bytes; engine/ files other than patches stay under %d" % (len(data), ENGINE_SIZE_LIMIT)))
            if b"\0" in data:
                findings.append(Finding("engine-size", path, "a NUL byte: a binary file"))
        if suffix in BUNDLE_SUFFIXES:
            first = next((ln for ln in data.decode("utf-8", "replace").splitlines() if ln.strip()), "")
            if ESBUILD_COMMENT.match(first):
                findings.append(Finding("bundle-comment", path, "the first line is the path comment esbuild writes above a bundled module"))
        if suffix in JS_SUFFIXES and any(len(ln) > LONG_LINE for ln in data.decode("utf-8", "replace").splitlines()):
            findings.append(Finding("long-line", path, "a line over %d characters: a minified or bundled file" % LONG_LINE))
    return findings


def scan_tree(root: Path) -> List[Finding]:
    """The hard rules over the tracked files of the git work tree at `root`."""
    return scan(tracked_entries(root), lambda p: _read(root, p))


def header_signals(entries: Iterable[Entry], read: Callable[[str], Optional[bytes]]) -> List[Tuple[str, int, str]]:
    """Diagnostics: (path, line, text) of a licence or copyright header in the first 25 lines of a source file. Our own SPDX lines show up too."""
    out = []  # type: List[Tuple[str, int, str]]
    for e in entries:
        if os.path.splitext(e.path)[1].lower() not in SIGNAL_SUFFIXES or e.mode in ("120000", "160000"):
            continue
        data = read(e.path)
        if data is None:
            continue
        for n, ln in enumerate(data[:4096].decode("utf-8", "replace").splitlines()[:25], 1):
            if SIGNAL.search(ln):
                out.append((e.path, n, ln.strip()[:120]))
                break
    return out


def main(argv: List[str]) -> int:
    root = Path(argv[1]) if len(argv) > 1 else Path(__file__).resolve().parents[2]
    try:
        entries = tracked_entries(root)
    except RuntimeError as e:
        print("THIRD-PARTY-SCAN FAIL %s" % e)
        return 2

    def read(p: str) -> Optional[bytes]:
        return _read(root, p)

    findings = scan(entries, read)
    for f in findings:
        print(f.line())
    for path, n, text in header_signals(entries, read):
        print("signal %s:%d: %s" % (path, n, text))
    if findings:
        print("THIRD-PARTY-SCAN FAIL %d finding(s)" % len(findings))
        return 1
    print("THIRD-PARTY-SCAN PASS %d tracked files" % len(entries))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
