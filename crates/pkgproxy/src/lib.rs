//! The loopback package proxy of a direct run.
//!
//! Cline installs what its tests need (pip, npm, Maven, Go, cargo) while the CodeArtifact token stays out of its reach:
//! agentd starts this proxy on `127.0.0.1`, the engine's package tools point at it, and the proxy adds the
//! credential to the upstream request. The token lives only in agentd's memory.
//!
//! The behaviour is the table of section 7.4: bind `127.0.0.1` only, `GET` and `HEAD` only, origin-form targets under
//! five fixed prefixes, the client's own credentials dropped, streamed bodies, link rewriting, redirects only to
//! `.amazonaws.com` without the credential, verified TLS, expiry, a connection limit.
//!
//! No dependency on `agentd`: the proxy is a library that agentd uses.

mod body;
mod cargo;
mod net;
mod redirect;
mod rewrite;
mod server;
mod stats;
mod target;

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// What the test probe of a [`Secret`] receives: where the bytes were stored and what they held just before release.
#[cfg(test)]
type Probe = Arc<std::sync::Mutex<Option<(usize, Vec<u8>)>>>;

/// A credential. `Debug` and `Display` print `<redacted>`, so it cannot reach a log through a containing type, and the memory
/// is wiped when it is dropped.
pub struct Secret {
    value: String,
    /// Test hook: told where the bytes were stored and what they held, just before the buffer is released.
    #[cfg(test)]
    probe: Option<Probe>,
}

impl Secret {
    pub fn new(secret: String) -> Self {
        Self {
            value: secret,
            #[cfg(test)]
            probe: None,
        }
    }

    /// The value, for the one place that builds the upstream `Authorization` header.
    pub(crate) fn expose(&self) -> &str {
        self.value.as_str()
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // The bytes leave the `String` without a copy, are overwritten where they were stored, and then the whole allocation
        // (its spare capacity too) is wiped before it is released.
        let mut bytes = std::mem::take(&mut self.value).into_bytes();
        bytes.as_mut_slice().zeroize();
        #[cfg(test)]
        if let Some(probe) = &self.probe
            && let Ok(mut seen) = probe.lock()
        {
            *seen = Some((bytes.as_ptr() as usize, bytes.clone()));
        }
        bytes.zeroize();
    }
}

// `Drop` above wipes the memory; this marks `Secret` as doing so.
impl ZeroizeOnDrop for Secret {}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// The first path segment of a proxied URL: `/pypi/...`, `/npm/...`, `/maven/...`, `/go/...` or `/cargo/...`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Prefix {
    Pypi,
    Npm,
    Maven,
    Go,
    /// A cargo sparse registry: the index and the crate downloads, read only, `config.json` made by the proxy.
    Cargo,
}

/// The credential the proxy adds to an upstream request.
#[derive(Debug)]
pub enum Auth {
    None,
    Basic {
        user: String,
        secret: Secret,
    },
    Bearer(Secret),
    /// The secret is the whole `Authorization` value, as it is: CodeArtifact's cargo endpoint answers 401 to `Bearer <token>`.
    Raw(Secret),
}

/// One upstream registry behind a prefix.
#[derive(Debug)]
pub struct Upstream {
    pub prefix: Prefix,
    /// `https://host/path`, no trailing slash.
    pub base: String,
    pub auth: Auth,
}

/// One line per request goes here (`pkgproxy <prefix> <status> <bytes> <ms>`); agentd adapts its log sink. No
/// header, query or token is ever passed to it.
pub trait Log: Send + Sync {
    fn line(&self, text: &str);
}

pub struct Config {
    /// `127.0.0.1:3128` by default; port 0 is allowed (tests, and when 3128 is busy).
    pub listen: SocketAddr,
    pub upstreams: Vec<Upstream>,
    /// After this Unix time every credentialed prefix answers 401.
    pub expires_at_unix: Option<u64>,
    /// 64.
    pub max_connections: usize,
    /// 900 s.
    pub request_timeout: Duration,
    /// 60 s between two bytes.
    pub idle_timeout: Duration,
    /// 5.
    pub max_redirects: u8,
    pub log: Arc<dyn Log>,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("listen", &self.listen)
            .field("upstreams", &self.upstreams)
            .field("expires_at_unix", &self.expires_at_unix)
            .field("max_connections", &self.max_connections)
            .field("request_timeout", &self.request_timeout)
            .field("idle_timeout", &self.idle_timeout)
            .field("max_redirects", &self.max_redirects)
            .finish_non_exhaustive()
    }
}

