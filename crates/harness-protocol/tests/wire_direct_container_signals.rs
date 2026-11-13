//! The container signals in the `compute` member of a start request: `containerSignals` is a list of lines the
//! repo-gateway reads against the files of the staged repository. Absent means the stage's default list, an empty list means no signals, and the
//! word `default` inside a list stands for the default list. The wire carries the lines and nothing else; the grammar lives in `ham-core`.
#![allow(clippy::unwrap_used)]

use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{ComputeMode, ComputeOptions};
use serde_json::json;

fn lines(list: &[&str]) -> Option<Vec<String>> {
    Some(list.iter().map(|s| (*s).to_string()).collect())
}

#[test]
fn b632_a_compute_member_with_only_a_mode_has_the_bytes_it_had_before() {
    for (mode, word) in [
        (ComputeMode::Microvm, "microvm"),
        (ComputeMode::Instances, "instances"),
    ] {
        let c = ComputeOptions::for_mode(mode).unwrap();
        assert_eq!(c.container_signals, None);
        assert_eq!(
            serde_json::to_string(&c).unwrap(),
            format!(r#"{{"mode":"{word}"}}"#)
        );
    }
    assert_eq!(ComputeOptions::for_mode(ComputeMode::Auto), None);
}

#[test]
fn b632_the_list_rides_in_camel_case_and_an_empty_list_is_kept_as_no_signals() {
    let c = ComputeOptions::new(
        ComputeMode::Auto,
        lines(&["default", "Gemfile contains testcontainers"]),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(&c).unwrap(),
        json!({"mode": "auto", "containerSignals": ["default", "Gemfile contains testcontainers"]})
    );
    let empty = ComputeOptions::new(ComputeMode::Auto, Some(vec![])).unwrap();
    assert_eq!(
        serde_json::to_string(&empty).unwrap(),
        r#"{"mode":"auto","containerSignals":[]}"#
    );
    let back: ComputeOptions =
        serde_json::from_str(r#"{"mode":"auto","containerSignals":[]}"#).unwrap();
    assert_eq!(back.container_signals, Some(vec![]));
    let absent: ComputeOptions = serde_json::from_str(r#"{"mode":"auto"}"#).unwrap();
    assert_eq!(absent.container_signals, None);
}

#[test]
fn b632_auto_with_no_list_is_no_member_and_every_other_pair_is_one() {
    assert_eq!(ComputeOptions::new(ComputeMode::Auto, None), None);
    assert!(ComputeOptions::new(ComputeMode::Auto, Some(vec![])).is_some());
    for mode in [ComputeMode::Microvm, ComputeMode::Instances] {
        assert_eq!(
            ComputeOptions::new(mode, None),
            ComputeOptions::for_mode(mode)
        );
        assert_eq!(
            ComputeOptions::new(mode, lines(&["a.txt"]))
                .unwrap()
                .container_signals,
            lines(&["a.txt"])
        );
    }
}

#[test]
fn b632_the_member_is_still_closed_to_unknown_names_and_wrong_types() {
    for bad in [
        r#"{"mode":"auto","containerSignal":[]}"#,
        r#"{"mode":"auto","containerSignals":"default"}"#,
        r#"{"mode":"auto","containerSignals":[1]}"#,
        r#"{"mode":"auto","containerSignals":[],"extra":1}"#,
        r#"{"containerSignals":[]}"#,
    ] {
        assert!(
            serde_json::from_str::<ComputeOptions>(bad).is_err(),
            "{bad}"
        );
    }
}

#[test]
fn b632_a_start_request_carries_the_list_and_one_without_it_is_unchanged() {
    let body = json!({
        "prompt": "p", "target": {"mode": "s3-only"}, "model": "m",
        "compute": {"mode": "instances", "containerSignals": ["Taskfile.yml contains docker"]}
    });
    let r: StartDirectRunRequest = serde_json::from_value(body).unwrap();
    assert_eq!(
        r.compute.as_ref().unwrap().container_signals,
        lines(&["Taskfile.yml contains docker"])
    );
    let again = serde_json::to_value(&r).unwrap();
    assert_eq!(
        again["compute"]["containerSignals"][0],
        "Taskfile.yml contains docker"
    );
    let plain: StartDirectRunRequest = serde_json::from_value(json!({
        "prompt": "p", "target": {"mode": "s3-only"}, "model": "m", "compute": {"mode": "microvm"}
    }))
    .unwrap();
    assert!(
        !serde_json::to_string(&plain)
            .unwrap()
            .contains("containerSignals")
    );
}
