//! Evidence record header: the first JSON block of every evidence record.

use crate::common::is_utc_timestamp;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const EVIDENCE_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum EvidenceResult {
    Pass,
    Fail,
}

/// Header fields order. Optional digests serialize as `null` when absent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceHeader {
    pub schema_version: u32,
    pub kind: String,
    #[schemars(extend("format" = "date-time"))]
    pub created_at: String,
    pub host: String,
    #[schemars(pattern(r"^[0-9]{12}$"))]
    pub account: String,
    pub region: String,
    pub stage: String,
    pub code_sha: Option<String>,
    pub source_tree: Option<String>,
    pub deployment_digest: Option<String>,
    pub image_digest: Option<String>,
    pub criteria_sha256: Option<String>,
    pub result: EvidenceResult,
    pub inputs: serde_json::Value,
    pub measurements: serde_json::Value,
    pub run_ids: Vec<String>,
    pub notes: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvidenceError {
    #[error("schemaVersion {0} is not supported (expected {EVIDENCE_SCHEMA_VERSION})")]
    SchemaVersion(u32),
    #[error("account {0:?} is not a 12-digit AWS account ID")]
    Account(String),
    #[error("createdAt {0:?} is not an RFC 3339 UTC timestamp")]
    CreatedAt(String),
    #[error("kind is empty")]
    Kind,
    #[error("{0} must be a JSON object")]
    NotAnObject(&'static str),
}

pub fn validate_header(h: &EvidenceHeader) -> Result<(), EvidenceError> {
    if h.schema_version != EVIDENCE_SCHEMA_VERSION {
        return Err(EvidenceError::SchemaVersion(h.schema_version));
    }
    if h.kind.trim().is_empty() {
        return Err(EvidenceError::Kind);
    }
    if h.account.len() != 12 || !h.account.bytes().all(|c| c.is_ascii_digit()) {
        return Err(EvidenceError::Account(h.account.clone()));
    }
    if !is_utc_timestamp(&h.created_at) {
        return Err(EvidenceError::CreatedAt(h.created_at.clone()));
    }
    if !h.inputs.is_object() {
        return Err(EvidenceError::NotAnObject("inputs"));
    }
    if !h.measurements.is_object() {
        return Err(EvidenceError::NotAnObject("measurements"));
    }
    Ok(())
}
