//! The piece of ham that runs `aws configure export-credentials --profile <name> --format process` and reads what it prints
//! (`ham::awscli`), tested on its own: the time limit and the kill, a flood of output, the credentials JSON and its RFC 3339 expiry, and the
//! AWS CLI version line. The whole path through the `ham` binary is in `aws_cli_profiles.rs`.
#![allow(clippy::unwrap_used)]

use ham::awscli::{
    INSTALL_URL, Program, TIME_LIMIT, aws_cli_version, export_credentials, parse_credentials,
    parse_rfc3339,
};
use ham::awsprofile::ProfileKind;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const AKID: &str = "AKIDTESTONLY";
const SECRET: &str = "test-secret-not-a-key";
const TOKEN: &str = "test-session-token-value";

fn scratch(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "ham-awscli-runner-{label}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A program that is a shell script with this body.
fn script(label: &str, body: &str) -> (Program, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch(label);
    let path = dir.join("aws");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (Program::named(path), dir)
}

fn secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap(),
        Err(e) => -i64::try_from(e.duration().as_secs()).unwrap(),
    }
}

fn json(extra: &str) -> String {
    format!(
        "{{\"Version\": 1, \"AccessKeyId\": \"{AKID}\", \"SecretAccessKey\": \"{SECRET}\"{extra}}}"
    )
}

#[test]
fn the_time_limit_is_30_seconds() {
    assert_eq!(TIME_LIMIT, Duration::from_secs(30));
}

#[test]
fn rfc_3339_times_read_with_fractions_and_offsets() {
    for (text, want) in [
        ("1970-01-01T00:00:00Z", 0),
        ("2099-01-01T00:00:00+00:00", 4_070_908_800),
        ("2099-01-01T00:00:00Z", 4_070_908_800),
        ("2020-02-29T13:45:59.5+00:00", 1_582_983_959),
        ("2020-02-29T13:45:59.123456+00:00", 1_582_983_959),
        ("2020-03-01T01:30:00+02:00", 1_583_019_000),
        ("1999-12-31T23:59:59-05:30", 946_704_599),
        ("2000-02-29T00:00:01+00:00", 951_782_401),
    ] {
        assert_eq!(parse_rfc3339(text).map(secs), Some(want), "{text}");
    }
}

#[test]
fn text_that_is_not_an_rfc_3339_time_is_refused() {
    for text in [
        "",
        "tomorrow",
        "2020-13-01T00:00:00Z",
        "2020-02-30T00:00:00Z",
        "2019-02-29T00:00:00Z",
        "2020-01-01 00:00:00Z",
        "2020-01-01T00:00:00",
        "2020-01-01T24:00:00Z",
        "2020-01-01T00:60:00Z",
        "2020-01-01T00:00:00+24:00",
        "2020-01-01T00:00:00+0000",
        "2020-1-1T00:00:00Z",
        "2020-01-01T00:00:00.Z",
        "x2020-01-01T00:00:00Z",
    ] {
        assert_eq!(parse_rfc3339(text), None, "{text:?}");
    }
}

#[test]
fn the_version_line_of_either_generation_of_the_aws_cli_reads() {
    for (line, want) in [
        (
            "aws-cli/2.37.2 Python/3.13.9 Linux/6.1.0 exe/x86_64.amzn.2023",
            Some((2, 37, 2)),
        ),
        (
            "aws-cli/1.18.147 Python/3.6.9 Linux/5.4.0 botocore/1.17.70",
            Some((1, 18, 147)),
        ),
        ("aws-cli/2.9.0", Some((2, 9, 0))),
        ("aws-cli/2.9 Python/3.11", Some((2, 9, 0))),
        ("  aws-cli/3.0.0 Python/3.14\n", Some((3, 0, 0))),
        (
            "WARNING: noise\naws-cli/2.8.12 Python/3.11.6",
            Some((2, 8, 12)),
        ),
        ("aws-cli/", None),
        ("aws-cli/x.y.z", None),
        ("something else entirely", None),
        ("", None),
    ] {
        assert_eq!(aws_cli_version(line), want, "{line:?}");
    }
}

#[test]
fn credentials_json_is_read_as_the_aws_cli_prints_it() {
    let full = json(&format!(
        ", \"SessionToken\": \"{TOKEN}\", \"Expiration\": \"2099-01-01T00:00:00+00:00\""
    ));
    let c = parse_credentials(&full).unwrap();
    assert_eq!(c.access_key_id(), AKID);
    assert_eq!(c.secret_access_key(), SECRET);
    assert_eq!(c.session_token(), Some(TOKEN));
    assert_eq!(c.expiry().map(secs), Some(4_070_908_800));
    // Keys that never expire: no token, no expiry. Key names are not case-sensitive, as in aws-config.
    let bare = parse_credentials(&json("").replace("AccessKeyId", "accesskeyid")).unwrap();
    assert_eq!(bare.session_token(), None);
    assert_eq!(bare.expiry(), None);
    // An AccountId or another field the AWS CLI may add is ignored.
    assert!(parse_credentials(&json(", \"AccountId\": \"123456789012\"")).is_ok());
}

