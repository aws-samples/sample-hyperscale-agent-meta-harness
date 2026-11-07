//! The purposes of the utility model: a second helper function, the run tags, has its own prompt and its own smoke
//! record, so a row of `assets/utility-models.json` may carry a `purposes` object beside its top-level record, and a setting is resolved
//! for one purpose. A purpose without a passing record is never served by another purpose's record and never by another row.
//!
//! The catalog tests use fixture strings; the last tests read the committed file and the record it cites.

#![allow(clippy::unwrap_used)]

mod runname_common;

use ham_core::runtag::judge::{MAX_TOKENS, PROMPT_VERSION, STOP, request, system_text, user_text};
use ham_core::runtag::vocab::Vocabulary;
use ham_core::utility::{
    UtilityCatalogError, UtilityChoice, UtilityError, UtilityModels, UtilityPurpose,
    bedrock_rejects, excerpt,
};
use runname_common::{CATALOG, asset_path};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::Duration;

const NAME_FILE: &str = "smoke-utility-fixture.json";
const TAGS_FILE: &str = "smoke-utility-tags-fixture.json";
const NAME_ID: &str = "s6:fixture/smoke-utility-fixture.json";
const TAGS_ID: &str = "s6:us.anthropic.claude-haiku-5-5:us-east-1:converse+u2:2026-10-21/smoke-utility-tags-fixture.json";
const NAME_AT: &str = "2026-10-20T07:00:00Z";
const TAGS_AT: &str = "2026-10-21T17:00:00Z";

fn tags_record() -> Value {
    json!({"smokeRecordId": TAGS_ID, "smokedAt": TAGS_AT})
}

/// The fixture catalog with `edit` applied to its JSON.
fn catalog_with(edit: impl FnOnce(&mut Value)) -> String {
    let mut v: Value = serde_json::from_str(CATALOG).unwrap();
    edit(&mut v);
    v.to_string()
}

/// The default row carries both records.
fn both() -> String {
    catalog_with(|v| v["models"][0]["purposes"] = json!({"run-tags": tags_record()}))
}

fn invalid(text: &str) -> String {
    match UtilityModels::parse(text) {
        Err(UtilityCatalogError::Invalid(m)) | Err(UtilityCatalogError::Parse(m)) => m,
        Ok(_) => panic!("the catalog should have been refused: {text}"),
    }
}

/// A second row that has no record of its own for the name purpose, only for the tags.
fn tags_only_row() -> Value {
    json!({
        "id": "us.anthropic.claude-fixture-1",
        "name": "Fixture Model",
        "vendor": "anthropic",
        "alias": "fixture",
        "baseModelId": "anthropic.claude-fixture-1",
        "protocol": "converse",
        "region": "us-east-1",
        "purposes": {"run-tags": {"smokeRecordId": "s6:fixture/smoke-utility-tags-fixture-1.json", "smokedAt": TAGS_AT}}
    })
}

/// A second row with a name record and no tags record.
fn name_only_row() -> Value {
    json!({
        "id": "us.anthropic.claude-fixture-2",
        "name": "Fixture Model Two",
        "vendor": "anthropic",
        "alias": "fixture2",
        "baseModelId": "anthropic.claude-fixture-2",
        "protocol": "converse",
        "region": "us-east-1",
        "smokeRecordId": "s6:fixture/smoke-utility-fixture-2.json",
        "smokedAt": NAME_AT
    })
}

#[test]
fn row_without_purposes_serves_run_name_only() {
    let c = UtilityModels::parse(CATALOG).unwrap();
    let d = c.default_model();
    assert!(d.selectable());
    assert!(d.selectable_for(UtilityPurpose::RunName));
    assert!(!d.selectable_for(UtilityPurpose::RunTags));
    assert_eq!(
        c.resolve_for(UtilityPurpose::RunName, None),
        Ok(UtilityChoice::Model(d))
    );
    let e = c.resolve_for(UtilityPurpose::RunTags, None).unwrap_err();
    assert!(
        matches!(
            e,
            UtilityError::NotSmoked {
                purpose: UtilityPurpose::RunTags,
                ..
            }
        ),
        "{e:?}"
    );
    let text = e.to_string();
    assert!(text.contains("smoke record"), "{text}");
    assert!(text.contains("run-tags"), "the purpose is named: {text}");
    // The old entry point keeps its meaning: the name purpose.
    assert_eq!(
        c.resolve(None),
        c.resolve_for(UtilityPurpose::RunName, None)
    );
    for s in [Some("haiku"), Some("off"), Some("nope"), Some("  ")] {
        assert_eq!(
            c.resolve(s),
            c.resolve_for(UtilityPurpose::RunName, s),
            "{s:?}"
        );
    }
    assert!(c.choices_for(UtilityPurpose::RunTags).is_empty());
    assert_eq!(c.choices(), c.choices_for(UtilityPurpose::RunName));
}

