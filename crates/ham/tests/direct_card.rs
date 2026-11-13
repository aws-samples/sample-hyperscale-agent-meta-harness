//! The direct run's result card and exit code: `ham watch`, `ham pull` and the library
//! `watch` print the label the harness wrote on `run.completed`, "Cline reported done" or "Cline stopped: <reason>", exactly as
//! delivered; no Evidence block is printed; the words verified, tested, gated and evaluated do
//! not appear; and the exit code follows the stop table (a run Cline stopped halfway ends `complete` and exits 0, a run with
//! nothing to deliver ends `blocked` and exits 12).
#![allow(clippy::unwrap_used)]

mod support;

use ham::api::{Api, Retry};
use ham::https::HttpsPlane;
use ham::pull::card;
use ham::sigv4::Signer;
use ham::watch::{WatchOptions, exit_of, watch};
use harness_protocol::direct::{DirectOutcome, banned_claim_word};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Output;
use std::sync::Arc;
use std::time::Duration;
use support::{AKID, Fake, REGION, SECRET, TOKEN, creds};

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 4,
        base: Duration::from_millis(10),
    })
}

fn opts(dir: Option<PathBuf>, json: bool) -> WatchOptions {
    WatchOptions {
        interval: Duration::from_millis(20),
        json,
        download_dir: dir,
    }
}

async fn new_run(fake: &Fake, prompt: &str) -> String {
    let req = serde_json::from_value::<harness_protocol::api::v1::StartDirectRunRequest>(
        json!({"prompt": prompt, "target": {"mode": "s3-only"}, "model": support::MODEL}),
    )
    .unwrap();
    api(fake)
        .start_direct_run(&req, &format!("k-{}", prompt.replace(' ', "-")))
        .await
        .unwrap()
        .run_id
}

/// Words the card never shows: an evidence block.
fn assert_no_evidence(text: &str) {
    for absent in [
        "Evidence",
        "  gates",
        "  tests",
        "  coverage",
        "  findings",
        "  verdicts",
    ] {
        assert!(
            !text.lines().any(|l| l.starts_with(absent)),
            "no {absent:?} line for a direct run:\n{text}"
        );
    }
}

fn last_status_line(text: &str) -> &str {
    text.lines().rfind(|l| l.starts_with("run ")).unwrap()
}

#[tokio::test]
async fn a_direct_run_that_reported_done_prints_the_label_on_the_card_and_the_last_status_line() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "direct card").await;
    fake.mem
        .complete_direct_run(&run, DirectOutcome::ReportedDone, None);
    let dir = support::scratch("direct-card");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let done = watch(
        &api(&fake),
        &run,
        &opts(Some(dir), false),
        &mut out,
        &mut err,
    )
    .await
    .unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
    assert_eq!(exit_of(&done.view), 0);
    assert!(
        text.lines().any(|l| l == "Outcome   Cline reported done"),
        "{text}"
    );
    assert!(
        last_status_line(&text).ends_with("outcome: Cline reported done"),
        "{text}"
    );
    assert!(text.contains(&format!("Run       {run} complete (delivered)")));
    assert_no_evidence(&text);
    assert_eq!(banned_claim_word(&text), None, "{text}");
    let c = card(done.detail.as_ref(), done.pulled.as_ref());
    assert_eq!(c.usage, None, "no usage was reported");
}

