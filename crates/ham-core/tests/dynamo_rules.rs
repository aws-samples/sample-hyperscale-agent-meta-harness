//! The DynamoDB expressions of the tag writers and the check that a fake rejects what DynamoDB rejects. The three writes and the
//! one query are plain; `expr_rejects` is what the in-memory stores of the other packages call on every request they receive.
#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use ham_core::runtag::dynamo::{
    Answered, Av, QuerySpec, TAG_ATTRIBUTES, UpdateSpec, auto_update, edit_update, expr_rejects,
    inherit_update, is_reserved, vocabulary_query,
};
use std::collections::{BTreeMap, BTreeSet};

fn v(tags: &[&str]) -> Vec<String> {
    tags.iter().map(|t| t.to_string()).collect()
}

fn names(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(a, n)| (a.to_string(), n.to_string()))
        .collect()
}

fn update(
    expression: &str,
    condition: Option<&str>,
    n: &[(&str, &str)],
    values: &[(&str, Av)],
) -> UpdateSpec {
    UpdateSpec {
        key_sk_prefix_note: "SRCH#".to_string(),
        expression: expression.to_string(),
        condition: condition.map(str::to_string),
        names: names(n),
        values: values
            .iter()
            .map(|(k, a)| (k.to_string(), a.clone()))
            .collect(),
        item_bytes: None,
    }
}

fn list(items: &[&str]) -> Av {
    Av::L(v(items))
}

fn rejects(spec: &UpdateSpec) -> String {
    expr_rejects(spec).unwrap_or_else(|| panic!("expected a rejection of {spec:?}"))
}

fn attribute_names(spec: &UpdateSpec) -> BTreeSet<String> {
    spec.names.values().cloned().collect()
}

#[test]
fn expr_rejects_what_dynamodb_rejects() {
    // A valid write passes.
    let ok = update(
        "SET #a = :a",
        None,
        &[("#a", "runTagsUser")],
        &[(":a", list(&["topic:x"]))],
    );
    assert_eq!(expr_rejects(&ok), None);

    // A path set and removed in one expression, set twice, or a parent and its child.
    let both = update(
        "SET #a = :a REMOVE #a",
        None,
        &[("#a", "runTagsUser")],
        &[(":a", list(&[]))],
    );
    assert!(rejects(&both).contains("overlap"), "{}", rejects(&both));
    let twice = update(
        "SET #a = :a, #a = :b",
        None,
        &[("#a", "runTagsUser")],
        &[(":a", list(&[])), (":b", list(&[]))],
    );
    assert!(rejects(&twice).contains("overlap"));
    let nested = update(
        "SET #a = :a, #a.#b = :b",
        None,
        &[("#a", "x"), ("#b", "y")],
        &[(":a", list(&[])), (":b", list(&[]))],
    );
    assert!(rejects(&nested).contains("overlap"));
    // The same attribute read on the right of a SET is fine.
    let read = update(
        "SET #a = if_not_exists(#a, :z) + :one",
        None,
        &[("#a", "runTagsRev")],
        &[(":z", Av::N(0)), (":one", Av::N(1))],
    );
    assert_eq!(expr_rejects(&read), None);
    // A clause keyword twice.
    let sections = update(
        "SET #a = :a SET #b = :b",
        None,
        &[("#a", "x"), ("#b", "y")],
        &[(":a", Av::N(1)), (":b", Av::N(1))],
    );
    assert!(rejects(&sections).contains("SET"));
    assert!(expr_rejects(&update("", None, &[], &[])).is_some());

    // An empty set cannot be stored; an empty list and an empty string can.
    let empty_set = update(
        "SET #a = :s",
        None,
        &[("#a", "x")],
        &[(":s", Av::Ss(vec![]))],
    );
    assert!(rejects(&empty_set).contains("empty set"));
    let dup_set = update(
        "SET #a = :s",
        None,
        &[("#a", "x")],
        &[(":s", Av::Ss(v(&["a", "a"])))],
    );
    assert!(rejects(&dup_set).contains("duplicate"));
    let empty_list = update(
        "SET #a = :s",
        None,
        &[("#a", "x")],
        &[(":s", Av::L(vec![]))],
    );
    assert_eq!(expr_rejects(&empty_list), None);
    let empty_string = update(
        "SET #a = :s",
        None,
        &[("#a", "x")],
        &[(":s", Av::S(String::new()))],
    );
    assert_eq!(expr_rejects(&empty_string), None);

    // An item over 400 KB when the caller says how big it would be.
    let mut big = ok.clone();
    big.item_bytes = Some(400 * 1024);
    assert_eq!(expr_rejects(&big), None);
    big.item_bytes = Some(400 * 1024 + 1);
    assert!(rejects(&big).contains("400 KB"));

    // Placeholders: used and not defined, defined and not used.
    let undefined = update("SET #a = :a", None, &[], &[(":a", Av::N(1))]);
    assert!(rejects(&undefined).contains("#a"));
    let unused_name = update(
        "SET #a = :a",
        None,
        &[("#a", "x"), ("#b", "y")],
        &[(":a", Av::N(1))],
    );
    assert!(rejects(&unused_name).contains("#b"));
    let unused_value = update(
        "SET #a = :a",
        None,
        &[("#a", "x")],
        &[(":a", Av::N(1)), (":b", Av::N(1))],
    );
    assert!(rejects(&unused_value).contains(":b"));
    let in_condition = update(
        "SET #a = :a",
        Some("attribute_not_exists(#c)"),
        &[("#a", "x"), ("#c", "z")],
        &[(":a", Av::N(1))],
    );
    assert_eq!(
        expr_rejects(&in_condition),
        None,
        "a placeholder used only in the condition is used"
    );

    // An expression over 4,096 bytes.
    let long = format!("SET #a = :a{}", " ".repeat(4100));
    assert!(rejects(&update(&long, None, &[("#a", "x")], &[(":a", Av::N(1))])).contains("4096"));
}

