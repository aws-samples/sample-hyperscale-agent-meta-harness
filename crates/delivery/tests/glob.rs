//! The validator's path globs.

use delivery::glob::glob_match;

#[test]
fn globs_match_segments_and_double_stars() {
    for (g, p, want) in [
        ("tests/**", "tests/test_calc.py", true),
        ("tests/**", "tests/unit/test_a.py", true),
        ("tests/**", "calc/tests.py", false),
        ("**/test_*.py", "test_calc.py", true),
        ("**/test_*.py", "a/b/test_calc.py", true),
        ("**/test_*.py", "a/b/calc_test.py", false),
        ("src/*.py", "src/a.py", true),
        ("src/*.py", "src/a/b.py", false),
        ("**", "anything/at/all.txt", true),
        ("!.git/**", "x.py", false),
        (".git/**", ".git/config", true),
        ("*.md", "README.md", true),
        ("*.md", "docs/README.md", false),
        ("**/*.md", "docs/README.md", true),
        ("calc/?.py", "calc/a.py", true),
        ("calc/?.py", "calc/ab.py", false),
    ] {
        assert_eq!(glob_match(g, p), want, "{g} {p}");
    }
}
