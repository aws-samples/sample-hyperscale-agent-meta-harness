//! The input caps a user meets are gone, and one guard stands in their place: a start request may weigh what the platform takes (6 MiB per
//! synchronous Lambda call, less room for the proxy event). Each test here fails on the old caps (the prompt 256 KiB, a rule file 64 KiB, a skill
//! file 64 KiB, an attachment 256 KiB, `review.md` 8 KiB, an answer 8,000 characters) and holds the new behaviour.

#![allow(clippy::unwrap_used)]

use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{
    Attachment, AttachmentProblem, DirectSummary, MAX_ATTACHMENT_NAME_BYTES,
    MAX_DIRECT_SUMMARY_BYTES, MAX_INPUT_TEXT_BYTES, MAX_REQUEST_BYTES, MAX_RULE_FILES,
    MAX_RULE_NAME_BYTES, MAX_SKILL_NAME_BYTES, MAX_SKILL_PATH_BYTES, MAX_SKILLS,
    PLATFORM_REQUEST_BYTES, REQUEST_ENVELOPE_MARGIN_BYTES, ReviewOptions, ReviewProblem, Skill,
    SkillFile, SkillProblem, check_attachments, check_request, check_request_body, check_review,
    check_skills, event_body_bytes,
};
use harness_protocol::messages::{
    MAX_MESSAGE_TEXT_BYTES, MAX_RUN_MESSAGE_BYTES, SendMessageRequest, validate_send,
};
use serde_json::{Value, json};

fn request(prompt: &str) -> StartDirectRunRequest {
    serde_json::from_value(json!({
        "prompt": prompt, "target": {"mode": "s3-only"}, "model": "us.openai.gpt-6.1-sol"
    }))
    .unwrap()
}

fn body(r: &StartDirectRunRequest) -> Vec<u8> {
    serde_json::to_vec(r).unwrap()
}

fn event_size(r: &StartDirectRunRequest) -> usize {
    event_body_bytes(&body(r))
}

const SKILL_MD: &str = "---\nname: demo\ndescription: A demo skill.\n---\nDo the demo.\n";

fn skill(name: &str) -> Skill {
    Skill {
        name: name.to_string(),
        files: vec![SkillFile::new(
            "SKILL.md",
            &format!("---\nname: {name}\ndescription: A demo skill.\n---\nDo the demo.\n"),
        )],
    }
}

// ---- the one guard -----------------------------------------------------------------------------------------------------------

#[test]
fn b776_the_bound_is_the_lambda_payload_less_a_margin_for_the_event() {
    assert_eq!(PLATFORM_REQUEST_BYTES, 6 * 1024 * 1024);
    assert_eq!(PLATFORM_REQUEST_BYTES, 6_291_456);
    assert_eq!(REQUEST_ENVELOPE_MARGIN_BYTES, 64 * 1024);
    assert_eq!(MAX_REQUEST_BYTES, 6_225_920);
}

#[test]
fn b776_event_bytes_count_quotes_backslashes_and_control_characters() {
    assert_eq!(event_body_bytes(b"abc"), 3);
    // `"` and `\` are escaped in the event, the five short controls cost two bytes and any other control six.
    assert_eq!(event_body_bytes(b"\""), 2);
    assert_eq!(event_body_bytes(b"\\"), 2);
    assert_eq!(event_body_bytes(b"\n\r\t"), 6);
    assert_eq!(event_body_bytes(&[0x08, 0x0c]), 4);
    assert_eq!(event_body_bytes(&[0x01]), 6);
    // DEL and the line and paragraph separators are counted as escaped, in case the gateway writes them that way (they almost never occur).
    assert_eq!(event_body_bytes(&[0x7f]), 6);
    assert_eq!(event_body_bytes("\u{2028}".as_bytes()), 6);
    assert_eq!(event_body_bytes("a\u{2029}b".as_bytes()), 8);
    assert_eq!(event_body_bytes("\u{2027}\u{202a}".as_bytes()), 6);
    // `/` and the HTML-sensitive characters are counted as they are until the live test shows the gateway escapes them.
    assert_eq!(event_body_bytes(b"/<>&'"), 5);
    // A character outside ASCII counts as its UTF-8 bytes (the assumption the live test plan checks).
    assert_eq!(event_body_bytes("\u{e9}".as_bytes()), 2);
    assert_eq!(event_body_bytes("\u{20ac}".as_bytes()), 3);
    assert_eq!(event_body_bytes("\u{1f600}".as_bytes()), 4);
    // The body is JSON, so a quote inside a prompt is `\"` in it and four bytes in the event.
    let r = request("\"");
    assert_eq!(event_size(&r) - event_size(&request("x")), 3);
}

