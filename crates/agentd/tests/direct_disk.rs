//! The session's two disks: the numbers, the limits, the words and the real measurement.
//!
//! A session has `/` (the root volume: the image layers, `/tmp`, `/var/lib/agentd`, the home folder; 5.7 GiB free at the start) and
//! `/mnt/workspace` (managed session storage with the checkout: a 1 GiB hard limit, and `df` and `statvfs` report 0 used on it). A full
//! mount made the engine answer error -32603 on every turn, and the failover ladder took that for a provider fault. These tests pin the
//! pure rules (what counts as full, what counts as pressure, the words that name the cause) and the measurement of a real folder.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::acp::AcpError;
use agentd::direct::disk::{
    self, DEFAULT_LOW_MB, DEFAULT_WORKSPACE_LIMIT_MB, DiskProbe, DiskWatch, HostDisk, Limits,
    Reading, Signal, WalkBudget, signal_of, signal_of_error,
};
use harness_protocol::direct::{DirectOutcome, HARNESS_STOP_PREFIX, banned_claim_word};
use std::path::Path;
use std::time::Duration;

fn reading(free: Option<u64>, used: Option<u64>) -> Reading {
    Reading {
        root_free_mb: free,
        workspace_used_mb: used,
        workspace_partial: false,
        writable: None,
    }
}

fn limits() -> Limits {
    Limits {
        root_low_mb: 512,
        workspace_limit_mb: 960,
    }
}

// ---- the settings ---------------------------------------------------------------------------------------------------------

#[test]
fn the_defaults_are_the_numbers_of_the_measured_session() {
    assert_eq!(DEFAULT_LOW_MB, 512);
    // The mount holds 1 GiB; a margin of 64 MiB is kept below it.
    assert_eq!(DEFAULT_WORKSPACE_LIMIT_MB, 960);
    assert_eq!(Limits::default(), limits());
}

#[test]
fn a_size_setting_is_a_whole_number_of_mib_and_anything_else_is_the_default() {
    assert_eq!(disk::parse_mb(None, 512), 512);
    for bad in ["", "  ", "abc", "-1", "1.5", "1e3", "512MiB", "0x10"] {
        assert_eq!(
            disk::parse_mb(Some(bad), 512),
            512,
            "{bad:?} is the default"
        );
    }
    assert_eq!(disk::parse_mb(Some("700"), 512), 700);
    assert_eq!(disk::parse_mb(Some(" 700 "), 512), 700);
    // 0 is allowed: it switches that check off.
    assert_eq!(disk::parse_mb(Some("0"), 512), 0);
    // Above the sane maximum is the maximum.
    assert_eq!(disk::parse_mb(Some("99999999999999"), 512), disk::MAX_MB);
}

#[test]
fn the_deployments_values_reach_the_direct_config() {
    use agentd::config::DirectConfig;
    use std::collections::BTreeMap;
    let get = |pairs: &[(&str, &str)]| {
        let m: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        DirectConfig::from_vars(&|k| m.get(k).cloned())
    };
    let d = get(&[]);
    assert_eq!(d.disk_low_mb, 512);
    assert_eq!(d.workspace_limit_mb, 960);
    let d = get(&[
        ("AGENTD_DISK_LOW_MB", "1024"),
        ("AGENTD_WORKSPACE_LIMIT_MB", "4000"),
    ]);
    assert_eq!((d.disk_low_mb, d.workspace_limit_mb), (1024, 4000));
    let d = get(&[
        ("AGENTD_DISK_LOW_MB", "lots"),
        ("AGENTD_WORKSPACE_LIMIT_MB", "-5"),
    ]);
    assert_eq!((d.disk_low_mb, d.workspace_limit_mb), (512, 960));
    let d = get(&[
        ("AGENTD_DISK_LOW_MB", "0"),
        ("AGENTD_WORKSPACE_LIMIT_MB", "0"),
    ]);
    assert_eq!((d.disk_low_mb, d.workspace_limit_mb), (0, 0));
}

// ---- what counts as full ---------------------------------------------------------------------------------------------------

#[test]
fn a_root_volume_below_the_low_mark_is_full() {
    let full = limits().full(&reading(Some(300), Some(10))).unwrap();
    assert!(full.root_short && !full.workspace_short);
    assert_eq!(full.root_free_mb, Some(300));
}

#[test]
fn a_workspace_at_the_limit_is_full() {
    let full = limits().full(&reading(Some(4000), Some(960))).unwrap();
    assert!(full.workspace_short && !full.root_short);
    assert_eq!(full.workspace_used_mb, Some(960));
}

#[test]
fn both_short_is_one_cause_with_both_numbers() {
    let full = limits().full(&reading(Some(100), Some(1000))).unwrap();
    assert!(full.root_short && full.workspace_short);
}

#[test]
fn the_edges_are_exact() {
    let l = limits();
    assert!(
        l.full(&reading(Some(512), Some(959))).is_none(),
        "512 free is not below 512"
    );
    assert!(l.full(&reading(Some(511), Some(0))).is_some());
    assert!(l.full(&reading(Some(5000), Some(959))).is_none());
    assert!(l.full(&reading(Some(5000), Some(960))).is_some());
}

#[test]
fn room_on_both_is_not_full() {
    assert!(limits().full(&reading(Some(5700), Some(40))).is_none());
}

#[test]
fn what_was_not_measured_is_never_read_as_full() {
    assert!(limits().full(&reading(None, None)).is_none());
    assert!(limits().full(&reading(None, Some(10))).is_none());
    assert!(limits().full(&reading(Some(5000), None)).is_none());
    // The other number can still say it.
    assert!(limits().full(&reading(None, Some(960))).is_some());
    assert!(limits().full(&reading(Some(1), None)).is_some());
}

