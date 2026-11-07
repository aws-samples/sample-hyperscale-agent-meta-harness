//! The event ids of agentd: ULIDs that sort in the order the events were made.
//!
//! `new_ulid` filled the 80 low bits with random bytes, so the second id sorted before the first one (`01M50G3GRVG93N21...` before
//! `01M50G3GRVMDZFZB...`), and every reader that orders by `eventId` (the API's shard read, `ham`, `fold_input`) saw the events in the
//! wrong order. Ids made by one process must be strictly increasing.
#![allow(clippy::unwrap_used)]

use agentd::time::new_ulid;

#[test]
fn a_thousand_ids_made_in_a_row_are_strictly_increasing() {
    let ids: Vec<String> = (0..1000).map(|_| new_ulid()).collect();
    let first_out_of_order = ids.windows(2).position(|w| w[0] >= w[1]);
    assert_eq!(
        first_out_of_order.map(|i| (i, ids[i].clone(), ids[i + 1].clone())),
        None,
        "an id made later sorts before or equal to the one made earlier"
    );
}

#[test]
fn ids_made_in_the_same_millisecond_share_their_time_part_and_still_increase() {
    // 1000 ids in a row take well under 1000 ms, so some of them share a millisecond: the time part (10 characters) repeats there.
    let ids: Vec<String> = (0..1000).map(|_| new_ulid()).collect();
    let shared: Vec<&[String]> = ids
        .windows(2)
        .filter(|w| w[0][..10] == w[1][..10])
        .collect();
    assert!(
        !shared.is_empty(),
        "no two of 1000 ids shared a millisecond; the test cannot see the tie"
    );
    for w in shared {
        assert!(
            w[0] < w[1],
            "same millisecond, but {} sorts at or after {}",
            w[0],
            w[1]
        );
    }
}

// ---- the generator with an injected clock and random source --------------------------------------------------------------------

use agentd::events::EventRecorder;
use agentd::time::{UlidState, ulid_from};
use harness_protocol::envelope::EventType;
use serde_json::json;
use std::sync::Mutex;

const RAND_MAX: u128 = (1 << 80) - 1;
const CROCKFORD: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A random source that goes DOWN on every call: the worst case for a generator that trusts it to sort.
fn descending() -> impl FnMut() -> u128 {
    let mut next = RAND_MAX;
    move || {
        next -= 0x1_0000_0000;
        next
    }
}

/// The millisecond and the 80 random bits an id holds.
fn parts(id: &str) -> (u64, u128) {
    assert_eq!(id.len(), 26, "{id}");
    let v = id.chars().fold(0u128, |acc, c| {
        (acc << 5) | CROCKFORD.find(c).unwrap_or_else(|| panic!("{id}: {c}")) as u128
    });
    ((v >> 80) as u64, v & RAND_MAX)
}

#[test]
fn ten_thousand_ids_of_one_millisecond_increase_whatever_the_random_source_does() {
    let mut state = UlidState::default();
    let mut rng = descending();
    let ids: Vec<String> = (0..10_000)
        .map(|_| ulid_from(1_700_000_000_000, &mut state, &mut rng))
        .collect();
    assert!(
        ids.windows(2).all(|w| w[0] < w[1]),
        "not strictly increasing"
    );
    assert!(
        ids.iter().all(|id| parts(id).0 == 1_700_000_000_000),
        "every id keeps the millisecond"
    );
}

#[test]
fn a_new_millisecond_takes_a_fresh_random_part_and_always_sorts_after() {
    let mut state = UlidState::default();
    let mut rng = descending();
    let mut draws = 0;
    let mut counted = || {
        draws += 1;
        rng()
    };
    let a = ulid_from(1000, &mut state, &mut counted);
    let b = ulid_from(1000, &mut state, &mut counted);
    let c = ulid_from(1001, &mut state, &mut counted);
    assert!(a < b && b < c, "{a} {b} {c}");
    assert_eq!(
        draws, 2,
        "the random source is read once per new millisecond"
    );
    assert_eq!(
        parts(&b).1,
        parts(&a).1 + 1,
        "inside a millisecond the random part counts up"
    );
}

#[test]
fn a_clock_that_steps_back_does_not_make_an_id_that_sorts_before() {
    let mut state = UlidState::default();
    let mut rng = descending();
    let first = ulid_from(5_000, &mut state, &mut rng);
    let second = ulid_from(5_001, &mut state, &mut rng);
    let after_step_back = ulid_from(4_990, &mut state, &mut rng);
    let again = ulid_from(4_990, &mut state, &mut rng);
    assert!(first < second && second < after_step_back && after_step_back < again);
    assert_eq!(
        parts(&after_step_back).0,
        5_001,
        "the newest millisecond stays"
    );
    // The clock catches up: time moves on from there.
    let later = ulid_from(5_002, &mut state, &mut rng);
    assert!(again < later);
    assert_eq!(parts(&later).0, 5_002);
}

