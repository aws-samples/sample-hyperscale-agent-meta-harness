//! Shared enums, accounting types and string validators.

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

/// The longest engine name: the profile name rule, 1 to 32 bytes.
pub const ENGINE_NAME_MAX: usize = 32;
/// An engine name: a lowercase ASCII letter, then lowercase letters, digits and hyphens.
pub const ENGINE_NAME_PATTERN: &str = "^[a-z][a-z0-9-]{0,31}$";

/// How a person reads the name of an engine: the one table of display names. An engine with no entry is shown by its own name, so a new
/// engine reads right before this table is edited. Every sentence that names the engine of a run takes the name from [`Engine::display_name`].
pub const ENGINE_DISPLAY_NAMES: &[(&str, &str)] = &[("cline", "Cline"), ("opencode", "OpenCode")];

/// Coding-agent engine: the name of its profile
/// `engine/profiles/<name>.toml`, so an engine enters through a profile and its fixtures with no
/// protocol change. The value is checked only for its shape ([`ENGINE_NAME_PATTERN`]); whether a
/// profile of that name exists is checked where profiles are loaded (agentd, the engine policy,
/// control-api). On the wire it is the name, so every record written before (`"cline"`,
/// `"opencode"`) reads the same. `Copy`: the name is stored inline, and the associated constants
/// work as `match` patterns.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Engine {
    // Zero-padded, so the derived order is the names' byte order.
    bytes: [u8; ENGINE_NAME_MAX],
    len: u8,
}

/// Why a string is not an engine name.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("engine {0:?} is not an engine profile name (^[a-z][a-z0-9-]{{0,31}}$)")]
pub struct EngineNameError(pub String);

impl Engine {
    /// The Cline fork, primary behind QG-C.
    pub const CLINE: Engine = Engine::named("cline");
    /// OpenCode, the QG-O qualified alternate.
    pub const OPENCODE: Engine = Engine::named("opencode");
    /// The engine a run uses when its request names none. Code outside the engine modules reads the default from here and does not name an engine.
    pub const DEFAULT: Engine = Engine::CLINE;

    /// `named`, or the default engine when the request or the record names none.
    pub fn or_default(named: Option<Engine>) -> Engine {
        named.unwrap_or(Engine::DEFAULT)
    }

    /// A name known at compile time; an invalid one fails the build of a `const`.
    pub const fn named(name: &str) -> Engine {
        match Engine::parse_bytes(name.as_bytes()) {
            Some(e) => e,
            None => panic!("not an engine profile name"),
        }
    }

    const fn parse_bytes(b: &[u8]) -> Option<Engine> {
        if b.is_empty() || b.len() > ENGINE_NAME_MAX || !b[0].is_ascii_lowercase() {
            return None;
        }
        let mut bytes = [0u8; ENGINE_NAME_MAX];
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-') {
                return None;
            }
            bytes[i] = c;
            i += 1;
        }
        Some(Engine {
            bytes,
            len: b.len() as u8,
        })
    }

    pub fn new(name: &str) -> Result<Engine, EngineNameError> {
        Engine::parse_bytes(name.as_bytes()).ok_or_else(|| EngineNameError(name.to_string()))
    }

    pub fn as_str(&self) -> &str {
        // Only ASCII is ever stored.
        std::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or_default()
    }

    /// The name a person reads (`Cline`, `OpenCode`), from [`ENGINE_DISPLAY_NAMES`]; the profile name itself when the table has no entry.
    pub fn display_name(&self) -> &str {
        let name = self.as_str();
        ENGINE_DISPLAY_NAMES
            .iter()
            .find(|(n, _)| *n == name)
            .map_or(name, |(_, shown)| shown)
    }
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Engine({:?})", self.as_str())
    }
}

impl FromStr for Engine {
    type Err = EngineNameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Engine::new(s)
    }
}

impl Serialize for Engine {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Engine {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = Cow::<'de, str>::deserialize(deserializer)?;
        Engine::new(&s).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Engine {
    fn schema_name() -> Cow<'static, str> {
        "Engine".into()
    }

    fn schema_id() -> Cow<'static, str> {
        concat!(module_path!(), "::Engine").into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "description": "Coding-agent engine: the name of its profile engine/profiles/<name>.toml (production: cline, opencode).",
            "pattern": ENGINE_NAME_PATTERN,
        })
    }
}

/// `agentd`-to-engine transport: ACP (transport A) or the Cline CLI per turn (transport B).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Acp,
    Cli,
}

/// Bedrock inference protocol of a catalog row.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    Converse,
    ConverseRelay,
    MantleResponses,
    /// Mantle Chat Completions on the Cline fork.
    MantleChat,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Converse => "converse",
            Protocol::ConverseRelay => "converse-relay",
            Protocol::MantleResponses => "mantle-responses",
            Protocol::MantleChat => "mantle-chat",
        }
    }
}

/// Who started a run, and so what ends it. A Run item without a kind is one `POST /v1/runs`
/// created: its run machine ends it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RunKind {
    Probe,
}

/// User-set run budget (`--budget tokens|usd`); there is no default.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum BudgetLimit {
    Tokens { limit: u64 },
    Usd { limit: f64 },
}

/// Answer to a relayed permission request (`--approvals relay`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalDecision {
    Allow,
    Deny,
}

/// Token accounting for one turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_write_input_tokens: u64,
}

const CROCKFORD: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// True for a canonical ULID: 26 Crockford base32 characters, first character at most `7`.
pub fn is_ulid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 26 && b[0] <= b'7' && b.iter().all(|c| CROCKFORD.contains(c))
}

/// True for an RFC 3339 UTC timestamp `YYYY-MM-DDTHH:MM:SS[.fraction]Z`.
pub fn is_utc_timestamp(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 20 || b[b.len() - 1] != b'Z' {
        return false;
    }
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    let num = |r: std::ops::Range<usize>| s[r].parse::<u32>().unwrap_or(u32::MAX);
    let shape = digits(0..4)
        && b[4] == b'-'
        && digits(5..7)
        && b[7] == b'-'
        && digits(8..10)
        && b[10] == b'T'
        && digits(11..13)
        && b[13] == b':'
        && digits(14..16)
        && b[16] == b':'
        && digits(17..19);
    if !shape {
        return false;
    }
    let rest = &b[19..b.len() - 1];
    let fraction_ok = rest.is_empty()
        || (rest.len() >= 2 && rest[0] == b'.' && rest[1..].iter().all(u8::is_ascii_digit));
    fraction_ok
        && (1..=12).contains(&num(5..7))
        && (1..=31).contains(&num(8..10))
        && num(11..13) <= 23
        && num(14..16) <= 59
        && num(17..19) <= 60
}

/// Unix seconds of an RFC 3339 UTC timestamp that [`is_utc_timestamp`] accepts (a fraction is ignored, a leap second `:60` counts as the
/// second after `:59`); `None` for anything else.
pub fn parse_utc_unix(ts: &str) -> Option<i64> {
    if !is_utc_timestamp(ts) {
        return None;
    }
    let num = |r: std::ops::Range<usize>| ts[r].parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // days from civil (Howard Hinnant's algorithm), proleptic Gregorian calendar
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3_600 + mm * 60 + ss)
}

/// True for lowercase hex of exactly `len` characters.
pub fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
