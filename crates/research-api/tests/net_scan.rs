//! The outbound scanner, the URL rules and the text cleaning. Every credential-shaped stand-in is built at run
//! time from pieces, so no key-shaped literal sits in the tree.

#![allow(clippy::unwrap_used)]

use harness_protocol::research::ResearchErrorCode as C;
use proptest::prelude::*;
use research_api::scan::{
    CheckedUrl, ScanKind, canonical_url, check_url, clean_text, excerpt, extract_links,
    github_repo, scan_outbound, url_hash,
};

fn cat(parts: &[&str]) -> String {
    parts.concat()
}

/// `n` characters of a repeating mixed-case alphanumeric pattern (digits, upper and lower case all present).
fn mixed(n: usize) -> String {
    "aB3dE5fG7hJ9kL1mN2pQ4rS6tU8vW0xY"
        .chars()
        .cycle()
        .take(n)
        .collect()
}

fn refused(text: &str, kind: ScanKind, own: &[&str]) -> String {
    let err = scan_outbound(text, kind, own).unwrap_err();
    assert_eq!(err.code, C::RefusedSecret, "{text:?}");
    assert!(
        !err.message.contains(text) || text.len() < 8,
        "the message never echoes the text: {}",
        err.message
    );
    err.message
}

// ---- credentials in outbound text ------------------------------------------------------------------------------------------------

#[test]
fn an_aws_access_key_id_is_refused_in_every_kind() {
    let id = cat(&["AS", "IA", "QRSTUVWXYZ234567"]);
    assert_eq!(id.len(), 20);
    let m = refused(&format!("why does {id} fail"), ScanKind::Query, &[]);
    assert!(m.contains("aws-access-key"), "{m}");
    refused(&id, ScanKind::Url, &[]);
    refused(&id, ScanKind::RepoPart, &[]);
    // Not an access key id: lower case, too short, or glued to a longer word.
    let short = cat(&["AK", "IA", "QRST"]);
    assert!(scan_outbound(&short, ScanKind::Query, &[]).is_ok());
    let lower = cat(&["ak", "iaqrstuvwxyz234567"]);
    assert!(scan_outbound(&lower, ScanKind::Query, &[]).is_ok());
}

#[test]
fn a_forty_character_aws_secret_in_a_query_is_refused() {
    let secret = mixed(40);
    let m = refused(&format!("secret {secret} leaked"), ScanKind::Query, &[]);
    assert!(m.contains("aws-secret-key"), "{m}");
    let with_slash = cat(&[&mixed(18), "/", &mixed(21)]);
    assert_eq!(with_slash.len(), 40);
    refused(&with_slash, ScanKind::Query, &[]);
    // Longer runs are refused as long base64, shorter ones pass.
    let m = refused(&mixed(64), ScanKind::Query, &[]);
    assert!(m.contains("long-base64"), "{m}");
    assert!(scan_outbound(&mixed(39), ScanKind::Query, &[]).is_ok());
    // Needs digits, upper and lower case: an all-lower word of 45 letters is a word, not a key.
    assert!(scan_outbound(&"abcdefghij".repeat(5), ScanKind::Query, &[]).is_ok());
    assert!(scan_outbound(&"ABCDEFGHIJ".repeat(5), ScanKind::Query, &[]).is_ok());
    // A deep path is not a key: four or more slashes.
    let path = "packages/Core/src/Tests2/fixtures/Models/NodeTypes3/index";
    assert!(scan_outbound(path, ScanKind::Query, &[]).is_ok());
}

#[test]
fn url_and_repo_parts_refuse_base64_runs_of_48_but_not_40() {
    let run48 = "A1".repeat(12) + &"b2".repeat(12);
    assert_eq!(run48.len(), 48);
    refused(&format!("https://example.org/{run48}"), ScanKind::Url, &[]);
    refused(&run48, ScanKind::RepoPart, &[]);
    let run40 = mixed(40);
    assert!(scan_outbound(&format!("https://example.org/{run40}"), ScanKind::Url, &[]).is_ok());
    // Slashes and hyphens end a run: a long readable path is fine.
    assert!(
        scan_outbound(
            "https://example.org/Docs/Reference/Version2/Some-Very-Long-Page-Title-About-Configuration-Options-v3",
            ScanKind::Url,
            &[]
        )
        .is_ok()
    );
}