#[tokio::test]
async fn the_json_watch_carries_the_label_and_the_exit_code_and_no_evidence_object() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "direct json").await;
    fake.mem.complete_direct_run(
        &run,
        DirectOutcome::StoppedMaxRequests,
        Some("the engine stopped after 50 requests"),
    );
    let dir = support::scratch("direct-card-json");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let done = watch(
        &api(&fake),
        &run,
        &opts(Some(dir), true),
        &mut out,
        &mut err,
    )
    .await
    .unwrap();
    let last: Value =
        serde_json::from_str(String::from_utf8(out).unwrap().lines().last().unwrap()).unwrap();
    assert_eq!(last["exitCode"], 0);
    assert_eq!(exit_of(&done.view), 0);
    assert_eq!(last["run"]["status"], "complete");
    assert_eq!(last["run"]["outcome"], "stopped_max_requests");
    assert_eq!(
        last["run"]["label"],
        "Cline stopped: it reached its request limit"
    );
    assert_eq!(
        last["run"]["detail"],
        "the engine stopped after 50 requests"
    );
    assert!(
        last.get("evidence").is_none(),
        "a direct run has no evidence object: {last}"
    );
    assert_eq!(banned_claim_word(&last.to_string()), None, "{last}");
}

#[tokio::test]
async fn every_stop_in_the_table_ends_complete_prints_its_own_label_and_exits_0() {
    let fake = Fake::start().await;
    for outcome in DirectOutcome::ALL {
        let run = new_run(&fake, &format!("stop {}", outcome.as_str())).await;
        fake.mem.complete_direct_run(&run, outcome, None);
        let dir = support::scratch("direct-card-stops");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let done = watch(
            &api(&fake),
            &run,
            &opts(Some(dir), false),
            &mut out,
            &mut err,
        )
        .await
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(exit_of(&done.view), 0, "{outcome:?}");
        assert!(
            text.lines()
                .any(|l| l == format!("Outcome   {}", outcome.label())),
            "{outcome:?}:\n{text}"
        );
        assert!(
            last_status_line(&text).ends_with(&format!("outcome: {}", outcome.label())),
            "{outcome:?}:\n{text}"
        );
        assert_no_evidence(&text);
        assert_eq!(banned_claim_word(&text), None, "{outcome:?}:\n{text}");
    }
}

#[tokio::test]
async fn a_direct_run_with_nothing_to_deliver_exits_12_with_its_reason_and_its_label() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "direct blocked").await;
    fake.mem.block_direct_run(
        &run,
        "the session ended before any work was saved",
        Some(DirectOutcome::StoppedSessionLost),
    );
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let done = watch(&api(&fake), &run, &opts(None, false), &mut out, &mut err)
        .await
        .unwrap();
    let text = String::from_utf8(out).unwrap();
    assert_eq!(exit_of(&done.view), 12);
    assert!(
        text.contains("blocked: the session ended before any work was saved"),
        "{text}"
    );
    assert!(
        last_status_line(&text).ends_with(&format!(
            "outcome: {}",
            DirectOutcome::StoppedSessionLost.label()
        )),
        "{text}"
    );
    assert_no_evidence(&text);
    // A blocked run that the harness gave no outcome (for example a run that was never staged) says only why.
    let run = new_run(&fake, "direct blocked bare").await;
    fake.mem.block_direct_run(
        &run,
        "the model us.mistral.large-9 is not in the catalog",
        None,
    );
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let done = watch(&api(&fake), &run, &opts(None, false), &mut out, &mut err)
        .await
        .unwrap();
    let text = String::from_utf8(out).unwrap();
    assert_eq!(exit_of(&done.view), 12);
    assert!(!text.contains("outcome:"), "{text}");
    assert_eq!(banned_claim_word(&text), None, "{text}");
}

