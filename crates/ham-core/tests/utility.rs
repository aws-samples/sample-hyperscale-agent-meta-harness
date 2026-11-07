//! The utility model: the catalog parser and its resolution (`HAMH_UTILITY_MODEL`), the Converse request, the reply and the
//! error classes, and the one function every fake of the service uses to refuse what the service refuses.
//! The catalog here is a fixture string.

#![allow(clippy::unwrap_used)]

mod runname_common;

use ham_core::models;
use ham_core::runname::{
    self, MAX_TOKENS, Reason, STOP, judge, plan, redacted_prompt, system_text,
};
use ham_core::utility::{
    UtilityCatalogError, UtilityChoice, UtilityError, UtilityFault, UtilityModels, UtilityPurpose,
    UtilityReply, bedrock_rejects, bedrock_rejects_with, parse_reply, request_body,
};
use runname_common::*;
use serde_json::{Value, json};
use std::time::Duration;

/// The fixture catalog with `edit` applied to its JSON.
fn catalog_with(edit: impl FnOnce(&mut Value)) -> String {
    let mut v: Value = serde_json::from_str(CATALOG).unwrap();
    edit(&mut v);
    v.to_string()
}

fn invalid(text: &str) -> String {
    match UtilityModels::parse(text) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("the catalog should have been refused: {text}"),
    }
}

/// A second row, listed but without a smoke record.
fn with_second_row(v: &mut Value) {
    v["models"].as_array_mut().unwrap().push(json!({
        "id": "us.anthropic.claude-fixture-1",
        "name": "Fixture Model",
        "vendor": "anthropic",
        "alias": "fixture",
        "baseModelId": "anthropic.claude-fixture-1",
        "protocol": "converse",
        "region": "us-east-1"
    }));
}

#[test]
fn parse_reads_the_fixture_catalog() {
    let c = UtilityModels::parse(CATALOG).unwrap();
    assert_eq!(c.models().len(), 1);
    let d = c.default_model();
    assert_eq!(d.id, "us.anthropic.claude-haiku-5-5");
    assert_eq!(d.alias, "haiku");
    assert_eq!(d.base_model_id, "anthropic.claude-haiku-5-5");
    assert_eq!(d.region, "us-east-1");
    assert!(d.selectable());
    assert_eq!(
        d.additional_model_request_fields,
        Some(json!({"thinking": {"type": "disabled"}}))
    );
    assert!(d.alternate_routes.is_empty());
    assert!(std::ptr::eq(c.find("HAIKU").unwrap(), d));
    assert!(std::ptr::eq(
        c.find(" us.anthropic.claude-haiku-5-5 ").unwrap(),
        d
    ));
    assert!(c.find("sonnet").is_none());
}

#[test]
fn parse_refuses_a_default_without_a_smoke_record() {
    let text = catalog_with(|v| {
        v["models"][0]
            .as_object_mut()
            .unwrap()
            .remove("smokeRecordId");
    });
    assert!(
        invalid(&text).contains("smoke record"),
        "{}",
        invalid(&text)
    );
    let text = catalog_with(|v| v["models"][0]["smokeRecordId"] = json!("   "));
    assert!(invalid(&text).contains("smoke record"));
    // A default that names a row without a record.
    let text = catalog_with(|v| {
        with_second_row(v);
        v["default"] = json!("fixture");
    });
    assert!(invalid(&text).contains("smoke record"));
}

#[test]
fn parse_refuses_two_rows_with_one_alias() {
    let text = catalog_with(|v| {
        with_second_row(v);
        v["models"][1]["alias"] = json!("haiku");
    });
    assert!(invalid(&text).contains("alias"), "{}", invalid(&text));
    // An alias that is another row's id.
    let text = catalog_with(|v| {
        with_second_row(v);
        v["models"][1]["alias"] = json!("us.anthropic.claude-haiku-5-5");
    });
    assert!(
        invalid(&text).contains("alias") || invalid(&text).contains("lower-case"),
        "{}",
        invalid(&text)
    );
    // The same id twice.
    let text = catalog_with(|v| {
        with_second_row(v);
        v["models"][1]["id"] = json!("us.anthropic.claude-haiku-5-5");
    });
    assert!(invalid(&text).contains("twice"), "{}", invalid(&text));
}

