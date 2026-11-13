"""The family checkers of load-kata's oracle tests (generated from fixtures/load-kata/families.py; the hidden tests embed the same functions)."""


def _raises(exc, fn, *args):
    try:
        fn(*args)
    except exc:
        return
    raise AssertionError(f"{getattr(fn, '__name__', fn)}{args!r} did not raise {exc.__name__}")


def check_ring(m, capacity, policy):
    assert m.CAPACITY == capacity and m.POLICY == policy
    r = m.Ring()
    assert len(r) == 0 and not r.is_full()
    _raises(IndexError, r.pop)
    _raises(IndexError, r.peek)
    for bad in (0, -1, True, "3"):
        _raises(ValueError, m.Ring, bad)
    _raises(ValueError, r.push, None)
    for i in range(capacity):
        assert r.push(i) is None
    assert len(r) == capacity and r.is_full()
    assert r.peek() == 0 and r.to_list() == list(range(capacity))
    copy = r.to_list()
    copy.append("x")
    assert r.to_list() == list(range(capacity))
    if policy == "overwrite":
        assert r.push(capacity) == 0
        assert r.to_list() == list(range(1, capacity + 1))
        assert r.extend([capacity + 1, capacity + 2]) == [1, 2]
        assert r.to_list() == list(range(3, capacity + 3))
        assert r.pop() == 3
    else:
        _raises(OverflowError, r.push, capacity)
        assert r.to_list() == list(range(capacity))
        assert r.pop() == 0
        _raises(OverflowError, r.extend, ["a", "b"])
        assert r.to_list() == list(range(1, capacity)) + ["a"]
        assert r.pop() == 1
    assert len(r) == capacity - 1 and not r.is_full()
    r.clear()
    assert len(r) == 0 and r.to_list() == []
    big = m.Ring(capacity + 2)
    assert big.extend(range(capacity + 2)) == [] and big.is_full()


def check_radix(m, base, width):
    assert m.BASE == base and m.WIDTH == width
    digits = "0123456789abcdefghijklmnopqrstuvwxyz"[:base]

    def plain(n):
        k, out = abs(n), ""
        while k:
            out = digits[k % base] + out
            k //= base
        return ("-" if n < 0 else "") + (out or "0")

    for n in (0, 1, base - 1, base, base * base + 1, 123456789, -1, -(base ** 3), 2 ** 40 + 7):
        text = plain(n)
        want = ("-" + text[1:].rjust(width, "0")) if n < 0 else text.rjust(width, "0")
        assert m.encode(n) == want, (n, m.encode(n), want)
        assert m.decode(want) == n and m.decode(want.upper()) == n
    assert m.encode(base - 1, 0) == digits[base - 1]
    assert m.encode(base, 8) == plain(base).rjust(8, "0")
    _raises(ValueError, m.encode, 1, -1)
    _raises(TypeError, m.encode, 1.5)
    _raises(TypeError, m.encode, True)
    _raises(TypeError, m.decode, 12)
    for bad in ("", "-", "+", " 1", "1 ", "1-", "?", "--1"):
        _raises(ValueError, m.decode, bad)
    if base < 36:
        _raises(ValueError, m.decode, "0123456789abcdefghijklmnopqrstuvwxyz"[base])
    assert m.decode("+" + plain(7)) == 7 and m.decode("-0") == 0 and m.decode("000") == 0
    assert m.add(plain(base), "-1") == plain(base - 1)
    assert m.add("-" + plain(3), "-" + plain(4)) == plain(-7)
    for n in (base * 2 + 1, 999, 0):
        assert m.digit_sum(plain(n)) == sum(digits.index(c) for c in plain(n))
        assert m.digit_sum("-" + plain(n)) == m.digit_sum(plain(n))
    _raises(ValueError, m.digit_sum, "")


