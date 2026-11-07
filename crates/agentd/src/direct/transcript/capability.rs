//! The engine capability record: what the engine says it can do in this session, read from the ACP handshake
//! in ACP's own words, and the harness's own lists. One record per session, posted as the agent event `session.capabilities`.
//!
//! The handshake is the engine's word, so nothing is kept as it arrives: every list and every string has a
//! cap, a plain value is the only kind of option value kept, and a string that looks like a secret is redacted. The record is cut to
//! the event payload limit by [`SessionCapabilitiesPayload::bounded`] (commands first, then option values, then session capabilities).
//! The builder names no engine: the display label is an argument ([`crate::engine::direct::engine_label`] in the caller).

use harness_core::redact::redact_with_secrets;
use harness_protocol::transcript::{
    CapabilityAgent, CapabilityCommand, CapabilityConfigOption, CapabilityMode, CapabilityModes,
    HarnessCapabilities, SessionCapabilitiesPayload,
};
use serde_json::{Map, Value};

/// The record's format version.
pub const RECORD_VERSION: u32 = 1;
const NAME_CHARS: usize = 64;
const ID_CHARS: usize = 128;
const DESCRIPTION_CHARS: usize = 160;
const VALUE_CHARS: usize = 128;
const MAX_OPTIONS: usize = 32;
const MAX_MODES: usize = 16;
const MAX_COMMANDS: usize = 64;
const MAX_CAPABILITY_KEYS: usize = 16;
/// A member of the capability objects is kept when its JSON is at most this long.
const CAPABILITY_VALUE_BYTES: usize = 128;

/// `s` redacted and cut to at most `max` characters.
fn clean(s: &str, max: usize) -> String {
    redact_with_secrets(s, &[]).chars().take(max).collect()
}

fn text(v: &Value, key: &str, max: usize) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(|s| clean(s, max))
        .filter(|s| !s.is_empty())
}

/// An object of capabilities cut to a bounded number of short members.
fn capability_object(v: Option<&Value>) -> Option<Value> {
    let obj = v?.as_object()?;
    let kept: Map<String, Value> = obj
        .iter()
        .filter(|(_, x)| serde_json::to_vec(x).is_ok_and(|b| b.len() <= CAPABILITY_VALUE_BYTES))
        .take(MAX_CAPABILITY_KEYS)
        .map(|(k, x)| (clean(k, NAME_CHARS), x.clone()))
        .collect();
    (!kept.is_empty()).then_some(Value::Object(kept))
}

/// A plain option value: a boolean, a number or a short string (redacted); anything else is not kept.
fn plain_value(v: Option<&Value>) -> Option<Value> {
    match v? {
        Value::Bool(b) => Some(Value::Bool(*b)),
        Value::Number(n) => Some(Value::Number(n.clone())),
        Value::String(s) => Some(Value::String(clean(s, VALUE_CHARS))),
        _ => None,
    }
}

fn modes(new_result: &Value) -> Option<CapabilityModes> {
    let m = new_result.get("modes")?;
    let available: Vec<CapabilityMode> = m
        .get("availableModes")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    Some(CapabilityMode {
                        id: text(x, "id", NAME_CHARS)?,
                        name: text(x, "name", NAME_CHARS).unwrap_or_default(),
                    })
                })
                .take(MAX_MODES)
                .collect()
        })
        .unwrap_or_default();
    let current = text(m, "currentModeId", NAME_CHARS);
    (!available.is_empty() || current.is_some()).then_some(CapabilityModes { current, available })
}

fn config_options(new_result: &Value) -> Vec<CapabilityConfigOption> {
    new_result
        .get("configOptions")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|o| {
                    Some(CapabilityConfigOption {
                        id: text(o, "id", NAME_CHARS)?,
                        name: text(o, "name", NAME_CHARS).unwrap_or_default(),
                        kind: text(o, "type", NAME_CHARS),
                        current: plain_value(o.get("currentValue")),
                    })
                })
                .take(MAX_OPTIONS)
                .collect()
        })
        .unwrap_or_default()
}

fn commands(list: &[Value]) -> Vec<CapabilityCommand> {
    list.iter()
        .filter_map(|c| {
            Some(CapabilityCommand {
                name: text(c, "name", NAME_CHARS)?,
                description: text(c, "description", DESCRIPTION_CHARS),
            })
        })
        .take(MAX_COMMANDS)
        .collect()
}

/// The record for one session: `initialize` is the result of the handshake's first call, `new_result` the result of `session/new`,
/// `advertised` the commands of the last `available_commands_update` seen. A reply that is no handshake gives the smallest record.
pub fn build(
    engine: &str,
    session: u32,
    session_id: Option<&str>,
    initialize: Option<&Value>,
    new_result: &Value,
    advertised: &[Value],
) -> SessionCapabilitiesPayload {
    let caps = initialize.and_then(|i| i.get("agentCapabilities"));
    let agent = initialize.and_then(|i| i.get("agentInfo")).and_then(|a| {
        Some(CapabilityAgent {
            name: text(a, "name", NAME_CHARS)?,
            version: text(a, "version", NAME_CHARS),
        })
    });
    SessionCapabilitiesPayload {
        v: RECORD_VERSION,
        engine: clean(engine, NAME_CHARS),
        session,
        session_id: session_id
            .map(|s| clean(s, ID_CHARS))
            .filter(|s| !s.is_empty()),
        acp_version: initialize
            .and_then(|i| i.get("protocolVersion"))
            .and_then(Value::as_u64),
        agent,
        load_session: caps
            .and_then(|c| c.get("loadSession"))
            .and_then(Value::as_bool),
        prompt_capabilities: capability_object(caps.and_then(|c| c.get("promptCapabilities"))),
        session_capabilities: capability_object(caps.and_then(|c| c.get("sessionCapabilities"))),
        modes: modes(new_result),
        config_options: config_options(new_result),
        commands: commands(advertised),
        harness: HarnessCapabilities::this_build(),
        truncated: false,
    }
}

/// The bytes the record of an engine's capabilities is cut to when it is sent as an event: 4 KiB, a limit of its own that does not move with the
/// inline payload limit of an event.
pub const CAPABILITY_PAYLOAD_BYTES: usize = 4096;

/// The record as the event's payload: cut to [`CAPABILITY_PAYLOAD_BYTES`].
pub fn event_value(p: SessionCapabilitiesPayload) -> Value {
    serde_json::to_value(p.bounded(CAPABILITY_PAYLOAD_BYTES)).unwrap_or(Value::Null)
}
