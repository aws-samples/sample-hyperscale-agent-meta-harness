//! The exact-copy rule: licensed third-party text that the run stored through the research
//! route must not ride out in the zip.
//!
//! A stored file counts when it is at least [`COPY_FLOOR_BYTES`] long and its name is not exempt ([`is_exempt_path`]). A delivered file
//! equals a stored one when the bytes are the same ([`MatchKind::Exact`]) or when they are the same after line endings, trailing blanks,
//! the final newline and a byte order mark are made alike ([`MatchKind::Normalised`], [`normalise`]); the second kind needs the
//! normalised text to be at least [`COPY_NORMALISED_FLOOR_BYTES`] long, so a stored file padded with blanks cannot make a short ordinary
//! file look like a copy. Renaming, reformatting or rewriting defeats the rule; it catches the verbatim copy, and the later near-copy
//! report looks at the rest.
//!
//! The bundle validator (crates/delivery) runs the rule over the blobs a run adds and `ham-research check` (crates/agentd) runs it over the
//! work tree, so a file the run is told about before it finishes is a file the validator would block. This module is pure: no I/O.

use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// A stored file shorter than this never counts.
pub const COPY_FLOOR_BYTES: u64 = 1024;

/// A stored file whose [`normalise`]d form is shorter than this has no normalised hash: it matches the same bytes only. The raw floor
/// alone lets a stored file of 1 KiB of blanks and a line of text normalise to that one line, which any short file then equals.
pub const COPY_NORMALISED_FLOOR_BYTES: u64 = 512;

/// SHA-256 of `bytes`, lower-case hexadecimal.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `bytes` with the differences that carry no meaning made alike: leading byte order marks removed, every line stripped of trailing
/// blanks (space, tab and carriage return, so CRLF files and CR-CR-LF files read the same), trailing empty lines dropped, and one final
/// newline when anything is left. Nothing is left for a file of blanks only. The function works on bytes, so it never fails on text in
/// another encoding; applying it twice gives the same result as applying it once. It makes one pass and keeps no list of lines, so
/// its memory is the size of its output (never more than the input plus one byte).
pub fn normalise(bytes: &[u8]) -> Vec<u8> {
    let mut rest = bytes;
    while let Some(after) = rest.strip_prefix(b"\xEF\xBB\xBF") {
        rest = after;
    }
    let mut out = Vec::with_capacity(rest.len() + 1);
    // Empty lines are written only when a line with text follows them, so the empty lines at the end never are.
    let mut pending_empty: usize = 0;
    for line in rest.split(|b| *b == b'\n') {
        let mut end = line.len();
        while end > 0 && matches!(line[end - 1], b' ' | b'\t' | b'\r') {
            end -= 1;
        }
        if end == 0 {
            pending_empty += 1;
            continue;
        }
        out.extend(std::iter::repeat_n(b'\n', pending_empty));
        pending_empty = 0;
        out.extend_from_slice(&line[..end]);
        out.push(b'\n');
    }
    out
}

/// SHA-256 of the [`normalise`]d bytes of a stored file, or `None` when that form is shorter than [`COPY_NORMALISED_FLOOR_BYTES`]
/// (including a file of blanks only): such a file is matched by its exact bytes alone.
pub fn stored_normalised_sha256(bytes: &[u8]) -> Option<String> {
    let normalised = normalise(bytes);
    (normalised.len() as u64 >= COPY_NORMALISED_FLOOR_BYTES).then(|| sha256_hex(&normalised))
}

/// SHA-256 of [`normalise`]`(bytes)`, lower-case hexadecimal.
pub fn normalised_sha256_hex(bytes: &[u8]) -> String {
    sha256_hex(&normalise(bytes))
}

/// Names that never count, on either side of the comparison: licence texts (`LICENSE*`, `LICENCE*`, `COPYING*`,
/// `NOTICE*`, `UNLICENSE`), `.gitignore`, `.editorconfig`, lockfiles and minified files. The test is on the last path segment, in any
/// case. A licence-like stem counts only as a whole name or followed by `.` or `-` with no programming-language extension, so
/// `license_check.py` and `LICENSE-MIT.js` are ordinary code.
pub fn is_exempt_path(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    base == ".gitignore"
        || base == ".editorconfig"
        || LOCKFILES.contains(&base.as_str())
        || base.contains(".min.")
        || is_licence_name(&base)
}

const LOCKFILES: [&str; 19] = [
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "cargo.lock",
    "poetry.lock",
    "pipfile.lock",
    "uv.lock",
    "pdm.lock",
    "go.sum",
    "composer.lock",
    "gemfile.lock",
    "bun.lockb",
    "mix.lock",
    "pubspec.lock",
    "gradle.lockfile",
    "packages.lock.json",
    "podfile.lock",
    "flake.lock",
];

