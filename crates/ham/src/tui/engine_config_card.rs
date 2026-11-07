//! The card of a run's engine settings on the run screen: when agentd placed the user's
//! settings and read them back, the run view carries a [`ham_core::engine_config_card::EngineConfigCard`] and the chat view shows it as a folded card
//! after the skills card, `engine settings placed · <engine> · N set · settings hash <12 digits>`; Space opens it to one row per placed setting. The
//! words are `ham_core`'s. The card says "placed", never "applied", and no line says the work was verified, tested, gated or evaluated.

use super::chat::Card;
use ham_core::state::RunView;

/// Adds the engine settings card to `cards`, when the run has one. `width` is the columns a card line gets before it wraps.
pub(crate) fn push_card(cards: &mut Vec<Card>, v: &RunView, width: usize) {
    if let Some(c) = v.engine_config.as_ref() {
        cards.push(Card {
            id: "engine-config".into(),
            title: c.title(),
            lines: c.lines(width),
            folded: true,
            alert: false,
        });
    }
}
