//! The interfaces the pieces of the route share, so each lane can be built and tested against fakes.
//!
//! Failures carry the research error word they will be answered with ([`Fail`]); no port returns an untyped string error. A port never
//! returns, logs or formats a credential, a header, or page text beyond a short excerpt.

use harness_protocol::research::{
    ResearchError, ResearchErrorCode, ResearchProvider, SearchRequest, SearchResult,
};
use std::fmt;
use std::future::Future;

/// A failure that already knows the word it is answered with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fail {
    pub code: ResearchErrorCode,
    /// One plain sentence for the caller and the audit object. Never a key, a header or fetched text beyond a short excerpt.
    pub message: String,
    pub retry_after_s: Option<u32>,
}

impl Fail {
    pub fn new(code: ResearchErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retry_after_s: None,
        }
    }

    pub fn with_retry_after(mut self, seconds: u32) -> Self {
        self.retry_after_s = Some(seconds);
        self
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ResearchErrorCode::Internal, message)
    }

    pub fn upstream(message: impl Into<String>) -> Self {
        Self::new(ResearchErrorCode::UpstreamError, message)
    }
}

impl fmt::Display for Fail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Fail {}

impl From<Fail> for ResearchError {
    fn from(f: Fail) -> Self {
        ResearchError {
            code: f.code,
            message: f.message,
            retry_after_s: f.retry_after_s,
            request_id: None,
        }
    }
}

// ---- search provider (tavily, tavily-keyless, agentcore, fixture): the NET lane -----------------------------------------------------------------

/// What a search returned, before the route cuts snippets and stores the record.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchOutcome {
    pub results: Vec<SearchResult>,
    /// Credits the provider charged, when it says.
    pub credits: Option<u32>,
    /// The provider's own request id.
    pub upstream_request_id: Option<String>,
}

/// One page as the provider returned it (text, before the route's single cleaning pass).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractedPage {
    pub url: String,
    pub text: String,
}

/// One URL the provider could not extract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractFailure {
    pub url: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractOutcome {
    pub pages: Vec<ExtractedPage>,
    pub failed: Vec<ExtractFailure>,
    pub credits: Option<u32>,
    pub upstream_request_id: Option<String>,
}

/// A search provider. The error mapping every implementation follows: no key stored (or the provider refuses the key after one re-read
/// of the secret) is `no_key`; 429 is `upstream_rate_limited` with the provider's retry-after; a timeout, a 5xx, a plan limit and an
/// unusable answer are `upstream_error`; nothing found at an address is `not_found`. An error text never contains the key, a header
/// value, or more than a short excerpt of the upstream's body.
pub trait Provider: Send + Sync {
    /// The word every response, audit object and manifest records.
    fn id(&self) -> ResearchProvider;

    /// The longest query this provider takes, in characters, when it is shorter than the route's own limit. The route refuses a longer
    /// query before it scans, reserves or calls anything.
    fn max_query_chars(&self) -> Option<usize> {
        None
    }

    /// Whether [`Provider::extract`] works. A provider that cannot fetch a page (the Web Search Tool returns excerpts only) says so here,
    /// and the route refuses `fetch` before it reserves or calls anything.
    fn can_extract(&self) -> bool {
        true
    }

    /// `req` is already validated and scanned. At most `req.max_results` (default 5) results, no raw page content.
    fn search(
        &self,
        req: &SearchRequest,
    ) -> impl Future<Output = Result<SearchOutcome, Fail>> + Send;

    /// `urls` are already checked, at most 5. Basic extraction. `query` asks for focused chunks.
    fn extract(
        &self,
        urls: &[String],
        query: Option<&str>,
    ) -> impl Future<Output = Result<ExtractOutcome, Fail>> + Send;
}

/// A provider that authenticates with a key in a secret store and can say whether a key is stored now. The answer is the key cache's (a found key is kept
/// five minutes, "no key" thirty seconds), so it costs a read only when the cache has expired. A blank value and a secret with no current version are no key.
/// `Err` is a store that could not be read while no key from an earlier read is still held.
pub trait KeyProbe: Send + Sync {
    fn key_stored(&self) -> impl Future<Output = Result<bool, Fail>> + Send;

    /// The Secrets Manager version id of the key held now, from the same cache (a question that finds the cache expired reads the secret again, as the next
    /// call would). `None` when no key is held or the store gave no id. The id is chosen by Secrets Manager; it is not the key and not derived from it.
    fn key_version(&self) -> impl Future<Output = Result<Option<String>, Fail>> + Send {
        async { Ok(None) }
    }
}

/// What the operator's key-state question reports: the provider a search would use now and the version id of the key the function holds
/// for it. Nothing else: never the key, nothing derived from it, no count and no time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyState {
    pub provider: ResearchProvider,
    /// `None` for a provider that holds no key, and when no key is stored.
    pub version_id: Option<String>,
}

