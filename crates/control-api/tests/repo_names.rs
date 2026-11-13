//! The repository-name rule of a stage (`control_api::repo_names`): which CodeCommit repository names an operator may allow. The rule is the list in
//! `HAMH_REPO_NAMES` (the CDK app sets it from the `repoNames` of the account-file entry); without it the rule is the default `hamh-e2e-*`. The words are one table,
//! `infra/test/fixtures/repo-names.json`, which the TypeScript rules (`infra/lib/repo-names.ts`), `scripts/lib/aws_target.py` and the copy of this matcher in the
//! `delivery` crate run as well.
#![allow(clippy::unwrap_used)]

use control_api::repo_names::{EARLIER_DEFAULT_PREFIX, RepoNames};
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

#[test]
fn the_default_rule_is_the_prefix_ham_e2e() {
    let d = RepoNames::default();
    assert_eq!(d.entries(), ["hamh-e2e-*"]);
    assert!(d.is_default());
    assert_eq!(strings(&table()["default"]), d.entries());
    assert!(!RepoNames::parse("my-app").unwrap().is_default());
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
fn every_refused_list_of_strings_is_a_refusal_with_the_reason_of_the_table() {
    let mut seen = 0;
    for row in table()["refuse"].as_array().unwrap() {
        let Some(list) = row["entry"]["repoNames"].as_array() else {
            continue;
        };
        let Some(entries) = list
            .iter()
            .map(|e| e.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        seen += 1;
        assert_eq!(
            RepoNames::from_entries(&entries).unwrap_err(),
            row["reason"].as_str().unwrap(),
            "{}",
            row["why"]
        );
    }
    assert!(seen >= 10, "only {seen} rows were run");
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

/// The rule took 8 entries and takes 10 now. The cap is measured against the policies, the event pattern and the environment of a Lambda function
/// (infra/test/delivery-repo-names.test.ts), so a later raise is one more measurement and not a guess.
#[test]
fn b776w_ten_entries_are_a_rule_and_eleven_are_not() {
    let ten: Vec<String> = (1..=10).map(|i| format!("repo-{i}")).collect();
    assert_eq!(RepoNames::from_entries(&ten).unwrap().entries(), ten);
    assert_eq!(RepoNames::parse(&ten.join(",")).unwrap().entries(), ten);
    let eleven: Vec<String> = (1..=11).map(|i| format!("repo-{i}")).collect();
    let why = RepoNames::from_entries(&eleven).unwrap_err();
    assert_eq!(
        why,
        "repoNames must be a list of 1 to 10 repository names, like [\"my-app\", \"team-*\"]"
    );
    assert_eq!(RepoNames::parse(&eleven.join(",")).unwrap_err(), why);
}

#[test]
fn b776w_the_longest_name_still_fits_ten_times_in_the_environment() {
    // 10 names of 100 characters and 9 commas: the value a function carries in HAMH_REPO_NAMES, well under the 4,096 bytes of a function's environment.
    let longest: Vec<String> = (0..10)
        .map(|i| format!("{i:x}{}", "a".repeat(99)))
        .collect();
    assert!(longest.iter().all(|n| n.len() == 100));
    let rule = RepoNames::from_entries(&longest).unwrap();
    assert_eq!(rule.entries().len(), 10);
    assert!(longest.join(",").len() < 1_100);
}
