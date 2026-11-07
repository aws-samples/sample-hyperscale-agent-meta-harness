//! `ham config [get|set|unset|show|check|edit|docs]` and `ham models`.
//! None of them needs the
//! network or AWS credentials.
//!
//! The file is read once, bounded ([`load_file`]: a regular file, at most the cap of the reader, its SHA-256 kept), changed as text by the
//! comment-keeping patcher through [`ham_core::config_write`], and written by [`commit`]: a temp file in the same folder, synced, then, just
//! before the rename, the file is read again and its SHA-256 compared with the one loaded, so an edit made by someone else in between stops the
//! write ("changed on disk; reloaded; nothing written"). That narrows the race and is not a lock: the last rename still wins, and no lock
//! file is made. A link at `config.toml` (a dotfile manager's) is followed and stays a link; the mode is kept (a new file is private); a file
//! that is not TOML is never touched. `get`, `show`, `check`, `edit` and `docs` run on a file with a bad key so that it can be repaired; `models`,
//! `runs` and a start refuse it and name the line ([`load`]). `models` prints `engine/catalog/models.json` (compiled in) with the default
//! marked and the model the next run would use marked with `*`, chosen by the same order a start uses (`ham_core::models`).

use ham_core::command::ConfigCommand;
use ham_core::config::{
    Config, ConfigError, ConfigRead, KEYS, config_path, get_key, notify_on, parse_config_bytes,
    read_config_bytes,
};
use ham_core::config_write::{Plan, PlanError, door_note, plan_set, plan_unset};
use ham_core::envnames::answered_by;
use ham_core::exit::ERROR;
use ham_core::models::{
    DEFAULT_MODEL_ID, ModelError, ModelList, ModelPrefs, ModelSource, builtin, render_models,
    resolve,
};
use ham_core::request::Origin;
use ham_core::sanitize::sanitize_line;
use ham_core::settings::{
    Applies, Finding, Key, LOCKED, MAX_CONFIG_BYTES, Order, Severity, reference_markdown,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// What a write that lost the race says.
const CHANGED: &str = "config.toml changed on disk; reloaded; nothing written";

/// The config file as it is on disk: where it is, its bytes (at most the reader's cap and one more), the SHA-256 of those bytes, and what the
/// reader found.
#[derive(Clone, Debug)]
pub struct Loaded {
    /// The path the environment names (a link stays a link).
    pub path: PathBuf,
    /// What the reader made of the bytes: the keys it could read and the findings with their lines.
    pub read: ConfigRead,
    bytes: Vec<u8>,
    file_len: u64,
    exists: bool,
    digest: Option<[u8; 32]>,
}

impl Loaded {
    /// True when there is a file.
    pub fn exists(&self) -> bool {
        self.exists
    }

    /// The text of the file (empty for no file). Empty too for a file the reader refused as a whole: check [`Loaded::refusal`] first.
    pub fn text(&self) -> &str {
        if self.bytes.len() > MAX_CONFIG_BYTES {
            return "";
        }
        std::str::from_utf8(&self.bytes).unwrap_or("")
    }

    /// Why the file cannot be used as TOML at all (too large, not UTF-8, a NUL byte, a byte-order mark, nested too deep, a syntax error),
    /// the findings one per line; `None` for a file whose keys can be read, including one with a bad key.
    pub fn refusal(&self) -> Option<String> {
        let why: Vec<String> = self
            .read
            .findings
            .errors()
            .filter(|f| f.code.is_unreadable())
            .map(Finding::render)
            .collect();
        (!why.is_empty()).then(|| why.join("\n"))
    }

    /// How many bytes of the file are held in memory: never more than the reader's cap and one more, whatever the size on disk.
    pub fn held_bytes(&self) -> usize {
        self.bytes.len()
    }

    /// The path as a message shows it.
    pub fn shown(&self) -> String {
        self.path.display().to_string()
    }

    /// The keys, or the error that a command which starts work refuses with (an error finding, or a newer file's unknown key that may steer
    /// a run).
    pub fn strict(&self) -> Result<Config, ConfigError> {
        parse_config_bytes(&self.bytes, self.file_len)
    }
}

fn digest_of(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// The bytes of a regular file, read to at most the cap and one more; `None` when there is no file. The size on disk comes with them. A
/// link is followed; a directory, a device or a pipe is refused.
fn read_bounded(path: &Path) -> Result<Option<(Vec<u8>, u64)>, String> {
    let shown = path.display();
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{shown}: {e}")),
    };
    if !meta.is_file() {
        return Err(format!("{shown}: not a regular file"));
    }
    let file = std::fs::File::open(path).map_err(|e| format!("{shown}: {e}"))?;
    // The handle is checked again: what was opened is what was looked at.
    let opened = file.metadata().map_err(|e| format!("{shown}: {e}"))?;
    if !opened.is_file() {
        return Err(format!("{shown}: not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{shown}: {e}"))?;
    Ok(Some((bytes, opened.len())))
}

/// Reads the config file once (absent is empty). An error is a file that cannot be read at all: no config location, not a regular file, an
/// I/O error. A file with bad content is `Ok`: its [`Loaded::read`] holds the findings.
pub fn load_file(env: Env<'_>) -> Result<Loaded, String> {
    let Some(path) = config_path(env) else {
        return Err("no config location: set XDG_CONFIG_HOME or HOME".into());
    };
    Ok(match read_bounded(&path)? {
        None => Loaded {
            read: read_config_bytes(&[], 0),
            path,
            bytes: Vec::new(),
            file_len: 0,
            exists: false,
            digest: None,
        },
        Some((bytes, file_len)) => Loaded {
            read: read_config_bytes(&bytes, file_len),
            digest: Some(digest_of(&bytes)),
            path,
            bytes,
            file_len,
            exists: true,
        },
    })
}

/// The config file (absent is empty) and its path, for a command that starts work or reads the stage: a file with an error, or a newer
/// file whose unknown key may steer a run, is refused with the line of each problem.
pub fn load(env: Env<'_>) -> Result<(Config, PathBuf), String> {
    let loaded = load_file(env)?;
    let shown = loaded.shown();
    loaded
        .strict()
        .map(|c| (c, loaded.path.clone()))
        .map_err(|e| format!("{shown}: {e}"))
}

/// Writes `new_text` over the file `loaded` was read from, through a temp file in the same directory and a rename, so an interrupted write
/// leaves the old file. A symbolic link at the path is followed: the file it points to is replaced, so the link stays, and that file must be
/// a regular one. The new file keeps the old file's permission bits whatever the umask (a new file is private, 0600), and the file and its
/// directory are synced before the call returns. Just before the rename the file is read again; if its SHA-256 is not the one loaded, nothing
/// is written ([`CHANGED`]). Two writers at the same moment are not serialized: the last rename wins.
pub fn commit(loaded: &Loaded, new_text: &str) -> Result<(), String> {
    commit_file(&loaded.path, loaded.digest, new_text, CHANGED)
}

/// [`commit`] for any file of the user's: `path` is where it is, `expected` the SHA-256 of the bytes it was read from (`None` for a file that was
/// not there), `changed` the words of the refusal when the file is not what was read. The temp file is named for the file it replaces. The engine
/// files of `ham engine` use it, so they are written exactly as `config.toml` is.
pub fn commit_file(
    path: &Path,
    expected: Option<[u8; 32]>,
    new_text: &str,
    changed: &str,
) -> Result<(), String> {
    let shown = path.display().to_string();
    let target = match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            std::fs::canonicalize(path).map_err(|e| format!("{shown}: {e}"))?
        }
        _ => path.to_path_buf(),
    };
    if let Ok(m) = std::fs::metadata(&target)
        && !m.is_file()
    {
        return Err(format!("{shown}: not a regular file"));
    }
    let dir = target
        .parent()
        .ok_or_else(|| format!("{shown}: no directory"))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mode = std::fs::metadata(&target).map_or(0o600, |m| {
        std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777
    });
    let name = target
        .file_name()
        .map_or_else(|| "file".to_string(), |n| n.to_string_lossy().into_owned());
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let result = (|| -> Result<(), String> {
        let io = |e: std::io::Error| format!("{shown}: {e}");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)
            .map_err(io)?;
        // The umask cuts the mode given to open(); the bits the old file had are set on the handle itself.
        f.set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(io)?;
        f.write_all(new_text.as_bytes()).map_err(io)?;
        f.sync_all().map_err(io)?;
        let now = read_bounded(&target)?.map(|(bytes, _)| digest_of(&bytes));
        if now != expected {
            return Err(changed.to_string());
        }
        std::fs::rename(&tmp, &target).map_err(io)?;
        std::fs::File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(io)
    })();
    if result.is_err() {
        // The temp file is ours (named for this process, created new): remove it so a failed write leaves nothing behind.
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// What a `set` or `unset` did.
#[derive(Clone, Debug)]
pub struct Applied {
    pub path: PathBuf,
    /// The name `ham config` prints for the key.
    pub name: &'static str,
    /// The value after the change (`None` after an unset).
    pub value: Option<String>,
    /// False for a no-op: the file was not written.
    pub wrote: bool,
    /// True when this write added the first table, the one older builds refuse.
    pub door: bool,
    /// What the file still holds that is wrong, one line each.
    pub still_wrong: Vec<String>,
}

fn apply(
    env: Env<'_>,
    plan: impl FnOnce(&str) -> Result<Plan, PlanError>,
) -> Result<Applied, String> {
    // COMPAT-CAH: while only the folder of the earlier command name holds the files, nothing is written.
    if let Some(text) = ham_core::config_dir::write_refusal(env) {
        return Err(text);
    }
    let loaded = load_file(env)?;
    let shown = loaded.shown();
    if let Some(why) = loaded.refusal() {
        return Err(format!("{shown}: {why}"));
    }
    let plan = plan(loaded.text()).map_err(|e| match e {
        PlanError::Config(c) => c.to_string(),
        other => format!("{shown}: {other}"),
    })?;
    let wrote = match &plan.text {
        Some(text) => {
            commit(&loaded, text)?;
            true
        }
        None => false,
    };
    Ok(Applied {
        path: loaded.path,
        name: plan.name,
        value: plan.stored,
        wrote,
        door: plan.opens_the_door,
        still_wrong: plan.still_wrong,
    })
}

/// `ham config set <key> <value>` on the file `env` points at. The key and value are checked first; nothing is written for a no-op.
pub fn apply_set(env: Env<'_>, key: &str, value: &str) -> Result<Applied, String> {
    apply(env, |text| plan_set(text, key, value))
}

/// `ham config unset <key>` on the file `env` points at.
pub fn apply_unset(env: Env<'_>, key: &str) -> Result<Applied, String> {
    apply(env, |text| plan_unset(text, key))
}

/// The note that setting `key` to `value` would have to print because the write adds the first table to the file, or `None`: for a key that is
/// not a table key, a table already there, a value that will be refused, or a file that cannot be edited. The message box shows it before the
/// write.
pub fn door_note_for(env: Env<'_>, key: &str, value: &str) -> Option<String> {
    let loaded = load_file(env).ok()?;
    if loaded.refusal().is_some() {
        return None;
    }
    let plan = plan_set(loaded.text(), key, value).ok()?;
    plan.opens_the_door
        .then(|| door_note(crate::version::CLIENT, plan.name))
}

/// The two weaker layers of the model choice: `HAMH_MODEL` and the config file's `default_model`.
pub fn model_prefs(env: Env<'_>, cfg: &Config) -> ModelPrefs {
    ModelPrefs {
        env: env("HAMH_MODEL"),
        config: cfg.default_model.clone(),
    }
}

/// The name `ham config` prints for a key given in either spelling.
fn name_of(key: &str) -> &str {
    KEYS.iter()
        .find(|(f, s)| *f == key || *s == key)
        .map_or(key, |(_, s)| s)
}

// ---- the effective value of a key ----------------------------------------------------------------------------------------------------

/// A key as `ham config show` and the Settings screen show it: the value in force, where it came from and when a change takes effect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Effective {
    pub key: Key,
    /// The value in force; for a key nothing sets, the default as the registry words it (`stage default`, `AWS chain`).
    pub value: String,
    pub origin: Origin,
    pub applies: Applies,
    /// True when a layer above the default sets it.
    pub set: bool,
}

