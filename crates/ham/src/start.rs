//! Starting a run: the request and its `Idempotency-Key` come from `ham_core::request`,
//! with a nonce fresh per launch, so a retry of one launch reuses the key and a new launch of the
//! same prompt starts a new run. The rules and the nudge are read by `crate::rules_cmd`.

use crate::api::{Api, ApiError};
use crate::http::ControlPlane;
use ham_core::request::direct_idempotency_key_on;
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::goal::GoalStatus;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Started {
    pub run_id: String,
    pub status: GoalStatus,
    pub idempotency_key: String,
}

/// `HAMH_IDEMPOTENCY_NONCE` when set (1-64 visible ASCII characters; the live E2E sets it so it can
/// resend the composer's request with its key), else [`new_nonce`]. An empty value is unset.
pub fn launch_nonce(value: Option<&str>) -> Result<String, String> {
    match value.filter(|v| !v.is_empty()) {
        None => Ok(new_nonce()),
        Some(v) if v.len() <= 64 && v.bytes().all(|b| (b'!'..=b'~').contains(&b)) => {
            Ok(v.to_string())
        }
        Some(_) => Err("HAMH_IDEMPOTENCY_NONCE must be 1-64 visible ASCII characters".into()),
    }
}

/// 16 random bytes, hex.
pub fn new_nonce() -> String {
    let mut b = [0u8; 16];
    // getrandom fails only when the OS has no entropy source.
    getrandom::fill(&mut b).unwrap_or_else(|e| panic!("no OS entropy: {e}"));
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// One `POST /v1/direct/runs`, retried with the same key on a transient failure. `request` comes from
/// `ham_core::request::direct_run_request`, which has already chosen the model (`--model`, `HAMH_MODEL`, `default_model`, else
/// Claude Sonnet 5.5) and sent nothing when the choice names no selectable model. The prompt is sent as typed.
pub async fn start_direct<P: ControlPlane>(
    api: &Api<P>,
    request: &StartDirectRunRequest,
    nonce: &str,
) -> Result<Started, ApiError> {
    // With several backends the Api is aimed at one and its alias is part of the key; with one backend the key is the one of before.
    let key = direct_idempotency_key_on(request, nonce, api.target());
    let r = api.start_direct_run(request, &key).await?;
    Ok(Started {
        run_id: r.run_id,
        status: r.status,
        idempotency_key: key,
    })
}