#[test]
fn a_limit_of_zero_switches_that_check_off() {
    let off_root = Limits {
        root_low_mb: 0,
        workspace_limit_mb: 960,
    };
    assert!(off_root.full(&reading(Some(0), Some(10))).is_none());
    assert!(off_root.full(&reading(Some(0), Some(960))).is_some());
    let off_ws = Limits {
        root_low_mb: 512,
        workspace_limit_mb: 0,
    };
    assert!(off_ws.full(&reading(Some(5000), Some(99999))).is_none());
    assert!(off_ws.full(&reading(Some(5), Some(99999))).is_some());
}

#[test]
fn a_partial_walk_is_a_lower_bound() {
    // Counted so far and already over the limit: full, and the words say "at least".
    let r = Reading {
        root_free_mb: Some(5000),
        workspace_used_mb: Some(970),
        workspace_partial: true,
        writable: None,
    };
    let full = limits().full(&r).unwrap();
    assert!(
        full.numbers().contains("at least 970"),
        "{}",
        full.numbers()
    );
    // Below the limit with the walk cut short: not full (it is not known).
    let r = Reading {
        workspace_used_mb: Some(900),
        ..r
    };
    assert!(limits().full(&r).is_none());
}

// ---- the words -------------------------------------------------------------------------------------------------------------

fn sample() -> disk::DiskFull {
    limits().full(&reading(Some(2400), Some(975))).unwrap()
}

#[test]
fn the_cause_is_named_with_the_numbers() {
    let full = sample();
    assert_eq!(full.cause(), "the session disk is full");
    assert_eq!(
        full.numbers(),
        "workspace 975 MiB against a stop mark of 960 MiB, root 2400 MiB free"
    );
    assert_eq!(
        full.line(),
        "the session disk is full, workspace 975 MiB against a stop mark of 960 MiB, root 2400 MiB free"
    );
    // What was not measured is said so, not shown as a number.
    let unknown = limits().full(&reading(None, Some(975))).unwrap();
    assert_eq!(
        unknown.numbers(),
        "workspace 975 MiB against a stop mark of 960 MiB, root free space unknown"
    );
    let no_walk = limits().full(&reading(Some(100), None)).unwrap();
    assert_eq!(
        no_walk.numbers(),
        "workspace use unknown, root 100 MiB free"
    );
    // A check that is switched off has no mark to name.
    let off = Limits {
        root_low_mb: 512,
        workspace_limit_mb: 0,
    }
    .full(&reading(Some(100), Some(10)))
    .unwrap();
    assert_eq!(off.numbers(), "workspace 10 MiB, root 100 MiB free");
}

#[test]
fn the_detail_makes_the_label_say_the_harness_ended_the_run_and_why() {
    let detail = sample().detail();
    assert!(detail.starts_with(HARNESS_STOP_PREFIX), "{detail}");
    assert!(
        detail.starts_with(
            "The harness ended the run: the session disk is full, workspace 975 MiB against a stop mark of 960 MiB, root 2400 MiB free."
        ),
        "{detail}"
    );
    assert_eq!(
        DirectOutcome::StoppedOther.label_for(Some(&detail)),
        "Cline stopped: the harness ended the run (the session disk is full, workspace 975 MiB against a stop mark of 960 MiB, root 2400 MiB free)"
    );
    // One line, within the detail limit, and no claim about the work.
    assert!(!detail.contains('\n'));
    assert!(detail.chars().count() <= harness_protocol::direct::MAX_DIRECT_DETAIL_CHARS);
    assert_eq!(banned_claim_word(&detail), None);
}

#[test]
fn the_notice_says_what_the_user_can_do() {
    let n = sample().notice();
    assert!(n.starts_with("The session disk is full"), "{n}");
    assert!(
        n.contains("workspace 975 MiB against a stop mark of 960 MiB"),
        "{n}"
    );
    assert!(n.contains("ham say") && n.contains("ham finish"), "{n}");
    assert_eq!(banned_claim_word(&n), None);
    let q = sample().question();
    assert!(q.contains("ham say") && q.contains("ham finish"), "{q}");
    assert!(
        q.chars().count() <= harness_protocol::direct::MAX_NEEDS_INPUT_QUESTION_CHARS,
        "{q}"
    );
}

// ---- pressure before it is full ---------------------------------------------------------------------------------------------

#[test]
fn pressure_is_below_the_low_mark_or_at_ninety_percent_of_the_limit() {
    let l = limits();
    assert!(l.root_low(&reading(Some(511), None)));
    assert!(!l.root_low(&reading(Some(512), None)));
    assert!(!l.root_low(&reading(None, None)));
    // 90 percent of 960 is 864.
    assert!(l.workspace_high(&reading(None, Some(864))));
    assert!(!l.workspace_high(&reading(None, Some(863))));
    assert!(!l.workspace_high(&reading(None, None)));
    let off = Limits {
        root_low_mb: 0,
        workspace_limit_mb: 0,
    };
    assert!(!off.root_low(&reading(Some(0), None)));
    assert!(!off.workspace_high(&reading(None, Some(99999))));
}

#[test]
fn one_notice_per_crossing_and_none_while_it_stays_crossed() {
    let l = limits();
    let mut w = DiskWatch::default();
    assert!(w.crossings(&l, &reading(Some(2000), Some(100))).is_empty());
    let first = w.crossings(&l, &reading(Some(400), Some(100)));
    assert_eq!(first.len(), 1, "{first:?}");
    assert!(
        first[0].contains("root volume") && first[0].contains("400 MiB free"),
        "{first:?}"
    );
    assert!(
        w.crossings(&l, &reading(Some(380), Some(100))).is_empty(),
        "still crossed"
    );
    // It recovers (something was deleted), and a later crossing is a new one.
    assert!(w.crossings(&l, &reading(Some(2000), Some(100))).is_empty());
    assert_eq!(w.crossings(&l, &reading(Some(300), Some(100))).len(), 1);
}

