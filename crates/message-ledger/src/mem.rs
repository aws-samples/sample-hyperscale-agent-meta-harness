//! The in-memory table of the tests (control-api, worker-api, run-session, the `ham` stand-in stage and the scripted E2E). It is a
//! table, not a mock of the ledger: a transaction is checked as DynamoDB checks it ([`check_transaction`]), every condition is judged
//! against the stored item, and nothing is written unless every condition holds. Two writers that read the same version therefore race
//! exactly as they do on the real table: the second one is told `ConditionFailed`.

use crate::table::{Item, PK, SK, Table, TableError, WriteOp, check_transaction, item_size};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

type Items = BTreeMap<(String, String), Item>;
type Hook = Box<dyn FnOnce(&MemTable) + Send>;

#[derive(Default)]
struct State {
    items: Items,
    /// Run once, before the next transaction is judged: a competing writer that slips in between a read and the write.
    before_transact: Vec<Hook>,
    /// The next transactions fail with these errors, in order, before anything is judged.
    fail_next: Vec<TableError>,
    gets: usize,
    queries: usize,
    transactions: usize,
    committed: usize,
}

/// What a test can count.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub gets: usize,
    pub queries: usize,
    /// Transactions started, failed ones included.
    pub transactions: usize,
    /// Transactions that wrote.
    pub committed: usize,
}

#[derive(Clone, Default)]
pub struct MemTable {
    inner: Arc<Mutex<State>>,
}

impl std::fmt::Debug for MemTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemTable").finish_non_exhaustive()
    }
}

impl MemTable {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Writes an item directly (a test's arrangement), judged by nothing.
    pub fn put_raw(&self, item: Item) {
        if let (Some(Value::String(pk)), Some(Value::String(sk))) = (item.get(PK), item.get(SK)) {
            let key = (pk.clone(), sk.clone());
            self.lock().items.insert(key, item);
        }
    }

    pub fn item(&self, pk: &str, sk: &str) -> Option<Item> {
        self.lock()
            .items
            .get(&(pk.to_string(), sk.to_string()))
            .cloned()
    }

    pub fn all(&self) -> Vec<Item> {
        self.lock().items.values().cloned().collect()
    }

    pub fn stats(&self) -> Stats {
        let s = self.lock();
        Stats {
            gets: s.gets,
            queries: s.queries,
            transactions: s.transactions,
            committed: s.committed,
        }
    }

    /// Runs `f` once, just before the next transaction is judged. It may write to the table: it stands for another request that wrote
    /// after this one read.
    pub fn before_next_transact(&self, f: impl FnOnce(&MemTable) + Send + 'static) {
        self.lock().before_transact.push(Box::new(f));
    }

    /// Makes the next transaction fail with `e` before anything is judged.
    pub fn fail_next(&self, e: TableError) {
        self.lock().fail_next.push(e);
    }
}

fn apply(items: &mut Items, op: &WriteOp) {
    match op {
        WriteOp::Put { item, .. } => {
            if let Some((pk, sk)) = op.key() {
                items.insert((pk, sk), item.clone());
            }
        }
        WriteOp::Update {
            pk,
            sk,
            set,
            remove,
            ..
        } => {
            let entry = items.entry((pk.clone(), sk.clone())).or_insert_with(|| {
                let mut i = Item::new();
                i.insert(PK.into(), Value::String(pk.clone()));
                i.insert(SK.into(), Value::String(sk.clone()));
                i
            });
            for (k, v) in set {
                entry.insert(k.clone(), v.clone());
            }
            for k in remove {
                entry.remove(k);
            }
        }
    }
}

impl Table for MemTable {
    async fn get(&self, pk: &str, sk: &str) -> Result<Option<Item>, TableError> {
        let mut s = self.lock();
        s.gets += 1;
        Ok(s.items.get(&(pk.to_string(), sk.to_string())).cloned())
    }

    async fn query(&self, pk: &str, sk_prefix: &str) -> Result<Vec<Item>, TableError> {
        let mut s = self.lock();
        s.queries += 1;
        Ok(s.items
            .iter()
            .filter(|((p, k), _)| p == pk && k.starts_with(sk_prefix))
            .map(|(_, v)| v.clone())
            .collect())
    }

    async fn transact(&self, ops: Vec<WriteOp>) -> Result<(), TableError> {
        // A hook may write to this table, so it runs without the lock held.
        let hooks: Vec<Hook> = {
            let mut s = self.lock();
            s.transactions += 1;
            std::mem::take(&mut s.before_transact)
        };
        for h in hooks {
            h(self);
        }
        let mut s = self.lock();
        if !s.fail_next.is_empty() {
            return Err(s.fail_next.remove(0));
        }
        check_transaction(&ops)?;
        for (index, op) in ops.iter().enumerate() {
            let stored = op.key().and_then(|k| s.items.get(&k));
            if let Some(c) = op.cond()
                && !c.holds(stored)
            {
                return Err(TableError::ConditionFailed { index });
            }
        }
        // Every item the transaction leaves behind must fit, judged on a copy so a refusal writes nothing.
        let mut next = s.items.clone();
        for op in &ops {
            apply(&mut next, op);
        }
        for op in &ops {
            if let Some(i) = op.key().and_then(|k| next.get(&k))
                && item_size(i) > crate::table::MAX_ITEM_BYTES
            {
                return Err(TableError::Invalid(format!(
                    "item size {} has exceeded the maximum allowed size {}",
                    item_size(i),
                    crate::table::MAX_ITEM_BYTES
                )));
            }
        }
        s.items = next;
        s.committed += 1;
        Ok(())
    }
}