/// The value of `key` in force, from the environment `env` and the file `cfg`, by the order of its registry row (a flag takes part only in a
/// run, so it never shows here).
pub fn effective(cfg: &Config, env: Env<'_>, key: Key) -> Effective {
    let row = key.row();
    let file = get_key(cfg, row.name).ok().flatten();
    let var = (!row.env.is_empty())
        .then(|| env(row.env))
        .flatten()
        .filter(|v| !v.trim().is_empty());
    let from_env = || -> Option<(String, Origin)> {
        match key {
            Key::Mouse => crate::tui::session::mouse_forced_off(env).then(|| {
                (
                    "false".to_string(),
                    Origin::Env(answered_by(env, "HAMH_NO_MOUSE")),
                )
            }),
            // `HAMH_NO_MASCOT` can only switch the mascot off: it is a layer only while it does.
            Key::UiMascot => (ham_core::config::mascot_mode(env, &Config::default())
                == ham_core::config::MascotMode::Off)
                .then(|| {
                    (
                        "off".to_string(),
                        Origin::Env(answered_by(env, "HAMH_NO_MASCOT")),
                    )
                }),
            // `HAMH_NO_DEPRECATION_NOTICES` can only switch the notices off: it is a layer only while it does.
            Key::UiDeprecationNotices => ham_core::envnames::silenced_by_env(env).then(|| {
                (
                    "false".to_string(),
                    Origin::Env("HAMH_NO_DEPRECATION_NOTICES"),
                )
            }),
            Key::UiNotify => var.is_some().then(|| {
                (
                    notify_on(env, cfg).to_string(),
                    Origin::Env(answered_by(env, "HAMH_NOTIFY")),
                )
            }),
            Key::DefaultModel => var.as_ref().map(|v| {
                let shown = ham_core::models::resolve_given(builtin(), v)
                    .map_or_else(|_| v.clone(), |r| r.model.id.clone());
                (shown, Origin::Env(answered_by(env, row.env)))
            }),
            _ => var
                .as_ref()
                .map(|v| (v.clone(), Origin::Env(answered_by(env, row.env)))),
        }
    };
    let from_file = || file.clone().map(|v| (v, Origin::File));
    let picked = match row.order {
        Order::FlagEnvFile | Order::EnvOverFile | Order::EnvForcesOff => {
            from_env().or_else(from_file)
        }
        Order::FlagFile | Order::FileOnly => from_file(),
        Order::FileOverEnv => from_file().or_else(from_env),
    };
    let set = picked.is_some();
    let (value, origin) = picked.unwrap_or_else(|| (row.default_text.to_string(), Origin::Default));
    Effective {
        key,
        value,
        origin,
        applies: row.applies,
        set,
    }
}

