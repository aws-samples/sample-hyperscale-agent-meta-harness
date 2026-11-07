//! Repository archive handling of the `research-job` Lambda: a GitHub `.tar.gz` goes in, a filtered,
//! deterministic `.tar.gz` and a per-file manifest come out.
//!
//! [`filter_archive`] streams the gunzipped tar one entry at a time (never holding the whole tree: only the files it keeps, each at most
//! 1 MiB, are in memory), keeps regular files that pass the rules in `filter.rs`, and records every other entry in a capped `dropped` list
//! with a reason from the fixed [`reason`] table. The commit id comes from the pax global header GitHub writes. Nothing is extracted or
//! written to a disk here: the output is a new tar built from the kept bytes, so a link, a device node or an escaping name has nowhere to
//! take effect.
//!
//! Failures: a cap exceeded (decompressed bytes, entries, kept files, kept bytes, output size) is `too_large`; a damaged or truncated
//! stream, a missing or malformed commit id, a commit that disagrees with a 40-hex top-directory suffix, an entry outside the top
//! directory, a path listed twice and every structure that cannot be skipped safely is `upstream_error`. A failure names the rule and
//! never an entry name or content. No partial result is returned.
//!
//! Memory: the compressed input (at most the download cap), the kept files (at most `repo_unpacked_bytes`) and the output archive (at
//! most `repo_compressed_bytes`) are in memory together, plus the one file being read (at most 1 MiB) and the dropped list (at most 200
//! entries). The licence candidates add nothing for the files that are kept (they are read from the kept map and never copied); a root
//! licence or declaration file that the rules drop is moved aside for detection, so the extra memory is at most `MAX_LICENCE_BYTES`
//! (4 MiB) in all, within 32 files, counted in archive order. Everything else streams.
//!
//! The gzip input must be one complete member: bytes after it are malformed. After the tar end blocks only zero bytes may follow (record
//! padding), read through the same size cap.
//!
//! The licence is read from the repository root (the licence files and the declared fields), also when `paths` leaves them out of the
//! output; it is evidence, not clearance (`crate::licence`).

mod filter;
mod tar;

