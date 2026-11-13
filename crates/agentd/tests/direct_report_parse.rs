//! Module `report`: the counts a runner prints, read from its own output, and the bounded tail of that output.
//! Every sample is the plain output of a runner; nothing here starts a process.
#![allow(clippy::unwrap_used)]

use agentd::direct::report::parse::{OutputScan, count_in};
use harness_protocol::direct::TestCounts;

fn counts(p: u32, f: u32, s: u32) -> TestCounts {
    TestCounts {
        passed: p,
        failed: f,
        skipped: s,
    }
}

fn found(command: &str, output: &str) -> Option<(&'static str, TestCounts)> {
    count_in(command, output)
}

// ---- pytest ----------------------------------------------------------------------------------------------------------------

#[test]
fn pytest_reads_the_quiet_summary_line() {
    let out = "..........  [100%]\n12 passed in 0.31s\n";
    assert_eq!(
        found("python3 -m pytest -q", out),
        Some(("pytest", counts(12, 0, 0)))
    );
}

#[test]
fn pytest_reads_the_banner_summary_with_failures_and_skips() {
    let out =
        "collected 21 items\n\n=========== 18 passed, 2 failed, 1 skipped in 0.40s ===========\n";
    assert_eq!(found("pytest", out), Some(("pytest", counts(18, 2, 1))));
}

#[test]
fn pytest_counts_errors_as_failed_and_xfail_as_skipped_and_xpass_as_passed() {
    assert_eq!(
        found("pytest", "1 failed, 2 passed, 1 warning in 0.2s\n"),
        Some(("pytest", counts(2, 1, 0)))
    );
    assert_eq!(
        found("pytest", "3 passed, 1 error in 0.2s\n"),
        Some(("pytest", counts(3, 1, 0)))
    );
    assert_eq!(
        found("pytest", "2 passed, 1 xfailed, 1 xpassed in 0.1s\n"),
        Some(("pytest", counts(3, 0, 1)))
    );
}

#[test]
fn pytest_reads_no_tests_ran_as_zero() {
    assert_eq!(
        found("pytest", "no tests ran in 0.01s\n"),
        Some(("pytest", counts(0, 0, 0)))
    );
}

#[test]
fn pytest_uses_the_last_summary_and_ignores_lines_that_only_look_like_one() {
    let out = "3 passed in 0.1s\ncollected 12 items\ntests/test_x.py::test_a PASSED\n12 passed in 0.9s\nthe run took 4 days\n";
    assert_eq!(found("pytest", out), Some(("pytest", counts(12, 0, 0))));
}

#[test]
fn colour_codes_in_the_summary_are_removed_before_reading() {
    let out = "\u{1b}[32m12 passed\u{1b}[0m\u{1b}[32m in 0.31s\u{1b}[0m\n";
    assert_eq!(found("pytest", out), Some(("pytest", counts(12, 0, 0))));
}

// ---- unittest --------------------------------------------------------------------------------------------------------------

#[test]
fn unittest_reads_ran_n_tests_and_the_result_line() {
    assert_eq!(
        found(
            "python3 -m unittest discover",
            "..\n----\nRan 5 tests in 0.002s\n\nOK\n"
        ),
        Some(("unittest", counts(5, 0, 0)))
    );
    assert_eq!(
        found("python3 -m unittest", "Ran 1 test in 0.000s\n\nOK\n"),
        Some(("unittest", counts(1, 0, 0)))
    );
    assert_eq!(
        found(
            "python3 -m unittest",
            "Ran 5 tests in 0.002s\n\nOK (skipped=2)\n"
        ),
        Some(("unittest", counts(3, 0, 2)))
    );
    assert_eq!(
        found(
            "python3 -m unittest",
            "Ran 6 tests in 0.1s\n\nFAILED (failures=1, errors=2)\n"
        ),
        Some(("unittest", counts(3, 3, 0)))
    );
}

// ---- jest, vitest, node ----------------------------------------------------------------------------------------------------

#[test]
fn jest_reads_the_tests_line_and_not_the_suites_line() {
    let out = "Test Suites: 1 failed, 2 passed, 3 total\nTests:       1 failed, 4 passed, 5 total\nSnapshots:   0 total\n";
    assert_eq!(found("npm test", out), Some(("jest", counts(4, 1, 0))));
    assert_eq!(
        found(
            "npx jest",
            "Tests:       2 skipped, 1 todo, 3 passed, 6 total\n"
        ),
        Some(("jest", counts(3, 0, 3)))
    );
}

