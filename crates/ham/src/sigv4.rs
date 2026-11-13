//! SigV4 signing for `/v1`. Credentials come from the profile of the config's `awsProfile`,
//! else the environment and then `AWS_PROFILE` or the default profile. A static-key or `credential_process` profile is read by the
//! aws-config profile provider. An IAM Identity Center (SSO) or assume-role profile (kinds by [`crate::awsprofile`]) is read by running
//! the AWS CLI ([`crate::awscli`]: `aws configure export-credentials`, 30 seconds at most), which prints what a `credential_process`
//! program prints; the expiry check below is the same for every source. Credentials are cached until five minutes before their expiry.
//! aws-config is built without its HTTPS client (S11), and its providers get an HTTP client that refuses every call, so a profile that
//! must call STS, SSO or IMDS itself is refused with [`NO_NETWORK`] (without it, the IMDS client aws-config builds eagerly panics when
//! cargo did not unify in an HTTPS client).

use aws_config_client::Region;
use aws_config_client::environment::EnvironmentVariableCredentialsProvider;
use aws_config_client::environment::region::EnvironmentVariableRegionProvider;
use aws_config_client::meta::credentials::CredentialsProviderChain;
use aws_config_client::meta::region::RegionProviderChain;
use aws_config_client::profile::{ProfileFileCredentialsProvider, ProfileFileRegionProvider};
use aws_config_client::provider_config::ProviderConfig;
use aws_credential_types::Credentials;
use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
use aws_sigv4::sign::v4;
use aws_smithy_runtime_api::client::http::{
    HttpConnector, HttpConnectorFuture, SharedHttpConnector, http_client_fn,
};
use aws_smithy_runtime_api::client::orchestrator::HttpRequest;
use aws_smithy_runtime_api::client::result::ConnectorError;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

use crate::awscli;
use crate::awsprofile::{self, ProfileKind};
use crate::http::TransportError;

pub const SERVICE: &str = "execute-api";
/// Cached credentials are replaced this long before they expire.
const REFRESH_BEFORE: Duration = Duration::from_secs(300);
/// A failed AWS CLI run is remembered this long, so that the requests of one command (`ham doctor` signs four) do not each wait for the
/// AWS CLI again, 30 seconds apiece when it hangs. A person who signs in and retries is served again after this time.
const FAILURE_MEMORY: Duration = Duration::from_secs(5);
/// Why a profile whose credentials need STS, SSO or IMDS is refused.
pub const NO_NETWORK: &str = "ham makes no network calls for credentials: use a credential_process \
profile, an SSO or assume-role profile (ham asks the AWS CLI, version 2.9.0 or later), static keys or the AWS_* environment variables";
/// What to do when a profile's `credential_process` exits 0 and prints something that is not credentials JSON. (When it exits
/// non-zero, aws-config puts the process's own stderr in the text, which says what to do: `aws sso login` for an ended SSO sign-in.)
const PROCESS_OUTPUT_HINT: &str = "Run the profile's credential_process command in a terminal: it must print the credentials as JSON, \
as `aws configure export-credentials --format process` does";

/// The credential providers' HTTP client: every call fails with [`NO_NETWORK`].
#[derive(Debug)]
struct NoNetwork;

impl HttpConnector for NoNetwork {
    fn call(&self, _request: HttpRequest) -> HttpConnectorFuture {
        HttpConnectorFuture::ready(Err(ConnectorError::other(NO_NETWORK.into(), None)))
    }
}

/// With the Region, so an STS or SSO call reaches the refusing client and fails with its reason.
fn provider_config(region: &str) -> ProviderConfig {
    ProviderConfig::without_region()
        .with_region(Some(Region::new(region.to_string())))
        .with_http_client(http_client_fn(|_, _| SharedHttpConnector::new(NoNetwork)))
}

enum Source {
    Static(Credentials),
    Provider {
        provider: SharedCredentialsProvider,
        cached: Mutex<Option<Credentials>>,
    },
    /// An SSO or assume-role profile: the AWS CLI exports its credentials.
    AwsCli {
        profile: String,
        kind: ProfileKind,
        cache: Mutex<AwsCliCache>,
    },
}

/// What the AWS CLI gave last: the credentials, or the way it failed and when (a failure is remembered for [`FAILURE_MEMORY`]).
#[derive(Default)]
struct AwsCliCache {
    credentials: Option<Credentials>,
    failure: Option<(Instant, String)>,
}

/// True when the environment holds an access key and its secret, which aws-config reads before any profile.
fn environment_keys_set() -> bool {
    let set = |names: &[&str]| {
        names
            .iter()
            .any(|n| std::env::var_os(n).is_some_and(|v| !v.is_empty()))
    };
    set(&["AWS_ACCESS_KEY_ID", "AWS_ACCESS_KEY"])
        && set(&["AWS_SECRET_ACCESS_KEY", "AWS_SECRET_KEY"])
}