// ---- the verbs -------------------------------------------------------------------------------------------------------------------------

fn fail(err: &mut dyn Write, msg: impl std::fmt::Display) -> i32 {
    let _ = writeln!(err, "ham: {msg}");
    ERROR
}

/// Reads the file for a verb that only reads: the error of a file that cannot be read, the refusal of a file that is not usable as TOML,
/// and a warning line for each finding of a file with a bad key, which the verb then goes on with.
fn load_for_reading(env: Env<'_>, err: &mut dyn Write) -> Result<Loaded, i32> {
    let loaded = load_file(env).map_err(|e| fail(err, e))?;
    if let Some(why) = loaded.refusal() {
        return Err(fail(err, format!("{}: {why}", loaded.shown())));
    }
    warn_about(&loaded.shown(), loaded.read.findings.iter(), err);
    Ok(loaded)
}

/// One warning line for each finding, then, if the file has an error, how to see and fix them.
fn warn_about<'a>(shown: &str, findings: impl Iterator<Item = &'a Finding>, err: &mut dyn Write) {
    let mut errors = 0;
    for f in findings {
        errors += usize::from(f.severity == Severity::Error);
        let _ = writeln!(err, "ham: warning: {shown}: {}", sanitize_line(&f.render()));
    }
    if errors > 0 {
        let _ = writeln!(
            err,
            "ham: warning: {shown} has a problem; `ham config check` lists it and `ham config edit` opens the file"
        );
    }
}

