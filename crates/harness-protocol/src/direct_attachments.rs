//! The attachments of a direct run.
//!
//! An attachment is a file of the user's own that Cline may read as context: an API description, a design note, a table of data. `ham` reads the
//! files named with `--attach <file>` (UTF-8 text only), sends them as the `attachments` member of `StartDirectRunRequest`, and agentd writes each
//! one read-only into `<agent data>/context/attachments/`, outside the work tree, so nothing in it reaches a snapshot, the zip or a pull request.
//! The request is stored as `runs/<run>/input/request.json`, so the invocation body does not change. The text of the user's prompt is never
//! changed: the engine learns that the files exist from the context directory, the environment variable `HAMH_ATTACHMENTS_DIR` and the rules.
//!
//! An attachment has no size of its own and the set has no count: the request as a whole is held to what the platform takes,
//! and `ham` refuses a file that alone is more than that before it reads it.
//!
//! One validator serves all three ends (`ham`, control-api, agentd): [`check_attachments`] is pure and returns the first problem as an
//! [`AttachmentError`] whose message names the item and the rule, never the content. Checks that need a disk (a link, a file that is not
//! regular, a file that is not UTF-8) belong to the code that reads the disk.
//!
//! The exact-copy delivery rule (`delivery::third_party`) never reads attachments: they are the user's own files and not material the research
//! route stored, so a delivered file that equals an attachment is not a third-party copy.
//!
//! Everything here is re-exported from [`crate::direct`].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The longest attachment name, in bytes: the longest name a file system takes (255 bytes on ext4 and APFS).
pub const MAX_ATTACHMENT_NAME_BYTES: usize = 255;
/// The names agentd writes beside the attachments (the index); an attachment cannot have one of them, whatever the letter case.
pub const RESERVED_NAMES: [&str; 2] = ["INDEX.md", "INDEX.json"];

/// One attachment of the `attachments` member of `StartDirectRunRequest` (protocol 1.38.0). `name` is a plain file name: a letter or a digit,
/// then letters, digits, `.`, `_` or `-`, at most [`MAX_ATTACHMENT_NAME_BYTES`] bytes, with no directory part; `INDEX.md` and `INDEX.json` are reserved, whatever the letter case. `content` is UTF-8 text with
/// no NUL byte and at least one byte; `size` is its length in bytes and `sha256` is the lowercase hex SHA-256 of those bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Attachment {
    pub name: String,
    pub sha256: String,
    pub size: u64,
    pub content: String,
}

impl Attachment {
    /// An attachment with its `size` and `sha256` computed from `content`.
    pub fn new(name: &str, content: &str) -> Attachment {
        Attachment {
            name: name.to_string(),
            sha256: sha256_hex(content.as_bytes()),
            size: content.len() as u64,
            content: content.to_string(),
        }
    }
}

/// The rule an attachment breaks. The words are the same in `ham`, control-api and agentd.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachmentProblem {
    /// The name is not a plain file name (see [`valid_attachment_name`]).
    BadName,
    /// The name is one agentd writes beside the files (`INDEX.md`, `INDEX.json`; compared without case).
    ReservedName,
    /// Two attachments have one name (compared without case: the Mac folds case, the VM does not).
    DuplicateName,
    /// The attachment holds no byte.
    Empty,
    /// Read from a disk: the file is longer than a whole request can be (`max` is [`crate::direct::MAX_REQUEST_BYTES`]). [`check_attachments`] never
    /// returns it: the size of the request as a whole is judged by [`crate::direct::check_request`].
    TooLarge { bytes: usize, max: usize },
    /// Read from disks: the files named together are longer than a whole request can be. Not returned by [`check_attachments`] either.
    TooLargeTotal { bytes: usize, max: usize },
    /// `size` is not the length of `content`.
    SizeMismatch,
    /// `sha256` is not the lowercase SHA-256 of `content`.
    ShaMismatch,
    /// `content` holds a NUL byte.
    Nul,
    /// Read from a disk: the path is a link, a directory or another kind of entry.
    NotRegular,
    /// Read from a disk: the file is not UTF-8 text.
    NotText,
}

impl AttachmentProblem {
    /// A stable word for the problem (snake case), the same in every crate.
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadName => "bad_name",
            Self::ReservedName => "reserved_name",
            Self::DuplicateName => "duplicate_name",
            Self::Empty => "empty",
            Self::TooLarge { .. } => "too_large",
            Self::TooLargeTotal { .. } => "too_large_total",
            Self::SizeMismatch => "size_mismatch",
            Self::ShaMismatch => "sha_mismatch",
            Self::Nul => "nul",
            Self::NotRegular => "not_regular",
            Self::NotText => "not_text",
        }
    }
}