#[test]
fn the_workspace_and_the_root_volume_each_have_their_own_crossing() {
    let l = limits();
    let mut w = DiskWatch::default();
    let both = w.crossings(&l, &reading(Some(100), Some(900)));
    assert_eq!(both.len(), 2, "{both:?}");
    assert!(
        both.iter().any(|n| n.contains("workspace")
            && n.contains("900 MiB used")
            && n.contains("stop mark of 960 MiB")),
        "{both:?}"
    );
    assert!(w.crossings(&l, &reading(Some(100), Some(900))).is_empty());
    // Only the workspace recovers.
    assert!(w.crossings(&l, &reading(Some(100), Some(10))).is_empty());
    let again = w.crossings(&l, &reading(Some(100), Some(950)));
    assert_eq!(again.len(), 1);
    assert!(again[0].contains("workspace"));
}

#[test]
fn a_reading_that_is_unknown_changes_nothing() {
    let l = limits();
    let mut w = DiskWatch::default();
    assert_eq!(w.crossings(&l, &reading(Some(100), None)).len(), 1);
    // An unreadable probe is not a recovery: the next real reading does not announce the same crossing again.
    assert!(w.crossings(&l, &reading(None, None)).is_empty());
    assert!(w.crossings(&l, &reading(Some(100), None)).is_empty());
}

// ---- the real measurement ----------------------------------------------------------------------------------------------------

fn write_mib(path: &Path, mib: usize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, vec![7u8; mib << 20]).unwrap();
}

fn host(root: &Path, workspace: &Path) -> HostDisk {
    HostDisk::new(root, workspace)
}

#[tokio::test]
async fn the_workspace_is_the_size_of_what_is_in_it_and_the_root_volume_reports_its_free_space() {
    let root = common::scratch("disk-root");
    let ws = common::scratch("disk-ws");
    write_mib(&ws.join("a.bin"), 1);
    write_mib(&ws.join("sub/deeper/b.bin"), 2);
    let r = host(&root, &ws).measure().await;
    assert_eq!(r.workspace_used_mb, Some(3), "{r:?}");
    assert!(!r.workspace_partial);
    assert!(r.root_free_mb.is_some_and(|mb| mb > 0), "{r:?}");
}

#[tokio::test]
async fn a_hard_link_is_counted_once() {
    let root = common::scratch("disk-hl-root");
    let ws = common::scratch("disk-hl-ws");
    write_mib(&ws.join("big.bin"), 2);
    std::fs::hard_link(ws.join("big.bin"), ws.join("link1.bin")).unwrap();
    std::fs::hard_link(ws.join("big.bin"), ws.join("link2.bin")).unwrap();
    let r = host(&root, &ws).measure().await;
    assert_eq!(r.workspace_used_mb, Some(2), "{r:?}");
}

#[tokio::test]
async fn a_symlink_is_not_followed() {
    let root = common::scratch("disk-sl-root");
    let ws = common::scratch("disk-sl-ws");
    let outside = common::scratch("disk-sl-outside");
    write_mib(&outside.join("huge.bin"), 5);
    std::os::unix::fs::symlink(&outside, ws.join("to-outside")).unwrap();
    std::os::unix::fs::symlink(outside.join("huge.bin"), ws.join("to-file")).unwrap();
    let r = host(&root, &ws).measure().await;
    assert_eq!(r.workspace_used_mb, Some(0), "{r:?}");
}

#[tokio::test]
async fn a_walk_over_its_budget_is_a_lower_bound_and_says_so() {
    let root = common::scratch("disk-b-root");
    let ws = common::scratch("disk-b-ws");
    for i in 0..60 {
        std::fs::write(ws.join(format!("f{i}")), b"x").unwrap();
    }
    let probe = HostDisk::with_budget(
        &root,
        &ws,
        WalkBudget {
            entries: 10,
            time: Duration::from_secs(30),
        },
    );
    let r = probe.measure().await;
    assert!(r.workspace_partial, "{r:?}");
    assert!(r.workspace_used_mb.is_some());
}

#[tokio::test]
async fn a_folder_that_is_missing_is_not_measured() {
    let root = common::scratch("disk-m-root");
    let gone = common::scratch("disk-m-gone").join("not-there");
    let r = host(&root, &gone).measure().await;
    assert_eq!(r.workspace_used_mb, None, "{r:?}");
    let r = host(&gone, &gone).measure().await;
    assert_eq!(r.root_free_mb, None, "{r:?}");
}

// ---- what the engine's own error text says ----------------------------------------------------------
//
// The managed session storage turns read-only once it is full, so the engine's -32603 answers carried "Read-only file system (os error 30)"
// and not only "No space left on device". The text names the disk by itself; the measurement stays the second signal.

#[test]
fn the_texts_of_a_full_disk_are_read_in_any_case() {
    for text in [
        "No space left on device (os error 28)",
        "write failed: no space left on device",
        "ENOSPC: no space left on device, write",
        "failed to write file: os error 28",
        "Error: ENOSPC",
        "OS ERROR 28",
    ] {
        assert_eq!(signal_of(text), Some(Signal::NoSpace), "{text:?}");
    }
}

#[test]
fn the_texts_of_a_read_only_disk_are_read_in_any_case() {
    for text in [
        "Read-only file system (os error 30)",
        "read-only file system",
        "EROFS: read-only file system, open '/mnt/workspace/x'",
        "failed to create directory: os error 30",
        "Error: EROFS",
        "Os Error 30",
    ] {
        assert_eq!(signal_of(text), Some(Signal::ReadOnly), "{text:?}");
    }
}

#[test]
fn a_full_and_read_only_disk_is_called_full() {
    // The mount is read-only because it is full: the cause that is true is the first.
    assert_eq!(
        signal_of("No space left on device (os error 28); Read-only file system (os error 30)"),
        Some(Signal::NoSpace)
    );
}

