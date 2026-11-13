//! The goal loop the harness drives itself, for an engine whose own goal feature the harness cannot supervise from the wire
//! (`LoopKind::Harness` in `crate::engine::goal`, which names the engine and holds the measurements).
//!
//! The plugin loop rests on a goal plugin that claims the goal and queues its own follow-up prompts, and on an observer plugin that writes the rounds
//! log the module reads. An engine whose native goal command returns at once with nothing on the wire, runs its loop unseen, keeps its state only in its
//! own database and stops itself after a fixed number of rounds cannot be supervised that way. So the harness takes the loop itself, with the same
//! promise: one prompt starts a loop that goes on until the work is done, under the same stall brakes.
//!
//! How it works. Each `session/prompt` is one round, so the rounds log is not needed: at every turn end the module (`GoalHook::harness_step`)
//! makes the round's start and end from the turn and feeds them to the same pure [`Machine`](super::machine::Machine) the plugin loop uses.
//!
//! - A round that ends with the claim line ends the loop. The claim line is the last line of the model's final message, in exactly the form
//!   `GOAL COMPLETE <nonce>: <summary>`. The nonce is made for the run and told to the model only in the module's own prompts, so a line a
//!   repository file or a web page held before the run began cannot be mistaken for the model's claim (the model itself could copy the nonce; the line is its own word). The line is read only in a round that a prompt of the module
//!   started (the verification prompt or a later continue), never in the round of the user's own prompt, which cannot know the nonce.
//! - A round that does not claim is followed by one prompt of the module: the verification prompt ("Are you sure you have completed the goal?")
//!   after the first round, the continue prompt after every later one. There is no cap on rounds: every brake is a stall, as in the plugin loop.
//! - The brakes are the machine's (the same `BLOCKED:` line twice, rounds with no tool call and no change, rounds with no change) and, inside a
//!   round, the watch's (tool calls that change no file, a wait for helper agents that never return), which the module runs for every engine.
//!
//! The claim is the model's own word, like a call of the completion tool is. The label of the run is the engine's stop and the harness never says
//! the work is good (root invariant 1).

use super::rounds::RoundEnd;
use super::*;

/// The words that start the claim line.
pub const CLAIM_WORD: &str = "GOAL COMPLETE";

/// The longest summary of a claim kept.
pub const CLAIM_SUMMARY_CHARS: usize = 300;

/// The most trees remembered for the cycle rule; a run that changes its tree more often than this has not been standing still.
const SEEN_TREES_MAX: usize = 4096;

/// The most characters of a round's final message kept for the rules (the end of it: the claim line and the `BLOCKED:` line are the last).
const KEEP_TEXT_CHARS: usize = 1500;

/// A nonce for one run: 16 characters of the random part of a ULID.
pub fn new_nonce() -> String {
    let id = crate::time::new_ulid();
    id.chars().skip(10).collect()
}

/// The claim in `text`, the final message of a round: `Some(summary)` when its last non-empty line is `GOAL COMPLETE <nonce>` or
/// `GOAL COMPLETE <nonce>: <summary>`, with `nonce` the run's. The summary is cut at [`CLAIM_SUMMARY_CHARS`]; an empty one is `None`.
pub fn claim_in(text: &str, nonce: &str) -> Option<Option<String>> {
    if nonce.is_empty() {
        return None;
    }
    let line = rounds::last_line(text)?;
    let after = line
        .strip_prefix(CLAIM_WORD)?
        .strip_prefix(' ')?
        .strip_prefix(nonce)?
        .trim_start();
    if after.is_empty() {
        return Some(None);
    }
    let summary = after.strip_prefix(':')?.trim();
    Some((!summary.is_empty()).then(|| {
        summary
            .chars()
            .take(CLAIM_SUMMARY_CHARS)
            .collect::<String>()
    }))
}

/// The rest of the text of both prompts: how to claim, how to give up, and (unless the goal text holds it already) the note about work outside
/// the checkout.
fn closing(goal_text: &str, outside_note: bool) -> String {
    let mut s = String::from(
        "If you cannot make progress, end your final message with a line that starts with BLOCKED: and says what is missing.",
    );
    if outside_note && !goal_text.contains(settings::OUTSIDE_CHECKOUT_NOTE) {
        s.push(' ');
        s.push_str(settings::OUTSIDE_CHECKOUT_NOTE);
    }
    s
}

