//! The hold on the brake inside a round, as the run's events tell it. While a command that the model started earlier is running, the
//! watch takes back the polls that wait for it and leaves their time out of the window; nobody reading the run's events could see that. The
//! watch notes the first time it holds the brake back (`HoldNote::Held`, with the age of the command) and, when no time was credited for the
//! release time, that the hold is over (`HoldNote::Released`, with the time held in all). One note for each change, so a hold that comes and goes
//! at every sample does not flood the log. Pure: the tests move time by hand.
#![allow(clippy::unwrap_used)]

use agentd::direct::goal::watch::{Command, HoldNote, Watch, WatchLimits};
use std::time::{Duration, Instant};

fn limits() -> WatchLimits {
    WatchLimits {
        calls: 30,
        window: Duration::from_secs(20 * 60),
        min_calls: 10,
        who: "Cline",
    }
}

struct Clock(Instant);

impl Clock {
    fn at(&self, s: u64) -> Instant {
        self.0 + Duration::from_secs(s)
    }
}

fn watch(c: &Clock) -> Watch {
    Watch::new(limits(), c.at(0), Some("t0".into())).with_release_after(Duration::from_secs(30))
}

/// A watch that has seen a call at second 1 and a first sample (nothing running) at the same second, so that the next sample covers the time
/// from second 1.
fn polling(c: &Clock) -> Watch {
    let mut w = watch(c);
    w.tool_call(c.at(1));
    w.command(c.at(1), Command::Idle);
    w
}

fn active(age_s: u64) -> Command {
    Command::Active {
        age: Duration::from_secs(age_s),
    }
}

#[test]
fn the_first_time_the_brake_is_held_back_is_noted_with_the_age_of_the_command() {
    let c = Clock(Instant::now());
    let mut w = polling(&c);
    assert_eq!(w.take_note(), None, "nothing was held yet");
    w.command(c.at(6), active(600));
    assert_eq!(
        w.take_note(),
        Some(HoldNote::Held {
            age: Duration::from_secs(600)
        })
    );
    assert_eq!(w.take_note(), None, "a note is taken once");
}

#[test]
fn a_hold_that_goes_on_is_noted_once() {
    let c = Clock(Instant::now());
    let mut w = polling(&c);
    w.command(c.at(6), active(600));
    assert!(w.take_note().is_some());
    for s in (11..300).step_by(5) {
        w.tool_call(c.at(s));
        w.command(c.at(s + 1), active(600 + s));
        assert_eq!(w.take_note(), None, "at second {s}");
    }
}

#[test]
fn the_hold_is_released_after_the_release_time_without_credit_and_not_before() {
    let c = Clock(Instant::now());
    let mut w = polling(&c);
    w.command(c.at(6), active(600));
    let _ = w.take_note();
    w.command(c.at(11), Command::Idle);
    w.command(c.at(30), Command::Idle);
    w.command(c.at(35), Command::Idle);
    assert_eq!(w.take_note(), None, "29 s after the last credit");
    w.command(c.at(37), Command::Idle);
    assert_eq!(
        w.take_note(),
        Some(HoldNote::Released {
            held: Duration::from_secs(5)
        }),
        "31 s after the last credit; 5 s of time were credited in all"
    );
    w.command(c.at(60), Command::Idle);
    assert_eq!(w.take_note(), None, "released once");
}

#[test]
fn a_hold_that_resumes_before_the_release_time_is_the_same_hold() {
    let c = Clock(Instant::now());
    let mut w = polling(&c);
    w.command(c.at(6), active(600));
    let _ = w.take_note();
    w.command(c.at(11), Command::Idle);
    w.command(c.at(25), Command::Idle);
    w.tool_call(c.at(26));
    w.command(c.at(30), active(630));
    assert_eq!(w.take_note(), None);
    w.command(c.at(70), Command::Idle);
    assert_eq!(
        w.take_note(),
        Some(HoldNote::Released {
            held: Duration::from_secs(5 + 5)
        }),
        "the 10 s credited in both stretches"
    );
}

#[test]
fn a_hold_after_a_release_is_noted_again() {
    let c = Clock(Instant::now());
    let mut w = polling(&c);
    w.command(c.at(6), active(600));
    let _ = w.take_note();
    w.command(c.at(50), Command::Idle);
    assert!(matches!(w.take_note(), Some(HoldNote::Released { .. })));
    w.tool_call(c.at(60));
    w.command(c.at(65), active(700));
    assert_eq!(
        w.take_note(),
        Some(HoldNote::Held {
            age: Duration::from_secs(700)
        })
    );
}

#[test]
fn a_command_the_call_started_holds_nothing_and_is_not_noted() {
    // the sessions that use the CPU are younger than the call: the model's own foreground command
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    w.tool_call(c.at(0));
    w.command(c.at(30), active(28));
    assert_eq!(w.take_note(), None);
    assert_eq!(w.protected(), Duration::ZERO);
}

#[test]
fn evidence_that_could_not_be_read_holds_nothing() {
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    w.tool_call(c.at(1));
    w.command(c.at(6), Command::Unknown);
    assert_eq!(w.take_note(), None);
}

#[test]
fn a_call_taken_back_in_a_sample_that_credited_no_time_is_a_hold_too() {
    // the sample comes at the instant of the previous one: no time to credit, but a poll that was waiting is taken back
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    w.command(c.at(5), Command::Idle);
    w.tool_call(c.at(5));
    w.command(c.at(5), active(600));
    assert_eq!(
        w.take_note(),
        Some(HoldNote::Held {
            age: Duration::from_secs(600)
        })
    );
}

#[test]
fn a_boundary_does_not_end_a_hold() {
    let c = Clock(Instant::now());
    let mut w = polling(&c);
    w.command(c.at(6), active(600));
    let _ = w.take_note();
    w.boundary(c.at(7));
    w.tool_call(c.at(8));
    w.command(c.at(11), active(605));
    assert_eq!(w.take_note(), None, "the same hold, not a new one");
}
