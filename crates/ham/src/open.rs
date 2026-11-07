//! Opening a link in the user's browser, for the dashboard's `o` key.
//! The program is the platform's opener (`open` on macOS, `xdg-open` elsewhere) or the one `HAMH_OPEN_COMMAND`
//! names, so a test or a person with another browser setup can choose. The link is handed over as one argument of an argument vector:
//! no shell reads it, and a link that does not start with `http://` or `https://` is not opened, so it cannot be read as an option.
//! Nothing here prints or returns the link: an error names the program and says what to set.

use std::process::Stdio;

/// The platform's program that opens a URL.
pub fn default_program() -> &'static str {
    if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    }
}

/// The program to run: `setting` (`HAMH_OPEN_COMMAND`) when it is not empty, else [`default_program`].
pub fn program(setting: Option<&str>) -> String {
    setting
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(default_program())
        .to_string()
}

/// Whether `url` is a link the opener may be given.
pub fn is_web_link(url: &str) -> bool {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://"))
        && !url.chars().any(|c| c.is_control() || c.is_whitespace())
}

const FIX: &str = "set HAMH_OPEN_COMMAND to a program that opens a URL";

/// Runs `program url` with no terminal attached and waits for it to end. The error never contains `url`.
pub async fn open_url(program: &str, url: &str) -> Result<(), String> {
    if !is_web_link(url) {
        return Err("the link is not an http or https link, so it was not opened".to_string());
    }
    let status = tokio::process::Command::new(program)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map_err(|e| format!("could not run {program}: {e} ({FIX})"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program} exited with {} ({FIX})",
            status
                .code()
                .map_or_else(|| "a signal".to_string(), |c| format!("code {c}"))
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_links_are_handed_to_the_opener() {
        for ok in [
            "https://example.test/a?b=c",
            "HTTP://example.test",
            "http://127.0.0.1:9/x",
        ] {
            assert!(is_web_link(ok), "{ok}");
        }
        for bad in [
            "",
            "-a Safari",
            "--help",
            "file:///tmp/x",
            "ftp://example.test/x",
            "https://example.test/a b",
            "https://example.test/\u{1b}[31m",
            "javascript:void(0)",
        ] {
            assert!(!is_web_link(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_program_is_the_setting_unless_it_is_empty() {
        assert_eq!(program(Some("/opt/bin/my-opener")), "/opt/bin/my-opener");
        assert_eq!(program(Some("  ")), default_program());
        assert_eq!(program(Some("")), default_program());
        assert_eq!(program(None), default_program());
    }

    #[tokio::test]
    async fn a_link_that_is_not_a_web_link_is_not_opened_and_not_repeated() {
        let e = open_url("/bin/true", "-a Safari secret").await.unwrap_err();
        assert!(!e.contains("secret") && !e.contains("Safari"), "{e}");
    }
}
