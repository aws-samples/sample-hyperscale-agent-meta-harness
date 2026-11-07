//! The shared redaction function: classes, the window that holds
//! back a trailing part of streamed text so a secret split across flushes is caught before any part of it is published, and the
//! limits. Every secret-shaped value below is built at run time from pieces: no token-shaped literal is committed.
#![allow(clippy::unwrap_used)]

use harness_core::redact::{
    MIN_WINDOW, REDACTED, Redactor, Stream, redact_checked, redact_with_secrets,
};
use std::time::{Duration, Instant};

// ---- stand-ins, made at run time ------------------------------------------------------------------------------------------

const MIXED: &str = "aB3dE5gH7jK9mN1pQ3sT5vW7yZ2cF4hJ6kL8nP0rS2uV4xY6";

/// `n` characters of mixed case letters and digits, starting at `seed` in the pattern (so two stand-ins differ).
fn alnum(n: usize, seed: usize) -> String {
    MIXED
        .chars()
        .cycle()
        .skip(seed % MIXED.len())
        .take(n)
        .collect()
}

fn digits(n: usize) -> String {
    "0123456789".chars().cycle().take(n).collect()
}

fn upper(n: usize) -> String {
    "ABCDEFGHJKLMNPQRSTUVWXYZ234567"
        .chars()
        .cycle()
        .take(n)
        .collect()
}

fn hex(n: usize) -> String {
    "0123456789abcdef".chars().cycle().take(n).collect()
}

fn b64url(n: usize) -> String {
    format!("{}-{}_", alnum(n / 2, 3), alnum(n - n / 2 - 2, 9))
        .chars()
        .take(n)
        .collect()
}

fn pem(kind: &str, lines: usize) -> String {
    let mut s = format!("-----{} {kind} PRIVATE KEY-----\n", "BEGIN");
    for i in 0..lines {
        s.push_str(&alnum(64, i * 5));
        s.push('\n');
    }
    s.push_str(&format!("-----{} {kind} PRIVATE KEY-----", "END"));
    s
}

/// One stand-in: the whole text, the secret inside it that must go, and a part of the context that must stay.
struct Fake {
    name: &'static str,
    text: String,
    secret: String,
    keep: &'static str,
}

fn fake(name: &'static str, prefix: &'static str, secret: String, suffix: &str) -> Fake {
    Fake {
        name,
        text: format!("{prefix}{secret}{suffix}"),
        secret,
        keep: prefix,
    }
}