#[test]
fn vitest_reads_the_tests_line_and_not_the_files_line() {
    let out =
        " Test Files  1 failed | 2 passed (3)\n      Tests  1 failed | 4 passed | 1 skipped (6)\n";
    assert_eq!(
        found("npx vitest run", out),
        Some(("vitest", counts(4, 1, 1)))
    );
}

#[test]
fn the_node_test_runner_is_read_from_its_info_lines() {
    let out = "ℹ tests 5\nℹ suites 0\nℹ pass 4\nℹ fail 1\nℹ cancelled 0\nℹ skipped 0\nℹ todo 0\n";
    assert_eq!(found("node --test", out), Some(("node", counts(4, 1, 0))));
}

// ---- go, cargo, nextest, maven ---------------------------------------------------------------------------------------------

#[test]
fn go_counts_top_level_test_functions_only_when_the_command_asks_for_verbose_output() {
    let out = "=== RUN   TestA\n--- PASS: TestA (0.00s)\n=== RUN   TestB\n=== RUN   TestB/sub\n    --- PASS: TestB/sub (0.00s)\n--- FAIL: TestB (0.00s)\n--- SKIP: TestC (0.00s)\nFAIL\nFAIL\texample.com/x\t0.01s\n";
    assert_eq!(
        found("go test -v ./...", out),
        Some(("go", counts(1, 1, 1)))
    );
    assert_eq!(
        found("go test ./...", out),
        None,
        "without -v the lines are not a count of the tests"
    );
    assert_eq!(found("go test ./...", "ok  \texample.com/x\t0.01s\n"), None);
}

#[test]
fn cargo_sums_the_result_line_of_every_test_binary() {
    let out = "running 3 tests\ntest result: ok. 3 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\n   Doc-tests foo\nrunning 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n";
    assert_eq!(found("cargo test", out), Some(("cargo", counts(4, 0, 1))));
    let bad = "test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
    assert_eq!(
        found("cargo test --workspace", bad),
        Some(("cargo", counts(2, 1, 0)))
    );
}

#[test]
fn nextest_reads_its_summary_line() {
    assert_eq!(
        found(
            "cargo nextest run",
            "     Summary [   0.015s] 12 tests run: 11 passed, 1 failed, 2 skipped\n"
        ),
        Some(("nextest", counts(11, 1, 2)))
    );
    assert_eq!(
        found(
            "cargo nextest run",
            "     Summary [   0.015s] 1 test run: 1 passed, 0 skipped\n"
        ),
        Some(("nextest", counts(1, 0, 0)))
    );
}

#[test]
fn maven_sums_the_aggregate_lines_and_leaves_out_the_per_class_lines() {
    let out = "[INFO] Running com.example.FooTest\n[INFO] Tests run: 2, Failures: 0, Errors: 0, Skipped: 0, Time elapsed: 0.05 s - in com.example.FooTest\n[INFO] Results:\n[INFO] \n[INFO] Tests run: 12, Failures: 1, Errors: 2, Skipped: 1\n";
    assert_eq!(found("mvn -B test", out), Some(("maven", counts(8, 3, 1))));
    let two = format!("{out}[INFO] Tests run: 3, Failures: 0, Errors: 0, Skipped: 0\n");
    assert_eq!(
        found("./mvnw -B test", &two),
        Some(("maven", counts(11, 3, 1)))
    );
}

// ---- which parser ----------------------------------------------------------------------------------------------------------

#[test]
fn a_wrapper_script_is_read_when_exactly_one_runner_printed_a_summary() {
    assert_eq!(
        found("sh scripts/test-all.sh", "building\n12 passed in 0.31s\n"),
        Some(("pytest", counts(12, 0, 0)))
    );
    let both = "12 passed in 0.31s\nTests:       1 failed, 4 passed, 5 total\n";
    assert_eq!(
        found("sh scripts/test-all.sh", both),
        None,
        "two runners: no count is chosen"
    );
}

#[test]
fn the_runner_named_by_the_command_wins_over_other_summaries() {
    let both = "12 passed in 0.31s\nTests:       1 failed, 4 passed, 5 total\n";
    assert_eq!(found("npm test", both), Some(("jest", counts(4, 1, 0))));
    assert_eq!(
        found("python3 -m pytest", both),
        Some(("pytest", counts(12, 0, 0)))
    );
}

