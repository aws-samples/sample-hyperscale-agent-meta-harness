//! The first words a session of a run reads, with every change of the release in force: how the run delivers (the block; none for a
//! run that switched it off), the harness's opening of a continued task, the task as the user first wrote it, the messages the user sent
//! earlier in the run, and why the earlier session stopped with the words about the copy this session starts from.
//!
//! The copy is the last saved copy for a session after one that reached its time limit or was lost, and the files the service read
//! back from an earlier session's storage when the service chose those: the invocation names that copy as its repository, so
//! the worker says which one it is. A run that `ham resume` or a retry starts is a new run: its first session reads the block and
//! then the prompt of that run, and nothing about an earlier session.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::Deps;
use agentd::direct::delivery_word::{HEADING, block};
use agentd::direct::hooks::{DirectHook, Hooks};
use agentd::direct::messages::earlier::{self, Ask, BLOCK_HEAD, Earlier, RECORD_HEAD};
use agentd::direct::messages::{
    ClaimOutcome, MemoryStore, Message, MessageStore, MessagesHook, Mode, Settings,
};
use direct_support::{BUCKET, Direct, Opts, PROMPT, RUN};
use harness_protocol::direct::{
    DirectDelivery, DirectSlice, s3 as keys, validate_direct_invocation,
};
use harness_protocol::runtime::{RepoSource, TurnOutcome};
use std::sync::Arc;
use std::time::Duration;

const BRIEF: &str = "I added pct(a, b) and its test; the workflow file is still open.";
const OPENING: &str = "You are continuing a task";
const TASK: &str = "Task, as the user first wrote it:";
const WHY: &str = "Why the earlier session stopped (written by the harness):\n";
/// A session that starts from a saved copy: the words of the releases before the read.
const SAVED_TITLE: &str = "The earlier session of this run ended: its machine reached its time limit, or the service lost the session.";
const SAVED_COPY: &str = "This session starts in a new machine from the last saved copy of the earlier session's work. Changes the earlier session made after that copy may be missing.";
/// A session that starts from files the service read back.
const READ_BACK_TITLE: &str = "The earlier session of this run ended without a result: it was stopped from outside, or the service lost it.";
const READ_BACK_COPY: &str = "This session starts in a new machine from the files the service read back from the storage of session 1 after that session ended. They are the files that storage held; a file that was still being written may lack its last lines.";

/// The options of a run that delivers a zip and kept the block (`with_block`), or that switched the block off.
fn opts(with_block: bool) -> Opts {
    if with_block {
        Opts {
            delivery_note: None,
            ..Opts::default()
        }
    } else {
        Opts::default()
    }
}

/// Runs session 1 to its end and returns what the service hands to session 2: the bundle of the work, its head and the run's base.
async fn first_session(with_block: bool) -> (Vec<u8>, String, String) {
    let d1 = Direct::start(opts(with_block)).await;
    let (_, resp) = d1.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let s = d1.summary();
    let head = s.git.head_sha.clone().expect("session 1 delivered a head");
    let base = s.git.base_sha.clone().expect("session 1 has a base");
    assert_ne!(head, base, "the trigger: session 1 changed the repository");
    let bundle = d1
        .aws
        .get(&keys::bundle_key(RUN))
        .expect("session 1 bundled its work");
    (bundle, head, base)
}

/// Session 2 of the run in a new machine. The bucket holds the work under `key`, and the invocation names it as the repository.
async fn second_session(
    bundle: Vec<u8>,
    key: &str,
    head: &str,
    base: &str,
    with_block: bool,
) -> Direct {
    let mut d2 = Direct::start(opts(with_block)).await;
    d2.aws.put(key, bundle);
    d2.req.turn = 2;
    d2.req.runtime_session_id = format!("{RUN}-direct-g00000002");
    d2.req.repo = RepoSource::S3Bundle {
        uri: keys::uri(BUCKET, key),
    };
    d2.req.slice = Some(DirectSlice {
        last: true,
        base_sha: Some(base.to_string()),
        restore_head: Some(head.to_string()),
        sessions_before: Some(1),
        brief: Some(BRIEF.to_string()),
    });
    validate_direct_invocation(&d2.req).expect("the invocation of session 2 is valid");
    d2
}

/// The one prompt session 2 received.
async fn first_words(d2: &Direct) -> String {
    d2.invoke().await;
    let prompts = d2.prompts_received();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    prompts[0].clone()
}

/// The places of `parts` in `p`, each of which must be there.
fn places(p: &str, parts: &[&str]) -> Vec<usize> {
    parts
        .iter()
        .map(|needle| {
            p.find(needle)
                .unwrap_or_else(|| panic!("{needle:?} is in the prompt: {p}"))
        })
        .collect()
}