#[test]
fn hex_runs_follow_the_kind() {
    let hex32 = "0123456789abcdef".repeat(2);
    let hex40 = "0123456789abcdef0123456789abcdef01234567";
    let hex41 = "0123456789abcdef0123456789abcdef012345678";
    // A query refuses 32 or more.
    let m = refused(&format!("token {hex32}"), ScanKind::Query, &[]);
    assert!(m.contains("long-hex"), "{m}");
    refused(hex40, ScanKind::Query, &[]);
    assert!(scan_outbound(&hex32[..31], ScanKind::Query, &[]).is_ok());
    // A URL or repository part allows a 40-hex commit id and refuses longer.
    assert!(
        scan_outbound(
            &format!("https://github.com/o/r/commit/{hex40}"),
            ScanKind::Url,
            &[]
        )
        .is_ok()
    );
    assert!(scan_outbound(hex40, ScanKind::RepoPart, &[]).is_ok());
    refused(&format!("https://example.org/{hex41}"), ScanKind::Url, &[]);
    refused(&"ab".repeat(32), ScanKind::RepoPart, &[]);
    // A UUID has hyphens and short hex groups.
    assert!(scan_outbound("123e4567-e89b-12d3-a456-426614174000", ScanKind::Query, &[]).is_ok());
}

#[test]
fn vendor_tokens_are_refused_by_class() {
    let tail36 = mixed(36);
    for (text, class) in [
        (cat(&["gh", "p_", &tail36]), "github-token"),
        (cat(&["gl", "pat-", &mixed(22)]), "gitlab-token"),
        (
            cat(&["xo", "xb-", "1234567890-", &mixed(20)]),
            "slack-token",
        ),
        (
            cat(&["-----BEGIN ", "RSA PRIVATE", " KEY-----"]),
            "private-key",
        ),
        (
            cat(&[
                "ey",
                "JhbGciOiJIUzI1NiJ9.",
                "ey",
                "JzdWIiOiIxMjM0In0.",
                "c2lnbmF0dXJlMTIz",
            ]),
            "jwt",
        ),
        (cat(&["Bea", "rer ", &mixed(30)]), "authorization"),
        (
            cat(&["Ba", "sic ", "dXNlcjpwYXNzd29yZA=="]),
            "authorization",
        ),
    ] {
        let m = refused(&format!("how do I use {text} here"), ScanKind::Query, &[]);
        assert!(m.contains(class), "{class}: {m}");
        refused(&text, ScanKind::Url, &[]);
        refused(&text, ScanKind::RepoPart, &[]);
    }
}

#[test]
fn near_misses_of_the_vendor_prefixes_pass() {
    for text in [
        "ghp_ is the prefix of a GitHub personal token",
        "the xoxb- prefix means a bot token",
        "glpat- tokens are GitLab personal access tokens",
        "set the Bearer token in the Authorization header",
        "Basic authentication versus Digest authentication",
        "basic usage of the Bearer scheme",
        "eyJ is how every JWT header starts",
        "-----BEGIN CERTIFICATE----- parsing in node",
        "what is a private key in PGP",
        "ghpx_short ghs_tiny",
    ] {
        assert!(scan_outbound(text, ScanKind::Query, &[]).is_ok(), "{text}");
    }
}

#[test]
fn the_callers_own_key_id_is_refused_in_every_kind() {
    let own = cat(&["AS", "IA", "TESTSESSION12345"]);
    let own2 = cat(&["AS", "IA", "OTHERSESSION9876"]);
    for kind in [ScanKind::Query, ScanKind::Url, ScanKind::RepoPart] {
        let m = refused(&format!("error with {own} in a log"), kind, &[&own, &own2]);
        assert!(
            m.contains("session-key-id") || m.contains("aws-access-key"),
            "{m}"
        );
    }
    // Shorter than 16 characters is ignored (a short id would match ordinary words); not in the text is fine.
    assert!(scan_outbound("a plain query", ScanKind::Query, &[&own]).is_ok());
    assert!(scan_outbound("shortid12345 here", ScanKind::Query, &["shortid12345"]).is_ok());
    // An id that is not AWS-shaped is still matched as a substring, case-sensitively.
    let odd = "customer-session-token-XYZ";
    refused(&format!("foo {odd} bar"), ScanKind::Query, &[odd]);
    assert!(
        scan_outbound(
            &format!("foo {} bar", odd.to_lowercase()),
            ScanKind::Query,
            &[odd]
        )
        .is_ok()
    );
}

