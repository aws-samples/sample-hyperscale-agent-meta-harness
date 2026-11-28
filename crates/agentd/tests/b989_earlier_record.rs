//! The look at the records of the earlier sessions' messages asks again for the first record whose read failed with an
//! answer that may be another one a moment later. The look itself runs as before (one GET for each record, newest first, to the
//! first complete one); the further GETs come after it, with what the look read in hand, inside a bound of their own that ends
//! before the limit of the whole read. A stand-in bucket; no AWS call is made.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::messages::earlier::{
    self, Again, AskedAgain, Earlier, RECORD_AGAIN_ENDS, RECORD_AGAIN_PAUSE, RECORD_AGAIN_TRIES,
};
use agentd::s3::S3Objects;
use agentd::time::{format_rfc3339, now_unix};
use common::{creds, scratch};
use direct_support::{BUCKET, FakeAws, GetFailure, RUN};
use std::time::Duration;
use tokio::time::Instant;

fn texts(e: &Earlier) -> Vec<&str> {
    e.messages.iter().map(|m| m.text.as_str()).collect()
}

/// The record a session of turn `turn` leaves with the one message `text` it delivered itself; `incomplete` as the session marks
/// its record when it could not read the records before it.
fn record_of(turn: u32, text: &str, incomplete: bool) -> Vec<u8> {
    let mut e = Earlier::default();
    e.push(turn, 1, text);
    e.incomplete = incomplete;
    e.to_json().into_bytes()
}

/// A stand-in bucket and a client of it.
async fn bucket() -> (FakeAws, S3Objects) {
    let aws = FakeAws::start().await;
    let s3 = S3Objects::new("us-west-2".into(), Some(aws.base.clone()));
    (aws, s3)
}

/// The further GETs of a test: the product's number of them, a short pause, and far more time than they need.
fn quick() -> Again {
    Again {
        tries: RECORD_AGAIN_TRIES,
        pause: Duration::from_millis(20),
        by: Instant::now() + Duration::from_secs(30),
    }
}

/// How many GETs the record of `turn` got.
fn gets(aws: &FakeAws, turn: u32) -> usize {
    aws.gets_of(&earlier::key(RUN, turn)).len()
}

/// The look of a session of turn `turn` with the further GETs of `again`.
async fn look(
    s3: &S3Objects,
    turn: u32,
    name: &str,
    again: Option<&Again>,
) -> (Earlier, Option<AskedAgain>) {
    let c = creds(&format_rfc3339(now_unix() + 3600));
    earlier::fetch_newest_again(s3, &c, BUCKET, RUN, turn, &scratch(name), again).await
}

#[test]
fn b989_the_bounds_of_the_further_gets_are_the_products() {
    assert_eq!(RECORD_AGAIN_TRIES, 2);
    assert_eq!(RECORD_AGAIN_PAUSE, Duration::from_secs(1));
    assert_eq!(RECORD_AGAIN_ENDS, Duration::from_secs(1));
    let ends = Instant::now() + Duration::from_secs(20);
    assert_eq!(
        Again::of_look(ends),
        Again {
            tries: 2,
            pause: Duration::from_secs(1),
            by: ends - Duration::from_secs(1),
        }
    );
}

/// An incomplete record in front of a complete one, and the read of the incomplete one fails once. Before the session
/// started without the message of that record. Now the record is read at a further GET, and the result is the result of a look
/// in which nothing failed.
#[tokio::test]
async fn b989_a_record_whose_read_fails_once_is_read_at_a_further_get() {
    let (aws, s3) = bucket().await;
    aws.put(&earlier::key(RUN, 1), record_of(1, "from turn one", false));
    aws.put(&earlier::key(RUN, 2), record_of(2, "from turn two", true));
    let (plain, none) = look(&s3, 3, "b989-plain", Some(&quick())).await;
    assert_eq!(texts(&plain), ["from turn one", "from turn two"]);
    assert!(!plain.incomplete);
    assert_eq!(
        none, None,
        "a look in which nothing failed asks nothing again"
    );

    let (aws, s3) = bucket().await;
    aws.put(&earlier::key(RUN, 1), record_of(1, "from turn one", false));
    aws.put(&earlier::key(RUN, 2), record_of(2, "from turn two", true));
    aws.fail_gets_as(&earlier::key(RUN, 2), 1, GetFailure::Status500);
    // The look as it was: the record of turn two is not read, and the result says so.
    let (before, _) = look(&s3, 3, "b989-once-before", None).await;
    assert_eq!(texts(&before), ["from turn one"]);
    assert!(
        before.incomplete,
        "TRIGGER PRESENT is not shown: the read of the record of turn two was to fail"
    );
    println!(
        "B989 TRIGGER PRESENT: one GET of the record of turn two failed, and a look with one GET for each record lacks its message"
    );

    aws.fail_gets_as(&earlier::key(RUN, 2), 1, GetFailure::Status500);
    let before_gets = gets(&aws, 2);
    let (got, asked) = look(&s3, 3, "b989-once", Some(&quick())).await;
    assert_eq!(got, plain, "the result of a look in which nothing failed");
    assert_eq!(
        asked,
        Some(AskedAgain {
            turn: 2,
            gets: 2,
            read: true
        })
    );
    assert_eq!(
        gets(&aws, 2) - before_gets,
        2,
        "one GET and one further GET"
    );
}