/// What a deployment holds as its search provider. A stage with one provider is that provider: every [`Provider`] is a
/// stage that serves itself, so the four words that pin a provider and every older test are unchanged. The `auto` stage chooses between two providers once
/// per call by one rule ([`crate::providers::AutoProvider`]). The route asks the stage for the provider that serves a call ([`Stage::serve`]) before it
/// spends anything, and uses that one provider for the whole call, so the answer, the stored record and the audit object always name the provider that
/// was asked.
pub trait Stage: Send + Sync {
    /// The provider type that serves a call.
    type Provider: Provider;

    /// Chooses the provider for one search, fetch or limits call. A stage of one provider answers at once; the `auto` stage reads the key cache. `Err`
    /// means the choice could not be made (the key store could not be read): the call fails and nothing is sent anywhere.
    fn serve(&self) -> impl Future<Output = Result<&Self::Provider, Fail>> + Send;

    /// The provider word of the audit object of a call that uses no search provider (a repository download, a job status). A stage of one provider names
    /// it, as before; the `auto` stage names the GitHub archive, which is what serves such a call.
    fn audit_word(&self) -> ResearchProvider;

    /// Whether the limits answer carries the serving provider's own caps (its shorter query limit). Only the `auto` stage does; a stage of one provider
    /// answers the run's limits, as it always did.
    fn reports_provider_caps(&self) -> bool {
        false
    }

    /// The operator's key-state question: which provider would serve a search now and the version id of the key held for it. It reads through the key
    /// cache and sends nothing to any provider. `Err` is a key store that could not be read while no key from an earlier read is still held.
    fn key_state(&self) -> impl Future<Output = Result<KeyState, Fail>> + Send;
}

impl<P: Provider> Stage for P {
    type Provider = P;

    async fn serve(&self) -> Result<&P, Fail> {
        Ok(self)
    }

    fn audit_word(&self) -> ResearchProvider {
        self.id()
    }

    /// A stage of one provider that holds no key (anything but a keyed Tavily client, which `AnyStage` answers for) names it and no version.
    async fn key_state(&self) -> Result<KeyState, Fail> {
        Ok(KeyState {
            provider: self.id(),
            version_id: None,
        })
    }
}

// ---- the GitHub archive source: the NET lane ---------------------------------------------------------------------------------------------

/// `https://github.com/<owner>/<repo>` after the host and name rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubRepo {
    pub owner: String,
    pub repo: String,
}

impl GithubRepo {
    /// `https://github.com/<owner>/<repo>`
    pub fn origin(&self) -> String {
        format!("https://github.com/{}/{}", self.owner, self.repo)
    }
}

/// The compressed archive as GitHub served it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Downloaded {
    pub bytes: Vec<u8>,
    /// The host that served the bytes after the redirect (always `codeload.github.com`).
    pub final_host: String,
}

/// Downloads `https://github.com/<owner>/<repo>/archive/<ref>.tar.gz` (the default branch when `git_ref` is `None`). The real source
/// follows exactly one redirect and only to `codeload.github.com`, sends no credential, stops reading at `max_bytes` (`too_large`) and maps
/// 404 to `not_found`, 403 and 429 to `upstream_rate_limited` and everything else to `upstream_error`.
pub trait ArchiveSource: Send + Sync {
    fn download(
        &self,
        repo: &GithubRepo,
        git_ref: Option<&str>,
        max_bytes: u64,
    ) -> impl Future<Output = Result<Downloaded, Fail>> + Send;
}

// ---- stage kind -------------------------------------------------------------------------------------------------------------------------------

/// Whether the stage is the official one or a test stage. The fixture search provider runs only on a test stage: the Lambda reads the kind
/// from its environment (`HAMH_STAGE_KIND`, set by the stack from the stage configuration) and refuses `fixture` otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageKind {
    Official,
    Test,
}

impl StageKind {
    /// `official` or `test`; anything else is `None` (the Lambda then refuses to start).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "official" => Some(Self::Official),
            "test" => Some(Self::Test),
            _ => None,
        }
    }
}

// ---- the route's own ports -------------------------------------------------------------------------------------------------------

use harness_protocol::research::{ItemRef, JobState, PolicyItem, ResearchLimits, ResearchUsage};
use serde::{Deserialize, Serialize};

/// What one call adds to the run's counters. `reserve` adds all of it or none of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Delta {
    pub searches: u32,
    pub fetched_urls: u32,
    pub repos: u32,
    pub outbound_bytes: u64,
    pub context_bytes: u64,
}

/// A counter sequence: stored items (`i-0001`) and searches (`s-0001`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdKind {
    Item,
    Search,
}

/// What a repeated idempotency key finds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Idem {
    /// No answer is stored for the key.
    Fresh,
    /// The same request was answered before: the stored response body.
    Replay(String),
    /// The key was used with a different request.
    Conflict,
}

