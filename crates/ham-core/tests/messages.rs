//! The client side of message control: the id the client mints, the confirmation of a held message, the words of a
//! message's status and of a refusal, and the choice between an answer, a message and a follow-up.

#![allow(clippy::unwrap_used)]

use ham_core::messages::{
    ConfirmError, Refusal, SendRoute, confirmation, explain_refusal, is_confirmable,
    is_withdrawable, plain, route_for, status_line, ulid_from,
};
use harness_protocol::api::v1::NeedsInputBrief;
use harness_protocol::common::is_ulid;
use harness_protocol::messages::{
    MessageMode, MessageState, MessageView, RejectReason, validate_send,
};

fn view(state: MessageState) -> MessageView {
    MessageView {
        message_id: "01M5A2R7K3C9XJ4D8E6FQ0WT1N".into(),
        seq: 2,
        mode: MessageMode::Next,
        state,
        text: "use the same style as the other modules".into(),
        epoch: Some("sess-2".into()),
        epoch_no: 2,
        reason: None,
        detail: None,
        delivered_turn: None,
        confirms: None,
        superseded_by: None,
        accepted_at: "2026-10-16T14:02:11Z".into(),
        updated_at: "2026-10-16T14:02:11Z".into(),
        history: vec![],
    }
}

#[test]
fn a_minted_id_is_a_ulid_that_sorts_by_time_and_carries_its_randomness() {
    let a = ulid_from(1_792_159_331_000, [1; 10]);
    let b = ulid_from(1_792_159_331_001, [0; 10]);
    let c = ulid_from(1_792_159_332_000, [255; 10]);
    for id in [&a, &b, &c] {
        assert!(is_ulid(id), "{id}");
        assert_eq!(id.len(), 26);
    }
    assert!(a < b && b < c, "{a} {b} {c}");
    // the same inputs give the same id; another random byte gives another
    assert_eq!(a, ulid_from(1_792_159_331_000, [1; 10]));
    let mut r = [1u8; 10];
    r[9] = 2;
    assert_ne!(a, ulid_from(1_792_159_331_000, r));
    // the first moment and the last the format holds
    assert_eq!(ulid_from(0, [0; 10]), "00000000000000000000000000");
    let last = ulid_from((1 << 48) - 1, [255; 10]);
    assert_eq!(last, "7ZZZZZZZZZZZZZZZZZZZZZZZZZ");
    assert!(is_ulid(&last));
    // a time past 48 bits is cut to 48 bits, not a panic and not a longer id
    assert_eq!(ulid_from(1 << 48, [0; 10]), "00000000000000000000000000");
}

#[test]
fn a_plain_request_passes_the_servers_own_check() {
    let id = ulid_from(1_792_159_331_000, [7; 10]);
    let r = plain(id.clone(), "hello", MessageMode::Now);
    assert_eq!(
        (r.message_id.as_str(), r.text.as_str(), r.mode),
        (id.as_str(), "hello", MessageMode::Now)
    );
    assert!(r.confirms.is_none());
    assert_eq!(validate_send(&r), Ok(()));
    // the text goes as typed: no trimming, no wrapping
    let r = plain(id, "  hello\n", MessageMode::Next);
    assert_eq!(r.text, "  hello\n");
}

#[test]
fn a_held_or_unknown_message_is_confirmed_by_a_copy_that_names_it_and_the_session_shown() {
    let new_id = ulid_from(1_792_159_340_000, [9; 10]);
    for s in [MessageState::Held, MessageState::DeliveryUnknown] {
        assert!(is_confirmable(&view(s)), "{s:?}");
        let c = confirmation(new_id.clone(), &view(s), "sess-3").unwrap();
        assert_eq!(c.text, "use the same style as the other modules");
        assert_eq!(c.mode, MessageMode::Next);
        let ref_ = c.confirms.as_ref().unwrap();
        assert_eq!(
            (ref_.message_id.as_str(), ref_.epoch.as_str()),
            ("01M5A2R7K3C9XJ4D8E6FQ0WT1N", "sess-3")
        );
        assert_eq!(validate_send(&c), Ok(()));
    }
    // the mode of the original is kept: a held interrupt is confirmed as an interrupt
    let mut now = view(MessageState::Held);
    now.mode = MessageMode::Now;
    assert_eq!(
        confirmation(new_id.clone(), &now, "sess-3").unwrap().mode,
        MessageMode::Now
    );
    // nothing else can be confirmed
    for s in [
        MessageState::Pending,
        MessageState::Dispatching,
        MessageState::Delivered,
        MessageState::Withdrawn,
        MessageState::Rejected,
        MessageState::Superseded,
        MessageState::Failed,
    ] {
        assert!(!is_confirmable(&view(s)), "{s:?}");
        assert_eq!(
            confirmation(new_id.clone(), &view(s), "sess-3"),
            Err(ConfirmError::NotConfirmable(s))
        );
    }
}

#[test]
fn only_a_pending_or_held_message_can_be_withdrawn() {
    for (s, ok) in [
        (MessageState::Pending, true),
        (MessageState::Held, true),
        (MessageState::Dispatching, false),
        (MessageState::Delivered, false),
        (MessageState::DeliveryUnknown, false),
    ] {
        assert_eq!(is_withdrawable(&view(s)), ok, "{s:?}");
    }
}

