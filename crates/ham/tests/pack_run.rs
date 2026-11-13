//! The pack in a run: what `ham` reads from `~/.config/ham/skills/` and sends as the `skills` member, what `--no-rules` leaves out,
//! how `ham rules show` lists skills, and `ham continue` against the real control-api handler. The real binary, a temp HOME and the
//! local `/v1` API of the `ham` tests.
#![allow(clippy::unwrap_used)]

mod support;

use ham_core::pack;
use harness_protocol::direct::{check_skills, skill_sha256};
use harness_protocol::store::s3;
use serde_json::Value;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Output;
use support::{AKID, Fake, SECRET, TOKEN};

const MODEL: &str = "global.anthropic.claude-sonnet-5-5";
const HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";

struct Home {
    root: PathBuf,
}

impl Home {
    fn skills_dir(&self) -> PathBuf {
        self.root.join(".config/ham/skills")
    }
}

fn home(fake: &Fake, label: &str) -> Home {
    let root = support::scratch(label);
    std::fs::create_dir_all(root.join(".config/ham")).unwrap();
    std::fs::write(
        root.join(".config/ham/config.toml"),
        format!(
            "apiEndpoint = \"{}\"\nregion = \"{}\"\nawsProfile = \"ham-test\"\n",
            fake.endpoint,
            support::REGION
        ),
    )
    .unwrap();
    let script = root.join("creds.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho '{{\"Version\": 1, \"AccessKeyId\": \"{AKID}\", \"SecretAccessKey\": \"{SECRET}\", \"SessionToken\": \"{TOKEN}\", \"Expiration\": \"2099-01-01T00:00:00Z\"}}'\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        root.join("aws-config"),
        format!(
            "[profile ham-test]\ncredential_process = {}\nregion = {}\n",
            script.display(),
            support::REGION
        ),
    )
    .unwrap();
    std::fs::write(root.join("aws-credentials"), "").unwrap();
    std::fs::create_dir_all(root.join("downloads")).unwrap();
    Home { root }
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
        .env("TMPDIR", &h.root)
        .env("HOME", &h.root)
        .env("HAMH_DOWNLOAD_DIR", h.root.join("downloads"))
        .env("AWS_CONFIG_FILE", h.root.join("aws-config"))
        .env(
            "AWS_SHARED_CREDENTIALS_FILE",
            h.root.join("aws-credentials"),
        )
        .env("AWS_EC2_METADATA_DISABLED", "true");
    cmd.stdin(std::process::Stdio::null())
        .output()
        .await
        .unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn start_args<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    let mut a = vec![
        "Add a percent_change function with tests",
        "--s3-only",
        "--direct",
        "--model",
        MODEL,
        "--json",
    ];
    a.extend(extra);
    a
}

fn body_of(fake: &Fake, n: usize) -> Value {
    let posts = fake.mem.posts("/v1/direct/runs");
    serde_json::from_slice(&posts[n].body).unwrap()
}