fn claim_form(nonce: &str) -> String {
    format!("{CLAIM_WORD} {nonce}: <one sentence on what you finished>")
}

/// The prompt after the user's own: the module asks whether the goal is met, as the follow-up of a goal plugin does. Sent once per session.
pub fn verification_prompt(goal_text: &str, nonce: &str, outside_note: bool) -> String {
    format!(
        "Are you sure you have completed the goal? The goal: {goal_text}\n\nRun the checks of the goal card again now and answer from their output, \
not from memory. If every check passes in a run made after your last edit, end your final message with one last line in exactly this form, with the \
words and the code as written: {form}\nWrite that line only when the work is finished. If a check fails, fix it, run the checks again, and end your \
final message without that line. {closing}",
        form = claim_form(nonce),
        closing = closing(goal_text, outside_note),
    )
}

/// The prompt after every later round that did not claim.
pub fn continue_prompt(goal_text: &str, nonce: &str, outside_note: bool) -> String {
    format!(
        "Continue working toward the goal: {goal_text}\n\nRun the checks of the goal card again and answer from their output. When every check passes in \
a run made after your last edit, end your final message with one last line in exactly this form, with the words and the code as written: {form}\n\
{closing}",
        form = claim_form(nonce),
        closing = closing(goal_text, outside_note),
    )
}

/// The limits of the harness loop: the plugin loop's, except that a claim needs no wait for a round that would take its place (no plugin can
/// start one), and the words of a brake name the engine.
pub fn harness_limits(settings: &GoalSettings, who: &'static str) -> Limits {
    Limits {
        who,
        settle: Duration::ZERO,
        ..settings.limits()
    }
}

/// The end of `text`, at most [`KEEP_TEXT_CHARS`] characters.
fn tail_chars(text: &str) -> String {
    let n = text.chars().count();
    text.chars()
        .skip(n.saturating_sub(KEEP_TEXT_CHARS))
        .collect()
}

impl GoalHook {
    /// The nonce of this run, made on first use.
    fn nonce(&self) -> &str {
        self.nonce.get_or_init(new_nonce)
    }

    /// Fixes the nonce of the run (a test names it so that a scripted engine can write the claim line). Has no effect once a nonce exists.
    pub fn use_nonce(&self, nonce: &str) {
        let _ = self.nonce.set(nonce.to_string());
    }

    /// A fresh session replaced the one that held the loop: the new session knows nothing of the verification, so it is asked again.
    pub(super) fn rearm_harness(&self) {
        {
            let mut c = lock(&self.core);
            // The trees seen stay (they are the run's evidence); the tree the last round ended in is the base of the new machine.
            c.machine = Machine::new(self.limits()).with_base(c.last_tree.clone());
            c.phase = Phase::Running;
            c.noted_ends = 0;
            c.verifying = false;
            c.asked = false;
            c.followup = None;
        }
        *lock(&self.wire) = WireState::default();
        if let Some(rw) = self.round.get() {
            lock(&rw.watch).boundary(now());
        }
        self.note(
            "rearm",
            "Goal loop: the goal is set again for the new session.",
        );
    }

