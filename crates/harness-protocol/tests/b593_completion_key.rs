//! The completion key. Only run-session and agentd hold the run's first nonce, so only they can make the key worker-api requires on
//! `/internal/events` and `/internal/turn-complete`. The Agent item stores the hash of the key and never the key.

#![allow(clippy::unwrap_used)]

use harness_protocol::store::{COMPLETION_KEY_HEADER, CompletionKeyCheck, CredsBinding};

fn binding(hash: Option<String>) -> CredsBinding {
    CredsBinding {
        nonce_sha256: CredsBinding::hash_nonce("n0"),
        revoked: false,
        tenant: "t".into(),
        mode: "repo".into(),
        session_policy: "{}".into(),
        completion_key_sha256: hash,
        output_pins_required: None,
    }
}

#[test]
fn b593_the_key_comes_from_the_first_nonce_and_differs_from_every_hash_of_it() {
    let key = CredsBinding::completion_key("first-nonce");
    assert_eq!(key.len(), 64);
    assert!(key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
    assert_eq!(key, CredsBinding::completion_key("first-nonce"), "stable");
    assert_ne!(key, CredsBinding::completion_key("another-nonce"));
    // Domain separation: neither the key nor its hash is the nonce's own hash, which the Agent item stores for the refresh check.
    assert_ne!(key, CredsBinding::hash_nonce("first-nonce"));
    assert_ne!(
        CredsBinding::hash_completion_key(&key),
        CredsBinding::hash_nonce("first-nonce")
    );
}

#[test]
fn b593_the_item_stores_the_hash_of_the_key_and_a_request_must_carry_the_key() {
    let key = CredsBinding::completion_key("first-nonce");
    let b = binding(Some(CredsBinding::hash_completion_key(&key)));
    assert_eq!(
        b.check_completion_key(Some(&key)),
        CompletionKeyCheck::Valid
    );
    // What is stored is not what opens the route.
    let stored = b.completion_key_sha256.clone().unwrap();
    assert_eq!(
        b.check_completion_key(Some(&stored)),
        CompletionKeyCheck::Refused
    );
    assert_eq!(b.check_completion_key(None), CompletionKeyCheck::Refused);
    assert_eq!(
        b.check_completion_key(Some("")),
        CompletionKeyCheck::Refused
    );
    assert_eq!(
        b.check_completion_key(Some(&CredsBinding::completion_key("another-nonce"))),
        CompletionKeyCheck::Refused
    );
}

#[test]
fn b593_an_item_without_the_member_is_legacy_and_a_malformed_member_refuses_everything() {
    let key = CredsBinding::completion_key("first-nonce");
    assert_eq!(
        binding(None).check_completion_key(None),
        CompletionKeyCheck::Legacy
    );
    assert_eq!(
        binding(None).check_completion_key(Some(&key)),
        CompletionKeyCheck::Legacy
    );
    for bad in ["", "abc", "not-a-hash"] {
        let b = binding(Some(bad.to_string()));
        assert_eq!(
            b.check_completion_key(Some(&key)),
            CompletionKeyCheck::Refused,
            "{bad:?}"
        );
        assert_eq!(
            b.check_completion_key(None),
            CompletionKeyCheck::Refused,
            "{bad:?}"
        );
    }
}

#[test]
fn b593_the_member_is_left_out_of_an_old_item_and_read_back_from_a_new_one() {
    let old =
        r#"{"nonceSha256":"aa","revoked":false,"tenant":"t","mode":"repo","sessionPolicy":"{}"}"#;
    let b: CredsBinding = serde_json::from_str(old).unwrap();
    assert_eq!(b.completion_key_sha256, None);
    assert!(
        !serde_json::to_string(&b)
            .unwrap()
            .contains("completionKeySha256")
    );
    let new = binding(Some("ab".repeat(32)));
    let json = serde_json::to_string(&new).unwrap();
    assert!(json.contains("\"completionKeySha256\""), "{json}");
    assert_eq!(serde_json::from_str::<CredsBinding>(&json).unwrap(), new);
    assert_eq!(COMPLETION_KEY_HEADER, "x-hamh-completion-key");
}
