//! A bounded gzip reader for the stored conversation.
//!
//! The chunks of a transcript are gzip files in a prefix the engine can write, so the reader must not trust their size, their
//! framing or their content. [`gunzip_bounded`] reads one gzip member (RFC 1952) and inflates it (RFC 1951) in the manner of
//! zlib's `puff.c`: no tables beyond the canonical code counts, every index checked, the output limit tested before every byte
//! or copy is written, and the CRC-32 and length of the trailer verified. A stream that is truncated, has a second member or
//! trailing bytes, or expands past the limit is an error; nothing here allocates more than `max_out` bytes of output.
//!
//! It exists in `ham-core` with no dependency because the worker image key hashes `Cargo.toml` and `Cargo.lock`: a new crate in
//! the Lambdas would move the key. The writer (agentd, a later issue) compresses with its own crate;
//! this is only the reader. [`gzip_stored`] and [`gzip_run_of`] build valid streams for fixtures and tools.

/// Why a stream was refused. Every variant means the whole chunk is unreadable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GzipError {
    /// The input ended inside the header, a block or the trailer.
    #[error("the stream is truncated")]
    Truncated,
    /// The first two bytes are not `1f 8b`.
    #[error("not a gzip stream")]
    BadMagic,
    /// The method is not 8 (deflate).
    #[error("unsupported compression method")]
    UnsupportedMethod,
    /// A reserved flag bit is set.
    #[error("reserved header flags are set")]
    ReservedFlags,
    /// FHCRC does not match the header.
    #[error("the header checksum does not match")]
    BadHeaderCrc,
    /// A block of type 3.
    #[error("invalid block type")]
    BadBlockType,
    /// A stored block whose length is not the complement of its check.
    #[error("stored block length does not match its complement")]
    BadStoredLength,
    /// An over-subscribed or incomplete Huffman code, a code that is not defined, or a bad table.
    #[error("invalid Huffman code")]
    BadCode,
    /// A match that reaches back before the first byte.
    #[error("distance reaches before the start of the output")]
    BadDistance,
    /// The output would pass the limit.
    #[error("the output is over the limit")]
    OutputLimit,
    /// The CRC-32 of the output does not match the trailer.
    #[error("the CRC-32 does not match")]
    BadCrc,
    /// The length of the output does not match the trailer.
    #[error("the length does not match")]
    BadSize,
    /// Bytes follow the member (a second member is refused too).
    #[error("bytes follow the gzip member")]
    TrailingData,
}

const FHCRC: u8 = 2;
const FEXTRA: u8 = 4;
const FNAME: u8 = 8;
const FCOMMENT: u8 = 16;
const FRESERVED: u8 = 0xE0;

const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
};

/// The CRC-32 of the gzip trailer (the IEEE polynomial, as zlib's `crc32`).
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = CRC_TABLE[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

/// The deflate bit stream: bits come least significant first.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u32,
    count: u32,
}

impl<'a> Bits<'a> {
    /// The next `need` (at most 16) bits as a number.
    fn take(&mut self, need: u32) -> Result<u32, GzipError> {
        let mut val = self.buf;
        while self.count < need {
            let byte = *self.data.get(self.pos).ok_or(GzipError::Truncated)?;
            self.pos += 1;
            val |= u32::from(byte) << self.count;
            self.count += 8;
        }
        self.buf = val >> need;
        self.count -= need;
        Ok(val & ((1u32 << need) - 1))
    }

    /// Drops the bits left in the current byte; the stream continues on a byte boundary.
    fn align(&mut self) {
        self.buf = 0;
        self.count = 0;
    }
}

const MAX_BITS: usize = 15;
const MAX_LCODES: usize = 286;
const MAX_DCODES: usize = 30;
const FIX_LCODES: usize = 288;

/// A canonical Huffman code as the number of codes of each length and the symbols in code order (as `puff.c`).
struct Huffman {
    count: [u16; MAX_BITS + 1],
    symbol: [u16; FIX_LCODES],
}

impl Huffman {
    /// Builds the code of `lengths`. `Ok(left)` is the number of unused codes (0: complete); an over-subscribed set is an error.
    fn new(lengths: &[u8]) -> Result<(Self, i32), GzipError> {
        let mut h = Self {
            count: [0; MAX_BITS + 1],
            symbol: [0; FIX_LCODES],
        };
        for &len in lengths {
            h.count[usize::from(len)] += 1;
        }
        if usize::from(h.count[0]) == lengths.len() {
            return Ok((h, 0));
        }
        let mut left = 1i32;
        for len in 1..=MAX_BITS {
            left <<= 1;
            left -= i32::from(h.count[len]);
            if left < 0 {
                return Err(GzipError::BadCode);
            }
        }
        let mut offs = [0u16; MAX_BITS + 1];
        for len in 1..MAX_BITS {
            offs[len + 1] = offs[len] + h.count[len];
        }
        for (symbol, &len) in lengths.iter().enumerate() {
            if len != 0 {
                let slot = &mut offs[usize::from(len)];
                h.symbol[usize::from(*slot)] = symbol as u16;
                *slot += 1;
            }
        }
        Ok((h, left))
    }