const LICENCE_STEMS: [&str; 5] = ["license", "licence", "copying", "notice", "unlicense"];

/// Extensions of programming languages: a licence-like name with one of them is code.
const CODE_EXTENSIONS: [&str; 24] = [
    "py", "js", "mjs", "cjs", "ts", "tsx", "jsx", "rs", "go", "java", "kt", "c", "h", "cc", "cpp",
    "hpp", "rb", "sh", "php", "cs", "swift", "scala", "lua", "pl",
];

fn is_licence_name(base: &str) -> bool {
    LICENCE_STEMS.iter().any(|stem| {
        let Some(rest) = base.strip_prefix(stem) else {
            return false;
        };
        if rest.is_empty() {
            return true;
        }
        if !rest.starts_with(['.', '-']) {
            return false;
        }
        let extension = base.rsplit_once('.').map(|(_, e)| e);
        !extension.is_some_and(|e| CODE_EXTENSIONS.contains(&e))
    })
}

/// One file of a stored research item, as its manifest lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredFile {
    /// The item that holds the file (`i-0002`).
    pub item_id: String,
    /// Where the item came from (a repository or page URL).
    pub origin: String,
    /// The licence the manifest records (an SPDX id, or `NOASSERTION`).
    pub licence: String,
    /// The file's path inside the item.
    pub path: String,
    /// Its size in bytes.
    pub bytes: u64,
    /// SHA-256 of its bytes.
    pub sha256: String,
    /// SHA-256 of [`normalise`]d bytes. `None` when the manifest does not carry it or the normalised form is under
    /// [`COPY_NORMALISED_FLOOR_BYTES`]: the file then matches exactly only.
    pub norm_sha256: Option<String>,
}

impl StoredFile {
    /// A stored file made from its bytes, with both hashes computed.
    pub fn from_bytes(
        item_id: &str,
        origin: &str,
        licence: &str,
        path: &str,
        bytes: &[u8],
    ) -> Self {
        Self {
            item_id: item_id.to_string(),
            origin: origin.to_string(),
            licence: licence.to_string(),
            path: path.to_string(),
            bytes: bytes.len() as u64,
            sha256: sha256_hex(bytes),
            norm_sha256: stored_normalised_sha256(bytes),
        }
    }
}

/// How a delivered file equals a stored one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchKind {
    /// The same bytes.
    Exact,
    /// The same after [`normalise`].
    Normalised,
}

/// A delivered file that equals a stored one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CopyMatch<'a> {
    pub stored: &'a StoredFile,
    pub kind: MatchKind,
}

/// The stored files that count, indexed by hash. Build it once per run, ask it about each delivered file.
#[derive(Clone, Debug, Default)]
pub struct CopyIndex {
    files: Vec<StoredFile>,
    by_sha: HashMap<String, Vec<usize>>,
    by_norm: HashMap<String, Vec<usize>>,
}

impl CopyIndex {
    /// Keeps the files of [`COPY_FLOOR_BYTES`] or more whose names are not exempt, in the given order (the first of several equal
    /// files is the one a match names).
    pub fn new(files: Vec<StoredFile>) -> Self {
        let mut index = Self::default();
        for f in files {
            if f.bytes < COPY_FLOOR_BYTES || is_exempt_path(&f.path) {
                continue;
            }
            let at = index.files.len();
            index.by_sha.entry(f.sha256.clone()).or_default().push(at);
            if let Some(n) = &f.norm_sha256 {
                index.by_norm.entry(n.clone()).or_default().push(at);
            }
            index.files.push(f);
        }
        index
    }

    /// How many stored files count.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The stored files that count.
    pub fn files(&self) -> &[StoredFile] {
        &self.files
    }

    /// The stored file a delivered file `path` with content `bytes` equals, if any. An exempt name or an empty file never matches.
    pub fn find(&self, path: &str, bytes: &[u8]) -> Option<CopyMatch<'_>> {
        if bytes.is_empty() || self.files.is_empty() || is_exempt_path(path) {
            return None;
        }
        if let Some(at) = self.by_sha.get(&sha256_hex(bytes)).and_then(|v| v.first()) {
            return Some(CopyMatch {
                stored: &self.files[*at],
                kind: MatchKind::Exact,
            });
        }
        let normalised = normalise(bytes);
        if (normalised.len() as u64) < COPY_NORMALISED_FLOOR_BYTES {
            return None;
        }
        self.by_norm
            .get(&sha256_hex(&normalised))
            .and_then(|v| v.first())
            .map(|at| CopyMatch {
                stored: &self.files[*at],
                kind: MatchKind::Normalised,
            })
    }
}
