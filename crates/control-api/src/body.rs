//! The one place a request body is parsed.
//!
//! serde's error text quotes what the caller sent: a wrong-typed value, an unknown member name, an unknown enum word and the words of a
//! custom `Deserialize` (a repository target prints the target it refused). A person can paste a token into a setting by mistake, and the
//! 400 travels through proxies, clients and logs. So a body that does not parse earns a 400 `bad-request` whose message is built from
//! three things only: the request type the handler names, one fixed phrase for the kind of problem, and the place serde_json reports
//! (line and column). No byte of serde's text and none of the body reaches the answer.
//!
//! The kind of problem comes from `serde_json::Error::classify` and, for a data error, from the first words of serde's text. That text is
//! read to choose a phrase and is dropped; a custom `Deserialize` that begins its words like serde's gets the wrong phrase and never an
//! echo. A wording serde changes falls back to the general phrase. `tests/bad_body.rs` pins the phrases, the places and the fall-back.
//!
//! The place: `line` and `column` are 1-based, the column counts bytes, and for a data error it is just after the offending token (for
//! a missing member, at the closing brace of the object that lacks it). A client reads the fixed phrases as its contract: for example the
//! words "a member this API does not know" are what a stage that predates a new request member answers.
//!
//! Callers do not log the error or the body: a handler that wants more than this answer adds a fixed log line of its own.

use crate::handler::{Reply, error};
use serde::de::DeserializeOwned;
use serde_json::error::Category;

/// What kind of problem a body has. Chosen from the error; the answer is built from this and from numbers, never from the error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Not JSON, or text after the JSON.
    Syntax,
    /// The JSON stops before it is complete.
    Eof,
    /// A number JSON can write and this parser cannot hold.
    Range,
    /// A member the type requires is missing.
    Missing,
    /// A member the type does not know (a strict type).
    Unknown,
    /// A member that appears twice.
    Twice,
    /// A member of the wrong type, or an enum word or value the type does not accept.
    Wrong,
    /// Any other refusal, among them the words of a custom `Deserialize`.
    Other,
}

impl Kind {
    fn of(e: &serde_json::Error) -> Kind {
        match e.classify() {
            Category::Eof => Kind::Eof,
            Category::Io => Kind::Syntax,
            Category::Syntax => {
                if e.to_string().starts_with("number out of range") {
                    Kind::Range
                } else {
                    Kind::Syntax
                }
            }
            Category::Data => {
                let text = e.to_string();
                let starts = |prefix: &str| text.starts_with(prefix);
                if starts("missing field `") {
                    Kind::Missing
                } else if starts("unknown field `") {
                    Kind::Unknown
                } else if starts("duplicate field `") {
                    Kind::Twice
                } else if starts("invalid type")
                    || starts("invalid value")
                    || starts("invalid length")
                    || starts("unknown variant `")
                {
                    Kind::Wrong
                } else {
                    Kind::Other
                }
            }
        }
    }

    /// The fixed phrase of this kind.
    fn phrase(self) -> &'static str {
        match self {
            Kind::Syntax => "the body is not valid JSON",
            Kind::Eof => "the body ends before the JSON is complete",
            Kind::Range => "the body holds a number that is out of range",
            Kind::Missing => "a required member is missing",
            Kind::Unknown => "the body has a member this API does not know",
            Kind::Twice => "a member appears twice",
            Kind::Wrong => "a member has the wrong type or a value this API does not accept",
            Kind::Other => "the body does not match the request format",
        }
    }
}

/// The message of a body that failed to parse as `what`: the type, the phrase of the kind and the place.
fn message(what: &str, kind: Kind, line: usize, column: usize) -> String {
    format!("{what}: {} (line {line}, column {column})", kind.phrase())
}

