//! The header of a git bundle (v2 or v3), read from the first bytes of the file. Pure: no I/O and no git.
//!
//! A bundle starts with a signature line, then (v3) `@capability` lines, then the commits the bundle needs and does not hold
//! (`-<id> <subject>`, its prerequisites), then the refs it holds (`<id> <name>`), then one blank line; the pack follows.
//!
//! * [`bundle_head`] and [`bundle_head_of`]: the commit the header names for one ref (the copy of a run's work, a delivered head).
//! * [`needs`]: the prerequisites. A copy of a run's work may leave out what the run's base bundle holds; whoever reads such a
//!   copy takes the base first. An empty answer is a bundle that clones alone.
//! * [`heads`]: the commits a bundle names as its heads, for the bundle a run started from: they are what the writer of a
//!   copy leaves out.
//!
//! The bytes are a worker's or a user's: every id is checked, nothing is echoed, and a header that does not end inside the bytes read
//! is refused, never guessed at.

/// The ref agentd bundles.
pub const SNAPSHOT_REF: &str = "refs/heads/snapshot";
/// The bytes of the bundle read to find its header; a header that does not end inside them is refused.
pub const HEADER_BYTES: u64 = 8192;
/// The most bytes read to find the header of the bundle a run started from, which may name many refs.
pub const HEADER_BYTES_MAX: u64 = 65_536;

/// Why a bundle header cannot be trusted.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BundleError {
    #[error("not a git bundle (v2 or v3)")]
    NotABundle,
    #[error("the header does not end within the bytes read")]
    Truncated,
    #[error("the bundle has no {SNAPSHOT_REF}")]
    NoSnapshotRef,
    /// [`bundle_head_of`]: no line names the ref that was asked for.
    #[error("the bundle does not name the ref asked for")]
    MissingRef,
    #[error("the bundle names {SNAPSHOT_REF} more than once")]
    Ambiguous,
    /// [`bundle_head_of`]: more than one line names the ref that was asked for (named here, so the words are about the right ref).
    #[error("the bundle names {0} more than once")]
    AmbiguousRef(String),
    /// A header line that does not start with a full object id. It holds the length of the offending text only: the bytes are the
    /// worker's, and an error is logged.
    #[error("a header line does not start with a full object id")]
    BadObjectId(usize),
    /// [`heads`]: the header names a prerequisite, so what the bundle holds cannot be told from its heads.
    #[error("the bundle needs commits it does not hold")]
    NeedsOthers,
}

/// A full git object id: 40 (SHA-1) or 64 (SHA-256) lowercase hex digits. An abbreviation cannot be compared with a header's ids.
pub fn is_object_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The commit a bundle's header names for [`SNAPSHOT_REF`]. `prefix` is the first bytes of the bundle (at least up to the blank line that
/// ends the header; the pack may be cut anywhere). A v3 header's `@capability` lines and the `-<id> subject` prerequisite lines are
/// skipped; exactly one `<id> refs/heads/snapshot` line must remain.
pub fn bundle_head(prefix: &[u8]) -> Result<String, BundleError> {
    bundle_head_of(prefix, SNAPSHOT_REF).map_err(|e| match e {
        BundleError::MissingRef => BundleError::NoSnapshotRef,
        BundleError::AmbiguousRef(_) => BundleError::Ambiguous,
        other => other,
    })
}

/// The commit a bundle's header names for `wanted` (a full ref name such as `refs/heads/harness/<runId>`); everything else is as for
/// [`bundle_head`]: the prefix must hold the whole header, and exactly one line must name the ref.
pub fn bundle_head_of(prefix: &[u8], wanted: &str) -> Result<String, BundleError> {
    let mut lines: Vec<&[u8]> = prefix.split(|b| *b == b'\n').collect();
    // The last piece has no newline after it: the empty rest, or a line the prefix cut in two. Neither is read.
    lines.pop();
    let mut it = lines.into_iter();
    match it.next() {
        Some(b"# v2 git bundle") | Some(b"# v3 git bundle") => {}
        None if prefix.starts_with(b"# v") => return Err(BundleError::Truncated),
        _ => return Err(BundleError::NotABundle),
    }
    let mut found: Option<String> = None;
    let mut ended = false;
    for line in it {
        if line.is_empty() {
            ended = true;
            break;
        }
        if line[0] == b'@' || line[0] == b'-' {
            continue;
        }
        let text = String::from_utf8_lossy(line);
        let Some((oid, name)) = text.split_once(' ') else {
            return Err(BundleError::BadObjectId(text.len()));
        };
        if !is_object_id(oid) {
            return Err(BundleError::BadObjectId(oid.len()));
        }
        if name == wanted {
            if found.is_some() {
                return Err(BundleError::AmbiguousRef(wanted.to_string()));
            }
            found = Some(oid.to_string());
        }
    }
    if !ended {
        return Err(BundleError::Truncated);
    }
    found.ok_or(BundleError::MissingRef)
}