/// A session after one that reached its time limit (its final bundle) and after one the service lost (its last saved copy): the words
/// of the saved copy, in their order, with and without the block.
#[tokio::test(flavor = "multi_thread")]
async fn b854_a_later_session_from_a_saved_copy_reads_the_block_the_task_and_the_words_of_that_copy()
 {
    for with_block in [true, false] {
        let (bundle, head, base) = first_session(with_block).await;
        for key in [
            // The session before reached its time limit and handed its work over.
            keys::bundle_key_t(RUN, 1),
            // The session before was lost, and its last saved copy is what there is.
            keys::snapshot_bundle_key_t(RUN, 1),
        ] {
            let d2 = second_session(bundle.clone(), &key, &head, &base, with_block).await;
            let p = first_words(&d2).await;
            let why = format!("{WHY}{SAVED_TITLE}");
            let order = places(&p, &[OPENING, TASK, &why, SAVED_COPY]);
            assert!(
                order.windows(2).all(|w| w[0] < w[1]),
                "{key}: the opening, the task, why the earlier session stopped, the saved copy: {order:?} in {p}"
            );
            if with_block {
                let b = block(Some(&DirectDelivery::zip())).unwrap();
                assert!(
                    p.starts_with(&b),
                    "{key}: the block is the first thing the engine reads: {p}"
                );
                assert_eq!(
                    order[0],
                    b.len() + 2,
                    "{key}: the opening follows the block: {p}"
                );
                assert_eq!(
                    p.matches(HEADING).count(),
                    1,
                    "{key}: the block is said once: {p}"
                );
            } else {
                assert_eq!(
                    order[0], 0,
                    "{key}: the opening is the first thing the engine reads: {p}"
                );
                assert!(!p.contains(HEADING), "{key}: no block on the delivery: {p}");
            }
            assert!(
                !p.contains("read back"),
                "{key}: no word of files read back: {p}"
            );
            assert!(p.contains(PROMPT) && p.contains(BRIEF), "{key}: {p}");
        }
    }
}

/// A session that starts from the files the service read back from the storage of session 1: the invocation names that copy, and the
/// first words say what it is. They do not call it the last saved copy.
#[tokio::test(flavor = "multi_thread")]
async fn b854_a_later_session_from_files_read_back_is_told_which_copy_it_starts_from() {
    for with_block in [true, false] {
        let (bundle, head, base) = first_session(with_block).await;
        let key = keys::recovery_bundle_key(RUN, 1);
        let d2 = second_session(bundle, &key, &head, &base, with_block).await;
        let p = first_words(&d2).await;
        println!(
            "B854 TRIGGER PRESENT: the session restored the files read back and got its first prompt: {}",
            p.contains(OPENING)
        );
        let why = format!("{WHY}{READ_BACK_TITLE}");
        let order = places(&p, &[OPENING, TASK, &why, READ_BACK_COPY]);
        assert!(
            order.windows(2).all(|w| w[0] < w[1]),
            "the opening, the task, why the earlier session stopped, the files read back: {order:?} in {p}"
        );
        assert!(
            !p.contains("last saved copy") && !p.contains("its machine reached its time limit"),
            "the files read back are not called the last saved copy: {p}"
        );
        if with_block {
            let b = block(Some(&DirectDelivery::zip())).unwrap();
            assert!(
                p.starts_with(&b),
                "the block is the first thing the engine reads: {p}"
            );
            assert_eq!(p.matches(HEADING).count(), 1, "the block is said once: {p}");
        } else {
            assert_eq!(
                order[0], 0,
                "the opening is the first thing the engine reads: {p}"
            );
            assert!(!p.contains(HEADING), "no block on the delivery: {p}");
        }
        assert!(p.contains(PROMPT) && p.contains(BRIEF), "{p}");
    }
}

/// A run that `ham resume` or a retry starts is a new run. Its first session reads the block and then the prompt of that run, as
/// typed: no opening of a continued task and no word about an earlier session or its copy.
#[tokio::test(flavor = "multi_thread")]
async fn b854_the_first_session_of_a_run_reads_the_block_and_then_its_prompt_and_nothing_of_an_earlier_session()
 {
    for with_block in [true, false] {
        let d = Direct::start(opts(with_block)).await;
        let (_, resp) = d.invoke().await;
        assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
        let prompts = d.prompts_received();
        assert!(!prompts.is_empty(), "the engine got a prompt");
        let expected = if with_block {
            format!(
                "{}\n\n{PROMPT}",
                block(Some(&DirectDelivery::zip())).unwrap()
            )
        } else {
            PROMPT.to_string()
        };
        assert_eq!(prompts[0], expected, "with the block: {with_block}");
        for words in [OPENING, WHY, "last saved copy", "read back"] {
            assert!(!prompts[0].contains(words), "{words:?} in {}", prompts[0]);
        }
    }
}

