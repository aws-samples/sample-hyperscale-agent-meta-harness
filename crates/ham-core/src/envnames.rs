// COMPAT-CAH: the earlier variable prefix `CAH_` is read after `HAMH_` until VERSION 5.50. At that version this file's
// fallback is deleted: `PAIRS` goes, `wrap` and `os_env` become the plain process environment, and `notices` has nothing left to say.
//! The variables of the client and their earlier names.
//!
//! Nine variables of the client had a name with the prefix `CAH_` before the rename and are documented: `HAMH_MODEL` was `CAH_MODEL`, and so on ([`PAIRS`]). For
//! the window the earlier name is still read, after the current one:
//!
//! - the current name answers when it is set and not empty, and wins when both are set;
//! - the earlier name answers when the current one is unset or empty;
//! - a name that is not in [`PAIRS`] is read as it is: the other variables of the program (test and development hooks, the launch nonce) were
//!   renamed with no fallback.
//!
//! The lookup is a wrapper around the closure every part of the client already takes (`&dyn Fn(&str) -> Option<String>`), so no signature
//! changes and every test seam keeps working: [`wrap`] makes the wrapped closure from a raw one, [`os_env`] is the process environment seen
//! through it. In the wrapped environment an earlier name is visible only while it answers, so [`answered_by`] names the variable that
//! answered with one extra lookup and `config show` reports the truth.
//!
//! The notices are made once, before anything is drawn: [`notices`] lists one line for each earlier name that is set, and the caller prints
//! them on standard error before the terminal screen opens. One switch silences every notice of the window: the variable
//! [`NO_NOTICES`] (read here, no earlier name, it is new in this release) or the setting `ui.deprecation_notices`
//! (`config::deprecation_notices_on`).

/// The variables of the client that have an earlier name: the current name and the one it had before the rename.
pub const PAIRS: [(&str, &str); 9] = [
    ("HAMH_AWS_CLI", "CAH_AWS_CLI"),
    ("HAMH_DOWNLOAD_DIR", "CAH_DOWNLOAD_DIR"),
    ("HAMH_INPUT_WAIT", "CAH_INPUT_WAIT"),
    ("HAMH_MODEL", "CAH_MODEL"),
    ("HAMH_NOTIFY", "CAH_NOTIFY"),
    ("HAMH_NO_MASCOT", "CAH_NO_MASCOT"),
    ("HAMH_NO_MOUSE", "CAH_NO_MOUSE"),
    ("HAMH_OPEN_COMMAND", "CAH_OPEN_COMMAND"),
    ("HAMH_RANGE_BYTES", "CAH_RANGE_BYTES"),
];

/// The variable that silences every notice of the window when it is `1`, `true`, `yes` or `on` (any case). It has no earlier name.
pub const NO_NOTICES: &str = "HAMH_NO_DEPRECATION_NOTICES";

type RawEnv<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The earlier name of a current name; `None` for a name that has none.
pub fn earlier_name(current: &str) -> Option<&'static str> {
    PAIRS.iter().find(|(c, _)| *c == current).map(|(_, e)| *e)
}

/// The current name of an earlier name; `None` for a name that is not one.
fn current_name(earlier: &str) -> Option<&'static str> {
    PAIRS.iter().find(|(_, e)| *e == earlier).map(|(c, _)| *c)
}

fn set(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}

/// `raw` seen through the fallback. A current name of [`PAIRS`] gets its own value when that is set and not empty, else the earlier name's
/// value, else what `raw` says. An earlier name gets its value only while it answers (the current name unset or empty). Any other name is
/// `raw`'s.
pub fn wrap<'a>(raw: RawEnv<'a>) -> impl Fn(&str) -> Option<String> + 'a {
    move |key| {
        if let Some(earlier) = earlier_name(key) {
            let current = raw(key);
            if set(current.clone()).is_some() {
                return current;
            }
            return set(raw(earlier)).or(current);
        }
        if let Some(current) = current_name(key) {
            return if set(raw(current)).is_some() {
                None
            } else {
                set(raw(key))
            };
        }
        raw(key)
    }
}

/// The process environment through [`wrap`]. The production closures of the client are this function.
pub fn os_env(key: &str) -> Option<String> {
    wrap(&|k| std::env::var(k).ok())(key)
}

/// The variable that answered a lookup of `current` through a wrapped environment: the earlier name while that is what answers, else
/// `current`. `config show` and the messages that name where a value came from use it.
pub fn answered_by(env: RawEnv<'_>, current: &'static str) -> &'static str {
    match earlier_name(current) {
        Some(earlier) if env(earlier).is_some() => earlier,
        _ => current,
    }
}

/// One line for each earlier name that is set (not empty), in the order of [`PAIRS`], on a raw environment. When the current name is set too,
/// the line says which one is used. Printing them, once, before the screen opens, is the caller's.
pub fn notices(raw: RawEnv<'_>) -> Vec<String> {
    PAIRS
        .iter()
        .filter_map(|(current, earlier)| {
            set(raw(earlier))?;
            Some(if set(raw(current)).is_some() {
                format!("Both {earlier} and {current} are set. {current} is used.")
            } else {
                format!(
                    "{earlier} is the old name of {current}. It still works and will stop working in a later release."
                )
            })
        })
        .collect()
}

/// Whether [`NO_NOTICES`] is on in `raw`.
pub fn silenced_by_env(raw: RawEnv<'_>) -> bool {
    raw(NO_NOTICES).is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}