fn skill_dir(h: &Home, name: &str, files: &[(&str, &str)]) {
    for (rel, content) in files {
        let p = h.skills_dir().join(name).join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
}

const ENTRY: &str = "---\nname: my-skill\ndescription: Mine.\n---\nDo my thing.\n";
const ENTRY_LINKED: &str = "---\nname: linked\ndescription: Linked.\n---\nDo it.\n";

// ---- skills are read from the config directory and sent --------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_installed_pack_travels_with_the_run_as_rules_and_skills() {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "run-pack");
    assert_eq!(
        ham(&h, &["rules", "init", "--pack", "lifecycle", "--skills"])
            .await
            .status
            .code(),
        Some(0)
    );
    let o = ham(&h, &start_args(&[])).await;
    assert_eq!(o.status.code(), Some(0), "{}\n{}", out(&o), err(&o));
    let b = body_of(&fake, 0);
    let rules: Vec<&str> = b["rules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["path"].as_str().unwrap())
        .collect();
    assert_eq!(rules, ["00-my-rules.md", "10-lifecycle.md"]);
    let sent: Vec<harness_protocol::direct::Skill> =
        serde_json::from_value(b["skills"].clone()).unwrap();
    assert_eq!(sent, pack::skills(), "the seven pack skills, byte for byte");
    assert_eq!(check_skills(&sent), Ok(()));
    // The real control-api handler stored it as sent.
    let stored = fake.mem.stored_request_of(b_run_id(&o));
    assert_eq!(stored["skills"], b["skills"]);
}

fn b_run_id(o: &Output) -> &str {
    let first = out(o).lines().next().unwrap().to_string();
    let v: Value = serde_json::from_str(&first).unwrap();
    Box::leak(v["runId"].as_str().unwrap().to_string().into_boxed_str())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_skills_directory_the_request_has_no_skills_member() {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "run-noskills");
    assert_eq!(
        ham(&h, &["rules", "init", "--no-skills"])
            .await
            .status
            .code(),
        Some(0)
    );
    let o = ham(&h, &start_args(&[])).await;
    assert_eq!(o.status.code(), Some(0), "{}\n{}", out(&o), err(&o));
    let b = body_of(&fake, 0);
    assert!(b.get("skills").is_none(), "{b}");
    assert!(b.get("continueFrom").is_none(), "{b}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_rules_sends_none_of_the_pack() {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "run-norules");
    assert_eq!(
        ham(&h, &["rules", "init", "--pack", "lifecycle", "--skills"])
            .await
            .status
            .code(),
        Some(0)
    );
    let o = ham(&h, &start_args(&["--no-rules"])).await;
    assert_eq!(o.status.code(), Some(0), "{}\n{}", out(&o), err(&o));
    let b = body_of(&fake, 0);
    assert_eq!(b["rules"].as_array().unwrap().len(), 0, "{b}");
    assert!(b.get("skills").is_none(), "{b}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hand_made_skill_is_sent_and_hidden_files_are_left_out() {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "run-mine");
    assert_eq!(
        ham(&h, &["rules", "init", "--no-skills"])
            .await
            .status
            .code(),
        Some(0)
    );
    skill_dir(
        &h,
        "my-skill",
        &[
            ("SKILL.md", ENTRY),
            ("references/notes.md", "Notes.\n"),
            (".DS_Store", "junk"),
        ],
    );
    std::fs::write(h.skills_dir().join(".hidden-file"), "x").unwrap();
    let o = ham(&h, &start_args(&[])).await;
    assert_eq!(o.status.code(), Some(0), "{}\n{}", out(&o), err(&o));
    let sent: Vec<harness_protocol::direct::Skill> =
        serde_json::from_value(body_of(&fake, 0)["skills"].clone()).unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].name, "my-skill");
    let paths: Vec<&str> = sent[0].files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["SKILL.md", "references/notes.md"]);
}