#[test]
fn every_status_has_its_words_and_an_unknown_delivery_is_never_called_not_delivered() {
    let line = |v: &MessageView, now: u32| status_line(v, now);
    assert_eq!(
        line(&view(MessageState::Pending), 2),
        "waiting for Cline to finish its turn"
    );
    let mut interrupt = view(MessageState::Pending);
    interrupt.mode = MessageMode::Now;
    assert_eq!(line(&interrupt, 2), "waiting to interrupt Cline");
    assert_eq!(
        line(&view(MessageState::Dispatching), 2),
        "Cline is taking it now"
    );
    let mut delivered = view(MessageState::Delivered);
    delivered.delivered_turn = Some(4);
    assert_eq!(line(&delivered, 2), "delivered in turn 4");
    assert_eq!(line(&view(MessageState::Delivered), 2), "delivered");
    assert_eq!(line(&view(MessageState::Withdrawn), 2), "withdrawn");
    assert_eq!(
        line(&view(MessageState::Superseded), 2),
        "replaced by a confirmed copy"
    );
    assert_eq!(
        line(&view(MessageState::Held), 3),
        "held: written to session 2, now session 3; confirm to send it to the new session"
    );
    let unknown = line(&view(MessageState::DeliveryUnknown), 3);
    assert!(unknown.contains("may have reached Cline"), "{unknown}");
    assert!(!unknown.contains("not delivered"), "{unknown}");
    let mut failed = view(MessageState::Failed);
    failed.detail = Some("the engine closed its input".into());
    assert_eq!(
        line(&failed, 2),
        "could not be sent: the engine closed its input"
    );
    for (r, text) in [
        (RejectReason::RunSealed, "not sent: the run was ending"),
        (RejectReason::RunCancelled, "not sent: you stopped the run"),
        (
            RejectReason::EngineBusy,
            "not sent: Cline's input queue was full",
        ),
        (
            RejectReason::NoMessageControl,
            "not sent: this run cannot take messages",
        ),
        (
            RejectReason::ContinuationLimit,
            "not sent: the run reached its limit of turns after done",
        ),
        (
            RejectReason::Limit,
            "not sent: the run reached its message limit",
        ),
    ] {
        let mut v = view(MessageState::Rejected);
        v.reason = Some(r);
        assert_eq!(line(&v, 2), text);
    }
    // a rejected message without a reason still reads as not sent
    assert_eq!(line(&view(MessageState::Rejected), 2), "not sent");
}

#[test]
fn a_status_line_never_carries_a_control_character_from_the_servers_detail() {
    let mut failed = view(MessageState::Failed);
    failed.detail = Some("bad\u{7} thing\nnext line".into());
    let l = status_line(&failed, 2);
    assert!(!l.chars().any(|c| c.is_control()), "{l:?}");
}

#[test]
fn every_refusal_of_the_send_route_has_words_and_says_what_happens_to_the_text() {
    let r = |status: u16, code: &str| explain_refusal(status, code, "server words");
    let old = r(409, "no-message-control");
    assert_eq!(
        old.text,
        "Cline is not waiting for an answer; /cancel stops the run and keeps the work"
    );
    assert!(old.restore_text && old.definitive);
    let q = r(409, "question-open");
    assert!(q.text.contains("answer"), "{}", q.text);
    assert!(q.restore_text && q.definitive);
    for code in ["run-finished", "no-live-session"] {
        let x = r(409, code);
        assert!(x.text.contains("follow-up"), "{code}: {}", x.text);
        assert!(x.restore_text);
    }
    let sealed = r(409, "run-sealed");
    assert!(sealed.text.contains("ending"), "{}", sealed.text);
    assert!(sealed.restore_text);
    for code in ["too-many-pending", "rate-limited", "run-limit"] {
        assert!(
            r(if code == "rate-limited" { 429 } else { 409 }, code).restore_text,
            "{code}"
        );
    }
    for code in ["epoch-changed", "not-held", "confirm-mismatch"] {
        let x = r(409, code);
        assert!(
            x.text.contains("confirm") || x.text.contains("session"),
            "{code}: {}",
            x.text
        );
    }
    // the server's own words for what the client cannot name
    let other = r(400, "bad-input");
    assert_eq!(other.text, "server words");
    assert!(other.restore_text && other.definitive);
    // a refusal that is a 404 reads as the run being gone
    assert!(r(404, "not-found").text.contains("no longer"));
}

#[test]
fn an_uncertain_send_keeps_its_id_and_does_not_restore_the_text_as_a_fresh_message() {
    for status in [500u16, 502, 503, 504, 0] {
        let x = explain_refusal(status, "unavailable", "retry");
        assert!(!x.definitive, "{status}");
        assert!(
            !x.restore_text,
            "{status}: the id is kept and the send is reconciled by it"
        );
        assert!(x.text.contains("sending"), "{}", x.text);
    }
    // a 429 is a definitive answer: nothing was stored
    assert!(explain_refusal(429, "rate-limited", "x").definitive);
}

#[test]
fn enter_answers_an_open_question_sends_a_message_to_a_working_run_and_prepares_a_follow_up_for_a_finished_one()
 {
    let q = NeedsInputBrief {
        request_id: "01M5A2R7K3C9XJ4D8E6FQ0WT2P".into(),
        reason: "engine_stop".into(),
        title: "Cline stopped its own prompt".into(),
        expires_at: None,
    };
    assert_eq!(
        route_for(true, Some(&q)),
        SendRoute::Answer {
            request_id: "01M5A2R7K3C9XJ4D8E6FQ0WT2P".into()
        }
    );
    assert_eq!(route_for(true, None), SendRoute::Message);
    assert_eq!(route_for(false, None), SendRoute::FollowUp);
    assert_eq!(
        route_for(false, Some(&q)),
        SendRoute::FollowUp,
        "a finished run has no open question"
    );
    let _ = Refusal {
        text: String::new(),
        restore_text: false,
        definitive: false,
    };
}
