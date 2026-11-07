//! The repository-name rule of a stage: which CodeCommit repository names an operator may allow for pull requests.
//!
//! The rule is a list of 1 to 8 entries. An entry is a repository name (1 to 100 letters, digits, `.`, `_` or `-`), such a prefix of up to 99 characters
//! followed by one `*`, or `*` alone. A name is covered when it equals an exact entry, starts with the prefix of a prefix entry, or an entry is `*`; names are
//! case sensitive. The CDK app sets the list as the environment variable `HAMH_REPO_NAMES` (comma separated) from the optional `repoNames` of the account-file
//! entry; without it the rule is the default `hamh-e2e-*`, which is the rule the product had before it was a setting. The `control-api` crate holds a copy of this
//! file (a Lambda crate does not depend on another), and `infra/test/fixtures/repo-names.json` is the table both copies, `infra/lib/repo-names.ts` and
//! `scripts/lib/aws_target.py` run. The rule decides which repository a run may reach; the names the harness gives its own repositories (`--new-repo`) are
//! covered whatever the rule says (`checks::codecommit_name_in`).

/// The environment variable the CDK app sets.
pub const ENV: &str = "HAMH_REPO_NAMES";
/// The entry of the default rule.
pub const DEFAULT_ENTRY: &str = "hamh-e2e-*";
/// The prefix of the default rule, as the refusal of the default rule names it.
const DEFAULT_PREFIX: &str = "hamh-e2e-";
/// The prefix of the default rule before the rename to `hamh-e2e-`. Repositories the earlier release made keep it.
pub const EARLIER_DEFAULT_PREFIX: &str = "cah-e2e-"; // COMPAT-CAH-DATA
const MAX_ENTRIES: usize = 8;

const ENTRY_RULE: &str = "a repository name (1 to 100 letters, digits, '.', '_' or '-'), such a prefix followed by '*', or '*'";

/// The rule of a stage. `Default` is `hamh-e2e-*`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoNames(Vec<String>);

impl Default for RepoNames {
    fn default() -> Self {
        RepoNames(vec![DEFAULT_ENTRY.to_string()])
    }
}

fn name_chars(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn entry_ok(e: &str) -> bool {
    if e == "*" {
        return true;
    }
    match e.strip_suffix('*') {
        Some(prefix) => (1..=99).contains(&prefix.len()) && name_chars(prefix),
        None => (1..=100).contains(&e.len()) && name_chars(e),
    }
}

impl RepoNames {
    /// The rule of a list of entries, or the reason it is not one (the words of `infra/test/fixtures/repo-names.json`).
    pub fn from_entries<S: AsRef<str>>(entries: &[S]) -> Result<RepoNames, String> {
        if entries.is_empty() || entries.len() > MAX_ENTRIES {
            return Err(
                "repoNames must be a list of 1 to 8 repository names, like [\"my-app\", \"team-*\"]"
                    .into(),
            );
        }
        for e in entries {
            let e = e.as_ref();
            if !entry_ok(e) {
                return Err(format!(
                    "repoNames entry {} must be {ENTRY_RULE}",
                    serde_json::Value::from(e)
                ));
            }
        }
        let mut seen: Vec<&str> = Vec::new();
        for e in entries {
            let e = e.as_ref();
            if seen.contains(&e) {
                return Err(format!(
                    "repoNames lists {} twice",
                    serde_json::Value::from(e)
                ));
            }
            seen.push(e);
        }
        Ok(RepoNames(
            entries.iter().map(|e| e.as_ref().to_string()).collect(),
        ))
    }

    /// The rule of a comma separated list; spaces around an entry are dropped.
    pub fn parse(setting: &str) -> Result<RepoNames, String> {
        let entries: Vec<&str> = setting.split(',').map(str::trim).collect();
        RepoNames::from_entries(&entries)
    }

    /// The rule the function environment gives: `HAMH_REPO_NAMES` when it is set and not empty, else the default. A value that is not a rule is a refusal that
    /// names the variable.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<RepoNames, String> {
        match get(ENV).filter(|v| !v.is_empty()) {
            None => Ok(RepoNames::default()),
            Some(v) => RepoNames::parse(&v).map_err(|e| format!("{ENV}: {e}")),
        }
    }

    pub fn entries(&self) -> &[String] {
        &self.0
    }

    /// Whether this is the default rule (`hamh-e2e-*` alone).
    pub fn is_default(&self) -> bool {
        self.0.len() == 1 && self.0[0] == DEFAULT_ENTRY
    }

    /// Whether the rule covers a repository name.
    pub fn allows(&self, name: &str) -> bool {
        self.0.iter().any(|e| match e.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => name == e,
        })
    }

    /// Whether `name` is a repository made under the earlier names that this rule still covers: the rule lists the default entry (`hamh-e2e-*`, which stood for
    /// the earlier release's prefix), alone or among other entries, and `name` starts with [`EARLIER_DEFAULT_PREFIX`]. The rule's own entries list the new
    /// prefix only, and a rule without the default entry covers exactly what it lists. The allowlist and the repository's ARN check still decide which
    /// repository a run reaches.
    pub fn allows_earlier_name(&self, name: &str) -> bool {
        self.0.iter().any(|e| e == DEFAULT_ENTRY) && name.starts_with(EARLIER_DEFAULT_PREFIX)
    }

    /// The exact reason for a name the rule does not cover. The default rule keeps the text it had as a fixed prefix.
    pub fn refusal(&self, name: &str) -> String {
        if self.is_default() {
            format!("refused: cc:{name} is outside the {DEFAULT_PREFIX} prefix")
        } else {
            format!(
                "refused: cc:{name} is outside the repository names this stage allows ({})",
                self.0.join(", ")
            )
        }
    }
}
