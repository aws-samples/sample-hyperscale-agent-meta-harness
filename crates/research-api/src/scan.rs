//! Outbound scanning, URL rules and text cleaning.
//!
//! Cline holds the same session credentials as agentd and can call the research route itself, so everything it sends out is judged here,
//! in the Lambda: a query, a URL, a repository name, a ref or a path is refused when it looks like a credential ([`scan_outbound`]) or
//! when a URL breaks the URL rules ([`check_url`]). The scan catches accidents; it cannot catch encoded text, and the byte budget
//! (counted by the caller) bounds what can leave.
//!
//! Every function here is pure. A refusal names the class of what it found ("aws-access-key") and never echoes the text.

use crate::ports::{Fail, GithubRepo};
use harness_protocol::research::ResearchErrorCode;
use sha2::{Digest, Sha256};
use std::collections::HashSet;

// ---- the scanner ------------------------------------------------------------------------------------------------------------------------

/// What the scanned text is. The thresholds for long runs differ: a query is prose, a URL or a repository part may legitimately carry a
/// 40-hex commit id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanKind {
    Query,
    Url,
    RepoPart,
}

/// Longest hex run a query may carry is 31 characters.
const QUERY_HEX_RUN: usize = 32;
/// A URL or repository part may carry a commit id (40 hex characters) and nothing longer.
const URL_HEX_RUN: usize = 41;
/// Base64-ish runs in a query: 40 characters is the length of an AWS secret access key.
const QUERY_BASE64_RUN: usize = 40;
/// Alphanumeric runs in a URL or repository part.
const URL_BASE64_RUN: usize = 48;
/// A run with this many slashes or more is a path, not a key.
const PATH_SLASHES: usize = 4;
/// Own key ids shorter than this are ignored: a short id would match ordinary words.
const MIN_OWN_KEY_ID: usize = 16;

/// AWS access key id prefixes (IAM unique id families).
const AWS_PREFIXES: [&str; 10] = [
    "AKIA", "ASIA", "AGPA", "AIDA", "AROA", "ANPA", "ANVA", "AIPA", "ABIA", "ACCA",
];

fn refuse(class: &str) -> Fail {
    Fail::new(
        ResearchErrorCode::RefusedSecret,
        format!("outbound text looks like a credential ({class}); remove it and ask again"),
    )
}

/// Refuses text that looks like a credential. `own_key_ids` are the caller's own session key ids (any of at least 16 characters is
/// matched as a case-sensitive substring). A URL or repository part is percent-decoded once before the scan.
pub fn scan_outbound(text: &str, kind: ScanKind, own_key_ids: &[&str]) -> Result<(), Fail> {
    match scan_class(text, kind, own_key_ids) {
        Some(class) => Err(refuse(class)),
        None => Ok(()),
    }
}

fn scan_class(text: &str, kind: ScanKind, own_key_ids: &[&str]) -> Option<&'static str> {
    let decoded;
    let scanned: &str = match kind {
        ScanKind::Query => text,
        ScanKind::Url | ScanKind::RepoPart => {
            decoded = percent_decode_once(text);
            &decoded
        }
    };
    for id in own_key_ids {
        if id.len() >= MIN_OWN_KEY_ID && (text.contains(id) || scanned.contains(id)) {
            return Some("session-key-id");
        }
    }
    if scanned.contains("PRIVATE KEY-----") && scanned.contains("-----BEGIN") {
        return Some("private-key");
    }
    if has_aws_access_key(scanned) {
        return Some("aws-access-key");
    }
    if has_prefixed_token(
        scanned,
        &["ghp_", "gho_", "ghu_", "ghs_", "ghr_"],
        is_alnum,
        20,
    ) || has_prefixed_token(scanned, &["github_pat_"], is_alnum_underscore, 20)
    {
        return Some("github-token");
    }
    if has_prefixed_token(scanned, &["glpat-"], is_alnum_underscore_hyphen, 20) {
        return Some("gitlab-token");
    }
    if has_slack_token(scanned) {
        return Some("slack-token");
    }
    if has_jwt(scanned) {
        return Some("jwt");
    }
    if has_authorization(scanned) {
        return Some("authorization");
    }
    long_runs(scanned, kind)
}

fn is_alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

