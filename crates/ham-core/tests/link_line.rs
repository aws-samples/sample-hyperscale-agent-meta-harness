//! The dashboard's short `Link` line.
//! The presigned URL is about 1,000 characters and wraps over eight
//! screen lines, so the TUI card shows one short line (label, host, zip name, expiry in local time, the keys) and the keys `o` and `d`
//! act on the link. This file pins the pure parts: the line, the host label, the clock text and the plain card that every
//! non-interactive mode keeps printing with the whole URL.

#![allow(clippy::unwrap_used)]

use ham_core::render::{
    CardUsage, FinalCard, LinkLine, PUSH_LABEL, final_card, link_host, link_line, utc_clock,
};
use ham_core::state::RunState;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use serde_json::json;

const NAME: &str = "python-calc-01K6A0000000000000000000R1-abc1234.zip";
const KEYS: &str = "o open · d download";

fn line_of(width: usize) -> String {
    link_line(
        &LinkLine {
            host: "s3",
            name: NAME,
            expires: Some("06:12 MDT"),
            expired: false,
            keys: KEYS,
        },
        width,
    )
}

/// The card is two columns narrower than the terminal (its border), so a 100-column terminal gives the line 98 characters.
#[test]
fn the_link_line_is_one_line_at_100_and_140_columns() {
    for (columns, inner) in [(100usize, 98usize), (140, 138)] {
        let line = line_of(inner);
        assert!(!line.contains('\n'), "{columns}: {line:?}");
        assert!(
            line.chars().count() <= inner,
            "{columns} columns: {} characters: {line}",
            line.chars().count()
        );
        assert!(line.starts_with("Link      s3 · "), "{line}");
        assert!(line.contains("expires 06:12 MDT"), "{line}");
        assert!(line.ends_with(KEYS), "{line}");
    }
    // At 140 columns the whole zip name fits; at 100 it is shortened in the middle and keeps its tail, so the sha and `.zip` stay.
    assert!(line_of(138).contains(NAME), "{}", line_of(138));
    let narrow = line_of(98);
    assert!(!narrow.contains(NAME), "{narrow}");
    assert!(
        narrow.contains('…') && narrow.contains("abc1234.zip"),
        "{narrow}"
    );
}

/// The line gives up its parts in order (name, host, expiry) and never wraps; the label and the keys stay while they can.
#[test]
fn the_line_keeps_fitting_and_keeps_the_keys_down_to_30_characters() {
    for width in 30..=200 {
        let line = line_of(width);
        assert!(
            line.chars().count() <= width,
            "{width}: {} characters: {line}",
            line.chars().count()
        );
        assert!(line.starts_with("Link      "), "{width}: {line}");
        assert!(line.ends_with(KEYS), "{width}: {line}");
    }
    // Below the width of the label and the keys the line is cut, with an ellipsis, and is still one line.
    for width in 1..30 {
        let line = line_of(width);
        assert!(line.chars().count() <= width, "{width}: {line}");
        assert!(!line.contains('\n'));
    }
}

#[test]
fn a_lapsed_link_says_expired_and_a_missing_expiry_says_nothing() {
    let lapsed = link_line(
        &LinkLine {
            host: "s3",
            name: NAME,
            expires: Some("05:40 MDT"),
            expired: true,
            keys: "o new link · d download",
        },
        138,
    );
    assert!(lapsed.contains("expired 05:40 MDT"), "{lapsed}");
    assert!(!lapsed.contains("expires"), "{lapsed}");
    assert!(lapsed.ends_with("o new link · d download"), "{lapsed}");
    let unknown = link_line(
        &LinkLine {
            host: "s3",
            name: NAME,
            expires: None,
            expired: false,
            keys: "d download",
        },
        138,
    );
    assert!(!unknown.contains("expire"), "{unknown}");
    assert!(unknown.ends_with("d download"), "{unknown}");
}