    /// The symbol whose code is next in the stream.
    fn decode(&self, bits: &mut Bits<'_>) -> Result<u16, GzipError> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..=MAX_BITS {
            code |= bits.take(1)? as i32;
            let count = i32::from(self.count[len]);
            if code - count < first {
                let at = usize::try_from(index + (code - first)).map_err(|_| GzipError::BadCode)?;
                return self.symbol.get(at).copied().ok_or(GzipError::BadCode);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err(GzipError::BadCode)
    }
}

const LENS: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEXT: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DISTS: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DEXT: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
const CODE_LENGTH_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

struct Inflater<'a> {
    bits: Bits<'a>,
    out: Vec<u8>,
    max_out: usize,
}

impl Inflater<'_> {
    fn stored(&mut self) -> Result<(), GzipError> {
        self.bits.align();
        let at = self.bits.pos;
        let head = self.bits.data.get(at..at + 4).ok_or(GzipError::Truncated)?;
        let len = usize::from(u16::from_le_bytes([head[0], head[1]]));
        let check = u16::from_le_bytes([head[2], head[3]]);
        if len as u16 != !check {
            return Err(GzipError::BadStoredLength);
        }
        let body = self
            .bits
            .data
            .get(at + 4..at + 4 + len)
            .ok_or(GzipError::Truncated)?;
        if self.out.len() + len > self.max_out {
            return Err(GzipError::OutputLimit);
        }
        self.out.extend_from_slice(body);
        self.bits.pos = at + 4 + len;
        Ok(())
    }

    /// One block of literals and matches under `lencode` and `distcode`.
    fn codes(&mut self, lencode: &Huffman, distcode: &Huffman) -> Result<(), GzipError> {
        loop {
            let symbol = usize::from(lencode.decode(&mut self.bits)?);
            if symbol < 256 {
                if self.out.len() >= self.max_out {
                    return Err(GzipError::OutputLimit);
                }
                self.out.push(symbol as u8);
            } else if symbol == 256 {
                return Ok(());
            } else {
                let symbol = symbol - 257;
                if symbol >= LENS.len() {
                    return Err(GzipError::BadCode);
                }
                let len =
                    usize::from(LENS[symbol]) + self.bits.take(u32::from(LEXT[symbol]))? as usize;
                let dsym = usize::from(distcode.decode(&mut self.bits)?);
                if dsym >= DISTS.len() {
                    return Err(GzipError::BadCode);
                }
                let dist =
                    usize::from(DISTS[dsym]) + self.bits.take(u32::from(DEXT[dsym]))? as usize;
                if dist > self.out.len() {
                    return Err(GzipError::BadDistance);
                }
                if self.out.len() + len > self.max_out {
                    return Err(GzipError::OutputLimit);
                }
                let start = self.out.len() - dist;
                for i in 0..len {
                    let byte = self.out[start + i];
                    self.out.push(byte);
                }
            }
        }
    }

    fn fixed(&mut self) -> Result<(), GzipError> {
        let mut lengths = [0u8; FIX_LCODES];
        for (symbol, len) in lengths.iter_mut().enumerate() {
            *len = match symbol {
                0..=143 => 8,
                144..=255 => 9,
                256..=279 => 7,
                _ => 8,
            };
        }
        let (lencode, _) = Huffman::new(&lengths)?;
        // Thirty distance symbols of five bits: an incomplete code that the format allows here.
        let (distcode, _) = Huffman::new(&[5u8; MAX_DCODES])?;
        self.codes(&lencode, &distcode)
    }

    fn dynamic(&mut self) -> Result<(), GzipError> {
        let nlen = self.bits.take(5)? as usize + 257;
        let ndist = self.bits.take(5)? as usize + 1;
        let ncode = self.bits.take(4)? as usize + 4;
        if nlen > MAX_LCODES || ndist > MAX_DCODES {
            return Err(GzipError::BadCode);
        }
        let mut lengths = [0u8; MAX_LCODES + MAX_DCODES];
        for &slot in CODE_LENGTH_ORDER.iter().take(ncode) {
            lengths[slot] = self.bits.take(3)? as u8;
        }
        // The code-length code must be complete.
        let (lencode, left) = Huffman::new(&lengths[..19])?;
        if left != 0 {
            return Err(GzipError::BadCode);
        }
        lengths = [0u8; MAX_LCODES + MAX_DCODES];
        let mut index = 0;
        while index < nlen + ndist {
            let symbol = lencode.decode(&mut self.bits)?;
            if symbol < 16 {
                lengths[index] = symbol as u8;
                index += 1;
            } else {
                let (prev, repeat) = match symbol {
                    16 => {
                        if index == 0 {
                            return Err(GzipError::BadCode);
                        }
                        (lengths[index - 1], 3 + self.bits.take(2)? as usize)
                    }
                    17 => (0, 3 + self.bits.take(3)? as usize),
                    _ => (0, 11 + self.bits.take(7)? as usize),
                };
                if index + repeat > nlen + ndist {
                    return Err(GzipError::BadCode);
                }
                for _ in 0..repeat {
                    lengths[index] = prev;
                    index += 1;
                }
            }
        }
        // The end-of-block code must exist.
        if lengths[256] == 0 {
            return Err(GzipError::BadCode);
        }
        // An incomplete code is allowed only as the single code of length one.
        let (lencode, left) = Huffman::new(&lengths[..nlen])?;
        if left != 0 && nlen != usize::from(lencode.count[0]) + usize::from(lencode.count[1]) {
            return Err(GzipError::BadCode);
        }
        let (distcode, left) = Huffman::new(&lengths[nlen..nlen + ndist])?;
        if left != 0 && ndist != usize::from(distcode.count[0]) + usize::from(distcode.count[1]) {
            return Err(GzipError::BadCode);
        }
        self.codes(&lencode, &distcode)
    }
}

