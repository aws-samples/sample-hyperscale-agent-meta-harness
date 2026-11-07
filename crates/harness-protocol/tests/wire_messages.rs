//! Protocol 1.38.0: the wire shapes of a message to a running Cline: the client routes, the hand-out to
//! agentd, the claim and seal calls, the outcome post, the store items and their keys. The golden files in `fixtures/direct-messages/` hold
//! the bytes the writers (control-api, worker-api, agentd, ham) must produce and the readers must accept: each parses into its type and
//! writes back the same JSON.

#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::api::internal::{EventsBatchResponse, PendingControl, split_pending};
use harness_protocol::api::{INTERNAL_ROUTES, V1_ROUTES};
use harness_protocol::messages::{
    Admission, ClaimRequest, ClaimResponse, ClaimedMessage, ConfirmHeld, GENERATION_CLOCK_SKEW_MS,
    MAX_CLAIM_BATCH, MAX_DETAIL_CHARS, MAX_MESSAGE_TEXT_BYTES, MAX_OUTSTANDING_MESSAGES,
    MAX_OWNER_ADMISSIONS_PER_MINUTE, MAX_RUN_ADMISSIONS_PER_MINUTE, MAX_RUN_MESSAGE_BYTES,
    MAX_RUN_MESSAGES, MESSAGE_NEXT_CONTROL, MESSAGE_NOW_CONTROL, MessageCtlItem, MessageInputError,
    MessageItem, MessageMode, MessageNotice, MessageOutcome, MessageResponse, MessageState,
    MessagesPage, OpenMessage, OutcomeRequest, OutcomeResponse, OwnerRateItem, RejectReason,
    SealMode, SealRequest, SealResponse, SendMessageRequest, SendMessageResponse, SendOutcome,
    SessionAck, SessionReport, validate_send,
};
use harness_protocol::store::{Keyed, StoreItem, keys};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const MSG: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
const MSG2: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT2P";
const GEN: &str = "01M5A0000000000000000000GE";
const OWNER: &str = "arn:aws:sts::700000000011:assumed-role/Admin/testuser";

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/direct-messages")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The file's bytes equal the pretty JSON of its type (the order of the members is the order of the struct).
fn assert_bytes_round_trip<T: Serialize + for<'de> Deserialize<'de>>(name: &str) -> T {
    let text = fixture(name);
    let typed: T = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
    let mut again = serde_json::to_string_pretty(&typed).unwrap();
    again.push('\n');
    assert_eq!(again, text, "{name} does not write back byte for byte");
    typed
}

fn request(text: &str) -> SendMessageRequest {
    SendMessageRequest {
        message_id: MSG.into(),
        text: text.into(),
        mode: MessageMode::Next,
        confirms: None,
    }
}

#[test]
fn the_protocol_is_1_38_0() {
    // The message routes, the research route and the stored conversation share the one version of the release.
    assert_eq!(PROTOCOL_VERSION, "1.56.0");
}