fn finding_json(f: &Finding) -> Value {
    json!({
        "severity": if f.severity == Severity::Error { "error" } else { "warning" },
        "code": f.code.as_str(),
        "line": f.line,
        "column": f.column,
        "key": f.key,
        "suggestion": f.suggestion,
        "message": sanitize_line(&f.message),
    })
}

/// The one thing that makes a clean-looking file refuse a run: a newer `ham` wrote a key or table outside `[ui]` that this build does not know.
pub(crate) fn blocks_runs(loaded: &Loaded) -> Option<String> {
    (!loaded.read.has_errors() && loaded.read.run_blocked()).then(|| {
        let version = loaded.read.config.config_version.unwrap_or(0);
        format!(
            "the file was written by a newer ham (configVersion {version}) and holds keys this ham does not know that may steer a run, so run starts are refused; update ham or remove those keys"
        )
    })
}

fn run_check(
    env: Env<'_>,
    strict: bool,
    json_out: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let loaded = match load_file(env) {
        Ok(l) => l,
        Err(e) => return fail(err, e),
    };
    let shown = loaded.shown();
    if !loaded.exists() {
        if json_out {
            let _ = writeln!(
                out,
                "{}",
                json!({"path": shown, "exists": false, "ok": true, "errors": 0, "warnings": 0, "strict": strict, "findings": []})
            );
        } else {
            let _ = writeln!(
                out,
                "no config file at {shown}; every setting is at its default"
            );
        }
        return 0;
    }
    let blocked = blocks_runs(&loaded);
    let errors = loaded.read.findings.errors().count() + usize::from(blocked.is_some());
    let warnings = loaded.read.findings.warnings().count();
    let failed = errors > 0 || (strict && warnings > 0);
    if json_out {
        let mut findings: Vec<Value> = loaded.read.findings.iter().map(finding_json).collect();
        if let Some(why) = &blocked {
            findings.push(json!({"severity": "error", "code": "blocks_run", "line": null, "column": null, "key": null, "suggestion": null, "message": why}));
        }
        let _ = writeln!(
            out,
            "{}",
            json!({"path": shown, "exists": true, "ok": !failed, "errors": errors, "warnings": warnings, "strict": strict, "findings": findings})
        );
    } else {
        for f in loaded.read.findings.iter() {
            let label = if f.severity == Severity::Error {
                "error"
            } else {
                "warning"
            };
            let _ = writeln!(out, "{label}: {}", sanitize_line(&f.render()));
        }
        if let Some(why) = &blocked {
            let _ = writeln!(out, "error: {why}");
        }
        let _ = if errors == 0 && warnings == 0 {
            writeln!(out, "{shown}: ok")
        } else {
            writeln!(out, "{shown}: {errors} error(s), {warnings} warning(s)")
        };
    }
    i32::from(failed)
}

