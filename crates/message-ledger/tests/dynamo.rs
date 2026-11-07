//! The DynamoDB table against recorded HTTP exchanges (aws-smithy StaticReplayClient, as the control-api and worker-api adapter tests do):
//! the exact requests it sends, how it reads the answers, and the whole of an admission through the real encoding.

#![allow(clippy::unwrap_used)]

mod support;

use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
use aws_smithy_types::body::SdkBody;
use harness_protocol::messages::{MessageState, SendOutcome};
use message_ledger::dynamo::DynamoTable;
use message_ledger::table::{Cond, Item, PK, SK, Table, TableError, WriteOp};
use message_ledger::{Ledger, ManualClock};
use serde_json::{Value, json};
use support::*;

fn replay(responses: Vec<(u16, String)>) -> StaticReplayClient {
    StaticReplayClient::new(
        responses
            .into_iter()
            .map(|(status, body)| {
                ReplayEvent::new(
                    http::Request::builder()
                        .uri("https://example.invalid/")
                        .body(SdkBody::empty())
                        .unwrap(),
                    http::Response::builder()
                        .status(status)
                        .header("content-type", "application/x-amz-json-1.0")
                        .body(SdkBody::from(body))
                        .unwrap(),
                )
            })
            .collect(),
    )
}

fn table(r: &StaticReplayClient) -> DynamoTable {
    use aws_sdk_dynamodb as sdk;
    DynamoTable::new(
        sdk::Client::from_conf(
            sdk::Config::builder()
                .behavior_version_latest()
                .region(sdk::config::Region::new("us-east-1"))
                .credentials_provider(sdk::config::Credentials::for_tests())
                .retry_config(sdk::config::retry::RetryConfig::disabled())
                .http_client(r.clone())
                .build(),
        ),
        "hamh-direct-harness",
    )
}

fn bodies(r: &StaticReplayClient) -> Vec<Value> {
    r.actual_requests()
        .map(|q| serde_json::from_slice(q.body().bytes().unwrap_or_default()).unwrap())
        .collect()
}

fn ok(body: &str) -> (u16, String) {
    (200, body.to_string())
}

fn item(pk: &str, sk: &str, attrs: &[(&str, Value)]) -> Item {
    let mut i = Item::new();
    i.insert(PK.into(), json!(pk));
    i.insert(SK.into(), json!(sk));
    for (k, v) in attrs {
        i.insert((*k).into(), v.clone());
    }
    i
}

#[tokio::test]
async fn a_get_is_a_consistent_read_of_one_key_and_numbers_come_back_as_numbers() {
    let r = replay(vec![
        ok(
            r#"{"Item":{"PK":{"S":"RUN#r"},"SK":{"S":"MSGCTL"},"version":{"N":"7"},"open":{"L":[]},"admission":{"S":"open"}}}"#,
        ),
        ok("{}"),
    ]);
    let t = table(&r);
    let got = t.get("RUN#r", "MSGCTL").await.unwrap().unwrap();
    assert_eq!(got["version"], json!(7));
    assert_eq!(got["open"], json!([]));
    assert_eq!(t.get("RUN#r", "MSG#x").await.unwrap(), None);
    let b = bodies(&r);
    assert_eq!(b[0]["TableName"], "hamh-direct-harness");
    assert_eq!(b[0]["ConsistentRead"], true);
    assert_eq!(
        b[0]["Key"],
        json!({"PK": {"S": "RUN#r"}, "SK": {"S": "MSGCTL"}})
    );
}

