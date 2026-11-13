//! The text is cleaned (no escape sequence or control survives), masked (presigned links and token shapes), and refused above 64 KiB
//! rather than cut. The route is a local tool run as an argument vector with the text on stdin (`pbcopy`, `wl-copy`, `xclip`, `xsel`),
//! or the OSC 52 sequence over ssh or when no tool exists. Only this module can build the OSC 52 bytes, and only from text that passed
//! `prepare`; transcript text never reaches them except as the text the user selected, after cleaning.
#![allow(clippy::unwrap_used)]

use ham::tui::copy::{
    COPY_LIMIT, CopyError, Os, Outcome, Prepared, Route, Runner, SystemRunner, base64, block_text,
    copy, last_answer, on_path, osc52, plan, plan_here, prepare, size_text,
};
use ham_core::chat::{Block, BlockKind, ChatModel, GapCause, Origin, Role, Source};
use ham_core::notice::NoticeKind;
use proptest::prelude::*;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

// ---- base64 -----------------------------------------------------------------------------------------------------------------------

fn unbase64(s: &str) -> Vec<u8> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => panic!("not base64: {c}"),
    };
    let mut out = Vec::new();
    let b = s.as_bytes();
    for chunk in b.chunks(4) {
        let pad = chunk.iter().filter(|c| **c == b'=').count();
        let v: Vec<u32> = chunk
            .iter()
            .filter(|c| **c != b'=')
            .map(|c| u32::from(val(*c)))
            .collect();
        let mut n = 0u32;
        for (i, x) in v.iter().enumerate() {
            n |= x << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    out
}

#[test]
fn base64_matches_the_rfc_4648_vectors() {
    let cases = [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ];
    for (plain, want) in cases {
        assert_eq!(base64(plain.as_bytes()), want, "{plain:?}");
    }
    assert_eq!(base64(&[0xff, 0xfe]), "//4=");
    assert_eq!(base64(&[0xfb, 0xff, 0xbf]), "+/+/");
}

// ---- prepare ----------------------------------------------------------------------------------------------------------------------

#[test]
fn plain_text_passes_through_unchanged() {
    let p = prepare("hello\nworld\twith a tab").unwrap();
    assert_eq!(p.text(), "hello\nworld\twith a tab");
    assert_eq!(p.bytes(), 22);
    assert!(!p.masked());
}

#[test]
fn escape_sequences_and_controls_never_survive() {
    let inputs = [
        ("a\u{1b}]0;title\u{7}b", "ab"),
        (
            "a\u{1b}]8;;https://example.test/\u{7}link\u{1b}]8;;\u{7}b",
            "alinkb",
        ),
        ("a\u{1b}[31mred\u{1b}[0mb", "aredb"),
        ("a\rb\u{8}c\u{7}d\u{7f}e", "abcde"),
        ("a\u{9b}31mb\u{90}dcs\u{9c}c", "abc"),
    ];
    for (input, want) in inputs {
        let p = prepare(input).unwrap();
        assert_eq!(p.text(), want, "{input:?}");
    }
}

#[test]
fn bidirectional_zero_width_and_tag_characters_are_shown_as_markers_so_the_copy_equals_what_is_visible()
 {
    let p = prepare("ab\u{202e}cd").unwrap();
    assert_eq!(p.text(), "ab<U+202E>cd");
    let q = prepare("a\u{200b}b\u{e0041}c\u{2066}d").unwrap();
    assert_eq!(q.text(), "a<U+200B>b<U+E0041>c<U+2066>d");
}

#[test]
fn nothing_to_copy_is_refused() {
    assert_eq!(prepare("").unwrap_err(), CopyError::Empty);
    assert_eq!(prepare(" \n\t ").unwrap_err(), CopyError::Empty);
    assert_eq!(
        prepare("\u{1b}[31m\u{1b}[0m").unwrap_err(),
        CopyError::Empty
    );
}

#[test]
fn a_presigned_link_and_token_shapes_are_masked() {
    let sig = "0123456789abcdef".repeat(4);
    let url = format!(
        "https://bucket.s3.us-east-1.amazonaws.com/run/zip?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIA-test%2F20261016&X-Amz-Signature={sig}"
    );
    let token = format!("ghp_{}", "A1b2".repeat(9));
    let p = prepare(&format!("get {url} and use {token} now")).unwrap();
    assert!(p.masked());
    assert!(!p.text().contains(&sig), "{}", p.text());
    assert!(!p.text().contains(&token), "{}", p.text());
    assert!(p.text().contains("[redacted]"));
    assert!(p.text().starts_with("get https://bucket.s3"));
}

#[test]
fn the_limit_is_sixty_four_kib_and_over_it_nothing_is_copied() {
    assert_eq!(COPY_LIMIT, 64 * 1024);
    let ok = "a".repeat(COPY_LIMIT);
    assert_eq!(prepare(&ok).unwrap().bytes(), COPY_LIMIT);
    let err = prepare(&"a".repeat(COPY_LIMIT + 1)).unwrap_err();
    assert_eq!(
        err,
        CopyError::TooLarge {
            bytes: COPY_LIMIT + 1,
            limit: COPY_LIMIT
        }
    );
    // Bytes, not characters.
    let wide = "é".repeat(COPY_LIMIT / 2 + 1);
    assert!(matches!(
        prepare(&wide).unwrap_err(),
        CopyError::TooLarge { .. }
    ));
    // The size that counts is the cleaned text.
    let padded = format!("{}\u{1b}[31m", "a".repeat(COPY_LIMIT));
    assert_eq!(prepare(&padded).unwrap().bytes(), COPY_LIMIT);
    assert!(
        err.to_string().contains("64 KiB") && err.to_string().contains("/export"),
        "{err}"
    );
}

#[test]
fn text_far_over_the_limit_is_refused_before_it_is_scanned() {
    let huge = "a".repeat(COPY_LIMIT * 4 + 1);
    assert_eq!(
        prepare(&huge).unwrap_err(),
        CopyError::TooLarge {
            bytes: COPY_LIMIT * 4 + 1,
            limit: COPY_LIMIT
        }
    );
}

#[test]
fn sizes_read_in_plain_units() {
    let cases = [
        (0, "0 bytes"),
        (1, "1 byte"),
        (12, "12 bytes"),
        (1023, "1023 bytes"),
        (1024, "1.0 KiB"),
        (1536, "1.5 KiB"),
        (65536, "64.0 KiB"),
        (1_048_576, "1.0 MiB"),
        (3_355_443, "3.2 MiB"),
    ];
    for (n, want) in cases {
        assert_eq!(size_text(n), want);
    }
}

// ---- the route --------------------------------------------------------------------------------------------------------------------

fn labels(os: Os, env: &[(&str, &str)], programs: &[&str]) -> Vec<String> {
    let env: HashMap<String, String> = env
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    let get = |k: &str| env.get(k).cloned();
    let has = |p: &str| programs.contains(&p);
    plan(os, &get, &has).iter().map(|r| r.label()).collect()
}

#[test]
fn the_route_follows_the_machine_and_ends_in_osc_52() {
    let all = ["pbcopy", "wl-copy", "xclip", "xsel"];
    #[allow(clippy::type_complexity)]
    let table: Vec<(&str, Os, Vec<(&str, &str)>, Vec<&str>, Vec<&str>)> = vec![
        (
            "mac",
            Os::MacOs,
            vec![],
            all.to_vec(),
            vec!["pbcopy", "OSC 52"],
        ),
        (
            "mac without pbcopy",
            Os::MacOs,
            vec![],
            vec![],
            vec!["OSC 52"],
        ),
        (
            "mac over ssh",
            Os::MacOs,
            vec![("SSH_CONNECTION", "10.0.0.1 22 10.0.0.2 5000")],
            all.to_vec(),
            vec!["OSC 52"],
        ),
        (
            "wayland",
            Os::Linux,
            vec![("WAYLAND_DISPLAY", "wayland-0")],
            all.to_vec(),
            vec!["wl-copy", "OSC 52"],
        ),
        (
            "wayland with xwayland",
            Os::Linux,
            vec![("WAYLAND_DISPLAY", "wayland-0"), ("DISPLAY", ":0")],
            all.to_vec(),
            vec!["wl-copy", "xclip", "xsel", "OSC 52"],
        ),
        (
            "wayland without wl-copy falls to x11",
            Os::Linux,
            vec![("WAYLAND_DISPLAY", "wayland-0"), ("DISPLAY", ":0")],
            vec!["xclip"],
            vec!["xclip", "OSC 52"],
        ),
        (
            "x11 with only xsel",
            Os::Linux,
            vec![("DISPLAY", ":1")],
            vec!["xsel"],
            vec!["xsel", "OSC 52"],
        ),
        (
            "x11 tools but no display",
            Os::Linux,
            vec![],
            all.to_vec(),
            vec!["OSC 52"],
        ),
        (
            "linux over ssh with a forwarded display",
            Os::Linux,
            vec![("SSH_TTY", "/dev/pts/3"), ("DISPLAY", "localhost:10.0")],
            all.to_vec(),
            vec!["OSC 52"],
        ),
        (
            "an empty ssh variable is unset",
            Os::Linux,
            vec![("SSH_TTY", ""), ("DISPLAY", ":0")],
            vec!["xclip"],
            vec!["xclip", "OSC 52"],
        ),
        (
            "another system",
            Os::Other,
            vec![],
            all.to_vec(),
            vec!["OSC 52"],
        ),
    ];
    for (name, os, env, programs, want) in table {
        assert_eq!(labels(os, &env, &programs), want, "{name}");
    }
}

#[test]
fn tmux_changes_nothing_because_the_sequence_is_sent_plain() {
    // Inside tmux a plain OSC 52 works when `set-clipboard on`; the wrapped form needs `allow-passthrough on`, which is off by default
    // since tmux 3.3, so the plain form is the one sent.
    let get = |k: &str| (k == "TMUX").then(|| "/tmp/tmux-1000/default,1,0".to_string());
    assert_eq!(plan(Os::Linux, &get, &|_| false), vec![Route::Osc52]);
    assert_eq!(plan(Os::Linux, &|_| None, &|_| false), vec![Route::Osc52]);
}

#[test]
fn tools_are_named_with_their_fixed_argument_vectors() {
    let get = |k: &str| (k == "DISPLAY").then(|| ":0".to_string());
    let routes = plan(Os::Linux, &get, &|_| true);
    let argv: Vec<Vec<&str>> = routes
        .iter()
        .filter_map(|r| match r {
            Route::Tool(t) => Some(t.argv()),
            Route::Osc52 => None,
        })
        .collect();
    assert_eq!(
        argv,
        vec![
            vec!["xclip", "-selection", "clipboard"],
            vec!["xsel", "--clipboard", "--input"]
        ]
    );
    let mac = plan(Os::MacOs, &|_| None, &|_| true);
    assert!(matches!(&mac[0], Route::Tool(t) if t.argv() == vec!["pbcopy"]));
}

#[test]
fn a_program_is_found_when_it_is_an_executable_file_on_the_path() {
    let path = std::env::var("PATH").ok();
    assert!(on_path("sh", path.as_deref()));
    assert!(!on_path("ham-no-such-program", path.as_deref()));
    assert!(!on_path("sh", None));
    assert!(!on_path("sh", Some("")));
    assert!(!on_path("", Some("/bin")));
    // A name with a slash is a path, never looked up.
    assert!(!on_path("../bin/sh", Some("/usr")));
    assert!(!on_path("bin/sh", Some("/")));
}

#[test]
fn the_plan_for_this_machine_always_ends_in_osc_52() {
    assert_eq!(plan_here().last(), Some(&Route::Osc52));
    assert_eq!(Os::current() == Os::MacOs, cfg!(target_os = "macos"));
}

// ---- copy -------------------------------------------------------------------------------------------------------------------------

/// A runner that records each call and answers from a script.
#[derive(Default)]
struct Script {
    calls: Vec<(Vec<String>, Vec<u8>)>,
    answers: Vec<io::Result<bool>>,
}

impl Runner for Script {
    fn run(&mut self, argv: &[&str], input: &[u8]) -> io::Result<bool> {
        self.calls.push((
            argv.iter().map(|a| (*a).to_string()).collect(),
            input.to_vec(),
        ));
        if self.answers.is_empty() {
            return Ok(true);
        }
        self.answers.remove(0)
    }
}

fn tool_routes() -> Vec<Route> {
    let get = |k: &str| (k == "DISPLAY").then(|| ":0".to_string());
    plan(Os::Linux, &get, &|_| true)
}

#[test]
fn the_first_tool_that_works_is_the_route_and_the_terminal_stays_untouched() {
    let p = prepare("copy me").unwrap();
    let mut r = Script::default();
    let mut term: Vec<u8> = Vec::new();
    let out = copy(&p, &tool_routes(), &mut r, &mut term).unwrap();
    assert_eq!(r.calls.len(), 1);
    assert_eq!(r.calls[0].0, vec!["xclip", "-selection", "clipboard"]);
    assert_eq!(r.calls[0].1, b"copy me");
    assert!(term.is_empty(), "no OSC 52 when a tool took it");
    assert_eq!(out.bytes, 7);
    assert_eq!(out.route.label(), "xclip");
    assert!(!out.masked);
}

#[test]
fn a_tool_that_fails_or_is_missing_hands_over_to_the_next_route() {
    let p = prepare("copy me").unwrap();
    let mut r = Script {
        answers: vec![
            Ok(false),
            Err(io::Error::new(io::ErrorKind::NotFound, "no xsel")),
        ],
        ..Script::default()
    };
    let mut term: Vec<u8> = Vec::new();
    let out = copy(&p, &tool_routes(), &mut r, &mut term).unwrap();
    assert_eq!(r.calls.len(), 2);
    assert_eq!(out.route, Route::Osc52);
    let want = [&[ESC][..], b"]52;c;", base64(b"copy me").as_bytes(), &[BEL]].concat();
    assert_eq!(term, want);
}

#[test]
fn the_sequence_is_one_osc_52_write_to_the_clipboard_selection() {
    let p = prepare("hi").unwrap();
    let want = [&[ESC][..], b"]52;c;", base64(b"hi").as_bytes(), &[BEL]].concat();
    assert_eq!(osc52(&p), want);
    assert_eq!(osc52(&p).iter().filter(|b| **b == ESC).count(), 1);
}

#[test]
fn text_travels_on_stdin_only_and_never_in_the_argument_vector() {
    let nasty = "text with $HOME; a space, \"quoted\" and 'single' marks && more\nline two";
    let p = prepare(nasty).unwrap();
    let mut r = Script::default();
    let mut term: Vec<u8> = Vec::new();
    copy(&p, &tool_routes(), &mut r, &mut term).unwrap();
    let (argv, input) = &r.calls[0];
    assert!(
        argv.iter()
            .all(|a| !a.contains("$HOME") && !a.contains("quoted"))
    );
    assert_eq!(argv[0], "xclip", "the program is called directly");
    assert_eq!(input.as_slice(), nasty.as_bytes());
}

#[test]
fn no_route_is_an_error_and_a_failing_terminal_is_reported() {
    let p = prepare("x").unwrap();
    let mut r = Script::default();
    let mut term: Vec<u8> = Vec::new();
    assert_eq!(
        copy(&p, &[], &mut r, &mut term).unwrap_err(),
        CopyError::NoRoute
    );
    struct Broken;
    impl io::Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("closed"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let e = copy(&p, &[Route::Osc52], &mut r, &mut Broken).unwrap_err();
    assert!(matches!(e, CopyError::Write(m) if m.contains("closed")));
}

#[test]
fn the_notice_names_the_path_that_was_used() {
    let tool = Outcome {
        route: tool_routes().remove(0),
        bytes: 12,
        masked: false,
    };
    assert_eq!(tool.notice(), "Copied 12 bytes with xclip.");
    let osc = Outcome {
        route: Route::Osc52,
        bytes: 1536,
        masked: true,
    };
    assert_eq!(
        osc.notice(),
        "Copied 1.5 KiB with OSC 52; the terminal decides whether it takes it. A secret-shaped value was hidden."
    );
}

// ---- the real runner ----------------------------------------------------------------------------------------------------------------

#[test]
fn the_system_runner_pipes_stdin_and_reports_the_exit_status() {
    let mut r = SystemRunner::default();
    assert!(r.run(&["cat"], b"hello").unwrap());
    assert!(!r.run(&["false"], b"").unwrap());
    let big = vec![b'x'; COPY_LIMIT];
    assert!(r.run(&["cat"], &big).unwrap());
    // A program that never reads its input must not hang the writer.
    assert!(r.run(&["true"], &big).unwrap());
    let e = r.run(&["ham-no-such-clipboard-tool"], b"x").unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::NotFound);
}

#[test]
fn the_system_runner_gives_up_on_a_tool_that_hangs() {
    let mut r = SystemRunner {
        deadline: Duration::from_millis(200),
    };
    let t = Instant::now();
    let e = r.run(&["sleep", "30"], b"x").unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::TimedOut);
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
}

