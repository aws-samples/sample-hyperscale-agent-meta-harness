//! The engine process's diagnostic inside a failed turn's error text.
//! The text convention, the scrub of the engine's stderr
//! tail and its bounds are pinned here; the file reading and the process reaping are agentd's (`tests/engine_stderr.rs`).
#![allow(clippy::unwrap_used)]

use harness_core::engine_diagnostic::{
    ERROR_BODY_CHARS, LOG_TAIL_CHARS, SEPARATOR, STDERR_WINDOW_BYTES, append, error_body, log_body,
    scrub, split, stable, strip, unreadable_body, with_note,
};

const BASE: &str = "the engine closed its output";

/// A stand-in built at run time: no secret-shaped literal sits in the source.
fn key_id() -> String {
    ["AS", "IA", "TESTTESTTESTTEST"].concat()
}

#[test]
fn the_separator_and_the_bounds_are_the_contract() {
    assert_eq!(SEPARATOR, " [engine process: ");
    assert_eq!(STDERR_WINDOW_BYTES, 4096);
    assert_eq!(ERROR_BODY_CHARS, 150);
    assert_eq!(LOG_TAIL_CHARS, 1500);
}

#[test]
fn append_and_strip_round_trip_and_leave_other_texts_alone() {
    let text = append(BASE, "exit status 3; stderr: boom");
    assert_eq!(
        text,
        "the engine closed its output [engine process: exit status 3; stderr: boom]"
    );
    assert_eq!(strip(&text), BASE);
    assert_eq!(strip(BASE), BASE);
    assert_eq!(strip(""), "");
    assert_eq!(
        strip("engine error -32603: Internal error"),
        "engine error -32603: Internal error"
    );
}

#[test]
fn strip_cuts_at_the_first_separator_whatever_the_tail_says() {
    let odd_tail =
        format!("exit status 1; stderr: {SEPARATOR}ECONNRESET{SEPARATOR}ThrottlingException");
    let text = append(BASE, &odd_tail);
    assert_eq!(strip(&text), BASE);
    let io = append(
        "engine I/O: Broken pipe (os error 32)",
        "exit status 1; stderr empty",
    );
    assert_eq!(strip(&io), "engine I/O: Broken pipe (os error 32)");
}

#[test]
fn stable_keeps_the_agentd_made_part_and_drops_the_stderr_part() {
    let a = append(
        BASE,
        "exit status 3; stderr: Error: EACCES pid 4711 at 10:21:07",
    );
    let b = append(BASE, "exit status 3; stderr: Error: ENOSPC");
    assert_eq!(
        stable(&a),
        "the engine closed its output [engine process: exit status 3]"
    );
    assert_eq!(stable(&a), stable(&b));
    assert_eq!(
        stable(&append(BASE, "exit status 3; stderr empty")),
        stable(&a)
    );
    assert_ne!(
        stable(&append(BASE, "killed by signal 9 (SIGKILL); stderr empty")),
        stable(&a)
    );
    assert_eq!(stable(BASE), BASE);
    // A sample cut before the stderr part ended keeps its exit part.
    assert_eq!(
        stable("the engine closed its output [engine process: exit status 3; stderr: Err"),
        "the engine closed its output [engine process: exit status 3]"
    );
    assert_eq!(
        stable("the engine closed its output [engine process: exit status 3"),
        "the engine closed its output [engine process: exit status 3]"
    );
}

#[test]
fn a_small_log_reads_whole_on_one_line() {
    let raw = b"Error: EACCES: permission denied, mkdir '/var/lib/agentd/r/a/home/.x'\n    at mkdir (node:fs:1)\nNode.js v20.1.0\n";
    let t = scrub(raw, false);
    assert_eq!(
        t.joined(),
        "Error: EACCES: permission denied, mkdir '/var/lib/agentd/r/a/home/.x' | at mkdir (node:fs:1) | Node.js v20.1.0"
    );
    assert!(!t.joined().contains('\n'));
    assert!(!t.cut);
    assert!(!t.long_line);
    assert_eq!(t.bytes, raw.len());
}

#[test]
fn an_empty_or_blank_log_is_empty() {
    assert!(scrub(b"", false).is_empty());
    assert!(scrub(b"\n  \n\t\n", false).is_empty());
    assert_eq!(scrub(b"", false).joined(), "");
    assert_eq!(scrub(b"", false).headline(), None);
}

#[test]
fn escape_sequences_and_control_characters_are_removed() {
    let t = scrub(
        b"\x1b[31mError:\x1b[0m bad\x07 thing\r\n\x1b]0;title\x07done\n",
        false,
    );
    assert_eq!(t.joined(), "Error: bad thing | done");
}

#[test]
fn bidi_overrides_and_zero_width_characters_are_removed() {
    let t = scrub("abc\u{202e}def\u{200b}ghi\u{feff}jkl\n".as_bytes(), false);
    assert_eq!(t.joined(), "abcdefghijkl");
}

#[test]
fn invalid_utf8_does_not_break_the_scrub() {
    let t = scrub(b"before \xff\xfe after", false);
    let s = t.joined();
    assert!(s.starts_with("before "), "{s:?}");
    assert!(s.ends_with(" after"), "{s:?}");
}

#[test]
fn secret_shapes_are_redacted() {
    let key = key_id();
    let pem_body = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7VJTUt9Us8cKj/MzEfYyjiWA4R4/M2bS1GB4t7NXp98C3SC6dVMvDuictGeurT8jNbvJZHtCSuYEvuNMoSfm76oqFvAp8Gy0iz5sxjZmSnXyCdPEovGhLa0VzMaQ8s";
    let hex = "0123456789abcdef".repeat(2);
    let gh = ["gh", "p_", "abcdefghijklmnopqrstuvwxyz0123456789"].concat();
    // Password-shaped stand-ins are built at run time: the repository holds no password literal.
    let pw = ["hunter", "2hunter2"].concat();
    let url_pw = ["pa55", "w0rd"].concat();
    let npm = ["npmtoken", "value1234"].concat();
    let quoted_auth = ["aPrivate", "RegistryCredential"].concat();
    let quoted_pw = ["quoted", "passw0rdvalue"].concat();
    let sk = ["sk-", "live-1234567890"].concat();
    let raw = format!(
        "key {key}\npassword={pw}\nAuthorization: Bearer abcdefghijklmnopqrstuvwxyz0123456789\n\
         clone https://user:{url_pw}@example.com/repo.git\n\
         GET https://bucket.s3.amazonaws.com/obj?X-Amz-Signature={hex}{hex}&X-Amz-Credential=zzz\n\
         {pem_body}\n\
         session {hex}\n\
         token {gh}\n\
         //registry.example.com/:_authToken={npm}\n\
         //registry.example.com/:_authToken=\"{quoted_auth}\"\n\
         _password='{quoted_pw}'\n\
         {{\"apiKey\":\"{sk}\"}}\n\
         denied on arn:aws:iam::123456789012:role/x\n"
    );
    let s = scrub(raw.as_bytes(), false).joined();
    for secret in [
        key.as_str(),
        pw.as_str(),
        "abcdefghijklmnopqrstuvwxyz0123456789",
        url_pw.as_str(),
        hex.as_str(),
        "X-Amz-Credential=zzz",
        "MIIEvQIBADANBgkq",
        "jiWA4R4/M2bS1GB4t7NXp98C3SC6dVMvDuictGeurT8jNbvJZHtCSuYEvuNMoSfm76",
        npm.as_str(),
        quoted_auth.as_str(),
        quoted_pw.as_str(),
        sk.as_str(),
        "123456789012",
    ] {
        assert!(!s.contains(secret), "{secret} leaked: {s}");
    }
    assert!(
        !s.contains("X-Amz-"),
        "the whole query of a URL goes, whatever redact replaced in it first: {s}"
    );
    assert!(s.contains("<redacted>"), "{s}");
    assert!(s.contains("<acct>"), "{s}");
    assert!(
        s.contains("https://bucket.s3.amazonaws.com/obj"),
        "the URL's host and path stay: {s}"
    );
}

#[test]
fn paths_and_ordinary_words_survive_the_scrub() {
    let raw = b"Error: EACCES: permission denied, mkdir '/var/lib/agentd/01M4G51DV50AEMK86GY37KPPW3/planner/home/.cline'\n";
    let s = scrub(raw, false).joined();
    assert_eq!(
        s,
        "Error: EACCES: permission denied, mkdir '/var/lib/agentd/01M4G51DV50AEMK86GY37KPPW3/planner/home/.cline'"
    );
}

#[test]
fn a_private_key_cut_at_the_window_edge_does_not_leak_its_body() {
    let body: String = (0..6)
        .map(|i| {
            format!(
                "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVphYmNkZWZnaGlqa2xtbm9wcXJzdHV2d3h5ejAxMjM{i}\n"
            )
        })
        .collect();
    let raw = format!("{body}-----END PRIVATE KEY-----\nError: after the key\n");
    let s = scrub(raw.as_bytes(), true).joined();
    assert!(!s.contains("QUJDREVGR0hJSktMTU5P"), "{s}");
    assert!(s.contains("Error: after the key"), "{s}");
}

#[test]
fn a_cut_window_drops_its_partial_first_line_and_a_whole_log_keeps_it() {
    let t = scrub(
        b"tial line that started before the window\nsecond line\nthird line\n",
        true,
    );
    assert!(t.cut);
    assert_eq!(t.joined(), "second line | third line");
    let whole = scrub(b"first line\nsecond line\n", false);
    assert_eq!(whole.joined(), "first line | second line");
}

#[test]
fn a_cut_window_without_a_newline_shows_nothing_of_the_fragment() {
    let t = scrub(
        b"mid-line fragment of something long {\"prompt\": \"do the thing",
        true,
    );
    assert!(t.is_empty());
    assert!(t.cut);
    assert!(t.long_line);
    let wide: String = "é".repeat(5000);
    let t = scrub(wide.as_bytes(), true);
    assert!(t.is_empty() && t.long_line);
    // A single complete line that fits the window is shown.
    let one = scrub(b"Error: only line\n", false);
    assert_eq!(one.joined(), "Error: only line");
    assert!(!one.long_line);
}

#[test]
fn the_headline_is_the_last_error_shaped_line_else_the_last_line() {
    let node = scrub(
        b"node:fs:1\n  throw err;\n\nError: EACCES: permission denied, open '/x'\n    at open (node:fs:1:1)\n    at new NodeError (node:internal/errors:405:5)\nNode.js v20.1.0\n",
        false,
    );
    assert_eq!(
        node.headline(),
        Some("Error: EACCES: permission denied, open '/x'")
    );
    let py = scrub(
        b"Traceback (most recent call last):\n  File \"x.py\", line 1, in <module>\nValueError: bad value\n",
        false,
    );
    assert_eq!(py.headline(), Some("ValueError: bad value"));
    for line in [
        "TypeError: x is not a function",
        "Error [ERR_MODULE_NOT_FOUND]: Cannot find module 'y'",
        "FATAL ERROR: Reached heap limit",
        "panic: runtime error: index out of range",
        "thread 'main' panicked at src/main.rs:1:1",
        "npm ERR! code ELIFECYCLE",
        "[ERROR] boom",
        "error: could not compile",
        "Uncaught Exception: z",
        "java.lang.IllegalStateException: no",
    ] {
        let t = scrub(format!("starting\n{line}\nafter\n").as_bytes(), false);
        assert_eq!(t.headline(), Some(line), "{line}");
    }
    let plain = scrub(b"some noise\nmore noise\n", false);
    assert_eq!(plain.headline(), Some("more noise"));
}

#[test]
fn the_error_body_names_the_exit_and_one_line_and_never_outgrows_its_bound() {
    let t = scrub(b"Error: EACCES: x\n    at y\nNode.js v20\n", false);
    assert_eq!(
        error_body("exit status 3", &t),
        "exit status 3; stderr: Error: EACCES: x"
    );
    assert_eq!(
        error_body("exit status 3", &scrub(b"", false)),
        "exit status 3; stderr empty"
    );
    let long = scrub(b"mid-line fragment", true);
    assert_eq!(
        error_body("exit status 3", &long),
        "exit status 3; stderr not shown (one line over the 4096-byte window)"
    );
    let wordy = scrub(
        format!("Error: {}\n", "very long message ".repeat(40)).as_bytes(),
        false,
    );
    let body = error_body("killed by signal 9 (SIGKILL)", &wordy);
    assert!(
        body.chars().count() <= ERROR_BODY_CHARS,
        "{}",
        body.chars().count()
    );
    assert!(
        body.starts_with("killed by signal 9 (SIGKILL); stderr: Error: very long"),
        "{body}"
    );
    assert!(body.contains("..."), "{body}");
    let huge_exit = "x".repeat(400);
    assert!(error_body(&huge_exit, &t).chars().count() <= ERROR_BODY_CHARS);
}

#[test]
fn the_log_body_carries_the_scrubbed_tail_within_its_bound() {
    let t = scrub(b"Error: boom\n", false);
    assert_eq!(
        log_body("exit status 3", &t),
        "exit status 3; stderr tail (12 bytes): Error: boom"
    );
    assert_eq!(
        log_body("exit status 3", &scrub(b"", false)),
        "exit status 3; stderr empty"
    );
    assert_eq!(
        log_body("exit status 3", &scrub(b"mid-line fragment", true)),
        "exit status 3; stderr not shown (one line over the 4096-byte window)"
    );
    let lines: Vec<String> = (0..100)
        .map(|i| format!("line-{i:03} padding padding"))
        .collect();
    let raw = format!("{}\nFINAL-WORDS: boom\n", lines.join("\n"));
    let whole = log_body("exit status 1", &scrub(raw.as_bytes(), false));
    assert!(whole.contains("line-000"), "{whole}");
    assert!(whole.ends_with("FINAL-WORDS: boom"), "{whole}");
    let cut = log_body(
        "exit status 1",
        &scrub(format!("x\n{raw}").as_bytes(), true),
    );
    assert!(
        cut.starts_with("exit status 1; stderr tail (last "),
        "{cut}"
    );
    assert!(cut.contains("earlier output cut): ..."), "{cut}");
    assert!(cut.ends_with("FINAL-WORDS: boom"), "{cut}");
    assert!(!cut.contains("line-000"), "{cut}");
    for b in [&whole, &cut] {
        assert!(
            b.chars().count() <= LOG_TAIL_CHARS + 120,
            "{}",
            b.chars().count()
        );
    }
}

#[test]
fn a_window_sized_or_larger_input_stays_bounded() {
    let big = "word ".repeat(STDERR_WINDOW_BYTES * 3);
    let t = scrub(big.as_bytes(), true);
    assert!(t.is_empty() && t.long_line, "one long line");
    let lines = "some words here\n".repeat(STDERR_WINDOW_BYTES);
    let t = scrub(lines.as_bytes(), true);
    assert!(t.bytes <= STDERR_WINDOW_BYTES, "{}", t.bytes);
    assert!(log_body("exit status 1", &t).chars().count() <= LOG_TAIL_CHARS + 120);
    assert!(error_body("exit status 1", &t).chars().count() <= ERROR_BODY_CHARS);
}