#[test]
fn every_reserved_word_is_refused_as_a_bare_attribute_name_and_passes_when_aliased() {
    let words: Vec<String> = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/dynamodb-reserved-words.txt"
    ))
    .unwrap()
    .lines()
    .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
    .map(str::to_string)
    .collect();
    assert_eq!(words.len(), 573);
    assert_eq!((words[0].as_str(), words[572].as_str()), ("abort", "zone"));
    for w in &words {
        assert!(is_reserved(w) && is_reserved(&w.to_uppercase()), "{w}");
        let bare = update(&format!("SET {w} = :a"), None, &[], &[(":a", Av::N(1))]);
        assert!(
            expr_rejects(&bare).is_some(),
            "SET {w} = :a should be refused"
        );
        // In a condition, AND, OR, NOT, BETWEEN and IN are operators; every other reserved word is refused as a bare name.
        if !["and", "or", "not", "between", "in"].contains(&w.as_str()) {
            let in_condition = update(
                "SET #a = :a",
                Some(&format!("attribute_exists({w})")),
                &[("#a", "x")],
                &[(":a", Av::N(1))],
            );
            assert!(
                expr_rejects(&in_condition).is_some(),
                "attribute_exists({w}) should be refused"
            );
        }
        let aliased = update(
            "SET #n = :a",
            None,
            &[("#n", w.as_str())],
            &[(":a", Av::N(1))],
        );
        assert_eq!(
            expr_rejects(&aliased),
            None,
            "{w} through a name alias is fine"
        );
    }
    // The function names of an expression are not attribute names, even when they are reserved words (`size`).
    let size = update(
        "SET #a = :a",
        Some("size(#a) > :a"),
        &[("#a", "x")],
        &[(":a", Av::N(1))],
    );
    assert_eq!(expr_rejects(&size), None);
    // The names the tag writers use are not reserved.
    for name in TAG_ATTRIBUTES
        .iter()
        .chain(["runId", "createdAt", "PK", "SK"].iter())
    {
        assert!(!is_reserved(name), "{name}");
    }
}

#[test]
fn a_query_without_a_projection_for_specific_attributes_is_refused() {
    let q = vocabulary_query("OWNER#abc");
    assert_eq!(expr_rejects(&q), None);

    let mut no_projection = q.clone();
    no_projection.projection = None;
    assert!(
        expr_rejects(&no_projection)
            .unwrap()
            .contains("SPECIFIC_ATTRIBUTES")
    );
    let mut all = q.clone();
    all.select = Some("ALL_ATTRIBUTES".to_string());
    assert!(expr_rejects(&all).unwrap().contains("ProjectionExpression"));
    let mut count = q.clone();
    count.select = Some("COUNT".to_string());
    assert!(expr_rejects(&count).is_some());
    let mut unknown = q.clone();
    unknown.select = Some("EVERYTHING".to_string());
    assert!(expr_rejects(&unknown).is_some());
    let mut zero = q.clone();
    zero.limit = 0;
    assert!(expr_rejects(&zero).unwrap().contains("Limit"));
    // No select at all with a projection is DynamoDB's own default (SPECIFIC_ATTRIBUTES).
    let mut implicit = q.clone();
    implicit.select = None;
    assert_eq!(expr_rejects(&implicit), None);
    // A projection that names a path twice, or an alias that is not defined.
    let mut twice = q.clone();
    twice.projection = Some("#runTags, #runTags".to_string());
    assert!(expr_rejects(&twice).unwrap().contains("overlap"));
    let mut undefined = q.clone();
    undefined.projection = Some("#runTags, #nope".to_string());
    assert!(expr_rejects(&undefined).unwrap().contains("#nope"));
    // A reserved word as a bare projected name.
    let mut reserved = q.clone();
    reserved.projection = Some("status".to_string());
    assert!(expr_rejects(&reserved).unwrap().contains("reserved"));
    let mut empty_key = q.clone();
    empty_key.key_condition = String::new();
    assert!(expr_rejects(&empty_key).is_some());
    let mut empty_set = q;
    empty_set.values.insert(":pk".to_string(), Av::Ss(vec![]));
    assert!(expr_rejects(&empty_set).unwrap().contains("empty set"));
}

