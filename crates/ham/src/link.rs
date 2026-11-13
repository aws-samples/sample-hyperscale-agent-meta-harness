//! The local-time text of a download link's expiry for the dashboard's `Link` line: `06:12 MDT`. The zone's abbreviation comes from
//! the operating system's `date`, which reads `TZ` and the system zone (`date -r <seconds>` on macOS, `date -d @<seconds>` elsewhere), so
//! the client carries no zone database. When `date` cannot be run the text is the UTC time, which is still a time the reader can use.

use ham_core::render::utc_clock;
use std::process::{Command, Stdio};

/// `HH:MM ZONE` for Unix seconds in the operating system's local zone.
pub fn local_clock(epoch: i64) -> String {
    local_clock_with("date", epoch, None)
}

/// [`local_clock`] with another `date` program and, optionally, another zone (`TZ`): for tests.
pub fn local_clock_with(date_program: &str, epoch: i64, tz: Option<&str>) -> String {
    let at = if cfg!(target_os = "macos") {
        vec!["-r".to_string(), epoch.to_string()]
    } else {
        vec!["-d".to_string(), format!("@{epoch}")]
    };
    let mut c = Command::new(date_program);
    c.args(at)
        .arg("+%H:%M %Z")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    if let Some(tz) = tz {
        c.env("TZ", tz);
    }
    c.output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| looks_like_clock(s))
        .unwrap_or_else(|| utc_clock(epoch))
}

/// `HH:MM` and a zone abbreviation of letters, digits, `+` and `-` (`-03` is an abbreviation too).
fn looks_like_clock(s: &str) -> bool {
    let Some((time, zone)) = s.split_once(' ') else {
        return false;
    };
    let b = time.as_bytes();
    b.len() == 5
        && b[2] == b':'
        && b[..2].iter().chain(&b[3..]).all(u8::is_ascii_digit)
        && (1..=8).contains(&zone.len())
        && zone
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_text_has_a_time_and_a_zone() {
        for ok in ["06:12 MDT", "12:12 UTC", "23:59 -03", "00:00 +0530"] {
            assert!(looks_like_clock(ok), "{ok}");
        }
        for bad in [
            "",
            "6:12 MDT",
            "06:12",
            "06:12 ",
            "06-12 MDT",
            "06:12 not a zone",
            "date: invalid",
        ] {
            assert!(!looks_like_clock(bad), "{bad:?}");
        }
    }

    #[test]
    fn local_clock_never_fails() {
        let text = local_clock(1_760_000_000);
        assert!(looks_like_clock(&text), "{text}");
    }
}