/// What the proxy counted; agentd copies it into `summary.json` (`DirectPackages`).
///
/// `requests` and `statuses` count every answer the proxy gave. `by_prefix` counts the requests that reached a configured
/// prefix (`pypi`, `npm`, `maven`, `go`, `cargo`). `refused` counts the answers the proxy decided itself: a bad request, an unknown
/// prefix, a wrong method, an expired token, a refused redirect, a full proxy. `bytes` counts body bytes sent to clients.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatsSnapshot {
    pub requests: u64,
    pub bytes: u64,
    pub by_prefix: BTreeMap<String, u64>,
    pub statuses: BTreeMap<u16, u64>,
    pub refused: u64,
}

/// A running proxy. Dropping it stops the listener; [`Handle::stop`] also waits until every connection is closed.
pub struct Handle {
    run: server::Running,
}

impl fmt::Debug for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Handle")
            .field("addr", &self.run.addr)
            .finish_non_exhaustive()
    }
}

/// Binds `cfg.listen` (which must be `127.0.0.1`) and starts serving. If port 3128 is busy the caller passes port 0 and reads
/// the real port from [`Handle::addr`]. An invalid `cfg` is an `InvalidInput` error whose text holds no secret.
pub async fn start(cfg: Config) -> std::io::Result<Handle> {
    Ok(Handle {
        run: server::launch(cfg, None).await?,
    })
}

impl Handle {
    pub fn addr(&self) -> SocketAddr {
        self.run.addr
    }

