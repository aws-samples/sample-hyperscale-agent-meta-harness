#!/usr/bin/env python3
"""Check a repository's DOX AGENTS.md tree (https://github.com/agent0ai/dox).

Rules:
  1. The root AGENTS.md exists, carries the DOX framework ("# DOX framework"), and has a
     "## Child DOX Index" section.
  2. Every durable boundary has an AGENTS.md: each top-level directory that holds files, and
     each directory with a package manifest (Cargo.toml, package.json, pyproject.toml, ...),
     except fixture repositories nested under the top-level data root fixtures/, which are test
     data owned by fixtures/AGENTS.md.
  3. Every child AGENTS.md is listed in the Child DOX Index of its nearest parent AGENTS.md.
  4. Every index entry points to an existing AGENTS.md.
  5. Child docs carry the DOX sections in order: Purpose, Ownership, Local Contracts,
     Work Guidance, Verification, Child DOX Index.
  6. Every direct subdirectory of a BOUNDARY_PARENTS directory (scripts/) that holds files anywhere below

Content that ships (SHIPPED_CONTENT: lifecycle/pack into the repositories of coding runs; engine/plugins/harness-rounds into
the worker image as an engine plugin folder; the goal plugin folder is fetched at build and is not in the tree) is not a DOX document of this repository: its AGENTS.md is the shipped file and its package.json is the
manifest the engine reads, so rules 2, 3 and 5 skip it; the directory that holds it still needs its own AGENTS.md.

Generated and hidden directories are skipped. Prints one line per problem and a final
"DOX PASS" or "DOX FAIL". Exit code 0 on pass, 1 on fail.

Usage: dox_lint.py [REPO_ROOT]
"""

from __future__ import annotations

import os
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

DOC = "AGENTS.md"
FRAMEWORK_MARKER = "# DOX framework"
INDEX_HEADING = "## Child DOX Index"
SECTIONS = ["Purpose", "Ownership", "Local Contracts", "Work Guidance", "Verification", "Child DOX Index"]
MANIFESTS = {
    "Cargo.toml", "package.json", "pyproject.toml", "setup.py", "go.mod",
    "pom.xml", "build.gradle", "build.gradle.kts",
}
IGNORED = {
    "node_modules", "target", "dist", "build", "cdk.out", "venv", "__pycache__",
    "vendor", "coverage", "out",
}
# Top-level data roots: manifests below them belong to fixture repositories, not the project.
DATA_ROOTS = {"fixtures"}
# Directories whose files ship into the repositories of coding runs (lifecycle/AGENTS.md owns them) or into the worker image as engine
# plugin folders (engine/plugins/AGENTS.md owns them).
SHIPPED_CONTENT = {"lifecycle/pack", "engine/plugins/harness-rounds"}
# Directories whose direct subdirectories are durable boundaries (rule 6).
BOUNDARY_PARENTS = {"scripts"}
ENTRY = re.compile(r"[\w.\-]+(?:/[\w.\-]+)*/AGENTS\.md")


@dataclass
class Result:
    ok: bool
    errors: list = field(default_factory=list)


def _skip(name: str) -> bool:
    return name.startswith(".") or name in IGNORED


def _index_section(text: str) -> str:
    # The index starts at the heading line. The same words quoted in a bullet above it (in backticks, mid-sentence) are prose.
    heading = re.search(r"^" + re.escape(INDEX_HEADING) + r"[ \t]*$", text, re.M)
    if heading is None:
        return ""
    rest = text[heading.end():]
    nxt = re.search(r"^## ", rest, re.M)
    return rest[: nxt.start()] if nxt else rest


def _shipped(rel: Path) -> bool:
    path = rel.as_posix()
    return any(path == s or path.startswith(s + "/") for s in SHIPPED_CONTENT)


def _walk(root: Path):
    for current, dirs, files in os.walk(root):
        dirs[:] = sorted(d for d in dirs if not _skip(d))
        yield Path(current), files


def lint(root: Path | str) -> Result:
    root = Path(root)
    errors: list = []
    root_doc = root / DOC
    if not root_doc.is_file():
        return Result(ok=False, errors=[f"missing root AGENTS.md at {root_doc}"])
    root_text = root_doc.read_text(encoding="utf-8")
    if FRAMEWORK_MARKER not in root_text:
        errors.append(f"root AGENTS.md lacks the DOX framework section ('{FRAMEWORK_MARKER}')")
    if not re.search(r"^" + re.escape(INDEX_HEADING) + r"[ \t]*$", root_text, re.M):
        errors.append(f"root AGENTS.md lacks '{INDEX_HEADING}'")

    docs: list = []
    has_files: dict = {}
    has_boundary_files: dict = {}
    for current, files in _walk(root):
        rel = current.relative_to(root)
        shipped = _shipped(rel)
        if DOC in files and current != root and not shipped:
            docs.append(current / DOC)
        if rel.parts:
            top = rel.parts[0]
            if files:
                has_files[top] = True
                if len(rel.parts) >= 2 and top in BOUNDARY_PARENTS:
                    has_boundary_files[(top, rel.parts[1])] = True
        manifest = MANIFESTS.intersection(files)
        in_data_root = len(rel.parts) >= 2 and rel.parts[0] in DATA_ROOTS
        if current != root and manifest and DOC not in files and not in_data_root and not shipped:
            errors.append(f"{rel.as_posix()}: missing AGENTS.md (package manifest {sorted(manifest)[0]})")

    for top in sorted(has_files):
        if not (root / top / DOC).is_file():
            message = f"{top}: missing AGENTS.md (top-level directory)"
            if message.split(" (")[0] + ":" not in " ".join(errors):
                errors.append(message)

    for parent_name, child_name in sorted(has_boundary_files):
        if not (root / parent_name / child_name / DOC).is_file():
            errors.append(f"{parent_name}/{child_name}: missing AGENTS.md (subdirectory of {parent_name})")

    for doc in sorted(docs):
        rel = doc.relative_to(root).as_posix()
        text = doc.read_text(encoding="utf-8")
        positions = []
        for name in SECTIONS:
            match = re.search(rf"^## {re.escape(name)}\s*$", text, re.M)
            if match is None:
                errors.append(f"{rel}: missing section '## {name}'")
            else:
                positions.append(match.start())
        if positions != sorted(positions):
            errors.append(f"{rel}: DOX sections out of order (expected {', '.join(SECTIONS)})")

        parent = doc.parent.parent
        while parent != root and not (parent / DOC).is_file():
            parent = parent.parent
        parent_doc = parent / DOC
        index = _index_section(parent_doc.read_text(encoding="utf-8"))
        from_parent = doc.relative_to(parent).as_posix()
        if from_parent not in index and rel not in index:
            errors.append(f"{rel}: not listed in the Child DOX Index of {parent_doc.relative_to(root).as_posix()}")

    for doc in [root_doc] + sorted(docs):
        for entry in ENTRY.findall(_index_section(doc.read_text(encoding="utf-8"))):
            if not (doc.parent / entry).is_file() and not (root / entry).is_file():
                errors.append(f"{doc.relative_to(root).as_posix()}: index entry {entry} does not exist")

    return Result(ok=not errors, errors=errors)


def main(argv: list | None = None) -> int:
    args = sys.argv[1:] if argv is None else argv
    result = lint(Path(args[0]) if args else Path.cwd())
    for error in result.errors:
        print(f"DOX FAIL: {error}")
    print("DOX PASS" if result.ok else "DOX FAIL")
    return 0 if result.ok else 1


if __name__ == "__main__":
    sys.exit(main())
