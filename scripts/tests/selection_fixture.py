"""Fake builds that carry the account selection a real build embeds.

The Rust binaries carry the accounts.json entry `HAMH_AWS_PROFILE` selected when they were built
(crates/harness-protocol/build.rs). Measured on the release builds of 2026-10-01 (scripts/AGENTS.md):
`dispatch-turn`, `evaluate` and `repo-gateway` embed the account id (`E2E_ACCOUNT`), `agentd` embeds the
Region (`STS_REGION`) and no other entry's, every other binary embeds neither in a form a byte scan can
tell apart, and `control-api` and `dispatch-turn` embed history-profile's id in a price-list source string
whatever they were built for. The fakes here write exactly that, plus the build script's
`aws_target.rs` next to them, so the tests exercise the checks of scripts/lib/build_selection.py on bytes
shaped like the real ones. This module names those sets itself (test_build_selection.py asserts they match
the library's), so the fakes still work when the library is missing.

Environment knobs of `write_build` (the fake cargo in test_deploy.py passes its own environment):
  HAMH_AWS_PROFILE          the selection the build was asked for ("" is the active entry)
  FAKE_BUILT_FOR           a profile the binaries are really built for, in place of the selection
  FAKE_BUILT_FOR_BINARIES  limit FAKE_BUILT_FOR to these binaries (comma-separated)
  FAKE_OUT_FOR             a profile the build script output names, in place of the selection
  FAKE_OUT_KINDS           the builds FAKE_OUT_FOR applies to: lambda, zig or both (default both)
  FAKE_NO_OUT              write no build script output
  FAKE_FOREIGN_IN          binaries that also embed the OTHER entry's account id (a price-list string)
  FAKE_CLOBBER_FOR         a profile whose deploy "runs at the same time" from the same checkout: after
                           this build it writes that selection's binaries over the legacy shared outputs
                           (target/lambda, target/<triple>/release) and into its own keyed directory
  FAKE_CLOBBER_STAGE       a stage whose lambda directory (same selection) is truncated by its own build
"""

from __future__ import annotations

import atexit
import os
import sys
import tempfile
from pathlib import Path
from typing import Mapping, Optional

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "scripts" / "lib"))
sys.path.insert(0, str(Path(__file__).resolve().parent))
import aws_target  # noqa: E402
import accounts_fixture as fx  # noqa: E402

# The accounts file of the fakes: placeholder accounts in a folder of this process, never the committed accounts.json (whose content is the maintainers' choice).
_TMP = tempfile.TemporaryDirectory(prefix="selection-fixture-")
atexit.register(_TMP.cleanup)
ACCOUNTS = fx.write_accounts(_TMP.name)

TRIPLE = "aarch64-unknown-linux-gnu"
ACCOUNT_BINARIES = frozenset({"repo-gateway"})
OUT_HASH = "0123456789abcdef"
STAGES = ("direct",)


def entries() -> list:
    return aws_target.load(ACCOUNTS)


def entry(profile: str = "") -> aws_target.Target:
    """The accounts.json entry a profile names ("" is the active one)."""
    return aws_target.select({aws_target.FILE_HOOK: str(ACCOUNTS), aws_target.SELECTOR: profile})


def other(profile: str = "") -> aws_target.Target:
    sel = entry(profile)
    return next(t for t in entries() if t.profile != sel.profile)


def key(profile: str = "") -> str:
    t = entry(profile)
    return f"{t.account}-{t.region}"


def driver_text(script: str, profile: str) -> str:
    """A fake driver or dispatcher script that, like the real `ham-e2e` and `hamh-dispatch`, embeds the account id (and
    Region) of the entry it was built for and no other entry's. The fakes print canned ARNs and URIs of history-profile
    whatever the case selects; every account id of accounts.json in the script text is split into two adjacent string
    literals (`"777788" "889999"`), which Python joins back, so what the fake prints is unchanged and the file's bytes
    carry only the marker's account."""
    t = entry(profile)
    for e in entries():
        script = script.replace(e.account, e.account[:6] + '" "' + e.account[6:])
    return script.rstrip("\n") + f"\n# built for E2E_ACCOUNT={t.account} E2E_REGION={t.region} E2E_PROFILE={t.profile}\n"