#[test]
fn strip_and_stable_read_only_the_two_texts_agentd_writes_for_the_engines_pipes() {
    // An engine's own RPC message, a stall, a start failure: whatever they hold, they are not cut.
    for foreign in [
        "engine error -32603: Internal error [engine process: exit status 1; stderr: ThrottlingException]",
        "engine start: invalid argument \" [engine process: example",
        "provider stall: no engine output for 900 s [engine process: x]",
    ] {
        assert_eq!(strip(foreign), foreign, "{foreign}");
        assert_eq!(stable(foreign), foreign, "{foreign}");
    }
    let io = "engine I/O: Broken pipe (os error 32) [engine process: exit status 1; stderr empty]";
    assert_eq!(strip(io), "engine I/O: Broken pipe (os error 32)");
    assert_eq!(
        stable(io),
        "engine I/O: Broken pipe (os error 32) [engine process: exit status 1]"
    );
}

#[test]
fn a_cut_window_whose_only_line_was_dropped_is_not_shown_as_empty() {
    // 4097 bytes of one line, then a line break: the window is the end of that line and the break.
    let mut raw = vec![b'x'; 4097];
    raw.push(b'\n');
    let t = scrub(&raw, false);
    assert!(t.is_empty());
    assert!(
        t.cut && t.long_line,
        "cut {} long_line {}",
        t.cut,
        t.long_line
    );
    assert_eq!(
        error_body("exit status 0", &t),
        "exit status 0; stderr not shown (one line over the 4096-byte window)"
    );
    // Blank lines after a dropped fragment are still no content.
    assert!(scrub(b"\n\n", true).is_empty());
    assert!(!scrub(b"\n\n", true).long_line);
}

#[test]
fn an_unreadable_log_is_said_so_within_the_bound() {
    assert_eq!(
        unreadable_body("exit status 1", "Input/output error (os error 5)"),
        "exit status 1; stderr unreadable (Input/output error (os error 5))"
    );
    let body = unreadable_body(&"x".repeat(300), &"y".repeat(400));
    assert!(
        body.chars().count() <= ERROR_BODY_CHARS,
        "{}",
        body.chars().count()
    );
}

// ---- Credentials in every spelling (review of e44c5417, M1) ------------------------------------------------------------
//
// The scrub passed the credential_process JSON this system hands its engines (`{"AccessKeyId":...,"SecretAccessKey":...,
// "SessionToken":...}`): `refusal::redact` keys its rule on the underscore spellings and its opaque-token class stops at a
// `/`, so the key id went and the secret key and the token's pieces stayed. Every stand-in below is built at run time (no
// secret-shaped literal sits in the source), and a piece of a secret that survives counts as a leak.

/// A 40-character AWS-secret-shaped stand-in with two slashes: no run of it reaches 40 characters once a `/` splits it.
fn secret_key() -> String {
    ["wJalrXUtnFEMI", "/K7MDENG", "/bPxRfiCYEXAMPLEKEY"].concat()
}

/// A session-token-shaped stand-in of about 280 base64 characters with slashes and plus signs, and padding at its end.
fn session_token() -> String {
    let unit = [
        "IQoJb3JpZ2luX2Vj",
        "EPr//////wEaCXVz",
        "LWVhc3QtMSJHMEUC",
        "IQD+1gJk3Fz0aBcd",
        "EfGhIjKlMnOpQrSt/UvWxYz0123456789+AbCdEfGh",
    ]
    .concat();
    format!("{unit}{unit}{unit}==")
}

/// A password-shaped stand-in, built at run time.
fn secret_word(tag: &str) -> String {
    ["hunter", "2", tag, "hunter2"].concat()
}

/// The first piece of `secret` (split at `/`, `+` and `=`, at least 10 characters) or the whole secret that `text` still holds.
fn leaked(text: &str, secret: &str) -> Option<String> {
    if text.contains(secret) {
        return Some(secret.to_string());
    }
    secret
        .split(['/', '+', '='])
        .filter(|p| p.len() >= 10)
        .find(|p| text.contains(*p))
        .map(str::to_string)
}

/// Scrubs `raw` and fails when any secret, or a piece of one, shows in the joined tail, the orchestrator's body or the
/// runtime log's. Returns the joined tail.
fn scrubbed_without(what: &str, raw: &str, secrets: &[String]) -> String {
    let tail = scrub(raw.as_bytes(), false);
    let joined = tail.joined();
    let body = error_body("exit status 1", &tail);
    let log = log_body("exit status 1", &tail);
    for secret in secrets {
        for (where_, text) in [
            ("tail", joined.as_str()),
            ("error body", body.as_str()),
            ("log body", log.as_str()),
        ] {
            if let Some(piece) = leaked(text, secret) {
                panic!("{what}: {piece:?} survived in the {where_}: {text}");
            }
        }
    }
    joined
}

/// A credential_process JSON line with these key spellings, then the same keys with short values (a rule on the key name, not
/// on the shape of a long token, must take those), then an error line the headline must still find.
macro_rules! credential_json_spelling {
    ($name:ident, $access:literal, $secret:literal, $session:literal) => {
        #[test]
        fn $name() {
            let (k, s, t) = (key_id(), secret_key(), session_token());
            let (w2, w3) = (secret_word("w2"), secret_word("w3"));
            let raw = format!(
                concat!(
                    "{{\"Version\":1,\"",
                    $access,
                    "\":\"{k}\",\"",
                    $secret,
                    "\":\"{s}\",\"",
                    $session,
                    "\":\"{t}\",\"Expiration\":\"2026-10-09T18:00:00Z\"}}\n",
                    "{{\"",
                    $secret,
                    "\":\"{w2}\",\"",
                    $session,
                    "\":\"{w3}\"}}\nError: boom\n"
                ),
                k = k,
                s = s,
                t = t,
                w2 = w2,
                w3 = w3
            );
            let out = scrubbed_without(stringify!($name), &raw, &[k, s, t, w2, w3]);
            assert!(out.contains("\"Version\":1"), "{out}");
            assert!(
                out.contains("\"Expiration\":\"2026-10-09T18:00:00Z\""),
                "the value after the last secret stays: {out}"
            );
            assert!(
                out.contains(concat!("\"", $secret, "\":\"<redacted>\"")),
                "the key stays and its quotes stay around the redaction: {out}"
            );
            assert!(
                out.contains(concat!("\"", $session, "\":\"<redacted>\"")),
                "{out}"
            );
            assert!(out.ends_with("Error: boom"), "{out}");
        }
    };
}

credential_json_spelling!(
    credential_json_pascal_case,
    "AccessKeyId",
    "SecretAccessKey",
    "SessionToken"
);
credential_json_spelling!(
    credential_json_camel_case,
    "accessKeyId",
    "secretAccessKey",
    "sessionToken"
);
credential_json_spelling!(
    credential_json_snake_case,
    "access_key_id",
    "secret_access_key",
    "session_token"
);
credential_json_spelling!(
    credential_json_upper_snake_case,
    "ACCESS_KEY_ID",
    "SECRET_ACCESS_KEY",
    "SESSION_TOKEN"
);
credential_json_spelling!(
    credential_json_aws_env_names,
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN"
);
credential_json_spelling!(
    credential_json_lower_case_run_together,
    "accesskeyid",
    "secretaccesskey",
    "sessiontoken"
);
credential_json_spelling!(
    credential_json_with_a_bare_token_key,
    "AccessKeyId",
    "SecretAccessKey",
    "Token"
);
credential_json_spelling!(
    credential_json_with_a_security_token_key,
    "AccessKeyId",
    "SecretAccessKey",
    "SecurityToken"
);

#[test]
fn credential_json_printed_over_several_lines() {
    let (k, s, t) = (key_id(), secret_key(), session_token());
    let raw = format!(
        "{{\n  \"Version\": 1,\n  \"AccessKeyId\": \"{k}\",\n  \"SecretAccessKey\": \"{s}\",\n  \"SessionToken\": \"{t}\"\n}}\nError: boom\n"
    );
    let out = scrubbed_without("pretty JSON", &raw, &[k, s, t]);
    assert!(out.contains("\"Version\": 1"), "{out}");
    assert!(out.ends_with("Error: boom"), "{out}");
}

#[test]
fn credential_json_in_single_quotes_as_a_dict_repr() {
    let (k, s, t) = (key_id(), secret_key(), session_token());
    let raw = format!(
        "helper said {{'Version': 1, 'AccessKeyId': '{k}', 'SecretAccessKey': '{s}', 'SessionToken': '{t}'}}\n"
    );
    scrubbed_without("single-quoted repr", &raw, &[k, s, t]);
}

#[test]
fn credential_json_inside_a_json_string_with_escaped_quotes() {
    let (k, s, t) = (key_id(), secret_key(), session_token());
    let raw = format!(
        "{{\"level\":\"error\",\"msg\":\"credential_process said {{\\\"AccessKeyId\\\":\\\"{k}\\\",\\\"SecretAccessKey\\\":\\\"{s}\\\",\\\"SessionToken\\\":\\\"{t}\\\"}}\"}}\n"
    );
    scrubbed_without("escaped JSON in a JSON string", &raw, &[k, s, t]);
}

#[test]
fn a_secret_after_key_equals_or_key_colon_in_every_case_is_redacted() {
    let (s, t) = (secret_key(), session_token());
    for key in [
        "SecretAccessKey",
        "secretAccessKey",
        "SECRET_ACCESS_KEY",
        "aws_secret_access_key",
        "AWS_SECRET_ACCESS_KEY",
        "secret-access-key",
    ] {
        for sep in ["=", ": ", " = ", ":"] {
            let raw = format!("{key}{sep}{s}\nError: boom\n");
            scrubbed_without(&format!("{key}{sep}"), &raw, std::slice::from_ref(&s));
        }
    }
    for key in [
        "SessionToken",
        "sessionToken",
        "SESSION_TOKEN",
        "AWS_SESSION_TOKEN",
        "Token",
        "token",
        "security_token",
    ] {
        for sep in ["=", ": ", " = ", ":"] {
            let raw = format!("{key}{sep}{t}\nError: boom\n");
            scrubbed_without(&format!("{key}{sep}"), &raw, std::slice::from_ref(&t));
        }
    }
}

#[test]
fn a_secret_on_the_line_after_its_key_is_redacted() {
    let (s, t) = (secret_key(), session_token());
    let raw = format!(
        "SecretAccessKey:\n  {s}\nSessionToken =\n{t}\n\"secretAccessKey\":\n\"{s}\"\nsessionToken:\n  \"{t}\"\nError: boom\n"
    );
    let out = scrubbed_without("value on the next line", &raw, &[s, t]);
    assert!(out.ends_with("Error: boom"), "{out}");
}

#[test]
fn yaml_and_ini_credentials_are_redacted() {
    let (k, s, t) = (key_id(), secret_key(), session_token());
    let raw = format!(
        "credentials:\n  accessKeyId: {k}\n  secretAccessKey: {s}\n  sessionToken: {t}\n[default]\naws_secret_access_key = {s}\naws_session_token = {t}\n"
    );
    scrubbed_without("yaml and ini", &raw, &[k, s, t]);
}

#[test]
fn an_env_dump_loses_every_aws_secret_and_keeps_the_rest() {
    let (k, s, t) = (key_id(), secret_key(), session_token());
    let raw = format!(
        "AWS_ACCESS_KEY_ID={k}\nAWS_SECRET_ACCESS_KEY={s}\nAWS_SESSION_TOKEN={t}\nAWS_REGION=us-east-1\nAWS_EC2_METADATA_DISABLED=true\nPWD=/var/lib/agentd/run/planner/home\n"
    );
    let out = scrubbed_without("env dump", &raw, &[k, s, t]);
    for kept in [
        "AWS_REGION=us-east-1",
        "AWS_EC2_METADATA_DISABLED=true",
        "PWD=/var/lib/agentd/run/planner/home",
    ] {
        assert!(out.contains(kept), "{kept} was lost: {out}");
    }
}

#[test]
fn a_bare_secret_key_alone_on_a_line_is_redacted() {
    let s = secret_key();
    let raw = format!("something failed\n{s}\nError: boom\n");
    let out = scrubbed_without("bare key on its own line", &raw, &[s]);
    assert!(out.contains("<redacted>"), "{out}");
    assert!(out.ends_with("Error: boom"), "{out}");
}

#[test]
fn a_bare_secret_key_inside_a_line_is_redacted() {
    let (s, t) = (secret_key(), session_token());
    let raw = format!(
        "helper returned {s} and then exited\n(response {t})\nvalue={s};\n\"{s}\"\nError: boom\n"
    );
    let out = scrubbed_without("bare key inside a line", &raw, &[s, t]);
    assert!(out.contains("helper returned"), "{out}");
    assert!(out.contains("and then exited"), "{out}");
}

#[test]
fn a_line_that_is_only_base64_text_is_never_shown() {
    // Single-case text that no other rule takes (not hex, no upper case for the opaque-token rule, no `/` for the word rule):
    // only the base64-only-line rule redacts it, so the test fails without that rule (the review's mutation M06).
    let body = "k3j4h5g6f7d8s9a0q1w2e3r4t5y6u7i8o9p0z1x2c3v4b5n6m7";
    let raw = format!("noise\n{body}\nError: boom\n");
    let out = scrub(raw.as_bytes(), false).joined();
    assert!(!out.contains("k3j4h5g6f7"), "{out}");
    assert!(!out.contains("v4b5n6m7"), "{out}");
    assert!(out.contains("noise | <redacted> | Error: boom"), "{out}");
    // A line with spaces is a sentence, not a blob.
    let sentence = scrub(
        b"this sentence has many ordinary words and is more than forty characters long\n",
        false,
    )
    .joined();
    assert!(sentence.contains("ordinary words"), "{sentence}");
}

#[test]
fn http_credentials_in_headers_are_redacted_to_the_end_of_the_header() {
    let (k, t) = (key_id(), session_token());
    let sig = "0123456789abcdef".repeat(4);
    let cookie_value = secret_word("cookie");
    let api = secret_word("apikey");
    let basic = ["dXNlcjpw", "YXNzd29yZA=="].concat();
    let digest = secret_word("digest");
    let raw = format!(
        "> Authorization: AWS4-HMAC-SHA256 Credential={k}/20261009/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature={sig}\n\
         > X-Amz-Security-Token: {t}\n\
         > Cookie: sid={cookie_value}; theme=dark\n\
         < Set-Cookie: sid={cookie_value}; Path=/; HttpOnly\n\
         > x-api-key: {api}\n\
         > Proxy-Authorization: Basic {basic}\n\
         > Authorization: Digest username=\"bob\", realm=\"r\", response=\"{digest}\"\n\
         < HTTP/1.1 403 Forbidden\n"
    );
    let out = scrubbed_without(
        "headers",
        &raw,
        &[k, t, sig, cookie_value, api, basic, digest],
    );
    assert!(out.contains("HTTP/1.1 403 Forbidden"), "{out}");
}

#[test]
fn a_secret_in_a_command_line_is_redacted() {
    let (t, word) = (session_token(), secret_word("flag"));
    let other = secret_word("other");
    let user_pass = secret_word("userpass");
    let bearer = secret_word("bearer");
    let eq_form = secret_word("eqform");
    let raw = format!(
        "spawn failed: tool --password {word} --token {t} --api-key {other} -u admin:{user_pass} --user bob:{user_pass} --client-secret={eq_form} -H 'Authorization: Bearer {bearer}' --verbose\n"
    );
    let out = scrubbed_without(
        "command line",
        &raw,
        &[word, t, other, user_pass, bearer, eq_form],
    );
    assert!(out.contains("spawn failed: tool"), "{out}");
    assert!(out.contains("--verbose"), "later flags stay: {out}");
}

