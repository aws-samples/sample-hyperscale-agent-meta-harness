//! The module's calls to the private internal API: the four routes of protocol 1.38.0.
//!
//! - `POST /internal/message-session`: the session (the epoch) agentd now holds, with the generation of this agentd.
//! - `POST /internal/message-claim`: the dispatch claim of one message, which returns its text.
//! - `POST /internal/message-seal`: "anything pending? if not, the run is done", and the terminal close.
//! - `POST /internal/message-outcome`: submitted, failed, released or rejected.
//!
//! Every body names the run, the agent (`direct`), the generation (a ULID made when this agentd starts; a larger one replaces a smaller
//! one, so the server can tell a replaced worker from the live one) and the session. The calls are signed with the run's session
//! credentials like every other call of agentd to that API ([`SignedTransport`]); the module's logic sees only [`MessageStore`].
//!
//! What an answer means to the module:
//!
//! - HTTP 200 carries the typed answer. A claim that lost is an answer (`not-pending`, `refused`, `nothing`), not an error.
//! - HTTP 409 with `stale-generation` or `stale-epoch` is the fence: this agentd must stop acting on messages. A claim reports it as a
//!   refusal ([`Refusal::StaleGeneration`], [`Refusal::EpochMoved`]); another call reports a final error naming the code. `no-session`
//!   (no session reported yet), `bad-transition` (a normal or limit seal while a message is dispatching, an outcome the state does not
//!   allow) and `stale-attempt` (an outcome of an earlier claim of the message) are final errors: they are not repeated.
//! - Every outcome names the `attempt` of the claim it answers (given in the claim's answer; 0 for the rejection of a message never
//!   claimed), and a session report carries its `number` unchanged on every retry.
//! - HTTP 404 or 405 on a message route (or the gateway's "Missing Authentication Token" for an unknown resource) means the plane serves no
//!   message control (an older control plane, or a test plane): the store marks itself unavailable ([`MessageStore::unavailable`]) and the
//!   module stays out of the run.
//! - HTTP 429, 5xx and a transport failure are retryable.

use crate::BoxFuture;
use crate::creds::CredStore;
use crate::direct::messages::store::MessageStore;
use crate::direct::messages::types::{
    ClaimOutcome, FailReason, Held, Message, Mode, Notice, Refusal, RejectReason, SealMode,
    SealOutcome, StoreError,
};
use crate::http::{SignedHttp, SignedRequest};
use harness_protocol::api::ApiError;
use harness_protocol::api::internal::PendingControl;
use harness_protocol::messages as wire;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// One POST to the internal API: the path, the JSON body, and back the HTTP status and body. A transport failure is a string.
pub trait Transport: Send + Sync {
    fn post<'a>(
        &'a self,
        path: &'a str,
        body: Vec<u8>,
    ) -> BoxFuture<'a, Result<(u16, Vec<u8>), String>>;
}

/// The private internal API, signed with the run's session credentials.
#[derive(Debug)]
pub struct SignedTransport {
    http: SignedHttp,
    creds: Arc<CredStore>,
    base_url: String,
    region: String,
}

impl SignedTransport {
    pub fn new(creds: Arc<CredStore>, base_url: String, region: String) -> Self {
        Self {
            http: SignedHttp::new(),
            creds,
            base_url,
            region,
        }
    }
}

impl Transport for SignedTransport {
    fn post<'a>(
        &'a self,
        path: &'a str,
        body: Vec<u8>,
    ) -> BoxFuture<'a, Result<(u16, Vec<u8>), String>> {
        Box::pin(async move {
            let creds = self.creds.current().await.map_err(|e| e.to_string())?;
            let url = format!("{}{path}", self.base_url.trim_end_matches('/'));
            let resp = self
                .http
                .send(SignedRequest {
                    method: "POST",
                    url: &url,
                    service: "execute-api",
                    region: &self.region,
                    creds: &creds,
                    content_type: Some("application/json"),
                    body,
                })
                .await
                .map_err(|e| e.to_string())?;
            Ok((resp.status, resp.body))
        })
    }
}

/// The generation of this agentd incarnation: a fresh ULID, made when the module is built, that is later than any one this process made
/// before. A new agentd process always starts a new generation; the control plane replaces the older incarnation with it (unless its
/// clock runs more than 60 s behind) and refuses the replaced one for good.
pub fn new_generation() -> String {
    crate::time::new_ulid()
}

