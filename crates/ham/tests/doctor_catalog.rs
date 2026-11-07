//! The catalog check of `ham doctor`: the age of the newest model smoke record is information, not a
//! fault. By default the line states the age and passes. With a limit (`ham doctor --catalog-max-age <hours>`, here `catalog_check_limited`),
//! a record older than the limit gives a WARN line ("catalog smoke records are N hours old (refresh them)") and exit 0, because the models are
//! still selectable and the stage works; "no selectable model" and "no smokedAt at all" stay FAIL with or without a limit. Before client polish 2
//! a healthy stage turned `ham doctor` red 24 hours after its last smoke run every install warned 24 hours after the release,
//! because the model list is compiled into the build and nobody can refresh it from the client.
#![allow(clippy::unwrap_used)]

use ham::doctor::{Check, catalog_check, catalog_check_limited};

const NOW: u64 = 1_792_000_000;

fn at(age_s: u64) -> String {
    control_api::ids::rfc3339(NOW - age_s)
}

fn hours(h: u64) -> u64 {
    h * 3600
}

/// The limit the tests of the warning use: `--catalog-max-age 24`, in seconds.
const LIMIT_24H: Option<u64> = Some(24 * 3600);

fn check(rows: usize, smoked: &[String]) -> Check {
    let refs: Vec<&str> = smoked.iter().map(String::as_str).collect();
    catalog_check(rows, &refs, NOW)
}

fn limited(rows: usize, smoked: &[String], max_age_s: Option<u64>) -> Check {
    let refs: Vec<&str> = smoked.iter().map(String::as_str).collect();
    catalog_check_limited(rows, &refs, NOW, max_age_s)
}

#[test]
fn a_fresh_record_passes_with_the_line_it_always_had() {
    let t = at(0);
    let c = check(2, std::slice::from_ref(&t));
    assert_eq!(
        c,
        Check {
            name: "catalog",
            pass: true,
            warn: false,
            detail: format!("2 selectable rows, newest smoke {t} (0.0 h old)"),
        }
    );
    let t = at(hours(5) + 1800);
    assert_eq!(
        check(5, std::slice::from_ref(&t)).detail,
        format!("5 selectable rows, newest smoke {t} (5.5 h old)")
    );
}

#[test]
fn exactly_24_hours_still_passes_and_one_second_more_warns() {
    let c = limited(2, &[at(hours(24))], LIMIT_24H);
    assert!(c.pass && !c.warn, "{c:?}");
    let t = at(hours(24) + 1);
    let c = limited(2, std::slice::from_ref(&t), LIMIT_24H);
    assert!(c.pass && c.warn, "a warning still passes: {c:?}");
    assert!(
        c.detail
            .contains("catalog smoke records are 24 hours old (refresh them)"),
        "{c:?}"
    );
    assert!(
        c.detail
            .starts_with(&format!("2 selectable rows, newest smoke {t}")),
        "the facts stay in front: {c:?}"
    );
}

#[test]
fn the_warning_counts_whole_hours() {
    for (age_s, want) in [
        (hours(30) + 1800, "30 hours old"),
        (hours(47) + 3599, "47 hours old"),
        (hours(120), "120 hours old"),
    ] {
        let c = limited(3, &[at(age_s)], LIMIT_24H);
        assert!(c.pass && c.warn, "{c:?}");
        assert!(
            c.detail
                .contains(&format!("catalog smoke records are {want} (refresh them)")),
            "{age_s}: {c:?}"
        );
    }
}

#[test]
fn the_newest_record_decides_and_a_record_from_the_future_is_fresh() {
    let c = limited(2, &[at(hours(80)), at(hours(2)), at(hours(300))], LIMIT_24H);
    assert!(
        c.pass && !c.warn,
        "one fresh row keeps the catalog fresh: {c:?}"
    );
    let c = limited(1, &[control_api::ids::rfc3339(NOW + 600)], LIMIT_24H);
    assert!(c.pass && !c.warn, "clock skew is not age: {c:?}");
}

#[test]
fn no_selectable_model_and_no_smoked_at_stay_failures() {
    // With the default and with a limit alike.
    for max_age_s in [None, LIMIT_24H] {
        let c = limited(0, &[], max_age_s);
        assert_eq!((c.pass, c.warn), (false, false));
        assert!(c.detail.contains("no selectable model"), "{c:?}");
        // Rows without a usable smokedAt: nothing to date the catalog by.
        let c = limited(2, &["not a time".to_string(), String::new()], max_age_s);
        assert_eq!((c.pass, c.warn), (false, false));
        assert!(c.detail.contains("no row has a smokedAt time"), "{c:?}");
        // No rows beats any timestamp.
        let c = limited(0, &[at(0)], max_age_s);
        assert!(!c.pass && c.detail.contains("no selectable model"), "{c:?}");
    }
}

// ---- the default: age is shown, never a warning -------------------------------------------------------------------
//
// The model list is compiled into the ham build and into the control-api Lambda, so its smoke records are as old as the build and no user
// or operator can refresh them from the command line. A warning that appears 24 hours after every release on every healthy install is
// noise (invariant 6 asks for a passing record, not a recent one). The age stays in the detail; a warning needs a limit that the caller
// asks for (`ham doctor --catalog-max-age <hours>`).

#[test]
fn an_old_record_passes_without_a_warning_by_default() {
    for h in [25, 270, 24 * 365] {
        let t = at(hours(h));
        let c = check(5, std::slice::from_ref(&t));
        assert_eq!(
            c,
            Check {
                name: "catalog",
                pass: true,
                warn: false,
                detail: format!("5 selectable rows, newest smoke {t} ({h}.0 h old)"),
            },
            "{h} h old"
        );
        assert_eq!(c.level(), "pass");
        assert!(!c.detail.contains("refresh them"), "{c:?}");
    }
}

#[test]
fn a_limit_decides_and_the_largest_limit_never_warns() {
    let old = at(hours(270));
    // The same record: no limit passes, a smaller limit warns, a larger one passes (an age equal to the limit passes, tested above).
    assert!(!check(5, std::slice::from_ref(&old)).warn);
    let c = limited(5, std::slice::from_ref(&old), Some(hours(269)));
    assert!(c.pass && c.warn, "{c:?}");
    assert!(
        c.detail
            .contains("catalog smoke records are 270 hours old (refresh them)"),
        "{c:?}"
    );
    let c = limited(5, std::slice::from_ref(&old), Some(hours(271)));
    assert!(c.pass && !c.warn, "{c:?}");
    // The command line turns hours into seconds with a saturating product; the largest value is a limit no record exceeds.
    let c = limited(5, std::slice::from_ref(&old), Some(u64::MAX));
    assert!(c.pass && !c.warn, "{c:?}");
    // The facts stay in front of the advice, as they did before.
    let c = limited(5, std::slice::from_ref(&old), Some(hours(1)));
    assert!(
        c.detail.starts_with(&format!(
            "5 selectable rows, newest smoke {old} (270.0 h old); "
        )),
        "{c:?}"
    );
}
