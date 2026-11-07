//! The stored record is redacted on the worker; this is the second line, so a token that got past it is not drawn. Every fake below is
//! built at run time from pieces, so no source line is shaped like a credential.

#![allow(clippy::unwrap_used)]

use ham_core::redact::{REDACTED, redact_tokens, redact_with_work};
use proptest::prelude::*;

fn rep(c: char, n: usize) -> String {
    std::iter::repeat_n(c, n).collect()
}

fn gh(kind: &str) -> String {
    format!("{}{}_{}", "g", "h".to_string() + kind, rep('a', 36))
}

#[test]
fn github_tokens_of_every_kind_are_hidden() {
    for kind in ["p", "o", "u", "s", "r"] {
        let t = gh(kind);
        let out = redact_tokens(&format!("clone with {t} now"));
        assert_eq!(out, format!("clone with {REDACTED} now"), "{kind}");
    }
    let pat = format!("{}_{}_{}", "github_pat", rep('A', 22), rep('b', 59));
    assert_eq!(
        redact_tokens(&format!("x {pat} y")),
        format!("x {REDACTED} y")
    );
}

#[test]
fn gitlab_slack_and_google_tokens_are_hidden() {
    let gl = format!("{}-{}", "glpat", rep('x', 20));
    let slack = format!(
        "{}-{}-{}-{}",
        "xoxb",
        "1234567890",
        "1234567890123",
        rep('q', 24)
    );
    let google = format!("{}{}", "AIza", rep('S', 35));
    for t in [gl, slack, google] {
        assert_eq!(redact_tokens(&format!("[{t}]")), format!("[{REDACTED}]"));
    }
}

#[test]
fn aws_access_key_ids_are_hidden_and_other_aws_identifiers_are_kept() {
    for prefix in ["AKIA", "ASIA"] {
        // Built from two halves and a run of digits: no line of this file is a key id.
        let id = format!("{}{}{}", &prefix[..2], &prefix[2..], rep('7', 16));
        let out = redact_tokens(&format!("key {id} here"));
        assert_eq!(out, format!("key {REDACTED} here"), "{prefix}");
    }
    // Role, user, group and policy ids name things, they do not sign anything.
    for prefix in ["AROA", "AIDA", "AGPA", "ANPA"] {
        let id = format!("{prefix}{}", rep('7', 16));
        assert_eq!(redact_tokens(&id), id, "{prefix}");
    }
}

#[test]
fn json_web_tokens_are_hidden() {
    let jwt = format!("eyJ{}.eyJ{}.{}", rep('h', 20), rep('p', 30), rep('s', 43));
    assert_eq!(
        redact_tokens(&format!("token was {jwt}.")),
        format!("token was {REDACTED}.")
    );
}

#[test]
fn bearer_credentials_and_authorization_headers_lose_their_value() {
    let tok = format!("{}7", rep('z', 29));
    assert_eq!(
        redact_tokens(&format!("Authorization: Bearer {tok}")),
        format!("Authorization: Bearer {REDACTED}")
    );
    assert_eq!(
        redact_tokens(&format!("curl -H 'authorization: Basic {}'", rep('Q', 24))),
        format!("curl -H 'authorization: Basic {REDACTED}'")
    );
    assert_eq!(
        redact_tokens(&format!("sent bearer {tok} twice")),
        format!("sent bearer {REDACTED} twice")
    );
}

#[test]
fn private_key_blocks_are_hidden_whole() {
    let begin = format!("{}{}", "-----BEGIN ", "RSA PRIVATE KEY-----");
    let end = format!("{}{}", "-----END ", "RSA PRIVATE KEY-----");
    let text = format!(
        "before\n{begin}\nMIIEowIBAAKCAQEA{}\n{end}\nafter",
        rep('k', 60)
    );
    assert_eq!(
        redact_tokens(&text),
        "before\n[redacted private key]\nafter"
    );
    // An unfinished block hides everything after its header.
    let cut = format!("before\n{begin}\nMIIEow{}", rep('k', 10));
    assert_eq!(redact_tokens(&cut), "before\n[redacted private key]");
}

#[test]
fn a_password_in_a_url_is_hidden_and_a_user_name_alone_is_kept() {
    // The secret is made at run time, so this file holds no URL with a password in it.
    let url = format!(
        "clone https://alice:{}@example.com/x.git please",
        rep('p', 8)
    );
    assert_eq!(
        redact_tokens(&url),
        format!("clone https://{REDACTED}@example.com/x.git please")
    );
    assert_eq!(
        redact_tokens("git@example.com:x.git and ssh://git@example.com/x.git"),
        "git@example.com:x.git and ssh://git@example.com/x.git"
    );
}