/// The card, the status line and the JSON name the model the run used.
#[tokio::test]
async fn the_card_the_last_status_line_and_the_json_name_the_model() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "direct model").await;
    fake.mem.complete_direct_run_on(
        &run,
        ham_core::models::DEFAULT_MODEL_ID,
        DirectOutcome::ReportedDone,
        None,
    );
    let dir = support::scratch("direct-card-model");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let done = watch(
        &api(&fake),
        &run,
        &opts(Some(dir.clone()), false),
        &mut out,
        &mut err,
    )
    .await
    .unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.lines()
            .any(|l| l == "Model     Claude Sonnet 5.5 (global.anthropic.claude-sonnet-5-5)"),
        "{text}"
    );
    assert!(
        last_status_line(&text).contains(" model Claude Sonnet 5.5 "),
        "{text}"
    );
    assert_eq!(banned_claim_word(&text), None, "{text}");
    assert_eq!(
        done.view.result.as_ref().unwrap().label,
        "Cline reported done"
    );
    // --json: the run object carries the name and the id.
    let run2 = new_run(&fake, "direct model json").await;
    fake.mem.complete_direct_run_on(
        &run2,
        ham_core::models::DEFAULT_MODEL_ID,
        DirectOutcome::ReportedDone,
        None,
    );
    let (mut out, mut err) = (Vec::new(), Vec::new());
    watch(
        &api(&fake),
        &run2,
        &opts(Some(dir), true),
        &mut out,
        &mut err,
    )
    .await
    .unwrap();
    let last: Value =
        serde_json::from_str(String::from_utf8(out).unwrap().lines().last().unwrap()).unwrap();
    assert_eq!(last["run"]["model"], "Claude Sonnet 5.5");
    assert_eq!(last["run"]["modelId"], "global.anthropic.claude-sonnet-5-5");
}

/// The model whose price row the assertions below use.
const PRICED_MODEL: &str = "global.anthropic.claude-sonnet-5-5";

const USAGE: harness_protocol::common::Usage = harness_protocol::common::Usage {
    input_tokens: 100_000,
    output_tokens: 100_000,
    cache_read_input_tokens: 1_000_000,
    cache_write_input_tokens: 0,
};

/// Client polish 2: the stage prices the usage the run's turn recorded into `cost` (the price table's row of the default model, global Claude Sonnet 5.5:
/// 100,000 input at 2.00, 100,000 output at 10.00 and 1,000,000 cache reads at 0.20 per million is 1.40 USD). The goal counter is 0 for a direct run, so
/// the card's tokens are the sum of the usage, not that counter.
#[tokio::test]
async fn the_card_the_json_and_pull_show_the_tokens_and_cost_the_stage_reported() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "direct usage").await;
    fake.mem
        .complete_direct_run(&run, DirectOutcome::ReportedDone, None);
    fake.mem.record_direct_usage(&run, PRICED_MODEL, USAGE);
    let dir = support::scratch("direct-card-usage");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    watch(
        &api(&fake),
        &run,
        &opts(Some(dir), false),
        &mut out,
        &mut err,
    )
    .await
    .unwrap();
    let text = String::from_utf8(out).unwrap();
    assert_eq!(
        text.lines().last(),
        Some("Tokens    1200000  USD 1.40"),
        "{text}"
    );

    let (mut out, mut err) = (Vec::new(), Vec::new());
    watch(
        &api(&fake),
        &run,
        &opts(Some(support::scratch("direct-card-usage-json")), true),
        &mut out,
        &mut err,
    )
    .await
    .unwrap();
    let last: Value =
        serde_json::from_str(String::from_utf8(out).unwrap().lines().last().unwrap()).unwrap();
    assert_eq!(last["tokens"], 1_200_000);
    assert_eq!(last["usd"], 1.4);

    // `ham pull` prints the same numbers, plain and as JSON.
    let h = home(&fake, "direct-card-usage-bin");
    let o = ham(&h, &["pull", &run]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text_of(&o.stderr));
    assert_eq!(
        text_of(&o.stdout).lines().last(),
        Some("Tokens    1200000  USD 1.40")
    );
    let o = ham(&h, &["pull", &run, "--json"]).await;
    let v: Value = serde_json::from_str(text_of(&o.stdout).trim()).unwrap();
    assert_eq!((&v["tokens"], &v["usd"]), (&json!(1_200_000), &json!(1.4)));
}