#[test]
fn urls_and_repo_parts_are_decoded_once_before_scanning() {
    let id = cat(&["AS", "IA", "QRSTUVWXYZ234567"]);
    let encoded: String = id.bytes().map(|b| format!("%{b:02X}")).collect();
    refused(
        &format!("https://example.org/?q={encoded}"),
        ScanKind::Url,
        &[],
    );
    refused(&encoded, ScanKind::RepoPart, &[]);
    // A malformed escape stays as it is.
    assert!(scan_outbound("https://example.org/%zz/%4", ScanKind::Url, &[]).is_ok());
}

#[test]
fn fifty_ordinary_queries_pass_in_query_mode() {
    let queries = [
        "is-plain-obj npm package what counts as a plain object",
        "rust tokio select! macro cancellation safety",
        "how to parse a pax extended header in a tar archive",
        "semantic versioning 2.0.0 precedence rules",
        "python 3.12 asyncio TaskGroup exception handling",
        "docker multi-stage build copy --from stage name",
        "kubernetes ingress nginx 502 bad gateway upstream sent too big header",
        "java.lang.OutOfMemoryError: Java heap space spring boot 3.3",
        "git rebase --onto abc1234 def5678 feature",
        "commit 9fceb02 broke the build on main",
        "what does error E0382 borrow of moved value mean",
        "express.js 4.19 body-parser limit option",
        "react 18 useEffect cleanup runs twice in strict mode",
        "postgres 16 jsonb_path_query vs jsonb_array_elements performance",
        "typescript 5.5 inferred type predicates",
        "123e4567-e89b-12d3-a456-426614174000 uuid v4 format",
        "ISO 8601 week number calculation 2026-W42",
        "aws sdk rust StaticReplayClient example",
        "CVE-2025-12345 affected versions of openssl",
        "RFC 9110 http semantics idempotent methods",
        "go 1.23 range over func iterators",
        "maven surefire fork count parallel tests",
        "gradle 8.10 configuration cache incompatible task",
        "lodash isPlainObject implementation",
        "difference between sha256 and sha-512 for file integrity",
        "vite 5 proxy websocket upgrade",
        "tailwind css v4 container queries",
        "ffmpeg -i input.mp4 -vf scale=1280:-2 output.mp4",
        "regex lookbehind support in javascript engines",
        "SELECT * FROM orders WHERE created_at > now() - interval '7 days'",
        "terraform aws_s3_bucket_policy deny insecure transport",
        "cargo deny advisories ignore RUSTSEC-2024-0001",
        "how to read /proc/pressure/io on linux",
        "nginx proxy_pass trailing slash behaviour",
        "OAuth 2.0 PKCE code_verifier length 43 to 128 characters",
        "bcrypt cost factor 12 vs argon2id memory 64 MiB",
        "sqlite WAL mode checkpoint starvation",
        "numpy 2.0 np.float_ removed use np.float64",
        "pandas read_csv dtype backend pyarrow",
        "swift 6 strict concurrency Sendable closure",
        "kotlin coroutines flow stateIn sharingStarted WhileSubscribed 5000",
        "elasticsearch 8.15 knn search num_candidates",
        "redis 7.4 hash field expiration HEXPIRE",
        "haskell cabal 3.12 multi-repl",
        "ansible 2.17 ansible.builtin.copy backup parameter",
        "unicode normalization NFC vs NFD for file names on macOS",
        "ipv6 unique local address fc00::/7 routing",
        "websocket close code 1006 abnormal closure",
        "zstd dictionary training samples size",
        "protobuf 3 field presence optional keyword",
    ];
    assert_eq!(queries.len(), 50);
    for q in queries {
        assert!(scan_outbound(q, ScanKind::Query, &[]).is_ok(), "{q}");
    }
}

// ---- URL rules ---------------------------------------------------------------------------------------------------------------------

