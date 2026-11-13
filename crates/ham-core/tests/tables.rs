//! The number renderings the TUI shares: a model's window with grouped digits, and the compact window of the
//! dashboard's CTX column.

use ham_core::tables::{compact_tokens, group_digits};

#[test]
fn group_digits_groups_by_three() {
    for (n, want) in [
        (0, "0"),
        (999, "999"),
        (1_000, "1,000"),
        (200_000, "200,000"),
        (1_000_000, "1,000,000"),
        (4_294_967_295, "4,294,967,295"),
    ] {
        assert_eq!(group_digits(n), want);
    }
}

#[test]
fn compact_tokens_fit_the_agent_table_column() {
    for (n, want) in [
        (0, "0"),
        (999, "999"),
        (1_000, "1k"),
        (150_000, "150k"),
        (200_000, "200k"),
        (999_999, "999999"),
        (1_000_000, "1M"),
        (1_048_576, "1.05M"),
        (2_000_000, "2M"),
    ] {
        assert_eq!(compact_tokens(n), want, "{n}");
    }
    assert!(compact_tokens(1_048_576).chars().count() <= 6);
}
