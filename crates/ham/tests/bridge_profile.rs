//! A `credential_process` profile whose command is the AWS CLI, end to end through the `ham` binary: a profile whose
//! `credential_process` is `aws configure export-credentials --profile <sso profile> --format process`. People who already made such a
//! profile keep a working setup: aws-config runs the command, and ham applies the expiry check to what it prints. An SSO or assume-role
//! profile needs no such profile any more: ham asks the AWS CLI itself (`aws_cli_profiles.rs`).
//!
//! The AWS CLI is replaced by a shell script on `PATH` that checks its own arguments and prints what the
//! real command prints. The shape comes from the AWS CLI source (`awscli/customizations/configure/exportcreds.py`,
//! `CredentialProcessFormatter`): `json.dumps(output, indent=2, separators=(',', ': '))` and a newline, with
//! `Version` 1, `AccessKeyId`, `SecretAccessKey`, `SessionToken` only when the credentials have one, and
//! `Expiration` only when they expire, as Python's `datetime.isoformat()` of a UTC time (`+00:00`, never `Z`).
//! A failed command exits with a non-zero code and writes the reason to stderr (`ConfigurationError`, code 253).
//! The local `/v1` API checks each signature against the keys the script printed, so a request that reaches it
//! signed proves the keys, the session token and the scope came through. The script's output is pinned byte for byte to
//! what aws-cli/2.37.2 printed for the same keys (`the_stand_in_prints_byte_for_byte_what_the_real_aws_cli_prints`).
//!
//! No real AWS call and no real SSO sign-in is made: one run against a real SSO session by a person who has one
//! is still open.
#![allow(clippy::unwrap_used)]

mod support;

use std::path::PathBuf;
use std::process::Output;
use support::{AKID, Fake, REGION, SECRET, TOKEN};

/// The profile `ham` is told to use, and the SSO profile it wraps.
const BRIDGE: &str = "ham-bridge";
const BASE: &str = "my-sso";
/// The arguments the wrapper profile passes to `aws` (the README's line).
const ARGS: &str = "configure export-credentials --profile my-sso --format process";
/// An expiry far in the future, as Python prints a UTC time without microseconds.
const LATER: &str = "2099-01-01T00:00:00+00:00";

struct Home {
    root: PathBuf,
    config: PathBuf,
    state: PathBuf,
    cache: PathBuf,
    downloads: PathBuf,
    bin: PathBuf,
}

/// Fresh XDG directories, a config naming the fake and the wrapper profile, an AWS config with the wrapper and the
/// SSO profile it wraps, and a `bin` directory for the stand-in `aws`.
fn home(fake: &Fake) -> Home {
    let root = support::scratch("bridge");
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
    std::fs::write(
        h.config.join("ham/config.toml"),
        format!(
            "apiEndpoint = \"{}\"\nregion = \"{REGION}\"\nawsProfile = \"{BRIDGE}\"\n",
            fake.endpoint
        ),
    )
    .unwrap();
    std::fs::write(
        h.root.join("aws-config"),
        format!(
            "[profile {BRIDGE}]\ncredential_process = aws {ARGS}\nregion = {REGION}\n\n\
             [profile {BASE}]\nsso_session = stand-in\nsso_account_id = 000000000000\nsso_role_name = Developer\nregion = {REGION}\n\n\
             [sso-session stand-in]\nsso_start_url = https://stand-in.invalid/start\nsso_region = {REGION}\n"
        ),
    )
    .unwrap();
    std::fs::write(h.root.join("aws-credentials"), "").unwrap();
    h
}

/// Installs the stand-in `aws`: it logs its arguments, refuses any other than [`ARGS`] (exit 252), then prints `stdout`
/// and `stderr` and exits with `code`.
fn stand_in(h: &Home, stdout: &str, stderr: &str, code: i32) {
    std::fs::write(h.root.join("aws-stdout.txt"), stdout).unwrap();
    std::fs::write(h.root.join("aws-stderr.txt"), stderr).unwrap();
    let root = h.root.display();
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{root}/aws-calls.log'\n\
         if [ \"$*\" != '{ARGS}' ]; then echo \"stand-in aws: unexpected arguments: $*\" >&2; exit 252; fi\n\
         cat '{root}/aws-stdout.txt'\ncat '{root}/aws-stderr.txt' >&2\nexit {code}\n"
    );
    let path = h.bin.join("aws");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
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

