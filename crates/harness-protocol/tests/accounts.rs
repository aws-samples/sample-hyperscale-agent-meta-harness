// COMPAT-CAH: this file names the earlier spellings of the account selector and of the account file variable, which are read until VERSION 5.50.
//! The account, home Region and profile every Rust binary is built for: one
//! entry of the repository's `accounts.json`, selected by `HAMH_AWS_PROFILE` at build time (unset or
//! empty: the active entry), with the file rules of `scripts/lib/aws_target.py`.

use std::ffi::OsStr;
use std::path::PathBuf;

use harness_protocol::accounts::{
    ACCOUNT_SELECTOR, ACCOUNT_SELECTOR_EARLIER, ACCOUNTS_FILE_ENV, ACCOUNTS_FILE_ENV_EARLIER,
    AccountEntry, Operator, OperatorKind, accounts_file_path, operator_pool_members,
    paired_variable, parse_accounts, select_account,
};
use harness_protocol::target::{
    ACCOUNTS, E2E_ACCOUNT, E2E_PROFILE, E2E_REGION, OPERATOR_POOL_MEMBER, OPERATOR_POOL_MEMBERS,
    SELECTED_BY,
};

/// The committed `accounts.json`: the file the repository ships, whatever file this build used.
fn committed() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../accounts.json");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// The file this build read: the account file the build was given (either spelling of the variable), else the committed one.
fn built_file() -> String {
    let given = option_env!("HAMH_ACCOUNTS_FILE")
        .filter(|v| !v.is_empty())
        .or(option_env!("CAH_ACCOUNTS_FILE").filter(|v| !v.is_empty())); // COMPAT-CAH
    match given {
        Some(path) => std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}")),
        None => committed(),
    }
}

fn entry(account: &str, region: &str, profile: &str, role: &str) -> AccountEntry {
    AccountEntry {
        account: account.into(),
        region: region.into(),
        profile: profile.into(),
        role: role.into(),
        operator: None,
    }
}

/// The one placeholder entry the committed `accounts.json` ships.
fn placeholder() -> AccountEntry {
    entry("123456789012", "us-east-1", "my-profile", "active")
}

/// One accounts file with the given entries, each completed with valid stage AZs and modes.
fn file(entries: &[serde_json::Value]) -> String {
    serde_json::json!({"schemaVersion": 1, "selector": "HAMH_AWS_PROFILE", "accounts": entries})
        .to_string()
}

fn valid(account: &str, region: &str, profile: &str, role: &str) -> serde_json::Value {
    serde_json::json!({
        "account": account, "region": region, "profile": profile, "role": role,
        "azs": {"live": [format!("{region}b")], "drill": [format!("{region}b")], "direct": [format!("{region}b")]},
        "azIds": {"live": ["x-az1"], "drill": ["x-az1"], "direct": ["x-az1"]},
        "invocationLogging": "read-only",
    })
}

fn with(mut v: serde_json::Value, key: &str, value: serde_json::Value) -> serde_json::Value {
    v[key] = value;
    v
}

#[test]
fn the_committed_file_is_one_placeholder_entry() {
    // A stranger's checkout names no account of the authors: one active placeholder entry and the stage `direct`.
    assert_eq!(parse_accounts(&committed()).unwrap(), [placeholder()]);
}

#[test]
fn the_selector_picks_by_profile_and_defaults_to_the_active_entry() {
    let history = valid("111111111111", "us-west-2", "history-profile", "history");
    let test = valid("222222222222", "us-east-1", "test-profile", "test");
    let active = valid("123456789012", "us-east-1", "my-profile", "active");
    let entries = parse_accounts(&file(&[history, active, test])).unwrap();
    assert_eq!(
        select_account(&entries, "", "accounts.json").unwrap(),
        placeholder()
    );
    assert_eq!(
        select_account(&entries, "my-profile", "accounts.json").unwrap(),
        placeholder()
    );
    assert_eq!(
        select_account(&entries, "history-profile", "accounts.json").unwrap(),
        entry("111111111111", "us-west-2", "history-profile", "history")
    );
    let e = select_account(&entries, "default", "accounts.json").unwrap_err();
    assert!(e.contains("HAMH_AWS_PROFILE=default"), "{e}");
    assert!(
        e.contains("history-profile, my-profile, test-profile"),
        "{e}"
    );
    assert_eq!(
        select_account(&entries, "test-profile", "accounts.json").unwrap(),
        entry("222222222222", "us-east-1", "test-profile", "test")
    );
}

