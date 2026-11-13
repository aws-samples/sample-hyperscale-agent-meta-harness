//! Display-side redaction of token-shaped strings. The worker redacts the stored record before it is written; this
//! is the second line, applied to text the client is about to draw, copy or export, so a token that got past the first is not shown.
//! It hides GitHub, GitLab, Slack, Google, npm and OpenAI-style tokens, AWS access key ids, JSON web tokens, bearer and basic
//! credentials, private key blocks, a password inside a URL, a presigned or signed query string and the value of a credential-named
//! setting. It does not touch commit hashes, ULIDs, UUIDs, AWS role or user ids or ordinary prose: a rule needs a token prefix, a
//! header, a credential name or a URL shape. No dependency: the rules are small scanners over the characters.
//!
//! The text is written by an engine and can be long and repetitive, so the scan is linear: a run of characters is scanned once and reused by
//! every rule that starts inside it, a candidate looks ahead only a bounded distance, and [`redact_with_work`] counts the steps so a
//! test can hold the cost to a multiple of the text's length.

/// What a hidden value is replaced by.
pub const REDACTED: &str = "[redacted]";
const PRIVATE_KEY: &str = "[redacted private key]";

/// Names whose value is hidden (compared without case, after a non-letter or at the start). A longer name comes before a shorter one
/// it starts with.
const CREDENTIAL_NAMES: [&str; 17] = [
    "aws_secret_access_key",
    "secret_access_key",
    "client_secret",
    "refresh_token",
    "access_token",
    "auth_token",
    "authtoken",
    "private_key",
    "api_key",
    "api-key",
    "apikey",
    "_authtoken",
    "_auth",
    "_password",
    "password",
    "passwd",
    "secret",
];

/// The most characters of a token body that one rule hides in one step (a longer run keeps its tail visible).
const TOKEN_BODY_CAP: usize = 200;
/// The most characters a quoted value may span.
const QUOTED_CAP: usize = 1024;
/// The most characters of user-info scanned after `://`.
const USERINFO_CAP: usize = 256;
/// How far back from `Bearer` or `Basic` the word `authorization` is looked for.
const BACK_CAP: usize = 16;

fn is_alnum(c: char) -> bool {
    c.is_ascii_alphanumeric()
}

fn b64url(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

fn bearer_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '+' | '/' | '=' | '-')
}

fn url_stop(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '<' | '>' | ')' | ']')
}

fn value_char(c: char) -> bool {
    !c.is_whitespace() && !matches!(c, ';' | ',' | '&' | ')' | '}' | ']' | '"' | '\'')
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    B64Url,
    Bearer,
    Url,
    Value,
}

impl Class {
    fn accepts(self, c: char) -> bool {
        match self {
            Class::B64Url => b64url(c),
            Class::Bearer => bearer_char(c),
            Class::Url => !url_stop(c),
            Class::Value => value_char(c),
        }
    }
}

/// The last position of each signature marker in one URL token.
#[derive(Clone, Copy, Default)]
struct Markers {
    amz_signature: Option<usize>,
    amz_token: Option<usize>,
    signature: Option<usize>,
    access_key_id: Option<usize>,
    key_pair_id: Option<usize>,
    goog_signature: Option<usize>,
    sig: Option<usize>,
    sv: Option<usize>,
}

struct Redactor<'a> {
    c: &'a [char],
    work: u64,
    /// The last maximal run scanned per class, `(start, end)`: a query inside it needs no scan.
    runs: [Option<(usize, usize)>; 4],
    /// The markers of the URL token `(start, end, markers)` last examined.
    markers: Option<(usize, usize, Markers)>,
    /// A `?` before this position cannot start a signed query (its token was examined and had no marker).
    unsigned_until: usize,
}

impl<'a> Redactor<'a> {
    fn new(c: &'a [char]) -> Self {
        Self {
            c,
            work: 0,
            runs: [None; 4],
            markers: None,
            unsigned_until: 0,
        }
    }

    fn starts(&mut self, i: usize, pat: &str) -> bool {
        self.work += 1;
        pat.chars()
            .enumerate()
            .all(|(k, p)| self.c.get(i + k) == Some(&p))
    }

    fn starts_nocase(&mut self, i: usize, pat: &str) -> bool {
        self.work += 1;
        pat.chars().enumerate().all(|(k, p)| {
            self.c
                .get(i + k)
                .is_some_and(|c| c.eq_ignore_ascii_case(&p))
        })
    }

