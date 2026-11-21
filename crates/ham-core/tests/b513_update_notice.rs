//! The update notice of the terminal app: the rules that need no process, file or network. Which version a listing of the update
//! source names (the version tag at the head of main, else the highest one), when a notice is due, its words, the web page of an update
//! source and the sentences after a click on the notice, and the schedule of the look.
#![allow(clippy::unwrap_used)]

use ham_core::update::{
    DEFAULT_SOURCE, Heads, LOOK_EVERY_S, LOOK_RETRY_S, LookSchedule, TO_UPDATE, UpdateNotice,
    newest_of, no_page_words, notice_for, notice_words, page_failed_words, page_opened_words,
    web_address,
};

/// A commit id made of one hex digit, so no test holds a text that reads like a key.
fn id(digit: char) -> String {
    digit.to_string().repeat(40)
}

/// One line of the listing: the id, a tab and the name.
fn line(digit: char, name: &str) -> String {
    format!("{}\t{name}\n", id(digit))
}

fn listing(lines: &[(char, &str)]) -> String {
    lines.iter().map(|(d, n)| line(*d, n)).collect()
}

fn newest(version: &str) -> Heads {
    Heads::Newest(version.to_string())
}

#[test]
fn b513_newest_is_the_tag_at_mains_head_else_the_highest() {
    // A lightweight v5.25 on the head of main and an annotated v5.24 on an older commit.
    let four = listing(&[
        ('2', "refs/heads/main"),
        ('b', "refs/tags/v5.24"),
        ('e', "refs/tags/v5.24^{}"),
        ('2', "refs/tags/v5.25"),
    ]);
    assert_eq!(newest_of(&four), newest("5.25"));

    // An annotated tag on the head of main: the line of the tag object, then the commit under ^{}.
    let annotated = listing(&[
        ('2', "refs/heads/main"),
        ('7', "refs/tags/v5.25"),
        ('2', "refs/tags/v5.25^{}"),
    ]);
    assert_eq!(newest_of(&annotated), newest("5.25"));

    // A commit on main after the tagged release: no tag has the commit of main, and the highest version tag is named.
    let after = listing(&[
        ('a', "refs/heads/main"),
        ('7', "refs/tags/v5.24"),
        ('6', "refs/tags/v5.24^{}"),
        ('4', "refs/tags/v5.25"),
        ('f', "refs/tags/v5.25^{}"),
    ]);
    assert_eq!(newest_of(&after), newest("5.25"));

    // Main stands at the commit of v5.24 while v5.25 stands on another commit: ham update installs 5.24 there.
    let behind = listing(&[
        ('6', "refs/heads/main"),
        ('7', "refs/tags/v5.24"),
        ('6', "refs/tags/v5.24^{}"),
        ('4', "refs/tags/v5.25"),
        ('f', "refs/tags/v5.25^{}"),
    ]);
    assert_eq!(newest_of(&behind), newest("5.24"));

    // Two version tags on the head of main: the higher one.
    let two = listing(&[
        ('2', "refs/heads/main"),
        ('2', "refs/tags/v5.24"),
        ('2', "refs/tags/v5.26"),
        ('2', "refs/tags/v5.25"),
    ]);
    assert_eq!(newest_of(&two), newest("5.26"));

    // Versions are compared as numbers: 10.01 is above 5.99.
    let numbers = listing(&[
        ('a', "refs/heads/main"),
        ('b', "refs/tags/v5.99"),
        ('c', "refs/tags/v10.01"),
    ]);
    assert_eq!(newest_of(&numbers), newest("10.01"));

    // A release line that starts with 0 counts.
    let zero = listing(&[('2', "refs/heads/main"), ('2', "refs/tags/v0.07")]);
    assert_eq!(newest_of(&zero), newest("0.07"));
}