/// The record that fails once is the complete one, in front of older records: the look read on past it. When the further GET gives
/// it, the look's result is the one it has when it stops at that record: what it read of the turns before it is left out.
#[tokio::test]
async fn b989_a_complete_record_read_at_a_further_get_is_the_record_the_look_stops_at() {
    let (aws, s3) = bucket().await;
    aws.put(&earlier::key(RUN, 1), record_of(1, "from turn one", false));
    aws.put(&earlier::key(RUN, 2), record_of(2, "from turn two", true));
    aws.put(
        &earlier::key(RUN, 3),
        record_of(3, "from turn three", false),
    );
    aws.put(&earlier::key(RUN, 4), record_of(4, "from turn four", true));
    let (plain, _) = look(&s3, 5, "b989-stop-plain", None).await;
    assert_eq!(texts(&plain), ["from turn three", "from turn four"]);
    assert!(!plain.incomplete);
    assert_eq!(
        gets(&aws, 2),
        0,
        "a look stops at the first complete record"
    );

    aws.fail_gets_as(&earlier::key(RUN, 3), 1, GetFailure::SlowDown);
    let (got, asked) = look(&s3, 5, "b989-stop", Some(&quick())).await;
    assert_eq!(
        gets(&aws, 2),
        1,
        "TRIGGER PRESENT is not shown: the look was to read on past the record that failed"
    );
    assert_eq!(got, plain, "the result of a look in which nothing failed");
    assert_eq!(
        asked,
        Some(AskedAgain {
            turn: 3,
            gets: 2,
            read: true
        })
    );
}

/// Done-when 3: a record that fails at every GET. The records the look read are kept, the result is marked incomplete as before,
/// the failing record got its GET and the further GETs of its bound, and a second failing record got one GET.
#[tokio::test]
async fn b989_a_record_that_fails_at_every_get_leaves_the_records_that_were_read() {
    let (aws, s3) = bucket().await;
    aws.put(&earlier::key(RUN, 1), record_of(1, "from turn one", false));
    aws.put(&earlier::key(RUN, 2), record_of(2, "from turn two", true));
    aws.put(&earlier::key(RUN, 3), record_of(3, "from turn three", true));
    aws.put(&earlier::key(RUN, 4), record_of(4, "from turn four", true));
    aws.put(&earlier::key(RUN, 5), record_of(5, "from turn five", true));
    aws.fail_gets_as(&earlier::key(RUN, 4), u32::MAX, GetFailure::Status500);
    aws.fail_gets_as(&earlier::key(RUN, 2), u32::MAX, GetFailure::Status500);
    let (as_before, _) = look(&s3, 6, "b989-every-before", None).await;
    assert_eq!(
        texts(&as_before),
        ["from turn one", "from turn three", "from turn five"]
    );
    assert!(as_before.incomplete);
    let (four, two) = (gets(&aws, 4), gets(&aws, 2));
    assert_eq!((four, two), (1, 1), "one GET for each record, as before");

    let started = Instant::now();
    let (got, asked) = look(&s3, 6, "b989-every", Some(&quick())).await;
    assert_eq!(
        got, as_before,
        "the records that were read are kept, and the result is the look's as before"
    );
    assert_eq!(
        asked,
        Some(AskedAgain {
            turn: 4,
            gets: 1 + RECORD_AGAIN_TRIES,
            read: false
        })
    );
    assert_eq!(
        gets(&aws, 4) - four,
        3,
        "the first failing record: its GET and two further GETs"
    );
    assert_eq!(
        gets(&aws, 2) - two,
        1,
        "the second failing record: its one GET"
    );
    // The pauses of the further GETs: the first, then twice the first.
    assert!(
        started.elapsed() >= Duration::from_millis(60),
        "{:?}",
        started.elapsed()
    );
}

