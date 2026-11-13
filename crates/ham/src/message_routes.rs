//! The client side of message control: typed calls on [`Api`] for the four message routes and the id of a message.
//!
//! - [`Api::send_message`]: `POST /v1/runs/{runId}/messages`. The message id is the idempotency key, so the call is retried like a read: a
//!   timeout that hid a 202 is answered with the stored message (`already-accepted`), never a second message. The answer says only that the
//!   server stored the message (`pending`); Cline has not read it.
//! - [`Api::list_messages`] and [`Api::get_message`]: the statuses the pending row shows. A single message is the way to reconcile an
//!   uncertain send: ask for the id that was sent.
//! - [`Api::withdraw_message`]: a pending or held message becomes withdrawn; one Cline has taken is `409 already-dispatching`.
//! - [`mint_message_id`]: a fresh ULID from the clock and the OS entropy.
//!
//! The words for a status or a refusal are `ham_core::messages`. Nothing here keeps state.

use crate::api::{Api, ApiError, json_headers};
use crate::http::ControlPlane;
use ham_core::messages::{MessageResponse, MessagesPage, SendMessageResponse, ulid_from};
use harness_protocol::messages::SendMessageRequest;

/// A message id: a ULID made from the clock (milliseconds) and ten bytes of OS entropy.
pub fn mint_message_id() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0));
    let mut random = [0u8; 10];
    // getrandom fails only when the OS has no entropy source.
    getrandom::fill(&mut random).unwrap_or_else(|e| panic!("no OS entropy: {e}"));
    ulid_from(ms, random)
}

impl<P: ControlPlane> Api<P> {
    /// `POST /v1/runs/{runId}/messages`: 202 `accepted` when the server stored the message, 200 `already-accepted` for a retry of the same
    /// id and payload. Retried like a read because the id is the key.
    pub async fn send_message(
        &self,
        run_id: &str,
        req: &SendMessageRequest,
    ) -> Result<SendMessageResponse, ApiError> {
        let body = serde_json::to_vec(req).map_err(|e| ApiError::Decode {
            what: "SendMessageRequest".into(),
            msg: e.to_string(),
        })?;
        self.retried(
            "SendMessageResponse",
            "POST",
            &format!("/v1/runs/{run_id}/messages"),
            &json_headers(None),
            &body,
        )
        .await
    }

    /// `GET /v1/runs/{runId}/messages`: every message of the run with its status, the current session and whether the run still takes
    /// messages.
    pub async fn list_messages(&self, run_id: &str) -> Result<MessagesPage, ApiError> {
        self.retried(
            "MessagesPage",
            "GET",
            &format!("/v1/runs/{run_id}/messages"),
            &[],
            &[],
        )
        .await
    }

    /// `GET /v1/runs/{runId}/messages/{messageId}`.
    pub async fn get_message(
        &self,
        run_id: &str,
        message_id: &str,
    ) -> Result<MessageResponse, ApiError> {
        self.retried(
            "MessageResponse",
            "GET",
            &format!("/v1/runs/{run_id}/messages/{message_id}"),
            &[],
            &[],
        )
        .await
    }

    /// `POST /v1/runs/{runId}/messages/{messageId}/withdraw`. A withdrawal repeated is the same answer, so it is retried like a read.
    pub async fn withdraw_message(
        &self,
        run_id: &str,
        message_id: &str,
    ) -> Result<MessageResponse, ApiError> {
        self.retried(
            "MessageResponse",
            "POST",
            &format!("/v1/runs/{run_id}/messages/{message_id}/withdraw"),
            &json_headers(None),
            b"{}",
        )
        .await
    }
}