#[test]
fn a_url_with_a_user_only_or_a_user_and_secret_is_redacted_and_a_plain_host_stays() {
    let token_user = secret_word("urluser");
    let word = secret_word("urlpw");
    let sig = "0123456789abcdef".repeat(2);
    let raw = format!(
        "clone https://{token_user}@github.example.com/org/repo.git\n\
         clone https://aws:{word}@registry.example.com/pypi/simple/\n\
         clone ssh://git@github.example.com/org/repo.git\n\
         GET https://bucket.s3.amazonaws.com/obj?X-Amz-Signature={sig}\n"
    );
    let out = scrubbed_without("urls", &raw, &[token_user, word, sig]);
    assert!(
        out.contains("ssh://git@github.example.com/org/repo.git"),
        "{out}"
    );
    assert!(out.contains("https://bucket.s3.amazonaws.com/obj"), "{out}");
    assert!(out.contains("github.example.com/org/repo.git"), "{out}");
}

#[test]
fn registry_docker_netrc_maven_and_java_property_credentials_are_redacted() {
    let npm = secret_word("npm");
    let npm_auth = secret_word("npmauth");
    let docker = ["ZG9ja2Vy", "dXNlcjpwYXNz", "d29yZDEyMw=="].concat();
    let netrc = secret_word("netrc");
    let maven = secret_word("maven");
    let java = secret_word("java");
    let raw = format!(
        "//registry.example.com/:_authToken={npm}\n\
         //registry.example.com/:_auth={npm_auth}\n\
         {{\"auths\":{{\"registry.example.com\":{{\"auth\":\"{docker}\"}}}}}}\n\
         machine registry.example.com login deploy password {netrc}\n\
         <server><id>central</id><password>{maven}</password></server>\n\
         java -Djavax.net.ssl.keyStorePassword={java} -jar app.jar\n\
         Error: boom\n"
    );
    let out = scrubbed_without(
        "registry, docker, netrc, maven, java",
        &raw,
        &[npm, npm_auth, docker, netrc, maven, java],
    );
    assert!(out.contains("registry.example.com"), "{out}");
    assert!(out.contains("-jar app.jar"), "{out}");
}

#[test]
fn the_orchestrators_one_line_never_starts_with_a_piece_of_a_secret() {
    // The credential JSON is the only line, so it is the headline: its first 150 characters leave the VM.
    let (k, s, t) = (key_id(), secret_key(), session_token());
    let raw = format!(
        "{{\"AccessKeyId\":\"{k}\",\"SecretAccessKey\":\"{s}\",\"SessionToken\":\"{t}\"}}\n"
    );
    let tail = scrub(raw.as_bytes(), false);
    let body = error_body("exit status 1", &tail);
    assert!(body.chars().count() <= ERROR_BODY_CHARS, "{body}");
    for secret in [&k, &s, &t] {
        assert_eq!(leaked(&body, secret), None, "{body}");
    }
    assert!(body.contains("SecretAccessKey"), "{body}");
}

#[test]
fn paths_stack_frames_versions_and_ordinary_words_that_hold_a_secret_word_stay_readable() {
    for line in [
        "Error: EACCES: permission denied, mkdir '/var/lib/agentd/01M4G51DV50AEMK86GY37KPPW3/planner/home/.cline'",
        "Error [ERR_MODULE_NOT_FOUND]: Cannot find module '/opt/cline/node_modules/@cline/core/dist/index.js'",
        "    at Object.<anonymous> (/home/user/project/src/index.ts:12:5)",
        "Error: cannot read src/main/java/com/example/demo/controller/UserController.java",
        "npm ERR! code ELIFECYCLE",
        "Node.js v20.18.1",
        "go: downloading github.com/aws/aws-sdk-go-v2 v1.32.2",
        "error: token expired, run `ham login` again",
        "ExpiredToken: The provided token has expired.",
        "at Object.<anonymous> (/srv/app/lib/token.js:12:5)",
        "at com.example.auth.TokenService.refresh(TokenService.java:42)",
        "--> src/auth/secret.rs:10:5",
        "credentials.json:3",
        "thread 'main' panicked at crates/agentd/src/token.rs:12:5:",
        "max_tokens: 200000 > 64000, which is the maximum allowed number of output tokens for the model",
        "\"inputTokens\": 10042,",
        "budget tokens=20000",
        "SignatureDoesNotMatch: The request signature we calculated does not match the signature you provided.",
        "JsonWebTokenError: invalid signature",
        "{\"token_type\":\"Bearer\",\"expires_in\":3600}",
        "thread 'main' panicked at crates/agentd/src/turn.rs:1262:9",
        "PWD=/var/lib/agentd/run-e2e-1/a1/home",
    ] {
        let out = scrub(format!("{line}\n").as_bytes(), false).joined();
        assert_eq!(out, line.trim(), "the scrub changed {line:?}");
    }
}

#[test]
fn a_url_whose_user_name_holds_a_secret_word_keeps_its_host_and_path() {
    let token = ["ghs", "_abcdefghijklmnop0123456789"].concat();
    let raw = format!("git clone https://x-access-token:{token}@github.example.com/org/repo.git\n");
    let out = scrub(raw.as_bytes(), false).joined();
    assert!(!out.contains(&token), "{out}");
    assert!(
        out.ends_with("@github.example.com/org/repo.git"),
        "the host and path stay: {out}"
    );
}

#[test]
fn a_secret_named_in_words_is_redacted() {
    let (a, b, c) = (
        secret_word("spoken1"),
        secret_word("spoken2"),
        secret_word("spoken3"),
    );
    let raw = format!("Secret access key: {a}\nAPI key = {b}\nsession token: {c}\nError: boom\n");
    let out = scrubbed_without("named in words", &raw, &[a, b, c]);
    assert!(out.ends_with("Error: boom"), "{out}");
}

#[test]
fn a_json_header_with_a_quoted_value_loses_the_value_and_keeps_the_next_pair() {
    let v = secret_word("jsonheader");
    let raw = format!(
        "{{\"Authorization\":\"Bearer {v}\",\"Accept\":\"application/json\"}}\nError: boom\n"
    );
    let out = scrubbed_without("json header", &raw, std::slice::from_ref(&v));
    assert!(out.contains("\"Accept\":\"application/json\""), "{out}");
    assert!(out.ends_with("Error: boom"), "{out}");
}

// ---- Text the orchestrator adds to an error that already holds the diagnostic (review of e44c5417, M3) ----------------------

#[test]
fn with_note_puts_the_note_before_the_diagnostic_so_strip_keeps_it() {
    let text = append(BASE, "exit status 3; stderr: Error: EACCES");
    let noted = with_note(&text, "it changed src/a.py outside the write globs");
    assert_eq!(
        noted,
        "the engine closed its output; it changed src/a.py outside the write globs [engine process: exit status 3; stderr: Error: EACCES]"
    );
    assert_eq!(
        strip(&noted),
        "the engine closed its output; it changed src/a.py outside the write globs"
    );
    assert_eq!(
        stable(&noted),
        "the engine closed its output; it changed src/a.py outside the write globs [engine process: exit status 3]"
    );
}

#[test]
fn with_note_on_a_text_without_a_diagnostic_appends_it_as_before() {
    assert_eq!(
        with_note(BASE, "it changed x"),
        "the engine closed its output; it changed x"
    );
    assert_eq!(
        with_note("turn exceeded 60 s", "it changed x"),
        "turn exceeded 60 s; it changed x"
    );
    // A text that is not one of agentd's two transport texts keeps its separator-shaped words: they are not a diagnostic.
    let foreign = "engine error -32603: Internal error [engine process: exit status 1; stderr: x]";
    assert_eq!(
        with_note(foreign, "it changed x"),
        format!("{foreign}; it changed x")
    );
}

#[test]
fn split_gives_the_text_before_and_the_diagnostic_after() {
    let text = append(BASE, "exit status 3; stderr empty");
    assert_eq!(
        split(&text),
        (
            "the engine closed its output",
            Some("[engine process: exit status 3; stderr empty]")
        )
    );
    let noted = with_note(&text, "it changed x");
    assert_eq!(
        split(&noted),
        (
            "the engine closed its output; it changed x",
            Some("[engine process: exit status 3; stderr empty]")
        )
    );
    assert_eq!(split(BASE), (BASE, None));
    assert_eq!(split("turn exceeded 60 s"), ("turn exceeded 60 s", None));
    let foreign = "engine error -32603: Internal error [engine process: exit status 1]";
    assert_eq!(split(foreign), (foreign, None));
}

#[test]
fn a_separator_inside_the_note_cannot_cut_the_note_short() {
    // The note holds paths the engine chose. A file named with the separator must not make `strip` stop inside the note, and the
    // real diagnostic stays the last part (Claude Opus 5.5 and GPT-6.1 Sol on the M3 design, 2026-10-09).
    let text = append(BASE, "exit status 3; stderr empty");
    let odd_note =
        format!("it changed a{SEPARATOR}b.py outside the write globs; the harness restored them");
    let noted = with_note(&text, &odd_note);
    let kept = strip(&noted);
    assert!(
        kept.contains("outside the write globs; the harness restored them"),
        "{kept}"
    );
    assert!(!kept.contains(SEPARATOR), "{kept}");
    assert_eq!(
        split(&noted).1,
        Some("[engine process: exit status 3; stderr empty]")
    );
}

// ---- Scrub gaps found by the second review ------------------------------------
//
// Every stand-in is built at run time, and a secret is short on purpose (under 40 characters, not base64-only, no `/`): only the
// rule under test can take it.

// S1: `authorization=Bearer <token>`.

#[test]
fn an_authorization_header_with_an_equals_sign_loses_its_token_whatever_the_scheme() {
    for header in [
        "authorization",
        "Authorization",
        "AUTHORIZATION",
        "proxy-authorization",
        "Proxy-Authorization",
        "proxy_authorization",
    ] {
        for scheme in ["Bearer", "Basic", "bearer", "BASIC"] {
            for sep in ["=", " = ", ": ", ":"] {
                let token = secret_word("hdr");
                let raw = format!("{header}{sep}{scheme} {token}\nError: boom\n");
                let out = scrubbed_without(&format!("{header}{sep}{scheme}"), &raw, &[token]);
                assert!(out.contains("<redacted>"), "{header}{sep}{scheme}: {out}");
                assert!(out.ends_with("Error: boom"), "{out}");
            }
        }
    }
}

#[test]
fn a_basic_credential_after_an_equals_sign_and_a_following_pair_on_the_line() {
    let b64 = ["dXNlcjpw", "YXNzd29yZA=="].concat();
    let raw = format!("curl authorization=Basic {b64}&accept=json\n");
    let out = scrubbed_without("basic then a pair", &raw, std::slice::from_ref(&b64));
    assert!(out.contains("accept=json"), "the next pair stays: {out}");
}

#[test]
fn an_authorization_scheme_word_with_nothing_after_it_is_the_value() {
    // No credential follows the scheme word: the word is the value, as it always was.
    let out = scrub(b"authorization=Bearer\n", false).joined();
    assert_eq!(out, "authorization=<redacted>");
}

#[test]
fn every_http_authentication_scheme_word_after_a_secret_key_is_not_the_value() {
    for scheme in ["Digest", "Negotiate", "NTLM", "digest", "negotiate", "ntlm"] {
        for key in [
            "authorization",
            "proxy_authorization",
            "auth_token",
            "api_key",
        ] {
            let token = secret_word("scheme");
            let raw = format!("{key}={scheme} {token}\nError: boom\n");
            let out = scrubbed_without(&format!("{key}={scheme}"), &raw, &[token]);
            assert!(out.ends_with("Error: boom"), "{out}");
        }
    }
}

// S2: URL user information.

#[test]
fn a_url_with_an_empty_user_and_a_password_loses_the_password() {
    for scheme in ["redis", "rediss", "amqp", "https", "postgres", "git+https"] {
        let pw = secret_word("empty");
        let raw =
            format!("connect {scheme}://:{pw}@cache.example.com:6379/0 failed\nError: boom\n");
        let out = scrubbed_without(scheme, &raw, &[pw]);
        assert!(
            out.contains(&format!(
                "{scheme}://<redacted>@cache.example.com:6379/0 failed"
            )),
            "the scheme, host, port and path stay: {out}"
        );
        assert!(out.ends_with("Error: boom"), "{out}");
    }
}

#[test]
fn a_url_password_with_an_at_sign_is_redacted_up_to_the_last_at_sign() {
    let (a, b) = (secret_word("urla"), secret_word("urlb"));
    for (raw_url, kept) in [
        (
            format!("https://user:{a}@{b}@example.com/x"),
            "https://<redacted>@example.com/x",
        ),
        (
            format!("redis://:{a}@{b}@cache:6379/0"),
            "redis://<redacted>@cache:6379/0",
        ),
        // A path after the host that holds an at sign (an npm scope) is not part of the user information.
        (
            format!("https://user:{a}@{b}@example.com/@scope/pkg"),
            "https://<redacted>@example.com/@scope/pkg",
        ),
        // Percent-encoded: the encoded at sign and slash are ordinary password characters.
        (
            format!("https://user:{a}%40{b}%2Fend@example.com/x"),
            "https://<redacted>@example.com/x",
        ),
    ] {
        let raw = format!("fetch {raw_url} failed\nError: boom\n");
        let out = scrubbed_without(&raw_url, &raw, &[a.clone(), b.clone()]);
        assert!(out.contains(kept), "{kept} is missing: {out}");
    }
}

#[test]
fn a_url_password_with_a_slash_is_redacted_up_to_the_at_sign_before_the_host() {
    let (a, b, c) = (
        secret_word("urla"),
        secret_word("urlb"),
        secret_word("urlc"),
    );
    for (raw_url, kept) in [
        (
            format!("https://user:{a}/{b}@example.com/x"),
            "https://<redacted>@example.com/x",
        ),
        (
            format!("redis://:{a}/{b}@cache:6379/0"),
            "redis://<redacted>@cache:6379/0",
        ),
        (
            format!("https://user:{a}/{b}/{c}@example.com/x"),
            "https://<redacted>@example.com/x",
        ),
        // Both an at sign and a slash in the password.
        (
            format!("https://user:{a}/{b}@{c}@example.com/x"),
            "https://<redacted>@example.com/x",
        ),
    ] {
        let raw = format!("fetch {raw_url} failed\nError: boom\n");
        let out = scrubbed_without(&raw_url, &raw, &[a.clone(), b.clone(), c.clone()]);
        assert!(out.contains(kept), "{kept} is missing: {out}");
    }
}

#[test]
fn urls_with_no_password_and_remotes_and_addresses_stay_readable() {
    for line in [
        "git@github.com:org/repo.git",
        "clone ssh://git@github.example.com/org/repo.git",
        "ssh://deploy@build.example.com/srv/app",
        "https://bob@example.com/dashboard",
        "mail bob@example.com for access",
        "mailto:bob@example.com",
        "https://registry.npmjs.org/@babel/core/-/core-7.0.0.tgz",
        "GET http://localhost:5173/@vite/client 200",
        "GET http://localhost:3000/@fs/home/x/y.ts",
        "https://host.example.com:8443/a/@scope/pkg",
        "git+https://github.com/org/repo.git@v1.2.3#egg=x",
        "http://[::1]:8080/@vite/client",
        "fetch s3://bucket/path/key@x",
        "jdbc:postgresql://localhost:5432/db",
        "listening on http://0.0.0.0:8080/",
    ] {
        let out = scrub(format!("{line}\n").as_bytes(), false).joined();
        assert_eq!(out, line, "the scrub changed {line:?}");
    }
}

