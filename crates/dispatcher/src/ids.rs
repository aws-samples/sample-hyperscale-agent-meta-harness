//! Run, agent and runtime session identifiers.

/// AgentCore requires runtime session ids of 33 to 256 characters.
pub const MIN_RUNTIME_SESSION_ID: usize = 33;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("{0} id must be [A-Za-z0-9-]+")]
    Bad(&'static str),
    #[error("<run>.<agent> exceeds 64 characters, the STS role session name limit")]
    TooLong,
}

/// Letters, digits and hyphens: safe in keys, S3 prefixes, session names and session ids.
pub fn is_id(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// `<run>.<agent>`, the role session name `/internal/creds` parses the caller from.
pub fn role_session_name(run: &str, agent: &str) -> Result<String, IdError> {
    if !is_id(run) {
        return Err(IdError::Bad("run"));
    }
    if !is_id(agent) {
        return Err(IdError::Bad("agent"));
    }
    let name = format!("{run}.{agent}");
    if name.len() > 64 {
        return Err(IdError::TooLong);
    }
    Ok(name)
}

/// `<run>-<agent>-g<generation>`, zero-padded before the generation up to 33 characters, so
/// every turn of one agent generation reuses one AgentCore session.
pub fn runtime_session_id(run: &str, agent: &str, generation: u32) -> String {
    let head = format!("{run}-{agent}-g");
    let tail = generation.to_string();
    let pad = MIN_RUNTIME_SESSION_ID.saturating_sub(head.len() + tail.len());
    format!("{head}{}{tail}", "0".repeat(pad))
}

const CROCKFORD: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A ULID (48-bit millisecond time, 80 random bits, Crockford base32): control item ids, so an
/// agent's controls sort by creation.
pub fn new_ulid() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut rnd = [0u8; 10];
    // getrandom fails only when the OS has no entropy source.
    getrandom::fill(&mut rnd).unwrap_or_else(|e| panic!("no OS entropy: {e}"));
    encode_ulid(ms, &rnd)
}

/// A ULID with the given time and 80 bits taken from the SHA-256 of `seed`: the same inputs give the
/// same id, so a writer that retries puts the same event item again instead of a second one.
pub fn ulid_from(ms: u64, seed: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(seed);
    encode_ulid(u128::from(ms), &digest[..10])
}

fn encode_ulid(ms: u128, rnd: &[u8]) -> String {
    let v = ((ms & ((1 << 48) - 1)) << 80)
        | rnd.iter().fold(0u128, |acc, b| (acc << 8) | u128::from(*b));
    (0..26)
        .map(|k| CROCKFORD[((v >> (125 - 5 * k)) & 31) as usize] as char)
        .collect()
}

/// An AgentCore capacity provider id: `<name>-<10 characters>`, the name `[a-zA-Z][a-zA-Z0-9_]{0,47}` (the API's own pattern). The id is
/// part of the path of `DeleteCapacityProviderSession`, so a value that is not one never leaves the function.
pub fn is_capacity_provider_id(s: &str) -> bool {
    let Some((name, tail)) = s.rsplit_once('-') else {
        return false;
    };
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && name.len() <= 48
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && tail.len() == 10
        && tail.chars().all(|c| c.is_ascii_alphanumeric())
}