fn run_show(env: Env<'_>, json_out: bool, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let loaded = match load_for_reading(env, err) {
        Ok(l) => l,
        Err(code) => return code,
    };
    let cfg = &loaded.read.config;
    let shown: Vec<Effective> = Key::ALL.iter().map(|k| effective(cfg, env, *k)).collect();
    // COMPAT-CAH: both folders hold files, so the folder of the earlier command name is not read.
    let unread = ham_core::config_dir::dirs(env)
        .filter(|d| d.earlier_unread)
        .map(|d| d.earlier.display().to_string());
    if json_out {
        let settings: Vec<Value> = shown
            .iter()
            .map(|e| {
                json!({
                    "key": e.key.row().name,
                    "value": e.value,
                    "origin": e.origin.label(),
                    "applies": e.applies.label(),
                    "set": e.set,
                })
            })
            .collect();
        let locked: Vec<Value> = LOCKED
            .iter()
            .map(|c| json!({"class": c.name, "label": c.label, "reason": c.reason}))
            .collect();
        let mut doc = json!({"path": loaded.shown(), "settings": settings, "locked": locked});
        if let (Some(path), Some(obj)) = (&unread, doc.as_object_mut()) {
            obj.insert("earlierFolderNotRead".into(), json!(path));
        }
        let _ = writeln!(out, "{doc}");
        return 0;
    }
    for e in &shown {
        let _ = writeln!(
            out,
            "{:<20} {:<44} {:<22} {}",
            e.key.row().name,
            sanitize_line(&e.value),
            e.origin.label(),
            e.applies.label()
        );
    }
    if let Some(path) = &unread {
        let _ = writeln!(out, "\n{path} also exists and is not read.");
    }
    let _ = writeln!(out, "\nLocked by the harness (no file changes these):");
    for c in &LOCKED {
        let _ = writeln!(out, "  {:<28} {}", c.name, c.reason);
    }
    0
}