/// A header read strictly: every prerequisite and every head line, in the header's order.
struct Header {
    /// The ids of the `-<id> <subject>` lines.
    needs: Vec<String>,
    /// The `<id> <name>` lines.
    heads: Vec<(String, String)>,
}

/// Reads the whole header of `prefix`. Every line must be one of the kinds a header holds, every id a full object id, and the blank
/// line that ends the header must be inside the bytes: a line that was not read could be a prerequisite.
fn read(prefix: &[u8]) -> Result<Header, BundleError> {
    let mut lines: Vec<&[u8]> = prefix.split(|b| *b == b'\n').collect();
    // The last piece has no newline after it: the empty rest, or a line the prefix cut in two. Neither is read.
    lines.pop();
    let mut it = lines.into_iter();
    match it.next() {
        Some(b"# v2 git bundle") | Some(b"# v3 git bundle") => {}
        None if prefix.starts_with(b"# v") => return Err(BundleError::Truncated),
        _ => return Err(BundleError::NotABundle),
    }
    let mut header = Header {
        needs: Vec::new(),
        heads: Vec::new(),
    };
    for line in it {
        if line.is_empty() {
            return Ok(header);
        }
        if line[0] == b'@' {
            continue;
        }
        let text = String::from_utf8_lossy(line);
        if let Some(rest) = text.strip_prefix('-') {
            // The id ends at the first blank; the subject after it is the worker's text and is not read.
            let oid = rest.split_once(' ').map_or(rest, |(oid, _)| oid);
            if !is_object_id(oid) {
                return Err(BundleError::BadObjectId(oid.len()));
            }
            header.needs.push(oid.to_string());
            continue;
        }
        let Some((oid, name)) = text.split_once(' ') else {
            return Err(BundleError::BadObjectId(text.len()));
        };
        if !is_object_id(oid) {
            return Err(BundleError::BadObjectId(oid.len()));
        }
        header.heads.push((oid.to_string(), name.to_string()));
    }
    Err(BundleError::Truncated)
}

/// The ids of `ids` in their order, each once.
fn distinct(ids: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in ids {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// The commits the bundle needs and does not hold: the full ids of its header's `-<id> <subject>` lines, in the header's order,
/// each once. Empty for a bundle that clones alone. `prefix` is the first bytes of the bundle; [`BundleError::Truncated`] when the
/// blank line that ends the header is not inside them.
pub fn needs(prefix: &[u8]) -> Result<Vec<String>, BundleError> {
    read(prefix).map(|h| distinct(h.needs))
}

/// The commits the bundle names as its heads, each once: the commit of the line named `HEAD` first, then the others in the
/// header's order. For the bundle a run started from, whose commits a copy of the run's work leaves out. A header that names a
/// prerequisite is refused ([`BundleError::NeedsOthers`]): what such a bundle holds cannot be told from its heads.
/// [`BundleError::Truncated`] when the blank line that ends the header is not inside `prefix`.
pub fn heads(prefix: &[u8]) -> Result<Vec<String>, BundleError> {
    let header = read(prefix)?;
    if !header.needs.is_empty() {
        return Err(BundleError::NeedsOthers);
    }
    let first = header
        .heads
        .iter()
        .find(|(_, name)| name == "HEAD")
        .map(|(oid, _)| oid.clone());
    Ok(distinct(
        first
            .into_iter()
            .chain(header.heads.into_iter().map(|(oid, _)| oid)),
    ))
}