#[tokio::test]
async fn a_prefix_read_follows_the_pages_and_is_consistent() {
    let r = replay(vec![
        ok(
            r#"{"Items":[{"PK":{"S":"RUN#r"},"SK":{"S":"MSG#1"}}],"LastEvaluatedKey":{"PK":{"S":"RUN#r"},"SK":{"S":"MSG#1"}}}"#,
        ),
        ok(r#"{"Items":[{"PK":{"S":"RUN#r"},"SK":{"S":"MSG#2"}}]}"#),
    ]);
    let got = table(&r).query("RUN#r", "MSG#").await.unwrap();
    assert_eq!(got.len(), 2);
    let b = bodies(&r);
    assert_eq!(
        b[0]["KeyConditionExpression"],
        "PK = :pk AND begins_with(SK, :sk)"
    );
    assert_eq!(
        b[0]["ExpressionAttributeValues"],
        json!({":pk": {"S": "RUN#r"}, ":sk": {"S": "MSG#"}})
    );
    assert_eq!(b[0]["ConsistentRead"], true);
    assert_eq!(
        b[1]["ExclusiveStartKey"],
        json!({"PK": {"S": "RUN#r"}, "SK": {"S": "MSG#1"}})
    );
}

#[tokio::test]
async fn a_transaction_sends_puts_and_updates_with_their_conditions_as_expressions() {
    let r = replay(vec![ok("{}")]);
    let ops = vec![
        WriteOp::Put {
            item: item(
                "RUN#r",
                "MSG#1",
                &[
                    ("seq", json!(1)),
                    ("text", json!("hi")),
                    ("history", json!([{"state": "pending"}])),
                ],
            ),
            cond: Some(Cond::NotExists(PK)),
        },
        WriteOp::Update {
            pk: "RUN#r".into(),
            sk: "MSGCTL".into(),
            set: {
                let mut s = Item::new();
                s.insert("version".into(), json!(8));
                s.insert("epoch".into(), json!("s1"));
                s
            },
            remove: vec!["closedAt".into()],
            cond: Some(Cond::And(vec![
                Cond::Exists(PK),
                Cond::Eq("version", json!(7)),
            ])),
        },
    ];
    table(&r).transact(ops).await.unwrap();
    let b = &bodies(&r)[0];
    let items = b["TransactItems"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    let put = &items[0]["Put"];
    assert_eq!(put["TableName"], "hamh-direct-harness");
    assert_eq!(put["Item"]["PK"], json!({"S": "RUN#r"}));
    assert_eq!(put["Item"]["seq"], json!({"N": "1"}));
    assert_eq!(
        put["Item"]["history"],
        json!({"L": [{"M": {"state": {"S": "pending"}}}]})
    );
    assert_eq!(put["ConditionExpression"], "attribute_not_exists(#a0)");
    assert_eq!(put["ExpressionAttributeNames"], json!({"#a0": "PK"}));
    assert!(
        put.get("ExpressionAttributeValues").is_none(),
        "no placeholder values when the condition has none"
    );
    let upd = &items[1]["Update"];
    assert_eq!(
        upd["Key"],
        json!({"PK": {"S": "RUN#r"}, "SK": {"S": "MSGCTL"}})
    );
    assert_eq!(
        upd["ConditionExpression"],
        "(attribute_exists(#a0) AND #a1 = :v0)"
    );
    let ue = upd["UpdateExpression"].as_str().unwrap();
    assert!(ue.starts_with("SET ") && ue.contains(" REMOVE "), "{ue}");
    let names = upd["ExpressionAttributeNames"].as_object().unwrap();
    let values = upd["ExpressionAttributeValues"].as_object().unwrap();
    assert_eq!(values[":v0"], json!({"N": "7"}));
    assert!(names.values().any(|n| n == "closedAt") && names.values().any(|n| n == "epoch"));
    assert_eq!(
        values.len(),
        3,
        "one value for the condition, two for the SET"
    );
    // no key attribute is ever in the SET
    assert!(!names.values().any(|n| n == "SK"));
}

#[tokio::test]
async fn the_answers_of_dynamodb_become_the_ledgers_table_errors() {
    let cancelled = |codes: &str| {
        format!(
            r#"{{"__type":"com.amazonaws.dynamodb.v20120810#TransactionCanceledException","message":"Transaction cancelled","CancellationReasons":[{codes}]}}"#
        )
    };
    let one = vec![WriteOp::Put {
        item: item("A", "B", &[]),
        cond: None,
    }];
    let cases = [
        (
            400,
            cancelled(r#"{"Code":"None"},{"Code":"ConditionalCheckFailed","Message":"x"}"#),
            TableError::ConditionFailed { index: 1 },
        ),
        (
            400,
            cancelled(r#"{"Code":"TransactionConflict"}"#),
            TableError::Contended("operation 0: TransactionConflict".into()),
        ),
        (
            400,
            cancelled(r#"{"Code":"ThrottlingError"}"#),
            TableError::Contended("operation 0: ThrottlingError".into()),
        ),
        (
            400,
            cancelled(
                r#"{"Code":"ValidationError","Message":"Item size has exceeded the maximum allowed size"}"#,
            ),
            TableError::Invalid("Item size has exceeded the maximum allowed size".into()),
        ),
    ];
    for (status, body, want) in cases {
        let r = replay(vec![(status, body)]);
        assert_eq!(table(&r).transact(one.clone()).await.unwrap_err(), want);
    }
    let r = replay(vec![(400, r#"{"__type":"com.amazonaws.dynamodb.v20120810#ProvisionedThroughputExceededException","message":"slow down"}"#.to_string())]);
    assert!(matches!(
        table(&r).transact(one.clone()).await,
        Err(TableError::Contended(_))
    ));
    let r = replay(vec![(400, r#"{"__type":"com.amazon.coral.validate#ValidationException","message":"Item size has exceeded the maximum allowed size"}"#.to_string())]);
    assert!(matches!(
        table(&r).transact(one.clone()).await,
        Err(TableError::Invalid(_))
    ));
    let r = replay(vec![(
        500,
        r#"{"__type":"com.amazonaws.dynamodb.v20120810#InternalServerError","message":"boom"}"#
            .to_string(),
    )]);
    assert!(matches!(
        table(&r).transact(one).await,
        Err(TableError::Unavailable(_))
    ));
}

#[tokio::test]
async fn an_admission_through_dynamodb_writes_the_message_the_control_item_and_the_owners_counter_in_one_transaction()
 {
    // four calls: the message by id (absent), the control item (absent), the owner's counter (absent), the transaction
    let r = replay(vec![ok("{}"), ok("{}"), ok("{}"), ok("{}")]);
    let ledger = Ledger::with_clock(table(&r), ManualClock::at(T0));
    let a = ledger.admit(RUN, OWNER, &send(1, "hello")).await.unwrap();
    assert_eq!(
        (a.outcome, a.message.state),
        (SendOutcome::Accepted, MessageState::Pending)
    );
    let b = bodies(&r);
    assert_eq!(b.len(), 4);
    assert_eq!(b[0]["Key"]["SK"]["S"], format!("MSG#{}", mid(1)));
    assert_eq!(b[1]["Key"]["SK"]["S"], "MSGCTL");
    assert_eq!(b[2]["Key"]["PK"]["S"], format!("MSGRATE#{OWNER}"));
    let tx = b[3]["TransactItems"].as_array().unwrap();
    assert_eq!(tx.len(), 3);
    // the control item is created by an update that cannot replace one (worker-api and run-session hold UpdateItem, not PutItem);
    // the message and the counter are puts of control-api
    let ctl = tx
        .iter()
        .find(|t| t["Update"]["Key"]["SK"]["S"] == "MSGCTL")
        .unwrap();
    assert_eq!(
        ctl["Update"]["ConditionExpression"],
        "attribute_not_exists(#a0)"
    );
    assert_eq!(ctl["Update"]["ExpressionAttributeNames"]["#a0"], "PK");
    let names = ctl["Update"]["ExpressionAttributeNames"]
        .as_object()
        .unwrap();
    let values = ctl["Update"]["ExpressionAttributeValues"]
        .as_object()
        .unwrap();
    let set_of = |attr: &str| {
        let ph = names
            .iter()
            .find(|(_, n)| *n == attr)
            .map(|(k, _)| k.clone())
            .unwrap();
        let ue = ctl["Update"]["UpdateExpression"].as_str().unwrap();
        let after = ue.split(&format!("{ph} = ")).nth(1).unwrap();
        let v = after.split(&[',', ' '][..]).next().unwrap().to_string();
        values[&v].clone()
    };
    assert_eq!(set_of("version"), json!({"N": "1"}));
    assert_eq!(set_of("seq"), json!({"N": "1"}));
    assert!(
        !ctl["Update"]["UpdateExpression"]
            .as_str()
            .unwrap()
            .contains("REMOVE")
    );
    let msg = tx
        .iter()
        .find(|t| t["Put"]["Item"]["SK"]["S"] == format!("MSG#{}", mid(1)))
        .unwrap();
    assert_eq!(msg["Put"]["Item"]["principalArn"]["S"], OWNER);
    assert_eq!(msg["Put"]["Item"]["state"]["S"], "pending");
    assert!(
        msg["Put"]["Item"].get("ownerPrincipalArn").is_none()
            && msg["Put"]["Item"].get("createdAt").is_none()
    );
    let rate = tx
        .iter()
        .find(|t| t["Put"]["Item"]["PK"]["S"] == format!("MSGRATE#{OWNER}"))
        .unwrap();
    assert_eq!(rate["Put"]["Item"]["count"], json!({"N": "1"}));
    assert!(
        rate["Put"]["Item"]["ttl"]["N"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            > T0 as u64
    );
}

#[tokio::test]
async fn a_lost_condition_makes_the_ledger_read_again_and_a_second_try_succeeds() {
    let cancelled = r#"{"__type":"com.amazonaws.dynamodb.v20120810#TransactionCanceledException","message":"x","CancellationReasons":[{"Code":"ConditionalCheckFailed"},{"Code":"None"},{"Code":"None"}]}"#;
    // first try: reads (3), the transaction loses; second try: reads (3), the transaction wins
    let r = replay(vec![
        ok("{}"),
        ok("{}"),
        ok("{}"),
        (400, cancelled.to_string()),
        ok("{}"),
        ok("{}"),
        ok("{}"),
        ok("{}"),
    ]);
    let ledger = Ledger::with_clock(table(&r), ManualClock::at(T0));
    let a = ledger.admit(RUN, OWNER, &send(1, "hello")).await.unwrap();
    assert_eq!(a.message.seq, 1);
    assert_eq!(bodies(&r).len(), 8);
}
