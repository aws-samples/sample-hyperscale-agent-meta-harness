//! On the command line: the compiled `ham` against loopback stand-ins for stages. A config with several backends lists the runs of all of
//! them, names the backend of each, says in one line which backend did not answer, aims a command with `--backend`, refuses a start that names no
//! backend or marks customer content on a backend not declared safe, and refuses two backends with one address. A config with one backend prints
//! the lines it always printed. The AWS identity is a test-only access key in the process environment; nothing leaves 127.0.0.1.
#![allow(clippy::unwrap_used)]

use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

type Log = Arc<Mutex<Vec<String>>>;

struct Stage {
    endpoint: String,
    log: Log,
}

fn row(id: &str, at: &str) -> serde_json::Value {
    json!({"runId": id, "status": "active", "phase": "green", "target": {"mode": "s3-only"},
        "createdAt": at, "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"})
}

async fn serve(
    rows: Vec<serde_json::Value>,
    version: &'static str,
    started: &'static str,
) -> Stage {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let log: Log = Arc::default();
    let seen = Arc::clone(&log);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let (rows, seen) = (rows.clone(), Arc::clone(&seen));
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break at + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                let want: usize = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse().unwrap_or(0))
                    })
                    .unwrap_or(0);
                while buf.len() < head_end + want {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let mut words = head.split_whitespace();
                let method = words.next().unwrap_or_default().to_string();
                let route = words
                    .next()
                    .unwrap_or_default()
                    .split('?')
                    .next()
                    .unwrap_or_default()
                    .to_string();
                let key = head.lines().find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("idempotency-key:")
                        .map(|v| v.trim().to_string())
                });
                seen.lock().unwrap().push(match key {
                    Some(k) => format!("{method} {route} key={k}"),
                    None => format!("{method} {route}"),
                });
                let (status, reply) = match (method.as_str(), route.as_str()) {
                    ("GET", "/direct/v1/runs") => ("200 OK", json!({"runs": rows})),
                    ("GET", "/direct/v1/whoami") => (
                        "200 OK",
                        json!({"principalArn": "arn:aws:sts::1:assumed-role/A/b", "account": "111111111111"}),
                    ),
                    ("POST", "/direct/v1/direct/runs") => {
                        ("200 OK", json!({"runId": started, "status": "active"}))
                    }
                    _ => (
                        "404 Not Found",
                        json!({"code": "not-found", "message": "no such route"}),
                    ),
                };
                let text = reply.to_string();
                let out = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\nx-hamh-backend-version: {version}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                    text.len()
                );
                let _ = sock.write_all(out.as_bytes()).await;
            });
        }
    });
    Stage {
        endpoint: format!("http://127.0.0.1:{port}/direct"),
        log,
    }
}

async fn closed_port() -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    format!("http://127.0.0.1:{port}/direct")
}

fn home(label: &str, config: &str) -> PathBuf {
    let base = std::env::var_os("TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from);
    let dir = base.join(format!("ham-b584-cli-{label}-{}", std::process::id()));
    let cfg = dir.join(".config/ham");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("config.toml"), config).unwrap();
    std::fs::write(dir.join("aws-config"), "").unwrap();
    std::fs::write(dir.join("aws-credentials"), "").unwrap();
    dir
}

struct Ran {
    code: i32,
    out: String,
    err: String,
}

async fn ham(home: &Path, args: &[&str]) -> Ran {
    let home = home.to_path_buf();
    let args: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ham"));
        cmd.args(&args).env_clear();
        for (k, v) in std::env::vars() {
            if k.starts_with("LLVM_") || k.starts_with("CARGO_LLVM_COV") {
                cmd.env(k, v);
            }
        }
        cmd.env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("AWS_ACCESS_KEY_ID", "ASIA-test-cli")
            .env("AWS_SECRET_ACCESS_KEY", "test-secret-not-a-key")
            .env("AWS_SESSION_TOKEN", "test-session-token")
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .env("AWS_CONFIG_FILE", home.join("aws-config"))
            .env("AWS_SHARED_CREDENTIALS_FILE", home.join("aws-credentials"))
            .env("HAMH_NO_DEPRECATION_NOTICES", "1");
        let o = cmd.output().unwrap();
        Ran {
            code: o.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&o.stdout).into_owned(),
            err: String::from_utf8_lossy(&o.stderr).into_owned(),
        }
    })
    .await
    .unwrap()
}

fn writes(log: &Log) -> Vec<String> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|l| l.starts_with("POST"))
        .cloned()
        .collect()
}

