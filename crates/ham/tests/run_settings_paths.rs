//! The three places that start a run read the same keys of `config.toml` through `ham_core::request::resolve_run_settings`: the command line
//! (`ham "<prompt>" --direct`, the real binary against the local `/v1` API), the composer (`tui::submit_direct_with_env`) and the follow-up
//! (`Job::StartFollowUp`). Before the function each site applied the file by hand, and the follow-up built its arguments with no goal, no
//! research and no third-party choice, so a default of the file did not reach it.
#![allow(clippy::unwrap_used)]

mod home;
mod support;

use ham::api::Api;
use ham::http::{ControlPlane, Response, TransportError};
use ham::tui::ext::{Done, Job, perform_with_env};
use ham_core::command::{Command, parse};
use home::{Home, ham_env, home, text};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use support::Fake;

const PROMPT: &str = "Add a percent_change(a, b) function";
const ENDPOINT: &str = "/v1/direct/runs";
const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const NEW: &str = "01M5A2R7K3C9XJ4D8E6FNEWRUN";
const SOL: &str = "us.openai.gpt-6.1-sol";

/// Every run key of the file set to something other than its default; rules and skills are off so that no folder is needed.
const ALL_KEYS: &str = "defaultModel = \"us.openai.gpt-6.1-sol\"\ninputWaitMinutes = 12\nreportTests = false\nconfigVersion = 1\n\n[run]\ngoal = false\nrules = false\nskills = false\n\n[research]\nenabled = true\n";

fn append_config(h: &Home, text: &str) {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(h.config.join("ham/config.toml"))
        .unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

// ---- the three entry points -------------------------------------------------------------------------------------------------------

async fn command_line(config: &str, extra: &[&str], label: &str) -> Value {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, label);
    append_config(&h, config);
    let mut words = vec![PROMPT, "--s3-only", "--direct", "--json"];
    words.extend_from_slice(extra);
    let o = ham_env(
        &h,
        &words,
        &[("HAMH_IDEMPOTENCY_NONCE", "run-settings-cli")],
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    serde_json::from_slice(&fake.mem.posts(ENDPOINT)[0].body).unwrap()
}

async fn composer(config: &str, extra: &[&str], label: &str) -> Value {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, label);
    append_config(&h, config);
    let mut words: Vec<String> = ["--prompt", PROMPT, "--s3-only", "--direct"]
        .iter()
        .map(ToString::to_string)
        .collect();
    words.extend(extra.iter().map(ToString::to_string));
    let Command::StartDirect(args) = parse(&words).unwrap().command else {
        panic!("the words do not make a direct start");
    };
    let (config_dir, home_dir) = (h.config.display().to_string(), h.root.display().to_string());
    let env = move |k: &str| match k {
        "XDG_CONFIG_HOME" => Some(config_dir.clone()),
        "HOME" => Some(home_dir.clone()),
        _ => None,
    };
    let api = Api::new(Arc::new(
        ham::https::HttpsPlane::new(
            &fake.endpoint,
            ham::sigv4::Signer::new(support::REGION, support::creds()),
        )
        .unwrap(),
    ));
    ham::tui::submit_direct_with_env(&api, &args, "run-settings-composer", &env)
        .await
        .unwrap();
    serde_json::from_slice(&fake.mem.posts(ENDPOINT)[0].body).unwrap()
}

type Call = (String, String, Value);

#[derive(Default)]
struct Scripted {
    calls: Mutex<Vec<Call>>,
}

impl ControlPlane for Scripted {
    async fn call(
        &self,
        method: &str,
        path: &str,
        _headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        self.calls.lock().unwrap().push((
            method.to_string(),
            path.to_string(),
            serde_json::from_slice(&body).unwrap_or(Value::Null),
        ));
        Ok(Response {
            status: 202,
            headers: BTreeMap::new(),
            body: serde_json::to_vec(&json!({"runId": NEW, "status": "active"})).unwrap(),
        })
    }
    async fn fetch(
        &self,
        _url: &str,
        _range: Option<(u64, u64)>,
        _if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        Err(TransportError::Refused("not used".into()))
    }
}