    /// The environment variables the engine's package tools read.
    pub fn env(&self) -> BTreeMap<String, String> {
        let p = self.run.addr.port();
        let pypi = format!("http://127.0.0.1:{p}/pypi/simple/");
        [
            ("PIP_INDEX_URL", pypi.clone()),
            ("PIP_TRUSTED_HOST", "127.0.0.1".to_string()),
            ("PIP_DISABLE_PIP_VERSION_CHECK", "1".to_string()),
            ("UV_DEFAULT_INDEX", pypi.clone()),
            ("UV_INDEX_URL", pypi),
            ("UV_INSECURE_HOST", format!("127.0.0.1:{p}")),
            ("UV_PYTHON_DOWNLOADS", "never".to_string()),
            ("npm_config_registry", format!("http://127.0.0.1:{p}/npm/")),
            // The npm CLI is given tarball links on registry.npmjs.org (the proxy decides by its User-Agent) and replaces that host
            // with the registry above, so the `resolved` URLs it writes into package-lock.json name the public host and not this
            // session's port. The default is already `npmjs`; naming it keeps an `.npmrc` or user setting from changing it.
            ("npm_config_replace_registry_host", "npmjs".to_string()),
            // pnpm 12 ignores `npm_config_registry` (measured with 12.6.0: it asked registry.npmjs.org, which the session cannot
            // reach) and reads its own variable. It follows the loopback tarball links literally and writes no URL into its lockfile.
            ("pnpm_config_registry", format!("http://127.0.0.1:{p}/npm/")),
            ("npm_config_update_notifier", "false".to_string()),
            ("npm_config_audit", "false".to_string()),
            ("npm_config_fund", "false".to_string()),
            ("GOPROXY", format!("http://127.0.0.1:{p}/go")),
            ("GOSUMDB", "off".to_string()),
            ("GOFLAGS", "-mod=mod".to_string()),
            ("GOTOOLCHAIN", "local".to_string()),
            // The Maven wrapper's script fetches its distribution from `$MVNW_REPOURL/org/apache/maven/...` (a settings.xml
            // mirror does not reach that curl): the proxy's Maven prefix serves it from CodeArtifact.
            ("MVNW_REPOURL", format!("http://127.0.0.1:{p}/maven")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }

    /// The Maven `settings.xml` mirror of `*` to `http://127.0.0.1:<port>/maven/`; it holds no secret.
    pub fn maven_settings_xml(&self) -> String {
        format!(
            "<settings><mirrors><mirror><id>loop</id><mirrorOf>*</mirrorOf><url>http://127.0.0.1:{}/maven/</url></mirror></mirrors></settings>",
            self.run.addr.port()
        )
    }

    /// The `config.toml` for the engine's `CARGO_HOME`: crates.io is replaced by this proxy's registry. A sparse registry URL
    /// ends in a slash. Cargo is given no token and no credential provider, because `config.json` says `auth-required: false`; the
    /// proxy retries a failed request itself, and cargo retries three times on top. It holds no secret.
    pub fn cargo_config_toml(&self) -> String {
        format!(
            "[source.crates-io]\nreplace-with = \"hamh-proxy\"\n\n[registries.hamh-proxy]\nindex = \"sparse+http://127.0.0.1:{}/cargo/\"\n\n[net]\nretry = 3\n\n[term]\ncolor = \"never\"\n",
            self.run.addr.port()
        )
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        self.run.shared.stats.snapshot()
    }

    /// Stops accepting, closes every connection and returns once they are gone.
    pub async fn stop(self) {
        self.run.stop().await;
    }
}

/// CodeArtifact's pypi, npm, maven and cargo on `host` (`<domain>-<owner>.d.codeartifact.<region>.amazonaws.com`),
/// repository `repo`, all with the same token.
pub fn codeartifact_upstreams(host: &str, repo: &str, token: Secret) -> Vec<Upstream> {
    let base = |kind: &str| format!("https://{host}/{kind}/{repo}");
    let copy = || Secret::new(token.expose().to_string());
    vec![
        Upstream {
            prefix: Prefix::Pypi,
            base: base("pypi"),
            auth: Auth::Basic {
                user: "aws".to_string(),
                secret: copy(),
            },
        },
        Upstream {
            prefix: Prefix::Npm,
            base: base("npm"),
            auth: Auth::Bearer(copy()),
        },
        Upstream {
            prefix: Prefix::Maven,
            base: base("maven"),
            auth: Auth::Basic {
                user: "aws".to_string(),
                secret: copy(),
            },
        },
        Upstream {
            prefix: Prefix::Cargo,
            base: base("cargo"),
            auth: Auth::Raw(copy()),
        },
    ]
}

/// The anonymous Go module store: the stage's GOMOD bucket URL.
pub fn go_upstream(url: &str) -> Upstream {
    Upstream {
        prefix: Prefix::Go,
        base: url.trim_end_matches('/').to_string(),
        auth: Auth::None,
    }
}

/// Test seams (feature `testing`; no production code calls them): a fake DNS table, extra trusted roots and a fake clock,
/// so the integration tests can run fake HTTPS upstreams. `start` always verifies against the webpki roots and resolves
/// through the system resolver.
#[cfg(feature = "testing")]
pub mod testing {
    use super::{Config, Handle, server};
    use std::collections::BTreeMap;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    /// How a test proxy reaches its fake upstreams.
    #[derive(Debug)]
    pub struct TestNet {
        /// DER certificates of the only roots the upstream TLS trusts (the webpki roots are not used).
        pub roots_der: Vec<Vec<u8>>,
        /// Host name (lower case) to the socket address of the fake that answers for it.
        pub dns: BTreeMap<String, SocketAddr>,
        /// The Unix time `expires_at_unix` is compared with (the system clock when `None`).
        pub clock: Option<Arc<AtomicU64>>,
    }

    /// Whether the accept loop keeps a connection from `peer`.
    pub fn peer_is_loopback(peer: SocketAddr) -> bool {
        server::peer_is_loopback(peer)
    }

    /// [`super::start`] over the test network.
    pub async fn start_with(cfg: Config, net: TestNet) -> std::io::Result<Handle> {
        let inputs = server::TestInputs {
            net: crate::net::TestNet {
                roots_der: net.roots_der,
                dns: net.dns,
            },
            clock: net.clock,
            head_cap: None,
        };
        Ok(Handle {
            run: server::launch(cfg, Some(inputs)).await?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn dropping_a_secret_wipes_the_buffer_it_was_stored_in() {
        let text = "x".repeat(40);
        let seen: Probe = Arc::new(Mutex::new(None));
        let mut secret = Secret::new(text);
        let stored_at = secret.value.as_ptr() as usize;
        secret.probe = Some(seen.clone());
        drop(secret);
        let Some((wiped_at, bytes)) = seen.lock().ok().and_then(|mut g| g.take()) else {
            panic!("dropping a Secret did not reach the wipe");
        };
        assert_eq!(
            wiped_at, stored_at,
            "the buffer that held the secret is the one that was wiped, not a copy"
        );
        assert_eq!(bytes.len(), 40);
        assert!(
            bytes.iter().all(|&b| b == 0),
            "the bytes were still readable when the buffer was released"
        );
    }
}
