//! The rules of the copy task that are numbers and small state: how long the git work of a look has, when the timer ticks around a
//! request to end, and what the copy task asks the file system of the work tree.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::snapshot::{
    COPY_GIT_FACTOR, COPY_GIT_FLOOR, COPY_GIT_LIMIT, CopyTimer, GitBudget, STOP_COPY_RETRY,
    TREE_ANSWER_LIMIT, TREE_ASK_EVERY, TreeAlive, ask_tree, git_limit, git_limit_gave_up,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const MIN: u64 = 60;

fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

#[test]
fn b823_the_git_work_of_a_look_has_five_minutes_or_ten_times_the_last_stored_copy() {
    assert_eq!(COPY_GIT_FLOOR, secs(5 * MIN));
    assert_eq!(COPY_GIT_FACTOR, 10);
    assert_eq!(COPY_GIT_LIMIT, secs(60 * MIN));
    // No copy stored yet, and a repository whose copies are quick: five minutes. (Before, every look had an hour: a look whose git hung
    // while the file system still answered held the copy task for that hour.)
    assert_eq!(git_limit(None, 0), secs(5 * MIN));
    assert_eq!(git_limit(Some(secs(1)), 0), secs(5 * MIN));
    assert_eq!(git_limit(Some(secs(30)), 0), secs(5 * MIN));
    // A repository whose copies take long keeps them: ten times the git work of its last stored copy.
    assert_eq!(git_limit(Some(secs(31)), 0), secs(310));
    assert_eq!(git_limit(Some(secs(2 * MIN)), 0), secs(20 * MIN));
    // Never more than an hour.
    assert_eq!(git_limit(Some(secs(7 * MIN)), 0), secs(60 * MIN));
    assert_eq!(git_limit(Some(Duration::MAX), 0), secs(60 * MIN));
}

#[test]
fn b823_a_look_that_its_git_limit_gave_up_has_twice_the_time_at_the_next_try() {
    // A copy that needs longer than its limit (a first copy of a large tree) is made at a later try: 5, 10, 20, 40 minutes, then an hour.
    assert_eq!(git_limit(None, 1), secs(10 * MIN));
    assert_eq!(git_limit(None, 2), secs(20 * MIN));
    assert_eq!(git_limit(None, 3), secs(40 * MIN));
    assert_eq!(git_limit(None, 4), secs(60 * MIN));
    assert_eq!(git_limit(None, 40), secs(60 * MIN));
    assert_eq!(git_limit(None, u32::MAX), secs(60 * MIN));
    assert_eq!(git_limit(Some(secs(MIN)), 1), secs(20 * MIN));

    let mut budget = GitBudget::default();
    assert_eq!(budget.limit(), secs(5 * MIN));
    budget.gave_up();
    budget.gave_up();
    assert_eq!(budget.limit(), secs(20 * MIN));
    // A look whose git ended in time and stored no copy (an unchanged tree, a failed upload) ends the doubling.
    budget.ended();
    assert_eq!(budget.limit(), secs(5 * MIN));
    budget.gave_up();
    // A stored copy sets what the next limit follows, and ends the doubling.
    budget.stored(secs(90));
    assert_eq!(budget.limit(), secs(15 * MIN));
    budget.gave_up();
    assert_eq!(budget.limit(), secs(30 * MIN));
    budget.stored(secs(2));
    assert_eq!(budget.limit(), secs(5 * MIN));
}

#[tokio::test(start_paused = true)]
async fn b823_a_look_for_a_request_to_end_leaves_the_tick_of_the_timer_where_it_is() {
    let t0 = tokio::time::Instant::now();
    let mut timer = CopyTimer::new(Some(secs(60)), t0);
    assert_eq!(timer.next(), Some(t0 + secs(60)));
    // A look of the timer or of a tool call arms the timer again: it counts from the start of one look to the start of the next.
    timer.look_starts(t0 + secs(60), false);
    assert_eq!(timer.next(), Some(t0 + secs(120)));
    timer.look_starts(t0 + secs(75), false);
    assert_eq!(timer.next(), Some(t0 + secs(135)));
    // A look that a request to end asked for, 15 s before the tick: the tick stays. (Before, it moved a whole period away: a session
    // that was asked to end at 12:10:52 had its tick of 12:11:07 moved to 12:11:52.)
    timer.look_starts(t0 + secs(120), true);
    assert_eq!(timer.next(), Some(t0 + secs(135)));
    // A request that comes when the tick is due takes its place.
    timer.look_starts(t0 + secs(135), true);
    assert_eq!(timer.next(), Some(t0 + secs(195)));
}