#[test]
fn parse_refuses_an_alternate_of_another_base_model() {
    let alt = |id: &str, smoke: Option<&str>| {
        let mut a = json!({"id": id, "region": "us-east-1"});
        if let Some(s) = smoke {
            a["smokeRecordId"] = json!(s);
        }
        a
    };
    // Another model's profile.
    let text = catalog_with(|v| {
        v["models"][0]["alternateRoutes"] =
            json!([alt("global.anthropic.claude-other-1", Some("s6:x"))])
    });
    assert!(
        invalid(&text).contains("another model"),
        "{}",
        invalid(&text)
    );
    // The same model's other profile, with a record: accepted.
    let text = catalog_with(|v| {
        v["models"][0]["alternateRoutes"] =
            json!([alt("global.anthropic.claude-haiku-5-5", Some("s6:x"))])
    });
    let c = UtilityModels::parse(&text).unwrap();
    assert_eq!(
        c.default_model().alternate_routes[0].id,
        "global.anthropic.claude-haiku-5-5"
    );
    // Without a record the route is never offered, so the file is refused.
    let text = catalog_with(|v| {
        v["models"][0]["alternateRoutes"] = json!([alt("global.anthropic.claude-haiku-5-5", None)])
    });
    assert!(invalid(&text).contains("smoke"), "{}", invalid(&text));
    let text = catalog_with(|v| {
        v["models"][0]["alternateRoutes"] =
            json!([alt("global.anthropic.claude-haiku-5-5", Some(" "))])
    });
    assert!(
        invalid(&text).contains("smoke record"),
        "{}",
        invalid(&text)
    );
    // A route that repeats the row's own id.
    let text = catalog_with(|v| {
        v["models"][0]["alternateRoutes"] =
            json!([alt("us.anthropic.claude-haiku-5-5", Some("s6:x"))])
    });
    assert!(invalid(&text).contains("repeats"), "{}", invalid(&text));
}

#[test]
fn parse_refuses_an_unknown_field() {
    let text = catalog_with(|v| v["models"][0]["contextTokens"] = json!(1_000_000));
    assert!(matches!(
        UtilityModels::parse(&text),
        Err(UtilityCatalogError::Parse(_))
    ));
    let text = catalog_with(|v| v["surprise"] = json!(true));
    assert!(matches!(
        UtilityModels::parse(&text),
        Err(UtilityCatalogError::Parse(_))
    ));
    assert!(matches!(
        UtilityModels::parse("not json"),
        Err(UtilityCatalogError::Parse(_))
    ));
}

#[test]
fn parse_refuses_a_missing_or_wrong_default() {
    let text = catalog_with(|v| {
        v.as_object_mut().unwrap().remove("default");
    });
    assert!(matches!(
        UtilityModels::parse(&text),
        Err(UtilityCatalogError::Parse(_))
    ));
    let text = catalog_with(|v| v["default"] = json!("nope"));
    assert!(invalid(&text).contains("default"), "{}", invalid(&text));
    let text = catalog_with(|v| v["default"] = json!(""));
    assert!(invalid(&text).contains("default"), "{}", invalid(&text));
    // The default may be named by id as well as by alias.
    let text = catalog_with(|v| v["default"] = json!("us.anthropic.claude-haiku-5-5"));
    assert_eq!(
        UtilityModels::parse(&text).unwrap().default_model().alias,
        "haiku"
    );
}