fn run_set(
    env: Env<'_>,
    key: &str,
    value: &str,
    json_out: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    match apply_set(env, key, value) {
        Err(e) => fail(err, e),
        Ok(a) => {
            let stored = a.value.clone().unwrap_or_default();
            if json_out {
                let _ = writeln!(
                    out,
                    "{}",
                    json!({"key": a.name, "value": stored, "path": a.path.display().to_string()})
                );
            } else {
                let _ = writeln!(out, "{} = {stored}", a.name);
            }
            after_write(&a, err);
            0
        }
    }
}

fn run_unset(
    env: Env<'_>,
    key: &str,
    json_out: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    match apply_unset(env, key) {
        Err(e) => fail(err, e),
        Ok(a) => {
            if json_out {
                let _ = writeln!(out, "{}", json!({"key": a.name, "value": null}));
            } else {
                let _ = writeln!(out, "{} = (not set)", a.name);
            }
            after_write(&a, err);
            0
        }
    }
}

/// What a write says on standard error once it is done: what the file still holds that is wrong, and, for the first table, the note about
/// older builds.
fn after_write(a: &Applied, err: &mut dyn Write) {
    let shown = a.path.display().to_string();
    for line in &a.still_wrong {
        let _ = writeln!(err, "ham: warning: {shown}: {}", sanitize_line(line));
    }
    if !a.still_wrong.is_empty() {
        let _ = writeln!(
            err,
            "ham: warning: {shown} still has a problem; `ham config check` lists it and `ham config edit` opens the file"
        );
    }
    if a.door {
        let _ = writeln!(
            err,
            "ham: note: {}",
            door_note(crate::version::CLIENT, a.name)
        );
    }
}

fn run_edit(env: Env<'_>, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let loaded = match load_file(env) {
        Ok(l) => l,
        Err(e) => return fail(err, e),
    };
    // The editor gets the file a link points to, because many editors replace the file they save, and the replacement of a link would
    // lose the link.
    let target = match std::fs::symlink_metadata(&loaded.path) {
        Ok(m) if m.file_type().is_symlink() => match std::fs::canonicalize(&loaded.path) {
            Ok(t) => t,
            Err(e) => return fail(err, format!("{}: {e}", loaded.shown())),
        },
        _ => loaded.path.clone(),
    };
    if !loaded.exists() {
        let made = target
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&target)
                    .map(drop)
            });
        if let Err(e) = made {
            return fail(err, format!("{}: {e}", target.display()));
        }
    }
    if let Err(e) = crate::rules_cmd::run_editor(&crate::rules_cmd::editor(env), &target) {
        return fail(err, e);
    }
    let code = run_check(env, false, false, out, err);
    if code != 0 {
        let _ = writeln!(err, "ham: fix the file with: ham config edit");
    }
    code
}

