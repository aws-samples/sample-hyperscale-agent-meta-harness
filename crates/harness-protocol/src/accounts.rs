// The `accounts.json` selection rule, shared by `build.rs` (through `include!`,
// so no inner doc comment and no `crate::` path here) and the crate (`harness_protocol::accounts`).
// It mirrors `scripts/lib/aws_target.py`: the same file rules, and `HAMH_AWS_PROFILE` names the
// entry by its profile, else the one entry whose role is `active`.
// COMPAT-CAH: the earlier spellings of the two variables and of the selector word of a stored file are read until VERSION 5.50.

/// The kind of IAM identity an entry's optional `operator` field names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperatorKind {
    /// An IAM role, by its name in the assumed-role ARN (an IAM Identity Center role is named `AWSReservedSSO_<permission set>_<hash>`).
    Role,
    /// An IAM user, by its user name.
    User,
}

/// The identity an entry's optional `operator` field names: the one that runs setup and later operates the stage
/// (`scripts/lib/aws_target.py` and `infra/lib/operator.ts` apply the same rules to the same words).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Operator {
    pub kind: OperatorKind,
    pub name: String,
}

/// The criteria fields of one `accounts.json` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountEntry {
    pub account: String,
    pub region: String,
    pub profile: String,
    pub role: String,
    /// The optional `operator`; an entry without it keeps the IAM role `Admin`.
    pub operator: Option<Operator>,
}

/// The variable that selects an entry by its profile.
pub const ACCOUNT_SELECTOR: &str = "HAMH_AWS_PROFILE";

/// The earlier spelling of [`ACCOUNT_SELECTOR`] (COMPAT-CAH): read when the new one is unset, and the `selector` word of an account file that
/// was generated and stored before the rename. New files carry [`ACCOUNT_SELECTOR`].
pub const ACCOUNT_SELECTOR_EARLIER: &str = "CAH_AWS_PROFILE";

/// The variable that names an accounts file to read in place of the committed `accounts.json`.
pub const ACCOUNTS_FILE_ENV: &str = "HAMH_ACCOUNTS_FILE";

/// The earlier spelling of [`ACCOUNTS_FILE_ENV`] (COMPAT-CAH).
pub const ACCOUNTS_FILE_ENV_EARLIER: &str = "CAH_ACCOUNTS_FILE";

/// The value of a variable that has two spellings, as `(value, notice)`. The new name wins; the earlier name is read when the new one is unset
/// or empty, and then `notice` says to use the new name (the caller prints it once). An empty value counts as unset, so the result is empty
/// when neither is set. Two different non-empty values are an error that names both: a variable that selects an account is never ignored and
/// never falls through to the default entry (COMPAT-CAH).
pub fn paired_variable(
    new_name: &str,
    new_value: Option<&std::ffi::OsStr>,
    old_name: &str,
    old_value: Option<&std::ffi::OsStr>,
) -> Result<(String, Option<String>), String> {
    let text = |name: &str, v: Option<&std::ffi::OsStr>| -> Result<String, String> {
        match v {
            None => Ok(String::new()),
            Some(v) => v
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{name} is not UTF-8 text")),
        }
    };
    let (new, old) = (text(new_name, new_value)?, text(old_name, old_value)?);
    match (new.is_empty(), old.is_empty()) {
        (true, true) => Ok((String::new(), None)),
        (false, true) => Ok((new, None)),
        (true, false) => Ok((
            old,
            Some(format!(
                "{old_name} is read; use {new_name} (the earlier name stops working at VERSION 5.50)"
            )),
        )),
        (false, false) if new == old => Ok((new, None)),
        (false, false) => Err(format!(
            "{new_name} and {old_name} are both set to different values; unset {old_name}, which is the earlier name of {new_name}"
        )),
    }
}

const ACCOUNT_ROLES: [&str; 3] = ["active", "history", "test"];
const ACCOUNT_STAGES: [&str; 3] = ["live", "drill", "direct"];
/// The stage every file names: the stage the product deploys. A file may name `live` and `drill` as well.
const DEPLOYED_STAGE: &str = "direct";
const LOGGING_MODES: [&str; 2] = ["enable-and-restore", "read-only"];

