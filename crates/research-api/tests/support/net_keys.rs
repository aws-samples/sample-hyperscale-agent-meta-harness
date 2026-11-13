//! Stand-ins for the key source and the clock in the Tavily and provider tests. Every key value is built at run time.

#![allow(dead_code, clippy::unwrap_used)]

use research_api::ports::Fail;
use research_api::secret::{Clock, SecretSource, SecretString, StoredKey};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub fn key(tag: &str) -> String {
    ["stand", "-in-key-", tag, "-0123456789abcdef"].concat()
}

#[derive(Default)]
pub struct FakeClock(AtomicU64);

impl FakeClock {
    pub fn advance_s(&self, s: u64) {
        self.0.fetch_add(s * 1000, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

type Answer = Result<Option<(String, Option<u64>)>, Fail>;

/// Answers one scripted value per read and counts the reads; the last answer repeats.
#[derive(Clone, Default)]
pub struct Script {
    answers: Arc<Mutex<VecDeque<Answer>>>,
    last: Arc<Mutex<Option<Answer>>>,
    reads: Arc<AtomicU64>,
}

impl Script {
    /// One answer per read, none of them dated.
    pub fn with(answers: Vec<Result<Option<String>, Fail>>) -> Self {
        Self {
            answers: Arc::new(Mutex::new(
                answers
                    .into_iter()
                    .map(|a| a.map(|v| v.map(|k| (k, None))))
                    .collect(),
            )),
            ..Self::default()
        }
    }
    pub fn keys(keys: &[&str]) -> Self {
        Self::with(keys.iter().map(|k| Ok(Some(k.to_string()))).collect())
    }
    /// One key per read, each with the epoch milliseconds its version was stored at.
    pub fn dated(keys: &[(&str, u64)]) -> Self {
        Self {
            answers: Arc::new(Mutex::new(
                keys.iter()
                    .map(|(k, at)| Ok(Some((k.to_string(), Some(*at)))))
                    .collect(),
            )),
            ..Self::default()
        }
    }
    pub fn reads(&self) -> u64 {
        self.reads.load(Ordering::SeqCst)
    }
}

impl SecretSource for Script {
    async fn get(&self) -> Result<Option<StoredKey>, Fail> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let next = self.answers.lock().unwrap().pop_front();
        let answer = match next {
            Some(a) => {
                *self.last.lock().unwrap() = Some(a.clone());
                a
            }
            None => self
                .last
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| Err(Fail::internal("script is empty"))),
        };
        answer.map(|v| v.map(|(k, at)| StoredKey::new(SecretString::new(k), at)))
    }
}