#[test]
fn b513_a_source_without_a_release_names_none() {
    assert_eq!(newest_of(""), Heads::NoRelease);
    assert_eq!(
        newest_of(&listing(&[('2', "refs/heads/main")])),
        Heads::NoRelease,
        "main and no version tag"
    );
    assert_eq!(
        newest_of(&listing(&[
            ('7', "refs/tags/v5.25"),
            ('2', "refs/tags/v5.25^{}")
        ])),
        Heads::NoRelease,
        "version tags and no main: ham update installs the head of main, so there is nothing to install"
    );
    // Names that are no version tag and no main branch are passed over, whatever they look like.
    for ignored in [
        "refs/tags/v5.25/x",
        "refs/tags/vfoo",
        "refs/tags/v05.25",
        "refs/tags/v5.2",
        "refs/tags/v5.25-pre1",
        "refs/tags/5.25",
        "refs/tags/V5.25",
        "refs/tags/v5.25^{}x",
        "refs/heads/x/refs/heads/main",
        "refs/tags/x/refs/tags/v9.99",
        "refs/heads/v9.99",
        "refs/heads/main2",
    ] {
        let text = listing(&[('2', "refs/heads/main"), ('2', ignored)]);
        assert_eq!(newest_of(&text), Heads::NoRelease, "{ignored}");
        let beside = listing(&[
            ('2', "refs/heads/main"),
            ('2', ignored),
            ('2', "refs/tags/v5.24"),
        ]);
        assert_eq!(newest_of(&beside), newest("5.24"), "{ignored}");
    }
}

#[test]
fn b513_an_answer_that_does_not_parse_is_no_result() {
    let main = line('2', "refs/heads/main");
    let tag = line('2', "refs/tags/v5.25");
    let cases: Vec<(&str, String)> = vec![
        (
            "two lines for main",
            format!("{main}{}{tag}", line('3', "refs/heads/main")),
        ),
        (
            "a line without a tab",
            format!("{main}{} refs/tags/v5.25\n", id('2')),
        ),
        (
            "an id of 39 characters",
            format!("{main}{}\trefs/tags/v5.25\n", &id('2')[..39]),
        ),
        (
            "an id of 41 characters",
            format!("{main}{}2\trefs/tags/v5.25\n", id('2')),
        ),
        (
            "an id in upper case",
            format!("{main}{}\trefs/tags/v5.25\n", id('A')),
        ),
        (
            "an id that is not hex",
            format!("{main}{}\trefs/tags/v5.25\n", id('g')),
        ),
        ("a line with no name", format!("{main}{}\t\n", id('2'))),
        ("an empty line between two lines", format!("{main}\n{tag}")),
        ("words", "fatal: something else\n".to_string()),
    ];
    for (what, text) in cases {
        assert_eq!(newest_of(&text), Heads::Garbled, "{what}");
    }
}

#[test]
fn b513_only_an_update_plan_raises_the_notice() {
    let with = |version: &str| {
        Some(UpdateNotice {
            version: version.to_string(),
        })
    };
    // Both behind, the client behind, the backend behind: ham update has something to install.
    assert_eq!(notice_for("5.24", Some("5.24"), Some("5.25")), with("5.25"));
    assert_eq!(notice_for("5.24", Some("5.25"), Some("5.25")), with("5.25"));
    assert_eq!(notice_for("5.25", Some("5.24"), Some("5.25")), with("5.25"));
    // Equal, the client newer, the backend newer, the backend unknown: nothing to install, or ham update would refuse.
    assert_eq!(notice_for("5.25", Some("5.25"), Some("5.25")), None);
    assert_eq!(notice_for("5.26", Some("5.25"), Some("5.25")), None);
    assert_eq!(notice_for("5.24", Some("5.26"), Some("5.25")), None);
    assert_eq!(notice_for("5.24", None, Some("5.25")), None);
    assert_eq!(notice_for("5.24", Some("?.??"), Some("5.25")), None);
    // The newest version is not known, or is no version.
    assert_eq!(notice_for("5.24", Some("5.24"), None), None);
    assert_eq!(notice_for("5.24", Some("5.24"), Some("newest")), None);
}

#[test]
fn b513_notice_words_are_these() {
    let named = UpdateNotice {
        version: "5.25".to_string(),
    };
    // The notice only indicates the newer release: it names no key and no command.
    assert_eq!(
        notice_words(&named),
        ("v5.25 available".to_string(), "update".to_string())
    );
    // The long text of a one-digit major number is 15 cells and the short one 6: the widths the first row is planned with.
    assert_eq!(notice_words(&named).0.chars().count(), 15);
    assert_eq!(notice_words(&named).1.chars().count(), 6);
    let two_digits = UpdateNotice {
        version: "10.01".to_string(),
    };
    assert_eq!(
        notice_words(&two_digits),
        ("v10.01 available".to_string(), "update".to_string())
    );
}

