//! How long a new run asks to wait for the user's answer, through the real binary against the local
//! `/v1` API: `--input-wait`, `HAMH_INPUT_WAIT`, `input_wait_minutes` in `config.toml`, nothing at all for a script, and an error that
//! names the layer when a value is wrong. A run started without a wait sends the request it sent before the feature.
#![allow(clippy::unwrap_used)]

mod home;
mod support;

use home::{Home, ham_env, home, text};
use serde_json::Value;
use std::sync::Arc;
use support::Fake;

async fn fake() -> Arc<Fake> {
    Fake::start_with(true).await
}

fn start<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    let mut a = vec!["build it", "--s3-only", "--direct", "--no-rules", "--json"];
    a.extend(extra);
    a
}

fn body(fake: &Fake, n: usize) -> Value {
    serde_json::from_slice(&fake.mem.posts("/v1/direct/runs")[n].body).unwrap()
}

fn config_line(h: &Home, line: &str) {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(h.config.join("ham/config.toml"))
        .unwrap();
    writeln!(f, "{line}").unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_flag_sets_the_wait_in_seconds() {
    let f = fake().await;
    let h = home(&f, "wait-flag");
    let o = ham_env(&h, &start(&["--input-wait", "5"]), &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(body(&f, 0)["inputWaitS"], 300);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_script_without_a_wait_sends_no_member_at_all() {
    let f = fake().await;
    let h = home(&f, "wait-none");
    // No terminal on stdin or stdout: not an interactive client, so the default is no wait.
    let o = ham_env(&h, &start(&[]), &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let b = body(&f, 0);
    assert!(b.get("inputWaitS").is_none(), "{b}");
    // A wait of zero says the same thing.
    let o = ham_env(
        &h,
        &start(&["--input-wait", "0"]),
        &[("HAMH_IDEMPOTENCY_NONCE", "n2")],
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert!(body(&f, 1).get("inputWaitS").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_layers_are_flag_then_env_then_config() {
    let f = fake().await;
    let h = home(&f, "wait-layers");
    config_line(&h, "inputWaitMinutes = 9");
    let n = |i: &'static str| [("HAMH_IDEMPOTENCY_NONCE", i)];
    let o = ham_env(&h, &start(&[]), &n("a")).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(body(&f, 0)["inputWaitS"], 540, "config");
    let o = ham_env(
        &h,
        &start(&[]),
        &[("HAMH_IDEMPOTENCY_NONCE", "b"), ("HAMH_INPUT_WAIT", "7")],
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(body(&f, 1)["inputWaitS"], 420, "env beats config");
    let o = ham_env(
        &h,
        &start(&["--input-wait", "2"]),
        &[("HAMH_IDEMPOTENCY_NONCE", "c"), ("HAMH_INPUT_WAIT", "7")],
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(body(&f, 2)["inputWaitS"], 120, "flag beats env");
    let o = ham_env(&h, &start(&["--input-wait", "0"]), &n("d")).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert!(
        body(&f, 3).get("inputWaitS").is_none(),
        "a zero flag beats a config wait"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_value_names_its_layer_and_nothing_is_sent() {
    let f = fake().await;
    let h = home(&f, "wait-bad");
    let o = ham_env(&h, &start(&[]), &[("HAMH_INPUT_WAIT", "ten")]).await;
    assert_eq!(o.status.code(), Some(1));
    let e = text(&o.stderr);
    assert!(
        e.contains("HAMH_INPUT_WAIT") && e.contains("ten") && e.contains("0 to 43200"),
        "{e}"
    );
    let o = ham_env(&h, &start(&["--input-wait", "43201"]), &[]).await;
    assert_eq!(o.status.code(), Some(2), "{}", text(&o.stderr));
    assert!(text(&o.stderr).contains("--input-wait"));
    assert!(
        f.mem.posts("/v1/direct/runs").is_empty(),
        "no run was started"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_config_file_with_a_wait_over_the_longest_run_stops_the_command_and_names_the_key() {
    let f = fake().await;
    let h = home(&f, "wait-config-bad");
    config_line(&h, "inputWaitMinutes = 43201");
    let o = ham_env(&h, &start(&[]), &[]).await;
    assert_eq!(o.status.code(), Some(1));
    let e = text(&o.stderr);
    assert!(
        e.contains("input_wait_minutes") && e.contains("43200"),
        "{e}"
    );
    assert!(f.mem.posts("/v1/direct/runs").is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ham_config_sets_lists_and_unsets_the_key() {
    let f = fake().await;
    let h = home(&f, "wait-config-cmd");
    let o = ham_env(&h, &["config", "set", "input_wait_minutes", "15"], &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(text(&o.stdout).trim(), "input_wait_minutes = 15");
    let o = ham_env(&h, &["config"], &[]).await;
    assert!(
        text(&o.stdout).contains("input_wait_minutes = 15"),
        "{}",
        text(&o.stdout)
    );
    let o = ham_env(&h, &["config", "set", "input_wait_minutes", "43201"], &[]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(
        text(&o.stderr).contains("0 (do not wait) to 43200"),
        "{}",
        text(&o.stderr)
    );
    assert!(
        std::fs::read_to_string(h.config.join("ham/config.toml"))
            .unwrap()
            .contains("inputWaitMinutes = 15")
    );
    let o = ham_env(&h, &["config", "unset", "input_wait_minutes"], &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert!(
        !std::fs::read_to_string(h.config.join("ham/config.toml"))
            .unwrap()
            .contains("inputWait")
    );
}