/// ONE test of the real order, with every change of the release in force. For each kind of start of a later session (after a session
/// that reached its time limit, after a lost session from its last saved copy, and from the files the service read back), with the
/// block on the delivery and without it, the first prompt holds in this order: the block (none for a run that switched it off), the
/// harness's opening, the task as the user first wrote it, the messages the user sent earlier in the run (from the record the earlier
/// session left in the run's bucket, quoted as that record), why the earlier session stopped, and the words about the copy this
/// session starts from. A session that starts from files read back gets the record of earlier messages like every other, and hands it
/// on under its own turn. The first session of a run (what `ham resume` and a retry start) reads the block and its prompt only.
#[tokio::test(flavor = "multi_thread")]
async fn b854_b857_the_first_words_of_a_session_in_their_order_for_every_kind_of_start() {
    const ONE: &str = "Recorded message 1 of 2:\n```text\nuse tabs, not spaces\n```\n";
    const TWO: &str = "Recorded message 2 of 2:\n```text\nskip the Windows build\n```\n";
    for with_block in [true, false] {
        let (bundle, head, base) = first_session(with_block).await;
        for (key, title, copy) in [
            (keys::bundle_key_t(RUN, 1), SAVED_TITLE, SAVED_COPY),
            (keys::snapshot_bundle_key_t(RUN, 1), SAVED_TITLE, SAVED_COPY),
            (
                keys::recovery_bundle_key(RUN, 1),
                READ_BACK_TITLE,
                READ_BACK_COPY,
            ),
        ] {
            let d2 = second_session(bundle.clone(), &key, &head, &base, with_block).await;
            // What session 1 left in the run's bucket: two messages it delivered.
            let mut said = Earlier::default();
            said.push(1, 1, "use tabs, not spaces");
            said.push(1, 1, "skip the Windows build");
            d2.aws
                .put(&earlier::key(RUN, 1), said.to_json().into_bytes());
            let p = first_words(&d2).await;
            let why = format!("{WHY}{title}");
            let order = places(
                &p,
                &[OPENING, TASK, RECORD_HEAD.trim_end(), ONE, TWO, &why, copy],
            );
            assert!(
                order.windows(2).all(|w| w[0] < w[1]),
                "{key}: the opening, the task, the earlier messages in their order, why the earlier session stopped, the copy: \
                 {order:?} in {p}"
            );
            if with_block {
                let b = block(Some(&DirectDelivery::zip())).unwrap();
                assert!(p.starts_with(&b), "{key}: the block comes first: {p}");
                assert_eq!(
                    order[0],
                    b.len() + 2,
                    "{key}: the opening follows the block: {p}"
                );
                assert_eq!(
                    p.matches(HEADING).count(),
                    1,
                    "{key}: the block is said once: {p}"
                );
            } else {
                assert_eq!(order[0], 0, "{key}: the opening comes first: {p}");
                assert!(!p.contains(HEADING), "{key}: no block on the delivery: {p}");
            }
            assert!(
                !p.contains(BLOCK_HEAD.trim_end()),
                "{key}: this session delivered none of the messages itself: {p}"
            );
            assert!(p.contains(PROMPT) && p.contains(BRIEF), "{key}: {p}");
            // The record goes on with the run: the session after this one finds it under this session's turn.
            let kept = said.clone().as_inherited();
            assert_eq!(
                Earlier::parse(&d2.aws.get(&earlier::key(RUN, 2)).unwrap_or_else(|| panic!(
                    "{key}: no record under turn 2: {:?}",
                    d2.aws.keys()
                ))),
                kept,
                "{key}"
            );
        }
        // The first session of a run: the block, then the prompt as typed, and no record of an earlier message.
        let d = Direct::start(opts(with_block)).await;
        let (_, resp) = d.invoke().await;
        assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
        let first = d.prompts_received()[0].clone();
        let expected = if with_block {
            format!(
                "{}\n\n{PROMPT}",
                block(Some(&DirectDelivery::zip())).unwrap()
            )
        } else {
            PROMPT.to_string()
        };
        assert_eq!(first, expected, "with the block: {with_block}");
        assert!(
            d.aws.get(&earlier::key(RUN, 1)).is_none(),
            "a run with no message stores no record"
        );
    }
}