/// `ham config ...` against the file `env` points at.
pub fn run_config(
    cmd: &ConfigCommand,
    env: Env<'_>,
    json_out: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    match cmd {
        ConfigCommand::List => {
            let loaded = match load_for_reading(env, err) {
                Ok(l) => l,
                Err(code) => return code,
            };
            let cfg = &loaded.read.config;
            let mut obj = serde_json::Map::new();
            for (file_key, snake_key) in KEYS {
                let v = get_key(cfg, file_key).ok().flatten();
                if json_out {
                    obj.insert((*snake_key).to_string(), json!(v));
                } else {
                    let _ = writeln!(out, "{snake_key} = {}", v.as_deref().unwrap_or("(not set)"));
                }
            }
            if json_out {
                obj.insert("path".into(), json!(loaded.shown()));
                let _ = writeln!(out, "{}", Value::Object(obj));
            }
            0
        }
        ConfigCommand::Get { key } => {
            let loaded = match load_for_reading(env, err) {
                Ok(l) => l,
                Err(code) => return code,
            };
            match get_key(&loaded.read.config, key) {
                Err(e) => fail(err, e),
                Ok(value) => {
                    let name = name_of(key);
                    if json_out {
                        let _ = writeln!(out, "{}", json!({"key": name, "value": value}));
                    }
                    match value {
                        Some(v) => {
                            if !json_out {
                                let _ = writeln!(out, "{v}");
                            }
                            0
                        }
                        None => {
                            let note = if name == "default_model" {
                                let n = builtin().default_model().map_or("", |m| m.name.as_str());
                                format!(
                                    "default_model is not set; the built-in default is {DEFAULT_MODEL_ID} ({n})"
                                )
                            } else {
                                format!("{name} is not set")
                            };
                            fail(err, note)
                        }
                    }
                }
            }
        }
        ConfigCommand::Set { key, value } => run_set(env, key, value, json_out, out, err),
        ConfigCommand::Unset { key } => run_unset(env, key, json_out, out, err),
        ConfigCommand::Show => run_show(env, json_out, out, err),
        ConfigCommand::Check { strict } => run_check(env, *strict, json_out, out, err),
        ConfigCommand::Edit => run_edit(env, out, err),
        ConfigCommand::Migrate { dry_run } => run_migrate(env, *dry_run, json_out, out, err),
        ConfigCommand::Docs => {
            let table = reference_markdown();
            if json_out {
                let _ = writeln!(out, "{}", json!({"reference": table}));
            } else {
                let _ = write!(out, "{table}");
            }
            0
        }
    }
}

// ---- ham models ------------------------------------------------------------------------------------------------------------------------

/// The model the next run uses and where the choice came from, or why there is none.
pub fn selected(env: Env<'_>, cfg: &Config) -> Result<(String, ModelSource), ModelError> {
    resolve(builtin(), None, &model_prefs(env, cfg)).map(|r| (r.model.id.clone(), r.source))
}

/// What `ham models` prints for `list`, with the selection computed from `env` and `cfg`. Returns the exit code.
pub fn print_models(
    list: &ModelList,
    env: Env<'_>,
    cfg: &Config,
    json_out: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let choice = selected(env, cfg);
    let sel = choice.as_ref().ok().map(|(id, s)| (id.as_str(), *s));
    if json_out {
        let mut v = serde_json::to_value(list).unwrap_or(Value::Null);
        if let Some(o) = v.as_object_mut() {
            o.insert(
                "selected".into(),
                match &sel {
                    Some((id, s)) => json!({"id": id, "source": s.label()}),
                    None => Value::Null,
                },
            );
            if let Err(e) = &choice {
                o.insert("error".into(), json!(e.to_string()));
            }
        }
        let _ = writeln!(out, "{v}");
    } else {
        let _ = write!(out, "{}", render_models(list, sel));
    }
    match choice {
        Ok(_) => 0,
        Err(e) => {
            let _ = writeln!(err, "ham: {e}");
            ERROR
        }
    }
}

/// A note for stderr when the server's default model is not this build's: the two lists have drifted, and a run still sends the
/// full id this build chose, which the server judges by its own list.
pub fn default_drift(server: &ModelList) -> Option<String> {
    let local = builtin().default_model()?;
    (server.default != local.id).then(|| {
        format!(
            "note: the server's default model is {} and this ham's is {}; the lists differ, so update ham or the stage",
            server.default, local.id
        )
    })
}

/// `ham models`: the compiled-in list.
pub fn run_models(env: Env<'_>, json_out: bool, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let (cfg, _) = match load(env) {
        Ok(x) => x,
        Err(e) => {
            let _ = writeln!(err, "ham: {e}");
            return ERROR;
        }
    };
    print_models(&builtin().list(), env, &cfg, json_out, out, err)
}

// ---- ham config migrate ----------------------------------------------------------------------------------------------------------------

