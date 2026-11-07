// COMPAT-CAH-DATA: this file names the earlier records folder and the earlier variable spellings, which are read until VERSION 6.00.
//! Repository target schemes.

use harness_protocol::target::{
    ACCOUNTS, CredentialMode, E2E_ACCOUNT, E2E_REGION, NEW_REPO_PREFIXES_EARLIER, RepoTarget,
    TargetError, new_repo_name_earlier, new_repo_refusal, new_repo_refusal_earlier,
};

#[test]
fn codecommit_targets_parse_and_format() {
    let t: RepoTarget = "cc:hamh-e2e-python-calc-20260925".parse().unwrap();
    assert_eq!(
        t,
        RepoTarget::CodeCommit {
            name: "hamh-e2e-python-calc-20260925".to_string()
        }
    );
    assert_eq!(t.to_string(), "cc:hamh-e2e-python-calc-20260925");
    assert_eq!(t.scheme(), "cc");
    assert_eq!(t.credential_mode(), CredentialMode::Iam);
    // The key names the home Region of the accounts.json entry the build selected.
    assert_eq!(
        t.allowlist_key(),
        format!("REPOALLOW#cc:{E2E_REGION}:hamh-e2e-python-calc-20260925")
    );
    assert!(
        ACCOUNTS
            .iter()
            .any(|(a, r, _, _)| (*a, *r) == (E2E_ACCOUNT, E2E_REGION))
    );
}

#[test]
fn github_targets_need_app_mode() {
    let t: RepoTarget = "gh:ex-sample1/hamh-e2e-fixture".parse().unwrap();
    assert_eq!(
        t,
        RepoTarget::GitHub {
            owner: "ex-sample1".to_string(),
            name: "hamh-e2e-fixture".to_string()
        }
    );
    assert_eq!(t.to_string(), "gh:ex-sample1/hamh-e2e-fixture");
    assert_eq!(t.credential_mode(), CredentialMode::App);
    assert_eq!(t.allowlist_key(), "REPOALLOW#ex-sample1/hamh-e2e-fixture");
}

#[test]
fn gitlab_targets_carry_the_full_project_path() {
    let t: RepoTarget = "gl:testuser/hyperscale-agent-meta-harness-output"
        .parse()
        .unwrap();
    assert_eq!(
        t,
        RepoTarget::GitLab {
            path: "testuser/hyperscale-agent-meta-harness-output".to_string()
        }
    );
    assert_eq!(t.credential_mode(), CredentialMode::GitlabCi);
    assert_eq!(
        t.allowlist_key(),
        "REPOALLOW#gl:testuser/hyperscale-agent-meta-harness-output"
    );
    let nested: RepoTarget = "gl:group/sub.group/project_1".parse().unwrap();
    assert_eq!(nested.to_string(), "gl:group/sub.group/project_1");
}

#[test]
fn invalid_targets_are_refused_with_a_reason() {
    for bad in [
        "",
        "hamh-e2e-x",
        "s3:bucket",
        "https://github.com/a/b",
        "bb:a/b",
    ] {
        let err = bad.parse::<RepoTarget>().unwrap_err();
        assert!(matches!(err, TargetError::UnknownScheme(_)), "{bad}: {err}");
    }
    let long = format!("cc:{}", "a".repeat(101));
    for bad in [
        "cc:",
        "cc:x.git",
        "cc:has space",
        "cc:a/b",
        long.as_str(),
        "gh:owner",
        "gh:/name",
        "gh:-owner/name",
        "gh:owner-/name",
        "gh:own_er/name",
        "gh:a/b/c",
        "gh:owner/",
        "gl:single",
        "gl:/a",
        "gl:a//b",
        "gl:a/b/",
        "gl:a/b c",
    ] {
        let err = bad.parse::<RepoTarget>().unwrap_err();
        assert!(
            matches!(err, TargetError::InvalidName { .. }),
            "{bad}: {err}"
        );
        assert!(!err.to_string().is_empty());
    }
}

#[test]
fn targets_serialize_as_strings() {
    let t: RepoTarget = "cc:hamh-e2e-new-20260925".parse().unwrap();
    assert_eq!(
        serde_json::to_value(&t).unwrap(),
        serde_json::json!("cc:hamh-e2e-new-20260925")
    );
    let back: RepoTarget = serde_json::from_value(serde_json::json!("gh:o/n")).unwrap();
    assert_eq!(back.to_string(), "gh:o/n");
    assert!(serde_json::from_value::<RepoTarget>(serde_json::json!("cc:x.git")).is_err());
}