#[test]
fn a_test_entry_is_accepted_beside_one_active_entry_and_never_stands_in_for_it() {
    let ok = file(&[
        valid("123456789012", "us-east-1", "my-profile", "active"),
        valid("111111111111", "us-east-1", "other", "test"),
    ]);
    let entries = parse_accounts(&ok).unwrap();
    assert_eq!(
        entries.iter().map(|e| e.role.as_str()).collect::<Vec<_>>(),
        ["active", "test"]
    );
    // The default selection stays the active entry; the test entry is chosen by its profile only.
    assert_eq!(
        select_account(&entries, "", "accounts.json")
            .unwrap()
            .profile,
        "my-profile"
    );
    assert_eq!(
        select_account(&entries, "other", "accounts.json")
            .unwrap()
            .role,
        "test"
    );
    let only_test = file(&[valid("111111111111", "us-east-1", "other", "test")]);
    assert!(
        parse_accounts(&only_test)
            .unwrap_err()
            .contains("exactly one active")
    );
    let two_active = file(&[
        valid("123456789012", "us-east-1", "my-profile", "active"),
        valid("111111111111", "us-east-1", "other", "active"),
        valid("222222222222", "us-east-1", "third", "test"),
    ]);
    assert!(
        parse_accounts(&two_active)
            .unwrap_err()
            .contains("exactly one active")
    );
}

#[test]
fn the_built_constants_are_the_selection_of_the_file_the_build_read() {
    // The integration test is compiled with the environment the crate was built with: the selector under either spelling, the account file
    // under either spelling.
    let selected = option_env!("HAMH_AWS_PROFILE")
        .filter(|v| !v.is_empty())
        .or(option_env!("CAH_AWS_PROFILE").filter(|v| !v.is_empty())) // COMPAT-CAH
        .unwrap_or("");
    assert_eq!(SELECTED_BY, selected);
    let entries = parse_accounts(&built_file()).unwrap();
    let t = select_account(&entries, SELECTED_BY, "accounts.json").unwrap();
    assert_eq!(
        (E2E_ACCOUNT, E2E_REGION, E2E_PROFILE),
        (t.account.as_str(), t.region.as_str(), t.profile.as_str())
    );
    assert_eq!(
        OPERATOR_POOL_MEMBER,
        format!("arn:aws:sts::{}:assumed-role/Admin/*", t.account)
    );
    assert_eq!(
        OPERATOR_POOL_MEMBERS,
        [OPERATOR_POOL_MEMBER],
        "an entry without the operator field has the one pool member the stage used before the field existed"
    );
    assert_eq!(
        OPERATOR_POOL_MEMBERS.to_vec(),
        operator_pool_members(&t.account, t.operator.as_ref())
    );
    let listed: Vec<AccountEntry> = ACCOUNTS
        .iter()
        .map(|(a, r, p, role)| entry(a, r, p, role))
        .collect();
    assert_eq!(listed, entries);
}

#[test]
fn exactly_one_active_entry_is_required() {
    let two = file(&[
        valid("123456789012", "us-east-1", "my-profile", "active"),
        valid("111111111111", "us-east-1", "other", "active"),
    ]);
    assert!(
        parse_accounts(&two)
            .unwrap_err()
            .contains("exactly one active")
    );
    let none = file(&[valid("123456789012", "us-east-1", "my-profile", "history")]);
    assert!(
        parse_accounts(&none)
            .unwrap_err()
            .contains("exactly one active")
    );
}

