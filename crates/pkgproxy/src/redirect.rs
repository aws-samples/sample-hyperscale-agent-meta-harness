//! Redirects. The proxy follows `301 302 303 307 308` itself, at most `max_redirects` times, and only
//! to `https` on the default port at a host that ends `.amazonaws.com` (or back to the upstream it started from). It sends the
//! credential only to the upstream's own host. Any other target is refused, and the client is told only "redirect refused".

/// Where the next request goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Loc {
    pub host: String,
    pub port: u16,
    /// Path and query, raw, starting with `/`.
    pub path_and_query: String,
}

impl Loc {
    /// `host` or `host:port` as a URL authority.
    pub fn authority(&self) -> String {
        if self.port == 443 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    pub fn url(&self) -> String {
        format!("https://{}{}", self.authority(), self.path_and_query)
    }

    fn same_origin(&self, other: &Loc) -> bool {
        self.host == other.host && self.port == other.port
    }
}

/// The only host suffix a redirect may lead to.
const ALLOWED_SUFFIX: &str = ".amazonaws.com";
/// Longest path and query of a redirect target.
const MAX_TARGET: usize = 8192;

pub(crate) fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn host_ok(host: &str) -> bool {
    host.len() <= 253
        && host.ends_with(ALLOWED_SUFFIX)
        && host.len() > ALLOWED_SUFFIX.len()
        && host.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && l.bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        })
}

/// Drops a fragment and makes the rest a path-and-query that starts with `/`.
fn path_and_query(rest: &str) -> String {
    let rest = rest.split('#').next().unwrap_or("");
    if rest.is_empty() {
        "/".into()
    } else if rest.starts_with('?') {
        format!("/{rest}")
    } else {
        rest.to_string()
    }
}

/// Joins a relative reference to the directory of `cur` and removes dot segments.
fn join_relative(cur: &str, reference: &str) -> Option<String> {
    let (cur_path, _) = cur.split_once('?').unwrap_or((cur, ""));
    let dir = &cur_path[..=cur_path.rfind('/')?];
    let (ref_path, ref_query) = match reference.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (reference, None),
    };
    let mut out: Vec<&str> = dir
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let mut trailing = false;
    for seg in ref_path.split('/') {
        trailing = false;
        match seg {
            "." => trailing = true,
            ".." => {
                out.pop()?;
                trailing = true;
            }
            "" => trailing = true,
            s => out.push(s),
        }
    }
    let mut p = format!("/{}", out.join("/"));
    if trailing && !p.ends_with('/') {
        p.push('/');
    }
    if let Some(q) = ref_query {
        p.push('?');
        p.push_str(q);
    }
    Some(p)
}

/// The next location for `location`, seen from `cur`, when it is allowed. `origin` is the upstream the request started at, and its
/// `path_and_query` is the base path a redirect back to the same host must stay under. `None` means the redirect is refused.
pub(crate) fn resolve(origin: &Loc, cur: &Loc, location: &str) -> Option<Loc> {
    if location.is_empty() || location.len() > MAX_TARGET {
        return None;
    }
    if location
        .bytes()
        .any(|c| c <= b' ' || c == 0x7f || c == b'\\')
    {
        return None;
    }
    let next = if let Some(rest) = location.strip_prefix("//") {
        absolute(rest)?
    } else if location.len() >= 8 && location[..8].eq_ignore_ascii_case("https://") {
        absolute(&location[8..])?
    } else if location.starts_with('/') {
        Loc {
            host: cur.host.clone(),
            port: cur.port,
            path_and_query: path_and_query(location),
        }
    } else if has_scheme(location) {
        return None;
    } else {
        Loc {
            host: cur.host.clone(),
            port: cur.port,
            path_and_query: join_relative(
                &cur.path_and_query,
                location.split('#').next().unwrap_or(""),
            )?,
        }
    };
    if next.path_and_query.len() > MAX_TARGET {
        return None;
    }
    // Back to the upstream the request started at is fine only inside the repository the configuration names: the credential is
    // the token of the whole domain. Anywhere else must pass the host rule (and gets no credential).
    if next.same_origin(origin) {
        confined(&next.path_and_query, &origin.path_and_query).then_some(next)
    } else if next.port == 443 && host_ok(&next.host) {
        Some(next)
    } else {
        None
    }
}

