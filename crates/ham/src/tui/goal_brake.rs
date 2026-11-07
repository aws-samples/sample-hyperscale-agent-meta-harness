//! The card of a goal-loop brake on the run screen: when agentd's goal module ended the run by
//! one of its stopping rules, the run view carries a [`ham_core::goal_card::GoalBrakeCard`] and the chat view shows it as an `alert` card (the
//! "needs input" style), after the question card. The words are `ham_core`'s: which rule, how far the loop got, Cline's own `BLOCKED:` line,
//! and the `ham continue` command. No line says the work was verified, tested, gated or evaluated.

use super::chat::Card;
use ham_core::state::RunView;

/// Adds the brake's card to `cards`, when the run has one. `width` is the columns a card line gets before it wraps.
pub(crate) fn push_card(cards: &mut Vec<Card>, v: &RunView, width: usize) {
    if let Some(b) = v.goal_brake.as_ref() {
        cards.push(Card {
            id: "goal-brake".into(),
            title: b.title(),
            lines: b.lines(&v.run_id, width),
            folded: false,
            alert: true,
        });
    }
}
