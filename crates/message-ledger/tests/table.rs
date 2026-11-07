//! The table abstraction: what a condition means, how it compiles to a DynamoDB expression, and that the in-memory table refuses what
//! DynamoDB refuses. The ledger's race tests rest on these behaving like the real table.

#![allow(clippy::unwrap_used)]

use message_ledger::mem::MemTable;
use message_ledger::table::{
    Cond, Item, MAX_ITEM_BYTES, PK, SK, Table, TableError, WriteOp, check_transaction, compile,
    diff, item_size,
};
use serde_json::{Value, json};

fn item(pk: &str, sk: &str, attrs: &[(&str, Value)]) -> Item {
    let mut i = Item::new();
    i.insert(PK.into(), json!(pk));
    i.insert(SK.into(), json!(sk));
    for (k, v) in attrs {
        i.insert((*k).into(), v.clone());
    }
    i
}

fn put(pk: &str, sk: &str, attrs: &[(&str, Value)], cond: Option<Cond>) -> WriteOp {
    WriteOp::Put {
        item: item(pk, sk, attrs),
        cond,
    }
}

fn update(
    pk: &str,
    sk: &str,
    set: &[(&str, Value)],
    remove: &[&str],
    cond: Option<Cond>,
) -> WriteOp {
    let mut s = Item::new();
    for (k, v) in set {
        s.insert((*k).into(), v.clone());
    }
    WriteOp::Update {
        pk: pk.into(),
        sk: sk.into(),
        set: s,
        remove: remove.iter().map(|x| (*x).to_string()).collect(),
        cond,
    }
}

#[test]
fn a_condition_is_judged_the_way_dynamodb_judges_it() {
    let i = item(
        "A",
        "B",
        &[("version", json!(3)), ("state", json!("pending"))],
    );
    assert!(Cond::Exists("version").holds(Some(&i)));
    assert!(!Cond::Exists("nope").holds(Some(&i)));
    assert!(!Cond::Exists(PK).holds(None));
    assert!(Cond::NotExists(PK).holds(None));
    assert!(!Cond::NotExists(PK).holds(Some(&i)));
    assert!(Cond::NotExists("nope").holds(Some(&i)));
    assert!(Cond::Eq("version", json!(3)).holds(Some(&i)));
    assert!(!Cond::Eq("version", json!(4)).holds(Some(&i)));
    // an attribute the item lacks equals nothing, and so does an item that is not there
    assert!(!Cond::Eq("nope", json!(3)).holds(Some(&i)));
    assert!(!Cond::Eq("version", json!(3)).holds(None));
    // a number is not its text
    assert!(!Cond::Eq("version", json!("3")).holds(Some(&i)));
    assert!(Cond::And(vec![Cond::Exists(PK), Cond::Eq("state", json!("pending"))]).holds(Some(&i)));
    assert!(!Cond::And(vec![Cond::Exists(PK), Cond::Eq("state", json!("held"))]).holds(Some(&i)));
    assert!(Cond::Or(vec![Cond::NotExists(PK), Cond::Eq("version", json!(3))]).holds(Some(&i)));
    assert!(Cond::Or(vec![Cond::NotExists(PK), Cond::Eq("version", json!(3))]).holds(None));
    assert!(!Cond::Or(vec![Cond::NotExists(PK), Cond::Eq("version", json!(9))]).holds(Some(&i)));
}

#[test]
fn a_condition_and_an_update_compile_to_expressions_over_placeholders() {
    let cond = Cond::And(vec![Cond::Exists(PK), Cond::Eq("version", json!(7))]);
    let mut set = Item::new();
    set.insert("state".into(), json!("held"));
    set.insert("version".into(), json!(8));
    let c = compile(Some(&cond), &set, &["reason".to_string()]);
    assert_eq!(
        c.condition.as_deref(),
        Some("(attribute_exists(#a0) AND #a1 = :v0)")
    );
    // the placeholder of "version" is shared by the condition and the update
    assert_eq!(c.names.get("#a1").map(String::as_str), Some("version"));
    assert_eq!(c.names.values().filter(|n| *n == "version").count(), 1);
    assert_eq!(c.values.get(":v0"), Some(&json!(7)));
    let update = c.update.unwrap();
    assert!(update.starts_with("SET "), "{update}");
    assert!(update.contains("#a1 = :v"), "{update}");
    assert!(
        update.ends_with("REMOVE #a4") || update.contains(" REMOVE #a"),
        "{update}"
    );
    // every placeholder the expressions use is defined, and every one defined is used
    for p in c.names.keys() {
        assert!(
            c.condition.as_deref().unwrap_or("").contains(p.as_str())
                || update.contains(p.as_str()),
            "{p} is unused"
        );
    }
    for p in c.values.keys() {
        assert!(
            c.condition.as_deref().unwrap_or("").contains(p.as_str())
                || update.contains(p.as_str()),
            "{p} is unused"
        );
    }
    // no condition, no update: nothing to send
    let none = compile(None, &Item::new(), &[]);
    assert_eq!((none.condition, none.update), (None, None));
    // an OR is parenthesised and single terms are not
    let or = compile(
        Some(&Cond::Or(vec![
            Cond::NotExists(PK),
            Cond::Eq("count", json!(2)),
        ])),
        &Item::new(),
        &[],
    );
    assert_eq!(
        or.condition.as_deref(),
        Some("(attribute_not_exists(#a0) OR #a1 = :v0)")
    );
    let single = compile(Some(&Cond::NotExists(PK)), &Item::new(), &[]);
    assert_eq!(
        single.condition.as_deref(),
        Some("attribute_not_exists(#a0)")
    );
}