/// Whether `path_and_query` is `base` or under `base/`, with no dot segment, empty segment or other path trick.
fn confined(path_and_query: &str, base: &str) -> bool {
    let path = path_and_query.split('?').next().unwrap_or("");
    crate::target::check_segments(path).is_ok()
        && (base.is_empty()
            || path == base
            || path
                .strip_prefix(base)
                .is_some_and(|rest| rest.starts_with('/')))
}

/// `scheme:` before any `/`, `?` or `#`.
fn has_scheme(s: &str) -> bool {
    match s.find([':', '/', '?', '#']) {
        Some(i) => s.as_bytes()[i] == b':',
        None => false,
    }
}

/// Parses `authority/path?query` (what follows `https://`).
fn absolute(rest: &str) -> Option<Loc> {
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    if authority.is_empty() || authority.contains('@') || authority.contains('[') {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => {
            if p.is_empty() || !p.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            (h, p.parse::<u16>().ok()?)
        }
        None => (authority, 443),
    };
    if host.is_empty() {
        return None;
    }
    Some(Loc {
        host: host.to_ascii_lowercase(),
        port,
        path_and_query: path_and_query(tail),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn loc(host: &str, path: &str) -> Loc {
        Loc {
            host: host.into(),
            port: 443,
            path_and_query: path.into(),
        }
    }

    fn go(location: &str) -> Option<String> {
        // The upstream's base path is `/pypi/packages`; the request being redirected is `/pypi/packages/x`.
        let origin = loc(
            "dom-1.d.codeartifact.us-east-1.amazonaws.com",
            "/pypi/packages",
        );
        let cur = loc(
            "dom-1.d.codeartifact.us-east-1.amazonaws.com",
            "/pypi/packages/x",
        );
        resolve(&origin, &cur, location).map(|l| l.url())
    }

    #[test]
    fn follows_amazonaws_hosts_with_their_path_and_query() {
        assert_eq!(
            go("https://b.s3.us-east-1.amazonaws.com/k?X-Amz-Signature=a&x=1#frag").unwrap(),
            "https://b.s3.us-east-1.amazonaws.com/k?X-Amz-Signature=a&x=1"
        );
        assert_eq!(
            go("https://B.S3.US-EAST-1.AMAZONAWS.COM:443").unwrap(),
            "https://b.s3.us-east-1.amazonaws.com/"
        );
        assert_eq!(
            go("//b.s3.us-east-1.amazonaws.com/x").unwrap(),
            "https://b.s3.us-east-1.amazonaws.com/x"
        );
        assert_eq!(
            go("HTTPS://b.s3.us-east-1.amazonaws.com/x").unwrap(),
            "https://b.s3.us-east-1.amazonaws.com/x"
        );
        assert_eq!(
            go("https://b.s3.us-east-1.amazonaws.com?x=1").unwrap(),
            "https://b.s3.us-east-1.amazonaws.com/?x=1"
        );
    }

    #[test]
    fn resolves_relative_references_against_the_current_directory() {
        assert_eq!(
            go("y").unwrap(),
            "https://dom-1.d.codeartifact.us-east-1.amazonaws.com/pypi/packages/y"
        );
        assert_eq!(
            go("./").unwrap(),
            "https://dom-1.d.codeartifact.us-east-1.amazonaws.com/pypi/packages/"
        );
        assert_eq!(
            go("/pypi/packages/deeper/x?q=1#f").unwrap(),
            "https://dom-1.d.codeartifact.us-east-1.amazonaws.com/pypi/packages/deeper/x?q=1"
        );
        assert!(
            go("../../../../x").is_none(),
            "climbing above the root is refused"
        );
    }

    #[test]
    fn a_redirect_to_the_upstream_host_stays_under_the_base_path() {
        for outside in [
            "/other",
            "../z?q=1",
            "/npm/packages/x",
            "/pypi/other-repo/x",
            "/pypi/packages-old/x",
            "/pypi/packages/../other",
            "/pypi/packages/%2e%2e/other",
            "/pypi/packages//x",
            "https://dom-1.d.codeartifact.us-east-1.amazonaws.com/",
            "https://dom-1.d.codeartifact.us-east-1.amazonaws.com:443/pypi",
        ] {
            assert!(go(outside).is_none(), "{outside:?} leaves the repository");
        }
        assert!(go("/pypi/packages").is_some(), "the base itself is inside");
    }

    #[test]
    fn refuses_everything_else() {
        for l in [
            "",
            "http://b.s3.us-east-1.amazonaws.com/x",
            "https://192.0.2.1/x",
            "https://127.0.0.1/x",
            "https://[::1]/x",
            "https://localhost/x",
            "https://amazonaws.com/x",
            "https://.amazonaws.com/x",
            "https://decoyamazonaws.com/x",
            "https://b.amazonaws.com.decoy.example/x",
            "https://user@b.s3.us-east-1.amazonaws.com/x",
            "https://b.s3.us-east-1.amazonaws.com@decoy.example/x",
            "https://b.s3.us-east-1.amazonaws.com:8443/x",
            "https://b.s3.us-east-1.amazonaws.com:/x",
            "https://b.s3.us-east-1.amazonaws.com:99999/x",
            "https://b..s3.amazonaws.com/x",
            "//decoy.example/x",
            "ftp://b.amazonaws.com/x",
            "file:///tmp/x",
            "data:text/plain,x",
            "mailto:a@b.amazonaws.com",
            "https://b.amazonaws.com/x y",
            "https://b.amazonaws.com\\@decoy/x",
            "https://b.amazonaws.com/\u{7f}",
        ] {
            assert!(go(l).is_none(), "{l:?} should be refused");
        }
    }

    #[test]
    fn a_location_over_8192_bytes_is_refused_even_when_its_path_alone_is_shorter() {
        // The host part counts: this one is 8,217 bytes long and its path and query only 8,181.
        let host = "https://b.s3.us-east-1.amazonaws.com";
        let long = format!("{host}/{}", "a".repeat(8180));
        assert_eq!(long.len(), 8217);
        assert!(go(&long).is_none());
        // Exactly 8,192 bytes is the largest accepted.
        let edge = format!("{host}/{}", "a".repeat(8192 - host.len() - 1));
        assert_eq!(edge.len(), 8192);
        assert!(go(&edge).is_some());
        let over = format!("{host}/{}", "a".repeat(8192 - host.len()));
        assert_eq!(over.len(), 8193);
        assert!(go(&over).is_none());
    }

    #[test]
    fn the_original_origin_is_allowed_even_off_the_suffix_but_only_under_its_base() {
        let origin = loc("registry.internal.example", "/p");
        let got = resolve(&origin, &origin, "https://registry.internal.example/p/q").unwrap();
        assert_eq!(got.url(), "https://registry.internal.example/p/q");
        assert!(resolve(&origin, &origin, "https://registry.internal.example/q").is_none());
        assert!(resolve(&origin, &origin, "https://other.internal.example/p/q").is_none());
        // A base with no path (the Go bucket) allows any path on its own host.
        let bucket = loc("b.s3.us-east-1.amazonaws.com", "");
        assert!(
            resolve(
                &bucket,
                &bucket,
                "https://b.s3.us-east-1.amazonaws.com/any/where"
            )
            .is_some()
        );
        assert!(
            resolve(
                &bucket,
                &bucket,
                "https://b.s3.us-east-1.amazonaws.com/a/../b"
            )
            .is_none()
        );
    }
}
