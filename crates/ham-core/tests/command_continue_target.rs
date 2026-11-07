//! `ham continue` and the typed `/continue` follow the target of the run they continue. The command names a target only when
//! the person does (`--repo cc:<name>` or `--s3-only`; that choice wins); otherwise the client reads the source run's target, and the words
//! that name it come from one place. The typed form keeps: the run on screen, or `--run <id>`; a leading run id is refused
//! with the line to type, because it would otherwise open a billable prompt.
#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, DirectArgs, TargetArg, parse};
use ham_core::followup::{continue_target_line, follow_up_target, target_words};
use ham_core::registry::{Effects, effects_of};
use ham_core::router::{RouteContext, RouteOutcome, route};
use harness_protocol::target::{RepoTarget, RunTarget};

const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const OTHER: &str = "01K6A0000000000000000000R2";

fn calc() -> RepoTarget {
    "cc:calc".parse().unwrap()
}

fn usage(args: &[&str]) -> String {
    let e = parse(args).unwrap_err().to_string();
    assert!(e.starts_with("usage: "), "{e}");
    e
}

/// The parsed follow-up, after a round trip through the canonical argv.
fn follow_up(args: &[&str]) -> (DirectArgs, Option<TargetArg>) {
    let inv = parse(args).unwrap();
    assert_eq!(parse(&inv.to_argv()).unwrap(), inv, "{args:?}");
    match inv.command {
        Command::Continue(d, explicit) => (d, explicit),
        other => panic!("{other:?}"),
    }
}

fn ctx(focus: Option<&str>) -> RouteContext {
    RouteContext {
        focused_run: focus.map(str::to_string),
        tainted: false,
    }
}

fn prepared(
    text: &str,
    focus: Option<&str>,
) -> (Vec<String>, DirectArgs, Option<TargetArg>, String) {
    match route(text, &ctx(focus)) {
        RouteOutcome::Command {
            argv,
            invocation,
            preview,
            ..
        } => match invocation.command {
            Command::Continue(d, explicit) => (argv, d, explicit, preview.unwrap().text),
            other => panic!("{other:?}"),
        },
        other => panic!("{text:?}: {other:?}"),
    }
}

fn refusal(text: &str, focus: Option<&str>) -> String {
    match route(text, &ctx(focus)) {
        RouteOutcome::Refused { reason, .. } => reason,
        other => panic!("{text:?} was not refused: {other:?}"),
    }
}

// ---- the parser ---------------------------------------------------------------------------------------------------------------

#[test]
fn a_follow_up_names_no_target_unless_the_person_does() {
    let (d, explicit) = follow_up(&["continue", RUN, "go on"]);
    assert_eq!(explicit, None);
    assert_eq!(d.continue_from.as_deref(), Some(RUN));
    assert_eq!(d.prompt, "go on");
}

#[test]
fn s3_only_and_a_repository_are_explicit_targets_of_a_follow_up() {
    let (d, explicit) = follow_up(&["continue", RUN, "go on", "--s3-only"]);
    assert_eq!(explicit, Some(TargetArg::S3Only));
    assert_eq!(d.target, TargetArg::S3Only);
    let (d, explicit) = follow_up(&["continue", RUN, "go on", "--repo", "cc:calc"]);
    assert_eq!(explicit, Some(TargetArg::Repo(calc())));
    assert_eq!(d.target, TargetArg::Repo(calc()));
    // The flag may stand before the text, and the text may be left for the editor.
    let (_, explicit) = follow_up(&["continue", RUN, "--repo", "cc:calc"]);
    assert_eq!(explicit, Some(TargetArg::Repo(calc())));
}

#[test]
fn a_follow_up_takes_one_target_and_refuses_a_new_repository_a_base_and_direct() {
    let e = usage(&["continue", RUN, "go", "--s3-only", "--repo", "cc:calc"]);
    assert!(e.contains("--repo") && e.contains("--s3-only"), "{e}");
    let e = usage(&["continue", RUN, "go", "--repo", "cc:a", "--repo", "cc:b"]);
    assert!(e.contains("--repo"), "{e}");
    for flag in [
        vec!["--new-repo", "cc:hamh-e2e-new-1"],
        vec!["--base", "main"],
        vec!["--direct"],
    ] {
        let mut args = vec!["continue", RUN, "go"];
        args.extend(flag.iter().copied());
        let e = usage(&args);
        assert!(e.contains("continue") && e.contains(flag[0]), "{e}");
    }
    // The new-repository refusal says what to do instead.
    let e = usage(&["continue", RUN, "go", "--new-repo", "cc:x"]);
    assert!(e.contains("--s3-only"), "{e}");
}

// ---- the words ------------------------------------------------------------------------------------------------------------------

#[test]
fn the_target_of_the_source_run_is_the_target_of_the_follow_up() {
    assert_eq!(
        follow_up_target(&RunTarget::Repo { repo: calc() }, RUN),
        Ok(TargetArg::Repo(calc()))
    );
    // A run that made its repository is followed on that repository.
    assert_eq!(
        follow_up_target(&RunTarget::NewRepo { repo: calc() }, RUN),
        Ok(TargetArg::Repo(calc()))
    );
    assert_eq!(
        follow_up_target(&RunTarget::S3Only, RUN),
        Ok(TargetArg::S3Only)
    );
}