#[test]
fn b513_the_web_page_of_a_source_with_a_query_or_a_fragment() {
    // An address with a query or a fragment is the page as it stands: `?`, `#`, `&`, `;` and `$` are characters of an address, and
    // nothing of it is rewritten. The opener gets it as one argument, and no shell reads it.
    for same in [
        "https://git.example.test/team/ham?ref=main",
        "https://git.example.test/team/ham#readme",
        "https://git.example.test/team/ham?a=1&b=2;c=$HOME",
        // A `.git` in front of a query or a fragment is not the end of the address: it stays.
        "https://git.example.test/team/ham.git?a=1&b=2;c=$HOME",
        "https://git.example.test/team/ham.git#readme",
        "https://git.example.test/team/ham.git/?ref=main",
        // A query or a fragment that ends as the address of a repository does keeps its end.
        "https://git.example.test/team/ham?path=docs/ham.git",
        "https://git.example.test/team/ham?path=docs/",
        "https://git.example.test/team/ham#notes.git",
        // An `@` in a query is no login.
        "https://git.example.test/team/ham?owner=a@b",
    ] {
        assert_eq!(web_address(same), Some(same.to_string()), "{same}");
    }
    // A query or a fragment is no host, and a login in front of the host is still a login.
    for none in [
        "https://?a=1",
        "https://#top",
        "https://user@git.example.test/team/ham?a=1",
        // The word of the login is put in at run time, so that no source line is shaped like a credential.
        &format!(
            "https://user:{}@git.example.test/team/ham.git#top",
            ["wo", "rd"].concat()
        ),
    ] {
        assert_eq!(web_address(none), None, "{none:?}");
    }
}

#[test]
fn b513_the_web_page_of_an_update_source() {
    let page = |source: &str| web_address(source);
    let some = |text: &str| Some(text.to_string());
    // The default source gives the page of the public repository.
    assert_eq!(
        page(DEFAULT_SOURCE),
        some("https://github.com/aws-samples/sample-hyperscale-agent-meta-harness")
    );
    // An https address without its trailing `.git` and without a trailing slash.
    assert_eq!(
        page("https://git.example.test/team/ham.git"),
        some("https://git.example.test/team/ham")
    );
    assert_eq!(
        page("https://git.example.test/team/ham.git/"),
        some("https://git.example.test/team/ham")
    );
    assert_eq!(
        page("https://git.example.test/team/ham/"),
        some("https://git.example.test/team/ham")
    );
    assert_eq!(
        page("https://git.example.test/team/ham"),
        some("https://git.example.test/team/ham")
    );
    assert_eq!(
        page("  https://git.example.test/team/ham.git  "),
        some("https://git.example.test/team/ham"),
        "the setting is read trimmed, as the source is"
    );
    assert_eq!(
        page("HTTPS://git.example.test/team/ham.git"),
        some("HTTPS://git.example.test/team/ham"),
        "the scheme in any case, as the source takes it"
    );
    // Only a trailing `.git` goes.
    assert_eq!(
        page("https://git.example.test/team/ham.github"),
        some("https://git.example.test/team/ham.github")
    );
    assert_eq!(
        page("https://git.example.test/a.git/b"),
        some("https://git.example.test/a.git/b")
    );
    // Every other source has no web page: a local path, ssh, the off switch, plain http, nothing.
    for none in [
        "file:///srv/mirror/ham.git",
        "ssh://git@git.example.test/team/ham.git",
        "git@git.example.test:team/ham.git",
        "http://git.example.test/team/ham.git",
        "off",
        "",
        "https://",
        "https:///",
        "https://.git",
        "/srv/mirror/ham.git",
        "xhttps://git.example.test/team/ham.git",
    ] {
        assert_eq!(page(none), None, "{none:?}");
    }
    // An address a browser must not be handed, or that would not read as one line on the screen: no page.
    for none in [
        "https://git.example.test/team/ham repo.git",
        "https://git.example.test/team/ham\u{1b}[31m.git",
        "https://git.example.test/team/\nham.git",
        "https://user@git.example.test/team/ham.git",
        &format!(
            "https://user:{}@git.example.test/team/ham.git",
            ["wo", "rd"].concat()
        ),
    ] {
        assert_eq!(page(none), None, "{none:?}");
    }
    // An `@` after the host is part of the path, not a login.
    assert_eq!(
        page("https://git.example.test/team/@ham.git"),
        some("https://git.example.test/team/@ham")
    );
}

