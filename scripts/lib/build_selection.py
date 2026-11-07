#!/usr/bin/env python3
"""Which accounts.json entry a Rust build was made for, and where a deploy builds.

Every Rust binary carries the entry `HAMH_AWS_PROFILE` selected when it was built: crates/harness-protocol/build.rs
writes the account, home Region and profile constants (`E2E_ACCOUNT`, `E2E_REGION`, ...) into the crate, and cargo
reruns it when the variable changes. Deploys for two entries that ran from one checkout at the same time would share
`target/`: a build for one entry could replace the `target/lambda` outputs between the other entry's build and its
publish, and a Lambda would then carry the account and Region of the wrong entry.

Layout. A deploy builds for its selection in `<repo>/target/deploy/<account>-<region>` (`target_dir`): the cargo
target directory of `cargo lambda build` and `cargo zigbuild` (the worker binaries in
`<target dir>/aarch64-unknown-linux-gnu/release`), and the Lambda bootstraps in `<target dir>/lambda/<stage>/<bin>/
bootstrap` (`lambda_dir`; the stage keeps live and drill of one selection, which share the cargo cache, from
rewriting each other's files while CDK zips them). Two selections never share a file.

Checks, run by deploy.py after each build and before anything is published, and by the live-run drivers:
- `check_binary`: a binary that embeds the account id of its selection (`E2E_ACCOUNT`) must embed the selected one. That is
  `repo-gateway`. A binary can carry another entry's account id in data it embeds (a price table, for example), so the
  selected id being present is the test, not the other id being absent. The other binaries embed neither id in a form a byte
  scan tells apart (control-api carries account ids in its price table; agentd and the rest carry the Region names of the
  AWS SDK tables), so a byte scan cannot vouch for them. `ACCOUNT_BINARIES` names the binaries it can vouch for; anything
  else is covered by the build output check.
- `check_build_output`: the harness-protocol build script output (`aws_target.rs`) under the target directory must name
  the selected entry. It is the one place every binary's selection comes from. In each build root the newest file
  decides (a stale directory from an older dependency set may stay behind). No output at all is a mismatch: nothing
  then confirms the selection.
- `check_driver`: a live-run driver must embed the selected account id and no other entry's.

A mismatch is a `Mismatch` naming the binary and both selections. Command line (for shell callers):
  build_selection.py check-driver <binary>    exit 0 silent; 1 with `build_selection: <binary> <reason>` on stderr;
                                              2 on usage errors or an unknown HAMH_AWS_PROFILE
Python 3.8+, standard library only.
"""

from __future__ import annotations

import os
import re
import sys
from pathlib import Path
from typing import Iterable, List, Optional, Sequence, Set

sys.path.insert(0, str(Path(__file__).resolve().parent))
import aws_target  # noqa: E402

TRIPLE = "aarch64-unknown-linux-gnu"
# The binaries whose bytes carry the account id of their selection (see the module docstring).
ACCOUNT_BINARIES = frozenset({"repo-gateway"})
# aws_target.rs, relative to a target directory: native builds, then cross builds (`<triple>/release`).
OUTPUT_GLOBS = ("release/build/harness-protocol-*/out/aws_target.rs", "*/release/build/harness-protocol-*/out/aws_target.rs",
                "debug/build/harness-protocol-*/out/aws_target.rs", "*/debug/build/harness-protocol-*/out/aws_target.rs")
CHUNK = 8 << 20
_CONST = re.compile(r'pub const (E2E_ACCOUNT|E2E_REGION|E2E_PROFILE): &str = "([^"]*)";')


class Mismatch(Exception):
    """A built file that was not built for the selected entry. `binary` names it, `detail` names both selections."""

    def __init__(self, binary: str, detail: str, built_for: Optional[aws_target.Target] = None) -> None:
        super().__init__(f"{binary} {detail}")
        self.binary = binary
        self.detail = detail
        self.built_for = built_for


def label(t: aws_target.Target) -> str:
    return f"{t.profile} ({t.account} {t.region})"


def key(t: aws_target.Target) -> str:
    return f"{t.account}-{t.region}"


def target_dir(root: Path, t: aws_target.Target) -> Path:
    """The cargo target directory a deploy of selection `t` builds in."""
    return Path(root) / "target" / "deploy" / key(t)


def lambda_dir(root: Path, t: aws_target.Target, stage: str) -> Path:
    """The directory CDK reads the Lambda bootstraps from (`HAMH_LAMBDA_DIR`) for `stage` of selection `t`."""
    return target_dir(root, t) / "lambda" / stage


def scan(path: Path, needles: Iterable[bytes], chunk: int = CHUNK) -> Set[bytes]:
    """The needles that occur anywhere in the file (chunked, so a 450 MB debug binary stays out of memory)."""
    want = [n for n in needles if n]
    found: Set[bytes] = set()
    if not want:
        return found
    keep = max(len(n) for n in want) - 1
    tail = b""
    with open(path, "rb") as f:
        while True:
            block = f.read(chunk)
            if not block:
                break
            window = tail + block
            found.update(n for n in want if n in window)
            if len(found) == len(set(want)):
                break
            tail = window[-keep:] if keep else b""
    return found


