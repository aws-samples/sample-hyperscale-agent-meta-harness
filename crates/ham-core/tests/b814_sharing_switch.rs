//! Provider data sharing in the AWS account: the pure part of the switch. Before a run on a model that needs provider data sharing, `ham` reads
//! the Bedrock data retention mode of the stage's account in the stage's Region with the user's own credentials, and decides what to do (`plan`): leave a
//! mode that already lets the model run (`provider_data_share`, `aws_review`), switch `default` or `inherit` to `provider_data_share`, and change nothing
//! it cannot read, does not know, or that belongs to another account than the credentials. After the switch the answer and a fresh read decide whether
//! the run may start (`settle`). The texts name the model, what sharing means, the account and the Region.
#![allow(clippy::unwrap_used)]

use ham_core::sharing::{
    Agreement, Plan, Put, Read, SWITCH_TO, Survey, Switching, account_of_arn, ask_text, declined,
    leave_line, mode_runs, owner_command, plan, settle, switched_line,
};

const NAME: &str = "Claude Fable 5.2";
const ID: &str = "global.anthropic.claude-fable-5-2";
const ACCOUNT: &str = "111122223333";
const OTHER: &str = "444455556666";
const REGION: &str = "us-east-1";

fn arn(account: &str) -> String {
    format!("arn:aws:sts::{account}:assumed-role/Dev/someone")
}

fn survey(read: Read) -> Survey {
    Survey {
        region: REGION.into(),
        caller_arn: Some(arn(ACCOUNT)),
        stage_account: Some(ACCOUNT.into()),
        read,
    }
}

fn mode(m: &str) -> Read {
    Read::Mode(m.into())
}

// ---- the modes ------------------------------------------------------------------------------------------------------------------------------

#[test]
fn b814_the_two_modes_measured_working_let_the_model_run_and_no_other_does() {
    assert!(mode_runs("provider_data_share"));
    assert!(mode_runs("aws_review"));
    for m in ["default", "inherit", "none", "", "PROVIDER_DATA_SHARE"] {
        assert!(!mode_runs(m), "{m:?}");
    }
    assert_eq!(SWITCH_TO, "provider_data_share");
}

#[test]
fn b814_a_read_answer_becomes_a_mode_a_denial_or_a_failure() {
    assert_eq!(
        Read::from_answer(
            200,
            br#"{"mode":"default","updatedAt":"2026-11-10T00:00:00Z"}"#
        ),
        mode("default")
    );
    assert_eq!(Read::from_answer(403, b"{}"), Read::Denied);
    assert!(matches!(Read::from_answer(200, b"{}"), Read::Failed(_)));
    assert!(matches!(Read::from_answer(500, b"x"), Read::Failed(w) if w.contains("HTTP 500")));
}

#[test]
fn b814_the_account_of_a_principal_arn() {
    assert_eq!(account_of_arn(&arn(ACCOUNT)), Some(ACCOUNT));
    assert_eq!(
        account_of_arn("arn:aws:iam::111122223333:user/dev"),
        Some(ACCOUNT)
    );
    for bad in [
        "",
        "not-an-arn",
        "arn:aws:sts:::assumed-role/x",
        "arn:aws:sts::12345:assumed-role/x",
        "arn:aws:sts::1111222233334:assumed-role/x",
        "arn:aws:sts::11112222333x:assumed-role/x",
    ] {
        assert_eq!(account_of_arn(bad), None, "{bad:?}");
    }
}

// ---- what ham does with the account --------------------------------------------------------------------------------------------------------

#[test]
fn b814_default_and_inherit_are_switched_in_the_account_of_the_credentials() {
    for from in ["default", "inherit"] {
        assert_eq!(
            plan(&survey(mode(from))),
            Plan::Switch {
                account: ACCOUNT.into(),
                from: from.into()
            }
        );
    }
}

#[test]
fn b814_a_mode_that_already_lets_the_model_run_is_kept() {
    for m in ["provider_data_share", "aws_review"] {
        assert_eq!(plan(&survey(mode(m))), Plan::Keep { mode: m.into() });
    }
}

#[test]
fn b814_a_mode_ham_does_not_know_is_left_as_it_is() {
    let Plan::Leave { why } = plan(&survey(mode("none"))) else {
        panic!("an unknown mode is never switched")
    };
    assert!(why.contains("none"), "{why}");
}

#[test]
fn b814_a_read_that_is_denied_or_fails_changes_nothing_and_names_the_permission() {
    let Plan::Leave { why } = plan(&survey(Read::Denied)) else {
        panic!("a denied read is never followed by a switch")
    };
    assert!(why.contains("bedrock:GetAccountDataRetention"), "{why}");
    let Plan::Leave { why } = plan(&survey(Read::Failed("timed out".into()))) else {
        panic!("a failed read is never followed by a switch")
    };
    assert!(why.contains("timed out"), "{why}");
}

#[test]
fn b814_credentials_of_another_account_than_the_stage_change_nothing() {
    let mut s = survey(mode("default"));
    s.caller_arn = Some(arn(OTHER));
    let Plan::Leave { why } = plan(&s) else {
        panic!("ham never switches an account the stage does not run in")
    };
    assert!(why.contains(OTHER) && why.contains(ACCOUNT), "{why}");
}

#[test]
fn b814_an_unknown_caller_or_stage_account_changes_nothing() {
    for (caller, stage) in [
        (None, Some(ACCOUNT.to_string())),
        (Some(arn(ACCOUNT)), None),
        (
            Some("arn:aws:sts::x:assumed-role/a".to_string()),
            Some(ACCOUNT.to_string()),
        ),
    ] {
        let s = Survey {
            region: REGION.into(),
            caller_arn: caller,
            stage_account: stage,
            read: mode("default"),
        };
        assert!(matches!(plan(&s), Plan::Leave { .. }), "{s:?}");
    }
}

