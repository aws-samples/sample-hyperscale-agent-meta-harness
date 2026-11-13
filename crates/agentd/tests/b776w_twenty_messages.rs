//! follow-up (limits audit row D09): twenty messages may wait for a running Cline. agentd keeps at most `queue_cap` hand-outs, and the
//! rest stay eligible on the server and come again, so all twenty must reach the engine, one prompt each, in the order the server gave, with no
//! two prompts overlapping.
#![allow(clippy::unwrap_used)]
#![cfg(target_os = "linux")]

mod common;
mod direct_messages_world;
mod direct_support;

use agentd::direct::messages::{Mode, Settings, State};
use direct_messages_world::*;
use harness_protocol::messages::MAX_OUTSTANDING_MESSAGES;

#[tokio::test(flavor = "multi_thread")]
async fn b776w_twenty_waiting_messages_are_all_delivered_one_prompt_each_in_the_order_they_were_sent()
 {
    let waiting = MAX_OUTSTANDING_MESSAGES;
    assert_eq!(waiting, 20);
    let w = World::start(&directive(r#"{"work_ms": 1500}"#), &[], quick(), &pid_dir()).await;
    let run = w.run();
    w.pump();
    w.until("the first prompt to be open", 20, |w| {
        w.count("prompt-open") == 1
    })
    .await;
    for i in 1..=waiting {
        w.send(
            &format!("m{i:02}"),
            &format!("message {i:02} @fake{{\"say\": \"N{i:02}\"}}"),
            Mode::Next,
        )
        .await;
    }
    // TRIGGER PRESENT: all twenty waited behind the first prompt, so every one of them was pending at once
    assert_eq!(w.count("prompt-end"), 0, "the first prompt was still open");
    run.await.unwrap();
    w.assert_serial();
    let got = w.d.prompts_received();
    assert_eq!(got.len(), 21, "the task and twenty messages: {got:?}");
    for i in 1..=waiting {
        assert!(
            got[i as usize].starts_with(&format!("message {i:02}")),
            "prompt {i}: {:?}",
            got[i as usize]
        );
        assert_eq!(
            w.store.state(&format!("m{i:02}")),
            Some(State::Delivered { turn: i + 1 }),
            "message {i}"
        );
    }
    assert!(!w.store.admission_open());
}

#[test]
fn b776w_the_hand_out_queue_of_agentd_holds_every_message_that_may_wait() {
    // a hand-out beyond the queue is not lost (the server offers it again), but a queue as long as the cap never drops one
    assert_eq!(
        Settings::default().queue_cap,
        MAX_OUTSTANDING_MESSAGES as usize
    );
}