#[test]
fn a_profile_or_an_account_and_region_twice_is_refused() {
    let profile = file(&[
        valid("123456789012", "us-east-1", "my-profile", "active"),
        valid("111111111111", "us-west-2", "my-profile", "history"),
    ]);
    assert!(
        parse_accounts(&profile)
            .unwrap_err()
            .contains("profile my-profile twice")
    );
    let pair = file(&[
        valid("123456789012", "us-east-1", "my-profile", "active"),
        valid("123456789012", "us-east-1", "other", "history"),
    ]);
    assert!(parse_accounts(&pair).unwrap_err().contains("appears twice"));
}

#[test]
fn malformed_fields_are_refused() {
    let base = valid("123456789012", "us-east-1", "my-profile", "active");
    for (key, value, want) in [
        ("account", serde_json::json!("70000000001"), "account"),
        ("region", serde_json::json!("useast1"), "region"),
        ("profile", serde_json::json!("acct a01"), "profile"),
        (
            "role",
            serde_json::json!("primary"),
            "active, history, test",
        ),
        (
            "invocationLogging",
            serde_json::json!("always"),
            "invocationLogging",
        ),
        (
            "azs",
            serde_json::json!({"live": ["us-west-2a"], "drill": ["us-east-1b"], "direct": ["us-east-1b"]}),
            "us-west-2a is not in us-east-1",
        ),
        (
            "azIds",
            serde_json::json!({"live": [], "drill": ["x-az1"], "direct": ["x-az1"]}),
            "azs.live",
        ),
        ("azs", serde_json::json!({"live": ["us-east-1b"]}), "azs"),
    ] {
        let e = parse_accounts(&file(&[with(base.clone(), key, value.clone())])).unwrap_err();
        assert!(e.contains(want), "{key}={value}: {e}");
    }
    for bad in [
        serde_json::json!({"schemaVersion": 2, "selector": "HAMH_AWS_PROFILE", "accounts": [base.clone()]}),
        serde_json::json!({"schemaVersion": 1, "selector": "AWS_PROFILE", "accounts": [base.clone()]}),
        serde_json::json!({"schemaVersion": 1, "selector": "HAMH_AWS_PROFILE", "accounts": []}),
    ] {
        assert!(parse_accounts(&bad.to_string()).is_err(), "{bad}");
    }
    assert!(parse_accounts("not json").is_err());
}

/// Stage `direct`: every entry of the committed file names the stage `direct` with the same stages in `azs` and
/// `azIds`, and only the stage names `live`, `drill` and `direct` appear.
#[test]
fn every_entry_of_the_committed_file_names_the_stage_direct() {
    let doc: serde_json::Value = serde_json::from_str(&committed()).unwrap();
    for e in doc["accounts"].as_array().unwrap() {
        let names = |key: &str| {
            let mut v: Vec<String> = e[key].as_object().unwrap().keys().cloned().collect();
            v.sort_unstable();
            v
        };
        assert!(
            names("azs").contains(&"direct".to_string()),
            "{}",
            e["profile"]
        );
        assert_eq!(names("azs"), names("azIds"), "{}", e["profile"]);
        for stage in names("azs") {
            assert!(
                ["live", "drill", "direct"].contains(&stage.as_str()),
                "{stage}"
            );
        }
    }
}

#[test]
fn the_direct_stage_gets_the_drill_stages_az_when_a_file_names_both() {
    let doc: serde_json::Value = serde_json::from_str(&committed()).unwrap();
    for e in doc["accounts"].as_array().unwrap() {
        if e["azs"].get("drill").is_some() {
            assert_eq!(e["azs"]["direct"], e["azs"]["drill"], "{}", e["profile"]);
            assert_eq!(
                e["azIds"]["direct"], e["azIds"]["drill"],
                "{}",
                e["profile"]
            );
        }
    }
}

