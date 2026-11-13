//! Helpers of the JOB lane tests: a tar builder that can write every shape the archive filter must handle (GitHub-style archives and the
//! odd ones), a gzip helper, and an independent reader of the filter's output (it shares no code with `research_api::archive`).
//! Nothing here is committed as a binary: every archive is built at run time.

#![allow(dead_code, clippy::unwrap_used)]

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

/// The commit of the is-plain-obj fixture of the build plan.
pub const COMMIT: &str = "666df7c10035f7e26f27ec214fe5ae3173435f34";

pub fn top() -> String {
    format!("is-plain-obj-{COMMIT}")
}

pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn octal(field: &mut [u8], value: u64) {
    let width = field.len() - 1;
    let text = format!("{value:0width$o}");
    assert!(text.len() <= width, "octal field overflow");
    field[..width].copy_from_slice(text.as_bytes());
    field[width] = 0;
}

/// One 512-byte ustar header.
pub fn header(name: &[u8], typeflag: u8, size: u64, linkname: &[u8]) -> [u8; 512] {
    let mut h = [0u8; 512];
    let n = name.len().min(100);
    h[..n].copy_from_slice(&name[..n]);
    h[100..108].copy_from_slice(b"0000644\0");
    h[108..116].copy_from_slice(b"0000000\0");
    h[116..124].copy_from_slice(b"0000000\0");
    octal(&mut h[124..136], size);
    octal(&mut h[136..148], 0);
    h[156] = typeflag;
    let l = linkname.len().min(100);
    h[157..157 + l].copy_from_slice(&linkname[..l]);
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[329..337].copy_from_slice(b"0000000\0");
    h[337..345].copy_from_slice(b"0000000\0");
    seal(&mut h);
    h
}

/// Writes the checksum field: the sum of all bytes with the field read as spaces.
pub fn seal(h: &mut [u8; 512]) {
    h[148..156].copy_from_slice(b"        ");
    let sum: u64 = h.iter().map(|b| u64::from(*b)).sum();
    let text = format!("{sum:06o}");
    h[148..154].copy_from_slice(text.as_bytes());
    h[154] = 0;
    h[155] = b' ';
}

/// A growing tar archive.
#[derive(Default)]
pub struct Tar {
    pub buf: Vec<u8>,
}

impl Tar {
    pub fn new() -> Self {
        Self::default()
    }

    fn data(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        let pad = (512 - data.len() % 512) % 512;
        self.buf.extend(std::iter::repeat_n(0u8, pad));
    }

    /// `records` as a pax record block: `<len> key=value\n` with `len` counting the whole record.
    pub fn pax_block(records: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (key, value) in records {
            let body_len = 1 + key.len() + 1 + value.len() + 1; // space, key, '=', value, newline
            let mut len = body_len + 1;
            while len.to_string().len() + body_len != len {
                len = len.to_string().len() + body_len;
            }
            out.extend_from_slice(len.to_string().as_bytes());
            out.push(b' ');
            out.extend_from_slice(key.as_bytes());
            out.push(b'=');
            out.extend_from_slice(value);
            out.push(b'\n');
        }
        out
    }

    pub fn raw(
        &mut self,
        name: &[u8],
        typeflag: u8,
        size: u64,
        linkname: &[u8],
        data: &[u8],
    ) -> &mut Self {
        self.buf
            .extend_from_slice(&header(name, typeflag, size, linkname));
        self.data(data);
        self
    }

    pub fn file(&mut self, name: &str, data: &[u8]) -> &mut Self {
        self.raw(name.as_bytes(), b'0', data.len() as u64, b"", data)
    }

    pub fn file_bytes_name(&mut self, name: &[u8], data: &[u8]) -> &mut Self {
        self.raw(name, b'0', data.len() as u64, b"", data)
    }

    pub fn dir(&mut self, name: &str) -> &mut Self {
        let name = if name.ends_with('/') {
            name.to_string()
        } else {
            format!("{name}/")
        };
        self.raw(name.as_bytes(), b'5', 0, b"", b"")
    }

    pub fn symlink(&mut self, name: &str, target: &str) -> &mut Self {
        self.raw(name.as_bytes(), b'2', 0, target.as_bytes(), b"")
    }

    pub fn hardlink(&mut self, name: &str, target: &str) -> &mut Self {
        self.raw(name.as_bytes(), b'1', 0, target.as_bytes(), b"")
    }

    pub fn chardev(&mut self, name: &str) -> &mut Self {
        self.raw(name.as_bytes(), b'3', 0, b"", b"")
    }

    pub fn blockdev(&mut self, name: &str) -> &mut Self {
        self.raw(name.as_bytes(), b'4', 0, b"", b"")
    }

    pub fn fifo(&mut self, name: &str) -> &mut Self {
        self.raw(name.as_bytes(), b'6', 0, b"", b"")
    }