def check_intervals(m, closed, lo_min):
    assert m.CLOSED is closed and m.LO_MIN == lo_min

    def cover(lo, hi):
        return set(range(lo, hi + 1 if closed else hi))

    def norm(points):
        spans = []
        for x in sorted(points):
            if spans and x == spans[-1][1] + 1:
                spans[-1][1] = x
            else:
                spans.append([x, x])
        return [(a, b) if closed else (a, b + 1) for a, b in spans]

    b = lo_min + 5
    s = m.IntervalSet()
    points = set()
    ops = [("add", b, b + 3), ("add", b + 10, b + 12), ("add", b + 4, b + 4), ("add", b + 2, b + 11),
           ("remove", b + 5, b + 7), ("add", lo_min, lo_min + 1), ("remove", b + 100, b + 200),
           ("add", b + 20, b + 20), ("remove", lo_min, b + 9)]
    for op, lo, hi in ops:
        getattr(s, op)(lo, hi)
        if op == "add":
            points |= cover(lo, hi)
        else:
            points -= cover(lo, hi)
        assert s.intervals() == norm(points), (op, lo, hi, s.intervals(), norm(points))
        assert s.total() == len(points) and len(s) == len(norm(points))
        for x in (lo_min, b, b + 4, b + 6, b + 11, b + 12, b + 20):
            assert s.contains(x) == (x in points), (op, x)
    _raises(ValueError, s.add, lo_min - 1, lo_min)
    _raises(ValueError, s.add, b + 3, b + 2)
    _raises(ValueError, s.remove, b + 3, b + 2)
    _raises(ValueError, s.remove, lo_min - 1, b)
    _raises(TypeError, s.add, 1.0, 2)
    _raises(TypeError, s.add, True, 2)
    empty = m.IntervalSet()
    assert empty.intervals() == [] and empty.total() == 0 and len(empty) == 0 and not empty.contains(b)
    t = m.IntervalSet([(b, b + 1), (b + 1, b + 3)])
    assert t.intervals() == norm(cover(b, b + 1) | cover(b + 1, b + 3)) and len(t) == 1


def check_rle(m, marker, min_run):
    assert m.MARKER == marker and m.MIN_RUN == min_run

    def enc(s):
        out, i = [], 0
        while i < len(s):
            j = i
            while j < len(s) and s[j] == s[i]:
                j += 1
            c, n = s[i], j - i
            out.append(f"{marker}{n}{marker}{c}" if c == marker or n >= min_run else c * n)
            i = j
        return "".join(out)

    samples = ["", "a", "ab", "aab", "aaab", "aaaab", "a" * 12 + "b", "3" * 11 + "z", marker,
               marker * 3 + "x", "xy" + marker + "yyyyy", "\n\n\t\t\t", "0" * 10]
    for s in samples:
        assert m.encode(s) == enc(s), (s, m.encode(s), enc(s))
        assert m.decode(m.encode(s)) == s
    assert m.runs("aabccc") == [("a", 2), ("b", 1), ("c", 3)] and m.runs("") == []
    assert m.runs(marker * 2 + "z") == [(marker, 2), ("z", 1)]
    assert m.decode(f"{marker}1{marker}q") == "q"
    assert m.decode(f"ab{marker}3{marker}{marker}") == "ab" + marker * 3
    for bad in (marker, f"{marker}2", f"{marker}2{marker}", f"{marker}x{marker}a", f"{marker}0{marker}a",
                f"{marker}02{marker}a", f"a{marker}"):
        _raises(ValueError, m.decode, bad)
    _raises(TypeError, m.encode, 5)
    _raises(TypeError, m.decode, None)
    assert m.ratio("") == 0.0
    for s in ("a" * 20, "abc", marker * 2):
        assert m.ratio(s) == round(len(enc(s)) / len(s), 3)


def check_calendar(m, months, week, offset):
    assert tuple(m.MONTHS) == tuple(months) and m.WEEK == week and m.OFFSET == offset
    year = sum(months)

    def ordn(y, mo, d):
        return (y - 1) * year + sum(months[: mo - 1]) + d - 1

    last = len(months)
    for date in ((1, 1, 1), (1, 1, months[0]), (1, 2, 1), (2, 1, 1), (7, last, months[-1]), (123, 3, 2)):
        assert m.is_valid(date)
        assert m.ordinal(date) == ordn(*date)
        assert m.from_ordinal(ordn(*date)) == date
        assert m.weekday(date) == (ordn(*date) + offset) % week
    assert m.add_days((1, 1, 1), year) == (2, 1, 1)
    assert m.add_days((2, 1, 1), -1) == (1, last, months[-1])
    assert m.add_days((1, 1, 1), months[0]) == (1, 2, 1)
    assert m.add_days((5, 2, 3), 0) == (5, 2, 3)
    assert m.days_between((1, 1, 1), (3, 1, 1)) == 2 * year
    assert m.days_between((3, 1, 1), (1, 1, 1)) == -2 * year
    for bad in ((0, 1, 1), (1, 0, 1), (1, last + 1, 1), (1, 1, 0), (1, 1, months[0] + 1)):
        assert not m.is_valid(bad)
        _raises(ValueError, m.ordinal, bad)
    for bad in ((1, 1), "1-1-1", (1, 1, 1.0), (True, 1, 1), [1, 1, 1], (1, 1, 1, 1)):
        _raises(TypeError, m.is_valid, bad)  # nosemgrep: is-function-without-parentheses -- the function is listed or passed as an object on purpose, not called there
    _raises(ValueError, m.from_ordinal, -1)
    _raises(ValueError, m.add_days, (1, 1, 1), -1)


