#!/usr/bin/env python3
"""Reader and installer for tools.lock.

  tools_lock.py install --lock tools.lock --dest <HAMH_TOOLS> [--only a,b] [--allow-file-urls]
  tools_lock.py gate-tools --lock tools.lock --dest <HAMH_TOOLS> [--installer <install.sh>] [--allow-file-urls]
  tools_lock.py check --lock tools.lock --dest <HAMH_TOOLS> [--allow-file-urls]   read-only: each tool at its pin
  tools_lock.py get --lock tools.lock <table.key>      print one scalar, e.g. rust.toolchain
  tools_lock.py list --lock tools.lock                 print "<name> <version>" per tool

Every download is verified against its pinned SHA-256 before anything is unpacked. Tools unpack to
<dest>/pkgs/<name>-<version>/ and each `bin` entry is linked into <dest>/bin. A dest outside the
workspace roots is refused, so nothing lands under /usr or /opt. The roots are the working directory and TMPDIR
(a ham session needs no path written for it); TOOLS_LOCK_ROOTS (colon separated), when set, replaces that list.

check reads only <dest>: a tool is present when its <dest>/.installed/<name>.json state equals its lock
version and sha256 and every `bin` entry exists; it prints TOOL OK <name> <version> (present) or
TOOL FAIL <name> not installed and exits 1 when any tool is missing. It never downloads or unpacks.

gate-tools builds the harness-gates tool set at <dest>/gate-tools: it installs the [gate_tools]
tools (uv, gitleaks), then CPython [gate_tools].python through `uv python install --no-bin` under
<dest>/python, then runs images/tools/install.sh <dest>/gate-tools <python> <uv>
<gitleaks>. Exit codes: 0 ok, 2 usage or refused dest, 3 invalid lock, 4 checksum mismatch,
5 download, unpack or install failure.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess  # nosec B404: starts programs with argument lists, never through a shell
import sys
import tarfile
import tempfile
import tomllib
import urllib.request
import zipfile
from pathlib import Path

KINDS = ("binary", "tar.gz", "tar.xz", "zip")
INSTALLER = Path(__file__).resolve().parents[2] / "images" / "tools" / "install.sh"
SHA_RE = re.compile(r"^[0-9a-f]{64}$")


class LockError(ValueError):
    pass


def load(path: Path | str, allow_file_urls: bool = False) -> dict:
    try:
        lock = tomllib.loads(Path(path).read_text())
    except (OSError, tomllib.TOMLDecodeError) as exc:
        raise LockError(f"{path}: {exc}") from exc
    if lock.get("schema") != "1":
        raise LockError('schema must be "1"')
    seen = set()
    for tool in lock.get("tool", []):
        name = tool.get("name")
        for key in ("name", "version", "url", "sha256", "kind", "bin"):
            if key not in tool:
                raise LockError(f"tool {name}: missing {key}")
        if name in seen:
            raise LockError(f"duplicate tool {name}")
        seen.add(name)
        url = tool["url"]
        if not (url.startswith("https://") or (allow_file_urls and url.startswith("file://"))):
            raise LockError(f"tool {name}: url must be https://")
        if not SHA_RE.match(tool["sha256"]):
            raise LockError(f"tool {name}: sha256 must be 64 lowercase hex")
        if tool["kind"] not in KINDS:
            raise LockError(f"tool {name}: kind must be one of {KINDS}")
        if not isinstance(tool["bin"], list):
            raise LockError(f"tool {name}: bin must be a list")
    return lock


def dest_roots() -> list[str]:
    """The directories a destination may be in. TOOLS_LOCK_ROOTS, when it is set, is the whole list (colon separated). Otherwise: the working
    directory and TMPDIR, the two places a command can count on on any machine, a ham session included. A caller that
    installs elsewhere names its own destination in TOOLS_LOCK_ROOTS. A directory that is not an absolute path, or that is the root of the file
    system, is left out: it would allow every destination."""
    named = os.environ.get("TOOLS_LOCK_ROOTS")
    if named is not None:
        return named.split(":")
    try:
        here = os.getcwd()
    except OSError:
        here = ""
    roots = []
    for candidate in (here, os.environ.get("TMPDIR", "")):
        if os.path.isabs(candidate) and os.path.realpath(candidate) != os.sep:
            roots.append(candidate)
    return roots


def allowed_dest(dest: Path) -> bool:
    roots = dest_roots()
    real = Path(os.path.realpath(dest))
    for root in filter(None, roots):
        r = Path(os.path.realpath(root))
        if real == r or r in real.parents:
            return True
    return False


def fetch(url: str, out: Path) -> None:
    req = urllib.request.Request(url, headers={"User-Agent": "hamh-bootstrap"})
    with urllib.request.urlopen(req, timeout=300) as resp, out.open("wb") as fh:  # nosec B310: URL from the pinned tools.lock that holds https URLs only, archive checked against its SHA-256 before unpacking  # nosemgrep: dynamic-urllib-use-detected -- URL from the pinned tools.lock that holds https URLs only, archive checked against its SHA-256 before unpacking
        shutil.copyfileobj(resp, fh, length=1 << 20)


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def unpack(tool: dict, archive: Path, into: Path) -> None:
    kind = tool["kind"]
    if kind == "binary":
        target = into / tool["bin"][0]
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(archive, target)
    elif kind in ("tar.gz", "tar.xz"):
        with tarfile.open(archive, "r:*") as tar:
            tar.extractall(into, filter="data")
    else:
        with zipfile.ZipFile(archive) as zf:
            for info in zf.infolist():
                zf.extract(info, into)
                mode = info.external_attr >> 16
                if mode:
                    os.chmod(into / info.filename, mode & 0o7777)
    for rel in tool["bin"]:
        p = into / rel
        p.chmod(p.stat().st_mode | 0o755)


def link_bins(tool: dict, pkg: Path, bindir: Path) -> None:
    bindir.mkdir(parents=True, exist_ok=True)
    for rel in tool["bin"]:
        link = bindir / Path(rel).name
        if link.is_symlink() or link.exists():
            link.unlink()
        link.symlink_to(pkg / rel)


def install(lock: dict, dest: Path, only: set[str] | None) -> int:
    state_dir = dest / ".installed"
    state_dir.mkdir(parents=True, exist_ok=True)
    rc = 0
    for tool in lock.get("tool", []):
        name, version = tool["name"], tool["version"]
        if only and name not in only:
            continue
        pkg = dest / "pkgs" / f"{name}-{version}"
        state = state_dir / f"{name}.json"
        want = {"version": version, "sha256": tool["sha256"]}
        if state.exists() and json.loads(state.read_text()) == want and all((pkg / b).exists() for b in tool["bin"]):
            link_bins(tool, pkg, dest / "bin")
            print(f"TOOL OK {name} {version} (present)")
            continue
        with tempfile.TemporaryDirectory(dir=dest, prefix=".dl-") as tmp:
            archive = Path(tmp) / "download"
            try:
                fetch(tool["url"], archive)
            except OSError as exc:
                print(f"TOOL FAIL {name} download {exc}")
                rc = rc or 5
                continue
            got = sha256_file(archive)
            if got != tool["sha256"]:
                print(f"TOOL FAIL {name} sha256 want={tool['sha256']} got={got}")
                rc = 4
                continue
            staging = Path(tmp) / "pkg"
            staging.mkdir()
            try:
                unpack(tool, archive, staging)
            except (OSError, tarfile.TarError, zipfile.BadZipFile) as exc:
                print(f"TOOL FAIL {name} unpack {exc}")
                rc = rc or 5
                continue
            if pkg.exists():
                shutil.rmtree(pkg)
            pkg.parent.mkdir(parents=True, exist_ok=True)
            staging.rename(pkg)
        link_bins(tool, pkg, dest / "bin")
        state.write_text(json.dumps(want) + "\n")
        print(f"TOOL OK {name} {version}")
    return rc


def check(lock: dict, dest: Path) -> int:
    rc = 0
    for tool in lock.get("tool", []):
        name, version = tool["name"], tool["version"]
        pkg, state = dest / "pkgs" / f"{name}-{version}", dest / ".installed" / f"{name}.json"
        try:
            ok = json.loads(state.read_text()) == {"version": version, "sha256": tool["sha256"]}
        except (OSError, ValueError):
            ok = False
        if ok and all((pkg / b).exists() for b in tool["bin"]):
            print(f"TOOL OK {name} {version} (present)")
        else:
            print(f"TOOL FAIL {name} not installed")
            rc = 1
    return rc


def gate_tools(lock: dict, dest: Path, installer: Path) -> int:
    table = lock.get("gate_tools")
    if not isinstance(table, dict) or not isinstance(table.get("python"), str) or not isinstance(table.get("tools"), list):
        print("invalid lock: [gate_tools] needs python and tools", file=sys.stderr)
        return 3
    names = {t["name"] for t in lock.get("tool", [])}
    missing = sorted(set(table["tools"]) - names)
    if missing or not {"uv", "gitleaks"} <= set(table["tools"]):
        print(f"invalid lock: [gate_tools] tools must name uv and gitleaks entries (missing {missing})", file=sys.stderr)
        return 3
    rc = install(lock, dest, set(table["tools"]))
    if rc:
        return rc
    version = table["python"]
    uv, gitleaks = dest / "bin" / "uv", dest / "bin" / "gitleaks"
    env = dict(os.environ, UV_PYTHON_INSTALL_DIR=str(dest / "python"))
    found = None
    if subprocess.run([str(uv), "python", "install", "--no-bin", version], env=env, cwd=dest).returncode == 0:  # nosec B603: argument list built here, no shell, program is git, the interpreter or a configured binary  # nosemgrep: dangerous-subprocess-use-audit -- argument list built here from its own options and paths, no shell
        proc = subprocess.run([str(uv), "python", "find", "--managed-python", "--no-project", version],  # nosec B603: argument list built here, no shell, program is git, the interpreter or a configured binary  # nosemgrep: dangerous-subprocess-use-audit -- argument list built here from its own options and paths, no shell
                              env=env, cwd=dest, capture_output=True, text=True)
        if proc.returncode == 0 and proc.stdout.strip():
            found = proc.stdout.strip().splitlines()[-1]
    if not found:
        print(f"GATE-TOOLS FAIL python {version} not installed under {dest / 'python'}")
        return 5
    rc = subprocess.run(["bash", str(installer), str(dest / "gate-tools"), found, str(uv), str(gitleaks)]).returncode  # nosec B603: argument list built here, no shell, program is git, the interpreter or a configured binary; B607: bare name of a standard tool such as git, bash, tar or aws looked up on the PATH of the host running this tool
    if rc != 0:
        print(f"GATE-TOOLS FAIL installer exit {rc}")
        return 5
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(prog="tools_lock.py")
    sub = ap.add_subparsers(dest="cmd", required=True)
    p_install = sub.add_parser("install")
    p_install.add_argument("--lock", required=True)
    p_install.add_argument("--dest", required=True)
    p_install.add_argument("--only", default="")
    p_install.add_argument("--allow-file-urls", action="store_true")
    p_get = sub.add_parser("get")
    p_get.add_argument("--lock", required=True)
    p_get.add_argument("key")
    p_list = sub.add_parser("list")
    p_list.add_argument("--lock", required=True)
    p_check = sub.add_parser("check")
    p_check.add_argument("--lock", required=True)
    p_check.add_argument("--dest", required=True)
    p_check.add_argument("--allow-file-urls", action="store_true")
    p_gate = sub.add_parser("gate-tools")
    p_gate.add_argument("--lock", required=True)
    p_gate.add_argument("--dest", required=True)
    p_gate.add_argument("--installer", default=str(INSTALLER))
    p_gate.add_argument("--allow-file-urls", action="store_true")
    args = ap.parse_args()

    if args.cmd in ("install", "gate-tools") and not allowed_dest(Path(args.dest)):
        print(f"refusing dest {args.dest}: outside the working directory and TMPDIR (TOOLS_LOCK_ROOTS names other roots)", file=sys.stderr)
        return 2
    try:
        lock = load(args.lock, allow_file_urls=getattr(args, "allow_file_urls", False))
    except LockError as exc:
        print(f"invalid lock: {exc}", file=sys.stderr)
        return 3
    if args.cmd == "get":
        value: object = lock
        for part in args.key.split("."):
            if not isinstance(value, dict) or part not in value:
                print(f"no key {args.key}", file=sys.stderr)
                return 3
            value = value[part]
        print(value)
        return 0
    if args.cmd == "list":
        for tool in lock.get("tool", []):
            print(tool["name"], tool["version"])
        return 0
    dest = Path(args.dest)
    if args.cmd == "check":
        return check(lock, dest)
    dest.mkdir(parents=True, exist_ok=True)
    if args.cmd == "gate-tools":
        return gate_tools(lock, dest, Path(args.installer))
    only = {s for s in args.only.split(",") if s} or None
    return install(lock, dest, only)


if __name__ == "__main__":
    sys.exit(main())
