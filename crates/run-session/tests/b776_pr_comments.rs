//! A message too long for the pull request description goes whole into the comments, in parts of at most 10,240 characters (the limit of
//! the repository service), instead of being cut at the end of the first comment.
#![allow(clippy::unwrap_used)]

use run_session::text::{self, COMMENT_MAX_CHARS};

#[test]
fn b776_a_message_of_25_thousand_characters_goes_into_three_comments_that_hold_all_of_it() {
    let m: String = (0..2_500).map(|i| format!("line {i:04}\n")).collect();
    assert!(m.chars().count() > 24_000);
    let c = text::comments(Some(&m));
    assert_eq!(
        c.len(),
        3,
        "{}",
        c.iter()
            .map(|x| x.chars().count().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    for (i, part) in c.iter().enumerate() {
        assert!(part.chars().count() <= COMMENT_MAX_CHARS, "part {i}");
        assert!(
            part.starts_with(&format!(
                "Cline's last message, whole (part {} of 3):\n",
                i + 1
            )),
            "{}",
            &part[..60]
        );
    }
    let body: String = c
        .iter()
        .map(|p| p.split_once(":\n").unwrap().1.to_string())
        .collect::<Vec<_>>()
        .join("");
    let quoted = text::quote(&m);
    assert_eq!(
        body, quoted,
        "the parts, joined, are the whole quoted message"
    );
}

#[test]
fn b776_a_message_that_fits_one_comment_keeps_the_old_header_and_one_comment() {
    let m = "a".repeat(8_000);
    let c = text::comments(Some(&m));
    assert_eq!(c.len(), 1);
    assert!(c[0].starts_with("Cline's last message, whole:\n"));
    assert!(c[0].contains(&m));
}