/// Skips a zero-terminated header string.
fn skip_zero_terminated(data: &[u8], mut pos: usize) -> Result<usize, GzipError> {
    loop {
        let byte = *data.get(pos).ok_or(GzipError::Truncated)?;
        pos += 1;
        if byte == 0 {
            return Ok(pos);
        }
    }
}

/// Reads one gzip member from `input` and returns its bytes; the output may not pass `max_out` bytes.
///
/// A header with FEXTRA, FNAME, FCOMMENT or FHCRC is read; the trailer's CRC-32 and length are checked; a second member or any
/// byte after the first is [`GzipError::TrailingData`].
pub fn gunzip_bounded(input: &[u8], max_out: usize) -> Result<Vec<u8>, GzipError> {
    if input.len() < 10 {
        return Err(GzipError::Truncated);
    }
    if input[0] != 0x1f || input[1] != 0x8b {
        return Err(GzipError::BadMagic);
    }
    if input[2] != 8 {
        return Err(GzipError::UnsupportedMethod);
    }
    let flags = input[3];
    if flags & FRESERVED != 0 {
        return Err(GzipError::ReservedFlags);
    }
    let mut pos = 10;
    if flags & FEXTRA != 0 {
        let len = input.get(pos..pos + 2).ok_or(GzipError::Truncated)?;
        let len = usize::from(u16::from_le_bytes([len[0], len[1]]));
        pos += 2;
        if input.len() < pos + len {
            return Err(GzipError::Truncated);
        }
        pos += len;
    }
    if flags & FNAME != 0 {
        pos = skip_zero_terminated(input, pos)?;
    }
    if flags & FCOMMENT != 0 {
        pos = skip_zero_terminated(input, pos)?;
    }
    if flags & FHCRC != 0 {
        let want = input.get(pos..pos + 2).ok_or(GzipError::Truncated)?;
        let want = u16::from_le_bytes([want[0], want[1]]);
        if (crc32(&input[..pos]) & 0xFFFF) as u16 != want {
            return Err(GzipError::BadHeaderCrc);
        }
        pos += 2;
    }
    let body = input.get(pos..).ok_or(GzipError::Truncated)?;
    let mut inflater = Inflater {
        bits: Bits {
            data: body,
            pos: 0,
            buf: 0,
            count: 0,
        },
        out: Vec::with_capacity(max_out.min(body.len().saturating_mul(4)).min(1 << 20)),
        max_out,
    };
    loop {
        let last = inflater.bits.take(1)?;
        match inflater.bits.take(2)? {
            0 => inflater.stored()?,
            1 => inflater.fixed()?,
            2 => inflater.dynamic()?,
            _ => return Err(GzipError::BadBlockType),
        }
        if last == 1 {
            break;
        }
    }
    inflater.bits.align();
    let trailer_at = inflater.bits.pos;
    let trailer = body
        .get(trailer_at..trailer_at + 8)
        .ok_or(GzipError::Truncated)?;
    let crc = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
    let size = u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]);
    let out = inflater.out;
    if crc != crc32(&out) {
        return Err(GzipError::BadCrc);
    }
    if size != out.len() as u32 {
        return Err(GzipError::BadSize);
    }
    if body.len() != trailer_at + 8 {
        return Err(GzipError::TrailingData);
    }
    Ok(out)
}

