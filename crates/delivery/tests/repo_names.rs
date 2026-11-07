//! The repository-name rule of a stage (`delivery::repo_names`) and the target check that applies it (`checks::codecommit_name_in`). The rule is the list in
//! `HAMH_REPO_NAMES` (the CDK app sets it from the `repoNames` of the account-file entry); without it the rule is the default `hamh-e2e-*`. The words are one table,
//! `infra/test/fixtures/repo-names.json`, which the TypeScript rules, `scripts/lib/aws_target.py` and the copy of this matcher in the `control-api` crate run as well.
//! A repository the harness itself names (`hamh-e2e-new-<yyyymmdd>`, `hamh-e2e-ntier-<kind>-<yyyymmdd>`, made by `--new-repo`) is covered whatever the rule says.
#![allow(clippy::unwrap_used)]

use delivery::checks::{codecommit_name, codecommit_name_in};
use delivery::error::DeliveryError;
use delivery::repo_names::{EARLIER_DEFAULT_PREFIX, RepoNames};
use harness_protocol::target::RepoTarget;
use serde_json::Value;

const TABLE: &str = include_str!("../../../infra/test/fixtures/repo-names.json");

fn table() -> Value {
    serde_json::from_str(TABLE).unwrap()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

fn target(t: &str) -> RepoTarget {
    t.parse().unwrap()
}

fn refused<T: std::fmt::Debug>(r: Result<T, DeliveryError>) -> String {
    match r {
        Err(DeliveryError::Refused(m)) => m,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn the_default_rule_is_the_prefix_ham_e2e() {
    let d = RepoNames::default();
    assert_eq!(d.entries(), ["hamh-e2e-*"]);
    assert!(d.is_default());
    assert_eq!(strings(&table()["default"]), d.entries());
}

#[test]
fn every_accepted_rule_parses_to_the_list_of_the_table() {
    for row in table()["accept"].as_array().unwrap() {
        let rule = strings(&row["rule"]);
        assert_eq!(
            RepoNames::from_entries(&rule).unwrap().entries(),
            rule,
            "{}",
            row["why"]
        );
        assert_eq!(
            RepoNames::parse(&rule.join(",")).unwrap().entries(),
            rule,
            "{}",
            row["why"]
        );
    }
}

#[test]
fn every_environment_row_parses_to_the_rule_of_the_table() {
    for row in table()["env"].as_array().unwrap() {
        let rule = strings(&row["rule"]);
        let env = row["env"].as_str().unwrap().to_string();
        let got =
            RepoNames::from_lookup(|k| (k == "HAMH_REPO_NAMES").then(|| env.clone())).unwrap();
        assert_eq!(got.entries(), rule, "{}", row["why"]);
    }
}

#[test]
fn an_unset_or_empty_variable_is_the_default() {
    assert!(RepoNames::from_lookup(|_| None).unwrap().is_default());
    assert!(
        RepoNames::from_lookup(|_| Some(String::new()))
            .unwrap()
            .is_default()
    );
}

#[test]
fn every_refused_variable_is_a_refusal_with_the_reason_of_the_table() {
    for row in table()["envRefuse"].as_array().unwrap() {
        let env = row["env"].as_str().unwrap().to_string();
        let err =
            RepoNames::from_lookup(|k| (k == "HAMH_REPO_NAMES").then(|| env.clone())).unwrap_err();
        assert_eq!(
            err,
            format!("HAMH_REPO_NAMES: {}", row["reason"].as_str().unwrap()),
            "{}",
            row["why"]
        );
    }
}

#[test]
fn every_cover_row_agrees_with_the_matcher() {
    for row in table()["cover"].as_array().unwrap() {
        let rule = RepoNames::from_entries(&strings(&row["rule"])).unwrap();
        for name in strings(&row["allows"]) {
            assert!(rule.allows(&name), "{}: {name}", row["why"]);
        }
        for name in strings(&row["refuses"]) {
            assert!(!rule.allows(&name), "{}: {name}", row["why"]);
        }
    }
}

#[test]
fn the_refusal_text_is_the_one_of_the_table() {
    for row in table()["refusal"].as_array().unwrap() {
        let rule = RepoNames::from_entries(&strings(&row["rule"])).unwrap();
        assert_eq!(
            rule.refusal(row["name"].as_str().unwrap()),
            row["text"].as_str().unwrap()
        );
    }
}

#[test]
fn the_default_rule_gives_the_answers_of_the_fixed_prefix_check() {
    let d = RepoNames::default();
    for name in ["hamh-e2e-python-calc-20260925", "hamh-e2e-new-20260925"] {
        let t = target(&format!("cc:{name}"));
        assert_eq!(codecommit_name_in(&t, &d).unwrap(), name);
        assert_eq!(codecommit_name(&t).unwrap(), name);
    }
    for name in ["python-calc", "xhamh-e2e-y", "ham-e2e", "HAM-E2E-x"] {
        let t = target(&format!("cc:{name}"));
        let want = format!("refused: cc:{name} is outside the hamh-e2e- prefix");
        assert_eq!(refused(codecommit_name_in(&t, &d)), want);
        assert_eq!(refused(codecommit_name(&t)), want);
    }
}

#[test]
fn a_configured_rule_covers_its_names_and_refuses_the_rest_with_the_rule_in_the_text() {
    let rule = RepoNames::parse("my-app,team-*").unwrap();
    assert_eq!(
        codecommit_name_in(&target("cc:my-app"), &rule).unwrap(),
        "my-app"
    );
    assert_eq!(
        codecommit_name_in(&target("cc:team-b"), &rule).unwrap(),
        "team-b"
    );
    assert_eq!(
        refused(codecommit_name_in(
            &target("cc:hamh-e2e-python-calc"),
            &rule
        )),
        "refused: cc:hamh-e2e-python-calc is outside the repository names this stage allows (my-app, team-*)"
    );
    assert_eq!(
        refused(codecommit_name_in(&target("cc:my-app-2"), &rule)),
        "refused: cc:my-app-2 is outside the repository names this stage allows (my-app, team-*)"
    );
}

#[test]
fn a_rule_never_widens_the_other_schemes() {
    let any = RepoNames::parse("*").unwrap();
    let gh = refused(codecommit_name_in(&target("gh:ex-sample1/app"), &any));
    assert!(gh.contains("tenant GitHub App"), "{gh}");
    let gl = refused(codecommit_name_in(&target("gl:group/app"), &any));
    assert!(gl.contains("GitLab target"), "{gl}");
}

#[test]
fn the_names_the_harness_gives_its_own_repositories_are_covered_whatever_the_rule_says() {
    let rule = RepoNames::parse("my-app").unwrap();
    for name in [
        "hamh-e2e-new-20260925",
        "hamh-e2e-ntier-web-20261001",
        "hamh-e2e-ntier-events-20261001",
    ] {
        assert_eq!(
            codecommit_name_in(&target(&format!("cc:{name}")), &rule).unwrap(),
            name
        );
    }
    // Only the exact forms: a look-alike is outside the rule.
    for name in [
        "hamh-e2e-new-20260925-x",
        "hamh-e2e-new-20261301",
        "hamh-e2e-ntier-web",
        "hamh-e2e-python-calc",
    ] {
        let reason = refused(codecommit_name_in(&target(&format!("cc:{name}")), &rule));
        assert!(
            reason.contains("outside the repository names this stage allows (my-app)"),
            "{name}: {reason}"
        );
    }
}

#[test]
fn the_earlier_default_prefix_is_the_one_the_earlier_release_used() {
    // COMPAT-CAH-DATA: repositories made under the earlier names keep that prefix for good.
    assert_eq!(EARLIER_DEFAULT_PREFIX, "cah-e2e-"); // COMPAT-CAH-DATA
    assert_ne!(EARLIER_DEFAULT_PREFIX, "hamh-e2e-");
}

#[test]
fn the_default_rule_also_covers_the_repositories_made_under_the_earlier_names() {
    // The rule itself lists the new prefix only; the earlier prefix is an allowance of the default rule, and the operator's allowlist still decides
    // which repository a run may reach.
    let d = RepoNames::default();
    for rest in ["python-calc", "new-20260925", "ntier-web-20261001", "x"] {
        let name = format!("{EARLIER_DEFAULT_PREFIX}{rest}");
        assert!(d.allows_earlier_name(&name), "{name}");
        assert!(
            !d.allows(&name),
            "the rule lists the new prefix only: {name}"
        );
    }
    assert_eq!(d.entries(), ["hamh-e2e-*"]);
}

#[test]
fn only_the_exact_earlier_prefix_at_the_start_of_a_name_is_covered() {
    let d = RepoNames::default();
    let bare = EARLIER_DEFAULT_PREFIX.trim_end_matches('-');
    for name in [
        bare.to_string(),
        format!("x{EARLIER_DEFAULT_PREFIX}y"),
        format!("a-{EARLIER_DEFAULT_PREFIX}y"),
        EARLIER_DEFAULT_PREFIX.to_uppercase() + "x",
        "python-calc".to_string(),
        "hamh-e2e-x".to_string(),
    ] {
        assert!(!d.allows_earlier_name(&name), "{name}");
    }
}

#[test]
fn a_rule_covers_the_earlier_prefix_only_when_it_lists_the_default_entry() {
    // The default entry (`hamh-e2e-*`) stands for the prefix of the earlier release: a rule that lists it, alone or among other entries, also covers the
    // earlier prefix. A rule without it covers exactly what it lists.
    let name = format!("{EARLIER_DEFAULT_PREFIX}python-calc");
    for rule in ["hamh-e2e-*", "my-app,hamh-e2e-*", "hamh-e2e-*,team-*"] {
        let r = RepoNames::parse(rule).unwrap();
        assert!(r.allows_earlier_name(&name), "{rule}");
        assert!(
            !r.allows(&name),
            "{rule}: the entries list the new prefix only"
        );
    }
    for rule in ["my-app", "team-*", "hamh-e2e-new-*", "hamh-e2e-python-calc"] {
        let r = RepoNames::parse(rule).unwrap();
        assert!(!r.allows_earlier_name(&name), "{rule}");
    }
    // A rule that lists the earlier prefix itself, or `*`, covers it as any entry does.
    assert!(
        RepoNames::parse(&format!("{EARLIER_DEFAULT_PREFIX}*"))
            .unwrap()
            .allows(&name)
    );
    assert!(RepoNames::parse("*").unwrap().allows(&name));
}

#[test]
fn the_default_entry_serves_an_earlier_repository_and_a_rule_without_it_does_not() {
    let name = format!("{EARLIER_DEFAULT_PREFIX}python-calc-20260925");
    let t = target(&format!("cc:{name}"));
    assert_eq!(codecommit_name(&t).unwrap(), name);
    assert_eq!(codecommit_name_in(&t, &RepoNames::default()).unwrap(), name);
    // A rule that lists the default entry among others serves it too.
    let with_default = RepoNames::parse("my-app,hamh-e2e-*").unwrap();
    assert_eq!(codecommit_name_in(&t, &with_default).unwrap(), name);
    // A rule without the default entry covers what it lists.
    let rule = RepoNames::parse("my-app").unwrap();
    assert_eq!(
        refused(codecommit_name_in(&t, &rule)),
        format!("refused: cc:{name} is outside the repository names this stage allows (my-app)")
    );
    // The names of the other schemes stay refused whatever the repository name is.
    let gh = refused(codecommit_name(&target("gh:ex-sample1/app")));
    assert!(gh.contains("tenant GitHub App"), "{gh}");
}

#[test]
fn the_default_rule_keeps_its_text_for_an_earlier_prefix_look_alike() {
    // Only the exact earlier prefix at the start is an allowance; a name that merely contains it is outside the rule, with the text of the fixed prefix.
    for name in [
        format!("x{EARLIER_DEFAULT_PREFIX}y"),
        EARLIER_DEFAULT_PREFIX.trim_end_matches('-').to_string(),
    ] {
        let t = target(&format!("cc:{name}"));
        assert_eq!(
            refused(codecommit_name(&t)),
            format!("refused: cc:{name} is outside the hamh-e2e- prefix")
        );
    }
}