#[test]
fn a_presigned_query_is_cut_at_the_question_mark() {
    let url = format!(
        "https://bucket.s3.us-east-1.amazonaws.com/runs/x.zip?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=c&X-Amz-Signature={}",
        rep('a', 64)
    );
    assert_eq!(
        redact_tokens(&format!("download {url} now")),
        format!("download https://bucket.s3.us-east-1.amazonaws.com/runs/x.zip?{REDACTED} now")
    );
    let old = "https://b.s3.amazonaws.com/k?AWSAccessKeyId=abc&Signature=def&Expires=1";
    assert_eq!(
        redact_tokens(old),
        format!("https://b.s3.amazonaws.com/k?{REDACTED}")
    );
}

#[test]
fn credential_names_with_a_value_lose_the_value() {
    let upper = ["PASS", "WORD"].concat();
    for (text, want) in [
        (
            "password=hunter2 next".to_string(),
            format!("password={REDACTED} next"),
        ),
        (format!("{upper}: s3cret!"), format!("{upper}: {REDACTED}")),
        (
            "api_key=abcd1234".to_string(),
            format!("api_key={REDACTED}"),
        ),
        (
            "aws_secret_access_key = wJalrXUtnFEMI".to_string(),
            format!("aws_secret_access_key = {REDACTED}"),
        ),
        (
            "client_secret=\"abc def\" x".to_string(),
            format!("client_secret={REDACTED} x"),
        ),
    ] {
        assert_eq!(redact_tokens(&text), want, "{text}");
    }
}

#[test]
fn a_quoted_value_of_a_credential_name_is_hidden_even_when_it_is_all_letters() {
    let t = format!("{{\"aws_secret_access_key\": \"{}\"}}", rep('A', 40));
    assert_eq!(
        redact_tokens(&t),
        format!("{{\"aws_secret_access_key\": {REDACTED}}}")
    );
    // Unquoted, after a colon, a plain word is prose.
    assert_eq!(redact_tokens("secret: required"), "secret: required");
}

#[test]
fn authorization_credentials_are_hidden_whatever_the_spacing_or_length() {
    let tok = rep('z', 16);
    for (text, want) in [
        (
            format!("Authorization: Bearer\t{tok}"),
            format!("Authorization: Bearer\t{REDACTED}"),
        ),
        (
            format!("Authorization: Bearer  {tok}"),
            format!("Authorization: Bearer  {REDACTED}"),
        ),
        (
            "Authorization: Basic dTpw".to_string(),
            format!("Authorization: Basic {REDACTED}"),
        ),
        (
            "authorization=Bearer abcd".to_string(),
            format!("authorization=Bearer {REDACTED}"),
        ),
    ] {
        assert_eq!(redact_tokens(&text), want, "{text}");
    }
}

#[test]
fn npm_tokens_and_auth_lines_are_hidden() {
    let tok = format!("{}{}", "npm_", rep('a', 36));
    assert_eq!(
        redact_tokens(&format!("use {tok} now")),
        format!("use {REDACTED} now")
    );
    let line = format!("//registry.npmjs.org/:_authToken={}", rep('q', 20));
    assert_eq!(
        redact_tokens(&line),
        format!("//registry.npmjs.org/:_authToken={REDACTED}")
    );
    let line = format!("//registry.example.org/:_auth={}", rep('Q', 24));
    assert_eq!(
        redact_tokens(&line),
        format!("//registry.example.org/:_auth={REDACTED}")
    );
}

#[test]
fn a_json_web_token_with_a_tiny_payload_is_hidden() {
    let jwt = format!("eyJ{}.e30.{}", rep('h', 17), rep('s', 43));
    assert_eq!(
        redact_tokens(&format!("t={jwt};")),
        format!("t={REDACTED};")
    );
}

#[test]
fn a_pgp_private_key_block_is_hidden_whole() {
    let begin = format!("{}{}", "-----BEGIN ", "PGP PRIVATE KEY BLOCK-----");
    let end = format!("{}{}", "-----END ", "PGP PRIVATE KEY BLOCK-----");
    let text = format!("a\n{begin}\nabc123\n{end}\nb");
    assert_eq!(redact_tokens(&text), "a\n[redacted private key]\nb");
    let public = format!("{}{}", "-----BEGIN ", "PGP PUBLIC KEY BLOCK-----");
    assert_eq!(redact_tokens(&public), public);
}

