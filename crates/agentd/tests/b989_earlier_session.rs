//! In a whole run: a later session of a run whose read of the earlier sessions' message record fails once is still told
//! what the user said before. The whole agentd runs in process against the fake AWS and the fake engine, as in
//! `b857_earlier_sessions.rs`. No AWS call is made.
//!
//! The service of this rig serves no route for the delivered messages, so the session reads the record in the run's bucket.
//! Before that record had one GET: after one answer 500 the first prompt of the session lacked the user's messages and said
//! that older messages may be missing. Now the session asks for that record again within a bound of its own (one second after
//! the look, in a session; this test waits that second).
#![allow(clippy::unwrap_used)]
#![cfg(target_os = "linux")]

mod common;
mod direct_support;

use agentd::direct::messages::earlier::{self, Earlier, RECORD_HEAD, UNREAD_LINE};
use direct_support::{BUCKET, Direct, GetFailure, Opts, PROMPT, RUN};
use harness_protocol::direct::{DirectSlice, s3 as keys, validate_direct_invocation};
use harness_protocol::runtime::{RepoSource, TurnOutcome};

const BRIEF: &str = "I added pct(a, b) and its test; the edge case for b = 0 is still open.";

async fn first_slice() -> (Vec<u8>, String, String) {
    let d1 = Direct::start(Opts::default()).await;
    let (_, resp) = d1.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let s = d1.summary();
    let head = s.git.head_sha.clone().expect("slice 1 delivered a head");
    let base = s.git.base_sha.clone().expect("slice 1 has a base");
    let bundle = d1
        .aws
        .get(&keys::bundle_key(RUN))
        .expect("slice 1 bundled its work");
    (bundle, head, base)
}

async fn second_slice(bundle: Vec<u8>, head: &str, base: &str) -> Direct {
    let mut d2 = Direct::start(Opts::default()).await;
    d2.aws.put(&keys::bundle_key_t(RUN, 1), bundle);
    d2.req.turn = 2;
    d2.req.runtime_session_id = format!("{RUN}-direct-g00000002");
    d2.req.repo = RepoSource::S3Bundle {
        uri: keys::uri(BUCKET, &keys::bundle_key_t(RUN, 1)),
    };
    d2.req.slice = Some(DirectSlice {
        last: true,
        base_sha: Some(base.to_string()),
        restore_head: Some(head.to_string()),
        sessions_before: Some(1),
        brief: Some(BRIEF.to_string()),
    });
    validate_direct_invocation(&d2.req).expect("the invocation of slice 2 is valid");
    d2
}

#[tokio::test(flavor = "multi_thread")]
async fn b989_a_later_session_whose_read_of_the_record_fails_once_is_still_told_what_the_user_said()
{
    let (bundle, head, base) = first_slice().await;
    let d2 = second_slice(bundle, &head, &base).await;
    // What the first session left in the run's bucket: two delivered messages.
    let mut said = Earlier::default();
    said.push(1, 1, "use tabs, not spaces");
    said.push(1, 1, "skip the Windows build");
    let key = earlier::key(RUN, 1);
    d2.aws.put(&key, said.to_json().into_bytes());
    // The first GET of that record is answered 500.
    d2.aws.fail_gets_as(&key, 1, GetFailure::Status500);
    let (_, resp) = d2.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let asked = d2.aws.gets_of(&key).len();
    assert!(
        asked >= 1,
        "TRIGGER PRESENT is not shown: the session did not ask for the record"
    );
    println!(
        "B989 TRIGGER PRESENT: the first GET of the earlier session's record was answered 500; the record got {asked} GET(s)"
    );

    let prompts = d2.prompts_received();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    let p = &prompts[0];
    assert!(p.contains(PROMPT), "the task: {p}");
    assert!(
        p.contains(RECORD_HEAD.trim_end())
            && p.contains("Recorded message 1 of 2:\n```text\nuse tabs, not spaces\n```\n")
            && p.contains("Recorded message 2 of 2:\n```text\nskip the Windows build\n```\n"),
        "the user's earlier messages are in the first prompt of the session, after {asked} GET(s) of the record: {p}"
    );
    assert!(
        !p.contains(UNREAD_LINE.trim_end())
            && !p.contains("older messages of the user may be missing"),
        "no message of the user is said to be missing: {p}"
    );
    assert_eq!(asked, 2, "the record's GET and one further GET");
    assert!(
        d2.log.lines().contains(
            &"direct turn earlier messages: the read of the record of session 1 was answered at try 2"
                .to_string()
        ),
        "{:?}",
        d2.log.lines()
    );
    // The session after this one finds the record under this session's own turn, whole.
    let kept = Earlier::parse(&d2.aws.get(&earlier::key(RUN, 2)).unwrap());
    assert_eq!(kept, said.clone().as_inherited());
    assert!(!kept.incomplete);
}

/// A record whose read fails at every GET: the session gives it the further GETs of its bound and then starts as it did.
/// Its first prompt says that messages of the user may be missing, and the log says how often the record was asked for.
/// The test waits the pauses of the further GETs (one second, then two).
#[tokio::test(flavor = "multi_thread")]
async fn b989_a_later_session_whose_read_of_the_record_fails_at_every_get_starts_as_before() {
    let (bundle, head, base) = first_slice().await;
    let d2 = second_slice(bundle, &head, &base).await;
    let mut said = Earlier::default();
    said.push(1, 1, "use tabs, not spaces");
    let key = earlier::key(RUN, 1);
    d2.aws.put(&key, said.to_json().into_bytes());
    d2.aws.fail_gets_as(&key, u32::MAX, GetFailure::Status500);
    let (_, resp) = d2.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let asked = d2.aws.gets_of(&key).len();
    assert_eq!(
        asked, 3,
        "TRIGGER PRESENT is not shown: the record was to get its GET and the two further GETs of its bound"
    );
    println!(
        "B989 TRIGGER PRESENT: every GET of the earlier session's record was answered 500; the record got {asked} GETs"
    );

    let prompts = d2.prompts_received();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    let p = &prompts[0];
    assert!(p.contains(PROMPT), "the task: {p}");
    assert!(
        p.contains(UNREAD_LINE.trim_end()),
        "the session is told that messages of the user may be missing: {p}"
    );
    assert!(
        !p.contains("use tabs, not spaces") && !p.contains(RECORD_HEAD.trim_end()),
        "no message of the record is shown: {p}"
    );
    assert!(
        d2.log.lines().contains(
            &"direct turn earlier messages: the read of the record of session 1 was asked 3 time(s) and not answered"
                .to_string()
        ),
        "{:?}",
        d2.log.lines()
    );
}
