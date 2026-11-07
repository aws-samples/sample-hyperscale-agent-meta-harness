//! The request target: what a client may ask for and how it maps to an upstream path.
//!
//! The target must be origin-form `/<prefix>/<rest>`. The path is percent-decoded once for checking only: a segment equal to `.`
//! or `..`, an empty segment (a run of slashes, which includes `%2f` runs), a NUL, a backslash, bad UTF-8 or a bad escape is a
//! 400. What goes upstream is the raw (still encoded) text, so a scoped npm package `@scope%2fname` keeps its `%2f`.

/// Longest path, in bytes, the proxy accepts.
pub(crate) const MAX_PATH: usize = 2048;
/// Longest query, in bytes, the proxy accepts.
pub(crate) const MAX_QUERY: usize = 1024;

/// Why a target is refused (always a 400), with the text the client reads.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Refusal(pub &'static str);

/// A checked target, borrowing from the request.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Routed<'a> {
    /// The first path segment, exactly as sent.
    pub prefix: &'a str,
    /// The raw path after the prefix: empty, or starting with `/`.
    pub rest: &'a str,
    /// The raw query when the target has a `?` (an empty query is `Some("")`).
    pub query: Option<&'a str>,
}

/// Percent-decodes once; `None` for a malformed escape.
fn decode_once(raw: &str) -> Option<Vec<u8>> {
    let b = raw.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            if !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(out)
}

/// A byte a request path may carry once percent-decoded once: package names, versions and file names use letters, digits and
/// `- . _ ~ ! $ & ' ( ) * + , = : @`, with `/` between segments. Everything else is refused, so a path whose meaning depends on how
/// an upstream decodes twice, strips `;params`, trims blanks or normalises Unicode never reaches it.
fn path_byte_ok(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'-' | b'.'
                | b'_'
                | b'~'
                | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b'='
                | b':'
                | b'@'
                | b'/'
        )
}

/// The path checks that do not depend on a prefix: percent-decoded once, only the bytes of `path_byte_ok`, no segment made of
/// dots only, no empty segment except a trailing one. `path` starts with `/`.
pub(crate) fn check_segments(path: &str) -> Result<(), Refusal> {
    let decoded = decode_once(path).ok_or(Refusal("bad percent-encoding"))?;
    if !decoded.iter().all(|&b| path_byte_ok(b)) {
        return Err(Refusal("bad character in path"));
    }
    let body = decoded.strip_prefix(b"/").unwrap_or(&decoded);
    let segments: Vec<&[u8]> = body.split(|&b| b == b'/').collect();
    let last = segments.len() - 1;
    for (i, seg) in segments.iter().enumerate() {
        if !seg.is_empty() && seg.iter().all(|&b| b == b'.') {
            return Err(Refusal("dot segment in path"));
        }
        if seg.is_empty() && i != last {
            return Err(Refusal("empty path segment"));
        }
    }
    Ok(())
}

