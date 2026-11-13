//! The `ui.mascot_motion` setting: `true` (the default) lets the smoke under the mascot's feet move in the empty Runs list, `false` shows the
//! still picture. No flag and no environment variable take part: `ui.mascot` and `HAMH_NO_MASCOT` still hide the whole mascot. One registry row
//! at the end, one reader, and a change takes effect now, like the bell and the mascot.
#![allow(clippy::unwrap_used)]

use ham_core::config::{
    Config, KEYS, get_key, mascot_motion_on, parse_config, read_config, render_config, set_key,
    unset_key,
};
use ham_core::settings::{Applies, DefaultValue, Group, Key, Kind, Order, ROWS};

#[test]
fn the_registry_has_one_row_for_it_at_the_end() {
    assert_eq!(Key::ALL.len(), 46);
    assert_eq!(ROWS.len(), 46);
    assert_eq!(KEYS.len(), 46);
    assert_eq!(Key::ALL[31], Key::UiMascotMotion);
    let row = Key::UiMascotMotion.row();
    assert_eq!(row.name, "ui.mascot_motion");
    assert_eq!(row.file, ("ui", "mascot_motion"));
    assert_eq!(row.kind, Kind::Bool);
    assert_eq!(row.env, "", "no environment variable takes part");
    assert_eq!(row.flag, "", "no flag sets it");
    assert_eq!(row.order, Order::FileOnly);
    assert_eq!(row.group, Group::Interface);
    assert_eq!(row.default_text, "on");
    assert_eq!(row.default, DefaultValue::Bool(true));
    assert_eq!(row.since, 1);
    // A change takes effect now, as the bell's and the mascot's do.
    assert_eq!(row.applies, Applies::Now);
    assert_eq!(row.applies, Key::UiBell.row().applies);
    assert!(!row.security);
    assert_eq!(row.wire, None, "the setting rides in no request");
    assert_eq!(KEYS[31], ("ui.mascot_motion", "ui.mascot_motion"));
    assert_eq!(Key::lookup("ui.mascot_motion"), Some(Key::UiMascotMotion));
}

#[test]
fn the_key_is_written_read_and_removed_like_the_other_interface_keys() {
    let cfg = set_key(&Config::default(), "ui.mascot_motion", "off").unwrap();
    assert_eq!(
        get_key(&cfg, "ui.mascot_motion").unwrap().as_deref(),
        Some("false")
    );
    let text = render_config(&cfg);
    assert!(
        text.contains("[ui]") && text.contains("mascot_motion = false"),
        "{text}"
    );
    assert_eq!(parse_config(&text).unwrap(), cfg);
    assert!(read_config(&text).findings.is_empty());
    for word in ["on", "true", "yes", "1"] {
        let on = set_key(&Config::default(), "ui.mascot_motion", word).unwrap();
        assert_eq!(
            get_key(&on, "ui.mascot_motion").unwrap().as_deref(),
            Some("true"),
            "{word}"
        );
    }
    for bad in ["maybe", "", "auto", "2"] {
        assert!(
            set_key(&Config::default(), "ui.mascot_motion", bad).is_err(),
            "{bad:?} is not a value of ui.mascot_motion"
        );
    }
    let cleared = unset_key(&cfg, "ui.mascot_motion").unwrap();
    assert_eq!(cleared, Config::default());
    // Not set reads as not set.
    assert_eq!(
        get_key(&Config::default(), "ui.mascot_motion").unwrap(),
        None
    );
}

#[test]
fn the_motion_is_on_unless_the_file_says_false() {
    assert!(mascot_motion_on(&Config::default()));
    let off = set_key(&Config::default(), "ui.mascot_motion", "false").unwrap();
    let on = set_key(&Config::default(), "ui.mascot_motion", "true").unwrap();
    assert!(!mascot_motion_on(&off));
    assert!(mascot_motion_on(&on));
    // The other interface keys do not decide it.
    let mascot_off = set_key(&Config::default(), "ui.mascot", "off").unwrap();
    assert!(mascot_motion_on(&mascot_off));
    let bell_off = set_key(&Config::default(), "ui.bell", "off").unwrap();
    assert!(mascot_motion_on(&bell_off));
}

#[test]
fn the_reader_takes_the_key_from_the_ui_table_and_names_a_bad_value() {
    let read = read_config("configVersion = 1\n[ui]\nmascot_motion = false\n");
    assert!(read.findings.is_empty(), "{:?}", read.findings);
    assert!(!mascot_motion_on(&read.config));
    let bad = read_config("configVersion = 1\n[ui]\nmascot_motion = \"slow\"\n");
    assert!(
        !bad.findings.is_empty(),
        "a word where a boolean belongs is a finding"
    );
    // The key does not exist at the top level.
    let top = read_config("mascot_motion = false\n");
    assert!(!top.findings.is_empty());
}
