// COMPAT-CAH: the notices are those of the window of the earlier names; this setting is the one switch that silences them.
//! The `ui.deprecation_notices` setting: on unless the file says `false`, and `HAMH_NO_DEPRECATION_NOTICES` (`1`, `true`, `yes` or `on`) turns
//! it off whatever the file says. One registry row (the second to last since `ui.mascot_motion`), one reader, a change takes effect at the next start
//! (the notices are printed once, before the screen opens).
#![allow(clippy::unwrap_used)]

use ham_core::config::{
    Config, KEYS, deprecation_notices_on, get_key, parse_config, read_config, render_config,
    set_key, unset_key,
};
use ham_core::settings::{Applies, DefaultValue, Group, Key, Kind, Order, ROWS};

fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |key| {
        pairs
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| (*value).to_string())
    }
}

#[test]
fn the_registry_has_one_row_for_it_before_the_last() {
    assert_eq!(Key::ALL.len(), 35);
    assert_eq!(ROWS.len(), 35);
    assert_eq!(KEYS.len(), 35);
    assert_eq!(Key::ALL[30], Key::UiDeprecationNotices);
    let row = Key::UiDeprecationNotices.row();
    assert_eq!(row.name, "ui.deprecation_notices");
    assert_eq!(row.file, ("ui", "deprecation_notices"));
    assert_eq!(row.kind, Kind::Bool);
    assert_eq!(row.env, "HAMH_NO_DEPRECATION_NOTICES");
    assert_eq!(row.order, Order::EnvForcesOff);
    assert_eq!(row.group, Group::Interface);
    assert_eq!(row.default_text, "on");
    assert_eq!(row.default, DefaultValue::Bool(true));
    assert_eq!(row.applies, Applies::Restart);
    assert_eq!(
        KEYS[30],
        ("ui.deprecation_notices", "ui.deprecation_notices")
    );
    assert_eq!(
        Key::lookup("ui.deprecation_notices"),
        Some(Key::UiDeprecationNotices)
    );
}

#[test]
fn the_key_is_written_read_and_removed_like_the_other_interface_keys() {
    let cfg = set_key(&Config::default(), "ui.deprecation_notices", "false").unwrap();
    assert_eq!(
        get_key(&cfg, "ui.deprecation_notices").unwrap().as_deref(),
        Some("false")
    );
    let text = render_config(&cfg);
    assert!(
        text.contains("[ui]") && text.contains("deprecation_notices = false"),
        "{text}"
    );
    assert_eq!(parse_config(&text).unwrap(), cfg);
    assert!(read_config(&text).findings.is_empty());
    assert!(set_key(&Config::default(), "ui.deprecation_notices", "maybe").is_err());
    let cleared = unset_key(&cfg, "ui.deprecation_notices").unwrap();
    assert_eq!(cleared, Config::default());
}

#[test]
fn the_notices_are_on_unless_the_file_or_the_switch_says_otherwise() {
    let on = Config::default();
    let off = set_key(&on, "ui.deprecation_notices", "false").unwrap();
    let explicit_on = set_key(&on, "ui.deprecation_notices", "true").unwrap();
    assert!(deprecation_notices_on(&env(&[]), &on));
    assert!(deprecation_notices_on(&env(&[]), &explicit_on));
    assert!(!deprecation_notices_on(&env(&[]), &off));
    // The switch can only turn the notices off.
    for word in ["1", "true", "yes", "on", "ON"] {
        let pairs = [("HAMH_NO_DEPRECATION_NOTICES", word)];
        assert!(!deprecation_notices_on(&env(&pairs), &on), "{word}");
        assert!(
            !deprecation_notices_on(&env(&pairs), &explicit_on),
            "{word}"
        );
    }
    for word in ["", "0", "false", "off"] {
        let pairs = [("HAMH_NO_DEPRECATION_NOTICES", word)];
        assert!(deprecation_notices_on(&env(&pairs), &on), "{word:?}");
        assert!(!deprecation_notices_on(&env(&pairs), &off), "{word:?}");
    }
}