/// The profile to run the AWS CLI for, and its kind: the named profile, else (when no keys are in the environment) `AWS_PROFILE` or the
/// default profile, in each case only when it is an SSO or assume-role profile.
fn aws_cli_profile(profile: Option<&str>) -> Option<(String, ProfileKind)> {
    let name = match profile {
        Some(p) => p.to_string(),
        None if environment_keys_set() => return None,
        None => std::env::var("AWS_PROFILE")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "default".to_string()),
    };
    let kind = awsprofile::profile_kind(&name);
    kind.uses_aws_cli().then_some((name, kind))
}

pub struct Signer {
    region: String,
    source: Source,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signer")
            .field("region", &self.region)
            .finish_non_exhaustive()
    }
}

impl Signer {
    /// Fixed credentials (tests and callers that already hold credentials).
    pub fn new(region: &str, credentials: Credentials) -> Self {
        Self {
            region: region.to_string(),
            source: Source::Static(credentials),
        }
    }

    /// The profile provider for `profile`, or the environment then the default profile. The
    /// Region is `region`, else `AWS_REGION`, else the profile's `region`.
    pub async fn from_profile(
        profile: Option<&str>,
        region: Option<&str>,
    ) -> Result<Self, TransportError> {
        let profile = profile.filter(|p| !p.is_empty());
        let region = match region.filter(|r| !r.is_empty()) {
            Some(r) => r.to_string(),
            None => {
                let mut from_file = ProfileFileRegionProvider::builder();
                if let Some(p) = profile {
                    from_file = from_file.profile_name(p);
                }
                RegionProviderChain::first_try(EnvironmentVariableRegionProvider::new())
                    .or_else(from_file.build())
                    .region()
                    .await
                    .map(|r| r.to_string())
                    .ok_or_else(|| {
                        TransportError::Credentials(
                            "no Region: set region in config.toml, AWS_REGION, or the profile's region"
                                .into(),
                        )
                    })?
            }
        };
        if let Some((name, kind)) = aws_cli_profile(profile) {
            return Ok(Self {
                region,
                source: Source::AwsCli {
                    profile: name,
                    kind,
                    cache: Mutex::new(AwsCliCache::default()),
                },
            });
        }
        let provider = match profile {
            Some(p) => SharedCredentialsProvider::new(
                ProfileFileCredentialsProvider::builder()
                    .configure(&provider_config(&region))
                    .profile_name(p)
                    .build(),
            ),
            None => SharedCredentialsProvider::new(
                CredentialsProviderChain::first_try(
                    "Environment",
                    EnvironmentVariableCredentialsProvider::new(),
                )
                .or_else(
                    "Profile",
                    ProfileFileCredentialsProvider::builder()
                        .configure(&provider_config(&region))
                        .build(),
                ),
            ),
        };
        Ok(Self {
            region,
            source: Source::Provider {
                provider,
                cached: Mutex::new(None),
            },
        })
    }

    pub fn region(&self) -> &str {
        &self.region
    }

    async fn credentials(&self) -> Result<Credentials, TransportError> {
        match &self.source {
            Source::Static(c) => Ok(c.clone()),
            Source::Provider { provider, cached } => {
                let mut slot = cached.lock().await;
                if !is_fresh(&slot) {
                    let c = provider.provide_credentials().await.map_err(|e| {
                        TransportError::Credentials(credentials_failure(error_chain(&e)))
                    })?;
                    refuse_expired(&c, |when| {
                        format!(
                            "the credentials expired at {when}. Sign in again (SSO: aws sso login --profile <your SSO profile>), then retry"
                        )
                    })?;
                    *slot = Some(c);
                }
                slot.clone()
                    .ok_or_else(|| TransportError::Credentials("no credentials".into()))
            }
            Source::AwsCli {
                profile,
                kind,
                cache,
            } => {
                let mut cache = cache.lock().await;
                if is_fresh(&cache.credentials) {
                    return cache
                        .credentials
                        .clone()
                        .ok_or_else(|| TransportError::Credentials("no credentials".into()));
                }
                if let Some((at, why)) = &cache.failure
                    && at.elapsed() < FAILURE_MEMORY
                {
                    return Err(TransportError::Credentials(why.clone()));
                }
                match export_from_aws_cli(profile, *kind).await {
                    Ok(c) => {
                        cache.failure = None;
                        cache.credentials = Some(c.clone());
                        Ok(c)
                    }
                    Err(why) => {
                        cache.failure = Some((Instant::now(), why.clone()));
                        Err(TransportError::Credentials(why))
                    }
                }
            }
        }
    }

