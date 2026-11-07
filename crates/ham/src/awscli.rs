//! ham's use of the AWS CLI for the profiles it cannot read itself: IAM Identity Center (SSO) and assume-role profiles.
//!
//! ham runs `aws configure export-credentials --profile <name> --format process` (AWS CLI 2.9.0 or later) and reads the credentials JSON
//! it prints, which is the shape a `credential_process` program prints: `Version` 1, `AccessKeyId`, `SecretAccessKey` and, when the
//! credentials are temporary, `SessionToken` and `Expiration` (RFC 3339). The AWS CLI does the sign-in work: it reads the SSO token that
//! `aws sso login` cached, or assumes the role from the source profile. ham makes no network call of its own.
//!
//! The program is `aws` on the `PATH`, or the one the variable `HAMH_AWS_CLI` names. The command gets [`TIME_LIMIT`] (30 seconds) and is
//! killed when it takes longer. Nothing it prints on standard output is ever shown or put in an error: the credentials go straight into the
//! signer. An error carries the AWS CLI's own words (standard error, cut short), or a short sentence of ours when the AWS CLI is missing,
//! too old, or the SSO sign-in has ended.

use crate::awsprofile::ProfileKind;
use aws_credential_types::Credentials;
use serde_json::Value;
use std::ffi::OsString;
use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// The variable that names the AWS CLI program to run instead of `aws` on the `PATH`.
pub const ENV_VAR: &str = "HAMH_AWS_CLI";
/// The name the variable had before the rename. It answers while [`ENV_VAR`] is unset or empty (`ham_core::envnames`).
const EARLIER_ENV_VAR: &str = "CAH_AWS_CLI"; // COMPAT-CAH
/// How long the AWS CLI may take to print the credentials.
pub const TIME_LIMIT: Duration = Duration::from_secs(30);
/// How long `aws --version` may take when ham looks for the reason the command failed.
const VERSION_LIMIT: Duration = Duration::from_secs(10);
/// Where to install or upgrade the AWS CLI.
pub const INSTALL_URL: &str =
    "https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html";
/// The first AWS CLI version with `aws configure export-credentials`.
pub const MIN_VERSION: (u32, u32, u32) = (2, 9, 0);
/// Standard output and standard error are read up to this many bytes each; the rest is read and thrown away.
const KEEP: usize = 64 * 1024;
/// An error quotes at most this many characters of what the AWS CLI said.
const SAID_LIMIT: usize = 400;
/// Credentials provider name recorded on the credentials.
const PROVIDER: &str = "ham-aws-cli";

/// Where the program name came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    /// `aws`, looked up on the `PATH`.
    Path,
    /// The variable [`ENV_VAR`].
    Variable,
}

/// The AWS CLI program ham runs.
#[derive(Clone, Debug)]
pub struct Program {
    command: OsString,
    origin: Origin,
}

impl Program {
    /// The program [`ENV_VAR`] names when it is set and not empty, else `aws` on the `PATH`.
    pub fn from_env() -> Self {
        Self::from_vars(&|name| std::env::var_os(name))
    }

    /// [`Program::from_env`] over any lookup of variables (a test passes its own). The earlier name of the variable answers while [`ENV_VAR`] is
    /// unset or empty.
    pub fn from_vars(var: &dyn Fn(&str) -> Option<OsString>) -> Self {
        let set = |name: &str| var(name).filter(|v| !v.is_empty());
        match set(ENV_VAR).or_else(|| set(EARLIER_ENV_VAR)) {
            Some(command) => Self::named(command),
            None => Self {
                command: OsString::from("aws"),
                origin: Origin::Path,
            },
        }
    }

    /// The program that runs: the variable's value or `aws`.
    pub fn command(&self) -> &std::ffi::OsStr {
        &self.command
    }

    /// The program at this path or with this name, as if [`ENV_VAR`] named it.
    pub fn named(command: impl Into<OsString>) -> Self {
        Self {
            command: command.into(),
            origin: Origin::Variable,
        }
    }

    fn shown(&self) -> String {
        Path::new(&self.command).display().to_string()
    }
}

// ---- what the AWS CLI printed ------------------------------------------------------------------------------------------------------------

/// `Y-M-D` to days since 1970-01-01 (H. Hinnant's days_from_civil).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