fn accepted(raw: &str) -> CheckedUrl {
    check_url(raw, 2048).unwrap_or_else(|e| panic!("{raw}: {e}"))
}

fn rejected(raw: &str) -> String {
    let e = check_url(raw, 2048).unwrap_err();
    assert_eq!(e.code, C::RefusedUrl, "{raw}");
    e.message
}

#[test]
fn acceptable_urls_are_normalised_the_same_way_every_time() {
    for (raw, canonical, host) in [
        ("https://example.org", "https://example.org/", "example.org"),
        (
            "https://Example.ORG:443/a/b?x=1&y=2#frag",
            "https://example.org/a/b?x=1&y=2",
            "example.org",
        ),
        (
            "HTTPS://WWW.NpmJS.com/package/is-plain-obj",
            "https://www.npmjs.com/package/is-plain-obj",
            "www.npmjs.com",
        ),
        (
            "https://github.com/sindresorhus/is-plain-obj",
            "https://github.com/sindresorhus/is-plain-obj",
            "github.com",
        ),
        (
            "https://example.org./x",
            "https://example.org/x",
            "example.org",
        ),
        (
            "https://example.org?x=1",
            "https://example.org/?x=1",
            "example.org",
        ),
        ("https://a.example/p?", "https://a.example/p?", "a.example"),
        (
            "https://a.example/p?#frag",
            "https://a.example/p?",
            "a.example",
        ),
        (
            "https://example.org?",
            "https://example.org/?",
            "example.org",
        ),
        (
            "https://xn--bcher-kva.example/",
            "https://xn--bcher-kva.example/",
            "xn--bcher-kva.example",
        ),
        (
            "https://example.org/a%2Fb%20c",
            "https://example.org/a%2Fb%20c",
            "example.org",
        ),
        (
            "https://example.org//a/./b/../c",
            "https://example.org//a/./b/../c",
            "example.org",
        ),
        (
            "https://example.org/wiki/Foo_(bar)?q=a+b&z=%41",
            "https://example.org/wiki/Foo_(bar)?q=a+b&z=%41",
            "example.org",
        ),
        (
            "https://example.org/ü",
            "https://example.org/ü",
            "example.org",
        ),
        (
            "https://example.org/p?key=short&a=1",
            "https://example.org/p?key=short&a=1",
            "example.org",
        ),
    ] {
        let c = accepted(raw);
        assert_eq!(c.canonical, canonical, "{raw}");
        assert_eq!(c.host, host, "{raw}");
        assert_eq!(canonical_url(raw).as_deref(), Some(canonical), "{raw}");
        // Normalising a canonical form changes nothing.
        assert_eq!(canonical_url(canonical).as_deref(), Some(canonical));
    }
    // One more query parameter is a different URL; so is a different order.
    assert_ne!(
        canonical_url("https://example.org/p?a=1"),
        canonical_url("https://example.org/p?a=1&b=2")
    );
    assert_ne!(
        canonical_url("https://example.org/p?a=1&b=2"),
        canonical_url("https://example.org/p?b=2&a=1")
    );
    // An explicitly empty query is part of the address: `/path?` and `/path` are two addresses (the provenance gate compares canonical
    // forms), and each keeps its own form when normalised again.
    assert_ne!(
        canonical_url("https://example.org/path?"),
        canonical_url("https://example.org/path")
    );
    assert_eq!(
        canonical_url("https://example.org/path?").as_deref(),
        Some("https://example.org/path?")
    );
    assert_eq!(
        canonical_url("https://example.org/path").as_deref(),
        Some("https://example.org/path")
    );
    assert_eq!(url_hash("https://example.org/").len(), 64);
    assert_eq!(
        url_hash("https://example.org/"),
        url_hash("https://example.org/")
    );
    assert_ne!(
        url_hash("https://example.org/"),
        url_hash("https://example.org/x")
    );
}

#[test]
fn the_scheme_must_be_https() {
    for raw in [
        "http://example.org/",
        "ftp://example.org/",
        "//example.org/",
        "example.org/path",
        "https:example.org",
        "https:/example.org",
        "",
        "https://",
        "https:///path",
    ] {
        rejected(raw);
        assert_eq!(canonical_url(raw), None, "{raw}");
    }
}