    /// One turn ended in a loop the harness drives: the turn is a round. The claim, the machine's rules and the follow-up prompt, in that order.
    pub(super) async fn harness_step(&self, at: Instant, end: &TurnEnd<'_>) -> TurnVote {
        let nonce = self.nonce().to_string();
        let stop = match &end.raw.watched {
            Watched::Done(Ok(done)) => Some(done.stop_reason.as_str()),
            _ => None,
        };
        let completed = stop == Some("end_turn");
        let calls = std::mem::take(&mut lock(&self.wire).turn_calls);
        let (n, started_by_us, asked) = {
            let mut c = lock(&self.core);
            c.rounds += 1;
            // The follow-up of this round, if there was one, has done its work.
            c.followup = None;
            (c.rounds, c.verifying, c.asked)
        };
        // The claim counts in a round the module's own prompt started, and only when the engine ended the turn itself.
        let claim = (completed && started_by_us)
            .then(|| claim_in(&end.raw.message, &nonce))
            .flatten();
        let fp = match self.tree.get() {
            Some(t) => t.fingerprint().await.map_or(Fp::Unknown, Fp::Is),
            None => Fp::Unknown,
        };
        if let Some(env) = self.env() {
            env.log.line(&format!(
                "direct turn goal loop round {n} ended: stop {}, {calls} tool calls, claim {}, tree {}",
                stop.unwrap_or("none"),
                claim.is_some(),
                match &fp {
                    Fp::Is(f) => f.chars().take(12).collect::<String>(),
                    _ => "unknown".to_string(),
                }
            ));
        }
        let (decision, note) = {
            let mut c = lock(&self.core);
            // The run may have changed while the tree was read (the module went off, a brake ended it): the turn is then no round of this loop.
            if !matches!(c.phase, Phase::Running) {
                return TurnVote::Done;
            }
            // A tree the work was already in is no progress, whatever the round before it looked like (edits that undo each other).
            let fp = match fp {
                Fp::Is(tree) => {
                    c.last_tree = Some(tree.clone());
                    if c.seen.contains(&tree) {
                        Fp::Same
                    } else {
                        if c.seen.len() < SEEN_TREES_MAX {
                            c.seen.insert(tree.clone());
                        }
                        Fp::Is(tree)
                    }
                }
                other => other,
            };
            c.machine.observe(at, Obs::Start { n });
            c.machine.observe(
                at,
                Obs::End {
                    end: RoundEnd {
                        n,
                        status: if completed {
                            "completed".to_string()
                        } else {
                            stop.unwrap_or("ended").to_string()
                        },
                        iterations: calls.saturating_add(1),
                        text: tail_chars(&end.raw.message),
                        error: None,
                    },
                    fp,
                },
            );
            if let Some(summary) = claim {
                c.machine.observe(at, Obs::Candidate { summary });
            }
            (c.machine.decide(at), self.round_note(&mut c, at))
        };
        if let Some((text, rounds, status)) = note {
            self.note_with("round", &text, json!({"rounds": rounds, "status": status}));
        }
        match decision {
            Decision::Done { summary } => {
                lock(&self.core).phase = Phase::Finished;
                self.watch_done();
                let label = self.engine.label;
                let words = match &summary {
                    Some(s) => format!(
                        "Goal loop: {label} ended its round with the goal-complete line and a message. Its summary: {s}"
                    ),
                    None => {
                        format!(
                            "Goal loop: {label} ended its round with the goal-complete line and a message."
                        )
                    }
                };
                let fields = match &summary {
                    Some(s) => json!({"summary": s}),
                    None => json!({}),
                };
                self.note_with("done", &words, fields);
                TurnVote::Done
            }
            Decision::Brake(brake) => {
                // The turn has ended and no round is in flight: the run ends here, with the harness's words.
                lock(&self.core).phase = Phase::Finished;
                self.watch_done();
                self.note_with("brake", &brake.detail, json!({"rule": brake.rule.word()}));
                TurnVote::Stop {
                    detail: scrub_detail(&brake.detail),
                }
            }
            Decision::Wait | Decision::Kick => {
                let cfg = self.cfg();
                let (text, again) = if asked {
                    (
                        continue_prompt(&cfg.goal_text, &nonce, cfg.outside_note),
                        true,
                    )
                } else {
                    (
                        verification_prompt(&cfg.goal_text, &nonce, cfg.outside_note),
                        false,
                    )
                };
                lock(&self.core).followup = Some(text.clone());
                self.note(
                    "kick",
                    &format!(
                        "Goal loop: {} is asked {} whether the goal is met.",
                        self.engine.label,
                        if again { "again" } else { "once" }
                    ),
                );
                TurnVote::Prompt(text)
            }
        }
    }

    /// A prompt is about to go to the engine. A round started by the follow-up this module voted may end in a claim; a round started by any other prompt
    /// (the user's, a notice, another module's) may not, even when the nonce is in the conversation. The calls of the turn are counted from here.
    pub(super) fn prompt_sent(&self, text: &str) {
        {
            let mut c = lock(&self.core);
            let ours = c.followup.as_deref() == Some(text);
            c.verifying = ours;
            // The verification question is asked once per session: the prompts after it are continues.
            c.asked |= ours;
        }
        lock(&self.wire).turn_calls = 0;
    }

    /// Whether this engine's loop is the one the harness drives.
    pub(super) fn drives_itself(&self) -> bool {
        self.engine.kind == LoopKind::Harness
    }
}