#[test]
fn parse_refuses_the_ids_aliases_and_routes_of_the_five_models() {
    let five = models::builtin();
    let first = five.models()[0].clone();
    // An id of the coding models.
    let text = catalog_with(|v| v["models"][0]["id"] = json!(first.id));
    assert!(
        invalid(&text).contains("coding model"),
        "{}",
        invalid(&text)
    );
    // An alias of the coding models.
    let alias = first.alias.clone().unwrap();
    let text = catalog_with(|v| v["models"][0]["alias"] = json!(alias));
    assert!(
        invalid(&text).contains("coding model"),
        "{}",
        invalid(&text)
    );
    // An alternate route of a coding model.
    let route = five
        .models()
        .iter()
        .find_map(|m| m.alternate_routes.first())
        .unwrap()
        .id
        .clone();
    let text = catalog_with(|v| v["models"][0]["id"] = json!(route));
    assert!(
        invalid(&text).contains("coding model"),
        "{}",
        invalid(&text)
    );
    // And the other way: no coding model answers to a utility id or alias.
    let c = UtilityModels::parse(CATALOG).unwrap();
    for m in c.models() {
        assert!(
            five.find(&m.id).is_none()
                && five.find(&m.alias).is_none()
                && five.find_route(&m.id).is_none()
        );
    }
}

#[test]
fn parse_refuses_other_protocols_versions_and_blank_ids() {
    let text = catalog_with(|v| v["models"][0]["protocol"] = json!("mantle-responses"));
    assert!(invalid(&text).contains("converse"), "{}", invalid(&text));
    let text = catalog_with(|v| v["schemaVersion"] = json!(2));
    assert!(
        invalid(&text).contains("schemaVersion"),
        "{}",
        invalid(&text)
    );
    let text = catalog_with(|v| v["models"] = json!([]));
    assert!(invalid(&text).contains("no models"), "{}", invalid(&text));
    let text = catalog_with(|v| v["models"][0]["id"] = json!("us.anthropic claude"));
    assert!(invalid(&text).contains("whitespace"), "{}", invalid(&text));
    let text = catalog_with(|v| v["models"][0]["alias"] = json!("Haiku"));
    assert!(invalid(&text).contains("alias"), "{}", invalid(&text));
    let text = catalog_with(|v| v["models"][0]["region"] = json!(" "));
    assert!(invalid(&text).contains("Region"), "{}", invalid(&text));
}

#[test]
fn resolve_unset_blank_and_spaces_give_the_default() {
    let c = UtilityModels::parse(CATALOG).unwrap();
    for setting in [None, Some(""), Some("   "), Some("\t\n")] {
        match c.resolve(setting).unwrap() {
            UtilityChoice::Model(m) => assert_eq!(m.alias, "haiku", "{setting:?}"),
            UtilityChoice::Disabled => panic!("{setting:?} should give the default"),
        }
    }
}

#[test]
fn resolve_finds_an_id_or_an_alias_in_any_case() {
    let c = UtilityModels::parse(CATALOG).unwrap();
    for setting in ["HAIKU", "haiku", " Haiku ", "us.anthropic.claude-haiku-5-5"] {
        match c.resolve(Some(setting)).unwrap() {
            UtilityChoice::Model(m) => {
                assert_eq!(m.id, "us.anthropic.claude-haiku-5-5", "{setting:?}")
            }
            UtilityChoice::Disabled => panic!("{setting:?}"),
        }
    }
}

#[test]
fn resolve_off_is_disabled_in_any_case() {
    let c = UtilityModels::parse(CATALOG).unwrap();
    for setting in ["off", "OFF", "Off", " off "] {
        assert_eq!(
            c.resolve(Some(setting)).unwrap(),
            UtilityChoice::Disabled,
            "{setting:?}"
        );
    }
}

#[test]
fn resolve_unknown_names_its_source_and_never_falls_through() {
    let c = UtilityModels::parse(CATALOG).unwrap();
    for setting in [
        "sonnet",
        "nope",
        "us.anthropic.claude-haiku-5-4",
        "haiku 5.5",
        "of",
    ] {
        let e = c.resolve(Some(setting)).unwrap_err();
        assert!(
            matches!(e, UtilityError::Unknown { .. }),
            "{setting}: {e:?}"
        );
        let text = e.to_string();
        assert!(text.contains("HAMH_UTILITY_MODEL"), "{text}");
        assert!(text.contains(setting), "{text}");
        assert!(
            text.contains("haiku") && text.contains("off"),
            "the choices are listed: {text}"
        );
    }
    // Inside a Lambda the variable has another name.
    let e = c.resolve_named(Some("nope"), "UTILITY_MODEL").unwrap_err();
    assert!(
        e.to_string().contains("UTILITY_MODEL") && !e.to_string().contains("HAMH_UTILITY_MODEL"),
        "{e}"
    );
}