fn assert_refused(fake: &Fake, o: &Output, must: &[&str]) {
    assert_eq!(o.status.code(), Some(1), "{}\n{}", out(o), err(o));
    for m in must {
        assert!(err(o).contains(m), "{m:?} in {}", err(o));
    }
    assert!(
        fake.mem.posts("/v1/direct/runs").is_empty(),
        "nothing was sent"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_skill_stops_the_run_before_anything_is_sent_and_names_the_skill_and_file() {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "run-bad");
    assert_eq!(ham(&h, &["rules", "init"]).await.status.code(), Some(0));

    // A folder with no SKILL.md.
    skill_dir(&h, "no-entry", &[("notes.md", "x\n")]);
    assert_refused(
        &fake,
        &ham(&h, &start_args(&[])).await,
        &["no-entry", "SKILL.md"],
    );
    std::fs::remove_file(h.skills_dir().join("no-entry/notes.md")).unwrap();
    std::fs::remove_dir(h.skills_dir().join("no-entry")).unwrap();

    // A name the server would refuse.
    skill_dir(&h, "Bad Name", &[("SKILL.md", ENTRY)]);
    assert_refused(
        &fake,
        &ham(&h, &start_args(&[])).await,
        &["Bad Name", "name"],
    );
    std::fs::remove_file(h.skills_dir().join("Bad Name/SKILL.md")).unwrap();
    std::fs::remove_dir(h.skills_dir().join("Bad Name")).unwrap();

    // A file that is not a skill folder at the top.
    std::fs::write(h.skills_dir().join("README.md"), "x").unwrap();
    assert_refused(&fake, &ham(&h, &start_args(&[])).await, &["README.md"]);
    std::fs::remove_file(h.skills_dir().join("README.md")).unwrap();

    // A file longer than a whole request can be: refused before it is read.
    skill_dir(&h, "big", &[("SKILL.md", ENTRY)]);
    std::fs::File::create(h.skills_dir().join("big/data.md"))
        .unwrap()
        .set_len(6_225_921)
        .unwrap();
    assert_refused(
        &fake,
        &ham(&h, &start_args(&[])).await,
        &["big", "data.md", "never be sent"],
    );
    std::fs::remove_file(h.skills_dir().join("big/data.md")).unwrap();

    // Not UTF-8 text.
    std::fs::write(
        h.skills_dir().join("big/blob.bin"),
        [0xff, 0xfe, 0x00, 0x01],
    )
    .unwrap();
    assert_refused(
        &fake,
        &ham(&h, &start_args(&[])).await,
        &["big", "blob.bin", "UTF-8"],
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_symbolic_link_in_a_skill_is_refused_and_a_linked_skill_folder_is_followed() {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "run-link");
    assert_eq!(
        ham(&h, &["rules", "init", "--no-skills"])
            .await
            .status
            .code(),
        Some(0)
    );
    let secret = h.root.join("outside.txt");
    std::fs::write(&secret, "not for the run\n").unwrap();
    skill_dir(&h, "my-skill", &[("SKILL.md", ENTRY)]);
    symlink(&secret, h.skills_dir().join("my-skill/leak.md")).unwrap();
    assert_refused(
        &fake,
        &ham(&h, &start_args(&[])).await,
        &["my-skill", "leak.md", "symbolic link"],
    );

    // A whole skill folder that is a link is followed (a dotfile manager makes them, as it does for rules); the files inside stay plain.
    std::fs::remove_file(h.skills_dir().join("my-skill/leak.md")).unwrap();
    let other = support::scratch("run-link-other");
    std::fs::create_dir_all(other.join("x")).unwrap();
    std::fs::write(other.join("x/SKILL.md"), ENTRY_LINKED).unwrap();
    symlink(other.join("x"), h.skills_dir().join("linked")).unwrap();
    let o = ham(&h, &start_args(&[])).await;
    assert_eq!(o.status.code(), Some(0), "{}\n{}", out(&o), err(&o));
    let sent: Vec<harness_protocol::direct::Skill> =
        serde_json::from_value(body_of(&fake, 0)["skills"].clone()).unwrap();
    let names: Vec<&str> = sent.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["linked", "my-skill"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thirty_three_skills_are_refused_by_the_client_with_the_servers_words() {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "run-nine");
    assert_eq!(
        ham(&h, &["rules", "init", "--no-skills"])
            .await
            .status
            .code(),
        Some(0)
    );
    for i in 0..33 {
        skill_dir(&h, &format!("skill-{i}"), &[("SKILL.md", ENTRY)]);
    }
    assert_refused(
        &fake,
        &ham(&h, &start_args(&[])).await,
        &["33 skills", "limit is 32"],
    );
}

/// A valid skill folder of the user's own, named `name` (the folder name and the `name` line agree).
fn own_skill(h: &Home, name: &str) {
    skill_dir(
        h,
        name,
        &[(
            "SKILL.md",
            &format!("---\nname: {name}\ndescription: Mine.\n---\nDo my thing.\n"),
        )],
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pack_and_two_skills_of_yours_stop_the_run_with_the_cause_and_the_fix() {
    // The default install puts seven skills in the folder, so a user with 26 of their own has 33: the run takes 32. The refusal used to say
    // only "33 skills; the limit is 32". It names where the skills come from and what to do, and the fix it names works.
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "run-pack-plus-two");
    assert_eq!(ham(&h, &["rules", "init"]).await.status.code(), Some(0));
    for i in 0..26 {
        own_skill(&h, &format!("my-skill-{i:02}"));
    }
    let o = ham(&h, &start_args(&[])).await;
    assert_refused(&fake, &o, &["33 skills", "limit is 32"]);
    let e = err(&o);
    for must in [
        "7 are the lifecycle pack's",
        "ham rules init",
        "26 are yours",
        "my-skill-00",
        "my-skill-25",
        "--no-skills",
    ] {
        assert!(e.contains(must), "{must:?} in {e}");
    }
    assert!(
        e.contains(&h.skills_dir().display().to_string()),
        "names the folder: {e}"
    );
    // `ham rules show` reads the same folder and says the same.
    let show = ham(&h, &["rules", "show"]).await;
    assert_eq!(
        show.status.code(),
        Some(1),
        "{}\n{}",
        out(&show),
        err(&show)
    );
    assert!(
        err(&show).contains("7 are the lifecycle pack's") && err(&show).contains("--no-skills"),
        "{}",
        err(&show)
    );
    // The fix it names: the run starts with `--no-skills` and sends none.
    let o = ham(&h, &start_args(&["--no-skills"])).await;
    assert_eq!(o.status.code(), Some(0), "{}\n{}", out(&o), err(&o));
    assert!(body_of(&fake, 0).get("skills").is_none());
    // And so does removing what you do not need: 25 of yours and the seven are 32.
    std::fs::remove_file(h.skills_dir().join("my-skill-25/SKILL.md")).unwrap();
    std::fs::remove_dir(h.skills_dir().join("my-skill-25")).unwrap();
    let o = ham(&h, &start_args(&[])).await;
    assert_eq!(o.status.code(), Some(0), "{}\n{}", out(&o), err(&o));
    let sent: Vec<harness_protocol::direct::Skill> =
        serde_json::from_value(body_of(&fake, 1)["skills"].clone()).unwrap();
    assert_eq!(sent.len(), 32, "the seven of the pack and 25 of yours");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thirty_three_skills_of_yours_get_the_same_help_without_the_pack_sentence() {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "run-nine-own");
    assert_eq!(
        ham(&h, &["rules", "init", "--no-skills"])
            .await
            .status
            .code(),
        Some(0)
    );
    for i in 0..33 {
        own_skill(&h, &format!("skill-{i}"));
    }
    let o = ham(&h, &start_args(&[])).await;
    assert_refused(&fake, &o, &["33 skills", "limit is 32", "--no-skills"]);
    assert!(
        !err(&o).contains("lifecycle pack's"),
        "none of them is the pack's: {}",
        err(&o)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_user_skill_in_the_lifecycle_namespace_is_refused_before_anything_is_sent() {
    // The prefix `lifecycle-` is the platform's. The installed pack (seven skills of that prefix) goes out with the run; a skill of
    // the user's own with another name inside the prefix does not, and the message says why and what to do.
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "run-reserved");
    assert_eq!(
        ham(&h, &["rules", "init", "--pack", "lifecycle", "--skills"])
            .await
            .status
            .code(),
        Some(0)
    );
    skill_dir(
        &h,
        "lifecycle-notes",
        &[(
            "SKILL.md",
            "---\nname: lifecycle-notes\ndescription: Mine.\n---\nDo my notes.\n",
        )],
    );
    let o = ham(&h, &start_args(&[])).await;
    assert_refused(
        &fake,
        &o,
        &[
            "lifecycle-notes",
            "reserved",
            "lifecycle-tdd",
            "another name",
        ],
    );
    // `ham rules show` reads the same folder and says the same.
    let show = ham(&h, &["rules", "show"]).await;
    assert_eq!(
        show.status.code(),
        Some(1),
        "{}\n{}",
        out(&show),
        err(&show)
    );
    assert!(err(&show).contains("reserved"), "{}", err(&show));
    // A name of the user's own beside the pack is sent with it.
    std::fs::remove_file(h.skills_dir().join("lifecycle-notes/SKILL.md")).unwrap();
    std::fs::remove_dir(h.skills_dir().join("lifecycle-notes")).unwrap();
    skill_dir(&h, "my-skill", &[("SKILL.md", ENTRY)]);
    let o = ham(&h, &start_args(&[])).await;
    assert_eq!(o.status.code(), Some(0), "{}\n{}", out(&o), err(&o));
    let sent: Vec<harness_protocol::direct::Skill> =
        serde_json::from_value(body_of(&fake, 0)["skills"].clone()).unwrap();
    assert_eq!(sent.len(), 8, "the seven of the pack and my-skill");
}

// ---- ham rules show lists the skills the next run would send ------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn show_lists_the_skills_with_their_digests() {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "run-show");
    assert_eq!(
        ham(&h, &["rules", "init", "--pack", "lifecycle", "--skills"])
            .await
            .status
            .code(),
        Some(0)
    );
    let o = ham(&h, &["rules", "show"]).await;
    assert_eq!(o.status.code(), Some(0), "{}{}", out(&o), err(&o));
    let text = out(&o);
    for s in pack::skills() {
        assert!(
            text.contains(&s.name) && text.contains(&skill_sha256(&s)),
            "{}: {text}",
            s.name
        );
    }
    let j: Value =
        serde_json::from_str(&out(&ham(&h, &["rules", "show", "--json"]).await)).unwrap();
    let skills = j["skills"].as_array().unwrap();
    assert_eq!(skills.len(), 7);
    assert_eq!(skills[0]["name"], "lifecycle-build-test");
    assert_eq!(skills[0]["sha256"], skill_sha256(&pack::skills()[0]));
    assert!(skills[0]["files"].as_u64().unwrap() >= 1);
    assert_eq!(j["skillsDir"], h.skills_dir().display().to_string());
    // Without skills the JSON says so.
    let h2 = home(&fake, "run-show-none");
    assert_eq!(
        ham(&h2, &["rules", "init", "--no-skills"])
            .await
            .status
            .code(),
        Some(0)
    );
    let j: Value =
        serde_json::from_str(&out(&ham(&h2, &["rules", "show", "--json"]).await)).unwrap();
    assert_eq!(j["skills"].as_array().unwrap().len(), 0);
}

// ---- ham continue ------------------------------------------------------------------------------------------------------------

fn bundle_for(head: &str, run: &str) -> Vec<u8> {
    let mut b = format!("# v2 git bundle\n{head} refs/heads/harness/{run}\n\nPACK").into_bytes();
    b.extend(std::iter::repeat_n(7u8, 2000));
    b
}

/// A run the fake finished (autopilot) plus the verified bundle a real delivery leaves.
async fn finished_run(fake: &Fake, h: &Home) -> String {
    let o = ham(h, &start_args(&[])).await;
    assert_eq!(o.status.code(), Some(0), "{}\n{}", out(&o), err(&o));
    let run = b_run_id(&o).to_string();
    // The control plane keeps Run.status; run-session sets it when a run ends, and control-api refuses to continue a run still active.
    fake.mem.finish_run(&run);
    fake.mem
        .put_object(&s3::verified_bundle_key(&run), bundle_for(HEAD, &run));
    run
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continue_starts_a_new_s3_only_run_from_the_delivered_head() {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "continue-ok");
    assert_eq!(ham(&h, &["rules", "init"]).await.status.code(), Some(0));
    let first = finished_run(&fake, &h).await;
    let o = ham(
        &h,
        &[
            "continue",
            &first,
            "add a second file",
            "--model",
            MODEL,
            "--json",
        ],
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}\n{}", out(&o), err(&o));
    let second = b_run_id(&o).to_string();
    assert_ne!(second, first);
    // The run it continues was a zip alone, so the follow-up is too, and the start line says where the target came from.
    let start: Value = serde_json::from_str(out(&o).lines().next().unwrap()).unwrap();
    assert_eq!(
        start["target"],
        serde_json::json!({"mode": "s3-only"}),
        "{start}"
    );
    assert_eq!(start["targetSource"], "run", "{start}");
    let b = body_of(&fake, 1);
    assert_eq!(b["prompt"], "add a second file");
    assert_eq!(b["target"]["mode"], "s3-only");
    assert!(b.get("baseRef").is_none());
    assert_eq!(b["continueFrom"]["runId"], first.as_str());
    assert!(
        b["continueFrom"].get("headSha").is_none(),
        "the client names no head; the server resolves it"
    );
    let stored = fake.mem.stored_request_of(&second);
    assert_eq!(stored["continueFrom"]["runId"], first.as_str());
    assert_eq!(
        stored["continueFrom"]["headSha"], HEAD,
        "control-api stored the head it read from the verified bundle"
    );
    assert_eq!(stored["prompt"], "add a second file");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continue_sends_the_rules_and_skills_like_any_new_run_and_none_with_no_rules() {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "continue-pack");
    assert_eq!(
        ham(&h, &["rules", "init", "--pack", "lifecycle", "--skills"])
            .await
            .status
            .code(),
        Some(0)
    );
    let first = finished_run(&fake, &h).await;
    let o = ham(
        &h,
        &["continue", &first, "go on", "--model", MODEL, "--json"],
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}\n{}", out(&o), err(&o));
    let b = body_of(&fake, 1);
    assert_eq!(b["rules"].as_array().unwrap().len(), 2);
    assert_eq!(b["skills"].as_array().unwrap().len(), 7);
    let o = ham(
        &h,
        &[
            "continue",
            &first,
            "again",
            "--model",
            MODEL,
            "--no-rules",
            "--json",
        ],
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}\n{}", out(&o), err(&o));
    let b = body_of(&fake, 2);
    assert_eq!(b["rules"].as_array().unwrap().len(), 0);
    assert!(b.get("skills").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continue_with_a_run_that_does_not_exist_fails_with_the_servers_answer_and_starts_nothing()
{
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "continue-missing");
    assert_eq!(ham(&h, &["rules", "init"]).await.status.code(), Some(0));
    let o = ham(
        &h,
        &[
            "continue",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "go on",
            "--model",
            MODEL,
        ],
    )
    .await;
    assert_eq!(o.status.code(), Some(1), "{}\n{}", out(&o), err(&o));
    assert!(
        err(&o).contains("not-found") || err(&o).contains("no run"),
        "{}",
        err(&o)
    );
    // The client reads the run's target before it starts anything, so the refusal is the lookup's and no start is posted. Before,
    // the start itself was posted and refused ("one refused post").
    assert!(
        fake.mem.posts("/v1/direct/runs").is_empty(),
        "no start was posted"
    );
    assert!(
        err(&o).contains("--s3-only"),
        "the refusal names the way to choose the target yourself: {}",
        err(&o)
    );
    assert!(fake.mem.run_ids().is_empty(), "no run was created");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continue_with_a_run_that_delivered_no_bundle_says_so() {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "continue-nobundle");
    assert_eq!(ham(&h, &["rules", "init"]).await.status.code(), Some(0));
    let o = ham(&h, &start_args(&[])).await;
    let first = b_run_id(&o).to_string();
    fake.mem.finish_run(&first);
    let o = ham(&h, &["continue", &first, "go on", "--model", MODEL]).await;
    assert_eq!(o.status.code(), Some(1), "{}\n{}", out(&o), err(&o));
    assert!(
        err(&o).contains("no-delivered-head") || err(&o).contains("delivered no code"),
        "{}",
        err(&o)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continue_needs_a_prompt_and_a_valid_run_id_before_any_request() {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "continue-usage");
    assert_eq!(ham(&h, &["rules", "init"]).await.status.code(), Some(0));
    let o = ham(&h, &["continue", "01ARZ3NDEKTSV4RRFFQ69G5FAV"]).await;
    assert_eq!(o.status.code(), Some(2), "{}\n{}", out(&o), err(&o));
    assert!(err(&o).contains("prompt"), "{}", err(&o));
    let o = ham(&h, &["continue"]).await;
    assert_eq!(o.status.code(), Some(2));
    assert!(
        fake.mem.seen().is_empty(),
        "no request was made for a usage error"
    );
}

// ---- ham continue follows the target of the run it continues -------------------------------------------------------

fn repo_start_args<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    let mut a = vec![
        "Add a percent_change function with tests",
        "--repo",
        "cc:calc",
        "--direct",
        "--model",
        MODEL,
        "--json",
    ];
    a.extend(extra);
    a
}

/// A finished run on the repository `cc:calc` plus the verified bundle a real delivery leaves.
async fn finished_repo_run(fake: &Fake, h: &Home) -> String {
    let o = ham(h, &repo_start_args(&[])).await;
    assert_eq!(o.status.code(), Some(0), "{}\n{}", out(&o), err(&o));
    let run = b_run_id(&o).to_string();
    fake.mem.finish_run(&run);
    fake.mem
        .put_object(&s3::verified_bundle_key(&run), bundle_for(HEAD, &run));
    run
}

fn run_lookups(fake: &Fake, run: &str) -> usize {
    fake.mem
        .seen()
        .iter()
        .filter(|s| s.method == "GET" && s.path == format!("/v1/runs/{run}"))
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continue_on_a_repository_run_goes_to_the_same_repository() {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "continue-repo");
    assert_eq!(ham(&h, &["rules", "init"]).await.status.code(), Some(0));
    let first = finished_repo_run(&fake, &h).await;
    let o = ham(
        &h,
        &[
            "continue",
            &first,
            "add the docs",
            "--model",
            MODEL,
            "--json",
        ],
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}\n{}", out(&o), err(&o));
    let start: Value = serde_json::from_str(out(&o).lines().next().unwrap()).unwrap();
    let second = start["runId"].as_str().unwrap().to_string();
    assert_eq!(
        start["target"],
        serde_json::json!({"mode": "repo", "repo": "cc:calc"}),
        "{start}"
    );
    assert_eq!(start["targetSource"], "run", "{start}");
    let b = body_of(&fake, 1);
    assert_eq!(
        b["target"],
        serde_json::json!({"mode": "repo", "repo": "cc:calc"})
    );
    assert!(
        b.get("baseRef").is_none(),
        "the head is the source's delivered commit"
    );
    assert_eq!(b["continueFrom"]["runId"], first.as_str());
    let stored = fake.mem.stored_request_of(&second);
    assert_eq!(stored["target"]["repo"], "cc:calc");
    assert_eq!(stored["continueFrom"]["headSha"], HEAD);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continue_says_which_target_the_new_run_has_and_where_that_came_from() {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "continue-says");
    assert_eq!(ham(&h, &["rules", "init"]).await.status.code(), Some(0));
    let first = finished_repo_run(&fake, &h).await;
    // Without --json: one line on standard error, before the run is followed.
    let o = ham(&h, &["continue", &first, "add the docs", "--model", MODEL]).await;
    assert_eq!(o.status.code(), Some(0), "{}\n{}", out(&o), err(&o));
    let line = format!(
        "target: cc:calc, a pull request in that repository and a zip (the target of run {first})"
    );
    assert!(err(&o).contains(&line), "{}", err(&o));
    assert!(out(&o).starts_with("started run "), "{}", out(&o));
    // A target typed on the line is named as chosen.
    let o = ham(
        &h,
        &["continue", &first, "again", "--model", MODEL, "--s3-only"],
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}\n{}", out(&o), err(&o));
    assert!(
        err(&o).contains("target: s3-only, a zip and no pull request (chosen on the command line)"),
        "{}",
        err(&o)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_explicit_target_wins_and_needs_no_lookup_of_the_run() {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "continue-explicit");
    assert_eq!(ham(&h, &["rules", "init"]).await.status.code(), Some(0));
    let first = finished_repo_run(&fake, &h).await;
    let before = run_lookups(&fake, &first);
    // A zip alone for a repository run.
    let o = ham(
        &h,
        &[
            "continue",
            &first,
            "zip only",
            "--model",
            MODEL,
            "--s3-only",
            "--json",
        ],
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}\n{}", out(&o), err(&o));
    let start: Value = serde_json::from_str(out(&o).lines().next().unwrap()).unwrap();
    assert_eq!(
        start["target"],
        serde_json::json!({"mode": "s3-only"}),
        "{start}"
    );
    assert_eq!(start["targetSource"], "flag", "{start}");
    assert_eq!(body_of(&fake, 1)["target"]["mode"], "s3-only");
    // The same repository, named.
    let o = ham(
        &h,
        &[
            "continue", &first, "named", "--model", MODEL, "--repo", "cc:calc", "--json",
        ],
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}\n{}", out(&o), err(&o));
    assert_eq!(body_of(&fake, 2)["target"]["repo"], "cc:calc");
    assert_eq!(
        run_lookups(&fake, &first),
        before,
        "an explicit target does not read the run first"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_explicit_repository_other_than_the_runs_is_refused_with_the_services_words() {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "continue-other-repo");
    assert_eq!(ham(&h, &["rules", "init"]).await.status.code(), Some(0));
    let first = finished_repo_run(&fake, &h).await;
    let o = ham(
        &h,
        &[
            "continue",
            &first,
            "elsewhere",
            "--model",
            MODEL,
            "--repo",
            "cc:other",
        ],
    )
    .await;
    assert_eq!(o.status.code(), Some(1), "{}\n{}", out(&o), err(&o));
    assert!(
        err(&o).contains("cc:calc") && err(&o).contains("cc:other"),
        "{}",
        err(&o)
    );
    assert_eq!(
        fake.mem.posts("/v1/direct/runs").len(),
        2,
        "the start was posted and refused; no run came of it"
    );
}

#[allow(dead_code)]
fn unused(_: &Path) {}
