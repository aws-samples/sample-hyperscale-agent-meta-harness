//! The rules of provider data sharing that the fixes of release 5.15 added: what a question showed and what the answer `y` agrees to,
//! the question that is asked again, the stop of a resume or a retry while nobody agreed to a switch, and the model of a stored request.
#![allow(clippy::unwrap_used)]

use ham_core::sharing::{
    ASKED_AGAIN, Plan, Put, Read, Shown, Survey, Switching, ask_again_text, ask_text, no_account,
    plan, settle, stored_stop,
};

const ME: &str = "arn:aws:sts::111122223333:assumed-role/Dev/me";

fn survey(read: Read) -> Survey {
    Survey {
        region: "us-east-1".into(),
        caller_arn: Some(ME.into()),
        stage_account: Some("111122223333".into()),
        read,
    }
}

fn mode(m: &str) -> Read {
    Read::Mode(m.into())
}

/// What a question shows is the Region, the stage's account and the plan. Two surveys show the same thing only when all three are equal: the
/// answer `y` to one is no agreement to the other.
#[test]
fn fix515_what_a_question_shows_differs_with_the_mode_the_account_the_region_and_the_read() {
    let shown = Shown::of(&survey(mode("default")));
    assert_eq!(
        shown,
        Shown {
            region: "us-east-1".into(),
            account: Some("111122223333".into()),
            plan: Plan::Switch {
                account: "111122223333".into(),
                from: "default".into()
            },
        }
    );
    assert_eq!(shown, Shown::of(&survey(mode("default"))), "the same facts");
    let other_mode = Shown::of(&survey(mode("inherit")));
    let already = Shown::of(&survey(mode("provider_data_share")));
    let failed = Shown::of(&survey(Read::Failed("network: timed out".into())));
    let denied = Shown::of(&survey(Read::Denied));
    let other_region = Shown::of(&Survey {
        region: "us-west-2".into(),
        ..survey(mode("default"))
    });
    let other_account = Shown::of(&Survey {
        caller_arn: Some("arn:aws:sts::444455556666:assumed-role/Dev/me".into()),
        stage_account: Some("444455556666".into()),
        ..survey(mode("default"))
    });
    for (what, now) in [
        ("another mode", &other_mode),
        ("a mode that already allows the model", &already),
        ("a read that now fails", &failed),
        ("a read that is now denied", &denied),
        ("another Region", &other_region),
        ("another account", &other_account),
    ] {
        assert_ne!(&shown, now, "{what}");
    }
    assert_ne!(failed, denied);
    assert_eq!(
        failed,
        Shown::of(&survey(Read::Failed("network: timed out".into()))),
        "a read that fails the same way shows the same question"
    );
}

/// The question asked again says first that nothing was changed or started, then asks for the state the account is in now.
#[test]
fn fix515_the_question_asked_again_says_that_nothing_changed_and_shows_the_new_facts() {
    let s = survey(mode("inherit"));
    let p = plan(&s);
    let again = ask_again_text(
        "Claude Fable 5.2",
        "global.anthropic.claude-fable-5-2",
        &s,
        &p,
    );
    let first = ask_text(
        "Claude Fable 5.2",
        "global.anthropic.claude-fable-5-2",
        &s,
        &p,
    );
    assert_eq!(again, format!("{ASKED_AGAIN} {first}"));
    assert!(
        again.starts_with("The account is not as the last question showed it"),
        "{again}"
    );
    assert!(
        again.contains("ham changed nothing and started nothing"),
        "{again}"
    );
    assert!(
        again.contains("from inherit to provider_data_share"),
        "{again}"
    );
    assert!(!again.contains('\n'), "{again}");
}

