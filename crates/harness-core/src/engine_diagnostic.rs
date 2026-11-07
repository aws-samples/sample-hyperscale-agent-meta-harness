//! What a failed turn's error text says about the engine process. A turn could die within
//! seconds with "the engine closed its output", agentd kept no word of why, and the cause had to be inferred from command
//! lines. When an engine's output closes, agentd now adds how the process ended and a short, scrubbed line of its stderr to
//! the error text, inside the `error` field that already exists (`TurnCompleteRequest.error`, `InvocationResponse.error`): the
//! protocol is unchanged.
//!
//! The text is `<agentd's own error><SEPARATOR><body>]`: agentd's text first, which no engine byte can forge, then the
//! diagnostic, whose last part is the engine's own words. Three readers know the separator, and this module is where they get
//! it: [`strip`] gives the part the orchestrator's classifier and the agent's next prompt read (the added text never changes a
//! verdict and never reaches a model), [`stable`] gives the part a Blocked fingerprint is made from (the same exit with another
//! tail is the same blocker), and a person reads all of it. Both act only on the two texts agentd writes for a failure on the
//! engine's pipes (`the engine closed its output`, `engine I/O: ...`); any other error text can hold engine words and is read
//! whole, so an engine cannot hide part of its own error by writing the separator. The diagnostic is always the last part of
//! the text: whoever adds a sentence to an error that has one adds it ahead of the diagnostic ([`with_note`]), and [`split`]
//! gives a person-facing reader both parts.
//!
//! The stderr tail is untrusted text from a process that may echo a prompt, a file or a credential. [`scrub`] is a pattern
//! scrub: it removes what looks like a secret and the control, escape and bidirectional characters that could forge a log
//! line. The shapes it removes, in the order it applies them: a line of nothing but base64 text; presigned query strings;
//! the value of any key that names a secret in any spelling (`SecretAccessKey`, `secretAccessKey`, `AWS_SESSION_TOKEN`,
//! `x-api-key`, `client-secret`), quoted, single-quoted, escaped inside a JSON string, or on the line after its key; HTTP
//! credential headers to the end of the header (`Authorization`, `Cookie`, `X-Amz-Security-Token`); the value after a
//! secret-named command-line flag and a `-u user:secret` pair; URL user information, with or without a secret after the
//! user; `.netrc`, Maven XML and Docker `"auth"` entries; any whole word of 40 or more standard-base64 characters that holds
//! a `/` or `+` and mixes upper case, lower case and digits (a path that starts with a lower-case directory is kept); then
//! the shapes of `refusal::redact`, `_authToken`-style keys, provider token prefixes, hex digests and bare account ids.
//! A pattern scrub cannot know that a sentence is a file's content, so what leaves the VM is bounded: the orchestrator gets
//! one line of at most [`ERROR_BODY_CHARS`] characters ([`error_body`]), the runtime log at most [`LOG_TAIL_CHARS`]
//! ([`log_body`]), and a cut window with no line break, the end of one long line (a dumped payload), is not shown at all.
//!
//! Pure: no I/O. agentd reads the file and reaps the process.

use crate::refusal::redact;
use regex::{Captures, Regex};
use std::sync::LazyLock;

/// Follows agentd's own error text and opens the diagnostic; the body ends with `]`.
pub const SEPARATOR: &str = " [engine process: ";

/// agentd reads this many bytes from the end of the engine's stderr log.
pub const STDERR_WINDOW_BYTES: usize = 4096;

/// The most characters between [`SEPARATOR`] and the closing `]` in the orchestrator's error text. With agentd's own text
/// (28 characters for a closed output) and the framing (19), the whole error stays under the 200 characters of a fault
/// sample (`redact_sample`), so the cut never takes the end of the diagnostic. An `engine I/O: ...` text is longer (the
/// operating system's words follow), so a sample may cut the end of the stderr part; the exit part comes first and survives,
/// and [`stable`] reads only that.
pub const ERROR_BODY_CHARS: usize = 150;

/// The most characters of scrubbed stderr in the runtime log line.
pub const LOG_TAIL_CHARS: usize = 1500;

/// Where the stderr part starts inside a body.
const STDERR_PART: &str = "; stderr";

/// The texts agentd writes for a failure on the engine's pipes (`AcpError::Closed` and `AcpError::Io`): the only ones that
/// carry a diagnostic. Any other error text may hold engine words (an RPC message, a provider reply) and is read whole, so
/// an engine cannot make a classifier skip part of its own error by writing the separator.
const TRANSPORT_TEXTS: [&str; 2] = ["the engine closed its output", "engine I/O: "];

fn is_transport_text(text: &str) -> bool {
    TRANSPORT_TEXTS.iter().any(|t| text.starts_with(t))
}

/// The most characters of the exit description inside a body.
const EXIT_CHARS: usize = 80;

/// `base` with the diagnostic `body` appended.
pub fn append(base: &str, body: &str) -> String {
    format!("{base}{SEPARATOR}{body}]")
}

/// agentd's text and the diagnostic apart: everything before the first separator (agentd's own error, plus any note the
/// orchestrator added ahead of the diagnostic with [`with_note`]) and the diagnostic itself, `[engine process: ...]`. Only the
/// two texts agentd writes for the engine's pipes (`TRANSPORT_TEXTS`) carry one; any other text comes back whole, so an engine
/// cannot hide part of its own error by writing the separator.
pub fn split(error: &str) -> (&str, Option<&str>) {
    if !is_transport_text(error) {
        return (error, None);
    }
    match error.find(SEPARATOR) {
        // The separator opens with a space that belongs to neither part.
        Some(at) => (&error[..at], Some(&error[at + 1..])),
        None => (error, None),
    }
}

/// `error` without the diagnostic: agentd's own text and whatever was added ahead of the diagnostic. The first separator
/// decides, so no tail can move the cut, and only the two texts agentd writes for the engine's pipes are cut at all.
pub fn strip(error: &str) -> &str {
    split(error).0
}

/// `error` with `note` added after agentd's own text and ahead of the diagnostic: `<agentd's text>; <note> [engine process:
/// ...]`. The diagnostic stays the last part of the text, so [`strip`] and [`stable`] still cut it off whole and keep the
/// note. A text with no diagnostic gets `; <note>` at its end, as it always did.
pub fn with_note(error: &str, note: &str) -> String {
    // The note can hold paths the engine chose; for a text `strip` cuts, a separator inside one would end the cut inside the note.
    let note = if is_transport_text(error) {
        note.replace(SEPARATOR, " (engine process: ")
    } else {
        note.to_string()
    };
    match split(error) {
        (head, Some(diagnostic)) => format!("{head}; {note} {diagnostic}"),
        (head, None) => format!("{head}; {note}"),
    }
}