def _read(path: Path, name: str, needles: Sequence[bytes], chunk: int) -> Set[bytes]:
    try:
        return scan(path, needles, chunk)
    except OSError as e:
        raise Mismatch(name, f"cannot read {path}: {e.strerror or e}") from None


def _others(entries: Sequence[aws_target.Target], selected: aws_target.Target) -> List[aws_target.Target]:
    return [t for t in entries if (t.account, t.region) != (selected.account, selected.region)]


def check_binary(path: Path, name: str, selected: aws_target.Target, entries: Sequence[aws_target.Target],
                 chunk: int = CHUNK) -> Optional[str]:
    """Checks binary `name` at `path` against the selection it must have been built for.

    Returns "account" for a binary this scan can vouch for and that embeds the selected entry's id, None for a
    binary it cannot vouch for; raises Mismatch otherwise."""
    if name in ACCOUNT_BINARIES:
        found = _read(path, name, [t.account.encode() for t in entries], chunk)
        if selected.account.encode() in found:
            return "account"
        built = next((t for t in _others(entries, selected) if t.account.encode() in found), None)
        if built:
            raise Mismatch(name, f"built for {label(built)}, target is {label(selected)}: {path} embeds {built.account}, "
                                 f"not {selected.account}", built)
        raise Mismatch(name, f"built for an unknown selection, target is {label(selected)}: {path} embeds no account id of "
                             f"accounts.json ({', '.join(t.account for t in entries)})")
    return None


def check_driver(path: Path, selected: aws_target.Target, entries: Sequence[aws_target.Target], chunk: int = CHUNK) -> None:
    """A live-run driver must embed the selected account id and no other entry's; raises Mismatch otherwise."""
    name = Path(path).name
    found = _read(Path(path), name, [t.account.encode() for t in entries], chunk)
    has_selected = selected.account.encode() in found
    built = next((t for t in _others(entries, selected) if t.account.encode() in found), None)
    if built and has_selected:
        raise Mismatch(name, f"built for {label(built)}, target is {label(selected)}: {path} embeds {built.account} and "
                             f"{selected.account}", built)
    if built:
        raise Mismatch(name, f"built for {label(built)}, target is {label(selected)}: {path} embeds {built.account}, "
                             f"not {selected.account}", built)
    if not has_selected:
        raise Mismatch(name, f"built for an unknown selection, target is {label(selected)}: {path} embeds no account id of "
                             f"accounts.json ({', '.join(t.account for t in entries)})")


def check_build_output(tdir: Path, selected: aws_target.Target, entries: Sequence[aws_target.Target]) -> List[Path]:
    """The harness-protocol build script output under target directory `tdir` must name the selected entry.

    Returns the files that decided (the newest of each build root); raises Mismatch, naming "harness-protocol",
    for one that names another entry or has no constants, and for a directory with none."""
    tdir = Path(tdir)
    files = {p for g in OUTPUT_GLOBS for p in tdir.glob(g)}
    if not files:
        raise Mismatch("harness-protocol", f"has no build output under {tdir} (the build script's aws_target.rs): nothing "
                                           f"confirms the build selection, target is {label(selected)}")
    roots = {}
    for p in files:
        roots.setdefault(p.parents[2], []).append(p)
    decided = sorted(max(ps, key=lambda q: (q.stat().st_mtime_ns, str(q))) for ps in roots.values())
    for p in decided:
        got = dict(_CONST.findall(p.read_text()))
        if set(got) != {"E2E_ACCOUNT", "E2E_REGION", "E2E_PROFILE"}:
            raise Mismatch("harness-protocol", f"{p} does not define E2E_ACCOUNT, E2E_REGION and E2E_PROFILE")
        if (got["E2E_ACCOUNT"], got["E2E_REGION"], got["E2E_PROFILE"]) != (selected.account, selected.region, selected.profile):
            built = next((t for t in entries if (t.account, t.region) == (got["E2E_ACCOUNT"], got["E2E_REGION"])), None)
            who = label(built) if built else f"{got['E2E_PROFILE']} ({got['E2E_ACCOUNT']} {got['E2E_REGION']})"
            raise Mismatch("harness-protocol", f"built for {who}, target is {label(selected)}: {p} names "
                                               f"{got['E2E_ACCOUNT']} {got['E2E_REGION']} {got['E2E_PROFILE']}", built)
    return decided


def main(argv: List[str]) -> int:
    if len(argv) != 2 or argv[0] != "check-driver":
        print("usage: build_selection.py check-driver <binary>", file=sys.stderr)
        return 2
    try:
        selected, entries = aws_target.select(os.environ), aws_target.accounts(os.environ)
    except aws_target.TargetError as e:
        print(f"build_selection: {e}", file=sys.stderr)
        return 2
    try:
        check_driver(Path(argv[1]), selected, entries)
    except Mismatch as e:
        print(f"build_selection: {e.binary} {e.detail}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
