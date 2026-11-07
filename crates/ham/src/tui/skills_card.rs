//! The card of a run's skills on the run screen: when agentd placed the run's skills, the run view carries a
//! [`ham_core::skills_card::SkillsCard`] and the chat view shows it as a folded card after the agent table, `skills · N placed`; Space opens
//! it to one row per skill (name, the first 12 digits of the digest, files and bytes) and the two sentences that say where the skills sit and
//! whether the engine's own list showed them. The words are `ham_core`'s. No line says the work was verified, tested, gated or evaluated.

use super::chat::Card;
use ham_core::state::RunView;

/// Adds the skills card to `cards`, when the run has one. `width` is the columns a card line gets before it wraps.
pub(crate) fn push_card(cards: &mut Vec<Card>, v: &RunView, width: usize) {
    if let Some(s) = v.skills.as_ref() {
        cards.push(Card {
            id: "skills".into(),
            title: s.title(),
            lines: s.lines(width),
            folded: true,
            alert: false,
        });
    }
}
