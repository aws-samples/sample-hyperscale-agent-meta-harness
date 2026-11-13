//! A strict, streaming tar reader and the deterministic tar writer of the archive filter.
//!
//! The reader takes one entry at a time from any `Read` (the filter feeds it a size-limited gunzip stream), resolves the pax and GNU
//! name extensions, and hands the caller an [`Entry`]; the caller then reads its data (only when it will keep the file) or skips it. It
//! holds no entry in memory beyond what the caller asks for. Every failure is a static phrase: the reader never copies an entry name or
//! content into an error.
//!
//! Supported: ustar and GNU headers (octal fields; the checksum is verified), pax `g` and `x` records (`comment`, `path`; a `size`
//! record is refused because it would desynchronise the stream; `GNU.sparse.*` marks the entry sparse), GNU `L` long names (at most
//! 4,096 bytes), GNU `K` long links (skipped), old GNU sparse entries (`S`, with their extension blocks). A tar must end with a zero block.
//!
//! The writer produces ustar entries for regular files only: mode 0644, uid, gid and mtime 0, a pax `path` record when the name is longer
//! than 100 bytes, two zero blocks at the end. The same files in the same order give the same bytes.

use std::io::{self, Read, Write};

pub(crate) const BLOCK: usize = 512;
/// The largest pax record block (`x` or `g`).
pub(crate) const MAX_PAX_BYTES: u64 = 64 * 1024;
/// The largest GNU long name or long link.
pub(crate) const MAX_LONG_NAME: u64 = 4096;
const MAX_SPARSE_EXTENSION_BLOCKS: u32 = 4096;

#[derive(Debug)]
pub(crate) enum TarError {
    /// The archive breaks a structural rule.
    Malformed(&'static str),
    /// The archive is over a cap.
    TooLarge(&'static str),
    /// The stream under the tar failed (a damaged or truncated gzip, or the read cap).
    Io(io::Error),
}

type Res<T> = Result<T, TarError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    File,
    Dir,
    Symlink,
    Hardlink,
    Char,
    Block,
    Fifo,
    Sparse,
    /// A typeflag this reader does not know (read as an entry with data, never extracted).
    Other,
}

/// One entry, with its name resolved from the pax path, the GNU long name or the header.
#[derive(Debug)]
pub(crate) struct Entry {
    pub name: Vec<u8>,
    pub kind: Kind,
    /// The bytes of data that follow the header (0 for a directory, link, device or FIFO).
    pub size: u64,
}

pub(crate) struct TarReader<R: Read> {
    r: R,
    max_entries: u64,
    entries: u64,
    /// Data and padding of the current entry not yet consumed.
    pending: u64,
    global_comment: Option<Vec<u8>>,
}

fn to_err(e: io::Error) -> TarError {
    if e.kind() == io::ErrorKind::UnexpectedEof {
        // Either the tar or the gzip under it ended early: the caller sees one rule.
        TarError::Malformed("the archive ends early")
    } else {
        TarError::Io(e)
    }
}

fn octal(field: &[u8]) -> Option<u64> {
    let mut v: u64 = 0;
    let mut any = false;
    let mut done = false;
    for &b in field {
        match b {
            b'0'..=b'7' if !done => {
                v = v.checked_mul(8)?.checked_add(u64::from(b - b'0'))?;
                any = true;
            }
            b' ' | 0 => {
                if any {
                    done = true;
                }
            }
            _ => return None,
        }
    }
    any.then_some(v)
}

fn cstr(field: &[u8]) -> &[u8] {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    &field[..end]
}

fn padded(size: u64) -> u64 {
    size.div_ceil(BLOCK as u64) * BLOCK as u64
}

fn parse_pax(data: &[u8]) -> Res<Vec<(String, Vec<u8>)>> {
    const BAD: TarError = TarError::Malformed("a pax record is malformed");
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let rest = &data[pos..];
        let sp = rest.iter().position(|b| *b == b' ').ok_or(BAD)?;
        if sp == 0 || sp > 7 || !rest[..sp].iter().all(u8::is_ascii_digit) {
            return Err(BAD);
        }
        let len: usize = std::str::from_utf8(&rest[..sp])
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or(BAD)?;
        if len < sp + 4 || len > rest.len() || rest[len - 1] != b'\n' {
            return Err(BAD);
        }
        let body = &rest[sp + 1..len - 1];
        let eq = body.iter().position(|b| *b == b'=').ok_or(BAD)?;
        let key = std::str::from_utf8(&body[..eq]).map_err(|_| BAD)?;
        if key.is_empty() {
            return Err(BAD);
        }
        out.push((key.to_string(), body[eq + 1..].to_vec()));
        pos += len;
    }
    Ok(out)
}

