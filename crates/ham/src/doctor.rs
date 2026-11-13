//! `ham doctor`: one `PASS|FAIL <check> <detail>`
//! line per check, then one indented line per repository allowlist entry; with `--json`
//! `{"checks": [{"name","pass","level","detail"}], "allowlist": [RepoAllowView]}` (`level` is `pass`, `warn` or `fail`; a warning
//! has `pass` true). Exit 0 when no check failed, else 1; a `WARN` line is advice and never changes the exit code.
//!
//! - `config`: only when `config.toml` has something to report: one line for each finding of the reader, a FAIL for an error and a WARN for
//!   a warning, and a FAIL when a newer file blocks run starts ([`config_checks`]). A healthy file adds no line. They come first.
//! - `identity`: `GET /v1/whoami` signed with the configured profile: the principal and account.
//! - `api`: the `/v1` endpoint answered (any HTTP answer); a call that never left, or never got an
//!   answer, fails.
//! - `catalog`: `GET /v1/models` has selectable rows (none: FAIL "no selectable model") and a `smokedAt` (none: FAIL). The detail states the
//!   age of the newest `smokedAt` and the check passes whatever it is: the list is compiled into the build, so its records are as old as
//!   the build and nobody can refresh them from here (root invariant 6 asks for a passing record, not a recent one). With a limit,
//!   `ham doctor --catalog-max-age <hours>`, a newest record older than the limit is a WARN, "catalog smoke records are N hours old
//!   (refresh them)"; the models are selectable and the stage works, so the exit code stays 0.
//! - `versions`: `ham v4.01 | backend v4.01`: this client's version and the backend's, as the `x-hamh-backend-version` header of the doctor's
//!   own signed calls reported it. A backend that differs, or reports none (one that predates the tag), is a WARN that says which way
//!   ("backend older than this ham"); it never changes the exit code.
//! - `git-host`: the credential mode per target scheme.
//! - `allowlist`: `GET /v1/repos` answered; the detail is the entry count.
//! - `provider-data-sharing`: whether `run.provider_sharing` is on and which Bedrock data retention mode the account has in the stage's Region,
//!   read with the user's own credentials (`GET /data-retention`). It is advice and never a failure: it warns when the setting is on and the mode does not
//!   let such a model run (a mode other than `provider_data_share` and `aws_review`), says that ham switches it when such a run starts and names the AWS
//!   command the account owner runs, and says `unknown` when the read is denied or does not answer. The doctor itself never changes the mode; the start of
//!   a run does.

use crate::api::{Api, ApiError};
use crate::http::{ControlPlane, Response, TransportError};
use crate::output::Output;
use crate::version;
use ham_core::sanitize::sanitize_line;
use ham_core::settings::Severity;
use harness_protocol::api::v1::RepoAllowView;
use serde_json::json;
use std::cmp::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The git-host credential modes per scheme. Only `cc:` is served: repo-gateway refuses `gh:` and `gl:` at the run's first state, because
/// GitHub and GitLab delivery are not supported. The line says so and the check still passes: it shows how the one served scheme is
/// reached.
pub const GIT_HOST_MODES: &str = "cc: iam (repo-gateway role credentials; no git secret) | gh: not supported | gl: not supported";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    /// False only for a failure; a warning passes.
    pub pass: bool,
    /// A passing check with advice for the operator (`WARN` line, `level` `warn`).
    pub warn: bool,
    pub detail: String,
}

impl Check {
    /// `pass`, `warn` or `fail`.
    pub fn level(&self) -> &'static str {
        match (self.pass, self.warn) {
            (false, _) => "fail",
            (true, true) => "warn",
            (true, false) => "pass",
        }
    }
}

/// The checks, and the allowlist entries the `allowlist` check read (empty when it failed).
#[derive(Clone, Debug, Default)]
pub struct Doctor {
    pub checks: Vec<Check>,
    pub allowlist: Vec<RepoAllowView>,
}

fn check(name: &'static str, pass: bool, detail: impl Into<String>) -> Check {
    Check {
        name,
        pass,
        warn: false,
        detail: sanitize_line(&detail.into()),
    }
}

fn warning(name: &'static str, detail: impl Into<String>) -> Check {
    Check {
        warn: true,
        ..check(name, true, detail)
    }
}

/// The versions check from this client's version and the backend's (`None`: it reported none). Equal versions pass; any difference, and an
/// unknown backend, is advice (a WARN, exit code 0).
pub fn versions_check(client: &str, backend: Option<&str>) -> Check {
    versions_check_for(client, backend, version::Generation::Current)
}