/// `text` with the stderr part of the diagnostic removed and the exit part kept: what a fingerprint reads. A sample cut
/// anywhere in the stderr part comes out the same (the exit part ends well inside the 200 characters of a sample); a text
/// that is not one of agentd's two transport texts comes out as it was.
pub fn stable(text: &str) -> String {
    let split = text
        .split_once(SEPARATOR)
        .filter(|_| is_transport_text(text));
    let Some((base, rest)) = split else {
        return text.to_string();
    };
    let exit = rest
        .split_once(STDERR_PART)
        .map_or_else(|| rest.trim_end_matches(']'), |(exit, _)| exit);
    format!("{base}{SEPARATOR}{exit}]")
}

/// The scrubbed tail of an engine's stderr log: its lines, and how much of the log they come from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StderrTail {
    lines: Vec<String>,
    /// Bytes of the log that were read (at most [`STDERR_WINDOW_BYTES`]).
    pub bytes: usize,
    /// The log held more than the window: earlier output was not read.
    pub cut: bool,
    /// The window held no line break, so it is the end of one long line (a dumped payload, minified source): not shown.
    pub long_line: bool,
}

impl StderrTail {
    /// Nothing to show: no lines (a blank log, or a long line that is not shown).
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The lines on one line, ` | ` between them.
    pub fn joined(&self) -> String {
        self.lines.join(" | ")
    }

    /// The line that says what failed: the last one shaped like an error message (`Error: ...`, `TypeError: ...`,
    /// `FATAL ERROR`, `panic:`, `thread 'x' panicked`, `npm ERR!`, `Traceback`, `[ERROR]`, an exception name), else the last
    /// line. A stack frame (`at f (file:1:1)`) is not error shaped, so a Node crash gives its message and not its last frame.
    pub fn headline(&self) -> Option<&str> {
        self.lines
            .iter()
            .rev()
            .find(|l| ERROR_SHAPE.is_match(l))
            .or_else(|| self.lines.last())
            .map(String::as_str)
    }
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|e| panic!("engine diagnostic pattern {pattern}: {e}"))
}

static ERROR_SHAPE: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)^\s*\[?\s*(?:[a-z0-9_.$]*(?:error|exception)\b|fatal\b|panic\b|thread\b.*\bpanicked\b|uncaught\b|unhandled\b|traceback\b|npm err|abort(?:ed)?\b)",
    )
});

/// Terminal escape sequences: CSI, OSC and two-character escapes.
static ESCAPES: LazyLock<Regex> = LazyLock::new(|| {
    re(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]")
});

/// A presigned or tokenised URL: everything after `?`. It runs before `refusal::redact`, which would put `<redacted>` inside
/// the query and so end the match early.
static URL_QUERY: LazyLock<Regex> =
    LazyLock::new(|| re(r#"(?i)\b((?:https?|wss?|ftp)://[^\s?#'"<>]*)\?[^\s'"<>]*"#));

/// The words that name a secret, in any spelling or case: `SecretAccessKey`, `secret_access_key`, `AWS_SESSION_TOKEN`,
/// `clientSecret`, `x-api-key`, `Proxy-Authorization`, `Set-Cookie`, `keyStorePassword`.
const SECRET_WORDS: &str = r"secret|token|passw(?:or)?d|passphrase|credential|api[_-]?key|access[_-]?key|private[_-]?key|signature|authorization|cookie";

/// The HTTP authentication schemes that come before the credential in an `Authorization` value.
const SCHEME_WORDS: &str = "bearer|basic|digest|negotiate|ntlm";

/// The schemes of an `Authorization`-style value whose credential is one token, after a blank or on the next line. `SCHEME_WORDS`
/// are the five the key rule takes after any secret key; the rest are taken only after an authorization key.
const TOKEN_SCHEMES: [&str; 16] = [
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
];

/// The schemes whose credential is a list of parameters (`Digest username="bob", nonce="...", response="..."`): all of it, to the
/// end of the line, is the credential.
const PARAMETER_SCHEMES: [&str; 6] = [
    "digest",
    "aws4-hmac-sha256",
    "oauth",
    "hawk",
    "signature",
    "scram-sha-256",
];

/// Short words that name a secret only as a whole part of a key (`DB_PASS`, `MYSQL_PWD`, `GITHUB_PAT`, `REDIS_AUTH`, `dbPass`):
/// inside a longer word they are ordinary (`compass`, `bypass`, `path`, `author`).
const SHORT_SECRET_WORDS: [&str; 6] = ["pass", "pwd", "pw", "pat", "auth", "bearer"];

/// `key`, the closing quote of the key (also escaped, inside a JSON string), a `:`, `=`, `:=` or `=>`, then a value that is
/// double-quoted or single-quoted (a quote inside it escaped with a backslash does not end it; a quote that is itself escaped,
/// inside a JSON string, is read up to the next quote) or one unquoted word, which may follow an authentication scheme word on its line
/// (`authorization=Bearer <token>`: the scheme word is not the value). The value may start on the next line.
///
/// The key is one of: a name that holds a secret word anywhere (`SECRET_WORDS`, and `webhook`); a name that has a short secret
/// word as a whole part (`DB_PASS`); a compound that ends in `key` (`OPENAI_KEY`); or a camelCase name that ends in a short word
/// or `Key` (`dbPass`, `openaiKey`, `ldapPw`), or a short word after the `D` of a JVM system property (`-Dpass=...`). `key_outcome` decides the last three, which are ambiguous.
static KEY_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    let short = SHORT_SECRET_WORDS.join("|");
    re(&format!(
        r#"(?i)\b([a-z0-9_.\-]*(?:{SECRET_WORDS}|webhook)[a-z0-9_.\-]*|(?:[a-z0-9]+[_.\-])*(?:{short})(?:[_.\-][a-z0-9]+)*|(?:[a-z0-9]+[_.\-])+key|[a-z0-9_.\-]*[a-z0-9](?-i:Pass|Pwd|Pat|Auth|Bearer|Key|Pw)|(?-i:D)(?:{short}))((?:\\*["'])?[ \t]*(?:=>|:=|[:=])\s*)("(?:[^"\\\r\n]|\\.)*"|'(?:[^'\\\r\n]|\\.)*'|\\*"[^"\r\n]*"|\\*'[^'\r\n]*'|(?:(?:{SCHEME_WORDS})[ \t]+)?[^\s"',;&|\\]+)"#
    ))
});

/// The words of `SECRET_WORDS`: a key that holds one names a secret whatever its value is.
static STRONG_KEY: LazyLock<Regex> = LazyLock::new(|| re(&format!("(?i){SECRET_WORDS}")));

