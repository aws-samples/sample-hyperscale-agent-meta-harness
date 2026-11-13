//! `ham` client state: reducers over control-plane events and the command parser.
//!
//! Renderer-free: no terminal, network or AWS code lives here. The crate holds no agent
//! logic and no local state of its own beyond the user's own files and the artifacts it downloads: the client file `config.toml` (whose
//! keys are the rows of `settings`), the rules and skills directories with the nudge file, and one file for each engine in `engines/`
//! (`engine_file`), all under `~/.config/ham/` (invariant 5).

pub mod agents;
pub mod alert;
pub mod attachments;
pub mod backends;
pub mod chat;
pub mod command;
pub mod config;
pub mod config_dir;
pub mod config_edit;
pub mod config_write;
pub mod container_signals;
pub mod download;
pub mod editor;
pub mod engine_config_card;
pub mod engine_file;
pub mod engines;
pub mod envnames;
pub mod events_view;
pub mod exit;
pub mod failover;
pub mod followup;
pub mod goal_card;
pub mod gzip;
pub mod head;
pub mod help;
pub mod history;
pub mod input;
pub mod messages;
pub mod meters;
pub mod models;
pub mod notice;
pub mod pack;
pub mod paste;
pub mod poll;
pub mod push;
pub mod redact;
pub mod registry;
pub mod render;
pub mod report_progress;
pub mod request;
pub mod resume;
pub mod retry;
pub mod router;
pub mod rules;
pub mod run_index;
pub mod runfilter;
pub mod runname;
pub mod runsearch;
pub mod runtag;
pub mod sanitize;
pub mod search;
pub mod settings;
pub mod sharing;
pub mod skills;
pub mod skills_card;
pub mod snapshot;
pub mod state;
pub mod stored;
pub mod tables;
pub mod tagexplore;
pub mod transcript;
pub mod update;
pub mod utility;
