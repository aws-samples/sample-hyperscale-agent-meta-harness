//! Link rewriting. CodeArtifact answers an index page with absolute links to itself; a client that
//! followed them would leave the proxy, reach the real host and fail without a credential. A textual answer therefore has every
//! absolute URL that starts with an upstream base rewritten to `http://127.0.0.1:<port>/<prefix>`, in plain form and in the
//! JSON-escaped form (`https:\/\/host\/...`) npm documents use.

/// One answer in flight holds at most this twice (the bytes and their rewrite) and the proxy holds at most four at once
/// (`MAX_CONCURRENT_REWRITES`), so the worst case is 4 x 128 MiB = 512 MiB, 6.25 percent of the 8 GB of an AgentCore Runtime session
/// (quota "Maximum hardware allocation per session": 2vCPU/8GB). `tests/rewrite_memory.rs` measures it.
pub(crate) const MAX_REWRITE_BYTES: usize = 64 * 1024 * 1024;

/// Whether an answer of this `Content-Type` is rewritten.
pub(crate) fn is_textual(content_type: &str) -> bool {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    ct.starts_with("text/")
        || ct == "application/json"
        || ct == "application/xml"
        || ct.starts_with("application/vnd.pypi")
        || ct.starts_with("application/vnd.npm")
}

/// One `from` to `to` replacement, applied only where the match ends at a URL boundary.
#[derive(Debug)]
struct Pair {
    from: Vec<u8>,
    to: Vec<u8>,
}

#[derive(Debug, Default)]
pub(crate) struct Rewriter {
    pairs: Vec<Pair>,
}

/// A byte that continues a path segment: a match followed by one is a different, longer name (`.../packages2/`).
fn continues_name(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'%' | b'+')
}

impl Rewriter {
    /// `(upstream base, local URL)` pairs, for example `("https://h/npm/packages", "http://127.0.0.1:3128/npm")`.
    pub fn new(bases: &[(String, String)]) -> Self {
        let mut pairs = Vec::new();
        for (from, to) in bases {
            pairs.push(Pair {
                from: from.clone().into_bytes(),
                to: to.clone().into_bytes(),
            });
            pairs.push(Pair {
                from: from.replace('/', "\\/").into_bytes(),
                to: to.replace('/', "\\/").into_bytes(),
            });
        }
        // The longest base first, so a base that extends another wins.
        pairs.sort_by_key(|p| std::cmp::Reverse(p.from.len()));
        Self { pairs }
    }

    /// The text with every matching URL rewritten.
    pub fn apply(&self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len());
        let mut i = 0;
        'scan: while i < input.len() {
            // Every pattern starts with `h` (`https:`), so skip to the next one cheaply.
            match input[i..].iter().position(|&b| b == b'h') {
                None => {
                    out.extend_from_slice(&input[i..]);
                    break;
                }
                Some(skip) => {
                    out.extend_from_slice(&input[i..i + skip]);
                    i += skip;
                }
            }
            for p in &self.pairs {
                if input[i..].starts_with(&p.from) {
                    let end = i + p.from.len();
                    if input.get(end).is_none_or(|&b| !continues_name(b)) {
                        out.extend_from_slice(&p.to);
                        i = end;
                        continue 'scan;
                    }
                }
            }
            out.push(input[i]);
            i += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rw() -> Rewriter {
        Rewriter::new(&[
            (
                "https://h.example/npm/packages".into(),
                "http://127.0.0.1:9/npm".into(),
            ),
            (
                "https://h.example/pypi/packages".into(),
                "http://127.0.0.1:9/pypi".into(),
            ),
        ])
    }

    fn s(b: Vec<u8>) -> String {
        String::from_utf8(b).unwrap_or_default()
    }

    #[test]
    fn rewrites_plain_and_escaped_urls_at_boundaries_only() {
        let text = r#"a https://h.example/npm/packages/x b "https:\/\/h.example\/pypi\/packages\/y" c https://h.example/npm/packages2/z d https://h.example/npm/packages."#;
        assert_eq!(
            s(rw().apply(text.as_bytes())),
            r#"a http://127.0.0.1:9/npm/x b "http:\/\/127.0.0.1:9\/pypi\/y" c https://h.example/npm/packages2/z d https://h.example/npm/packages."#
        );
    }

    #[test]
    fn a_base_at_the_very_end_and_before_quotes_is_rewritten() {
        assert_eq!(
            s(rw().apply(b"https://h.example/npm/packages")),
            "http://127.0.0.1:9/npm"
        );
        assert_eq!(
            s(rw().apply(b"\"https://h.example/npm/packages\"")),
            "\"http://127.0.0.1:9/npm\""
        );
        assert_eq!(
            s(rw().apply(b"(https://h.example/npm/packages)")),
            "(http://127.0.0.1:9/npm)"
        );
    }

    #[test]
    fn leaves_other_text_alone_including_partial_prefixes_and_multibyte() {
        let t = "héllo http://h.example/npm/packages https://h.example/npm https://h.exampl hhhttps:// ünï";
        assert_eq!(s(rw().apply(t.as_bytes())), t);
        assert_eq!(rw().apply(b""), b"");
        let binary: Vec<u8> = (0..=255u8).collect();
        assert_eq!(rw().apply(&binary), binary);
    }

    #[test]
    fn the_longest_base_wins() {
        let r = Rewriter::new(&[
            ("https://h.example/a".into(), "http://l/a".into()),
            ("https://h.example/a/b".into(), "http://l/ab".into()),
        ]);
        assert_eq!(
            s(r.apply(b"https://h.example/a/b/x https://h.example/a/c")),
            "http://l/ab/x http://l/a/c"
        );
    }

    #[test]
    fn textual_types() {
        for t in [
            "text/html",
            "TEXT/PLAIN; charset=x",
            "application/json",
            "application/xml",
            "application/vnd.pypi.simple.v1+json",
            "application/vnd.npm.install-v1+json",
        ] {
            assert!(is_textual(t), "{t}");
        }
        for t in [
            "",
            "application/octet-stream",
            "application/zip",
            "application/java-archive",
            "image/png",
            "application/gzip",
        ] {
            assert!(!is_textual(t), "{t}");
        }
    }
}
