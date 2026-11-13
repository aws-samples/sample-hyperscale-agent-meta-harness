//! Wave 2 (client): the longest wait for an answer is the longest run, and the clipboard limit of the terminal screen is a setting.
//!
//! * `--input-wait`, `HAMH_INPUT_WAIT` and `input_wait_minutes` take minutes up to the protocol's ceiling, which is the longest run. The agent cuts
//!   every window to the time its session has left, so the client needs no bound of its own below that.
//! * `ui.copy_limit_kib` sets the most KiB one copy to the clipboard carries; the default stays 64.
#![allow(clippy::unwrap_used)]

use ham_core::command::MAX_INPUT_WAIT_MINUTES;
use ham_core::config::{Config, copy_limit_bytes, get_key, read_config, set_key, unset_key};
use ham_core::request::{WaitPrefs, input_wait_minutes};
use ham_core::settings::{DefaultValue, Group, Key, Kind, Order};

#[test]
fn b776w_input_wait_takes_minutes_up_to_the_longest_run() {
    assert_eq!(
        u64::from(MAX_INPUT_WAIT_MINUTES) * 60,
        u64::from(harness_protocol::direct::MAX_INPUT_WAIT_S)
    );
    let none = WaitPrefs::default();
    // 8 hours was refused over the old 4 hour ceiling.
    assert_eq!(input_wait_minutes(Some(480), &none).unwrap().0, 480);
    assert_eq!(
        input_wait_minutes(Some(MAX_INPUT_WAIT_MINUTES), &none)
            .unwrap()
            .0,
        MAX_INPUT_WAIT_MINUTES
    );
    let e = input_wait_minutes(Some(MAX_INPUT_WAIT_MINUTES + 1), &none)
        .unwrap_err()
        .to_string();
    assert!(e.contains(&MAX_INPUT_WAIT_MINUTES.to_string()), "{e}");
    // The file key reads the same ceiling.
    let ok = read_config(&format!(
        "configVersion = 1\ninputWaitMinutes = {MAX_INPUT_WAIT_MINUTES}\n"
    ));
    assert!(ok.findings.is_empty(), "{:?}", ok.findings);
    let over = read_config(&format!(
        "configVersion = 1\ninputWaitMinutes = {}\n",
        MAX_INPUT_WAIT_MINUTES + 1
    ));
    assert!(!over.findings.is_empty());
}

#[test]
fn b776w_the_copy_limit_has_a_registry_row_and_defaults_to_64_kib() {
    let key = Key::lookup("ui.copy_limit_kib").expect("the registry holds ui.copy_limit_kib");
    let row = key.row();
    assert_eq!(row.name, "ui.copy_limit_kib");
    assert_eq!(row.file, ("ui", "copy_limit_kib"));
    assert_eq!(row.default, DefaultValue::Number(64));
    assert_eq!(row.order, Order::FileOnly);
    assert_eq!(row.group, Group::Interface);
    assert!(
        !row.security && row.wire.is_none(),
        "it rides in no request"
    );
    match row.kind {
        Kind::Number { min, max, unit } => {
            assert_eq!((min, unit), (1, "KiB"));
            assert!(
                max >= 16 * 1024,
                "terminals that take MiB can be served: {max}"
            );
        }
        other => panic!("a whole number of KiB, got {other:?}"),
    }
    assert_eq!(copy_limit_bytes(&Config::default()), 64 * 1024);
}

#[test]
fn b776w_the_copy_limit_is_written_read_and_removed_like_the_other_interface_keys() {
    let cfg = set_key(&Config::default(), "ui.copy_limit_kib", "512").unwrap();
    assert_eq!(copy_limit_bytes(&cfg), 512 * 1024);
    assert_eq!(
        get_key(&cfg, "ui.copy_limit_kib").unwrap().as_deref(),
        Some("512")
    );
    let read = read_config("configVersion = 1\n[ui]\ncopy_limit_kib = 2048\n");
    assert!(read.findings.is_empty(), "{:?}", read.findings);
    assert_eq!(copy_limit_bytes(&read.config), 2048 * 1024);
    for bad in ["0", "-1", "lots", "", "1.5"] {
        assert!(
            set_key(&Config::default(), "ui.copy_limit_kib", bad).is_err(),
            "{bad:?} is no number of KiB"
        );
    }
    let too_big = set_key(&Config::default(), "ui.copy_limit_kib", "99999999");
    assert!(too_big.is_err(), "a value past the ceiling is refused");
    let wrong_type = read_config("configVersion = 1\n[ui]\ncopy_limit_kib = \"big\"\n");
    assert!(!wrong_type.findings.is_empty());
    assert_eq!(
        unset_key(&cfg, "ui.copy_limit_kib").unwrap(),
        Config::default()
    );
    assert_eq!(copy_limit_bytes(&Config::default()), 64 * 1024);
}
