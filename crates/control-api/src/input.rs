//! The pieces of `POST /v1/runs/{runId}/input` and of the `needsInput` mark in `GET /v1/runs` that need no store access:
//! the body rules, the admission of a request id against the event it names, the comparison of a repeated
//! answer with the stored one, the acknowledgement search and the brief of an open question. `crate::controls::answer_input`
//! does the reads and the write around them.

use crate::direct_models;
use crate::handler::{Reply, Store, StoreError, error};
use harness_protocol::api::internal::PendingControl;
use harness_protocol::api::v1::{InputAnswer, NeedsInputBrief};
use harness_protocol::common::{is_ulid, parse_utc_unix};
use harness_protocol::direct::DIRECT_AGENT_ID;
use harness_protocol::direct_input::{
    INPUT_ADMISSION_GRACE_S, InstructAction, MAX_INPUT_TEXT_BYTES, NeedsInputPayload,
    PROGRESS_INPUT_CLOSED, PROGRESS_INPUT_IGNORED, PROGRESS_INPUT_RECEIVED, PROGRESS_NEEDS_INPUT,
    fold_input,
};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use harness_protocol::store::event_shard;
use std::time::{SystemTime, UNIX_EPOCH};

/// Events of the direct agent the run list reads to find an open question. A question that is open leaves the agent waiting, so
/// few events follow it; the tail only has to reach back over the quiet wait.
pub(crate) const TAIL_EVENTS: usize = 200;
/// A control agentd took this long ago without an acknowledgement is queued again by a resend of the same answer.
pub(crate) const REARM_AFTER_S: i64 = 12;
/// Events after the request the resend path reads to look for an acknowledgement.
pub(crate) const ACK_SCAN_EVENTS: usize = 500;

/// The shard the direct agent's events are in.
pub(crate) fn direct_shard() -> u8 {
    event_shard(Authority::Agent, Some(DIRECT_AGENT_ID))
}

