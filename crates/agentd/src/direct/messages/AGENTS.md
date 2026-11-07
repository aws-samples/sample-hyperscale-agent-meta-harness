# crates/agentd/src/direct/messages


## Purpose


- **Message control.** agentd, the message queue owner, sends each user's message separately. Each message gets its own engine prompt turn.
- **Queue owner.** agentd sends exactly ONE session/prompt at a time.
- **Engine queue.** Never use the pinned engine's internal queue.

## Ownership


- **Parent contract.** crates/agentd/src/direct/AGENTS.md owns this module's boundary.
- **Hook order.** Hooks::standard registers this module last in hooks.rs. Pending messages beat another module's continuation. An earlier module's Stop is final before any claim.
- **Edit boundary.** Edit only this directory and crates/agentd/tests/direct_messages_*.rs. The fake engine in crates/ham-e2e, src/fake_cline.rs, is also allowed.
- **Control plane.** IM2c, the control-plane helper, owns routes and DynamoDB items. It owns protocol types in crates/harness-protocol/src/messages.rs.
- **mod.rs.** Contains hook settings, state, and DirectHook implementation.
- **types.rs.** Defines Mode, Notice (hand-out without text), and Message (claimed with text). Defines the ten States, RejectReason, FailReason, Refusal, and ClaimOutcome. Also defines SealMode, SealOutcome, and StoreError.
- **store.rs.** Defines MessageStore, the store trait, and MemoryStore, the in-memory server model. Every module test uses MemoryStore, including fault injection.
- **http.rs.** Defines HttpMessageStore, the four signed internal API routes. Defines notice_of, the PendingControl::Message reader.
- **machine.rs.** Defines Queue and Budget.
- **reap.rs.** Defines Reaper, the engine process finder and killer.

## Local Contracts


1. **One prompt at a time.**
   - Send only through TurnVote::Prompt, the prompt vote, at on_turn_end or on_seal. Never claim or send while a prompt is open.
   - If the driver already sent a nudge, log "a prompt is open". on_turn_end and on_seal vote Done. A message during that nudge waits for the next turn end.
   - Give each message one turn, in order. Send `now` messages first.
   - The helper chose separate turns over joining messages into one prompt. This keeps "delivered (turn k)" exact for each message. Each claim and withdrawal concerns one message.
2. **Claim before send.**
   - on_turn_end takes the next hand-out. Report the session first if the store lacks it. Claim by id with POST /internal/message-claim.
   - The answer carries text and an attempt number. Only then return Prompt(text). Losing a claim costs nothing.
   - Loss causes include withdrawal, a hold, queued cancel, or another session. Repeat a claim whose answer was lost. The repeat finds the same claim.
   - An unanswered claim marks doubt; the plane may have applied it. An unanswered store request never holds the run. The vote is Done.
   - Before sealing and at close, resolve doubt through MessageStore::held, the held-message lookup. This claims without a message id. With no held messages, the plane claims the next pending message.
   - The plane returns every message this incarnation holds in dispatching. It returns Held { message, redelivered, current_session } with the same attempt. This also applies after the session changed.
   - Take over an unknown held message belonging to the current session. Send it before sealing; at close, fail it with not-sent. Never end that message as delivery-unknown.
   - Release a returning claim belonging to an older session. The control plane holds it until the user confirms a copy. Never resend a returning claim already voted or sent.
   - Fix the deadline at turn end; reread remaining time before dispatch. Release the claim when less than 300 s remain. Release an unsent vote when the session changes.
   - Settings.decision_cap, the decision time limit, stops searching after 90 s. The driver cuts a module off at 120 s.
   - A never-claimed rejection carries attempt 0. A rejection after release carries its last claim's attempt. Every other outcome carries its claim's exact attempt.
   - A release rejected for the closed run returns 200 on repeats. HTTP 409 stale-attempt and bad-transition are final. Never repeat them.
3. **Acknowledge on send.**
   - PromptSent, the send event, carries the claimed text. Text and session id must match the claimed message. Record submission through POST /internal/message-outcome kind submitted.
   - Set turn = the prompt number. The message becomes "delivered (turn k)".
   - The retry ladder may resend the same text. This remains the same delivery in the same turn.
   - A claimed message never sent ends failed (not-sent). Record that result at shutdown.
   - Keep unposted submitted, failed, release, and reject outcomes as owed. Retry them at the seal and the close. A rejection waits behind the same message's release.
