//! The secret values of one run, as a set that grows while the run goes on. It starts with what the invocation carried (the session's
//! secret key and token, the refresh nonce, the package token) and gains every secret key, session token and nonce a `/internal/creds`
//! refresh mints after that ([`crate::creds::CredStore::watch_secrets`] adds them the moment they exist, so there is no window in which
//! the engine could print a value agentd does not know). Nothing is ever removed: the engine is given the current credentials by design,
//! so a model can print a value the invocation never held, and one it printed before a rotation is still a secret afterwards.
//!
//! The set is shared by handle (cheap to clone): the collector, the recorder and the end sequence read it at the moment they redact
//! ([`SecretSet::redact`]), so every reader sees what every refresh so far minted. The pure replacement stays
//! [`crate::direct::classify::redact_known`]; this type only holds the values.

use crate::direct::classify::redact_known;
use std::sync::{Arc, Mutex};

/// The growing set of exact secret values of a run. `Default` is the empty set.
#[derive(Clone, Default)]
pub struct SecretSet(Arc<Mutex<Vec<String>>>);

impl std::fmt::Debug for SecretSet {
    /// The count only; a value is never printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretSet")
            .field("values", &self.snapshot().len())
            .finish()
    }
}

impl SecretSet {
    /// A set holding `values` (empty values and repeats left out).
    pub fn new(values: Vec<String>) -> Self {
        let set = Self::default();
        set.add(&values);
        set
    }

    /// Adds `more` to the set. An empty value and a value already in the set are skipped; nothing is removed.
    pub fn add(&self, more: &[String]) {
        let mut values = self.0.lock().unwrap_or_else(|p| p.into_inner());
        for v in more {
            if !v.is_empty() && !values.iter().any(|have| have == v) {
                values.push(v.clone());
            }
        }
    }

    /// The values in the set now, in the order they were added.
    pub fn snapshot(&self) -> Vec<String> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// How many values the set holds now.
    pub fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// True when the set holds no value.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `value` with every string in it (members, elements, keys left alone) passed through [`SecretSet::redact`].
    pub fn redact_json(&self, value: &serde_json::Value) -> serde_json::Value {
        use serde_json::Value;
        let values = self.snapshot();
        fn walk(v: &Value, values: &[String]) -> Value {
            match v {
                Value::String(s) => Value::String(redact_known(s, values)),
                Value::Array(a) => Value::Array(a.iter().map(|x| walk(x, values)).collect()),
                Value::Object(m) => Value::Object(
                    m.iter()
                        .map(|(k, x)| (k.clone(), walk(x, values)))
                        .collect(),
                ),
                other => other.clone(),
            }
        }
        walk(value, &values)
    }

    /// `text` with every value in the set now replaced by `<redacted>` ([`redact_known`]: a value under 8 characters is ignored, it
    /// would eat ordinary words).
    pub fn redact(&self, text: &str) -> String {
        redact_known(text, &self.snapshot())
    }
}

#[cfg(test)]
mod tests {
    use super::SecretSet;

    #[test]
    fn a_set_grows_skips_repeats_and_empties_and_prints_only_its_count() {
        let set = SecretSet::new(vec!["first-secret-value".into(), String::new()]);
        assert_eq!(set.snapshot(), ["first-secret-value"]);
        set.add(&["first-secret-value".into(), "second-secret-value".into()]);
        set.add(&[]);
        assert_eq!(
            set.snapshot(),
            ["first-secret-value", "second-secret-value"]
        );
        let shared = set.clone();
        shared.add(&["third-secret-value".into()]);
        assert_eq!(set.snapshot().len(), 3, "a clone is the same set");
        assert_eq!(
            set.redact("first-secret-value and third-secret-value, short"),
            "<redacted> and <redacted>, short"
        );
        assert_eq!(format!("{set:?}"), "SecretSet { values: 3 }");
    }
}
