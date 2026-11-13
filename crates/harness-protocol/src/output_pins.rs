//! The version records of the three end-of-run objects.
//!
//! agentd writes `bundle`, `summary.json` and `test-report.json` under `runs/<run>/agents/direct/turns/<n>/` with the run's session
//! credentials, and the engine holds the same credentials: a process it leaves behind can write a newer version of any of these keys. So the
//! harness does not read "the object at the key". agentd keeps what S3 answered to each of its own writes (the SHA-256 of the body it sent
//! and the version id S3 gave it) and sends the records in `turn-complete`, a call that needs the run's completion key, which the engine does
//! not hold. The Step Functions state carries them to `plan-delivery` and the bundle validator, which read exactly that version and check the
//! digest. An object whose record is missing, whose version is gone or whose bytes differ is not used.
//!
//! The records are only trusted for a run whose Agent item says so ([`crate::store::CredsBinding::output_pins_required`]): worker-api
//! decides that when the run is launched, never the body of a request.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The longest version id S3 gives (its documented bound).
pub const MAX_VERSION_ID_CHARS: usize = 1024;

/// What agentd knows about one object it wrote.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutputPin {
    /// Lowercase hex SHA-256 of the bytes agentd sent. S3 stores the same digest as the object's checksum, so a reader can compare it
    /// without downloading the body.
    pub sha256: String,
    /// The version id S3 returned for agentd's own write. Absent when the answer carried none (an unversioned bucket); the digest then
    /// has to hold on the current version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
}

impl OutputPin {
    /// The record of `body` as agentd sent it.
    pub fn for_body(body: &[u8], version_id: Option<String>) -> Self {
        OutputPin {
            sha256: sha256_hex(body),
            version_id,
        }
    }

    /// Whether `body` is the object this record describes.
    pub fn matches_body(&self, body: &[u8]) -> bool {
        self.sha256 == sha256_hex(body)
    }

    /// A reason when the record is not well formed: a digest that is not 64 lowercase hex digits, or a version id that is empty, `null`,
    /// longer than [`MAX_VERSION_ID_CHARS`] or holds anything but printable ASCII.
    pub fn validate(&self) -> Result<(), String> {
        if self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("sha256 is not 64 lowercase hex digits".into());
        }
        if let Some(v) = &self.version_id
            && (v.is_empty()
                || v == "null"
                || v.len() > MAX_VERSION_ID_CHARS
                || !v.bytes().all(|b| b.is_ascii_graphic()))
        {
            return Err("versionId is not one printable token".into());
        }
        Ok(())
    }

    /// The digest as S3 reports a stored checksum: the base64 of the 32 bytes. A digest that is not well formed gives an empty string,
    /// which equals no checksum.
    pub fn sha256_base64(&self) -> String {
        let mut bytes = Vec::with_capacity(32);
        let hex = self.sha256.as_bytes();
        if hex.len() != 64 {
            return String::new();
        }
        for pair in hex.chunks(2) {
            let (Some(hi), Some(lo)) = (nibble(pair[0]), nibble(pair[1])) else {
                return String::new();
            };
            bytes.push((hi << 4) | lo);
        }
        base64(&bytes)
    }
}

/// The records of the objects one turn wrote. Each member is present when agentd's write of that object was answered; an object that was
/// not written has none.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutputPins {
    /// The full-history bundle, `turns/<n>/bundle`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<OutputPin>,
    /// `turns/<n>/summary.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<OutputPin>,
    /// `turns/<n>/test-report.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_report: Option<OutputPin>,
}

impl OutputPins {
    /// No object has a record.
    pub fn is_empty(&self) -> bool {
        self.bundle.is_none() && self.summary.is_none() && self.test_report.is_none()
    }

    /// A reason, naming the member, when one record is not well formed. A set with one bad member is refused as a whole.
    pub fn validate(&self) -> Result<(), String> {
        for (name, pin) in [
            ("bundle", &self.bundle),
            ("summary", &self.summary),
            ("testReport", &self.test_report),
        ] {
            if let Some(p) = pin {
                p.validate()
                    .map_err(|e| format!("outputPins.{name}: {e}"))?;
            }
        }
        Ok(())
    }
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

/// Standard base64 with padding (RFC 4648 section 4).
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}