4. **Seal.**
   - on_seal atomically checks pending messages and closes an empty run. Use POST /internal/message-seal, mode normal.
   - Pending messages return as the next prompt. No pending messages means admission closes.
   - Use limit-reached when the continuation allowance is spent. Use terminal when less than 300 s remain.
   - Before asking, wait for interrupt claims and acknowledgements in flight. Post every owed outcome first. Resolve claim doubt before asking.
   - Normal or limit-reached seals return 409 bad-transition while messages dispatch. They also refuse while agentd owes a claimed message's outcome. This includes user cancel; response text names the message ids.
   - A refused seal ends the vote with Done. The close settles the message. The terminal seal does not wait and changes dispatching messages to delivery-unknown.
   - Pending messages may return but remain unsendable twice in a row. The third seal is terminal. Those messages end rejected as run-sealed.
   - on_shutdown and on_finalize use an idempotent terminal seal. Later messages receive 409 run-sealed. They store nothing.
   - The close has a 12 s budget. Each call makes one bounded attempt. First, stop the engine's processes within 2 s.
   - Then wait up to 3 s for an interrupt claim. Allow acknowledgements in flight 2 s to reach the store.
   - Resolve doubt and fail claimed messages never sent with not-sent. Retry every owed outcome. Finally, attempt the terminal seal up to two times.
   - Once close starts, no interrupt begins. Leave returning claims for the close. After user cancel, close asks only for the terminal seal.
5. **Epoch.**
   - The epoch is the ACP session id from Event::SessionStarted. Report it through POST /internal/message-session before any claim.
   - A session report carries its number. Repeat that number unchanged on a retry.
   - Older-session messages leave the queue until the user confirms a copy. The control plane holds them. A model change creates no session and holds no messages.
6. **Continuation.**
   - Governs this allowance. It starts at the first done-looking stop, end_turn.
   - Allow at most 8 message-driven turns and 1,800 s extra execution. Keep 300 s before the run's deadline. That deadline already includes the 900 s delivery reserve.
   - Check the 1,800 s limit before starting another turn. Once spent, start no new turn. Do not cut short the turn in flight.
   - Messages beyond the allowance end rejected (continuation-limit). Deliveries before the first done-looking stop do not count.
   - Charge an unanswered message turn when its session ends. Also charge it when another prompt replaces it. allowance(), the allowance report, reports turns and time.
   - Another stop or model change does not reset the allowance. Neither does an interrupt or an answer. Tests: `a_model_change_an_answer_and_a_new_session_leave_the_allowance_where_it_was` (`direct_messages_hook.rs`: after 3 turns a model switch, an answer and a new session change nothing and the ninth message is rejected for the limit), `interrupts_count_against_the_allowance_and_the_limit_still_holds_after_them` and the turn counts of the sixth-interrupt test (`direct_messages_e2e.rs`).
7. **/now.**
   - Aim the interrupt at the prompt number open when requested. If another prompt is open when the claim returns, write no cancel. Send the message at the next turn end.
   - A stage ending on its own clears a pending interrupt. Send one session/cancel. The client waits for the prompt's own answer.
   - Measured waits were 0.19 to 0.48 s. At stage end, on_turn_end checks TurnEnd.interrupted.
   - Reap the engine's tool processes, then vote the new message prompt. Cancel kills the shell but leaves processes the shell started.
   - Serve interrupts one at a time. Allow at most 5 per run. An active interrupt or quarantined session prevents another interrupt.
   - An unsafe cancel or no open prompt also prevents it. Send that `now` message at the next turn end.
   - The allowance is checked before anything else. When it is spent, a `now` message is refused at once with `continuation-limit`, without a claim and without a `session/cancel`: the turn that is running keeps its work, the engine is not resumed with the "interrupted" sentence, and the user gets no promise of delivery (the row shows the refusal). Tests: `a_now_message_beyond_the_allowance_is_refused_and_the_running_turn_is_not_cancelled` (`direct_messages_e2e.rs`), `a_now_message_beyond_the_allowance_is_refused_at_once_and_interrupts_nothing` and `the_allowance_decides_before_the_other_reasons_a_now_message_cannot_interrupt` (`direct_messages_hook.rs`). The time left is not known at that moment, so a message that comes too late is still refused at the turn end.
   - Every other `now` message that cannot interrupt (the 5 interrupts are spent, the session is quarantined, the cancel is unsafe, or an interrupt is under way) writes no `session/cancel`, becomes a `next` message at the head of the queue, and gets the user notice below. The log line names the reason. Test: `a_sixth_interrupt_is_not_made_the_message_goes_out_after_the_turn_and_the_user_is_told` (`direct_messages_e2e.rs`: five cancels and no sixth, delivered as prompt 8, one notice, no needs-input question).
   - CANCEL_GRACE, the cancel grace period, is 10 s. Missing acknowledgement or surviving engine processes demotes `now`, an interrupting message, to `next`, delivery after this turn, retaining its claim. agentd writes the user notice (`turn.progress`, kind `notify`, reason `message-not-interrupted`) with detail "could not interrupt; delivered after this turn".
   - Kill covers the engine and every process of its family. New prompts remain blocked until the driver starts a fresh session. The message goes first after the restart turn, once without confirmation; no needs-input question opens, and the interrupt counter remains unchanged at 5 per run.
