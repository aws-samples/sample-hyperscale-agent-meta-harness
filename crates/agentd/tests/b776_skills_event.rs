//! A run may carry 32 skills (it was 8). The `skills` event is an inline event (16 KiB since the output lane); the 32 skills with a digest
//! each fit it whole. A list that does not fit lists as many as fit and names the total beside them; the card on the run screen counts the total.
#![allow(clippy::unwrap_used)]

use agentd::direct::skills::place::{PlacedFile, PlacedSkill};
use agentd::direct::skills::skills_event;
use harness_protocol::direct::MAX_SKILLS;
use harness_protocol::envelope::MAX_INLINE_PAYLOAD_BYTES;
use std::path::PathBuf;

fn placed(i: usize, name_len: usize) -> PlacedSkill {
    PlacedSkill {
        name: format!("{i:02}{}", "s".repeat(name_len - 2)),
        dir: PathBuf::new(),
        skill_md: PathBuf::new(),
        body: String::new(),
        digest: "a".repeat(64),
        files: (0..12)
            .map(|j| PlacedFile {
                path: format!("references/r{j}.md"),
                sha256: "b".repeat(64),
                bytes: 1_000_000,
            })
            .collect(),
    }
}

fn size(v: &serde_json::Value) -> usize {
    serde_json::to_vec(v).unwrap().len()
}

#[test]
fn b776_the_event_of_the_most_skills_a_run_carries_fits_an_inline_event_and_names_the_total() {
    let all: Vec<PlacedSkill> = (0..MAX_SKILLS).map(|i| placed(i, 64)).collect();
    let ev = skills_event(&all, Some(40));
    assert!(size(&ev) <= MAX_INLINE_PAYLOAD_BYTES, "{} bytes", size(&ev));
    assert_eq!(ev["total"], MAX_SKILLS);
    let listed = ev["skills"].as_array().unwrap().len();
    assert_eq!(
        listed, MAX_SKILLS,
        "the most skills a run carries are listed whole"
    );
    assert_eq!(ev["skills"][0]["name"], all[0].name.as_str());
    assert_eq!(ev["preflight"]["ms"], 40);
}

#[test]
fn b776_a_list_too_long_for_an_inline_event_lists_as_many_as_fit_and_names_the_total() {
    let all: Vec<PlacedSkill> = (0..400).map(|i| placed(i, 64)).collect();
    let ev = skills_event(&all, Some(40));
    assert!(size(&ev) <= MAX_INLINE_PAYLOAD_BYTES, "{} bytes", size(&ev));
    assert_eq!(ev["total"], 400);
    let listed = ev["skills"].as_array().unwrap().len();
    assert!(listed > 0 && listed < 400, "{listed} listed");
    assert_eq!(ev["skills"][0]["name"], all[0].name.as_str());
}

#[test]
fn b776_a_list_that_fits_is_listed_whole() {
    let few: Vec<PlacedSkill> = (0..8).map(|i| placed(i, 20)).collect();
    let ev = skills_event(&few, None);
    assert_eq!(ev["skills"].as_array().unwrap().len(), 8);
    assert_eq!(ev["total"], 8);
    assert!(size(&ev) <= MAX_INLINE_PAYLOAD_BYTES);
}
