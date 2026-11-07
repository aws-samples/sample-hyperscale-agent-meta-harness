//! The Tavily client: web search and page extraction for `research-api`.
//!
//! Two modes, chosen by construction and never by a fallback: **keyed** (the key from [`CachedSecret`], sent as `Authorization: Bearer`,
//! the only place the key is ever put in a request) and **keyless** (the header `X-Tavily-Access-Mode: keyless`, no credential; free
//! and rate limited, for an explicit first wiring check). A keyed client with no key stored fails with `no_key` and sends nothing.
//!
//! The error mapping is the `Provider` contract of `crate::ports`: a refused key (401 or 403) re-reads the secret once and retries only
//! with a different key, else `no_key`; 429 is `upstream_rate_limited` with Tavily's `retry-after` (at most an hour); plan limits, other
//! statuses, timeouts and unreadable answers are `upstream_error`. An answer whose raw body repeats the key is discarded unread with a
//! fixed `upstream_error` (the body of a 401 or 403 is never read, so those two are exempt). Any other error message carries the status
//! and an excerpt of Tavily's body of at most 200 characters, cleaned of control characters, with every occurrence of the key replaced by
//! `<redacted>`. This module writes no output of its own: it reports to an injected sink ([`crate::metrics`]) one event for an answer that refused the
//! key (401 or 403), limited the rate (429) or reached a usage limit (432 or 433), and the creation time of each key version it reads, so the operator
//! can see the age of the key. An event holds a status class or a time, never the key, a header, a body or an address, so there is no line for the
//! key to reach. 432 is Tavily's plan limit and 433 its pay-as-you-go limit (Tavily's help article "Understanding HTTP errors"). Only a keyed client
//! reports; a keyless client has no key to report on.

use crate::http::{HttpClient, HttpError, HttpRequest, HttpResponse};
use crate::metrics::{Event, Metrics, NoMetrics};
use crate::ports::{
    ExtractFailure, ExtractOutcome, ExtractedPage, Fail, KeyProbe, Provider, SearchOutcome,
};
use crate::scan::{clean_text, excerpt};
use crate::secret::{CachedSecret, Clock, SecretSource, SecretString, SystemClock};
use harness_protocol::research::{
    ResearchErrorCode, ResearchProvider, SearchDepth, SearchRequest, SearchResult,
};
use serde_json::{Value, json};
use std::fmt;
use std::sync::Arc;

/// Results when the request names no count.
const DEFAULT_RESULTS: u8 = 5;
/// The seconds Tavily may spend extracting a page.
const EXTRACT_TIMEOUT_S: u32 = 15;
/// The longest part of Tavily's body that an error message carries.
const ERROR_EXCERPT_CHARS: usize = 200;
/// The longest `retry-after` that is passed on.
const MAX_RETRY_AFTER_S: u64 = 3600;

enum Auth<S, C> {
    Keyed(CachedSecret<S, C>),
    Keyless,
}

/// A Tavily client over one [`HttpClient`] (its timeouts and response cap are the caller's).
pub struct TavilyClient<S, C = SystemClock> {
    http: HttpClient,
    /// `https://api.tavily.com`, without a trailing slash.
    base: String,
    auth: Auth<S, C>,
    /// Where the key's health is reported (keyed mode only); nothing by default.
    metrics: Arc<dyn Metrics>,
}

impl<S, C> fmt::Debug for TavilyClient<S, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mode = match self.auth {
            Auth::Keyed(_) => "keyed",
            Auth::Keyless => "keyless",
        };
        f.debug_struct("TavilyClient")
            .field("mode", &mode)
            .finish_non_exhaustive()
    }
}

impl<S, C> TavilyClient<S, C> {
    /// The keyed client: `secret` holds the stage's key.
    pub fn keyed(http: HttpClient, base_url: String, secret: CachedSecret<S, C>) -> Self {
        Self {
            http,
            base: base_url.trim_end_matches('/').to_string(),
            auth: Auth::Keyed(secret),
            metrics: Arc::new(NoMetrics),
        }
    }

    /// The keyless client (an explicit stage setting; it sends no credential and never reads a secret).
    pub fn keyless(http: HttpClient, base_url: String) -> Self {
        Self {
            http,
            base: base_url.trim_end_matches('/').to_string(),
            auth: Auth::Keyless,
            metrics: Arc::new(NoMetrics),
        }
    }

    /// Reports the key's health to `metrics` (a keyed client only; a keyless client has no key to report on). The default reports nothing.
    pub fn with_metrics(mut self, metrics: Arc<dyn Metrics>) -> Self {
        self.metrics = metrics;
        self
    }
}

impl<S: SecretSource, C: Clock> TavilyClient<S, C> {
    /// The key for one attempt: `None` in keyless mode; a keyed client without a stored key fails here, before anything is sent.
    async fn key(&self) -> Result<Option<Arc<SecretString>>, Fail> {
        match &self.auth {
            Auth::Keyless => Ok(None),
            Auth::Keyed(secret) => match secret.get().await? {
                Some(key) => {
                    self.report_load(secret).await;
                    Ok(Some(key))
                }
                None => Err(Fail::new(
                    ResearchErrorCode::NoKey,
                    "no Tavily key is stored in this stage",
                )),
            },
        }
    }

