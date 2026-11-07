//! The open-files cap, the parts that start no process: the pure function that decides the new soft limit, the reading of the stage
//! variable `AGENTD_OPEN_FILES_CAP`, and `apply` over a stand-in for the kernel's limit with the lines it logs. The processes that must see
//! the cap are in `nofile_process.rs` (the engine launcher and the report step) and `nofile_serve.rs` (the `agentd serve` binary).
#![allow(clippy::unwrap_used)]

use agentd::config::open_files_cap;
use agentd::logging::MemoryLog;
use agentd::nofile::{
    DEFAULT_CAP, Limits, MAX_CAP, MIN_CAP, Outcome, Setting, UNLIMITED, VAR, apply, cap_nofile,
};
use std::cell::Cell;

// ---- the pure function ------------------------------------------------------------------------------------------------------------

#[test]
fn a_soft_limit_above_the_cap_is_lowered_to_the_cap() {
    // The container default of some machines: a soft and a hard limit of about a billion.
    assert_eq!(
        cap_nofile(1_073_741_816, 1_073_741_816, 65_536),
        Some(65_536)
    );
    // A workstation's hard limit above the cap changes nothing for the cap.
    assert_eq!(cap_nofile(1_048_576, 1_048_576, 65_536), Some(65_536));
    // No limit at all is above every cap.
    assert_eq!(cap_nofile(UNLIMITED, UNLIMITED, 65_536), Some(65_536));
    assert_eq!(cap_nofile(UNLIMITED, 1_000_000, 65_536), Some(65_536));
}

#[test]
fn a_soft_limit_at_or_below_the_cap_is_left_alone() {
    assert_eq!(cap_nofile(1_024, 4_096, 65_536), None);
    assert_eq!(cap_nofile(65_536, 1_048_576, 65_536), None);
    assert_eq!(cap_nofile(65_536, 65_536, 65_536), None);
    assert_eq!(cap_nofile(256, 256, 65_536), None);
}

#[test]
fn the_new_soft_limit_never_passes_the_hard_limit() {
    // The hard limit below the cap: the soft limit goes to the hard limit, never above it.
    assert_eq!(cap_nofile(100_000, 70_000, 65_536), Some(65_536));
    assert_eq!(cap_nofile(100_000, 50_000, 65_536), Some(50_000));
    // A soft limit above its own hard limit does not occur in the kernel; if it were read, the result would still stay under the hard one.
    assert_eq!(cap_nofile(1_000_000, 4_096, 65_536), Some(4_096));
}

#[test]
fn a_cap_of_zero_and_a_new_limit_of_zero_change_nothing() {
    assert_eq!(cap_nofile(1_073_741_816, 1_073_741_816, 0), None);
    // A hard limit of zero would make a soft limit of zero: that is never set.
    assert_eq!(cap_nofile(100_000, 0, 65_536), None);
}

#[test]
fn the_function_only_lowers() {
    let values = [
        0,
        1,
        255,
        256,
        1_024,
        4_096,
        65_535,
        65_536,
        65_537,
        1_048_576,
        1_073_741_816,
        UNLIMITED,
    ];
    for soft in values {
        for hard in values {
            for cap in values {
                if let Some(new) = cap_nofile(soft, hard, cap) {
                    assert!(new < soft, "raised or kept: {soft} {hard} {cap} -> {new}");
                    assert!(new <= hard, "above hard: {soft} {hard} {cap} -> {new}");
                    assert!(new <= cap, "above cap: {soft} {hard} {cap} -> {new}");
                    assert!(new > 0, "zero: {soft} {hard} {cap}");
                }
            }
        }
    }
}

// ---- the variable ---------------------------------------------------------------------------------------------------------------------

#[test]
fn unset_or_blank_is_the_default_cap() {
    assert_eq!(DEFAULT_CAP, 65_536);
    assert_eq!(Setting::from_value(None), Setting::Cap(65_536));
    assert_eq!(Setting::from_value(Some("")), Setting::Cap(65_536));
    assert_eq!(Setting::from_value(Some("  \t")), Setting::Cap(65_536));
}

