//! Ordinary AWS CLI profiles, end to end through the `ham` binary: an IAM Identity Center (SSO) profile and an assume-role profile
//! (`role_arn` with `source_profile`, or with `credential_source`) sign requests with no hand-made helper profile. ham runs
//! `aws configure export-credentials --profile <name> --format process` itself (AWS CLI 2.9.0 or later), reads the credentials JSON the
//! command prints, applies the same expiry check as for any other profile and never prints a credential. Static-key profiles and
//! `credential_process` profiles sign as before, with no AWS CLI involved.
//!
//! The AWS CLI is replaced by a shell script on `PATH` (or named by `HAMH_AWS_CLI`) that checks its own arguments and prints what the real
//! command prints; `bridge_profile.rs` pins that output byte for byte to what aws-cli/2.37.2 printed. The local `/v1` API checks each
//! signature against the keys the script printed, so a request that reaches it signed proves the keys, the session token and the scope came
//! through. No real AWS call and no real sign-in is made.
#![allow(clippy::unwrap_used)]

mod support;

use std::path::PathBuf;
use std::process::Output;
use std::time::{Duration, Instant};
use support::{AKID, Fake, REGION, SECRET, TOKEN};

/// The account of the identity the local `/v1` API reports for the stand-in keys (the fifth field of its ARN).
fn account() -> &'static str {
    support::ME.split(':').nth(4).unwrap()
}

const SSO: &str = "my-sso";
const JUMP: &str = "jump";
const ON_HOST: &str = "on-host";
const KEYS: &str = "keys";
const VIA_PROC: &str = "via-proc";
const LATER: &str = "2099-01-01T00:00:00+00:00";
/// Where a person who has no AWS CLI, or an old one, is sent.
const INSTALL_URL: &str =
    "https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html";

struct Home {
    root: PathBuf,
    config: PathBuf,
    state: PathBuf,
    cache: PathBuf,
    downloads: PathBuf,
    bin: PathBuf,
}

/// The arguments ham passes to the AWS CLI for `profile`.
fn args_for(profile: &str) -> String {
    format!("configure export-credentials --profile {profile} --format process")
}

/// Fresh XDG directories, a ham config naming the fake (and `aws_profile` when given), an AWS config with an SSO profile, two assume-role
/// profiles (one with `source_profile`, one with `credential_source`) and a `credential_process` profile, an AWS credentials file with
/// static keys, and a `bin` directory for the stand-in `aws`. `extra` is appended to the AWS config.
fn home(fake: &Fake, aws_profile: Option<&str>, extra: &str) -> Home {
    let root = support::scratch("awscli");
    let h = Home {
        config: root.join("config"),
        state: root.join("state"),
        cache: root.join("cache"),
        downloads: root.join("downloads"),
        bin: root.join("bin"),
        root,
    };
    for d in [&h.config, &h.state, &h.cache, &h.downloads, &h.bin] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::create_dir_all(h.config.join("ham")).unwrap();
    let profile_line = aws_profile
        .map(|p| format!("awsProfile = \"{p}\"\n"))
        .unwrap_or_default();
    std::fs::write(
        h.config.join("ham/config.toml"),
        format!(
            "apiEndpoint = \"{}\"\nregion = \"{REGION}\"\n{profile_line}",
            fake.endpoint
        ),
    )
    .unwrap();
    // What the credential_process profile prints.
    std::fs::write(
        h.root.join("proc-out.json"),
        export_output(Some(TOKEN), Some(LATER)),
    )
    .unwrap();
    std::fs::write(
        h.root.join("aws-config"),
        format!(
            "[profile {SSO}]\nsso_session = stand-in\nsso_account_id = 123456789012\nsso_role_name = Developer\nregion = {REGION}\n\n\
             [sso-session stand-in]\nsso_start_url = https://stand-in.invalid/start\nsso_region = {REGION}\n\n\
             [profile {JUMP}]\nrole_arn = arn:aws:iam::123456789012:role/stand-in\nsource_profile = {KEYS}\nregion = {REGION}\n\n\
             [profile {ON_HOST}]\nrole_arn = arn:aws:iam::123456789012:role/stand-in\ncredential_source = Ec2InstanceMetadata\nregion = {REGION}\n\n\
             [profile {VIA_PROC}]\ncredential_process = cat '{}/proc-out.json'\nregion = {REGION}\n\n{extra}",
            h.root.display()
        ),
    )
    .unwrap();
    std::fs::write(
        h.root.join("aws-credentials"),
        format!("[{KEYS}]\naws_access_key_id = {AKID}\naws_secret_access_key = {SECRET}\n"),
    )
    .unwrap();
    h
}