#[test]
fn a_token_in_the_user_place_of_a_url_is_still_redacted() {
    let token = secret_word("urluser");
    let raw = format!("clone https://{token}@github.example.com/org/repo.git\n");
    let out = scrubbed_without("token as the user", &raw, &[token]);
    assert!(
        out.ends_with("https://<redacted>@github.example.com/org/repo.git"),
        "{out}"
    );
}

#[test]
fn only_a_plain_account_name_stays_in_the_user_place_of_a_url() {
    let at_the_cap = "z".repeat(24);
    for line in [
        "clone https://git@host.example.com/org/repo.git".to_string(),
        "clone https://x-access-token@host.example.com/org/repo.git".to_string(),
        "clone https://deploy@host.example.com/org/repo.git".to_string(),
        format!("clone https://{at_the_cap}@host.example.com/org/repo.git"),
    ] {
        let out = scrub(format!("{line}\n").as_bytes(), false).joined();
        assert_eq!(out, line, "a plain account name was redacted: {line:?}");
    }
    // Not hex: a run of 32 or more hex digits goes as a digest whatever sits around it.
    let long = "zulu".repeat(10);
    let just_over = "z".repeat(25);
    for user in [
        "Deploy",
        "deploy_bot",
        "deploy1",
        "a1",
        just_over.as_str(),
        long.as_str(),
    ] {
        let raw = format!("clone https://{user}@host.example.com/org/repo.git\n");
        let out = scrub(raw.as_bytes(), false).joined();
        assert_eq!(
            out, "clone https://<redacted>@host.example.com/org/repo.git",
            "{user} is not a plain account name"
        );
    }
}

#[test]
fn a_scheme_inside_a_password_does_not_start_a_second_url() {
    let (a, b) = (secret_word("inner"), secret_word("inner2"));
    let raw = format!("fetch https://user:{a}://{b}:x@y@example.com/x failed\n");
    let out = scrubbed_without("scheme in a password", &raw, &[a, b]);
    assert!(
        out.contains("https://<redacted>@example.com/x failed"),
        "{out}"
    );
}

#[test]
fn a_url_in_quotes_ends_at_its_quote_and_a_later_address_is_not_part_of_it() {
    let (a, b) = (secret_word("quoted"), secret_word("quoted2"));
    let raw = format!(
        "[\"https://user:{a}/{b}@example.com/x\",\"owner@example.org\"] ['https://user:{a}/{b}@example.net/y','other@example.org']\n"
    );
    let out = scrubbed_without("quoted urls", &raw, &[a, b]);
    assert!(
        out.contains("\"https://<redacted>@example.com/x\""),
        "{out}"
    );
    assert!(out.contains("\"owner@example.org\""), "{out}");
    assert!(out.contains("'https://<redacted>@example.net/y'"), "{out}");
    assert!(out.contains("'other@example.org'"), "{out}");
}

#[test]
fn a_password_in_a_url_with_an_ipv6_host_is_redacted_and_the_host_stays() {
    let pw = secret_word("ipv6");
    let raw = format!("connect http://user:{pw}@[::1]:8080/x failed\n");
    let out = scrubbed_without("ipv6 host", &raw, &[pw]);
    assert!(
        out.contains("http://<redacted>@[::1]:8080/x failed"),
        "{out}"
    );
}

// S3: keys that name a secret and that the word list missed.

/// The secret and the value of `key`: a webhook key holds a URL whose last path part is the secret, short on purpose (a path of
/// 40 characters that holds a `/` is taken whole by the base64-word rule).
fn secret_and_value_for(key: &str) -> (String, String) {
    if key.to_ascii_lowercase().contains("webhook") {
        let secret = ["Hunt", "er2wH9"].concat();
        let value = format!("https://hooks.slack.com/services/T000/B000/{secret}");
        (secret, value)
    } else {
        let secret = secret_word("fam");
        (secret.clone(), secret)
    }
}

#[test]
fn keys_of_every_secret_family_and_spelling_lose_their_value() {
    for key in [
        // Short words as a whole part of the key.
        "DB_PASS",
        "db_pass",
        "db-pass",
        "dbPass",
        "PASS",
        "pass",
        "MYSQL_PWD",
        "mysql_pwd",
        "userPwd",
        "pwd",
        "PWD_FILE_CONTENT",
        "DB_PW",
        "GITHUB_PAT",
        "PAT",
        "githubPat",
        "AUTH",
        "auth",
        "REDIS_AUTH",
        "BASIC_AUTH",
        "bearer",
        "BEARER",
        // A key that ends in `key`.
        "OPENAI_KEY",
        "openai_key",
        "openai-key",
        "openaiKey",
        "STRIPE_KEY",
        "ssh_key",
        // A webhook URL is a secret.
        "SLACK_WEBHOOK",
        "SLACK_WEBHOOK_URL",
        "discordWebhook",
        "webhook",
        // Families the word list already took, pinned with their spellings.
        "private_key",
        "PRIVATE_KEY",
        "privateKey",
        "private-key",
        "client_secret",
        "clientSecret",
        "client-secret",
        "CLIENT_SECRET",
        "api_key",
        "api-key",
        "apiKey",
        "APIKEY",
        "X-Api-Key",
        "passwd",
        "Passwd",
        "credentials",
        "auth_token",
        "authToken",
        "auth-token",
        "access_token",
        "id_token",
        "refresh_token",
        "secret_key",
        "SecretKey",
    ] {
        for sep in ["=", ": ", " = "] {
            let (secret, value) = secret_and_value_for(key);
            let raw = format!("{key}{sep}{value}\nError: boom\n");
            let out = scrubbed_without(&format!("{key}{sep}"), &raw, &[secret]);
            assert!(
                out.contains(&format!("{key}{sep}<redacted>")),
                "{key}{sep}: the key stays and the value goes: {out}"
            );
            assert!(out.ends_with("Error: boom"), "{out}");
        }
    }
}

#[test]
fn a_secret_named_key_in_an_object_a_json_member_or_an_export_line_loses_its_value() {
    let (a, b, c, d) = (
        secret_word("obja"),
        secret_word("objb"),
        secret_word("objc"),
        secret_word("objd"),
    );
    let raw = format!(
        "called with {{ user: 'bob', pass: '{a}', pwd: \"{b}\" }}\n\
         {{\"pass\":\"{c}\",\"auth\":\"{d}\",\"name\":\"bob\"}}\n"
    );
    let out = scrubbed_without("objects", &raw, &[a, b, c, d]);
    assert!(out.contains("user: 'bob'"), "{out}");
    assert!(out.contains("\"name\":\"bob\""), "{out}");
    let e = secret_word("export");
    let out = scrubbed_without(
        "export line",
        &format!("export DB_PASS={e}\nexport OPENAI_KEY={e}\n"),
        &[e],
    );
    assert!(out.contains("export DB_PASS=<redacted>"), "{out}");
}

#[test]
fn an_env_dump_with_the_missed_key_families_loses_those_and_keeps_the_rest() {
    let (k, s, t) = (key_id(), secret_key(), session_token());
    let words: Vec<String> = (0..8).map(|i| secret_word(&format!("env{i}"))).collect();
    let raw = format!(
        "PATH=/usr/bin:/bin\nHOME=/home/x\nPWD=/var/lib/agentd/run/planner/home\nOLDPWD=/home/x\nAWS_ACCESS_KEY_ID={k}\nAWS_SECRET_ACCESS_KEY={s}\nAWS_SESSION_TOKEN={t}\n\
         DB_PASS={}\nMYSQL_PWD={}\nPASS={}\nPWD_FILE_CONTENT={}\nREDIS_AUTH={}\nOPENAI_KEY={}\nPAT={}\nAUTH={}\nAWS_REGION=us-east-1\n",
        words[0], words[1], words[2], words[3], words[4], words[5], words[6], words[7]
    );
    let mut secrets = vec![k, s, t];
    secrets.extend(words);
    let out = scrubbed_without("env dump", &raw, &secrets);
    for kept in [
        "PATH=/usr/bin:/bin",
        "HOME=/home/x",
        "PWD=/var/lib/agentd/run/planner/home",
        "OLDPWD=/home/x",
        "AWS_REGION=us-east-1",
    ] {
        assert!(out.contains(kept), "{kept} was lost: {out}");
    }
}

#[test]
fn ordinary_words_and_keys_that_only_contain_a_short_secret_word_stay_readable() {
    for line in [
        "pass: 12",
        "pass=true",
        "auth: false",
        "auth=none",
        "PASS src/auth/login.test.ts",
        "Tests: 3 passed, 3 total",
        "compass: north",
        "bypass=true",
        "path=/usr/bin:/bin",
        "author: Jane Smith",
        "authority=example.com:443",
        "patch: applied",
        "auth-service:8080",
        "auth.example.com:443",
        "auth.example.com: connection refused",
        "https://auth.example.com:8443/oauth2/authorize",
        "at login (/app/src/auth.js:10:5)",
        "--> src/pass.rs:12:5",
        "primary_key: id",
        "primaryKey: true",
        "sort_key=created_at",
        "cache_key=users:42",
        "partitionKey: 'tenant'",
        "key: value",
        "monkey: banana",
        "keyboard=us",
        "OLDPWD=/home/x",
        "pass_through=1",
        "auth_method=oidc",
        "auth_url=https://auth.example.com/authorize",
        "webhook=true",
    ] {
        let out = scrub(format!("{line}\n").as_bytes(), false).joined();
        assert_eq!(out, line, "the scrub changed {line:?}");
    }
}

#[test]
fn camel_case_and_acronym_and_digit_keys_are_split_into_their_parts() {
    for key in [
        "SSHKey",
        "DBPass",
        "s3Key",
        "v2Pass",
        "myAppAuth",
        "apiGatewayPat",
        "stripeKey",
    ] {
        let secret = secret_word("camel");
        let raw = format!("{key}={secret}\nError: boom\n");
        let out = scrubbed_without(key, &raw, &[secret]);
        assert!(out.contains(&format!("{key}=<redacted>")), "{key}: {out}");
    }
}

#[test]
fn a_key_that_ends_in_key_but_says_what_it_is_for_stays_readable() {
    for kind in [
        "primary",
        "foreign",
        "sort",
        "partition",
        "hash",
        "cache",
        "group",
        "order",
        "unique",
        "composite",
        "idempotency",
        "dedup",
        "object",
        "map",
        "row",
        "item",
        "search",
        "lookup",
        "shard",
        "routing",
        "join",
        "natural",
        "surrogate",
        "range",
    ] {
        for line in [
            format!("{kind}_key: customer-1234"),
            format!("{kind}Key=customer-1234"),
            format!("{}-key = customer-1234", kind.to_uppercase()),
        ] {
            let out = scrub(format!("{line}\n").as_bytes(), false).joined();
            assert_eq!(out, line, "the scrub changed {line:?}");
        }
    }
}

#[test]
fn a_short_secret_word_followed_by_a_kind_or_a_state_stays_readable() {
    for kind in [
        "type", "method", "mode", "scheme", "provider", "enabled", "disabled", "required",
        "header", "headers", "realm", "flow", "status", "strategy", "timeout", "through", "count",
        "retries", "attempts", "failed", "error", "errors",
    ] {
        for word in ["auth", "pass", "pwd", "pat", "bearer", "webhook"] {
            let line = format!("{word}_{kind}=customer-1234");
            let out = scrub(format!("{line}\n").as_bytes(), false).joined();
            assert_eq!(out, line, "the scrub changed {line:?}");
        }
    }
}

#[test]
fn a_plain_value_of_a_short_secret_word_key_stays_readable() {
    // Status words and an opener of a structure, for every short-word key.
    for value in [
        "true",
        "false",
        "null",
        "none",
        "nil",
        "undefined",
        "yes",
        "no",
        "on",
        "off",
        "ok",
        "fail",
        "failed",
        "success",
        "denied",
        "required",
        "disabled",
        "enabled",
        "TRUE",
        "None",
        "''",
        "\"\"",
        "{",
        "[",
        "(",
        "{}",
        "[]",
        "()",
    ] {
        for key in [
            "pass",
            "DB_PASS",
            "auth",
            "GITHUB_PAT",
            "MYSQL_PWD",
            "bearer",
            "openai_key",
            "slack_webhook",
        ] {
            let line = format!("{key}={value}");
            let out = scrub(format!("{line}\n").as_bytes(), false).joined();
            assert_eq!(out, line, "the scrub changed {line:?}");
        }
    }
    // A host and its port, with the colon against the number, for every short-word key (a JSON member is not one).
    for line in [
        "auth-service:8080",
        "auth.example.com:443",
        "auth.example.com:8443/path",
        "pass.example.com:5432",
    ] {
        let out = scrub(format!("{line}\n").as_bytes(), false).joined();
        assert_eq!(out, line, "the scrub changed {line:?}");
    }
    // A counter under the log-style key `pass`.
    for line in ["pass: 12", "pass=0", "pass: 7 fail: 0"] {
        let out = scrub(format!("{line}\n").as_bytes(), false).joined();
        assert_eq!(out, line, "the scrub changed {line:?}");
    }
}

#[test]
fn a_short_number_under_any_other_short_word_key_may_be_a_pin_and_is_redacted() {
    // Only the key `pass` and a host:port form are counters. Names that the password scan reads (DB_PASS, MYSQL_PWD) are put
    // together at run time.
    let lines: Vec<String> = vec![
        "pw: 1234".into(),
        ["DB", "_PASS", "=12345"].concat(),
        "PASS=1234".into(),
        "Pass=1234".into(),
        "auth: 12345".into(),
        ["MYSQL", "_PWD", "=0"].concat(),
        "GITHUB_PAT=7".into(),
        "openai_key=123".into(),
    ];
    for line in lines {
        let out = scrub(format!("{line}\n").as_bytes(), false).joined();
        let (key, _) = line.split_once(['=', ':']).unwrap();
        assert!(
            out.starts_with(key) && out.ends_with("<redacted>"),
            "{line:?} became {out:?}"
        );
        assert!(!out.contains(|c: char| c.is_ascii_digit()), "{out}");
    }
    // Digits followed by other characters can be a credential too.
    for value in ["123456", "1234567890", "12a45", "443abc"] {
        let out = scrub(format!("DB_PASS={value}\n").as_bytes(), false).joined();
        assert_eq!(out, "DB_PASS=<redacted>", "{value}");
    }
}

#[test]
fn a_bracketed_word_is_a_value_and_only_a_bare_opener_is_a_structure() {
    for word in ["[summer2026]", "{admin}", "(hunter2)", "[a]", "{{x}}"] {
        for key in ["pwd", "pass", "auth", "DB_PASS"] {
            let out = scrub(format!("{key}: {word}\n").as_bytes(), false).joined();
            assert_eq!(out, format!("{key}: <redacted>"), "{key}: {word}");
        }
    }
}

