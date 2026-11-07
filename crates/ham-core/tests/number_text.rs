//! How a number of the registry reads to a person: the same words whether the value is the registry's default or one
//! the user set, with the unit, in the singular for one, and "off" for the zero of a key whose least value is zero.

#![allow(clippy::unwrap_used)]

use ham_core::settings::{DefaultValue, Kind, ROWS};

const CALLS: Kind = Kind::Number {
    min: 5,
    max: 1000,
    unit: "calls",
};
const MINUTES: Kind = Kind::Number {
    min: 1,
    max: 240,
    unit: "minutes",
};
const MINUTES_OR_OFF: Kind = Kind::Number {
    min: 0,
    max: 1440,
    unit: "minutes",
};

#[test]
fn a_number_is_written_with_its_unit() {
    assert_eq!(CALLS.number_text("45").as_deref(), Some("45 calls"));
    assert_eq!(MINUTES.number_text("90").as_deref(), Some("90 minutes"));
    assert_eq!(
        MINUTES_OR_OFF.number_text("120").as_deref(),
        Some("120 minutes")
    );
}

#[test]
fn one_takes_the_singular_unit() {
    let one = Kind::Number {
        min: 1,
        max: 100,
        unit: "calls",
    };
    assert_eq!(one.number_text("1").as_deref(), Some("1 call"));
    assert_eq!(MINUTES.number_text("1").as_deref(), Some("1 minute"));
    assert_eq!(
        MINUTES.number_text("11").as_deref(),
        Some("11 minutes"),
        "only 1 is singular"
    );
}

#[test]
fn the_zero_of_a_key_that_may_be_zero_reads_off() {
    assert_eq!(MINUTES_OR_OFF.number_text("0").as_deref(), Some("off"));
    assert_eq!(MINUTES_OR_OFF.number_text("1").as_deref(), Some("1 minute"));
    // a key whose least value is above zero has no off
    assert_eq!(CALLS.number_text("0").as_deref(), Some("0 calls"));
}

#[test]
fn only_a_number_has_number_text() {
    assert_eq!(Kind::Bool.number_text("1"), None);
    assert_eq!(Kind::Text.number_text("30"), None);
    assert_eq!(Kind::Minutes { max: 240 }.number_text("30"), None);
}

#[test]
fn the_default_of_every_number_row_reads_as_the_same_value_set_by_the_user_would() {
    let mut seen = 0;
    for r in &ROWS {
        if let (Kind::Number { .. }, DefaultValue::Number(n)) = (r.kind, r.default) {
            seen += 1;
            assert_eq!(
                Some(r.default_text.to_string()),
                r.kind.number_text(&n.to_string()),
                "{}: the default and a set value read alike",
                r.name
            );
        }
    }
    assert_eq!(
        seen, 6,
        "the four stall keys that are numbers, the allowance of new calls and the total time of a run"
    );
}