#[test]
fn new_repo_names_are_the_greenfield_forms_with_a_calendar_date() {
    // (`hamh-e2e-new-<yyyymmdd>`) and (`hamh-e2e-ntier-<kind>-<yyyymmdd>`).
    for ok in [
        "hamh-e2e-new-20260925",
        "hamh-e2e-new-20240229",
        "hamh-e2e-ntier-web-20261001",
        "hamh-e2e-ntier-events-20261001",
        "hamh-e2e-ntier-enterprise-20261231",
        "hamh-e2e-ntier-a1-20261001",
    ] {
        assert_eq!(new_repo_refusal(ok), None, "{ok}");
    }
    let long_kind = format!("hamh-e2e-ntier-{}-20261001", "k".repeat(33));
    for bad in [
        "hamh-e2e-python-calc-20260925",
        "hamh-e2e-python-calc",
        "hamh-e2e-new-2026092",
        "hamh-e2e-new-20261301",
        "hamh-e2e-new-20260931",
        "hamh-e2e-new-20250229",
        "hamh-e2e-new-20260925-x",
        "hamh-e2e-new-",
        "hamh-e2e-ntier-20261001",
        "hamh-e2e-ntier--20261001",
        "hamh-e2e-ntier-Web-20261001",
        "hamh-e2e-ntier-web-2026100",
        "hamh-e2e-ntier-web-20261032",
        "hamh-e2e-ntier-web_x-20261001",
        "hamh-e2e-ntier-a-b-20261001",
        long_kind.as_str(),
        "new-20260925",
    ] {
        assert_eq!(
            new_repo_refusal(bad),
            Some(format!(
                "refused: cc:{bad} is not a new-repo name; use cc:hamh-e2e-new-<yyyymmdd> or cc:hamh-e2e-ntier-<kind>-<yyyymmdd>"
            )),
            "{bad}"
        );
    }
}

/// COMPAT-CAH-DATA: a backend of the earlier stage generation creates `cah-e2e-new-<yyyymmdd>` and `cah-e2e-ntier-<kind>-<yyyymmdd>` only; the
/// forms follow the same grammar, and the one reason names the forms of the generation asked about.
#[test]
fn the_earlier_generation_has_the_same_new_repo_forms_under_its_prefixes() {
    assert_eq!(
        NEW_REPO_PREFIXES_EARLIER,
        ["cah-e2e-new-", "cah-e2e-ntier-"]
    );
    for ok in [
        "cah-e2e-new-20260925",
        "cah-e2e-new-20240229",
        "cah-e2e-ntier-web-20261001",
        "cah-e2e-ntier-a1-20261001",
    ] {
        assert_eq!(new_repo_refusal_earlier(ok), None, "{ok}");
        assert!(
            new_repo_refusal(ok).is_some(),
            "{ok} is not a form of the current generation"
        );
    }
    for ok in ["hamh-e2e-new-20260925", "hamh-e2e-ntier-web-20261001"] {
        assert!(
            new_repo_refusal_earlier(ok).is_some(),
            "{ok} is not a form of the earlier generation"
        );
    }
    assert_eq!(
        new_repo_refusal_earlier("cah-e2e-new-20261301"),
        Some("refused: cc:cah-e2e-new-20261301 is not a new-repo name; use cc:cah-e2e-new-<yyyymmdd> or cc:cah-e2e-ntier-<kind>-<yyyymmdd>".to_string())
    );
}

#[test]
fn a_current_new_repo_name_maps_to_the_earlier_generations_name() {
    assert_eq!(
        new_repo_name_earlier("hamh-e2e-new-20260925").as_deref(),
        Some("cah-e2e-new-20260925")
    );
    assert_eq!(
        new_repo_name_earlier("hamh-e2e-ntier-web-20261001").as_deref(),
        Some("cah-e2e-ntier-web-20261001")
    );
    for other in [
        "cah-e2e-new-20260925",
        "python-calc",
        "hamh-e2e-python-calc",
        "",
    ] {
        assert_eq!(new_repo_name_earlier(other), None, "{other}");
    }
    // A mapped name is a form of the earlier generation, and its source is a form of the current one.
    for name in ["hamh-e2e-new-20260925", "hamh-e2e-ntier-web-20261001"] {
        assert_eq!(new_repo_refusal(name), None, "{name}");
        let earlier = new_repo_name_earlier(name).unwrap();
        assert_eq!(new_repo_refusal_earlier(&earlier), None, "{earlier}");
    }
}