#[test]
fn purposes_object_is_parsed_and_each_record_is_required() {
    let c = UtilityModels::parse(&both()).unwrap();
    let d = c.default_model();
    assert!(d.selectable_for(UtilityPurpose::RunName));
    assert!(d.selectable_for(UtilityPurpose::RunTags));
    assert_eq!(d.record_id_for(UtilityPurpose::RunName), Some(NAME_ID));
    assert_eq!(d.record_id_for(UtilityPurpose::RunTags), Some(TAGS_ID));
    assert_eq!(d.smoked_at_for(UtilityPurpose::RunTags), Some(TAGS_AT));
    for p in UtilityPurpose::ALL {
        assert_eq!(c.resolve_for(p, None), Ok(UtilityChoice::Model(d)), "{p:?}");
        assert_eq!(
            c.resolve_for(p, Some("HAIKU")),
            Ok(UtilityChoice::Model(d)),
            "{p:?}"
        );
        assert_eq!(c.resolve_for(p, Some("off")), Ok(UtilityChoice::Disabled));
    }

    // The name record may be repeated inside `purposes` when it is the same record; the infra reader sees only the top level.
    let same = catalog_with(|v| {
        v["models"][0]["purposes"] = json!({
            "run-name": {"smokeRecordId": NAME_ID, "smokedAt": NAME_AT},
            "run-tags": tags_record()
        });
    });
    let c = UtilityModels::parse(&same).unwrap();
    assert!(c.default_model().selectable_for(UtilityPurpose::RunTags));
    assert_eq!(
        c.default_model().record_id_for(UtilityPurpose::RunName),
        Some(NAME_ID)
    );

    // A record is a pair: both members, non-blank.
    for (label, record, needle) in [
        ("no smokedAt", json!({"smokeRecordId": TAGS_ID}), "smokedAt"),
        (
            "no smokeRecordId",
            json!({"smokedAt": TAGS_AT}),
            "smokeRecordId",
        ),
        ("empty", json!({}), "smoke"),
        (
            "blank id",
            json!({"smokeRecordId": "  ", "smokedAt": TAGS_AT}),
            "smoke record",
        ),
        (
            "blank time",
            json!({"smokeRecordId": TAGS_ID, "smokedAt": " "}),
            "smokedAt",
        ),
        (
            "extra member",
            json!({"smokeRecordId": TAGS_ID, "smokedAt": TAGS_AT, "gate": "pass"}),
            "gate",
        ),
        (
            "id without a plain .json file name",
            json!({"smokeRecordId": "s6:x/not-a-file", "smokedAt": TAGS_AT}),
            ".json",
        ),
    ] {
        let text = catalog_with(|v| v["models"][0]["purposes"] = json!({"run-tags": record}));
        let m = invalid(&text);
        assert!(m.contains(needle), "{label}: {m}");
    }

    // An unknown purpose key and a repeated one are refused: the parser keeps deny_unknown_fields.
    let text = catalog_with(|v| {
        v["models"][0]["purposes"] =
            json!({"run-tags": tags_record(), "run-summary": tags_record()});
    });
    let m = invalid(&text);
    assert!(m.contains("run-summary"), "{m}");
    let doubled = both().replacen(
        "\"run-tags\":",
        "\"run-tags\":{\"smokeRecordId\":\"x.json\",\"smokedAt\":\"t\"},\"run-tags\":",
        1,
    );
    let m = invalid(&doubled);
    assert!(m.contains("duplicate") || m.contains("run-tags"), "{m}");
    let text = catalog_with(|v| v["models"][0]["purposes"] = json!([]));
    assert!(matches!(
        UtilityModels::parse(&text),
        Err(UtilityCatalogError::Parse(_))
    ));

    // `run-name` inside `purposes` must repeat the top-level record exactly, and needs one.
    let differs = catalog_with(|v| {
        v["models"][0]["purposes"] =
            json!({"run-name": {"smokeRecordId": "s6:other/x.json", "smokedAt": NAME_AT}});
    });
    let m = invalid(&differs);
    assert!(m.contains("run-name") && m.contains("top-level"), "{m}");
    let later = catalog_with(|v| {
        v["models"][0]["purposes"] =
            json!({"run-name": {"smokeRecordId": NAME_ID, "smokedAt": "2026-10-21T00:00:00Z"}});
    });
    let m = invalid(&later);
    assert!(m.contains("run-name") && m.contains("top-level"), "{m}");
    let alone = catalog_with(|v| {
        let row = v["models"][0].as_object_mut().unwrap();
        row.remove("smokeRecordId");
        row.remove("smokedAt");
        row.insert(
            "purposes".into(),
            json!({"run-name": {"smokeRecordId": NAME_ID, "smokedAt": NAME_AT}}),
        );
    });
    let m = invalid(&alone);
    assert!(m.contains("run-name") && m.contains("top-level"), "{m}");
}