#[test]
fn b776_a_request_at_the_bound_passes_and_one_byte_over_is_refused() {
    let base = event_size(&request(""));
    let fits = request(&"x".repeat(MAX_REQUEST_BYTES - base));
    assert_eq!(event_size(&fits), MAX_REQUEST_BYTES);
    assert_eq!(check_request(&fits), Ok(()));
    let over = request(&"x".repeat(MAX_REQUEST_BYTES - base + 1));
    let e = check_request(&over).unwrap_err();
    assert_eq!((e.bytes, e.max), (MAX_REQUEST_BYTES + 1, MAX_REQUEST_BYTES));
    assert_eq!(e.largest, "the prompt");
}

#[test]
fn b776_a_prompt_over_the_old_256_kib_cap_passes() {
    for n in [262_145, 1024 * 1024, 4 * 1024 * 1024] {
        assert_eq!(check_request(&request(&"x".repeat(n))), Ok(()), "{n} bytes");
    }
}

#[test]
fn b776_quotes_count_twice_in_the_event() {
    // The same number of bytes: plain text fits, quotes (`\"` in the body, `\\\"` in the event) do not.
    let n = MAX_REQUEST_BYTES / 3;
    assert_eq!(check_request(&request(&"x".repeat(n))), Ok(()));
    let quotes = request(&"\"".repeat(n));
    let e = check_request(&quotes).unwrap_err();
    assert!(e.bytes > MAX_REQUEST_BYTES, "{e}");
}

#[test]
fn b776_the_refusal_names_the_bound_and_the_largest_part_and_file() {
    let mut r = request("do it");
    r.attachments = vec![
        Attachment::new("small.txt", &"a".repeat(1024 * 1024)),
        Attachment::new("notes.txt", &"b".repeat(5 * 1024 * 1024)),
    ];
    let e = check_request(&r).unwrap_err();
    assert_eq!(e.largest, "the attachments");
    assert_eq!(e.largest_item.as_ref().unwrap().0, "notes.txt");
    let words = e.to_string();
    for want in [
        "6,225,920 bytes",
        "6 MiB",
        "64 KiB",
        "AWS Lambda",
        "the attachments",
        "notes.txt",
        "bytes over",
    ] {
        assert!(words.contains(want), "{want} missing from: {words}");
    }
    // The message holds names and sizes and never a byte of content.
    assert!(
        !words.contains("aaaa") && !words.contains("bbbb"),
        "{words}"
    );
}

#[test]
fn b776_the_refusal_names_the_largest_rule_file_and_the_prompt() {
    let mut r = request("do it");
    r.rules = vec![harness_protocol::api::v1::RuleFile {
        path: "big-rules.md".into(),
        sha256: "0".repeat(64),
        content: "r".repeat(MAX_REQUEST_BYTES),
    }];
    let e = check_request(&r).unwrap_err();
    assert_eq!(e.largest, "the rules");
    assert_eq!(e.largest_item.as_ref().unwrap().0, "big-rules.md");
    let e = check_request(&request(&"p".repeat(MAX_REQUEST_BYTES))).unwrap_err();
    assert_eq!((e.largest.as_str(), e.largest_item), ("the prompt", None));
}

#[test]
fn b776_a_body_with_spaces_is_counted_as_sent() {
    // The client counts the bytes it sends, so a pretty-printed body of a hand-made client is judged by its own size.
    let r = request("do it");
    let pretty = serde_json::to_vec_pretty(&r).unwrap();
    assert!(pretty.len() > body(&r).len());
    assert_eq!(check_request_body(&pretty, &r), Ok(()));
    let mut padded = body(&r);
    padded.extend(std::iter::repeat_n(b' ', MAX_REQUEST_BYTES));
    assert!(check_request_body(&padded, &r).is_err());
}

// ---- the members have no size of their own -------------------------------------------------------------------------------------

