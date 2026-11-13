//! The search providers and the choice among them.
//!
//! The stage configuration chooses the provider; a caller cannot. `auto` holds both
//! Tavily and AgentCore and chooses for each call by one rule, see [`AutoProvider`]. The four older words pin one provider: `tavily` is Tavily
//! with the stage's key, `tavily-keyless` is an explicit first wiring check without a key, `agentcore` is AWS's own web index through an
//! AgentCore Gateway and `fixture` answers from canned data on test stages only. The fixture
//! lock is a run-time rule: [`FixtureProvider::new`] and [`build_provider`] refuse it unless the stage kind is `test`.
//! Every answer reports the provider that
//! made it, so each record can be checked against the provider that served it.
//!
//! # The fixture data
//!
//! Hosts it answers for: `www.npmjs.com` and `github.com` (the `is-plain-obj` set the DIRECT-RESEARCH end-to-end searches for),
//! `docs.example.org` and `blog.example.org`. A query containing `is-plain-obj` returns the npm page, the repository and one guide; any
//! other query returns two pages whose addresses carry a slug of the query. A page whose path ends in `/gone` fails to extract. The
//! npm page links to the repository, so the provenance table of a run that fetched it knows the repository address. No canned text
//! holds a key-shaped string or echoes the address it was asked for.

use crate::agentcore::{AgentcoreProvider, GatewayTarget};
use crate::http::HttpClient;
use crate::metrics::Metrics;
use crate::ports::{
    ExtractFailure, ExtractOutcome, ExtractedPage, Fail, KeyProbe, KeyState, Provider,
    SearchOutcome, Stage, StageKind,
};
use crate::scan::check_url;
use crate::secret::{AwsSecretSource, CachedSecret};
use crate::tavily::TavilyClient;
use aws_credential_types::provider::SharedCredentialsProvider;
use harness_protocol::research::{
    ResearchErrorCode, ResearchProvider, SearchRequest, SearchResult,
};
use std::sync::Arc;

/// Where Tavily is.
pub const TAVILY_BASE_URL: &str = "https://api.tavily.com";

/// The `request_id` a fixture answer carries.
const FIXTURE_REQUEST_ID: &str = "fixture";
/// Results when the request names no count.
const DEFAULT_RESULTS: usize = 5;
/// The longest slug in a generic fixture address.
const MAX_SLUG: usize = 40;

const NPM_URL: &str = "https://www.npmjs.com/package/is-plain-obj";
const REPO_URL: &str = "https://github.com/sindresorhus/is-plain-obj";
const GUIDE_URL: &str = "https://docs.example.org/guides/plain-objects";

const NPM_PAGE: &str = "# is-plain-obj\n\nCheck if a value is a plain object.\n\n\
An object is plain if it's created by either `{}`, `new Object()`, or `Object.create(null)`.\n\n\
## Install\n\nnpm install is-plain-obj\n\n\
## Source\n\nThe source is at https://github.com/sindresorhus/is-plain-obj and the licence is MIT.\n";

const REPO_PAGE: &str = "# sindresorhus/is-plain-obj\n\nCheck if a value is a plain object.\n\n\
The package exports one function. It returns true for values created by `{}`, `new Object()` or `Object.create(null)`.\n";

const GUIDE_PAGE: &str = "# Plain objects\n\nA plain object has no prototype chain beyond Object.prototype or null. \
Class instances, arrays and functions are not plain objects.\n";

/// Canned answers, no network. Only a test stage can have one.
#[derive(Clone, Copy, Debug)]
pub struct FixtureProvider {
    _lock: (),
}

impl FixtureProvider {
    /// Refused unless the stage is a test stage.
    pub fn new(kind: StageKind) -> Result<Self, Fail> {
        match kind {
            StageKind::Test => Ok(Self { _lock: () }),
            StageKind::Official => Err(Fail::new(
                ResearchErrorCode::Internal,
                "the fixture provider runs only on a test stage; this stage is official",
            )),
        }
    }
}

fn result(title: &str, url: &str, snippet: &str, score: f64) -> SearchResult {
    SearchResult {
        title: title.to_string(),
        url: url.to_string(),
        snippet: snippet.to_string(),
        published: None,
        score: Some(score),
    }
}

/// Lower-case letters and digits of `query`, words joined by `-`, at most [`MAX_SLUG`] characters; `query` when nothing is left.
fn slug(query: &str) -> String {
    let words: Vec<String> = query
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let joined = words.join("-");
    let cut: String = joined.chars().take(MAX_SLUG).collect();
    let cut = cut.trim_end_matches('-').to_string();
    if cut.is_empty() {
        "query".to_string()
    } else {
        cut
    }
}

impl Provider for FixtureProvider {
    fn id(&self) -> ResearchProvider {
        ResearchProvider::Fixture
    }