def check_lru(m, capacity, ttl):
    assert m.CAPACITY == capacity and m.TTL == ttl
    _raises(ValueError, m.TtlLru, 0, ttl)
    _raises(ValueError, m.TtlLru, capacity, 0)
    c = m.TtlLru()
    entries, hits, misses, now, last = {}, 0, 0, 0, 0
    for step in range(4 * (capacity + ttl)):
        for k in [k for k, (_, exp) in entries.items() if exp <= now]:
            del entries[k]
        key = "k%d" % ((step * 7) % (capacity + 2))
        if step % 3 == 2:
            got = c.get(key, now)
            if key in entries:
                hits += 1
                entries[key] = entries.pop(key)
                assert got == entries[key][0], (step, key, got)
            else:
                misses += 1
                assert got is None, (step, key, got)
        else:
            entries.pop(key, None)
            entries[key] = (step, now + ttl)
            want = []
            while len(entries) > capacity:
                oldest = next(iter(entries))
                del entries[oldest]
                want.append(oldest)
            assert c.put(key, step, now) == want, (step, key)
        assert c.keys(now) == list(entries), (step, c.keys(now), list(entries))
        assert c.size(now) == len(entries)
        last = now
        now += step % 2
    assert c.stats() == (hits, misses)
    # The largest `now` seen is the last call's; one tick before it goes backwards.
    _raises(ValueError, c.get, "k0", last - 1)


def check_numerals(m, symbols, values):
    assert m.SYMBOLS == symbols and tuple(m.VALUES) == tuple(values)
    top = 4 * values[-1] - 1
    assert m.MAX == top

    def to(n):
        out = ""
        for sym, val in zip(reversed(symbols), reversed(values)):
            out += sym * (n // val)
            n %= val
        return out

    for n in sorted({1, 2, values[1] - 1, values[1], values[-1], values[-1] + values[1] + 1, top, top // 2}):
        assert m.to_numeral(n) == to(n), n
        assert m.from_numeral(to(n)) == n
    for n in range(1, min(top, 150) + 1):
        assert m.from_numeral(m.to_numeral(n)) == n
    for bad in (0, top + 1, -3):
        _raises(ValueError, m.to_numeral, bad)
    _raises(TypeError, m.to_numeral, 2.0)
    _raises(TypeError, m.to_numeral, True)
    _raises(TypeError, m.from_numeral, 5)
    lo, hi = symbols[0], symbols[-1]
    for bad in ("", lo + hi, lo * (values[1] // values[0]), hi * 4, "?", symbols[1] + lo + symbols[1]):
        _raises(ValueError, m.from_numeral, bad)
    assert m.compare(to(3), to(5)) == -1 and m.compare(to(9), to(9)) == 0 and m.compare(to(top), to(1)) == 1


def check_csvline(m, delimiter, quote):
    assert m.DELIMITER == delimiter and m.QUOTE == quote
    d, q = delimiter, quote

    def fmt(fields):
        out = []
        for f in fields:
            if d in f or q in f or "\n" in f or f.startswith(" ") or f.endswith(" "):
                out.append(q + f.replace(q, q + q) + q)
            else:
                out.append(f)
        return d.join(out)

    cases = [["a"], ["a", "b"], ["", ""], ["x" + d + "y", "z"], [q, q + q], [" lead", "trail ", "mid dle"],
             ["line\nbreak"], ["plain", "", "end"]]
    for f in cases:
        assert m.format(f) == fmt(f), (f, m.format(f), fmt(f))
        assert m.parse(m.format(f)) == f
    assert m.format(("t", "u")) == fmt(["t", "u"])
    assert m.parse("") == [""] and m.parse(d) == ["", ""]
    assert m.parse(f"a{q}b{d}c") == [f"a{q}b", "c"]
    assert m.parse(f"{q}x{d}y{q}{d}z") == [f"x{d}y", "z"]
    assert m.parse(f"{q}a{q}{q}b{q}") == [f"a{q}b"]
    for bad in (f"{q}abc", f"{q}a{q}b", f"x{d}{q}unterminated"):
        _raises(ValueError, m.parse, bad)
    _raises(TypeError, m.parse, 3)
    _raises(TypeError, m.format, ["a", 1])
    _raises(TypeError, m.format, "ab")
    _raises(ValueError, m.format, [])
    assert m.columns([]) == 0 and m.columns(["a", f"a{d}b{d}c", f"{q}x{d}y{q}"]) == 3