fn is_region_name(s: &str) -> bool {
    // `[a-z]{2}(-[a-z]+)+-[0-9]`, as aws_target.py checks it.
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() >= 3
        && parts[0].len() == 2
        && parts[0].bytes().all(|b| b.is_ascii_lowercase())
        && parts[1..parts.len() - 1]
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase()))
        && parts[parts.len() - 1].len() == 1
        && parts[parts.len() - 1].bytes().all(|b| b.is_ascii_digit())
}

fn stage_lists(
    e: &serde_json::Value,
    key: &str,
    at: &str,
) -> Result<Vec<(String, Vec<String>)>, String> {
    let bad =
        || format!("{at}: {key} must name the stage {DEPLOYED_STAGE}, and may name live and drill");
    let m = e.get(key).and_then(|v| v.as_object()).ok_or_else(bad)?;
    if !m.contains_key(DEPLOYED_STAGE) || !m.keys().all(|k| ACCOUNT_STAGES.contains(&k.as_str())) {
        return Err(bad());
    }
    let mut out = Vec::new();
    for s in ACCOUNT_STAGES.into_iter().filter(|s| m.contains_key(*s)) {
        let names = m[s]
            .as_array()
            .and_then(|a| {
                a.iter()
                    .map(|v| v.as_str().filter(|x| !x.is_empty()).map(str::to_owned))
                    .collect::<Option<Vec<String>>>()
            })
            .ok_or_else(|| format!("{at}: {key}.{s} must be a list of names"))?;
        out.push((s.to_owned(), names));
    }
    Ok(out)
}

/// The operator an entry names, or a refusal that starts with `at` (the place in the file).
fn parse_operator(raw: &serde_json::Value, at: &str) -> Result<Operator, String> {
    let Some(o) = raw.as_object() else {
        return Err(format!(
            "{at}: operator must be an object with kind (role or user) and name (an IAM name), like {{\"kind\": \"role\", \"name\": \"Admin\"}}"
        ));
    };
    let kind = match o.get("kind").and_then(|v| v.as_str()) {
        Some("role") => OperatorKind::Role,
        Some("user") => OperatorKind::User,
        _ => return Err(format!("{at}: operator.kind must be role or user")),
    };
    let name = o
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|n| {
            (1..=64).contains(&n.len())
                && n.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"+=.@_-".contains(&b))
        })
        .ok_or_else(|| {
            format!(
                "{at}: operator.name must be 1 to 64 letters, digits or + = . @ _ - (a name, not a path, a pattern or an ARN)"
            )
        })?;
    if o.len() != 2 {
        return Err(format!("{at}: operator has the keys kind and name only"));
    }
    Ok(Operator {
        kind,
        name: name.to_owned(),
    })
}

/// The caller ARN patterns of the operator an entry names (`None`: the IAM role `Admin`, which the stage trusted before the field existed), as
/// `infra/lib/operator.ts` `callerArnPatterns` writes them for control-api: the session ARN of a role, the user ARN of a user with and without a path.
/// The first pattern is the pool member the `harness-protocol` build writes as `OPERATOR_POOL_MEMBER`.
pub fn operator_pool_members(account: &str, operator: Option<&Operator>) -> Vec<String> {
    match operator {
        None => vec![format!("arn:aws:sts::{account}:assumed-role/Admin/*")],
        Some(Operator {
            kind: OperatorKind::Role,
            name,
        }) => vec![format!("arn:aws:sts::{account}:assumed-role/{name}/*")],
        Some(Operator {
            kind: OperatorKind::User,
            name,
        }) => vec![
            format!("arn:aws:iam::{account}:user/{name}"),
            format!("arn:aws:iam::{account}:user/*/{name}"),
        ],
    }
}