    async fn search(&self, req: &SearchRequest) -> Result<SearchOutcome, Fail> {
        let max = req.max_results.map_or(DEFAULT_RESULTS, usize::from);
        let mut results = if req.query.to_ascii_lowercase().contains("is-plain-obj") {
            vec![
                result(
                    "is-plain-obj - npm",
                    NPM_URL,
                    "Check if a value is a plain object. An object is plain if it's created by either {}, new Object(), or Object.create(null).",
                    0.93,
                ),
                result(
                    "GitHub - sindresorhus/is-plain-obj: Check if a value is a plain object",
                    REPO_URL,
                    "Check if a value is a plain object. The repository holds the source, the tests and the licence.",
                    0.87,
                ),
                result(
                    "Plain objects - guides",
                    GUIDE_URL,
                    "What a plain object is, and how it differs from class instances, arrays and functions.",
                    0.61,
                ),
            ]
        } else {
            let slug = slug(&req.query);
            vec![
                result(
                    "Fixture result 1",
                    &format!("https://docs.example.org/search/{slug}"),
                    "Canned documentation text for a fixture query.",
                    0.5,
                ),
                result(
                    "Fixture result 2",
                    &format!("https://blog.example.org/posts/{slug}"),
                    "Canned blog text for a fixture query.",
                    0.4,
                ),
            ]
        };
        results.truncate(max);
        Ok(SearchOutcome {
            results,
            credits: None,
            upstream_request_id: Some(FIXTURE_REQUEST_ID.to_string()),
        })
    }

    async fn extract(&self, urls: &[String], _query: Option<&str>) -> Result<ExtractOutcome, Fail> {
        let mut pages = Vec::new();
        let mut failed = Vec::new();
        for url in urls {
            let host = check_url(url, usize::MAX).map(|c| c.host).ok();
            let gone = url
                .split(['?', '#'])
                .next()
                .is_some_and(|p| p.trim_end_matches('/').ends_with("/gone"));
            if gone {
                failed.push(ExtractFailure {
                    url: url.clone(),
                    message: "Failed to fetch url".to_string(),
                });
                continue;
            }
            let text = match url.as_str() {
                NPM_URL => NPM_PAGE.to_string(),
                REPO_URL => REPO_PAGE.to_string(),
                GUIDE_URL => GUIDE_PAGE.to_string(),
                _ => format!(
                    "# Fixture page\n\nCanned text for a page on {}. It links to https://docs.example.org/more and nothing else.\n",
                    host.as_deref().unwrap_or("an unknown host")
                ),
            };
            pages.push(ExtractedPage {
                url: url.clone(),
                text,
            });
        }
        Ok(ExtractOutcome {
            pages,
            failed,
            credits: None,
            upstream_request_id: Some(FIXTURE_REQUEST_ID.to_string()),
        })
    }
}

/// A provider chosen by the stage: static dispatch over the kinds.
#[derive(Debug)]
pub enum AnyProvider {
    /// `tavily` (the key from Secrets Manager) or `tavily-keyless`.
    Tavily(Box<TavilyClient<AwsSecretSource>>),
    /// `agentcore`: the Web Search Tool through an AgentCore Gateway.
    Agentcore(Box<AgentcoreProvider>),
    Fixture(FixtureProvider),
}

impl AnyProvider {
    /// Reports the Tavily key's health to `metrics`. Only the keyed Tavily client has a key to report on (the keyless one ignores the sink); the
    /// `agentcore` and `fixture` providers are returned as they are, so they report nothing.
    pub fn with_metrics(self, metrics: Arc<dyn Metrics>) -> Self {
        match self {
            Self::Tavily(client) => Self::Tavily(Box::new((*client).with_metrics(metrics))),
            other => other,
        }
    }
}

impl Provider for AnyProvider {
    fn id(&self) -> ResearchProvider {
        match self {
            Self::Tavily(client) => client.id(),
            Self::Agentcore(provider) => provider.id(),
            Self::Fixture(fixture) => fixture.id(),
        }
    }

    fn max_query_chars(&self) -> Option<usize> {
        match self {
            Self::Tavily(client) => client.max_query_chars(),
            Self::Agentcore(provider) => provider.max_query_chars(),
            Self::Fixture(fixture) => fixture.max_query_chars(),
        }
    }

    fn can_extract(&self) -> bool {
        match self {
            Self::Tavily(client) => client.can_extract(),
            Self::Agentcore(provider) => provider.can_extract(),
            Self::Fixture(fixture) => fixture.can_extract(),
        }
    }

    async fn search(&self, req: &SearchRequest) -> Result<SearchOutcome, Fail> {
        match self {
            Self::Tavily(client) => client.search(req).await,
            Self::Agentcore(provider) => provider.search(req).await,
            Self::Fixture(fixture) => fixture.search(req).await,
        }
    }