    /// Tells the sink when the key version the cache read since the last report was created (a refresh of the cache or a re-read).
    async fn report_load(&self, secret: &CachedSecret<S, C>) {
        if let Some(stored_at_ms) = secret.take_load().await {
            self.metrics.record(Event::KeyLoaded { stored_at_ms });
        }
    }

    /// One event for an answer by its status. Only a keyed client reports; the status alone decides, whatever the body holds.
    fn report_status(&self, status: u16) {
        if matches!(self.auth, Auth::Keyless) {
            return;
        }
        let event = match status {
            401 | 403 => Event::KeyRefused,
            429 => Event::RateLimited,
            // 432 is the usage limit of the plan (or of the key); 433 is the pay-as-you-go limit.
            432 | 433 => Event::PlanLimit,
            _ => return,
        };
        self.metrics.record(event);
    }

    async fn post(
        &self,
        path: &str,
        payload: &[u8],
        key: Option<&SecretString>,
    ) -> Result<HttpResponse, Fail> {
        let mut headers = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("accept".to_string(), "application/json".to_string()),
        ];
        match key {
            Some(key) => headers.push((
                "authorization".to_string(),
                format!("Bearer {}", key.expose()),
            )),
            None => headers.push(("x-tavily-access-mode".to_string(), "keyless".to_string())),
        }
        let request = HttpRequest {
            method: http::Method::POST,
            url: format!("{}{path}", self.base),
            headers,
            body: payload.to_vec(),
        };
        self.http.send(request).await.map_err(map_http)
    }

    /// One call with the refusal rule: a 401 or 403 re-reads the secret once and retries only when the key changed.
    async fn call(&self, path: &str, body: &Value) -> Result<Value, Fail> {
        let payload = serde_json::to_vec(body)
            .map_err(|_| Fail::internal("the request could not be encoded"))?;
        let mut retried = false;
        loop {
            let key = self.key().await?;
            let response = self.post(path, &payload, key.as_deref()).await?;
            self.report_status(response.status);
            // An answer that repeats the key is not parsed, copied or excerpted. The body of a 401 or 403 is never read, so it passes on
            // to the re-read below.
            if !matches!(response.status, 401 | 403) && repeats_key(&response, key.as_deref()) {
                return Err(Fail::upstream(
                    "Tavily's answer repeats the key and was discarded",
                ));
            }
            match response.status {
                200..=299 => {
                    return serde_json::from_slice(&response.body)
                        .map_err(|_| Fail::upstream("Tavily's answer could not be read"));
                }
                401 | 403 => match &self.auth {
                    Auth::Keyless => {
                        return Err(Fail::upstream("Tavily refused keyless access"));
                    }
                    Auth::Keyed(secret) => {
                        if !retried {
                            retried = true;
                            // Whether this call read the secret or the window was open because another caller just did, the key
                            // cached now is the one to compare with the key that was refused.
                            secret.reread_after_auth_failure().await?;
                            self.report_load(secret).await;
                            if let (Some(old), Some(fresh)) = (key.as_deref(), secret.get().await?)
                                && !fresh.matches(old.expose())
                            {
                                continue;
                            }
                        }
                        return Err(Fail::new(
                            ResearchErrorCode::NoKey,
                            "Tavily refused the stored key",
                        ));
                    }
                },
                429 => return Err(rate_limited(&response)),
                status => {
                    return Err(Fail::upstream(format!(
                        "Tavily answered {status}: {}",
                        body_excerpt(&response, key.as_deref())
                    )));
                }
            }
        }
    }
}

/// Whether the raw body of `response` holds the text of `key`.
fn repeats_key(response: &HttpResponse, key: Option<&SecretString>) -> bool {
    let Some(key) = key else { return false };
    let needle = key.expose().as_bytes();
    !needle.is_empty() && response.body.windows(needle.len()).any(|w| w == needle)
}

fn map_http(e: HttpError) -> Fail {
    match e {
        HttpError::Timeout => Fail::upstream("Tavily did not answer in time"),
        HttpError::TooLarge => Fail::upstream("Tavily's answer was too large"),
        HttpError::InvalidRequest(_) => Fail::internal("the Tavily request could not be built"),
        other => Fail::upstream(format!("Tavily could not be reached ({})", other.class())),
    }
}

fn rate_limited(response: &HttpResponse) -> Fail {
    let fail = Fail::new(
        ResearchErrorCode::UpstreamRateLimited,
        "Tavily is limiting the rate of calls",
    );
    match response
        .header("retry-after")
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        Some(seconds) => {
            fail.with_retry_after(u32::try_from(seconds.min(MAX_RETRY_AFTER_S)).unwrap_or(3600))
        }
        None => fail,
    }
}