/// The product deploys the stage `direct` only: a file may name it alone, and `live` and `drill` stay optional names.
#[test]
fn a_file_with_only_the_direct_stage_is_accepted() {
    let mut one = valid("123456789012", "us-east-1", "my-profile", "active");
    one["azs"] = serde_json::json!({"direct": ["us-east-1a"]});
    one["azIds"] = serde_json::json!({"direct": ["use1-az1"]});
    assert_eq!(
        parse_accounts(&file(&[one])).unwrap(),
        [entry("123456789012", "us-east-1", "my-profile", "active")]
    );
}

#[test]
fn azs_and_az_ids_name_the_same_stages() {
    let mut e = valid("123456789012", "us-east-1", "my-profile", "active");
    e["azs"] = serde_json::json!({"drill": ["us-east-1b"], "direct": ["us-east-1b"]});
    e["azIds"] = serde_json::json!({"direct": ["x-az1"]});
    let e = parse_accounts(&file(&[e])).unwrap_err();
    assert!(e.contains("azs and azIds must name the same stages"), "{e}");
}

#[test]
fn a_stage_name_other_than_live_drill_direct_is_refused() {
    let mut e = valid("123456789012", "us-east-1", "my-profile", "active");
    e["azs"] = serde_json::json!({"direct": ["us-east-1b"], "canary": ["us-east-1b"]});
    e["azIds"] = serde_json::json!({"direct": ["x-az1"], "canary": ["x-az1"]});
    let e = parse_accounts(&file(&[e])).unwrap_err();
    assert!(e.contains("must name the stage direct"), "{e}");
}

#[test]
fn a_file_without_the_direct_stage_is_refused() {
    let mut old = valid("123456789012", "us-east-1", "my-profile", "active");
    old["azs"] = serde_json::json!({"live": ["us-east-1b"], "drill": ["us-east-1b"]});
    old["azIds"] = serde_json::json!({"live": ["x-az1"], "drill": ["x-az1"]});
    let e = parse_accounts(&file(&[old])).unwrap_err();
    assert!(e.contains("must name the stage direct"), "{e}");
    let mut empty = valid("123456789012", "us-east-1", "my-profile", "active");
    empty["azs"]["direct"] = serde_json::json!([]);
    let e = parse_accounts(&file(&[empty])).unwrap_err();
    assert!(e.contains("azs.direct"), "{e}");
}

/// The generated account file `setup.sh` hands a build instead of the committed one:
/// placeholders only, one active entry.
fn generated() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/setup/accounts-generated.json");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

#[test]
fn the_override_variable_is_ham_accounts_file() {
    assert_eq!(ACCOUNTS_FILE_ENV, "HAMH_ACCOUNTS_FILE");
}

#[test]
fn an_unset_or_empty_override_names_the_committed_file() {
    let manifest = PathBuf::from("/crate/harness-protocol");
    let committed = manifest.join("../../accounts.json");
    assert_eq!(accounts_file_path(None, &manifest).unwrap(), committed);
    assert_eq!(
        accounts_file_path(Some(OsStr::new("")), &manifest).unwrap(),
        committed
    );
}

#[test]
fn an_absolute_override_replaces_the_committed_file() {
    let manifest = PathBuf::from("/crate/harness-protocol");
    assert_eq!(
        accounts_file_path(Some(OsStr::new("/work/generated/accounts.json")), &manifest).unwrap(),
        PathBuf::from("/work/generated/accounts.json")
    );
}

#[test]
fn a_relative_override_is_refused_because_cargo_runs_a_build_script_in_the_crate_directory() {
    let manifest = PathBuf::from("/crate/harness-protocol");
    for rel in [
        "accounts.json",
        "./accounts.json",
        "../generated/accounts.json",
        "~/accounts.json",
    ] {
        let e = accounts_file_path(Some(OsStr::new(rel)), &manifest).unwrap_err();
        assert!(
            e.contains("HAMH_ACCOUNTS_FILE must be an absolute path"),
            "{e}"
        );
        assert!(e.contains(rel), "{e}");
    }
}

