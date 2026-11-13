//! Wave 2 (client): the most one copy to the clipboard carries is `ui.copy_limit_kib` (64 by default). A copy over the limit is still
//! refused whole, never cut, and the refusal names the limit that applies.
#![allow(clippy::unwrap_used)]

use ham::cli::apply_ui_settings;
use ham::tui::copy::{COPY_LIMIT, CopyError, prepare, prepare_within};
use ham::tui::{App, DirectSettings};
use ham_core::config::{Config, set_key};

fn no_env(_: &str) -> Option<String> {
    None
}

#[test]
fn b776w_a_copy_may_carry_what_the_caller_allows() {
    let text = "a".repeat(512 * 1024);
    // The default still refuses it, whole.
    assert!(matches!(
        prepare(&text),
        Err(CopyError::TooLarge { bytes, limit }) if bytes == text.len() && limit == COPY_LIMIT
    ));
    // A caller that allows 1 MiB gets all of it.
    let ok = prepare_within(&text, 1024 * 1024).unwrap();
    assert_eq!(ok.bytes(), text.len());
    assert_eq!(ok.text(), text);
    // One byte past the limit is refused and the refusal names the limit in force, not 64.
    let at = "b".repeat(2 * 1024);
    assert_eq!(prepare_within(&at, 2 * 1024).unwrap().bytes(), 2 * 1024);
    let e = prepare_within(&format!("{at}b"), 2 * 1024).unwrap_err();
    assert_eq!(
        e,
        CopyError::TooLarge {
            bytes: 2 * 1024 + 1,
            limit: 2 * 1024
        }
    );
    assert!(e.to_string().contains("2 KiB"), "{e}");
    // Text far over the limit is refused before the scan, whatever the limit is.
    let huge = "c".repeat(9 * 1024);
    assert!(prepare_within(&huge, 2 * 1024).is_err());
}

#[test]
fn b776w_the_screen_takes_its_copy_limit_from_the_file() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    assert_eq!(app.copy_limit(), COPY_LIMIT, "a person who changed nothing");
    apply_ui_settings(&mut app, &no_env, &Config::default());
    assert_eq!(app.copy_limit(), 64 * 1024);
    let cfg = set_key(&Config::default(), "ui.copy_limit_kib", "256").unwrap();
    apply_ui_settings(&mut app, &no_env, &cfg);
    assert_eq!(app.copy_limit(), 256 * 1024);
}
