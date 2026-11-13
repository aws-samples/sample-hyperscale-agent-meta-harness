//! The variables a `landlock` run sets so that its tools keep their per-user and temp files in folders the engine may write.
//!
//! In the microVM the engine is a user of its own with a home that it owns, and a Java program that takes `user.home` from the password database
//! lands there. In the `landlock` mode the engine is agentd's own user (root in the Instances container), the password database names `/root`, and no
//! grant covers it: Maven cannot make `${user.home}/.m2/repository`, and Gradle cannot make `~/.gradle`. The JVM also names `/tmp` as `java.io.tmpdir` and
//! ignores `TMPDIR`. So the run's own home and temp folder are named for it: `JAVA_TOOL_OPTIONS` (every JVM reads it, the forked test JVM of a build
//! tool included) gets `-Duser.home` and `-Djava.io.tmpdir`, and `GRADLE_USER_HOME` is a folder of the run's home. The engine's environment and the
//! environment of the report's steps (which have no agent to adapt) both get them, from their own home and temp folder.

use std::collections::BTreeMap;
use std::path::Path;

/// `NAME`'s value for a path: quoted when the path has a space, because the JVM splits `JAVA_TOOL_OPTIONS` at spaces.
fn property(name: &str, path: &Path) -> String {
    let text = path.display().to_string();
    if text.chars().any(char::is_whitespace) {
        format!("{name}=\"{text}\"")
    } else {
        format!("{name}={text}")
    }
}

/// `JAVA_TOOL_OPTIONS` with `-Duser.home=<home>` and `-Djava.io.tmpdir=<tmp>` after whatever `existing` already holds (the last property of a name
/// wins, so the run's folders win over an earlier setting).
pub fn java_tool_options(existing: Option<&str>, home: &Path, tmp: &Path) -> String {
    let ours = format!(
        "-D{} -D{}",
        property("user.home", home),
        property("java.io.tmpdir", tmp)
    );
    match existing.map(str::trim).filter(|e| !e.is_empty()) {
        Some(e) => format!("{e} {ours}"),
        None => ours,
    }
}

/// The variables to add to the environment `current` of a sandboxed process whose writable home is `home` and temp folder `tmp`.
pub fn tool_env(
    current: &BTreeMap<String, String>,
    home: &Path,
    tmp: &Path,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "JAVA_TOOL_OPTIONS".to_string(),
            java_tool_options(
                current.get("JAVA_TOOL_OPTIONS").map(String::as_str),
                home,
                tmp,
            ),
        ),
        (
            "GRADLE_USER_HOME".to_string(),
            home.join(".gradle").display().to_string(),
        ),
    ])
}