#[test]
fn resolve_a_row_without_a_record_is_not_smoked() {
    let text = catalog_with(with_second_row);
    let c = UtilityModels::parse(&text).unwrap();
    for setting in ["fixture", "us.anthropic.claude-fixture-1"] {
        let e = c.resolve(Some(setting)).unwrap_err();
        assert!(matches!(e, UtilityError::NotSmoked { .. }), "{e:?}");
        assert!(e.to_string().contains("smoke record"), "{e}");
    }
    // The default is still the smoked row.
    assert!(matches!(c.resolve(None).unwrap(), UtilityChoice::Model(m) if m.alias == "haiku"));
}

#[test]
fn a_row_must_run_in_the_stage_region() {
    let c = UtilityModels::parse(CATALOG).unwrap();
    let d = c.default_model();
    assert!(d.check_region("us-east-1").is_ok());
    let e = d.check_region("us-west-2").unwrap_err();
    assert!(matches!(e, UtilityError::Region { .. }));
    assert!(
        e.to_string().contains("us-east-1") && e.to_string().contains("us-west-2"),
        "{e}"
    );
}

#[test]
fn request_body_has_no_sampling_fields() {
    let c = UtilityModels::parse(CATALOG).unwrap();
    let body = request_body(
        c.default_model(),
        "system words",
        "user words",
        64,
        &["</title>"],
    );
    assert_eq!(body["system"], json!([{"text": "system words"}]));
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": [{"text": "user words"}]}])
    );
    assert_eq!(
        body["inferenceConfig"],
        json!({"maxTokens": 64, "stopSequences": ["</title>"]})
    );
    assert_eq!(
        body["additionalModelRequestFields"],
        json!({"thinking": {"type": "disabled"}})
    );
    let text = body.to_string();
    for banned in ["temperature", "topP", "top_p", "top_k", "topK"] {
        assert!(!text.contains(banned), "{banned}");
    }
    assert_eq!(bedrock_rejects(&body), None);
    // Several stop sequences, and a row with no extra fields: the member is left out, not null.
    let text = catalog_with(|v| {
        v["models"][0]
            .as_object_mut()
            .unwrap()
            .remove("additionalModelRequestFields");
    });
    let c = UtilityModels::parse(&text).unwrap();
    let body = request_body(c.default_model(), "s", "u", 32, &["</a>", "</b>"]);
    assert!(body.get("additionalModelRequestFields").is_none());
    assert_eq!(
        body["inferenceConfig"]["stopSequences"],
        json!(["</a>", "</b>"])
    );
    // No stop sequence at all: the member is left out.
    let body = request_body(c.default_model(), "s", "u", 32, &[]);
    assert!(body["inferenceConfig"].get("stopSequences").is_none());
}

fn golden() -> Value {
    read_json("request.golden.json")
}

type Edit = Box<dyn Fn(&mut Value)>;

fn edit(f: impl Fn(&mut Value) + 'static) -> Edit {
    Box::new(f)
}

fn with_model(mut body: Value, id: &str) -> Value {
    body.as_object_mut()
        .unwrap()
        .insert("modelId".into(), json!(id));
    body
}