#[test]
fn a_full_random_part_moves_to_the_next_millisecond_and_never_panics() {
    let mut state = UlidState::at(7_000, RAND_MAX);
    let mut rng = descending();
    let last = ulid_from(7_000, &mut state, &mut rng);
    assert_eq!(parts(&last), (7_001, RAND_MAX - 0x1_0000_0000));
    let next = ulid_from(7_000, &mut state, &mut rng);
    assert!(last < next);
    assert_eq!(parts(&next).0, 7_001);
    // Also at the largest millisecond a ULID holds: wraps, never panics.
    let mut top = UlidState::at((1 << 48) - 1, RAND_MAX);
    let _ = ulid_from(0, &mut top, &mut rng);
}

// ---- the recorder: the order of the log is the order of the appends --------------------------------------------------------------

/// A random source a test hands over.
type Rng = Box<dyn FnMut() -> u128 + Send>;

static FROZEN: Mutex<(UlidState, Option<Rng>)> = Mutex::new((UlidState::at(0, 0), None));

/// Ids of a frozen clock and a descending random source, kept in one place for the recorder under test.
fn frozen_mint() -> String {
    let mut guard = FROZEN.lock().unwrap();
    let (state, rng) = &mut *guard;
    let rng = rng.get_or_insert_with(|| Box::new(descending()));
    ulid_from(1_800_000_000_000, state, rng)
}

#[test]
fn two_events_of_a_frozen_millisecond_keep_their_order_in_the_log_and_after_the_first_returns_the_second()
 {
    let rec = EventRecorder::with_mint("01M50G1C5RKR2QG5MCXVXRCC93", "direct", frozen_mint);
    rec.record(
        EventType::TurnProgress,
        json!({"kind": "input-received", "requestId": "q"}),
    );
    rec.record(
        EventType::TurnProgress,
        json!({"kind": "input-closed", "requestId": "q", "how": "finish"}),
    );
    let pushed = rec.drain();
    assert_eq!(pushed.len(), 2);
    // The log, as the API reads it: sorted by id.
    let mut page = pushed.clone();
    page.sort_by(|a, b| a.event_id.cmp(&b.event_id));
    assert_eq!(
        page.iter().map(|e| e.seq).collect::<Vec<_>>(),
        pushed.iter().map(|e| e.seq).collect::<Vec<_>>(),
        "page order is push order"
    );
    assert_eq!(
        parts(&pushed[0].event_id).0,
        parts(&pushed[1].event_id).0,
        "one millisecond"
    );
    // `?after=<first id>` returns the second event.
    let after: Vec<_> = page
        .iter()
        .filter(|e| e.event_id > pushed[0].event_id)
        .collect();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].event_id, pushed[1].event_id);
}

#[test]
fn events_pushed_by_many_threads_are_appended_in_id_order() {
    let rec = EventRecorder::new("01M50G1C5RKR2QG5MCXVXRCC93", "direct");
    let handles: Vec<_> = (0..8)
        .map(|t| {
            let rec = rec.clone();
            std::thread::spawn(move || {
                for i in 0..150 {
                    rec.record(EventType::TurnProgress, json!({"t": t, "i": i}));
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let events = rec.drain();
    assert_eq!(events.len(), 1200);
    assert!(
        events.windows(2).all(|w| w[0].event_id < w[1].event_id),
        "the id of an event made later sorts before the id of one appended earlier"
    );
    assert!(
        events.windows(2).all(|w| w[0].seq + 1 == w[1].seq),
        "seq follows the append order"
    );
}

#[test]
fn a_question_is_given_its_own_event_id_at_the_append() {
    let rec = EventRecorder::new("01M50G1C5RKR2QG5MCXVXRCC93", "direct");
    rec.record(
        EventType::TurnProgress,
        json!({"kind": "message", "text": "before"}),
    );
    let id = rec.record_urgent_minted(
        EventType::TurnProgress,
        |id| json!({"kind": "needs-input", "requestId": id}),
    );
    rec.record(
        EventType::TurnProgress,
        json!({"kind": "message", "text": "after"}),
    );
    let events = rec.drain();
    assert_eq!(events.len(), 3);
    assert_eq!(events[1].event_id, id, "the request id is the event id");
    let payload = serde_json::to_value(&events[1].payload).unwrap();
    assert_eq!(payload["value"]["requestId"], id.as_str());
    assert!(events[0].event_id < events[1].event_id && events[1].event_id < events[2].event_id);
}