use crate::licence::{self, LicenceCandidate, LicenceFile};
use crate::ports::Fail;
use flate2::bufread::GzDecoder;
use flate2::{Compression, GzBuilder};
use harness_protocol::common::is_lower_hex;
use harness_protocol::research::{
    DroppedEntry, LicenceEvidence, MAX_PATH_CHARS, ManifestFile, ResearchErrorCode, ResearchLimits,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::io::{self, Read, Write};
use tar::{Entry, Kind, TarError, TarReader, TarWriter};

/// Why an entry is not in the output. These words appear in the manifest's `dropped` list and are pinned by a test.
pub mod reason {
    pub const SYMLINK: &str = "symlink";
    pub const HARDLINK: &str = "hardlink";
    pub const DEVICE: &str = "device";
    pub const FIFO: &str = "fifo";
    pub const SPARSE: &str = "sparse";
    pub const SPECIAL_ENTRY: &str = "special-entry";
    pub const INVALID_UTF8: &str = "invalid-utf8";
    pub const NUL_IN_NAME: &str = "nul-in-name";
    pub const BACKSLASH: &str = "backslash";
    pub const UNSUPPORTED_CHARACTER: &str = "unsupported-character";
    pub const ABSOLUTE_PATH: &str = "absolute-path";
    pub const DOTDOT: &str = "dotdot";
    pub const DOT_COMPONENT: &str = "dot-component";
    pub const EMPTY_COMPONENT: &str = "empty-component";
    pub const NAME_TOO_LONG: &str = "name-too-long";
    pub const EXCLUDED_DIR: &str = "excluded-dir";
    pub const OUTSIDE_PATHS: &str = "outside-paths";
    pub const ENV_FILE: &str = "env-file";
    pub const PRIVATE_KEY_NAME: &str = "private-key-name";
    pub const TOO_LARGE_FILE: &str = "too-large-file";
    pub const BINARY: &str = "binary";
    pub const PRIVATE_KEY_CONTENT: &str = "private-key-content";
    pub const MINIFIED: &str = "minified";

    /// Every reason, in the order the rules apply.
    pub const ALL: [&str; 23] = [
        SYMLINK,
        HARDLINK,
        DEVICE,
        FIFO,
        SPARSE,
        SPECIAL_ENTRY,
        INVALID_UTF8,
        NUL_IN_NAME,
        BACKSLASH,
        UNSUPPORTED_CHARACTER,
        ABSOLUTE_PATH,
        DOTDOT,
        DOT_COMPONENT,
        EMPTY_COMPONENT,
        NAME_TOO_LONG,
        EXCLUDED_DIR,
        OUTSIDE_PATHS,
        ENV_FILE,
        PRIVATE_KEY_NAME,
        TOO_LARGE_FILE,
        BINARY,
        PRIVATE_KEY_CONTENT,
        MINIFIED,
    ];
}

/// At most this many entries are listed as dropped; the last one is a summary when there were more.
pub const MAX_DROPPED: usize = 200;
/// Licence candidates are collected from the root only, at most this many files...
const MAX_LICENCE_CANDIDATES: usize = 32;
/// ...and at most this many bytes in all (in archive order; a file that would pass the budget is not read for detection).
pub const MAX_LICENCE_BYTES: u64 = 4 * 1024 * 1024;
/// The gzip level of the output (fixed, so the bytes are reproducible).
const OUTPUT_LEVEL: u32 = 6;

/// The result of filtering one archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filtered {
    /// The 40-hex commit id of the pax global header.
    pub commit: String,
    /// The single top directory of the archive (`<repo>-<ref>`).
    pub top_dir: String,
    /// The filtered archive: regular files only, paths relative to the repository root, sorted, deterministic.
    pub tar_gz: Vec<u8>,
    /// Every kept file with its SHA-256 and size, sorted by path.
    pub files: Vec<ManifestFile>,
    /// Entries left out, at most [`MAX_DROPPED`], in archive order.
    pub dropped: Vec<DroppedEntry>,
    pub licence: LicenceEvidence,
    /// Decompressed bytes read from the archive, kept or not.
    pub read_bytes: u64,
    /// Bytes of the kept files.
    pub unpacked_bytes: u64,
}

// ---- the read cap -------------------------------------------------------------------------------------------------------------------------

#[derive(Debug)]
struct ReadCapExceeded;

impl fmt::Display for ReadCapExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("decompressed size cap")
    }
}

impl std::error::Error for ReadCapExceeded {}

/// Counts the bytes read and fails once more than `cap` were read: an archive that expands past the cap stops here, whatever the tar says.
struct LimitedRead<R> {
    inner: R,
    read: u64,
    cap: u64,
}

impl<R: Read> Read for LimitedRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read += n as u64;
        if self.read > self.cap {
            return Err(io::Error::other(ReadCapExceeded));
        }
        Ok(n)
    }
}

fn is_cap(e: &io::Error) -> bool {
    e.get_ref()
        .is_some_and(|inner| inner.is::<ReadCapExceeded>())
}

fn fail(e: TarError) -> Fail {
    match e {
        TarError::Malformed(rule) => Fail::new(
            ResearchErrorCode::UpstreamError,
            format!("the archive was rejected: {rule}"),
        ),
        TarError::TooLarge(rule) => Fail::new(
            ResearchErrorCode::TooLarge,
            format!("the archive is over a cap: {rule}"),
        ),
        TarError::Io(e) if is_cap(&e) => Fail::new(
            ResearchErrorCode::TooLarge,
            "the archive is over a cap: it expands past the decompressed size cap",
        ),
        TarError::Io(_) => Fail::new(
            ResearchErrorCode::UpstreamError,
            "the archive was rejected: the gzip stream is damaged",
        ),
    }
}

// ---- the dropped list ------------------------------------------------------------------------------------------------------------------

#[derive(Default)]
struct Drops {
    list: Vec<DroppedEntry>,
    total: usize,
}

impl Drops {
    fn push(&mut self, path: String, reason: &str) {
        self.total += 1;
        if self.list.len() < MAX_DROPPED {
            self.list.push(DroppedEntry {
                path: filter::listed(&path),
                reason: reason.to_string(),
            });
        }
    }

