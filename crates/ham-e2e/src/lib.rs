//! The local fake engine of `ham` and the tests that play it.
//!
//! [`acp`] holds the replay of recorded ACP transcripts behind the `fake-acp-engine` binary (`engine/profiles/fake-acp.toml`) and the in-process
//! drivers that test it. [`fake_cline`] is a small ACP agent with the measured behaviour of the pinned Cline engine. [`goal_sim`] plays the goal
//! plugin of that engine. [`fake_mcp`] is the fake engine's MCP client: the part of an engine that reads its tool servers from a settings file and
//! calls them, played from a transcript, for the tests of agentd's tool hook.

pub mod acp;
pub mod fake_cline;
pub mod fake_mcp;
pub mod goal_sim;