fn is_alnum_underscore(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_alnum_underscore_hyphen(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn is_base64url(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Number of bytes from `start` that satisfy `pred`.
fn run_len(bytes: &[u8], start: usize, pred: fn(u8) -> bool) -> usize {
    bytes[start.min(bytes.len())..]
        .iter()
        .take_while(|b| pred(**b))
        .count()
}

fn has_prefixed_token(
    text: &str,
    prefixes: &[&str],
    pred: fn(u8) -> bool,
    min_tail: usize,
) -> bool {
    let bytes = text.as_bytes();
    prefixes.iter().any(|p| {
        text.match_indices(p)
            .any(|(i, _)| run_len(bytes, i + p.len(), pred) >= min_tail)
    })
}

fn has_aws_access_key(text: &str) -> bool {
    let bytes = text.as_bytes();
    AWS_PREFIXES.iter().any(|p| {
        text.match_indices(p).any(|(i, _)| {
            let boundary = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
            boundary
                && run_len(bytes, i + 4, |b| {
                    b.is_ascii_uppercase() || b.is_ascii_digit()
                }) >= 16
        })
    })
}

fn has_slack_token(text: &str) -> bool {
    let bytes = text.as_bytes();
    text.match_indices("xox").any(|(i, _)| {
        matches!(bytes.get(i + 3), Some(b'a' | b'b' | b'p' | b'r' | b's'))
            && bytes.get(i + 4) == Some(&b'-')
            && run_len(bytes, i + 5, is_alnum_underscore_hyphen) >= 10
    })
}

fn has_jwt(text: &str) -> bool {
    let bytes = text.as_bytes();
    text.match_indices("eyJ").any(|(i, _)| {
        let first = run_len(bytes, i, is_base64url);
        if first < 10 || bytes.get(i + first) != Some(&b'.') {
            return false;
        }
        let second = run_len(bytes, i + first + 1, is_base64url);
        if second < 4 || bytes.get(i + first + 1 + second) != Some(&b'.') {
            return false;
        }
        run_len(bytes, i + first + second + 2, is_base64url) >= 1
    })
}

fn has_authorization(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let boundary = |i: usize| i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
    // `Bearer <token>`: 16 or more token characters with a digit in them.
    for (i, _) in lower.match_indices("bearer ") {
        if !boundary(i) {
            continue;
        }
        let start = i + "bearer ".len();
        let n = run_len(bytes, start, |b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'+' | b'/' | b'=' | b'-')
        });
        if n >= 16
            && text.as_bytes()[start..start + n]
                .iter()
                .any(u8::is_ascii_digit)
        {
            return true;
        }
    }
    // `Basic <base64>`: 12 or more base64 characters that look encoded, not like a word.
    for (i, _) in lower.match_indices("basic ") {
        if !boundary(i) {
            continue;
        }
        let start = i + "basic ".len();
        let orig = text.as_bytes();
        let n = run_len(orig, start, |b| {
            b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')
        });
        if n < 12 {
            continue;
        }
        let token = &orig[start..start + n];
        let digit = token.iter().any(u8::is_ascii_digit);
        let padded = token.last() == Some(&b'=');
        let inner_upper = token.iter().skip(1).any(u8::is_ascii_uppercase);
        let lower_case = token.iter().any(u8::is_ascii_lowercase);
        if digit || padded || (inner_upper && lower_case) {
            return true;
        }
    }
    false
}

fn is_hex(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

fn long_runs(text: &str, kind: ScanKind) -> Option<&'static str> {
    let bytes = text.as_bytes();
    let (hex_min, base64_min) = match kind {
        ScanKind::Query => (QUERY_HEX_RUN, QUERY_BASE64_RUN),
        ScanKind::Url | ScanKind::RepoPart => (URL_HEX_RUN, URL_BASE64_RUN),
    };
    // Query: base64 alphabet with `+`, `/` and `=`; a URL part: letters and digits only, so slashes and hyphens end a run.
    let in_base64: fn(u8) -> bool = match kind {
        ScanKind::Query => |b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='),
        ScanKind::Url | ScanKind::RepoPart => |b| b.is_ascii_alphanumeric(),
    };
    let mut i = 0;
    while i < bytes.len() {
        if is_hex(bytes[i]) {
            let n = run_len(bytes, i, is_hex);
            if n >= hex_min {
                return Some("long-hex");
            }
            i += n;
        } else {
            i += 1;
        }
    }
    let mut i = 0;
    while i < bytes.len() {
        if in_base64(bytes[i]) {
            let n = run_len(bytes, i, in_base64);
            let run = &bytes[i..i + n];
            if n >= base64_min && mixes_digits_and_both_cases(run) {
                let slashes = run.iter().filter(|b| **b == b'/').count();
                if slashes < PATH_SLASHES {
                    return Some(if kind == ScanKind::Query && n == 40 {
                        "aws-secret-key"
                    } else {
                        "long-base64"
                    });
                }
            }
            i += n;
        } else {
            i += 1;
        }
    }
    None
}

fn mixes_digits_and_both_cases(run: &[u8]) -> bool {
    run.iter().any(u8::is_ascii_digit)
        && run.iter().any(u8::is_ascii_uppercase)
        && run.iter().any(u8::is_ascii_lowercase)
}

/// Percent-decodes once: `%41` becomes `A`, a malformed escape stays as written. The result is lossy UTF-8.
fn percent_decode_once(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex_val(b[i + 1]), hex_val(b[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ---- URLs ---------------------------------------------------------------------------------------------------------------------------------

/// An acceptable URL after normalisation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckedUrl {
    /// `https://<host><path>[?<query>]`: lower-case host, no default port, no fragment, an empty path as `/`; the path and the query are
    /// kept exactly as written, so one more query parameter makes a different URL.
    pub canonical: String,
    pub host: String,
}

fn refuse_url(why: &str) -> Fail {
    Fail::new(ResearchErrorCode::RefusedUrl, format!("URL refused: {why}"))
}

/// Top-level names that resolve inside a network, never on the public internet.
const PRIVATE_TLDS: [&str; 8] = [
    "localhost",
    "local",
    "internal",
    "localdomain",
    "lan",
    "home",
    "corp",
    "intranet",
];

/// Query parameter names (lower case, percent-decoded) that carry a credential or a signature.
const CREDENTIAL_PARAMS: [&str; 19] = [
    "x-amz-signature",
    "x-amz-credential",
    "x-amz-security-token",
    "awsaccesskeyid",
    "signature",
    "x-goog-signature",
    "x-goog-credential",
    "sig",
    "token",
    "access_token",
    "api_key",
    "apikey",
    "password",
    "client_secret",
    "refresh_token",
    "id_token",
    "secret",
    "auth",
    "authorization",
];

/// A `key` parameter with a value this long is a key, not a search term.
const LONG_KEY_VALUE: usize = 20;

/// Whether `label` could be read as a number by the platform's address parser (decimal, octal or `0x` hex).
fn numeric_label(label: &str) -> bool {
    if label.is_empty() {
        return false;
    }
    if label.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    let lower = label.to_ascii_lowercase();
    lower
        .strip_prefix("0x")
        .is_some_and(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !label.starts_with('-')
        && !label.ends_with('-')
}

/// The host rules; `authority` has no userinfo. Returns the lower-case host without a trailing dot.
fn check_host(authority: &str) -> Result<String, Fail> {
    if authority.contains('[') || authority.contains(']') {
        return Err(refuse_url("an IP address is not a host name"));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (authority, None),
    };
    // A port that is written must be 443; an empty one (`host:`) is refused like any other port that is not 443, so the canonical form
    // never has to decide what it meant.
    if let Some(p) = port {
        let n: u32 = if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) && p.len() <= 6 {
            p.parse().unwrap_or(u32::MAX)
        } else {
            u32::MAX
        };
        if n != 443 {
            return Err(refuse_url("only port 443 is allowed"));
        }
    }
    let host = host.to_ascii_lowercase();
    let host = host.strip_suffix('.').unwrap_or(&host);
    if host.is_empty() {
        return Err(refuse_url("the host is empty"));
    }
    if !host.is_ascii() || host.len() > 253 {
        return Err(refuse_url("the host name is not a plain ASCII name"));
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 {
        return Err(refuse_url("the host name needs a dot"));
    }
    if !labels.iter().all(|l| valid_label(l)) {
        return Err(refuse_url("the host name has an invalid label"));
    }
    if labels.iter().all(|l| numeric_label(l))
        || labels
            .last()
            .is_some_and(|t| t.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(refuse_url("an IP address is not a host name"));
    }
    if labels.last().is_some_and(|t| PRIVATE_TLDS.contains(t)) {
        return Err(refuse_url("the host name is not on the public internet"));
    }
    Ok(host.to_string())
}

fn check_query(query: &str) -> Result<(), Fail> {
    for part in query.split(['&', ';']) {
        let (name, value) = match part.split_once('=') {
            Some((n, v)) => (n, v),
            None => (part, ""),
        };
        let name = percent_decode_once(name).to_ascii_lowercase();
        if CREDENTIAL_PARAMS.contains(&name.as_str()) {
            return Err(refuse_url(
                "a query parameter carries a credential or signature",
            ));
        }
        if name == "key" && value.len() >= LONG_KEY_VALUE {
            return Err(refuse_url(
                "a query parameter carries a credential or signature",
            ));
        }
    }
    Ok(())
}

/// Applies the URL rules and normalises. `max_bytes` bounds the raw text.
pub fn check_url(raw: &str, max_bytes: usize) -> Result<CheckedUrl, Fail> {
    if raw.len() > max_bytes {
        return Err(refuse_url(&format!("longer than {max_bytes} bytes")));
    }
    if raw
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return Err(refuse_url(
            "control characters, spaces and backslashes are not allowed",
        ));
    }
    let rest = match raw.get(..8) {
        Some(scheme) if scheme.eq_ignore_ascii_case("https://") => &raw[8..],
        _ => return Err(refuse_url("only https URLs are allowed")),
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    if authority.contains('@') {
        return Err(refuse_url("user information is not allowed"));
    }
    let host = check_host(authority)?;
    let without_fragment = tail.split('#').next().unwrap_or("");
    let (path, query) = match without_fragment.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (without_fragment, None),
    };
    check_query(query.unwrap_or(""))?;
    let path = if path.is_empty() { "/" } else { path };
    // An empty query that is written (`/path?`) is kept: it is a different address from `/path`.
    let canonical = match query {
        Some(q) => format!("https://{host}{path}?{q}"),
        None => format!("https://{host}{path}"),
    };
    Ok(CheckedUrl { canonical, host })
}

/// The canonical form of an acceptable URL, `None` for anything [`check_url`] refuses (no length limit).
pub fn canonical_url(raw: &str) -> Option<String> {
    check_url(raw, usize::MAX).ok().map(|c| c.canonical)
}

/// 64 lower-case hex characters: SHA-256 of a canonical URL.
pub fn url_hash(canonical: &str) -> String {
    Sha256::digest(canonical.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The longest link [`extract_links`] considers.
const LINK_MAX_BYTES: usize = 2048;

/// The https links in `text` (markdown links, angle brackets, bare addresses), canonical, acceptable to [`check_url`], without
/// duplicates, in order of appearance, at most `max`.
pub fn extract_links(text: &str, max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if max == 0 {
        return out;
    }
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut seen: HashSet<String> = HashSet::new();
    for (i, _) in lower.match_indices("https://") {
        if i > 0 && bytes[i - 1].is_ascii_alphanumeric() {
            continue;
        }
        let tail = &text[i..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`' | '|' | '\\'))
            .unwrap_or(tail.len());
        let candidate = trim_link(&tail[..end]);
        if let Ok(c) = check_url(candidate, LINK_MAX_BYTES)
            && seen.insert(c.canonical.clone())
        {
            out.push(c.canonical);
            if out.len() >= max {
                break;
            }
        }
    }
    out
}

/// Strips trailing punctuation, and a closing bracket the link does not open.
fn trim_link(mut s: &str) -> &str {
    loop {
        let Some(last) = s.chars().last() else {
            return s;
        };
        let unbalanced = |open: char, close: char| {
            s.chars().filter(|c| *c == close).count() > s.chars().filter(|c| *c == open).count()
        };
        let strip = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '*' => true,
            ')' => unbalanced('(', ')'),
            ']' => unbalanced('[', ']'),
            '}' => unbalanced('{', '}'),
            _ => false,
        };
        if !strip {
            return s;
        }
        s = &s[..s.len() - last.len_utf8()];
    }
}

// ---- GitHub repositories ---------------------------------------------------------------------------------------------------------------

fn valid_owner(o: &str) -> bool {
    (1..=39).contains(&o.len())
        && o.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !o.starts_with('-')
        && !o.ends_with('-')
}

fn valid_repo(r: &str) -> bool {
    (1..=100).contains(&r.len())
        && r != "."
        && r != ".."
        && r.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// `https://github.com/<owner>/<repo>` (a `.git` suffix and one trailing slash are accepted) and nothing else: another host is
/// `refused_host`, anything malformed is `refused_url`.
pub fn github_repo(url: &str) -> Result<GithubRepo, Fail> {
    let rest = match url.get(..8) {
        Some(scheme) if scheme.eq_ignore_ascii_case("https://") => &url[8..],
        _ => return Err(refuse_url("only https URLs are allowed")),
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    if authority.contains('@') {
        return Err(refuse_url("user information is not allowed"));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (authority, None),
    };
    if !host.eq_ignore_ascii_case("github.com") {
        return Err(Fail::new(
            ResearchErrorCode::RefusedHost,
            "only github.com repositories are served in this release",
        ));
    }
    if port.is_some_and(|p| p != "443") {
        return Err(refuse_url("only port 443 is allowed"));
    }
    let path = tail
        .strip_prefix('/')
        .ok_or_else(|| refuse_url("the URL names no repository"))?;
    let path = path.strip_suffix('/').unwrap_or(path);
    let mut parts = path.split('/');
    let (Some(owner), Some(repo), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(refuse_url("expected https://github.com/<owner>/<repo>"));
    };
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    if !valid_owner(owner) || !valid_repo(repo) || repo.ends_with(".git") {
        return Err(refuse_url("the owner or repository name is not valid"));
    }
    Ok(GithubRepo {
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

// ---- text cleaning -----------------------------------------------------------------------------------------------------------------------

/// Removes what a terminal or a reader should never get from a web page: C0 and C1 controls (except newline and tab), escape sequences
/// (CSI, OSC, DCS and the two-character forms), bidirectional overrides, zero-width characters and the byte-order mark. CRLF becomes LF.
/// An unterminated string sequence is dropped to the end of its line, so a stray escape never swallows the rest of a page.
pub fn clean_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\u{1b}' => skip_escape(&mut it),
            '\u{9b}' => skip_csi(&mut it),
            '\u{9d}' => skip_string(&mut it),
            '\n' | '\t' => out.push(c),
            '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
            | '\u{200b}'..='\u{200d}'
            | '\u{2060}'
            | '\u{feff}' => {}
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

type Chars<'a> = std::iter::Peekable<std::str::Chars<'a>>;

/// After `ESC`.
fn skip_escape(it: &mut Chars<'_>) {
    match it.peek().copied() {
        Some('[') => {
            it.next();
            skip_csi(it);
        }
        Some(']' | 'P' | 'X' | '^' | '_') => {
            it.next();
            skip_string(it);
        }
        _ => {
            // Intermediate bytes 0x20..=0x2F, then one final byte 0x30..=0x7E.
            while let Some(c) = it.peek().copied() {
                if ('\u{20}'..='\u{2f}').contains(&c) {
                    it.next();
                } else {
                    break;
                }
            }
            if let Some(c) = it.peek().copied()
                && ('\u{30}'..='\u{7e}').contains(&c)
            {
                it.next();
            }
        }
    }
}

/// Parameter bytes 0x20..=0x3F, then a final byte 0x40..=0x7E; anything else ends the sequence unconsumed.
fn skip_csi(it: &mut Chars<'_>) {
    while let Some(c) = it.peek().copied() {
        if ('\u{20}'..='\u{3f}').contains(&c) {
            it.next();
        } else if ('\u{40}'..='\u{7e}').contains(&c) {
            it.next();
            return;
        } else {
            return;
        }
    }
}

/// An OSC or DCS-style string: ends at BEL, at `ESC \`, at C1 ST, or before a newline.
fn skip_string(it: &mut Chars<'_>) {
    while let Some(c) = it.peek().copied() {
        match c {
            '\n' => return,
            '\u{7}' | '\u{9c}' => {
                it.next();
                return;
            }
            '\u{1b}' => {
                it.next();
                if it.peek() == Some(&'\\') {
                    it.next();
                }
                return;
            }
            _ => {
                it.next();
            }
        }
    }
}

/// The first `max_chars` characters of `s`, cut on a character boundary; nothing is appended.
pub fn excerpt(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}