#[test]
fn an_output_with_no_summary_has_no_counts() {
    assert_eq!(
        found(
            "pytest",
            "Traceback (most recent call last):\n  File x\nImportError: nope\n"
        ),
        None
    );
    assert_eq!(found("true", ""), None);
}

// ---- the stream: bounds ----------------------------------------------------------------------------------------------------

#[test]
fn a_summary_split_across_reads_is_still_found() {
    let mut scan = OutputScan::new(4096);
    for b in "setup\n12 passed in 0.31s\n".as_bytes() {
        scan.feed(std::slice::from_ref(b));
    }
    let done = scan.finish("pytest");
    assert_eq!(done.counts, Some(("pytest", counts(12, 0, 0))));
    assert!(done.tail.ends_with("12 passed in 0.31s\n"));
    assert!(!done.truncated);
}

#[test]
fn a_carriage_return_ends_a_line_so_progress_output_cannot_hide_a_summary() {
    let mut scan = OutputScan::new(4096);
    scan.feed(b"working 10%\rworking 90%\r12 passed in 0.31s\r\n");
    assert_eq!(
        scan.finish("pytest").counts,
        Some(("pytest", counts(12, 0, 0)))
    );
}

#[test]
fn the_tail_keeps_the_end_of_a_long_output_within_the_cap() {
    let mut scan = OutputScan::new(1024);
    for i in 0..5000 {
        scan.feed(format!("line {i}\n").as_bytes());
    }
    scan.feed(b"7 passed in 1.0s\n");
    let done = scan.finish("pytest");
    assert!(done.tail.len() <= 1024, "{}", done.tail.len());
    assert!(done.tail.ends_with("7 passed in 1.0s\n"));
    assert!(done.truncated);
    assert_eq!(done.counts, Some(("pytest", counts(7, 0, 0))));
}

#[test]
fn the_tail_is_cut_at_a_character_boundary_and_has_no_control_characters() {
    let mut scan = OutputScan::new(64);
    scan.feed("é".repeat(100).as_bytes());
    scan.feed(b"\nbell \x07 and escape \x1b[1m done\n");
    let done = scan.finish("true");
    assert!(done.tail.len() <= 64);
    assert!(
        done.tail
            .chars()
            .all(|c| !c.is_control() || c == '\n' || c == '\t'),
        "{:?}",
        done.tail
    );
    assert!(done.tail.ends_with("done\n"), "{:?}", done.tail);
}

#[test]
fn a_very_long_line_is_dropped_past_its_limit_and_the_summary_after_it_is_still_read() {
    let mut scan = OutputScan::new(4096);
    scan.feed("x".repeat(200_000).as_bytes());
    scan.feed(b"\n12 passed in 0.31s\n");
    assert_eq!(
        scan.finish("pytest").counts,
        Some(("pytest", counts(12, 0, 0)))
    );
}

#[test]
fn a_short_output_is_returned_whole_and_not_marked_truncated() {
    let mut scan = OutputScan::new(4096);
    scan.feed(b"hello\nworld\n");
    let done = scan.finish("true");
    assert_eq!(done.tail, "hello\nworld\n");
    assert!(!done.truncated);
    assert_eq!(done.counts, None);
}

// ---- a shell's "program not found" line --------------------------------------------------------------------------------------

use agentd::direct::report::parse::missing_program_in;

#[test]
fn the_shells_not_found_lines_name_a_bare_program() {
    for (line, want) in [
        ("sh: 1: cargo: not found", "cargo"),
        ("/bin/sh: 1: cargo: not found", "cargo"),
        ("sh: 3: cargo: not found", "cargo"),
        ("bash: cargo: command not found", "cargo"),
        ("bash: line 1: cargo: command not found", "cargo"),
        ("/bin/bash: line 12: npm: command not found", "npm"),
        ("sh: line 1: python3.11: command not found", "python3.11"),
        ("sh: 1: g++: not found", "g++"),
        ("cargo: command not found", "cargo"),
        ("cargo: not found\r", "cargo"),
    ] {
        assert_eq!(missing_program_in(line).as_deref(), Some(want), "{line:?}");
    }
}

#[test]
fn a_path_an_option_or_another_message_names_no_program() {
    for line in [
        "sh: 1: ./gradlew: not found",
        "sh: 1: scripts/test-all.sh: not found",
        "sh: 1: /usr/bin/cargo: not found",
        "sh: 1: -x: not found",
        "bash: line 1: ./run: No such file or directory",
        "error: package not found",
        "ModuleNotFoundError: No module named 'x'",
        "FAILED tests/test_a.py::test_x - AssertionError: file not found",
        "the file was not found",
        "sh: 1: cargo: Permission denied",
        ": not found",
        "sh: 1:  : not found",
        "sh: 1: two words: not found",
        "",
    ] {
        assert_eq!(missing_program_in(line), None, "{line:?}");
    }
}

