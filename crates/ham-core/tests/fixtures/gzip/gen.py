#!/usr/bin/env python3
"""Regenerates the gzip fixtures that tests/gzip.rs decodes with ham_core::gzip.

The streams come from Python's zlib, not from our code, so the decoder is checked against another implementation:
stored, fixed-Huffman and dynamic-Huffman blocks, the header fields FNAME, FEXTRA, FCOMMENT and FHCRC, and a 16 MiB bomb.
`index.json` lists each file with the SHA-256 and length of what it must decode to. Run: python3 gen.py
Standard library only; the output is deterministic (fixed seed, mtime 0).
"""
import gzip, hashlib, io, json, random, struct, zlib

rng = random.Random(7)  # nosec B311: seeded generator of deterministic test fixtures, not security-sensitive
WORDS = ("agent message chunk tool call edit file path src lib main test pass fail run build commit push "
         "transcript record sequence incarnation redact window search owner cursor").split()


def text(n):
    out, size = [], 0
    while size < n:
        line = " ".join(rng.choice(WORDS) for _ in range(rng.randint(4, 16))) + "\n"
        out.append(line)
        size += len(line)
    return "".join(out).encode()[:n]


def ndjson(n):
    out, size, i = [], 0, 0
    while size < n:
        rec = {"n": i, "t": "2026-10-16T10:00:%02d.%03dZ" % (i % 60, i % 1000), "dir": "in", "kind": "agent_message_chunk",
               "data": {"content": {"type": "text", "text": " ".join(rng.choice(WORDS) for _ in range(6))}, "messageId": "m%d" % (i // 5)}}
        line = json.dumps(rec, separators=(",", ":")) + "\n"
        out.append(line)
        size += len(line)
        i += 1
    return "".join(out).encode()[:n]


def raw(data, level=6, strategy=zlib.Z_DEFAULT_STRATEGY, mem=9):
    """A gzip member with mtime 0 built from zlib directly, so the strategy is ours to pick."""
    c = zlib.compressobj(level, zlib.DEFLATED, -15, mem, strategy)
    body = c.compress(data) + c.flush()
    header = b"\x1f\x8b\x08\x00" + b"\x00\x00\x00\x00" + b"\x00\xff"
    return header + body + struct.pack("<II", zlib.crc32(data) & 0xFFFFFFFF, len(data) & 0xFFFFFFFF)


def with_header(data, flags, extra=b"", name=b"", comment=b"", hcrc=False):
    head = b"\x1f\x8b\x08" + bytes([flags]) + b"\x00\x00\x00\x00\x00\xff"
    if flags & 4:
        head += struct.pack("<H", len(extra)) + extra
    if flags & 8:
        head += name + b"\x00"
    if flags & 16:
        head += comment + b"\x00"
    if flags & 2:
        head += struct.pack("<H", zlib.crc32(head) & 0xFFFF)
    c = zlib.compressobj(6, zlib.DEFLATED, -15)
    body = c.compress(data) + c.flush()
    return head + body + struct.pack("<II", zlib.crc32(data) & 0xFFFFFFFF, len(data))


files = {}
t40 = text(40000)
files["empty.gz"] = (b"", raw(b""))
files["one-byte.gz"] = (b"x", raw(b"x"))
for lvl in (0, 1, 6, 9):
    files["text-l%d.gz" % lvl] = (t40, raw(t40, lvl))
files["text-fixed.gz"] = (t40, raw(t40, 6, zlib.Z_FIXED))
files["text-huffman-only.gz"] = (t40, raw(t40, 6, zlib.Z_HUFFMAN_ONLY))
files["text-rle.gz"] = (t40, raw(t40, 6, zlib.Z_RLE))
nd = ndjson(30000)
files["ndjson-l6.gz"] = (nd, raw(nd, 6))
files["ndjson-l1.gz"] = (nd, raw(nd, 1))
rnd = bytes(rng.randrange(256) for _ in range(20000))
files["random-l6.gz"] = (rnd, raw(rnd, 6))
cyc = bytes(range(256)) * 80
files["all-bytes.gz"] = (cyc, raw(cyc, 9))
rep = b"abc" * 33333
files["repeat-abc.gz"] = (rep, raw(rep, 9))
run = b"\x00" * 100000
files["zeros-100k.gz"] = (run, raw(run, 9))
big = text(300000)
files["text-300k.gz"] = (big, raw(big, 6))
files["fname.gz"] = (t40[:5000], with_header(t40[:5000], 8, name=b"chunk.ndjson"))
files["fextra.gz"] = (t40[:5000], with_header(t40[:5000], 4, extra=b"AB\x04\x00abcd"))
files["fcomment.gz"] = (t40[:5000], with_header(t40[:5000], 16, comment=b"a comment"))
files["fhcrc.gz"] = (t40[:5000], with_header(t40[:5000], 2 | 8, name=b"x"))
files["all-flags.gz"] = (t40[:5000], with_header(t40[:5000], 2 | 4 | 8 | 16, extra=b"zz", name=b"n", comment=b"c"))
# The PyPI gzip module's own output (its header carries FNAME when a file name is given).
buf = io.BytesIO()
with gzip.GzipFile(filename="data.ndjson", mode="wb", fileobj=buf, mtime=0) as f:
    f.write(nd)
files["python-gzipfile.gz"] = (nd, buf.getvalue())
bomb = b"\x00" * (16 * 1024 * 1024)
files["bomb-16mib.gz"] = (bomb, raw(bomb, 9))

def hexdump(path, data):
    """The fleet's snapshot refuses binary files, so a fixture is kept as hex text (96 digits a line); tests read it with ham_core::gzip::unhex."""
    h = data.hex()
    open(path, "w").write("\n".join(h[i:i + 96] for i in range(0, len(h), 96)) + "\n")


index = {}
for name, (plain, packed) in sorted(files.items()):
    hexdump(name + ".hex", packed)
    index[name] = {"sha256": hashlib.sha256(plain).hexdigest(), "len": len(plain), "gz": len(packed)}
json.dump(index, open("index.json", "w"), indent=1, sort_keys=True)
print(len(files), "files,", sum(v["gz"] for v in index.values()), "bytes")