    /// At most [`MAX_DROPPED`] entries; when there were more, the last one says how many.
    fn finish(mut self) -> Vec<DroppedEntry> {
        if self.total > MAX_DROPPED {
            self.list.truncate(MAX_DROPPED - 1);
            let more = self.total - (MAX_DROPPED - 1);
            self.list.push(DroppedEntry {
                path: "*".to_string(),
                reason: format!("{more} more entries dropped"),
            });
        }
        self.list
    }
}

// ---- the filter ------------------------------------------------------------------------------------------------------------------------

struct State<'a> {
    paths: &'a [String],
    limits: &'a ResearchLimits,
    top: Option<String>,
    kept: BTreeMap<String, Vec<u8>>,
    kept_bytes: u64,
    drops: Drops,
    excluded_roots: HashSet<String>,
    names: Names,
    /// Root licence files that are in `kept` (read from there at the end, never copied).
    kept_candidates: Vec<String>,
    /// Root licence files the rules dropped (moved here, not copied).
    dropped_candidates: Vec<LicenceCandidate>,
    candidate_bytes: u64,
}

/// Every path the archive names, to catch a path listed twice and a file that shares its name with a directory (a file `docs` beside
/// `docs/guide.md`): extracting either would be ambiguous.
#[derive(Default)]
struct Names {
    /// Every entry by its own path (files, links, directory entries).
    entries: HashSet<String>,
    files: HashSet<String>,
    /// Directory entries and every proper ancestor of every entry.
    dirs: HashSet<String>,
}

impl Names {
    fn add(&mut self, path: &str, is_dir: bool) -> Result<(), TarError> {
        const CONFLICT: TarError = TarError::Malformed("a file and a directory share a name");
        if self.entries.contains(path) {
            return Err(TarError::Malformed("a path is listed twice"));
        }
        let mut from = 0;
        while let Some(i) = path[from..].find('/') {
            let ancestor = &path[..from + i];
            if self.files.contains(ancestor) {
                return Err(CONFLICT);
            }
            if !self.dirs.contains(ancestor) {
                self.dirs.insert(ancestor.to_string());
            }
            from += i + 1;
        }
        if is_dir {
            if self.files.contains(path) {
                return Err(CONFLICT);
            }
            self.dirs.insert(path.to_string());
        } else {
            if self.dirs.contains(path) {
                return Err(CONFLICT);
            }
            self.files.insert(path.to_string());
        }
        self.entries.insert(path.to_string());
        Ok(())
    }
}