#[test]
fn credentials_json_that_is_not_credentials_json_is_refused_without_echoing_it() {
    for text in [
        "".to_string(),
        "not json".to_string(),
        "[]".to_string(),
        json("").replace("\"Version\": 1", "\"Version\": 2"),
        json("").replace("\"Version\": 1, ", ""),
        json("").replace(&format!(", \"SecretAccessKey\": \"{SECRET}\""), ""),
        json("").replace(AKID, ""),
        json(", \"Expiration\": \"tomorrow\""),
        json(", \"SessionToken\": 5"),
        format!("export AWS_ACCESS_KEY_ID={AKID}\nexport AWS_SECRET_ACCESS_KEY={SECRET}\n"),
    ] {
        let e = parse_credentials(&text).unwrap_err();
        assert!(e.contains("not credentials JSON"), "{e}");
        for secret in [AKID, SECRET, TOKEN] {
            assert!(!e.contains(secret), "{e}");
        }
    }
}

#[test]
fn a_program_that_prints_credentials_gives_credentials() {
    let (program, _dir) = script(
        "ok",
        &format!(
            "if [ \"$*\" != 'configure export-credentials --profile my-profile --format process' ]; then echo \"unexpected: $*\" >&2; exit 252; fi\n\
             printf '%s' '{}'",
            json(&format!(
                ", \"SessionToken\": \"{TOKEN}\", \"Expiration\": \"2099-01-01T00:00:00+00:00\""
            ))
        ),
    );
    let c = export_credentials(
        &program,
        "my-profile",
        ProfileKind::Sso,
        Duration::from_secs(20),
    )
    .unwrap();
    assert_eq!(c.access_key_id(), AKID);
    assert_eq!(c.session_token(), Some(TOKEN));
}

#[test]
fn a_program_that_does_not_answer_is_killed_at_the_limit() {
    let dir_probe = scratch("hang-pid");
    let pid_file = dir_probe.join("pid");
    let (program, _dir) = script(
        "hang",
        &format!("echo $$ > '{}'\nexec sleep 120", pid_file.display()),
    );
    let started = Instant::now();
    let e = export_credentials(
        &program,
        "my-profile",
        ProfileKind::Sso,
        Duration::from_secs(2),
    )
    .unwrap_err();
    let took = started.elapsed();
    assert!(e.contains("did not answer within 2 seconds"), "{e}");
    assert!(
        e.contains("aws sts get-caller-identity --profile my-profile"),
        "{e}"
    );
    assert!(took >= Duration::from_secs(2), "{took:?}");
    assert!(took < Duration::from_secs(10), "{took:?}");
    // The process is gone, not left running.
    let pid = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .to_string();
    let mut alive = true;
    for _ in 0..40 {
        alive = std::process::Command::new("kill")
            .args(["-0", &pid])
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !alive {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!alive, "process {pid} still runs after the limit");
}

#[test]
fn a_flood_of_output_neither_blocks_the_program_nor_fills_the_error() {
    // 600 KB on each stream: the pipes would fill and block a program whose output nobody reads.
    let (program, _dir) = script(
        "flood",
        "i=0\nwhile [ $i -lt 600 ]; do\n  head -c 1000 /dev/zero | tr '\\0' 'x'\n  head -c 1000 /dev/zero | tr '\\0' 'y' >&2\n  i=$((i+1))\ndone\nexit 3",
    );
    let started = Instant::now();
    let e = export_credentials(
        &program,
        "p",
        ProfileKind::AssumeRole,
        Duration::from_secs(20),
    )
    .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(15));
    assert!(e.contains("status 3"), "{e}");
    assert!(e.len() < 2_000, "the error holds {} bytes", e.len());
    assert!(!e.contains("xxxx"), "standard output is never echoed");
}

#[test]
fn a_program_that_is_not_there_says_so_with_the_install_page() {
    let e = export_credentials(
        &Program::named("/no/such/dir/aws"),
        "p",
        ProfileKind::Sso,
        Duration::from_secs(5),
    )
    .unwrap_err();
    assert!(e.contains("HAMH_AWS_CLI names /no/such/dir/aws"), "{e}");
    assert!(e.contains("not found"), "{e}");
    assert!(e.contains(INSTALL_URL), "{e}");
}

// COMPAT-CAH: the earlier name of the variable, `CAH_AWS_CLI`, answers while `HAMH_AWS_CLI` is unset or empty.
fn vars<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<std::ffi::OsString> + 'a {
    move |k| {
        pairs
            .iter()
            .find(|(name, _)| *name == k)
            .map(|(_, v)| std::ffi::OsString::from(v))
    }
}

#[test]
fn the_aws_cli_variable_has_the_earlier_name_as_a_fallback() {
    let command = |pairs: &[(&str, &str)]| {
        Program::from_vars(&vars(pairs))
            .command()
            .to_string_lossy()
            .into_owned()
    };
    assert_eq!(command(&[]), "aws");
    assert_eq!(command(&[("HAMH_AWS_CLI", "/new/aws")]), "/new/aws");
    assert_eq!(command(&[("CAH_AWS_CLI", "/old/aws")]), "/old/aws"); // COMPAT-CAH
    assert_eq!(
        command(&[("HAMH_AWS_CLI", ""), ("CAH_AWS_CLI", "/old/aws")]), // COMPAT-CAH
        "/old/aws"
    );
    assert_eq!(
        command(&[("HAMH_AWS_CLI", "/new/aws"), ("CAH_AWS_CLI", "/old/aws")]), // COMPAT-CAH
        "/new/aws",
        "the new name wins"
    );
}
