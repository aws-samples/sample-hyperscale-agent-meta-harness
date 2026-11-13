//! The table the ledger reads and writes: three calls (a consistent read of one item, a read of a key prefix, one transaction) and the
//! conditions and updates a transaction carries. DynamoDB implements it ([`crate::dynamo::DynamoTable`]); so does the tests' in-memory copy
//! ([`crate::mem::MemTable`]), which refuses what DynamoDB refuses (a failed condition, two operations on one item, an item over 400 KB, a
//! key that is not a string). A condition is data, not text, so both copies judge the same condition: the in-memory one by evaluating it
//! ([`Cond::holds`]), the DynamoDB one by compiling it to an expression ([`compile`]).

use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::future::Future;

/// One item: its attributes by name, `PK` and `SK` included.
pub type Item = Map<String, Value>;
pub const PK: &str = "PK";
pub const SK: &str = "SK";

/// DynamoDB's largest item.
pub const MAX_ITEM_BYTES: usize = 400 * 1024;
/// DynamoDB's largest transaction, in operations.
pub const MAX_TRANSACTION_OPS: usize = 100;
/// DynamoDB's largest transaction, in bytes.
pub const MAX_TRANSACTION_BYTES: usize = 4 * 1024 * 1024;
/// DynamoDB's longest partition key and sort key, in bytes.
pub const MAX_PK_BYTES: usize = 2048;
pub const MAX_SK_BYTES: usize = 1024;