    /// The pax global header GitHub writes, with the commit id in `comment`.
    pub fn pax_global(&mut self, comment: &str) -> &mut Self {
        let block = Self::pax_block(&[("comment", comment.as_bytes())]);
        self.raw(b"pax_global_header", b'g', block.len() as u64, b"", &block)
    }

    pub fn pax_global_records(&mut self, records: &[(&str, &[u8])]) -> &mut Self {
        let block = Self::pax_block(records);
        self.raw(b"pax_global_header", b'g', block.len() as u64, b"", &block)
    }

    /// A pax extended header for the next entry.
    pub fn pax_next(&mut self, records: &[(&str, &[u8])]) -> &mut Self {
        let block = Self::pax_block(records);
        self.raw(b"PaxHeader/entry", b'x', block.len() as u64, b"", &block)
    }

    /// A GNU long-name entry for the next entry (`name` plus a NUL, as GNU tar writes it).
    pub fn gnu_long_name(&mut self, name: &[u8]) -> &mut Self {
        let mut data = name.to_vec();
        data.push(0);
        self.raw(b"././@LongLink", b'L', data.len() as u64, b"", &data)
    }

    pub fn gnu_long_name_exact(&mut self, data: &[u8]) -> &mut Self {
        self.raw(b"././@LongLink", b'L', data.len() as u64, b"", data)
    }

    pub fn gnu_long_link(&mut self, target: &[u8]) -> &mut Self {
        let mut data = target.to_vec();
        data.push(0);
        self.raw(b"././@LongLink", b'K', data.len() as u64, b"", &data)
    }

    /// The two zero blocks that end an archive.
    pub fn end(&mut self) -> &mut Self {
        self.buf.extend(std::iter::repeat_n(0u8, 1024));
        self
    }

    pub fn bytes(&self) -> Vec<u8> {
        self.buf.clone()
    }
}

/// A GitHub-style archive: the pax global header, the top directory, `build`'s entries below it, the end blocks.
pub fn github(top: &str, commit: &str, build: impl FnOnce(&mut Tar, &str)) -> Vec<u8> {
    let mut t = Tar::new();
    t.pax_global(commit);
    t.dir(top);
    build(&mut t, top);
    t.end();
    t.bytes()
}

/// `files` below the standard top directory and commit.
pub fn archive_of(files: &[(&str, &[u8])]) -> Vec<u8> {
    let top = top();
    gz(&github(&top, COMMIT, |t, top| {
        for (name, data) in files {
            t.file(&format!("{top}/{name}"), data);
        }
    }))
}

pub fn gz(bytes: &[u8]) -> Vec<u8> {
    let mut e = GzEncoder::new(Vec::new(), Compression::new(6));
    e.write_all(bytes).unwrap();
    e.finish().unwrap()
}

/// Text that is not binary, not minified and not a key: `len` bytes of 60-character lines.
pub fn text(len: usize) -> Vec<u8> {
    let line = b"the quick brown fox jumps over the lazy dog 0123456789 abcdef\n";
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let n = (len - out.len()).min(line.len());
        out.extend_from_slice(&line[..n]);
    }
    out.truncate(len);
    if len > 0 {
        out[len - 1] = b'\n';
    }
    out
}

/// Deterministic pseudo-random bytes that do not compress.
pub fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// The pieces of a PEM private-key marker, joined at run time so no key-shaped literal sits in the tree.
pub fn pem_begin() -> String {
    ["-----BEGIN ", "RSA PRIVATE", " KEY-----"].concat()
}

pub fn pem_block() -> Vec<u8> {
    let mut s = pem_begin();
    s.push('\n');
    s.push_str("bm90IGEgcmVhbCBrZXk=\n");
    s.push_str(&["-----END ", "RSA PRIVATE", " KEY-----\n"].concat());
    s.into_bytes()
}

// ---- an independent reader of the filter's output ---------------------------------------------------------------------------------

/// One file of an output archive.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct OutFile {
    pub path: String,
    pub data: Vec<u8>,
}

fn parse_octal(field: &[u8]) -> u64 {
    let s: String = field
        .iter()
        .take_while(|b| **b != 0 && **b != b' ')
        .map(|b| *b as char)
        .collect();
    u64::from_str_radix(s.trim(), 8).unwrap()
}