fn fakes() -> Vec<Fake> {
    let mut v = Vec::new();
    for c in ['p', 'o', 'u', 's', 'r'] {
        v.push(fake(
            "github token",
            "export GH=",
            format!("gh{c}_{}", alnum(36, c as usize)),
            " && gh pr list",
        ));
    }
    v.push(fake(
        "github fine-grained",
        "token in log: ",
        format!("{}_{}_{}", "github_pat", alnum(22, 1), alnum(59, 4)),
        ".",
    ));
    v.push(fake(
        "gitlab",
        "remote add origin # ",
        format!("glpat-{}", alnum(20, 2)),
        "\n",
    ));
    for c in ['b', 'p', 'a', 'r', 's'] {
        v.push(fake(
            "slack",
            "SLACK=",
            format!(
                "xox{c}-{}-{}-{}",
                digits(12),
                digits(13),
                alnum(24, c as usize)
            ),
            "\n",
        ));
    }
    v.push(fake(
        "npm",
        "publish with ",
        format!("npm_{}", alnum(36, 6)),
        " now",
    ));
    v.push(fake(
        "openai",
        "key is ",
        format!("sk-{}", alnum(48, 7)),
        ", ok",
    ));
    v.push(fake(
        "stripe",
        "stripe ",
        format!("sk_live_{}", alnum(24, 8)),
        " end",
    ));
    v.push(fake(
        "google api key",
        "maps key ",
        format!("AIza{}", b64url(35)),
        " end",
    ));
    v.push(fake(
        "aws access key id",
        "id ",
        format!("AK{}{}", "IA", upper(16)),
        " end",
    ));
    v.push(fake(
        "aws temporary key id",
        "id ",
        format!("AS{}{}", "IA", upper(16)),
        " end",
    ));
    v.push(fake(
        "jwt",
        "cookie jar: ",
        format!("eyJ{}.eyJ{}.{}", b64url(20), b64url(30), b64url(43)),
        " end",
    ));
    v.push(fake(
        "jwt without signature",
        "t=",
        format!("eyJ{}.eyJ{}.", b64url(20), b64url(30)),
        " end",
    ));
    v.push(fake(
        "bearer",
        "curl -H 'Authorization: Bearer ",
        alnum(40, 3),
        "' https://x",
    ));
    v.push(fake(
        "basic",
        "Authorization: Basic ",
        format!("{}==", alnum(30, 11)),
        "\n",
    ));
    v.push(fake(
        "token scheme",
        "Authorization: Token ",
        alnum(30, 12),
        "\n",
    ));
    v.push(fake(
        "digest scheme",
        "authorization=Digest ",
        alnum(30, 13),
        "\n",
    ));
    v.push(fake(
        "cookie",
        "Cookie: ",
        format!("session={}; theme=dark", alnum(32, 14)),
        "\n",
    ));
    v.push(fake(
        "set-cookie",
        "Set-Cookie: ",
        format!("sid={}; Path=/; HttpOnly", alnum(32, 15)),
        "\n",
    ));
    v.push(fake(
        "npmrc auth token",
        "//registry.example.org/:_authToken=",
        alnum(36, 16),
        "\n",
    ));
    v.push(fake(
        "npmrc _auth",
        "_auth=",
        format!("{}==", alnum(30, 17)),
        "\n",
    ));
    v.push(fake("npmrc _password", "_password=", alnum(20, 18), "\n"));
    v.push(fake("password", "password=", alnum(14, 19), " "));
    v.push(fake("passwd json", "{\"passwd\": \"", alnum(14, 20), "\"}"));
    v.push(fake("pwd", "pwd=", alnum(12, 21), ";"));
    v.push(fake("pass", "pass: ", alnum(12, 22), "\n"));
    v.push(fake("secret", "client_secret = ", alnum(30, 23), "\n"));
    v.push(fake("api key", "API_KEY=", alnum(30, 24), "\n"));
    v.push(fake(
        "aws secret key",
        "aws_secret_access_key = ",
        alnum(40, 25),
        "\n",
    ));
    v.push(fake(
        "url userinfo",
        "git clone https://alice:",
        alnum(18, 26),
        "@host.example/r.git",
    ));
    v.push(fake(
        "db url",
        "DATABASE_URL=postgres://app:",
        alnum(16, 27),
        "@db:5432/app",
    ));
    v.push(fake(
        "presigned signature",
        "https://b.s3.amazonaws.com/k?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Signature=",
        hex(64),
        "&x=1",
    ));
    v.push(fake(
        "presigned token",
        "u?X-Amz-Security-Token=",
        alnum(60, 28),
        "&y=2",
    ));
    v.push(fake(
        "presigned credential",
        "u?X-Amz-Credential=",
        format!("{}%2F20261016%2Fus-east-1", upper(20)),
        "&y=2",
    ));
    v.push(fake(
        "signature query",
        "https://cdn.example/a?Expires=1&Signature=",
        b64url(40),
        "&Key-Pair-Id=K1",
    ));
    v.push(fake(
        "aws access key query",
        "u?AWSAccessKeyId=",
        upper(20),
        "&Expires=1",
    ));
    v.push(fake(
        "long base64 after equals",
        "blob=",
        alnum(88, 29),
        "\n",
    ));
    v.push(fake(
        "long base64 after colon",
        "data: ",
        alnum(60, 30),
        "\n",
    ));
    v.push(fake("long hex after colon", "digest: ", hex(64), "\n"));
    v.push(fake("rsa key", "key:\n", pem("RSA", 25), "\nafter"));
    v.push(fake(
        "openssh key",
        "id_ed25519:\n",
        pem("OPENSSH", 6),
        "\nafter",
    ));
    v.push(fake("ec key", "", pem("EC", 4), "\nafter"));
    v.push(fake(
        "pgp key",
        "",
        pem("PGP", 4).replace("PGP PRIVATE KEY", "PGP PRIVATE KEY BLOCK"),
        "\nafter",
    ));
    v
}

