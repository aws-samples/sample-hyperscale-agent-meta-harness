# crates/agentd/src/direct/transcript

## Purpose

- The module stores the conversation between agentd and the engine as numbered, write-once, redacted, gzip chunk objects in the run's S3 bucket. It also posts one capability record per engine session as the agent event `session.capabilities`.
- It is a `DirectHook` module. It plugs into the seam in `crates/agentd/src/direct/hooks.rs`, and the driver is not edited for it.

## Ownership

- Owned by `crates/agentd/src/direct/AGENTS.md`, which owns the seam and the module table. The directory has one owner.
- The module edits this directory.
- `crates/agentd/tests/direct_transcript*.rs` contains the module's tests.
- `crates/agentd/tests/direct_transcript_support/` contains their support code.
- `crates/agentd/tests/golden_capability/` contains the golden capability records.
- `crates/harness-protocol/src/transcript.rs` holds the wire types.
- `crates/harness-core/src/redact.rs` holds the shared redactor.
- `crates/dispatcher/src/items.rs` sets the Agent item's `features`.
- `crates/worker-api` includes `record_turn_transcript`, which stores the turn's transcript summary.
- `flate2` (gzip) is a dependency in the workspace `Cargo.toml` and `crates/agentd/Cargo.toml`.

## Local Contracts

- `runs/<run>/agents/direct/turns/1/transcript/{seq:012}.ndjson.gz` is the object key shape. `harness_protocol::direct::s3::transcript_prefix` builds the transcript prefix. `transcript_chunk_key` builds the chunk key. Each object is written once with `If-None-Match: *`. Answer 412 means the key is taken. Never overwrite that object.
- `s3store.rs`, the S3 implementation, provides the module's own SigV4 client. It uses crate `aws-sigv4`. S3 is the only store. agentd has no AWS SDK.

- `{v, seq, incarnation, firstRec, lastRec, createdAt, engine, protocol}` is the header. Each object is a gzip stream. Line 1 contains that header. The last object of a clean end adds `closed: true`. Each following line contains a record shaped `{n, t, dir, kind, data}`. `ham_core::transcript`, the transcript reader, defines the matching shapes through `decode_chunk`. The version is 1. The header's `seq` equals the key's number. `incarnation`, the writer's instance identifier, is a ULID made once per writer. `createdAt` and `t` use UTC timestamps. Labels contain at most 32 characters. Record numbers `n` increase within the header's range.
- Each object holds at most 1 MiB compressed. Its body holds at most 1 MiB before compression. The writer cuts a batch at 900 KiB. Each object holds at most 10,000 records. The reader inflates at most 4 MiB.

- Every wire-message record has `turn_id`, its prompt id, beside `n`, `t`, `dir`, `kind` and `data`. It preserves the open `session/prompt` request's JSON-RPC id for that record's session. The id stays a number or string. Without an open prompt, it is `null`. Records without a session use the latest open prompt. The prompt record names itself. Its answer carries the same id. Messages keep their starting prompt even when written after its answer. An `initialize` request for a new engine process closes all open prompts. agentd's own `gap` and `end` records omit `turn_id`. `ham_core::transcript::decode_chunk`, the reader, ignores unknown members. The protocol version stays unchanged.
- Records start at 0 within each `incarnation`. Objects take `seq` from the highest valid object plus one. The search covers the transcript prefix. A restarted agentd creates a new `incarnation`. It resumes from the highest valid header. Its first record is `gap`, a record describing missing data. That record's data is `{kind: "tail-unknown", afterSeq, reason}`. Skip this opening gap when the previous object is `closed`.
- A taken key is a conflict unless this writer wrote it earlier. After a conflict, the writer lists keys after that key. It takes the first free number after the taken-key run. Many taken keys therefore cost one refused put. A put with a lost answer is found through its header. That object counts as stored.

- Every engine-connection JSON-RPC message is recorded in both directions. Records use the message's update word. `session`, the session reply record, includes the initialize result. It covers replies from `session/new` or `session/load`. Other names include `prompt`, `prompt_result`, `tool_call`, `agent_message_chunk`, and `usage_update`. Cancel and permission records are also stored. `end`, the clean-end record, and `gap` are stored. `opaque`, the unknown-message record, holds the name and size. Its payload is cut at 4 KiB. Message text fragments are coalesced. They are written at 8 KiB or 2 s. They are also written when another message starts. The end also writes remaining fragments.
- Thoughts are never stored. Replayed history during session loading is never stored. Messages from an abandoned session are never stored. An unchanged configuration update is never stored. A digest decides whether that configuration changed. Binary output is replaced by its size. Tool output from reading a credential file becomes `[file contents withheld]`.

- `harness_core::redact`, the redaction rules, processes every string and property name. This happens before a record exists. The rules run in linear time. `Stream`, the streamed-text redactor, holds back a window. Its default window is 4 KiB. Secrets split across two fragments never reach a record. A hold beyond `max_hold` is forced out. `on_startup`, the startup hook, supplies the run's session secrets. These are the secret access key and session token. The secrets are refreshed every 60 s.
- Payloads larger than 1 MiB are cut before redaction. The rules therefore see bounded text. After redaction, payloads are cut to the 256 KiB record cap. This cut keeps the head and tail. It adds a `_truncated {originalBytes, sha256}` member. If redaction cannot run, replace the payload. Never store that payload raw.

- The reader refuses a whole chunk when one record's payload nests deeper than 32 levels (`ham_core::transcript::MAX_RECORD_DEPTH`; the root is depth 0 and a scalar at depth 32 is allowed). `finish::limit_depth` therefore replaces every non-empty array or object at depth 32 by the text `[nested too deeply: <n> bytes left out]` before redaction and counts the record as `truncated`. The engine's JSON can be 128 levels deep, and a stored object can never be rewritten.

