// The shape check of the root `VERSION` file. `build.rs` includes this file and the library exports it as `version_check`, so the one
// function runs at build time and under test. Plain comments only: an inner doc comment cannot be included.

/// The version text of the root `VERSION` file: its content with the surrounding white space removed, which must be one or more ASCII digits,
/// a dot and exactly two ASCII digits (`^[0-9]+\.[0-9]{2}$`). The error names the value that was refused and the form expected.
pub fn check_version(raw: &str) -> Result<&str, String> {
    let value = raw.trim();
    let ascii_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    match value.split_once('.') {
        Some((release, counter))
            if ascii_digits(release) && counter.len() == 2 && ascii_digits(counter) =>
        {
            Ok(value)
        }
        _ => Err(format!(
            "{value:?} is not a version of the form N.NN (digits, a dot, two digits; for example 4.01)"
        )),
    }
}