#[test]
fn a_scan_reports_the_first_program_a_line_of_the_stream_says_is_missing() {
    let mut scan = OutputScan::new(4096);
    scan.feed(b"running\nsh: 1: cargo: not found\nbash: line 2: other: command not found\n");
    let out = scan.finish("cargo test");
    assert_eq!(out.missing_program.as_deref(), Some("cargo"));
    let mut quiet = OutputScan::new(4096);
    quiet.feed(b"12 passed in 0.31s\n");
    assert_eq!(quiet.finish("pytest").missing_program, None);
}

// ---- a full disk ---------------------------------------------------------------------------------------------------------------
//
// The report's checkout sits on the session's root volume. A build that fills it dies with the system's own words, and the report names that
// instead of reading the exit code as a failing suite.

use agentd::direct::report::parse::out_of_disk_line;

#[test]
fn the_systems_words_for_a_full_disk_are_found_in_each_tools_own_wrapping() {
    for (line, want) in [
        (
            "error: couldn't create a temp dir: No space left on device (os error 28) at path \"/var/lib/agentd/01/direct/report-02/checkout/target/debug/deps/rmetaAbC123\"",
            "couldn't create a temp dir: No space left on device (os error 28)",
        ),
        (
            "OSError: [Errno 28] No space left on device",
            "OSError: [Errno 28] No space left on device",
        ),
        (
            "npm ERR! ENOSPC: no space left on device, write",
            "npm ERR! ENOSPC: no space left on device",
        ),
        (
            "write /tmp/go-build123/b001/_pkg_.a: no space left on device",
            "write /tmp/go-build123/b001/_pkg_.a: no space left on device",
        ),
        (
            "java.io.IOException: No space left on device",
            "java.io.IOException: No space left on device",
        ),
        (
            "  \x1b[31mNO SPACE LEFT ON DEVICE\x1b[0m  ",
            "NO SPACE LEFT ON DEVICE",
        ),
    ] {
        assert_eq!(out_of_disk_line(line).as_deref(), Some(want), "{line:?}");
    }
}

#[test]
fn a_line_that_only_talks_about_space_or_devices_is_not_a_full_disk() {
    for line in [
        "test disk::reports_a_missing_device ... ok",
        "no space left",
        "left on device",
        "Space left: 3 GiB on device sda",
        "tests::handles_no_space_left_on_device_error ... ok",
        // Node's ENOSPC for file watchers is a limit of the kernel and no full disk.
        "Error: ENOSPC: System limit for number of file watchers reached, watch '/x/src'",
        "",
    ] {
        assert_eq!(out_of_disk_line(line), None, "{line:?}");
    }
}

#[test]
fn the_line_is_kept_to_a_hundred_characters_that_still_hold_the_phrase() {
    let long = format!(
        "{} No space left on device (os error 28) at path \"/a/b\"",
        "x".repeat(300)
    );
    let kept = out_of_disk_line(&long).unwrap();
    assert!(kept.chars().count() <= 100, "{}", kept.chars().count());
    assert!(
        kept.ends_with("No space left on device (os error 28)"),
        "{kept}"
    );
}

#[test]
fn a_scan_keeps_the_first_line_that_says_the_disk_is_full_even_when_it_left_the_tail() {
    let mut scan = OutputScan::new(64);
    scan.feed(
        b"compiling a\nerror: couldn't create a temp dir: No space left on device (os error 28)\n",
    );
    for i in 0..200 {
        scan.feed(format!("error[E0463]: can't find crate {i}\n").as_bytes());
    }
    scan.feed(b"OSError: [Errno 28] No space left on device\n");
    let out = scan.finish("cargo test");
    assert!(out.truncated);
    assert!(
        !out.tail.contains("temp dir"),
        "the first line left the tail"
    );
    assert_eq!(
        out.out_of_disk.as_deref(),
        Some("couldn't create a temp dir: No space left on device (os error 28)")
    );
    let mut quiet = OutputScan::new(4096);
    quiet.feed(b"12 passed in 0.31s\n");
    assert_eq!(quiet.finish("pytest").out_of_disk, None);
}