#[test]
fn the_wire_words_are_the_ones_of_the_decision_record() {
    let words = |v: &[Value]| -> Vec<String> {
        v.iter().map(|x| x.as_str().unwrap().to_string()).collect()
    };
    let states = [
        MessageState::Pending,
        MessageState::Held,
        MessageState::Dispatching,
        MessageState::Delivered,
        MessageState::Withdrawn,
        MessageState::Rejected,
        MessageState::Superseded,
        MessageState::Failed,
        MessageState::DeliveryUnknown,
    ];
    let got: Vec<Value> = states
        .iter()
        .map(|s| serde_json::to_value(s).unwrap())
        .collect();
    assert_eq!(
        words(&got),
        [
            "pending",
            "held",
            "dispatching",
            "delivered",
            "withdrawn",
            "rejected",
            "superseded",
            "failed",
            "delivery-unknown"
        ]
    );
    for s in states {
        assert_eq!(serde_json::to_value(s).unwrap(), json!(s.word()));
    }
    let reasons = [
        RejectReason::RunSealed,
        RejectReason::RunCancelled,
        RejectReason::EngineBusy,
        RejectReason::NoMessageControl,
        RejectReason::ContinuationLimit,
        RejectReason::Limit,
    ];
    let got: Vec<Value> = reasons
        .iter()
        .map(|s| serde_json::to_value(s).unwrap())
        .collect();
    assert_eq!(
        words(&got),
        [
            "run-sealed",
            "run-cancelled",
            "engine-busy",
            "no-message-control",
            "continuation-limit",
            "limit"
        ]
    );
    assert_eq!(serde_json::to_value(MessageMode::Next).unwrap(), "next");
    assert_eq!(serde_json::to_value(MessageMode::Now).unwrap(), "now");
    assert_eq!(MessageMode::default(), MessageMode::Next);
    assert_eq!(serde_json::to_value(SealMode::Normal).unwrap(), "normal");
    assert_eq!(
        serde_json::to_value(SealMode::LimitReached).unwrap(),
        "limit-reached"
    );
    assert_eq!(
        serde_json::to_value(SealMode::Terminal).unwrap(),
        "terminal"
    );
    assert_eq!(serde_json::to_value(Admission::Open).unwrap(), "open");
    assert_eq!(serde_json::to_value(Admission::Closed).unwrap(), "closed");
    assert_eq!(MESSAGE_NEXT_CONTROL, "message.next");
    assert_eq!(MESSAGE_NOW_CONTROL, "message.now");
}

#[test]
fn only_pending_held_and_dispatching_messages_are_open_and_only_the_first_two_can_be_withdrawn() {
    for (s, open, withdrawable, settled) in [
        (MessageState::Pending, true, true, false),
        (MessageState::Held, true, true, false),
        (MessageState::Dispatching, true, false, false),
        (MessageState::Delivered, false, false, true),
        (MessageState::Withdrawn, false, false, true),
        (MessageState::Rejected, false, false, true),
        (MessageState::Superseded, false, false, true),
        (MessageState::Failed, false, false, true),
        (MessageState::DeliveryUnknown, false, false, true),
    ] {
        assert_eq!(s.is_open(), open, "{s:?}");
        assert_eq!(s.can_withdraw(), withdrawable, "{s:?}");
        assert_eq!(s.is_settled(), settled, "{s:?}");
        // a state is open or settled, never both and never neither
        assert!(s.is_open() ^ s.is_settled(), "{s:?}");
    }
}

#[test]
fn the_limits_are_the_initial_values_of_the_decision_record() {
    assert_eq!(MAX_MESSAGE_TEXT_BYTES, 256 * 1024);
    assert_eq!(MAX_OUTSTANDING_MESSAGES, 3);
    assert_eq!(MAX_RUN_MESSAGE_BYTES, 4 * 1024 * 1024);
    assert_eq!(MAX_RUN_MESSAGES, 100);
    assert_eq!(MAX_RUN_ADMISSIONS_PER_MINUTE, 10);
    assert_eq!(MAX_OWNER_ADMISSIONS_PER_MINUTE, 30);
    assert_eq!(MAX_CLAIM_BATCH, 3);
    assert_eq!(MAX_DETAIL_CHARS, 200);
    // an unseen agentd incarnation whose ULID is older than the current one's by more than this does not replace it
    assert_eq!(GENERATION_CLOCK_SKEW_MS, 60_000);
}

