#!/usr/bin/env python3
"""The one recipe that builds the worker binary, `agentd`.

The production worker image carries an `agentd` cross-built for aarch64 glibc 2.34 by `cargo zigbuild`. Deploy takes
`zigbuild_args` and `build_env`; the CI step runs `prove`.

  agentd_build.py build --root DIR [--out-dir DIR]
  agentd_build.py prove --root DIR --work DIR [--out-dir DIR]

`build` runs the recipe in DIR in the deploy layout (`<DIR>/target/deploy/<account>-<region>`,
build_selection.target_dir) and prints `AGENTD-BUILD root=A sha256=<agentd> file=<path> recipe=<digest>`;
the binary is checked against the selected accounts.json entry (build_selection). `prove` builds root A that way and
root B from a copy of the same working tree (tracked and untracked files, no history) under WORK with a freshly fetched
CARGO_HOME and its own target directory, then prints

  AGENTD-BUILD root=A|B ...        one line per root
  AGENTD-RECIPE digest=<64 hex> target= rustflags= zig= cargo-zigbuild= rustc= profile=
  AGENTD-LEAKS root=A|B none|FAIL <paths>   host paths the binary embeds (both CARGO_HOMEs, both roots, HOME, TMPDIR, ...)
  AGENTD-PRODUCTION-SHA256 <hex> source=rebuild-B     only when A and B are equal and clean
  AGENTD-EQUAL PASS | FAIL a-and-b-differ|leaks|build

and exits 0 only on PASS. The B hash is the production-recipe rebuild: it shows the recipe's output does not depend on
the build root, the registry path or the clone, which is what makes the deployed image's `AGENTD_SHA256` comparable
(verify row R24 reads both).

Exit codes: 0 ok, 1 a build, selection or equality failure, 2 usage or a wrong tool version (zig, cargo-zigbuild,
rustc against the pins below). Test hooks: AGENTD_BUILD_CARGO (default `cargo`), AGENTD_BUILD_SLOT (a command to prefix
cargo with; empty for none; default `<root>/scripts/cargo-slot.sh`), AGENTD_BUILD_ZIG, AGENTD_BUILD_ZIGBUILD,
AGENTD_BUILD_RUSTC, AGENTD_BUILD_SKIP_TOOL_CHECK=1, AGENTD_BUILD_RETRY_SLEEP (the seconds unit between the three attempts of root B's registry fetch,
default 5), AGENTD_BUILD_TIMEOUT_S (a cargo call's deadline, default 3600). Python 3.11+, standard library only.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shlex
import shutil
import subprocess
import sys
import time
import tomllib
from pathlib import Path
from typing import Iterable, Mapping, Sequence

sys.path.insert(0, str(Path(__file__).resolve().parent))
import aws_target  # noqa: E402
import build_selection  # noqa: E402

TRIPLE = "aarch64-unknown-linux-gnu"
GLIBC = "2.34"
TARGET_ARG = f"{TRIPLE}.{GLIBC}"
WORKER_PACKAGES = ("agentd",)
# The pins the recipe asserts; test_agentd_build.py compares them with tools.lock and rust-toolchain.toml. This file is part
# of the worker image key (digests.IMAGE_PATHS), so a new pin moves the key.
ZIG_VERSION = "0.16.0"
CARGO_ZIGBUILD_VERSION = "0.23.4"
RUSTC_VERSION = "1.98.1"
CARGO_HOME_REMAP = "/cargo-home"
# Variables that change what the compiler emits; the recipe sets its own.
STRIPPED = ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_RUSTFLAGS", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER",
            "CARGO_INCREMENTAL")
HEX64 = "0123456789abcdef"


class BuildError(Exception):
    """A failed build, selection check or equality (exit 1)."""


class ToolError(Exception):
    """A tool at the wrong version (exit 2)."""


def zigbuild_args(packages: Sequence[str] = WORKER_PACKAGES) -> list[str]:
    """The arguments after `cargo` of the cross-build."""
    # --locked: a stale Cargo.lock fails the build instead of being rewritten (root A and root B would resolve differently).
    return ["zigbuild", "--release", "--locked", *[a for p in packages for a in ("-p", p)], "--target", TARGET_ARG]


def cargo_home(env: Mapping[str, str]) -> str:
    return env.get("CARGO_HOME") or str(Path(env.get("HOME") or os.path.expanduser("~")) / ".cargo")


def rustflags(home: str) -> str:
    """Remaps the registry source paths rustc embeds in panic locations: without it the hash depends on CARGO_HOME."""
    return f"--remap-path-prefix={home}={CARGO_HOME_REMAP}"


def build_env(env: Mapping[str, str], *, target_dir: Path | str, profile: str, cargo_home_dir: str | None = None) -> dict[str, str]:
    """`env` plus the selection (the accounts.json profile harness-protocol's build script compiles in), the target
    directory and the remap, without the variables that change the compiler's output."""
    out = {k: v for k, v in env.items() if k not in STRIPPED}
    out.update(CARGO_TARGET_DIR=str(target_dir), HAMH_AWS_PROFILE=profile, AWS_PROFILE=profile, CARGO_INCREMENTAL="0",
               RUSTFLAGS=rustflags(cargo_home_dir or cargo_home(env)))
    return out


def recipe(root: Path | str, profile: str) -> dict:
    """Every element of the recipe. The digest of this document goes into the CI record and the image-build record."""
    manifest = Path(root) / "Cargo.toml"
    release = {}
    if manifest.is_file():
        release = tomllib.loads(manifest.read_text()).get("profile", {}).get("release", {})
    return {"argv": zigbuild_args(), "packages": list(WORKER_PACKAGES), "target": TARGET_ARG,
            "rustflags": rustflags("$CARGO_HOME"), "stripped": list(STRIPPED), "zig": ZIG_VERSION,
            "cargoZigbuild": CARGO_ZIGBUILD_VERSION, "rustc": RUSTC_VERSION, "profile": profile, "releaseProfile": release}


def recipe_digest(root: Path | str, profile: str) -> str:
    return hashlib.sha256(json.dumps(recipe(root, profile), sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def path_leaks(binary: Path, forbidden: Iterable[str]) -> list[str]:
    """The host paths (non-empty strings) the binary embeds."""
    needles = sorted({p for p in forbidden if p and len(p) > 3})
    found = build_selection.scan(Path(binary), [n.encode() for n in needles])
    return [n for n in needles if n.encode() in found]


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def _run_version(cmd: list[str], cwd: Path) -> str:
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, cwd=cwd, timeout=60)
    except (OSError, subprocess.TimeoutExpired) as e:
        return f"unavailable ({e})"
    return (p.stdout + p.stderr).strip() if p.returncode == 0 else f"unavailable (exit {p.returncode})"


def check_tools(root: Path, env: Mapping[str, str]) -> list[str]:
    """The installed tools that differ from the pins, as `zig 0.15.0, pinned 0.16.0`."""
    if env.get("AGENTD_BUILD_SKIP_TOOL_CHECK") == "1":
        return []
    bad = []
    zig = _run_version([env.get("AGENTD_BUILD_ZIG", "zig"), "version"], root).split()
    if zig[:1] != [ZIG_VERSION]:
        bad.append(f"zig {' '.join(zig) or 'unavailable'}, pinned {ZIG_VERSION}")
    zb = _run_version([env.get("AGENTD_BUILD_ZIGBUILD", "cargo-zigbuild"), "--version"], root).split()
    if zb[-1:] != [CARGO_ZIGBUILD_VERSION]:
        bad.append(f"cargo-zigbuild {' '.join(zb) or 'unavailable'}, pinned {CARGO_ZIGBUILD_VERSION}")
    rc = _run_version([env.get("AGENTD_BUILD_RUSTC", "rustc"), "--version"], root).split()
    if rc[1:2] != [RUSTC_VERSION]:
        bad.append(f"rustc {' '.join(rc) or 'unavailable'}, pinned {RUSTC_VERSION}")
    return bad


def deadline(env: Mapping[str, str]) -> float:
    """Seconds a cargo call may take (AGENTD_BUILD_TIMEOUT_S, default 3600): a hung fetch or link must not hold a CI shard."""
    return float(env.get("AGENTD_BUILD_TIMEOUT_S", "3600"))


def cargo_argv(root: Path, env: Mapping[str, str]) -> list[str]:
    slot = env.get("AGENTD_BUILD_SLOT")
    prefix = [str(root / "scripts" / "cargo-slot.sh")] if slot is None else shlex.split(slot)
    return [*prefix, *shlex.split(env.get("AGENTD_BUILD_CARGO", "cargo"))]


def build(root: Path, *, env: Mapping[str, str], home: str | None = None, label: str = "A") -> dict:
    """Runs the recipe in `root`; returns the facts of the two binaries. Raises BuildError."""
    root = Path(root)
    try:
        target, entries = aws_target.select(env), aws_target.accounts(env)
    except aws_target.TargetError as e:
        raise BuildError(f"root={label} {e}") from None
    tdir = build_selection.target_dir(root, target)
    benv = build_env(env, target_dir=tdir, profile=target.profile, cargo_home_dir=home)
    if home:
        benv["CARGO_HOME"] = home
    limit = deadline(env)
    try:
        proc = subprocess.run([*cargo_argv(root, env), *zigbuild_args()], cwd=root, env=benv, timeout=limit)
    except subprocess.TimeoutExpired:
        raise BuildError(f"root={label} cargo zigbuild timed out after {limit:g}s") from None
    release = tdir / TRIPLE / "release"
    files = {n: release / n for n in WORKER_PACKAGES}
    missing = [n for n, p in files.items() if not p.is_file()]
    if proc.returncode != 0 or missing:
        raise BuildError(f"root={label} cargo zigbuild exit {proc.returncode} missing {','.join(missing) or 'none'}")
    try:
        build_selection.check_build_output(tdir, target, entries)
        build_selection.check_binary(files["agentd"], "agentd", target, entries)
    except build_selection.Mismatch as e:
        raise BuildError(f"root={label} {e}") from None
    return {"label": label, "agentd": sha256_file(files["agentd"]),
            "file": files["agentd"], "home": home or cargo_home(env), "root": root, "profile": target.profile}


def line(b: dict, digest: str) -> str:
    return (f"AGENTD-BUILD root={b['label']} sha256={b['agentd']} file={b['file']} "
            f"cargoHome={b['home']} recipe={digest}")


def tracked_and_untracked(root: Path) -> list[str]:
    out = subprocess.run(["git", "-C", str(root), "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
                         capture_output=True, check=True).stdout
    return [p.decode() for p in out.split(b"\0") if p]


def copy_tree(root: Path, dest: Path) -> None:
    """The working tree's files (tracked, and untracked but not ignored) without history, modes and links kept."""
    for rel in tracked_and_untracked(root):
        src, dst = root / rel, dest / rel
        if not (src.is_file() or src.is_symlink()):
            continue  # deleted in the working tree but still listed in the index
        dst.parent.mkdir(parents=True, exist_ok=True)
        if src.is_symlink():
            os.symlink(os.readlink(src), dst)
        else:
            shutil.copy2(src, dst)


def leak_candidates(env: Mapping[str, str], *paths: object) -> list[str]:
    out = [str(p) for p in paths]
    for name in ("HOME", "TMPDIR", "RUSTUP_HOME", "CARGO_HOME", "CODEBUILD_SRC_DIR"):
        v = env.get(name)
        if v and v not in ("/", "/root"):
            out.append(v)
    return [*out, "/codebuild/output"]


def prove(root: Path, work: Path, *, env: Mapping[str, str], out_dir: Path | None = None) -> tuple[int, list[str]]:
    """Builds root A in place and root B from a copy; returns (exit code, output lines)."""
    root, work = Path(root).resolve(), Path(work)
    out: list[str] = []
    target = aws_target.select(env)
    digest = recipe_digest(root, target.profile)
    tag = recipe(root, target.profile)
    out.append(f"AGENTD-RECIPE digest={digest} target={tag['target']} rustflags=remap-cargo-home zig={ZIG_VERSION} "
               f"cargo-zigbuild={CARGO_ZIGBUILD_VERSION} rustc={RUSTC_VERSION} profile={target.profile}")
    try:
        a = build(root, env=env, label="A")
        out.append(line(a, digest))
        bdir = work / "b"
        src, home = bdir / "src", bdir / "cargo-home"
        if bdir.exists():
            raise BuildError(f"root=B {bdir} exists: WORK must be a fresh directory")
        home.mkdir(parents=True)
        copy_tree(root, src)
        benv = dict(env, CARGO_HOME=str(home))
        # crates.io answers 5xx for a second now and then: three attempts, pauses of 0, 1 and 4 units.
        unit = float(env.get("AGENTD_BUILD_RETRY_SLEEP", "5"))
        rc = 0
        for pause in (0, 1, 4):
            time.sleep(pause * unit)
            try:
                rc = subprocess.run([*cargo_argv(src, env), "fetch", "--locked"], cwd=src, env=benv, timeout=deadline(env)).returncode
            except subprocess.TimeoutExpired:
                rc = 124
            if rc == 0:
                break
        if rc != 0:
            raise BuildError(f"root=B cargo fetch --locked exit {rc}")
        b = build(src, env=benv, home=str(home), label="B")
        out.append(line(b, digest))
    except BuildError as e:
        out.append(f"AGENTD-BUILD FAIL {e}")
        out.append("AGENTD-EQUAL FAIL build")
        return 1, out
    leaks_bad = False
    for x in (a, b):
        found = path_leaks(x["file"], leak_candidates(env, a["home"], b["home"], a["root"], b["root"], work))
        out.append(f"AGENTD-LEAKS root={x['label']} " + ("FAIL " + " ".join(found) if found else "none"))
        leaks_bad = leaks_bad or bool(found)
    if a["agentd"] != b["agentd"]:
        out.append("AGENTD-EQUAL FAIL a-and-b-differ")
        return 1, out
    if leaks_bad:
        out.append("AGENTD-EQUAL FAIL leaks")
        return 1, out
    out.append(f"AGENTD-PRODUCTION-SHA256 {b['agentd']} source=rebuild-B")
    out.append("AGENTD-EQUAL PASS")
    if out_dir is not None:
        out_dir.mkdir(parents=True, exist_ok=True)
        dest = out_dir / "agentd"
        shutil.copy2(a["file"], dest)
        dest.chmod(0o555)
        staged = sha256_file(dest)
        if staged != a["agentd"]:
            # The qualification runs the staged file; it must be the binary that was hashed and compared.
            out.remove("AGENTD-EQUAL PASS")
            out.remove(f"AGENTD-PRODUCTION-SHA256 {b['agentd']} source=rebuild-B")
            out.append("AGENTD-EQUAL FAIL staged-differs")
            return 1, out
        out.append(f"AGENTD-STAGED {staged} file={dest}")
    return 0, out


def main(argv: Sequence[str]) -> int:
    ap = argparse.ArgumentParser(prog="agentd_build.py")
    sub = ap.add_subparsers(dest="cmd", required=True)
    for name in ("build", "prove"):
        p = sub.add_parser(name)
        p.add_argument("--root", required=True)
        p.add_argument("--out-dir")
        if name == "prove":
            p.add_argument("--work", required=True)
    a = ap.parse_args(list(argv))
    root = Path(a.root).resolve()
    env = dict(os.environ)
    bad = check_tools(root, env)
    if bad:
        print("AGENTD-BUILD FAIL tools " + "; ".join(bad))
        print("agentd_build: " + "; ".join(bad), file=sys.stderr)
        return 2
    if a.cmd == "build":
        try:
            target = aws_target.select(env)
            b = build(root, env=env)
        except (BuildError, aws_target.TargetError) as e:
            print(f"AGENTD-BUILD FAIL {e}")
            return 1
        print(line(b, recipe_digest(root, target.profile)))
        if a.out_dir:
            dest = Path(a.out_dir) / "agentd"
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(b["file"], dest)
            dest.chmod(0o555)
            print(f"AGENTD-STAGED {sha256_file(dest)} file={dest}")
        return 0
    try:
        code, lines = prove(root, Path(a.work), env=env, out_dir=Path(a.out_dir) if a.out_dir else None)
    except aws_target.TargetError as e:
        print(f"AGENTD-BUILD FAIL {e}")
        return 1
    print("\n".join(lines))
    return code


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
