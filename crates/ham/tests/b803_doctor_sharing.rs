//! `ham doctor` and provider data sharing: one informational line, `provider-data-sharing`, reads the account's Bedrock data retention mode in the
//! stage's Region with the user's own credentials (`GET /data-retention` on the Bedrock control plane). It warns when the setting is on and the mode is not
//! `provider_data_share`, and names the exact AWS command the account owner runs. A read that is denied or does not answer says `unknown` and never fails.
//! The doctor never changes the account mode.
#![allow(clippy::unwrap_used)]

use ham::api::Api;
use ham::doctor::{Check, sharing_check, sharing_check_of};
use ham::http::{ControlPlane, Response, RetentionAnswer, TransportError};
use std::sync::Arc;

struct Plane {
    answer: Option<Result<RetentionAnswer, TransportError>>,
}

impl Plane {
    fn says(status: u16, body: &str) -> Plane {
        Plane {
            answer: Some(Ok(RetentionAnswer {
                region: "us-east-1".into(),
                response: Response {
                    status,
                    body: body.as_bytes().to_vec(),
                    ..Response::default()
                },
            })),
        }
    }
}

impl ControlPlane for Plane {
    async fn call(
        &self,
        _method: &str,
        _path: &str,
        _headers: &[(String, String)],
        _body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        Ok(Response::default())
    }

    async fn fetch(
        &self,
        _url: &str,
        _range: Option<(u64, u64)>,
        _if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        Ok(Response::default())
    }

    async fn data_retention(
        &self,
        _target: Option<&str>,
    ) -> Option<Result<RetentionAnswer, TransportError>> {
        self.answer.clone()
    }
}

const OWNER_COMMAND: &str =
    "aws bedrock put-account-data-retention --region us-east-1 --mode provider_data_share";

fn line(c: &Check) -> String {
    format!("{} {} {}", c.level().to_uppercase(), c.name, c.detail)
}

async fn check_with(plane: Plane, on: bool) -> Option<Check> {
    sharing_check(&Api::new(Arc::new(plane)), on).await
}

#[tokio::test]
async fn b803_a_plane_with_no_credentials_of_its_own_adds_no_line() {
    assert!(check_with(Plane { answer: None }, true).await.is_none());
}

#[tokio::test]
async fn b803_the_mode_provider_data_share_passes_with_the_setting_on_or_off() {
    for on in [true, false] {
        let c = check_with(
            Plane::says(
                200,
                r#"{"mode":"provider_data_share","updatedAt":"2026-07-15T00:00:00Z"}"#,
            ),
            on,
        )
        .await
        .unwrap();
        assert_eq!(c.name, "provider-data-sharing");
        assert_eq!((c.pass, c.warn), (true, false), "{}", line(&c));
        assert!(
            c.detail.contains("provider_data_share") && c.detail.contains("us-east-1"),
            "{}",
            line(&c)
        );
    }
}

#[tokio::test]
async fn b803_the_setting_on_and_a_mode_that_does_not_share_warns_with_the_owners_command() {
    for mode in ["default", "none", "inherit"] {
        let c = check_with(Plane::says(200, &format!(r#"{{"mode":"{mode}"}}"#)), true)
            .await
            .unwrap();
        assert_eq!(
            (c.pass, c.warn),
            (true, true),
            "a warning passes: {}",
            line(&c)
        );
        assert_eq!(c.level(), "warn");
        for want in [
            mode,
            "us-east-1",
            "run.provider_sharing is on",
            OWNER_COMMAND,
        ] {
            assert!(c.detail.contains(want), "{want:?} missing: {}", line(&c));
        }
    }
}

#[tokio::test]
async fn b803_the_setting_off_and_a_mode_that_does_not_share_is_only_information() {
    let c = check_with(Plane::says(200, r#"{"mode":"default"}"#), false)
        .await
        .unwrap();
    assert_eq!((c.pass, c.warn), (true, false), "{}", line(&c));
    assert!(
        c.detail.contains("default") && c.detail.contains("run.provider_sharing is off"),
        "{}",
        line(&c)
    );
    assert!(
        !c.detail.contains("put-account-data-retention"),
        "no command is offered to someone who did not ask for sharing: {}",
        line(&c)
    );
}

#[tokio::test]
async fn b803_a_denied_or_unanswered_read_says_unknown_and_never_fails() {
    let denied =
        r#"{"message":"User is not authorized to perform: bedrock:GetAccountDataRetention"}"#;
    let cases = [
        Plane::says(403, denied),
        Plane::says(500, "oops"),
        Plane::says(200, "not json"),
        Plane::says(200, r#"{"mode":42}"#),
        Plane {
            answer: Some(Err(TransportError::Network("connection reset".into()))),
        },
        Plane {
            answer: Some(Err(TransportError::Credentials("no credentials".into()))),
        },
    ];
    for plane in cases {
        for on in [true, false] {
            let c = check_with(
                Plane {
                    answer: plane.answer.clone(),
                },
                on,
            )
            .await
            .unwrap();
            assert!(c.pass && !c.warn, "unknown is information: {}", line(&c));
            assert!(c.detail.contains("unknown"), "{}", line(&c));
        }
    }
    let c = check_with(Plane::says(403, denied), true).await.unwrap();
    assert!(
        c.detail.contains("bedrock:GetAccountDataRetention"),
        "a denial names the permission: {}",
        line(&c)
    );
}

#[test]
fn b803_the_pure_form_takes_a_region_a_setting_and_a_read() {
    let ok = Response {
        status: 200,
        body: br#"{"mode":"none"}"#.to_vec(),
        ..Response::default()
    };
    let c = sharing_check_of("us-west-2", true, Ok(&ok));
    assert!(c.warn);
    assert!(
        c.detail
            .contains("--region us-west-2 --mode provider_data_share"),
        "the command carries the region of the read: {}",
        line(&c)
    );
}

#[test]
fn b803_the_doctor_never_writes_the_account_mode() {
    // The doctor only reads the mode. Since the client switches it in one place, right before a start (`src/sharing.rs`; tests/b814_sharing_switch.rs):
    // no other source of a command or a screen calls the write.
    assert!(!include_str!("../src/doctor.rs").contains("put_data_retention"));
    assert!(include_str!("../src/sharing.rs").contains("put_data_retention"));
    for src in [
        include_str!("../src/cli.rs"),
        include_str!("../src/tui/mod.rs"),
        include_str!("../src/tui/ext.rs"),
        include_str!("../src/tui/app.rs"),
    ] {
        assert!(
            !src.contains("put_data_retention"),
            "a start switches the mode only through crate::sharing::preflight"
        );
    }
}