#[test]
fn the_generated_file_passes_the_file_rules_and_selects_its_active_entry() {
    let entries = parse_accounts(&generated()).unwrap();
    assert_eq!(entries.len(), 1, "setup writes one entry");
    assert_eq!(entries[0].role, "active");
    assert_eq!(
        select_account(&entries, "", "accounts-generated.json").unwrap(),
        entries[0]
    );
    // A profile the file does not name selects nothing.
    let e = select_account(
        &entries,
        "a-profile-of-another-file",
        "accounts-generated.json",
    )
    .unwrap_err();
    assert!(
        e.contains("names no entry of accounts-generated.json"),
        "{e}"
    );
}

/// The optional `operator` field of an entry, held to the one table that the CDK app
/// (`infra/test/operator.test.ts`) and the Python rules (`scripts/tests/test_operator_identity.py`) run.
fn operator_table() -> serde_json::Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../infra/test/fixtures/operator-identity.json");
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

const ACCOUNT: &str = "111122223333";

fn with_operator(operator: serde_json::Value) -> String {
    file(&[with(
        valid(ACCOUNT, "us-east-1", "my-stage-profile", "active"),
        "operator",
        operator,
    )])
}

fn operator_of(kind: OperatorKind, name: &str) -> Operator {
    Operator {
        kind,
        name: name.into(),
    }
}

#[test]
fn an_entry_without_the_operator_field_keeps_the_pool_member_it_had_before() {
    for e in parse_accounts(&committed()).unwrap() {
        assert_eq!(e.operator, None, "{}", e.profile);
    }
    let plain = file(&[valid(ACCOUNT, "us-east-1", "my-stage-profile", "active")]);
    let entries = parse_accounts(&plain).unwrap();
    assert_eq!(entries[0].operator, None);
    assert_eq!(
        operator_pool_members(ACCOUNT, entries[0].operator.as_ref()),
        [format!("arn:aws:sts::{ACCOUNT}:assumed-role/Admin/*")]
    );
}

#[test]
fn every_accepted_operator_of_the_shared_table_parses_and_yields_its_caller_patterns() {
    let table = operator_table();
    let rows = table["accept"].as_array().unwrap();
    assert!(
        rows.len() >= 5,
        "the table holds a role, an SSO role and users"
    );
    for row in rows {
        let entries = parse_accounts(&with_operator(row["operator"].clone()))
            .unwrap_or_else(|e| panic!("{}: {e}", row["label"]));
        let op = entries[0].operator.as_ref().expect("the operator is kept");
        let kind = match row["operator"]["kind"].as_str().unwrap() {
            "role" => OperatorKind::Role,
            _ => OperatorKind::User,
        };
        assert_eq!(
            op,
            &operator_of(kind, row["operator"]["name"].as_str().unwrap())
        );
        let want: Vec<String> = row["caller"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap().replace("{account}", ACCOUNT))
            .collect();
        assert_eq!(
            operator_pool_members(ACCOUNT, Some(op)),
            want,
            "{}",
            row["label"]
        );
    }
}

#[test]
fn every_refused_operator_of_the_shared_table_is_refused_with_its_reason() {
    let table = operator_table();
    let rows = table["refuse"].as_array().unwrap();
    assert!(rows.len() >= 19);
    for row in rows {
        let e = parse_accounts(&with_operator(row["operator"].clone())).unwrap_err();
        let reason = row["reason"].as_str().unwrap();
        assert!(
            e.contains(reason),
            "{}: wanted {reason:?} in {e:?}",
            row["why"]
        );
        assert!(e.starts_with("accounts[0]: "), "{e}");
    }
}

#[test]
fn a_role_gets_one_assumed_role_pattern_and_a_user_gets_the_two_user_arn_forms() {
    let role = operator_of(
        OperatorKind::Role,
        "AWSReservedSSO_AdministratorAccess_0123456789abcdef",
    );
    assert_eq!(
        operator_pool_members(ACCOUNT, Some(&role)),
        [format!(
            "arn:aws:sts::{ACCOUNT}:assumed-role/AWSReservedSSO_AdministratorAccess_0123456789abcdef/*"
        )]
    );
    let user = operator_of(OperatorKind::User, "alice");
    assert_eq!(
        operator_pool_members(ACCOUNT, Some(&user)),
        [
            format!("arn:aws:iam::{ACCOUNT}:user/alice"),
            format!("arn:aws:iam::{ACCOUNT}:user/*/alice"),
        ]
    );
}

