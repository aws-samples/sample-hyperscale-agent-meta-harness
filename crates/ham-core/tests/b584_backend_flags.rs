//! Steps 5 and 7: the options `--backend <alias>` and `--customer-content` stand outside the command grammar and are taken from the
//! arguments wherever they are.
#![allow(clippy::unwrap_used)]

use ham_core::backends::{Flags, take_flags};

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn b584_no_flag_leaves_the_arguments_alone() {
    let (f, rest) = take_flags(&args(&["runs", "--json"])).unwrap();
    assert_eq!(f, Flags::default());
    assert_eq!(rest, args(&["runs", "--json"]));
}

#[test]
fn b584_backend_is_taken_wherever_it_stands_in_either_spelling() {
    for a in [
        &["--backend", "work", "watch", "R1"][..],
        &["watch", "--backend", "work", "R1"][..],
        &["watch", "R1", "--backend", "work"][..],
        &["watch", "R1", "--backend=work"][..],
    ] {
        let (f, rest) = take_flags(&args(a)).unwrap();
        assert_eq!(f.backend.as_deref(), Some("work"), "{a:?}");
        assert_eq!(rest, args(&["watch", "R1"]), "{a:?}");
    }
}

#[test]
fn b584_a_backend_flag_needs_a_value_and_is_given_once() {
    for a in [
        &["watch", "--backend"][..],
        &["watch", "--backend", "--json"][..],
        &["watch", "--backend="][..],
    ] {
        let e = take_flags(&args(a)).unwrap_err();
        assert!(e.contains("--backend"), "{e}");
    }
    let e = take_flags(&args(&["--backend", "a", "--backend", "b", "runs"])).unwrap_err();
    assert!(e.contains("once"), "{e}");
}

#[test]
fn b584_the_value_of_another_option_is_never_taken_for_a_flag() {
    // `--text` takes the next word whatever it is.
    let (f, rest) = take_flags(&args(&["say", "R1", "--text", "--backend"])).unwrap();
    assert_eq!(f.backend, None);
    assert_eq!(rest, args(&["say", "R1", "--text", "--backend"]));
}

#[test]
fn b584_customer_content_is_a_switch() {
    let (f, rest) = take_flags(&args(&[
        "do it",
        "--s3-only",
        "--customer-content",
        "--direct",
    ]))
    .unwrap();
    assert!(f.customer_content);
    assert_eq!(rest, args(&["do it", "--s3-only", "--direct"]));
}