#[test]
fn a_record_that_names_a_missing_file_fails_the_checked_parse() {
    // The admission parser asks for the text of the file beside the catalog that each record id names.
    let name_json = json!({
        "profileId": "us.anthropic.claude-haiku-5-5", "region": "us-east-1", "result": "pass",
        "smokeRecordId": NAME_ID, "startedAt": NAME_AT
    })
    .to_string();
    let tags_json = json!({
        "profileId": "us.anthropic.claude-haiku-5-5", "region": "us-east-1", "result": "pass",
        "smokeRecordId": TAGS_ID, "startedAt": TAGS_AT, "purposeKey": "run-tags"
    })
    .to_string();
    let both_files = |file: &str| match file {
        NAME_FILE => Some(name_json.clone()),
        TAGS_FILE => Some(tags_json.clone()),
        _ => None,
    };
    let only_name = |file: &str| (file == NAME_FILE).then(|| name_json.clone());
    let nothing = |_: &str| None;

    let c = UtilityModels::parse_checked(&both(), &both_files).unwrap();
    assert!(c.default_model().selectable_for(UtilityPurpose::RunTags));

    let e = UtilityModels::parse_checked(&both(), &only_name).unwrap_err();
    let text = e.to_string();
    assert!(
        text.contains(TAGS_FILE),
        "the missing file is named: {text}"
    );
    assert!(text.contains("run-tags"), "{text}");

    // The top-level record is held to the same rule.
    let e = UtilityModels::parse_checked(CATALOG, &nothing).unwrap_err();
    assert!(e.to_string().contains(NAME_FILE), "{e}");

    // The pure parser does not look at files (fixture catalogs name files that do not exist).
    assert!(UtilityModels::parse(&both()).is_ok());
}

#[test]
fn the_record_of_an_alternate_route_is_looked_for() {
    // An alternate route has a record of its own; the admission parser asks for its file like the others.
    let global_file = "smoke-utility-global.json";
    let with_alt = |id: &str| {
        catalog_with(|v| {
            v["models"][0]["alternateRoutes"] = json!([{
                "id": "global.anthropic.claude-haiku-5-5",
                "region": "us-east-1",
                "smokeRecordId": id
            }]);
        })
    };
    let name_json = json!({
        "profileId": "us.anthropic.claude-haiku-5-5", "region": "us-east-1", "result": "pass",
        "smokeRecordId": NAME_ID, "startedAt": NAME_AT
    })
    .to_string();
    let without = |file: &str| (file == NAME_FILE).then(|| name_json.clone());
    let with = |file: &str| match file {
        NAME_FILE => Some(name_json.clone()),
        "smoke-utility-global.json" => Some("{}".to_string()),
        _ => None,
    };
    let text = with_alt("s6:global/smoke-utility-global.json");
    let e = UtilityModels::parse_checked(&text, &without)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains(global_file) && e.contains("alternate route"),
        "{e}"
    );
    assert!(UtilityModels::parse_checked(&text, &with).is_ok());
    let e = UtilityModels::parse_checked(&with_alt("s6:global/not-a-file"), &with)
        .unwrap_err()
        .to_string();
    assert!(e.contains(".json") && e.contains("alternate route"), "{e}");
}