fn account_entry(e: &serde_json::Value, i: usize) -> Result<AccountEntry, String> {
    let at = format!("accounts[{i}]");
    let text = |k: &str| {
        e.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned()
    };
    let (account, region, profile, role) = (
        text("account"),
        text("region"),
        text("profile"),
        text("role"),
    );
    if account.len() != 12 || !account.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{at}: account must be 12 digits"));
    }
    if !is_region_name(&region) {
        return Err(format!("{at}: region {region:?} is not a Region name"));
    }
    if profile.is_empty()
        || !profile
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(format!("{at}: profile must be a profile name"));
    }
    if !ACCOUNT_ROLES.contains(&role.as_str()) {
        return Err(format!("{at}: role must be one of active, history, test"));
    }
    if !LOGGING_MODES.contains(&text("invocationLogging").as_str()) {
        return Err(format!(
            "{at}: invocationLogging must be one of enable-and-restore, read-only"
        ));
    }
    let (azs, ids) = (stage_lists(e, "azs", &at)?, stage_lists(e, "azIds", &at)?);
    if azs.iter().map(|(s, _)| s).ne(ids.iter().map(|(s, _)| s)) {
        return Err(format!("{at}: azs and azIds must name the same stages"));
    }
    for ((stage, names), (_, az_ids)) in azs.iter().zip(&ids) {
        if names.is_empty() || names.len() != az_ids.len() {
            return Err(format!(
                "{at}: azs.{stage} and azIds.{stage} must pair up one to one"
            ));
        }
        for az in names {
            let suffix = az.strip_prefix(region.as_str()).unwrap_or_default();
            if suffix.len() != 1 || !suffix.bytes().all(|b| b.is_ascii_lowercase()) {
                return Err(format!("{at}: {az} is not in {region}"));
            }
        }
    }
    let operator = e
        .get("operator")
        .map(|raw| parse_operator(raw, &at))
        .transpose()?;
    Ok(AccountEntry {
        account,
        region,
        profile,
        role,
        operator,
    })
}

/// Every entry of an `accounts.json` text, in file order, after the file rules.
pub fn parse_accounts(text: &str) -> Result<Vec<AccountEntry>, String> {
    let doc: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let selector = doc.get("selector").and_then(|v| v.as_str());
    if doc.get("schemaVersion").and_then(|v| v.as_u64()) != Some(1)
        || ![Some(ACCOUNT_SELECTOR), Some(ACCOUNT_SELECTOR_EARLIER)].contains(&selector)
    {
        return Err(format!(
            "expected schemaVersion 1 and selector {ACCOUNT_SELECTOR}"
        ));
    }
    let raw = doc
        .get("accounts")
        .and_then(|v| v.as_array())
        .filter(|a| !a.is_empty())
        .ok_or("accounts must be a non-empty list")?;
    let out = raw
        .iter()
        .enumerate()
        .map(|(i, e)| account_entry(e, i))
        .collect::<Result<Vec<_>, _>>()?;
    for (i, t) in out.iter().enumerate() {
        if out[..i].iter().any(|u| u.profile == t.profile) {
            return Err(format!("profile {} twice", t.profile));
        }
        if out[..i]
            .iter()
            .any(|u| (&u.account, &u.region) == (&t.account, &t.region))
        {
            return Err("an account and Region pair appears twice".to_owned());
        }
    }
    if out.iter().filter(|t| t.role == "active").count() != 1 {
        return Err("exactly one active entry is required".to_owned());
    }
    Ok(out)
}

/// The entry `selector` (the `HAMH_AWS_PROFILE` value) names by profile; empty selects the active
/// entry. `file` names the accounts file in the refusal.
pub fn select_account(
    entries: &[AccountEntry],
    selector: &str,
    file: &str,
) -> Result<AccountEntry, String> {
    let found = if selector.is_empty() {
        entries.iter().find(|t| t.role == "active")
    } else {
        entries.iter().find(|t| t.profile == selector)
    };
    found.cloned().ok_or_else(|| {
        let known: Vec<&str> = entries.iter().map(|t| t.profile.as_str()).collect();
        format!(
            "{ACCOUNT_SELECTOR}={selector} names no entry of {file} (profiles: {})",
            known.join(", ")
        )
    })
}

/// The accounts file a build reads: the override (the `HAMH_ACCOUNTS_FILE` value) when it is set and not
/// empty, which must be an absolute path because cargo runs a build script in the crate directory and a
/// relative path would name another file than the wrapper script meant; else the committed
/// `accounts.json` two directories above `manifest_dir`.
pub fn accounts_file_path(
    override_value: Option<&std::ffi::OsStr>,
    manifest_dir: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    match override_value.filter(|v| !v.is_empty()) {
        None => Ok(manifest_dir.join("../../accounts.json")),
        Some(v) => {
            let path = std::path::PathBuf::from(v);
            if path.is_absolute() {
                Ok(path)
            } else {
                Err(format!(
                    "{ACCOUNTS_FILE_ENV} must be an absolute path (got {v:?})"
                ))
            }
        }
    }
}
