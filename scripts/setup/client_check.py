#!/usr/bin/env python3
"""client_check.py: will this built client binary start on the machine it was built for?

usage: client_check.py <kind> <file>

  macho-arm64   a thin 64-bit arm64 Mach-O executable that carries a code signature whose SHA-256 page hashes match the file.
                Apple Silicon refuses to run an arm64 binary without a valid signature (the kernel kills the process at launch). The zig linker
                writes an ad-hoc, linker-signed one for aarch64-apple-darwin; a binary changed after linking, or a linker that stops signing,
                fails here instead of on a user's Mac. Nothing here proves the file runs; it proves the property a Mac checks first.
  elf-x86_64    a 64-bit little-endian ELF executable (ET_EXEC or ET_DYN) for x86-64 with no program interpreter: a static binary starts on
  elf-aarch64   any Linux of that architecture, a dynamic one only where its loader and libc exist. The same for AArch64.

Output, one line on stdout: `OK <kind> <facts>` (exit 0) or `FAIL <code>: <reason>` (exit 1). Codes: unreadable, too-short, bad-magic, wrong-cpu,
wrong-machine, wrong-filetype, bad-class, bad-elf, bad-macho, no-code-signature, bad-signature, hash-mismatch, dynamic-interpreter. Usage errors print
`usage: ...` on stderr and exit 2. Standard library only; reads the one file and writes nothing. Python 3.7 or later.
"""

from __future__ import annotations

import hashlib
import struct
import sys

KINDS = ("macho-arm64", "elf-x86_64", "elf-aarch64")

MH_MAGIC_64 = 0xFEEDFACF
CPU_TYPE_ARM64 = 0x0100000C
MH_EXECUTE = 2
LC_CODE_SIGNATURE = 0x1D
LC_BUILD_VERSION = 0x32
LC_VERSION_MIN_MACOSX = 0x24
CSMAGIC_EMBEDDED_SIGNATURE = 0xFADE0CC0
CSMAGIC_CODEDIRECTORY = 0xFADE0C02
CS_ADHOC = 0x2
CS_HASHTYPE_SHA256 = 2

EM_X86_64 = 0x3E
EM_AARCH64 = 0xB7
ET_EXEC, ET_DYN = 2, 3
PT_INTERP = 3
ELF_MACHINE = {"elf-x86_64": EM_X86_64, "elf-aarch64": EM_AARCH64}


class Fail(Exception):
    def __init__(self, code: str, reason: str) -> None:
        super().__init__(reason)
        self.code = code
        self.reason = reason


def _version(v: int) -> str:
    return "%d.%d.%d" % (v >> 16, (v >> 8) & 0xFF, v & 0xFF)


