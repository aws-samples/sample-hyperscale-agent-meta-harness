//! A planted-secret property of the stderr scrub.
//! Each case plants a secret in a shape the scrub claims to cover, puts
//! random noise before and after it, and fails when any 8 characters of the secret survive in the joined tail, the orchestrator's
//! error body or the runtime log body. The fuzz test beside it (`engine_diagnostic_fuzz.rs`) plants nothing, so it cannot see a
//! leak. The count is `ENGINE_DIAGNOSTIC_PLANT_N` (default 20,000, seconds in a debug build) and the seed
//! `ENGINE_DIAGNOSTIC_PLANT_SEED` (default fixed); the third-pass run was 300,000 cases.
#![allow(clippy::unwrap_used)]

use harness_core::engine_diagnostic::{error_body, log_body, scrub};
use std::collections::BTreeMap;

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
    fn pick<'a>(&mut self, v: &'a [&'a str]) -> &'a str {
        v[self.below(v.len())]
    }
}

const NOISE: &[&str] = &[
    "Error: ",
    "at f (a.js:1:2)",
    "connect ",
    "failed",
    "GET ",
    "POST ",
    "curl -H ",
    "export ",
    "env: ",
    "[ERROR] ",
    "{",
    "}",
    "[",
    "]",
    "(",
    ")",
    ",",
    ";",
    ":",
    "=",
    "\"",
    "'",
    "`",
    " ",
    "  ",
    "\t",
    "\n",
    "\r\n",
    "https://example.com/x",
    "/usr/bin/node",
    "src/main/java/com/example/Demo.java",
    "token budget",
    "exit 1",
    "-- ",
    "| ",
    "> ",
    "# ",
    "// ",
    "--",
    "&",
    "?",
    "%20",
    "user@host",
    "git@github.com:a/b.git",
    "key",
    "pass",
    "auth",
    "name",
    "value",
    "header",
    "headers",
    "json",
    "yaml",
    "<",
    ">",
    "\\",
    "\\\"",
    "0123",
    "abc",
    "Bearer",
    "Basic",
];

/// Keys that name a secret.
const KEYS: &[&str] = &[
    "DB_PASS",
    "db_pass",
    "dbPass",
    "MYSQL_PWD",
    "REDIS_AUTH",
    "GITHUB_PAT",
    "OPENAI_KEY",
    "openaiKey",
    "STRIPE_KEY",
    "SLACK_WEBHOOK_URL",
    "DISCORD_WEBHOOK",
    "PGPASSWORD",
    "DB_PASSWORD",
    "password",
    "Password",
    "passwd",
    "secret",
    "client_secret",
    "api_key",
    "apiKey",
    "access_token",
    "AWS_SECRET_ACCESS_KEY",
    "SessionToken",
    "x-api-key",
    "auth_token",
    "pass",
    "PASS",
    "pwd",
    "PAT",
    "AUTH",
    "bearer",
    "private_key",
    "signing_key",
    "ENCRYPTION_KEY",
    "keystore.pass",
    "smtpPass",
    "SSH_PASS",
];

/// Keys that hold a secret word of the key rule itself.
const STRONG_KEYS: &[&str] = &[
    "password",
    "Password",
    "passwd",
    "secret",
    "client_secret",
    "api_key",
    "apiKey",
    "access_token",
    "AWS_SECRET_ACCESS_KEY",
    "SessionToken",
    "x-api-key",
    "auth_token",
    "private_key",
    "DB_PASSWORD",
    "PGPASSWORD",
];

/// Labels that are keys by a short word, in front of a second key.
const LABELS: &[&str] = &[
    "auth", "Auth", "pass", "PASS", "pat", "pw", "pwd", "bearer", "Bearer", "webhook", "DB_PASS",
    "dbPass", "token", "secret",
];

const SEPS: &[&str] = &[": ", "=", " = ", ":", " : ", " := ", " => "];

/// A secret of 9 to 18 characters with a digit and an upper-case letter, built at run time: no word rule is the reason it goes.
fn secret(r: &mut Rng) -> String {
    let alpha = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let n = 9 + r.below(10);
    let mut s: Vec<u8> = (0..n).map(|_| alpha[r.below(alpha.len())]).collect();
    s[0] = b'Q';
    s[1] = b'7';
    s[2] = b'x';
    let mut secret = String::from_utf8(s).unwrap();
    // A token that ends like a camel-case key name (`...Pw`, `...Key`) and is followed by a separator reads as that key: a limit of
    // the key rule, not a leak of a shape it claims to cover. The planted secrets do not end that way.
    if ["Pass", "Pwd", "Pat", "Auth", "Bearer", "Key", "Pw"]
        .iter()
        .any(|suffix| secret.ends_with(suffix))
    {
        secret.pop();
        secret.push('9');
    }
    secret
}

