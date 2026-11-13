//! The endpoints a run's sessions are called at. AgentCore's
//! `qualifier` is an endpoint name: a bare runtime version is refused with
//! `ResourceNotFoundException: No endpoint or agent found with qualifier '50'`. deploy.sh keeps a
//! `v<N>` endpoint for each recent worker version beside `live`; a run's first dispatch pins the
//! `v<N>` of the version `live` serves, or `live` itself (recorded) when that endpoint is missing.
#![allow(clippy::unwrap_used)]

use dispatcher::endpoints::{
    EndpointState, LIVE, MAX_ENDPOINT_NAME, Pin, PinChoice, choose_pin, endpoint_version,
    is_endpoint_name, session_qualifier, version_endpoint,
};

fn state(name: &str, status: &str, live: Option<&str>) -> EndpointState {
    EndpointState {
        name: name.into(),
        status: status.into(),
        live_version: live.map(Into::into),
    }
}

#[test]
fn an_endpoint_name_starts_with_a_letter_and_a_bare_version_is_refused() {
    for ok in ["live", "DEFAULT", "v50", "v1", "a", "canary_2"] {
        assert!(is_endpoint_name(ok), "{ok}");
    }
    assert!(is_endpoint_name(&format!(
        "v{}",
        "9".repeat(MAX_ENDPOINT_NAME - 1)
    )));
    assert_eq!(MAX_ENDPOINT_NAME, 48);
    for bad in [
        "50",
        "7",
        "",
        "_v50",
        "v-50",
        "v50 ",
        "live;x",
        "ünicode",
        &format!("v{}", "9".repeat(MAX_ENDPOINT_NAME)),
    ] {
        assert!(!is_endpoint_name(bad), "{bad:?}");
    }
    assert_eq!(LIVE, "live");
}

#[test]
fn a_version_has_one_endpoint_name_and_back() {
    assert_eq!(version_endpoint("50").as_deref(), Some("v50"));
    assert_eq!(version_endpoint("1").as_deref(), Some("v1"));
    for bad in ["", "0", "050", "v50", "5a", "-1", "99999999999999999999999"] {
        assert_eq!(version_endpoint(bad), None, "{bad:?}");
    }
    assert_eq!(endpoint_version("v50"), Some(50));
    for bad in ["live", "v", "v050", "v0", "50", "V50", "v5x"] {
        assert_eq!(endpoint_version(bad), None, "{bad:?}");
    }
}

#[test]
fn the_first_dispatch_pins_the_version_endpoint_of_the_version_live_serves() {
    let live = state("live", "READY", Some("50"));
    let v50 = state("v50", "READY", Some("50"));
    assert_eq!(
        choose_pin(&live, Some(&v50)).unwrap(),
        PinChoice {
            pin: Pin {
                version: "50".into(),
                endpoint: "v50".into(),
            },
            fallback: None,
        }
    );
}

#[test]
fn without_a_ready_version_endpoint_the_run_is_pinned_to_live_and_says_why() {
    let live = state("live", "READY", Some("50"));
    let on_live = |why: &str| PinChoice {
        pin: Pin {
            version: "50".into(),
            endpoint: "live".into(),
        },
        fallback: Some(why.into()),
    };
    assert_eq!(choose_pin(&live, None).unwrap(), on_live("v50-absent"));
    assert_eq!(
        choose_pin(&live, Some(&state("v50", "CREATING", None))).unwrap(),
        on_live("v50-CREATING")
    );
    assert_eq!(
        choose_pin(&live, Some(&state("v50", "READY", Some("49")))).unwrap(),
        on_live("v50-serves-49")
    );
}

#[test]
fn a_live_endpoint_without_a_version_cannot_pin_a_run() {
    for live in [
        state("live", "READY", None),
        state("live", "READY", Some("")),
        state("live", "READY", Some("latest")),
    ] {
        assert!(choose_pin(&live, None).is_err(), "{live:?}");
    }
}

#[test]
fn a_session_is_called_at_its_pinned_endpoint_and_an_unpinned_one_at_live() {
    assert_eq!(session_qualifier(Some("v50")), "v50");
    assert_eq!(session_qualifier(None), "live");
}
