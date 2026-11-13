//! The `run-session` Lambda of a direct run.
//!
//! One run is one Cline session in one AgentCore microVM. The Step Functions machine `hamh-direct-run`
//! (`statemachines/direct.asl.json`) sequences the work and this Lambda decides every step the machine only routes:
//!
//! | op | what it does |
//! | --- | --- |
//! | [`prepare`](ops::Op::Prepare) | reads the run and its stored request, refuses what can never run, builds the repository-gateway payloads |
//! | [`next-slice`](ops::Op::NextSlice) | decides how the slice that ended ended and whether another follows: stops its machine, plans the next slice from the best head it left |
//! | [`plan-delivery`](ops::Op::PlanDelivery) | picks the head (the final bundle, else the last snapshot), the outcome and its label; builds the validate, pull request and package payloads |
//! | [`finalize`](ops::Op::Finalize) | publishes the delivery events, moves the run to `complete`, stops the session (and deletes an Instances session's provider session) |
//! | [`mark-blocked`](ops::Op::MarkBlocked) | ends a run that delivered nothing: `blocked` with the reason |
//! | [`name-run`](ops::Op::NameRun) | asks the utility model for the 5 to 7 word name of the run and stores it on the run's search item, and, when `UTILITY_PURPOSES` lists `run-tags`, for its topic and tech tags ([`tag_run`]); the machine does not wait for it |
//! | `run-ended` (EventBridge) | clears a run whose execution ended ABORTED, FAILED or TIMED_OUT and stops its session |
//!
//! The harness never judges the work. The label of a run is agentd's: `Cline reported done` only when Cline said so,
//! else `Cline stopped: <reason>` ([`harness_protocol::direct::DirectOutcome::label`]); nothing here writes
//! "verified", "tested", "gated" or "evaluated" about the code.
//!
//! The AWS adapters in [`aws`] are the only code that calls AWS. Everything else is generic over the ports in
//! [`ports`], so the tests run the real ops over in-memory fakes and the real ASL definition over a walker.

pub mod aws;
pub mod compute;
pub mod error;
pub mod events;
pub mod finalize;
pub mod index;
pub mod lambda;
pub mod name_run;
pub mod ops;
pub mod pinned;
pub mod plan_delivery;
pub mod ports;
pub mod prepare;
pub mod run_ended;
pub mod slices;
pub mod start_session;
pub mod tag_run;
pub mod text;
pub mod utility;
pub mod wiring;

pub use error::RunSessionError;
pub use ops::{Config, Deps, Op};