/// A backend with no AWS account behind it: nothing can be read, so the plan changes nothing.
#[test]
fn fix515_a_backend_with_no_account_is_left_alone() {
    let s = no_account("the stage's Region");
    assert_eq!(
        (s.caller_arn.as_deref(), s.stage_account.as_deref()),
        (None, None)
    );
    assert!(matches!(plan(&s), Plan::Leave { .. }), "{:?}", plan(&s));
    assert_eq!(Shown::of(&s), Shown::of(&no_account("the stage's Region")));
}

/// The stop of a resume or a retry: the model, the account, the Region and the mode, why ham does not switch, both ways on, and that no run
/// was started; one line.
#[test]
fn fix515_the_stop_of_a_stored_request_names_the_account_the_mode_and_both_ways_on() {
    let stop = stored_stop("Claude Fable 5.2", "111122223333", "us-east-1", "inherit");
    for want in [
        "Claude Fable 5.2 needs provider data sharing",
        "AWS account 111122223333 in us-east-1 is inherit",
        "only when you agree",
        "ham config set run.provider_sharing on",
        "aws bedrock put-account-data-retention --region us-east-1 --mode provider_data_share",
        "No run was started.",
    ] {
        assert!(stop.contains(want), "{want:?} missing: {stop}");
    }
    assert!(!stop.contains('\n'), "{stop}");
}

/// A stored request names its model by id: the row of that id in any engine's list says whether it needs provider data sharing.
#[test]
fn fix515_the_model_of_a_stored_request_is_found_by_its_id() {
    let catalog = ham_core::models::builtin_catalog();
    let fable = catalog
        .sharing_row("global.anthropic.claude-fable-5-2")
        .unwrap();
    assert_eq!(fable.name, "Claude Fable 5.2");
    assert!(fable.requires_provider_sharing);
    assert!(
        catalog
            .sharing_row("global.anthropic.claude-sonnet-5-5")
            .is_none()
    );
    assert!(catalog.sharing_row("no-such-model").is_none());
    assert!(catalog.sharing_row("").is_none());
}

// ---- answers of the switch that the earlier tests left out --------------------------------------------------------------------------------

fn cx() -> Switching<'static> {
    Switching {
        name: "Claude Fable 5.2",
        account: "111122223333",
        region: "us-east-1",
        from: "default",
    }
}

/// A refusal whose body is no JSON (a proxy's page) is told by its status alone; the body is never shown.
#[test]
fn fix515_a_refused_switch_with_a_body_that_is_no_json_is_told_by_its_status() {
    let put = Put::from_answer(400, b"<html>Bad Request</html>");
    assert_eq!(put, Put::Rejected("HTTP 400".into()));
    let stop = settle(&put, &Read::Failed("not read".into()), &cx()).unwrap_err();
    assert!(
        stop.contains("Bedrock refused the switch") && stop.contains("(HTTP 400)"),
        "{stop}"
    );
    assert!(!stop.contains("html"), "{stop}");
    assert!(stop.ends_with("No run was started."), "{stop}");
}

/// The switch answered 200 with a body that names no mode and the read back failed: ham cannot confirm the mode, so the start stops.
#[test]
fn fix515_a_switch_that_cannot_be_confirmed_stops_the_start() {
    let put = Put::from_answer(200, b"{}");
    assert_eq!(
        put,
        Put::Done(Read::Failed("the answer holds no mode".into()))
    );
    for readback in [Read::Failed("network: reset".into()), Read::Denied] {
        let stop = settle(&put, &readback, &cx()).unwrap_err();
        assert!(
            stop.contains("ham could not confirm the switch")
                && stop.contains("111122223333")
                && stop.contains("us-east-1"),
            "{readback:?}: {stop}"
        );
        assert!(stop.ends_with("No run was started."), "{stop}");
    }
    // The answer of the switch itself names the new mode: that confirms it when the read back fails.
    let named = Put::from_answer(200, br#"{"mode":"provider_data_share"}"#);
    assert_eq!(
        settle(&named, &Read::Failed("network: reset".into()), &cx()),
        Ok("provider_data_share".to_string())
    );
}