fn all_updates() -> Vec<(&'static str, UpdateSpec)> {
    let user = v(&["project:billing", "topic:mine"]);
    let off = v(&["tech:s3"]);
    let tags = v(&["topic:csv-export", "tech:python"]);
    let answered = Some(Answered {
        route: "us.anthropic.claude-haiku-5-5",
        version: "rt-1",
    });
    vec![
        (
            "inherit",
            inherit_update("R1", "2026-10-20T16:00:00Z", "R0", &user, &off).unwrap(),
        ),
        (
            "inherit user only",
            inherit_update("R1", "2026-10-20T16:00:00Z", "R0", &user, &[]).unwrap(),
        ),
        (
            "inherit off only",
            inherit_update("R1", "2026-10-20T16:00:00Z", "R0", &[], &off).unwrap(),
        ),
        (
            "auto",
            auto_update(
                "R1",
                "2026-10-20T16:00:00Z",
                &tags,
                "2026-10-20T16:00:05Z",
                answered,
            ),
        ),
        (
            "auto inherited only",
            auto_update(
                "R1",
                "2026-10-20T16:00:00Z",
                &tags,
                "2026-10-20T16:00:05Z",
                None,
            ),
        ),
        (
            "auto empty answer",
            auto_update(
                "R1",
                "2026-10-20T16:00:00Z",
                &[],
                "2026-10-20T16:00:05Z",
                answered,
            ),
        ),
        (
            "edit",
            edit_update("R1", "2026-10-20T16:00:00Z", &user, &off, Some(3)),
        ),
        (
            "edit first",
            edit_update("R1", "2026-10-20T16:00:00Z", &user, &off, None),
        ),
        (
            "edit no user",
            edit_update("R1", "2026-10-20T16:00:00Z", &[], &off, Some(1)),
        ),
        (
            "edit no off",
            edit_update("R1", "2026-10-20T16:00:00Z", &user, &[], Some(1)),
        ),
        (
            "edit nothing left",
            edit_update("R1", "2026-10-20T16:00:00Z", &[], &[], Some(2)),
        ),
    ]
}

