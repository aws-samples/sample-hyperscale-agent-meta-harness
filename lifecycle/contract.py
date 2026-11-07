#!/usr/bin/env python3
"""contract.py: render the three texts of the lifecycle contract from lifecycle/contract.src and keep their files equal to the render.

The contract is one source in three texts. lifecycle/contract.src is written once, in blocks. A line that starts with `%%` opens a block and
names the texts it belongs to: `full`, `quick`, `auto`, or `all` for the three. The block runs to the next marker. A text is the blocks that name it, in file order.

  full   the eight steps (the text of pack 1.2.0 plus the outside-this-checkout rule)
  quick  the quick path alone
  auto   the flow statement, the quick path and the eight steps: the model picks the flow

Where each text is kept, relative to the repository root (the folder above lifecycle/):

  auto   lifecycle/pack/AGENTS.md (the pack's contract; `ham lifecycle init` copies it and pack.lock.json locks it) and, byte for byte,
         crates/ham-core/assets/lifecycle-rules.md (the client's copy)
  full   crates/ham-core/assets/lifecycle-rules-full.md
  quick  crates/ham-core/assets/lifecycle-rules-quick.md

  contract.py                  check: print one line per problem, then LIFECYCLE-CONTRACT PASS or LIFECYCLE-CONTRACT FAIL (exit 0 or 1)
  contract.py --write          rewrite the files from the source (then run lock.py --write --version X.Y.Z: the pack contract is a pack file)
  contract.py --print VARIANT  print one text
  --root DIR                   the lifecycle directory (default: the folder of this script)

The renderer refuses what the pack does not allow in a file: text before the first marker, a marker with no tag or an unknown tag, a block with no text,
a character that is not ASCII, a tab and a carriage return. A text ends with one newline and holds no run of blank lines. Rendering is a pure function of the
source and the variant. Standard library only; Python 3.8 or later.
"""

from __future__ import annotations

import argparse
import hashlib
import re
import sys
from pathlib import Path

SOURCE = "contract.src"
MARKER = "%%"
VARIANTS = ("full", "quick", "auto")
TARGETS = {
    "auto": ("lifecycle/pack/AGENTS.md", "crates/ham-core/assets/lifecycle-rules.md"),
    "full": ("crates/ham-core/assets/lifecycle-rules-full.md",),
    "quick": ("crates/ham-core/assets/lifecycle-rules-quick.md",),
}
TAGS = VARIANTS + ("all",)


def names() -> str:
    return "full, quick, auto or all"


def parse(source: str) -> list:
    """[(variants, text)] for the blocks of a source, in file order. Raises ValueError, with the line, for what a source may not hold."""
    lines = source.split("\n")
    if source.endswith("\n"):
        lines.pop()
    for number, line in enumerate(lines, 1):
        if any(not 32 <= ord(c) <= 126 for c in line):
            raise ValueError("line %d: a character the pack does not allow (only printable ASCII; no tab, no carriage return)" % number)
    blocks: list = []
    current = None
    for number, line in enumerate(lines, 1):
        if line.startswith(MARKER):
            tags = line[len(MARKER):].split()
            if not tags or any(t not in TAGS for t in tags):
                raise ValueError("line %d: a marker names one or more of %s; got %r" % (number, names(), line))
            variants = tuple(v for v in VARIANTS if v in tags or "all" in tags)
            current = [variants, [], number]
            blocks.append(current)
        elif current is None:
            raise ValueError("line %d: text before the first marker (a line that starts with %s)" % (number, MARKER))
        else:
            current[1].append(line)
    out = []
    for variants, lines, number in blocks:
        text = "".join(ln + "\n" for ln in lines)
        if not text.strip():
            raise ValueError("line %d: the block has no text" % number)
        out.append((variants, text))
    return out


def render(source: str, variant: str) -> str:
    """The text of `variant` (full, quick or auto) from a source: its blocks in file order, one newline at the end, no run of blank lines."""
    if variant not in VARIANTS:
        raise ValueError("the variant is one of %s; got %r" % (", ".join(VARIANTS), variant))
    text = "".join(block for variants, block in parse(source) if variant in variants)
    text = re.sub(r"\n{3,}", "\n\n", text)
    return text.strip("\n") + "\n"


def paths(root: Path) -> dict:
    """{variant: [file, ...]} under the repository root, which is the folder above the lifecycle directory."""
    repo = Path(root).resolve().parent
    return {variant: [repo / rel for rel in rels] for variant, rels in TARGETS.items()}


def texts(root: Path) -> dict:
    """{variant: text} rendered from contract.src; raises ValueError when the source cannot be rendered, OSError when it cannot be read."""
    source = (Path(root) / SOURCE).read_bytes().decode("utf-8")
    return {variant: render(source, variant) for variant in VARIANTS}


def problems(root: Path) -> list:
    """One line per fault: the source cannot be read or rendered, or a file is missing or differs from its text."""
    try:
        rendered = texts(root)
    except OSError as e:
        return ["%s cannot be read: %s" % (SOURCE, e.strerror or e)]
    except (ValueError, UnicodeDecodeError) as e:
        return ["%s: %s" % (SOURCE, e)]
    repo = Path(root).resolve().parent
    out = []
    for variant, files in paths(root).items():
        for path in files:
            rel = path.relative_to(repo).as_posix()
            if not path.is_file():
                out.append("%s: missing (contract.py --write makes it)" % rel)
            elif path.read_bytes() != rendered[variant].encode("ascii"):
                out.append("%s: differs from the %s text rendered from %s (contract.py --write)" % (rel, variant, SOURCE))
    return out


def summary(text: str) -> str:
    data = text.encode("ascii")
    return "%d bytes sha256 %s" % (len(data), hashlib.sha256(data).hexdigest())


def main(argv: list) -> int:
    parser = argparse.ArgumentParser(description="Render the three texts of the lifecycle contract from contract.src and keep their files equal to the render.")
    parser.add_argument("--root", default=str(Path(__file__).resolve().parent))
    parser.add_argument("--write", action="store_true", help="rewrite the files from the source")
    parser.add_argument("--print", dest="show", choices=VARIANTS, help="print one text")
    args = parser.parse_args(argv)
    if args.write and args.show:
        parser.error("--print stands alone")
    root = Path(args.root)
    try:
        rendered = texts(root)
    except OSError as e:
        print("LIFECYCLE-CONTRACT %s cannot be read: %s" % (SOURCE, e.strerror or e))
        return 1
    except (ValueError, UnicodeDecodeError) as e:
        print("LIFECYCLE-CONTRACT %s: %s" % (SOURCE, e))
        return 1
    if args.show:
        sys.stdout.write(rendered[args.show])
        return 0
    if args.write:
        repo = root.resolve().parent
        for variant, files in paths(root).items():
            for path in files:
                data = rendered[variant].encode("ascii")
                if path.is_file() and path.read_bytes() == data:
                    continue
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(data)
                print("LIFECYCLE-CONTRACT WROTE %s (%s)" % (path.relative_to(repo).as_posix(), variant))
        print("LIFECYCLE-CONTRACT WROTE done")
        return 0
    found = problems(root)
    for line in found:
        print("LIFECYCLE-CONTRACT %s" % line)
    if not found:
        for variant in VARIANTS:
            print("LIFECYCLE-CONTRACT %s %s" % (variant, summary(rendered[variant])))
    print("LIFECYCLE-CONTRACT %s" % ("FAIL" if found else "PASS"))
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
