#!/usr/bin/env python3
"""Writes a randomized corpus for the ignored test `tests/gzip_corpus.rs`: python3 gen_corpus.py OUT_DIR [COUNT] [SEED].

Each case is `<i>.gz` (a gzip member made by Python's zlib with a random level, strategy, memory level and window size over random,
text, repeated or mixed data of 0 to 300,000 bytes) and `<i>.sha256` (the hex SHA-256 and the length of what it must decode to).
Run the test with HAMH_GZIP_CORPUS=OUT_DIR cargo test -p ham-core --test gzip_corpus -- --ignored. Standard library only.
"""
import hashlib, os, random, struct, sys, zlib

out = sys.argv[1]
count = int(sys.argv[2]) if len(sys.argv) > 2 else 2000
rng = random.Random(int(sys.argv[3]) if len(sys.argv) > 3 else 20261016)  # nosec B311: seeded generator of deterministic test fixtures, not security-sensitive
os.makedirs(out, exist_ok=True)
WORDS = [bytes(rng.randrange(97, 123) for _ in range(rng.randint(2, 9))) for _ in range(300)]


def data(kind, n):
    if kind == 0:
        return bytes(rng.randrange(256) for _ in range(n))
    if kind == 1:
        buf = bytearray()
        while len(buf) < n:
            buf += rng.choice(WORDS) + b" "
        return bytes(buf[:n])
    if kind == 2:
        unit = bytes(rng.randrange(256) for _ in range(rng.randint(1, 40)))
        return (unit * (n // len(unit) + 1))[:n]
    if kind == 3:
        buf = bytearray()
        while len(buf) < n:
            buf += data(rng.randrange(3), rng.randint(1, 4000))
        return bytes(buf[:n])
    return bytes([rng.randrange(256)]) * n


for i in range(count):
    n = int(2 ** rng.uniform(0, 18.2)) if rng.random() > 0.05 else rng.randint(0, 3)
    n = min(n, 300000)
    plain = data(rng.randrange(5), n)
    level = rng.randint(0, 9)
    strategy = rng.choice([zlib.Z_DEFAULT_STRATEGY, zlib.Z_FILTERED, zlib.Z_HUFFMAN_ONLY, zlib.Z_RLE, zlib.Z_FIXED])
    c = zlib.compressobj(level, zlib.DEFLATED, -rng.randint(9, 15), rng.randint(1, 9), strategy)
    body = b""
    pos = 0
    while pos < len(plain):  # random flush points make extra block boundaries, some of them empty stored blocks
        step = rng.randint(1, 70000)
        body += c.compress(plain[pos:pos + step])
        pos += step
        if rng.random() < 0.15:
            body += c.flush(rng.choice([zlib.Z_SYNC_FLUSH, zlib.Z_FULL_FLUSH]))
    body += c.flush()
    gz = b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x00\xff" + body + struct.pack("<II", zlib.crc32(plain) & 0xFFFFFFFF, len(plain))
    open(os.path.join(out, "%d.gz" % i), "wb").write(gz)
    open(os.path.join(out, "%d.sha256" % i), "w").write("%s %d\n" % (hashlib.sha256(plain).hexdigest(), len(plain)))
print("wrote", count, "cases to", out)
