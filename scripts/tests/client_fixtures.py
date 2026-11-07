"""Test support (not a test): small, structurally valid client binaries for the tests of `scripts/setup/build-client.sh` and
`scripts/setup/client_check.py`.

`macho_arm64` builds a thin 64-bit arm64 Mach-O executable with an ad-hoc code signature: a header, one `LC_BUILD_VERSION`, an
`LC_CODE_SIGNATURE` and, after the page-aligned code, a SuperBlob that holds one CodeDirectory whose SHA-256 page hashes cover every byte
before the signature. That is the layout the zig linker writes for `aarch64-apple-darwin` (measured on a real `ham`: 2810 pages, flags 0x20002).
`elf64` builds a 64-bit little-endian ELF executable header with program headers, with or without an interpreter (`PT_INTERP`).
The functions take flags that damage one property each, so a test can prove that the checker refuses it.
"""

from __future__ import annotations

import hashlib
import struct

MH_MAGIC_64 = 0xFEEDFACF
CPU_TYPE_ARM64 = 0x0100000C
LC_BUILD_VERSION = 0x32
LC_CODE_SIGNATURE = 0x1D
CSMAGIC_EMBEDDED_SIGNATURE = 0xFADE0CC0
CSMAGIC_CODEDIRECTORY = 0xFADE0C02
PAGE = 4096
IDENT = b"ham-fixture"
IDENT_OFFSET = 88  # the fixed part of a version 0x20400 CodeDirectory


def _code_directory(code: bytes, tamper: bool, hash_type: int = 2) -> bytes:
    hashes = [hashlib.sha256(code[i : i + PAGE]).digest() for i in range(0, len(code), PAGE)]
    if tamper and hashes:
        hashes[len(hashes) // 2] = b"\x00" * 32
    hash_off = IDENT_OFFSET + len(IDENT) + 1
    head = struct.pack(
        ">IIIIIIIIIBBBB",
        CSMAGIC_CODEDIRECTORY,
        hash_off + 32 * len(hashes),
        0x20400,
        0x20002,  # ad-hoc and linker-signed, what the zig linker writes
        hash_off,
        IDENT_OFFSET,
        0,  # special slots
        len(hashes),
        len(code),  # codeLimit
        32,  # hashSize
        hash_type,
        0,  # platform
        12,  # pageSize, log2
    )
    return head + b"\x00" * (IDENT_OFFSET - len(head)) + IDENT + b"\x00" + b"".join(hashes)


def macho_arm64(
    sign: bool = True,
    tamper_hash: bool = False,
    cputype: int = CPU_TYPE_ARM64,
    filetype: int = 2,
    pages: int = 3,
    minos: int = 13 << 16,
    truncate_signature: bool = False,
    hash_type: int = 2,
    payload: bytes = b"",
) -> bytes:
    code_len = PAGE * pages
    cd_len = IDENT_OFFSET + len(IDENT) + 1 + 32 * pages
    blob_len = 20 + cd_len
    cmds = struct.pack("<6I", LC_BUILD_VERSION, 24, 1, minos, minos, 0)  # platform 1 is macOS; no tools
    if sign:
        cmds += struct.pack("<4I", LC_CODE_SIGNATURE, 16, code_len, blob_len)
    header = struct.pack("<8I", MH_MAGIC_64, cputype, 0, filetype, 2 if sign else 1, len(cmds), 0x200085, 0)
    code = header + cmds + payload
    code += b"\x00" * (code_len - len(code))
    if not sign:
        return code
    cd = _code_directory(code, tamper_hash, hash_type)
    blob = struct.pack(">3I", CSMAGIC_EMBEDDED_SIGNATURE, blob_len, 1) + struct.pack(">2I", 0, 20) + cd
    if truncate_signature:
        blob = blob[: len(blob) // 2]
    return code + blob


EM_X86_64 = 0x3E
EM_AARCH64 = 0xB7
PT_LOAD = 1
PT_INTERP = 3


def elf64(
    machine: int,
    interp: bool = False,
    etype: int = 2,
    ident_class: int = 2,
    ident_data: int = 1,
    payload: bytes = b"ham-fixture",
) -> bytes:
    phnum = 2 if interp else 1
    ehsize, phentsize = 64, 56
    ident = b"\x7fELF" + bytes([ident_class, ident_data, 1, 0]) + b"\x00" * 8
    header = ident + struct.pack("<HHIQQQIHHHHHH", etype, machine, 1, 0x400000, ehsize, 0, 0, ehsize, phentsize, phnum, 0, 0, 0)
    phdrs = struct.pack("<IIQQQQQQ", PT_LOAD, 5, 0, 0x400000, 0x400000, 0, 0, 0x1000)
    interp_text = b"/lib/ld-musl-x86_64.so.1\x00"
    if interp:
        interp_off = ehsize + phentsize * phnum
        phdrs += struct.pack("<IIQQQQQQ", PT_INTERP, 4, interp_off, 0x400000 + interp_off, 0x400000 + interp_off, len(interp_text), len(interp_text), 1)
    body = (interp_text if interp else b"") + payload
    return header + phdrs + body