// ---- which block --------------------------------------------------------------------------------------------------------------------

fn model(blocks: Vec<Block>) -> ChatModel {
    ChatModel::from_blocks(Source::FullHistory, "cline", blocks)
}

fn answer(id: &str, text: &str) -> Block {
    Block::message(id, Role::Engine, Origin::Engine, text)
}

#[test]
fn the_last_answer_is_the_last_engine_message() {
    let m = model(vec![
        Block::message("p", Role::You, Origin::User, "build it"),
        answer("a1", "first"),
        Block::tool_call("t", "run_commands: ls", None, Some("completed"), "out"),
        answer("a2", "second"),
        Block::thought("th", "thinking about it"),
        Block::usage("u", "113k tokens"),
        Block::notice("n", NoticeKind::RunStatus, "Run active"),
        Block::gap("g", GapCause::Shed, None),
    ]);
    let b = last_answer(&m).unwrap();
    assert_eq!(b.id, "a2");
    assert_eq!(b.kind, BlockKind::Message);
}

#[test]
fn a_chat_with_no_engine_message_has_no_last_answer() {
    assert!(last_answer(&model(vec![])).is_none());
    let only_user = model(vec![Block::message("p", Role::You, Origin::User, "hi")]);
    assert!(last_answer(&only_user).is_none());
}