#[test]
fn pwd_with_a_path_is_the_working_directory_and_any_other_value_is_a_password() {
    // Only the environment variable `PWD` holds a path: a key of any other spelling that starts with a slash is a password.
    for line in [
        "PWD=/var/lib/agentd/run/planner/home",
        "PWD=/",
        "PWD=/summer",
        "\"PWD\":\"/srv/app\"",
    ] {
        let out = scrub(format!("{line}\n").as_bytes(), false).joined();
        assert_eq!(out, line, "the scrub changed {line:?}");
    }
    let secret = secret_word("pwd");
    let out = scrubbed_without("pwd value", &format!("PWD={secret}\n"), &[secret]);
    assert_eq!(out, "PWD=<redacted>");
    let slashed = ["/Hunt", "er2sl9"].concat();
    for key in ["pwd", "Pwd", "MYSQL_PWD", "DB_PASS"] {
        let out = scrubbed_without(
            "slash password",
            &format!("{key}={slashed}\n"),
            std::slice::from_ref(&slashed),
        );
        assert_eq!(out, format!("{key}=<redacted>"));
    }
    let home = "~/work";
    let out = scrub(format!("pwd: {home}\n").as_bytes(), false).joined();
    assert_eq!(out, "pwd: <redacted>");
}

#[test]
fn a_short_secret_word_key_at_the_end_of_a_line_does_not_take_the_next_line() {
    let raw =
        "services:\n  api:\n    auth:\n      user: bob\n    pass:\n    key_file: x\nError: boom\n";
    let out = scrub(raw.as_bytes(), false).joined();
    assert_eq!(
        out,
        "services: | api: | auth: | user: bob | pass: | key_file: x | Error: boom"
    );
}

#[test]
fn a_webhook_url_is_the_credential_and_a_webhook_id_is_not() {
    let secret = ["Hunt", "er2wH9"].concat();
    let raw = format!(
        "WEBHOOK_URL=https://hooks.slack.com/services/T000/B000/{secret}\nwebhook_id=1234567890\n"
    );
    let out = scrubbed_without("webhook url", &raw, &[secret]);
    assert!(out.contains("WEBHOOK_URL=<redacted>"), "{out}");
    assert!(out.contains("webhook_id=1234567890"), "{out}");
}

// S4: invisible and format characters.

/// Text as Unicode tag characters (U+E0000 plus the ASCII code): invisible to a person, readable to a model.
fn tag_text(text: &str) -> String {
    text.chars()
        .map(|c| char::from_u32(0xE0000 + c as u32).unwrap())
        .collect()
}

#[test]
fn text_hidden_in_unicode_tag_characters_is_removed_from_the_tail_the_body_and_the_log() {
    let payload = tag_text("hidden note for the reader");
    assert!(
        payload
            .chars()
            .all(|c| ('\u{e0000}'..='\u{e007f}').contains(&c))
    );
    let raw = format!("Error: boom{payload}\nNode.js v20\n{payload}\n");
    let tail = scrub(raw.as_bytes(), false);
    assert_eq!(tail.joined(), "Error: boom | Node.js v20");
    let body = error_body("exit status 1", &tail);
    let log = log_body("exit status 1", &tail);
    for (where_, text) in [("tail", tail.joined()), ("body", body), ("log", log)] {
        assert!(
            !text
                .chars()
                .any(|c| ('\u{e0000}'..='\u{e0fff}').contains(&c)),
            "a tag character survived in the {where_}: {text:?}"
        );
    }
}

/// The format characters (Cf), the default-ignorable code points, the variation selectors, the tag characters and the fillers:
/// each range shows nothing, so it can hide text or split a word a rule looks for.
const INVISIBLE_RANGES: [(u32, u32); 26] = [
    (0x00AD, 0x00AD),
    (0x034F, 0x034F),
    (0x0600, 0x0605),
    (0x061C, 0x061C),
    (0x06DD, 0x06DD),
    (0x070F, 0x070F),
    (0x0890, 0x0891),
    (0x08E2, 0x08E2),
    (0x115F, 0x1160),
    (0x17B4, 0x17B5),
    (0x180B, 0x180F),
    (0x200B, 0x200F),
    (0x202A, 0x202E),
    (0x2060, 0x206F),
    (0x2800, 0x2800),
    (0x3164, 0x3164),
    (0xFE00, 0xFE0F),
    (0xFEFF, 0xFEFF),
    (0xFFA0, 0xFFA0),
    (0xFFF0, 0xFFFB),
    (0x110BD, 0x110BD),
    (0x110CD, 0x110CD),
    (0x13430, 0x1343F),
    (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A),
    (0xE0000, 0xE0FFF),
];

#[test]
fn every_invisible_format_tag_variation_selector_and_filler_character_is_removed() {
    for (first, last) in INVISIBLE_RANGES {
        // 200 characters per line keep the line inside the window (the tag block alone is 16 KB).
        let codes: Vec<u32> = (first..=last).collect();
        for chunk in codes.chunks(200) {
            let run: String = chunk.iter().map(|&u| char::from_u32(u).unwrap()).collect();
            let raw = format!("a{run}b\n");
            let out = scrub(raw.as_bytes(), false).joined();
            assert_eq!(
                out,
                "ab",
                "U+{:04X}..U+{:04X} survived: {out:?}",
                chunk[0],
                chunk[chunk.len() - 1]
            );
        }
    }
}

#[test]
fn visible_characters_next_to_the_invisible_ranges_and_other_scripts_stay() {
    for line in [
        "café ñ Ω 錯誤 🙂 ✓ — “quoted”",
        "\u{00ac}\u{00ae}\u{0606}\u{2070}\u{2801}\u{3163}\u{3165}\u{fe10}\u{ffa1}\u{fffc}\u{fffd}\u{10000}\u{1f642}",
    ] {
        let out = scrub(format!("{line}\n").as_bytes(), false).joined();
        assert_eq!(out, line, "the scrub changed {line:?}");
    }
}

#[test]
fn an_invisible_character_cannot_split_a_secret_key_or_its_value() {
    let (a, b) = (secret_word("hidea"), secret_word("hideb"));
    let hidden = tag_text("x");
    let raw = format!(
        "Secret{hidden}AccessKey={a}\npass\u{fe0f}word: {b}\nAuthor\u{3164}ization: Bearer {a}\n"
    );
    let out = scrubbed_without("hidden characters in keys", &raw, &[a, b]);
    assert!(out.contains("<redacted>"), "{out}");
}

// S6: a long relative path is a path, not a secret.

#[test]
fn a_long_path_with_upper_case_lower_case_and_a_digit_stays_readable() {
    for line in [
        // A file extension follows.
        "cannot find src/main/java/com/example/demo/UserController1.java",
        "at src/main/java/com/example/demo/service/OrderService2.java:42",
        "src/UserProfileDetailsComponentFactory2/Index.tsx failed",
        "src/UserProfileDetailsComponentFactory2/Index.c failed",
        "src/UserProfileDetailsComponentFactory2/Index.java failed",
        "src/UserProfileDetailsComponentFactory2/application.properties failed",
        // Three slashes or more, no extension.
        "Error: cannot read app/Http/Controllers/V2/Auth/LoginController",
        "src/main/UserProfileDetailsComponentFactory2/Index",
        // An absolute path whose first directory is not a lower-case name of up to 12 characters.
        "/Users/testuser/Documents/Projects/MyApp2/Source/Models/UserProfile.swift",
        // A path alone on its line.
        "src/main/resources/templates/fragments/Header2",
    ] {
        let out = scrub(format!("{line}\n").as_bytes(), false).joined();
        assert_eq!(out, line, "the scrub changed {line:?}");
    }
}

#[test]
fn a_base64_secret_with_slashes_is_still_redacted_where_a_path_would_not_be() {
    // 4 slashes and a mix of cases and digits, but only about a third of its letters and digits are lower case.
    let low = "aB3dE/fG7hJ/kL9mN/pQ2rS/tUvWxYz0123456789AbCdEf";
    // 4 slashes and 59% lower case: just under the share a path has (65%).
    let near = "abcdefghijkl/mnopqrstuvwx/yzABCD/EFGH12/IJKLMN12";
    // One extension and 2 slashes, the shape of the AWS secret key.
    let aws = secret_key();
    for secret in [low, near, aws.as_str()] {
        for raw in [
            format!("{secret}\n"),
            format!("key {secret} end\n"),
            format!("see {secret}.pem now\n"),
        ] {
            let out = scrubbed_without("path-shaped secret", &raw, &[secret.to_string()]);
            assert!(out.contains("<redacted>"), "{out}");
        }
    }
}

#[test]
fn a_long_word_with_a_plus_sign_or_a_tail_that_is_no_extension_is_not_taken_for_a_path() {
    // A plus sign is base64, not a path character.
    let plus = "src/main/java/com/example/demo/service/Foo+Bar1";
    let out = scrub(format!("{plus}\n").as_bytes(), false).joined();
    assert_eq!(out, "<redacted>");
    // Two slashes and a dot followed by 13 letters: no file extension.
    let word = "src/UserProfileDetailsComponentFactory2/Index";
    let out = scrub(format!("{word}.abcdefghijklm\n").as_bytes(), false).joined();
    assert_eq!(out, "<redacted>.abcdefghijklm");
    // The same word with 2 slashes and no extension at all.
    let out = scrub(format!("{word}\n").as_bytes(), false).joined();
    assert_eq!(out, "<redacted>");
    // A line of base64 characters with 2 slashes and no extension is a blob, also when it is all lower case.
    let blob = "very/longdirectorynamewithoutanyseparatorsatall/file";
    let out = scrub(format!("{blob}\n").as_bytes(), false).joined();
    assert_eq!(out, "<redacted>");
}

// S5: the key rule takes a value that starts on the line after its key, also when the value is short.

#[test]
fn a_short_secret_on_the_line_after_its_key_is_redacted() {
    // Each value is 17 characters: under 32 letters and digits, so the base64-only-line rule does not take it, and under 40
    // characters, so neither does the word rule. Only the key rule's newline after the separator can (the review's mutation X1:
    // `\s*` changed to `[ \t]*` passed every test when this property was pinned with 40-character values only).
    let words: Vec<String> = (0..5).map(|i| secret_word(&format!("nl{i}"))).collect();
    let raw = format!(
        "Password:\n{}\nSecretAccessKey =\n  {}\nsessionToken:\n\n  {}\n\"apiKey\":\n\"{}\"\nclient_secret:\r\n{}\nError: boom\n",
        words[0], words[1], words[2], words[3], words[4]
    );
    let out = scrubbed_without("short value on the next line", &raw, &words);
    assert!(
        out.starts_with("Password: | <redacted> | SecretAccessKey = | <redacted>"),
        "{out}"
    );
    assert!(out.ends_with("Error: boom"), "{out}");
}

#[test]
fn a_password_with_an_at_sign_and_then_a_slash_is_redacted_to_the_last_at_sign_that_is_not_after_a_slash()
 {
    let (a, b, c) = (
        secret_word("urla"),
        secret_word("urlb"),
        secret_word("urlc"),
    );
    for (raw_url, kept) in [
        // An `@` and then a `/` in the password: the authority holds an `@`, and the host is after a later one.
        (
            format!("https://alice:{a}@{b}/{c}@db.example/query"),
            "https://<redacted>@db.example/query",
        ),
        // A `/` and then an `@` after it; a path part with an npm-scope `@` is not the host.
        (
            format!("https://alice:{a}/{b}@db.example/export/@daily/result"),
            "https://<redacted>@db.example/export/@daily/result",
        ),
        // A password that ends in a slash.
        (
            format!("https://user:{a}/@host.example/x"),
            "https://<redacted>@host.example/x",
        ),
        // An npm scope after a clean user information stays.
        (
            format!("https://user:{a}@registry.example/@scope/pkg"),
            "https://<redacted>@registry.example/@scope/pkg",
        ),
    ] {
        let raw = format!("fetch {raw_url} failed\n");
        let out = scrubbed_without(&raw_url, &raw, &[a.clone(), b.clone(), c.clone()]);
        assert!(out.contains(kept), "{kept} is missing: {out}");
    }
}

#[test]
fn a_bracket_that_is_no_ipv6_host_does_not_escape_the_password_check() {
    let (a, b) = (secret_word("brka"), secret_word("brkb"));
    let raw = format!("fetch http://[{a}:ab/{b}@host.example/x failed\n");
    scrubbed_without("malformed bracket", &raw, &[a, b]);
}

// ---- Third pass: the second review (R1 to R5) -----------------------------------------------------
//
// Every stand-in is built at run time. A secret here is 17 characters with a digit and an upper-case letter (`secret_word`): no
// base64 or hex rule takes it, so only the rule under test can.

// R1: a label that is a secret key by a short word, in front of a second secret key. The label's value is the second key's name,
// and the scan has to go on at that name: the secret after it is the second key's value.

#[test]
fn a_short_label_in_front_of_a_secret_key_does_not_take_the_keys_name() {
    for (head, kept) in [
        ("pass: password: ", "pass: password: <redacted>"),
        ("Bearer: auth_token: ", "Bearer: auth_token: <redacted>"),
        ("auth: SessionToken = ", "auth: SessionToken = <redacted>"),
        ("pwd: client_secret = ", "pwd: client_secret = <redacted>"),
        ("pat: api_key = ", "pat: api_key = <redacted>"),
        (
            "pw: AWS_SECRET_ACCESS_KEY = ",
            "pw: AWS_SECRET_ACCESS_KEY = <redacted>",
        ),
        ("webhook=password: ", "webhook=password: <redacted>"),
        ("dbPass: password: ", "dbPass: password: <redacted>"),
        // Three keys in a row.
        ("pass: auth: password: ", "pass: auth: password: <redacted>"),
    ] {
        let secret = secret_word("lbl");
        let raw = format!("{head}{secret}\nError: boom\n");
        let out = scrubbed_without(head, &raw, std::slice::from_ref(&secret));
        assert!(out.contains(kept), "{kept} is missing: {out}");
        assert!(out.ends_with("Error: boom"), "{out}");
    }
}

#[test]
fn a_label_that_holds_a_secret_word_in_front_of_a_secret_key_leaves_no_secret() {
    // `refusal::redact` reads `token: password:` as a key and its value and takes the second key's name; the secret is gone.
    for head in [
        "token: password: ",
        "secret = SessionToken : ",
        "password: auth_token: ",
        "api_key: client_secret = ",
    ] {
        let secret = secret_word("strong");
        let raw = format!("{head}{secret}\nError: boom\n");
        let out = scrubbed_without(head, &raw, std::slice::from_ref(&secret));
        assert!(out.ends_with("Error: boom"), "{out}");
    }
}

#[test]
fn a_short_label_then_a_secret_key_on_the_next_line_or_a_value_on_the_next_line() {
    for (raw_head, kept) in [
        ("auth:\npassword: ", "auth: | password: <redacted>"),
        ("auth:\nDB_PASS: ", "auth: | DB_PASS: <redacted>"),
        ("pass:\nauth_token = ", "pass: | auth_token = <redacted>"),
        ("pass: password:\n", "pass: password: | <redacted>"),
        ("Bearer: auth_token:\n", "Bearer: auth_token: | <redacted>"),
    ] {
        let secret = secret_word("nl");
        let raw = format!("{raw_head}{secret}\nError: boom\n");
        let out = scrubbed_without(raw_head, &raw, std::slice::from_ref(&secret));
        assert!(out.contains(kept), "{kept} is missing: {out}");
    }
}

