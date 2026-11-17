//! What the engines agentd knows by name say when their provider path fails,
//! for `crate::acp::AcpError::fault_hint`. Measured shapes of a JSON-RPC error's `data`: the first
//! engine (the Cline fork, 3.0.65) sends `{message}`, the second (OpenCode 1.18.32) `{service,
//! errorName}`, the fork's patch 0011 `{message, statusCode, errorName, isRetryable, retryAfterS}`.

/// The text an engine gives when its own idle timeout fires: the first engine (3.0.65) and the
/// second (1.18.32, `options.timeout`) `The operation timed out.`, the second's response-header
/// wait `Provider response headers timed out after <n>ms`, after the `Internal error: ` the ACP
/// layer adds. Anchored and case-sensitive: any other text is not read as a timeout.
pub fn is_idle_timeout(message: &str) -> bool {
    let m = message.strip_prefix("Internal error: ").unwrap_or(message);
    matches!(m, "The operation timed out." | "The operation timed out")
        || m.strip_prefix("Provider response headers timed out after ")
            .and_then(|rest| rest.strip_suffix("ms"))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// True when the output of a command of the first engine says its shell tool cut the command (`Command timed out after 30000ms`; the fork's
/// patch 0015 sets the limit). Any digits count; nothing depends on the number.
pub fn cline_cut(text: &str) -> bool {
    text.find("Command timed out after ")
        .map(|k| &text[k + "Command timed out after ".len()..])
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
}

/// A shell call the second engine cut at a time limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineCut {
    /// The limit the engine's text names, in milliseconds; `None` when the text names none.
    pub ms: Option<u64>,
}

/// Reads a cut by the second engine from a call's final update. The sign is the engine's own flag, `rawOutput.metadata.timeout == true`,
/// which a command's output cannot set. The number is the last "Command exceeded timeout of <digits> ms" in the text of the update's `content`
/// blocks: the engine's sentence stands behind the command's output, and for a call it ran under another tool name another sentence follows it.
/// A command that only prints those words has no flag and is no cut.
pub fn opencode_cut(update: &serde_json::Value) -> Option<EngineCut> {
    if update
        .pointer("/rawOutput/metadata/timeout")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return None;
    }
    const BEFORE: &str = "Command exceeded timeout of ";
    let mut ms = None;
    for block in update
        .get("content")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(text) = block
            .pointer("/content/text")
            .or_else(|| block.get("text"))
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        let mut rest = text;
        while let Some(at) = rest.find(BEFORE) {
            rest = &rest[at + BEFORE.len()..];
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if !digits.is_empty() && rest[digits.len()..].starts_with(" ms") {
                ms = digits.parse::<u64>().ok().or(ms);
            }
        }
    }
    Some(EngineCut { ms })
}