/// A key that ends in one of these names a kind, an identifier or a count, not a secret (`ExpiredTokenException`,
/// `JsonWebTokenError`, `InvalidClientTokenId`, `token_type`, `max_tokens`, `inputTokens`): its value is read as it is.
const NOT_SECRET_SUFFIXES: [&str; 13] = [
    "exception",
    "error",
    "id",
    "type",
    "name",
    "expiration",
    "expiry",
    "expires",
    "file",
    "path",
    "url",
    "uri",
    "tokens",
];

/// File extensions that make a key a file name: `token.js:12:5` in a stack frame is a position, not a secret.
const SOURCE_EXTENSIONS: [&str; 24] = [
    "js", "mjs", "cjs", "ts", "tsx", "jsx", "json", "py", "rs", "go", "java", "kt", "rb", "php",
    "c", "cc", "cpp", "h", "cs", "sh", "yaml", "yml", "toml", "xml",
];

/// `<file name with a secret word>.<extension>:<line>[:<column>]`, as a stack frame, a compiler message or a Rust panic (which
/// ends it with a colon) prints it.
fn is_source_position(key: &str, value: &str) -> bool {
    let extension = key
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase());
    extension.is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext.as_str()))
        && value
            .trim_end_matches([')', ':'])
            .split(':')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

/// HTTP headers whose whole value is a credential: redacted to the end of the line, to the closing `'` of a quoted command-line
/// argument (`-H 'Authorization: ...'`), or, when the value itself is in single quotes (`{ cookie: 'sid=...' }`, as Node prints
/// a header object), to its closing quote; an array value (`set-cookie: [ ... ]`) to its closing bracket. A JSON member
/// (`"Authorization": "..."`) has a quote between the name and the colon, so it is the key rule's and the next member stays. The
/// value starts with a character that is neither blank nor a quote, or is a quoted text of at least one character: a pattern that
/// allowed a blank first would let the blanks after the colon backtrack into the value and leave a quoted value behind.
static CREDENTIAL_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r#"(?i)\b((?:proxy-)?authorization|(?:set-)?cookie|x-amz-security-token|x-api-key|x-auth-token)([ \t]*:[ \t]*)('[^'\r\n]+'?|[^\s'][^\r\n']*)"#,
    )
});

/// A command-line flag that names a secret, with its value after a space: `--password VALUE`, `--token VALUE`, `-api-key VALUE`.
static SECRET_FLAG: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r#"(?im)(^|[ \t])(--?[a-z0-9-]*(?:password|passwd|passphrase|secret|token|api-?key|access-?key|private-?key|credential|authorization)[a-z0-9-]*)([ \t]+)("[^"\r\n]*"|'[^'\r\n]*'|[^\s"'-]\S*)"#,
    )
});

/// `-u user:secret`, `--user user:secret`, `--proxy-user user:secret` (a value with no `:` is a user name only).
static USER_PAIR_FLAG: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r#"(?im)(^|[ \t])((?:-u|--user|--proxy-user)[ \t]+)("[^"\r\n]*:[^"\r\n]*"|'[^'\r\n]*:[^'\r\n]*'|[^\s:"']+:[^\s"']+)"#,
    )
});