#[test]
fn a_send_request_is_checked_before_anything_is_read() {
    assert_eq!(validate_send(&request("hello")), Ok(()));
    let mut bad_id = request("hello");
    bad_id.message_id = "not-a-ulid".into();
    assert_eq!(validate_send(&bad_id), Err(MessageInputError::BadId));
    assert_eq!(
        validate_send(&request("   \n")),
        Err(MessageInputError::BlankText)
    );
    assert_eq!(
        validate_send(&request("")),
        Err(MessageInputError::BlankText)
    );
    let at_limit = "a".repeat(MAX_MESSAGE_TEXT_BYTES);
    assert_eq!(validate_send(&request(&at_limit)), Ok(()));
    let over = "a".repeat(MAX_MESSAGE_TEXT_BYTES + 1);
    assert!(matches!(
        validate_send(&request(&over)),
        Err(MessageInputError::TextTooLong { .. })
    ));
    // the limit counts bytes: 65,536 four-byte characters are exactly the limit, one more is over it
    let four = "\u{1f600}".repeat(MAX_MESSAGE_TEXT_BYTES / 4);
    assert_eq!(four.len(), MAX_MESSAGE_TEXT_BYTES);
    assert_eq!(validate_send(&request(&four)), Ok(()));
    let over_emoji = "\u{1f600}".repeat(MAX_MESSAGE_TEXT_BYTES / 4 + 1);
    assert!(matches!(
        validate_send(&request(&over_emoji)),
        Err(MessageInputError::TextTooLong { .. })
    ));
    // a confirmation names a message (a ULID) and a session (not blank)
    let mut confirm = request("hello");
    confirm.confirms = Some(ConfirmHeld {
        message_id: "x".into(),
        epoch: "sess-3".into(),
    });
    assert_eq!(validate_send(&confirm), Err(MessageInputError::BadConfirm));
    confirm.confirms = Some(ConfirmHeld {
        message_id: MSG2.into(),
        epoch: " ".into(),
    });
    assert_eq!(validate_send(&confirm), Err(MessageInputError::BadConfirm));
    confirm.confirms = Some(ConfirmHeld {
        message_id: MSG.into(),
        epoch: "sess-3".into(),
    });
    assert_eq!(
        validate_send(&confirm),
        Err(MessageInputError::BadConfirm),
        "a message cannot confirm itself"
    );
    confirm.confirms = Some(ConfirmHeld {
        message_id: MSG2.into(),
        epoch: "sess-3".into(),
    });
    assert_eq!(validate_send(&confirm), Ok(()));
}

#[test]
fn a_send_request_refuses_a_member_it_does_not_know_and_defaults_to_the_next_turn() {
    let ok: SendMessageRequest =
        serde_json::from_value(json!({"messageId": MSG, "text": "hi"})).unwrap();
    assert_eq!(ok.mode, MessageMode::Next);
    assert_eq!(ok.confirms, None);
    let extra = serde_json::from_value::<SendMessageRequest>(
        json!({"messageId": MSG, "text": "hi", "colour": "blue"}),
    );
    assert!(extra.is_err());
    let extra = serde_json::from_value::<SendMessageRequest>(
        json!({"messageId": MSG, "text": "hi", "priority": 3}),
    );
    assert!(extra.is_err(), "a client cannot set the state");
    let bad_mode = serde_json::from_value::<SendMessageRequest>(
        json!({"messageId": MSG, "text": "hi", "mode": "steer"}),
    );
    assert!(bad_mode.is_err());
}

#[test]
fn the_payload_digest_names_what_a_retry_must_repeat() {
    let a = request("hello");
    assert_eq!(a.payload_sha256(), request("hello").payload_sha256());
    assert_eq!(a.payload_sha256().len(), 64);
    // the id is the key of the message, not part of its payload
    let mut other_id = request("hello");
    other_id.message_id = MSG2.into();
    assert_eq!(a.payload_sha256(), other_id.payload_sha256());
    assert_ne!(a.payload_sha256(), request("hello!").payload_sha256());
    let mut now = request("hello");
    now.mode = MessageMode::Now;
    assert_ne!(a.payload_sha256(), now.payload_sha256());
    let mut confirm = request("hello");
    confirm.confirms = Some(ConfirmHeld {
        message_id: MSG2.into(),
        epoch: "s".into(),
    });
    assert_ne!(a.payload_sha256(), confirm.payload_sha256());
    let mut other_epoch = confirm.clone();
    other_epoch.confirms.as_mut().unwrap().epoch = "t".into();
    assert_ne!(confirm.payload_sha256(), other_epoch.payload_sha256());
    // fields cannot slide into each other
    let mut slide = request("a\nb");
    let mut slide2 = request("b");
    slide.mode = MessageMode::Next;
    slide2.mode = MessageMode::Next;
    assert_ne!(slide.payload_sha256(), slide2.payload_sha256());
}