impl State<'_> {
    fn too_large(what: &'static str) -> TarError {
        TarError::TooLarge(what)
    }

    fn step<R: Read>(&mut self, tar: &mut TarReader<R>, entry: Entry) -> Result<(), TarError> {
        let name = match filter::check_name(&entry.name) {
            Ok(n) => n,
            Err(why) => {
                self.drops.push(filter::shown(&entry.name), why);
                return tar.skip_data();
            }
        };
        let is_dir = entry.kind == Kind::Dir;
        let comps: Vec<&str> = name.split('/').collect();
        if self.top.is_none() {
            if comps.len() == 1 && !is_dir {
                return Err(TarError::Malformed(
                    "the first entry is not inside a directory",
                ));
            }
            self.top = Some(comps[0].to_string());
        }
        let top = self.top.as_deref().unwrap_or_default();
        if comps[0] != top {
            return Err(TarError::Malformed("an entry is outside the top directory"));
        }
        let rel = &comps[1..];
        if rel.is_empty() {
            return if is_dir {
                Ok(())
            } else {
                Err(TarError::Malformed(
                    "a file has the name of the top directory",
                ))
            };
        }

        let rel_path = rel.join("/");
        self.names.add(&rel_path, is_dir)?;

        // A directory that is never kept: one record per root, whatever is below it.
        let dirs = if is_dir { rel } else { &rel[..rel.len() - 1] };
        if let Some(i) = dirs.iter().position(|c| filter::EXCLUDED_DIRS.contains(c)) {
            let root = format!("{}/", rel[..=i].join("/"));
            if self.excluded_roots.insert(root.clone()) {
                self.drops.push(root, reason::EXCLUDED_DIR);
            }
            return tar.skip_data();
        }
        if is_dir {
            return Ok(());
        }

        // The item manifest holds a file path of at most 256 bytes: a longer one is left out here, listed, and the rest is kept.
        if rel_path.len() > MAX_PATH_CHARS {
            self.drops.push(rel_path, reason::NAME_TOO_LONG);
            return tar.skip_data();
        }

        let why = match entry.kind {
            Kind::Symlink => Some(reason::SYMLINK),
            Kind::Hardlink => Some(reason::HARDLINK),
            Kind::Char | Kind::Block => Some(reason::DEVICE),
            Kind::Fifo => Some(reason::FIFO),
            Kind::Sparse => Some(reason::SPARSE),
            Kind::Other => Some(reason::SPECIAL_ENTRY),
            Kind::File | Kind::Dir => None,
        };
        if let Some(why) = why {
            self.drops.push(rel_path, why);
            return tar.skip_data();
        }
        self.file(tar, &entry, rel_path)
    }

    fn file<R: Read>(
        &mut self,
        tar: &mut TarReader<R>,
        entry: &Entry,
        rel_path: String,
    ) -> Result<(), TarError> {
        let base = rel_path.rsplit('/').next().unwrap_or(&rel_path);
        let is_candidate = !rel_path.contains('/') && licence::is_candidate_name(base);
        let in_paths = filter::matches_paths(&rel_path, self.paths);
        let name_reason = if !in_paths && !is_candidate {
            Some(reason::OUTSIDE_PATHS)
        } else if filter::is_env_file(base) {
            Some(reason::ENV_FILE)
        } else if filter::is_key_name(base) {
            Some(reason::PRIVATE_KEY_NAME)
        } else if filter::is_minified_name(base) {
            Some(reason::MINIFIED)
        } else if entry.size > filter::MAX_FILE_BYTES {
            Some(reason::TOO_LARGE_FILE)
        } else {
            None
        };
        if let Some(why) = name_reason {
            self.drops.push(rel_path, why);
            return tar.skip_data();
        }

        let data = tar.read_file(entry)?;
        let why = if !in_paths {
            Some(reason::OUTSIDE_PATHS)
        } else if filter::looks_binary(&data) {
            Some(reason::BINARY)
        } else if filter::has_private_key_block(&data) {
            Some(reason::PRIVATE_KEY_CONTENT)
        } else if filter::has_long_line(&data) {
            Some(reason::MINIFIED)
        } else {
            None
        };
        // A root licence or declaration file is read for the licence evidence also when the rules drop it, within a total budget.
        let wanted = is_candidate
            && self.kept_candidates.len() + self.dropped_candidates.len() < MAX_LICENCE_CANDIDATES
            && self.candidate_bytes + data.len() as u64 <= MAX_LICENCE_BYTES;
        if wanted {
            self.candidate_bytes += data.len() as u64;
        }
        if let Some(why) = why {
            if wanted {
                self.dropped_candidates.push(LicenceCandidate {
                    path: rel_path.clone(),
                    bytes: data,
                });
            }
            self.drops.push(rel_path, why);
            return Ok(());
        }

        if self.kept.len() as u64 >= u64::from(self.limits.repo_files) {
            return Err(Self::too_large(
                "the archive has more kept files than allowed",
            ));
        }
        let bytes = data.len() as u64;
        if self.kept_bytes + bytes > self.limits.repo_unpacked_bytes {
            return Err(Self::too_large("the kept files are larger than allowed"));
        }
        self.kept_bytes += bytes;
        if wanted {
            self.kept_candidates.push(rel_path.clone());
        }
        self.kept.insert(rel_path, data);
        Ok(())
    }
}

/// A writer that fails once more than `cap` bytes were written.
struct CappedWriter {
    buf: Vec<u8>,
    cap: u64,
}