#[test]
fn other_texts_are_not_the_disk() {
    for text in [
        "",
        "Internal error",
        "No such file or directory (os error 2)",
        "Permission denied (os error 13)",
        "os error 280",
        "os error 3",
        "os error 300",
        "spaceship",
        "erofs_utils",
        "The provider returned a 503",
    ] {
        assert_eq!(signal_of(text), None, "{text:?}");
    }
}

#[test]
fn the_text_is_read_from_the_message_and_the_data_of_a_32603_and_from_no_other_error() {
    let rpc = |code: i64, message: &str, data: Option<serde_json::Value>| AcpError::Rpc {
        code,
        message: message.into(),
        data,
    };
    assert_eq!(
        signal_of_error(&rpc(
            -32603,
            "Internal error: Read-only file system (os error 30)",
            None
        )),
        Some(Signal::ReadOnly)
    );
    // The engine sends the cause in `data` and a plain message.
    assert_eq!(
        signal_of_error(&rpc(
            -32603,
            "Internal error",
            Some(serde_json::json!({"detail": "No space left on device (os error 28)"}))
        )),
        Some(Signal::NoSpace)
    );
    assert_eq!(
        signal_of_error(&rpc(
            -32603,
            "Internal error",
            Some(serde_json::json!({"errorName": "EROFS"}))
        )),
        Some(Signal::ReadOnly)
    );
    // Another code, another kind of failure, no data: no signal.
    assert_eq!(
        signal_of_error(&rpc(-32000, "Read-only file system (os error 30)", None)),
        None
    );
    assert_eq!(signal_of_error(&rpc(-32603, "Internal error", None)), None);
    assert_eq!(
        signal_of_error(&AcpError::Io("Read-only file system (os error 30)".into())),
        None
    );
    assert_eq!(signal_of_error(&AcpError::Closed), None);
}

#[test]
fn a_text_alone_is_the_disk_even_when_both_numbers_show_room() {
    for signal in [Signal::ReadOnly, Signal::NoSpace] {
        let full = limits()
            .full_with(&reading(Some(5700), Some(40)), Some(signal))
            .unwrap();
        assert!(!full.root_short && !full.workspace_short);
        assert_eq!(full.signal, Some(signal));
        assert_eq!(
            full.numbers(),
            "workspace 40 MiB against a stop mark of 960 MiB, root 5700 MiB free"
        );
    }
    // Without a text, room is room.
    assert!(
        limits()
            .full_with(&reading(Some(5700), Some(40)), None)
            .is_none()
    );
    // The measurement still says what it says, with or without the text.
    let both = limits()
        .full_with(&reading(Some(100), Some(975)), Some(Signal::NoSpace))
        .unwrap();
    assert!(both.root_short && both.workspace_short);
    let measured = limits()
        .full_with(&reading(Some(100), Some(975)), None)
        .unwrap();
    assert_eq!(measured.signal, None);
}

#[test]
fn a_read_only_disk_is_named_read_only_or_full_and_a_full_disk_is_named_full() {
    let ro = limits()
        .full_with(&reading(Some(5700), Some(40)), Some(Signal::ReadOnly))
        .unwrap();
    assert_eq!(ro.cause(), "the session disk is read-only or full");
    assert_eq!(ro.title(), "The session disk is read-only or full");
    assert_eq!(
        ro.line(),
        "the session disk is read-only or full, workspace 40 MiB against a stop mark of 960 MiB, root 5700 MiB free"
    );
    let detail = ro.detail();
    assert!(
        detail.starts_with(
            "The harness ended the run: the session disk is read-only or full, workspace 40 MiB against a stop mark of 960 MiB, root 5700 MiB free."
        ),
        "{detail}"
    );
    assert!(
        detail.contains("The engine's error says: read-only file system."),
        "{detail}"
    );
    assert!(
        detail.chars().count() <= harness_protocol::direct::MAX_DIRECT_DETAIL_CHARS,
        "{detail}"
    );
    assert_eq!(banned_claim_word(&detail), None);
    assert_eq!(
        DirectOutcome::StoppedOther.label_for(Some(&detail)),
        "Cline stopped: the harness ended the run (the session disk is read-only or full, workspace 40 MiB against a stop mark of 960 MiB, root 5700 MiB free)"
    );
    let notice = ro.notice();
    assert!(
        notice.starts_with("The session disk is read-only or full"),
        "{notice}"
    );
    assert!(notice.contains("read-only file system"), "{notice}");
    assert!(
        notice.contains("ham say") && notice.contains("ham finish"),
        "{notice}"
    );

    // A text that says no space while a number is short: the disk is full.
    let nospace = limits()
        .full_with(&reading(Some(100), Some(40)), Some(Signal::NoSpace))
        .unwrap();
    assert_eq!(nospace.cause(), "the session disk is full");
    assert_eq!(nospace.title(), "The session disk is full");
    assert!(
        nospace
            .detail()
            .contains("The engine's error says: no space left on device.")
    );
    // A cause found by the measurement alone names no text.
    assert!(!sample().detail().contains("The engine's error says"));
    assert_eq!(sample().title(), "The session disk is full");
}

// ---- the test write ------------------------------------------------------------------------------------------------

fn with_writable(writable: Option<bool>) -> Reading {
    Reading {
        writable,
        ..reading(Some(5700), Some(40))
    }
}

#[test]
fn a_refused_test_write_is_the_cause_by_itself_whatever_the_numbers_show() {
    let full = limits().full(&with_writable(Some(false))).unwrap();
    assert!(full.unwritable);
    assert!(!full.root_short && !full.workspace_short && full.signal.is_none());
    assert_eq!(full.cause(), "the session disk is read-only or full");
    assert_eq!(full.title(), disk::TITLE_READ_ONLY);
    assert_eq!(full.reason_word(), "disk_full");
    // The numbers stay in the words, and the words say that a write was refused.
    let line = full.line();
    assert!(line.contains("root 5700 MiB free"), "{line}");
    assert!(
        full.detail()
            .contains("A test write to a folder of the run failed."),
        "{}",
        full.detail()
    );
    assert!(full.detail().starts_with(HARNESS_STOP_PREFIX));
}