#[test]
fn b776_attachments_pass_whatever_their_number_and_size_and_names_take_255_bytes() {
    let many: Vec<Attachment> = (0..40)
        .map(|i| Attachment::new(&format!("f{i}.txt"), &"a".repeat(300 * 1024)))
        .collect();
    assert_eq!(check_attachments(&many), Ok(()));
    assert_eq!(MAX_ATTACHMENT_NAME_BYTES, 255);
    let longest = format!("{}.txt", "n".repeat(MAX_ATTACHMENT_NAME_BYTES - 4));
    assert_eq!(longest.len(), 255);
    assert_eq!(check_attachments(&[Attachment::new(&longest, "x")]), Ok(()));
    let too_long = format!("n{longest}");
    assert!(check_attachments(&[Attachment::new(&too_long, "x")]).is_err());
}

#[test]
fn b776_a_skill_may_hold_many_large_files_deep_in_folders_and_paths_take_255_bytes() {
    let mut s = skill("big-skill");
    for i in 0..50 {
        s.files.push(SkillFile::new(
            &format!("references/r{i}.md"),
            &"x".repeat(100 * 1024),
        ));
    }
    s.files
        .push(SkillFile::new("a/b/c/d/e/f/g/h/i/j/deep.md", "deep\n"));
    assert_eq!(check_skills(&[s]), Ok(()));
    assert_eq!(MAX_SKILL_PATH_BYTES, 255);
    let part = "p".repeat(100);
    let long_ok = format!("{part}/{part}/{}.md", "q".repeat(255 - 100 - 100 - 2 - 3));
    assert_eq!(long_ok.len(), 255);
    let mut s = skill("path-skill");
    s.files.push(SkillFile::new(&long_ok, "x\n"));
    assert_eq!(check_skills(&[s]), Ok(()));
    let mut s = skill("path-skill");
    s.files.push(SkillFile::new(&format!("{long_ok}x"), "x\n"));
    assert_eq!(
        check_skills(&[s]).unwrap_err().problem,
        SkillProblem::BadPath
    );
    // The name and the description follow the Agent Skills specification and stay as they were.
    assert_eq!(MAX_SKILL_NAME_BYTES, 64);
}

#[test]
fn b776_skills_are_counted_at_the_number_the_summary_can_list() {
    assert_eq!(MAX_SKILLS, 32);
    let ok: Vec<Skill> = (0..MAX_SKILLS).map(|i| skill(&format!("s{i}"))).collect();
    assert_eq!(check_skills(&ok), Ok(()));
    let over: Vec<Skill> = (0..=MAX_SKILLS).map(|i| skill(&format!("s{i}"))).collect();
    assert_eq!(
        check_skills(&over).unwrap_err().problem,
        SkillProblem::TooManySkills {
            count: MAX_SKILLS + 1,
            max: MAX_SKILLS
        }
    );
    let _ = SKILL_MD;
}

#[test]
fn b776_review_text_may_be_any_size() {
    let big = "r".repeat(100 * 1024);
    assert_eq!(check_review(&ReviewOptions::with_text(&big, None)), Ok(()));
}

#[test]
fn b776_an_answer_or_message_takes_256_kib_and_no_character_count() {
    assert_eq!(MAX_INPUT_TEXT_BYTES, 256 * 1024);
    assert_eq!(MAX_MESSAGE_TEXT_BYTES, 256 * 1024);
    let send = |text: String| SendMessageRequest {
        message_id: "01JABCDEFGHJKMNPQRSTVWXYZ0".to_string(),
        text,
        mode: Default::default(),
        confirms: None,
    };
    // 20,000 characters, more than the old 8,000.
    assert_eq!(validate_send(&send("a".repeat(20_000))), Ok(()));
    assert_eq!(
        validate_send(&send("a".repeat(MAX_MESSAGE_TEXT_BYTES))),
        Ok(())
    );
    assert!(validate_send(&send("a".repeat(MAX_MESSAGE_TEXT_BYTES + 1))).is_err());
    // The limit counts bytes: 65,536 four-byte characters are exactly the limit.
    assert_eq!(validate_send(&send("\u{1f600}".repeat(65_536))), Ok(()));
    // One maximum message does not use the run's allowance: the allowance holds at least sixteen.
    assert!(MAX_RUN_MESSAGE_BYTES >= 16 * MAX_MESSAGE_TEXT_BYTES as u64);
}

// ---- what the removed caps used to protect -------------------------------------------------------------------------------------

#[test]
fn b776_rule_files_are_counted_at_the_number_the_summary_can_list() {
    assert_eq!(MAX_RULE_FILES, 100);
    assert_eq!(MAX_RULE_NAME_BYTES, 255);
}