impl std::fmt::Display for AttachmentProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadName => write!(
                f,
                "has a name that is not allowed: a plain file name of at most {MAX_ATTACHMENT_NAME_BYTES} bytes that starts with a letter or a digit and holds only letters, digits, '.', '_' and '-'"
            ),
            Self::ReservedName => write!(
                f,
                "has a name that agentd uses for its index ({})",
                RESERVED_NAMES.join(" and ")
            ),
            Self::DuplicateName => write!(
                f,
                "has the name of another attachment (names are compared without case)"
            ),
            Self::Empty => write!(f, "is empty"),
            Self::TooLarge { bytes, .. } => write!(
                f,
                "holds {bytes} bytes; a request carries at most {}, so this file can never be sent",
                crate::direct_request::bound_words()
            ),
            Self::TooLargeTotal { bytes, .. } => write!(
                f,
                "the attachments together hold {bytes} bytes; a request carries at most {}",
                crate::direct_request::bound_words()
            ),
            Self::SizeMismatch => write!(f, "has a size that is not its content's length"),
            Self::ShaMismatch => write!(f, "has a sha256 that is not its content's"),
            Self::Nul => write!(f, "holds a NUL byte"),
            Self::NotRegular => write!(
                f,
                "is not a regular file (a link, a directory or another kind of entry)"
            ),
            Self::NotText => write!(f, "is not UTF-8 text; attach text files only"),
        }
    }
}

/// The first rule a set of attachments breaks, with the attachment it names (never its content; the name is cut and made printable).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentError {
    pub name: Option<String>,
    pub problem: AttachmentProblem,
}

impl std::fmt::Display for AttachmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.name {
            Some(n) => write!(f, "attachment {n:?} {}", self.problem),
            None => write!(f, "attachments: {}", self.problem),
        }
    }
}

impl std::error::Error for AttachmentError {}

impl AttachmentError {
    /// An error naming an attachment; the name is cut to 40 characters and every character that is not printable ASCII becomes `?`.
    pub fn new(name: Option<&str>, problem: AttachmentProblem) -> AttachmentError {
        AttachmentError {
            name: name.map(printable),
            problem,
        }
    }
}

fn printable(text: &str) -> String {
    let mut out: String = text
        .chars()
        .take(40)
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .collect();
    if text.chars().count() > 40 {
        out.push_str("...");
    }
    out
}

/// SHA-256 of `bytes` as lowercase hex.
fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Whether `name` is a plain file name: 1 to [`MAX_ATTACHMENT_NAME_BYTES`] bytes, the first a letter or a digit, the rest letters, digits,
/// `.`, `_` or `-`. A directory part, a leading dot or hyphen, a space and any non-ASCII character are refused.
pub fn valid_attachment_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= MAX_ATTACHMENT_NAME_BYTES
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// Whether `name` is one of [`RESERVED_NAMES`], compared without case.
pub fn is_reserved_attachment_name(name: &str) -> bool {
    RESERVED_NAMES.iter().any(|r| r.eq_ignore_ascii_case(name))
}

/// Checks a set of attachments and returns the first problem. Pure: it reads nothing but the items. No count and no size is judged here:
/// the request as a whole is held to [`crate::direct::MAX_REQUEST_BYTES`] by [`crate::direct::check_request`].
///
/// Order: each item in turn (name, reserved name, empty, size field, hash, NUL, duplicate name).
pub fn check_attachments(items: &[Attachment]) -> Result<(), AttachmentError> {
    let mut seen: std::collections::HashSet<String> =
        std::collections::HashSet::with_capacity(items.len());
    for a in items {
        let fail = |p: AttachmentProblem| Err(AttachmentError::new(Some(&a.name), p));
        if !valid_attachment_name(&a.name) {
            return fail(AttachmentProblem::BadName);
        }
        let bytes = a.content.len();
        if is_reserved_attachment_name(&a.name) {
            return fail(AttachmentProblem::ReservedName);
        }
        if bytes == 0 {
            return fail(AttachmentProblem::Empty);
        }
        if a.size != bytes as u64 {
            return fail(AttachmentProblem::SizeMismatch);
        }
        if a.content.as_bytes().contains(&0) {
            return fail(AttachmentProblem::Nul);
        }
        if a.sha256 != sha256_hex(a.content.as_bytes()) {
            return fail(AttachmentProblem::ShaMismatch);
        }
        if !seen.insert(a.name.to_ascii_lowercase()) {
            return fail(AttachmentProblem::DuplicateName);
        }
    }
    Ok(())
}