fn digits(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// An RFC 3339 time (`2099-01-01T00:00:00Z`, `2099-01-01T00:00:00+00:00`, with or without a fraction of a second, any offset), as the AWS
/// CLI prints the expiry of credentials. `None` for anything else.
pub fn parse_rfc3339(text: &str) -> Option<SystemTime> {
    let b = text.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let (y, mo, d) = (
        digits(text.get(0..4)?)?,
        digits(text.get(5..7)?)?,
        digits(text.get(8..10)?)?,
    );
    let (h, mi, s) = (
        digits(text.get(11..13)?)?,
        digits(text.get(14..16)?)?,
        digits(text.get(17..19)?)?,
    );
    if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    let mut rest = text.get(19..)?;
    if let Some(fraction) = rest.strip_prefix('.') {
        let n = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if n == 0 {
            return None;
        }
        rest = fraction.get(n..)?;
    }
    let offset = if rest == "Z" {
        0
    } else {
        let sign = match rest.as_bytes().first()? {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        if rest.len() != 6 || rest.as_bytes()[3] != b':' {
            return None;
        }
        let (oh, om) = (digits(rest.get(1..3)?)?, digits(rest.get(4..6)?)?);
        if oh > 23 || om > 59 {
            return None;
        }
        sign * (oh * 3_600 + om * 60)
    };
    let secs = days_from_civil(y, mo, d) * 86_400 + h * 3_600 + mi * 60 + s - offset;
    match u64::try_from(secs) {
        Ok(up) => SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(up)),
        Err(_) => SystemTime::UNIX_EPOCH.checked_sub(Duration::from_secs(secs.unsigned_abs())),
    }
}

fn not_credentials_json(reason: &str) -> String {
    format!("the AWS CLI printed something that is not credentials JSON ({reason})")
}

/// The credentials in what `aws configure export-credentials --format process` printed. The error never holds a part of the text.
pub fn parse_credentials(text: &str) -> Result<Credentials, String> {
    let value: Value =
        serde_json::from_str(text).map_err(|_| not_credentials_json("it is not JSON"))?;
    let object = value
        .as_object()
        .ok_or_else(|| not_credentials_json("it is not an object"))?;
    let get = |name: &str| {
        object
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v)
    };
    if get("Version").and_then(Value::as_i64) != Some(1) {
        return Err(not_credentials_json("Version is not 1"));
    }
    let required = |name: &str| {
        get(name)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .ok_or_else(|| not_credentials_json(&format!("{name} is missing")))
    };
    let key_id = required("AccessKeyId")?;
    let secret = required("SecretAccessKey")?;
    let token = match get("SessionToken") {
        None | Some(Value::Null) => None,
        Some(Value::String(t)) => Some(t.clone()),
        Some(_) => return Err(not_credentials_json("SessionToken is not text")),
    };
    let expiry = match get("Expiration") {
        None | Some(Value::Null) => None,
        Some(Value::String(t)) => Some(
            parse_rfc3339(t)
                .ok_or_else(|| not_credentials_json("Expiration is not an RFC 3339 time"))?,
        ),
        Some(_) => return Err(not_credentials_json("Expiration is not text")),
    };
    Ok(Credentials::new(key_id, secret, token, expiry, PROVIDER))
}

/// The version in the first `aws-cli/<major>.<minor>[.<patch>]` word of what `aws --version` printed (the AWS CLI 1.x prints it on standard
/// error, 2.x on standard output; the caller passes both). `None` when there is no such word.
pub fn aws_cli_version(text: &str) -> Option<(u32, u32, u32)> {
    let word = text
        .split_whitespace()
        .find_map(|w| w.strip_prefix("aws-cli/"))?;
    let mut parts = word.split('.').map(|p| {
        let n = p.bytes().take_while(u8::is_ascii_digit).count();
        p.get(..n).filter(|d| !d.is_empty())?.parse::<u32>().ok()
    });
    let major = parts.next()??;
    let minor = parts.next()??;
    let patch = parts.next().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

// ---- running the command ----------------------------------------------------------------------------------------------------------------

/// What a finished command left.
struct Finished {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

enum RunError {
    NotFound,
    Spawn(std::io::Error),
    TimedOut,
}

/// A thread that reads a pipe to its end, keeping the first [`KEEP`] bytes. The bytes can be read at any time, also while the pipe is open.
struct Reader {
    kept: Arc<Mutex<Vec<u8>>>,
    done: Arc<AtomicBool>,
}

impl Reader {
    fn start<R: Read + Send + 'static>(mut pipe: R) -> Self {
        let kept = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        let (k, d) = (Arc::clone(&kept), Arc::clone(&done));
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            while let Ok(n) = pipe.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                if let Ok(mut buf) = k.lock() {
                    let room = KEEP.saturating_sub(buf.len());
                    buf.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
            d.store(true, Ordering::SeqCst);
        });
        Self { kept, done }
    }

    /// Waits up to `wait` for the end of the pipe, then returns what has been read.
    fn take(&self, wait: Duration) -> Vec<u8> {
        let until = Instant::now() + wait;
        while !self.done.load(Ordering::SeqCst) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        self.kept.lock().map(|b| b.clone()).unwrap_or_default()
    }
}

fn run(program: &Program, args: &[&str], limit: Duration) -> Result<Finished, RunError> {
    let mut child = Command::new(&program.command)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => RunError::NotFound,
            _ => RunError::Spawn(e),
        })?;
    let out = child.stdout.take().map(Reader::start);
    let err = child.stderr.take().map(Reader::start);
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= limit => {
                // The readers are left to end by themselves: a process the program started may hold the pipes open for a while.
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunError::TimedOut);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(RunError::Spawn(e)),
        }
    };
    let grace = Duration::from_secs(1);
    Ok(Finished {
        status,
        stdout: out.map(|r| r.take(grace)).unwrap_or_default(),
        stderr: err.map(|r| r.take(grace)).unwrap_or_default(),
    })
}

