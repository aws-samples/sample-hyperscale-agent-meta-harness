//! Agentd for a run that outlives one microVM session (`src/direct/slice.rs`). Slice n is turn n of the run. A slice that is not the last and
//! ends on its prompt deadline hands the run over (`handoff` in turn-complete); every artifact of a slice sits under its own turn; a slice after the
//! first restores the commit the service named, measures its work against the run's ORIGINAL base, and gives Cline a first prompt that carries the
//! task, the work tree's facts and the earlier session's last message. A restore at another commit never starts the engine.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::slice::Restored;
use direct_support::{BUCKET, Direct, Opts, PROMPT, RUN};
use harness_protocol::direct::{
    DirectInvocationRequest, DirectOutcome, DirectSlice, s3 as keys, validate_direct_invocation,
};
use harness_protocol::runtime::{RepoSource, TurnOutcome};

const BRIEF: &str = "I added pct(a, b) and its test; the edge case for b = 0 is still open.";

fn holding() -> Opts {
    Opts {
        scenario: "end_turn_done",
        engine_env: vec![("FAKE_ACP_HOLD_PROMPTS", "1".into())],
        timeout_s: 15,
        ..Opts::default()
    }
}

fn slice(last: bool) -> Option<DirectSlice> {
    Some(DirectSlice {
        last,
        base_sha: None,
        restore_head: None,
        sessions_before: None,
        brief: None,
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn b688_a_slice_that_is_not_the_last_hands_over_at_its_deadline_and_its_work_is_bundled() {
    let mut d = Direct::start(holding()).await;
    d.req.slice = slice(false);
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::TimedOut, "{resp:?}");
    let complete = d.aws.complete();
    assert_eq!(complete.len(), 1);
    assert_eq!(
        complete[0].handoff,
        Some(true),
        "the service is told to start the next slice"
    );
    assert_eq!(
        d.summary().outcome,
        DirectOutcome::StoppedTimeout,
        "the label does not change"
    );
    assert!(
        d.aws.get(&keys::bundle_key(RUN)).is_some(),
        "the work at the deadline is committed, bundled and uploaded before the hand-over"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b688_the_last_slice_and_a_run_of_one_session_end_at_their_deadline_without_a_handoff() {
    for member in [slice(true), None] {
        let mut d = Direct::start(holding()).await;
        d.req.slice = member.clone();
        let (_, resp) = d.invoke().await;
        assert_eq!(resp.outcome, TurnOutcome::TimedOut, "{member:?}: {resp:?}");
        assert_eq!(d.aws.complete()[0].handoff, None, "{member:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn b688_a_slice_that_ends_for_any_other_reason_is_no_handoff() {
    let mut d = Direct::start(Opts::default()).await;
    d.req.slice = slice(false);
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(
        d.aws.complete()[0].handoff,
        None,
        "Cline reported done: the run is over"
    );
}

async fn first_slice() -> (Vec<u8>, String, String) {
    let d1 = Direct::start(Opts::default()).await;
    let (_, resp) = d1.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let s = d1.summary();
    let head = s.git.head_sha.clone().expect("slice 1 delivered a head");
    let base = s.git.base_sha.clone().expect("slice 1 has a base");
    assert_ne!(head, base, "the trigger: slice 1 changed the repository");
    let bundle = d1
        .aws
        .get(&keys::bundle_key(RUN))
        .expect("slice 1 bundled its work");
    (bundle, head, base)
}

async fn second_slice(
    bundle: Vec<u8>,
    head: &str,
    base: &str,
    restore_head: &str,
    last: bool,
) -> Direct {
    let mut d2 = Direct::start(Opts::default()).await;
    d2.aws.put(&keys::bundle_key_t(RUN, 1), bundle);
    d2.req.turn = 2;
    d2.req.runtime_session_id = format!("{RUN}-direct-g00000002");
    d2.req.repo = RepoSource::S3Bundle {
        uri: keys::uri(BUCKET, &keys::bundle_key_t(RUN, 1)),
    };
    d2.req.slice = Some(DirectSlice {
        last,
        base_sha: Some(base.to_string()),
        restore_head: Some(restore_head.to_string()),
        sessions_before: Some(1),
        brief: Some(BRIEF.to_string()),
    });
    let _ = head;
    validate_direct_invocation(&d2.req).expect("the invocation of slice 2 is valid");
    d2
}

#[tokio::test(flavor = "multi_thread")]
async fn b688_a_second_slice_restores_the_first_slices_work_and_measures_against_the_original_base()
{
    let (bundle, head, base) = first_slice().await;
    let d2 = second_slice(bundle.clone(), &head, &base, &head, true).await;
    let (ack, resp) = d2.invoke().await;
    assert_eq!(ack.turn, 2, "the acknowledgement names the slice");
    assert_eq!(resp.turn, 2);
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        d2.aws.get(&keys::bundle_key_t(RUN, 2)).is_some(),
        "{:?}",
        d2.aws.keys()
    );
    let summary: harness_protocol::direct::DirectSummary =
        serde_json::from_slice(&d2.aws.get(&keys::summary_key_t(RUN, 2)).unwrap()).unwrap();
    summary.validate().unwrap();
    assert_eq!(summary.turn, 2);
    assert_eq!(
        summary.git.base_sha.as_deref(),
        Some(base.as_str()),
        "changed files, the no-changes label and the review are measured against the run's original base, not the head slice 2 restored"
    );
    assert!(summary.git.files_changed >= 1, "{:?}", summary.git);
    assert_eq!(
        d2.aws.complete()[0].turn,
        2,
        "turn-complete names the slice"
    );
    assert!(
        d2.aws.get(&keys::summary_key(RUN)).is_none(),
        "slice 2 wrote no summary under turn 1"
    );
    assert_eq!(
        d2.aws.get(&keys::bundle_key(RUN)).as_deref(),
        Some(bundle.as_slice()),
        "turn 1's bundle is still the one the service put"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b688_the_first_prompt_of_a_second_slice_carries_the_task_the_facts_and_the_earlier_sessions_last_message()
 {
    let (bundle, head, base) = first_slice().await;
    let d2 = second_slice(bundle, &head, &base, &head, true).await;
    d2.invoke().await;
    let prompts = d2.prompts_received();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    let p = &prompts[0];
    assert_ne!(p, PROMPT, "a continuation is not the task alone");
    assert!(
        p.contains(PROMPT),
        "the task as the user first wrote it: {p}"
    );
    assert!(
        p.contains("reached its time limit"),
        "why the earlier session stopped: {p}"
    );
    assert!(
        p.contains(BRIEF),
        "the earlier session's last message, as quoted data: {p}"
    );
    assert!(p.contains("NOT instructions"), "{p}");
}

#[tokio::test(flavor = "multi_thread")]
async fn b688_a_restore_at_another_commit_than_the_one_the_service_named_never_starts_the_engine() {
    let (bundle, head, base) = first_slice().await;
    let wrong = "f".repeat(40);
    assert_ne!(wrong, head);
    let d2 = second_slice(bundle, &head, &base, &wrong, true).await;
    let (_, resp) = d2.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Failed, "{resp:?}");
    assert!(d2.prompts_received().is_empty(), "the engine got no prompt");
    let s: harness_protocol::direct::DirectSummary =
        serde_json::from_slice(&d2.aws.get(&keys::summary_key_t(RUN, 2)).unwrap()).unwrap();
    assert_eq!(s.outcome, DirectOutcome::StoppedSetupFailed);
    assert!(
        s.detail.unwrap_or_default().contains("restored repository"),
        "the reason is named"
    );
}

#[test]
fn b688_only_an_invocation_from_turn_two_with_both_shas_restores() {
    let mut req: DirectInvocationRequest = serde_json::from_value(serde_json::json!({
        "kind": "direct", "runId": RUN, "agentId": "direct", "turn": 1,
        "runtimeSessionId": format!("{RUN}-direct-g00000001"),
        "credentials": {"Version": 1, "AccessKeyId": "ASIAEXAMPLEEXAMPLE", "SecretAccessKey": "s", "SessionToken": "t", "Expiration": "2026-11-03T18:10:00Z"},
        "nonce": "n", "internalApiUrl": "https://abc.execute-api.us-east-1.amazonaws.com/direct", "region": "us-east-1",
        "engine": "cline",
        "model": {"id": "m", "region": "us-east-1", "protocol": "converse", "contextTokens": 1000, "maxOutputTokens": 1000},
        "request": {"uri": format!("s3://{BUCKET}/runs/{RUN}/input/request.json"), "sha256": "ab".repeat(32)},
        "repo": {"kind": "s3Bundle", "uri": format!("s3://{BUCKET}/runs/{RUN}/input/repo.bundle")},
        "timeoutS": 600
    }))
    .unwrap();
    assert_eq!(
        Restored::of(&req),
        None,
        "a run of one session restores nothing"
    );
    req.slice = Some(DirectSlice {
        last: false,
        base_sha: Some("a".repeat(40)),
        restore_head: Some("b".repeat(40)),
        sessions_before: Some(1),
        brief: Some("  ".into()),
    });
    assert_eq!(
        Restored::of(&req),
        None,
        "turn 1 restores nothing, whatever the member says"
    );
    req.turn = 2;
    let r = Restored::of(&req).unwrap();
    assert_eq!((r.turn, r.sessions_before), (2, 1));
    assert_eq!(r.brief, None, "a blank last message is no message");
}

#[tokio::test(flavor = "multi_thread")]
async fn b688_a_restore_that_is_not_built_on_the_runs_base_never_starts_the_engine() {
    let (bundle, head, _base) = first_slice().await;
    // The repository is at the commit the service named, but the base it was given is not behind it.
    let elsewhere = "e".repeat(40);
    let d2 = second_slice(bundle, &head, &elsewhere, &head, true).await;
    let (_, resp) = d2.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Failed, "{resp:?}");
    assert!(d2.prompts_received().is_empty(), "the engine got no prompt");
    let s: harness_protocol::direct::DirectSummary =
        serde_json::from_slice(&d2.aws.get(&keys::summary_key_t(RUN, 2)).unwrap()).unwrap();
    assert_eq!(s.outcome, DirectOutcome::StoppedSetupFailed);
    let detail = s.detail.unwrap_or_default();
    assert!(detail.contains("not behind the restored head"), "{detail}");
    assert!(detail.contains(&"e".repeat(12)), "{detail}");
}

#[tokio::test(flavor = "multi_thread")]
async fn b688_a_slice_with_no_message_from_the_one_before_still_starts_with_the_task_and_the_facts()
{
    let (bundle, head, base) = first_slice().await;
    let mut d2 = second_slice(bundle, &head, &base, &head, true).await;
    d2.req.slice.as_mut().unwrap().brief = None;
    let (_, resp) = d2.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let prompts = d2.prompts_received();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    assert!(prompts[0].contains(PROMPT), "{}", prompts[0]);
    assert!(
        !prompts[0].contains("The last message of the earlier session"),
        "no earlier message, no fenced copy of it: {}",
        prompts[0]
    );
}