#[test]
fn the_client_golden_files_round_trip() {
    let req: SendMessageRequest = assert_bytes_round_trip("send-request.json");
    assert_eq!(req.message_id, MSG);
    assert_eq!(req.mode, MessageMode::Next);
    let confirm: SendMessageRequest = assert_bytes_round_trip("send-request-confirm.json");
    assert_eq!(confirm.confirms.as_ref().unwrap().message_id, MSG2);
    assert_eq!(confirm.mode, MessageMode::Now);
    let sent: SendMessageResponse = assert_bytes_round_trip("send-response.json");
    assert_eq!(sent.outcome, SendOutcome::Accepted);
    assert_eq!(sent.message.state, MessageState::Pending);
    let again: SendMessageResponse = assert_bytes_round_trip("send-response-already.json");
    assert_eq!(again.outcome, SendOutcome::AlreadyAccepted);
    let delivered: MessageResponse = assert_bytes_round_trip("message-delivered.json");
    assert_eq!(delivered.message.state, MessageState::Delivered);
    assert_eq!(delivered.message.delivered_turn, Some(4));
    let rejected: MessageResponse = assert_bytes_round_trip("message-rejected.json");
    assert_eq!(rejected.message.reason, Some(RejectReason::RunSealed));
    let held: MessageResponse = assert_bytes_round_trip("message-held.json");
    assert_eq!(held.message.state, MessageState::Held);
    assert_eq!(held.message.epoch_no, 2);
    let page: MessagesPage = assert_bytes_round_trip("messages-page.json");
    assert_eq!(page.messages.len(), 3);
    assert_eq!(page.admission, Admission::Open);
    assert_eq!(page.epoch_no, 3);
    assert_eq!(page.watermark, 3);
}

#[test]
fn the_agentd_golden_files_round_trip() {
    let notice: MessageNotice = assert_bytes_round_trip("notice.json");
    assert_eq!(notice.message_id, MSG);
    assert_eq!(notice.mode, MessageMode::Now);
    let control: PendingControl = assert_bytes_round_trip("pending-control-message.json");
    assert!(matches!(&control, PendingControl::Message(n) if n.message_id == MSG));
    let report: SessionReport = assert_bytes_round_trip("session-report.json");
    assert_eq!(report.generation, GEN);
    assert_eq!(report.number, 3, "the third session of this incarnation");
    let ack: SessionAck = assert_bytes_round_trip("session-ack.json");
    assert_eq!((ack.epoch_no, ack.held, ack.delivery_unknown), (3, 1, 1));
    let claim: ClaimRequest = assert_bytes_round_trip("claim-request.json");
    assert_eq!(claim.message_id, None);
    assert_eq!(claim.limit, Some(3));
    let by_id: ClaimRequest = assert_bytes_round_trip("claim-request-by-id.json");
    assert_eq!(by_id.message_id.as_deref(), Some(MSG));
    let claimed: ClaimResponse = assert_bytes_round_trip("claim-response-claimed.json");
    match claimed {
        ClaimResponse::Claimed {
            messages,
            redelivered,
        } => {
            assert!(!redelivered, "a first claim is not a redelivery");
            assert_eq!(messages.len(), 2);
            assert_eq!(messages[0].message_id, MSG);
            assert_eq!(
                messages[0].attempt, 1,
                "the first claim of a message is attempt 1"
            );
        }
        other => panic!("{other:?}"),
    }
    for (name, expect) in [
        ("claim-response-nothing.json", "nothing"),
        ("claim-response-not-pending.json", "not-pending"),
        ("claim-response-refused.json", "refused"),
    ] {
        let r: ClaimResponse = assert_bytes_round_trip(name);
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["outcome"], expect, "{name}");
    }
    let seal: SealRequest = assert_bytes_round_trip("seal-request.json");
    assert_eq!(seal.mode, SealMode::Normal);
    let seal_terminal: SealRequest = assert_bytes_round_trip("seal-request-terminal.json");
    assert_eq!(seal_terminal.mode, SealMode::Terminal);
    let pending: SealResponse = assert_bytes_round_trip("seal-response-pending.json");
    assert!(matches!(pending, SealResponse::Pending { ref messages } if messages.len() == 1));
    let sealed: SealResponse = assert_bytes_round_trip("seal-response-sealed.json");
    assert!(matches!(
        sealed,
        SealResponse::Sealed {
            watermark: 3,
            rejected: 1,
            delivery_unknown: 0
        }
    ));
    for (name, check) in [
        ("outcome-submitted.json", "submitted"),
        ("outcome-failed.json", "failed"),
        ("outcome-released.json", "released"),
        ("outcome-rejected.json", "rejected"),
    ] {
        let r: OutcomeRequest = assert_bytes_round_trip(name);
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["outcome"]["kind"], check, "{name}");
        assert_eq!(
            r.attempt, 1,
            "{name}: an outcome names the claim attempt it answers"
        );
    }
    let resp: OutcomeResponse = assert_bytes_round_trip("outcome-response.json");
    assert_eq!(resp.state, MessageState::Delivered);
}