/// Checks the path (always starting with `/`) and the query, and splits the prefix off. The caller decides whether the prefix
/// is configured.
pub(crate) fn route<'a>(path: &'a str, query: Option<&'a str>) -> Result<Routed<'a>, Refusal> {
    if path.len() > MAX_PATH {
        return Err(Refusal("path too long"));
    }
    if query.is_some_and(|q| q.len() > MAX_QUERY) {
        return Err(Refusal("query too long"));
    }
    let Some(after) = path.strip_prefix('/') else {
        return Err(Refusal("bad request target"));
    };
    check_segments(path)?;
    let (prefix, rest) = match after.find('/') {
        Some(i) => (&after[..i], &after[i..]),
        None => (after, ""),
    };
    Ok(Routed {
        prefix,
        rest,
        query,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn ok(path: &str) -> Routed<'_> {
        route(path, None).unwrap_or_else(|e| panic!("{path}: {e:?}"))
    }

    fn bad(path: &str) {
        assert!(
            matches!(route(path, None), Err(Refusal(_))),
            "{path} should be a 400"
        );
    }

    #[test]
    fn splits_prefix_and_rest_without_decoding() {
        assert_eq!(
            ok("/npm/@babel%2fcore"),
            Routed {
                prefix: "npm",
                rest: "/@babel%2fcore",
                query: None
            }
        );
        assert_eq!(ok("/pypi").rest, "");
        assert_eq!(ok("/pypi/").rest, "/");
        assert_eq!(ok("/").prefix, "");
        assert_eq!(
            ok("/%70ypi/x").prefix,
            "%70ypi",
            "the prefix is compared as sent"
        );
    }

    #[test]
    fn a_query_is_carried_and_an_empty_one_is_kept() {
        assert_eq!(route("/pypi/x", Some("")).unwrap().query, Some(""));
        assert_eq!(route("/pypi/x", Some("a=1")).unwrap().query, Some("a=1"));
    }

    #[test]
    fn refuses_dot_segments_empty_segments_nul_backslash_and_bad_escapes() {
        for p in [
            "/pypi/..",
            "/pypi/.",
            "/pypi/a/../b",
            "/pypi/%2e%2E",
            "/pypi//a",
            "/pypi/a//b",
            "/pypi/a%2f%2fb",
            "/pypi/%00",
            "/pypi/a%5cb",
            "/pypi/%zz",
            "/pypi/%",
            "/pypi/%4",
            "/pypi/%ff",
            "/pypi/a%2f..%2fb",
            "/pypi/...",
            "/pypi/a/..../b",
            "/pypi/..;/x",
            "/pypi/a;b",
            "/pypi/%252e%252e/x",
            "/pypi/a%25b",
            "/pypi/%20../x",
            "/pypi/a%20b",
            "/pypi/a%0d%0ab",
            "/pypi/a%3fb",
            "/pypi/a%23b",
            "/pypi/caf%C3%A9",
            "/pypi/%EF%BC%8E%EF%BC%8E/x",
        ] {
            bad(p);
        }
    }

    #[test]
    fn allows_a_trailing_slash_dots_inside_names_and_percent_escapes_of_ordinary_bytes() {
        for p in [
            "/pypi/a/",
            "/pypi/a.b/c..d/.hidden",
            "/pypi/%41%42",
            "/maven/org/x/1.0/x-1.0.pom",
            "/go/github.com/!a!b/c/@v/list",
            "/npm/@babel%2fcore",
            "/pypi/simple/torch-2.1.0%2Bcpu-cp311-linux_x86_64.whl",
            "/maven/org/x/y/1.0-20200101.120000-1/y-1.0-20200101.120000-1.jar",
            "/pypi/a%7Eb/c'd(e)*f,g=h:i@j$k&l!",
            // Shapes the four real clients send; the allow list costs them nothing.
            "/pypi/simple/typing-extensions/",
            "/pypi/packages/ab/cd/ef0123/numpy-1.26.4-cp311-cp311-manylinux_2_17_x86_64.manylinux2014_x86_64.whl",
            "/npm/%40scope%2Fname",
            "/npm/@scope%2fname/-/name-1.0.0.tgz",
            "/maven/org/apache/maven/maven-metadata.xml",
            "/maven/org/x/y/1.0/y-1.0-sources.jar.sha256",
            "/go/github.com/!burnt!sushi/toml/@v/v1.2.3+incompatible.info",
            "/go/golang.org/x/text/@v/v0.0.0-20210101000000-abcdef123456.mod",
            "/go/example.com/m/@latest",
        ] {
            ok(p);
        }
    }

    #[test]
    fn enforces_the_length_limits_at_the_boundary() {
        let p2048 = format!("/pypi/{}", "a".repeat(2048 - 6));
        assert!(route(&p2048, None).is_ok());
        let p2049 = format!("/pypi/{}", "a".repeat(2049 - 6));
        assert!(matches!(route(&p2049, None), Err(Refusal(_))));
        assert!(route("/pypi/x", Some(&"q".repeat(1024))).is_ok());
        assert!(route("/pypi/x", Some(&"q".repeat(1025))).is_err());
    }
}
