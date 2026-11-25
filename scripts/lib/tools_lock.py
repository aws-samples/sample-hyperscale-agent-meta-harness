#!/usr/bin/env python3
"""Reader and installer for tools.lock.

  tools_lock.py install --lock tools.lock --dest <HAMH_TOOLS> [--only a,b] [--allow-file-urls]
  tools_lock.py gate-tools --lock tools.lock --dest <HAMH_TOOLS> [--installer <install.sh>] [--allow-file-urls]
  tools_lock.py check --lock tools.lock --dest <HAMH_TOOLS> [--allow-file-urls]   read-only: each tool at its pin
  tools_lock.py get --lock tools.lock <table.key>      print one scalar, e.g. rust.toolchain
  tools_lock.py list --lock tools.lock                 print "<name> <version>" per tool

Every download is verified against its pinned SHA-256 before anything is unpacked. Tools unpack to
<dest>/pkgs/<name>-<version>/ and each `bin` entry is linked into <dest>/bin. A tool may name further sources of the
same archive in `mirrors` (a list of https URLs) and then names the archive's `size` in bytes: install draws their order
at random for each install, tries at most SOURCE_TRIES of them before `url`, gives one up when it fails, delivers other bytes than the pinned SHA-256, sends more than `size` bytes or is not
complete after SOURCE_LIMIT_S seconds, removes its file and says so in a
`TOOL SOURCE <name> <host> given up after <seconds>s: <why>` line; <why> is one line of at most REASON_MOST
characters of printable ASCII, whatever the source sent. The `url` is always the last source and behaves as
the one source of a tool without `mirrors`: no limit of its own, and its failure is the tool's `TOOL FAIL` line and exit code. `size` is
read only where `mirrors` names a source: a tool without further sources is read as it was before the key existed.
install prints its lines as they happen: `TOOL GET <name> <version>` when a tool's download starts, then
`TOOL OK <name> <version> <seconds>s from <host>` or `TOOL FAIL <name> ...`; a tool at its pin prints `TOOL OK <name> <version> (present)`. A dest outside the
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
import http.client
import json
import os
import random
import re
import shutil
import subprocess  # nosec B404: starts programs with argument lists, never through a shell
import sys
import tarfile
import tempfile
import threading
import time
import tomllib
import urllib.parse
import urllib.request
import zipfile
from collections.abc import Callable
from pathlib import Path

KINDS = ("binary", "tar.gz", "tar.xz", "zip")
INSTALLER = Path(__file__).resolve().parents[2] / "images" / "tools" / "install.sh"
SHA_RE = re.compile(r"^[0-9a-f]{64}$")
USER_AGENT = {"User-Agent": "hamh-bootstrap"}
# A source that is not a tool's last one is given up when its whole download (connection, redirects and body) has taken this many seconds.
# 60 is three times the 18 to 20 seconds in which the five tools of a setup build usually download together (47 build logs), and a sixth of the
# seven minutes one slow server needed at its best on the day this limit was set. The last source has no such limit and behaves as the one
# source of a tool always has, so a download that passes without further sources cannot fail because of this number.
SOURCE_LIMIT_S = 60.0
# At most this many of a tool's further sources are asked before its last one, whatever their number in the lock: further sources that all stall
# can then add no more than twice the limit above to an install.
SOURCE_TRIES = 2
# Why a source was given up is printed as one line of at most this many characters (one_line), so that a source's own words cannot fill a log.
REASON_MOST = 300


class LockError(ValueError):
    pass


def load(path: Path | str, allow_file_urls: bool = False) -> dict:
    try:
        lock = tomllib.loads(Path(path).read_text())
    except (OSError, tomllib.TOMLDecodeError) as exc:
        raise LockError(f"{path}: {exc}") from exc
    if lock.get("schema") != "1":
        raise LockError('schema must be "1"')

    def allowed_url(url: object) -> bool:
        return isinstance(url, str) and (url.startswith("https://") or (allow_file_urls and url.startswith("file://")))

    seen = set()
    for tool in lock.get("tool", []):
        name = tool.get("name")
        for key in ("name", "version", "url", "sha256", "kind", "bin"):
            if key not in tool:
                raise LockError(f"tool {name}: missing {key}")
        if name in seen:
            raise LockError(f"duplicate tool {name}")
        seen.add(name)
        if not allowed_url(tool["url"]):
            raise LockError(f"tool {name}: url must be https://")
        mirrors = tool.get("mirrors", [])
        if not isinstance(mirrors, list) or not all(allowed_url(m) for m in mirrors):
            raise LockError(f"tool {name}: mirrors must be a list of https:// URLs")
        if mirrors:
            # `size` is the ceiling of a further source and is read only here, where one is named: a tool without further sources is read as it
            # was before the key existed, whatever its entry holds under `size`.
            size = tool.get("size")
            if size is None:
                raise LockError(f"tool {name}: mirrors need size, the archive's size in bytes: a further source is given up when it sends more")
            if type(size) is not int or size < 1:
                raise LockError(f"tool {name}: size must be the archive's size in bytes, a whole number above 0")
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
    req = urllib.request.Request(url, headers=USER_AGENT)
    with urllib.request.urlopen(req, timeout=300) as resp, out.open("wb") as fh:  # nosec B310: URL from the pinned tools.lock that holds https URLs only, archive checked against its SHA-256 before unpacking  # nosemgrep: dynamic-urllib-use-detected -- URL from the pinned tools.lock that holds https URLs only, archive checked against its SHA-256 before unpacking
        shutil.copyfileobj(resp, fh, length=1 << 20)


def fetch_within(url: str, out: Path, limit_s: float, most: int) -> str:
    """Download url into out and give up when the whole download has taken limit_s seconds, or at once when the source sends more than `most` bytes
    (the archive's size in the lock). Returns "" when the body came to its end, else why not. out never holds more than `most` bytes.

    The download runs in a thread, so a server that connects slowly, sends nothing or trickles is left exactly at the limit. The thread creates no
    file: it writes into the one opened here, and it ends by itself at its next read or when its socket's timeout (the limit) runs out; after the
    limit it writes nothing more. Whichever of the two ends a download at the limit, this wait or the socket's timeout, the reason reads
    "not complete after <limit> s"."""
    done: list[str] = []
    stop = threading.Event()
    fh = out.open("wb")

    def work() -> None:
        room = most
        try:
            with fh:
                req = urllib.request.Request(url, headers=USER_AGENT)
                with urllib.request.urlopen(req, timeout=limit_s) as resp:  # nosec B310: URL from the pinned tools.lock that holds https URLs only, archive checked against its SHA-256 before unpacking  # nosemgrep: dynamic-urllib-use-detected -- URL from the pinned tools.lock that holds https URLs only, archive checked against its SHA-256 before unpacking
                    while not stop.is_set():
                        chunk = resp.read1(1 << 16)
                        if stop.is_set():
                            return
                        if not chunk:
                            done.append("")
                            return
                        if len(chunk) > room:
                            done.append(f"more than the archive's {most} bytes")
                            return
                        room -= len(chunk)
                        fh.write(chunk)
        except (OSError, http.client.HTTPException, ValueError) as exc:
            done.append(str(exc) or type(exc).__name__)

    worker = threading.Thread(target=work, daemon=True)
    started = time.monotonic()
    worker.start()
    worker.join(limit_s)
    # The download's own timeout has the limit's length too. On a crowded machine it can end the download with its own words before this wait
    # is back: a download that failed when the limit was over is the same end, said in the same words.
    if worker.is_alive() or not done or (done[0] and time.monotonic() - started >= limit_s):
        stop.set()
        return f"not complete after {limit_s:g} s"
    return done[0]


def source_host(url: str) -> str:
    """The name of a source in a TOOL line: the host of its URL, and `file` for a file URL."""
    return urllib.parse.urlsplit(url).netloc or "file"


def draw_sources(mirrors: list[str]) -> list[str]:
    """The further sources of a tool in an order drawn at random, anew for each install, so that no one source carries every build."""
    return random.SystemRandom().sample(mirrors, len(mirrors))


def one_line(text: str) -> str:
    """text as one line of at most REASON_MOST characters of printable ASCII: every other character (a line break, a tab, a control character, a
    character above 126) becomes a blank, blanks in a row become one, and a longer text is cut and ends in "...".

    Why a source was given up can hold the source's own words (the reason text of its answer). They reach a TOOL line only through this, so a
    source cannot start a line of its own in a log that follows the installer, and printing the line cannot fail on an output that takes ASCII
    only."""
    line = " ".join("".join(ch if " " <= ch <= "~" else " " for ch in text).split())
    return line if len(line) <= REASON_MOST else line[: REASON_MOST - 3] + "..."


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


def say(line: str) -> None:
    """Print one TOOL line and hand it on at once, so that a log which follows the installer holds each line, and its time, when it happens."""
    print(line, flush=True)


def download(tool: dict, tmp: Path, limit_s: float, draw: Callable[[list[str]], list[str]] | None = None) -> tuple[Path | None, str, int]:
    """The tool's archive in tmp, from its sources: at most SOURCE_TRIES of the `mirrors` of its lock entry, in the order that `draw` gives them
    (draw_sources, a random order, when none is passed), then its `url`. Returns (archive, the source's host, 0) or (None, "", exit code).

    Every source must deliver the pinned SHA-256. A mirror that fails, delivers other bytes, sends more than the entry's `size` bytes or is not
    complete after limit_s seconds is given up: its file is removed, a TOOL SOURCE line says why in one line (one_line), and the next source is
    tried. The `url` is the last source: no limit of this function applies to it, and its failure is the TOOL FAIL line and the exit code of a
    tool with one source (5 for a download that fails, 4 for other bytes)."""
    name, want = tool["name"], tool["sha256"]
    mirrors = tool.get("mirrors", [])
    for n, url in enumerate((draw or draw_sources)(mirrors)[:SOURCE_TRIES] if mirrors else []):
        started = time.monotonic()
        archive = tmp / f"download-{n}"
        why = fetch_within(url, archive, limit_s, tool["size"])
        if not why:
            got = sha256_file(archive)
            if got == want:
                return archive, source_host(url), 0
            why = f"sha256 want={want} got={got}"
        archive.unlink(missing_ok=True)
        say(f"TOOL SOURCE {name} {source_host(url)} given up after {time.monotonic() - started:.1f}s: {one_line(why)}")
    archive = tmp / "download"
    try:
        fetch(tool["url"], archive)
    except OSError as exc:
        say(f"TOOL FAIL {name} download {exc}")
        return None, "", 5
    got = sha256_file(archive)
    if got != want:
        say(f"TOOL FAIL {name} sha256 want={want} got={got}")
        return None, "", 4
    return archive, source_host(tool["url"]), 0


def install(lock: dict, dest: Path, only: set[str] | None, limit_s: float = SOURCE_LIMIT_S, draw: Callable[[list[str]], list[str]] | None = None) -> int:
    """Install the tools of the lock (those named in `only`, when it is given) under dest. limit_s is the time a source that is not a tool's
    last one gets for its whole download (SOURCE_LIMIT_S; the tests pass a short one). draw gives the order in which a tool's further sources
    are asked (download); the tests pass a fixed one, and no caller of the product passes any.

    The lines come as they happen. A tool that is at its pin prints `TOOL OK <name> <version> (present)`. Any other tool prints
    `TOOL GET <name> <version>` when its download starts and then `TOOL OK <name> <version> <seconds>s from <host>` (the seconds since its
    TOOL GET line, the host of the source that delivered the archive) or its TOOL FAIL line."""
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
            say(f"TOOL OK {name} {version} (present)")
            continue
        say(f"TOOL GET {name} {version}")
        started = time.monotonic()
        with tempfile.TemporaryDirectory(dir=dest, prefix=".dl-") as tmp:
            archive, host, code = download(tool, Path(tmp), limit_s, draw)
            if archive is None:
                rc = 4 if code == 4 else rc or code
                continue
            staging = Path(tmp) / "pkg"
            staging.mkdir()
            try:
                unpack(tool, archive, staging)
            except (OSError, tarfile.TarError, zipfile.BadZipFile) as exc:
                say(f"TOOL FAIL {name} unpack {exc}")
                rc = rc or 5
                continue
            if pkg.exists():
                shutil.rmtree(pkg)
            pkg.parent.mkdir(parents=True, exist_ok=True)
            staging.rename(pkg)
        link_bins(tool, pkg, dest / "bin")
        state.write_text(json.dumps(want) + "\n")
        say(f"TOOL OK {name} {version} {time.monotonic() - started:.1f}s from {host}")
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