fn r() -> Redactor {
    Redactor::new(Vec::new())
}

/// Equality that reports where two long texts first differ instead of printing both.
fn same(a: &str, b: &str, what: &str) {
    if a != b {
        let i = a
            .bytes()
            .zip(b.bytes())
            .position(|(x, y)| x != y)
            .unwrap_or(a.len().min(b.len()));
        let show = |s: &str| {
            let from = (0..=i.saturating_sub(40))
                .rev()
                .find(|k| s.is_char_boundary(*k))
                .unwrap_or(0);
            let to = (i + 60..=s.len().min(i + 60))
                .find(|k| s.is_char_boundary(*k))
                .unwrap_or(s.len());
            s[from..to.max(from)].to_string()
        };
        panic!(
            "{what}: first difference at byte {i} (lengths {} and {})\n  left : {:?}\n  right: {:?}",
            a.len(),
            b.len(),
            show(a),
            show(b)
        );
    }
}

// ---- the classes, whole ---------------------------------------------------------------------------------------------------

#[test]
fn there_are_at_least_thirty_stand_ins_and_they_are_not_all_one_class() {
    let all = fakes();
    assert!(all.len() >= 30, "{} stand-ins", all.len());
    let names: std::collections::BTreeSet<_> = all.iter().map(|f| f.name).collect();
    assert!(names.len() >= 25, "{names:?}");
}

#[test]
fn every_stand_in_loses_its_secret_and_keeps_its_context() {
    for f in fakes() {
        let out = r().redact(&f.text);
        assert!(
            !out.contains(&f.secret),
            "{}: the secret is still there: {out}",
            f.name
        );
        assert!(out.contains(REDACTED), "{}: {out}", f.name);
        assert!(out.contains(f.keep), "{}: lost its context: {out}", f.name);
        for part in f.secret.as_bytes().windows(14).step_by(7) {
            let part = std::str::from_utf8(part).unwrap();
            assert!(
                !out.contains(part),
                "{}: a part of the secret remains: {out}",
                f.name
            );
        }
    }
}

#[test]
fn redaction_is_idempotent() {
    for f in fakes() {
        let once = r().redact(&f.text);
        assert_eq!(r().redact(&once), once, "{}", f.name);
    }
}

#[test]
fn a_private_key_block_goes_whole_and_the_text_around_it_stays() {
    let key = pem("RSA", 25);
    let out = r().redact(&format!("before\n{key}\nafter"));
    assert_eq!(out, format!("before\n{REDACTED}\nafter"));
}

#[test]
fn a_lone_begin_marker_hides_everything_to_the_end_of_the_text() {
    let body = alnum(300, 3);
    let text = format!(
        "head\n-----{} PRIVATE KEY-----\n{body}\nmore text that is part of it",
        "BEGIN"
    );
    let out = r().redact(&text);
    assert_eq!(out, format!("head\n{REDACTED}"));
}

#[test]
fn two_blocks_and_text_between_them() {
    let text = format!("a\n{}\nb\n{}\nc", pem("RSA", 3), pem("EC", 3));
    assert_eq!(
        r().redact(&text),
        format!("a\n{REDACTED}\nb\n{REDACTED}\nc")
    );
}