/// URL user information with no secret after the user: `https://<token>@host/...`. A plain account name (lower-case letters
/// and hyphens: `git`, `deploy`) stays.
static URL_USER: LazyLock<Regex> =
    LazyLock::new(|| re(r#"(?i)\b([a-z][a-z0-9+.-]*://)([^/\s:@"'<>]+)@"#));

/// `scheme://`, where the user information of a URL starts.
static URL_SCHEME: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\b[a-z][a-z0-9+.-]*://"));

/// A user name that is not a credential: at most 24 lower-case letters and hyphens (`git`, `deploy`, `x-access-token`). A token
/// has digits, upper case or an underscore.
fn plain_account_name(user: &str) -> bool {
    user.len() <= 24 && user.chars().all(|c| c.is_ascii_lowercase() || c == '-')
}

/// Where the user information of the URL whose text after `scheme://` is `run` (up to the next blank or quote) ends: the byte
/// offset of the `@` that precedes the host, when the user information holds a password. `None` for a URL with no user
/// information, or with a user name only (`URL_USER`'s).
///
/// The authority ends at the first `/`, `?` or `#`, and its user information at the last `@` inside it, so a raw `@` in a
/// password is covered (the way a browser reads it). A raw `/` ends the authority early: when the authority is `user:rest` where
/// `rest` is not a port (`host:8080/@vite/client`), the last `@` of the run is taken as the one before the host.
fn password_userinfo_end(run: &str) -> Option<usize> {
    let authority_end = run.find(['/', '?', '#']).unwrap_or(run.len());
    let authority = &run[..authority_end];
    if let Some(at) = authority.rfind('@') {
        // A password with an `@` and then a `/` ends the authority early: the host is after a later `@`.
        return run[..at]
            .contains(':')
            .then(|| last_at_not_after_a_slash(run).unwrap_or(at));
    }
    // An IPv6 host has colons of its own.
    if authority.starts_with('[') && authority.contains(']') {
        return None;
    }
    let colon = authority.find(':')?;
    if authority[colon + 1..].bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    last_at_not_after_a_slash(run)
        .or_else(|| run[authority_end..].rfind('@').map(|at| authority_end + at))
}

/// The offset of the last `@` in `run` that does not follow a `/`: an `@` after a slash starts an npm scope or a path part
/// (`/@scope/pkg`), not the host.
fn last_at_not_after_a_slash(run: &str) -> Option<usize> {
    run.match_indices('@')
        .map(|(i, _)| i)
        .rfind(|&i| i > 0 && run.as_bytes()[i - 1] != b'/')
}

/// `text` with the user information of every URL that holds a password replaced: `scheme://<redacted>@host/path`. A password
/// may be empty-user (`redis://:secret@cache`), may hold `@` and `/`, and is percent-encoded or raw.
fn redact_url_passwords(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut done = 0;
    for scheme in URL_SCHEME.find_iter(text) {
        // Inside user information that is already replaced.
        if scheme.start() < done {
            continue;
        }
        let rest = &text[scheme.end()..];
        let run_len = rest
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`'))
            .unwrap_or(rest.len());
        if let Some(end) = password_userinfo_end(&rest[..run_len]) {
            out.push_str(&text[done..scheme.end()]);
            out.push_str(REDACTED_MARK);
            done = scheme.end() + end;
        }
    }
    out.push_str(&text[done..]);
    out
}

/// `.netrc`: `login <user> password <secret>`.
static NETRC: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\b(login[ \t]+\S+[ \t]+passw(?:or)?d[ \t]+)\S+"));

/// A Maven or other XML element that holds a secret: `<password>...</password>`.
static XML_SECRET: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)(<[a-z0-9._-]*(?:password|passphrase|secret|token)[a-z0-9._-]*>)[^<]*(</)")
});

/// A Docker `config.json` entry: `"auth": "<base64 of user:secret>"`, also escaped inside a JSON string.
static DOCKER_AUTH: LazyLock<Regex> =
    LazyLock::new(|| re(r#"(?i)(\\*["']auth\\*["'][ \t]*:\s*)\\*(["'])[^"'\r\n]*["']"#));

/// A secret named in words: `Secret access key: VALUE`, `Session token = VALUE`, `API key: VALUE`.
static SPOKEN_KEY: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r#"(?i)\b((?:secret|session|security|access|api|private|client|auth|bearer)[ \t](?:access[ \t])?(?:key|token|secret)(?:[ \t]id)?[ \t]*[:=]\s*)[^\s"',;&|\\]+"#,
    )
});

/// A run of standard-base64 text, `-` and `_` included so that a url-safe word is not cut into pieces: the word rule decides.
static BASE64_WORD: LazyLock<Regex> = LazyLock::new(|| re(r"[A-Za-z0-9+/_-]{40,}={0,2}"));

/// A path that starts at a directory in lower case (`/var/lib/agentd/...`): not a secret, whatever its ids hold.
static ABSOLUTE_PATH: LazyLock<Regex> = LazyLock::new(|| re(r"^/[a-z][a-z0-9]{0,11}/"));

/// Whether a run of base64 characters is a file path (`src/main/java/com/example/demo/UserController1`) and not a secret. A
/// secret has about 41% lower-case letters among its letters and digits, a path 65% or more, so the share tells them apart;
/// beyond that the run needs the shape of a path, which `followed_by_extension` (`.java` right after it) or three slashes give.
fn looks_like_a_path(word: &str, followed_by_extension: bool) -> bool {
    let slashes = word.matches('/').count();
    if slashes == 0 || word.contains('+') || !(followed_by_extension || slashes >= 3) {
        return false;
    }
    let alphanumeric = word.chars().filter(char::is_ascii_alphanumeric).count();
    let lower = word.chars().filter(char::is_ascii_lowercase).count();
    lower * 100 >= alphanumeric * 65
}

/// Whether `rest`, the text right after a word, starts a file extension: a dot and one to twelve letters or digits.
fn starts_with_extension(rest: &str) -> bool {
    rest.strip_prefix('.').is_some_and(|after| {
        let n = after
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .count();
        (1..=12).contains(&n)
    })
}

/// What a key ending in `key` is the key of, when it is not a secret (`primary_key`, `cacheKey`).
const KEY_KINDS: [&str; 26] = [
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
    "public",
    "kms",
];

/// The last part of a key that names a kind, a mode or a state, not a secret (`auth_method`, `pass_through`, `auth_enabled`).
const SHORT_KEY_KINDS: [&str; 22] = [
    "type", "method", "mode", "scheme", "provider", "enabled", "disabled", "required", "header",
    "headers", "realm", "flow", "status", "strategy", "timeout", "through", "count", "retries",
    "attempts", "failed", "error", "errors",
];

/// Values that say how things went and are not a credential (`pass: ok`, `auth=none`, `pass=true`).
const PLAIN_VALUES: [&str; 18] = [
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
];

/// The last part of a key that names a number: a port, a rate, a limit, a size, a duration (`auth_port`, `pass_rate`). With a number
/// as the value such a key is read as it is.
const NUMERIC_KEY_KINDS: [&str; 16] = [
    "port",
    "rate",
    "total",
    "ratio",
    "percent",
    "percentage",
    "limit",
    "size",
    "length",
    "duration",
    "max",
    "min",
    "ttl",
    "interval",
    "level",
    "version",
];

/// The last part of a key that names a place: a host, a domain, a server, an endpoint (`auth_host`, `auth_server`). With a host
/// name, an address or a path as the value such a key is read as it is.
const PLACE_KEY_KINDS: [&str; 6] = ["host", "domain", "server", "endpoint", "address", "addr"];

/// The directories under which a working directory is (`pwd: /home/x`): a `pwd` value that starts with one of these and goes on is a path.
const WORKING_DIRECTORY_ROOTS: [&str; 16] = [
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
];

/// Whether `s` is a number with at most a two-character unit (`8080`, `0.95`, `30s`, `95%`, `512kb`).
fn is_number_with_unit(s: &str) -> bool {
    let digits = s
        .bytes()
        .take_while(|b| b.is_ascii_digit() || *b == b'.')
        .count();
    let unit = &s[digits..];
    s.starts_with(|c: char| c.is_ascii_digit())
        && unit.len() <= 2
        && unit.bytes().all(|b| b.is_ascii_alphabetic() || b == b'%')
}

/// The parts of a key, lower-cased: split at `_`, `.` and `-` and where a lower-case letter or a digit meets an upper-case one
/// (`dbPass` is `db` and `pass`; `APIKey` is `api` and `key`).
fn key_parts(key: &str) -> Vec<String> {
    let chars: Vec<char> = key.chars().collect();
    let mut parts = Vec::new();
    let mut part = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if matches!(c, '_' | '.' | '-') {
            if !part.is_empty() {
                parts.push(std::mem::take(&mut part));
            }
            continue;
        }
        let boundary = c.is_ascii_uppercase()
            && i > 0
            && (chars[i - 1].is_ascii_lowercase()
                || chars[i - 1].is_ascii_digit()
                || (chars[i - 1].is_ascii_uppercase()
                    && chars.get(i + 1).is_some_and(char::is_ascii_lowercase)));
        if boundary && !part.is_empty() {
            parts.push(std::mem::take(&mut part));
        }
        part.push(c.to_ascii_lowercase());
    }
    if !part.is_empty() {
        parts.push(part);
    }
    parts
}

/// Whether a key that holds no secret word of `SECRET_WORDS` names a secret all the same: a webhook (its URL is the credential),
/// a short secret word as a whole part (`DB_PASS`, `githubPat`), or a compound that ends in `key` (`OPENAI_KEY`) unless the part
/// before says what the key is for (`primary_key`).
fn short_key_names_a_secret(key: &str) -> bool {
    let parts = key_parts(key);
    let Some(last) = parts.last() else {
        return false;
    };
    if SHORT_KEY_KINDS.contains(&last.as_str()) {
        return false;
    }
    if key.to_ascii_lowercase().contains("webhook") {
        return true;
    }
    if parts
        .iter()
        .any(|p| SHORT_SECRET_WORDS.contains(&p.as_str()))
    {
        return true;
    }
    parts
        .iter()
        .rev()
        .nth(1)
        .is_some_and(|before| last == "key" && !KEY_KINDS.contains(&before.as_str()))
}

/// Whether the value of a key that is a secret only by a short word is plainly not one: nothing, a bare opener of a structure
/// (`auth: {`), a status word (`pass=true`), a small number or a port, or, for the environment variable `PWD`, a path. A small
/// number is a counter only under the log-style key `pass` (`pass: 12`) and in a host:port form (`auth-service:8080`, with the
/// colon against the number); under any other key (`pw: 1234`, or a `DB_PASS` of 12345) it may be a PIN. A bracketed word
/// (`pwd: [summer2026]`) is a value. A key whose last part names a number (`auth_port`, `pass_rate`) or a place (`auth_host`) with a
/// value of that kind, an S3 object key with a path (`s3Key: runs/01ABC/plan.json`) and `pwd` with a path under a well-known root
/// (`/home/x`), are read as they are.
fn plain_value_of_a_short_key(key: &str, sep: &str, value: &str) -> bool {
    let bare = value.trim_matches(['"', '\'', '\\']);
    let digits = bare.bytes().take_while(u8::is_ascii_digit).count();
    let small_number =
        (1..=5).contains(&digits) && (digits == bare.len() || bare.as_bytes()[digits] == b'/');
    let counter = small_number && (key == "pass" || sep == ":");
    let mut parts = key_parts(key);
    let last = parts.pop().unwrap_or_default();
    // An S3 object key is a path (`s3Key: runs/01ABC/plan.json`); the same name with a value that is no path is still a secret.
    let object_key = last == "key" && parts.last().is_some_and(|p| p == "s3") && bare.contains('/');
    bare.is_empty()
        || matches!(bare, "{" | "[" | "(" | "{}" | "[]" | "()")
        || PLAIN_VALUES.contains(&bare.to_ascii_lowercase().as_str())
        || counter
        || (NUMERIC_KEY_KINDS.contains(&last.as_str()) && is_number_with_unit(bare))
        || (PLACE_KEY_KINDS.contains(&last.as_str()) && bare.contains(['.', '/']))
        || object_key
        || (key == "PWD" && bare.starts_with('/'))
        || (key.eq_ignore_ascii_case("pwd")
            && WORKING_DIRECTORY_ROOTS
                .iter()
                .any(|root| bare.starts_with(root) && bare.len() > root.len()))
}

/// Whether `lower`, a lower-cased key, ends in a kind, an identifier or a count (`token_type`, `max_tokens`, `ExpiredTokenException`):
/// the key says what the value is, not that it is secret. A webhook's URL is the credential, so `slack_webhook_url` is not read as
/// a plain URL.
fn ends_in_a_kind(lower: &str) -> bool {
    NOT_SECRET_SUFFIXES
        .iter()
        .any(|s| lower.ends_with(s) && !(lower.contains("webhook") && matches!(*s, "url" | "uri")))
}

/// Whether `key` names a secret whatever its value is: it holds a secret word, or a short secret word as a whole part of it, and it
/// does not end in a kind.
fn names_a_secret(key: &str) -> bool {
    !ends_in_a_kind(&key.to_ascii_lowercase())
        && (STRONG_KEY.is_match(key) || short_key_names_a_secret(key))
}

/// Whether `s` starts like `key<separator>`: key characters, optional blanks, then `:` or `=`. A cheap test that spares the pattern a
/// search where the value cannot be a key.
fn starts_like_a_key(s: &str) -> bool {
    s.trim_start_matches(|c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        .trim_start_matches([' ', '\t'])
        .starts_with([':', '='])
}

/// Whether a key that names a secret starts at `at` in `text` (`password: ...`, `x-api-key=...`).
fn secret_key_at(text: &str, at: usize) -> bool {
    starts_like_a_key(&text[at..])
        && KEY_VALUE
            .captures_at(text, at)
            .is_some_and(|c| c.get(0).is_some_and(|m| m.start() == at) && names_a_secret(&c[1]))
}

/// Where a second key that names a secret starts at the value of the key at `value_start` (`value` is its text), when the value
/// is that key's name: `pass: password: <secret>` has the label `pass` and the key `password`. The value may start with an
/// authentication scheme word (`auth: Bearer password: <secret>`), or be quoted: a quoted name of a key (`auth: "password":
/// "<secret>"`) or a quoted text whose closing quote is the opening quote of the second key's value (`auth: "x password =
/// "<secret>"`: a stray quote pairs the two) both hold a key that starts inside the value and ends after it.
fn second_key_at(text: &str, value_start: usize, value: &str) -> Option<usize> {
    let bare = value.trim_start_matches('\\');
    if bare.starts_with(['"', '\'']) {
        // Just after the opening quote (an escaped quote inside a JSON string opens with a backslash): a secret key that starts inside
        // the value and ends after its closing quote has taken that quote as its own.
        let open = value_start + (value.len() - bare.len()) + 1;
        let value_end = value_start + value.len();
        let mut from = open;
        while let Some(m) = KEY_VALUE
            .captures_at(text, from)
            .filter(|c| c.get(0).is_some_and(|m| m.start() < value_end))
        {
            let (start, end) = m.get(0).map_or((0, 0), |w| (w.start(), w.end()));
            // The second key's span can reach further than its pattern (a Digest header takes its whole line).
            let reach = match label_outcome(text, &m) {
                KeyOutcome::Replace { end: own, .. } => own.max(end),
                _ => end,
            };
            if reach > value_end && names_a_secret(&m[1]) {
                return Some(start);
            }
            from = end.max(start + 1);
        }
        return None;
    }
    let mut starts = vec![value_start];
    if let Some(blank) = value.find([' ', '\t']) {
        let word = value[blank..].trim_start_matches([' ', '\t']);
        starts.push(value_start + value.len() - word.len());
    }
    starts.into_iter().find(|&at| secret_key_at(text, at))
}

/// Whether a part of `key` is `authorization` or `auth` (`Authorization`, `proxy_authorization`, `x-auth-token`, `REDIS_AUTH`).
fn is_authorization_key(key: &str) -> bool {
    key_parts(key)
        .iter()
        .any(|p| p == "authorization" || p == "auth")
}

/// Whether `s`, the text after a word, is blanks, a `:` or `=`, and then a blank or the end of the line: the word is a key set off
/// from its value (`signing_key = x`).
fn spaced_separator(s: &str) -> bool {
    let t = s.trim_start_matches([' ', '\t']);
    t.len() < s.len()
        && t.starts_with([':', '='])
        && (t.len() == 1 || t[1..].starts_with([' ', '\t', '\n']))
}

/// The end of the token after the scheme word that ends at `word_end`: it follows the blanks and line breaks after the word, and is
/// the run up to the next blank, quote or separator. A word that ends in a colon is a new message (`Error:`), and a word that starts
/// a key that names a secret, with a separator attached or set off by blanks, is that key (`key authorization=Bearer`), not a token.
fn scheme_token_end(text: &str, word_end: usize) -> Option<usize> {
    let rest = &text[word_end..];
    let token = rest.trim_start_matches([' ', '\t', '\r', '\n']);
    let len = token
        .bytes()
        .take_while(|b| {
            !b.is_ascii_whitespace()
                && !matches!(b, b'"' | b'\'' | b',' | b';' | b'&' | b'|' | b'\\')
        })
        .count();
    if len == 0 || token[..len].ends_with(':') {
        return None;
    }
    // A word that starts a secret key, with its separator attached (`key authorization=Bearer`) or set off by blanks (`key signing_key
    // = x`), is that key, not a token; a token that merely ends like a key name (`...Pw`) with a separator and no blank after it
    // (`...Pw =#`) is a token.
    if (token[..len].contains([':', '=']) || spaced_separator(&token[len..]))
        && secret_key_at(text, text.len() - token.len())
    {
        return None;
    }
    Some(text.len() - token.len() + len)
}

/// The end of the credential in an `Authorization`-style value that starts at `start`, when its scheme word is one the key rule's
/// own pattern does not take whole: a token scheme (`Token`, `ApiKey`) with its token, on this line or the next, and a parameter
/// scheme (`Digest`) with everything to the end of the line or to the closing quote of a quoted command-line argument.
fn authorization_value_end(text: &str, start: usize) -> Option<usize> {
    let rest = &text[start..];
    let len = rest
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || *b == b'-')
        .count();
    let word = rest[..len].to_ascii_lowercase();
    let word_end = start + len;
    if PARAMETER_SCHEMES.contains(&word.as_str()) && text[word_end..].starts_with([' ', '\t']) {
        return Some(
            text[word_end..]
                .find(['\r', '\n', '\''])
                .map_or(text.len(), |i| word_end + i),
        );
    }
    if TOKEN_SCHEMES.contains(&word.as_str()) {
        return scheme_token_end(text, word_end);
    }
    None
}

/// `key` as the rules read it: the `D` of a JVM `-Dname=value` system property is not part of a lone short word (`-Dpass=...` is the
/// key `pass`). `start` is where `key` begins in `text`.
fn property_key<'a>(text: &str, start: usize, key: &'a str) -> &'a str {
    match key.strip_prefix('D') {
        Some(rest)
            if text[..start].ends_with('-')
                && SHORT_SECRET_WORDS.contains(&rest.to_ascii_lowercase().as_str()) =>
        {
            rest
        }
        _ => key,
    }
}

/// What the key rule does with one match.
enum KeyOutcome {
    /// The match stays as it is.
    Keep,
    /// The text from the match start to `end` becomes `text`.
    Replace { text: String, end: usize },
    /// The value is another key that names a secret: the label stays as it is and the scan goes on at the value, `from`.
    Resume { from: usize },
}

/// The outcome for the key at `caps`. A value that is itself a key that names a secret is the key's: `pass: password: <secret>`
/// keeps `pass: ` and `password: ` and redacts the secret. When that second key starts inside a quoted value, after other text
/// (a stray quote paired the two: `password="C:\dir\" token="<secret>"`), the text in front of it is the label's and goes with the
/// label's value.
fn key_outcome(text: &str, caps: &Captures) -> KeyOutcome {
    let (Some(_), Some(value_at)) = (caps.get(0), caps.get(3)) else {
        return KeyOutcome::Keep;
    };
    let value = value_at.as_str();
    let Some(from) = second_key_at(text, value_at.start(), value) else {
        return label_outcome(text, caps);
    };
    let bare = value.trim_start_matches('\\');
    let text_start = value_at.start() + (value.len() - bare.len()) + 1;
    if bare.starts_with(['"', '\''])
        && from > text_start
        && matches!(label_outcome(text, caps), KeyOutcome::Replace { .. })
    {
        let lead = &value[..value.len() - bare.len() + 1];
        return KeyOutcome::Replace {
            text: format!(
                "{}{}{lead}{REDACTED_MARK}{} ",
                &caps[1],
                &caps[2],
                &bare[..1]
            ),
            end: from,
        };
    }
    KeyOutcome::Resume { from }
}

/// The outcome for the key at `caps` when its value is not a second key: its value redacted, quotes kept. A key that names a kind
/// or an identifier, a key inside URL user information, and a plain word that starts a sentence after a colon (`ExpiredToken: The
/// provided token has expired.`), are left as they are.
fn label_outcome(text: &str, caps: &Captures) -> KeyOutcome {
    let (Some(whole), Some(value_at)) = (caps.get(0), caps.get(3)) else {
        return KeyOutcome::Keep;
    };
    let (shown_key, sep, value) = (&caps[1], &caps[2], value_at.as_str());
    let key = property_key(text, whole.start(), shown_key);
    let lower = key.to_ascii_lowercase();
    if ends_in_a_kind(&lower) {
        return KeyOutcome::Keep;
    }
    // A key that is a secret by a short word only is ambiguous: its value must look like a credential, and it must be on the same line.
    if !STRONG_KEY.is_match(key)
        && (!short_key_names_a_secret(key)
            || sep.contains('\n')
            // A double colon joins the parts of a path (`tests/test_auth.py::test_login`, `auth::tests::x`).
            || (sep.ends_with(':') && value.starts_with(':'))
            || plain_value_of_a_short_key(key, sep, value))
    {
        return KeyOutcome::Keep;
    }
    if sep.starts_with(':') && is_source_position(key, value) {
        return KeyOutcome::Keep;
    }
    // URL user information (`https://x-access-token:<secret>@host/path`) is `redact`'s: this rule would take the host with it.
    if text[..whole.start()].ends_with("://") {
        return KeyOutcome::Keep;
    }
    let quoted = value.trim_start_matches('\\').starts_with(['"', '\'']);
    // The credential of an authorization key can be longer than the value the pattern took (`authorization=Digest username=...`,
    // `authorization=Token` then a line break).
    let scheme_end = if is_authorization_key(key) {
        authorization_value_end(text, value_at.start())
    } else {
        None
    };
    if scheme_end.is_none() && !quoted && sep.contains(':') && !sep.contains('=') {
        let rest = &text[whole.end()..];
        let more_words = rest
            .split('\n')
            .next()
            .is_some_and(|line| !line.trim().is_empty());
        if more_words && value.chars().all(|c| c.is_ascii_alphabetic()) {
            return KeyOutcome::Keep;
        }
    }
    let shown = if quoted {
        let bare = value.trim_start_matches('\\');
        let quote = &bare[..1];
        let lead = &value[..value.len() - bare.len() + 1];
        format!("{lead}{REDACTED_MARK}{quote}")
    } else {
        REDACTED_MARK.to_string()
    };
    // An array value (`'set-cookie': [ 'sid=...' ]`) is secret to its closing bracket.
    let end = if !quoted && value.starts_with('[') {
        array_value_end(text, value_at.start(), whole.end())
    } else if let Some(e) = scheme_end {
        e.max(whole.end())
    } else {
        whole.end()
    };
    KeyOutcome::Replace {
        text: format!("{shown_key}{sep}{shown}"),
        end,
    }
}

/// `text` with the value of every key that names a secret replaced. The scan is by hand (not `replace_all`) because a match can
/// send it back: a value that is another secret key is scanned again from its start, and so is the value of a match that stays.
fn redact_keys(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let (mut copied, mut at) = (0, 0);
    while let Some(caps) = KEY_VALUE.captures_at(text, at) {
        let Some(whole) = caps.get(0) else { break };
        match key_outcome(text, &caps) {
            // A match that stays leaves its value to the scan: a key inside it (`-DDB_PASS=...` on the line after a short label) is a key.
            KeyOutcome::Keep => at = caps.get(3).map_or(whole.end(), |v| v.start()),
            KeyOutcome::Resume { from } => at = from,
            KeyOutcome::Replace { text: shown, end } => {
                out.push_str(&text[copied..whole.start()]);
                out.push_str(&shown);
                copied = end;
                at = end;
            }
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// The offset after the `]` that closes the `[` at `open`, counting nested brackets (across lines); `None` when it never closes.
fn array_end(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, b) in text.bytes().enumerate().skip(open) {
        match b {
            b'[' => depth += 1,
            b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// The end of the array that opens at `open`: its closing bracket; for an array that never closes, the end of its line, or the start
/// of a secret key inside it (that key takes its own value, which may be on the next line, even when the key sits inside the
/// pattern's own value). Except for that cut, never before `floor`.
fn array_value_end(text: &str, open: usize, floor: usize) -> usize {
    if let Some(end) = array_end(text, open) {
        return end.max(floor);
    }
    let line_end = text[open..].find('\n').map_or(text.len(), |i| open + i);
    let mut from = open + 1;
    while let Some(m) = KEY_VALUE
        .captures_at(text, from)
        .filter(|c| c.get(0).is_some_and(|m| m.start() < line_end))
    {
        let (start, end) = m.get(0).map_or((0, 0), |w| (w.start(), w.end()));
        if names_a_secret(&m[1]) {
            return start;
        }
        from = end.max(start + 1);
    }
    line_end.max(floor)
}

/// `text` with the value of every credential header replaced (`CREDENTIAL_HEADER`).
fn redact_credential_headers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for caps in CREDENTIAL_HEADER.captures_iter(text) {
        let (Some(whole), Some(lead), Some(value)) = (caps.get(0), caps.get(2), caps.get(3)) else {
            continue;
        };
        // Inside a span that an earlier header already took.
        if whole.start() < copied {
            continue;
        }
        let word = value.as_str();
        let end = if value.as_str().starts_with('[') {
            array_value_end(text, value.start(), whole.end()).max(whole.end())
        } else if TOKEN_SCHEMES.contains(&word.to_ascii_lowercase().as_str()) {
            // A scheme word alone at the end of the line: the token is on the next line.
            scheme_token_end(text, value.start() + word.len())
                .map_or(whole.end(), |e| e.max(whole.end()))
        } else {
            whole.end()
        };
        out.push_str(&text[copied..lead.end()]);
        out.push_str(REDACTED_MARK);
        copied = end;
    }
    out.push_str(&text[copied..]);
    out
}

const REDACTED_MARK: &str = "<redacted>";

/// `text` with the secrets that are named by a key, a header, a flag or the shape of their container removed.
fn redact_named(text: &str) -> String {
    let t = redact_url_passwords(text);
    let t = redact_credential_headers(&t);
    let t = redact_keys(&t);
    let t = SPOKEN_KEY.replace_all(&t, "${1}<redacted>").into_owned();
    let t = SECRET_FLAG
        .replace_all(&t, "${1}${2}${3}<redacted>")
        .into_owned();
    let t = USER_PAIR_FLAG
        .replace_all(&t, "${1}${2}<redacted>")
        .into_owned();
    let t = URL_USER
        .replace_all(&t, |c: &Captures| {
            if plain_account_name(&c[2]) {
                c[0].to_string()
            } else {
                format!("{}<redacted>@", &c[1])
            }
        })
        .into_owned();
    let t = NETRC.replace_all(&t, "${1}<redacted>").into_owned();
    let t = XML_SECRET
        .replace_all(&t, "${1}<redacted>${2}")
        .into_owned();
    DOCKER_AUTH
        .replace_all(&t, "${1}${2}<redacted>${2}")
        .into_owned()
}

/// `text` with every whole word of 40 or more standard-base64 characters that holds a `/` or `+` and mixes upper case,
/// lower case and digits replaced (the opaque-token rule of `refusal::redact` stops at a `/`, so a key body that a `/`
/// splits into short pieces escaped it). A path that starts with a lower-case directory stays readable.
fn redact_base64_words(text: &str) -> String {
    BASE64_WORD
        .replace_all(text, |c: &Captures| {
            let word = &c[0];
            let core = word.trim_end_matches('=');
            let standard = core
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '+' || ch == '/');
            let joined = core.contains('/') || core.contains('+');
            let mixed = core.chars().any(|ch| ch.is_ascii_uppercase())
                && core.chars().any(|ch| ch.is_ascii_lowercase())
                && core.chars().any(|ch| ch.is_ascii_digit());
            let path = ABSOLUTE_PATH.is_match(core)
                || c.get(0).is_some_and(|m| {
                    looks_like_a_path(core, starts_with_extension(&text[m.end()..]))
                });
            if standard && joined && mixed && !path {
                REDACTED_MARK.to_string()
            } else {
                word.to_string()
            }
        })
        .into_owned()
}

/// Extra secret shapes beyond `refusal::redact`, in order, with their replacements.
static EXTRA: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    vec![
        // Registry-style keys that `redact` misses because an underscore leads them, quoted or not.
        (
            re(r#"(?i)(_auth(?:token)?|_password)(\s*[:=]\s*["']?)[^\s"',;&|]+"#),
            "${1}${2}<redacted>",
        ),
        // Provider token prefixes.
        (
            re(
                r"\b(?:gh[pousr]_|github_pat_|glpat-|xox[abprs]-|npm_|pypi-|sk-|AIza)[A-Za-z0-9_-]{8,}",
            ),
            "<redacted>",
        ),
        // Hex digests and lowercase-hex keys.
        (re(r"\b[0-9a-fA-F]{32,}\b"), "<redacted>"),
        // Bare account ids, also inside ARNs.
        (re(r"\b\d{12}\b"), "<acct>"),
    ]
});

/// A line that is only base64 text of 40 or more characters: a key body or a blob, never a sentence.
static BASE64_LINE: LazyLock<Regex> = LazyLock::new(|| re(r"^[A-Za-z0-9+/=_-]{40,}$"));

/// A character that shows nothing, so it can hide text from a person (a model still reads it: the tag characters carry ASCII) or
/// split a word that a rule looks for: the Unicode format characters (Cf: soft hyphen, zero-width, bidirectional and the
/// Arabic, Syriac, Kaithi, hieroglyph, shorthand and musical format controls), the other default-ignorable code points
/// (U+034F, the Hangul and Khmer fillers, the Mongolian variation selectors, U+3164, U+FFA0, U+FFF0 to U+FFF8), the variation
/// selectors, the whole tag block U+E0000 to U+E0FFF (tags and the supplementary variation selectors), and U+2800, the blank
/// Braille pattern.
fn invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00ad}'
            | '\u{034f}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061c}'
            | '\u{06dd}'
            | '\u{070f}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08e2}'
            | '\u{115f}'..='\u{1160}'
            | '\u{17b4}'..='\u{17b5}'
            | '\u{180b}'..='\u{180f}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{2800}'
            | '\u{3164}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{feff}'
            | '\u{ffa0}'
            | '\u{fff0}'..='\u{fffb}'
            | '\u{110bd}'
            | '\u{110cd}'
            | '\u{13430}'..='\u{1343f}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0000}'..='\u{e0fff}'
    )
}

/// The scrubbed tail of `raw`, the end of an engine's stderr log. `cut` says earlier output was not read; a `raw` longer
/// than [`STDERR_WINDOW_BYTES`] is read from its end and counts as cut. A cut window starts mid-line: its first line is
/// dropped, and a cut window with no line break shows nothing (`long_line`).
pub fn scrub(raw: &[u8], cut: bool) -> StderrTail {
    let window = &raw[raw.len().saturating_sub(STDERR_WINDOW_BYTES)..];
    let cut = cut || window.len() < raw.len();
    let mut tail = StderrTail {
        bytes: window.len(),
        cut,
        ..StderrTail::default()
    };
    let text = String::from_utf8_lossy(window);
    let text = ESCAPES.replace_all(&text, "");
    let mut clean = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' => clean.push('\n'),
            '\t' => clean.push(' '),
            c if c.is_control() || invisible(c) => {}
            c => clean.push(c),
        }
    }
    let body = if cut {
        match clean.split_once('\n') {
            // The partial first line goes. When it was all there was, the window held a long line, which is not shown.
            Some((first, rest)) if rest.trim().is_empty() => {
                tail.long_line = !first.trim().is_empty();
                return tail;
            }
            Some((_, rest)) => rest.to_string(),
            None => {
                tail.long_line = !clean.trim().is_empty();
                return tail;
            }
        }
    } else {
        clean
    };
    // A line that is only base64 text goes first: `redact` would leave the pieces of a key body that a `/` splits short.
    let lines: Vec<String> = body
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .map(|l| {
            let alnum = l.chars().filter(char::is_ascii_alphanumeric).count();
            if alnum >= 32 && BASE64_LINE.is_match(&l) && !looks_like_a_path(&l, false) {
                "<redacted>".to_string()
            } else {
                l
            }
        })
        .collect();
    let queryless = URL_QUERY
        .replace_all(&lines.join("\n"), "${1}?<redacted>")
        .into_owned();
    // The named shapes and the base64 words go before `redact`, which would cut a token at a `/` and leave its pieces.
    let mut scrubbed = redact(&redact_base64_words(&redact_named(&queryless)));
    for (r, with) in EXTRA.iter() {
        scrubbed = r.replace_all(&scrubbed, *with).into_owned();
    }
    tail.lines = scrubbed
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect();
    tail
}