#[test]
fn the_stand_in_prints_byte_for_byte_what_the_real_aws_cli_prints() {
    // Captured on a Linux machine from aws-cli/2.37.2: `aws configure export-credentials --profile <p> --format process` for a profile
    // whose credential_process printed these keys, this session token and the expiry 2099-01-01T00:00:00Z (the CLI prints it again as
    // `+00:00`), and for a profile with static keys (no SessionToken, no Expiration). Changing the test keys of tests/support means
    // capturing the real output again.
    assert_eq!(
        (AKID, SECRET, TOKEN),
        (
            "AKIDTESTONLY",
            "test-secret-not-a-key",
            "test-session-token-value"
        )
    );
    assert_eq!(
        export_output(Some(TOKEN), Some(LATER)),
        "{\n  \"Version\": 1,\n  \"AccessKeyId\": \"AKIDTESTONLY\",\n  \"SecretAccessKey\": \"test-secret-not-a-key\",\n  \"SessionToken\": \"test-session-token-value\",\n  \"Expiration\": \"2099-01-01T00:00:00+00:00\"\n}\n"
    );
    assert_eq!(
        export_output(None, None),
        "{\n  \"Version\": 1,\n  \"AccessKeyId\": \"AKIDTESTONLY\",\n  \"SecretAccessKey\": \"test-secret-not-a-key\"\n}\n"
    );
}