#[test]
fn the_message_notice_rides_the_pending_list_and_an_older_agentd_skips_it() {
    let batch = json!({"accepted": 2, "pending": [
        {"kind": "cancel"},
        {"kind": "message", "messageId": MSG, "seq": 1, "mode": "next", "epoch": "s1", "bytes": 5, "acceptedAt": "2026-10-16T14:00:00Z"}
    ]});
    let resp: EventsBatchResponse = serde_json::from_value(batch.clone()).unwrap();
    assert_eq!(resp.pending.len(), 2);
    assert!(matches!(&resp.pending[1], PendingControl::Message(n) if n.bytes == 5));
    // what an agentd built before 1.38.0 does with the same list: the item it cannot read is set aside and the cancel is kept
    let items: Vec<Value> = batch["pending"].as_array().unwrap().clone();
    let (ok, skipped) = split_pending(items);
    assert_eq!(ok.len(), 2, "this build reads both");
    assert!(skipped.is_empty());
    let (ok, skipped) = split_pending(vec![
        json!({"kind": "cancel"}),
        json!({"kind": "message-v9"}),
    ]);
    assert_eq!((ok.len(), skipped.len()), (1, 1));
}

fn item() -> MessageItem {
    serde_json::from_str(&fixture("item-message.json")).unwrap()
}

fn ctl() -> MessageCtlItem {
    serde_json::from_str(&fixture("item-ctl.json")).unwrap()
}

#[test]
fn the_store_items_round_trip_and_have_the_keys_of_the_decision_record() {
    let i: MessageItem = assert_bytes_round_trip("item-message.json");
    assert_eq!(i.key().pk, format!("RUN#{RUN}"));
    assert_eq!(i.key().sk, format!("MSG#{MSG}"));
    assert_eq!(keys::message_sk(MSG), format!("MSG#{MSG}"));
    let c: MessageCtlItem = assert_bytes_round_trip("item-ctl.json");
    assert_eq!(c.key().pk, format!("RUN#{RUN}"));
    assert_eq!(c.key().sk, "MSGCTL");
    assert_eq!(keys::MESSAGE_CTL, "MSGCTL");
    let r: OwnerRateItem = assert_bytes_round_trip("item-owner-rate.json");
    assert_eq!(r.key().pk, format!("MSGRATE#{OWNER}"));
    assert_eq!(r.key().sk, "000029869322");
    assert_eq!(keys::owner_rate_sk(29_869_322), "000029869322");
    // a prefix read of the messages never returns the control item, and one message id never reads as another
    assert!(!keys::MESSAGE_CTL.starts_with(keys::MESSAGE_PREFIX));
    assert!(keys::message_sk(MSG).starts_with(keys::MESSAGE_PREFIX));
    // the store enum carries all three
    for v in [
        serde_json::to_value(StoreItem::Message(Box::new(item()))).unwrap(),
        serde_json::to_value(StoreItem::MessageCtl(Box::new(ctl()))).unwrap(),
        serde_json::to_value(StoreItem::OwnerRate(r)).unwrap(),
    ] {
        let back: StoreItem = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(serde_json::to_value(&back).unwrap(), v);
    }
}