#[test]
fn bedrock_rejects_what_bedrock_rejects() {
    // The golden request is accepted, with or without a known model id.
    assert_eq!(bedrock_rejects(&golden()), None);
    assert_eq!(
        bedrock_rejects(&with_model(golden(), "us.anthropic.claude-haiku-5-5")),
        None
    );
    assert_eq!(
        bedrock_rejects(&with_model(golden(), "global.anthropic.claude-haiku-5-5")),
        None
    );

    let with = |edit: &dyn Fn(&mut Value)| {
        let mut b = golden();
        edit(&mut b);
        bedrock_rejects(&b)
    };
    // Sampling fields: refused whatever their value, wherever Converse takes them.
    for (label, edit) in [
        (
            "temperature 0",
            edit(|b| b["inferenceConfig"]["temperature"] = json!(0.0)),
        ),
        (
            "temperature 1",
            edit(|b| b["inferenceConfig"]["temperature"] = json!(1)),
        ),
        ("topP", edit(|b| b["inferenceConfig"]["topP"] = json!(0.5))),
        (
            "topP 0.99",
            edit(|b| b["inferenceConfig"]["topP"] = json!(0.99)),
        ),
        (
            "top_k",
            edit(|b| b["additionalModelRequestFields"]["top_k"] = json!(5)),
        ),
        (
            "top_p in the extra fields",
            edit(|b| b["additionalModelRequestFields"]["top_p"] = json!(0.5)),
        ),
    ] {
        let reason = with(&*edit).unwrap_or_else(|| panic!("{label} should be refused"));
        assert!(reason.contains("deprecated"), "{label}: {reason}");
    }
    assert!(
        with(&|b| b["inferenceConfig"]["temperature"] = json!(0.0))
            .unwrap()
            .contains("temperature")
    );
    assert!(
        with(&|b| b["inferenceConfig"]["topP"] = json!(0.5))
            .unwrap()
            .contains("top_p")
    );
    assert!(
        with(&|b| b["additionalModelRequestFields"]["top_k"] = json!(5))
            .unwrap()
            .contains("top_k")
    );

    // A last assistant turn (prefill).
    let reason = with(&|b| {
        b["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role": "assistant", "content": [{"text": "<title>"}]}));
    })
    .unwrap();
    assert!(reason.contains("prefill"), "{reason}");

    // Thinking off with an effort of xhigh or max; low and a missing thinking member are fine.
    for effort in ["xhigh", "max"] {
        let reason = with(&|b| {
            b["additionalModelRequestFields"]["output_config"] = json!({"effort": effort})
        })
        .unwrap();
        assert!(
            reason.contains(effort) && reason.contains("thinking"),
            "{reason}"
        );
    }
    assert_eq!(
        with(&|b| b["additionalModelRequestFields"]["output_config"] = json!({"effort": "low"})),
        None
    );
    assert_eq!(
        with(&|b| b["additionalModelRequestFields"] = json!({"output_config": {"effort": "max"}})),
        None,
        "with thinking left on, max is a valid effort"
    );

    // No messages at all.
    assert!(with(&|b| b["messages"] = json!([])).is_some());
    assert!(
        with(&|b| {
            b.as_object_mut().unwrap().remove("messages");
        })
        .is_some()
    );

    // The bare model id (on-demand throughput is not supported) and an unknown id.
    let reason = bedrock_rejects(&with_model(golden(), "anthropic.claude-haiku-5-5")).unwrap();
    assert!(reason.contains("on-demand throughput"), "{reason}");
    let reason = bedrock_rejects(&with_model(golden(), "us.anthropic.claude-nope-9")).unwrap();
    assert!(reason.contains("model identifier is invalid"), "{reason}");
    assert!(bedrock_rejects(&with_model(golden(), "")).is_some());
    assert!(bedrock_rejects(&with_model(golden(), "   ")).is_some());
    // The caller can name the ids its catalog holds.
    assert_eq!(
        bedrock_rejects_with(
            &with_model(golden(), "us.anthropic.claude-fixture-1"),
            &["us.anthropic.claude-fixture-1"]
        ),
        None
    );
    assert!(
        bedrock_rejects_with(
            &with_model(golden(), "us.anthropic.claude-haiku-5-5"),
            &["us.anthropic.claude-fixture-1"]
        )
        .is_some()
    );
}

#[test]
fn parse_reply_joins_text_blocks_and_ignores_reasoning() {
    let r = parse_reply(read_fixture("replies/two-text-blocks.json").as_bytes()).unwrap();
    assert_eq!(
        r.text,
        "<title>Add exponential backoff retry to S3 uploader"
    );
    assert_eq!(r.stop_reason, "stop_sequence");
    assert_eq!((r.input_tokens, r.output_tokens), (453, 23));
    assert_eq!(r.latency_ms, Some(902));
    assert_eq!(r.reasoning_blocks, 0);

    let r = parse_reply(read_fixture("replies/reasoning-first.json").as_bytes()).unwrap();
    assert_eq!(
        r.text, "<title>Ratenbegrenzung im Bestell-Service samt Tests",
        "the reasoning text is not the answer"
    );
    assert_eq!(r.reasoning_blocks, 1);
    assert_eq!((r.input_tokens, r.output_tokens), (469, 213));
}