fn gzip_header() -> Vec<u8> {
    vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff]
}

fn gzip_trailer(out: &mut Vec<u8>, data_crc: u32, len: usize) {
    out.extend_from_slice(&data_crc.to_le_bytes());
    out.extend_from_slice(&(len as u32).to_le_bytes());
}

/// The bytes a hex text spells (two digits per byte, upper or lower case; ASCII whitespace between pairs is ignored). The test fixtures
/// of the stored conversation are kept as hex text because the source snapshot of some build services refuses binary files.
pub fn unhex(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len() / 2);
    let mut high: Option<u8> = None;
    for c in text.chars() {
        if c.is_ascii_whitespace() {
            if high.is_some() {
                return Err("a pair of hex digits is split by whitespace".into());
            }
            continue;
        }
        let d = c
            .to_digit(16)
            .and_then(|d| u8::try_from(d).ok())
            .filter(|_| c.is_ascii())
            .ok_or_else(|| format!("{c:?} is not a hex digit"))?;
        match high.take() {
            None => high = Some(d),
            Some(h) => out.push((h << 4) | d),
        }
    }
    if high.is_some() {
        return Err("an odd number of hex digits".into());
    }
    Ok(out)
}

/// A valid gzip member that stores `data` without compression (blocks of at most 65,535 bytes). For fixtures and tools; the
/// writer compresses.
pub fn gzip_stored(data: &[u8]) -> Vec<u8> {
    let mut out = gzip_header();
    if data.is_empty() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    let mut blocks = data.chunks(65_535).peekable();
    while let Some(block) = blocks.next() {
        out.push(u8::from(blocks.peek().is_none()));
        let len = block.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(block);
    }
    gzip_trailer(&mut out, crc32(data), data.len());
    out
}

/// Packs bits least significant first.
struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl BitWriter {
    fn bits(&mut self, value: u32, count: u32) {
        self.acc |= value << self.n;
        self.n += count;
        while self.n >= 8 {
            self.out.push((self.acc & 0xFF) as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }

    /// A Huffman code: its bits go most significant first.
    fn code(&mut self, code: u32, len: u32) {
        let mut rev = 0;
        for i in 0..len {
            rev |= ((code >> i) & 1) << (len - 1 - i);
        }
        self.bits(rev, len);
    }

    fn literal(&mut self, byte: u8) {
        let b = u32::from(byte);
        if b < 144 {
            self.code(0x30 + b, 8);
        } else {
            self.code(0x190 + (b - 144), 9);
        }
    }

    /// A match of `len` bytes (3 to 258) at distance 1.
    fn run_match(&mut self, len: usize) {
        let symbol = LENS
            .iter()
            .rposition(|&base| usize::from(base) <= len)
            .unwrap_or(0);
        let code = 257 + symbol as u32;
        if code < 280 {
            self.code(code - 256, 7);
        } else {
            self.code(0xC0 + (code - 280), 8);
        }
        self.bits(
            (len - usize::from(LENS[symbol])) as u32,
            u32::from(LEXT[symbol]),
        );
        self.code(0, 5);
    }
}

/// A valid gzip member that expands to `len` copies of `byte`, in a fixed-Huffman block of one literal and matches of 258 bytes
/// at distance 1 (about 160 to 1). For tests of the output limit; it needs a few hundred kilobytes for 50 MiB.
pub fn gzip_run_of(byte: u8, len: usize) -> Vec<u8> {
    let mut w = BitWriter {
        out: gzip_header(),
        acc: 0,
        n: 0,
    };
    w.bits(1, 1);
    w.bits(1, 2);
    if len > 0 {
        w.literal(byte);
    }
    let mut left = len.saturating_sub(1);
    while left > 0 {
        if left < 3 {
            w.literal(byte);
            left -= 1;
            continue;
        }
        let mut take = left.min(258);
        // Leave no remainder of one or two, which a match cannot express.
        if left - take != 0 && left - take < 3 {
            take -= 3 - (left - take);
        }
        w.run_match(take);
        left -= take;
    }
    w.code(0, 7);
    if w.n > 0 {
        w.out.push((w.acc & 0xFF) as u8);
    }
    let data_crc = crc_of_run(byte, len);
    let mut out = w.out;
    gzip_trailer(&mut out, data_crc, len);
    out
}

/// The CRC-32 of `len` copies of `byte`, without building them.
fn crc_of_run(byte: u8, len: usize) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for _ in 0..len {
        c = CRC_TABLE[((c ^ u32::from(byte)) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}