#[test]
fn a_password_in_a_url_is_hidden_even_with_a_closing_bracket_in_it() {
    let pw = format!("pa){}", rep('s', 4));
    let url = format!("https://user:{pw}@example.com/x");
    assert_eq!(
        redact_tokens(&url),
        format!("https://{REDACTED}@example.com/x")
    );
}

#[test]
fn signed_urls_of_other_clouds_are_cut_at_the_question_mark() {
    for q in [
        "?Expires=123&Signature=abc123&Key-Pair-Id=K123",
        "?X-Goog-Signature=abc123&X-Goog-Algorithm=GOOG4-RSA-SHA256",
        "?sv=2023-11-03&ss=b&sig=abc123",
    ] {
        let url = format!("https://host.example/a/b{q} next");
        assert_eq!(
            redact_tokens(&url),
            format!("https://host.example/a/b?{REDACTED} next"),
            "{q}"
        );
    }
    assert_eq!(
        redact_tokens("https://x.example/?sv=2&a=1"),
        "https://x.example/?sv=2&a=1"
    );
}

#[test]
fn ordinary_words_after_a_scheme_name_or_a_prefix_are_kept() {
    assert_eq!(
        redact_tokens("Bearer ofgreatresponsibility"),
        "Bearer ofgreatresponsibility"
    );
    assert_eq!(
        redact_tokens("sk-this-is-an-ordinary-variable-name"),
        "sk-this-is-an-ordinary-variable-name"
    );
}

#[test]
fn pathological_text_costs_work_in_proportion_to_its_length() {
    // The engine writes this text; a scan that restarts at every character would freeze the screen.
    let n = 20_000usize;
    let cases: Vec<(&str, String)> = vec![
        ("question marks", "?".repeat(n)),
        ("name equals dollar", "password=$".repeat(n / 10)),
        ("jwt starts", "eyJ-".repeat(n / 4)),
        ("github_pat starts", "github_pat_".repeat(n / 11)),
        ("scheme starts", "://".repeat(n / 3)),
        ("userinfo starts", "a://b:".repeat(n / 6)),
        ("bearer starts", "bearer ".repeat(n / 7)),
        ("quotes", "secret=\"".repeat(n / 8)),
        ("begin markers", "-----BEGIN ".repeat(n / 11)),
        ("one long run", "a".repeat(n)),
    ];
    for (name, text) in cases {
        let (out, work) = redact_with_work(&text);
        assert!(
            work <= 64 * text.len() as u64 + 4096,
            "{name}: {work} steps for {} characters",
            text.len()
        );
        assert_eq!(redact_tokens(&out), out, "{name}: not stable");
    }
}

#[test]
fn text_that_only_looks_a_little_like_a_secret_is_kept() {
    for t in [
        "commit 0123456789abcdef0123456789abcdef01234567 fixed it",
        "run 01M5A2R7K3C9XJ4D8E6FQ0WT1N started",
        "uuid 123e4567-e89b-12d3-a456-426614174000",
        "ghp_short and glpat-short and xoxb-1",
        "the password field is required; set a password policy",
        "token: 5 of 12 done",
        "no secrets here, only https://example.com/path?a=1&b=2",
        "AKIA is a prefix; so is eyJ",
        "sha256 e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    ] {
        assert_eq!(redact_tokens(t), t);
    }
}

#[test]
fn several_secrets_in_one_text_are_all_hidden() {
    let t = format!("a {} b {} c", gh("p"), gh("s"));
    assert_eq!(redact_tokens(&t), format!("a {REDACTED} b {REDACTED} c"));
}

proptest! {
    #[test]
    fn redaction_is_idempotent(s in ".{0,200}") {
        let once = redact_tokens(&s);
        prop_assert_eq!(redact_tokens(&once), once.clone());
    }

    #[test]
    fn a_fake_token_inside_random_text_is_always_hidden(pre in "[a-z ]{0,30}", post in "[a-z ]{0,30}") {
        let t = gh("p");
        let out = redact_tokens(&format!("{pre} {t} {post}"));
        prop_assert!(!out.contains(&t));
        prop_assert!(out.contains(REDACTED));
    }
}