/// COMPAT-CAH: a file generated before the rename names the earlier selector variable, and it still loads; a file that names any other
/// selector is refused.
#[test]
fn a_file_stored_before_the_rename_names_the_earlier_selector_and_still_loads() {
    assert_eq!(ACCOUNT_SELECTOR, "HAMH_AWS_PROFILE");
    assert_eq!(ACCOUNT_SELECTOR_EARLIER, "CAH_AWS_PROFILE");
    let body = |selector: &str| {
        serde_json::json!({
            "schemaVersion": 1, "selector": selector,
            "accounts": [valid("123456789012", "us-east-1", "my-profile", "active")]
        })
        .to_string()
    };
    assert_eq!(
        parse_accounts(&body("CAH_AWS_PROFILE")).unwrap(),
        [placeholder()]
    );
    assert_eq!(
        parse_accounts(&body("HAMH_AWS_PROFILE")).unwrap(),
        [placeholder()]
    );
    for other in ["AWS_PROFILE", "", "hamh_aws_profile", "CAH_ACCOUNTS_FILE"] {
        assert!(parse_accounts(&body(other)).is_err(), "{other}");
    }
}

#[test]
fn the_earlier_account_file_variable_is_cah_accounts_file() {
    assert_eq!(ACCOUNTS_FILE_ENV_EARLIER, "CAH_ACCOUNTS_FILE");
}

/// A variable with two spellings: the new name wins, the earlier name is read with one notice, empty counts as unset, and two different
/// values are an error that names both (a selector is never ignored and never falls through to the default entry).
#[test]
fn a_variable_with_two_spellings_follows_the_rule_of_the_design() {
    let some = |v: &'static str| Some(OsStr::new(v));
    let pv = |new, old| paired_variable("HAMH_AWS_PROFILE", new, "CAH_AWS_PROFILE", old);
    assert_eq!(pv(None, None).unwrap(), (String::new(), None));
    assert_eq!(pv(some("p"), None).unwrap(), ("p".to_string(), None));
    let (value, notice) = pv(None, some("p")).unwrap();
    assert_eq!(value, "p");
    let notice = notice.expect("the earlier name gets one notice");
    assert!(
        notice.contains("CAH_AWS_PROFILE") && notice.contains("HAMH_AWS_PROFILE"),
        "{notice}"
    );
    assert_eq!(pv(some("p"), some("p")).unwrap(), ("p".to_string(), None));
    assert_eq!(pv(some(""), some("")).unwrap(), (String::new(), None));
    assert_eq!(pv(some(""), None).unwrap(), (String::new(), None));
    assert_eq!(pv(some("p"), some("")).unwrap(), ("p".to_string(), None));
    let (value, notice) = pv(some(""), some("p")).unwrap();
    assert_eq!((value.as_str(), notice.is_some()), ("p", true));
    let e = pv(some("a"), some("b")).unwrap_err();
    assert!(
        e.contains("HAMH_AWS_PROFILE") && e.contains("CAH_AWS_PROFILE"),
        "{e}"
    );
    assert!(e.contains("different"), "{e}");
}

#[test]
fn a_variable_that_is_not_text_is_refused_by_name() {
    use std::os::unix::ffi::OsStrExt;
    let bad = OsStr::from_bytes(&[0x66, 0xff, 0x6f]);
    let e = paired_variable("HAMH_AWS_PROFILE", Some(bad), "CAH_AWS_PROFILE", None).unwrap_err();
    assert!(e.contains("HAMH_AWS_PROFILE") && e.contains("UTF-8"), "{e}");
}