/// `summary.json` lists every rule file and every skill and is held to 64 KiB. The most it can be asked to list (100 rule files and 32 skills with
/// the longest names, the largest sizes and a message already cut to nothing) still validates; this is why those two counts exist.
#[test]
fn b776_a_summary_that_lists_the_most_rules_and_skills_still_fits_its_64_kib() {
    let mut v: Value = json!({
        "schemaVersion": 1, "runId": "01JABCDEFGHJKMNPQRSTVWXYZ0", "agentId": "direct", "turn": 1,
        "outcome": "stopped_setup_failed", "label": "Cline did not start",
        "detail": "request.json sha256 differs", "stopReason": null, "turnOutcome": "failed",
        "clineMessage": null, "error": "request.json sha256 differs",
        "model": {"id": "us.anthropic.claude-opus-5-5", "region": "us-east-1"},
        "engine": {"name": "cline"},
        "rules": [], "rulesPreflight": {"ran": false},
        "request": {"sha256": "9f".repeat(32), "promptBytes": 0},
        "nudge": {"requested": false, "honored": false},
        "usage": null, "toolCalls": 0,
        "timing": {"startedAt": "2026-10-13T18:00:00Z", "endedAt": "2026-10-13T18:00:01Z", "wallS": 1},
        "git": {"committedByHarness": false, "filesChanged": 0},
        "snapshots": {"count": 0},
        "packages": {"enabled": false, "requests": 0, "bytes": 0, "byPrefix": {}, "statuses": {}},
        "faultHint": null,
        "versions": {"agentd": "0.1.0", "protocol": "1.30.0"}
    });
    let rules: Vec<Value> = (0..MAX_RULE_FILES)
        .map(|i| {
            let name = format!("{i:03}{}.md", "n".repeat(MAX_RULE_NAME_BYTES - 6));
            json!({"name": name, "sha256": "ab".repeat(32), "bytes": 4_294_967_295u64})
        })
        .collect();
    let skills: Vec<Value> = (0..MAX_SKILLS)
        .map(|i| {
            let name = format!("{i:02}{}", "s".repeat(MAX_SKILL_NAME_BYTES - 2));
            json!({"name": name, "sha256": "cd".repeat(32), "files": 4_294_967_295u64, "bytes": 4_294_967_295u64})
        })
        .collect();
    v["rules"] = Value::Array(rules);
    v["skills"] = Value::Array(skills);
    let s: DirectSummary = serde_json::from_value(v).unwrap();
    let size = serde_json::to_vec(&s).unwrap().len();
    assert!(
        size <= MAX_DIRECT_SUMMARY_BYTES - 6 * 1024,
        "the full lists weigh {size} bytes; the summary holds {MAX_DIRECT_SUMMARY_BYTES} and needs room for the rest"
    );
    assert_eq!(s.validate(), Ok(()));
}

// ---- the words of a refusal --------------------------------------------------------------------------------------------------------

#[test]
fn b776_every_refusal_of_a_size_names_the_bound_and_the_size() {
    let total = 8 * 1024 * 1024;
    for words in [
        AttachmentProblem::TooLarge {
            bytes: total,
            max: MAX_REQUEST_BYTES,
        }
        .to_string(),
        AttachmentProblem::TooLargeTotal {
            bytes: total,
            max: MAX_REQUEST_BYTES,
        }
        .to_string(),
        SkillProblem::TooLarge {
            bytes: total,
            max: MAX_REQUEST_BYTES,
        }
        .to_string(),
        SkillProblem::TooLargeTotal {
            bytes: total,
            max: MAX_REQUEST_BYTES,
        }
        .to_string(),
    ] {
        assert!(
            words.contains("8388608") && words.contains("6,225,920 bytes"),
            "{words}"
        );
        assert!(
            words.contains("6 MiB") && words.contains("AWS Lambda"),
            "{words}"
        );
    }
}

#[test]
fn b776_a_review_problem_names_its_member() {
    assert_eq!(ReviewProblem::TextEmpty.key(), "text");
    assert_eq!(ReviewProblem::TextNul.key(), "text");
    assert_eq!(ReviewProblem::Sha.key(), "sha256");
    assert_eq!(ReviewProblem::Guard.key(), "guardS");
}

#[test]
fn b776_a_request_with_a_member_of_its_own_is_named_by_that_member() {
    let mut r = request("do it");
    r.model = "m".repeat(MAX_REQUEST_BYTES);
    let e = check_request(&r).unwrap_err();
    assert_eq!(
        (e.largest.as_str(), e.largest_item),
        ("the model setting", None)
    );
}
