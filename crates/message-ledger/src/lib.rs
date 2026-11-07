//! The message ledger: the rules that move a message of the user through its states, written once and used by
//! control-api (admit, withdraw, list), worker-api (session, claim, seal, outcome, the hand-out) and run-session (close at finalize).
//!
//! The ledger works on a [`table::Table`]. [`dynamo::DynamoTable`] is the real one; [`mem::MemTable`] is the tests' copy, which judges
//! the same conditions and refuses what DynamoDB refuses. The wire and item types are in `harness_protocol::messages`.

pub mod dynamo;
pub mod error;
pub mod ledger;
pub mod mem;
pub mod table;

pub use error::{LedgerError, Refusal};
pub use ledger::{
    Admitted, Clock, CloseReason, Closed, Ledger, ManualClock, Now, SystemClock, rfc3339,
};
pub use table::{Cond, Item, Table, TableError, WriteOp};
