//! Target and ref checks:
//! `repo-gateway` refuses, before any repository read or push and with the exact reason, every
//! `cc:` target outside the `hamh-e2e-` prefix, outside the allowlist or outside the home account and
//! Region (the `accounts.json` entry the build selected), every `gh:` target while `app` mode is not installed, every `gl:` target, and every
//! push to a ref other than `refs/heads/harness/<runId>`.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

use delivery::checks::{
    BUILD_FILE, COMMENT_MAX_CHARS, DESTINATION_REF, NO_CHECK_NOTE, PR_DESCRIPTION_MAX_CHARS,
    PR_TITLE_MAX_CHARS, PRCHECK_RULE, check_allowlisted, check_comment, check_head_sha,
    check_new_repo_name, check_pr_text, check_repository_arn, codecommit_arn, codecommit_name,
    harness_ref, is_build_file, normalize_base_ref, pr_console_url, prcheck_rule_content, push_ref,
};
use delivery::error::DeliveryError;
use delivery::git::TreeEntry;
use harness_protocol::store::RepoAllowItem;
use harness_protocol::target::{E2E_ACCOUNT as ACCOUNT, E2E_REGION as REGION, RepoTarget};
use serde_json::{Value, json};

const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";

/// A pinned inference Region other than the home Region.
fn other_region() -> &'static str {
    if REGION == "us-east-1" {
        "us-west-2"
    } else {
        "us-east-1"
    }
}

/// A 12-digit account number other than the stage account, whichever `accounts.json` entry the build selected (`ACCOUNT` is the account the build embeds).
fn other_account() -> &'static str {
    if ACCOUNT == "123456789012" {
        "210987654321"
    } else {
        "123456789012"
    }
}

fn target(s: &str) -> RepoTarget {
    s.parse().unwrap()
}