/// A run whose turn recorded no usage: no Tokens line, and no `tokens` or `usd` member in the JSON (never a fake 0).
#[tokio::test]
async fn a_direct_run_with_no_reported_usage_prints_no_tokens_line_and_no_json_members() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "direct no usage").await;
    fake.mem
        .complete_direct_run(&run, DirectOutcome::StoppedSilence, None);
    let dir = support::scratch("direct-card-no-usage");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    watch(
        &api(&fake),
        &run,
        &opts(Some(dir), false),
        &mut out,
        &mut err,
    )
    .await
    .unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(!text.contains("Tokens") && !text.contains("USD"), "{text}");
    assert!(text.contains("Outcome   Cline stopped: no output for the silence limit"));

    let (mut out, mut err) = (Vec::new(), Vec::new());
    watch(
        &api(&fake),
        &run,
        &opts(Some(support::scratch("direct-card-no-usage-json")), true),
        &mut out,
        &mut err,
    )
    .await
    .unwrap();
    let last: Value =
        serde_json::from_str(String::from_utf8(out).unwrap().lines().last().unwrap()).unwrap();
    assert!(last.get("tokens").is_none(), "{last}");
    assert!(last.get("usd").is_none(), "{last}");
    assert_eq!(
        last["run"]["label"],
        "Cline stopped: no output for the silence limit"
    );

    let h = home(&fake, "direct-card-no-usage-bin");
    let o = ham(&h, &["pull", &run, "--json"]).await;
    let v: Value = serde_json::from_str(text_of(&o.stdout).trim()).unwrap();
    assert!(v.get("tokens").is_none() && v.get("usd").is_none(), "{v}");
}

// ---- the binary -----------------------------------------------------------------------------------------------------------

struct Home {
    root: PathBuf,
    downloads: PathBuf,
}

