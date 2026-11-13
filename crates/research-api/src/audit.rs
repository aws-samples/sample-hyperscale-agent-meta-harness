//! The audit object: one immutable record per call under `runs/<run>/agents/direct/research/audit/`. S3 cannot append, so each call writes
//! its own. The record holds the exact query and URLs (and the provider and counts), never a header, a key or fetched text.

use crate::ports::{Blobs, Fail};
use harness_protocol::research::{
    AUDIT_SCHEMA, AuditInput, AuditObject, AuditStatus, ResearchErrorCode, ResearchOp,
    ResearchProvider, s3,
};

/// What an operation knows for its audit object; the handler adds who, when and how it ended.
#[derive(Clone, Debug)]
pub struct AuditDraft {
    pub op: ResearchOp,
    pub input: AuditInput,
    pub credits: Option<u32>,
    pub items: Vec<String>,
    pub seen_before: Option<bool>,
    pub upstream_request_id: Option<String>,
    pub job_id: Option<String>,
}

impl AuditDraft {
    pub fn new(op: ResearchOp) -> Self {
        Self {
            op,
            input: AuditInput::default(),
            credits: None,
            items: Vec::new(),
            seen_before: None,
            upstream_request_id: None,
            job_id: None,
        }
    }
}

/// Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn parts(ms: u64) -> (i64, u32, u32, u64, u64, u64) {
    let s = ms / 1000;
    let (y, m, d) = civil((s / 86_400) as i64);
    let rem = s % 86_400;
    (y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// `2026-10-16T15:00:00Z`
pub fn rfc3339(ms: u64) -> String {
    let (y, m, d, hh, mm, ss) = parts(ms);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// `20261016T150000Z`, the time part of an audit key.
pub fn stamp(ms: u64) -> String {
    let (y, m, d, hh, mm, ss) = parts(ms);
    format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z")
}

/// A request id made safe for a key: letters, digits, `.`, `_`, `-`, at most 64 characters.
pub fn safe_id(id: &str) -> String {
    let s: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    if s.is_empty() { "unknown".into() } else { s }
}

/// How a call ended, as the audit object records it: policy refusals and limits are `refused`, mistakes and upstream failures `error`.
pub fn status_of(code: Option<ResearchErrorCode>) -> AuditStatus {
    use ResearchErrorCode as C;
    match code {
        None => AuditStatus::Ok,
        Some(
            C::Disabled
            | C::RefusedSecret
            | C::RefusedUrl
            | C::RefusedUnseenUrl
            | C::RefusedHost
            | C::LimitCalls
            | C::LimitBytes
            | C::LimitRate,
        ) => AuditStatus::Refused,
        Some(_) => AuditStatus::Error,
    }
}

#[allow(clippy::too_many_arguments)]
pub fn build(
    run: &str,
    agent: &str,
    provider: ResearchProvider,
    request_id: &str,
    now_ms: u64,
    ms: u64,
    code: Option<ResearchErrorCode>,
    d: AuditDraft,
) -> AuditObject {
    AuditObject {
        schema: AUDIT_SCHEMA.to_string(),
        run: run.to_string(),
        agent: agent.to_string(),
        op: d.op,
        provider,
        at: rfc3339(now_ms),
        request_id: request_id.to_string(),
        status: status_of(code),
        code,
        ms,
        input: d.input,
        credits: d.credits,
        items: d.items,
        seen_before: d.seen_before,
        upstream_request_id: d.upstream_request_id,
        job_id: d.job_id,
    }
}

/// Writes the object (create-only). A key that exists is not overwritten and is an error: request ids are unique.
pub async fn write<B: Blobs>(blobs: &B, a: &AuditObject) -> Result<(), Fail> {
    let key = s3::audit_key(&a.run, &stamp_of(a), &safe_id(&a.request_id));
    let body = serde_json::to_vec(a).map_err(|e| Fail::internal(format!("audit object: {e}")))?;
    match blobs.put_new(&key, body, "application/json").await? {
        crate::ports::PutOutcome::Created => Ok(()),
        crate::ports::PutOutcome::Exists => {
            Err(Fail::internal("an audit object with this key exists"))
        }
    }
}

fn stamp_of(a: &AuditObject) -> String {
    // `at` is RFC 3339 UTC written by `rfc3339`; the key part drops the separators.
    a.at.chars().filter(|c| c.is_ascii_alphanumeric()).collect()
}
