//! Module `transcript`: the conversation as the engine connection carried it, stored as
//! write-once, redacted, numbered chunk objects in the run's bucket
//! (`runs/<run>/agents/<agent>/turns/1/transcript/{seq:012}.ndjson.gz`, written with `If-None-Match: *`), and one capability event per
//! session (`session.capabilities`).
//!
//! What it does, by hook:
//! - `on_startup`: gives the redaction the session secrets, before the engine runs (and a task keeps them current as the credentials
//!   rotate). It never fails the run.
//! - `on_wire`: copies every message of the connection, in both directions, into the writer's bounded queue and returns; it also keeps
//!   the `initialize` result and the advertised commands for the capability record.
//! - `on_event`: `SessionStarted` posts the capability record (ACP's words, at most 4,096 bytes, the harness's own lists); `Cancel`
//!   writes what is pending.
//! - `on_shutdown`: the engine is gone, so the wire is complete; writes what is pending.
//! - `on_finalize`: the final flush, bounded by a deadline inside the 15 s the driver allows; sets `transcript` (the last stored
//!   chunk number and what was left out, cut or lost) on the `turn-complete` request. A store that fails changes nothing about the run.
//!
//! The parts: [`pipeline`] (messages to numbered record lines: coalescing, left-out kinds, caps), [`finish`] (credential files, binary
//! output, redaction, the record cap), [`chunk`] (the gzip object and its header), [`writer`] (batches, uploads, numbers, the final
//! flush, the summary), [`store`] and [`s3store`] (where objects go), [`capability`] and [`settings`]
//! (`AGENTD_TRANSCRIPT_*`). The module names no engine: the label comes from `crate::engine::direct::engine_label`.

pub mod capability;
pub mod chunk;
pub mod finish;
pub mod pipeline;
pub mod s3store;
pub mod settings;
pub mod store;
pub mod writer;

use crate::BoxFuture;
use crate::creds::CredStore;
use crate::direct::hooks::{DirectHook, Event, Finalize, HookEnv, Shutdown, StartupCtx, Wire};
use crate::direct::transcript::s3store::S3Store;
use crate::direct::transcript::settings::Settings;
use crate::direct::transcript::store::ChunkStore;
use crate::direct::transcript::writer::{Writer, WriterConfig};
use crate::engine::direct::engine_label;
use crate::events::EventRecorder;
use crate::logging::LogSink;
use crate::time::new_ulid;
use harness_protocol::envelope::EventType;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;

/// How long the session secrets are given a chance to be read at startup, so a refresh in progress never delays the engine.
const SECRETS_WAIT: Duration = Duration::from_secs(2);
/// How often the secrets are read again: the credentials rotate about every 50 minutes.
const SECRETS_EVERY: Duration = Duration::from_secs(60);

/// The module, or `None` when its settings switch it off or the run has no bucket. `Hooks::standard` calls it once per run.
pub fn hook(env: &HookEnv) -> Option<Arc<dyn DirectHook>> {
    hook_with_settings(env, Settings::from_env())
}

/// The module with these settings, or `None` when they switch it off (the run is then as it was before the module).
pub fn hook_with_settings(env: &HookEnv, settings: Settings) -> Option<Arc<dyn DirectHook>> {
    if !settings.enabled || env.bucket.is_empty() {
        return None;
    }
    tokio::runtime::Handle::try_current().ok()?;
    let store = Arc::new(S3Store::new(
        env.creds.clone(),
        env.region.clone(),
        env.s3_endpoint.clone(),
        env.bucket.clone(),
    ));
    Some(hook_with(env, settings, store))
}

/// The module over another store (tests).
pub fn hook_with(
    env: &HookEnv,
    settings: Settings,
    store: Arc<dyn ChunkStore>,
) -> Arc<TranscriptHook> {
    let writer = Arc::new(Writer::start(
        WriterConfig {
            run_id: env.run_id.clone(),
            engine: engine_label(&env.engine).to_string(),
            protocol: harness_protocol::PROTOCOL_VERSION.to_string(),
            incarnation: new_ulid(),
            settings,
            secrets: Vec::new(),
        },
        store,
    ));
    Arc::new(TranscriptHook {
        writer,
        creds: env.creds.clone(),
        rec: env.rec.clone(),
        log: env.log.clone(),
        engine_label: engine_label(&env.engine).to_string(),
        handshake: Mutex::new(Handshake::default()),
        keeper: Mutex::new(None),
    })
}

/// What the capability record needs from the wire before the session starts.
#[derive(Default)]
struct Handshake {
    initialize: Option<Value>,
    commands: Vec<Value>,
}

