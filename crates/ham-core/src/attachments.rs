//! Reading the user's attachments from disk: `ham... --attach <file>`, repeatable. A file the user names is read as
//! UTF-8 text and sent as an item of the `attachments` member of the start request; agentd places it read-only in the run's context directory,
//! outside the work tree.
//!
//! The shared validator (`harness_protocol::direct::check_attachments`) judges the wire form (the name, the size field, the hash, a name used
//! twice); the checks that need the disk are here:
//!
//! - the path must be a regular file (a link the user names is followed to one; a folder, a device or a pipe is refused);
//! - an attachment has no size of its own and the set has no count, but a file longer than a whole request can be
//!   (`harness_protocol::direct::MAX_REQUEST_BYTES`), or files that together are, are refused before they are read, with the bound named: they
//!   could never be sent. A file that is not UTF-8 text is an error;
//! - the item is named by the file's own name, which must be a plain file name (`.env` and `my notes.md` are refused: rename or copy the file).
//!
//! A problem names the path as the user typed it and the rule, never the content of a file.

use harness_protocol::direct::{
    Attachment, AttachmentProblem, MAX_REQUEST_BYTES, check_attachments, exceeds_request,
};
use std::fmt;
use std::path::Path;

/// What is wrong with an attachment or the set of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Why {
    /// A rule of the shared validator, or a disk check that uses its words (`NotRegular`, `NotText`).
    Invalid(AttachmentProblem),
    /// The operating system's reason.
    Unreadable(String),
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Why::Invalid(p) => write!(f, "{p}"),
            Why::Unreadable(e) => write!(f, "cannot be read: {e}"),
        }
    }
}

/// A problem, with the path it concerns (as the user typed it; `None` for a rule about the whole set).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentsError {
    pub path: Option<String>,
    pub why: Why,
}

impl fmt::Display for AttachmentsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(p) => write!(f, "--attach {p:?} {}", self.why),
            None => write!(f, "--attach: {}", self.why),
        }
    }
}

impl std::error::Error for AttachmentsError {}

fn error(path: Option<&str>, why: Why) -> AttachmentsError {
    AttachmentsError {
        path: path.map(printable),
        why,
    }
}

/// The path with every control character replaced, so a hostile name cannot reach a terminal.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// The attachments `paths` name, in the order given, validated as a set. No path is no attachments.
pub fn read_attachments(paths: &[String]) -> Result<Vec<Attachment>, AttachmentsError> {
    let mut items = Vec::with_capacity(paths.len());
    let mut total: u64 = 0;
    for shown in paths {
        let path = Path::new(shown);
        let meta = std::fs::metadata(path)
            .map_err(|e| error(Some(shown), Why::Unreadable(e.kind().to_string())))?;
        if !meta.is_file() {
            return Err(error(
                Some(shown),
                Why::Invalid(AttachmentProblem::NotRegular),
            ));
        }
        if exceeds_request(meta.len()) {
            return Err(error(
                Some(shown),
                Why::Invalid(AttachmentProblem::TooLarge {
                    bytes: usize::try_from(meta.len()).unwrap_or(usize::MAX),
                    max: MAX_REQUEST_BYTES,
                }),
            ));
        }
        total = total.saturating_add(meta.len());
        if exceeds_request(total) {
            return Err(error(
                Some(shown),
                Why::Invalid(AttachmentProblem::TooLargeTotal {
                    bytes: usize::try_from(total).unwrap_or(usize::MAX),
                    max: MAX_REQUEST_BYTES,
                }),
            ));
        }
        let bytes = std::fs::read(path)
            .map_err(|e| error(Some(shown), Why::Unreadable(e.kind().to_string())))?;
        let text = String::from_utf8(bytes)
            .map_err(|_| error(Some(shown), Why::Invalid(AttachmentProblem::NotText)))?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let item = Attachment::new(&name, &text);
        // The rules of one item (name, empty, size), so the error names the path that broke them.
        check_attachments(std::slice::from_ref(&item))
            .map_err(|e| error(Some(shown), Why::Invalid(e.problem)))?;
        items.push(item);
    }
    // The rules of the set (count, a name used twice, the total): the item the rule names is shown when there is one.
    check_attachments(&items).map_err(|e| error(e.name.as_deref(), Why::Invalid(e.problem)))?;
    Ok(items)
}