// ---- the words ----------------------------------------------------------------------------------------------------------------------------

fn limit_text(limit: Duration) -> String {
    if limit.subsec_millis() == 0 {
        format!("{} seconds", limit.as_secs())
    } else {
        format!("{} ms", limit.as_millis())
    }
}

fn not_found(program: &Program) -> String {
    match program.origin {
        Origin::Path => format!(
            "the AWS CLI was not found. A profile that signs in through IAM Identity Center (SSO) or assumes a role needs the AWS CLI, \
             version 2.9.0 or later, on your PATH (or its path in {ENV_VAR}). Install or upgrade it: {INSTALL_URL}"
        ),
        Origin::Variable => format!(
            "{ENV_VAR} names {}, which was not found. Set it to the path of the AWS CLI, version 2.9.0 or later, or unset it. \
             Install or upgrade the AWS CLI: {INSTALL_URL}",
            program.shown()
        ),
    }
}

/// What the AWS CLI said, on one line, cut short, without any line that names a credential field.
fn said(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let words: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| {
            let lower = l.to_ascii_lowercase();
            !["accesskeyid", "secretaccesskey", "sessiontoken"]
                .iter()
                .any(|f| lower.contains(f))
        })
        .collect();
    let line = words.join(" ");
    let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > SAID_LIMIT {
        let cut: String = line.chars().take(SAID_LIMIT).collect();
        format!("{cut}...")
    } else {
        line
    }
}

/// True when the AWS CLI's words say that the IAM Identity Center sign-in is gone or was never made.
fn sign_in_ended(said: &str) -> bool {
    let lower = said.to_ascii_lowercase();
    [
        "sso login",
        "sso session",
        "sso token",
        "sso access token",
        "token has expired",
    ]
    .iter()
    .any(|w| lower.contains(w))
}

fn lacks_the_command(said: &str) -> bool {
    said.to_ascii_lowercase().contains("invalid choice")
}

/// Why the command failed when the AWS CLI does not have `configure export-credentials`: the version it reports, or a plain statement.
fn too_old(program: &Program) -> String {
    let version = run(program, &["--version"], VERSION_LIMIT)
        .ok()
        .and_then(|f| {
            let both = format!(
                "{}\n{}",
                String::from_utf8_lossy(&f.stdout),
                String::from_utf8_lossy(&f.stderr)
            );
            aws_cli_version(&both)
        });
    match version {
        Some((a, b, c)) if (a, b, c) < MIN_VERSION => format!(
            "the AWS CLI on this machine (aws-cli/{a}.{b}.{c}) is older than 2.9.0 and cannot export credentials. Upgrade it: {INSTALL_URL}"
        ),
        _ => format!(
            "this AWS CLI does not have `aws configure export-credentials`, which came in version 2.9.0. Install or upgrade the AWS CLI: {INSTALL_URL}"
        ),
    }
}

/// The credentials of the profile `name` of kind `kind`, from `program configure export-credentials --profile <name> --format process`.
/// The error is a sentence for the person: it never holds what the program printed on standard output.
pub fn export_credentials(
    program: &Program,
    name: &str,
    kind: ProfileKind,
    limit: Duration,
) -> Result<Credentials, String> {
    let args = [
        "configure",
        "export-credentials",
        "--profile",
        name,
        "--format",
        "process",
    ];
    let check = format!("aws sts get-caller-identity --profile {name}");
    let finished = match run(program, &args, limit) {
        Ok(f) => f,
        Err(RunError::NotFound) => return Err(not_found(program)),
        Err(RunError::Spawn(e)) => {
            return Err(format!(
                "could not start the AWS CLI ({}): {e}",
                program.shown()
            ));
        }
        Err(RunError::TimedOut) => {
            return Err(format!(
                "the AWS CLI did not answer within {}, so ham stopped it. Check the profile in a terminal with: {check}",
                limit_text(limit)
            ));
        }
    };
    if finished.status.success() {
        let text = String::from_utf8_lossy(&finished.stdout);
        return parse_credentials(&text)
            .map_err(|e| format!("{e}. Check the profile in a terminal with: {check}"));
    }
    // The two questions below are asked of everything the AWS CLI said: the AWS CLI 1.x puts a banner of about 450 characters before the words
    // that tell the command is missing, and only the first [`SAID_LIMIT`] characters are quoted in an error.
    let all = String::from_utf8_lossy(&finished.stderr);
    let words = said(&finished.stderr);
    if lacks_the_command(&all) {
        return Err(too_old(program));
    }
    if kind == ProfileKind::Sso && sign_in_ended(&all) {
        return Err(format!(
            "your IAM Identity Center sign-in has ended. Run: aws sso login --profile {name}"
        ));
    }
    let status = match finished.status.code() {
        Some(code) => format!("it exited with status {code}"),
        None => "it was stopped by a signal".to_string(),
    };
    Err(if words.is_empty() {
        format!(
            "the AWS CLI could not export credentials for this profile ({status} and said nothing)"
        )
    } else {
        format!("the AWS CLI could not export credentials for this profile ({status}): {words}")
    })
}