impl Write for CappedWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.buf.len() as u64 + data.len() as u64 > self.cap {
            return Err(io::Error::other(ReadCapExceeded));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn write_output(kept: &BTreeMap<String, Vec<u8>>, cap: u64) -> Result<Vec<u8>, Fail> {
    let too_big = || {
        Fail::new(
            ResearchErrorCode::TooLarge,
            "the archive is over a cap: the filtered archive is larger than allowed",
        )
    };
    let internal = |_: io::Error| Fail::internal("the filtered archive could not be written");
    let gz = GzBuilder::new().mtime(0).operating_system(255).write(
        CappedWriter {
            buf: Vec::new(),
            cap,
        },
        Compression::new(OUTPUT_LEVEL),
    );
    let mut tar = TarWriter::new(gz);
    for (path, data) in kept {
        if let Err(e) = tar.add_file(path, data) {
            return Err(if is_cap(&e) { too_big() } else { internal(e) });
        }
    }
    let gz = tar
        .finish()
        .map_err(|e| if is_cap(&e) { too_big() } else { internal(e) })?;
    let out = gz
        .finish()
        .map_err(|e| if is_cap(&e) { too_big() } else { internal(e) })?;
    Ok(out.buf)
}

/// Filters a GitHub archive (`gz` is the compressed `.tar.gz`) down to the files worth giving Cline as context.
///
/// `paths` keeps only entries below one of the prefixes (a prefix ending in `/` is a directory, any other names a file or a directory);
/// empty keeps everything. `limits` carries the caps (`repo_read_bytes`, `repo_read_entries`, `repo_files`, `repo_unpacked_bytes`,
/// `repo_compressed_bytes`).
pub fn filter_archive(
    gz: &[u8],
    paths: &[String],
    limits: &ResearchLimits,
) -> Result<Filtered, Fail> {
    // The decoder reads exactly one gzip member and leaves what follows it in `input`.
    let mut input: &[u8] = gz;
    let reader = LimitedRead {
        inner: GzDecoder::new(&mut input),
        read: 0,
        cap: limits.repo_read_bytes,
    };
    let mut tar = TarReader::new(reader, u64::from(limits.repo_read_entries));
    let mut st = State {
        paths,
        limits,
        top: None,
        kept: BTreeMap::new(),
        kept_bytes: 0,
        drops: Drops::default(),
        excluded_roots: HashSet::new(),
        names: Names::default(),
        kept_candidates: Vec::new(),
        dropped_candidates: Vec::new(),
        candidate_bytes: 0,
    };
    while let Some(entry) = tar.next_entry().map_err(fail)? {
        st.step(&mut tar, entry).map_err(fail)?;
    }
    let commit = match tar.global_comment().map(std::str::from_utf8) {
        Some(Ok(c)) if is_lower_hex(c, 40) => c.to_string(),
        _ => {
            return Err(fail(TarError::Malformed(
                "the archive carries no valid commit id",
            )));
        }
    };
    let reader = tar.finish().map_err(fail)?;
    let read_bytes = reader.read;
    drop(reader);
    if !input.is_empty() {
        return Err(fail(TarError::Malformed(
            "the gzip input has data after its first member",
        )));
    }

    let top_dir = st
        .top
        .ok_or_else(|| fail(TarError::Malformed("the archive has no entries")))?;
    if let Some((_, suffix)) = top_dir.rsplit_once('-')
        && is_lower_hex(suffix, 40)
        && suffix != commit
    {
        return Err(fail(TarError::Malformed(
            "the commit id disagrees with the top directory",
        )));
    }

    let tar_gz = write_output(&st.kept, limits.repo_compressed_bytes)?;
    let files = st
        .kept
        .iter()
        .map(|(path, data)| ManifestFile {
            path: path.clone(),
            sha256: sha256_hex(data),
            bytes: data.len() as u64,
        })
        .collect();
    let mut licence_files: Vec<LicenceFile<'_>> = st
        .dropped_candidates
        .iter()
        .map(|c| LicenceFile {
            path: c.path.as_str(),
            bytes: c.bytes.as_slice(),
        })
        .collect();
    for path in &st.kept_candidates {
        if let Some(bytes) = st.kept.get(path) {
            licence_files.push(LicenceFile {
                path: path.as_str(),
                bytes: bytes.as_slice(),
            });
        }
    }
    licence_files.sort_by(|a, b| a.path.cmp(b.path));
    Ok(Filtered {
        commit,
        top_dir,
        tar_gz,
        files,
        dropped: st.drops.finish(),
        licence: licence::detect_files(&licence_files),
        read_bytes,
        unpacked_bytes: st.kept_bytes,
    })
}