#[test]
fn no_message_item_enters_the_sparse_run_list_index() {
    // GSI1 holds every item that has BOTH `ownerPrincipalArn` and `createdAt` (infra data stack): a message that carried both would
    // appear in `GET /v1/runs` as a run. The items use `principalArn` and `acceptedAt`, the way the idempotency claim does.
    for v in [
        serde_json::to_value(item()).unwrap(),
        serde_json::to_value(ctl()).unwrap(),
        serde_json::to_value(
            serde_json::from_str::<OwnerRateItem>(&fixture("item-owner-rate.json")).unwrap(),
        )
        .unwrap(),
    ] {
        let o = v.as_object().unwrap();
        assert!(!o.contains_key("ownerPrincipalArn"), "{v}");
        assert!(!o.contains_key("createdAt"), "{v}");
    }
}

#[test]
fn the_client_view_leaves_out_what_only_the_server_needs() {
    let i = item();
    let v = serde_json::to_value(i.view()).unwrap();
    let o = v.as_object().unwrap();
    for hidden in [
        "principalArn",
        "payloadSha256",
        "claimedBy",
        "version",
        "runId",
    ] {
        // the view keeps the run id out of its members (the route names it); the others are server business
        if hidden == "runId" {
            continue;
        }
        assert!(!o.contains_key(hidden), "{hidden} leaked into the view");
    }
    assert_eq!(o["messageId"], MSG);
    assert_eq!(o["text"], i.text);
    assert_eq!(o["state"], serde_json::to_value(i.state).unwrap());
    assert_eq!(o["epochNo"], i.epoch_no);
}

#[test]
fn the_control_item_counts_what_counts_toward_the_cap() {
    let mut c = ctl();
    c.open = vec![
        open(MSG, 1, MessageState::Pending),
        open(MSG2, 2, MessageState::Held),
        open("01M5A2R7K3C9XJ4D8E6FQ0WT3Q", 3, MessageState::Dispatching),
    ];
    assert_eq!(c.outstanding(), 3);
    assert_eq!(c.pending().count(), 1);
    c.open.remove(2);
    assert_eq!(c.outstanding(), 2);
    assert!(c.find(MSG).is_some());
    assert!(c.find("nope").is_none());
}

fn open(id: &str, seq: u32, state: MessageState) -> OpenMessage {
    OpenMessage {
        message_id: id.into(),
        seq,
        mode: MessageMode::Next,
        state,
        epoch: Some("s1".into()),
        bytes: 10,
        accepted_at: "2026-10-16T14:00:00Z".into(),
    }
}

#[test]
fn the_routes_are_listed_with_their_operation_ids() {
    let find = |routes: &'static [harness_protocol::api::Route], op: &str| {
        routes.iter().find(|r| r.operation_id == op).copied()
    };
    let expect = [
        ("sendMessage", "POST", "/v1/runs/{runId}/messages"),
        ("listMessages", "GET", "/v1/runs/{runId}/messages"),
        ("getMessage", "GET", "/v1/runs/{runId}/messages/{messageId}"),
        (
            "withdrawMessage",
            "POST",
            "/v1/runs/{runId}/messages/{messageId}/withdraw",
        ),
    ];
    for (op, method, path) in expect {
        let r = find(V1_ROUTES, op).unwrap_or_else(|| panic!("{op} is not a /v1 route"));
        assert_eq!((r.method, r.path), (method, path), "{op}");
    }
    let internal = [
        ("reportMessageSession", "/internal/message-session"),
        ("claimMessages", "/internal/message-claim"),
        ("sealMessages", "/internal/message-seal"),
        ("postMessageOutcome", "/internal/message-outcome"),
    ];
    for (op, path) in internal {
        let r =
            find(INTERNAL_ROUTES, op).unwrap_or_else(|| panic!("{op} is not an /internal route"));
        assert_eq!((r.method, r.path), ("POST", path), "{op}");
    }
}