/// `text` cut to at most `max` characters, keeping its start and its end around `...` (two thirds and one third).
fn clip_middle(text: &str, max: usize) -> String {
    let n = text.chars().count();
    if n <= max {
        return text.to_string();
    }
    if max < 4 {
        return text.chars().take(max).collect();
    }
    let keep = max - 3;
    let tail = keep / 3;
    let head = keep - tail;
    let chars: Vec<char> = text.chars().collect();
    let mut out: String = chars[..head].iter().collect();
    out.push_str("...");
    out.extend(&chars[n - tail..]);
    out
}

/// `text` cut to its last characters behind a `...`, at most `max` in all.
fn clip_end(text: &str, max: usize) -> String {
    let n = text.chars().count();
    if n <= max {
        return text.to_string();
    }
    if max < 4 {
        return text.chars().skip(n - max).collect();
    }
    let mut out = String::from("...");
    out.extend(text.chars().skip(n - (max - 3)));
    out
}

fn not_shown() -> String {
    format!("stderr not shown (one line over the {STDERR_WINDOW_BYTES}-byte window)")
}

/// The body for the error text the orchestrator reads: the exit and the headline of the stderr tail, at most
/// [`ERROR_BODY_CHARS`] characters.
pub fn error_body(exit: &str, tail: &StderrTail) -> String {
    let exit = clip_middle(exit, EXIT_CHARS);
    let body = if tail.long_line {
        format!("{exit}; {}", not_shown())
    } else if let Some(line) = tail.headline() {
        let prefix = format!("{exit}; stderr: ");
        let room = ERROR_BODY_CHARS.saturating_sub(prefix.chars().count());
        format!("{prefix}{}", clip_middle(line, room))
    } else {
        format!("{exit}; stderr empty")
    };
    clip_middle(&body, ERROR_BODY_CHARS)
}