/// A planted secret in one shape, and the shape's name.
fn template(r: &mut Rng, s: &str) -> (String, &'static str) {
    let key = r.pick(KEYS).to_string();
    match r.below(17) {
        0 => (format!("{key}={s}"), "key=value"),
        1 => (format!("{key}: {s}"), "key: value"),
        2 => (format!("\"{key}\":\"{s}\""), "json"),
        3 => (format!("\"{key}\": \"{s}\""), "json-space"),
        4 => (format!("export {key}='{s}'"), "export-sq"),
        5 => (format!("export {key}=\"{s}\""), "export-dq"),
        6 => (format!("{key} = \"{s}\""), "spaced-dq"),
        7 => {
            let scheme = r.pick(&[
                "Bearer",
                "Basic",
                "bearer",
                "BASIC",
                "Digest",
                "NTLM",
                "Negotiate",
            ]);
            let header = r.pick(&[
                "authorization",
                "Authorization",
                "proxy-authorization",
                "proxy_authorization",
                "AUTHORIZATION",
            ]);
            (format!("{header}={scheme} {s}"), "auth-eq")
        }
        8 => {
            let scheme = r.pick(&[
                "https",
                "http",
                "redis",
                "postgres",
                "amqp",
                "mongodb",
                "git+https",
                "ftp",
            ]);
            (
                format!("{scheme}://user:{s}@host.example.com:5432/db"),
                "url-user",
            )
        }
        9 => {
            let scheme = r.pick(&["https", "redis", "amqp"]);
            (
                format!("{scheme}://:{s}@cache.example.com/0"),
                "url-empty-user",
            )
        }
        10 => {
            let scheme = r.pick(&["https", "redis"]);
            (
                format!("{scheme}://user:ab/{s}@host.example.com/x"),
                "url-slash",
            )
        }
        11 => {
            let scheme = r.pick(&["https", "postgres"]);
            (
                format!("{scheme}://user:ab@{s}@host.example.com/x"),
                "url-at",
            )
        }
        12 => {
            // The value on the next line is taken for a key that holds a secret word; an ambiguous short key at the end of a line
            // (`DB_PASS=`) does not take the next line (YAML structure), so those are not planted.
            let strong = r.pick(STRONG_KEYS);
            (format!("{strong}=\n{s}"), "key-nl-value")
        }
        13 => {
            let quoted = r.pick(&[
                "Cookie: '{s}'",
                "cookie: 'sid={s}; theme=dark'",
                "x-api-key: '{s}'",
                "{ cookie: 'sid={s}' }",
                "{ Authorization: 'Token {s}' }",
                "{ 'set-cookie': [ 'sid={s}; Path=/' ] }",
                "Set-Cookie: [ 'sid={s}' ]",
                "proxy-authorization: 'Basic {s}'",
            ]);
            (quoted.replace("{s}", s), "quoted-header")
        }
        14 => {
            let shape = r.pick(&[
                "authorization=Token {s}",
                "authorization=ApiKey {s}",
                "Proxy-Authorization=Key {s}",
                "authorization=Digest username=\"bob\", nonce=\"n1\", response=\"{s}\"",
                "{\"Authorization\":\"Digest username=\\\"bob\\\", response=\\\"{s}\\\"\"}",
                "authorization=Bearer\n{s}",
                "Authorization: Basic\n{s}",
                "{\"password\":\"x\\\"{s}\"}",
            ]);
            (shape.replace("{s}", s), "auth-scheme")
        }
        15 => {
            // A JVM system property, also named by a lone short word (`-Dpass=`, `-Dauth=`).
            (format!("-D{key}={s}"), "minus-D")
        }
        // A label that is a key, then a second key and the secret.
        _ => {
            let label = r.pick(LABELS);
            let (first, second) = (r.pick(SEPS), r.pick(SEPS));
            (format!("{label}{first}{key}{second}{s}"), "label-then-key")
        }
    }
}

#[test]
fn a_planted_secret_in_random_surroundings_does_not_survive() {
    let n: u32 = std::env::var("ENGINE_DIAGNOSTIC_PLANT_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000);
    let seed: u64 = std::env::var("ENGINE_DIAGNOSTIC_PLANT_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0xA5A5_1234_5678_9ABC);
    let mut r = Rng(seed | 1);
    let mut by_shape: BTreeMap<&'static str, (u32, String)> = BTreeMap::new();
    let mut leaks = 0u32;
    for _ in 0..n {
        let s = secret(&mut r);
        let (planted, shape) = template(&mut r, &s);
        let mut before = String::new();
        for _ in 0..r.below(6) {
            before.push_str(r.pick(NOISE));
        }
        let mut after = String::new();
        for _ in 0..r.below(6) {
            after.push_str(r.pick(NOISE));
        }
        let raw = format!("{before} {planted} {after}\nError: boom\n");
        let tail = scrub(raw.as_bytes(), false);
        let (joined, body, log) = (
            tail.joined(),
            error_body("exit status 1", &tail),
            log_body("exit status 1", &tail),
        );
        let leaked = (0..=s.len() - 8)
            .map(|i| &s[i..i + 8])
            .any(|piece| joined.contains(piece) || body.contains(piece) || log.contains(piece));
        if leaked {
            leaks += 1;
            let entry = by_shape.entry(shape).or_insert((0, String::new()));
            entry.0 += 1;
            if entry.1.is_empty() || raw.len() < entry.1.len() {
                entry.1 = format!("{raw:?} -> {joined:?}");
            }
        }
    }
    println!("PLANT n={n} leaks={leaks}");
    for (shape, (count, example)) in &by_shape {
        println!("PLANT-LEAK {shape}: {count}, e.g. {example}");
    }
    assert_eq!(
        leaks, 0,
        "{leaks} of {n} planted secrets survived: {by_shape:#?}"
    );
}