/// What a refusal of `ham config migrate` says and the exit code it ends with: 2 for what the user settles (a link, a name that is not text, a file
/// that differs), 1 for a failure, 0 for "nothing to migrate".
// COMPAT-CAH: this verb ends with the window of the earlier names (VERSION 5.50).
fn migrate_refusal(
    env: Env<'_>,
    refusal: ham_core::config_dir::Refusal,
    dry_run: bool,
    json_out: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    use ham_core::config_dir::{Refusal, dirs};
    let (from, to) = dirs(env).map_or_else(
        || (String::new(), String::new()),
        |d| {
            (
                d.earlier.display().to_string(),
                d.current.display().to_string(),
            )
        },
    );
    let list = |names: &[String]| sanitize_line(&names.join(", "));
    match refusal {
        Refusal::NoLocation => fail(err, "no config location: set XDG_CONFIG_HOME or HOME"),
        Refusal::NothingToMigrate { from } => {
            if json_out {
                let _ = writeln!(
                    out,
                    "{}",
                    json!({"from": from.display().to_string(), "dryRun": dry_run, "copy": [], "alreadyThere": []})
                );
            } else {
                let _ = writeln!(
                    out,
                    "There is nothing to migrate: {} holds none of your ham files.",
                    from.display()
                );
            }
            0
        }
        Refusal::Symlink(names) => {
            let _ = writeln!(
                err,
                "ham: symbolic links stand in the way ({}), and ham does not follow links. Copy those files by hand, or replace each link with the file, then run `ham config migrate` again.",
                list(&names)
            );
            ham_core::exit::USAGE
        }
        Refusal::NotRegular(names) => {
            let _ = writeln!(
                err,
                "ham: {from} holds entries that are neither files nor folders, or whose names are not text ({}). Move them away, then run `ham config migrate` again.",
                list(&names)
            );
            ham_core::exit::USAGE
        }
        Refusal::Differs(names) => {
            let _ = writeln!(
                err,
                "ham: these files are in {to} already and differ from the ones in {from}: {}. ham never overwrites a file. Compare them, move the ones you do not want out of the way, then run `ham config migrate` again.",
                list(&names)
            );
            ham_core::exit::USAGE
        }
        Refusal::Io(e) => fail(err, e),
    }
}

/// `ham config migrate [--dry-run]`: the files of the folder of the earlier command name are copied into the folder of ham
/// (`ham_core::config_dir`). Exit 0 when it copied, had nothing to copy or listed the files of a dry run; 2 for a refusal that is the user's to
/// settle (a link, a file that differs in the new folder); 1 for a failure.
// COMPAT-CAH: this verb ends with the window of the earlier names (VERSION 5.50).
fn run_migrate(
    env: Env<'_>,
    dry_run: bool,
    json_out: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    use ham_core::config_dir::{apply_migrate, plan_migrate};
    let plan = match plan_migrate(env) {
        Ok(p) => p,
        Err(r) => return migrate_refusal(env, r, dry_run, json_out, out, err),
    };
    let files: Vec<&ham_core::config_dir::Item> = plan.copy.iter().filter(|i| !i.is_dir).collect();
    if !dry_run && let Err(r) = apply_migrate(&plan) {
        return migrate_refusal(env, r, dry_run, json_out, out, err);
    }
    let (from, to) = (
        plan.from.display().to_string(),
        plan.to.display().to_string(),
    );
    if json_out {
        let copy: Vec<&str> = files.iter().map(|i| i.rel.as_str()).collect();
        let _ = writeln!(
            out,
            "{}",
            json!({"from": from, "to": to, "dryRun": dry_run, "copy": copy, "alreadyThere": plan.same})
        );
        return 0;
    }
    if dry_run {
        let _ = writeln!(
            out,
            "Dry run: nothing is written. These files would be copied from {from} to {to}:"
        );
    } else if !files.is_empty() {
        let _ = writeln!(out, "Copied from {from} to {to}:");
    }
    for item in &files {
        let _ = writeln!(out, "  {} ({} bytes)", sanitize_line(&item.rel), item.bytes);
    }
    if !plan.same.is_empty() {
        let _ = writeln!(
            out,
            "{} file(s) are in {to} already, byte for byte, and stay as they are.",
            plan.same.len()
        );
    }
    let _ = if files.is_empty() {
        writeln!(
            out,
            "Nothing to copy: every file is in {to} already. The old folder is unchanged."
        )
    } else if dry_run {
        writeln!(
            out,
            "{} file(s) would be copied. The old folder would not change.",
            files.len()
        )
    } else {
        writeln!(
            out,
            "Copied {} file(s). The old folder is unchanged.",
            files.len()
        )
    };
    0
}