#[test]
fn the_session_secrets_go_wherever_they_are_and_short_ones_are_ignored() {
    let secret = alnum(40, 5);
    let rd = Redactor::new(vec![secret.clone(), "short".into(), String::new()]);
    let out = rd.redact(&format!("x {secret} y {secret}z short short"));
    assert_eq!(out, format!("x {REDACTED} y {REDACTED}z short short"));
    assert_eq!(redact_with_secrets("v", std::slice::from_ref(&secret)), "v");
}

#[test]
fn redact_checked_returns_the_text_and_never_panics() {
    assert_eq!(
        redact_checked(&r(), "nothing here").unwrap(),
        "nothing here"
    );
    let f = fakes().remove(0);
    assert!(!redact_checked(&r(), &f.text).unwrap().contains(&f.secret));
}

// ---- what it must leave alone ---------------------------------------------------------------------------------------------

#[test]
fn ordinary_code_and_prose_pass_unchanged() {
    let keep = [
        "the tokenizer=bert setting and PASSWORD_FILE=/etc/app/pw stay as written",
        "bypass: the compass: points north; passenger=5",
        "token_type: Bearer, password_hash = compute(x)",
        "uuid 550e8400-e29b-41d4-a716-446655440000 and ulid 01M4ZHFBV5SDX7J92E6YRH4QTW",
        "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        "cwd: /home/user/projects/some-project/worktrees/some-branch/target/debug",
        "name = \"a-rather-long-kebab-case-package-name-in-a-manifest-file\"",
        "version: 1.38.0, ghp is a prefix, sk-short, Bearer tok",
        "fn main() { let password = ask(); }",
        "Authorization header missing",
        "see https://example.com/docs/path?page=2&sort=asc",
    ];
    for k in keep {
        assert_eq!(r().redact(k), k, "{k}");
    }
}

#[test]
fn source_code_idioms_that_name_a_secret_but_hold_none_pass_unchanged() {
    let keep = [
        "pub next_token: Option<String>,",
        "    pub credentials: ProcessCredentials,",
        "let token = self.token(ciphertext)?;",
        "let mut secret = Secret::new(text);",
        "report.next_token = page_token;",
        "secret_access_key: c.secret_access_key,",
        "pass: self.passes,",
        "const SECRET: &str = SECRET_NAME;",
        "if (token == null) { return; }",
        "password: required",
        "\"password\": null,",
    ];
    for k in keep {
        assert_eq!(r().redact(k), k, "{k}");
    }
    // and the neighbours that are secrets still go
    for hide in [
        "password=hunter2hunter",
        "secret: correct-horse-battery",
        concat!("token = '", "abcdef", "123456", "'"),
        "api_key=user_123456",
    ] {
        assert!(r().redact(hide).contains(REDACTED), "{hide}");
    }
}

#[test]
fn a_git_commit_id_survives_in_its_contexts_and_a_bare_hex_digest_does_not() {
    let sha = hex(40);
    for ctx in [
        format!("commit: {sha}"),
        format!("HEAD: {sha}"),
        format!("HEAD={sha}"),
        format!("blob: {sha}"),
        format!("[main {sha}]"),
        format!("commit {sha}\nAuthor: x"),
    ] {
        assert_eq!(r().redact(&ctx), ctx, "{ctx}");
    }
    let digest = format!("digest: {}", hex(64));
    assert!(r().redact(&digest).contains(REDACTED));
    let sha_elsewhere = format!("key={sha}");
    assert!(
        r().redact(&sha_elsewhere).contains(REDACTED),
        "a 40-hex value after a key name is not a commit"
    );
}

// ---- the work stays linear ------------------------------------------------------------------------------------------------

