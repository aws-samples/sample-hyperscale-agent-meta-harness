//! The Tavily key: how `research-api` holds, caches and re-reads it.
//!
//! The key lives in a Secrets Manager secret that only the `research-api` role can read. This module reads it, keeps it in memory for
//! five minutes (and, while the store is down, for at most one hour after the last good read), and re-reads it at most once every 30
//! seconds after Tavily refuses it (a rotation). There is no timer: an expired key is replaced or let go at the next access. It prints as
//! `<redacted>` through `Debug` and `Display`, its memory is wiped when the last holder drops it, and the only place its text is exposed
//! is the `Authorization` header of the Tavily client (`expose` is crate-private). Rust cannot promise that no copy remains in the
//! allocator or the SDK's own buffers; the aim is that the key is in no log line, no error text, no audit object and no response.
//!
//! The store also tells when the version it read was created ([`StoredKey::stored_at_ms`], the `CreatedDate` of `GetSecretValue`, which needs no
//! permission beyond the one that reads the key). The cache keeps that time as a number and hands it on once per read ([`CachedSecret::take_load`]),
//! so the Tavily client, outside this module, can report the age of the key. This module reports nothing itself.

use crate::ports::Fail;
use aws_sdk_secretsmanager::Client;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use tokio::sync::Mutex;
use zeroize::Zeroize;

/// A key. `Debug` and `Display` print `<redacted>`; the memory is overwritten on drop.
pub struct SecretString {
    value: String,
}

impl SecretString {
    pub fn new(value: String) -> Self {
        Self { value }
    }

    /// The text, for the one place that builds the `Authorization` header.
    pub(crate) fn expose(&self) -> &str {
        &self.value
    }

    /// Whether the secret equals `other`, compared without stopping at the first difference.
    pub fn matches(&self, other: &str) -> bool {
        let (a, b) = (self.value.as_bytes(), other.as_bytes());
        a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }
}

impl Clone for SecretString {
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
        }
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.value.zeroize();
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// A key as the store holds it, with the time its version was stored.
pub struct StoredKey {
    pub secret: SecretString,
    /// The creation time of the version that was read, in epoch milliseconds, when the store says it (`CreatedDate` of `GetSecretValue`).
    pub stored_at_ms: Option<u64>,
    /// The version id of the version that was read, when the store says it (`VersionId` of `GetSecretValue`). Secrets Manager chooses it; it is not derived
    /// from the key.
    pub version_id: Option<String>,
}

impl StoredKey {
    pub fn new(secret: SecretString, stored_at_ms: Option<u64>) -> Self {
        Self {
            secret,
            stored_at_ms,
            version_id: None,
        }
    }

    /// The same key with the version id the store gave it.
    pub fn with_version_id(mut self, version_id: String) -> Self {
        self.version_id = Some(version_id);
        self
    }

    /// A key whose version time is not known.
    pub fn undated(secret: SecretString) -> Self {
        Self::new(secret, None)
    }
}

impl fmt::Debug for StoredKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredKey")
            .field("secret", &"<redacted>")
            .field("stored_at_ms", &self.stored_at_ms)
            .field("version_id", &self.version_id)
            .finish()
    }
}

/// Where the key is read from. `None` means no key is stored (a secret with no current version, or a blank value).
pub trait SecretSource: Send + Sync {
    fn get(&self) -> impl Future<Output = Result<Option<StoredKey>, Fail>> + Send;
}

/// Milliseconds on a clock that only moves forward; injectable so the cache tests do not sleep.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

impl<T: Clock + ?Sized> Clock for Arc<T> {
    fn now_ms(&self) -> u64 {
        (**self).now_ms()
    }
}