/// One asynchronous job (a repository download) in the research table: `run#<run>` / `job#<id>`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRecord {
    pub run: String,
    pub agent: String,
    pub job_id: String,
    /// SHA-256 of the request that created the job: a repeated key with other arguments is `conflict`.
    pub req_hash: String,
    pub state: JobState,
    /// Epoch seconds.
    pub created_s: u64,
    pub updated_s: u64,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<ItemRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<harness_protocol::research::ResearchError>,
}

/// The research table. Every counter write is a conditional update, so concurrent calls cannot pass a cap.
pub trait Table: Send + Sync {
    /// The run's policy row, if any.
    fn get_policy(
        &self,
        run: &str,
    ) -> impl Future<Output = Result<Option<PolicyItem>, Fail>> + Send;

    /// Adds `delta` to the run's counters only if every non-zero part stays within `limits`; the usage after. `limit_calls` for a
    /// count, `limit_bytes` for a byte budget. Nothing is added on a refusal.
    fn reserve(
        &self,
        run: &str,
        delta: &Delta,
        limits: &ResearchLimits,
    ) -> impl Future<Output = Result<ResearchUsage, Fail>> + Send;

    /// One call this minute for the run; `limit_rate` when the run already made `cap` calls in `minute` (epoch minutes).
    fn rate_tick(
        &self,
        run: &str,
        minute: u64,
        cap: u32,
    ) -> impl Future<Output = Result<(), Fail>> + Send;

    /// One upstream call this minute for the whole stage (the Tavily key's rate); `limit_rate` past `cap`.
    fn stage_tick(&self, minute: u64, cap: u32) -> impl Future<Output = Result<(), Fail>> + Send;

    /// The run's counters, all zero when nothing was used.
    fn usage(&self, run: &str) -> impl Future<Output = Result<ResearchUsage, Fail>> + Send;

    /// The next number of a sequence (1, 2, 3, ...), atomically.
    fn next_id(&self, run: &str, kind: IdKind) -> impl Future<Output = Result<u32, Fail>> + Send;

    /// The stored answer of an idempotency key (search and fetch), compared by the request hash.
    fn idem_get(
        &self,
        run: &str,
        key: &str,
        req_hash: &str,
    ) -> impl Future<Output = Result<Idem, Fail>> + Send;

    /// Stores the answer of a request under its key (kept for a day); an existing entry is left alone.
    fn idem_put(
        &self,
        run: &str,
        key: &str,
        req_hash: &str,
        response: &str,
        expires_s: u64,
    ) -> impl Future<Output = Result<(), Fail>> + Send;

    /// Marks URL hashes (64 hex) as seen by the run: search results, the prompt, links of fetched pages.
    fn seen_put(
        &self,
        run: &str,
        hashes: &[String],
        expires_s: u64,
    ) -> impl Future<Output = Result<(), Fail>> + Send;

    /// Which of the hashes the run has seen.
    fn seen_get(
        &self,
        run: &str,
        hashes: &[String],
    ) -> impl Future<Output = Result<Vec<String>, Fail>> + Send;

    /// Creates the job unless one with that id exists; `true` when created. The existing record is returned by `job_get`.
    fn job_create(&self, job: &JobRecord) -> impl Future<Output = Result<bool, Fail>> + Send;

    fn job_get(
        &self,
        run: &str,
        job_id: &str,
    ) -> impl Future<Output = Result<Option<JobRecord>, Fail>> + Send;

    /// Moves a job to `to` only if it is now in one of `from`; `true` when it moved. `item` and `error` are stored with the new state.
    #[allow(clippy::too_many_arguments)]
    fn job_move(
        &self,
        run: &str,
        job_id: &str,
        from: &[JobState],
        to: JobState,
        now_s: u64,
        item: Option<&ItemRef>,
        error: Option<&harness_protocol::research::ResearchError>,
    ) -> impl Future<Output = Result<bool, Fail>> + Send;
}

/// Whether a create-only write wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutOutcome {
    Created,
    /// The key already holds an object; nothing was written.
    Exists,
}

/// The harness bucket, as `research-api` and `research-job` use it. Objects are immutable: a write never replaces.
pub trait Blobs: Send + Sync {
    fn put_new(
        &self,
        key: &str,
        body: Vec<u8>,
        content_type: &str,
    ) -> impl Future<Output = Result<PutOutcome, Fail>> + Send;

    fn get(&self, key: &str) -> impl Future<Output = Result<Option<Vec<u8>>, Fail>> + Send;
}

/// The asynchronous start of the `research-job` function.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobEvent {
    pub run: String,
    pub agent: String,
    pub job_id: String,
}

pub trait Invoker: Send + Sync {
    /// Starts the job (an asynchronous Lambda invoke, no retry); `Ok` means the invoke was accepted.
    fn start_job(&self, event: &JobEvent) -> impl Future<Output = Result<(), Fail>> + Send;
}

/// Epoch milliseconds; a function so a test can fix the clock.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

/// The system clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }
}