#[test]
fn a_long_document_is_scanned_in_time_that_grows_with_its_length() {
    // a source file or a log of 256 KiB with a few secrets in it, and the same text four times as long
    let page = |n: usize| -> String {
        let mut out = String::new();
        for i in 0..n {
            out.push_str(&format!(
                "fn handler_{i}(request: Request) -> Response {{ // line {i} of the module\n"
            ));
            if i % 50 == 0 {
                out.push_str(&format!("    let password = \"{}\";\n", alnum(16, i)));
            }
            out.push_str("    respond(request.body(), 200)\n}\n");
        }
        out
    };
    let (small, large) = (page(2_500), page(10_000));
    assert!(small.len() > 200_000);
    let t = Instant::now();
    let a = r().redact(&small);
    let small_time = t.elapsed();
    let t = Instant::now();
    let b = r().redact(&large);
    let large_time = t.elapsed();
    assert!(a.contains(REDACTED) && b.contains(REDACTED));
    assert!(large_time < Duration::from_secs(20), "{large_time:?}");
    // four times the text takes about four times as long (a generous factor for a loaded host)
    assert!(
        large_time <= small_time * 16 + Duration::from_secs(1),
        "{small_time:?} then {large_time:?}"
    );
}

// ---- the window -----------------------------------------------------------------------------------------------------------

fn filler(n: usize) -> String {
    // plain prose with multi-byte characters, so a cut can land inside one
    "lorem ipsum dolor sit amet \u{e9}\u{1F600} consectetur adipiscing "
        .chars()
        .cycle()
        .take(n)
        .collect()
}

/// Feeds `text` to a stream in pieces of `piece` characters and returns what each push and the finish emitted.
fn run_stream(
    rd: &Redactor,
    text: &str,
    window: usize,
    max_hold: usize,
    piece: usize,
) -> Vec<String> {
    let mut s = Stream::new(window, max_hold);
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    for c in chars.chunks(piece) {
        out.push(s.push(rd, &c.iter().collect::<String>()));
    }
    out.push(s.finish(rd));
    out
}

#[test]
fn a_window_below_the_longest_rule_context_is_raised() {
    const { assert!(MIN_WINDOW >= 256) };
    let s = Stream::new(1, 10);
    assert_eq!(s.window(), MIN_WINDOW);
    let s = Stream::new(4096, 100);
    assert!(
        s.max_hold() >= s.window(),
        "the hold limit is never below the window"
    );
}

#[test]
fn a_cut_between_the_letters_of_a_name_does_not_make_a_secret_of_what_follows() {
    // `bypass=` is not the name `pass=`; a stream that cut after `by` would hand its held text `pass=...` to the rules.
    let rd = r();
    let text = format!("{} bypass=abcdefghij {}", filler(900), filler(900));
    let whole = rd.redact(&text);
    assert!(whole.contains("bypass=abcdefghij"));
    for piece in [1, 3, 17, 100] {
        same(
            &run_stream(&rd, &text, 300, 8192, piece).concat(),
            &whole,
            &format!("pieces of {piece}"),
        );
    }
}

#[test]
fn a_stream_holds_back_its_window_and_emits_the_rest() {
    let rd = r();
    let mut s = Stream::new(1024, 64 * 1024);
    let text = filler(5000);
    let out = s.push(&rd, &text);
    assert!(
        s.held_len() >= 1024 && s.held_len() <= 1024 + 128 + 8,
        "held {}",
        s.held_len()
    );
    assert_eq!(out.len() + s.held_len(), text.len());
    assert!(text.starts_with(&out));
    let rest = s.finish(&rd);
    assert_eq!(format!("{out}{rest}"), text);
    assert_eq!(s.held_len(), 0);
}

#[test]
fn a_stream_never_cuts_inside_a_character() {
    let rd = r();
    let text = "\u{1F600}".repeat(3000);
    for piece in [1, 2, 5, 333] {
        let out = run_stream(&rd, &text, 1000, 64 * 1024, piece).concat();
        assert_eq!(out, text);
    }
}