/// A short piece of an error body for a message: the key replaced, control characters removed, one line, at most 200 characters.
fn body_excerpt(response: &HttpResponse, key: Option<&SecretString>) -> String {
    let mut text = String::from_utf8_lossy(&response.body).into_owned();
    if let Some(key) = key
        && !key.expose().is_empty()
    {
        text = text.replace(key.expose(), "<redacted>");
    }
    let one_line = clean_text(&text).replace(['\n', '\t'], " ");
    excerpt(&one_line, ERROR_EXCERPT_CHARS)
}

fn credits_of(answer: &Value) -> Option<u32> {
    let c = &answer["usage"]["credits"];
    c.as_u64()
        .or_else(|| c.as_f64().map(|f| f.max(0.0).ceil() as u64))
        .and_then(|n| u32::try_from(n).ok())
}

fn request_id_of(answer: &Value) -> Option<String> {
    answer["request_id"].as_str().map(str::to_string)
}

impl<S: SecretSource, C: Clock> Provider for TavilyClient<S, C> {
    fn id(&self) -> ResearchProvider {
        match self.auth {
            Auth::Keyed(_) => ResearchProvider::Tavily,
            Auth::Keyless => ResearchProvider::TavilyKeyless,
        }
    }

    async fn search(&self, req: &SearchRequest) -> Result<SearchOutcome, Fail> {
        let max = req.max_results.unwrap_or(DEFAULT_RESULTS);
        let depth = req.depth.unwrap_or(SearchDepth::Basic);
        let mut body = json!({
            "query": req.query,
            "max_results": max,
            "search_depth": depth,
            "include_answer": false,
            "include_raw_content": false,
        });
        if !req.include_domains.is_empty() {
            body["include_domains"] = json!(req.include_domains);
        }
        if let Some(range) = req.time_range {
            body["time_range"] = json!(range);
        }
        let answer = self.call("/search", &body).await?;
        let results: Vec<SearchResult> = answer["results"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|r| {
                        Some(SearchResult {
                            title: r["title"].as_str().unwrap_or("").to_string(),
                            url: r["url"].as_str()?.to_string(),
                            // `content` only: the page text Tavily may also send (`raw_content`) is never used.
                            snippet: r["content"].as_str().unwrap_or("").to_string(),
                            published: r["published_date"].as_str().map(str::to_string),
                            score: r["score"].as_f64(),
                        })
                    })
                    .take(usize::from(max))
                    .collect()
            })
            .unwrap_or_default();
        Ok(SearchOutcome {
            results,
            credits: credits_of(&answer).or(Some(match depth {
                SearchDepth::Basic => 1,
                SearchDepth::Advanced => 2,
            })),
            upstream_request_id: request_id_of(&answer),
        })
    }

    async fn extract(&self, urls: &[String], query: Option<&str>) -> Result<ExtractOutcome, Fail> {
        let mut body = json!({
            "urls": urls,
            "extract_depth": "basic",
            "format": "text",
            "timeout": EXTRACT_TIMEOUT_S,
        });
        if let Some(q) = query {
            body["query"] = json!(q);
        }
        let answer = self.call("/extract", &body).await?;
        let mut pages = Vec::new();
        let mut failed: Vec<ExtractFailure> = answer["failed_results"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|f| {
                        Some(ExtractFailure {
                            url: f["url"].as_str()?.to_string(),
                            message: excerpt(
                                &clean_text(f["error"].as_str().unwrap_or("extraction failed")),
                                ERROR_EXCERPT_CHARS,
                            ),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut empty = Vec::new();
        for r in answer["results"].as_array().into_iter().flatten() {
            let Some(url) = r["url"].as_str() else {
                continue;
            };
            match r["raw_content"].as_str().filter(|t| !t.trim().is_empty()) {
                Some(text) => pages.push(ExtractedPage {
                    url: url.to_string(),
                    text: text.to_string(),
                }),
                None => empty.push(ExtractFailure {
                    url: url.to_string(),
                    message: "the page has no text".to_string(),
                }),
            }
        }
        failed.extend(empty);
        let credits = credits_of(&answer).or_else(|| u32::try_from(pages.len().div_ceil(5)).ok());
        Ok(ExtractOutcome {
            pages,
            failed,
            credits,
            upstream_request_id: request_id_of(&answer),
        })
    }
}

/// Whether a key is stored now, from the key cache (the one the calls authenticate with). A keyless client has no key and reads nothing. The `auto` stage
/// asks this to choose between Tavily and AgentCore before it spends anything.
impl<S: SecretSource, C: Clock> KeyProbe for TavilyClient<S, C> {
    async fn key_stored(&self) -> Result<bool, Fail> {
        match &self.auth {
            Auth::Keyless => Ok(false),
            Auth::Keyed(secret) => Ok(secret.get().await?.is_some()),
        }
    }

    /// The version id of the key in the cache, for the operator's key-state question. A keyless client has none.
    async fn key_version(&self) -> Result<Option<String>, Fail> {
        match &self.auth {
            Auth::Keyless => Ok(None),
            Auth::Keyed(secret) => secret.version_now().await,
        }
    }
}