/// The whole read stays inside its limit: the further GETs end at their own end, which lies before the limit of the read,
/// whatever they have come to. Here the time that is left is shorter than the first pause: no further GET is made, and the
/// records the look read are handed over before that end.
#[tokio::test]
async fn b989_the_further_gets_end_at_their_bound_with_what_the_look_read() {
    let (aws, s3) = bucket().await;
    aws.put(&earlier::key(RUN, 1), record_of(1, "from turn one", false));
    aws.put(&earlier::key(RUN, 2), record_of(2, "from turn two", true));
    aws.put(&earlier::key(RUN, 3), record_of(3, "from turn three", true));
    aws.fail_gets_as(&earlier::key(RUN, 2), u32::MAX, GetFailure::Status500);
    let started = Instant::now();
    let short = Again {
        tries: RECORD_AGAIN_TRIES,
        pause: Duration::from_secs(5),
        by: started + Duration::from_millis(300),
    };
    let (got, asked) = look(&s3, 4, "b989-bound", Some(&short)).await;
    let took = started.elapsed();
    assert_eq!(texts(&got), ["from turn one", "from turn three"]);
    assert!(got.incomplete);
    assert_eq!(
        asked,
        Some(AskedAgain {
            turn: 2,
            gets: 1,
            read: false
        })
    );
    assert_eq!(
        gets(&aws, 2),
        1,
        "no further GET inside a time shorter than the pause"
    );
    assert!(
        took >= Duration::from_millis(250) && took < Duration::from_secs(4),
        "the look ended at the end of the further GETs' time, not after their pause: {took:?}"
    );

    // An end that has already passed: nothing waits and nothing is asked again.
    let passed = Again {
        tries: RECORD_AGAIN_TRIES,
        pause: Duration::from_millis(20),
        by: Instant::now(),
    };
    let again_started = Instant::now();
    let (got, asked) = look(&s3, 4, "b989-passed", Some(&passed)).await;
    assert_eq!(texts(&got), ["from turn one", "from turn three"]);
    assert_eq!(asked.map(|a| (a.gets, a.read)), Some((1, false)));
    assert!(again_started.elapsed() < Duration::from_secs(4));
}

/// A record that is not there, and a record the bucket refuses, stay what they are: one GET each, and nothing is asked again.
#[tokio::test]
async fn b989_a_record_that_is_absent_or_refused_keeps_its_one_get() {
    let (aws, s3) = bucket().await;
    aws.put(&earlier::key(RUN, 1), record_of(1, "from turn one", false));
    aws.put(&earlier::key(RUN, 3), record_of(3, "from turn three", true));
    aws.put(&earlier::key(RUN, 4), record_of(4, "from turn four", true));
    // Turn two left no record (404); the record of turn three is there and the bucket answers 403 for it.
    aws.fail_gets_as(&earlier::key(RUN, 3), u32::MAX, GetFailure::Denied);
    let (got, asked) = look(&s3, 5, "b989-absent", Some(&quick())).await;
    assert_eq!(texts(&got), ["from turn one", "from turn four"]);
    assert_eq!(asked, None);
    assert_eq!((gets(&aws, 3), gets(&aws, 2)), (1, 1));
}

/// A further GET that says the record is not there settles it: the turn left none, and the result is not marked for it.
#[tokio::test]
async fn b989_a_further_get_that_finds_no_record_settles_the_turn() {
    let (aws, s3) = bucket().await;
    aws.put(&earlier::key(RUN, 1), record_of(1, "from turn one", false));
    // Turn two left no record, and the first GET for it fails.
    aws.fail_gets_as(&earlier::key(RUN, 2), 1, GetFailure::Status500);
    let (got, asked) = look(&s3, 3, "b989-none", Some(&quick())).await;
    assert_eq!(texts(&got), ["from turn one"]);
    assert!(
        !got.incomplete,
        "the turn left no record: nothing may be missing"
    );
    assert_eq!(
        asked,
        Some(AskedAgain {
            turn: 2,
            gets: 2,
            read: true
        })
    );
}

/// The look of six arguments is the look as it was: one GET for each record and nothing asked again.
#[tokio::test]
async fn b989_the_look_without_further_gets_is_the_look_as_it_was() {
    let (aws, s3) = bucket().await;
    aws.put(&earlier::key(RUN, 1), record_of(1, "from turn one", false));
    aws.put(&earlier::key(RUN, 2), record_of(2, "from turn two", true));
    aws.fail_gets_as(&earlier::key(RUN, 2), u32::MAX, GetFailure::Status500);
    let c = creds(&format_rfc3339(now_unix() + 3600));
    let got = earlier::fetch_newest(&s3, &c, BUCKET, RUN, 3, &scratch("b989-six")).await;
    assert_eq!(texts(&got), ["from turn one"]);
    assert!(got.incomplete);
    assert_eq!(gets(&aws, 2), 1);
}
