//! The bell and the terminal notification for "Cline needs you". A run that starts to wait for the user rings the bell once
//! (one bell for a batch of runs) and, when the option is on and the window is not focused, sends one OSC 9 notification. A run that
//! keeps waiting does not ring again; one that is answered and waits again does. The bytes are built only from fixed words and a
//! run id that passes the ULID check: nothing the engine or a transcript wrote can reach the terminal through this module.

#![allow(clippy::unwrap_used)]

use ham_core::alert::{Alert, Alerts, bell_bytes, notification_bytes, notification_bytes_tmux};
use proptest::prelude::*;

const A: &str = "01M5287J0000000000000000AB";
const B: &str = "01M5287J0000000000000000AC";
const C: &str = "01M5287J0000000000000000AD";

fn bytes(a: &Alert) -> Vec<u8> {
    let mut out = Vec::new();
    a.write_to(&mut out).unwrap();
    out
}

#[test]
fn a_run_that_starts_waiting_rings_the_bell_once() {
    let mut al = Alerts::new();
    assert!(al.observe(&[]).is_empty());
    let got = al.observe(&[A]);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].run_id, A);
    assert_eq!(bytes(&got[0]), b"\x07");
    // Still waiting at the next poll: nothing new.
    assert!(al.observe(&[A]).is_empty());
    assert!(al.observe(&[A]).is_empty());
}

#[test]
fn a_run_that_is_answered_and_waits_again_rings_again() {
    let mut al = Alerts::new();
    assert_eq!(al.observe(&[A]).len(), 1);
    assert!(al.observe(&[]).is_empty(), "answered");
    assert_eq!(al.observe(&[A]).len(), 1, "a new question");
}

#[test]
fn a_batch_of_new_waiting_runs_rings_one_bell_and_names_each_run() {
    let mut al = Alerts::new();
    al.set_notify(true);
    al.set_focus(false);
    let got = al.observe(&[A, B, C]);
    assert_eq!(got.len(), 3);
    assert_eq!(
        got.iter().filter(|a| a.bell).count(),
        1,
        "one bell for the batch"
    );
    assert!(got[0].bell);
    assert!(
        got.iter().all(|a| a.notify),
        "each run has its own notification"
    );
    let ids: Vec<&str> = got.iter().map(|a| a.run_id.as_str()).collect();
    assert_eq!(ids, [A, B, C]);
}

#[test]
fn the_first_read_can_be_seeded_without_a_sound() {
    let mut al = Alerts::new();
    al.seed(&[A, B]);
    assert!(al.observe(&[A, B]).is_empty());
    let got = al.observe(&[A, B, C]);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].run_id, C);
}

#[test]
fn the_bell_can_be_switched_off() {
    let mut al = Alerts::new();
    al.set_bell(false);
    let got = al.observe(&[A]);
    assert_eq!(got.len(), 1);
    assert!(!got[0].bell);
    assert!(bytes(&got[0]).is_empty());
}

#[test]
fn the_notification_needs_the_option_and_an_unfocused_window() {
    let mut al = Alerts::new();
    // Option off: bell only, whatever the focus.
    al.set_focus(false);
    assert!(!al.observe(&[A])[0].notify);
    al.observe(&[]);
    // Option on, window focused: bell only.
    al.set_notify(true);
    al.set_focus(true);
    assert!(!al.observe(&[A])[0].notify);
    al.observe(&[]);
    // Option on, window not focused: bell and notification.
    al.set_focus(false);
    let got = al.observe(&[A]);
    assert!(got[0].notify);
    assert_eq!(
        got[0].notification_text().as_deref(),
        Some("ham: Cline needs you (run 01M5287J)")
    );
    assert_eq!(
        bytes(&got[0]),
        b"\x07\x1b]9;ham: Cline needs you (run 01M5287J)\x07"
    );
}

