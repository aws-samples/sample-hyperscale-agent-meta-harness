//! The setting `ui.update_notice`: whether the terminal app looks for a newer release and shows the update notice. On unless the file
//! says `false`; `HAMH_NO_UPDATE_NOTICE` (`1`, `true`, `yes` or `on`) turns it off whatever the file says and can never turn it on. One
//! registry row, the last; it rides in no request and is no security key; a change takes effect at once.
#![allow(clippy::unwrap_used)]

use ham_core::config::{
    Config, KEYS, NO_UPDATE_NOTICE, get_key, parse_config, read_config, render_config, set_key,
    unset_key, update_notice_on,
};
use ham_core::settings::{
    Applies, DefaultValue, Group, Key, Kind, Order, ROWS, reference_markdown,
};

fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |key| {
        pairs
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| (*value).to_string())
    }
}

#[test]
fn b513_the_switch_is_a_row_of_the_registry() {
    assert_eq!(Key::ALL.len(), 52);
    assert_eq!(ROWS.len(), 52);
    assert_eq!(KEYS.len(), 52);
    assert_eq!(Key::ALL[50], Key::UiUpdateNotice);
    assert_eq!(KEYS[50], ("ui.update_notice", "ui.update_notice"));
    assert_eq!(Key::lookup("ui.update_notice"), Some(Key::UiUpdateNotice));

    let row = Key::UiUpdateNotice.row();
    assert_eq!(row.key, Key::UiUpdateNotice);
    assert_eq!(row.name, "ui.update_notice");
    assert_eq!(row.file, ("ui", "update_notice"));
    assert_eq!(row.kind, Kind::Bool);
    assert_eq!(row.env, "HAMH_NO_UPDATE_NOTICE");
    assert_eq!(row.env, NO_UPDATE_NOTICE);
    assert_eq!(row.order, Order::EnvForcesOff);
    assert_eq!(row.group, Group::Interface);
    assert_eq!(row.default_text, "on");
    assert_eq!(row.default, DefaultValue::Bool(true));
    assert_eq!(row.applies, Applies::Now);
    assert_eq!(row.since, 1);
    assert_eq!(row.label, "Update notice");
    assert_eq!(row.wire, None, "the key rides in no request");
    assert!(!row.security, "the key is no security key");
    assert!(row.flag.is_empty(), "no flag sets it");
    // The help says what the look is, when it runs, what it sends and how to turn it off.
    for words in [
        "when the terminal app starts and once a day while it stays open",
        "again after an hour when a look failed",
        "one git call that asks update_source for the names of its branches and tags",
        "ham writes no file for it and sends nothing of its own",
        "~/.netrc",
        "HAMH_NO_UPDATE_NOTICE=1 turns it off, and so does update_source = off",
        // A running app reads the file when a look starts, so only the Settings screen reaches it at once.
        "A change made on the Settings screen applies at once; a change made any other way applies with the next look or the next start of the terminal app.",
    ] {
        assert!(row.help.contains(words), "{words}: {}", row.help);
    }
    // The help of the source it reads points at the switch.
    assert!(
        Key::UpdateSource.row().help.ends_with(
            "The terminal app looks at the same repository for a newer release (ui.update_notice)."
        ),
        "{}",
        Key::UpdateSource.row().help
    );

    // The line `ham config docs` prints for it, which the user guide holds between its two markers.
    // It is the row before the last: added the row of the earlier run's records after it.
    let table = reference_markdown();
    assert_eq!(
        table.lines().rev().nth(1).unwrap(),
        "| `ui.update_notice` | true or false | on | none | `HAMH_NO_UPDATE_NOTICE` (forces off) | now |"
    );
}

#[test]
fn b513_the_key_is_written_read_and_removed_like_the_other_interface_keys() {
    let cfg = set_key(&Config::default(), "ui.update_notice", "false").unwrap();
    assert_eq!(
        get_key(&cfg, "ui.update_notice").unwrap().as_deref(),
        Some("false")
    );
    let text = render_config(&cfg);
    assert!(
        text.contains("[ui]") && text.contains("update_notice = false"),
        "{text}"
    );
    assert_eq!(parse_config(&text).unwrap(), cfg);
    assert!(read_config(&text).findings.is_empty());
    assert!(set_key(&Config::default(), "ui.update_notice", "maybe").is_err());
    assert_eq!(
        unset_key(&cfg, "ui.update_notice").unwrap(),
        Config::default()
    );
}

#[test]
fn b513_the_look_is_on_unless_the_file_or_the_switch_says_otherwise() {
    let on = Config::default();
    let off = set_key(&on, "ui.update_notice", "false").unwrap();
    let explicit_on = set_key(&on, "ui.update_notice", "true").unwrap();
    assert!(update_notice_on(&env(&[]), &on));
    assert!(update_notice_on(&env(&[]), &explicit_on));
    assert!(!update_notice_on(&env(&[]), &off));
    // The variable can only switch the look off.
    for word in ["1", "true", "yes", "on", "ON", " Yes "] {
        let pairs = [("HAMH_NO_UPDATE_NOTICE", word)];
        assert!(!update_notice_on(&env(&pairs), &on), "{word}");
        assert!(!update_notice_on(&env(&pairs), &explicit_on), "{word}");
    }
    for word in ["", "0", "false", "off", "no"] {
        let pairs = [("HAMH_NO_UPDATE_NOTICE", word)];
        assert!(update_notice_on(&env(&pairs), &on), "{word:?}");
        assert!(!update_notice_on(&env(&pairs), &off), "{word:?}");
    }
}