#[test]
fn a_record_that_claims_to_be_the_user_is_not_the_users_and_counts_as_the_engine() {
    // Engine origin with the role "you": the transcript draws it with the engine's word, so it is an answer.
    let m = model(vec![Block::message(
        "x",
        Role::You,
        Origin::Engine,
        "sample",
    )]);
    assert_eq!(last_answer(&m).unwrap().id, "x");
}

#[test]
fn a_block_copies_as_its_head_and_its_body() {
    assert_eq!(block_text(&answer("a", "just the text")), "just the text");
    let tool = Block::tool_call(
        "t",
        "run_commands: pytest -q",
        Some("execute"),
        Some("completed"),
        "12 passed",
    );
    assert_eq!(block_text(&tool), "run_commands: pytest -q\n12 passed");
    let head_only = Block::tool_call("t2", "read_files: a.py", None, None, "");
    assert_eq!(block_text(&head_only), "read_files: a.py");
    let diff = Block::diff("d", "src/a.py", "@@ -1 +1 @@\n-a\n+b");
    assert_eq!(block_text(&diff), "src/a.py\n@@ -1 +1 @@\n-a\n+b");
}

// ---- the sequence is built here and nowhere else -------------------------------------------------------------------------------------

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn only_the_copy_module_names_the_clipboard_sequence_and_it_runs_no_shell() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    assert!(files.len() > 20, "the scan found {} files", files.len());
    let mut holders = Vec::new();
    for f in &files {
        let text = std::fs::read_to_string(f).unwrap();
        if text.contains("]52") {
            holders.push(f.strip_prefix(&src).unwrap().to_path_buf());
        }
    }
    assert_eq!(holders, vec![PathBuf::from("tui/copy.rs")]);
    let copy_src = std::fs::read_to_string(src.join("tui/copy.rs")).unwrap();
    for banned in [
        "Command::new(\"sh\")",
        "Command::new(\"bash\")",
        "Command::new(\"/bin/sh\")",
        "\"-c\"",
    ] {
        assert!(!copy_src.contains(banned), "{banned}");
    }
}