async fn world() -> (Stage, Stage, String, PathBuf) {
    let default = serve(
        vec![
            row("01K6A0000000000000000000D9", "2026-11-04T10:00:09Z"),
            row("01K6A0000000000000000000D1", "2026-11-04T10:00:01Z"),
        ],
        "5.05",
        "01K6A0000000000000000000N1",
    )
    .await;
    let spare = serve(
        vec![row("01K6A0000000000000000000S5", "2026-11-04T10:00:05Z")],
        "5.05",
        "01K6A0000000000000000000N2",
    )
    .await;
    let down = closed_port().await;
    let config = format!(
        "apiEndpoint = \"{}\"\nregion = \"us-east-1\"\nconfigVersion = 1\n\n[[backend]]\nalias = \"spare\"\napiEndpoint = \"{}\"\nregion = \"us-east-1\"\ncustomerContent = true\n\n[[backend]]\nalias = \"lab\"\napiEndpoint = \"{}\"\nregion = \"us-east-1\"\n",
        default.endpoint, spare.endpoint, down
    );
    let h = home("many", &config);
    (default, spare, down, h)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b584_runs_lists_every_backend_newest_first_names_the_backend_and_says_which_did_not_answer()
 {
    let (_default, _spare, _down, h) = world().await;
    let r = ham(&h, &["runs"]).await;
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    let lines: Vec<&str> = r.out.lines().collect();
    assert_eq!(lines.len(), 3, "{}", r.out);
    let fields: Vec<Vec<&str>> = lines.iter().map(|l| l.split("  ").collect()).collect();
    assert_eq!(
        fields[0][0], "01K6A0000000000000000000D9",
        "the run id is the first field: {lines:?}"
    );
    assert_eq!(fields[0][1], "default");
    assert_eq!(
        (fields[1][0], fields[1][1]),
        ("01K6A0000000000000000000S5", "spare")
    );
    assert_eq!(
        (fields[2][0], fields[2][1]),
        ("01K6A0000000000000000000D1", "default")
    );
    assert!(r.err.contains("lab: not reachable"), "{}", r.err);

    let j = ham(&h, &["runs", "--json"]).await;
    let v: serde_json::Value = serde_json::from_str(j.out.trim()).unwrap();
    let runs = v["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[1]["backend"], "spare");
    assert_eq!(runs[0]["runId"], "01K6A0000000000000000000D9");

    // `--backend` aims the read at one backend.
    let one = ham(&h, &["runs", "--backend", "spare"]).await;
    assert_eq!(one.code, 0, "{}{}", one.out, one.err);
    assert_eq!(one.out.lines().count(), 1, "{}", one.out);
    assert!(
        one.out.contains("01K6A0000000000000000000S5"),
        "{}",
        one.out
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b584_a_command_names_its_backend_or_is_refused_and_a_start_goes_to_one_backend_only() {
    let (default, spare, _down, h) = world().await;
    let r = ham(&h, &["--backend", "nope", "runs"]).await;
    assert_eq!(r.code, 1, "{}{}", r.out, r.err);
    assert!(
        r.err.contains("no backend is named") && r.err.contains("default, spare, lab"),
        "{}",
        r.err
    );

    let r = ham(&h, &["watch"]).await;
    assert_eq!(r.code, 1);
    assert!(r.err.contains("name the run to watch"), "{}", r.err);

    let r = ham(&h, &["--backend"]).await;
    assert_eq!(r.code, 2, "{}{}", r.out, r.err);
    assert!(r.err.contains("--backend takes the alias"), "{}", r.err);

    // A start with no backend named and no default_backend is refused before any call.
    let r = ham(&h, &["add a test", "--s3-only", "--direct", "--no-rules"]).await;
    assert_eq!(r.code, 1, "{}{}", r.out, r.err);
    assert!(
        r.err.contains("none is chosen") && r.err.contains("--backend"),
        "{}",
        r.err
    );
    assert!(writes(&default.log).is_empty() && writes(&spare.log).is_empty());

    // Marked as customer content on the default backend, which is not declared safe: refused, nothing sent.
    let r = ham(
        &h,
        &[
            "add a test",
            "--s3-only",
            "--direct",
            "--no-rules",
            "--backend",
            "default",
            "--customer-content",
        ],
    )
    .await;
    assert_eq!(r.code, 1, "{}{}", r.out, r.err);
    assert!(
        r.err.contains("not declared safe for customer content") && r.err.contains("default"),
        "{}",
        r.err
    );
    assert!(writes(&default.log).is_empty());

    // On `spare`, which is declared safe, the start goes to `spare` with a key, and to no other backend.
    let r = ham(
        &h,
        &[
            "add a test",
            "--s3-only",
            "--direct",
            "--no-rules",
            "--backend=spare",
            "--customer-content",
        ],
    )
    .await;
    // The start is followed by a watch, which the stand-in cannot serve: the 404 that ends it is not the start's.
    assert!(r.err.contains("HTTP 404"), "{}{}", r.out, r.err);
    assert!(
        r.out.contains("started run 01K6A0000000000000000000N2"),
        "{}",
        r.out
    );
    let sent = writes(&spare.log);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].contains("key="), "{sent:?}");
    assert!(
        writes(&default.log).is_empty(),
        "{:?}",
        default.log.lock().unwrap()
    );

    // A switch that only means something for a start is refused for any other command.
    let r = ham(&h, &["runs", "--customer-content"]).await;
    assert_eq!(r.code, 1);
    assert!(
        r.err
            .contains("--customer-content marks a run that you start"),
        "{}",
        r.err
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b584_default_backend_takes_a_start_that_names_none_and_doctor_checks_every_backend() {
    let default = serve(
        vec![row("01K6A0000000000000000000D9", "2026-11-04T10:00:09Z")],
        "5.05",
        "01K6A0000000000000000000N1",
    )
    .await;
    let spare = serve(
        vec![row("01K6A0000000000000000000S5", "2026-11-04T10:00:05Z")],
        "5.04",
        "01K6A0000000000000000000N2",
    )
    .await;
    let config = format!(
        "apiEndpoint = \"{}\"\nregion = \"us-east-1\"\nconfigVersion = 1\ndefaultBackend = \"spare\"\n\n[[backend]]\nalias = \"spare\"\napiEndpoint = \"{}\"\nregion = \"us-east-1\"\n",
        default.endpoint, spare.endpoint
    );
    let h = home("default", &config);
    let r = ham(&h, &["add a test", "--s3-only", "--direct", "--no-rules"]).await;
    assert!(r.err.contains("HTTP 404"), "{}{}", r.out, r.err);
    assert!(
        r.out.contains("started run 01K6A0000000000000000000N2"),
        "{}",
        r.out
    );
    assert_eq!(writes(&spare.log).len(), 1);
    assert!(writes(&default.log).is_empty());

    let d = ham(&h, &["doctor"]).await;
    let text = format!("{}{}", d.out, d.err);
    assert!(
        text.contains("default: ") && text.contains("spare: "),
        "{text}"
    );
    assert!(
        text.contains("identity") && text.contains("111111111111"),
        "{text}"
    );
    assert!(text.contains("versions"), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b584_two_backends_with_one_address_are_refused_and_a_lone_backend_prints_what_it_always_did()
 {
    let only = serve(
        vec![row("01K6A0000000000000000000D9", "2026-11-04T10:00:09Z")],
        "5.05",
        "01K6A0000000000000000000N1",
    )
    .await;
    let twice = format!(
        "apiEndpoint = \"{0}\"\nregion = \"us-east-1\"\nconfigVersion = 1\n\n[[backend]]\nalias = \"again\"\napiEndpoint = \"{0}/\"\n",
        only.endpoint
    );
    let r = ham(&home("twice", &twice), &["runs"]).await;
    assert_eq!(r.code, 1, "{}{}", r.out, r.err);
    assert!(r.err.contains("same apiEndpoint"), "{}", r.err);

    let h = home(
        "one",
        &format!(
            "apiEndpoint = \"{}\"\nregion = \"us-east-1\"\n",
            only.endpoint
        ),
    );
    let r = ham(&h, &["runs"]).await;
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    assert_eq!(
        r.out, "01K6A0000000000000000000D9  active (green)  s3-only  2026-11-04T10:00:09Z\n",
        "{}",
        r.out
    );
    assert_eq!(r.err, "");
    let j = ham(&h, &["runs", "--json"]).await;
    assert!(!j.out.contains("\"backend\""), "{}", j.out);
    // Naming the only backend changes nothing.
    let named = ham(&h, &["runs", "--backend", "default"]).await;
    assert_eq!(named.out, r.out);
    let r = ham(&h, &["--backend", "work", "runs"]).await;
    assert_eq!(r.code, 1);
    assert!(r.err.contains("no backend is named"), "{}", r.err);
}