    async fn extract(&self, urls: &[String], query: Option<&str>) -> Result<ExtractOutcome, Fail> {
        match self {
            Self::Tavily(client) => client.extract(urls, query).await,
            Self::Agentcore(provider) => provider.extract(urls, query).await,
            Self::Fixture(fixture) => fixture.extract(urls, query).await,
        }
    }
}

/// Only the keyed Tavily client holds a key; every other provider has none to probe.
impl KeyProbe for AnyProvider {
    async fn key_stored(&self) -> Result<bool, Fail> {
        match self {
            Self::Tavily(client) => client.key_stored().await,
            Self::Agentcore(_) | Self::Fixture(_) => Ok(false),
        }
    }

    async fn key_version(&self) -> Result<Option<String>, Fail> {
        match self {
            Self::Tavily(client) => client.key_version().await,
            Self::Agentcore(_) | Self::Fixture(_) => Ok(None),
        }
    }
}

/// What the `agentcore` provider is built from: the gateway, the tool of its target and the function's own credentials. No secret.
#[derive(Debug)]
pub struct AgentcoreDeps {
    pub target: GatewayTarget,
    pub tool: String,
    pub credentials: SharedCredentialsProvider,
}

/// What the providers are built from.
#[derive(Debug)]
pub struct ProviderDeps {
    /// The client for Tavily and for the gateway; required by `tavily`, `tavily-keyless`, `agentcore` and `auto`.
    pub http: Option<HttpClient>,
    /// The stage's key; required by `tavily` and `auto`.
    pub secret: Option<CachedSecret<AwsSecretSource>>,
    /// [`TAVILY_BASE_URL`] unless a test points it elsewhere.
    pub tavily_base_url: String,
    /// The gateway settings; required by `agentcore` and `auto`.
    pub agentcore: Option<AgentcoreDeps>,
}

fn missing(what: &str) -> Fail {
    Fail::new(
        ResearchErrorCode::Internal,
        format!("the search provider needs {what}, which this function was not given"),
    )
}

/// The provider the stage names.
pub fn build_provider(
    provider: ResearchProvider,
    kind: StageKind,
    deps: ProviderDeps,
) -> Result<AnyProvider, Fail> {
    match provider {
        ResearchProvider::Fixture => FixtureProvider::new(kind).map(AnyProvider::Fixture),
        ResearchProvider::Tavily => {
            let http = deps.http.ok_or_else(|| missing("an HTTP client"))?;
            let secret = deps.secret.ok_or_else(|| missing("the key secret"))?;
            Ok(AnyProvider::Tavily(Box::new(TavilyClient::keyed(
                http,
                deps.tavily_base_url,
                secret,
            ))))
        }
        ResearchProvider::TavilyKeyless => {
            let http = deps.http.ok_or_else(|| missing("an HTTP client"))?;
            Ok(AnyProvider::Tavily(Box::new(TavilyClient::keyless(
                http,
                deps.tavily_base_url,
            ))))
        }
        ResearchProvider::Agentcore => {
            let http = deps.http.ok_or_else(|| missing("an HTTP client"))?;
            let gateway = deps
                .agentcore
                .ok_or_else(|| missing("the gateway settings"))?;
            Ok(AnyProvider::Agentcore(Box::new(AgentcoreProvider::new(
                http,
                gateway.target,
                gateway.tool,
                gateway.credentials,
            ))))
        }
        ResearchProvider::GithubArchive | ResearchProvider::User => Err(Fail::new(
            ResearchErrorCode::Internal,
            "that provider is not a search provider",
        )),
    }
}

// ---- the stage: one provider, or `auto` ----------------------------------------------------------------------

/// How a stage names its search provider: the word `auto`, or one of the four words that pin a provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageChoice {
    Auto,
    Pinned(ResearchProvider),
}

/// The `auto` stage: a keyed provider and a keyless one side by side, with one rule that picks the provider of each call.
///
/// A Tavily key that is stored, meaning the key secret has a current, non-blank version, makes Tavily the provider of the call (search, page fetch and
/// limits). With no key stored AgentCore Web Search is: it searches, it cannot fetch a page (`fetch` answers `disabled`, as on an `agentcore` stage), and
/// the limits answer reports its shorter query cap. The rule reads the key cache (a found key is kept five minutes, "no key" thirty seconds), so a key
/// that is stored or cleared takes effect within that time. It never looks at how an earlier call ended: a stored key that Tavily refuses answers
/// `no_key`, a 429 answers `upstream_rate_limited`, a plan limit answers `upstream_error`, and none of them sends the query to the other provider. A key
/// store that cannot be read, with no key held from an earlier read, fails the call (`upstream_error`) and sends nothing anywhere: the choice is
/// unknown, and a query must not go to a service the stored key does not name.
///
/// The two halves are one type `P` so that either can be handed to a call as `&P`. Production holds [`AnyProvider`]s: the keyed Tavily client and the
/// AgentCore provider.
#[derive(Debug)]
pub struct AutoProvider<P> {
    keyed: P,
    gateway: P,
}