fn refused(r: Result<impl std::fmt::Debug, DeliveryError>) -> String {
    match r {
        Err(DeliveryError::Refused(reason)) => reason,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn allow(t: &str) -> RepoAllowItem {
    RepoAllowItem {
        target: target(t),
        added_by: format!("arn:aws:sts::{ACCOUNT}:assumed-role/Admin/testuser"),
        added_at: "2026-09-25T20:00:00Z".into(),
    }
}

#[test]
fn a_ham_e2e_codecommit_target_yields_its_name() {
    assert_eq!(
        codecommit_name(&target("cc:hamh-e2e-python-calc-20260925")).unwrap(),
        "hamh-e2e-python-calc-20260925"
    );
}

#[test]
fn a_codecommit_target_outside_the_prefix_is_refused_with_the_reason() {
    for name in [
        "python-calc",
        "ham-e2e",
        "hamh-live-harness",
        "HAM-E2E-x",
        "xhamh-e2e-y",
    ] {
        let reason = refused(codecommit_name(&target(&format!("cc:{name}"))));
        assert_eq!(
            reason,
            format!("refused: cc:{name} is outside the hamh-e2e- prefix")
        );
    }
}

#[test]
fn a_github_target_is_refused_because_app_mode_is_not_installed() {
    let reason = refused(codecommit_name(&target("gh:ex-sample1/hamh-e2e-fixture")));
    assert_eq!(
        reason,
        "refused: gh:ex-sample1/hamh-e2e-fixture needs the tenant GitHub App (app mode), which is not installed"
    );
}

#[test]
fn a_gitlab_target_is_refused_because_gitlab_delivery_is_not_supported() {
    let reason = refused(codecommit_name(&target(
        "gl:testuser/hyperscale-agent-meta-harness-output",
    )));
    assert_eq!(
        reason,
        "refused: gl:testuser/hyperscale-agent-meta-harness-output is a GitLab target, which is not supported; repo-gateway makes no GitLab call"
    );
}

#[test]
fn the_allowlist_entry_must_exist_and_name_the_target() {
    let t = target("cc:hamh-e2e-python-calc-20260925");
    assert!(check_allowlisted(&t, Some(&allow("cc:hamh-e2e-python-calc-20260925"))).is_ok());
    assert_eq!(
        refused(check_allowlisted(&t, None)),
        "refused: cc:hamh-e2e-python-calc-20260925 is not on the allowlist of this stage; an operator allows it with `ham-admin repos allow cc:hamh-e2e-python-calc-20260925`"
    );
    // An item stored under the key whose body names another repository never allows this one.
    assert_eq!(
        refused(check_allowlisted(&t, Some(&allow("cc:hamh-e2e-other")))),
        format!(
            "refused: the allowlist entry REPOALLOW#cc:{REGION}:hamh-e2e-python-calc-20260925 names cc:hamh-e2e-other"
        )
    );
}

#[test]
fn the_codecommit_arn_is_account_region_and_name() {
    assert_eq!(
        codecommit_arn("hamh-e2e-x"),
        format!("arn:aws:codecommit:{REGION}:{ACCOUNT}:hamh-e2e-x")
    );
}

#[test]
fn a_repository_in_the_e2e_account_and_region_passes() {
    let arn = format!("arn:aws:codecommit:{REGION}:{ACCOUNT}:hamh-e2e-x");
    assert!(check_repository_arn("hamh-e2e-x", Some(&arn), Some(ACCOUNT)).is_ok());
}

#[test]
fn a_repository_in_another_account_is_refused() {
    let other = other_account();
    let arn = format!("arn:aws:codecommit:{REGION}:{other}:hamh-e2e-x");
    assert_eq!(
        refused(check_repository_arn("hamh-e2e-x", Some(&arn), Some(other))),
        format!("refused: repository {arn} is outside {ACCOUNT} {REGION}")
    );
    // The account field alone disagreeing is enough.
    let ours = format!("arn:aws:codecommit:{REGION}:{ACCOUNT}:hamh-e2e-x");
    assert_eq!(
        refused(check_repository_arn("hamh-e2e-x", Some(&ours), Some(other))),
        format!("refused: repository {ours} is outside {ACCOUNT} {REGION}")
    );
}

#[test]
fn a_repository_in_another_region_is_refused() {
    let arn = format!("arn:aws:codecommit:{}:{ACCOUNT}:hamh-e2e-x", other_region());
    assert_eq!(
        refused(check_repository_arn(
            "hamh-e2e-x",
            Some(&arn),
            Some(ACCOUNT)
        )),
        format!("refused: repository {arn} is outside {ACCOUNT} {REGION}")
    );
}

#[test]
fn a_repository_arn_for_another_name_or_no_arn_is_refused() {
    let arn = format!("arn:aws:codecommit:{REGION}:{ACCOUNT}:hamh-e2e-y");
    assert_eq!(
        refused(check_repository_arn(
            "hamh-e2e-x",
            Some(&arn),
            Some(ACCOUNT)
        )),
        format!("refused: repository {arn} is not cc:hamh-e2e-x")
    );
    assert_eq!(
        refused(check_repository_arn("hamh-e2e-x", None, Some(ACCOUNT))),
        "refused: CodeCommit returned no ARN for cc:hamh-e2e-x"
    );
}

#[test]
fn the_harness_ref_is_built_from_a_ulid_run_id_only() {
    assert_eq!(
        harness_ref(RUN).unwrap().as_str(),
        format!("refs/heads/harness/{RUN}")
    );
    for bad in [
        "",
        "run-1",
        "../main",
        "01K5ZQ4T8M3N6P7R9S0V1W2X3",
        "01K5ZQ4T8M3N6P7R9S0V1W2X3Y/x",
    ] {
        assert!(
            matches!(harness_ref(bad), Err(DeliveryError::Invalid(_))),
            "{bad:?}"
        );
    }
}

#[test]
fn a_push_to_the_harness_branch_is_accepted_in_short_or_full_form() {
    let full = format!("refs/heads/harness/{RUN}");
    assert_eq!(push_ref(RUN, None).unwrap().as_str(), full);
    assert_eq!(push_ref(RUN, Some(&full)).unwrap().as_str(), full);
    assert_eq!(
        push_ref(RUN, Some(&format!("harness/{RUN}")))
            .unwrap()
            .as_str(),
        full
    );
}

#[test]
fn a_push_to_any_other_ref_is_refused_with_the_reason() {
    let other_run = "01K5ZQ4T8M3N6P7R9S0V1W2X00";
    for bad in [
        "refs/heads/main".to_string(),
        "main".to_string(),
        "refs/heads/prod".to_string(),
        "refs/tags/v1".to_string(),
        format!("refs/heads/harness/{other_run}"),
        format!("refs/heads/harness/{RUN}/x"),
        format!("refs/heads/harness/{RUN}:refs/heads/main"),
        format!("+refs/heads/harness/{RUN}"),
        "refs/heads/harness/".to_string(),
        "HEAD".to_string(),
    ] {
        assert_eq!(
            refused(push_ref(RUN, Some(&bad))),
            format!("refused: push to {bad}; repo-gateway pushes only refs/heads/harness/{RUN}"),
            "{bad}"
        );
    }
}

#[test]
fn new_repo_names_are_ham_e2e_new_or_ntier_and_a_calendar_date() {
    assert!(check_new_repo_name("hamh-e2e-new-20260925").is_ok());
    assert!(check_new_repo_name("hamh-e2e-new-20240229").is_ok());
    assert!(check_new_repo_name("hamh-e2e-ntier-web-20261001").is_ok());
    assert!(check_new_repo_name("hamh-e2e-ntier-enterprise-20261001").is_ok());
    for bad in [
        "hamh-e2e-python-calc-20260925",
        "hamh-e2e-new-2026092",
        "hamh-e2e-new-20261301",
        "hamh-e2e-new-20260931",
        "hamh-e2e-new-20250229",
        "hamh-e2e-new-20260925-x",
        "hamh-e2e-new-",
        "hamh-e2e-ntier-web-20261301",
        "hamh-e2e-ntier-20261001",
    ] {
        assert_eq!(
            refused(check_new_repo_name(bad)),
            format!(
                "refused: cc:{bad} is not a new-repo name; use cc:hamh-e2e-new-<yyyymmdd> or cc:hamh-e2e-ntier-<kind>-<yyyymmdd>"
            ),
            "{bad}"
        );
    }
}

#[test]
fn base_refs_normalize_to_branch_refs_and_others_are_invalid() {
    assert_eq!(normalize_base_ref("main").unwrap(), "refs/heads/main");
    assert_eq!(
        normalize_base_ref("refs/heads/main").unwrap(),
        "refs/heads/main"
    );
    assert_eq!(
        normalize_base_ref("release/1.2").unwrap(),
        "refs/heads/release/1.2"
    );
    for bad in [
        "",
        "refs/tags/v1",
        "a..b",
        "x y",
        "-main",
        "main/",
        "main.lock",
        "@{u}",
        "a//b",
    ] {
        assert!(
            matches!(normalize_base_ref(bad), Err(DeliveryError::Invalid(_))),
            "{bad:?}"
        );
    }
}

#[test]
fn head_shas_are_forty_lowercase_hex() {
    assert!(check_head_sha(&"a".repeat(40)).is_ok());
    for bad in ["", "abc", &"A".repeat(40), &"g".repeat(40), &"a".repeat(41)] {
        assert!(
            matches!(check_head_sha(bad), Err(DeliveryError::Invalid(_))),
            "{bad:?}"
        );
    }
}

#[test]
fn pr_text_limits_are_the_codecommit_limits() {
    assert_eq!(PR_TITLE_MAX_CHARS, 150);
    assert_eq!(PR_DESCRIPTION_MAX_CHARS, 10_240);
    assert_eq!(COMMENT_MAX_CHARS, 10_240);
    // Characters, not bytes: a title of 150 multi-byte characters fits.
    assert!(check_pr_text(&"é".repeat(150), &"d".repeat(10_240)).is_ok());
    assert!(matches!(
        check_pr_text(&"t".repeat(151), "d"),
        Err(DeliveryError::Invalid(m)) if m.contains("title has 151 characters")
    ));
    assert!(matches!(
        check_pr_text("t", &"d".repeat(10_241)),
        Err(DeliveryError::Invalid(m)) if m.contains("description has 10241 characters")
    ));
    assert!(matches!(
        check_pr_text("  ", "d"),
        Err(DeliveryError::Invalid(_))
    ));
    assert!(check_comment(&"c".repeat(10_240)).is_ok());
    assert!(matches!(
        check_comment(&"c".repeat(10_241)),
        Err(DeliveryError::Invalid(_))
    ));
    assert!(matches!(check_comment(""), Err(DeliveryError::Invalid(_))));
}

#[test]
fn the_prcheck_rule_names_one_pool_member_the_stage_check_build_role() {
    assert_eq!(PRCHECK_RULE, "hamh-prcheck-required");
    assert_eq!(DESTINATION_REF, "refs/heads/main");
    let v: Value = serde_json::from_str(&prcheck_rule_content("live")).unwrap();
    assert_eq!(
        v,
        json!({"Version": "2018-11-08", "Statements": [{
            "Type": "Approvers", "NumberOfApprovalsNeeded": 1,
            "ApprovalPoolMembers": [format!("arn:aws:sts::{ACCOUNT}:assumed-role/hamh-live-prcheck-build/*")]
        }]})
    );
    let drill: Value = serde_json::from_str(&prcheck_rule_content("drill")).unwrap();
    assert_eq!(
        drill["Statements"][0]["ApprovalPoolMembers"],
        json!([format!(
            "arn:aws:sts::{ACCOUNT}:assumed-role/hamh-drill-prcheck-build/*"
        )])
    );
}

#[test]
fn the_pr_link_is_the_codecommit_console_page() {
    assert_eq!(
        pr_console_url("hamh-e2e-python-calc-20260925", "2"),
        format!(
            "https://{REGION}.console.aws.amazon.com/codesuite/codecommit/repositories/hamh-e2e-python-calc-20260925/pull-requests/2/details"
        )
    );
}

fn entry(mode: &str, kind: &str, path: &str) -> TreeEntry {
    TreeEntry {
        mode: mode.into(),
        kind: kind.into(),
        sha: "a".repeat(40),
        size: None,
        path: path.into(),
    }
}

#[test]
fn only_a_regular_file_named_buildspec_yml_at_the_root_is_the_build_file() {
    // The PR check runs the source commit's root buildspec.yml (CodeBuild setting `buildspec.yml`), so the check is attached only for
    // an entry CodeBuild can read as that file. A link is left out: the validator protects the link, not the file it points at, so a run
    // could edit the target and write the check that approves its own pull request.
    assert_eq!(BUILD_FILE, "buildspec.yml");
    assert!(is_build_file(&entry("100644", "blob", "buildspec.yml")));
    assert!(is_build_file(&entry("100755", "blob", "buildspec.yml")));
    for (mode, kind, path) in [
        ("120000", "blob", "buildspec.yml"),    // a symbolic link
        ("160000", "commit", "buildspec.yml"),  // a submodule
        ("040000", "tree", "buildspec.yml"),    // a folder of that name
        ("100644", "blob", "ci/buildspec.yml"), // nested
        ("100644", "blob", "Buildspec.yml"),    // another spelling
        ("100644", "blob", "buildspec.yaml"),
        ("100644", "blob", "buildspec.yml "),
        ("100644", "blob", ""),
    ] {
        assert!(
            !is_build_file(&entry(mode, kind, path)),
            "{mode} {kind} {path:?}"
        );
    }
}

#[test]
fn the_note_on_an_unchecked_pull_request_says_what_is_missing_and_claims_nothing_else() {
    for part in [
        "No pull request check is attached.",
        "buildspec.yml",
        "hamh-prcheck-required",
        "does not block merging",
        "Review the changes yourself",
    ] {
        assert!(NO_CHECK_NOTE.contains(part), "{part}: {NO_CHECK_NOTE}");
    }
    // Root invariant 1: the product never calls a run verified, tested, gated or evaluated; the note must not either, and it is a
    // legal comment.
    assert_eq!(
        harness_protocol::direct::banned_claim_word(NO_CHECK_NOTE),
        None
    );
    check_comment(NO_CHECK_NOTE).unwrap();
    assert!(
        NO_CHECK_NOTE.len() < 600,
        "a short note: {}",
        NO_CHECK_NOTE.len()
    );
}
