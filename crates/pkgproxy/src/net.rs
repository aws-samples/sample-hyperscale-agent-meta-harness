//! The upstream side: name resolution, TLS and the pooled HTTP client.
//!
//! Production resolves through the system resolver and refuses an address that is loopback, link-local, unspecified or multicast
//! (a name under `amazonaws.com` must not lead to the instance metadata address or to the host itself), and verifies the server
//! certificate against the webpki roots with no switch to skip it. The `testing` seams swap in a fixed name table and a private
//! root, nothing else.

use bytes::Bytes;
use http_body_util::Empty;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::connect::dns::Name;
use hyper_util::rt::{TokioExecutor, TokioTimer};
use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

pub(crate) type UpstreamClient = Client<HttpsConnector<HttpConnector<Resolver>>, Empty<Bytes>>;

/// Time allowed to open a TCP connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// What a test supplies instead of the system resolver and the webpki roots.
#[derive(Clone, Debug, Default)]
pub(crate) struct TestNet {
    pub roots_der: Vec<Vec<u8>>,
    pub dns: BTreeMap<String, SocketAddr>,
}

/// Whether the proxy may connect to `ip`.
pub(crate) fn ip_allowed(ip: IpAddr) -> bool {
    let ip = ip.to_canonical();
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    match ip {
        IpAddr::V4(v4) => !(v4.is_link_local() || v4.is_broadcast()),
        // fe80::/10 link-local.
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) != 0xfe80,
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Resolver {
    table: Option<Arc<BTreeMap<String, SocketAddr>>>,
}

type ResolveFuture =
    Pin<Box<dyn Future<Output = io::Result<std::vec::IntoIter<SocketAddr>>> + Send>>;

impl tower_service::Service<Name> for Resolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = io::Error;
    type Future = ResolveFuture;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let host = name.as_str().to_ascii_lowercase();
        let table = self.table.clone();
        Box::pin(async move {
            if let Some(table) = table {
                return match table.get(&host) {
                    Some(addr) => Ok(vec![*addr].into_iter()),
                    None => Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "dns resolution failed",
                    )),
                };
            }
            let found: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "dns resolution failed"))?
                .collect();
            if found.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "dns resolution failed",
                ));
            }
            let allowed: Vec<SocketAddr> =
                found.into_iter().filter(|a| ip_allowed(a.ip())).collect();
            if allowed.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "address not allowed",
                ));
            }
            Ok(allowed.into_iter())
        })
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The pooled client. `test` replaces the resolver and the trusted roots; `None` is production.
pub(crate) fn build_client(test: Option<&TestNet>) -> io::Result<UpstreamClient> {
    let resolver = Resolver {
        table: test.map(|t| Arc::new(t.dns.clone())),
    };
    let mut http = HttpConnector::new_with_resolver(resolver);
    http.enforce_http(false);
    http.set_connect_timeout(Some(CONNECT_TIMEOUT));
    http.set_nodelay(true);
    http.set_keepalive(Some(Duration::from_secs(30)));
    let other = |e: rustls::Error| io::Error::other(format!("tls setup: {e}"));
    let builder = match test {
        None => HttpsConnectorBuilder::new()
            .with_provider_and_webpki_roots(provider())
            .map_err(other)?,
        Some(t) => {
            let mut roots = rustls::RootCertStore::empty();
            for der in &t.roots_der {
                roots
                    .add(rustls::pki_types::CertificateDer::from(der.clone()))
                    .map_err(other)?;
            }
            let config = rustls::ClientConfig::builder_with_provider(provider())
                .with_safe_default_protocol_versions()
                .map_err(other)?
                .with_root_certificates(roots)
                .with_no_client_auth();
            HttpsConnectorBuilder::new().with_tls_config(config)
        }
    };
    let https = builder.https_only().enable_http1().wrap_connector(http);
    Ok(Client::builder(TokioExecutor::new())
        .pool_timer(TokioTimer::new())
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(8)
        .build(https))
}