/// The process's monotonic clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        static START: OnceLock<Instant> = OnceLock::new();
        u64::try_from(START.get_or_init(Instant::now).elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// How long a key that was found is kept.
const TTL_MS: u64 = 300_000;
/// How long "no key stored" is remembered: a key the owner stores becomes usable within this time.
const NO_KEY_TTL_MS: u64 = 30_000;
/// The least time between two re-reads after an authentication failure.
const REREAD_MS: u64 = 30_000;
/// How long a key that was read is still served while the store is down: one hour after the last good read, then the failure is
/// returned and the key is let go.
const STALE_MAX_MS: u64 = 3_600_000;

#[derive(Default)]
struct State {
    value: Option<Arc<SecretString>>,
    read_at_ms: Option<u64>,
    /// The time of the last read that succeeded (a key or "no key"), which the one-hour limit counts from.
    last_ok_ms: Option<u64>,
    last_reread_ms: Option<u64>,
    /// The creation time of the version a read found, until [`CachedSecret::take_load`] hands it on.
    unreported_stored_at_ms: Option<u64>,
    /// The version id of the key in `value`, as the read that found it gave it. It stays with the key: a key served while the store is down keeps its id.
    held_version_id: Option<String>,
}

/// A secret source behind a cache. One read at a time: concurrent callers wait for the read in progress.
pub struct CachedSecret<S, C = SystemClock> {
    source: S,
    clock: C,
    state: Mutex<State>,
}

impl<S: SecretSource> CachedSecret<S, SystemClock> {
    pub fn new(source: S) -> Self {
        Self::with_clock(source, SystemClock)
    }
}

impl<S: SecretSource, C: Clock> CachedSecret<S, C> {
    pub fn with_clock(source: S, clock: C) -> Self {
        Self {
            source,
            clock,
            state: Mutex::new(State::default()),
        }
    }

    /// The key, from the cache while it is fresh (five minutes). An expired key is replaced by the next read, or let go when the store
    /// says there is none. When the source fails, the last key that was read is served for at most one hour after that read (and the
    /// source is tried again after 30 seconds); after the hour, or with no key held, the failure is returned and nothing is kept.
    pub async fn get(&self) -> Result<Option<Arc<SecretString>>, Fail> {
        let mut state = self.state.lock().await;
        let now = self.clock.now_ms();
        if let Some(at) = state.read_at_ms {
            let ttl = if state.value.is_some() {
                TTL_MS
            } else {
                NO_KEY_TTL_MS
            };
            // A value served while the store is down counts as fresh for 30 seconds at a time; it is never served past the hour.
            let within_hour = state
                .last_ok_ms
                .is_none_or(|ok| now.saturating_sub(ok) < STALE_MAX_MS);
            if now.saturating_sub(at) < ttl && within_hour {
                return Ok(state.value.clone());
            }
        }
        match self.source.get().await {
            Ok(found) => {
                state.unreported_stored_at_ms = found.as_ref().and_then(|k| k.stored_at_ms);
                state.held_version_id = found.as_ref().and_then(|k| k.version_id.clone());
                state.value = found.map(|k| Arc::new(k.secret));
                state.read_at_ms = Some(now);
                state.last_ok_ms = Some(now);
                Ok(state.value.clone())
            }
            Err(fail) => {
                let young_enough = state
                    .last_ok_ms
                    .is_some_and(|ok| now.saturating_sub(ok) < STALE_MAX_MS);
                match state.value.clone() {
                    Some(old) if young_enough => {
                        state.read_at_ms = Some(now.saturating_sub(TTL_MS - NO_KEY_TTL_MS));
                        Ok(Some(old))
                    }
                    _ => {
                        // Too old to serve: the cache lets go of the key.
                        state.value = None;
                        state.held_version_id = None;
                        state.read_at_ms = None;
                        state.last_ok_ms = None;
                        Err(fail)
                    }
                }
            }
        }
    }

    /// Reads the secret again because Tavily refused the key (it may have been rotated). Returns whether it read: at most once every 30
    /// seconds, and `false` inside that window. The newly read value is what [`Self::get`] returns afterwards; a failed read keeps the
    /// old value and returns the failure.
    pub async fn reread_after_auth_failure(&self) -> Result<bool, Fail> {
        let mut state = self.state.lock().await;
        let now = self.clock.now_ms();
        if let Some(last) = state.last_reread_ms
            && now.saturating_sub(last) < REREAD_MS
        {
            return Ok(false);
        }
        state.last_reread_ms = Some(now);
        let found = self.source.get().await?;
        state.unreported_stored_at_ms = found.as_ref().and_then(|k| k.stored_at_ms);
        state.held_version_id = found.as_ref().and_then(|k| k.version_id.clone());
        state.value = found.map(|k| Arc::new(k.secret));
        state.read_at_ms = Some(now);
        state.last_ok_ms = Some(now);
        Ok(true)
    }

    /// The version id of the key held now, after the same refresh [`Self::get`] does: the cached key within five minutes of its read, a new read
    /// after that. `None` when no key is held or the store gave no id. It leaves the creation time for [`Self::take_load`] alone.
    pub async fn version_now(&self) -> Result<Option<String>, Fail> {
        if self.get().await?.is_none() {
            return Ok(None);
        }
        Ok(self.state.lock().await.held_version_id.clone())
    }
}

impl<S, C> CachedSecret<S, C> {
    /// The creation time (epoch milliseconds) of the key version the last read found, once: the next call returns `None` until another read
    /// finds a key. `None` also when the store gave no creation time or no key is stored. A read from the store is a cache refresh or the re-read
    /// after a refusal; a key served from the cache, or kept while the store is down, is no new read. It holds a number, never the key.
    pub async fn take_load(&self) -> Option<u64> {
        self.state.lock().await.unreported_stored_at_ms.take()
    }
}

impl<S, C> fmt::Debug for CachedSecret<S, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CachedSecret { <redacted> }")
    }
}

/// The key in Secrets Manager: the `AWSCURRENT` version of one named secret.
#[derive(Clone, Debug)]
pub struct AwsSecretSource {
    client: Client,
    secret_id: String,
}

impl AwsSecretSource {
    pub fn new(client: Client, secret_id: String) -> Self {
        Self { client, secret_id }
    }
}

impl SecretSource for AwsSecretSource {
    async fn get(&self) -> Result<Option<StoredKey>, Fail> {
        let answer = self
            .client
            .get_secret_value()
            .secret_id(&self.secret_id)
            .version_stage("AWSCURRENT")
            .send()
            .await;
        match answer {
            Ok(out) => {
                // `CreatedDate` is when this version was created (Unix seconds, with a fraction when Secrets Manager gives one). The same call
                // returns it with the value, so no other permission is needed.
                let stored_at_ms = out.created_date().and_then(|d| {
                    let ms = d
                        .secs()
                        .checked_mul(1000)?
                        .checked_add(i64::from(d.subsec_nanos() / 1_000_000))?;
                    u64::try_from(ms).ok()
                });
                Ok(out
                    .secret_string()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| {
                        let key = StoredKey::new(SecretString::new(s.to_string()), stored_at_ms);
                        match out.version_id() {
                            Some(id) => key.with_version_id(id.to_string()),
                            None => key,
                        }
                    }))
            }
            // A secret created empty has no AWSCURRENT version, which Secrets Manager reports as not found.
            Err(e)
                if e.as_service_error()
                    .is_some_and(|se| se.is_resource_not_found_exception()) =>
            {
                Ok(None)
            }
            // Nothing from the service's answer is kept.
            Err(_) => Err(Fail::upstream("the key store did not answer")),
        }
    }
}