#[test]
fn a_record_must_agree_with_the_row_it_admits() {
    let good_name = json!({
        "profileId": "us.anthropic.claude-haiku-5-5", "region": "us-east-1", "result": "pass",
        "smokeRecordId": NAME_ID, "startedAt": NAME_AT
    });
    let good_tags = json!({
        "profileId": "us.anthropic.claude-haiku-5-5", "region": "us-east-1", "result": "pass",
        "smokeRecordId": TAGS_ID, "startedAt": TAGS_AT, "purposeKey": "run-tags"
    });
    let parse = |name: &Value, tags: &Value| {
        let (n, t) = (name.to_string(), tags.to_string());
        UtilityModels::parse_checked(&both(), &move |file: &str| match file {
            NAME_FILE => Some(n.clone()),
            TAGS_FILE => Some(t.clone()),
            _ => None,
        })
    };
    parse(&good_name, &good_tags).unwrap();
    let with = |base: &Value, key: &str, value: Value| {
        let mut v = base.clone();
        v[key] = value;
        v
    };
    for (label, tags, needle) in [
        (
            "failed run",
            with(&good_tags, "result", json!("fail")),
            "result",
        ),
        (
            "another route",
            with(
                &good_tags,
                "profileId",
                json!("us.anthropic.claude-other-1"),
            ),
            "profileId",
        ),
        (
            "another Region",
            with(&good_tags, "region", json!("us-west-2")),
            "region",
        ),
        (
            "another id",
            with(&good_tags, "smokeRecordId", json!("s6:other")),
            "smokeRecordId",
        ),
        (
            "another time",
            with(&good_tags, "startedAt", json!("2026-10-21T18:00:00Z")),
            "startedAt",
        ),
        (
            "another purpose",
            with(&good_tags, "purposeKey", json!("run-name")),
            "purposeKey",
        ),
        (
            "no purpose",
            with(&good_tags, "purposeKey", Value::Null),
            "purposeKey",
        ),
    ] {
        let e = parse(&good_name, &tags).unwrap_err().to_string();
        assert!(e.contains(needle), "{label}: {e}");
        assert!(e.contains("run-tags"), "{label}: {e}");
    }
    let e = parse(&with(&good_name, "result", json!("fail")), &good_tags)
        .unwrap_err()
        .to_string();
    assert!(e.contains("result") && e.contains("run-name"), "{e}");
    // A file that is not a JSON object is refused as well.
    let e = UtilityModels::parse_checked(&both(), &|file: &str| {
        (file == NAME_FILE || file == TAGS_FILE).then(|| "[]".to_string())
    })
    .unwrap_err()
    .to_string();
    assert!(e.contains("JSON object"), "{e}");
}

#[test]
fn a_purpose_without_a_record_never_falls_through() {
    // Row 0 (default `haiku`) has both records; `fixture` has only a tags record; `fixture2` has only a name record.
    let text = catalog_with(|v| {
        v["models"][0]["purposes"] = json!({"run-tags": tags_record()});
        let rows = v["models"].as_array_mut().unwrap();
        rows.push(tags_only_row());
        rows.push(name_only_row());
    });
    let c = UtilityModels::parse(&text).unwrap();
    let default = c.default_model();

    // A row that has the name record only: tags never use it, whatever the setting says.
    let e = c
        .resolve_for(UtilityPurpose::RunTags, Some("fixture2"))
        .unwrap_err();
    assert!(
        matches!(
            e,
            UtilityError::NotSmoked {
                purpose: UtilityPurpose::RunTags,
                ..
            }
        ),
        "{e:?}"
    );
    assert!(e.to_string().contains("HAMH_UTILITY_MODEL"), "{e}");
    assert!(
        c.resolve_for(UtilityPurpose::RunName, Some("fixture2"))
            .is_ok()
    );

    // A row that has the tags record only: names never use it.
    let e = c
        .resolve_for(
            UtilityPurpose::RunName,
            Some("us.anthropic.claude-fixture-1"),
        )
        .unwrap_err();
    assert!(
        matches!(
            e,
            UtilityError::NotSmoked {
                purpose: UtilityPurpose::RunName,
                ..
            }
        ),
        "{e:?}"
    );
    assert!(matches!(
        c.resolve_for(UtilityPurpose::RunTags, Some("fixture")),
        Ok(UtilityChoice::Model(m)) if m.alias == "fixture"
    ));
    assert!(
        !c.models()[1].selectable(),
        "selectable() is the name purpose"
    );

    // An unknown setting lists the aliases that serve the purpose, and only those.
    let choices = |purpose: UtilityPurpose| match c.resolve_for(purpose, Some("nope")) {
        Err(UtilityError::Unknown { choices, .. }) => choices,
        other => panic!("{other:?}"),
    };
    assert_eq!(choices(UtilityPurpose::RunName), ["haiku", "fixture2"]);
    assert_eq!(choices(UtilityPurpose::RunTags), ["haiku", "fixture"]);
    assert_eq!(
        c.choices_for(UtilityPurpose::RunTags),
        [
            "haiku = us.anthropic.claude-haiku-5-5",
            "fixture = us.anthropic.claude-fixture-1"
        ]
    );

    // The origin name is kept for a Lambda setting.
    let e = c
        .resolve_for_named(UtilityPurpose::RunTags, Some("fixture2"), "UTILITY_MODEL")
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("UTILITY_MODEL") && !e.contains("HAMH_UTILITY_MODEL"),
        "{e}"
    );

    // The default row stays the default for both purposes.
    assert_eq!(
        c.resolve_for(UtilityPurpose::RunTags, None),
        Ok(UtilityChoice::Model(default))
    );
    assert_eq!(
        c.resolve(None),
        c.resolve_for(UtilityPurpose::RunName, None)
    );
}