#[test]
fn a_label_in_front_of_a_value_that_is_no_secret_key_still_takes_the_value() {
    // The scan goes on at the value only when the value is itself a key that names a secret.
    for (line, kept) in [
        ("pass: summer2026Q", "pass: <redacted>"),
        ("auth: user_id = 5", "auth: <redacted> = 5"),
        ("pwd: token_type: bearer", "pwd: <redacted> bearer"),
        ("token: user_id: 5", "token: <redacted> 5"),
    ] {
        let out = scrub(format!("{line}\n").as_bytes(), false).joined();
        assert_eq!(out, kept, "{line}");
    }
    // A label that is not a secret leaves a secret key behind it to that key's own rule.
    let secret = secret_word("plain");
    let out = scrubbed_without(
        "plain label",
        &format!("status: password: {secret}\n"),
        std::slice::from_ref(&secret),
    );
    assert_eq!(out, "status: password: <redacted>");
    let out = scrubbed_without(
        "kind label",
        &format!("token_type: password: {secret}\n"),
        std::slice::from_ref(&secret),
    );
    assert_eq!(out, "token_type: password: <redacted>");
}

#[test]
fn a_quoted_key_name_or_a_stray_quote_after_a_label_does_not_hide_the_second_keys_secret() {
    for (head, tail, kept) in [
        // The second key is quoted, as in an object.
        (
            "auth: \"password\": \"",
            "\"",
            "auth: \"password\": \"<redacted>\"",
        ),
        ("pass: 'api_key': '", "'", "pass: 'api_key': '<redacted>'"),
        // A stray quote pairs the label's quote with the opening quote of the second key's value: the text in front of the second
        // key is the label's, and goes with it.
        (
            "auth:\"x keystore.pass = \"",
            "\" tail",
            "auth:\"<redacted>\" keystore.pass = \"<redacted>\" tail",
        ),
        (
            "pass: 'x password: '",
            "' tail",
            "pass: '<redacted>' password: '<redacted>' tail",
        ),
    ] {
        let secret = secret_word("quote");
        let raw = format!("{head}{secret}{tail}\nError: boom\n");
        let out = scrubbed_without(head, &raw, std::slice::from_ref(&secret));
        assert!(out.contains(kept), "{kept} is missing: {out}");
    }
    // A quoted value that is no key stays the label's value.
    for (line, kept) in [
        (
            "auth: \"plain text value\" next",
            "auth: \"<redacted>\" next",
        ),
        ("pass: \"a: b\" next", "pass: \"<redacted>\" next"),
    ] {
        assert_eq!(scrub(format!("{line}\n").as_bytes(), false).joined(), kept);
    }
}

#[test]
fn an_escaped_quote_after_a_label_that_pairs_with_the_next_keys_quote_hides_no_secret() {
    // The label's value opens with an escaped quote (inside a JSON string) and the next plain quote pairs with it.
    for (head, tail) in [
        ("auth=\\\" keystore.pass = \"", "\" tail"),
        ("pass: \\\"x password: \"", "\" tail"),
        ("pwd = \\'y api_key = '", "' tail"),
    ] {
        let secret = secret_word("esc");
        let raw = format!("{head}{secret}{tail}\nError: boom\n");
        let out = scrubbed_without(head, &raw, std::slice::from_ref(&secret));
        assert!(out.ends_with("Error: boom"), "{out}");
    }
}

/// Labels that are keys by a short word, as a compound or as a webhook name: ambiguous keys.
const R1_LABELS: [&str; 26] = [
    "auth",
    "Auth",
    "AUTH",
    "pass",
    "Pass",
    "PASS",
    "pat",
    "PAT",
    "pw",
    "PW",
    "pwd",
    "Pwd",
    "bearer",
    "Bearer",
    "BEARER",
    "webhook",
    "Webhook",
    "DB_PASS",
    "db_pass",
    "dbPass",
    "MYSQL_PWD",
    "GITHUB_PAT",
    "REDIS_AUTH",
    "OPENAI_KEY",
    "openaiKey",
    "SLACK_WEBHOOK",
];

/// Labels that hold a secret word themselves.
const R1_STRONG_LABELS: [&str; 4] = ["password", "token", "secret", "api_key"];

/// Second keys, each of which names a secret.
const R1_INNER_KEYS: [&str; 30] = [
    "password",
    "Password",
    "passwd",
    "secret",
    "client_secret",
    "clientSecret",
    "api_key",
    "apiKey",
    "x-api-key",
    "auth_token",
    "authToken",
    "access_token",
    "SessionToken",
    "session_token",
    "AWS_SECRET_ACCESS_KEY",
    "SecretAccessKey",
    "private_key",
    "credentials",
    "DB_PASS",
    "dbPass",
    "MYSQL_PWD",
    "GITHUB_PAT",
    "OPENAI_KEY",
    "pass",
    "pwd",
    "pat",
    "auth",
    "bearer",
    "SLACK_WEBHOOK_URL",
    "keyStorePassword",
];

/// The first of `secrets` (or a piece of one) that `raw` still shows after the scrub, in the tail, the error body or the log body.
fn survivor(raw: &str, secret: &str) -> Option<String> {
    let tail = scrub(raw.as_bytes(), false);
    let (joined, body, log) = (
        tail.joined(),
        error_body("exit status 1", &tail),
        log_body("exit status 1", &tail),
    );
    [joined.as_str(), body.as_str(), log.as_str()]
        .iter()
        .find_map(|text| leaked(text, secret).map(|_| joined.clone()))
}

#[test]
fn a_planted_secret_behind_every_label_key_and_separator_does_not_survive() {
    let label_seps = ["=", ": ", " = ", ":", " : ", " => "];
    let inner_seps = [": ", "=", " = ", ":", " : ", " := ", " => "];
    let wrappers = [
        ("", ""),
        ("export ", " extra"),
        ("{ ", " }"),
        ("Error: ", ", next: 1"),
    ];
    let (mut cases, mut leaks, mut lost_names) = (0usize, vec![], vec![]);
    for label in R1_LABELS.iter().chain(R1_STRONG_LABELS.iter()) {
        for inner in R1_INNER_KEYS {
            for (i, label_sep) in label_seps.iter().enumerate() {
                for (j, inner_sep) in inner_seps.iter().enumerate() {
                    let (before, after) = wrappers[(i + j + cases) % wrappers.len()];
                    let secret = secret_word("plant");
                    let raw = format!(
                        "{before}{label}{label_sep}{inner}{inner_sep}{secret}{after}\nError: boom\n"
                    );
                    cases += 1;
                    if let Some(shown) = survivor(&raw, &secret) {
                        leaks.push(format!("{raw:?} -> {shown}"));
                        continue;
                    }
                    // A space beside the second key's separator ends the label's value at the key's name: the name stays.
                    // (A label that ends in `_auth` is the registry rule's too, and one that holds a secret word is `refusal::redact`'s:
                    // both take the second key's name along with the label's value, the secret is gone either way.)
                    let keeps_names =
                        R1_LABELS.contains(label) && !label.to_ascii_lowercase().ends_with("_auth");
                    if keeps_names && inner_sep.contains(' ') {
                        let out = scrub(raw.as_bytes(), false).joined();
                        // `:=` and `=>` come out mangled (`password =<redacted> <redacted>`: the later rules read the second
                        // character as the value); the secret is gone and the key keeps its name.
                        let named = if inner_sep.contains(":=") || inner_sep.contains("=>") {
                            inner.to_string()
                        } else {
                            format!("{inner}{inner_sep}<redacted>")
                        };
                        if !out.contains(&named) {
                            lost_names.push(format!("{raw:?} -> {out}"));
                        }
                    }
                }
            }
        }
    }
    assert_eq!(
        cases,
        (R1_LABELS.len() + R1_STRONG_LABELS.len()) * R1_INNER_KEYS.len() * 6 * 7
    );
    assert!(
        leaks.is_empty(),
        "{} of {cases} combinations leaked a secret, e.g. {:#?}",
        leaks.len(),
        &leaks[..leaks.len().min(6)]
    );
    assert!(
        lost_names.is_empty(),
        "{} of {cases} combinations lost the second key's name, e.g. {:#?}",
        lost_names.len(),
        &lost_names[..lost_names.len().min(6)]
    );
}

// R2: an HTTP credential header whose value is in quotes, and the array a Node response prints for `set-cookie`.

#[test]
fn a_header_whose_value_is_in_single_quotes_loses_the_value_and_its_quotes() {
    for header in [
        "Cookie",
        "cookie",
        "COOKIE",
        "Set-Cookie",
        "x-api-key",
        "X-Api-Key",
        "Authorization",
        "authorization",
        "Proxy-Authorization",
        "X-Amz-Security-Token",
        "x-auth-token",
    ] {
        for sep in [": ", ":", " : "] {
            for shape in [
                "{s}",
                "sid={s}",
                "sid={s}; theme=dark",
                "Token {s}",
                "Bearer {s}",
            ] {
                let secret = secret_word("hq");
                let value = shape.replace("{s}", &secret);
                let raw = format!("{header}{sep}'{value}'\nError: boom\n");
                let what = format!("{header}{sep}'{shape}'");
                let out = scrubbed_without(&what, &raw, std::slice::from_ref(&secret));
                assert_eq!(
                    out,
                    format!("{header}{sep}<redacted> | Error: boom"),
                    "{what}"
                );
            }
        }
    }
}

#[test]
fn a_node_header_object_loses_its_credentials_and_keeps_the_other_members() {
    let (a, b, c, d) = (
        secret_word("n1"),
        secret_word("n2"),
        secret_word("n3"),
        secret_word("n4"),
    );
    let raw = format!(
        "headers: {{ cookie: 'sid={a}; theme=dark', accept: '*/*', authorization: 'Token {b}', 'x-api-key': '{c}', host: 'example.com', 'set-cookie': [ 'sid={d}; Path=/' ] }}\nError: boom\n"
    );
    let out = scrubbed_without("node headers", &raw, &[a, b, c, d]);
    assert_eq!(
        out,
        "headers: { cookie: <redacted>, accept: '*/*', authorization: <redacted>, 'x-api-key': '<redacted>', host: 'example.com', 'set-cookie': <redacted> } | Error: boom"
    );
}

#[test]
fn a_quoted_header_in_a_command_line_or_with_nothing_in_the_quotes_keeps_its_arguments() {
    let secret = secret_word("curl");
    let raw = format!("curl -H 'Cookie: sid={secret}' -H 'Accept: */*' https://example.com/x\n");
    let out = scrubbed_without("curl", &raw, std::slice::from_ref(&secret));
    assert_eq!(
        out,
        "curl -H 'Cookie: <redacted>' -H 'Accept: */*' https://example.com/x"
    );
    // Nothing to hide: a header with no value stays, and an empty quoted value keeps its space and its quotes (the key rule marks
    // the empty text, as it does for `password=""`).
    assert_eq!(scrub(b"Cookie:\n", false).joined(), "Cookie:");
    let out = scrub(b"cookie: ''\n", false).joined();
    assert_eq!(out, "cookie: '<redacted>'");
}

#[test]
fn a_set_cookie_array_loses_every_element_on_one_line_or_several() {
    let (a, b) = (secret_word("arr1"), secret_word("arr2"));
    for (raw, kept) in [
        (
            format!(
                "{{ 'set-cookie': [ 'sid={a}; Path=/; HttpOnly', 'theme={b}' ], server: 'nginx' }}\nError: boom\n"
            ),
            "{ 'set-cookie': <redacted>, server: 'nginx' } | Error: boom",
        ),
        // Nested brackets inside the array.
        (
            format!("'set-cookie': [ ['sid={a}'], 'theme={b}' ], server: 'nginx'\nError: boom\n"),
            "'set-cookie': <redacted>, server: 'nginx' | Error: boom",
        ),
        // The array spans several lines (Node breaks long arrays).
        (
            format!(
                "'set-cookie': [\n    'sid={a}; Path=/',\n    'theme={b}'\n  ],\nserver: 'nginx'\nError: boom\n"
            ),
            "'set-cookie': <redacted>, | server: 'nginx' | Error: boom",
        ),
        // The header form.
        (
            format!("Set-Cookie: [ 'sid={a}', 'theme={b}' ]\nError: boom\n"),
            "Set-Cookie: <redacted> | Error: boom",
        ),
        // An array with no closing bracket in the window goes to the end of its line.
        (
            format!("'set-cookie': [ 'sid={a}', 'theme={b}'\nError: boom\n"),
            "'set-cookie': <redacted> | Error: boom",
        ),
        // Another secret key with an array value.
        (
            format!("secrets: [ '{a}', '{b}' ] done\nError: boom\n"),
            "secrets: <redacted> done | Error: boom",
        ),
    ] {
        let out = scrubbed_without(&raw, &raw, &[a.clone(), b.clone()]);
        assert_eq!(out, kept, "{raw:?}");
    }
}

// R3: authentication schemes after an equals sign, the Digest parameters, escaped quotes in a JSON value, a scheme on its own line.

#[test]
fn an_authorization_value_with_a_token_style_scheme_loses_its_credential() {
    for header in [
        "authorization",
        "Authorization",
        "AUTHORIZATION",
        "proxy-authorization",
        "proxy_authorization",
    ] {
        for scheme in [
            "Token",
            "ApiKey",
            "Api-Key",
            "Key",
            "Bot",
            "JWT",
            "DPoP",
            "SSWS",
            "token",
            "APIKEY",
            "Bearer",
            "Basic",
            "Negotiate",
            "NTLM",
            "Splunk",
            "GNAP",
            "PrivateToken",
            "Vapid",
        ] {
            for sep in ["=", " = ", ": ", ":"] {
                let secret = secret_word("sch");
                let raw = format!("{header}{sep}{scheme} {secret}\nError: boom\n");
                let what = format!("{header}{sep}{scheme}");
                let out = scrubbed_without(&what, &raw, std::slice::from_ref(&secret));
                assert_eq!(
                    out,
                    format!("{header}{sep}<redacted> | Error: boom"),
                    "{what}"
                );
            }
        }
    }
}

#[test]
fn a_token_style_scheme_after_an_equals_sign_keeps_the_pair_that_follows() {
    for scheme in ["Token", "ApiKey", "Bearer"] {
        let secret = secret_word("pair");
        let raw = format!("curl authorization={scheme} {secret}&accept=json\n");
        let out = scrubbed_without(scheme, &raw, std::slice::from_ref(&secret));
        assert_eq!(out, "curl authorization=<redacted>&accept=json", "{scheme}");
    }
}