impl<R: Read> TarReader<R> {
    pub(crate) fn new(r: R, max_entries: u64) -> Self {
        Self {
            r,
            max_entries,
            entries: 0,
            pending: 0,
            global_comment: None,
        }
    }

    /// The `comment` of the pax global header, when there was one.
    pub(crate) fn global_comment(&self) -> Option<&[u8]> {
        self.global_comment.as_deref()
    }

    /// Fills `buf`; `false` at a clean end of stream (nothing read), an error when the stream ends inside the buffer.
    fn read_block(&mut self, buf: &mut [u8]) -> Res<bool> {
        let mut got = 0;
        while got < buf.len() {
            match self.r.read(&mut buf[got..]) {
                Ok(0) if got == 0 => return Ok(false),
                Ok(0) => return Err(TarError::Malformed("the archive ends early")),
                Ok(n) => got += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(to_err(e)),
            }
        }
        Ok(true)
    }

    fn discard(&mut self, mut n: u64) -> Res<()> {
        let mut scratch = [0u8; 16 * 1024];
        while n > 0 {
            let want = usize::try_from(n.min(scratch.len() as u64)).unwrap_or(scratch.len());
            match self.r.read(&mut scratch[..want]) {
                Ok(0) => return Err(TarError::Malformed("the archive ends early")),
                Ok(got) => n -= got as u64,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(to_err(e)),
            }
        }
        Ok(())
    }

    /// Reads a meta entry's data (a pax block, a long name) and its padding.
    fn read_meta(&mut self, size: u64, cap: u64, what: &'static str) -> Res<Vec<u8>> {
        if size > cap {
            return Err(TarError::Malformed(what));
        }
        let mut data = vec![0u8; usize::try_from(size).unwrap_or(0)];
        let mut got = 0;
        while got < data.len() {
            match self.r.read(&mut data[got..]) {
                Ok(0) => return Err(TarError::Malformed("the archive ends early")),
                Ok(n) => got += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(to_err(e)),
            }
        }
        self.discard(padded(size) - size)?;
        Ok(data)
    }

    /// Skips the data of the entry last returned (a no-op when it was read or has none).
    pub(crate) fn skip_data(&mut self) -> Res<()> {
        let n = std::mem::take(&mut self.pending);
        self.discard(n)
    }

    /// Reads the data of the entry last returned. The caller has checked `entry.size`.
    pub(crate) fn read_file(&mut self, entry: &Entry) -> Res<Vec<u8>> {
        const MAX_READ: u64 = 64 * 1024 * 1024;
        if entry.size > MAX_READ || self.pending != padded(entry.size) {
            return Err(TarError::Malformed(
                "a file is read twice or is too large to read",
            ));
        }
        self.pending = 0;
        let mut data = vec![0u8; usize::try_from(entry.size).unwrap_or(0)];
        let mut got = 0;
        while got < data.len() {
            match self.r.read(&mut data[got..]) {
                Ok(0) => return Err(TarError::Malformed("the archive ends early")),
                Ok(n) => got += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(to_err(e)),
            }
        }
        self.discard(padded(entry.size) - entry.size)?;
        Ok(data)
    }