#[test]
fn a_run_id_that_is_not_a_ulid_is_left_out_of_the_text() {
    for bad in [
        "",
        "short",
        "x\u{1b}[2J",
        "01M5287J\u{7}0000000000000000AB",
        "not a ulid at all.......",
        "01M5287J0000000000000000A!",
    ] {
        let n = notification_bytes(bad);
        assert_eq!(n, b"\x1b]9;ham: Cline needs you\x07", "{bad:?}");
    }
    assert_eq!(
        notification_bytes(A),
        b"\x1b]9;ham: Cline needs you (run 01M5287J)\x07"
    );
    assert_eq!(bell_bytes(), b"\x07");
}

#[test]
fn inside_tmux_the_notification_is_wrapped_to_pass_through() {
    // tmux drops an OSC 9 it does not know unless it is wrapped in a DCS "tmux;" with every ESC doubled (and allow-passthrough on).
    assert_eq!(
        notification_bytes_tmux(A),
        b"\x1bPtmux;\x1b\x1b]9;ham: Cline needs you (run 01M5287J)\x07\x1b\\"
    );
    let mut al = Alerts::new();
    al.set_notify(true);
    al.set_focus(false);
    al.set_tmux(true);
    let got = al.observe(&[A]);
    assert_eq!(
        bytes(&got[0]),
        b"\x07\x1bPtmux;\x1b\x1b]9;ham: Cline needs you (run 01M5287J)\x07\x1b\\"
    );
    // The bell is not wrapped.
    let mut al = Alerts::new();
    al.set_tmux(true);
    assert_eq!(bytes(&al.observe(&[A])[0]), b"\x07");
}

#[test]
fn an_alert_built_by_hand_still_writes_only_fixed_words_and_a_checked_id() {
    let a = Alert {
        run_id: "x\u{1b}[2J".into(),
        bell: false,
        notify: true,
        tmux: false,
    };
    assert_eq!(bytes(&a), b"\x1b]9;ham: Cline needs you\x07");
    assert_eq!(
        a.notification_text().as_deref(),
        Some("ham: Cline needs you")
    );
    let none = Alert {
        run_id: A.into(),
        bell: false,
        notify: false,
        tmux: false,
    };
    assert!(bytes(&none).is_empty());
    assert_eq!(none.notification_text(), None);
}

proptest! {
    #[test]
    fn nothing_but_fixed_words_and_a_checked_id_reaches_the_terminal(id in "\\PC{0,40}|[\\x00-\\x7f]{0,40}") {
        let out = notification_bytes(&id);
        // One OSC 9 sequence: ESC ] 9 ; printable ASCII BEL.
        prop_assert!(out.starts_with(b"\x1b]9;"));
        prop_assert!(out.ends_with(b"\x07"));
        let body = &out[4..out.len() - 1];
        prop_assert!(body.iter().all(|b| (0x20..0x7f).contains(b)), "{out:?}");
        prop_assert_eq!(out.iter().filter(|&&b| b == 0x1b).count(), 1);
        prop_assert_eq!(out.iter().filter(|&&b| b == 0x07).count(), 1);
    }

    #[test]
    fn an_observation_never_rings_for_a_run_that_was_already_waiting(
        steps in proptest::collection::vec(proptest::collection::vec(0usize..4, 0..4), 0..12)
    ) {
        let ids = [A, B, C, "01M5287J0000000000000000AE"];
        let mut al = Alerts::new();
        let mut waiting: Vec<&str> = Vec::new();
        for s in steps {
            let now: Vec<&str> = {
                let mut v: Vec<&str> = s.iter().map(|&i| ids[i]).collect();
                v.sort_unstable();
                v.dedup();
                v
            };
            let got = al.observe(&now);
            let want: Vec<&str> = now.iter().copied().filter(|r| !waiting.contains(r)).collect();
            let got_ids: Vec<&str> = got.iter().map(|a| a.run_id.as_str()).collect();
            prop_assert_eq!(got_ids, want);
            waiting = now;
        }
    }
}