// ---- properties -------------------------------------------------------------------------------------------------------------------

fn arb_text() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            4 => any::<char>(),
            2 => prop::sample::select(vec![
                '\u{1b}', '\u{7}', '\r', '\u{8}', '\u{9b}', '\u{9c}', '\u{202e}', '\u{200b}',
                ']', '5', '2', ';', '[', '\\', 'a', ' ', '\n', '\t',
            ]),
        ],
        0..200,
    )
    .prop_map(|cs| cs.into_iter().collect())
}

fn sequences(bytes: &[u8]) -> usize {
    bytes.windows(4).filter(|w| w == b"]52;").count()
}

proptest! {
    #[test]
    fn prepared_text_is_clean_and_the_sequence_carries_exactly_that_text(text in arb_text()) {
        let Ok(p): Result<Prepared, _> = prepare(&text) else { return Ok(()); };
        prop_assert!(p.text().chars().all(|c| !(c < ' ' && c != '\n' && c != '\t') && c != '\u{7f}' && !('\u{80}'..='\u{9f}').contains(&c)),
            "{:?}", p.text());
        let bytes = osc52(&p);
        prop_assert_eq!(sequences(&bytes), 1);
        prop_assert_eq!(bytes.iter().filter(|b| **b == ESC).count(), 1);
        let start = bytes.windows(6).position(|w| w == b"]52;c;").unwrap() + 6;
        let end = bytes.iter().rposition(|b| *b == BEL).unwrap();
        let payload = std::str::from_utf8(&bytes[start..end]).unwrap();
        prop_assert!(payload.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'='));
        prop_assert_eq!(unbase64(payload), p.text().as_bytes());
    }

    #[test]
    fn base64_round_trips(bytes in prop::collection::vec(any::<u8>(), 0..300)) {
        let s = base64(&bytes);
        prop_assert_eq!(s.len() % 4, 0);
        prop_assert_eq!(unbase64(&s), bytes);
    }

    #[test]
    fn a_selection_that_holds_a_clipboard_sequence_copies_as_inert_text(
        before in "[a-z ]{0,10}", payload in "[A-Za-z0-9+/=]{0,20}", after in "[a-z ]{0,10}",
    ) {
        let text = format!("x{before}\u{1b}]52;c;{payload}\u{7}{after}y");
        let p = prepare(&text).unwrap();
        prop_assert_eq!(sequences(p.text().as_bytes()), 0);
        let mut r = Script::default();
        let mut term: Vec<u8> = Vec::new();
        copy(&p, &[Route::Osc52], &mut r, &mut term).unwrap();
        prop_assert_eq!(sequences(&term), 1, "the user's own write is the only sequence");
        prop_assert!(r.calls.is_empty());
    }
}