#[test]
fn an_unknown_setting_with_no_row_for_the_purpose_still_says_how_to_turn_it_off() {
    let c = UtilityModels::parse(CATALOG).unwrap();
    let e = c
        .resolve_for(UtilityPurpose::RunTags, Some("nope"))
        .unwrap_err();
    assert!(matches!(e, UtilityError::Unknown { .. }), "{e:?}");
    let text = e.to_string();
    assert!(text.contains("nope") && text.contains("off"), "{text}");
    assert!(
        !text.contains(": , or off"),
        "no empty list of choices: {text}"
    );
}

#[test]
fn run_tags_caps_match_the_tag_module_and_the_worst_request_fits() {
    let p = UtilityPurpose::RunTags;
    assert_eq!(p.as_str(), "run-tags");
    assert_eq!(UtilityPurpose::from_word("run-tags"), Some(p));
    assert_eq!(
        UtilityPurpose::from_word("run-name"),
        Some(UtilityPurpose::RunName)
    );
    assert_eq!(UtilityPurpose::from_word("Run-Tags"), None);
    assert_eq!(UtilityPurpose::from_word("run-summary"), None);
    assert_eq!(UtilityPurpose::ALL, [UtilityPurpose::RunName, p]);
    assert_eq!(p.max_output_tokens(), MAX_TOKENS);
    assert_eq!(p.attempts(), 2);
    assert_eq!(p.attempt_timeout(), Duration::from_secs(6));
    assert_eq!(
        p.op_deadline(),
        Duration::from_secs(20),
        "the tag branch's own budget, inside the 30 s of the operation"
    );

    // The worst user turn: a full 40-line vocabulary and the longest escaped excerpt (every character becomes `&amp;`).
    let word = |i: usize| -> String {
        let a = |n: usize| char::from(b'a' + (n % 26) as u8);
        format!("{}{}{}", a(i / 676), a(i / 26), a(i))
    };
    let block: Vec<String> = (0..24)
        .map(|i| {
            format!(
                "topic:{}{}-{} (200)",
                word(i),
                "b".repeat(13),
                "c".repeat(15)
            )
        })
        .chain((0..16).map(|i| {
            format!(
                "tech:{}{}-{} (200)",
                word(100 + i),
                "d".repeat(13),
                "e".repeat(15)
            )
        }))
        .collect();
    let vocab = Vocabulary::from_block(&block.join("\n"));
    assert_eq!(vocab.lines.len(), 40, "the worst vocabulary has 40 lines");
    for prompt in ["&".repeat(5000), "<".repeat(5000), "word ".repeat(2000)] {
        let ex = excerpt(&prompt);
        let user = user_text(&vocab, &ex.escaped).unwrap();
        assert!(
            user.chars().count() <= p.max_input_chars(),
            "{} characters against a cap of {}",
            user.chars().count(),
            p.max_input_chars()
        );
        let body = request(
            &json!({"thinking": {"type": "disabled"}}),
            &vocab,
            &ex.escaped,
        )
        .unwrap();
        assert_eq!(bedrock_rejects(&body), None);
    }
}