#[test]
fn an_equals_form_digest_or_signature_header_loses_every_parameter() {
    let (n, r, o, k) = (
        secret_word("nonce"),
        secret_word("resp"),
        secret_word("opaque"),
        secret_word("cred"),
    );
    for (raw, kept) in [
        (
            format!(
                "authorization=Digest username=\"bob\", realm=\"r\", nonce=\"{n}\", uri=\"/x\", response=\"{r}\", opaque=\"{o}\"\nError: boom\n"
            ),
            "authorization=<redacted> | Error: boom",
        ),
        (
            format!(
                "Proxy-Authorization = Digest username=\"bob\", nonce=\"{n}\", response=\"{r}\"\nError: boom\n"
            ),
            "Proxy-Authorization = <redacted> | Error: boom",
        ),
        (
            format!(
                "authorization=AWS4-HMAC-SHA256 Credential={k}/20261009/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature={r}\nError: boom\n"
            ),
            "authorization=<redacted> | Error: boom",
        ),
        (
            format!(
                "authorization=OAuth oauth_consumer_key=\"{k}\", oauth_nonce=\"{n}\", oauth_signature=\"{r}\"\nError: boom\n"
            ),
            "authorization=<redacted> | Error: boom",
        ),
        // A quoted command-line argument ends the header.
        (
            format!(
                "curl -H 'authorization=Digest username=\"bob\", nonce=\"{n}\", response=\"{r}\"' https://example.com/x\n"
            ),
            "curl -H 'authorization=<redacted>' https://example.com/x",
        ),
    ] {
        let out = scrubbed_without(&raw, &raw, &[n.clone(), r.clone(), o.clone(), k.clone()]);
        assert_eq!(out, kept, "{raw:?}");
    }
}

#[test]
fn a_json_value_with_escaped_quotes_is_taken_to_its_real_end() {
    let (a, b) = (secret_word("esc1"), secret_word("esc2"));
    for (raw, kept) in [
        (
            format!(
                "{{\"Authorization\":\"Digest username=\\\"bob\\\", realm=\\\"r\\\", response=\\\"{a}\\\"\"}}\nError: boom\n"
            ),
            "{\"Authorization\":\"<redacted>\"} | Error: boom",
        ),
        (
            format!("{{\"password\":\"{a}\\\"{b}\",\"user\":\"bob\"}}\nError: boom\n"),
            "{\"password\":\"<redacted>\",\"user\":\"bob\"} | Error: boom",
        ),
        (
            format!("{{'authorization': 'Token it\\'s {a}{b}', accept: 'json'}}\nError: boom\n"),
            "{'authorization': '<redacted>', accept: 'json'} | Error: boom",
        ),
        // An unbalanced escape at the end of a value (a path that ends in a backslash) is still taken whole.
        (
            format!("password=\"{a}{b}\\\"\nError: boom\n"),
            "password=\"<redacted>\" | Error: boom",
        ),
    ] {
        let out = scrubbed_without(&raw, &raw, &[a.clone(), b.clone()]);
        assert_eq!(out, kept, "{raw:?}");
    }
}

#[test]
fn a_scheme_word_at_the_end_of_a_line_takes_the_token_on_the_next_line() {
    for (head, kept) in [
        (
            "Authorization: Bearer",
            "Authorization: <redacted> | Error: boom",
        ),
        (
            "authorization=Bearer",
            "authorization=<redacted> | Error: boom",
        ),
        (
            "authorization=Basic",
            "authorization=<redacted> | Error: boom",
        ),
        (
            "proxy-authorization: Basic",
            "proxy-authorization: <redacted> | Error: boom",
        ),
        (
            "authorization=Token",
            "authorization=<redacted> | Error: boom",
        ),
        (
            "Authorization: ApiKey",
            "Authorization: <redacted> | Error: boom",
        ),
    ] {
        let secret = secret_word("nlsch");
        let raw = format!("{head}\n  {secret}\nError: boom\n");
        let out = scrubbed_without(head, &raw, std::slice::from_ref(&secret));
        assert_eq!(out, kept, "{head}");
    }
    // A next line that starts a new message (a word that ends in a colon) is not the token.
    let out = scrub(b"authorization=Bearer\nError: boom\n", false).joined();
    assert_eq!(out, "authorization=<redacted> | Error: boom");
    let out = scrub(b"Authorization: Bearer\nError: boom\n", false).joined();
    assert_eq!(out, "Authorization: <redacted> | Error: boom");
}

// R4: camel-case `Pw`, and a lone short word in a JVM `-D` system property.

#[test]
fn camel_case_pw_keys_lose_their_value_like_their_snake_case_twins() {
    for key in [
        "ldapPw", "dbPw", "rootPw", "userPw", "adminPw", "smtpPw", "db_pw", "DB_PW", "ROOT_PW",
        "root-pw",
    ] {
        for sep in ["=", ": ", " = "] {
            let secret = secret_word("pw");
            let raw = format!("{key}{sep}{secret}\nError: boom\n");
            let out = scrubbed_without(&format!("{key}{sep}"), &raw, std::slice::from_ref(&secret));
            assert_eq!(
                out,
                format!("{key}{sep}<redacted> | Error: boom"),
                "{key}{sep}"
            );
        }
    }
}

#[test]
fn a_jvm_system_property_named_by_a_lone_short_word_loses_its_value() {
    for word in [
        "pass", "pw", "pwd", "pat", "auth", "bearer", "PASS", "Pw", "Auth", "BEARER",
    ] {
        let secret = secret_word("jvm");
        let raw = format!("java -D{word}={secret} -jar app.jar\nError: boom\n");
        let out = scrubbed_without(&format!("-D{word}"), &raw, std::slice::from_ref(&secret));
        assert_eq!(
            out,
            format!("java -D{word}=<redacted> -jar app.jar | Error: boom"),
            "-D{word}"
        );
    }
}

#[test]
fn jvm_properties_that_only_look_like_a_short_word_key_stay_readable() {
    for line in [
        "java -Dpass=true -jar app.jar",
        "java -Dpass=12 -jar app.jar",
        "java -Dauth.method=oidc -jar app.jar",
        "java -DPWD=/home/x -jar app.jar",
        "java -Dspring.profiles.active=prod -jar app.jar",
        "java -Dfile.encoding=UTF-8 -jar app.jar",
        // Without the dash, `Dpass` is just a word.
        "Dpass: summer2026",
        "Dauth=oidc",
    ] {
        let out = scrub(format!("{line}\n").as_bytes(), false).joined();
        assert_eq!(out, line, "the scrub changed {line:?}");
    }
}

// R5: the false-positive guards stay green, and the readable lines the second review saw redacted stay readable. Nothing here
// trades secret coverage: each companion test below keeps a secret in the same key family redacted.

#[test]
fn the_false_positive_guards_of_the_second_review_stay_readable() {
    for line in [
        "max_tokens: 200000",
        "ExpiredToken: The provided token has expired.",
        "at Object.<anonymous> (/srv/app/lib/token.js:12:5)",
        "token.js:12:5",
        "PWD=/var/lib/agentd/run/planner/home",
        "primary_key: id",
        "auth.example.com:443",
        "compass: north",
        "bypass=true",
        "test_login_bad_pass",
    ] {
        let out = scrub(format!("{line}\n").as_bytes(), false).joined();
        assert_eq!(out, line, "the scrub changed {line:?}");
    }
}

#[test]
fn test_names_that_hold_a_short_secret_word_stay_readable() {
    for line in [
        "FAILED tests/test_auth.py::test_login_bad_pass - AssertionError: 401 != 200",
        "FAILED tests/test_auth.py::test_login_bad_pass",
        "tests/test_auth.py::test_pass_through PASSED",
        "tests/test_pass.py::TestAuth::test_roundtrip FAILED",
        "test auth::tests::rejects_bad_password ... FAILED",
        "test auth::tests::rejects_bad_token ... ok",
        "test pass::tests::roundtrip ... ok",
        "test bearer::tests::parses_scheme ... ok",
    ] {
        let out = scrub(format!("{line}\n").as_bytes(), false).joined();
        assert_eq!(out, line, "the scrub changed {line:?}");
    }
}

#[test]
fn settings_that_name_a_host_a_port_a_rate_or_a_limit_next_to_a_short_secret_word_stay_readable() {
    for line in [
        "auth_port=8080",
        "auth_host=db.example.com",
        "auth_hostname: login.example.com",
        "auth_server=https://login.example.com",
        "auth_domain=example.com",
        "auth_endpoint=/oauth2/token",
        "auth_level=2",
        "auth_version=3",
        "auth_ttl=3600",
        "pass_rate=0.95",
        "pass_total=12",
        "pass_ratio=0.5",
        "pass_percent=95",
        "pat_limit=5",
        "bearer_size=1024",
        "pwd_max=64",
        "pwd_min=8",
        "pwd_length=12",
        "pass_duration=30s",
        "pass_interval=5",
    ] {
        let out = scrub(format!("{line}\n").as_bytes(), false).joined();
        assert_eq!(out, line, "the scrub changed {line:?}");
    }
}

#[test]
fn a_public_key_an_s3_object_key_and_a_kms_key_are_no_secret() {
    for line in [
        "public_key: ssh-ed25519",
        "PUBLIC_KEY=ssh-rsa",
        "publicKey: ssh-ed25519",
        "ssh_public_key=ssh-rsa",
        "s3Key: runs/01ABC/plan.json",
        "s3_key=runs/01ABC/plan.json",
        "kms_key=alias/prod",
        "kmsKey: alias/prod",
    ] {
        let out = scrub(format!("{line}\n").as_bytes(), false).joined();
        assert_eq!(out, line, "the scrub changed {line:?}");
    }
}

#[test]
fn pwd_with_a_path_under_a_well_known_root_is_the_working_directory() {
    for line in [
        "pwd: /home/x",
        "pwd = /Users/testuser/work",
        "Pwd: /var/lib/app",
        "pwd=/tmp/build",
        "\"pwd\":\"/workspace/repo\"",
    ] {
        let out = scrub(format!("{line}\n").as_bytes(), false).joined();
        assert_eq!(out, line, "the scrub changed {line:?}");
    }
}

#[test]
fn what_is_still_a_secret_in_the_same_key_families_stays_redacted() {
    let secret = secret_word("r5");
    // The key words the readable lines above use, with a value that is a credential.
    let cases: Vec<String> = vec![
        // Other shapes of `pwd`: a path-like password, the same words under another key, a root that is no root.
        format!("pwd: /{secret}"),
        format!("pwd: /homes/{secret}"),
        format!("MYSQL_PWD=/home/{secret}"),
        format!("pwd: {secret}"),
        // A host, a port, a rate or a limit is a kind only as the last part of the key, and only with a value of that kind.
        format!("auth_host_token={secret}"),
        format!("pass_rate_secret: {secret}"),
        format!("auth_port: {secret}"),
        format!("auth_host: {secret}"),
        format!("pass_total={secret}"),
        // A user or a role is not a kind this scrub reads as plain: a value there stays redacted.
        format!("auth_user: {secret}"),
        format!("auth_user: bob-{secret}"),
        // A key part that is not the last one does not make the key a kind.
        format!("pass_hash={secret}"),
        format!("auth_header_value={secret}"),
        // An S3 object key is a path; the same name with a value that is no path is a secret, and only `key` after `s3` is an object key.
        format!("s3Key={secret}"),
        format!("s3_key: {secret}"),
        format!("s3_pass: runs/{secret}"),
        // A key that ends in `key` and names no purpose.
        format!("signing_key={secret}"),
        format!("session_key: {secret}"),
        // A strong key beside a test name.
        format!("tests/test_auth.py::test_x password: {secret}"),
        format!("FAILED tests/test_auth.py::test_login_bad_pass - password={secret}"),
        // A double colon is a path only after a short-word key: after a strong key it is still a value.
        format!("password::{secret}"),
    ];
    for line in cases {
        let out = scrub(format!("{line}\n").as_bytes(), false).joined();
        assert!(!out.contains(&secret), "{line:?} became {out:?}");
    }
}

// Pins found by the mutation check of the third pass: each test below fails under at least one mutation of the rule it names.

#[test]
fn a_header_value_in_a_quote_that_never_closes_is_taken_to_the_end_of_the_line() {
    let secret = secret_word("open");
    let out = scrubbed_without(
        "open quote",
        &format!("Cookie: 'sid={secret}\nError: boom\n"),
        std::slice::from_ref(&secret),
    );
    assert_eq!(out, "Cookie: <redacted> | Error: boom");
}

#[test]
fn a_header_inside_an_array_that_a_header_took_is_not_taken_twice() {
    let (a, b) = (secret_word("in1"), secret_word("in2"));
    let raw = format!("Set-Cookie: [ 'sid={a}', 'x-api-key: {b}' ] done\nError: boom\n");
    let out = scrubbed_without("nested header", &raw, &[a, b]);
    assert_eq!(out, "Set-Cookie: <redacted> done | Error: boom");
}

#[test]
fn an_array_value_that_a_word_follows_without_a_blank_is_taken_whole() {
    let secret = secret_word("tail");
    let out = scrubbed_without(
        "array then word",
        &format!("secrets: [{secret}]tail\nError: boom\n"),
        std::slice::from_ref(&secret),
    );
    assert_eq!(out, "secrets: <redacted> | Error: boom");
}

#[test]
fn a_parameter_scheme_word_with_no_parameters_after_a_blank_is_only_the_value() {
    let out = scrub(b"authorization=Digest&accept=json\n", false).joined();
    assert_eq!(out, "authorization=<redacted>&accept=json");
}

#[test]
fn the_token_after_a_scheme_word_ends_at_a_quote_a_comma_a_semicolon_or_a_bar() {
    // (A backslash ends it too, but `refusal::redact` takes `<redacted>\x` whole afterwards, so the end is not visible.)
    for end in ['"', '\'', ',', ';', '|'] {
        let secret = secret_word("end");
        let raw = format!("authorization=Token {secret}{end}x\n");
        let out = scrubbed_without(&format!("end {end}"), &raw, std::slice::from_ref(&secret));
        assert!(
            out.starts_with("authorization=<redacted>") && out.ends_with(&format!("{end}x")),
            "the token ended wrongly at {end:?}: {out}"
        );
    }
}

#[test]
fn a_parameter_scheme_header_of_every_scheme_loses_every_parameter() {
    for scheme in [
        "Digest",
        "AWS4-HMAC-SHA256",
        "OAuth",
        "Hawk",
        "Signature",
        "SCRAM-SHA-256",
    ] {
        let (a, b) = (secret_word("par1"), secret_word("par2"));
        let raw =
            format!("authorization={scheme} id=\"bob\", mac=\"{a}\", ext=\"{b}\"\nError: boom\n");
        let out = scrubbed_without(scheme, &raw, &[a, b]);
        assert_eq!(out, "authorization=<redacted> | Error: boom", "{scheme}");
    }
}

#[test]
fn a_key_with_the_part_auth_takes_a_token_style_scheme_like_an_authorization_key() {
    for key in ["auth", "AUTH", "x-auth-token", "basic_auth", "REDIS_AUTH"] {
        let secret = secret_word("auth");
        let raw = format!("{key}=Token {secret}&accept=json\n");
        let out = scrubbed_without(key, &raw, std::slice::from_ref(&secret));
        assert!(out.ends_with("&accept=json"), "{key}: {out}");
    }
}

#[test]
fn a_value_that_starts_with_a_colon_after_a_blank_is_a_value_not_a_double_colon() {
    let secret = secret_word("colon");
    for line in [
        format!("pass: :{secret}"),
        format!("auth = :{secret}"),
        format!("pwd: ::{secret}"),
    ] {
        let out = scrubbed_without(&line, &format!("{line}\n"), std::slice::from_ref(&secret));
        assert!(out.contains("<redacted>"), "{line}: {out}");
    }
}