/// What a failed call was, before it is turned into a [`StoreError`] or a refusal.
struct Failure {
    code: Option<String>,
    detail: String,
    retryable: bool,
}

impl Failure {
    fn error(self) -> StoreError {
        let text = match &self.code {
            Some(c) => format!("{c}: {}", self.detail),
            None => self.detail,
        };
        if self.retryable {
            StoreError::retryable(text)
        } else {
            StoreError::fatal(text)
        }
    }
}

/// The module's store over the four routes.
pub struct HttpMessageStore {
    transport: Arc<dyn Transport>,
    run_id: String,
    agent_id: String,
    generation: String,
    /// The session now held, and for each claimed message the session it was claimed in and the attempt of that claim (its outcome
    /// names both).
    epochs: Mutex<Epochs>,
    no_routes: AtomicBool,
    fenced: AtomicBool,
}

#[derive(Default)]
struct Epochs {
    /// The number of the session within the run, as the control plane's answer to the session report gave it (`epochNo`).
    session_no: u32,
    current: Option<String>,
    claimed: HashMap<String, (String, u32)>,
}

impl std::fmt::Debug for HttpMessageStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpMessageStore")
            .field("run_id", &self.run_id)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

fn mode_of(m: wire::MessageMode) -> Mode {
    match m {
        wire::MessageMode::Next => Mode::Next,
        wire::MessageMode::Now => Mode::Now,
    }
}

fn reject_of(r: RejectReason) -> wire::RejectReason {
    match r {
        RejectReason::RunSealed => wire::RejectReason::RunSealed,
        RejectReason::RunCancelled => wire::RejectReason::RunCancelled,
        RejectReason::EngineBusy => wire::RejectReason::EngineBusy,
        RejectReason::NoMessageControl => wire::RejectReason::NoMessageControl,
        RejectReason::ContinuationLimit => wire::RejectReason::ContinuationLimit,
        RejectReason::Limit => wire::RejectReason::Limit,
    }
}

fn seal_mode_of(m: SealMode) -> wire::SealMode {
    match m {
        SealMode::Normal => wire::SealMode::Normal,
        SealMode::LimitReached => wire::SealMode::LimitReached,
        SealMode::Terminal => wire::SealMode::Terminal,
    }
}

fn notice_from(n: &wire::MessageNotice) -> Notice {
    Notice {
        id: n.message_id.clone(),
        seq: u64::from(n.seq),
        mode: mode_of(n.mode),
        epoch: n.epoch.clone().unwrap_or_default(),
        bytes: n.bytes as usize,
    }
}

/// The message a `PendingControl::Message` hand-out names; every other control is not about messages.
pub fn notice_of(c: &PendingControl) -> Option<Notice> {
    match c {
        PendingControl::Message(n) => Some(notice_from(n)),
        _ => None,
    }
}

/// The detail of a failed outcome: the reason's word and a short sentence the user can read in `ham`.
fn fail_detail(r: FailReason) -> String {
    match r {
        FailReason::NotSent => "not-sent: the run ended before the prompt was written".into(),
        FailReason::TooLarge => "too-large: the text is longer than agentd sends".into(),
        FailReason::SessionChanged => {
            "session-changed: the session changed before the prompt was written".into()
        }
    }
}

impl HttpMessageStore {
    /// `generation` is the ULID of this agentd process.
    pub fn new(
        transport: Arc<dyn Transport>,
        run_id: &str,
        agent_id: &str,
        generation: &str,
    ) -> Self {
        Self {
            transport,
            run_id: run_id.to_string(),
            agent_id: agent_id.to_string(),
            generation: generation.to_string(),
            epochs: Mutex::new(Epochs::default()),
            no_routes: AtomicBool::new(false),
            fenced: AtomicBool::new(false),
        }
    }

    /// A 404 on a message route was seen: this plane serves no message control.
    pub fn plane_has_no_routes(&self) -> bool {
        self.no_routes.load(Ordering::SeqCst)
    }