/// A short class for a failed upstream request: no URL, no host, no header and no token ever reaches the client.
pub(crate) fn classify(err: &(dyn std::error::Error + 'static)) -> &'static str {
    let mut text = String::new();
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        text.push_str(&e.to_string().to_ascii_lowercase());
        text.push('|');
        cur = e.source();
    }
    let has = |needles: &[&str]| needles.iter().any(|n| text.contains(n));
    if has(&["address not allowed"]) {
        "blocked"
    } else if has(&["dns resolution failed", "dns error"]) {
        "dns"
    } else if has(&[
        "close_notify",
        "peer closed",
        "unexpected eof",
        "broken pipe",
        "connection reset",
        "connection closed",
    ]) {
        "reset"
    } else if has(&[
        "certificate",
        "unknownissuer",
        "invalid peer",
        "handshake",
        "tls",
        "alert",
    ]) {
        "tls"
    } else if has(&["timed out", "timeout", "deadline"]) {
        "timeout"
    } else if has(&["reset", "eof", "closed before"]) {
        "reset"
    } else if has(&["connect", "refused", "unreachable"]) {
        "connect"
    } else {
        "protocol"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_loopback_link_local_unspecified_and_multicast() {
        for ip in [
            "127.0.0.1",
            "127.1.2.3",
            "0.0.0.0",
            "169.254.169.254",
            "169.254.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fe80::1",
            "febf::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
        ] {
            assert!(
                !ip_allowed(ip.parse().unwrap_or(IpAddr::from([0, 0, 0, 0]))),
                "{ip}"
            );
        }
    }

    #[test]
    fn allows_public_and_private_addresses_endpoints_resolve_to() {
        // A VPC interface endpoint resolves to a private address; S3 and CodeArtifact are public.
        for ip in [
            "10.0.1.5",
            "172.16.3.4",
            "192.168.1.1",
            "52.216.1.1",
            "2600:1f18::1",
            "fc00::1",
        ] {
            assert!(
                ip_allowed(ip.parse().unwrap_or(IpAddr::from([127, 0, 0, 1]))),
                "{ip}"
            );
        }
    }

    fn name(host: &str) -> Name {
        host.parse()
            .unwrap_or_else(|_| panic!("{host} is not a name"))
    }

    #[tokio::test]
    async fn production_resolution_refuses_a_name_that_leads_to_this_host_or_the_metadata_address()
    {
        // `table: None` is the production path: the system resolver, then the address filter. IP literals resolve without DNS, so
        // this stands for any `*.amazonaws.com` name whose record points at an address the proxy must never connect to.
        let mut prod = Resolver { table: None };
        for host in [
            "127.0.0.1",
            "127.0.0.53",
            "169.254.169.254",
            "169.254.0.1",
            "0.0.0.0",
            "::1",
            "::",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "localhost",
        ] {
            let got = tower_service::Service::call(&mut prod, name(host)).await;
            assert!(got.is_err(), "{host} must not be connectable");
        }
        for host in ["127.0.0.1", "169.254.169.254", "0.0.0.0", "::1"] {
            let err = tower_service::Service::call(&mut prod, name(host))
                .await
                .expect_err(host);
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{host}");
            assert_eq!(classify(&err), "blocked", "{host}");
        }
        // A public or private address is let through (port 0: the connector fills in the real port).
        for (host, want) in [
            ("52.216.1.1", "52.216.1.1:0"),
            ("10.0.1.5", "10.0.1.5:0"),
            ("2600:1f18::1", "[2600:1f18::1]:0"),
        ] {
            let got: Vec<String> = tower_service::Service::call(&mut prod, name(host))
                .await
                .unwrap_or_else(|e| panic!("{host}: {e}"))
                .map(|a| a.to_string())
                .collect();
            assert_eq!(got, vec![want.to_string()], "{host}");
        }
    }

    #[tokio::test]
    async fn the_test_table_resolves_listed_names_only_and_ignores_case() {
        let addr: SocketAddr = "127.0.0.1:9".parse().unwrap_or_else(|_| unreachable!());
        let mut fake = Resolver {
            table: Some(Arc::new(BTreeMap::from([(
                "fake.example".to_string(),
                addr,
            )]))),
        };
        let got: Vec<SocketAddr> = tower_service::Service::call(&mut fake, name("FAKE.example"))
            .await
            .unwrap_or_else(|e| panic!("{e}"))
            .collect();
        assert_eq!(got, vec![addr]);
        let err = tower_service::Service::call(&mut fake, name("other.example"))
            .await
            .expect_err("unlisted");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(classify(&err), "dns");
    }

    #[test]
    fn the_connect_timeout_is_the_contracts_ten_seconds() {
        // The value reaches the connector in `build_client`.
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(10));
    }

    #[test]
    fn classes_carry_no_text_of_the_error() {
        let e = io::Error::new(
            io::ErrorKind::PermissionDenied,
            "address not allowed to https://secret.example/x?token=abc",
        );
        assert_eq!(classify(&e), "blocked");
        let e = io::Error::new(io::ErrorKind::TimedOut, "operation timed out");
        assert_eq!(classify(&e), "timeout");
        let e = io::Error::new(io::ErrorKind::ConnectionReset, "Connection reset by peer");
        assert_eq!(classify(&e), "reset");
        let e = io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid peer certificate: UnknownIssuer",
        );
        assert_eq!(classify(&e), "tls");
    }
}
