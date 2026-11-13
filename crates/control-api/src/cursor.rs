//! The opaque position token of the transcript route: the chunk and record to read next, bound to the run and the
//! caller it was issued to.
//!
//! The token is `base64url(JSON {v, b, s, i, k, e})`: the version, a binding (the first 16 bytes of the SHA-256 of the run id and the
//! caller's ARN, in hex), the chunk number `s`, the index of the next record in that chunk `i`, after a listing that ran into its
//! cap the last key examined `k` (the part after the transcript prefix, at most [`MAX_KEY_CHARS`] bytes, and only a key that
//! [`safe_key`] accepts) and, for a position inside a chunk, the ETag `e` the chunk had when the position was issued (a chunk that
//! changed since is reported, never read at a stale index). The binding is a guard against mix-ups, such as a cursor of
//! one run used on another: it is not a secret and not an authorization. The route checks the caller owns the run before it reads a
//! token, and a token only positions a read the owner could make anyway.
//!
//! A token is at most [`MAX_TOKEN_CHARS`] long and never appears in a log line: the access line carries the path only, and the route's
//! diagnostics never carry a query value.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The longest token accepted.
pub const MAX_TOKEN_CHARS: usize = 2048;
/// The longest key suffix a token carries, in bytes (S3 keys are at most 1,024 bytes); a longer one ends the read instead.
pub const MAX_KEY_CHARS: usize = 1024;
/// The longest ETag a token carries.
pub const MAX_ETAG_CHARS: usize = 64;
const VERSION: u32 = 1;

/// Where the next read starts: record `idx` of chunk `seq`, listing after `after_key` when it is set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Position {
    pub seq: u64,
    pub idx: u32,
    /// The last key examined, relative to the transcript prefix (a listing that ran into its cap continues after it).
    pub after_key: Option<String>,
    /// The ETag the chunk had when this position inside it was issued.
    pub etag: Option<String>,
}

/// Whether a key suffix may be carried in a token: 1 to [`MAX_KEY_CHARS`] bytes, no control character, and nothing a redaction would
/// change (a token is returned to the caller and never logged, but it must not become a place a stored secret leaves through).
pub fn safe_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_KEY_CHARS
        && !key.chars().any(char::is_control)
        && ham_core::redact::redact_tokens(key) == key
}

fn safe_etag(etag: &str) -> bool {
    !etag.is_empty() && etag.len() <= MAX_ETAG_CHARS && etag.bytes().all(|b| b.is_ascii_graphic())
}

impl Position {
    pub fn at(seq: u64, idx: u32) -> Self {
        Self {
            seq,
            idx,
            after_key: None,
            etag: None,
        }
    }
}

/// Why a token was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CursorError {
    #[error("the cursor is longer than {} characters", MAX_TOKEN_CHARS)]
    TooLong,
    #[error("the cursor is not one this API issued")]
    Malformed,
    #[error("the cursor is for another version of this API")]
    Version,
    #[error("the cursor was issued for another run or another caller")]
    WrongBinding,
}

#[derive(Serialize, Deserialize)]
struct Wire {
    v: u32,
    b: String,
    s: u64,
    i: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    k: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    e: Option<String>,
}

fn binding(run_id: &str, caller: &str) -> String {
    let mut h = Sha256::new();
    h.update(run_id.as_bytes());
    h.update([0u8]);
    h.update(caller.as_bytes());
    h.finalize()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The token for `pos`, bound to `run_id` and `caller`.
pub fn encode(run_id: &str, caller: &str, pos: &Position) -> String {
    let wire = Wire {
        v: VERSION,
        b: binding(run_id, caller),
        s: pos.seq,
        i: pos.idx,
        k: pos.after_key.clone().filter(|k| safe_key(k)),
        e: pos.etag.clone().filter(|e| safe_etag(e)),
    };
    let json = serde_json::to_vec(&wire).unwrap_or_default();
    URL_SAFE_NO_PAD.encode(json)
}

/// The position a token names, if it was issued for this run and caller.
pub fn decode(run_id: &str, caller: &str, token: &str) -> Result<Position, CursorError> {
    if token.len() > MAX_TOKEN_CHARS {
        return Err(CursorError::TooLong);
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(token)
        .map_err(|_| CursorError::Malformed)?;
    let wire: Wire = serde_json::from_slice(&bytes).map_err(|_| CursorError::Malformed)?;
    if wire.v != VERSION {
        return Err(CursorError::Version);
    }
    if wire.b != binding(run_id, caller) {
        return Err(CursorError::WrongBinding);
    }
    if wire.k.as_ref().is_some_and(|k| !safe_key(k))
        || wire.e.as_ref().is_some_and(|e| !safe_etag(e))
    {
        return Err(CursorError::Malformed);
    }
    Ok(Position {
        seq: wire.s,
        idx: wire.i,
        after_key: wire.k,
        etag: wire.e,
    })
}