#[test]
fn off_false_no_and_zero_leave_the_limit_as_it_is() {
    for v in [
        "0", "off", "OFF", "Off", "false", "False", "no", " no ", "NO",
    ] {
        assert_eq!(Setting::from_value(Some(v)), Setting::Off, "{v:?}");
    }
}

#[test]
fn a_number_from_256_to_4194304_is_the_cap() {
    assert_eq!((MIN_CAP, MAX_CAP), (256, 4_194_304));
    for (v, n) in [
        ("256", 256),
        ("1024", 1_024),
        (" 65536 ", 65_536),
        ("1048576", 1_048_576),
        ("4194304", 4_194_304),
    ] {
        assert_eq!(Setting::from_value(Some(v)), Setting::Cap(n), "{v:?}");
    }
}

#[test]
fn anything_else_is_a_config_error_and_the_default_applies() {
    for v in [
        "255",
        "1",
        "4194305",
        "99999999999999999999",
        "-5",
        "1.5",
        "65,536",
        "65 536",
        "1e5",
        "on",
        "yes",
        "abc",
    ] {
        match Setting::from_value(Some(v)) {
            Setting::Invalid(shown) => {
                assert!(shown.contains(v.trim()), "{v:?} shown as {shown:?}")
            }
            other => panic!("{v:?} gave {other:?}"),
        }
    }
}

#[test]
fn the_setting_is_read_next_to_the_other_stage_variables() {
    assert_eq!(VAR, "AGENTD_OPEN_FILES_CAP");
    let with = |value: Option<&'static str>| {
        open_files_cap(&|k: &str| {
            if k == VAR {
                value.map(str::to_string)
            } else {
                None
            }
        })
    };
    assert_eq!(with(None), Setting::Cap(65_536));
    assert_eq!(with(Some("4096")), Setting::Cap(4_096));
    assert_eq!(with(Some("off")), Setting::Off);
    assert_eq!(with(Some("lots")), Setting::Invalid("lots".to_string()));
    // Another variable does not move it.
    assert_eq!(
        open_files_cap(&|k: &str| (k == "AGENTD_DISK_LOW_MB").then(|| "off".to_string())),
        Setting::Cap(65_536)
    );
}

// ---- apply, over a stand-in for the kernel ------------------------------------------------------------------------------

struct Fake {
    soft: Cell<u64>,
    hard: u64,
    refuse: Option<&'static str>,
    /// The call is accepted and changes nothing.
    ignore: bool,
    sets: Cell<u32>,
}

impl Fake {
    fn new(soft: u64, hard: u64) -> Self {
        Self {
            soft: Cell::new(soft),
            hard,
            refuse: None,
            ignore: false,
            sets: Cell::new(0),
        }
    }
}

impl Limits for Fake {
    fn soft_hard(&self) -> (u64, u64) {
        (self.soft.get(), self.hard)
    }

    fn set_soft(&self, soft: u64, hard: u64) -> Result<(), String> {
        self.sets.set(self.sets.get() + 1);
        assert_eq!(hard, self.hard, "the hard limit is passed back unchanged");
        if let Some(why) = self.refuse {
            return Err(why.to_string());
        }
        if !self.ignore {
            self.soft.set(soft);
        }
        Ok(())
    }
}

#[test]
fn apply_lowers_the_soft_limit_and_logs_one_line_with_both_numbers() {
    let limits = Fake::new(1_073_741_816, 1_073_741_816);
    let log = MemoryLog::new();
    let out = apply(&Setting::Cap(65_536), &limits, log.as_ref());
    assert_eq!(
        out,
        Outcome::Lowered {
            from: 1_073_741_816,
            to: 65_536,
            hard: 1_073_741_816
        }
    );
    assert_eq!(limits.soft_hard(), (65_536, 1_073_741_816));
    let lines = log.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("open files limit lowered"),
        "{lines:?}"
    );
    for n in ["1073741816", "65536"] {
        assert!(lines[0].contains(n), "{lines:?}");
    }
}