/// Unix seconds now.
pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The wire word of an action (`continue_with_prompt`).
pub(crate) fn word(action: InstructAction) -> String {
    serde_json::to_value(action)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn bad_input(message: impl AsRef<str>) -> Reply {
    error(400, "bad-input", message.as_ref())
}

/// The 400 a body earns before anything is read: the request id is a ULID, the text is some text of at most 262,144 bytes
/// when present, and each action takes exactly its own members.
pub(crate) fn validate(a: &InputAnswer) -> Result<(), Reply> {
    if !is_ulid(&a.request_id) {
        return Err(error(
            400,
            "bad-request",
            "InputAnswer: requestId must be the id of a question (a ULID)",
        ));
    }
    if let Some(t) = &a.text {
        if t.trim().is_empty() {
            return Err(bad_input(
                "the message is blank; write something for Cline or leave the message out",
            ));
        }
        if t.len() > MAX_INPUT_TEXT_BYTES {
            return Err(bad_input(format!(
                "the message is {} bytes; the limit is {MAX_INPUT_TEXT_BYTES} bytes (256 KiB)",
                t.len()
            )));
        }
    }
    let model_blank = a.model.as_deref().is_some_and(|m| m.trim().is_empty());
    match a.action {
        InstructAction::ContinueWithPrompt => {
            if a.text.is_none() {
                return Err(bad_input(format!(
                    "continue_with_prompt needs a message for Cline (some text of at most {MAX_INPUT_TEXT_BYTES} bytes)"
                )));
            }
            if a.model.is_some() {
                return Err(bad_input("continue_with_prompt takes no model"));
            }
        }
        InstructAction::NewSessionWithPrompt => {
            if a.model.is_some() {
                return Err(bad_input("new_session_with_prompt takes no model"));
            }
        }
        InstructAction::TryModel => {
            if a.model.is_none() || model_blank {
                return Err(bad_input("try_model needs the model to switch to"));
            }
        }
        InstructAction::Finish => {
            if a.text.is_some() || a.model.is_some() {
                return Err(bad_input("finish takes no message and no model"));
            }
        }
    }
    Ok(())
}

/// The catalog check of a `try_model` target: a route (the model's own or an alternate one) of a model with a passing smoke
/// record. The question's `models` list holds the run's routes, alternates included, so the check accepts what the question
/// offers; agentd then checks the id against the routes of this run (`input-ignored bad-model`).
pub(crate) fn check_model(model: &str) -> Result<(), Reply> {
    let models = direct_models::builtin();
    if models.find_route(model).is_some_and(|m| m.selectable()) {
        return Ok(());
    }
    models
        .check(model)
        .map(|_| ())
        .map_err(|e| error(422, e.code(), &e.to_string()))
}

/// The question the event names, or the 404 `no-such-request`: the event exists, belongs to this run, is a `turn.progress` of the
/// direct agent with authority `agent`, decodes as a `needs-input` payload with `blocking: true` and carries its own id as the
/// request id.
pub(crate) fn admit(run_id: &str, event: Option<&Envelope>) -> Result<NeedsInputPayload, Reply> {
    let gone = || {
        error(
            404,
            "no-such-request",
            "the run has no such question; read its events again",
        )
    };
    let e = event.ok_or_else(gone)?;
    if e.run_id != run_id
        || e.event_type != EventType::TurnProgress
        || e.authority != Authority::Agent
        || e.agent_id.as_deref() != Some(DIRECT_AGENT_ID)
    {
        return Err(gone());
    }
    let Payload::Inline { value } = &e.payload else {
        return Err(gone());
    };
    let p: NeedsInputPayload = serde_json::from_value(value.clone()).map_err(|_| gone())?;
    if p.kind != PROGRESS_NEEDS_INPUT || !p.blocking || p.request_id != e.event_id {
        return Err(gone());
    }
    Ok(p)
}

/// True when the window closed more than the admission grace ago (agentd's monotonic clock decides; this check is advisory).
pub(crate) fn expired(p: &NeedsInputPayload, now: i64) -> bool {
    p.expires_at
        .as_deref()
        .and_then(parse_utc_unix)
        .is_some_and(|t| now > t.saturating_add(INPUT_ADMISSION_GRACE_S))
}

/// The action of a stored control, when it is an answer.
pub(crate) fn stored_action(c: &PendingControl) -> Option<InstructAction> {
    match c {
        PendingControl::Instruct { action, .. } => Some(*action),
        _ => None,
    }
}

/// Whether the stored control is this very answer (the origin is control-api's to write, so it is not compared).
pub(crate) fn same_answer(stored: &PendingControl, a: &InputAnswer) -> bool {
    matches!(
        stored,
        PendingControl::Instruct { request_id, action, text, model, .. }
            if *request_id == a.request_id && *action == a.action && *text == a.text && *model == a.model
    )
}

/// The `kind` and `requestId` of a direct-agent progress event.
fn progress_of(e: &Envelope) -> Option<(&str, &str)> {
    if e.event_type != EventType::TurnProgress || e.agent_id.as_deref() != Some(DIRECT_AGENT_ID) {
        return None;
    }
    let Payload::Inline { value } = &e.payload else {
        return None;
    };
    Some((
        value.get("kind")?.as_str()?,
        value.get("requestId")?.as_str()?,
    ))
}

/// Whether agentd has answered the request: an `input-received`, `input-ignored` or `input-closed` for it.
pub(crate) fn acknowledged(events: &[Envelope], request_id: &str) -> bool {
    events.iter().filter_map(progress_of).any(|(kind, id)| {
        id == request_id
            && matches!(
                kind,
                PROGRESS_INPUT_RECEIVED | PROGRESS_INPUT_IGNORED | PROGRESS_INPUT_CLOSED
            )
    })
}

/// The open blocking question in these events (oldest first), as the run list shows it. A non-blocking notice is not a question.
pub(crate) fn brief_of(events: &[Envelope]) -> Option<NeedsInputBrief> {
    let direct: Vec<&Envelope> = events
        .iter()
        .filter(|e| e.agent_id.as_deref() == Some(DIRECT_AGENT_ID))
        .collect();
    let open = fold_input(direct).open?;
    open.payload.blocking.then_some(NeedsInputBrief {
        request_id: open.request_id,
        reason: open.payload.reason,
        title: open.payload.title,
        expires_at: open.payload.expires_at,
    })
}

/// What one tail read of an active run's direct-agent shard gives the run list: the open question (`needsInput`), folded, and the time
/// of the newest event (the run's last activity).
pub(crate) async fn active_tail<S: Store>(
    store: &S,
    run_id: &str,
) -> Result<(Option<NeedsInputBrief>, Option<String>), StoreError> {
    let events = store
        .latest_events(run_id, direct_shard(), TAIL_EVENTS)
        .await?;
    let newest = events.last().map(|e| e.ts.clone());
    Ok((brief_of(&events), newest))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use harness_protocol::api::v1::InputAnswer;
    use harness_protocol::direct_input::InstructOrigin;
    use harness_protocol::envelope::SCHEMA_VERSION;
    use serde_json::{Value, json};

    const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
    const REQ: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";

    fn question() -> Value {
        json!({"turn": 1, "kind": "needs-input", "requestId": REQ, "reason": "engine_stop", "title": "t",
               "blocking": true, "options": ["finish"], "canContinue": true, "waitS": 1800,
               "expiresAt": "2026-10-15T14:11:07Z"})
    }

    fn event(f: impl FnOnce(&mut Envelope)) -> Envelope {
        let mut e = Envelope {
            schema_version: SCHEMA_VERSION,
            event_id: REQ.into(),
            run_id: RUN.into(),
            task_id: None,
            agent_id: Some("direct".into()),
            seq: 1,
            ts: "2026-10-15T13:41:07Z".into(),
            event_type: EventType::TurnProgress,
            authority: Authority::Agent,
            payload: Payload::Inline { value: question() },
        };
        f(&mut e);
        e
    }

    fn refused(r: Result<NeedsInputPayload, Reply>) -> (u16, String) {
        let reply = r.unwrap_err();
        (reply.0.status, reply.0.body)
    }

    #[test]
    fn a_question_is_admitted_only_as_the_blocking_needs_input_event_of_this_run() {
        assert_eq!(admit(RUN, Some(&event(|_| {}))).unwrap().request_id, REQ);
        let wrong: Vec<Envelope> = vec![
            event(|e| e.run_id = "01K5ZQ4T8M3N6P7R9S0V1W2X4Z".into()),
            event(|e| e.event_type = EventType::ToolSummary),
            event(|e| e.authority = Authority::Harness),
            event(|e| e.agent_id = Some("planner".into())),
            event(|e| {
                e.payload = Payload::ClaimCheck {
                    s3_uri: "s3://b/k".into(),
                    sha256: "ab".repeat(32),
                    size_bytes: 1,
                }
            }),
            event(|e| {
                e.payload = Payload::Inline {
                    value: json!({"kind": "needs-input"}),
                }
            }),
            event(|e| {
                let mut v = question();
                v["kind"] = json!("input-received");
                e.payload = Payload::Inline { value: v };
            }),
            event(|e| {
                let mut v = question();
                v["blocking"] = json!(false);
                e.payload = Payload::Inline { value: v };
            }),
            event(|e| {
                let mut v = question();
                v["requestId"] = json!("01M5B8T4W2N6QJ3C7D9EH1FZ0A");
                e.payload = Payload::Inline { value: v };
            }),
        ];
        for (n, e) in wrong.iter().enumerate() {
            let (status, body) = refused(admit(RUN, Some(e)));
            assert_eq!(status, 404, "case {n}: {body}");
            assert!(body.contains("no-such-request"), "case {n}: {body}");
        }
        assert_eq!(refused(admit(RUN, None)).0, 404);
    }

    #[test]
    fn only_an_instruct_control_is_an_answer() {
        let cancel = PendingControl::Cancel;
        assert_eq!(stored_action(&cancel), None);
        let answer = InputAnswer {
            request_id: REQ.into(),
            action: InstructAction::Finish,
            text: None,
            model: None,
        };
        assert!(!same_answer(&cancel, &answer));
        let instruct = PendingControl::Instruct {
            request_id: REQ.into(),
            action: InstructAction::Finish,
            text: None,
            model: None,
            origin: InstructOrigin::User,
        };
        assert_eq!(stored_action(&instruct), Some(InstructAction::Finish));
        assert!(same_answer(&instruct, &answer));
    }

    #[test]
    fn an_acknowledgement_is_a_direct_agent_progress_event_naming_the_request() {
        let ack = |f: &dyn Fn(&mut Envelope)| {
            vec![event(|e| {
                e.payload = Payload::Inline {
                    value: json!({"kind": "input-received", "requestId": REQ}),
                };
                f(e);
            })]
        };
        assert!(acknowledged(&ack(&|_| {}), REQ));
        assert!(!acknowledged(
            &ack(&|e| e.agent_id = Some("planner".into())),
            REQ
        ));
        assert!(!acknowledged(
            &ack(&|e| e.event_type = EventType::ToolSummary),
            REQ
        ));
        assert!(!acknowledged(
            &ack(&|e| {
                e.payload = Payload::ClaimCheck {
                    s3_uri: "s3://b/k".into(),
                    sha256: "ab".repeat(32),
                    size_bytes: 1,
                }
            }),
            REQ
        ));
        assert!(!acknowledged(
            &ack(&|e| e.payload = Payload::Inline {
                value: json!({"requestId": REQ})
            }),
            REQ
        ));
        assert!(!acknowledged(
            &ack(&|e| e.payload = Payload::Inline {
                value: json!({"kind": "input-received"})
            }),
            REQ
        ));
    }

    #[test]
    fn the_catalog_check_names_the_refusal_code() {
        assert!(check_model("us.anthropic.claude-opus-5-5").is_ok());
        assert!(check_model("global.anthropic.claude-opus-5-5").is_ok());
        let (status, body) = {
            let r = check_model("nope").unwrap_err();
            (r.0.status, r.0.body)
        };
        assert_eq!(status, 422);
        assert!(body.contains("unknown-model"), "{body}");
    }
}
