//! Wave 2 (backend lambdas): the text a run takes in messages is counted in the bytes JSON writes for it, so the list of a full run cannot
//! outgrow what one Lambda answer holds (6 MB), whatever characters the messages hold. The count of messages per run stays where it is until the
//! list is paged; the second test pins that the full run still lists inside the budget.

#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::messages::{MAX_RUN_MESSAGE_BYTES, MAX_RUN_MESSAGES};
use message_ledger::Refusal;
use support::*;

/// The most one Lambda answers with, less 5 percent for the envelope.
const ANSWER_BUDGET: usize = 5_900_000;

#[tokio::test]
async fn b776w_message_text_counts_the_bytes_json_writes_for_it() {
    let r = rig();
    // 131,000 quotation marks are 131,000 bytes of text and 262,000 bytes inside JSON.
    let quotes = "\"".repeat(131_000);
    let fit = (MAX_RUN_MESSAGE_BYTES / 262_000) as u32;
    assert_eq!(fit, 16);
    for n in 1..=fit {
        r.clock.set(T0 + i64::from(n / 5) * 60);
        r.admit(n, &quotes).await;
        r.ledger.withdraw(RUN, &mid(n)).await.unwrap();
    }
    r.clock.advance(120);
    let e = r.refused(&send(fit + 1, &quotes)).await;
    assert!(
        matches!(e, Refusal::RunLimit(ref m) if m.contains("4096")),
        "{e:?}"
    );
    // What the run took lists inside one Lambda answer.
    let page = r.ledger.list(RUN).await.unwrap();
    assert_eq!(page.messages.len(), fit as usize);
    let size = serde_json::to_vec(&page).unwrap().len();
    assert!(size < ANSWER_BUDGET, "the list is {size} bytes");
    // Plain text still counts one byte a character: a message of 2,000 letters fits beside the 16.
    r.admit(fit + 2, &"y".repeat(2_000)).await;
}

#[tokio::test]
async fn b776w_a_run_that_took_its_last_message_lists_inside_the_answer_budget() {
    let r = rig();
    let text = "z".repeat(2_000);
    for n in 1..=MAX_RUN_MESSAGES {
        r.clock.set(T0 + i64::from(n / 5) * 60);
        r.admit(n, &text).await;
        r.ledger.withdraw(RUN, &mid(n)).await.unwrap();
    }
    r.clock.advance(120);
    let e = r.refused(&send(MAX_RUN_MESSAGES + 1, "one more")).await;
    assert!(
        matches!(e, Refusal::RunLimit(ref m) if m.contains("follow-up run")),
        "{e:?}"
    );
    let page = r.ledger.list(RUN).await.unwrap();
    assert_eq!(page.messages.len(), MAX_RUN_MESSAGES as usize);
    let size = serde_json::to_vec(&page).unwrap().len();
    assert!(size < ANSWER_BUDGET, "the list is {size} bytes");
}
