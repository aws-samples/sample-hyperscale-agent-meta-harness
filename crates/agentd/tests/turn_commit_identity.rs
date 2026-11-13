//! Which role wrote a turn, in the harness's own turn commit.
//! The custom docs-writer mode has no command group, so it can
//! never commit; agentd's `harness: turn 3` commit by `ham agent` was the only commit of its green phase, and
//! a run-level evaluator read that author as proof that docs-writer never ran (not_met, gap task gap1).
//! Every commit agentd makes for a turn now ends with git trailers that name the role, the agent and the
//! turn (and the task, when the turn names one); the subject and the author stay as they were, so readers
//! of `%s` and `%an` see no change. The evaluator's git log tool prints `%B`, so the trailers reach it.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::workspace::{TurnAuthor, commit_turn, commit_turn_as, prepare_local, turn_message};
use common::{git, python_calc, scratch};

fn trailer(repo: &std::path::Path, key: &str) -> String {
    git(
        repo,
        &[
            "log",
            "-1",
            &format!("--format=%(trailers:key={key},valueonly,separator=%x2C)"),
        ],
    )
}

const DOCS: TurnAuthor<'static> = TurnAuthor {
    role: "docs-writer",
    agent: "docs-a1",
    task: None,
};

#[tokio::test]
async fn a_harness_turn_commit_names_the_role_the_agent_and_the_turn_in_trailers() {
    let d = scratch("ident-docs");
    let wt = prepare_local(&d.join("ws"), "run", "docs-a1", &python_calc(&d))
        .await
        .unwrap();
    std::fs::create_dir_all(wt.repo.join("docs")).unwrap();
    std::fs::write(wt.repo.join("docs/API.md"), "# API\n").unwrap();

    commit_turn_as(&wt, 3, &DOCS).await.unwrap();

    assert_eq!(
        git(&wt.repo, &["log", "-1", "--format=%s"]),
        "harness: turn 3"
    );
    assert_eq!(
        git(&wt.repo, &["log", "-1", "--format=%an <%ae>"]),
        "ham agent <agent@harness.invalid>"
    );
    assert_eq!(trailer(&wt.repo, "Ham-Role"), "docs-writer");
    assert_eq!(trailer(&wt.repo, "Ham-Agent"), "docs-a1");
    assert_eq!(trailer(&wt.repo, "Ham-Turn"), "3");
    assert_eq!(
        trailer(&wt.repo, "Ham-Task"),
        "",
        "no task named, none guessed"
    );
    // git itself parses the block as trailers, one per key.
    let body = git(&wt.repo, &["log", "-1", "--format=%B"]);
    assert_eq!(
        body,
        "harness: turn 3\n\nHam-Role: docs-writer\nHam-Agent: docs-a1\nHam-Turn: 3"
    );
}

#[tokio::test]
async fn the_task_trailer_appears_when_the_turn_names_its_task() {
    let d = scratch("ident-task");
    let wt = prepare_local(&d.join("ws"), "run", "docs-a1", &python_calc(&d))
        .await
        .unwrap();
    std::fs::write(wt.repo.join("README.md"), "readme\n").unwrap();
    let by = TurnAuthor {
        task: Some("docs"),
        ..DOCS
    };

    commit_turn_as(&wt, 1, &by).await.unwrap();

    assert_eq!(trailer(&wt.repo, "Ham-Task"), "docs");
    assert_eq!(trailer(&wt.repo, "Ham-Role"), "docs-writer");
}

#[test]
fn a_value_that_could_forge_or_break_a_trailer_is_left_out_and_never_rewritten() {
    // A newline would start a forged trailer; control characters, spaces, an empty value or an overlong one
    // are not identifiers. The commit keeps every valid trailer.
    let long = "r".repeat(129);
    for bad in [
        "docs\nHam-Role: implementer",
        "docs\r",
        "docs\u{0}",
        "docs writer",
        "",
        long.as_str(),
    ] {
        let by = TurnAuthor {
            role: bad,
            agent: "docs-a1",
            task: None,
        };
        let m = turn_message(2, Some(&by));
        assert_eq!(
            m, "harness: turn 2\n\nHam-Agent: docs-a1\nHam-Turn: 2",
            "{bad:?}"
        );
    }
    assert_eq!(turn_message(2, None), "harness: turn 2");
}

#[tokio::test]
async fn without_a_change_no_commit_is_made_and_commit_turn_keeps_its_plain_message() {
    let d = scratch("ident-none");
    let wt = prepare_local(&d.join("ws"), "run", "docs-a1", &python_calc(&d))
        .await
        .unwrap();
    let base = git(&wt.repo, &["rev-parse", "HEAD"]);
    assert_eq!(commit_turn_as(&wt, 1, &DOCS).await.unwrap(), base);

    std::fs::write(wt.repo.join("README.md"), "readme\n").unwrap();
    commit_turn(&wt, 2).await.unwrap();
    assert_eq!(
        git(&wt.repo, &["log", "-1", "--format=%B"]),
        "harness: turn 2"
    );
}