fn make_executable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Installs a stand-in `aws` at `dir/name` with this script body; every call appends its arguments to `<root>/<log>`.
fn script_at(h: &Home, dir: &std::path::Path, name: &str, log: &str, body: &str) {
    let root = h.root.display();
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{root}/{log}'\n{body}\n"),
    )
    .unwrap();
    make_executable(&path);
}

/// The standard stand-in `aws` on `PATH`: it refuses any arguments but those for `profile` (exit 252), then prints `stdout` and `stderr`
/// and exits with `code`. `aws --version` prints `version` (to stderr when `version_on_stderr`, as the AWS CLI 1.x does).
fn stand_in(
    h: &Home,
    profile: &str,
    stdout: &str,
    stderr: &str,
    code: i32,
    version: &str,
    version_on_stderr: bool,
) {
    std::fs::write(h.root.join("aws-stdout.txt"), stdout).unwrap();
    std::fs::write(h.root.join("aws-stderr.txt"), stderr).unwrap();
    let root = h.root.display();
    let args = args_for(profile);
    let to = if version_on_stderr { " >&2" } else { "" };
    script_at(
        h,
        &h.bin,
        "aws",
        "aws-calls.log",
        &format!(
            "if [ \"$1\" = '--version' ]; then echo '{version}'{to}; exit 0; fi\n\
             if [ \"$*\" != '{args}' ]; then echo \"stand-in aws: unexpected arguments: $*\" >&2; exit 252; fi\n\
             cat '{root}/aws-stdout.txt'\ncat '{root}/aws-stderr.txt' >&2\nexit {code}"
        ),
    );
}

/// The usual case: AWS CLI 2.37.2 prints `stdout` and exits 0.
fn works(h: &Home, profile: &str, stdout: &str) {
    stand_in(
        h,
        profile,
        stdout,
        "",
        0,
        "aws-cli/2.37.2 Python/3.13.9 Linux/6.1.0 exe/x86_64.amzn.2023",
        false,
    );
}