    /// The next entry, or `None` at the end marker (a zero block). Pax, GNU long-name and long-link entries are consumed here.
    pub(crate) fn next_entry(&mut self) -> Res<Option<Entry>> {
        self.skip_data()?;
        let mut pax_path: Option<Vec<u8>> = None;
        let mut long_name: Option<Vec<u8>> = None;
        let mut pax_sparse = false;
        // An x, L or K header was read and no entry has followed it yet, whatever it carried.
        let mut extension_pending = false;
        loop {
            let mut block = [0u8; BLOCK];
            if !self.read_block(&mut block)? {
                return Err(TarError::Malformed("the archive has no end marker"));
            }
            if block.iter().all(|b| *b == 0) {
                if extension_pending {
                    return Err(TarError::Malformed(
                        "an extended header has no entry after it",
                    ));
                }
                return Ok(None);
            }
            self.entries += 1;
            if self.entries > self.max_entries {
                return Err(TarError::TooLarge(
                    "the archive has more entries than allowed",
                ));
            }
            verify_checksum(&block)?;
            if block[124] & 0x80 != 0 {
                return Err(TarError::Malformed(
                    "a size field uses an unsupported encoding",
                ));
            }
            let size =
                octal(&block[124..136]).ok_or(TarError::Malformed("a size field is not octal"))?;
            let typeflag = block[156];
            match typeflag {
                b'g' => {
                    let data =
                        self.read_meta(size, MAX_PAX_BYTES, "a pax header is larger than 64 KiB")?;
                    for (k, v) in parse_pax(&data)? {
                        if k == "size" || k == "path" || k.starts_with("GNU.sparse.") {
                            return Err(TarError::Malformed(
                                "a global pax header carries a field that belongs to one entry",
                            ));
                        }
                        if k == "comment" {
                            self.global_comment = Some(v);
                        }
                    }
                    continue;
                }
                b'x' => {
                    let data =
                        self.read_meta(size, MAX_PAX_BYTES, "a pax header is larger than 64 KiB")?;
                    extension_pending = true;
                    for (k, v) in parse_pax(&data)? {
                        if k == "path" {
                            pax_path = Some(v);
                        } else if k == "size" {
                            return Err(TarError::Malformed("a pax size record is not supported"));
                        } else if k.starts_with("GNU.sparse.") {
                            pax_sparse = true;
                        }
                    }
                    continue;
                }
                b'L' => {
                    let data = self.read_meta(
                        size,
                        MAX_LONG_NAME,
                        "a GNU long name is longer than 4,096 bytes",
                    )?;
                    let end = data.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1);
                    extension_pending = true;
                    long_name = Some(data[..end].to_vec());
                    continue;
                }
                b'K' => {
                    self.read_meta(
                        size,
                        MAX_LONG_NAME,
                        "a GNU long link is longer than 4,096 bytes",
                    )?;
                    extension_pending = true;
                    continue;
                }
                _ => {}
            }
            let mut kind = match typeflag {
                0 | b'0' | b'7' => Kind::File,
                b'1' => Kind::Hardlink,
                b'2' => Kind::Symlink,
                b'3' => Kind::Char,
                b'4' => Kind::Block,
                b'5' => Kind::Dir,
                b'6' => Kind::Fifo,
                b'S' => Kind::Sparse,
                _ => Kind::Other,
            };
            let name = match pax_path.take().or(long_name.take()) {
                Some(n) => n,
                None => header_name(&block),
            };
            if kind == Kind::File && name.last() == Some(&b'/') {
                kind = Kind::Dir;
            }
            if kind == Kind::File && pax_sparse {
                kind = Kind::Sparse;
            }
            if typeflag == b'S' {
                // Old GNU sparse: extension blocks follow the header while the "extended" byte is set.
                let mut extended = block[482] != 0;
                let mut blocks = 0;
                while extended {
                    blocks += 1;
                    if blocks > MAX_SPARSE_EXTENSION_BLOCKS {
                        return Err(TarError::Malformed(
                            "a sparse entry has too many extension blocks",
                        ));
                    }
                    let mut ext = [0u8; BLOCK];
                    if !self.read_block(&mut ext)? {
                        return Err(TarError::Malformed("the archive ends early"));
                    }
                    extended = ext[504] != 0;
                }
            }
            let has_data = matches!(kind, Kind::File | Kind::Sparse | Kind::Other);
            if !has_data && size != 0 {
                // The payload would be read as the next header: the stream would lose step.
                return Err(TarError::Malformed(
                    "a directory, link, device or fifo entry declares data",
                ));
            }
            self.pending = padded(size);
            return Ok(Some(Entry { name, kind, size }));
        }
    }

    /// Reads what is left of the stream and gives the stream back. Only zero bytes may follow the end marker (the padding that fills a
    /// record), and reading to the end is what checks the gzip trailer.
    pub(crate) fn finish(mut self) -> Res<R> {
        let mut scratch = [0u8; 16 * 1024];
        loop {
            match self.r.read(&mut scratch) {
                Ok(0) => return Ok(self.r),
                Ok(n) => {
                    if scratch[..n].iter().any(|b| *b != 0) {
                        return Err(TarError::Malformed("data follows the end of the archive"));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(to_err(e)),
            }
        }
    }
}

fn verify_checksum(block: &[u8; BLOCK]) -> Res<()> {
    let stored =
        octal(&block[148..156]).ok_or(TarError::Malformed("a header checksum is not octal"))?;
    let sum: u64 = block
        .iter()
        .enumerate()
        .map(|(i, b)| {
            if (148..156).contains(&i) {
                u64::from(b' ')
            } else {
                u64::from(*b)
            }
        })
        .sum();
    if stored != sum {
        return Err(TarError::Malformed("a header checksum does not match"));
    }
    Ok(())
}

/// The name of a header: the `prefix` field joins the name only in POSIX ustar (`ustar\0` and version `00`); GNU headers use those bytes for
/// other fields.
fn header_name(block: &[u8; BLOCK]) -> Vec<u8> {
    let name = cstr(&block[0..100]);
    let posix = &block[257..263] == b"ustar\0" && &block[263..265] == b"00";
    let prefix = if posix { cstr(&block[345..500]) } else { &[] };
    if prefix.is_empty() {
        name.to_vec()
    } else {
        let mut full = prefix.to_vec();
        full.push(b'/');
        full.extend_from_slice(name);
        full
    }
}

// ---- the writer -----------------------------------------------------------------------------------------------------------------------

fn put_octal(field: &mut [u8], value: u64) {
    let width = field.len() - 1;
    let text = format!("{value:0width$o}");
    let bytes = text.as_bytes();
    let start = bytes.len().saturating_sub(width);
    field[..width].copy_from_slice(&bytes[start..]);
    field[width] = 0;
}

fn header(name: &[u8], typeflag: u8, size: u64) -> [u8; BLOCK] {
    let mut h = [0u8; BLOCK];
    let n = name.len().min(100);
    h[..n].copy_from_slice(&name[..n]);
    h[100..108].copy_from_slice(b"0000644\0");
    h[108..116].copy_from_slice(b"0000000\0");
    h[116..124].copy_from_slice(b"0000000\0");
    put_octal(&mut h[124..136], size);
    put_octal(&mut h[136..148], 0);
    h[156] = typeflag;
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[329..337].copy_from_slice(b"0000000\0");
    h[337..345].copy_from_slice(b"0000000\0");
    h[148..156].copy_from_slice(b"        ");
    let sum: u64 = h.iter().map(|b| u64::from(*b)).sum();
    let text = format!("{sum:06o}");
    h[148..154].copy_from_slice(text.as_bytes());
    h[154] = 0;
    h[155] = b' ';
    h
}

/// The longest prefix of `path` that fits a 100-byte name field and ends on a character boundary.
fn truncated_name(path: &str) -> &[u8] {
    let mut end = path.len().min(100);
    while !path.is_char_boundary(end) {
        end -= 1;
    }
    &path.as_bytes()[..end]
}

fn pax_path_record(path: &str) -> Vec<u8> {
    let body = format!(" path={path}\n");
    let mut len = body.len() + 1;
    while len.to_string().len() + body.len() != len {
        len = len.to_string().len() + body.len();
    }
    format!("{len}{body}").into_bytes()
}

pub(crate) struct TarWriter<W: Write> {
    w: W,
}

impl<W: Write> TarWriter<W> {
    pub(crate) fn new(w: W) -> Self {
        Self { w }
    }

    fn data(&mut self, data: &[u8]) -> io::Result<()> {
        self.w.write_all(data)?;
        let pad = usize::try_from(padded(data.len() as u64) - data.len() as u64).unwrap_or(0);
        self.w.write_all(&[0u8; BLOCK][..pad])
    }

    /// One regular file. `path` has no NUL (the filter has checked the name).
    pub(crate) fn add_file(&mut self, path: &str, data: &[u8]) -> io::Result<()> {
        if path.len() > 100 {
            let record = pax_path_record(path);
            self.w
                .write_all(&header(b"PaxHeader/path", b'x', record.len() as u64))?;
            self.data(&record)?;
        }
        self.w
            .write_all(&header(truncated_name(path), b'0', data.len() as u64))?;
        self.data(data)
    }

    pub(crate) fn finish(mut self) -> io::Result<W> {
        self.w.write_all(&[0u8; 2 * BLOCK])?;
        Ok(self.w)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn octal_fields_accept_padding_and_refuse_other_digits() {
        assert_eq!(octal(b"0000644\0"), Some(0o644));
        assert_eq!(octal(b"   644 \0"), Some(0o644));
        assert_eq!(octal(b"\0\0\0\0"), None);
        assert_eq!(octal(b"12 34"), None);
        assert_eq!(octal(b"0000089\0"), None);
        assert_eq!(octal(&[b'7'; 30]), None, "overflow");
    }

    #[test]
    fn a_pax_record_has_its_own_length() {
        let r = pax_path_record("a/b.txt");
        assert_eq!(r, b"16 path=a/b.txt\n");
        let long = pax_path_record(&"x".repeat(93));
        assert!(long.starts_with(b"103 path="));
        assert_eq!(long.len(), 103);
        assert_eq!(parse_pax(&r).unwrap()[0].0, "path");
        assert_eq!(parse_pax(&long).unwrap()[0].1.len(), 93);
    }
}