/// The transcript module of one run.
pub struct TranscriptHook {
    writer: Arc<Writer>,
    creds: Arc<CredStore>,
    rec: Arc<EventRecorder>,
    log: Arc<dyn LogSink>,
    engine_label: String,
    handshake: Mutex<Handshake>,
    /// The task that keeps the session secrets current.
    keeper: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for TranscriptHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TranscriptHook")
            .field("writer", &self.writer)
            .finish_non_exhaustive()
    }
}

impl TranscriptHook {
    /// The writer, for tests.
    pub fn writer(&self) -> &Writer {
        &self.writer
    }

    /// Keeps what the capability record needs: the `initialize` result and the commands the engine advertises.
    fn note_handshake(&self, wire: &Wire<'_>) {
        if wire.dir != crate::acp::Dir::FromEngine || wire.stale {
            return;
        }
        let m = wire.msg;
        let init = m
            .get("result")
            .filter(|r| r.get("agentCapabilities").is_some() || r.get("agentInfo").is_some());
        let commands = (m.get("method").and_then(Value::as_str) == Some("session/update"))
            .then(|| m.pointer("/params/update"))
            .flatten()
            .filter(|u| {
                u.get("sessionUpdate").and_then(Value::as_str) == Some("available_commands_update")
            })
            .and_then(|u| u.get("availableCommands"))
            .and_then(Value::as_array);
        if init.is_none() && commands.is_none() {
            return;
        }
        let mut h = self.handshake.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(i) = init {
            h.initialize = Some(i.clone());
        }
        if let Some(c) = commands {
            h.commands.clone_from(c);
        }
    }

    async fn read_secrets(creds: &CredStore, writer: &Writer) {
        if let Ok(Ok(c)) = tokio::time::timeout(SECRETS_WAIT, creds.current()).await {
            writer.add_secrets(&[c.secret_access_key, c.session_token]);
        }
    }
}

impl Drop for TranscriptHook {
    fn drop(&mut self) {
        if let Some(k) = self.keeper.lock().unwrap_or_else(|p| p.into_inner()).take() {
            k.abort();
        }
    }
}

impl DirectHook for TranscriptHook {
    fn name(&self) -> &'static str {
        "transcript"
    }

    fn on_startup<'a, 'b: 'a>(
        &'a self,
        ctx: &'a mut StartupCtx<'b>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            // The writer hides every value of the run's own secret set from here on, whenever the value was minted.
            self.writer.follow_secrets(ctx.env.secrets.clone());
            // The secrets are known before the engine can print one; the task keeps them current when the credentials rotate.
            Self::read_secrets(&self.creds, &self.writer).await;
            let creds = self.creds.clone();
            let writer = self.writer.clone();
            let task = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(SECRETS_EVERY).await;
                    Self::read_secrets(&creds, &writer).await;
                }
            });
            *self.keeper.lock().unwrap_or_else(|p| p.into_inner()) = Some(task);
            Ok(())
        })
    }

    fn on_wire(&self, wire: &Wire<'_>) {
        self.note_handshake(wire);
        self.writer.push(wire.dir, wire.msg, wire.stale);
    }

    fn on_event(&self, event: &Event<'_>) {
        match event {
            Event::SessionStarted(s) => {
                let (init, commands) = {
                    let h = self.handshake.lock().unwrap_or_else(|p| p.into_inner());
                    (h.initialize.clone(), h.commands.clone())
                };
                let record = capability::build(
                    &self.engine_label,
                    s.number,
                    Some(s.session_id),
                    init.as_ref(),
                    s.response,
                    &commands,
                );
                self.rec.record(
                    EventType::SessionCapabilities,
                    capability::event_value(record),
                );
            }
            Event::Cancel { .. } => self.writer.flush_now(),
            _ => {}
        }
    }

    fn on_shutdown<'a>(&'a self, _end: &'a Shutdown<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            // The engine is gone: the wire is complete. The final flush follows in `on_finalize`.
            self.writer.flush_now();
        })
    }

    fn on_finalize<'a>(&'a self, fin: Finalize<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let summary = self.writer.finish().await;
            let outside = self.writer.stats().unattributed;
            self.log.line(&format!(
                "direct transcript: last chunk {} entries {}{}",
                summary
                    .last_transcript_seq
                    .map_or_else(|| "none".to_string(), |s| s.to_string()),
                summary.entries.len(),
                if outside > 0 {
                    format!(" unattributed {outside}")
                } else {
                    String::new()
                }
            ));
            fin.complete.transcript = Some(summary);
        })
    }
}