8. **User cancel stays final.**
   - Once the user's cancel is up, claim nothing. Close with run-cancelled. A Cancel beats messages in the same batch.
9. **Tripwire.**
   - Detect end_turn with usage 0 and no session/update since the prompt. The answer must arrive within 2 s. This indicates the engine's own queue.
   - Never call that result a done label from the engine. Mark the session cancel-unsafe.
   - Kill the engine and its family. Vote the resume sentence so the driver starts a fresh session.
10. **Kill path.**
    - Tool processes survive killing the engine's process group. A shell tool runs in its own session. A command that forks and exits leaves its child to init.
    - on_startup sets AGENTD_ENGINE_TAG=<random>, the engine process tag, in the engine's environment. Reaper finds every process carrying that tag through /proc.
    - Reaper reports unverified when /proc cannot be read. Accept an empty family only after two consecutive empty scans.
    - The container runtime is out of every scan and reap. Its front runs as the engine's user and Podman's service as the namespace's root, which is that user on the host, so the sweep would stop both when the engine ends and the test report would find a socket nobody listens on. `Reaper::protect_pid` marks a process by pid and start time; `on_startup` marks the pids in `StartupCtx::runtime_pids` (named by `ham-containers up` before the engine exists), and a scan leaves a marked process and everything below it, whatever other mark it carries. A marked process that is gone, or whose pid another process holds, protects nothing. The log line of the engine-stop reap names what it stopped; a process name is text the engine's tools choose, so the line writes every character that is not plain printable ASCII as an escape (`reap::killed_list`). Tests: `crates/agentd/tests/b765_runtime_reap.rs`, `crates/agentd/tests/b765_runtime_pids.rs`.
    - AGENTD_ENGINE_USER, the engine user setting, controls the microVM user-wide sweep. sweep_uid, the user-wide sweep, requires both users to be known. agentd's user and the engine's user must differ.
    - In the landlock mode (`StartupCtx.sandbox` is set, `Reaper::for_run`) the engine has no user of its own, and a tool that runs with an empty environment (`env -i program`) and is left to init escapes the tag. Two more marks count then. A process with more seccomp filters than agentd: the launcher installs one filter on the engine, the kernel keeps it through fork, setsid, a double fork and exec, and `Seccomp_filters` of `/proc/<pid>/status` (Linux 5.9 and later) counts it; agentd's own helpers have agentd's count. And a child of any process of the family, which finds a tool where the kernel reports no count. Both read the machine and not the run: right where one run owns the machine (the Instances machine), wrong beside another sandboxed run. So they are off for the uid mode, for a run with no sandbox and for tests that do not ask. agentd is not made a subreaper: an adopted orphan stays a zombie after the kill, because the runtime owns the waiting for agentd's children and a `waitpid` of agentd's own would take their exit status.
    - Sandbox marks read the seccomp filter count and parent chain from `/proc`. Processes with more filters than agentd count as engine processes regardless of user; filtered host services of other users cannot be stopped and keep `Report::remaining` nonempty. `real_processes::a_tool_that_scrubs_its_environment_and_is_left_to_init_is_gone_after_the_reap` in `crates/agentd/tests/direct_messages_reap_sandboxed.rs` reruns itself inside `unshare --user --map-root-user --pid --fork --mount-proc` and asserts that it is process 1 and `/proc` lists only its own processes; the reap cannot see or stop host processes there. Missing or refused `unshare` leaves the test on the machine's `/proc` and prints `NOT ISOLATED`. The `seccomp-children` group in `.config/nextest.toml` runs every `sandbox_*` test binary and `direct_messages_reap_sandboxed` one at a time.
    - mark_engine, the engine identity marker, records its pid and start time. Mark that identity when a session starts. Keep the engine by that identity.
    - Being agentd's child does not identify the engine. Orphaned tools reparent to agentd when agentd is the machine's init.
    - Never list zombies or agentd. Repeat scans and kills until a scan is empty. Bound the repeat.
    - Reap after an acknowledged interrupt; keep the engine. After an unacknowledged interrupt or tripwire, kill the engine too.
    - Unverified removal makes on_turn_end vote Stop ("could not be stopped"). Stop before sending another prompt beside those processes. Confirmed removal allows the resume sentence as before.
    - Reap on every EngineStopped event and at shutdown. Finish before the end commit and the bundle.
