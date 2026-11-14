//! The backend's version. `build.rs` embeds
//! the root `VERSION` file (`N.NN`) as `HAMH_VERSION`; `lambda::to_response_labelled` returns the version the backend reports in `VERSION_HEADER` on every
//! answer, so `ham` shows `backend vN.NN` next to its own version.
//!
//! The version the backend reports is the compiled one, unless the stage holds a release label. A release whose deployed files are unchanged (no file under the
//! deployed paths differs) deploys nothing, so no control-api is built for it; setup writes the stage's parameter `/hamh/<stage>/release-version`
//! (`N.NN <source tree>`) alone, and [`reported_version`] reports it when it is a valid version, no older than the compiled one and bound to the source tree the
//! stage runs. [`ReleaseLabel`] reads it with a short cache and a timeout; any state it cannot use reports the compiled version.

use std::future::Future;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The version compiled into this build: the content of the root `VERSION` file when this crate was built. It is the floor of the reported version.
pub const VERSION: &str = env!("HAMH_VERSION");

/// The response header that carries the reported version on every answer of the `/v1` API, whatever the status.
pub const VERSION_HEADER: &str = "x-hamh-backend-version";

/// The earlier name of [`VERSION_HEADER`].
pub const LEGACY_VERSION_HEADER: &str = "x-cah-backend-version"; // COMPAT-CAH

/// How long a label that was read is kept before the next read.
pub const LABEL_TTL: Duration = Duration::from_secs(30);
/// How long the compiled version, reported because the label could not be used or read, is kept before the next read.
pub const FALLBACK_TTL: Duration = Duration::from_secs(10);
/// How long a read of the parameters may take: a slower store reports the compiled version and never holds an answer back.
pub const READ_TIMEOUT: Duration = Duration::from_millis(500);

/// `N.NN` as numbers: digits, a dot and exactly two digits. Anything else (a sign, a prefix, another counter width) is `None`.
fn pair(text: &str) -> Option<(u32, u32)> {
    let (major, counter) = text.split_once('.')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(major) || counter.len() != 2 || !digits(counter) {
        return None;
    }
    Some((major.parse().ok()?, counter.parse().ok()?))
}

/// The label parameter as numbers and text: `N.NN`, one space and a 64-digit lower-case hexadecimal source tree, with white space only around them. That is the form
/// `scripts/lib/deploy.py` writes and its relabel reads (`fixtures/release-label/cases.json` holds both sides to it); `None` for any other value.
fn parse_label(label: &str) -> Option<((u32, u32), &str)> {
    let (version, tree) = label.trim().split_once(' ')?;
    let is_tree = tree.len() == 64 && tree.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    is_tree.then_some(())?;
    Some((pair(version)?, tree))
}

/// Whether the label names the source tree the stage runs now: a label of this deploy, whatever its version.
fn label_is_current(label: Option<&str>, live_tree: Option<&str>) -> bool {
    match (label.and_then(parse_label), live_tree) {
        (Some((_, tree)), Some(live)) => tree == live.trim(),
        _ => false,
    }
}

/// The version to report from the stage's label parameter (`N.NN <source tree>`), the stage's live `deployed-source-tree` parameter and the compiled version.
/// The label counts when it is exactly a version and a 64-digit source tree, the tree is the one the stage runs and the version is no older than the compiled
/// one; the numbers are compared as numbers. In every other state the compiled version is the answer, so a stale or foreign label can only be ignored.
pub fn reported_version(label: Option<&str>, live_tree: Option<&str>, compiled: &str) -> String {
    if label_is_current(label, live_tree)
        && let (Some((v, _)), Some(c)) = (label.and_then(parse_label), pair(compiled))
        && v >= c
    {
        return label
            .and_then(|l| l.split_ascii_whitespace().next())
            .unwrap_or(compiled)
            .to_string();
    }
    compiled.to_string()
}

/// What the two parameters hold: the label and the source tree the stage runs (`None` for a parameter that does not exist).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LabelParameters {
    pub label: Option<String>,
    pub tree: Option<String>,
}

/// Where the parameters are read from: the stage's parameter store in the Lambda, a scripted answer in the tests. The error is a sentence for the log.
pub trait LabelStore: Send + Sync {
    fn read(&self) -> impl Future<Output = Result<LabelParameters, String>> + Send;
}

/// The version the backend reports now: [`reported_version`] over what the store holds, read at most once per interval and never waited for longer than the
/// timeout. A read that fails or times out reports the compiled version, kept for the short fallback interval, so a store that is down is asked again soon and
/// never turns every answer into a wait.
pub struct ReleaseLabel<S> {
    store: S,
    ttl: Duration,
    fallback_ttl: Duration,
    timeout: Duration,
    kept: Mutex<Option<(Instant, Duration, String)>>,
}

impl<S> std::fmt::Debug for ReleaseLabel<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReleaseLabel")
            .field("ttl", &self.ttl)
            .field("fallback_ttl", &self.fallback_ttl)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl<S: LabelStore> ReleaseLabel<S> {
    pub fn new(store: S) -> Self {
        Self::with_times(store, LABEL_TTL, FALLBACK_TTL, READ_TIMEOUT)
    }

    pub fn with_times(store: S, ttl: Duration, fallback_ttl: Duration, timeout: Duration) -> Self {
        Self {
            store,
            ttl,
            fallback_ttl,
            timeout,
            kept: Mutex::new(None),
        }
    }

    /// The version to put in the header of the answer being built.
    pub async fn current(&self) -> String {
        if let Some((at, ttl, version)) = self.kept.lock().ok().and_then(|k| k.clone())
            && at.elapsed() < ttl
        {
            return version;
        }
        let (version, ttl) = match tokio::time::timeout(self.timeout, self.store.read()).await {
            Ok(Ok(p)) => {
                let version = reported_version(p.label.as_deref(), p.tree.as_deref(), VERSION);
                // a label of this deploy (its version is the compiled one in the steady state) is kept for the whole interval; no label, a label of another
                // deploy or one that is not a version is looked at again soon, because a deploy or a relabel may write it any moment
                let ttl = if label_is_current(p.label.as_deref(), p.tree.as_deref()) {
                    self.ttl
                } else {
                    self.fallback_ttl
                };
                (version, ttl)
            }
            Ok(Err(e)) => {
                println!(
                    "{}",
                    serde_json::json!({ "diagnostic": format!("release label not read: {e}") })
                );
                (VERSION.to_string(), self.fallback_ttl)
            }
            Err(_) => {
                println!(
                    "{}",
                    serde_json::json!({ "diagnostic": format!("release label not read: no answer within {} ms", self.timeout.as_millis()) })
                );
                (VERSION.to_string(), self.fallback_ttl)
            }
        };
        if let Ok(mut kept) = self.kept.lock() {
            *kept = Some((Instant::now(), ttl, version.clone()));
        }
        version
    }
}