async fn ham(h: &Home, args: &[&str]) -> Output {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_ham"));
    cmd.env_clear();
    for (k, v) in std::env::vars() {
        if k.starts_with("LLVM_") || k.starts_with("CARGO_LLVM_COV") {
            cmd.env(k, v);
        }
    }
    cmd.args(args)
        .env(
            "PATH",
            format!(
                "{}:{}",
                h.bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
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
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .unwrap()
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_export_credentials_output_signs_every_request_with_its_keys_and_session_token() {
    // Python prints no fraction when the microseconds are zero and six digits otherwise; both end in `+00:00`.
    for expiration in [LATER, "2099-01-01T00:00:00.123456+00:00"] {
        let fake = Fake::start().await;
        let h = home(&fake);
        stand_in(&h, &export_output(Some(TOKEN), Some(expiration)), "", 0);
        let o = ham(&h, &["doctor"]).await;
        let out = text(&o.stdout);
        assert_eq!(
            o.status.code(),
            Some(0),
            "{expiration}:\n{out}{}",
            text(&o.stderr)
        );
        assert_eq!(
            out.lines().next().unwrap(),
            format!("PASS identity {} account 800000000022", support::ME)
        );
        assert!(!out.contains("FAIL"), "{out}");
        let seen = api_calls(&fake);
        assert!(seen.iter().any(|s| s.path == "/v1/whoami"), "{seen:?}");
        for s in &seen {
            // The local API recomputes the signature from the keys the script printed and the headers it received.
            assert!(s.signature.is_ok(), "{}: {:?}", s.path, s.signature);
            assert_eq!(s.caller.as_deref(), Some(support::ME), "{}", s.path);
            assert_eq!(
                s.headers.get("x-amz-security-token").map(String::as_str),
                Some(TOKEN),
                "{}",
                s.path
            );
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
            let signed = auth
                .split("SignedHeaders=")
                .nth(1)
                .and_then(|r| r.split(',').next())
                .unwrap();
            assert!(
                signed.split(';').any(|n| n == "x-amz-security-token"),
                "{auth}"
            );
        }
        // The credentials are cached until five minutes before they expire: one process for the whole run.
        assert_eq!(aws_calls(&h), vec![ARGS.to_string()], "{expiration}");
        assert_no_secrets(&o);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_export_of_static_keys_has_no_session_token_and_still_signs() {
    // A base profile with static keys: the command prints no `SessionToken` and no `Expiration`.
    let fake = Fake::start().await;
    let h = home(&fake);
    stand_in(&h, &export_output(None, None), "", 0);
    let o = ham(&h, &["doctor"]).await;
    let out = text(&o.stdout);
    assert_eq!(o.status.code(), Some(0), "{out}{}", text(&o.stderr));
    assert!(out.starts_with("PASS identity "), "{out}");
    let seen = api_calls(&fake);
    assert!(!seen.is_empty());
    for s in &seen {
        assert!(s.signature.is_ok(), "{}: {:?}", s.path, s.signature);
        assert!(!s.headers.contains_key("x-amz-security-token"), "{s:?}");
    }
    assert_eq!(aws_calls(&h), vec![ARGS.to_string()]);
    assert_no_secrets(&o);
}

/// The reason `aws configure export-credentials` writes once the SSO sign-in has ended (`ConfigurationError`, exit 253; the
/// sentence is `UnauthorizedSSOTokenError` of botocore).
const SSO_ENDED: &str = "\nUnable to retrieve credentials: The SSO session associated with this profile has expired or is otherwise invalid. To refresh this SSO session run aws sso login with the corresponding profile.\n\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ended_sso_sign_in_fails_the_identity_check_with_the_profile_and_the_sign_in_command() {
    let fake = Fake::start().await;
    let h = home(&fake);
    stand_in(&h, "", SSO_ENDED, 253);
    let o = ham(&h, &["doctor"]).await;
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stdout));
    let line = identity_failure(&o);
    assert!(line.contains(&format!("profile {BRIDGE}")), "{line}");
    assert!(line.contains("aws sso login"), "{line}");
    assert!(
        api_calls(&fake).is_empty(),
        "no request leaves without credentials"
    );
    assert_no_secrets(&o);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_aws_cli_without_the_command_fails_with_its_own_words() {
    // `configure export-credentials` first appeared in AWS CLI 2.9.0 (its changelog). aws-cli/1.18.147 (the system `aws` of the build
    // machines) exits 2 with empty stdout and this stderr (the first line of its banner is left out).
    const V1_STDERR: &str = "usage: aws [options] <command> <subcommand> [<subcommand> ...] [parameters]\nTo see help text, you can run:\n\n  aws help\n  aws <command> help\n  aws <command> <subcommand> help\naws: error: argument subcommand: Invalid choice, valid choices are:\n\nlist                                     | get                                     \nset                                      | add-model                               \n";
    let fake = Fake::start().await;
    let h = home(&fake);
    stand_in(&h, "", V1_STDERR, 2);
    let o = ham(&h, &["doctor"]).await;
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stdout));
    let line = identity_failure(&o);
    assert!(line.contains(&format!("profile {BRIDGE}")), "{line}");
    assert!(line.contains("Invalid choice"), "{line}");
    assert!(api_calls(&fake).is_empty());
    assert_no_secrets(&o);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn credentials_that_have_already_expired_are_refused_before_any_request() {
    // A stale export: exit 0, a past `Expiration`. The client does not send what the service will refuse, and names the time in UTC
    // (a century year, a leap day, a fraction of a second and a non-UTC offset exercise the date arithmetic).
    for (given, shown) in [
        ("2020-01-01T00:00:00+00:00", "2020-01-01T00:00:00Z"),
        ("1999-12-31T23:59:59+00:00", "1999-12-31T23:59:59Z"),
        ("2000-02-29T00:00:01+00:00", "2000-02-29T00:00:01Z"),
        ("2020-02-29T13:45:59.5+00:00", "2020-02-29T13:45:59Z"),
        ("2020-03-01T01:30:00+02:00", "2020-02-29T23:30:00Z"),
    ] {
        let fake = Fake::start().await;
        let h = home(&fake);
        stand_in(&h, &export_output(Some(TOKEN), Some(given)), "", 0);
        let o = ham(&h, &["doctor"]).await;
        assert_eq!(o.status.code(), Some(1), "{given}: {}", text(&o.stdout));
        let line = identity_failure(&o);
        assert!(
            line.contains(&format!("profile {BRIDGE}")),
            "{given}: {line}"
        );
        assert!(
            line.contains(&format!("the credentials expired at {shown}.")),
            "{given}: {line}"
        );
        assert!(line.contains("Sign in again"), "{given}: {line}");
        assert!(line.contains("aws sso login"), "{given}: {line}");
        assert!(
            api_calls(&fake).is_empty(),
            "{given}: an expired request must not be sent"
        );
        assert_no_secrets(&o);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_that_is_not_the_credentials_json_fails_the_check_and_is_never_printed() {
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
        let h = home(&fake);
        stand_in(&h, &stdout, "", 0);
        let o = ham(&h, &["doctor"]).await;
        assert_eq!(o.status.code(), Some(1), "{what}: {}", text(&o.stdout));
        let line = identity_failure(&o);
        assert!(
            line.contains(&format!("profile {BRIDGE}")),
            "{what}: {line}"
        );
        assert!(line.contains("could not parse response"), "{what}: {line}");
        // What to do: run the profile's command by hand and compare it with the JSON it should print.
        assert!(line.contains("credential_process"), "{what}: {line}");
        assert!(line.contains("in a terminal"), "{what}: {line}");
        assert!(api_calls(&fake).is_empty(), "{what}");
        assert_no_secrets(&o);
    }
}