#[test]
fn parse_reply_reads_every_recorded_shape() {
    for (file, stop, text, reasoning) in [
        (
            "ok-no-close-tag",
            "stop_sequence",
            "<title>Exponential backoff retry for S3 uploader",
            0,
        ),
        (
            "reasoning-first",
            "stop_sequence",
            "<title>Ratenbegrenzung im Bestell-Service samt Tests",
            1,
        ),
        ("max-tokens-reasoning-only", "max_tokens", "", 1),
        ("content-filtered", "content_filtered", "", 0),
        (
            "two-text-blocks",
            "stop_sequence",
            "<title>Add exponential backoff retry to S3 uploader",
            0,
        ),
        (
            "untagged-lines",
            "end_turn",
            "Here is a name for the run:\nAdd exponential backoff retry to S3 uploader",
            0,
        ),
    ] {
        let r = parse_reply(read_fixture(&format!("replies/{file}.json")).as_bytes())
            .unwrap_or_else(|e| panic!("{file}: {e:?}"));
        assert_eq!(
            (r.stop_reason.as_str(), r.text.as_str(), r.reasoning_blocks),
            (stop, text, reasoning),
            "{file}"
        );
    }
    // A body with no output at all, and a body that is not a Converse answer.
    assert_eq!(
        parse_reply(read_fixture("replies/no-output.json").as_bytes()).unwrap_err(),
        UtilityFault::NoText
    );
    assert_eq!(parse_reply(b"{}").unwrap_err(), UtilityFault::NoText);
    assert_eq!(
        parse_reply(br#"{"output": {"message": {"role": "assistant"}}, "stopReason": "end_turn"}"#)
            .unwrap_err(),
        UtilityFault::NoText
    );
    assert_eq!(
        parse_reply(b"not json at all").unwrap_err(),
        UtilityFault::Service
    );
    assert_eq!(parse_reply(b"[1, 2]").unwrap_err(), UtilityFault::Service);
    assert_eq!(parse_reply(b"").unwrap_err(), UtilityFault::Service);
}

#[test]
fn recorded_replies_through_the_judge() {
    let judged = |file: &str, prompt: &str| {
        let r = parse_reply(read_fixture(&format!("replies/{file}.json")).as_bytes()).unwrap();
        judge(&r, &redacted_prompt(prompt))
            .map(|j| j.name)
            .map_err(Reason::as_str)
    };
    assert_eq!(
        judged("ok-no-close-tag", &prompt_of("c01")),
        Ok("Exponential backoff retry for S3 uploader".to_string())
    );
    assert_eq!(
        judged("two-text-blocks", &prompt_of("c01")),
        Ok("Add exponential backoff retry to S3 uploader".to_string())
    );
    assert_eq!(
        judged("reasoning-first", &prompt_of("c14")),
        Ok("Ratenbegrenzung im Bestell-Service samt Tests".to_string())
    );
    assert_eq!(
        judged("max-tokens-reasoning-only", &prompt_of("c01")),
        Err("stop_max_tokens")
    );
    assert_eq!(
        judged("content-filtered", &prompt_of("c01")),
        Err("stop_content_filtered")
    );
    assert_eq!(
        judged("untagged-lines", &prompt_of("c01")),
        Err("stop_end_turn")
    );
}

#[test]
fn fault_from_status_reads_the_error_class_before_the_first_colon() {
    let expect = |name: &str| match name {
        "throttle" => UtilityFault::Throttle,
        "timeout" => UtilityFault::Timeout,
        "service" => UtilityFault::Service,
        "config" => UtilityFault::Config,
        other => panic!("{other}"),
    };
    let errors = read_json("replies/errors.json");
    let errors = errors.as_array().unwrap();
    assert_eq!(errors.len(), 9);
    for e in errors {
        let status = e["status"].as_u64().unwrap() as u16;
        let errortype = e["errortype"].as_str().unwrap();
        assert!(
            errortype.contains(':'),
            "the recorded header carries a namespace after a colon"
        );
        assert_eq!(
            UtilityFault::from_http(status, Some(errortype)),
            expect(e["expect"].as_str().unwrap()),
            "{}",
            e["name"]
        );
    }
    // The five statuses of the package, with and without the header.
    for (status, header, fault) in [
        (
            429,
            Some("ThrottlingException:http://internal.amazon.com/coral/com.amazon.bedrock/"),
            UtilityFault::Throttle,
        ),
        (429, None, UtilityFault::Throttle),
        (408, None, UtilityFault::Timeout),
        (408, Some("ModelTimeoutException"), UtilityFault::Timeout),
        (503, None, UtilityFault::Service),
        (
            500,
            Some("InternalServerException:x"),
            UtilityFault::Service,
        ),
        (502, None, UtilityFault::Service),
        (403, None, UtilityFault::Config),
        (400, None, UtilityFault::Config),
        (404, Some("ResourceNotFoundException"), UtilityFault::Config),
        (418, None, UtilityFault::Config),
        // The class wins over the status: Bedrock answers a model that is not ready with 429.
        (
            429,
            Some("ModelNotReadyException:http://internal.amazon.com/coral/com.amazon.bedrock/"),
            UtilityFault::Service,
        ),
        // Lower case, with or without the word "exception".
        (400, Some("throttlingexception"), UtilityFault::Throttle),
        (
            400,
            Some("TooManyRequestsException"),
            UtilityFault::Throttle,
        ),
        (400, Some("Throttling"), UtilityFault::Throttle),
        (200, Some("ValidationException:x"), UtilityFault::Config),
        // A status below 400 that is not a success is a service fault.
        (302, None, UtilityFault::Service),
    ] {
        assert_eq!(
            UtilityFault::from_http(status, header),
            fault,
            "{status} {header:?}"
        );
    }
}

#[test]
fn fault_codes_and_retries() {
    for (fault, code, retry, reason) in [
        (UtilityFault::Timeout, "timeout", true, Reason::Timeout),
        (UtilityFault::Throttle, "throttle", true, Reason::Throttle),
        (
            UtilityFault::Service,
            "service_error",
            true,
            Reason::ServiceError,
        ),
        (
            UtilityFault::Config,
            "config_error",
            false,
            Reason::ConfigError,
        ),
        (UtilityFault::NoText, "no_text", false, Reason::NoText),
    ] {
        assert_eq!(fault.code(), code);
        assert_eq!(fault.retryable(), retry, "{code}");
        assert_eq!(Reason::from(fault), reason);
        assert_eq!(reason.as_str(), code);
    }
}

#[test]
fn utility_purpose_caps_match_the_run_name_module() {
    let p = UtilityPurpose::RunName;
    assert_eq!(p.as_str(), "run-name");
    assert_eq!(p.max_output_tokens(), MAX_TOKENS);
    assert_eq!(p.max_input_chars(), runname::MAX_USER_TEXT_CHARS);
    assert_eq!(p.attempts(), 2);
    assert_eq!(p.attempt_timeout(), Duration::from_secs(6));
    assert_eq!(p.op_deadline(), Duration::from_secs(30));
    // The request the purpose sends respects its own caps.
    let c = UtilityModels::parse(CATALOG).unwrap();
    let worst = plan(&rep('<', 5000)).user_text;
    assert!(worst.chars().count() <= p.max_input_chars());
    let body = request_body(
        c.default_model(),
        system_text(),
        &worst,
        p.max_output_tokens(),
        &[STOP],
    );
    assert_eq!(bedrock_rejects(&body), None);
}

#[test]
fn a_reply_new_is_a_stop_and_a_text() {
    let r = UtilityReply::new("stop_sequence", "<title>x");
    assert_eq!(
        (r.stop_reason.as_str(), r.text.as_str()),
        ("stop_sequence", "<title>x")
    );
    assert_eq!(
        (
            r.input_tokens,
            r.output_tokens,
            r.latency_ms,
            r.reasoning_blocks
        ),
        (0, 0, None, 0)
    );
}