def binary_bytes(name: str, profile: str = "", foreign: bool = False) -> bytes:
    """What a build for `profile` writes as binary `name`: the old fakes' `<name>-binary`, then the constants
    the real binary embeds (merged into read-only data next to unrelated strings, so a digit may precede)."""
    t = entry(profile)
    body = f"{name}-binary".encode() + b"\x00" + bytes(range(1, 40)) * 3
    if name in ACCOUNT_BINARIES:
        body += b"us-west-2" + t.account.encode() + b"\x00arn:aws:sts::" + t.account.encode() + b":assumed-role/Admin/*"
    if foreign:
        body += b"\x00account " + other(profile).account.encode() + b", retrieved 2026-09-24\x00"
    return body


def aws_target_rs(profile: str = "") -> str:
    """The build script's `aws_target.rs` for a build selected by `profile` (build.rs)."""
    t = entry(profile)
    lines = [f'pub const E2E_ACCOUNT: &str = "{t.account}";', f'pub const E2E_REGION: &str = "{t.region}";',
             f'pub const E2E_PROFILE: &str = "{t.profile}";',
             f'pub const OPERATOR_POOL_MEMBER: &str = "arn:aws:sts::{t.account}:assumed-role/Admin/*";',
             f'pub const SELECTED_BY: &str = "{profile}";', "pub const ACCOUNTS: &[(&str, &str, &str, &str)] = &["]
    lines += [f'    ("{e.account}", "{e.region}", "{e.profile}", "{e.role}"),' for e in entries()]
    return "\n".join(lines + ["];", ""])


def write_tree(kind: str, names, target_dir: Path, lambda_dir: Optional[Path], profile: str, out_profile: Optional[str] = None,
               foreign=(), per_binary: Optional[Mapping[str, str]] = None, with_out: bool = True) -> None:
    """One build's outputs: lambda bootstraps under `lambda_dir`, or zigbuild binaries under the target directory."""
    for name in names:
        p = (per_binary or {}).get(name, profile)
        if kind == "lambda":
            path = Path(lambda_dir) / name / "bootstrap"
        else:
            path = Path(target_dir) / TRIPLE / "release" / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(binary_bytes(name, p, foreign=name in foreign))
    if with_out:
        out = Path(target_dir) / TRIPLE / "release" / "build" / f"harness-protocol-{OUT_HASH}" / "out" / "aws_target.rs"
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(aws_target_rs(profile if out_profile is None else out_profile))


def write_build(kind: str, names, target_dir: str, lambda_dir: Optional[str], environ: Mapping[str, str]) -> None:
    selected = environ.get("HAMH_AWS_PROFILE", "")
    built_for = environ.get("FAKE_BUILT_FOR")
    only = {n for n in environ.get("FAKE_BUILT_FOR_BINARIES", "").split(",") if n}
    per_binary = {n: built_for for n in names if built_for and (not only or n in only)}
    foreign = {n for n in environ.get("FAKE_FOREIGN_IN", "").split(",") if n}
    out_kinds = [k for k in environ.get("FAKE_OUT_KINDS", "lambda,zig").split(",") if k]
    write_tree(kind, names, Path(target_dir), Path(lambda_dir) if lambda_dir else None, selected,
               out_profile=environ.get("FAKE_OUT_FOR") if kind in out_kinds else None, foreign=foreign,
               per_binary=per_binary, with_out=not environ.get("FAKE_NO_OUT"))
    clobber = environ.get("FAKE_CLOBBER_FOR")
    if clobber is not None:
        # The legacy shared outputs, then the clobbering selection's own keyed directory (both stages).
        write_tree(kind, names, Path("target"), Path("target") / "lambda", clobber)
        keyed = Path("target") / "deploy" / key(clobber)
        for stage in STAGES:
            write_tree(kind, names, keyed, keyed / "lambda" / stage, clobber)
    stage = environ.get("FAKE_CLOBBER_STAGE")
    if stage and kind == "lambda":
        for name in names:
            path = Path(target_dir) / "lambda" / stage / name / "bootstrap"
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"")