#[test]
fn a_run_on_another_git_host_has_no_follow_up_on_it_and_the_refusal_says_what_to_add() {
    // The service continues a run on a CodeCommit repository or by a zip alone; it would refuse the other hosts after the editor was used.
    for spelling in ["gh:acme/calc", "gl:group/calc"] {
        let repo: RepoTarget = spelling.parse().unwrap();
        let why = follow_up_target(&RunTarget::Repo { repo }, RUN).unwrap_err();
        assert!(
            why.contains(RUN)
                && why.contains(spelling)
                && why.contains("cc:<name>")
                && why.contains("--s3-only"),
            "{why}"
        );
    }
}

#[test]
fn one_function_words_the_target_for_the_shell_and_the_preview() {
    let repo = target_words(&TargetArg::Repo(calc()));
    assert!(
        repo.starts_with("cc:calc,") && repo.contains("pull request") && repo.contains("zip"),
        "{repo}"
    );
    let zip = target_words(&TargetArg::S3Only);
    assert!(
        zip.starts_with("s3-only,") && zip.contains("zip") && zip.contains("no pull request"),
        "{zip}"
    );
    let line = continue_target_line(&TargetArg::Repo(calc()), RUN, false);
    assert_eq!(line, format!("target: {repo} (the target of run {RUN})"));
    let line = continue_target_line(&TargetArg::S3Only, RUN, true);
    assert_eq!(line, format!("target: {zip} (chosen on the command line)"));
}

// ---- what the box makes of a typed follow-up --------------------------------------------------------------------------------------------

#[test]
fn a_follow_up_with_text_needs_the_terminal_like_the_one_without() {
    // Both are handed to a child `ham continue`, which follows the new run until it ends: the in-process runner cannot.
    for args in [
        vec!["continue", RUN, "go"],
        vec!["continue", RUN],
        vec!["continue", RUN, "go", "--s3-only"],
    ] {
        let command = parse(&args).unwrap().command;
        assert_eq!(
            effects_of(&command),
            Effects::BILLABLE.union(Effects::TERMINAL_HANDOFF),
            "{args:?}"
        );
    }
}

#[test]
fn a_typed_follow_up_uses_the_run_on_screen_and_a_run_option_picks_another() {
    let (argv, d, explicit, _) = prepared("/continue add the tests", Some(RUN));
    assert_eq!(argv, ["continue", RUN, "add the tests"]);
    assert_eq!((d.continue_from.as_deref(), explicit), (Some(RUN), None));
    let (_, d, _, _) = prepared(&format!("/continue --run {OTHER} add the tests"), Some(RUN));
    assert_eq!(d.continue_from.as_deref(), Some(OTHER));
    assert_eq!(d.prompt, "add the tests");
    // No text: the editor opens in the child.
    let (_, d, _, _) = prepared("/continue", Some(RUN));
    assert_eq!(d.prompt, "");
    // The shell form typed in the box is tokenised like a shell and takes its run id first; its flags are read.
    let (argv, d, explicit, _) = prepared(
        &format!("ham continue {OTHER} 'add tests' --s3-only"),
        Some(RUN),
    );
    assert_eq!(argv[..2], ["continue", OTHER]);
    assert_eq!(
        (d.continue_from.as_deref(), explicit),
        (Some(OTHER), Some(TargetArg::S3Only))
    );
}

#[test]
fn a_leading_run_id_after_the_slash_word_is_refused_with_the_line_to_type() {
    // The word would otherwise be the first word of a billable prompt, or the whole prompt.
    for text in [
        format!("/continue {OTHER} add the tests"),
        format!("/continue {OTHER}"),
    ] {
        let why = refusal(&text, Some(RUN));
        assert!(
            why.contains("--run") && why.contains(OTHER) && why.contains("/continue"),
            "{why}"
        );
    }
    let why = refusal(&format!("/continue {OTHER} add the tests"), Some(RUN));
    assert!(
        why.contains(&format!("/continue --run {OTHER} add the tests")),
        "the refusal shows the line to type: {why}"
    );
    // With --run given the first word of the text is text, even when it looks like an id.
    let (_, d, _, _) = prepared(
        &format!("/continue --run {RUN} {OTHER} is the run to compare"),
        Some(RUN),
    );
    assert_eq!(d.continue_from.as_deref(), Some(RUN));
    assert_eq!(d.prompt, format!("{OTHER} is the run to compare"));
    // An id in lower case is not one `ham runs` prints, so it is text.
    let (_, d, _, _) = prepared(&format!("/continue {}", OTHER.to_lowercase()), Some(RUN));
    assert_eq!(d.prompt, OTHER.to_lowercase());
    // The shell form is not a slash form: its first word is the run id.
    prepared(&format!("ham continue {OTHER} 'add the tests'"), Some(RUN));
}

#[test]
fn the_preview_of_a_follow_up_names_the_run_the_target_and_the_wait() {
    let (_, _, _, text) = prepared("/continue add the tests", Some(RUN));
    for want in [
        "ham continue",
        RUN,
        "billable",
        &format!("Target: the target of run {RUN}"),
        "until the new run ends",
    ] {
        assert!(text.contains(want), "{want}: {text}");
    }
    // A target the person chose is named as chosen.
    let (_, _, _, text) = prepared(&format!("ham continue {RUN} go --s3-only"), Some(RUN));
    assert!(
        text.contains("Target: s3-only,") && text.contains("chosen on the line"),
        "{text}"
    );
    let (_, _, _, text) = prepared(&format!("ham continue {RUN} go --repo cc:calc"), Some(RUN));
    assert!(
        text.contains("Target: cc:calc,") && text.contains("chosen on the line"),
        "{text}"
    );
}