#[test]
fn a_diff_sets_what_changed_removes_what_is_gone_and_never_touches_a_key() {
    let old = item(
        "A",
        "B",
        &[("a", json!(1)), ("b", json!("x")), ("gone", json!(true))],
    );
    let new = item(
        "A",
        "B",
        &[("a", json!(1)), ("b", json!("y")), ("fresh", json!([1]))],
    );
    let (set, remove) = diff(&old, &new);
    assert_eq!(set.get("b"), Some(&json!("y")));
    assert_eq!(set.get("fresh"), Some(&json!([1])));
    assert!(!set.contains_key("a"), "an unchanged attribute is not set");
    assert!(!set.contains_key(PK) && !set.contains_key(SK));
    assert_eq!(remove, vec!["gone".to_string()]);
}

#[tokio::test]
async fn a_transaction_writes_all_its_operations_or_none() {
    let t = MemTable::new();
    t.transact(vec![put(
        "A",
        "1",
        &[("v", json!(1))],
        Some(Cond::NotExists(PK)),
    )])
    .await
    .unwrap();
    // the second operation's condition fails, so the first is not written either
    let err = t
        .transact(vec![
            put("A", "2", &[("v", json!(2))], Some(Cond::NotExists(PK))),
            put("A", "1", &[("v", json!(9))], Some(Cond::NotExists(PK))),
        ])
        .await
        .unwrap_err();
    assert_eq!(err, TableError::ConditionFailed { index: 1 });
    assert!(t.item("A", "2").is_none());
    assert_eq!(t.item("A", "1").unwrap()["v"], json!(1));
    assert_eq!(t.stats().committed, 1);
    assert_eq!(t.stats().transactions, 2);
}

#[tokio::test]
async fn an_update_sets_and_removes_and_a_condition_decides_whether_it_may_create() {
    let t = MemTable::new();
    // an update with no condition creates the item, as DynamoDB does
    t.transact(vec![update("A", "1", &[("v", json!(1))], &[], None)])
        .await
        .unwrap();
    assert_eq!(t.item("A", "1").unwrap()["v"], json!(1));
    assert_eq!(t.item("A", "1").unwrap()[PK], json!("A"));
    // a guarded update of a missing item does not
    let err = t
        .transact(vec![update(
            "A",
            "9",
            &[("v", json!(1))],
            &[],
            Some(Cond::Exists(PK)),
        )])
        .await
        .unwrap_err();
    assert_eq!(err, TableError::ConditionFailed { index: 0 });
    assert!(t.item("A", "9").is_none());
    t.transact(vec![update(
        "A",
        "1",
        &[("w", json!("x"))],
        &["v"],
        Some(Cond::Eq("v", json!(1))),
    )])
    .await
    .unwrap();
    let i = t.item("A", "1").unwrap();
    assert_eq!(i.get("w"), Some(&json!("x")));
    assert!(!i.contains_key("v"));
}