#[test]
fn user_information_ports_and_ip_literals_are_refused() {
    for raw in [
        "https://user@example.org/",
        "https://@example.org/",
        "https://example.org:8443/",
        "https://example.org:80/",
        "https://example.org:0/",
        "https://example.org:/x",
        "https://example.org:/",
        "https://example.org:abc/",
        "https://example.org:65536/",
        "https://127.0.0.1/",
        "https://10.0.0.5/x",
        "https://1.1.1.1:443/",
        "https://[::1]/",
        "https://[2001:db8::1]/",
        "https://localhost/",
        "https://app.localhost/",
        "https://printer.local/",
        "https://db.internal/",
    ] {
        rejected(raw);
    }
}

#[test]
fn malformed_hosts_and_forbidden_characters_are_refused() {
    let long_label = format!("https://{}.example/", "a".repeat(64));
    let long_host = format!("https://{}/", vec!["a".repeat(60); 5].join("."));
    for raw in [
        "https://.example.org/",
        "https://example..org/",
        "https://-bad.example.org/",
        "https://bad-.example.org/",
        "https://exa_mple.org/",
        "https://exa mple.org/",
        "https://example.org/a b",
        "https://example.org/a\tb",
        "https://example.org/a\nb",
        "https://example.org/a\u{7f}b",
        "https://example.org/a\\b",
        "https://example.org/a\u{a0}b",
        "https://example.org/a\u{2028}b",
        "https://münchen.example/",
        "https://example",
        &long_label,
        &long_host,
    ] {
        rejected(raw);
    }
    // The size limit counts bytes of the raw text.
    let at = format!("https://example.org/{}", "a".repeat(2048 - 20));
    assert!(check_url(&at, 2048).is_ok());
    assert!(check_url(&format!("{at}b"), 2048).is_err());
    assert!(check_url("https://example.org/", 19).is_err());
}

#[test]
fn presigned_and_credential_parameters_are_refused_whatever_their_case() {
    for q in [
        "X-Amz-Signature=abc",
        "x-amz-credential=abc",
        "Signature=abc",
        "sig=abc&se=2026&sp=r&sv=2020",
        "a=1&token=abc",
        "api_key=abc",
    ] {
        let raw = format!("https://example.org/file?{q}");
        rejected(&raw);
    }
    // A long `key` value is built here, not written out, so no source line looks like a key to a secret scan.
    rejected(&format!(
        "https://example.org/file?key={}",
        "abcdefghij".repeat(2)
    ));
    // Ordinary parameters, including a short `key` and names that only contain a refused word, pass.
    for q in [
        "key=abc",
        "keyword=rust",
        "tokenizer=bpe",
        "sort=signature",
        "design=sigma",
        "sess=1",
    ] {
        accepted(&format!("https://example.org/search?{q}"));
    }
}