/// The body when agentd could not read the log at all: the exit and why, at most [`ERROR_BODY_CHARS`] characters. It
/// stands in for [`error_body`] and [`log_body`] both.
pub fn unreadable_body(exit: &str, why: &str) -> String {
    let body = format!(
        "{}; stderr unreadable ({why})",
        clip_middle(exit, EXIT_CHARS)
    );
    clip_middle(&body, ERROR_BODY_CHARS)
}

/// The body for the runtime log line: the exit and the scrubbed tail on one line, at most [`LOG_TAIL_CHARS`] characters
/// of it. A cut window shows its end, a whole log its start and its end.
pub fn log_body(exit: &str, tail: &StderrTail) -> String {
    let exit = clip_middle(exit, EXIT_CHARS);
    if tail.long_line {
        return format!("{exit}; {}", not_shown());
    }
    if tail.is_empty() {
        return format!("{exit}; stderr empty");
    }
    let joined = tail.joined();
    if tail.cut {
        format!(
            "{exit}; stderr tail (last {} bytes, earlier output cut): {}",
            tail.bytes,
            clip_end(&joined, LOG_TAIL_CHARS)
        )
    } else {
        format!(
            "{exit}; stderr tail ({} bytes): {}",
            tail.bytes,
            clip_middle(&joined, LOG_TAIL_CHARS)
        )
    }
}