11. **Fencing.**
    - Every agentd process makes a new generation through http::new_generation(). Its ULID is later than any this process made before.
    - The plane replaces an older incarnation with an unseen generation. It refuses one more than 60 s older under GENERATION_CLOCK_SKEW_MS. A replaced incarnation receives 409 stale-generation on every route.
    - HTTP 409 stale-generation or stale-epoch makes the store unavailable(). The module makes no further calls. The server model also reports replaced worker generations as unavailable().
    - HTTP 404 on a message route means no plane message control. This also marks the store unavailable. The module stays out of the run; the run remains unharmed.
12. **Text limits.**
    - No harness text says a run is verified. Logs name ids and sizes. Never log message text.

## Work Guidance


- **Test first.** Write the failing test, then save its red output. Write the code afterward.
- **Fake plane.** Use MemoryStore for the fake plane. Plane, the in-process test control plane, lives in tests/direct_messages_world/mod.rs. It refuses what the real routes refuse.
- **Plane rules.** Enforce generation rules, claims without ids, and attempts. Enforce sealing rules while dispatching and the 409 codes.
- **Fake engine.** FAKE_ACP_CLINE=1 selects the fake engine. Put @fake{...} directives in prompt text. See crates/ham-e2e/src/fake_cline.rs.
- **Scenario checks.** Assert in the engine's log that prompts never overlapped. Assert that each scenario's trigger was present.
- **Process tests.** Spawn only harmless commands: sleep, setsid, the shell's own & and exec. Read pids by the run's tag or the spawn call. Ban exploit-looking command-line text.
- **Fake tools.** Each tool is plain `sleep`; record the spawn call's pid. An orphan comes from a short-lived shell holding only `sleep <n> &`. Find the orphan's pid by its unique command line. The fake also streams message chunks (`stream`) and writes a file in parts (`edit`); a cancel stops both at once. The `now` end-to-end tests interrupt a shell tool, a model stream and a file edit, and the next turn continues after each.
- **Open-prompt tests.** MessagesHook::pretend_prompt_open(true), the open-prompt test setting, needs no driver. It answers yes to the next "is a prompt open" question.
- **Golden records.** Record changes require re-recording tests/direct_golden.rs golden files. Put those changes in their own commit.
- **Module switch.** AGENTD_MESSAGES=off turns the module off.
- **Turn setting.** AGENTD_MESSAGES_MAX_TURNS defaults to 8.
- **Execution setting.** AGENTD_MESSAGES_MAX_EXEC_S defaults to 1800.
- **Deadline setting.** AGENTD_MESSAGES_MIN_LEFT_S defaults to 300.
- **Interrupt setting.** AGENTD_MESSAGES_INTERRUPTS defaults to 5.
- **Grace setting.** AGENTD_MESSAGES_QUIESCE_S defaults to 10.
- **Queue setting.** AGENTD_MESSAGES_QUEUE defaults to 16 hand-outs kept.
- **Nudge gap.** Today the driver sends its nudge before asking the module. A nudge can therefore precede a pending message. Forbids nudges while messages are pending.
- **Nudge proposal.** Add synchronous DirectHook::before_nudge(&self) -> bool, a nudge-skip check. True means skip the nudge. Ask inside send_and_nudge before each nudge.
- **Wanted order.** a_pending_message_goes_before_the_harness_nudge, the ignored nudge-order test, states the wanted order. It lives in tests/direct_messages_e2e.rs.
- **Settle proposal.** After an interrupted stage, drain the engine's pipe until quiet. Follow settle_after_cancel, the silence-settling operation. This keeps late cancelled-turn updates out of the new turn.
- **Restart proposal.** Today the module kills the engine itself. The driver discovers the closed pipe. TurnVote::FreshSession would request the restart directly.

## Verification


- **Contract source.** Promises come from IM2c's control-plane contract, crates/message-ledger/AGENTS.md, on branch.
- **Offline twins.** `crates/agentd/tests/direct_messages_e2e.rs` uses a fake engine copying pinned-engine measurements. `direct_messages_wire.rs` checks the same through the four routes.
- **Hardening checks.** Checks include crates/agentd/tests/direct_messages_hardening.rs, direct_messages_http.rs, direct_messages_unit.rs and direct_messages_reap.rs.
- **Changed contracts.** Changes are verified by crates/agentd/tests/direct_messages_contract.rs, direct_messages_hardening.rs, direct_messages_unit.rs and direct_messages_http.rs.
- **Live checks.** Run the chat client's scenario battery after deploy.
- **Module tests.**
  ```sh
  cargo nextest run -p agentd --test direct_messages_unit --test direct_messages_hook --test direct_messages_http --test direct_messages_reap --test direct_messages_reap_sandboxed --test direct_messages_fake_engine --test direct_messages_e2e --test direct_messages_wire --test direct_messages_hardening --test direct_messages_contract
  ```
- **DOX check.**
  ```sh
  python3 scripts/dox_lint.py
  ```

## Child DOX Index


None.