#[test]
fn a_test_write_that_passed_or_was_not_tried_is_never_the_cause() {
    assert!(limits().full(&with_writable(Some(true))).is_none());
    assert!(limits().full(&with_writable(None)).is_none());
    // The engine's text and the numbers decide as before.
    assert!(limits().full(&reading(Some(100), Some(10))).is_some());
}

#[test]
fn the_engine_failing_on_every_route_is_named_as_that_when_nothing_names_the_disk() {
    let fault = limits().engine_fault(&reading(Some(5700), Some(40)));
    assert!(fault.engine_fault);
    assert_eq!(
        fault.cause(),
        "the engine failed on every route while every route answered a test request"
    );
    assert_eq!(fault.title(), disk::TITLE_ENGINE_FAULT);
    assert_eq!(fault.reason_word(), "engine_fault");
    assert!(
        fault.question().contains("ham finish") && !fault.question().contains("build output"),
        "{}",
        fault.question()
    );
    let detail = fault.detail();
    assert!(detail.starts_with(HARNESS_STOP_PREFIX), "{detail}");
    assert!(
        detail.contains("Another try on any route would fail the same way."),
        "{detail}"
    );
    assert!(!detail.contains("space is freed"), "{detail}");
    // The label reads what was seen and names no provider.
    let label = DirectOutcome::StoppedOther.label_for(Some(&detail));
    assert!(!label.contains("provider"), "{label}");
    assert!(banned_claim_word(&detail).is_none(), "{detail}");
}

#[test]
fn the_engine_failing_on_every_route_says_the_disk_when_the_numbers_or_a_write_do() {
    let short = limits().engine_fault(&reading(Some(100), Some(40)));
    assert!(short.engine_fault && short.root_short);
    assert_eq!(short.cause(), "the session disk is full");
    assert_eq!(short.reason_word(), "disk_full");
    let refused = limits().engine_fault(&with_writable(Some(false)));
    assert_eq!(refused.cause(), "the session disk is read-only or full");
    assert_eq!(refused.reason_word(), "disk_full");
}

#[tokio::test]
async fn the_host_probe_writes_a_file_in_each_folder_and_leaves_none_behind() {
    let root = common::scratch("wp-root");
    let ws = common::scratch("wp-ws");
    let run = common::scratch("wp-run");
    let host = HostDisk::new(&root, &ws).with_write_dirs([run.clone(), run.clone()]);
    let r = host.measure().await;
    assert_eq!(r.writable, Some(true));
    for dir in [&root, &ws, &run] {
        assert!(
            std::fs::read_dir(dir).unwrap().next().is_none(),
            "{} still holds a file",
            dir.display()
        );
    }
}