#[test]
fn b513_the_sentences_after_a_click_are_these() {
    assert_eq!(TO_UPDATE, "To update, quit ham and run ham update.");
    // The page was opened: the address, then how to update.
    assert_eq!(
        page_opened_words("https://git.example.test/team/ham"),
        "Opened https://git.example.test/team/ham. To update, quit ham and run ham update."
    );
    // The source has no web page: the release, then how to update.
    assert_eq!(
        no_page_words("5.25"),
        "v5.25 is available. To update, quit ham and run ham update."
    );
    // The opener failed: its sentence as the app says it for any link, then how to update.
    assert_eq!(
        page_failed_words(
            "could not open the link: could not run xdg-open: not found (set HAMH_OPEN_COMMAND to a program that opens a URL)"
        ),
        "could not open the link: could not run xdg-open: not found (set HAMH_OPEN_COMMAND to a program that opens a URL). To update, quit ham and run ham update."
    );
    assert_eq!(
        page_failed_words("could not open the link."),
        "could not open the link. To update, quit ham and run ham update.",
        "one full stop, not two"
    );
}

#[test]
fn b513_schedule_first_look_then_a_day() {
    assert_eq!(LOOK_EVERY_S, 86_400);
    // The first look is due at once, also before the app has a wall clock (the time 0).
    assert!(LookSchedule::new().due(0));
    let t = 1_800_000_000;
    let mut first = LookSchedule::new();
    assert!(first.due(t));
    let generation = first.started();
    assert!(first.ended(generation, false, t), "the result applies");
    assert!(!first.due(t));
    assert!(!first.due(t + 86_399));
    assert!(first.due(t + 86_400));
    assert!(first.due(t + 200_000), "a laptop that slept gets one look");

    // The wall clock went backwards: not due, and the day starts again at that time.
    let mut s = LookSchedule::new();
    let generation = s.started();
    assert!(s.ended(generation, false, t));
    assert!(!s.due(t - 1_000));
    assert!(!s.due(t - 1_000 + 86_399));
    assert!(s.due(t - 1_000 + 86_400));
}

#[test]
fn b513_schedule_a_failed_look_is_tried_after_an_hour() {
    assert_eq!(LOOK_RETRY_S, 3_600);
    let t = 1_800_000_000;
    let mut s = LookSchedule::new();
    let generation = s.started();
    assert!(s.ended(generation, true, t));
    assert!(!s.due(t + 3_599));
    assert!(s.due(t + 3_600));
    // The look that follows found nothing: the next one is a day later.
    let generation = s.started();
    assert!(s.ended(generation, false, t + 3_600));
    assert!(!s.due(t + 3_600 + 86_399));
    assert!(s.due(t + 3_600 + 86_400));
}

#[test]
fn b513_schedule_one_look_at_a_time_and_a_reset_looks_again() {
    let t = 1_800_000_000;
    let mut s = LookSchedule::new();
    let generation = s.started();
    for now in [0, t, t + 86_400, t + 10 * 86_400] {
        assert!(!s.due(now), "never a second look while one is in flight");
    }
    assert!(s.ended(generation, false, t));
    assert!(!s.due(t + 60));

    // A Settings write with no look in flight: due at once.
    s.reset();
    assert!(s.due(t + 60));
    let second = s.started();
    assert_ne!(second, generation);
    assert!(!s.due(t + 61));

    // A Settings write during a flight: not due until that look has ended, then at once.
    s.reset();
    assert!(!s.due(t + 62));
    assert!(
        !s.ended(second, false, t + 63),
        "the look of the earlier settings does not apply"
    );
    assert!(s.due(t + 63));
    let third = s.started();
    assert!(!s.due(t + 64), "the reset asked for one look, not for two");
    assert!(s.ended(third, false, t + 65));
    assert!(!s.due(t + 66));
    assert!(s.due(t + 65 + 86_400));
}

#[test]
fn b513_schedule_drops_a_result_of_an_older_generation() {
    let t = 1_800_000_000;
    let mut s = LookSchedule::new();
    let first = s.started();
    assert!(s.ended(first, false, t));
    let old = s.started();
    s.reset();
    // The earlier generation: the result does not apply, no end is recorded (the look the reset asked for is still due at any time), and
    // "in flight" is cleared.
    assert!(!s.ended(old, false, t + 86_400));
    assert!(s.due(t));
    let new = s.started();
    assert_eq!(new, old + 1);
    assert!(s.ended(new, true, t + 5));
    assert!(!s.due(t + 5 + 3_599));
    assert!(s.due(t + 5 + 3_600));
}
