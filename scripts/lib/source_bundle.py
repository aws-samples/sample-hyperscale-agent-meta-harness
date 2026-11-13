#!/usr/bin/env python3
"""The source bundle a child CodeBuild build receives, plus the generated account file when the deploy uses one.

scripts/codebuild-run.sh zips `git archive HEAD` and uploads it as the source of an image build. A deploy that reads its accounts from a
generated file (HAMH_ACCOUNTS_FILE, scripts/lib/aws_target.py) must hand the same file to the child builds, and it must not edit the committed
tree to do so: the archive is the commit, the generated file is a separate input. This module adds it to the zip as one more member.

  source_bundle.py add-accounts-file <zip> [--expect <sha256>]
      reads the file HAMH_ACCOUNTS_FILE names, adds it to <zip> as `.hamh-build/accounts.json` and prints its SHA-256.
      With --expect, a file whose digest differs is refused, so the bytes bundled are the bytes the key and the build environment name.
      Exit 2 and `source_bundle: <reason>` on stderr for an unset or relative variable, a missing file, a zip that is not there or a member
      the archive already has; the zip is not touched then.

The member is written with a fixed time and mode, so the same file gives the same bytes. lib/child-build-env.sh reads it back inside the
build, checks the digest (HAMH_ACCOUNTS_FILE_SHA256, set by codebuild-run.sh) and exports HAMH_ACCOUNTS_FILE for the commands that follow.
Python 3.8+, standard library only.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import sys
import zipfile
from pathlib import Path
from typing import List, Optional

sys.path.insert(0, str(Path(__file__).resolve().parent))
import aws_target  # noqa: E402

# Where the bundle carries the generated account file; lib/child-build-env.sh holds the same path (test_codebuild_run.py pins both).
MEMBER = ".hamh-build/accounts.json"
DIGEST_ENV = "HAMH_ACCOUNTS_FILE_SHA256"
FIXED_TIME = (2020, 1, 1, 0, 0, 0)


class BundleError(Exception):
    """A refusal; the zip is left as it was."""


def add_accounts_file(zip_path: Path, env: Optional[dict] = None, expect: Optional[str] = None) -> str:
    """Adds the override file to the zip as MEMBER and returns its SHA-256."""
    env = os.environ if env is None else env
    if not aws_target.env_get(aws_target.FILE_HOOK, env):
        raise BundleError(f"{aws_target.FILE_HOOK} is not set: there is no generated account file to add")
    try:
        path = aws_target.accounts_path(env)
        body = path.read_bytes()
    except aws_target.TargetError as exc:
        raise BundleError(str(exc)) from None
    except OSError as exc:
        raise BundleError(f"{path}: {exc.strerror or exc}") from None
    digest = hashlib.sha256(body).hexdigest()
    if expect is not None and digest != expect:
        raise BundleError(f"{path} has sha256 {digest}, not the {expect} the key and the build environment name")
    if not Path(zip_path).is_file():
        raise BundleError(f"{zip_path}: no such zip")
    try:
        with zipfile.ZipFile(zip_path, "a") as z:
            if MEMBER in z.namelist():
                raise BundleError(f"{zip_path} already has {MEMBER}")
            info = zipfile.ZipInfo(MEMBER, FIXED_TIME)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o100644 << 16
            z.writestr(info, body)
    except zipfile.BadZipFile as exc:
        raise BundleError(f"{zip_path}: {exc}") from None
    return digest


def main(argv: List[str]) -> int:
    ap = argparse.ArgumentParser(prog="source_bundle.py", add_help=False)
    sub = ap.add_subparsers(dest="cmd")
    add = sub.add_parser("add-accounts-file", add_help=False)
    add.add_argument("zip")
    add.add_argument("--expect", default=None)
    try:
        ns = ap.parse_args(argv)
    except SystemExit:
        print("usage: source_bundle.py add-accounts-file <zip> [--expect <sha256>]", file=sys.stderr)
        return 2
    if ns.cmd != "add-accounts-file":
        print("usage: source_bundle.py add-accounts-file <zip> [--expect <sha256>]", file=sys.stderr)
        return 2
    try:
        print(add_accounts_file(Path(ns.zip), expect=ns.expect))
    except BundleError as exc:
        print(f"source_bundle: {exc}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