def check_macho_arm64(data: bytes) -> str:
    if len(data) < 4:
        raise Fail("too-short", "the file has %d bytes" % len(data))
    if struct.unpack("<I", data[:4])[0] != MH_MAGIC_64:
        raise Fail("bad-magic", "not a thin 64-bit little-endian Mach-O file (first bytes %s)" % data[:4].hex())
    if len(data) < 32:
        raise Fail("too-short", "the Mach-O header needs 32 bytes, the file has %d" % len(data))
    _, cputype, _, filetype, ncmds, sizeofcmds, _, _ = struct.unpack("<8I", data[:32])
    if cputype != CPU_TYPE_ARM64:
        raise Fail("wrong-cpu", "cputype 0x%x is not arm64 (0x%x)" % (cputype, CPU_TYPE_ARM64))
    if filetype != MH_EXECUTE:
        raise Fail("wrong-filetype", "filetype %d is not an executable (%d)" % (filetype, MH_EXECUTE))
    end = 32 + sizeofcmds
    if end > len(data):
        raise Fail("bad-macho", "the load commands run past the end of the file")
    off, sig, minos = 32, None, None
    for _ in range(ncmds):
        if off + 8 > end:
            raise Fail("bad-macho", "the load command list is shorter than ncmds")
        cmd, size = struct.unpack("<2I", data[off : off + 8])
        if size < 8 or off + size > end:
            raise Fail("bad-macho", "a load command of %d bytes does not fit" % size)
        if cmd == LC_CODE_SIGNATURE and size >= 16:
            sig = struct.unpack("<2I", data[off + 8 : off + 16])
        elif cmd == LC_BUILD_VERSION and size >= 20:
            minos = _version(struct.unpack("<I", data[off + 12 : off + 16])[0])
        elif cmd == LC_VERSION_MIN_MACOSX and size >= 16 and minos is None:
            minos = _version(struct.unpack("<I", data[off + 8 : off + 12])[0])
        off += size
    if sig is None:
        raise Fail("no-code-signature", "the binary has no LC_CODE_SIGNATURE; Apple Silicon refuses to run an unsigned arm64 binary")
    dataoff, datasize = sig
    if datasize < 12 or dataoff + datasize > len(data):
        raise Fail("bad-signature", "the signature (offset %d, %d bytes) is not inside the file (%d bytes)" % (dataoff, datasize, len(data)))
    blob = data[dataoff : dataoff + datasize]
    magic, _, count = struct.unpack(">3I", blob[:12])
    if magic != CSMAGIC_EMBEDDED_SIGNATURE:
        raise Fail("bad-signature", "the signature is not an embedded SuperBlob (magic 0x%x)" % magic)
    if 12 + 8 * count > len(blob):
        raise Fail("bad-signature", "the SuperBlob index runs past the signature")
    cd = None
    for i in range(count):
        kind, where = struct.unpack(">2I", blob[12 + 8 * i : 20 + 8 * i])
        if kind == 0 and where + 40 <= len(blob):
            cd = blob[where:]
            break
    if cd is None or struct.unpack(">I", cd[:4])[0] != CSMAGIC_CODEDIRECTORY:
        raise Fail("bad-signature", "the signature holds no CodeDirectory")
    _, cd_len, _, flags, hash_off, _, nspecial, ncode, code_limit, hash_size, hash_type, _, page_log2 = struct.unpack(">IIIIIIIIIBBBB", cd[:40])
    if hash_type != CS_HASHTYPE_SHA256 or hash_size != 32:
        raise Fail("bad-signature", "the CodeDirectory hashes with type %d, size %d, not SHA-256" % (hash_type, hash_size))
    if code_limit > len(data):
        raise Fail("bad-signature", "the CodeDirectory covers %d bytes of a %d-byte file" % (code_limit, len(data)))
    page = (1 << page_log2) if page_log2 else max(code_limit, 1)
    want_slots = -(-code_limit // page)
    if ncode != want_slots:
        raise Fail("bad-signature", "the CodeDirectory has %d page hashes for %d bytes in pages of %d (want %d)" % (ncode, code_limit, page, want_slots))
    if hash_off + (ncode + 0) * hash_size > min(cd_len, len(cd)) or hash_off < nspecial * hash_size:
        raise Fail("bad-signature", "the CodeDirectory's hash array does not fit")
    for i in range(ncode):
        chunk = data[i * page : min((i + 1) * page, code_limit)]
        stored = cd[hash_off + i * hash_size : hash_off + (i + 1) * hash_size]
        if hashlib.sha256(chunk).digest() != stored:
            raise Fail("hash-mismatch", "page %d of %d does not match its hash: the file changed after it was signed" % (i, ncode))
    return "signature=%s pages=%d minos=%s bytes=%d" % ("adhoc" if flags & CS_ADHOC else "signed", ncode, minos or "unknown", len(data))


def check_elf(data: bytes, kind: str) -> str:
    if len(data) < 4:
        raise Fail("too-short", "the file has %d bytes" % len(data))
    if data[:4] != b"\x7fELF":
        raise Fail("bad-magic", "not an ELF file (first bytes %s)" % data[:4].hex())
    if len(data) < 64:
        raise Fail("too-short", "the ELF header needs 64 bytes, the file has %d" % len(data))
    if data[4] != 2 or data[5] != 1:
        raise Fail("bad-class", "not a 64-bit little-endian ELF file (class %d, data %d)" % (data[4], data[5]))
    etype, machine = struct.unpack("<HH", data[16:20])
    if etype not in (ET_EXEC, ET_DYN):
        raise Fail("wrong-filetype", "e_type %d is neither an executable nor a position-independent executable" % etype)
    if machine != ELF_MACHINE[kind]:
        raise Fail("wrong-machine", "e_machine 0x%x is not 0x%x for %s" % (machine, ELF_MACHINE[kind], kind))
    phoff = struct.unpack("<Q", data[32:40])[0]
    phentsize, phnum = struct.unpack("<HH", data[54:58])
    if phnum and phentsize < 56:
        raise Fail("bad-elf", "program header entries of %d bytes are too small" % phentsize)
    if phoff + phentsize * phnum > len(data):
        raise Fail("too-short", "the program headers run past the end of the file")
    for i in range(phnum):
        at = phoff + i * phentsize
        if struct.unpack("<I", data[at : at + 4])[0] == PT_INTERP:
            offset, filesz = struct.unpack("<Q", data[at + 8 : at + 16])[0], struct.unpack("<Q", data[at + 32 : at + 40])[0]
            interp = data[offset : offset + min(filesz, 256)].split(b"\0")[0].decode("ascii", "replace")
            raise Fail("dynamic-interpreter", "the binary is dynamically linked (program interpreter %s); build the musl target for a static one" % interp)
    return "static bytes=%d" % len(data)


def check(kind: str, data: bytes) -> str:
    return check_macho_arm64(data) if kind == "macho-arm64" else check_elf(data, kind)


def main(argv: list) -> int:
    if len(argv) != 3 or argv[1] not in KINDS:
        sys.stderr.write("usage: client_check.py <%s> <file>\n" % "|".join(KINDS))
        return 2
    kind, path = argv[1], argv[2]
    try:
        with open(path, "rb") as f:
            data = f.read()
    except OSError as e:
        print("FAIL unreadable: %s" % e)
        return 1
    try:
        facts = check(kind, data)
    except Fail as e:
        print("FAIL %s: %s" % (e.code, e.reason))
        return 1
    print("OK %s %s" % (kind, facts))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