/// A home with the user's rules, as `ham rules init` leaves it, and `config` as `config.toml`.
fn follow_up_env(label: &str, config: &str) -> impl Fn(&str) -> Option<String> {
    let root = support::scratch(label);
    let ham = root.join("config/ham");
    std::fs::create_dir_all(ham.join("rules")).unwrap();
    std::fs::write(ham.join("rules/10-tdd.md"), "Write the test first.\n").unwrap();
    std::fs::write(ham.join("config.toml"), config).unwrap();
    let (config_dir, home_dir) = (root.join("config"), root);
    move |k: &str| match k {
        "XDG_CONFIG_HOME" => Some(config_dir.display().to_string()),
        "HOME" => Some(home_dir.display().to_string()),
        _ => None,
    }
}

async fn follow_up(config: &str, label: &str) -> Value {
    let plane = Arc::new(Scripted::default());
    let api = Api::new(plane.clone());
    let env = follow_up_env(label, config);
    let done = perform_with_env(
        &api,
        Job::StartFollowUp {
            run_id: RUN.into(),
            text: "now add the docs".into(),
            head: None,
        },
        &env,
    )
    .await;
    match done {
        Done::FollowUpStarted { result, .. } => assert_eq!(result, Ok(NEW.to_string())),
        other => panic!("{other:?}"),
    }
    let calls = plane.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(
        (calls[0].0.as_str(), calls[0].1.as_str()),
        ("POST", ENDPOINT)
    );
    calls[0].2.clone()
}