#[test]
fn the_three_writes_and_the_query_pass_the_rules_and_stay_disjoint() {
    for (what, spec) in all_updates() {
        assert_eq!(expr_rejects(&spec), None, "{what}: {spec:?}");
        assert!(spec.key_sk_prefix_note.contains("SRCH#"), "{what}");
        let allowed: BTreeSet<&str> = TAG_ATTRIBUTES
            .iter()
            .copied()
            .chain(["runId", "createdAt"])
            .collect();
        for name in spec.names.values() {
            assert!(allowed.contains(name.as_str()), "{what} names {name}");
        }
        for banned in [
            "promptHead",
            "promptTruncated",
            "model",
            "repository",
            "outcome",
            "lastActivity",
            "runName",
            "runNameSource",
            "runNameModel",
        ] {
            assert!(
                !spec.names.values().any(|n| n == banned),
                "{what} names {banned}"
            );
            assert!(
                !spec.expression.contains(banned)
                    && !spec.condition.clone().unwrap_or_default().contains(banned),
                "{what}"
            );
        }
        // Every attribute goes through an alias: no bare name for a reserved word to hide in.
        for name in TAG_ATTRIBUTES.iter().chain(["runId", "createdAt"].iter()) {
            let bare = spec.expression.replace(&format!("#{name}"), "");
            assert!(!bare.contains(name), "{what}: bare {name}");
        }
    }

    // The automatic write and the edit write share only the revision and the two creation attributes.
    let all = all_updates();
    let get = |label: &str| {
        all.iter()
            .find(|(w, _)| *w == label)
            .map(|(_, s)| s)
            .unwrap()
    };
    let auto = attribute_names(get("auto"));
    let edit = attribute_names(get("edit"));
    let shared: BTreeSet<String> = auto.intersection(&edit).cloned().collect();
    assert_eq!(
        shared,
        ["createdAt", "runId", "runTagsRev"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    );
    // The inherit write and the automatic write share the same three, and the inherit write and the edit also share the two lists.
    let inherit = attribute_names(get("inherit"));
    assert_eq!(inherit.intersection(&auto).count(), 3);
    assert!(
        inherit.contains("runTagsUser")
            && inherit.contains("runTagsOff")
            && inherit.contains("runTagsInh")
    );
}

#[test]
fn the_expressions_are_the_designed_ones() {
    let all = all_updates();
    let get = |label: &str| {
        all.iter()
            .find(|(w, _)| *w == label)
            .map(|(_, s)| s.clone())
            .unwrap()
    };

    let inherit = get("inherit");
    assert_eq!(
        inherit.expression,
        "SET #runId = if_not_exists(#runId, :rid), #createdAt = if_not_exists(#createdAt, :cat), #runTagsUser = :user, #runTagsOff = :off, #runTagsInh = :inh, #runTagsRev = if_not_exists(#runTagsRev, :zero) + :one"
    );
    assert_eq!(
        inherit.condition.as_deref(),
        Some("attribute_not_exists(#runTagsInh) AND attribute_not_exists(#runTagsRev)")
    );
    assert_eq!(
        inherit.values[":user"],
        list(&["project:billing", "topic:mine"])
    );
    assert_eq!(inherit.values[":inh"], Av::S("R0".to_string()));
    assert_eq!(
        (&inherit.values[":zero"], &inherit.values[":one"]),
        (&Av::N(0), &Av::N(1))
    );
    // A list that is empty is left out; both empty means there is nothing to inherit.
    assert!(!get("inherit user only").expression.contains(":off"));
    assert!(!get("inherit off only").expression.contains(":user"));
    assert_eq!(
        inherit_update("R1", "2026-10-20T16:00:00Z", "R0", &[], &[]),
        None
    );

    let auto = get("auto");
    assert_eq!(
        auto.expression,
        "SET #runId = if_not_exists(#runId, :rid), #createdAt = if_not_exists(#createdAt, :cat), #runTags = :tags, #runTagsAt = :at, #runTagsModel = :route, #runTagsVer = :ver, #runTagsRev = if_not_exists(#runTagsRev, :zero) + :one"
    );
    assert_eq!(
        auto.condition.as_deref(),
        Some("attribute_not_exists(#runTagsAt)")
    );
    assert_eq!(
        auto.values[":route"],
        Av::S("us.anthropic.claude-haiku-5-5".to_string())
    );
    assert_eq!(auto.values[":ver"], Av::S("rt-1".to_string()));
    assert_eq!(
        auto.values[":at"],
        Av::S("2026-10-20T16:00:05Z".to_string())
    );
    // Without a model answer there is no model or version; with an empty list there is no tags attribute but the marker stays.
    let inherited = get("auto inherited only");
    assert!(
        !inherited.expression.contains("runTagsModel")
            && !inherited.expression.contains("runTagsVer")
    );
    let empty = get("auto empty answer");
    assert!(
        !empty.expression.contains("#runTags ="),
        "{}",
        empty.expression
    );
    assert!(
        empty.expression.contains("#runTagsAt = :at") && empty.expression.contains("#runTagsModel")
    );
    assert!(!attribute_names(&empty).contains("runTags"));

    let edit = get("edit");
    assert_eq!(
        edit.expression,
        "SET #runId = if_not_exists(#runId, :rid), #createdAt = if_not_exists(#createdAt, :cat), #runTagsUser = :user, #runTagsOff = :off, #runTagsRev = if_not_exists(#runTagsRev, :zero) + :one"
    );
    assert_eq!(
        edit.condition.as_deref(),
        Some("attribute_not_exists(#runTagsRev) OR #runTagsRev = :r")
    );
    assert_eq!(edit.values[":r"], Av::N(3));
    assert_eq!(get("edit first").values[":r"], Av::N(0));
    // An empty list is removed, never set to a placeholder, and never both.
    let no_user = get("edit no user");
    assert!(
        no_user.expression.ends_with(" REMOVE #runTagsUser"),
        "{}",
        no_user.expression
    );
    assert!(!no_user.expression.contains("#runTagsUser = "));
    let none = get("edit nothing left");
    assert!(
        none.expression
            .ends_with(" REMOVE #runTagsUser, #runTagsOff"),
        "{}",
        none.expression
    );
    assert!(!none.values.contains_key(":user") && !none.values.contains_key(":off"));
}

#[test]
fn the_vocabulary_query_is_narrow() {
    let q: QuerySpec = vocabulary_query("OWNER#0123");
    assert_eq!(q.key_condition, "#pk = :pk AND begins_with(#sk, :sk)");
    assert_eq!(q.values[":pk"], Av::S("OWNER#0123".to_string()));
    assert_eq!(q.values[":sk"], Av::S("SRCH#".to_string()));
    assert_eq!(q.filter, None, "no filter expression");
    assert_eq!(q.select.as_deref(), Some("SPECIFIC_ATTRIBUTES"));
    assert!(!q.consistent_read);
    assert_eq!(q.limit, 200);
    assert!(!q.scan_index_forward, "newest first");
    // The projection names exactly the three tag lists.
    let projected: Vec<String> = q
        .projection
        .as_deref()
        .unwrap()
        .split(',')
        .map(|a| q.names[a.trim()].clone())
        .collect();
    assert_eq!(projected, ["runTags", "runTagsUser", "runTagsOff"]);
    assert_eq!(q.names["#pk"], "PK");
    assert_eq!(q.names["#sk"], "SK");
    assert_eq!(q.names.len(), 5);
    assert_eq!(expr_rejects(&q), None);
}

#[test]
fn literals_key_updates_and_operand_types_are_refused() {
    // DynamoDB expressions have no literals: every value is a placeholder (a list index is the one number).
    let literal = update("SET #a = 5", None, &[("#a", "x")], &[]);
    assert!(
        rejects(&literal).contains("literal"),
        "{}",
        rejects(&literal)
    );
    let literal = update("SET #a = :a + 1", None, &[("#a", "x")], &[(":a", Av::N(1))]);
    assert!(rejects(&literal).contains("literal"));
    let index = update(
        "SET #a[0] = :a",
        None,
        &[("#a", "x")],
        &[(":a", Av::S("v".to_string()))],
    );
    assert_eq!(expr_rejects(&index), None);

    // The key attributes of the table cannot be updated.
    let key = update(
        "SET #k = :a",
        None,
        &[("#k", "PK")],
        &[(":a", Av::S("v".to_string()))],
    );
    assert!(rejects(&key).contains("key attribute"), "{}", rejects(&key));
    let key = update("REMOVE #k", None, &[("#k", "SK")], &[]);
    assert!(rejects(&key).contains("key attribute"));

    // ADD takes a number or a set, DELETE a set.
    let on =
        |expression: &str, value: Av| update(expression, None, &[("#a", "x")], &[(":v", value)]);
    assert_eq!(expr_rejects(&on("ADD #a :v", Av::N(1))), None);
    assert_eq!(expr_rejects(&on("ADD #a :v", Av::Ss(v(&["a"])))), None);
    assert!(rejects(&on("ADD #a :v", Av::L(v(&["a"])))).contains("ADD"));
    assert!(rejects(&on("ADD #a :v", Av::S("a".to_string()))).contains("ADD"));
    assert_eq!(expr_rejects(&on("DELETE #a :v", Av::Ss(v(&["a"])))), None);
    assert!(rejects(&on("DELETE #a :v", Av::N(1))).contains("DELETE"));
}

#[test]
fn each_writer_names_only_its_own_attributes() {
    let all = all_updates();
    let names_of = |label: &str| {
        attribute_names(
            all.iter()
                .find(|(w, _)| *w == label)
                .map(|(_, s)| s)
                .unwrap(),
        )
    };
    let none_of = |label: &str, banned: &[&str]| {
        let names = names_of(label);
        for b in banned {
            assert!(!names.contains(*b), "{label} names {b}");
        }
    };
    // The automatic write never touches what the person owns, so a late model answer cannot undo an edit.
    none_of("auto", &["runTagsUser", "runTagsOff", "runTagsInh"]);
    none_of(
        "auto empty answer",
        &["runTagsUser", "runTagsOff", "runTagsInh"],
    );
    // The edit never touches what the model owns.
    none_of(
        "edit",
        &[
            "runTags",
            "runTagsAt",
            "runTagsModel",
            "runTagsVer",
            "runTagsInh",
        ],
    );
    none_of(
        "edit nothing left",
        &[
            "runTags",
            "runTagsAt",
            "runTagsModel",
            "runTagsVer",
            "runTagsInh",
        ],
    );
    // The copy for a follow-up writes the person's lists only.
    none_of(
        "inherit",
        &["runTags", "runTagsAt", "runTagsModel", "runTagsVer"],
    );
}