    /// The headers SigV4 adds to a request (`authorization`, `x-amz-date` and, with session
    /// credentials, `x-amz-security-token`). `headers` are signed too; `host` comes from `url`.
    pub async fn sign(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<Vec<(String, String)>, TransportError> {
        self.sign_for(SERVICE, method, url, headers, body).await
    }

    /// [`Signer::sign`] for another service (`bedrock-agentcore` for `ham shell`).
    pub async fn sign_for(
        &self,
        service: &str,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<Vec<(String, String)>, TransportError> {
        let creds = self.credentials().await?;
        sign_request_for(
            service,
            &creds,
            &self.region,
            method,
            url,
            headers,
            body,
            SystemTime::now(),
        )
    }
}

/// Signs one request for `execute-api` at `time`.
pub fn sign_request(
    creds: &Credentials,
    region: &str,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    time: SystemTime,
) -> Result<Vec<(String, String)>, TransportError> {
    sign_request_for(SERVICE, creds, region, method, url, headers, body, time)
}

/// Signs one request for `service` at `time`.
#[allow(clippy::too_many_arguments)]
pub fn sign_request_for(
    service: &str,
    creds: &Credentials,
    region: &str,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    time: SystemTime,
) -> Result<Vec<(String, String)>, TransportError> {
    let identity = creds.clone().into();
    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name(service)
        .time(time)
        .settings(SigningSettings::default())
        .build()
        .map_err(|e| TransportError::Sign(e.to_string()))?
        .into();
    let request = SignableRequest::new(
        method,
        url,
        headers.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        SignableBody::Bytes(body),
    )
    .map_err(|e| TransportError::Sign(e.to_string()))?;
    let (instructions, _signature) = sign(request, &params)
        .map_err(|e| TransportError::Sign(e.to_string()))?
        .into_parts();
    let mut carrier = http::Request::new(());
    instructions.apply_to_request_http1x(&mut carrier);
    Ok(carrier
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                v.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect())
}

/// The credentials of an SSO or assume-role profile from the AWS CLI, off the async threads (the command takes up to 30 seconds), with the
/// expiry check every source gets. The error is the sentence for the person.
async fn export_from_aws_cli(profile: &str, kind: ProfileKind) -> Result<Credentials, String> {
    let asked = profile.to_string();
    let c = tokio::task::spawn_blocking(move || {
        awscli::export_credentials(
            &awscli::Program::from_env(),
            &asked,
            kind,
            awscli::TIME_LIMIT,
        )
    })
    .await
    .map_err(|e| format!("the AWS CLI could not be run: {e}"))??;
    match c.expiry().filter(|e| *e <= SystemTime::now()) {
        Some(expiry) if kind == ProfileKind::Sso => Err(format!(
            "the credentials expired at {}. Run: aws sso login --profile {profile}",
            utc_text(expiry)
        )),
        Some(expiry) => Err(format!(
            "the credentials expired at {}. Sign in again, then retry",
            utc_text(expiry)
        )),
        None => Ok(c),
    }
}

/// True while cached credentials have more than [`REFRESH_BEFORE`] left (or never expire).
fn is_fresh(slot: &Option<Credentials>) -> bool {
    slot.as_ref().is_some_and(|c| {
        c.expiry()
            .is_none_or(|e| e > SystemTime::now() + REFRESH_BEFORE)
    })
}

/// The service refuses expired credentials: say so here, with the way out given by `message(<UTC time>)`, and send nothing.
fn refuse_expired(
    c: &Credentials,
    message: impl FnOnce(&str) -> String,
) -> Result<(), TransportError> {
    match c.expiry().filter(|e| *e <= SystemTime::now()) {
        Some(expiry) => Err(TransportError::Credentials(message(&utc_text(expiry)))),
        None => Ok(()),
    }
}

/// The text of a failed profile provider; adds [`PROCESS_OUTPUT_HINT`] when aws-config could not parse what a `credential_process` printed.
fn credentials_failure(chain: String) -> String {
    if chain.contains("could not parse response") {
        format!("{chain}. {PROCESS_OUTPUT_HINT}")
    } else {
        chain
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` of a time in UTC (the inverse of `doctor::parse_utc`; a time before 1970 reads as 1970).
fn utc_text(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Days since 1970-01-01 to a civil date (H. Hinnant's days_from_civil, run backwards).
    let z = i64::try_from(days).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

/// An error and its sources on one line.
pub(crate) fn error_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(cause) = src {
        let c = cause.to_string();
        if !s.contains(&c) {
            s.push_str(": ");
            s.push_str(&c);
        }
        src = cause.source();
    }
    s
}
