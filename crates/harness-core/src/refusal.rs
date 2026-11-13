//! Secret redaction: `redact` replaces every secret shape in a text, so an
//! engine's stderr line inside a failed turn's error text (`engine_diagnostic`) never carries one.

use regex::Regex;
use std::sync::LazyLock;

const REDACTED: &str = "<redacted>";

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|e| panic!("refusal pattern {pattern}: {e}"))
}

/// Secret shapes, in order, with the replacement that keeps their label.
static SECRETS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    vec![
        // PEM private keys.
        (
            re(r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?(?:-----END [A-Z ]*PRIVATE KEY-----|$)"),
            REDACTED,
        ),
        // URL user information: `https://aws:<token>@<domain>`, the pip index form.
        (
            re(r"(?i)\b([a-z][a-z0-9+.-]*://)[^/\s:@]+:[^/\s@]+@"),
            "${1}<redacted>@",
        ),
        // Authorization schemes.
        (
            re(r"(?i)\b(bearer|basic|token)\s+[A-Za-z0-9._~+/=-]{8,}"),
            "${1} <redacted>",
        ),
        // `key=value`, `key: value`, `"key": "value"` for secret-named keys.
        (
            re(
                r#"(?i)\b((?:aws_)?(?:secret_access_key|session_token|secret|password|passwd|token|api[_-]?key|access[_-]?key|auth[_-]?token|authorization|credentials?))(["']?\s*[:=]\s*["']?)[^\s"',;&|]+"#,
            ),
            "${1}${2}<redacted>",
        ),
        // AWS access key ids.
        (
            re(r"\b(?:AKIA|ASIA|AIDA|AROA|AGPA|ANPA|ANVA|AIPA)[A-Z0-9]{16}\b"),
            REDACTED,
        ),
        // JSON Web Tokens (CodeArtifact authorization tokens are this shape).
        (
            re(r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}(?:\.[A-Za-z0-9_-]+)?"),
            REDACTED,
        ),
    ]
});

/// A run of token characters long enough to be a credential.
static OPAQUE: LazyLock<Regex> = LazyLock::new(|| re(r"[A-Za-z0-9+_=-]{40,}"));

/// `text` with every secret shape replaced by `<redacted>`: URL user information, authorization
/// schemes, secret-named assignments, AWS access key ids, JWTs, PEM private keys, and any run of
/// 40 or more token characters that mixes upper case, lower case and digits (hex digests and
/// paths are kept).
pub fn redact(text: &str) -> String {
    let mut out = text.to_string();
    for (r, with) in SECRETS.iter() {
        out = r.replace_all(&out, *with).into_owned();
    }
    OPAQUE
        .replace_all(&out, |c: &regex::Captures| {
            let s = &c[0];
            let mixed = s.chars().any(|c| c.is_ascii_uppercase())
                && s.chars().any(|c| c.is_ascii_lowercase())
                && s.chars().any(|c| c.is_ascii_digit());
            if mixed {
                REDACTED.to_string()
            } else {
                s.to_string()
            }
        })
        .into_owned()
}