// ---- the tests --------------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_composer_and_follow_up_build_equal_requests_for_every_run_key() {
    let cli = command_line(ALL_KEYS, &[], "rs-all-cli").await;
    let tui = composer(ALL_KEYS, &[], "rs-all-tui").await;
    let fu = follow_up(
        &format!("apiEndpoint = \"https://unused.example/v1\"\n{ALL_KEYS}"),
        "rs-all-fu",
    )
    .await;
    // The request members that the keys of the file decide (a request with no rules carries an empty `rules` list and no `skills`).
    let want: [(&str, Option<Value>); 8] = [
        ("model", Some(json!(SOL))),
        ("inputWaitS", Some(json!(720))),
        ("goal", Some(json!({"enabled": false}))),
        ("research", Some(json!({"enabled": true}))),
        ("reportTests", Some(json!(false))),
        ("rules", Some(json!([]))),
        ("skills", None),
        ("thirdParty", None),
    ];
    for (site, sent) in [
        ("command line", &cli),
        ("composer", &tui),
        ("follow-up", &fu),
    ] {
        for (member, value) in &want {
            assert_eq!(
                sent.get(*member).cloned(),
                *value,
                "{site}: {member} in {sent}"
            );
        }
    }
    // The follow-up also carries its own member, and the target a follow-up always has.
    assert_eq!(fu["continueFrom"], json!({"runId": RUN}));
    // Marker for the record: the trigger of this test is present (the file holds every key).
    println!(
        "RUN-SETTINGS TRIGGER PRESENT: keys={} goal={} research={} model={}",
        ALL_KEYS.lines().filter(|l| l.contains('=')).count(),
        fu["goal"],
        fu["research"],
        fu["model"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_follow_up_honours_the_goal_and_research_keys_of_the_file() {
    // The defect: the follow-up built its arguments with goal: None and research: None, so these did not reach it.
    let off = follow_up("[run]\ngoal = false\n", "rs-fu-goal").await;
    assert_eq!(off["goal"], json!({"enabled": false}), "{off}");
    let research = follow_up("[research]\nenabled = false\n", "rs-fu-research").await;
    assert_eq!(
        research["research"],
        json!({"enabled": false}),
        "{research}"
    );
    // No key, no member: the request is the one a follow-up always sent.
    let plain = follow_up("", "rs-fu-plain").await;
    for member in ["goal", "research", "reportTests", "thirdParty"] {
        assert!(plain.get(member).is_none(), "{member} in {plain}");
    }
}

/// A rules folder and a skills folder beside `config.toml`, each with one file.
fn rules_and_skills(h: &Home) {
    let ham = h.config.join("ham");
    std::fs::create_dir_all(ham.join("rules")).unwrap();
    std::fs::write(ham.join("rules/10-tdd.md"), "Write the test first.\n").unwrap();
    let skill = ham.join("skills/demo-skill");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: demo-skill\ndescription: A skill the tests use.\n---\nDo the demo.\n",
    )
    .unwrap();
}

async fn sent_with_folders(config: &str, extra: &[&str], label: &str) -> Value {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, label);
    append_config(&h, config);
    rules_and_skills(&h);
    let mut words = vec![PROMPT, "--s3-only", "--direct", "--json"];
    words.extend_from_slice(extra);
    let o = ham_env(
        &h,
        &words,
        &[("HAMH_IDEMPOTENCY_NONCE", "run-settings-folders")],
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    serde_json::from_slice(&fake.mem.posts(ENDPOINT)[0].body).unwrap()
}

fn count(v: &Value, member: &str) -> usize {
    v.get(member).and_then(Value::as_array).map_or(0, Vec::len)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_rules_and_skills_keys_decide_which_folders_a_run_reads() {
    // Default: both folders are sent.
    let v = sent_with_folders("", &[], "rs-folders-default").await;
    assert_eq!((count(&v, "rules"), count(&v, "skills")), (1, 1), "{v}");
    // run.rules = false is --no-rules, which sends no skills either.
    let v = sent_with_folders(
        "configVersion = 1\n\n[run]\nrules = false\n",
        &[],
        "rs-folders-norules",
    )
    .await;
    assert_eq!((count(&v, "rules"), count(&v, "skills")), (0, 0), "{v}");
    // run.skills = false keeps the rules and sends no skills.
    let v = sent_with_folders(
        "configVersion = 1\n\n[run]\nskills = false\n",
        &[],
        "rs-folders-noskills",
    )
    .await;
    assert_eq!((count(&v, "rules"), count(&v, "skills")), (1, 0), "{v}");
    // A flag beats the file: --no-rules with rules = true; --rules <dir> with rules = false.
    let v = sent_with_folders(
        "configVersion = 1\n\n[run]\nrules = true\n",
        &["--no-rules"],
        "rs-folders-flag-off",
    )
    .await;
    assert_eq!((count(&v, "rules"), count(&v, "skills")), (0, 0), "{v}");
    let other = support::scratch("rs-other-rules");
    std::fs::write(other.join("90-other.md"), "Other rule.\n").unwrap();
    let dir = other.display().to_string();
    let v = sent_with_folders(
        "configVersion = 1\n\n[run]\nrules = false\n",
        &["--rules", &dir],
        "rs-folders-flag-on",
    )
    .await;
    assert_eq!(v["rules"][0]["path"], json!("90-other.md"), "{v}");
    assert_eq!(count(&v, "rules"), 1);
}

#[test]
fn no_start_site_calls_with_report_setting_any_more() {
    // The three sites call the one function; the report switch and the wait are folded into it.
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for file in ["cli.rs", "tui/mod.rs", "tui/ext.rs"] {
        let text = std::fs::read_to_string(src.join(file)).unwrap();
        for gone in ["with_report_setting", "with_input_wait", "WaitPrefs"] {
            assert!(!text.contains(gone), "{file} still names {gone}");
        }
    }
    for file in ["cli.rs", "tui/mod.rs", "tui/ext.rs"] {
        let text = std::fs::read_to_string(src.join(file)).unwrap();
        assert!(
            text.contains("resolve_run_settings"),
            "{file} does not call the resolver"
        );
    }
}