/// [`versions_check`] for the generation of the header the backend's last answer carried. A stage that answers with the header of the earlier
/// names only still works with this `ham`; the line says so, says which `--new-repo` names it takes and what the delivery card of a run on it
/// shows (the default rules put the records under `.ham/`, which that backend does not read), and it is advice, exit code 0.
// COMPAT-CAH: this ends with the window of the earlier names (VERSION 5.50).
pub fn versions_check_for(
    client: &str,
    backend: Option<&str>,
    generation: version::Generation,
) -> Check {
    let check = versions_check_of(client, backend);
    if generation == version::Generation::Current || backend.is_none() {
        return check;
    }
    warning(
        "versions",
        format!(
            "{}; this stage was deployed under the old names. It still works with this ham, and `--new-repo` takes names that start with {}. Until the stage is torn down and redeployed under the new names, the delivery card of a run on it shows \"No test command was found\"; the code and the tests of the run are unaffected.",
            check.detail,
            version::new_repo_prefix(generation)
        ),
    )
}

fn versions_check_of(client: &str, backend: Option<&str>) -> Check {
    let head = format!("ham v{client} | backend v{}", backend.unwrap_or("?.??"));
    let Some(backend) = backend else {
        return warning(
            "versions",
            format!(
                "{head}; the backend reports no version (an older backend, or no answer from it)"
            ),
        );
    };
    match version::compare(client, backend) {
        Some(Ordering::Equal) => check("versions", true, head),
        Some(Ordering::Greater) => {
            warning("versions", format!("{head}; backend older than this ham"))
        }
        Some(Ordering::Less) => warning("versions", format!("{head}; backend newer than this ham")),
        None => warning("versions", format!("{head}; backend differs from this ham")),
    }
}

/// The catalog check with no age limit, which is what `ham doctor` runs by default: the age of the newest record is in the detail and is
/// never a warning. See [`catalog_check_limited`].
pub fn catalog_check(rows: usize, smoked_at: &[&str], now: u64) -> Check {
    catalog_check_limited(rows, smoked_at, now, None)
}

/// The catalog check from the number of selectable rows and the `smokedAt` of each (`now` in Unix seconds). With `max_age_s`, a newest
/// record older than that many seconds warns (an age equal to the limit passes); without it the check never warns about age, because the
/// age of the records says nothing about whether the models can be used. No selectable row and no usable `smokedAt` fail either way.
pub fn catalog_check_limited(
    rows: usize,
    smoked_at: &[&str],
    now: u64,
    max_age_s: Option<u64>,
) -> Check {
    if rows == 0 {
        return check("catalog", false, "no selectable model");
    }
    let Some((t, at)) = smoked_at
        .iter()
        .filter_map(|s| parse_utc(s).map(|t| (t, *s)))
        .max()
    else {
        return check("catalog", false, "no row has a smokedAt time");
    };
    let age = now.saturating_sub(t);
    let detail = format!(
        "{rows} selectable rows, newest smoke {at} ({:.1} h old)",
        age as f64 / 3600.0
    );
    match max_age_s {
        Some(limit) if age > limit => warning(
            "catalog",
            format!(
                "{detail}; catalog smoke records are {} hours old (refresh them)",
                age / 3600
            ),
        ),
        _ => check("catalog", true, detail),
    }
}

