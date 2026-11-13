//! A property test of the stderr scrub.
//! Random stderr made of secret-shaped, quote-heavy, multi-byte and invisible fragments goes through `scrub`,
//! `error_body`, `log_body` and the text helpers; none may panic, and the bounds the orchestrator and the log rely on must
//! hold for every input. The count is `ENGINE_DIAGNOSTIC_FUZZ_N` (default 2,000, a few seconds): the review's run is
//! `ENGINE_DIAGNOSTIC_FUZZ_N=400000`.
#![allow(clippy::unwrap_used)]

use harness_core::engine_diagnostic::{
    ERROR_BODY_CHARS, LOG_TAIL_CHARS, append, error_body, log_body, scrub, split, stable, strip,
    with_note,
};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const VOCAB: &[&str] = &[
    "token",
    "Token",
    "TOKEN",
    "secret",
    "password",
    "passwd",
    "Authorization",
    "authorization",
    "proxy-authorization",
    "cookie",
    "api-key",
    "x-api-key",
    "SecretAccessKey",
    "DB_PASS",
    "MYSQL_PWD",
    "PAT",
    "AUTH",
    "OPENAI_KEY",
    "SLACK_WEBHOOK",
    "dbPass",
    ":",
    "=",
    ":=",
    "=>",
    " ",
    "  ",
    "\t",
    "\n",
    "\r\n",
    "\r",
    "\"",
    "'",
    "\\",
    "\\\"",
    "\\'",
    "://",
    "@",
    "?",
    "&",
    ";",
    ",",
    "|",
    "/",
    "+",
    "-",
    "_",
    ".",
    "[",
    "]",
    "{",
    "}",
    "(",
    ")",
    "<",
    ">",
    "%40",
    "%2F",
    "Bearer ",
    "Basic ",
    "Digest ",
    "-p ",
    "--password ",
    "-u ",
    "--user ",
    "login ",
    "<password>",
    "</password>",
    "\"auth\"",
    "auth",
    "https://",
    "http://",
    "ssh://",
    "redis://:",
    "git@",
    "user:pw@host",
    "src/main/java/com/example/demo/UserController1.java",
    "/Users/x/Projects/MyApp2/Source/",
    "AKIA",
    "ASIA",
    "abc",
    "XYZ",
    "0123456789",
    "deadbeef",
    "Error: ",
    "TypeError: ",
    "at f (a.js:1:2)",
    "é",
    "錯",
    "\u{200b}",
    "\u{202e}",
    "\u{feff}",
    "\u{e0049}",
    "\u{e0100}",
    "\u{fe0f}",
    "\u{3164}",
    "\u{34f}",
    "\u{180e}",
    "\u{1b}[31m",
    "\u{1b}]0;x\u{7}",
    "\u{0}",
    "\u{7f}",
    "\u{ff}",
    "-----BEGIN PRIVATE KEY-----",
    "-----END PRIVATE KEY-----",
    "123456789012",
    "ghp_abcdefghijkl",
    "npm_abcdefghijkl",
];

/// The secret-shaped fragments, built at run time: the repository holds no secret-shaped literal (a gitleaks finding otherwise).
fn built_fragments() -> Vec<String> {
    vec![
        ["wJalrXUtnFEMI", "/K7MDENG", "/bPxRfiCYEXAMPLEKEY"].concat(),
        [
            "IQoJb3JpZ2luX2Vj",
            "EPr//////wEaCXVz",
            "LWVhc3QtMSJHMEUC",
            "IQD+1gJk3Fz0",
        ]
        .concat(),
        ["eyJhbGciOiJIUzI1NiJ9", ".eyJzdWIiOiIxIn0", ".sig"].concat(),
    ]
}

fn build(r: &mut Rng, built: &[String]) -> Vec<u8> {
    let n = 1 + r.below(120);
    let mut out: Vec<u8> = vec![];
    for _ in 0..n {
        match r.below(13) {
            0 => out.push((r.next() & 0xff) as u8),
            1 => {
                for _ in 0..(1 + r.below(300)) {
                    out.push(b'a' + (r.below(26) as u8));
                }
            }
            2 => out.extend_from_slice(built[r.below(built.len())].as_bytes()),
            _ => out.extend_from_slice(VOCAB[r.below(VOCAB.len())].as_bytes()),
        }
    }
    out
}

/// The characters the scrub must never leave: the format, tag and variation-selector characters this fuzz can produce.
fn hidden(c: char) -> bool {
    matches!(c,
        '\u{ad}' | '\u{34f}' | '\u{180e}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{3164}'
        | '\u{fe00}'..='\u{fe0f}' | '\u{feff}' | '\u{e0000}'..='\u{e0fff}')
}

#[test]
fn the_scrub_never_panics_stays_bounded_and_keeps_one_line() {
    let n: u32 = std::env::var("ENGINE_DIAGNOSTIC_FUZZ_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000);
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    let built = built_fragments();
    let (mut max_body, mut max_log) = (0, 0);
    for i in 0..n {
        let mut raw = build(&mut r, &built);
        if i % 50 == 0 {
            // A prefix over the window: the cut path.
            let mut big = vec![b'x'; 4096 + r.below(2000)];
            big.extend_from_slice(&raw);
            raw = big;
        }
        let tail = scrub(&raw, r.below(2) == 0);
        let joined = tail.joined();
        assert!(
            !joined.contains('\n') && !joined.contains('\r'),
            "a line break survived: {joined:?}"
        );
        assert!(
            !joined.chars().any(char::is_control),
            "a control character survived: {joined:?}"
        );
        assert!(
            !joined.chars().any(hidden),
            "an invisible character survived: {joined:?}"
        );
        for exit in [
            "exit status 3",
            "killed by signal 9 (SIGKILL)",
            "still running 5 s after its output closed; agentd stopped it",
        ] {
            let body = error_body(exit, &tail);
            let log = log_body(exit, &tail);
            max_body = max_body.max(body.chars().count());
            max_log = max_log.max(log.chars().count());
            assert!(
                body.chars().count() <= ERROR_BODY_CHARS,
                "body too long: {body:?}"
            );
            assert!(
                log.chars().count() <= LOG_TAIL_CHARS + 200,
                "log too long ({})",
                log.chars().count()
            );
            assert!(
                body.starts_with(exit)
                    || body.starts_with(&exit.chars().take(10).collect::<String>()),
                "the exit left the body: {body:?}"
            );
            assert!(!body.contains('\n') && !log.contains('\n'));
            assert!(!body.chars().any(hidden) && !log.chars().any(hidden));
            let error = append("the engine closed its output", &body);
            assert_eq!(strip(&error), "the engine closed its output");
            assert!(stable(&error).starts_with("the engine closed its output [engine process: "));
            let noted = with_note(
                &error,
                &format!(
                    "note {}",
                    String::from_utf8_lossy(&raw[..raw.len().min(40)])
                ),
            );
            let (head, diagnostic) = split(&noted);
            assert!(
                head.starts_with("the engine closed its output; note"),
                "{head:?}"
            );
            assert_eq!(
                diagnostic.unwrap(),
                &error[error.find("[engine process: ").unwrap()..]
            );
        }
    }
    println!("FUZZ ok n={n} max_body={max_body} max_log={max_log}");
}