#[test]
fn dotted_ipv4_hosts_are_not_hosts() {
    for raw in [
        "https://192.0.2.1/",
        "https://100.64.0.1/",
        "https://0.0.0.0/",
    ] {
        rejected(raw);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn canonicalisation_is_idempotent(
        host in "[a-zA-Z0-9.-]{0,30}",
        port in prop::option::of(0u32..70000),
        path in "[ -~]{0,40}",
        tail in ".{0,20}",
        scheme in prop::sample::select(vec!["https://", "HTTPS://", "http://", "https:/", ""]),
    ) {
        let raw = match port {
            Some(p) => format!("{scheme}{host}:{p}/{path}{tail}"),
            None => format!("{scheme}{host}/{path}{tail}"),
        };
        if let Some(c) = canonical_url(&raw) {
            prop_assert_eq!(canonical_url(&c), Some(c.clone()));
            prop_assert!(check_url(&c, usize::MAX).is_ok());
            prop_assert!(c.starts_with("https://"));
        }
    }

    #[test]
    fn the_scanner_never_panics_and_is_deterministic(s in ".{0,300}", kind in 0u8..3) {
        let kind = [ScanKind::Query, ScanKind::Url, ScanKind::RepoPart][kind as usize];
        let a = scan_outbound(&s, kind, &["session-key-id-123456"]);
        let b = scan_outbound(&s, kind, &["session-key-id-123456"]);
        prop_assert_eq!(a, b);
        let _ = check_url(&s, 2048);
        let _ = extract_links(&s, 10);
        let _ = github_repo(&s);
    }

    #[test]
    fn clean_text_leaves_no_banned_code_point(s in ".{0,200}", esc in prop::collection::vec(0usize..8, 0..5)) {
        // Sprinkle escape sequences and banned characters into arbitrary text.
        let extras = ["\u{1b}[31m", "\u{1b}]0;title\u{7}", "\u{202e}", "\u{200b}", "\u{feff}", "\u{9b}2J", "\r\n", "\u{2066}"];
        let mut mixed_in = s.clone();
        for i in esc { mixed_in.push_str(extras[i]); mixed_in.push_str(&s); }
        let out = clean_text(&mixed_in);
        for c in out.chars() {
            let banned = (c.is_control() && c != '\n' && c != '\t')
                || ('\u{202a}'..='\u{202e}').contains(&c)
                || ('\u{2066}'..='\u{2069}').contains(&c)
                || ('\u{200b}'..='\u{200d}').contains(&c)
                || c == '\u{2060}' || c == '\u{feff}';
            prop_assert!(!banned, "U+{:04X} survived", c as u32);
        }
        prop_assert_eq!(clean_text(&out), out);
    }
}

// ---- links ---------------------------------------------------------------------------------------------------------------------------

#[test]
fn links_are_found_in_markdown_angle_brackets_and_bare_text() {
    let text = "See [the repo](https://github.com/sindresorhus/is-plain-obj) and <https://www.npmjs.com/package/is-plain-obj>.\n\
        Also https://example.org/a_(b), (https://example.org/c). Repeat: https://github.com/sindresorhus/is-plain-obj#readme\n\
        Skipped: http://insecure.example/x https://127.0.0.1/x https://u@example.org/ and ftp://example.org/y\n\
        Trailing: https://example.org/end.\n\
        Quoted \"https://example.org/q\" and 'https://example.org/s'; done! https://example.org/z?a=1&b=2, ok";
    let links = extract_links(text, 50);
    assert_eq!(
        links,
        [
            "https://github.com/sindresorhus/is-plain-obj",
            "https://www.npmjs.com/package/is-plain-obj",
            "https://example.org/a_(b)",
            "https://example.org/c",
            "https://example.org/end",
            "https://example.org/q",
            "https://example.org/s",
            "https://example.org/z?a=1&b=2",
        ]
    );
    // The cap applies after deduplication, in order of appearance.
    assert_eq!(extract_links(text, 2), &links[..2]);
    assert!(extract_links(text, 0).is_empty());
    assert!(extract_links("no links here, only http://x and example.org", 5).is_empty());
    // Every returned link passes check_url and is canonical.
    for l in &links {
        assert_eq!(canonical_url(l).as_deref(), Some(l.as_str()));
    }
}

// ---- GitHub repositories -------------------------------------------------------------------------------------------------------------

#[test]
fn github_repository_urls_are_parsed_strictly() {
    for (raw, owner, repo) in [
        (
            "https://github.com/sindresorhus/is-plain-obj",
            "sindresorhus",
            "is-plain-obj",
        ),
        (
            "https://github.com/sindresorhus/is-plain-obj/",
            "sindresorhus",
            "is-plain-obj",
        ),
        (
            "https://github.com/sindresorhus/is-plain-obj.git",
            "sindresorhus",
            "is-plain-obj",
        ),
        (
            "https://GitHub.com/Kelsey-Hightower/nocode",
            "Kelsey-Hightower",
            "nocode",
        ),
        ("https://github.com:443/a/b.c_d-e", "a", "b.c_d-e"),
        ("https://github.com/a/.github", "a", ".github"),
    ] {
        let r = github_repo(raw).unwrap_or_else(|e| panic!("{raw}: {e}"));
        assert_eq!((r.owner.as_str(), r.repo.as_str()), (owner, repo), "{raw}");
        assert_eq!(r.origin(), format!("https://github.com/{owner}/{repo}"));
    }
    for raw in [
        "https://gitlab.com/a/b",
        "https://www.github.com/a/b",
        "https://api.github.com/repos/a/b",
        "https://codeload.github.com/a/b",
        "https://github.com.example.org/a/b",
        "https://example.org/github.com/a/b",
    ] {
        assert_eq!(github_repo(raw).unwrap_err().code, C::RefusedHost, "{raw}");
    }
    let long_owner = format!("https://github.com/{}/r", "a".repeat(40));
    let long_repo = format!("https://github.com/o/{}", "r".repeat(101));
    for raw in [
        "http://github.com/a/b",
        "github.com/a/b",
        "https://user@github.com/a/b",
        "https://github.com/",
        "https://github.com/a",
        "https://github.com/a/",
        "https://github.com/a/b/tree/main",
        "https://github.com/a/b/blob/main/x",
        "https://github.com/a/b?tab=readme",
        "https://github.com/a/b#readme",
        "https://github.com/a//b",
        "https://github.com//b",
        "https://github.com/a/..",
        "https://github.com/a/.",
        "https://github.com/../b",
        "https://github.com/a/.git",
        "https://github.com/a/b.git.git/",
        "https://github.com/a b/c",
        "https://github.com/a/b c",
        "https://github.com/a_b/c",
        "https://github.com/-/c",
        "https://github.com/a/b%2Fc",
        "https://github.com/a/b\\c",
        "https://github.com:8443/a/b",
        "https://github.com:/o/r",
        &long_owner,
        &long_repo,
    ] {
        let e = github_repo(raw).unwrap_err();
        assert_eq!(e.code, C::RefusedUrl, "{raw}");
    }
}

// ---- text cleaning ---------------------------------------------------------------------------------------------------------------------

#[test]
fn cleaning_removes_escapes_controls_and_invisible_overrides_but_nothing_else() {
    assert_eq!(clean_text("plain text\nwith\ttab"), "plain text\nwith\ttab");
    assert_eq!(clean_text("a\r\nb\r\nc"), "a\nb\nc");
    assert_eq!(clean_text("a\rb"), "ab");
    assert_eq!(clean_text("\u{1b}[31mred\u{1b}[0m"), "red");
    assert_eq!(clean_text("x\u{1b}[2J\u{1b}[Hy"), "xy");
    assert_eq!(clean_text("a\u{1b}]0;window title\u{7}b"), "ab");
    assert_eq!(
        clean_text("a\u{1b}]8;;https://example.net\u{1b}\\link\u{1b}]8;;\u{1b}\\b"),
        "alinkb"
    );
    assert_eq!(clean_text("a\u{1b}Pq data\u{1b}\\b"), "ab");
    assert_eq!(clean_text("a\u{1b}cb"), "ab");
    assert_eq!(clean_text("a\u{9b}31mb"), "ab");
    assert_eq!(clean_text("a\u{9d}0;t\u{7}b"), "ab");
    assert_eq!(clean_text("a\u{0}b\u{8}c\u{7f}d\u{85}e"), "abcde");
    assert_eq!(clean_text("a\u{202e}b\u{202a}c\u{2066}d\u{2069}e"), "abcde");
    assert_eq!(
        clean_text("a\u{200b}b\u{200c}c\u{200d}d\u{2060}e\u{feff}f"),
        "abcdef"
    );
    // Everything else, including right-to-left letters, accents and emoji, stays.
    assert_eq!(
        clean_text("שלום café ñ 日本語 🦀 é"),
        "שלום café ñ 日本語 🦀 é"
    );
    // An unterminated escape at the end is dropped with its tail, never panics.
    assert_eq!(clean_text("end\u{1b}"), "end");
    assert_eq!(clean_text("end\u{1b}[31"), "end");
    assert_eq!(clean_text("end\u{1b}]0;title"), "end");
    assert_eq!(clean_text(""), "");
}

#[test]
fn an_excerpt_cuts_on_a_character_boundary_and_adds_nothing() {
    assert_eq!(excerpt("hello", 10), "hello");
    assert_eq!(excerpt("hello", 3), "hel");
    assert_eq!(excerpt("héllo wörld", 4), "héll");
    assert_eq!(excerpt("🦀🦀🦀", 2), "🦀🦀");
    assert_eq!(excerpt("abc", 0), "");
    assert_eq!(excerpt("", 5), "");
}