    fn epochs(&self) -> std::sync::MutexGuard<'_, Epochs> {
        self.epochs.lock().unwrap_or_else(|p| p.into_inner())
    }

    async fn post<Req: Serialize, Resp: DeserializeOwned>(
        &self,
        path: &str,
        req: &Req,
    ) -> Result<Resp, Failure> {
        let body = serde_json::to_vec(req).map_err(|e| Failure {
            code: None,
            detail: format!("the request could not be written: {e}"),
            retryable: false,
        })?;
        let (status, bytes) = self.transport.post(path, body).await.map_err(|e| Failure {
            code: None,
            detail: format!("transport: {e}"),
            retryable: true,
        })?;
        if status == 200 {
            return serde_json::from_slice(&bytes).map_err(|_| Failure {
                code: None,
                detail: format!("the answer of {path} is not the answer the route gives"),
                retryable: false,
            });
        }
        let api: Option<ApiError> = serde_json::from_slice(&bytes).ok();
        let (code, message) = match api {
            Some(a) => (Some(a.code), a.message),
            None => (None, format!("{path} returned {status}")),
        };
        // The route does not exist on this plane: 404, 405 (the path belongs to something else) or the gateway's answer to an unknown
        // resource (403, "Missing Authentication Token"). Another 403 is the caller's credentials and may pass.
        let unknown_route = status == 404
            || status == 405
            || (status == 403 && message.contains("Missing Authentication Token"));
        if unknown_route {
            self.no_routes.store(true, Ordering::SeqCst);
            return Err(Failure {
                code: Some("no-route".into()),
                detail: format!("{path} is not served: {message}"),
                retryable: false,
            });
        }
        if matches!(code.as_deref(), Some("stale-generation" | "stale-epoch")) {
            self.fenced.store(true, Ordering::SeqCst);
        }
        Err(Failure {
            code,
            detail: message,
            retryable: status == 429 || status >= 500,
        })
    }

    fn ids(&self) -> (String, String, String) {
        (
            self.run_id.clone(),
            self.agent_id.clone(),
            self.generation.clone(),
        )
    }

    async fn outcome(&self, id: &str, outcome: wire::MessageOutcome) -> Result<(), StoreError> {
        let (run_id, agent_id, generation) = self.ids();
        // The claim's session and attempt; a message never claimed (rejected before a claim) names the current session and attempt 0.
        let (epoch, attempt) = {
            let e = self.epochs();
            match e.claimed.get(id) {
                Some((epoch, attempt)) => (epoch.clone(), *attempt),
                None => (e.current.clone().unwrap_or_default(), 0),
            }
        };
        let req = wire::OutcomeRequest {
            run_id,
            agent_id,
            generation,
            epoch,
            message_id: id.to_string(),
            attempt,
            outcome,
        };
        self.post::<_, wire::OutcomeResponse>("/internal/message-outcome", &req)
            .await
            .map(|_| ())
            .map_err(Failure::error)
    }
}