#[tokio::test]
async fn a_folder_that_does_not_exist_is_not_tried() {
    let root = common::scratch("wp-none").join("gone");
    let ws = common::scratch("wp-none2").join("gone");
    let host = HostDisk::new(&root, &ws);
    assert_eq!(host.measure().await.writable, None);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_folder_that_refuses_the_write_makes_the_reading_unwritable() {
    // `/proc/self` is a directory that takes no new file, for any user: the same refusal a read-only file system gives.
    let ok = common::scratch("wp-ok");
    let host = HostDisk::new(&ok, &ok).with_write_dirs([Path::new("/proc/self").to_path_buf()]);
    let r = host.measure().await;
    assert_eq!(r.writable, Some(false));
    assert!(disk::test_write(Path::new("/proc/self")).is_err());
    assert!(disk::test_write(&ok).is_ok());
}

#[tokio::test]
async fn the_machines_report_names_each_folder_and_stays_bounded() {
    let root = common::scratch("wp-diag");
    let ws = common::scratch("wp-diag-ws");
    let host = HostDisk::new(&root, &ws);
    let text = host.diagnose().await;
    assert!(
        text.contains(&format!("write test {}: ok", root.display())),
        "{text}"
    );
    assert!(
        text.contains(&format!("write test {}: ok", ws.display())),
        "{text}"
    );
    assert!(text.contains("df "), "{text}");
    assert!(text.contains("inodes"), "{text}");
    assert!(text.contains("dmesg"), "{text}");
    assert!(
        text.chars().count() <= 4096,
        "{} characters",
        text.chars().count()
    );
}

// ----: a stop with room names the storage's refusal, not a full disk ------------------------------------------------------

/// The numbers of the two measured refusals: files of 158 MiB on a storage of 1024 MiB, 5343 MiB free on the root volume.
fn room() -> Reading {
    reading(Some(5343), Some(158))
}

/// The closing of a stop with room.
const B881_CLOSING: &str = "More prompts can fail the same way while the storage refuses writes. The refusal can end by itself.";

#[test]
fn b881_the_words_of_a_stop_with_room_do_not_call_the_disk_full() {
    for own_write_refused in [false, true] {
        let mut full = limits().full_with(&room(), Some(Signal::NoSpace)).unwrap();
        full.storage_refused = own_write_refused;
        full.engine = "cline".to_string();
        let line = "the engine reported no space left on device, but the files add up to 158 MiB of 1024 MiB, root 5343 MiB free";
        assert_eq!(full.line(), line);
        assert_eq!(full.title(), "The engine reported no space left on device");
        assert_eq!(
            full.detail(),
            format!("The harness ended the run: {line}. {B881_CLOSING}")
        );
        assert_eq!(
            DirectOutcome::StoppedOther.label_for(Some(&full.detail())),
            format!("Cline stopped: the harness ended the run ({line})")
        );
        let own = if own_write_refused {
            "ham's own write to the session's storage was refused too. "
        } else {
            ""
        };
        assert_eq!(
            full.notice(),
            format!(
                "The engine reported no space left on device, but the files add up to 158 MiB of 1024 MiB, root 5343 MiB free. Prompts can fail while the storage refuses writes. {own}Send a message with `ham say` to go on once the storage takes writes again (Cline may not be able to answer while it refuses them), or end the run with `ham finish` and keep what exists."
            )
        );
        for text in [
            full.line(),
            full.detail(),
            full.notice(),
            full.title().to_string(),
        ] {
            for wrong in [
                "disk is full",
                "read-only or full",
                "until space is freed",
                "The engine's error says",
                "A test write to a folder of the run failed",
                "stop mark",
            ] {
                assert!(!text.contains(wrong), "{wrong:?} in {text:?}");
            }
        }
        // The wire word of the notice and of the question's reason stays.
        assert_eq!(full.reason_word(), "disk_full");
        assert_eq!(banned_claim_word(&full.detail()), None);
        assert!(full.detail().chars().count() <= harness_protocol::direct::MAX_DIRECT_DETAIL_CHARS);
    }
    // The label holds the whole reason with the longest numbers too: a walk cut short and a root number of six digits.
    let mut long = limits()
        .full_with(
            &Reading {
                workspace_partial: true,
                ..reading(Some(123_456), Some(958))
            },
            Some(Signal::NoSpace),
        )
        .unwrap();
    long.engine = "cline".to_string();
    let line = "the engine reported no space left on device, but the files add up to at least 958 MiB of 1024 MiB, root 123456 MiB free";
    assert_eq!(long.line(), line);
    assert!(line.chars().count() <= 120, "{}", line.chars().count());
    assert_eq!(
        DirectOutcome::StoppedOther.label_for(Some(&long.detail())),
        format!("Cline stopped: the harness ended the run ({line})")
    );
}

#[test]
fn b881_a_bare_error_with_a_refused_write_and_room_names_the_refusal() {
    // No text of the engine; the disk rule's own test write failed as it does today, and ham's private write got errno 28.
    for disk_rule_write in [Some(false), Some(true), None] {
        let reading = Reading {
            writable: disk_rule_write,
            ..room()
        };
        // The stop decision is the one of today: without a text it needs the disk rule's failed write.
        let Some(mut full) = limits().full_with(&reading, None) else {
            assert_ne!(disk_rule_write, Some(false));
            continue;
        };
        full.storage_refused = true;
        full.engine = "opencode".to_string();
        let line = "the session's storage refused a write while its files add up to 158 MiB of 1024 MiB, root 5343 MiB free";
        assert_eq!(full.line(), line);
        assert_eq!(full.title(), "The session's storage refused a write");
        assert_eq!(
            full.detail(),
            format!("The harness ended the run: {line}. {B881_CLOSING}")
        );
        assert_eq!(
            DirectOutcome::StoppedOther.label_for(Some(&full.detail())),
            format!("Cline stopped: the harness ended the run ({line})")
        );
        assert_eq!(
            full.notice(),
            "The session's storage refused a write while its files add up to 158 MiB of 1024 MiB, root 5343 MiB free. Prompts can fail while the storage refuses writes. Send a message with `ham say` to go on once the storage takes writes again (OpenCode may not be able to answer while it refuses them), or end the run with `ham finish` and keep what exists."
        );
        for text in [
            full.line(),
            full.detail(),
            full.notice(),
            full.title().to_string(),
        ] {
            for wrong in ["read-only or full", "until space is freed", "disk is full"] {
                assert!(!text.contains(wrong), "{wrong:?} in {text:?}");
            }
        }
        assert_eq!(full.reason_word(), "disk_full");
    }
    // The longest numbers fit the label.
    let mut long = limits()
        .full_with(
            &Reading {
                workspace_partial: true,
                writable: Some(false),
                ..reading(Some(123_456), Some(958))
            },
            None,
        )
        .unwrap();
    long.storage_refused = true;
    let line = "the session's storage refused a write while its files add up to at least 958 MiB of 1024 MiB, root 123456 MiB free";
    assert_eq!(long.line(), line);
    assert!(line.chars().count() <= 120);
    // A refused write of ham's own changes no word while a number is short or was not measured: the disk rule's words cover those.
    let mut short = limits().full(&reading(Some(2400), Some(975))).unwrap();
    short.storage_refused = true;
    assert_eq!(
        short.line(),
        "the session disk is full, workspace 975 MiB against a stop mark of 960 MiB, root 2400 MiB free"
    );
    let mut unknown = limits()
        .full_with(&reading(None, Some(40)), Some(Signal::NoSpace))
        .unwrap();
    unknown.storage_refused = true;
    assert_eq!(unknown.cause(), "the session disk is full");
}

#[test]
fn b881_every_other_stop_keeps_its_words() {
    // A disk the measurement calls full.
    let full = sample();
    assert_eq!(
        full.line(),
        "the session disk is full, workspace 975 MiB against a stop mark of 960 MiB, root 2400 MiB free"
    );
    assert_eq!(full.title(), "The session disk is full");
    assert_eq!(
        full.detail(),
        "The harness ended the run: the session disk is full, workspace 975 MiB against a stop mark of 960 MiB, root 2400 MiB free. More prompts would fail the same way until space is freed."
    );
    assert!(full.notice().starts_with("The session disk is full, workspace 975 MiB against a stop mark of 960 MiB, root 2400 MiB free. Prompts fail until space is freed. Send a message with `ham say` that asks Cline to delete build output"));
    // A text that says no space while a number is short: the disk is full.
    let both = limits()
        .full_with(&reading(Some(100), Some(975)), Some(Signal::NoSpace))
        .unwrap();
    assert_eq!(both.cause(), "the session disk is full");
    assert!(
        both.detail()
            .contains("The engine's error says: no space left on device.")
    );
    // A text that says read-only, with room.
    let ro = limits()
        .full_with(&reading(Some(5700), Some(40)), Some(Signal::ReadOnly))
        .unwrap();
    assert_eq!(
        ro.line(),
        "the session disk is read-only or full, workspace 40 MiB against a stop mark of 960 MiB, root 5700 MiB free"
    );
    assert_eq!(ro.title(), "The session disk is read-only or full");
    assert!(ro.detail().ends_with("The engine's error says: read-only file system. More prompts would fail the same way until space is freed."));
    // A failed test write of the disk rule with no text and no refused write of ham's own.
    let unwritable = limits().full(&with_writable(Some(false))).unwrap();
    assert_eq!(unwritable.cause(), "the session disk is read-only or full");
    assert!(
        unwritable
            .detail()
            .contains("A test write to a folder of the run failed.")
    );
    // The engine failing on every route.
    let fault = limits().engine_fault(&reading(Some(5700), Some(40)));
    assert_eq!(
        fault.line(),
        "the engine failed on every route while every route answered a test request, workspace 40 MiB used, root 5700 MiB free"
    );
    assert_eq!(fault.reason_word(), "engine_fault");
    assert!(
        fault
            .detail()
            .ends_with("Another try on any route would fail the same way.")
    );
}

#[test]
fn b881_the_question_names_the_engine_that_ran() {
    for (engine, name) in [("cline", "Cline"), ("opencode", "OpenCode")] {
        let mut full = limits().full_with(&room(), Some(Signal::NoSpace)).unwrap();
        full.engine = engine.to_string();
        let q = full.question();
        assert_eq!(
            q,
            format!(
                "Send a message with `ham say` to go on once the storage takes writes again ({name} may not be able to answer while it refuses them), or end the run with `ham finish` and keep what exists."
            )
        );
        assert!(q.chars().count() <= harness_protocol::direct::MAX_NEEDS_INPUT_QUESTION_CHARS);
        assert_eq!(
            agentd::engine::direct_words::storage_refused_question(engine),
            q
        );
    }
    // A cause that was given no engine name says "The engine".
    let bare = limits().full_with(&room(), Some(Signal::NoSpace)).unwrap();
    assert!(
        bare.question()
            .contains("(The engine may not be able to answer"),
        "{}",
        bare.question()
    );
}

#[test]
fn b881_a_result_is_read_for_no_space_as_an_error_text_is() {
    // `names_no_space` reads a tool result in one pass; it answers what `signal_of` answers for every text.
    let texts = [
        "No space left on device",
        "NO SPACE LEFT ON DEVICE",
        "cp: error writing 'x': No space left on device\n",
        "no space left on devices",
        "no  space left on device",
        "ENOSPC",
        "Error: ENOSPC: no space left, write",
        "enospc_handler",
        "xENOSPC",
        "an ENOSPC.",
        "os error 28",
        "(os error 28)",
        "os error 280",
        "macos error 28",
        "Os Error 28",
        "os error 2",
        "Read-only file system (os error 30)",
        "EROFS",
        "no space",
        "",
        "n",
        "o",
        "e",
        "\u{e9}nospc",
        "\u{e9}ENOSPC\u{e9}",
        "ENOSPC\u{4e2d}",
        "\u{4e2d} ENOSPC \u{4e2d}",
        "os error 28\u{e9}",
        "both: Read-only file system and No space left on device",
    ];
    for text in texts {
        assert_eq!(
            disk::names_no_space(text),
            signal_of(text) == Some(Signal::NoSpace),
            "{text:?}"
        );
    }
    assert!(disk::names_no_space(
        "write failed: No space left on device (os error 28)"
    ));
    assert!(!disk::names_no_space(
        "touch: Read-only file system (os error 30)"
    ));
}

// ----: the note about a large workspace folder --------------------------------------------------------------------------

const B889_N1_920: &str = "The workspace folder holds 920 MiB. In measurements this storage at times refused writes from 300 MiB up and none up to 200 MiB. The run goes on.";
const B889_N2: &str = "A new run with --compute instances, where the stage has such machines, keeps the workspace folder on the machine's own disk. That machine runs no containers.";

fn b889_partial(used: u64) -> Reading {
    Reading {
        workspace_partial: true,
        ..reading(Some(5000), Some(used))
    }
}

/// T1: the note comes with the first reading of a session and never later.
#[test]
fn b889_the_note_comes_with_the_first_reading_of_a_session_and_only_then() {
    let l = limits();
    // A first reading of 100 brings no row, and a later 201 or 950 brings none either.
    let mut small = disk::FolderNote::default();
    assert_eq!(
        small.first_reading(&l, &reading(Some(5000), Some(100))),
        Some(Vec::new())
    );
    assert_eq!(
        small.first_reading(&l, &reading(Some(5000), Some(201))),
        None
    );
    assert_eq!(
        small.first_reading(&l, &reading(Some(5000), Some(950))),
        None
    );
    // 950 still brings the nearly-full notice, as before.
    let mut watch = DiskWatch::default();
    assert_eq!(
        watch.crossings(&l, &reading(Some(5000), Some(950))).len(),
        1
    );
    // A first reading of 201 brings both rows; a second boundary of that session brings none.
    let mut large = disk::FolderNote::default();
    assert_eq!(
        large.first_reading(&l, &reading(Some(5000), Some(201))),
        Some(vec![
            "The workspace folder holds 201 MiB. In measurements this storage at times refused writes from 300 MiB up and none up to 200 MiB. The run goes on.".to_string(),
            B889_N2.to_string(),
        ])
    );
    assert_eq!(
        large.first_reading(&l, &reading(Some(5000), Some(201))),
        None
    );
    assert_eq!(
        large.first_reading(&l, &reading(Some(5000), Some(920))),
        None
    );
}

/// T2: the mark is "more than 200 MiB", in whole MiB rounded down.
#[test]
fn b889_the_mark_is_above_200_mib_and_a_sum_of_bytes_is_whole_mib_rounded_down() {
    assert_eq!(disk::NOTE_ABOVE_MB, 200);
    assert_eq!(disk::REFUSALS_FROM_MB, 300);
    let l = limits();
    assert_eq!(
        disk::FolderNote::default().first_reading(&l, &reading(Some(5000), Some(200))),
        Some(Vec::new()),
        "200 MiB is the size the measurements saw no refusal at"
    );
    let at_201 = disk::FolderNote::default()
        .first_reading(&l, &reading(Some(5000), Some(201)))
        .unwrap();
    assert_eq!(at_201.len(), 2, "{at_201:?}");
    // The reading is a sum of allocated bytes shifted to whole MiB: the last byte below 201 MiB still reads 200.
    assert_eq!(disk::whole_mib(210_763_775), 200);
    assert_eq!(disk::whole_mib(210_763_776), 201);
    assert_eq!(disk::whole_mib(0), 0);
}

/// T3: a walk cut short is a lower bound and says so; a first reading without a number brings nothing, also later.
#[test]
fn b889_a_walk_cut_short_reads_at_least_and_an_unmeasured_first_reading_brings_nothing_later() {
    let l = limits();
    let rows = disk::FolderNote::default()
        .first_reading(&l, &b889_partial(350))
        .unwrap();
    assert_eq!(
        rows.first().map(String::as_str),
        Some(
            "The workspace folder holds at least 350 MiB. In measurements this storage at times refused writes from 300 MiB up and none up to 200 MiB. The run goes on."
        )
    );
    assert_eq!(rows.get(1).map(String::as_str), Some(B889_N2));
    // A lower bound at or below the mark brings nothing: the folder may be larger, and the worker does not guess.
    assert_eq!(
        disk::FolderNote::default().first_reading(&l, &b889_partial(200)),
        Some(Vec::new())
    );
    assert_eq!(
        disk::FolderNote::default().first_reading(&l, &b889_partial(150)),
        Some(Vec::new())
    );
    // The first reading measured no workspace number: nothing now, and nothing at the next reading either.
    let mut unread = disk::FolderNote::default();
    assert_eq!(
        unread.first_reading(&l, &reading(Some(5000), None)),
        Some(Vec::new())
    );
    assert_eq!(
        unread.first_reading(&l, &reading(Some(5000), Some(920))),
        None
    );
}

/// T4: the note follows the workspace check; a deployment (and the Instances runtime) that sets its limit to 0 has no note.
#[test]
fn b889_with_the_workspace_check_off_there_is_no_note() {
    let off = Limits {
        root_low_mb: 512,
        workspace_limit_mb: 0,
    };
    assert_eq!(
        disk::FolderNote::default().first_reading(&off, &reading(Some(5000), Some(920))),
        Some(Vec::new())
    );
    // The root volume's check alone being off changes nothing for the note.
    let root_off = Limits {
        root_low_mb: 0,
        workspace_limit_mb: 960,
    };
    assert_eq!(
        disk::FolderNote::default()
            .first_reading(&root_off, &reading(None, Some(920)))
            .map(|rows| rows.len()),
        Some(2)
    );
}

/// T5: a notice holds 160 characters in the client; each row fits for every size the storage can hold, and the second holds no number.
#[test]
fn b889_each_row_fits_a_notice_and_the_second_row_holds_no_digit() {
    for used in 201..=1024u64 {
        for at_least in [false, true] {
            let row = disk::note_size_row(used, at_least);
            assert!(
                row.chars().count() <= 160,
                "{} characters: {row}",
                row.chars().count()
            );
            assert_eq!(row.contains("at least"), at_least, "{row}");
        }
    }
    assert_eq!(disk::note_size_row(920, false), B889_N1_920);
    assert_eq!(disk::note_size_row(920, false).chars().count(), 145);
    assert_eq!(disk::note_size_row(1024, true).chars().count(), 155);
    assert_eq!(disk::NOTE_COMPUTE_ROW, B889_N2);
    assert_eq!(disk::NOTE_COMPUTE_ROW.chars().count(), 157);
    assert!(
        !disk::NOTE_COMPUTE_ROW.chars().any(|c| c.is_ascii_digit()),
        "the second row names no count and no size"
    );
    assert_eq!(disk::NOTE_REASON, "disk_note");
    // The rows name the two constants, so a changed constant changes the row with it.
    let row = disk::note_size_row(250, false);
    assert!(
        row.contains(&format!("from {} MiB up", disk::REFUSALS_FROM_MB)),
        "{row}"
    );
    assert!(
        row.contains(&format!("none up to {} MiB", disk::NOTE_ABOVE_MB)),
        "{row}"
    );
}

/// T6 (the words): at 920 MiB the nearly-full sentence and the root volume's sentence are byte for byte what they were, and the
/// note's rows come beside them. The order on the wire is pinned by the driver test in `direct_disk_stop.rs`.
#[test]
fn b889_the_older_sentences_are_byte_for_byte_what_they_were() {
    let l = limits();
    let mut watch = DiskWatch::default();
    assert_eq!(
        watch.crossings(&l, &reading(Some(400), Some(920))),
        vec![
            "The session's root volume is running low: 400 MiB free, below the 512 MiB mark. Nothing is deleted.".to_string(),
            "The workspace folder is nearly full: 920 MiB used against a stop mark of 960 MiB, over 90 percent of it. Nothing is deleted.".to_string(),
        ]
    );
    assert_eq!(
        disk::FolderNote::default().first_reading(&l, &reading(Some(400), Some(920))),
        Some(vec![B889_N1_920.to_string(), B889_N2.to_string()])
    );
    // The session's first reading in the log: the folder's size, and a folder that was not read says so.
    assert_eq!(
        disk::reading_words(&reading(Some(5700), Some(920))),
        "workspace 920 MiB used"
    );
    assert_eq!(
        disk::reading_words(&b889_partial(350)),
        "workspace at least 350 MiB used"
    );
    assert_eq!(
        disk::reading_words(&reading(None, None)),
        "workspace not measured"
    );
}