impl<P> AutoProvider<P> {
    /// `keyed` is the provider used when a key is stored, `gateway` the one used when none is.
    pub fn new(keyed: P, gateway: P) -> Self {
        Self { keyed, gateway }
    }
}

impl<P: Provider + KeyProbe> Stage for AutoProvider<P> {
    type Provider = P;

    async fn serve(&self) -> Result<&P, Fail> {
        if self.keyed.key_stored().await? {
            Ok(&self.keyed)
        } else {
            Ok(&self.gateway)
        }
    }

    fn audit_word(&self) -> ResearchProvider {
        ResearchProvider::GithubArchive
    }

    fn reports_provider_caps(&self) -> bool {
        true
    }

    /// The provider the next search would use and, for the keyed one, the version id of the key it holds: one question to the key cache, nothing sent to
    /// either provider.
    async fn key_state(&self) -> Result<KeyState, Fail> {
        if self.keyed.key_stored().await? {
            Ok(KeyState {
                provider: self.keyed.id(),
                version_id: self.keyed.key_version().await?,
            })
        } else {
            Ok(KeyState {
                provider: self.gateway.id(),
                version_id: None,
            })
        }
    }
}

/// The stage of a deployment, chosen by the stage configuration: one provider, or `auto`.
#[derive(Debug)]
pub enum AnyStage {
    Fixed(AnyProvider),
    Auto(Box<AutoProvider<AnyProvider>>),
}

impl AnyStage {
    /// Reports the Tavily key's health to `metrics`: a stage of one provider hands the sink to it (only the keyed Tavily client reports), and the `auto`
    /// stage hands it to its keyed half, so the events of Tavily's answers are written while Tavily serves and none while AgentCore does.
    pub fn with_metrics(self, metrics: Arc<dyn Metrics>) -> Self {
        match self {
            Self::Fixed(provider) => Self::Fixed(provider.with_metrics(metrics)),
            Self::Auto(auto) => {
                let AutoProvider { keyed, gateway } = *auto;
                Self::Auto(Box::new(AutoProvider::new(
                    keyed.with_metrics(metrics),
                    gateway,
                )))
            }
        }
    }
}

impl Stage for AnyStage {
    type Provider = AnyProvider;

    async fn serve(&self) -> Result<&AnyProvider, Fail> {
        match self {
            Self::Fixed(provider) => Ok(provider),
            Self::Auto(auto) => auto.serve().await,
        }
    }

    fn audit_word(&self) -> ResearchProvider {
        match self {
            Self::Fixed(provider) => provider.id(),
            Self::Auto(auto) => auto.audit_word(),
        }
    }

    fn reports_provider_caps(&self) -> bool {
        matches!(self, Self::Auto(_))
    }

    async fn key_state(&self) -> Result<KeyState, Fail> {
        match self {
            Self::Fixed(provider) => Ok(KeyState {
                provider: provider.id(),
                version_id: provider.key_version().await?,
            }),
            Self::Auto(auto) => auto.key_state().await,
        }
    }
}

/// The stage the configuration names. A pinned word builds exactly what [`build_provider`] builds. `auto` needs everything both providers need: the HTTP
/// client, the key secret and the gateway settings. It builds the keyed Tavily client and the AgentCore provider over one client. Without any of the
/// three it is refused with an `internal` failure (the function then fails at start and no call is served).
pub fn build_stage(
    choice: StageChoice,
    kind: StageKind,
    deps: ProviderDeps,
) -> Result<AnyStage, Fail> {
    match choice {
        StageChoice::Pinned(provider) => build_provider(provider, kind, deps).map(AnyStage::Fixed),
        StageChoice::Auto => {
            let http = deps.http.ok_or_else(|| missing("an HTTP client"))?;
            let secret = deps.secret.ok_or_else(|| missing("the key secret"))?;
            let gateway = deps
                .agentcore
                .ok_or_else(|| missing("the gateway settings"))?;
            let keyed = AnyProvider::Tavily(Box::new(TavilyClient::keyed(
                http.clone(),
                deps.tavily_base_url,
                secret,
            )));
            let search = AnyProvider::Agentcore(Box::new(AgentcoreProvider::new(
                http,
                gateway.target,
                gateway.tool,
                gateway.credentials,
            )));
            Ok(AnyStage::Auto(Box::new(AutoProvider::new(keyed, search))))
        }
    }
}