#[tokio::test(start_paused = true)]
async fn b823_after_a_stop_copy_that_failed_the_next_look_comes_soon() {
    assert_eq!(STOP_COPY_RETRY, secs(5));
    let t0 = tokio::time::Instant::now();
    // The tick is 40 s away: the next look comes after the retry time.
    let mut timer = CopyTimer::new(Some(secs(60)), t0);
    timer.try_again(t0 + secs(20), STOP_COPY_RETRY);
    assert_eq!(timer.next(), Some(t0 + secs(25)));
    // The tick is 2 s away: it stays.
    let mut timer = CopyTimer::new(Some(secs(60)), t0);
    timer.try_again(t0 + secs(58), STOP_COPY_RETRY);
    assert_eq!(timer.next(), Some(t0 + secs(60)));
    // The copies are switched off, so no tick comes: the retry comes all the same, and after it no tick again.
    let mut timer = CopyTimer::new(None, t0);
    assert_eq!(timer.next(), None);
    timer.look_starts(t0 + secs(10), true);
    assert_eq!(timer.next(), None);
    timer.try_again(t0 + secs(12), STOP_COPY_RETRY);
    assert_eq!(timer.next(), Some(t0 + secs(17)));
    timer.look_starts(t0 + secs(17), false);
    assert_eq!(timer.next(), None);
}

#[test]
fn b823_the_tree_is_asked_every_twenty_seconds_and_has_five_to_answer() {
    assert_eq!(TREE_ASK_EVERY, secs(20));
    assert_eq!(TREE_ANSWER_LIMIT, secs(5));
}

#[tokio::test(flavor = "multi_thread")]
async fn b823_the_question_to_the_tree_reads_a_file_the_copy_needs() {
    let dir = common::scratch("tree-question");
    let tree = dir.join("repo");
    std::fs::create_dir_all(tree.join(".git")).unwrap();
    std::fs::write(tree.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    assert_eq!(ask_tree(&tree), Ok(()));
    // The engine may have removed `.git`, or put something else there: the file system has answered all the same.
    std::fs::remove_file(tree.join(".git/HEAD")).unwrap();
    assert_eq!(ask_tree(&tree), Ok(()));
    std::fs::create_dir_all(tree.join(".git/HEAD")).unwrap();
    assert_eq!(ask_tree(&tree), Ok(()));
    std::fs::remove_dir_all(tree.join(".git")).unwrap();
    assert_eq!(ask_tree(&tree), Ok(()));
    // A tree that is gone is no answer.
    assert!(ask_tree(&dir.join("no-such-tree")).is_err());
}

/// A storage that answers nothing: the question to it does not return for a while. The copy task waits for the limit, not for the
/// question, and asks no second question while the first is outstanding.
#[tokio::test(flavor = "multi_thread")]
async fn b823_a_question_that_does_not_return_is_no_answer_and_is_asked_once() {
    let dir = common::scratch("tree-silent");
    let hangs = Arc::new(AtomicBool::new(true));
    let asked = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let (hang, count) = (hangs.clone(), asked.clone());
    let alive = TreeAlive::with_question(
        &dir,
        Arc::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            while hang.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(())
        }),
    );
    let began = std::time::Instant::now();
    let silent = alive.answers(Duration::from_millis(300)).await.unwrap_err();
    assert!(
        silent.starts_with("the work tree did not answer within"),
        "{silent}"
    );
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "{:?}",
        began.elapsed()
    );
    // The first question is still outstanding: no second one is asked.
    let again = alive.answers(Duration::from_millis(300)).await.unwrap_err();
    assert_eq!(again, "the work tree has not answered an earlier question");
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    // The storage answers again: the question returns, and the next one is asked and answered.
    hangs.store(false, Ordering::SeqCst);
    let mut answered = false;
    for _ in 0..100 {
        if alive.answers(Duration::from_millis(300)).await.is_ok() {
            answered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(answered, "the tree answers once its storage does");
    assert_eq!(asked.load(Ordering::SeqCst), 2);
}

/// The git limit doubles only for a look that this limit gave up. A look whose git ended in time and whose copy could not be stored
/// afterwards fails for the store, also when the store was given up after the git limit had passed: a bundle of 400 MiB has 430 s to
/// be stored, and the git limit is 300 s. Counted as given up, four such looks during an outage of the bucket raised the git limit to
/// one hour, and a git that then hung was held for that hour.
#[test]
fn b823_a_copy_that_could_not_be_stored_after_its_git_ended_does_not_double_the_git_limit() {
    // git ended after 20 s; the store was given up at 450 s, past the git limit of 300 s.
    assert!(!git_limit_gave_up(false, true, true));
    // The git work was cut at its limit.
    assert!(git_limit_gave_up(false, false, true));
    // A silent tree ended the git work, or the look failed before the limit: no doubling.
    assert!(!git_limit_gave_up(true, false, true));
    assert!(!git_limit_gave_up(false, false, false));
    // Four looks whose store failed late leave the limit where it was.
    let mut budget = GitBudget::default();
    for _ in 0..4 {
        if git_limit_gave_up(false, true, true) {
            budget.gave_up();
        } else {
            budget.ended();
        }
    }
    assert_eq!(budget.limit(), COPY_GIT_FLOOR);
}