#[test]
fn settings_of_the_remaining_number_and_place_kinds_and_the_edges_of_a_number_with_a_unit() {
    for line in [
        "pass_percentage=95",
        "pass_percent=95%",
        "pass_rate=0.95",
        "pass_size=512kb",
        "auth_address=10.0.0.1",
        "auth_addr=db.example.com",
        "auth_endpoint=/oauth2/token",
    ] {
        let out = scrub(format!("{line}\n").as_bytes(), false).joined();
        assert_eq!(out, line, "the scrub changed {line:?}");
    }
    // A value that is not a number with at most a two-character unit stays a secret, also under a number kind.
    for line in [
        "pass_total=12abc",
        "pass_rate=ab",
        "pass_size=a512",
        "auth_port=8080x9",
        "pass_limit=.",
        "pass_limit=",
    ] {
        let out = scrub(format!("{line}\n").as_bytes(), false).joined();
        let value = line.split_once('=').unwrap().1;
        if value.is_empty() {
            assert_eq!(out, line, "an empty value stays: {out}");
        } else {
            assert!(
                !out.contains(value) || out.contains("<redacted>"),
                "{line:?} became {out:?}"
            );
        }
    }
}

#[test]
fn pwd_with_a_path_needs_something_after_a_well_known_root_and_every_root_counts() {
    for root in [
        "/home/",
        "/Users/",
        "/var/",
        "/tmp/",
        "/opt/",
        "/usr/",
        "/srv/",
        "/mnt/",
        "/workspace/",
        "/app/",
        "/data/",
        "/root/",
        "/etc/",
        "/local/",
        "/Volumes/",
        "/private/",
    ] {
        let line = format!("pwd: {root}x");
        let out = scrub(format!("{line}\n").as_bytes(), false).joined();
        assert_eq!(out, line, "the scrub changed {line:?}");
    }
    // The root alone, or a root with a different name, is not a working directory.
    for line in [
        "pwd: /home/",
        "pwd: /var/",
        "pwd: /homes/x",
        "pwd: /Homer/x",
    ] {
        let out = scrub(format!("{line}\n").as_bytes(), false).joined();
        assert_eq!(out, "pwd: <redacted>", "{line:?} became {out:?}");
    }
}

#[test]
fn an_s3_object_key_is_a_path_only_with_a_slash_in_its_value() {
    for line in [
        "s3Key: runs/01ABC/plan.json",
        "s3_key=runs/x",
        "S3-KEY: a/b",
    ] {
        let out = scrub(format!("{line}\n").as_bytes(), false).joined();
        assert_eq!(out, line, "the scrub changed {line:?}");
    }
    // `key` after another word than `s3` is no object key even with a slash.
    let line = ["signing_", "key", "=", "runs/01ABC/plan\n"].concat();
    let out = scrub(line.as_bytes(), false).joined();
    assert_eq!(out, ["signing_", "key", "=<redacted>"].concat());
}

// More pins from the mutation check: the second-key scan in `second_key_at`, the scheme tables, and rules of the earlier review.

#[test]
fn a_stray_quote_resumes_only_at_a_second_key_that_names_a_secret() {
    // `token_type` is a key the pattern finds that names no secret: the label keeps its job and takes the quoted text.
    let out = scrub(b"auth: \"x token_type = \"bob\" tail\n", false).joined();
    assert_eq!(out, "auth: \"<redacted>\"bob\" tail");
    // A secret key that follows other keys inside the quotes is found all the same.
    let secret = secret_word("many");
    let raw = format!("auth: \"x token_type: y password = \"{secret}\" tail\nError: boom\n");
    let out = scrubbed_without(
        "second key after another key",
        &raw,
        std::slice::from_ref(&secret),
    );
    assert!(out.contains("password = \"<redacted>\" tail"), "{out}");
}

#[test]
fn two_quoted_values_with_no_blank_between_them_both_go() {
    let (a, b) = (secret_word("adj1"), secret_word("adj2"));
    let raw = format!("pass: '{a}'password: '{b}'\n");
    let out = scrubbed_without("adjacent", &raw, &[a, b]);
    assert_eq!(out, "pass: '<redacted>'password: '<redacted>'");
}

#[test]
fn a_scheme_word_in_front_of_the_second_key_is_looked_through() {
    for scheme in ["NTLM", "Digest", "Negotiate"] {
        let secret = secret_word("sch2");
        let raw = format!("auth: {scheme} password: {secret}\n");
        let out = scrubbed_without(scheme, &raw, std::slice::from_ref(&secret));
        assert_eq!(
            out,
            format!("auth: {scheme} password: <redacted>"),
            "{scheme}"
        );
    }
}

#[test]
fn a_label_whose_value_is_followed_by_a_secret_key_further_on_keeps_its_own_value_redacted() {
    // `summer2026` is the label's value; the secret key `token` comes after a plain word, so nothing starts at the value.
    let (a, b) = (secret_word("lab"), secret_word("tok"));
    let raw = format!("pass: {a} = bar token: {b}\n");
    let out = scrubbed_without("label then a later key", &raw, &[a, b]);
    assert_eq!(out, "pass: <redacted> = bar token: <redacted>");
}

#[test]
fn a_backslash_led_quoted_key_name_after_a_label_hides_no_secret() {
    let secret = secret_word("bsq");
    let raw = format!("auth: \\\"password\\\": \\\"{secret}\\\"\n");
    scrubbed_without("escaped quoted key", &raw, std::slice::from_ref(&secret));
}

#[test]
fn a_word_that_is_a_scheme_name_after_a_key_that_is_no_authorization_key_stays_a_word() {
    // `key must be ...` starts a sentence; only an authorization key reads `key` as a scheme with a token after it.
    for line in [
        "passphrase: key must be at least 8 characters",
        "passphrase: token rotated, see docs",
    ] {
        let out = scrub(format!("{line}\n").as_bytes(), false).joined();
        assert_eq!(out, line, "the scrub changed {line:?}");
    }
}

#[test]
fn every_token_scheme_at_the_end_of_a_line_takes_the_token_on_the_next_line() {
    for scheme in [
        "bearer",
        "basic",
        "negotiate",
        "ntlm",
        "token",
        "apikey",
        "api-key",
        "key",
        "bot",
        "jwt",
        "dpop",
        "ssws",
        "splunk",
        "gnap",
        "privatetoken",
        "vapid",
    ] {
        for head in [
            format!("authorization={scheme}"),
            format!("Authorization: {scheme}"),
        ] {
            let secret = secret_word("tsch");
            let raw = format!("{head}\n{secret}\nError: boom\n");
            let out = scrubbed_without(&head, &raw, std::slice::from_ref(&secret));
            assert!(out.ends_with("| Error: boom"), "{head}: {out}");
        }
    }
}

#[test]
fn rules_of_the_earlier_review_that_the_mutation_check_found_unpinned() {
    // A camel-case `Bearer` key.
    let secret = secret_word("old");
    let out = scrubbed_without(
        "apiBearer",
        &format!("apiBearer={secret}\n"),
        std::slice::from_ref(&secret),
    );
    assert_eq!(out, "apiBearer=<redacted>");
    // A number of five digits is a counter under `pass`; six digits may be a PIN.
    assert_eq!(scrub(b"pass: 12345\n", false).joined(), "pass: 12345");
    assert_eq!(scrub(b"pass: 123456\n", false).joined(), "pass: <redacted>");
    // A path whose lower-case share is 68 percent stays; the secret-shaped word with the same parts and no extension does not.
    let path = ["abcdefghijklmnopqrstuvwxyza", "BBBBBBBBBBBB", "/c1"].concat();
    assert_eq!(
        scrub(format!("{path}.json x\n").as_bytes(), false).joined(),
        format!("{path}.json x")
    );
    assert_eq!(
        scrub(format!("{path}. x\n").as_bytes(), false).joined(),
        "<redacted>. x"
    );
    // A URL run ends at `<`, `>` and a backtick, and its authority at `?` and `#`.
    let pw = secret_word("url");
    for (raw, kept) in [
        (
            format!("see <https://user:{pw}@example.com/x> and"),
            "see <https://<redacted>@example.com/x> and",
        ),
        (
            format!("run `curl https://user:{pw}@host.example.com/x` now"),
            "run `curl https://<redacted>@host.example.com/x` now",
        ),
        (
            format!("https://user:{pw}@host.example.com#frag"),
            "https://<redacted>@host.example.com#frag",
        ),
        (
            format!("https://user:{pw}@host.example.com?x=1"),
            "https://<redacted>@host.example.com?<redacted>",
        ),
    ] {
        let out = scrubbed_without(&raw, &format!("{raw}\n"), std::slice::from_ref(&pw));
        assert_eq!(out, kept, "{raw}");
    }
}

#[test]
fn a_scheme_word_at_the_end_of_a_line_followed_by_a_line_that_starts_with_a_delimiter_takes_nothing()
 {
    let out = scrub(b"authorization=Bearer\n\"quoted line\"\n", false).joined();
    assert_eq!(out, "authorization=<redacted> | \"quoted line\"");
}

#[test]
fn keys_inside_a_json_string_with_escaped_quotes_lose_a_short_value() {
    // Short stand-ins, so no shape rule is the reason they go: only the key rule's alternatives for an escaped opening quote.
    let (a, b, c, d) = (
        secret_word("jd1"),
        secret_word("jd2"),
        secret_word("jd3"),
        secret_word("jd4"),
    );
    let raw = format!(
        "{{\"msg\":\"x {{\\\"password\\\":\\\"{a}\\\",\\\"api_key\\\":\\\"{b}\\\"}}\"}}\n\
         log {{\\'password\\':\\'{c}\\',\\'api_key\\':\\'{d}\\'}}\nError: boom\n"
    );
    let out = scrubbed_without("escaped json", &raw, &[a, b, c, d]);
    assert!(out.ends_with("Error: boom"), "{out}");
}

#[test]
fn a_quoted_secret_that_ends_in_a_backslash_does_not_hide_the_next_secret_or_stay_behind() {
    // A Windows-path secret: the closing quote follows a backslash, and a second secret key comes later on the line. (The keys are
    // ones `refusal::redact` does not know, so only the key rule can take the values.)
    let (a, b) = (secret_word("win1"), secret_word("win2"));
    let raw = format!("client_secret=\"{a}\\\" private_key=\"{b}\"\nError: boom\n");
    let out = scrubbed_without("backslash then quote", &raw, &[a, b]);
    assert_eq!(
        out,
        "client_secret=\"<redacted>\" private_key=\"<redacted>\" | Error: boom"
    );
}

#[test]
fn a_stray_quote_after_a_key_that_is_no_secret_leaves_the_text_in_front_of_the_second_key_alone() {
    let secret = secret_word("kind");
    let raw = format!("token_type: \"x password = \"{secret}\" tail\n");
    let out = scrubbed_without("kind key", &raw, std::slice::from_ref(&secret));
    assert_eq!(out, "token_type: \"x password = \"<redacted>\" tail");
}

#[test]
fn a_key_inside_the_value_of_a_label_that_stays_is_still_scanned() {
    // A short label at the end of a line does not take the next line, and a kind key takes no value: the word that follows them is
    // text, and a key inside it (after `-D`, `--`, or another word) still takes its own value.
    for head in [
        "pass=\n-DDB_PASS=",
        "auth:\n--api-key=",
        "pwd=\n-Dclient_secret=",
        "token_type: x=password=",
        "max_tokens: 5=secret=",
        "auth_method: --client-secret=",
    ] {
        let secret = secret_word("inner");
        let raw = format!("{head}{secret}\nError: boom\n");
        let out = scrubbed_without(head, &raw, std::slice::from_ref(&secret));
        assert!(out.ends_with("| Error: boom"), "{head}: {out}");
    }
}

#[test]
fn a_token_scheme_word_followed_by_a_secret_key_is_a_word_and_the_key_keeps_its_own_value() {
    // The token after `key` or `Token` would be the next key; the scheme word is the whole value and the key is scanned.
    for head in [
        "auth: key authorization=Bearer\n",
        "authorization=Token password=\n",
        "auth_token: Bot api_key:\n",
        "x-auth-token=ApiKey client_secret=\n",
    ] {
        let secret = secret_word("swd");
        let raw = format!("{head}{secret}\nError: boom\n");
        let out = scrubbed_without(head, &raw, std::slice::from_ref(&secret));
        assert!(out.ends_with("| Error: boom"), "{head}: {out}");
    }
    // A token that only looks like a key at its start is still a token: base64 padding, a JWT and an `=` inside a value.
    let secret = secret_word("pad");
    let padded = ["dXNlcjpw", "YXNzd29yZA=="].concat();
    for token in [padded.as_str(), secret.as_str()] {
        let raw = format!("authorization=Token {token}&accept=json\n");
        let out = scrubbed_without(token, &raw, &[token.to_string()]);
        assert_eq!(out, "authorization=<redacted>&accept=json", "{token}");
    }
}

#[test]
fn an_array_that_never_closes_stops_before_a_secret_key_that_takes_its_value_on_the_next_line() {
    let (a, b) = (secret_word("arr1"), secret_word("arr2"));
    for head in [
        "Bearer =[POST auth_token=",
        "secrets: [auth_token=",
        "secrets: [ 'x' auth_token=",
        "'set-cookie': [ 'sid=A', auth_token=",
    ] {
        let head = head.replace('A', &a);
        let raw = format!("{head}\n{b}\nError: boom\n");
        let out = scrubbed_without(&head, &raw, &[a.clone(), b.clone()]);
        assert!(out.ends_with("| Error: boom"), "{head}: {out}");
    }
}

#[test]
fn a_token_after_a_scheme_word_that_ends_like_a_key_name_is_still_a_token() {
    // A random token can end in `Pw`, `Key` or `Auth` and be followed by a separator character; only a word with the separator
    // attached (`authorization=Bearer`, `password=`) is the next key.
    for suffix in ["Pw", "Key", "Auth", "Pass", "Pat", "Bearer", "Pwd"] {
        let token = ["hunter", "2tail", suffix].concat();
        for tail in [" =#", " :x"] {
            let raw = format!("authorization=ApiKey {token}{tail}\nError: boom\n");
            let out = scrubbed_without(suffix, &raw, std::slice::from_ref(&token));
            assert!(
                out.contains("authorization=<redacted>"),
                "{suffix}{tail}: {out}"
            );
        }
    }
}

#[test]
fn a_stray_quote_that_ends_in_front_of_a_digest_header_does_not_leave_its_parameters() {
    // The label's quoted text ends inside the Digest header; the header's own span reaches past the quote.
    for head in [
        "\"Bearer:\" authorization=Digest username=\"bob\", nonce=\"n1\", response=\"",
        "pass: \"x authorization=Digest username=\"bob\", response=\"",
    ] {
        let secret = secret_word("dig");
        let raw = format!("{head}{secret}\" ;>\nError: boom\n");
        let out = scrubbed_without(head, &raw, std::slice::from_ref(&secret));
        assert!(out.ends_with("| Error: boom"), "{head}: {out}");
    }
}

#[test]
fn a_token_scheme_word_followed_by_a_spaced_secret_key_is_a_word_and_the_key_keeps_its_value() {
    for (template, key) in [
        ("auth=key signing_key = \"{s}\"", "signing_key"),
        (
            "Authorization: Bearer\nclient_secret = {s}",
            "client_secret",
        ),
        ("auth_token: Bot api_key : {s}", "api_key"),
    ] {
        let secret = secret_word("spk");
        let raw = template.replace("{s}", &secret);
        let out = scrubbed_without(&raw, &format!("{raw}\n"), std::slice::from_ref(&secret));
        assert!(out.contains(key), "the key lost its name: {out}");
    }
}
