//! The run screen hands its cards to the conversation at every draw. Making a card into a block redacts and cleans its whole
//! body, and the agents card of a large run holds thousands of rows: a draw of 5,000 agents spent 46 of 100 samples making blocks
//! that it then found equal to the ones it held. The view makes the block of a card once, makes it again only when that card changes,
//! and shows exactly what it showed.

use ham::tui::chat::{Card, ChatView};
use ham_core::chat::{Block, ChatModel, Source};
use ham_core::notice::NoticeKind;

fn card(id: &str, rows: usize, folded: bool) -> Card {
    Card {
        id: id.into(),
        title: format!("{id} {rows}"),
        lines: (0..rows)
            .map(|i| format!("A{i:04} cline us.anthropic.claude-sonnet-5 3 edited src/lib.rs"))
            .collect(),
        folded,
        alert: false,
    }
}

fn cards() -> Vec<Card> {
    vec![card("status", 3, false), card("agents", 5000, true)]
}

/// The blocks a view that has seen nothing else makes of `cards`.
fn fresh(cards: &[Card]) -> Vec<Block> {
    let mut chat = ChatView::new();
    chat.set_cards(cards);
    chat.model().blocks.clone()
}

#[test]
fn b836_the_same_cards_are_made_into_blocks_once() {
    let mut chat = ChatView::new();
    let cards = cards();
    chat.set_cards(&cards);
    assert_eq!(chat.card_blocks_made(), 2);
    let shown = chat.model().blocks.clone();
    assert_eq!(shown.len(), 2);
    for _ in 0..10 {
        chat.set_cards(&cards);
    }
    assert_eq!(
        chat.card_blocks_made(),
        2,
        "ten more draws with the same cards made their blocks again"
    );
    assert_eq!(chat.model().blocks, shown);
}

#[test]
fn b836_a_changed_card_is_made_again_and_shown() {
    let mut chat = ChatView::new();
    let mut cards = cards();
    chat.set_cards(&cards);
    let before = chat.model().blocks.clone();
    cards[1].lines[0] = "A0000 cline - 4 ran the tests".into();
    chat.set_cards(&cards);
    assert_eq!(
        chat.card_blocks_made(),
        3,
        "only the card that changed is made again"
    );
    assert_ne!(chat.model().blocks, before);
    assert!(
        chat.model().blocks[1]
            .body
            .starts_with("A0000 cline - 4 ran the tests")
    );
    assert_eq!(chat.model().blocks, fresh(&cards));
    // One card fewer, then the first set again: the view shows what a new view would show.
    chat.set_cards(&cards[..1]);
    assert_eq!(chat.model().blocks, fresh(&cards[..1]));
    assert_eq!(chat.card_blocks_made(), 3);
    chat.set_cards(&self::cards());
    assert_eq!(chat.model().blocks, before);
    assert_eq!(
        chat.card_blocks_made(),
        4,
        "the agents card that came back is made again"
    );
}

#[test]
fn b836_a_status_card_that_changes_at_every_draw_does_not_make_the_agents_card_again() {
    // The status card names the time since the last save and the state of the sessions: on a live run it changes while the agents do not.
    let mut chat = ChatView::new();
    let mut cards = cards();
    chat.set_cards(&cards);
    for second in 1..=10u64 {
        cards[0].lines[2] = format!("Saved  17:40 MST, {second} s ago");
        chat.set_cards(&cards);
        assert_eq!(chat.card_blocks_made(), 2 + second);
        assert_eq!(chat.model().blocks, fresh(&cards));
    }
}

#[test]
fn b836_the_cards_stay_at_the_end_of_a_new_conversation() {
    let mut chat = ChatView::new();
    let cards = cards();
    chat.set_cards(&cards);
    let said = Block::card("said", "a message", "hello", NoticeKind::RunStatus, false);
    chat.set_model(ChatModel::from_blocks(
        Source::FullHistory,
        "",
        vec![said.clone()],
    ));
    chat.set_cards(&cards);
    assert_eq!(chat.card_blocks_made(), 2);
    let mut want = vec![said];
    want.extend(fresh(&cards));
    assert_eq!(chat.model().blocks, want);
}
