//! The `ham` thin client: SigV4-signed `/v1` calls over hyper and rustls, the argv
//! start form, `ham watch`, `ham pull`, `ham clone`, `ham push`, `cancel`, `pr`, `runs`, `link` and `doctor`, and the Ratatui TUI. State, reducers, the parser, the
//! download plan and renderings live in `ham-core`; this crate adds the terminal, network and AWS
//! credential code and holds no agent logic (invariant 5). `rules_cmd` is `ham rules` and the reading of the user's rules
//! directory for a direct run, and `config_cmd` is `ham config` and `ham models` (the config file and the model list; the model
//! choice is `ham_core::models`). `shell` and `ws` hold the AgentCore command shell's framing, endpoint rule and constants.

/// The terminal library the TUI reads events with, re-exported so that a test of the TUI reads the bytes its driver sends
/// through the very crate and features `ham` uses, without a second dependency edge.
pub use crossterm;

pub mod api;
mod api_multi;
pub mod awscli;
pub mod awsprofile;
pub mod backends;
pub mod chat_routes;
pub mod cli;
pub mod clone;
pub mod commands;
pub mod config_cmd;
pub mod doctor;
pub mod engine_cmd;
pub mod flow;
pub mod http;
pub mod https;
pub mod lifecycle_cmd;
pub mod link;
pub mod message_routes;
pub mod open;
pub mod output;
pub mod pull;
pub mod push;
pub mod report;
pub mod rules_cmd;
pub mod shell;
pub mod sigv4;
pub mod snapshot_cmd;
pub mod start;
pub mod tty;
pub mod tui;
pub mod version;
pub mod watch;
pub mod ws;
