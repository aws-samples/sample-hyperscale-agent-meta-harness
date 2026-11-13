//! The `ui.mascot` setting: `auto` (the mascot in the look the environment asks for), `ascii` (the same, in the ASCII look, for a terminal that
//! draws the block glyphs double wide) or `off` (no mascot beside the name and none in the empty table). `HAMH_NO_MASCOT=1` wins over the file.
//! One registry row, one reader; the row says the change takes effect now, like `ui.bell`.
#![allow(clippy::unwrap_used)]

use ham_core::config::{
    Config, KEYS, MascotMode, get_key, mascot_mode, parse_config, read_config, render_config,
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

fn with(mode: &str) -> Config {
    set_key(&Config::default(), "ui.mascot", mode).unwrap()
}

#[test]
fn the_registry_has_one_row_for_it_next_to_the_bell() {
    assert_eq!(Key::ALL.len(), 46);
    assert_eq!(ROWS.len(), 46);
    assert_eq!(KEYS.len(), 46);
    assert_eq!(Key::ALL[14], Key::UiBell);
    assert_eq!(Key::ALL[15], Key::UiMascot);
    let row = Key::UiMascot.row();
    assert_eq!(row.name, "ui.mascot");
    assert_eq!(row.file, ("ui", "mascot"));
    assert_eq!(row.kind, Kind::Mascot);
    assert_eq!(row.env, "HAMH_NO_MASCOT");
    assert_eq!(row.order, Order::EnvForcesOff);
    assert_eq!(row.group, Group::Interface);
    assert_eq!(row.default_text, "auto");
    assert_eq!(row.default, DefaultValue::Absent);
    assert_eq!(row.since, 1);
    // A change takes effect now, as the bell's does.
    assert_eq!(row.applies, Applies::Now);
    assert_eq!(row.applies, Key::UiBell.row().applies);
    assert!(!row.security);
    assert!(KEYS.contains(&("ui.mascot", "ui.mascot")));
    assert_eq!(Key::lookup("ui.mascot"), Some(Key::UiMascot));
}

#[test]
fn the_three_words_are_the_values_and_nothing_else_is() {
    for word in ["auto", "ascii", "off"] {
        let cfg = with(word);
        assert_eq!(get_key(&cfg, "ui.mascot").unwrap().as_deref(), Some(word));
    }
    for word in ["AUTO", " Ascii ", "OFF"] {
        let cfg = with(word);
        assert_eq!(
            get_key(&cfg, "ui.mascot").unwrap(),
            Some(word.trim().to_ascii_lowercase())
        );
    }
    for bad in [
        "",
        "on",
        "true",
        "false",
        "blocks",
        "none",
        "1",
        "loud",
        "auto ascii",
    ] {
        assert!(
            set_key(&Config::default(), "ui.mascot", bad).is_err(),
            "{bad:?} is not a value of ui.mascot"
        );
    }
}

#[test]
fn the_default_is_auto_and_unset_goes_back_to_it() {
    let none = env(&[]);
    assert_eq!(mascot_mode(&none, &Config::default()), MascotMode::Auto);
    assert_eq!(mascot_mode(&none, &with("ascii")), MascotMode::Ascii);
    assert_eq!(mascot_mode(&none, &with("off")), MascotMode::Off);
    let back = unset_key(&with("off"), "ui.mascot").unwrap();
    assert_eq!(get_key(&back, "ui.mascot").unwrap(), None);
    assert_eq!(mascot_mode(&none, &back), MascotMode::Auto);
}

#[test]
fn the_environment_can_only_switch_it_off_and_beats_the_file() {
    // On: 1, true, yes and on, in any case, with white space around.
    for on in ["1", "true", "TRUE", "yes", "On", " 1 "] {
        let e = [("HAMH_NO_MASCOT", on)];
        for file in [Config::default(), with("auto"), with("ascii"), with("off")] {
            assert_eq!(mascot_mode(&env(&e), &file), MascotMode::Off, "{on:?}");
        }
    }
    // Absent, empty, 0, no, false and anything else leave the file's value in force.
    for quiet in ["", "0", "no", "false", "off", "banana", "2"] {
        let e = [("HAMH_NO_MASCOT", quiet)];
        assert_eq!(
            mascot_mode(&env(&e), &Config::default()),
            MascotMode::Auto,
            "{quiet:?}"
        );
        assert_eq!(
            mascot_mode(&env(&e), &with("ascii")),
            MascotMode::Ascii,
            "{quiet:?}"
        );
        assert_eq!(
            mascot_mode(&env(&e), &with("off")),
            MascotMode::Off,
            "{quiet:?}"
        );
    }
}

#[test]
fn the_file_reads_it_from_the_ui_table_and_refuses_another_value() {
    let ok = read_config("configVersion = 1\n\n[ui]\nmascot = \"ascii\"\n");
    assert!(!ok.findings.has_errors(), "{:?}", ok.findings);
    assert_eq!(mascot_mode(&env(&[]), &ok.config), MascotMode::Ascii);
    // A value that is not one of the three words is a finding, and the key is not read.
    let bad = read_config("configVersion = 1\n\n[ui]\nmascot = \"loud\"\n");
    assert!(bad.findings.has_errors());
    assert_eq!(mascot_mode(&env(&[]), &bad.config), MascotMode::Auto);
    let wrong_type = read_config("configVersion = 1\n\n[ui]\nmascot = true\n");
    assert!(wrong_type.findings.has_errors());
    // A file with the key and no configVersion gets the version when it is written.
    let written = render_config(&with("off"));
    assert!(written.contains("configVersion = 1"), "{written}");
    assert!(written.contains("mascot = \"off\""), "{written}");
    assert_eq!(parse_config(&written).unwrap(), with("off"));
}