#[test]
fn what_a_stream_emits_joined_equals_the_whole_text_redacted_whatever_the_pieces() {
    let rd = r();
    for f in fakes() {
        let text = format!("{}{}{}", filler(900), f.text, filler(900));
        let whole = rd.redact(&text);
        for piece in [1, 2, 3, 7, 50, 333, 5000] {
            let outs = run_stream(&rd, &text, 512, 64 * 1024, piece);
            let joined = outs.concat();
            same(&joined, &whole, &format!("{} in pieces of {piece}", f.name));
        }
    }
}

#[test]
fn no_part_of_a_secret_is_emitted_before_the_whole_of_it_has_been_seen() {
    let rd = r();
    for f in fakes() {
        // on a line of its own: a stand-in glued to the end of a word is not a secret to the rules (`\b`), whole or streamed
        let text = format!("{}\n{}\n{}", filler(700), f.text, filler(700));
        for piece in [1, 4, 40] {
            let mut s = Stream::new(512, 64 * 1024);
            let mut emitted = String::new();
            let chars: Vec<char> = text.chars().collect();
            for c in chars.chunks(piece) {
                emitted.push_str(&s.push(&rd, &c.iter().collect::<String>()));
                for part in f.secret.as_bytes().windows(16).step_by(5) {
                    let part = std::str::from_utf8(part).unwrap();
                    assert!(
                        !emitted.contains(part),
                        "{} in pieces of {piece}: {part:?} was emitted",
                        f.name
                    );
                }
            }
            emitted.push_str(&s.finish(&rd));
            assert!(!emitted.contains(&f.secret), "{}", f.name);
        }
    }
}

#[test]
fn a_secret_split_at_every_position_is_still_caught() {
    let rd = r();
    for f in fakes().into_iter().filter(|f| f.text.len() < 400) {
        let text = format!("{}{}{}", filler(600), f.text, filler(10));
        let whole = rd.redact(&text);
        let cuts: Vec<usize> = text
            .char_indices()
            .map(|(i, _)| i)
            .filter(|i| *i >= 600 - 5 && *i <= 600 + f.text.len() + 5)
            .collect();
        for i in cuts {
            let mut s = Stream::new(300, 64 * 1024);
            let mut out = s.push(&rd, &text[..i]);
            out.push_str(&s.push(&rd, &text[i..]));
            out.push_str(&s.finish(&rd));
            same(&out, &whole, &format!("{} split at {i}", f.name));
        }
    }
}

#[test]
fn a_private_key_longer_than_the_hold_limit_is_hidden_to_its_end() {
    let rd = r();
    let begin = format!("-----{} RSA PRIVATE KEY-----\n", "BEGIN");
    let end = format!("\n-----{} RSA PRIVATE KEY-----", "END");
    let body = alnum(9000, 1);
    let text = format!(
        "{}{begin}{body}{end} and after {}",
        filler(800),
        filler(800)
    );
    let outs = run_stream(&rd, &text, 512, 2048, 100);
    let joined = outs.concat();
    assert!(
        !joined.contains(&body[..40]),
        "no part of the body is emitted"
    );
    assert!(!joined.contains(&body[8000..8040]));
    assert_eq!(joined.matches(REDACTED).count(), 1, "one block, one marker");
    assert!(
        joined.contains(" and after "),
        "the text after the block is emitted again"
    );
    assert!(joined.starts_with(&filler(800)));
}

#[test]
fn a_key_with_no_end_marker_is_hidden_to_the_end_of_the_message() {
    let rd = r();
    let begin = format!("-----{} PRIVATE KEY-----\n", "BEGIN");
    let text = format!("intro {begin}{}", alnum(5000, 2));
    let outs = run_stream(&rd, &text, 512, 1024, 37);
    let joined = outs.concat();
    assert_eq!(joined, format!("intro {REDACTED}"));
}