/// Unix seconds of `YYYY-MM-DDTHH:MM:SS[.frac]Z`.
pub fn parse_utc(ts: &str) -> Option<u64> {
    let b = ts.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || !ts.ends_with('Z') {
        return None;
    }
    let n = |r: std::ops::Range<usize>| ts.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (hh, mm, ss) = (n(11..13)?, n(14..16)?, n(17..19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hh * 3600 + mm * 60 + ss).ok()
}

/// The current time in Unix seconds (0 when the clock is before 1970).
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The Bedrock data retention mode in which a model that needs provider data sharing can run.
pub const SHARING_MODE: &str = "provider_data_share";

/// The `provider-data-sharing` line from one read of the account's mode. `region` is the Region that was asked, `setting_on` is `run.provider_sharing`,
/// and `read` is the answer or the transport failure. It is never a failure: a read that was denied or did not answer says `unknown`, and only the setting
/// on with a mode in which such a model does not run (`ham_core::sharing::mode_runs`) is a warning, with the command the account owner runs.
pub fn sharing_check_of(
    region: &str,
    setting_on: bool,
    read: Result<&Response, &TransportError>,
) -> Check {
    const NAME: &str = "provider-data-sharing";
    let state = if setting_on { "on" } else { "off" };
    let unknown = |why: String| {
        check(
            NAME,
            true,
            format!(
                "run.provider_sharing is {state}; the Bedrock data retention mode in {region} is unknown ({why})"
            ),
        )
    };
    let response = match read {
        Ok(r) => r,
        Err(e) => return unknown(e.to_string()),
    };
    if response.status == 403 {
        return unknown(
            "your credentials were denied; they need bedrock:GetAccountDataRetention".into(),
        );
    }
    if !(200..300).contains(&response.status) {
        return unknown(format!("HTTP {}", response.status));
    }
    let mode = serde_json::from_slice::<serde_json::Value>(&response.body)
        .ok()
        .and_then(|v| v.get("mode").and_then(|m| m.as_str()).map(str::to_string));
    let Some(mode) = mode else {
        return unknown("the answer holds no mode".into());
    };
    let head = format!(
        "run.provider_sharing is {state}; the Bedrock data retention mode in {region} is {mode}"
    );
    match (ham_core::sharing::mode_runs(&mode), setting_on) {
        (true, true) => check(
            NAME,
            true,
            format!("{head}; a model that needs provider data sharing can run"),
        ),
        (true, false) => check(
            NAME,
            true,
            format!(
                "{head}; a run on a model that needs it asks first in the terminal app, or needs the setting on"
            ),
        ),
        (false, true) => warning(
            NAME,
            format!(
                "{head}; ham switches it to {SHARING_MODE} when a run on a model that needs provider data sharing starts, if your credentials allow bedrock:PutAccountDataRetention. The account owner can also run: aws bedrock put-account-data-retention --region {region} --mode {SHARING_MODE}"
            ),
        ),
        (false, false) => check(NAME, true, head),
    }
}

/// The `provider-data-sharing` line for the backend `api` talks to, or `None` for a plane with no credentials of its own.
pub async fn sharing_check<P: ControlPlane>(api: &Api<P>, setting_on: bool) -> Option<Check> {
    let answer = api.plane().data_retention(api.target()).await?;
    Some(match &answer {
        Ok(a) => sharing_check_of(&a.region, setting_on, Ok(&a.response)),
        Err(e) => sharing_check_of("the stage's Region", setting_on, Err(e)),
    })
}

/// The checks against `api` (`profile` names the credentials in the failure text), with no catalog age limit.
pub async fn checks<P: ControlPlane>(api: &Api<P>, endpoint: &str, profile: &str) -> Doctor {
    checks_at(api, endpoint, profile, None, unix_now()).await
}

/// [`checks`] with the catalog age limit of `ham doctor --catalog-max-age <hours>` and the clock (`now`, Unix seconds) given. The hours
/// become seconds with a saturating product, so the largest value means "no record is old enough" and never overflows.
pub async fn checks_at<P: ControlPlane>(
    api: &Api<P>,
    endpoint: &str,
    profile: &str,
    catalog_max_age_hours: Option<u64>,
    now: u64,
) -> Doctor {
    let max_age_s = catalog_max_age_hours.map(|h| h.saturating_mul(3600));
    let mut out = Vec::new();
    match api.whoami().await {
        Ok(w) => {
            out.push(check(
                "identity",
                true,
                format!("{} account {}", w.principal_arn, w.account),
            ));
            out.push(check("api", true, format!("{endpoint} answered")));
        }
        Err(e) => {
            out.push(check("identity", false, format!("profile {profile}: {e}")));
            out.push(match &e {
                ApiError::Http { status, .. } => {
                    check("api", true, format!("{endpoint} answered HTTP {status}"))
                }
                _ => check("api", false, format!("{endpoint} not reached: {e}")),
            });
        }
    }
    // The versions line sits after identity and api; it is written once every call has answered, so it holds the newest header seen.
    let versions_at = out.len();
    out.push(match api.models().await {
        Ok(m) => {
            let smoked: Vec<&str> = m.rows.iter().map(|r| r.smoked_at.as_str()).collect();
            catalog_check_limited(m.rows.len(), &smoked, now, max_age_s)
        }
        Err(e) => check("catalog", false, format!("GET /v1/models: {e}")),
    });
    out.push(check("git-host", true, GIT_HOST_MODES));
    let allowlist = match api.list_repos().await {
        Ok(r) => {
            out.push(check("allowlist", true, format!("{} entries", r.len())));
            r
        }
        Err(e) => {
            out.push(check("allowlist", false, e.to_string()));
            Vec::new()
        }
    };
    out.insert(
        versions_at,
        versions_check_for(
            version::CLIENT,
            version::backend().as_deref(),
            version::generation(),
        ),
    );
    Doctor {
        checks: out,
        allowlist,
    }
}

/// How long one backend has for all of its checks when `ham doctor` holds several backends.
pub const DOCTOR_DEADLINE: Duration = Duration::from_secs(20);

/// One backend `ham doctor` checks: its alias, its address and the profile that signs for it (the last two for the words of a failure).
#[derive(Clone, Debug)]
pub struct DoctorTarget {
    pub alias: String,
    pub endpoint: String,
    pub profile: String,
}

fn within(deadline: Duration) -> String {
    if deadline.as_millis() < 1000 {
        format!("{} ms", deadline.as_millis())
    } else {
        format!("{} s", deadline.as_secs())
    }
}

/// The checks of several backends: every backend is checked at once, each with its own `deadline` for all of its checks, so one
/// backend that does not answer costs one deadline and no more. The result holds the checks of each backend in the order of `targets`, every
/// detail led by the alias, and a versions line of each from what its own answers reported. A backend that ran out of time has a failed identity
/// line and a failed api line that name the deadline, and is marked slow for the banners.
pub async fn checks_many<P: ControlPlane>(
    api: &Api<P>,
    targets: &[DoctorTarget],
    catalog_max_age_hours: Option<u64>,
    now: u64,
    deadline: Duration,
) -> Doctor {
    let mut set = tokio::task::JoinSet::new();
    for (i, t) in targets.iter().cloned().enumerate() {
        let one = api.on(&t.alias);
        set.spawn(async move {
            let r = tokio::time::timeout(
                deadline,
                checks_at(&one, &t.endpoint, &t.profile, catalog_max_age_hours, now),
            )
            .await;
            (i, r.ok())
        });
    }
    let mut results: Vec<Option<Option<Doctor>>> = (0..targets.len()).map(|_| None).collect();
    while let Some(joined) = set.join_next().await {
        if let Ok((i, r)) = joined {
            results[i] = Some(r);
        }
    }
    let mut all = Doctor::default();
    for (t, result) in targets.iter().zip(results) {
        let mut d = match result {
            Some(Some(d)) => d,
            _ => {
                api.plane().note_slow(&t.alias);
                let why = format!("no answer in {}", within(deadline));
                Doctor {
                    checks: vec![
                        check("identity", false, format!("profile {}: {why}", t.profile)),
                        check("api", false, format!("{} not reached: {why}", t.endpoint)),
                    ],
                    allowlist: Vec::new(),
                }
            }
        };
        d.checks.retain(|c| c.name != "versions");
        let seen = api
            .plane()
            .backend_view()
            .and_then(|v| v.info(&t.alias).and_then(|i| i.version.clone()));
        d.checks
            .push(versions_check(version::CLIENT, seen.as_deref()));
        for c in &mut d.checks {
            c.detail = format!("{}: {}", t.alias, c.detail);
        }
        all.checks.append(&mut d.checks);
        if all.allowlist.is_empty() {
            all.allowlist = d.allowlist;
        }
    }
    all
}

/// The lines `ham doctor` adds for the config file: one for each finding (a warning passes), one more when a newer file blocks run starts.
/// A file that is absent or has nothing to report adds none, so the output of a healthy machine is what it always was.
pub fn config_checks(env: &dyn Fn(&str) -> Option<String>) -> Vec<Check> {
    let line = |pass: bool, warn: bool, detail: String| Check {
        name: "config",
        pass,
        warn,
        detail: sanitize_line(&detail),
    };
    let loaded = match crate::config_cmd::load_file(env) {
        Ok(l) => l,
        Err(e) => return vec![line(false, false, e)],
    };
    let mut checks: Vec<Check> = loaded
        .read
        .findings
        .iter()
        .map(|f| {
            let warning = f.severity == Severity::Warning;
            line(warning, warning, f.render())
        })
        .collect();
    if let Some(why) = crate::config_cmd::blocks_runs(&loaded) {
        checks.push(line(false, false, why));
    }
    checks
}

/// One allowlist entry as `ham doctor` prints it, indented under the checks.
pub fn entry_line(e: &RepoAllowView) -> String {
    format!(
        "  {} added by {} at {}",
        e.target,
        sanitize_line(&e.added_by),
        sanitize_line(&e.added_at)
    )
}

/// Writes the checks and the allowlist; the exit code.
pub fn report(d: &Doctor, json: bool, out: &Output) -> i32 {
    let checks = &d.checks;
    if json {
        let c: Vec<serde_json::Value> = checks
            .iter()
            .map(
                |c| json!({"name": c.name, "pass": c.pass, "level": c.level(), "detail": c.detail}),
            )
            .collect();
        out.out_line(json!({ "checks": c, "allowlist": d.allowlist }));
    } else {
        for c in checks {
            out.out_line(format!(
                "{} {} {}",
                c.level().to_uppercase(),
                c.name,
                c.detail
            ));
        }
        for e in &d.allowlist {
            out.out_line(entry_line(e));
        }
    }
    if checks.iter().all(|c| c.pass) {
        0
    } else {
        ham_core::exit::ERROR
    }
}