#[test]
fn built_in_row_serves_both_purposes() {
    let c = UtilityModels::builtin();
    let row = c.default_model();
    assert!(row.selectable_for(UtilityPurpose::RunName));
    assert!(row.selectable_for(UtilityPurpose::RunTags));
    for p in UtilityPurpose::ALL {
        assert!(row.selectable_for(p), "{p:?}");
        assert_eq!(
            c.resolve_for(p, None),
            Ok(UtilityChoice::Model(row)),
            "{p:?}"
        );
        assert_eq!(
            c.choices_for(p),
            vec![format!("haiku = {}", row.id)],
            "{p:?}"
        );
    }
    assert_eq!(
        c.resolve(None),
        c.resolve_for(UtilityPurpose::RunName, None)
    );
    let tags = row.record_id_for(UtilityPurpose::RunTags).unwrap();
    assert!(
        tags.starts_with("s6:us.anthropic.claude-haiku-5-5:us-east-1:converse+u2:"),
        "{tags}"
    );
    assert!(
        tags.ends_with("/smoke-utility-tags-us.anthropic.claude-haiku-5-5.json"),
        "{tags}"
    );
    assert!(
        row.smoked_at_for(UtilityPurpose::RunTags)
            .is_some_and(|t| t.ends_with('Z'))
    );
    // The name record is the one it was: the row's top-level pair.
    assert_eq!(
        row.record_id_for(UtilityPurpose::RunName),
        row.smoke_record_id.as_deref()
    );

    // deny_unknown_fields still holds on the committed file, at the row and at the `purposes` object.
    let text = std::fs::read_to_string(asset_path("utility-models.json")).unwrap();
    let with = |edit: &dyn Fn(&mut Value)| {
        let mut v: Value = serde_json::from_str(&text).unwrap();
        edit(&mut v);
        v.to_string()
    };
    let e = UtilityModels::parse(&with(&|v| {
        v["models"][0]["purposes"]["run-summary"] = json!({})
    }));
    assert!(matches!(e, Err(UtilityCatalogError::Parse(_))), "{e:?}");
    let e = UtilityModels::parse(&with(&|v| v["models"][0]["surprise"] = json!(1)));
    assert!(matches!(e, Err(UtilityCatalogError::Parse(_))), "{e:?}");
    let e = UtilityModels::parse(&with(&|v| {
        v["models"][0]["purposes"]["run-tags"]["note"] = json!("x")
    }));
    assert!(matches!(e, Err(UtilityCatalogError::Parse(_))), "{e:?}");
}

#[test]
fn built_in_tags_record_matches_the_code_it_admits() {
    // A record admits the prompt, the request shape and the model it measured. If any of them changes, the record is stale and this
    // test fails until the probe has run again.
    let c = UtilityModels::builtin();
    let row = c.default_model();
    let id = row.record_id_for(UtilityPurpose::RunTags).unwrap();
    let file = id.rsplit('/').next().unwrap();
    let record: Value =
        serde_json::from_str(&std::fs::read_to_string(asset_path(file)).unwrap()).unwrap();
    assert_eq!(record["purposeKey"], "run-tags");
    assert_eq!(record["result"], "pass");
    assert_eq!(record["smokeRecordId"], id);
    assert_eq!(record["profileId"], row.id);
    assert_eq!(record["bedrockBaseModelId"], row.base_model_id);
    assert_eq!(record["region"], row.region);
    assert_eq!(
        record["startedAt"].as_str(),
        row.smoked_at_for(UtilityPurpose::RunTags)
    );
    assert_eq!(record["promptVersion"], PROMPT_VERSION);
    let sha: String = Sha256::digest(system_text().as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(record["systemTextSha256"], sha.as_str());
    let shape = &record["requestShape"];
    assert_eq!(shape["maxTokens"], MAX_TOKENS);
    assert_eq!(shape["stopSequences"], json!([STOP]));
    assert_eq!(
        Some(&shape["additionalModelRequestFields"]),
        row.additional_model_request_fields.as_ref()
    );
    assert_eq!(shape["temperatureAccepted"], false);
    assert_eq!(shape["thinkingDisabledAccepted"], true);
    assert_eq!(shape["bareIdAccepted"], false);
    // The whole run: 38 prompts, two vocabulary conditions, two repetitions.
    assert_eq!(record["u2"]["calls"], 152);
    assert_eq!(record["u2"]["conditions"], json!(["cold", "vocab"]));
    assert_eq!(record["u2"]["vocabularyLines"], 40);
}