- `on_wire`, the wire-message hook, only copies into a bounded queue. It then returns. The queue and objects awaiting upload share the `queue_bytes` limit. That limit defaults to 32 MiB. Above it, tool-call bodies are dropped first. Their records remain. Next, a whole message is dropped. Next, the oldest waiting objects are dropped. Every drop is counted and named in a `gap` record.
- Uploads are tried up to 6 times. Retry pauses double from 200 ms. A batch that cannot be stored becomes a gap. The next object names that gap. Module failures never fail the run. A store that always fails leaves the run as it was. The summary reports the losses.

- Batches are written at 256 KiB. They are also written 30 s after their first record. A prompt answer writes the batch. A cancel through `Event::Cancel`, the cancel event, writes the batch. `on_shutdown`, the shutdown hook, also writes the batch.
- `on_finalize`, the finalization hook, performs the final flush. That flush takes at most 10 s. It never exceeds 12 s. The driver gives the hook 15 s. The last clean-end object holds an `end` record. Its header says `closed`.

- `TurnCompleteRequest.transcript`, the turn-completion transcript field, is set by `on_finalize`. Its value is `TranscriptSummary {lastTranscriptSeq, entries}`. Entries have the shape `CompletenessEntry {kind, fromSeq, toSeq, count}`. The kinds are `excluded`, `truncated`, `shed`, `suppressed`, `failed`, and `conflict`. They also include `unsupported` and `tail-unknown`. Wire names use kebab-case. The reader declares `unsupported` for an older image. The summary holds at most 256 entries.
- `is_lossless`, the lossless-summary check, means valid with only `excluded` entries. worker-api stores the summary through `record_turn_transcript`. Summary storage never fails turn-complete.

- `Event::SessionStarted`, the session-start event, triggers one `session.capabilities` event per session. `capability::build` builds the capability record. `capability::event_value` supplies its event value. The record uses ACP's own words from the initialize result. It also uses the session response. It includes agent name and version. It includes prompt and session capabilities. It includes modes, commands, and config options with plain values only. `HarnessCapabilities::this_build()` supplies the harness's own lists. Those lists include controls `message.next` and `message.now`. They include feature `transcript.v1`.
- Every capability list and string is capped. Strings that look like secrets are redacted. `SessionCapabilitiesPayload::bounded` caps the event at 4,096 bytes. It cuts commands first. It cuts option values next. It cuts session capabilities last. The dispatcher registers the same lists on the Agent item. They are stored in `AgentItem.features`.

- `AGENTD_TRANSCRIPT_*` environment variables supply settings through `settings.rs`, the settings reader. Do not edit `config.rs`. `AGENTD_TRANSCRIPT=off` switches the module off. The run then behaves as before the module. The module is also off without a run bucket. `_FLUSH_MS` ranges from 100 to 30,000. Its default is 30,000. `_FLUSH_BYTES` ranges from 1 KiB to 512 KiB. Its default is 256 KiB. Settings also include `_COALESCE_BYTES` and `_COALESCE_MS`. `_RECORD_CAP_BYTES` ranges from 1 KiB to 440 KiB. Its default is 256 KiB. `_WINDOW_BYTES` ranges from 256 B to 64 KiB. Its default is 4 KiB. Settings also include `_MAX_HOLD_BYTES`.
- `_QUEUE_BYTES` ranges from 1 MiB to 256 MiB. Its default is 32 MiB. `_FINAL_MS` ranges from 100 to 12,000. Its default is 10,000. `_UPLOAD_TRIES` ranges from 1 to 20. Its default is 6. Settings also include `_RETRY_MS`. Values that fail to parse keep their defaults. Out-of-range values are held to their bounds.

- `crate::engine::direct::engine_label` supplies the engine's display label. The module names no engine.
- `crates/agentd/tests/engine_neutral.rs` scans this directory for engine product names.

- `mod.rs` implements `DirectHook`. `pipeline.rs` turns messages into numbered record lines. `finish.rs` handles credential files and binary output. It also handles redaction and the record cap. `chunk.rs` handles the gzip object and its header. `writer.rs` handles processor and uploader tasks. It also handles numbers, the final flush, and the summary.
- `store.rs` defines `ChunkStore`, the chunk storage interface. Its methods are `list`, `head`, and `put_new`. `s3store.rs` implements S3 storage. `settings.rs` reads settings.

- Every file here feeds the worker image key. A branch editing any file here moves that key.

## Work Guidance

- Write the failing test first. Keep the red output.
- Writer tests use an in-memory `ChunkStore`. Use a paused clock through `tokio::test(start_paused = true)`.
- Whole-run tests use the fake bucket in `crates/agentd/tests/direct_support/`.
- Build secret-shaped values at run time as in-memory data. Never put exploit-looking text on a command line.
- Changing a recorded event or field requires re-recording 13 golden streams. Use `GOLDEN_UPDATE=1 cargo nextest run -p agentd --test direct_golden`. Give the reason in the commit message.
- Chunk-format changes must remain readable by `ham_core::transcript::decode_chunk`.
- `HAMH_TRANSCRIPT_DUMP_DIR=<dir> cargo nextest run -p agentd --test direct_transcript_writer` also writes every object of those tests to `<dir>` (`<test name>-<key file name>`), so the reader of another crate can be run over what the writer made.

## Verification

- `cargo nextest run -p agentd -E 'binary(~direct_transcript)'`
- `cargo nextest run -p harness-core --test redact`
- `cargo nextest run -p harness-protocol --test wire_transcript`
- `python3 scripts/dox_lint.py`

## Child DOX Index

None.
