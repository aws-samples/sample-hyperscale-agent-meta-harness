#!/usr/bin/env python3
"""Runs the production `agentd` (aarch64, glibc 2.34) on an x86_64 host: QEMU user-mode binfmt and the glibc sysroot
.

  arm64_sysroot.py ensure --dest DIR     sysroot in DIR and an aarch64-capable host; prints `ARM64-SYSROOT PASS ...` (exit 0)

An aarch64 ELF needs two things on an x86_64 host. The kernel must hand it to a QEMU user-mode interpreter,
and the emulated process needs an aarch64 dynamic loader and libc (`QEMU_LD_PREFIX`). The sysroot holds only
`lib/ld-linux-aarch64.so.1`, `lib64/libc.so.6` and `lib64/libm.so.6` (the `NEEDED` set of `agentd`), copied out of a container
created (never started) from the digest-pinned arm64 image that the worker image is built FROM
(`images/worker-python/Dockerfile`, the arm64 child of the index it names), each file checked against the SHA-256 pinned
below. Nothing else of that image is visible to the emulated process: a path the sysroot does not hold resolves on the host,
so the x86_64 fake engine, git and sh run natively.

Whether the host can run an aarch64 ELF is decided by running the sysroot's loader (`ld-linux-aarch64.so.1 --version`), not by
reading binfmt_misc: a build container may not mount that file although the kernel's registration applies to its processes.
A host whose loader fails is registered through the pinned binfmt image and probed again.

The pinned digest equals tools.lock `[containers] al2023_arm64` (test_arm64_sysroot.py). A host that cannot register binfmt or
pull the image fails the step; nothing is skipped. A pull or registration that fails is tried three times (pauses of 0, 1 and 4 units,
ARM64_SYSROOT_RETRY_SLEEP seconds, default 5): a shared egress can fail a Docker Hub or public ECR pull for a second. Test hooks:
ARM64_SYSROOT_DOCKER (the docker command), ARM64_BINFMT_PATH (the binfmt_misc entry), ARM64_SYSROOT_RETRY_SLEEP.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import shlex
import struct
import subprocess  # nosec B404: starts programs with argument lists, never through a shell
import sys
import time
from pathlib import Path
from typing import Mapping, Sequence

IMAGE = "public.ecr.aws/amazonlinux/amazonlinux@sha256:6b60f399998548f63d564a8591eb9493a1aed0d6ef447db83b9ea884ed82065d"
# dest-relative path -> (path inside the image, sha256) for glibc-2.34-231.amzn2023.0.5.aarch64
FILES = {
    "lib/ld-linux-aarch64.so.1": ("/usr/lib/ld-linux-aarch64.so.1", "054dbd4ee63da000baee7f71ce5a92b733f3e6ffdcd77bfbdbb3997637d11721"),
    "lib64/libc.so.6": ("/usr/lib64/libc.so.6", "cb88fe0c003423df781bc79f34a1063b8f11deeeefeec6dac5f2102f3022d795"),
    "lib64/libm.so.6": ("/usr/lib64/libm.so.6", "7e67f0d75af6b409514f2b2f7a22e42ac1981cfdeb1b8d215f184803e67bb00f"),
}
BINFMT_PATH = "/proc/sys/fs/binfmt_misc/qemu-aarch64"
E_MACHINE = {62: "x86_64", 183: "aarch64"}


class SysrootError(Exception):
    pass


def elf_arch(path: Path | str) -> str | None:
    """`aarch64` or `x86_64` from the ELF header; None for anything else."""
    with open(path, "rb") as f:
        head = f.read(20)
    if len(head) < 20 or head[:4] != b"\x7fELF":
        return None
    return E_MACHINE.get(struct.unpack_from("<H", head, 18)[0])


def host_arch() -> str:
    return os.uname().machine


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def retrying(env: Mapping[str, str], attempt):
    """Runs `attempt()` (returns None on success, else an error text) up to three times; returns the last error or None."""
    unit = float(env.get("ARM64_SYSROOT_RETRY_SLEEP", "5"))
    err = None
    for pause in (0, 1, 4):
        time.sleep(pause * unit)
        err = attempt()
        if err is None:
            return None
    return err


def docker_cmd(env: Mapping[str, str]) -> list[str]:
    return shlex.split(env.get("ARM64_SYSROOT_DOCKER", "docker"))


def binfmt_entry(env: Mapping[str, str]) -> dict | None:
    """The parsed qemu-aarch64 binfmt_misc entry, None when it is absent or disabled."""
    path = Path(env.get("ARM64_BINFMT_PATH", BINFMT_PATH))
    try:
        lines = path.read_text().splitlines()
    except OSError:
        return None
    kv = {ln.split(" ", 1)[0].rstrip(":"): ln.split(" ", 1)[1].strip() for ln in lines[1:] if " " in ln}
    if not lines or lines[0].strip() != "enabled":
        return None
    return {"interpreter": kv.get("interpreter"), "flags": kv.get("flags", "")}


def probe_loader(loader: Path, env: Mapping[str, str]) -> tuple[bool, str]:
    """Runs the aarch64 loader: it prints its version only where the kernel hands aarch64 ELFs to QEMU."""
    try:
        p = subprocess.run([str(loader), "--version"], capture_output=True, text=True, timeout=60, env=dict(env))  # nosec B603: argument list built here, no shell, program is git, the interpreter or a configured binary  # nosemgrep: dangerous-subprocess-use-audit -- argument list built here from its own options and paths, no shell
    except (OSError, subprocess.TimeoutExpired) as e:
        return False, str(e)
    text = (p.stdout + p.stderr).strip()
    return (p.returncode == 0 and "GNU libc" in text), (text.splitlines() or [f"exit {p.returncode}"])[0]


def ensure_binfmt(env: Mapping[str, str], loader: Path, binfmt_image: str | None = None) -> dict:
    """The host runs aarch64 ELFs: as it is, or after registering QEMU through the pinned binfmt image."""
    entry = binfmt_entry(env) or {"interpreter": "unreadable", "flags": "unreadable"}
    ok, detail = probe_loader(loader, env)
    how = "present"
    if not ok:
        image = binfmt_image or tools_lock_get("containers.binfmt")
        def register() -> str | None:
            proc = subprocess.run([*docker_cmd(env), "run", "--privileged", "--rm", image, "--install", "arm64"],  # nosec B603: argument list built here, no shell, program is git, the interpreter or a configured binary  # nosemgrep: dangerous-subprocess-use-audit -- argument list built here from its own options and paths, no shell
                                  capture_output=True, text=True, env=dict(env))
            return None if proc.returncode == 0 else f"exit {proc.returncode}: {(proc.stderr or proc.stdout).strip()[-300:]}"

        err = retrying(env, register)
        if err is not None:
            raise SysrootError(f"binfmt registration failed ({err})")
        ok, detail = probe_loader(loader, env)
        how = "registered-now"
        if not ok:
            raise SysrootError(f"the host still cannot run an aarch64 ELF after the binfmt registration: {detail}")
        entry = binfmt_entry(env) or entry
    return {**entry, "how": how, "probe": detail}


def tools_lock_get(key: str) -> str:
    import tomllib  # noqa: PLC0415
    lock = tomllib.loads((Path(__file__).resolve().parents[2] / "tools.lock").read_text())
    node = lock
    for part in key.split("."):
        node = node[part]
    return node


def verify(dest: Path) -> list[str]:
    """The files of the sysroot that are missing or differ from their pins."""
    bad = []
    for rel, (_, sha) in FILES.items():
        p = dest / rel
        if not p.is_file():
            bad.append(f"{rel} missing")
        elif sha256_file(p) != sha:
            bad.append(f"{rel} sha256 {sha256_file(p)[:12]} != {sha[:12]}")
    return bad


def _docker(env: Mapping[str, str], *args: str) -> str:
    out: list[str] = []

    def attempt() -> str | None:
        proc = subprocess.run([*docker_cmd(env), *args], capture_output=True, text=True, env=dict(env))  # nosec B603: argument list built here, no shell, program is git, the interpreter or a configured binary  # nosemgrep: dangerous-subprocess-use-audit -- argument list built here from its own options and paths, no shell
        if proc.returncode == 0:
            out[:] = [proc.stdout.strip()]
            return None
        return f"docker {args[0]} exit {proc.returncode}: {(proc.stderr or proc.stdout).strip()[-300:]}"

    err = retrying(env, attempt)
    if err is not None:
        raise SysrootError(f"sysroot extraction failed ({err})")
    return out[0]


def ensure_sysroot(dest: Path, env: Mapping[str, str]) -> dict:
    """The sysroot at `dest`, copied out of a created container of the pinned image when a file is missing or wrong."""
    dest = Path(dest)
    if verify(dest):
        dest.mkdir(parents=True, exist_ok=True)
        cid = _docker(env, "create", "--platform", "linux/arm64", IMAGE)
        try:
            for rel, (src, _) in FILES.items():
                (dest / rel).parent.mkdir(parents=True, exist_ok=True)
                _docker(env, "cp", f"{cid}:{src}", str(dest / rel))
        finally:
            subprocess.run([*docker_cmd(env), "rm", cid], capture_output=True, text=True, env=dict(env))  # nosec B603: argument list built here, no shell, program is git, the interpreter or a configured binary  # nosemgrep: dangerous-subprocess-use-audit -- argument list built here from its own options and paths, no shell
        bad = verify(dest)
        if bad:
            raise SysrootError("sysroot differs from its pins: " + "; ".join(bad))
    return {"dest": str(dest), "image": IMAGE, "files": {rel: sha for rel, (_, sha) in FILES.items()}}


def describe(info: dict) -> str:
    files = " ".join(f"{Path(rel).name}={sha[:12]}" for rel, sha in info["files"].items())
    return f"image={info['image'].rsplit('@', 1)[1][:19]} {files}"


def main(argv: Sequence[str]) -> int:
    ap = argparse.ArgumentParser(prog="arm64_sysroot.py")
    sub = ap.add_subparsers(dest="cmd", required=True)
    e = sub.add_parser("ensure")
    e.add_argument("--dest", required=True)
    a = ap.parse_args(list(argv))
    env = dict(os.environ)
    try:
        s = ensure_sysroot(Path(a.dest), env)
        b = ensure_binfmt(env, Path(a.dest) / "lib" / "ld-linux-aarch64.so.1")
    except SysrootError as ex:
        print(f"ARM64-SYSROOT FAIL {ex}")
        return 1
    print(f"ARM64-SYSROOT PASS {b['how']} dest={s['dest']} {describe(s)} interpreter={b['interpreter']} flags={b['flags']}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
