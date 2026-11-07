//! The mode of the engine's confinement, chosen by the deployment.

use super::launch::SandboxError;
use std::sync::atomic::{AtomicU8, Ordering};

/// The environment variable that names the mode. The microVM runtime sets `uid`, the Instances runtime `landlock`. It is a property of the
/// deployment and not a user setting: root invariant 2 is locked, and no setting may change how it is kept.
pub const ENV_VAR: &str = "AGENTD_ENGINE_SANDBOX";

/// How the engine is kept out of agentd's memory and private files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// agentd (root) starts the engine as its own user through `setpriv` (the microVM).
    Uid,
    /// The engine runs in a Landlock domain and agentd is non-dumpable (the Instances machine, whose container has no capabilities).
    Landlock,
}

/// What `AGENTD_ENGINE_SANDBOX` said. A value that is not a mode is kept with the reason, so the run can end with it: no engine starts then.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Setting {
    Mode(Mode),
    Refused(String),
}

impl Setting {
    /// The setting for the text of the variable. Only the two names count, in lower case; there is no `auto`.
    pub fn from_value(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("uid") => Setting::Mode(Mode::Uid),
            Some("landlock") => Setting::Mode(Mode::Landlock),
            None => Setting::Refused(format!(
                "{ENV_VAR} is not set; the deployment must set it to uid or landlock"
            )),
            Some(other) => Setting::Refused(format!(
                "{ENV_VAR}={other:?} is not a mode; the deployment must set it to uid or landlock"
            )),
        }
    }

    /// The mode for a run whose engine user is `engine_user`, or why the engine must not start. The `landlock` mode has no second user, so a
    /// configured engine user is a conflict, and the conflict ends the run instead of choosing one of the two.
    pub fn resolve(&self, engine_user: Option<&str>) -> Result<Mode, String> {
        match self {
            Setting::Refused(why) => Err(why.clone()),
            Setting::Mode(Mode::Uid) => Ok(Mode::Uid),
            Setting::Mode(Mode::Landlock) => match engine_user {
                Some(user) if !user.trim().is_empty() => Err(format!(
                    "{ENV_VAR}=landlock runs the engine as agentd's own user, but AGENTD_ENGINE_USER names {user:?}; \
                     the deployment must set AGENTD_ENGINE_USER to the empty string"
                )),
                _ => Ok(Mode::Landlock),
            },
        }
    }
}

const UNSET: u8 = 0;
const UID: u8 = 1;
const LANDLOCK: u8 = 2;

static PROCESS_MODE: AtomicU8 = AtomicU8::new(UNSET);

/// Records the mode this process runs in (`agentd serve` does, once, at its start), so the helpers that give files to root know whether
/// that means anything.
pub fn note_process_mode(mode: Mode) {
    let v = match mode {
        Mode::Uid => UID,
        Mode::Landlock => LANDLOCK,
    };
    PROCESS_MODE.store(v, Ordering::SeqCst);
}

/// False once the process runs in the `landlock` mode: there is no second user to give a file to, and the container does not allow the
/// change of owner. A process that never said (a test, a developer's machine) is as before.
pub fn ownership_changes_allowed() -> bool {
    PROCESS_MODE.load(Ordering::SeqCst) != LANDLOCK
}

/// Makes this process non-dumpable and reads the flag back: a process of the same user then cannot read its environment, its memory or its
/// mappings through `/proc`. Called first thing in `agentd serve`, before any secret is loaded.
pub fn set_non_dumpable() -> Result<(), SandboxError> {
    use rustix::process::{DumpableBehavior, dumpable_behavior, set_dumpable_behavior};
    let err = |detail: String| SandboxError::Enforce {
        stage: "non-dumpable".to_string(),
        detail,
    };
    set_dumpable_behavior(DumpableBehavior::NotDumpable).map_err(|e| err(e.to_string()))?;
    match dumpable_behavior() {
        Ok(DumpableBehavior::NotDumpable) => Ok(()),
        Ok(other) => Err(err(format!("the flag reads {other:?} after it was set"))),
        Err(e) => Err(err(e.to_string())),
    }
}