/// Reads an output archive strictly: gzip header mtime 0, ustar headers with valid checksums, mode 0644, uid, gid and mtime 0, regular files
/// only, pax `path` records for long names, sorted by path, two zero blocks at the end.
pub fn read_output(tar_gz: &[u8]) -> Vec<OutFile> {
    assert_eq!(&tar_gz[..2], &[0x1f, 0x8b], "gzip magic");
    assert_eq!(&tar_gz[4..8], &[0, 0, 0, 0], "gzip header mtime is 0");
    let mut tar = Vec::new();
    GzDecoder::new(tar_gz).read_to_end(&mut tar).unwrap();
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut pending_path: Option<String> = None;
    loop {
        let h = &tar[pos..pos + 512];
        pos += 512;
        if h.iter().all(|b| *b == 0) {
            assert!(
                tar[pos..pos + 512].iter().all(|b| *b == 0),
                "two zero blocks end the archive"
            );
            assert_eq!(pos + 512, tar.len(), "nothing follows the end blocks");
            break;
        }
        let stored = parse_octal(&h[148..156]);
        let mut copy = h.to_vec();
        copy[148..156].copy_from_slice(b"        ");
        let sum: u64 = copy.iter().map(|b| u64::from(*b)).sum();
        assert_eq!(stored, sum, "header checksum");
        assert_eq!(&h[257..263], b"ustar\0");
        assert_eq!(&h[263..265], b"00");
        assert_eq!(parse_octal(&h[108..116]), 0, "uid");
        assert_eq!(parse_octal(&h[116..124]), 0, "gid");
        assert_eq!(parse_octal(&h[136..148]), 0, "mtime");
        let size = parse_octal(&h[124..136]) as usize;
        let data = tar[pos..pos + size].to_vec();
        pos += size.div_ceil(512) * 512;
        match h[156] {
            b'x' => {
                let text = String::from_utf8(data).unwrap();
                let mut rest = text.as_str();
                while !rest.is_empty() {
                    let (len, tail) = rest.split_once(' ').unwrap();
                    let len: usize = len.parse().unwrap();
                    let record = &rest[..len];
                    assert!(record.ends_with('\n'));
                    let body = tail.split_once('\n').unwrap().0;
                    if let Some(p) = body.strip_prefix("path=") {
                        pending_path = Some(p.to_string());
                    }
                    rest = &rest[len..];
                }
            }
            b'0' => {
                assert_eq!(parse_octal(&h[100..108]), 0o644, "mode");
                let name = match pending_path.take() {
                    Some(p) => p,
                    None => {
                        let end = h[..100].iter().position(|b| *b == 0).unwrap_or(100);
                        String::from_utf8(h[..end].to_vec()).unwrap()
                    }
                };
                out.push(OutFile { path: name, data });
            }
            other => panic!("unexpected typeflag {other:?} in an output archive"),
        }
    }
    let mut sorted: Vec<&str> = out.iter().map(|f| f.path.as_str()).collect();
    let before = sorted.clone();
    sorted.sort();
    assert_eq!(before, sorted, "entries are sorted by path");
    out
}

pub fn out_map(out: &[OutFile]) -> Vec<(String, Vec<u8>)> {
    out.iter()
        .map(|f| (f.path.clone(), f.data.clone()))
        .collect()
}

/// Letters and digits in 60-character lines: text that compresses to about three quarters of its size.
pub fn alnum_noise(len: usize, seed: u64) -> Vec<u8> {
    const SET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut out: Vec<u8> = noise(len, seed)
        .iter()
        .map(|b| SET[usize::from(*b) % SET.len()])
        .collect();
    for i in (59..len).step_by(60) {
        out[i] = b'\n';
    }
    out
}

pub const MIT_BODY: &str = "MIT License\n\nCopyright (c) 2019 Example Author\n\nPermission is hereby granted, free of charge, to any person obtaining a copy\nof this software and associated documentation files (the \"Software\"), to deal\nin the Software without restriction, including without limitation the rights\nto use, copy, modify, merge, publish, distribute, sublicense, and/or sell\ncopies of the Software, and to permit persons to whom the Software is\nfurnished to do so, subject to the following conditions:\n\nThe above copyright notice and this permission notice shall be included in all\ncopies or substantial portions of the Software.\n\nTHE SOFTWARE IS PROVIDED \"AS IS\", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR\nIMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,\nFITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE\nAUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER\nLIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,\nOUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE\nSOFTWARE.\n";

/// The title block of the Apache-2.0 text (enough for the detector).
pub const APACHE_TITLE: &str = "                                 Apache License\n                           Version 2.0, January 2004\n                        http://www.apache.org/licenses/\n\n   TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION\n";

/// The title line of the Boost licence.
pub const BSL_TITLE: &str = "Boost Software License - Version 1.0 - August 17th, 2003\n\nPermission is hereby granted, free of charge, to any person or organization obtaining a copy of the software.\n";

/// `head` followed by plain text lines up to `len` bytes in all.
pub fn padded_text(head: &str, len: usize) -> Vec<u8> {
    let mut v = head.as_bytes().to_vec();
    assert!(v.len() <= len);
    let rest = text(len - v.len());
    v.extend_from_slice(&rest);
    v
}
