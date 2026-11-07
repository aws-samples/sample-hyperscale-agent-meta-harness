//! Secret redaction for text the harness shows or stores (engine stderr in a failed turn's error).
//! Secret-shaped values are built at run time so none is ever written to the repository.
#![allow(clippy::unwrap_used)]

use harness_core::refusal::redact;

const PIP: &str = "`pip install` can modify files; commands of role implementer may not";

fn key_id() -> String {
    format!("{}{}{}", "AS", "IA", "Q7".repeat(8))
}

fn jwt() -> String {
    format!(
        "e{}.e{}.{}",
        "yJ2ZXIiOjEsImlzdSI6MTc5MDY4MDEy",
        "yJzdWIiOiJhd3MiLCJleHAiOjE3OTA",
        "s1g_n4tur3-".repeat(3)
    )
}

fn opaque() -> String {
    "Ab1".repeat(20)
}

#[test]
fn secrets_are_redacted_and_ordinary_text_is_kept() {
    let (key, jwt, opaque) = (key_id(), jwt(), opaque());
    for (text, secret) in [
        (format!("aws s3 ls # {key}"), key.clone()),
        (
            format!("curl -H 'Authorization: Bearer {jwt}' https://x.test"),
            jwt.clone(),
        ),
        (
            format!(
                "pip config set global.index-url https://aws:{opaque}@d-800000000022.d.codeartifact.us-west-2.amazonaws.com/pypi/p/simple/"
            ),
            opaque.clone(),
        ),
        (format!("export TOKEN={opaque}"), opaque.clone()),
        (
            format!("git clone https://user:{opaque}@gitlab.example/r.git"),
            opaque.clone(),
        ),
        (format!("echo {jwt}"), jwt.clone()),
        (format!("aws_secret_access_key = {opaque}"), opaque.clone()),
        (format!("password: '{opaque}'"), opaque.clone()),
        (format!("cat token.txt; echo {opaque}"), opaque.clone()),
    ] {
        let r = redact(&text);
        assert!(!r.contains(&secret), "{text} -> {r}");
        assert!(r.contains("<redacted>"), "{text} -> {r}");
    }
    for keep in [
        "cd /mnt/workspace/01M3QGZV29X84YSSH9ZWVDE5HG/textstats-pkg-a1/repo && python3 -m pytest -q",
        "git show 0123456789abcdef0123456789abcdef01234567",
        "print('total tokens', len(text.split()))",
        "python3 -c \"import tests.test_textstats as t; print(t.test_count_multiple_lines_and_words)\"",
        PIP,
        "network tools are denied for every role (fetch_web_content)",
        "sha256sum dist/textstats-0.1.0-py3-none-any.whl",
    ] {
        assert_eq!(redact(keep), keep);
    }
}