/// The argument lines the stand-in `aws` was called with.
fn aws_calls(h: &Home) -> Vec<String> {
    std::fs::read_to_string(h.root.join("aws-calls.log"))
        .map(|t| t.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// What `aws configure export-credentials --format process` prints for these credentials.
fn export_output(session_token: Option<&str>, expiration: Option<&str>) -> String {
    let mut s = format!(
        "{{\n  \"Version\": 1,\n  \"AccessKeyId\": \"{AKID}\",\n  \"SecretAccessKey\": \"{SECRET}\""
    );
    if let Some(t) = session_token {
        s.push_str(&format!(",\n  \"SessionToken\": \"{t}\""));
    }
    if let Some(e) = expiration {
        s.push_str(&format!(",\n  \"Expiration\": \"{e}\""));
    }
    s.push_str("\n}\n");
    s
}

/// `ham <args>` in the home `h`: `PATH` is `h.bin` then the system path unless `path` replaces it; `env` adds variables.
async fn ham(h: &Home, args: &[&str], env: &[(&str, &str)], path: Option<&str>) -> Output {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_ham"));
    cmd.env_clear();
    for (k, v) in std::env::vars() {
        if k.starts_with("LLVM_") || k.starts_with("CARGO_LLVM_COV") {
            cmd.env(k, v);
        }
    }
    let default_path = format!(
        "{}:{}",
        h.bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    cmd.args(args)
        .env("PATH", path.unwrap_or(&default_path))
        .env("TMPDIR", std::env::temp_dir())
        .env("HOME", &h.root)
        .env("XDG_CONFIG_HOME", &h.config)
        .env("XDG_STATE_HOME", &h.state)
        .env("XDG_CACHE_HOME", &h.cache)
        .env("HAMH_DOWNLOAD_DIR", &h.downloads)
        .env("AWS_CONFIG_FILE", h.root.join("aws-config"))
        .env(
            "AWS_SHARED_CREDENTIALS_FILE",
            h.root.join("aws-credentials"),
        )
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .stdin(std::process::Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().await.unwrap()
}

async fn doctor(h: &Home) -> Output {
    ham(h, &["doctor"], &[], None).await
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Neither stream holds a key, a secret, a session token or a signature.
fn assert_no_secrets(o: &Output) {
    for stream in [text(&o.stdout), text(&o.stderr)] {
        for secret in [AKID, SECRET, TOKEN, "AWS4-HMAC-SHA256", "Signature="] {
            assert!(!stream.contains(secret), "{secret:?} leaked:\n{stream}");
        }
    }
}

/// The `FAIL identity` line of `ham doctor`.
fn identity_failure(o: &Output) -> String {
    let out = text(&o.stdout);
    out.lines()
        .find(|l| l.starts_with("FAIL identity "))
        .unwrap_or_else(|| panic!("no FAIL identity line:\n{out}\n{}", text(&o.stderr)))
        .to_string()
}

/// The calls the local API saw, objects left out.
fn api_calls(fake: &Fake) -> Vec<support::Seen> {
    fake.mem
        .seen()
        .into_iter()
        .filter(|s| !s.path.starts_with("/objects/"))
        .collect()
}

/// `ham doctor` passed, and every request reached the local API signed with the stand-in's keys (and its session token when `token`).
fn assert_signed(fake: &Fake, o: &Output, token: bool) {
    let out = text(&o.stdout);
    assert_eq!(o.status.code(), Some(0), "{out}{}", text(&o.stderr));
    assert_eq!(
        out.lines().next().unwrap(),
        format!("PASS identity {} account {}", support::ME, account())
    );
    assert!(!out.contains("FAIL"), "{out}");
    let seen = api_calls(fake);
    assert!(seen.iter().any(|s| s.path == "/v1/whoami"), "{seen:?}");
    for s in &seen {
        assert!(s.signature.is_ok(), "{}: {:?}", s.path, s.signature);
        assert_eq!(s.caller.as_deref(), Some(support::ME), "{}", s.path);
        let auth = &s.headers["authorization"];
        assert!(
            auth.starts_with(&format!("AWS4-HMAC-SHA256 Credential={AKID}/")),
            "{auth}"
        );
        assert!(
            auth.contains(&format!(
                "/{REGION}/execute-api/aws4_request, SignedHeaders="
            )),
            "{auth}"
        );
        assert_eq!(
            s.headers.get("x-amz-security-token").map(String::as_str),
            token.then_some(TOKEN),
            "{}",
            s.path
        );
    }
    assert_no_secrets(o);
}

/// `ham doctor` failed at the identity check and sent nothing to the API.
fn assert_refused_before_any_request(fake: &Fake, o: &Output) -> String {
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stdout));
    assert!(
        api_calls(fake).is_empty(),
        "no request leaves without credentials"
    );
    assert_no_secrets(o);
    identity_failure(o)
}

// ---- the profile kinds that sign through the AWS CLI ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_sso_profile_signs_with_what_the_aws_cli_exports() {
    let fake = Fake::start().await;
    let h = home(&fake, Some(SSO), "");
    works(&h, SSO, &export_output(Some(TOKEN), Some(LATER)));
    let o = doctor(&h).await;
    assert_signed(&fake, &o, true);
    // The credentials are cached until five minutes before they expire: one AWS CLI process for the whole run.
    assert_eq!(aws_calls(&h), vec![args_for(SSO)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_assume_role_profile_with_a_source_profile_signs_the_same_way() {
    let fake = Fake::start().await;
    let h = home(&fake, Some(JUMP), "");
    works(&h, JUMP, &export_output(Some(TOKEN), Some(LATER)));
    let o = doctor(&h).await;
    assert_signed(&fake, &o, true);
    assert_eq!(aws_calls(&h), vec![args_for(JUMP)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_assume_role_profile_with_a_credential_source_signs_the_same_way() {
    let fake = Fake::start().await;
    let h = home(&fake, Some(ON_HOST), "");
    works(&h, ON_HOST, &export_output(Some(TOKEN), Some(LATER)));
    let o = doctor(&h).await;
    assert_signed(&fake, &o, true);
    assert_eq!(aws_calls(&h), vec![args_for(ON_HOST)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn credentials_without_a_session_token_or_an_expiry_sign_without_the_token() {
    // A role chain can end in keys that never expire: no SessionToken, no Expiration.
    let fake = Fake::start().await;
    let h = home(&fake, Some(JUMP), "");
    works(&h, JUMP, &export_output(None, None));
    let o = doctor(&h).await;
    assert_signed(&fake, &o, false);
    assert_eq!(aws_calls(&h), vec![args_for(JUMP)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aws_profile_names_the_sso_profile_when_the_ham_config_names_none() {
    let fake = Fake::start().await;
    let h = home(&fake, None, "");
    works(&h, SSO, &export_output(Some(TOKEN), Some(LATER)));
    let o = ham(&h, &["doctor"], &[("AWS_PROFILE", SSO)], None).await;
    assert_signed(&fake, &o, true);
    assert_eq!(aws_calls(&h), vec![args_for(SSO)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_default_profile_that_is_an_sso_profile_works_with_no_other_setting() {
    let fake = Fake::start().await;
    let extra = format!(
        "[default]\nsso_session = stand-in\nsso_account_id = 123456789012\nsso_role_name = Developer\nregion = {REGION}\n"
    );
    let h = home(&fake, None, &extra);
    works(&h, "default", &export_output(Some(TOKEN), Some(LATER)));
    let o = doctor(&h).await;
    assert_signed(&fake, &o, true);
    assert_eq!(aws_calls(&h), vec![args_for("default")]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aws_keys_in_the_environment_come_before_an_sso_profile_and_start_no_aws_cli() {
    let fake = Fake::start().await;
    let extra = format!("[default]\nsso_session = stand-in\nregion = {REGION}\n");
    let h = home(&fake, None, &extra);
    works(
        &h,
        "default",
        &export_output(Some("some-other-token"), Some(LATER)),
    );
    let o = ham(
        &h,
        &["doctor"],
        &[
            ("AWS_ACCESS_KEY_ID", AKID),
            ("AWS_SECRET_ACCESS_KEY", SECRET),
            ("AWS_SESSION_TOKEN", TOKEN),
        ],
        None,
    )
    .await;
    assert_signed(&fake, &o, true);
    assert!(aws_calls(&h).is_empty(), "{:?}", aws_calls(&h));
}

// ---- the profile kinds that sign as before ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_static_key_profile_signs_as_before_and_starts_no_aws_cli() {
    let fake = Fake::start().await;
    let h = home(&fake, Some(KEYS), "");
    works(&h, KEYS, "this must never be asked for");
    let o = doctor(&h).await;
    assert_signed(&fake, &o, false);
    assert!(aws_calls(&h).is_empty(), "{:?}", aws_calls(&h));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_credential_process_profile_signs_as_before_and_starts_no_aws_cli() {
    let fake = Fake::start().await;
    let h = home(&fake, Some(VIA_PROC), "");
    works(&h, VIA_PROC, "this must never be asked for");
    let o = doctor(&h).await;
    assert_signed(&fake, &o, true);
    assert!(aws_calls(&h).is_empty(), "{:?}", aws_calls(&h));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_credential_process_profile_works_on_a_machine_with_no_aws_cli() {
    let fake = Fake::start().await;
    let h = home(&fake, Some(VIA_PROC), "");
    // `cat` is all the profile needs; PATH holds the system directories and no `aws`.
    let o = ham(&h, &["doctor"], &[], Some("/usr/bin:/bin")).await;
    let aws_on_path = ["/usr/bin/aws", "/bin/aws"]
        .iter()
        .any(|p| std::path::Path::new(p).exists());
    if !aws_on_path {
        assert_signed(&fake, &o, true);
    } else {
        // This machine has an AWS CLI in a system directory; the profile still never starts it.
        assert_signed(&fake, &o, true);
        assert!(aws_calls(&h).is_empty());
    }
}

// ---- sign-in ended, and the other ways the AWS CLI fails ---------------------------------------------------------------------------

/// The reason `aws configure export-credentials` writes once the SSO sign-in has ended (`ConfigurationError`, exit 253; the sentence is
/// `UnauthorizedSSOTokenError` of botocore).
const SSO_ENDED: &str = "\nUnable to retrieve credentials: The SSO session associated with this profile has expired or is otherwise invalid. To refresh this SSO session run aws sso login with the corresponding profile.\n\n";
/// What it writes when the person never signed in on this machine.
const SSO_NEVER: &str = "\nUnable to retrieve credentials: Error loading SSO Token: Token for https://stand-in.invalid/start does not exist\n\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ended_sso_sign_in_says_the_one_command_that_fixes_it() {
    for (what, stderr) in [("expired", SSO_ENDED), ("never signed in", SSO_NEVER)] {
        let fake = Fake::start().await;
        let h = home(&fake, Some(SSO), "");
        stand_in(&h, SSO, "", stderr, 253, "aws-cli/2.37.2", false);
        let o = doctor(&h).await;
        let line = assert_refused_before_any_request(&fake, &o);
        assert!(line.contains(&format!("profile {SSO}")), "{what}: {line}");
        assert!(
            line.ends_with("Run: aws sso login --profile my-sso"),
            "{what}: {line}"
        );
        // `ham doctor` signs four requests; a failure is remembered for a few seconds, so one AWS CLI run answers them all.
        assert_eq!(aws_calls(&h), vec![args_for(SSO)], "{what}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_assume_role_profile_shows_the_aws_clis_own_words() {
    // The role's source profile may be an SSO profile whose sign-in has ended: the profile `jump` is no SSO profile, so ham does not
    // tell the person to log in to it. The AWS CLI says what is wrong.
    let fake = Fake::start().await;
    let h = home(&fake, Some(JUMP), "");
    stand_in(&h, JUMP, "", SSO_ENDED, 253, "aws-cli/2.37.2", false);
    let o = doctor(&h).await;
    let line = assert_refused_before_any_request(&fake, &o);
    assert!(line.contains(&format!("profile {JUMP}")), "{line}");
    assert!(
        line.contains("The SSO session associated with this profile has expired"),
        "{line}"
    );
    assert!(
        !line.contains("Run: aws sso login --profile jump"),
        "{line}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_sso_profile_with_another_failure_shows_the_aws_clis_own_words() {
    let fake = Fake::start().await;
    let h = home(&fake, Some(SSO), "");
    let said = "\nUnable to retrieve credentials: An error occurred (AccessDeniedException) when calling the GetRoleCredentials operation: No access\n\n";
    stand_in(&h, SSO, "", said, 253, "aws-cli/2.37.2", false);
    let o = doctor(&h).await;
    let line = assert_refused_before_any_request(&fake, &o);
    assert!(line.contains("AccessDeniedException"), "{line}");
    assert!(line.contains("No access"), "{line}");
    assert!(!line.contains("aws sso login"), "{line}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failure_with_no_words_still_names_the_exit_status() {
    let fake = Fake::start().await;
    let h = home(&fake, Some(JUMP), "");
    stand_in(&h, JUMP, "", "", 255, "aws-cli/2.37.2", false);
    let o = doctor(&h).await;
    let line = assert_refused_before_any_request(&fake, &o);
    assert!(line.contains("255"), "{line}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn credentials_that_have_already_expired_are_refused_before_any_request() {
    // A stale export: exit 0, a past `Expiration`. The client does not send what the service will refuse, and names the time in UTC.
    for (profile, then) in [
        (SSO, "Run: aws sso login --profile my-sso"),
        (JUMP, "Sign in again"),
    ] {
        let fake = Fake::start().await;
        let h = home(&fake, Some(profile), "");
        works(
            &h,
            profile,
            &export_output(Some(TOKEN), Some("2020-02-29T13:45:59.5+00:00")),
        );
        let o = doctor(&h).await;
        let line = assert_refused_before_any_request(&fake, &o);
        assert!(line.contains(&format!("profile {profile}")), "{line}");
        assert!(
            line.contains("the credentials expired at 2020-02-29T13:45:59Z."),
            "{line}"
        );
        assert!(line.contains(then), "{profile}: {line}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_that_is_not_the_credentials_json_fails_and_is_never_printed() {
    let env_format = format!(
        "export AWS_ACCESS_KEY_ID={AKID}\nexport AWS_SECRET_ACCESS_KEY={SECRET}\nexport AWS_SESSION_TOKEN={TOKEN}\n"
    );
    let no_secret = format!("{{\n  \"Version\": 1,\n  \"AccessKeyId\": \"{AKID}\"\n}}\n");
    let version_2 =
        export_output(Some(TOKEN), Some(LATER)).replace("\"Version\": 1", "\"Version\": 2");
    let bad_expiry = export_output(Some(TOKEN), Some("tomorrow"));
    let cases: Vec<(&str, String)> = vec![
        ("the env format of --format env", env_format),
        ("no SecretAccessKey", no_secret),
        ("Version 2", version_2),
        ("an Expiration that is not RFC 3339", bad_expiry),
        ("no output at all", String::new()),
    ];
    for (what, stdout) in cases {
        let fake = Fake::start().await;
        let h = home(&fake, Some(SSO), "");
        works(&h, SSO, &stdout);
        let o = doctor(&h).await;
        let line = assert_refused_before_any_request(&fake, &o);
        assert!(line.contains(&format!("profile {SSO}")), "{what}: {line}");
        assert!(line.contains("not credentials JSON"), "{what}: {line}");
        // What to do: ask the AWS CLI who the profile is, which prints no credential.
        assert!(
            line.contains("aws sts get-caller-identity --profile my-sso"),
            "{what}: {line}"
        );
    }
}

// ---- no AWS CLI, an old one, one that hangs, and HAMH_AWS_CLI --------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_aws_cli_says_how_to_install_it() {
    let fake = Fake::start().await;
    let h = home(&fake, Some(SSO), "");
    // `bin` holds no `aws`, and it is the only directory on PATH.
    let only_bin = h.bin.display().to_string();
    let o = ham(&h, &["doctor"], &[], Some(&only_bin)).await;
    let line = assert_refused_before_any_request(&fake, &o);
    assert!(line.contains(&format!("profile {SSO}")), "{line}");
    assert!(line.contains("AWS CLI was not found"), "{line}");
    assert!(line.contains("2.9.0"), "{line}");
    assert!(line.contains("HAMH_AWS_CLI"), "{line}");
    assert!(line.contains(INSTALL_URL), "{line}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ham_aws_cli_that_does_not_exist_is_named() {
    let fake = Fake::start().await;
    let h = home(&fake, Some(SSO), "");
    works(&h, SSO, &export_output(Some(TOKEN), Some(LATER)));
    let missing = h.root.join("no-such-dir/aws");
    let o = ham(
        &h,
        &["doctor"],
        &[("HAMH_AWS_CLI", &missing.display().to_string())],
        None,
    )
    .await;
    let line = assert_refused_before_any_request(&fake, &o);
    assert!(
        line.contains(&format!("HAMH_AWS_CLI names {}", missing.display())),
        "{line}"
    );
    assert!(line.contains("not found"), "{line}");
    assert!(line.contains(INSTALL_URL), "{line}");
    assert!(
        aws_calls(&h).is_empty(),
        "the AWS CLI on PATH is not used when HAMH_AWS_CLI names another"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ham_aws_cli_names_the_program_to_run_instead_of_the_one_on_path() {
    let fake = Fake::start().await;
    let h = home(&fake, Some(SSO), "");
    // The `aws` on PATH is broken; the one HAMH_AWS_CLI names (not on PATH, another file name) is the stand-in that works.
    script_at(
        &h,
        &h.bin,
        "aws",
        "wrong-calls.log",
        "echo 'the wrong aws was started' >&2; exit 1",
    );
    let other = h.root.join("elsewhere");
    let stdout = export_output(Some(TOKEN), Some(LATER));
    std::fs::write(h.root.join("aws-stdout.txt"), stdout).unwrap();
    script_at(
        &h,
        &other,
        "my-aws",
        "aws-calls.log",
        &format!(
            "if [ \"$*\" != '{}' ]; then echo \"stand-in aws: unexpected arguments: $*\" >&2; exit 252; fi\ncat '{}/aws-stdout.txt'",
            args_for(SSO),
            h.root.display()
        ),
    );
    let program = other.join("my-aws").display().to_string();
    let o = ham(&h, &["doctor"], &[("HAMH_AWS_CLI", &program)], None).await;
    assert_signed(&fake, &o, true);
    assert_eq!(aws_calls(&h), vec![args_for(SSO)]);
    assert!(
        !h.root.join("wrong-calls.log").exists(),
        "the aws on PATH must not run"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_aws_cli_older_than_2_9_0_says_so_and_how_to_upgrade() {
    // `configure export-credentials` first appeared in AWS CLI 2.9.0 (its changelog). aws-cli/1.x prints its version on stderr and exits 2;
    // 2.x before 2.9.0 prints the version on stdout and exits 252. V1_STDERR is what aws-cli/1.18.147 printed for this command, byte for
    // byte: the words that tell the command is missing ("Invalid choice") come after a banner, 475 characters in, which is further than
    // the 400 characters of the AWS CLI's words that ham quotes in an error.
    const V1_STDERR: &str = "usage: \rNote: AWS CLI version 2, the latest major version of the AWS CLI, is now stable and recommended for general use. For more information, see the AWS CLI version 2 installation instructions at: https://docs.aws.amazon.com/cli/latest/userguide/install-cliv2.html\n\nusage: aws [options] <command> <subcommand> [<subcommand> ...] [parameters]\nTo see help text, you can run:\n\n  aws help\n  aws <command> help\n  aws <command> <subcommand> help\naws: error: argument subcommand: Invalid choice, valid choices are:\n\nlist                                     | get                                     \nset                                      | add-model                               \n";
    const V2_8_STDERR: &str = "usage: aws [options] <command> <subcommand> [<subcommand> ...] [parameters]\nTo see help text, you can run:\n\n  aws help\n  aws <command> help\n  aws <command> <subcommand> help\n\naws: error: argument subcommand: Invalid choice, valid choices are:\n\nlist | get\nset | add-model\n";
    for (version, on_stderr, code, shown, stderr) in [
        (
            "aws-cli/1.18.147 Python/2.7.18 Linux/5.10.270 botocore/1.18.6",
            true,
            2,
            "aws-cli/1.18.147",
            V1_STDERR,
        ),
        (
            "aws-cli/2.8.12 Python/3.11.6 Darwin/23.1.0 exe/x86_64 prompt/off",
            false,
            252,
            "aws-cli/2.8.12",
            V2_8_STDERR,
        ),
    ] {
        let fake = Fake::start().await;
        let h = home(&fake, Some(JUMP), "");
        stand_in(&h, JUMP, "", stderr, code, version, on_stderr);
        let o = doctor(&h).await;
        let line = assert_refused_before_any_request(&fake, &o);
        assert!(line.contains(&format!("profile {JUMP}")), "{line}");
        assert!(line.contains(shown), "{line}");
        assert!(line.contains("older than 2.9.0"), "{line}");
        assert!(line.contains(INSTALL_URL), "{line}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_aws_cli_whose_version_cannot_be_read_still_gets_the_upgrade_advice() {
    const INVALID: &str = "aws: error: argument subcommand: Invalid choice, valid choices are:\n\nlist | get | set | add-model\n";
    let fake = Fake::start().await;
    let h = home(&fake, Some(SSO), "");
    stand_in(&h, SSO, "", INVALID, 2, "not a version line", false);
    let o = doctor(&h).await;
    let line = assert_refused_before_any_request(&fake, &o);
    assert!(line.contains("export-credentials"), "{line}");
    assert!(line.contains("2.9.0"), "{line}");
    assert!(line.contains(INSTALL_URL), "{line}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_aws_cli_that_does_not_answer_is_stopped_after_30_seconds() {
    let fake = Fake::start().await;
    let h = home(&fake, Some(SSO), "");
    script_at(&h, &h.bin, "aws", "aws-calls.log", "exec sleep 120");
    let started = Instant::now();
    let o = doctor(&h).await;
    let took = started.elapsed();
    let line = assert_refused_before_any_request(&fake, &o);
    assert!(line.contains(&format!("profile {SSO}")), "{line}");
    assert!(line.contains("did not answer within 30 seconds"), "{line}");
    assert!(
        line.contains("aws sts get-caller-identity --profile my-sso"),
        "{line}"
    );
    assert!(
        took >= Duration::from_secs(29),
        "stopped too early: {took:?}"
    );
    assert!(took < Duration::from_secs(60), "stopped too late: {took:?}");
    assert_eq!(aws_calls(&h), vec![args_for(SSO)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_profile_that_is_neither_sso_nor_assume_role_keeps_its_old_error() {
    // A profile with only a Region has no credentials: the AWS CLI is not the answer, and the message names the ways that work.
    let fake = Fake::start().await;
    let extra = format!("[profile bare]\nregion = {REGION}\n");
    let h = home(&fake, Some("bare"), &extra);
    works(&h, "bare", "this must never be asked for");
    let o = doctor(&h).await;
    let line = assert_refused_before_any_request(&fake, &o);
    assert!(line.contains("profile bare"), "{line}");
    assert!(aws_calls(&h).is_empty());
}