fn home(fake: &Fake, label: &str) -> Home {
    let root = support::scratch(label);
    let h = Home {
        downloads: root.join("downloads"),
        root,
    };
    std::fs::create_dir_all(&h.downloads).unwrap();
    std::fs::create_dir_all(h.root.join("config/ham")).unwrap();
    std::fs::write(
        h.root.join("config/ham/config.toml"),
        format!(
            "apiEndpoint = \"{}\"\nregion = \"{}\"\nawsProfile = \"ham-test\"\n",
            fake.endpoint, REGION
        ),
    )
    .unwrap();
    let script = h.root.join("creds.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho '{{\"Version\": 1, \"AccessKeyId\": \"{AKID}\", \"SecretAccessKey\": \"{SECRET}\", \"SessionToken\": \"{TOKEN}\", \"Expiration\": \"2099-01-01T00:00:00Z\"}}'\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        h.root.join("aws-config"),
        format!(
            "[profile ham-test]\ncredential_process = {}\nregion = {}\n",
            script.display(),
            REGION
        ),
    )
    .unwrap();
    std::fs::write(h.root.join("aws-credentials"), "").unwrap();
    h
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
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("TMPDIR", std::env::temp_dir())
        .env("HOME", &h.root)
        .env("XDG_CONFIG_HOME", h.root.join("config"))
        .env("XDG_STATE_HOME", h.root.join("state"))
        .env("XDG_CACHE_HOME", h.root.join("cache"))
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

fn text_of(b: &[u8]) -> String {
    text(b)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_binary_exits_0_for_a_halfway_stop_and_12_for_nothing_delivered() {
    let fake = Fake::start().await;
    let h = home(&fake, "direct-card-bin");

    let run = new_run(&fake, "bin halfway").await;
    fake.mem
        .complete_direct_run(&run, DirectOutcome::StoppedSilence, None);
    let o = ham(&h, &["watch", &run]).await;
    let out = text(&o.stdout);
    assert_eq!(o.status.code(), Some(0), "{out}\n{}", text(&o.stderr));
    assert!(
        out.lines()
            .any(|l| l == "Outcome   Cline stopped: no output for the silence limit"),
        "{out}"
    );
    assert_no_evidence(&out);
    assert_eq!(banned_claim_word(&out), None, "{out}");

    // `ham pull` prints the same card and, with --json, no evidence object.
    let o = ham(&h, &["pull", &run]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let out = text(&o.stdout);
    assert!(
        out.lines()
            .any(|l| l == "Outcome   Cline stopped: no output for the silence limit"),
        "{out}"
    );
    assert_no_evidence(&out);
    let o = ham(&h, &["pull", &run, "--json"]).await;
    let v: Value = serde_json::from_str(text(&o.stdout).trim()).unwrap();
    assert!(v.get("evidence").is_none(), "{v}");
    assert_eq!(v["label"], "Cline stopped: no output for the silence limit");

    let blocked = new_run(&fake, "bin blocked").await;
    fake.mem.block_direct_run(
        &blocked,
        "the session ended before any work was saved",
        Some(DirectOutcome::StoppedSessionLost),
    );
    let o = ham(&h, &["watch", &blocked]).await;
    assert_eq!(o.status.code(), Some(12), "{}", text(&o.stdout));
    let o = ham(&h, &["watch", &blocked, "--json"]).await;
    assert_eq!(o.status.code(), Some(12));
    let last: Value = serde_json::from_str(text(&o.stdout).lines().last().unwrap()).unwrap();
    assert_eq!(last["exitCode"], 12);
    assert_eq!(
        last["run"]["label"],
        DirectOutcome::StoppedSessionLost.label()
    );
}

/// Through the real transport: `ham watch` prints the A12 run's card with the model it ended on, the requested model, and the Detail
/// row as sentences; the line-mode status line says where the run ended; the `--json` object keeps the harness's `detail` word for word.
#[tokio::test]
async fn a_run_that_switched_models_prints_where_it_ended_and_keeps_the_harness_detail_in_json() {
    const DETAIL: &str = "Ran on us.zai.glm-5.3 after 9 switches. the engine ended the prompt cancelled with no cancel request (mistake limit or a cut model stream)";
    let fake = Fake::start().await;
    let run = new_run(&fake, "direct switched").await;
    fake.mem
        .complete_direct_run(&run, DirectOutcome::StoppedEngineCancelled, Some(DETAIL));
    let dir = support::scratch("direct-card-switched");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    watch(
        &api(&fake),
        &run,
        &opts(Some(dir.clone()), false),
        &mut out,
        &mut err,
    )
    .await
    .unwrap();
    let text = String::from_utf8(out).unwrap();
    for line in [
        "Detail    Ran on GLM 5.3 after 9 switches. The engine ended the prompt cancelled with no cancel request (mistake limit or a cut model stream).",
        "Model     GLM 5.3 (us.zai.glm-5.3)",
        "Requested Claude Sonnet 5.5 (global.anthropic.claude-sonnet-5-5)",
    ] {
        assert!(text.lines().any(|l| l == line), "no {line:?} in:\n{text}");
    }
    let status = last_status_line(&text);
    assert!(
        status.contains(" model Claude Sonnet 5.5 ran on GLM 5.3 after 9 switches "),
        "{status}"
    );
    // The label is the protocol's frozen text for the outcome, whatever the wording of the day.
    assert!(
        status.ends_with(&format!(
            "outcome: {}",
            DirectOutcome::StoppedEngineCancelled.label()
        )),
        "{status}"
    );
    assert_eq!(banned_claim_word(&text), None, "{text}");

    let run2 = new_run(&fake, "direct switched json").await;
    fake.mem
        .complete_direct_run(&run2, DirectOutcome::StoppedEngineCancelled, Some(DETAIL));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    watch(
        &api(&fake),
        &run2,
        &opts(Some(dir), true),
        &mut out,
        &mut err,
    )
    .await
    .unwrap();
    let last: Value =
        serde_json::from_str(String::from_utf8(out).unwrap().lines().last().unwrap()).unwrap();
    assert_eq!(last["run"]["detail"], DETAIL);
}