#[test]
fn apply_leaves_a_limit_at_or_below_the_cap_and_says_so() {
    let limits = Fake::new(1_024, 4_096);
    let log = MemoryLog::new();
    assert_eq!(
        apply(&Setting::Cap(65_536), &limits, log.as_ref()),
        Outcome::Left {
            soft: 1_024,
            hard: 4_096
        }
    );
    assert_eq!(limits.sets.get(), 0, "no call when nothing changes");
    let lines = log.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("open files limit left alone"),
        "{lines:?}"
    );
    assert!(
        lines[0].contains("1024") && lines[0].contains("4096"),
        "{lines:?}"
    );
}

#[test]
fn apply_with_the_switch_off_touches_nothing() {
    let limits = Fake::new(1_073_741_816, 1_073_741_816);
    let log = MemoryLog::new();
    assert_eq!(
        apply(&Setting::Off, &limits, log.as_ref()),
        Outcome::Off {
            soft: 1_073_741_816,
            hard: 1_073_741_816
        }
    );
    assert_eq!(limits.sets.get(), 0);
    assert_eq!(limits.soft_hard().0, 1_073_741_816);
    let lines = log.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("open files limit left alone") && lines[0].contains(VAR),
        "{lines:?}"
    );
}

#[test]
fn apply_names_a_bad_value_and_uses_the_default() {
    let limits = Fake::new(1_073_741_816, 1_073_741_816);
    let log = MemoryLog::new();
    let out = apply(&Setting::Invalid("lots".to_string()), &limits, log.as_ref());
    assert_eq!(
        out,
        Outcome::Lowered {
            from: 1_073_741_816,
            to: DEFAULT_CAP,
            hard: 1_073_741_816
        }
    );
    let lines = log.lines();
    assert_eq!(
        lines.len(),
        2,
        "the config error, then what was done: {lines:?}"
    );
    assert!(
        lines[0].contains(VAR) && lines[0].contains("lots"),
        "{lines:?}"
    );
    assert!(
        lines[0].contains("256") && lines[0].contains("4194304"),
        "the line names the range: {lines:?}"
    );
    assert!(
        lines[1].starts_with("open files limit lowered"),
        "{lines:?}"
    );
}

#[test]
fn apply_that_the_kernel_refuses_is_a_log_line_and_never_a_failure_to_start() {
    let mut limits = Fake::new(1_073_741_816, 1_073_741_816);
    limits.refuse = Some("Operation not permitted");
    let log = MemoryLog::new();
    let out = apply(&Setting::Cap(65_536), &limits, log.as_ref());
    match out {
        Outcome::Failed { soft, hard, why } => {
            assert_eq!((soft, hard), (1_073_741_816, 1_073_741_816));
            assert!(why.contains("Operation not permitted"), "{why}");
        }
        other => panic!("{other:?}"),
    }
    let lines = log.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("open files limit not lowered"),
        "{lines:?}"
    );
    assert!(lines[0].contains("Operation not permitted"), "{lines:?}");
}

#[test]
fn apply_reads_the_limit_back_and_does_not_claim_a_change_that_did_not_happen() {
    let mut limits = Fake::new(1_073_741_816, 1_073_741_816);
    limits.ignore = true;
    let log = MemoryLog::new();
    let out = apply(&Setting::Cap(65_536), &limits, log.as_ref());
    assert!(matches!(out, Outcome::Failed { .. }), "{out:?}");
    let lines = log.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("open files limit not lowered"),
        "{lines:?}"
    );
}

#[test]
fn an_unlimited_limit_is_shown_in_words() {
    let limits = Fake::new(UNLIMITED, UNLIMITED);
    let log = MemoryLog::new();
    let out = apply(&Setting::Cap(65_536), &limits, log.as_ref());
    assert_eq!(
        out,
        Outcome::Lowered {
            from: UNLIMITED,
            to: 65_536,
            hard: UNLIMITED
        }
    );
    let lines = log.lines();
    assert!(lines[0].contains("unlimited"), "{lines:?}");
    assert!(!lines[0].contains("18446744073709551615"), "{lines:?}");
}