/// `body` as a `T`, or the 400 `bad-request` for a body that does not parse as the request type `what` (a fixed name the handler
/// passes; it is never taken from the caller). Nothing the caller wrote is in the answer.
pub(crate) fn parse_body<T: DeserializeOwned>(what: &'static str, body: &str) -> Result<T, Reply> {
    if body.trim().is_empty() {
        return Err(error(
            400,
            "bad-request",
            &format!("{what}: the body is empty"),
        ));
    }
    serde_json::from_str(body).map_err(|e| {
        let text = message(what, Kind::of(&e), e.line(), e.column());
        error(400, "bad-request", &text)
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Strict {
        #[allow(dead_code)]
        n: u32,
    }

    /// A type whose own `Deserialize` fails with words that begin like serde's and hold the caller's text.
    #[derive(Debug)]
    struct Mimic;

    impl<'de> Deserialize<'de> for Mimic {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            let s = String::deserialize(d)?;
            Err(serde::de::Error::custom(format!("unknown field `{s}`")))
        }
    }

    fn kind_of(body: &str) -> Kind {
        let e = serde_json::from_str::<Strict>(body).unwrap_err();
        Kind::of(&e)
    }

    #[test]
    fn every_kind_has_its_own_phrase() {
        let kinds = [
            Kind::Syntax,
            Kind::Eof,
            Kind::Range,
            Kind::Missing,
            Kind::Unknown,
            Kind::Twice,
            Kind::Wrong,
            Kind::Other,
        ];
        let mut phrases: Vec<&str> = kinds.iter().map(|k| k.phrase()).collect();
        phrases.sort_unstable();
        phrases.dedup();
        assert_eq!(phrases.len(), kinds.len());
    }

    #[test]
    fn serde_json_s_own_words_choose_the_kind() {
        assert_eq!(kind_of("nope"), Kind::Syntax);
        assert_eq!(kind_of(r#"{"n":1} x"#), Kind::Syntax);
        assert_eq!(kind_of(r#"{"n":"#), Kind::Eof);
        assert_eq!(kind_of(r#"{"n":1e999}"#), Kind::Range);
        assert_eq!(kind_of("{}"), Kind::Missing);
        assert_eq!(kind_of(r#"{"n":1,"x":2}"#), Kind::Unknown);
        assert_eq!(kind_of(r#"{"n":1,"n":2}"#), Kind::Twice);
        assert_eq!(kind_of(r#"{"n":"1"}"#), Kind::Wrong);
        assert_eq!(kind_of(r#"{"n":-1}"#), Kind::Wrong);
    }

    #[test]
    fn a_custom_deserialize_that_borrows_serde_s_first_words_is_mislabelled_and_never_echoed() {
        let body = r#""SENTINEL-4c2f""#;
        let e = serde_json::from_str::<Mimic>(body).unwrap_err();
        assert!(e.to_string().contains("SENTINEL"), "trigger: {e}");
        assert_eq!(Kind::of(&e), Kind::Unknown);
        let (reply, logs) = parse_body::<Mimic>("Mimic", body).unwrap_err();
        assert_eq!(reply.status, 400);
        assert!(!reply.body.contains("SENTINEL"), "{}", reply.body);
        assert!(logs.is_empty());
    }

    /// The documents say the column counts bytes: a two-byte letter before the error moves it by two columns, not by one.
    #[test]
    fn the_column_counts_bytes_not_characters() {
        let column = |name: &str| {
            let body = format!(r#"{{"{name}":1,"x":2}}"#);
            serde_json::from_str::<Strict>(&body).unwrap_err().column()
        };
        assert_eq!(column("\u{e9}"), column("e") + 1);
        assert_eq!(column("\u{20ac}"), column("e") + 2);
    }

    #[test]
    fn the_message_is_the_type_the_phrase_and_the_place() {
        assert_eq!(
            message("T", Kind::Wrong, 2, 7),
            "T: a member has the wrong type or a value this API does not accept (line 2, column 7)"
        );
    }

    #[test]
    fn an_empty_or_blank_body_is_said_to_be_empty() {
        for body in ["", " ", "\n\t "] {
            let (reply, _) = parse_body::<Strict>("Strict", body).unwrap_err();
            assert_eq!(reply.status, 400);
            assert!(
                reply.body.contains("Strict: the body is empty"),
                "{}",
                reply.body
            );
        }
    }

    #[test]
    fn a_good_body_parses() {
        assert!(parse_body::<Strict>("Strict", r#"{"n":3}"#).is_ok());
    }
}