impl MessageStore for HttpMessageStore {
    fn report_epoch<'a>(
        &'a self,
        epoch: &'a str,
        number: u32,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let (run_id, agent_id, generation) = self.ids();
            let req = wire::SessionReport {
                run_id,
                agent_id,
                generation,
                epoch: epoch.to_string(),
                number,
            };
            let ack = self
                .post::<_, wire::SessionAck>("/internal/message-session", &req)
                .await
                .map_err(Failure::error)?;
            let mut e = self.epochs();
            e.current = Some(epoch.to_string());
            e.session_no = ack.epoch_no;
            Ok(())
        })
    }

    fn held<'a>(&'a self, epoch: &'a str) -> BoxFuture<'a, Result<Vec<Held>, StoreError>> {
        Box::pin(async move {
            let (run_id, agent_id, generation) = self.ids();
            let req = wire::ClaimRequest {
                run_id,
                agent_id,
                generation,
                epoch: epoch.to_string(),
                message_id: None,
                limit: None,
            };
            let reply = self
                .post::<_, wire::ClaimResponse>("/internal/message-claim", &req)
                .await
                .map_err(Failure::error)?;
            Ok(match reply {
                wire::ClaimResponse::Claimed {
                    messages,
                    redelivered,
                } => {
                    let mut e = self.epochs();
                    let current_no = e.session_no;
                    messages
                        .into_iter()
                        .map(|m| {
                            e.claimed
                                .insert(m.message_id.clone(), (epoch.to_string(), m.attempt));
                            Held {
                                current_session: m.epoch_no == current_no,
                                redelivered,
                                message: Message {
                                    id: m.message_id,
                                    text: m.text,
                                    mode: mode_of(m.mode),
                                    epoch: epoch.to_string(),
                                    seq: u64::from(m.seq),
                                },
                            }
                        })
                        .collect()
                }
                // nothing is held and nothing is pending, or the run was cancelled or sealed: there is nothing to adopt
                _ => Vec::new(),
            })
        })
    }

    fn claim<'a>(
        &'a self,
        id: &'a str,
        epoch: &'a str,
    ) -> BoxFuture<'a, Result<ClaimOutcome, StoreError>> {
        Box::pin(async move {
            let (run_id, agent_id, generation) = self.ids();
            let req = wire::ClaimRequest {
                run_id,
                agent_id,
                generation,
                epoch: epoch.to_string(),
                message_id: Some(id.to_string()),
                limit: None,
            };
            let reply = match self
                .post::<_, wire::ClaimResponse>("/internal/message-claim", &req)
                .await
            {
                Ok(r) => r,
                Err(f) => {
                    return match f.code.as_deref() {
                        Some("stale-generation") => {
                            Ok(ClaimOutcome::Refused(Refusal::StaleGeneration))
                        }
                        Some("stale-epoch") => Ok(ClaimOutcome::Refused(Refusal::EpochMoved)),
                        _ => Err(f.error()),
                    };
                }
            };
            Ok(match reply {
                wire::ClaimResponse::Claimed { messages, .. } => {
                    match messages.into_iter().find(|m| m.message_id == id) {
                        Some(m) => {
                            self.epochs()
                                .claimed
                                .insert(m.message_id.clone(), (epoch.to_string(), m.attempt));
                            ClaimOutcome::Claimed(Message {
                                id: m.message_id,
                                text: m.text,
                                mode: mode_of(m.mode),
                                epoch: epoch.to_string(),
                                seq: u64::from(m.seq),
                            })
                        }
                        None => ClaimOutcome::Refused(Refusal::Unknown),
                    }
                }
                wire::ClaimResponse::Nothing { .. } => {
                    ClaimOutcome::Refused(Refusal::NotPending("none".into()))
                }
                wire::ClaimResponse::NotPending { state } => {
                    ClaimOutcome::Refused(Refusal::NotPending(state.word().to_string()))
                }
                wire::ClaimResponse::Refused { reason } => ClaimOutcome::Refused(match reason {
                    wire::ClaimRefusal::CancelQueued => Refusal::Cancelled,
                    wire::ClaimRefusal::AdmissionClosed => Refusal::Sealed,
                }),
            })
        })
    }

    fn submitted<'a>(&'a self, id: &'a str, turn: u32) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            self.outcome(id, wire::MessageOutcome::Submitted { turn })
                .await
        })
    }

    fn failed<'a>(
        &'a self,
        id: &'a str,
        reason: FailReason,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            self.outcome(
                id,
                wire::MessageOutcome::Failed {
                    detail: fail_detail(reason),
                },
            )
            .await
        })
    }

    fn release<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move { self.outcome(id, wire::MessageOutcome::Released).await })
    }

    fn reject<'a>(
        &'a self,
        id: &'a str,
        reason: RejectReason,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            self.outcome(
                id,
                wire::MessageOutcome::Rejected {
                    reason: reject_of(reason),
                },
            )
            .await
        })
    }

    fn seal<'a>(
        &'a self,
        epoch: &'a str,
        mode: SealMode,
    ) -> BoxFuture<'a, Result<SealOutcome, StoreError>> {
        Box::pin(async move {
            let (run_id, agent_id, generation) = self.ids();
            let req = wire::SealRequest {
                run_id,
                agent_id,
                generation,
                epoch: epoch.to_string(),
                mode: seal_mode_of(mode),
            };
            let reply = self
                .post::<_, wire::SealResponse>("/internal/message-seal", &req)
                .await
                .map_err(Failure::error)?;
            Ok(match reply {
                wire::SealResponse::Pending { messages } => {
                    SealOutcome::Pending(messages.iter().map(notice_from).collect())
                }
                wire::SealResponse::Sealed { watermark, .. } => SealOutcome::Sealed {
                    watermark: u64::from(watermark),
                },
            })
        })
    }

    fn unavailable(&self) -> bool {
        self.plane_has_no_routes() || self.fenced.load(Ordering::SeqCst)
    }
}
