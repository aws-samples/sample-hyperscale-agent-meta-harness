//! Repository-relative path globs for the validator's allowed paths: `**` spans segments, `*` and `?` stay in one.

use regex::Regex;

fn glob_regex(glob: &str) -> Regex {
    let mut re = String::from("^");
    let chars: Vec<char> = glob.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                // `**/` matches any number of leading segments, a trailing `**` anything.
                if chars.get(i + 2) == Some(&'/') {
                    re.push_str("(?:.*/)?");
                    i += 3;
                } else {
                    re.push_str(".*");
                    i += 2;
                }
                continue;
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    re.push('$');
    Regex::new(&re).unwrap_or_else(|e| panic!("glob {glob}: {e}"))
}

/// A repository-relative path against one glob: `**` spans segments, `*` and `?` stay in one.
pub fn glob_match(glob: &str, path: &str) -> bool {
    glob_regex(glob).is_match(path)
}
