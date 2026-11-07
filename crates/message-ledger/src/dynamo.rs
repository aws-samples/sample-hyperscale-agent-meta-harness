//! DynamoDB behind [`Table`]. Reads are consistent. A transaction is one `TransactWriteItems` call; a failed condition comes back as
//! [`TableError::ConditionFailed`] with the index of the first operation whose condition failed, a conflicting transaction or throttling as
//! [`TableError::Contended`], and a request DynamoDB refuses (an item over 400 KB) as [`TableError::Invalid`].
//!
//! IAM: a `Put` needs `dynamodb:PutItem` and an `Update` needs `dynamodb:UpdateItem` on the item; there is no separate permission for the
//! transaction itself (DynamoDB documentation, "Using IAM with DynamoDB transactions").

use crate::table::{Compiled, Item, PK, SK, Table, TableError, WriteOp, compile};
use aws_sdk_dynamodb::types::{AttributeValue, Put, TransactWriteItem, Update};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct DynamoTable {
    pub client: aws_sdk_dynamodb::Client,
    pub table: String,
}

impl DynamoTable {
    pub fn new(client: aws_sdk_dynamodb::Client, table: impl Into<String>) -> Self {
        Self {
            client,
            table: table.into(),
        }
    }
}

fn unavailable<E: std::fmt::Display>(e: E) -> TableError {
    TableError::Unavailable(e.to_string())
}

fn s(v: &str) -> AttributeValue {
    AttributeValue::S(v.to_string())
}

fn attr(v: &Value) -> Result<AttributeValue, TableError> {
    serde_dynamo::to_attribute_value(v).map_err(unavailable)
}

fn item_of(m: HashMap<String, AttributeValue>) -> Result<Item, TableError> {
    serde_dynamo::from_item::<_, Item>(m).map_err(unavailable)
}

fn attrs(item: &Item) -> Result<HashMap<String, AttributeValue>, TableError> {
    item.iter()
        .map(|(k, v)| Ok((k.clone(), attr(v)?)))
        .collect()
}

type Names = Option<HashMap<String, String>>;
type Values = Option<HashMap<String, AttributeValue>>;

fn names_and_values(c: &Compiled) -> Result<(Names, Values), TableError> {
    let names: HashMap<String, String> = c.names.clone().into_iter().collect();
    let values: HashMap<String, AttributeValue> = c
        .values
        .iter()
        .map(|(k, v)| Ok((k.clone(), attr(v)?)))
        .collect::<Result<_, TableError>>()?;
    Ok((
        (!names.is_empty()).then_some(names),
        (!values.is_empty()).then_some(values),
    ))
}

/// One operation as a transaction item.
fn transact_item(table: &str, op: &WriteOp) -> Result<TransactWriteItem, TableError> {
    match op {
        WriteOp::Put { item, cond } => {
            let c = compile(cond.as_ref(), &Item::new(), &[]);
            let (names, values) = names_and_values(&c)?;
            let put = Put::builder()
                .table_name(table)
                .set_item(Some(attrs(item)?))
                .set_condition_expression(c.condition)
                .set_expression_attribute_names(names)
                .set_expression_attribute_values(values)
                .build()
                .map_err(unavailable)?;
            Ok(TransactWriteItem::builder().put(put).build())
        }
        WriteOp::Update {
            pk,
            sk,
            set,
            remove,
            cond,
        } => {
            let c = compile(cond.as_ref(), set, remove);
            let (names, values) = names_and_values(&c)?;
            let update = Update::builder()
                .table_name(table)
                .key(PK, s(pk))
                .key(SK, s(sk))
                .set_update_expression(c.update)
                .set_condition_expression(c.condition)
                .set_expression_attribute_names(names)
                .set_expression_attribute_values(values)
                .build()
                .map_err(unavailable)?;
            Ok(TransactWriteItem::builder().update(update).build())
        }
    }
}

impl Table for DynamoTable {
    async fn get(&self, pk: &str, sk: &str) -> Result<Option<Item>, TableError> {
        let out = self
            .client
            .get_item()
            .table_name(&self.table)
            .key(PK, s(pk))
            .key(SK, s(sk))
            .consistent_read(true)
            .send()
            .await
            .map_err(|e| unavailable(aws_sdk_dynamodb::error::DisplayErrorContext(&e)))?;
        out.item.map(item_of).transpose()
    }

    async fn query(&self, pk: &str, sk_prefix: &str) -> Result<Vec<Item>, TableError> {
        let mut out = Vec::new();
        let mut start: Option<HashMap<String, AttributeValue>> = None;
        loop {
            let page = self
                .client
                .query()
                .table_name(&self.table)
                .key_condition_expression("PK = :pk AND begins_with(SK, :sk)")
                .expression_attribute_values(":pk", s(pk))
                .expression_attribute_values(":sk", s(sk_prefix))
                .consistent_read(true)
                .set_exclusive_start_key(start.take())
                .send()
                .await
                .map_err(|e| unavailable(aws_sdk_dynamodb::error::DisplayErrorContext(&e)))?;
            for item in page.items.unwrap_or_default() {
                out.push(item_of(item)?);
            }
            match page.last_evaluated_key {
                Some(k) if !k.is_empty() => start = Some(k),
                _ => break,
            }
        }
        Ok(out)
    }

    async fn transact(&self, ops: Vec<WriteOp>) -> Result<(), TableError> {
        let items = ops
            .iter()
            .map(|op| transact_item(&self.table, op))
            .collect::<Result<Vec<_>, _>>()?;
        let res = self
            .client
            .transact_write_items()
            .set_transact_items(Some(items))
            .send()
            .await;
        match res {
            Ok(_) => Ok(()),
            Err(e) => {
                if let Some(se) = e.as_service_error() {
                    if se.is_transaction_canceled_exception() {
                        return Err(canceled(se));
                    }
                    if se.is_provisioned_throughput_exceeded_exception()
                        || se.is_request_limit_exceeded()
                        || se.is_transaction_in_progress_exception()
                    {
                        return Err(TableError::Contended(
                            aws_sdk_dynamodb::error::DisplayErrorContext(&e).to_string(),
                        ));
                    }
                }
                let text = aws_sdk_dynamodb::error::DisplayErrorContext(&e).to_string();
                if text.contains("ValidationException") {
                    Err(TableError::Invalid(text))
                } else {
                    Err(unavailable(text))
                }
            }
        }
    }
}

/// The reasons of a cancelled transaction: one per operation, in order. A failed condition is the answer the ledger acts on; a
/// conflict with another transaction or throttling is worth a retry; a validation error is the request's fault.
fn canceled(
    e: &aws_sdk_dynamodb::operation::transact_write_items::TransactWriteItemsError,
) -> TableError {
    use aws_sdk_dynamodb::operation::transact_write_items::TransactWriteItemsError as E;
    let E::TransactionCanceledException(t) = e else {
        return unavailable("a transaction was cancelled");
    };
    let reasons = t.cancellation_reasons();
    for (index, r) in reasons.iter().enumerate() {
        match r.code() {
            Some("ConditionalCheckFailed") => return TableError::ConditionFailed { index },
            Some("TransactionConflict" | "ThrottlingError" | "ProvisionedThroughputExceeded") => {
                return TableError::Contended(format!(
                    "operation {index}: {}",
                    r.code().unwrap_or_default()
                ));
            }
            Some("ValidationError") => {
                return TableError::Invalid(r.message().unwrap_or("validation error").to_string());
            }
            _ => {}
        }
    }
    unavailable(format!("a transaction was cancelled: {t}"))
}