/// A condition on the item an operation writes. An attribute that the item lacks is not equal to anything, as in DynamoDB.
#[derive(Clone, Debug, PartialEq)]
pub enum Cond {
    Exists(&'static str),
    NotExists(&'static str),
    Eq(&'static str, Value),
    And(Vec<Cond>),
    Or(Vec<Cond>),
}

impl Cond {
    /// Whether the condition holds for the stored item (`None`: there is none).
    pub fn holds(&self, item: Option<&Item>) -> bool {
        match self {
            Cond::Exists(a) => item.is_some_and(|i| i.contains_key(*a)),
            Cond::NotExists(a) => item.is_none_or(|i| !i.contains_key(*a)),
            Cond::Eq(a, v) => item.and_then(|i| i.get(*a)).is_some_and(|x| x == v),
            Cond::And(list) => list.iter().all(|c| c.holds(item)),
            Cond::Or(list) => list.iter().any(|c| c.holds(item)),
        }
    }
}

/// One operation of a transaction. An update of an item that does not exist creates it, as in DynamoDB; every update the ledger sends
/// carries an `Exists(PK)` or a version condition, so it never does.
#[derive(Clone, Debug, PartialEq)]
pub enum WriteOp {
    /// Creates or replaces an item; `item` holds its `PK` and `SK`.
    Put { item: Item, cond: Option<Cond> },
    /// Sets the attributes in `set` and removes those in `remove` (disjoint; neither names a key attribute).
    Update {
        pk: String,
        sk: String,
        set: Item,
        remove: Vec<String>,
        cond: Option<Cond>,
    },
}

impl WriteOp {
    /// The `(PK, SK)` the operation writes.
    pub fn key(&self) -> Option<(String, String)> {
        match self {
            WriteOp::Put { item, .. } => Some((
                item.get(PK)?.as_str()?.to_string(),
                item.get(SK)?.as_str()?.to_string(),
            )),
            WriteOp::Update { pk, sk, .. } => Some((pk.clone(), sk.clone())),
        }
    }

    pub fn cond(&self) -> Option<&Cond> {
        match self {
            WriteOp::Put { cond, .. } | WriteOp::Update { cond, .. } => cond.as_ref(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TableError {
    /// The condition of the operation at `index` did not hold: somebody else changed the item since it was read.
    #[error("a condition of the write failed (operation {index})")]
    ConditionFailed { index: usize },
    /// The table refused the request itself (an item over 400 KB, two operations on one item, a bad key).
    #[error("the table refused the write: {0}")]
    Invalid(String),
    /// The table is busy with a conflicting write or throttling; the same request may succeed in a moment.
    #[error("the table is busy: {0}")]
    Contended(String),
    #[error("the table is unavailable: {0}")]
    Unavailable(String),
}

pub trait Table: Send + Sync {
    /// One item by key, read consistently.
    fn get(
        &self,
        pk: &str,
        sk: &str,
    ) -> impl Future<Output = Result<Option<Item>, TableError>> + Send;
    /// Every item of the partition whose sort key starts with `sk_prefix`, in sort-key order, read consistently (every page).
    fn query(
        &self,
        pk: &str,
        sk_prefix: &str,
    ) -> impl Future<Output = Result<Vec<Item>, TableError>> + Send;
    /// All the operations or none (`TransactWriteItems`).
    fn transact(&self, ops: Vec<WriteOp>) -> impl Future<Output = Result<(), TableError>> + Send;
}

/// The size DynamoDB counts for an item: the sum of its attribute names and values (documented rules; numbers at their ceiling of 21
/// bytes, strings and names as UTF-8, a list or map at 3 bytes plus 1 per element plus its contents).
pub fn item_size(item: &Item) -> usize {
    item.iter().map(|(k, v)| k.len() + value_size(v)).sum()
}

fn value_size(v: &Value) -> usize {
    match v {
        Value::Null | Value::Bool(_) => 1,
        Value::Number(_) => 21,
        Value::String(s) => s.len(),
        Value::Array(a) => 3 + a.iter().map(|x| 1 + value_size(x)).sum::<usize>(),
        Value::Object(m) => {
            3 + m
                .iter()
                .map(|(k, x)| 1 + k.len() + value_size(x))
                .sum::<usize>()
        }
    }
}

/// The checks DynamoDB makes of a transaction before it looks at any item.
pub fn check_transaction(ops: &[WriteOp]) -> Result<(), TableError> {
    if ops.is_empty() || ops.len() > MAX_TRANSACTION_OPS {
        return Err(TableError::Invalid(format!(
            "a transaction holds 1 to {MAX_TRANSACTION_OPS} operations, this one {}",
            ops.len()
        )));
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut bytes = 0usize;
    for op in ops {
        let Some((pk, sk)) = op.key() else {
            return Err(TableError::Invalid(
                "an item needs a string PK and a string SK".into(),
            ));
        };
        if pk.is_empty() || sk.is_empty() || pk.len() > MAX_PK_BYTES || sk.len() > MAX_SK_BYTES {
            return Err(TableError::Invalid(format!(
                "a key is empty or too long (PK {} bytes, SK {} bytes)",
                pk.len(),
                sk.len()
            )));
        }
        if !seen.insert((pk.clone(), sk.clone())) {
            return Err(TableError::Invalid(format!(
                "two operations of a transaction address the same item ({pk} / {sk})"
            )));
        }
        match op {
            WriteOp::Put { item, .. } => {
                let size = item_size(item);
                if size > MAX_ITEM_BYTES {
                    return Err(TableError::Invalid(format!(
                        "item size {size} has exceeded the maximum allowed size {MAX_ITEM_BYTES}"
                    )));
                }
                bytes += size;
            }
            WriteOp::Update { set, remove, .. } => {
                if set.contains_key(PK)
                    || set.contains_key(SK)
                    || remove.iter().any(|a| a == PK || a == SK)
                {
                    return Err(TableError::Invalid(
                        "an update cannot change a key attribute".into(),
                    ));
                }
                if remove.iter().any(|a| set.contains_key(a)) {
                    return Err(TableError::Invalid(
                        "an update cannot set and remove the same attribute".into(),
                    ));
                }
                if set.is_empty() && remove.is_empty() {
                    return Err(TableError::Invalid(
                        "an update expression needs at least one action".into(),
                    ));
                }
                bytes += item_size(set);
            }
        }
    }
    if bytes > MAX_TRANSACTION_BYTES {
        return Err(TableError::Invalid(format!(
            "the transaction holds {bytes} bytes; the limit is {MAX_TRANSACTION_BYTES}"
        )));
    }
    Ok(())
}

/// The attributes `new` sets and removes to turn `old` into `new`, both with their keys left out.
pub fn diff(old: &Item, new: &Item) -> (Item, Vec<String>) {
    let mut set = Item::new();
    for (k, v) in new {
        if k == PK || k == SK {
            continue;
        }
        if old.get(k) != Some(v) {
            set.insert(k.clone(), v.clone());
        }
    }
    let remove = old
        .keys()
        .filter(|k| *k != PK && *k != SK && !new.contains_key(*k))
        .cloned()
        .collect();
    (set, remove)
}

/// A condition or an update as DynamoDB takes it: an expression over placeholders, and what each placeholder stands for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Compiled {
    pub condition: Option<String>,
    pub update: Option<String>,
    pub names: BTreeMap<String, String>,
    pub values: BTreeMap<String, Value>,
}

struct Placeholders {
    names: BTreeMap<String, String>,
    by_attr: BTreeMap<String, String>,
    values: BTreeMap<String, Value>,
}

impl Placeholders {
    fn name(&mut self, attr: &str) -> String {
        if let Some(p) = self.by_attr.get(attr) {
            return p.clone();
        }
        let p = format!("#a{}", self.by_attr.len());
        self.by_attr.insert(attr.to_string(), p.clone());
        self.names.insert(p.clone(), attr.to_string());
        p
    }

    fn value(&mut self, v: &Value) -> String {
        let p = format!(":v{}", self.values.len());
        self.values.insert(p.clone(), v.clone());
        p
    }

    fn cond(&mut self, c: &Cond) -> String {
        match c {
            Cond::Exists(a) => format!("attribute_exists({})", self.name(a)),
            Cond::NotExists(a) => format!("attribute_not_exists({})", self.name(a)),
            Cond::Eq(a, v) => {
                let n = self.name(a);
                let p = self.value(v);
                format!("{n} = {p}")
            }
            Cond::And(list) => self.join(list, " AND "),
            Cond::Or(list) => self.join(list, " OR "),
        }
    }

    fn join(&mut self, list: &[Cond], sep: &str) -> String {
        let parts: Vec<String> = list.iter().map(|c| self.cond(c)).collect();
        match parts.len() {
            0 => "attribute_exists(#a0)".to_string(),
            1 => parts.into_iter().next().unwrap_or_default(),
            _ => format!("({})", parts.join(sep)),
        }
    }
}

/// Compiles the condition and the update of one operation. Placeholders are shared, so `names` and `values` serve both expressions.
pub fn compile(cond: Option<&Cond>, set: &Item, remove: &[String]) -> Compiled {
    let mut p = Placeholders {
        names: BTreeMap::new(),
        by_attr: BTreeMap::new(),
        values: BTreeMap::new(),
    };
    let condition = cond.map(|c| p.cond(c));
    let update = if set.is_empty() && remove.is_empty() {
        None
    } else {
        let mut parts = Vec::new();
        if !set.is_empty() {
            let sets: Vec<String> = set
                .iter()
                .map(|(k, v)| {
                    let n = p.name(k);
                    let val = p.value(v);
                    format!("{n} = {val}")
                })
                .collect();
            parts.push(format!("SET {}", sets.join(", ")));
        }
        if !remove.is_empty() {
            let names: Vec<String> = remove.iter().map(|a| p.name(a)).collect();
            parts.push(format!("REMOVE {}", names.join(", ")));
        }
        Some(parts.join(" "))
    };
    Compiled {
        condition,
        update,
        names: p.names,
        values: p.values,
    }
}