#[test]
fn a_run_of_token_characters_longer_than_the_hold_limit_is_hidden_and_the_text_after_it_comes_back()
{
    let rd = r();
    let run = alnum(6000, 4);
    let text = format!(
        "{}key={run} then plain words follow {}",
        filler(700),
        filler(700)
    );
    let outs = run_stream(&rd, &text, 512, 1500, 64);
    let joined = outs.concat();
    assert!(!joined.contains(&run[..30]));
    assert!(!joined.contains(&run[5000..5030]));
    assert!(joined.contains("key="));
    assert!(joined.contains(" then plain words follow "));
    assert_eq!(
        joined.matches(REDACTED).count(),
        1,
        "{}",
        &joined[joined.len().saturating_sub(120)..]
    );
}

#[test]
fn two_streams_are_independent() {
    let rd = r();
    let a = format!("{}password={}{}", filler(600), alnum(20, 1), filler(600));
    let b = format!("{}Cookie: s={}{}", filler(600), alnum(20, 2), filler(600));
    let mut sa = Stream::new(300, 8192);
    let mut sb = Stream::new(300, 8192);
    let (mut oa, mut ob) = (String::new(), String::new());
    let (ca, cb): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    for i in (0..ca.len().max(cb.len())).step_by(11) {
        if i < ca.len() {
            oa.push_str(&sa.push(
                &rd,
                &ca[i..(i + 11).min(ca.len())].iter().collect::<String>(),
            ));
        }
        if i < cb.len() {
            ob.push_str(&sb.push(
                &rd,
                &cb[i..(i + 11).min(cb.len())].iter().collect::<String>(),
            ));
        }
    }
    oa.push_str(&sa.finish(&rd));
    ob.push_str(&sb.finish(&rd));
    same(&oa, &rd.redact(&a), "stream a");
    same(&ob, &rd.redact(&b), "stream b");
}

#[test]
fn the_session_secrets_are_caught_across_a_split_too() {
    let secret = alnum(40, 9);
    let rd = Redactor::new(vec![secret.clone()]);
    let text = format!("{}{secret}{}", filler(900), filler(900));
    let whole = rd.redact(&text);
    assert!(!whole.contains(&secret));
    for piece in [1, 9, 100] {
        same(
            &run_stream(&rd, &text, 512, 8192, piece).concat(),
            &whole,
            &format!("pieces of {piece}"),
        );
    }
}

// ---- the spans ------------------------------------------------------------------------------------------------------------

#[test]
fn spans_are_ordered_disjoint_inside_the_text_and_replace_exactly_what_redact_replaces() {
    for f in fakes() {
        let text = format!("lead {} tail", f.text);
        let spans = r().spans(&text);
        assert!(!spans.is_empty(), "{}", f.name);
        let mut prev_end = 0;
        for s in &spans {
            assert!(
                s.lead <= s.start && s.start < s.end && s.end <= text.len(),
                "{}: {s:?}",
                f.name
            );
            assert!(
                text.is_char_boundary(s.lead)
                    && text.is_char_boundary(s.start)
                    && text.is_char_boundary(s.end)
            );
            assert!(s.lead >= prev_end, "{}: spans overlap: {spans:?}", f.name);
            prev_end = s.end;
        }
        let mut rebuilt = String::new();
        let mut at = 0;
        for s in &spans {
            rebuilt.push_str(&text[at..s.start]);
            rebuilt.push_str(REDACTED);
            at = s.end;
        }
        rebuilt.push_str(&text[at..]);
        assert_eq!(rebuilt, r().redact(&text), "{}", f.name);
    }
}

#[test]
fn a_span_that_reaches_the_end_of_the_text_is_open_and_one_that_does_not_is_closed() {
    let rd = r();
    let tok = format!("gh{}_{}", 'p', alnum(36, 1));
    let open = rd.spans(&format!("x {tok}"));
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].end, "x ".len() + tok.len());
    let closed = rd.spans(&format!("x {tok} y"));
    assert_eq!(closed[0].end, "x ".len() + tok.len());
    assert!(closed[0].end < "x  y".len() + tok.len());
}