    /// The end of the maximal run of `class` characters that contains or starts at `i` (`i` itself when `c[i]` is not one).
    fn run(&mut self, class: Class, i: usize) -> usize {
        let slot = class as usize;
        if let Some((s, e)) = self.runs[slot]
            && s <= i
            && i < e
        {
            return e;
        }
        let mut j = i;
        while j < self.c.len() && class.accepts(self.c[j]) {
            j += 1;
        }
        self.work += (j - i) as u64 + 1;
        if j > i {
            self.runs[slot] = Some((i, j));
        }
        j
    }

    /// A token that begins at `i` (a word boundary) by one of the token shapes; the index after the part of it that is hidden.
    fn token_end(&mut self, i: usize) -> Option<usize> {
        let c = self.c;
        let at = |k: usize| c.get(i + k).copied();
        // GitHub: ghp_, gho_, ghu_, ghs_, ghr_ and 30 or more letters and digits.
        if at(0) == Some('g')
            && at(1) == Some('h')
            && matches!(at(2), Some('p' | 'o' | 'u' | 's' | 'r'))
            && at(3) == Some('_')
        {
            return self.body(i, 4, 30, None);
        }
        if self.starts(i, "github_pat_") {
            return self.body(i, 11, 30, None);
        }
        if self.starts(i, "glpat-") {
            return self.body(i, 6, 16, None);
        }
        if at(0) == Some('x')
            && at(1) == Some('o')
            && at(2) == Some('x')
            && matches!(at(3), Some('a' | 'b' | 'p' | 'r' | 's'))
            && at(4) == Some('-')
        {
            return self.body(i, 5, 10, None);
        }
        if self.starts(i, "AIza") {
            return self.body(i, 4, 35, None);
        }
        if self.starts(i, "npm_") {
            return self.body(i, 4, 30, None);
        }
        if self.starts(i, "sk-") {
            // A secret key has digits; a hyphenated word does not.
            return self.body(i, 3, 24, Some(char::is_ascii_digit));
        }
        for p in ["AKIA", "ASIA"] {
            if self.starts(i, p) {
                self.work += 20;
                let ok = (4..20).all(|k| {
                    at(k).is_some_and(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
                }) && !at(20).is_some_and(|ch| ch.is_ascii_alphanumeric());
                return ok.then_some(i + 20);
            }
        }
        // JSON web token: three dot-separated base64url segments, the first beginning with a JSON brace ("eyJ").
        if self.starts(i, "eyJ") {
            let a = self.run(Class::B64Url, i);
            if a - i >= 10 && c.get(a) == Some(&'.') {
                let b = self.run(Class::B64Url, a + 1);
                if b - (a + 1) >= 2 && c.get(b) == Some(&'.') {
                    let e = self.run(Class::B64Url, b + 1);
                    if e - (b + 1) >= 8 {
                        return Some(e);
                    }
                }
            }
        }
        None
    }

    /// A prefixed token: `prefix` characters, then a run of at least `min` base64url characters (with a `needs` character among them
    /// when given); the index after the run, cut at [`TOKEN_BODY_CAP`].
    fn body(
        &mut self,
        i: usize,
        prefix: usize,
        min: usize,
        needs: Option<fn(&char) -> bool>,
    ) -> Option<usize> {
        let end = self.run(Class::B64Url, i + prefix);
        let len = end - (i + prefix);
        if len < min {
            return None;
        }
        let end = end.min(i + prefix + TOKEN_BODY_CAP);
        if let Some(f) = needs
            && !self.c[i + prefix..end].iter().any(f)
        {
            return None;
        }
        Some(end)
    }

    /// `Bearer <token>` or `Basic <token>` at `i`: where the token starts and where it ends.
    fn scheme_token(&mut self, i: usize) -> Option<(usize, usize)> {
        let (word, bearer) = if self.starts_nocase(i, "bearer") {
            (6, true)
        } else if self.starts_nocase(i, "basic") {
            (5, false)
        } else {
            return None;
        };
        // One or more blanks between the scheme and its credential.
        let blank_from = i + word;
        let mut from = blank_from;
        while from < self.c.len() && matches!(self.c[from], ' ' | '\t') && from - blank_from < 8 {
            from += 1;
        }
        self.work += (from - blank_from) as u64;
        if from == blank_from {
            return None;
        }
        let end = self.run(Class::Bearer, from).min(from + 4096);
        let len = end - from;
        let in_header = self.authorization_before(i);
        let head = &self.c[from..end.min(from + 64)];
        let looks_random = head
            .iter()
            .any(|ch| ch.is_ascii_digit() || matches!(ch, '.' | '_' | '~' | '+' | '/' | '=' | '-'));
        let ok = if in_header {
            len >= 4
        } else if bearer {
            len >= 16 && looks_random
        } else {
            len >= 12 && looks_random
        };
        ok.then_some((from, end))
    }

    /// True when the text just before `i` is `authorization:` or `authorization=` (with blanks and quotes around).
    fn authorization_before(&mut self, i: usize) -> bool {
        fn skip(me: &mut Redactor<'_>, from: usize) -> usize {
            let mut k = from;
            while k > 0 && from - k < BACK_CAP && matches!(me.c[k - 1], ' ' | '\t' | '"' | '\'') {
                k -= 1;
            }
            me.work += (from - k) as u64;
            k
        }
        let k = skip(self, i);
        if k == 0 || !matches!(self.c[k - 1], ':' | '=') {
            return false;
        }
        let k = skip(self, k - 1);
        const WORD: &str = "authorization";
        k >= WORD.len() && self.starts_nocase(k - WORD.len(), WORD)
    }

    /// A password in the user-info of a URL: at `://`, the index of the `@` when the part before it holds a colon.
    fn userinfo_end(&mut self, i: usize) -> Option<usize> {
        if !self.starts(i, "://") {
            return None;
        }
        let from = i + 3;
        let mut j = from;
        while j < self.c.len() && j - from < USERINFO_CAP {
            let ch = self.c[j];
            if ch.is_whitespace()
                || matches!(ch, '/' | '?' | '#' | '@' | '"' | '\'' | '`' | '<' | '>')
            {
                break;
            }
            j += 1;
        }
        self.work += (j - from) as u64 + 1;
        (self.c.get(j) == Some(&'@') && self.c[from..j].contains(&':')).then_some(j)
    }

    /// The markers of the URL token `[start, end)`, from the cache or by one scan of the token.
    fn markers_of(&mut self, start: usize, end: usize) -> Markers {
        if let Some((s, e, m)) = self.markers
            && s <= start
            && end <= e
        {
            return m;
        }
        let text: String = self.c[start..end].iter().collect();
        self.work += (end - start) as u64 * 8;
        let last = |pat: &str| -> Option<usize> {
            text.rfind(pat).map(|b| start + text[..b].chars().count())
        };
        let m = Markers {
            amz_signature: last("X-Amz-Signature="),
            amz_token: last("X-Amz-Security-Token="),
            signature: last("Signature="),
            access_key_id: last("AWSAccessKeyId="),
            key_pair_id: last("Key-Pair-Id="),
            goog_signature: last("X-Goog-Signature="),
            sig: last("&sig=").max(last("?sig=")),
            sv: last("&sv=").max(last("?sv=")),
        };
        self.markers = Some((start, end, m));
        m
    }

    /// The end of a signed query that starts at the `?` at `i`.
    fn presigned_query_end(&mut self, i: usize) -> Option<usize> {
        if i < self.unsigned_until {
            return None;
        }
        let end = self.run(Class::Url, i);
        let start = self.runs[Class::Url as usize].map_or(i, |(s, _)| s.min(i));
        let m = self.markers_of(start, end);
        let after = |p: Option<usize>| p.is_some_and(|p| p >= i);
        let signed = after(m.amz_signature)
            || after(m.amz_token)
            || after(m.goog_signature)
            || (after(m.signature) && (after(m.access_key_id) || after(m.key_pair_id)))
            || (after(m.sig) && after(m.sv));
        if signed {
            Some(end)
        } else {
            // Every `?` of this token has the same markers or fewer.
            self.unsigned_until = end;
            None
        }
    }

    /// A credential-named setting at `i`: where its value starts (after the separator and the blanks; a quote counts as part of the
    /// value) and where the value ends (after its closing quote).
    fn named_value(&mut self, i: usize) -> Option<(usize, usize)> {
        let mut found = None;
        for n in CREDENTIAL_NAMES {
            if self.starts_nocase(i, n) {
                found = Some(n.len());
                break;
            }
        }
        let mut k = i + found?;
        // A name inside a longer word ("passwords", "secretary") is not one.
        if self.c.get(k).is_some_and(|c| is_alnum(*c)) {
            return None;
        }
        if matches!(self.c.get(k), Some('"' | '\'')) {
            k += 1;
        }
        fn blanks(me: &mut Redactor<'_>, from: usize) -> usize {
            let mut k = from;
            while matches!(me.c.get(k), Some(' ' | '\t')) && k - from < 16 {
                k += 1;
            }
            me.work += (k - from) as u64;
            k
        }
        k = blanks(self, k);
        let sep = *self.c.get(k)?;
        if sep != '=' && sep != ':' {
            return None;
        }
        k = blanks(self, k + 1);
        let first = *self.c.get(k)?;
        let quoted = first == '"' || first == '\'';
        let (v_start, v_end, end) = if quoted {
            let limit = (k + 1 + QUOTED_CAP).min(self.c.len());
            let close = (k + 1..limit).find(|j| self.c[*j] == first);
            self.work += (close.unwrap_or(limit) - k) as u64;
            let close = close?;
            (k + 1, close, close + 1)
        } else {
            let e = self.run(Class::Value, k);
            (k, e, e)
        };
        let len = v_end - v_start;
        let value = &self.c[v_start..v_end];
        let word: String = value
            .iter()
            .take(9)
            .collect::<String>()
            .to_ascii_lowercase();
        let placeholder = matches!(value.first(), Some('$' | '{' | '<' | '%'))
            || word.starts_with("[redacted")
            || (len <= 8
                && matches!(
                    word.as_str(),
                    "true" | "false" | "null" | "none" | "required"
                ));
        if len < 3 || placeholder {
            return None;
        }
        // After a colon, an unquoted plain word is prose ("password: required"); a quoted value is data.
        if sep == ':' && !quoted && value.iter().all(char::is_ascii_alphabetic) {
            self.work += len as u64;
            return None;
        }
        Some((k, end))
    }

    fn redact(mut self) -> (String, u64) {
        let c = self.c;
        let mut out = String::with_capacity(c.len());
        let mut i = 0;
        while i < c.len() {
            self.work += 4;
            let boundary = i == 0 || !is_alnum(c[i - 1]);
            if boundary {
                if let Some(end) = self.token_end(i) {
                    out.push_str(REDACTED);
                    i = end;
                    continue;
                }
                if let Some((from, end)) = self.scheme_token(i) {
                    out.extend(&c[i..from]);
                    out.push_str(REDACTED);
                    i = end;
                    continue;
                }
                if let Some((value_start, end)) = self.named_value(i) {
                    // The name, the separator and the blanks as typed, then the hidden value.
                    out.extend(&c[i..value_start]);
                    out.push_str(REDACTED);
                    i = end;
                    continue;
                }
            }
            if c[i] == ':'
                && let Some(at) = self.userinfo_end(i)
            {
                out.push_str("://");
                out.push_str(REDACTED);
                i = at;
                continue;
            }
            if c[i] == '?'
                && let Some(end) = self.presigned_query_end(i)
            {
                out.push('?');
                out.push_str(REDACTED);
                i = end;
                continue;
            }
            out.push(c[i]);
            i += 1;
        }
        (out, self.work)
    }
}

/// True for the name in the header of a private key block (`RSA PRIVATE KEY`, `PGP PRIVATE KEY BLOCK`, ...).
fn key_header(h: &str) -> bool {
    h.ends_with("PRIVATE KEY") || h.ends_with("PRIVATE KEY BLOCK")
}

/// Replaces every private key block, whole, or from its header to the end of the text when it is not finished.
fn redact_private_keys(input: &str) -> String {
    const BEGIN: &str = "-----BEGIN ";
    const END: &str = "-----END ";
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(b) = rest.find(BEGIN) {
        let after = &rest[b + BEGIN.len()..];
        let header = after.find("-----").map(|e| (&after[..e], e + 5));
        let Some((name, header_len)) = header.filter(|(name, _)| key_header(name)) else {
            out.push_str(&rest[..b + BEGIN.len()]);
            rest = after;
            continue;
        };
        out.push_str(&rest[..b]);
        out.push_str(PRIVATE_KEY);
        let body = &after[header_len..];
        // The end line carries the same name; any other END line inside the body is part of it.
        let want = format!("{END}{name}-----");
        match body.find(&want) {
            Some(e) => rest = &body[e + want.len()..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Hides token-shaped strings in `input`; everything else is returned as it was. See the module doc for what counts.
pub fn redact_tokens(input: &str) -> String {
    redact_with_work(input).0
}

/// [`redact_tokens`] and the number of steps the scan took (characters examined), for tests that bound the cost by the text's length.
pub fn redact_with_work(input: &str) -> (String, u64) {
    let text = redact_private_keys(input);
    let chars: Vec<char> = text.chars().collect();
    let (out, work) = Redactor::new(&chars).redact();
    (out, work + input.len() as u64)
}