/// The service's record of the run after its first session: the worker of that session delivered these messages, in this order.
async fn service_with(delivered: &[(&str, &str)]) -> Arc<MemoryStore> {
    const FIRST: &str = "session-of-the-first-machine";
    let store = MemoryStore::new();
    store.report_epoch(FIRST, 1).await.unwrap();
    for (n, (id, text)) in delivered.iter().enumerate() {
        store
            .accept(Message {
                id: (*id).into(),
                text: (*text).into(),
                mode: Mode::Next,
                epoch: FIRST.into(),
                seq: 0,
            })
            .unwrap();
        assert!(matches!(
            store.claim(id, FIRST).await.unwrap(),
            ClaimOutcome::Claimed(_)
        ));
        store.submitted(id, n as u32 + 2).await.unwrap();
    }
    store
}

/// The same order with the service's answer in place. The worker of a later session asks the service for
/// the messages the user sent earlier, and the first prompt carries them as the user's own words between the task and the harness's
/// words about the earlier session: for a session after a time limit, after a lost session from its last saved copy, and from the files
/// the service read back, with the block on the delivery and without it. The record in the run's bucket is not shown then.
#[tokio::test(flavor = "multi_thread")]
async fn b854_b857_the_first_words_with_the_services_messages_in_their_order_for_every_kind_of_start()
 {
    const ONE: &str =
        "Message 1 of 2 (delivered in an earlier session):\n```text\nuse tabs, not spaces\n```\n";
    const TWO: &str =
        "Message 2 of 2 (delivered in an earlier session):\n```text\nskip the Windows build\n```\n";
    for with_block in [true, false] {
        let (bundle, head, base) = first_session(with_block).await;
        for (key, title, copy) in [
            (keys::bundle_key_t(RUN, 1), SAVED_TITLE, SAVED_COPY),
            (keys::snapshot_bundle_key_t(RUN, 1), SAVED_TITLE, SAVED_COPY),
            (
                keys::recovery_bundle_key(RUN, 1),
                READ_BACK_TITLE,
                READ_BACK_COPY,
            ),
        ] {
            let d2 = second_session(bundle.clone(), &key, &head, &base, with_block).await;
            // What an earlier session's engine could have left in the bucket: it must not be shown beside the service's answer.
            let mut record = Earlier::default();
            record.push(1, 1, "a text only the bucket's record holds");
            d2.aws
                .put(&earlier::key(RUN, 1), record.to_json().into_bytes());
            let service = service_with(&[
                ("m1", "use tabs, not spaces"),
                ("m2", "skip the Windows build"),
            ])
            .await;
            let mut settings = Settings::for_tests();
            settings.min_left = Duration::ZERO;
            settings.earlier = Ask {
                limit: Duration::from_millis(400),
                pause: Duration::from_millis(10),
                ..Ask::default()
            };
            let seen: Arc<dyn MessageStore> = service.clone();
            let hook = MessagesHook::new(seen, settings);
            let deps = Deps {
                hooks: Some(Hooks::new(vec![hook as Arc<dyn DirectHook>])),
                ..Deps::default()
            };
            let outcome = agentd::direct::run_with(&d2.app, d2.req.clone(), &deps)
                .await
                .outcome;
            assert_eq!(outcome, TurnOutcome::Completed, "{key}");
            assert_eq!(
                service.calls().iter().filter(|c| *c == "history").count(),
                1,
                "{key}: TRIGGER PRESENT: the session asked the service once: {:?}",
                service.calls()
            );
            let prompts = d2.prompts_received();
            assert!(!prompts.is_empty(), "{key}: the engine received no prompt");
            let p = prompts[0].clone();
            let why = format!("{WHY}{title}");
            let order = places(
                &p,
                &[OPENING, TASK, BLOCK_HEAD.trim_end(), ONE, TWO, &why, copy],
            );
            assert!(
                order.windows(2).all(|w| w[0] < w[1]),
                "{key}: the opening, the task, the user's earlier messages in the service's order, why the earlier session \
                 stopped, the copy: {order:?} in {p}"
            );
            if with_block {
                let b = block(Some(&DirectDelivery::zip())).unwrap();
                assert!(p.starts_with(&b), "{key}: the block comes first: {p}");
                assert_eq!(
                    order[0],
                    b.len() + 2,
                    "{key}: the opening follows the block: {p}"
                );
                assert_eq!(
                    p.matches(HEADING).count(),
                    1,
                    "{key}: the block is said once: {p}"
                );
            } else {
                assert_eq!(order[0], 0, "{key}: the opening comes first: {p}");
                assert!(!p.contains(HEADING), "{key}: no block on the delivery: {p}");
            }
            assert!(
                !p.contains(RECORD_HEAD.trim_end())
                    && !p.contains("a text only the bucket's record holds"),
                "{key}: the bucket's record is not shown beside the service's answer: {p}"
            );
            assert!(p.contains(PROMPT) && p.contains(BRIEF), "{key}: {p}");
        }
    }
}