#[test]
fn the_host_label_is_s3_for_an_s3_host_and_never_user_info_port_path_or_query() {
    for url in [
        "https://bucket.s3.us-west-2.amazonaws.com/runs/R1/x.zip?X-Amz-Signature=fake",
        "https://s3.amazonaws.com/bucket/x.zip?X-Amz-Expires=900",
        "https://s3.us-east-1.amazonaws.com/bucket/x.zip",
        "https://my-bucket.s3.amazonaws.com/x.zip",
    ] {
        assert_eq!(link_host(url), "s3", "{url}");
    }
    assert_eq!(
        link_host("http://127.0.0.1:41999/objects/k?X-Amz-Date=1"),
        "127.0.0.1"
    );
    // The user information is put in at run time: the password scan flags a URL with a literal password in a tracked file.
    let userinfo = "user:secret";
    assert_eq!(
        link_host(&format!("https://{userinfo}@example.test:8443/a/b?c=d#e")),
        "example.test"
    );
    // Not a URL: nothing to show, and nothing from the input leaks into the label.
    assert_eq!(link_host("not a url"), "");
    assert_eq!(link_host(""), "");
}

#[test]
fn utc_clock_is_hours_and_minutes_with_the_zone() {
    // 2025-10-09T08:53:20Z
    assert_eq!(utc_clock(1_760_000_000), "08:53 UTC");
    assert_eq!(utc_clock(0), "00:00 UTC");
    assert_eq!(utc_clock(86_399), "23:59 UTC");
    assert_eq!(utc_clock(-1), "23:59 UTC");
}

fn ev(n: u8, t: EventType, v: serde_json::Value) -> Envelope {
    Envelope {
        schema_version: 1,
        event_id: format!("01J9ZZZZZZ00000000000000{n:02}"),
        run_id: "R1".into(),
        task_id: None,
        agent_id: None,
        seq: u64::from(n),
        ts: "2026-09-25T02:00:00Z".into(),
        event_type: t,
        authority: Authority::Harness,
        payload: Payload::Inline { value: v },
    }
}

fn delivered() -> RunState {
    let mut s = RunState::new("R1");
    s.apply(ev(1, EventType::RunCreated, json!({})));
    s.apply(ev(
        14,
        EventType::DeliveryReady,
        json!({"artifactId": "ART1", "sha256": "ab".repeat(32), "sizeBytes": 2048}),
    ));
    s
}

/// Non-interactive output (`ham watch` without a terminal, `ham pull`) keeps the whole link on one line, copyable; only the TUI
/// card shortens it. The card also names the command that pushes the code to another remote, after the clone command.
#[test]
fn the_plain_card_keeps_the_whole_url_and_names_the_push_command() {
    let url = format!(
        "https://b.s3.us-west-2.amazonaws.com/runs/R1/x.zip?X-Amz-Signature={}",
        "f".repeat(900)
    );
    let card = FinalCard {
        local_path: "/downloads/x.zip".into(),
        presigned_url: Some(url.clone()),
        expires_at: Some("2026-09-25T03:00:00Z".into()),
        usage: Some(CardUsage {
            tokens: 1,
            usd: Some(0.5),
        }),
        label_missing_after: None,
        tests: None,
        flow: None,
        session: None,
    };
    let text = final_card(&delivered().view(), &card);
    let link: Vec<&str> = text.lines().filter(|l| l.starts_with("Link ")).collect();
    assert_eq!(link.len(), 1, "{text}");
    assert_eq!(
        link[0],
        format!("Link      {url} (expires 2026-09-25T03:00:00Z)")
    );
    assert!(PUSH_LABEL.starts_with("Push"));
    let (clone, push) = (
        text.find("\nClone     ham clone R1\n").unwrap(),
        text.find("\nPush      ham push R1 <git-url>\n").unwrap(),
    );
    assert!(clone < push, "{text}");
    // No zip, no clone and no push.
    let mut s = RunState::new("R1");
    s.apply(ev(1, EventType::RunCreated, json!({})));
    let none = final_card(&s.view(), &card);
    assert!(!none.contains("Push"), "{none}");
}