#[tokio::test]
async fn the_table_refuses_what_dynamodb_refuses() {
    let t = MemTable::new();
    // two operations on one item
    let e = t
        .transact(vec![
            put("A", "1", &[], None),
            update("A", "1", &[("v", json!(1))], &[], None),
        ])
        .await
        .unwrap_err();
    assert!(matches!(e, TableError::Invalid(m) if m.contains("same item")));
    // an empty transaction and one of 101 operations
    assert!(matches!(
        t.transact(vec![]).await,
        Err(TableError::Invalid(_))
    ));
    let many: Vec<WriteOp> = (0..101)
        .map(|n| put("A", &format!("{n:03}"), &[], None))
        .collect();
    assert!(matches!(
        t.transact(many).await,
        Err(TableError::Invalid(_))
    ));
    let hundred: Vec<WriteOp> = (0..100)
        .map(|n| put("A", &format!("{n:03}"), &[], None))
        .collect();
    t.transact(hundred).await.unwrap();
    // a key that is missing, not a string, empty or too long
    let mut no_sk = item("A", "x", &[]);
    no_sk.remove(SK);
    assert!(matches!(
        t.transact(vec![WriteOp::Put {
            item: no_sk,
            cond: None
        }])
        .await,
        Err(TableError::Invalid(_))
    ));
    let mut num_pk = item("A", "x", &[]);
    num_pk.insert(PK.into(), json!(5));
    assert!(matches!(
        t.transact(vec![WriteOp::Put {
            item: num_pk,
            cond: None
        }])
        .await,
        Err(TableError::Invalid(_))
    ));
    assert!(matches!(
        t.transact(vec![put("", "x", &[], None)]).await,
        Err(TableError::Invalid(_))
    ));
    assert!(matches!(
        t.transact(vec![put(&"p".repeat(2049), "x", &[], None)])
            .await,
        Err(TableError::Invalid(_))
    ));
    assert!(matches!(
        t.transact(vec![put("A", &"s".repeat(1025), &[], None)])
            .await,
        Err(TableError::Invalid(_))
    ));
    // an update may not change a key, nor set and remove one attribute, nor be empty
    assert!(matches!(
        t.transact(vec![update("A", "x", &[(PK, json!("B"))], &[], None)])
            .await,
        Err(TableError::Invalid(_))
    ));
    assert!(matches!(
        t.transact(vec![update("A", "x", &[("v", json!(1))], &["v"], None)])
            .await,
        Err(TableError::Invalid(_))
    ));
    assert!(matches!(
        t.transact(vec![update("A", "x", &[], &[], None)]).await,
        Err(TableError::Invalid(_))
    ));
}

#[tokio::test]
async fn an_item_over_400_kb_is_refused_and_nothing_of_its_transaction_is_written() {
    let t = MemTable::new();
    let big = "x".repeat(MAX_ITEM_BYTES);
    let e = t
        .transact(vec![
            put("A", "small", &[("v", json!(1))], None),
            put("A", "big", &[("blob", json!(big))], None),
        ])
        .await
        .unwrap_err();
    assert!(matches!(e, TableError::Invalid(m) if m.contains("maximum allowed size")));
    assert!(t.item("A", "small").is_none());
    // an update that grows an item past the limit is refused as well
    t.transact(vec![put(
        "A",
        "grow",
        &[("blob", json!("y".repeat(300 * 1024)))],
        None,
    )])
    .await
    .unwrap();
    let e = t
        .transact(vec![update(
            "A",
            "grow",
            &[("more", json!("z".repeat(200 * 1024)))],
            &[],
            None,
        )])
        .await
        .unwrap_err();
    assert!(matches!(e, TableError::Invalid(_)));
    assert!(!t.item("A", "grow").unwrap().contains_key("more"));
    assert!(item_size(&t.item("A", "grow").unwrap()) < MAX_ITEM_BYTES);
    // the size rule counts names, strings, numbers at 21 bytes and containers at 3 plus their parts
    let i = item(
        "A",
        "B",
        &[
            ("n", json!(1)),
            ("s", json!("abc")),
            ("l", json!([1, "ab"])),
        ],
    );
    let expect = (2 + 1) + (2 + 1) // PK, SK names and values
        + 1 + 21 // n
        + 1 + 3 // s
        + 1 + (3 + (1 + 21) + (1 + 2)); // l
    assert_eq!(item_size(&i), expect);
    assert!(check_transaction(&[put("A", "B", &[], None)]).is_ok());
}

#[tokio::test]
async fn a_prefix_read_is_ordered_by_sort_key_and_stays_in_its_partition() {
    let t = MemTable::new();
    for (pk, sk) in [
        ("A", "MSG#2"),
        ("A", "MSG#1"),
        ("A", "MSGCTL"),
        ("B", "MSG#1"),
        ("A", "OTHER"),
    ] {
        t.put_raw(item(pk, sk, &[]));
    }
    let got = t.query("A", "MSG#").await.unwrap();
    let sks: Vec<&str> = got.iter().map(|i| i[SK].as_str().unwrap()).collect();
    assert_eq!(sks, ["MSG#1", "MSG#2"]);
    assert!(t.get("A", "MSGCTL").await.unwrap().is_some());
    assert!(t.get("A", "MSG#9").await.unwrap().is_none());
}

#[tokio::test]
async fn a_writer_that_slips_in_before_a_transaction_is_judged_defeats_its_condition() {
    let t = MemTable::new();
    t.put_raw(item("A", "1", &[("version", json!(1))]));
    let rival = t.clone();
    t.before_next_transact(move |_| {
        rival.put_raw(item("A", "1", &[("version", json!(2))]));
    });
    let e = t
        .transact(vec![update(
            "A",
            "1",
            &[("version", json!(2))],
            &[],
            Some(Cond::Eq("version", json!(1))),
        )])
        .await
        .unwrap_err();
    assert_eq!(e, TableError::ConditionFailed { index: 0 });
    t.fail_next(TableError::Contended("busy".into()));
    assert!(matches!(
        t.transact(vec![put("A", "2", &[], None)]).await,
        Err(TableError::Contended(_))
    ));
}