// ---- the words ------------------------------------------------------------------------------------------------------------------------------

#[test]
fn b814_the_question_names_the_model_what_sharing_means_the_account_the_region_and_the_change() {
    let s = survey(mode("default"));
    let q = ask_text(NAME, ID, &s, &plan(&s));
    for want in [
        NAME,
        ID,
        "provider data sharing",
        "model's provider",
        "30 days",
        ACCOUNT,
        REGION,
        "from default to provider_data_share",
        "every Bedrock call",
        "stays after the run",
    ] {
        assert!(q.contains(want), "{want:?} missing: {q}");
    }
}

#[test]
fn b814_the_question_for_a_kept_or_unread_mode_says_ham_changes_nothing() {
    let s = survey(mode("aws_review"));
    let q = ask_text(NAME, ID, &s, &plan(&s));
    assert!(
        q.contains("already aws_review") && q.contains("changes nothing"),
        "{q}"
    );
    assert!(q.contains(ACCOUNT) && q.contains(REGION), "{q}");
    let s = survey(Read::Denied);
    let q = ask_text(NAME, ID, &s, &plan(&s));
    assert!(
        q.contains("changes nothing") && q.contains("bedrock:GetAccountDataRetention"),
        "{q}"
    );
}

#[test]
fn b814_the_switched_line_names_the_account_the_region_the_modes_the_agreement_and_the_way_back() {
    for (agreement, words) in [
        (Agreement::Setting, "run.provider_sharing is on"),
        (Agreement::ThisRun, "you agreed for this run"),
    ] {
        let line = switched_line(
            NAME,
            ACCOUNT,
            REGION,
            "default",
            "provider_data_share",
            agreement,
        );
        for want in [
            NAME,
            ACCOUNT,
            REGION,
            "from default",
            "provider_data_share",
            words,
            "stays after the run",
            &owner_command(REGION, "default"),
        ] {
            assert!(line.contains(want), "{want:?} missing: {line}");
        }
        assert!(!line.contains('\n'), "one line: {line}");
    }
}

#[test]
fn b814_the_leave_line_and_the_refusal_are_one_plain_line_each() {
    let line = leave_line(NAME, REGION, "your credentials may not read it");
    assert!(line.contains(&owner_command(REGION, SWITCH_TO)), "{line}");
    assert!(line.contains("did not check or change"), "{line}");
    let no = declined(NAME, ID);
    assert!(no.starts_with("Not started"), "{no}");
    assert!(
        no.contains(NAME) && no.contains("Nothing was sent") && no.contains("not changed"),
        "{no}"
    );
    for text in [line, no] {
        assert!(!text.contains('\n'), "{text}");
    }
}

// ---- after the switch -----------------------------------------------------------------------------------------------------------------------

fn switching() -> Switching<'static> {
    Switching {
        name: NAME,
        account: ACCOUNT,
        region: REGION,
        from: "default",
    }
}

#[test]
fn b814_a_switch_whose_read_back_lets_the_model_run_is_settled() {
    let put = Put::from_answer(
        200,
        br#"{"mode":"provider_data_share","updatedAt":"2026-11-10T16:00:00Z"}"#,
    );
    assert_eq!(
        settle(&put, &mode("provider_data_share"), &switching()),
        Ok("provider_data_share".to_string())
    );
    // A read back that is denied leaves the answer of the switch itself.
    assert_eq!(
        settle(&put, &Read::Denied, &switching()),
        Ok("provider_data_share".to_string())
    );
}

#[test]
fn b814_a_read_back_that_does_not_let_the_model_run_stops_the_run() {
    let put = Put::from_answer(200, br#"{"mode":"provider_data_share"}"#);
    let why = settle(&put, &mode("default"), &switching()).unwrap_err();
    assert!(
        why.contains("reads default") && why.contains("No run was started"),
        "{why}"
    );
}

#[test]
fn b814_a_denied_switch_stops_the_run_naming_the_permission_and_the_owners_command() {
    let put = Put::from_answer(403, br#"{"message":"not authorized"}"#);
    assert!(matches!(put, Put::Denied(_)));
    let why = settle(&put, &Read::Denied, &switching()).unwrap_err();
    for want in [
        "bedrock:PutAccountDataRetention",
        ACCOUNT,
        REGION,
        &owner_command(REGION, SWITCH_TO),
        "No run was started",
    ] {
        assert!(why.contains(want), "{want:?} missing: {why}");
    }
    assert!(!why.contains('\n'), "{why}");
}

#[test]
fn b814_a_switch_bedrock_refuses_stops_the_run_with_its_reason() {
    let put = Put::from_answer(
        400,
        br#"{"__type":"ValidationException","message":"mode not allowed"}"#,
    );
    assert!(matches!(put, Put::Rejected(_)));
    let why = settle(&put, &mode("default"), &switching()).unwrap_err();
    assert!(
        why.contains("ValidationException") && why.contains("mode not allowed"),
        "{why}"
    );
    assert!(why.contains("No run was started"), "{why}");
}

#[test]
fn b814_a_switch_that_did_not_answer_is_settled_by_the_read_back() {
    for status in [500, 503, 429] {
        let put = Put::from_answer(status, b"{}");
        assert!(matches!(put, Put::Unsure(_)), "{status}");
        assert_eq!(
            settle(&put, &mode("provider_data_share"), &switching()),
            Ok("provider_data_share".to_string())
        );
        let why = settle(&put, &mode("default"), &switching()).unwrap_err();
        assert!(why.contains("No run was started"), "{why}");
        assert!(settle(&put, &Read::Denied, &switching()).is_err());
    }
}