#[test]
fn a_claimed_message_carries_its_text_and_an_outcome_carries_no_state_word_the_agent_may_not_set() {
    let c = ClaimedMessage {
        message_id: MSG.into(),
        seq: 1,
        mode: MessageMode::Next,
        epoch_no: 2,
        attempt: 3,
        text: "hello".into(),
        accepted_at: "2026-10-16T14:00:00Z".into(),
    };
    assert_eq!(serde_json::to_value(&c).unwrap()["text"], "hello");
    assert_eq!(serde_json::to_value(&c).unwrap()["attempt"], 3);
    // an agent can say submitted, failed, released or rejected; it cannot post "withdrawn", "held" or "delivery-unknown"
    for kind in ["withdrawn", "held", "delivery-unknown", "superseded"] {
        let r = serde_json::from_value::<MessageOutcome>(json!({"kind": kind}));
        assert!(r.is_err(), "{kind}");
    }
    let r: MessageOutcome =
        serde_json::from_value(json!({"kind": "rejected", "reason": "engine-busy"})).unwrap();
    assert_eq!(
        r,
        MessageOutcome::Rejected {
            reason: RejectReason::EngineBusy
        }
    );
    // a reason only the server may set is refused from an agent
    assert!(RejectReason::EngineBusy.agent_may_set());
    assert!(RejectReason::ContinuationLimit.agent_may_set());
    for r in [
        RejectReason::RunSealed,
        RejectReason::RunCancelled,
        RejectReason::NoMessageControl,
        RejectReason::Limit,
    ] {
        assert!(!r.agent_may_set(), "{r:?}");
    }
}

#[test]
fn the_payload_digest_cannot_be_confused_by_a_line_break_inside_a_field() {
    // an epoch is an engine session id and may hold any character: "a\nb" + "c" and "a" + "b\nc" are different payloads
    let mut x = request("c");
    x.confirms = Some(ConfirmHeld {
        message_id: MSG2.into(),
        epoch: "a\nb".into(),
    });
    let mut y = request("b\nc");
    y.confirms = Some(ConfirmHeld {
        message_id: MSG2.into(),
        epoch: "a".into(),
    });
    assert_ne!(x.payload_sha256(), y.payload_sha256());
    // and the text that carries the separators of the encoding cannot stand for a mode or a confirmation
    let plain_text = request("next\n\n\nhello");
    assert_ne!(
        plain_text.payload_sha256(),
        request("hello").payload_sha256()
    );
}

#[test]
fn a_control_item_remembers_the_incarnations_it_replaced_and_the_session_number_of_the_current_one()
{
    let c = ctl();
    assert_eq!(c.session_number, 3);
    assert_eq!(c.retired.len(), 1);
    assert_ne!(
        c.retired[0],
        c.generation.clone().unwrap(),
        "the current incarnation is not retired"
    );
    let m = item();
    assert_eq!(m.attempt, 1, "a delivered message was claimed once");
}

#[test]
fn the_message_arrays_of_the_claim_and_seal_answers_are_bounded_in_the_schema() {
    // the scanner rule CKV_OPENAPI_21 flags an array without maxItems: a claim takes at most MAX_CLAIM_BATCH messages and a seal
    // returns at most MAX_OUTSTANDING_MESSAGES notices
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas/openapi-internal.json");
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let schemas = &doc["components"]["schemas"];
    assert_eq!(
        schemas["ClaimResponse"]["oneOf"][0]["properties"]["messages"]["maxItems"],
        json!(u64::from(MAX_CLAIM_BATCH))
    );
    assert_eq!(
        schemas["SealResponse"]["oneOf"][0]["properties"]["messages"]["maxItems"],
        json!(u64::from(MAX_OUTSTANDING_MESSAGES))
    );
}
