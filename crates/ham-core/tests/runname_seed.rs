//! The seed name: the name a run shows before (or without) the utility model, built from the stored prompt head with no
//! model and no network. It is also the floor: a model that fails, a prompt that is too
//! short and a reply that the judge refuses all leave the seed.
//!
//! The other eighteen were produced by the code and read one by one (the list is in the report of the package that added them).

#![allow(clippy::unwrap_used)]

mod runname_common;

use ham_core::redact::redact_tokens;
use ham_core::run_index::{RunIndexItem, prompt_head};
use ham_core::runname::{UNTITLED, accept_stored, display_width, plan, seed_name};
use ham_core::sanitize::{marker, sanitize};
use harness_protocol::target::RunTarget;
use proptest::prelude::*;
use runname_common::*;

const R3_EXAMPLES: [(&str, &str); 9] = [
    ("c01", "Add a retry with exponential backoff"),
    ("c07", "Change the nightly export Lambda"),
    ("c10", "Make it better"),
    ("c11", "Refactor"),
    ("c12", "Fix the bug"),
    ("c15", "ユーザー認証のAPIにレート制限を追加し"),
    ("c16", "These two functions give the wrong answer"),
    ("c22", "Fix the login bug"),
    ("c24", "Write migration for the users table"),
];

fn seed_of_prompt(prompt: &str) -> String {
    seed_name(&prompt_head(prompt).0)
}

/// What every seed must be, whatever the input.
fn assert_safe_seed(input_label: &str, s: &str) {
    assert!(!s.is_empty(), "{input_label}: empty seed");
    assert_eq!(s.trim(), s, "{input_label}: {s:?}");
    assert!(
        display_width(s) <= 64,
        "{input_label}: {s:?} is {} columns",
        display_width(s)
    );
    let unspaced = s.chars().filter(|c| is_unspaced(*c)).count();
    let non_space = s.chars().filter(|c| !c.is_whitespace()).count();
    if unspaced * 2 >= non_space {
        assert!(
            non_space <= 20,
            "{input_label}: {s:?} has {non_space} characters of an unspaced script"
        );
    } else {
        assert!(s.split_whitespace().count() <= 7, "{input_label}: {s:?}");
    }
    assert_eq!(sanitize(s), s, "{input_label}: control characters in {s:?}");
    assert_eq!(
        redact_tokens(s),
        s,
        "{input_label}: a credential shape in {s:?}"
    );
    assert!(
        !s.chars().any(|c| marker(c).is_some()),
        "{input_label}: invisible character in {s:?}"
    );
    assert!(
        !s.chars().any(|c| "<>{}[]$\\@".contains(c)),
        "{input_label}: markup in {s:?}"
    );
    assert!(
        accept_stored(s),
        "{input_label}: {s:?} fails the stored-name checks"
    );
    let first = s.chars().next().unwrap();
    let upper: String = first.to_uppercase().collect();
    assert!(
        !first.is_lowercase() || upper == first.to_string(),
        "{input_label}: {s:?} starts in lower case"
    );
}

fn is_unspaced(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x30ff | 0x3400..=0x9fff | 0xe00..=0xe7f | 0x20000..=0x2fa1f)
}

#[test]
fn seed_goldens_for_the_corpus() {
    let golden = read_json("seeds.golden.json");
    let golden = golden.as_object().unwrap();
    let corpus = corpus();
    assert_eq!(
        golden.len(),
        corpus.len(),
        "every corpus prompt has a golden seed"
    );
    for (id, expected) in R3_EXAMPLES {
        assert_eq!(
            golden[id].as_str().unwrap(),
            expected,
            "{id}: the golden file departs from research section 7"
        );
    }
    for e in &corpus {
        let seed = seed_of_prompt(&e.prompt);
        assert_eq!(
            seed,
            golden[&e.id].as_str().unwrap(),
            "{} ({})",
            e.id,
            e.category
        );
        assert_safe_seed(&e.id, &seed);
    }
}

#[test]
fn seed_defects_stay_fixed() {
    // 1. A removed URL ends the clause: no dangling "in and follow".
    assert_eq!(
        seed_name(
            "Implement the feature described in https://example.com/issues/123 and follow the style guide at https://example.com/style"
        ),
        "Implement the feature described"
    );
    assert_eq!(
        seed_name("https://example.com/issues/123 add retry logic to the uploader"),
        "Add retry logic to the uploader"
    );
    assert_eq!(
        seed_name("Send the report to ops@example.com every night"),
        "Send the report"
    );
    // 2. Emphasis is stripped before the filler rule, so "**Please**" is filler.
    assert_eq!(
        seed_name(
            "**Please** create a \"hello world\" web server in Node.js with `express` and a /health route"
        ),
        "Create a hello world web server"
    );
    assert_eq!(
        seed_name("__Please__ add *retry* logic to the uploader"),
        "Add retry logic to the uploader"
    );
    // 3. An all-capitals result is lower-cased.
    assert_eq!(
        seed_name("BUILD ME A TODO APP IN REACT WITH LOCAL STORAGE!!!"),
        "Build me a todo app in react"
    );
    // 4. The first version took code for prose.
    let code = "def total(items):\n    t = 0\n    for i in items:\n        t += i.price\n    return t\n\nThis function gives the wrong answer for strings. Fix it.";
    assert_eq!(seed_name(code), "This function gives the wrong answer");
    let trace = "java.lang.NullPointerException\n\tat com.shop.cart.CartService.total(CartService.java:114)\nWhy does this happen when the cart is empty?";
    assert!(
        !seed_name(trace).contains("java.lang"),
        "{}",
        seed_name(trace)
    );
    // Fenced code is dropped, and a list marker is not part of the name.
    assert_eq!(
        seed_name("```\nrm_all()\n```\n- [ ] write the migration for users"),
        "Write the migration for users"
    );
    // The redaction marks are not words.
    assert_eq!(
        seed_name(&format!("Use {} to deploy the uploader", aws_key_id())),
        "Use to deploy the uploader"
    );
}

#[test]
fn seed_is_safe_for_any_input() {
    let esc = "\u{1b}[31m";
    let edge: Vec<(&str, String)> = vec![
        ("empty", String::new()),
        ("blanks", "  \n\t \n   ".to_string()),
        ("one character", "x".to_string()),
        ("NUL", "\u{0}".to_string()),
        ("NUL in a sentence", "Add\u{0}retry to the uploader code".to_string()),
        ("256 KiB of one word", "word ".repeat(52_000)),
        ("10,000 lines", "line of text\n".repeat(10_000)),
        ("only a code fence", "```\nfn main() {}\n```".to_string()),
        ("an unclosed code fence", "```\nfn main() {}\nAdd retry logic".to_string()),
        ("only a URL", "https://example.com/a/b?c=d".to_string()),
        ("only emoji", "\u{1f680}\u{1f525}\u{1f4a5}".to_string()),
        ("right to left", "مرحبا بالعالم هذا اختبار للنظام الجديد".to_string()),
        ("Han and Latin", "修复 login bug 的问题 please check the uploader code".to_string()),
        ("a title tag", "<title>NONE</title>".to_string()),
        ("escape sequences", format!("{esc}Add retry{esc} logic to the uploader {esc}now")),
        ("zero width characters", "Add\u{200b} retry\u{200d} logic to the uploader now\u{feff}".to_string()),
        ("bidi", "Add retry \u{202e}redaolpu\u{202c} logic to the uploader".to_string()),
        ("a key and nothing else", aws_key_id()),
        (
            "a key split by a zero width space",
            format!("Fix the leak of {}\u{200b}{} in the uploader now", &aws_key_id()[..6], &aws_key_id()[6..]),
        ),
        (
            "a key split by braces",
            format!("Fix the leak of {}{{}}{} in the uploader now", &aws_key_id()[..6], &aws_key_id()[6..]),
        ),
        ("a 200 character word", rep('q', 200)),
        ("a long word among others", format!("Fix {} soon please", rep('z', 60))),
        ("markup", "<b>Add</b> [retry]{logic} $HOME @user to the uploader".to_string()),
        ("only punctuation", "?!?! ... --- ***".to_string()),
        ("many combining marks", format!("Add re{}try logic to the uploader", "\u{301}".repeat(40))),
        ("wide characters", "ＡＢＣＤＥＦＧ ＨＩＪＫＬＭＮ ＯＰＱＲＳＴＵ ＶＷＸＹＺ ａｂｃｄｅｆ ｇｈｉｊｋｌ ｍｎｏｐ ｑｒｓ".to_string()),
        ("a heading and a list", "# Tasks\n\n1. Fix the login bug\n2. Update the README".to_string()),
    ];
    for (label, input) in edge {
        let seed = seed_name(&input);
        assert_safe_seed(label, &seed);
    }
    // Nothing usable gives the constant.
    for input in [
        "",
        "  \n ",
        "```\nfn main() {}\n```",
        "https://example.com/a",
        "\u{1f680}\u{1f525}",
        "\u{0}\u{1b}[31m",
        "?!?! ...",
    ] {
        assert_eq!(seed_name(input), UNTITLED, "{input:?}");
    }
    assert_eq!(seed_name(&aws_key_id()), UNTITLED);
}

fn markdown_ish() -> impl Strategy<Value = String> {
    let mut pieces: Vec<String> = vec![
        "# ",
        "## ",
        "- ",
        "* ",
        "1. ",
        "[ ] ",
        "> ",
        "```\n",
        "\n",
        "\n\n",
        "Please ",
        "Hey! ",
        "Could you ",
        "fix ",
        "add ",
        "the ",
        "retry ",
        "uploader ",
        "https://example.com/x ",
        "ops@example.com ",
        "**bold** ",
        "`code` ",
        "def f(x):\n",
        "{\"a\": 1}\n",
        "TODO.md:\n",
        "What I want: ",
        "ユーザー認証 ",
        "ありがとう ",
        "\u{1f680} ",
        "\u{1b}[31m",
        "\u{200b}",
        "ALL CAPS HERE ",
        "a ",
        "of ",
        "and ",
        "x",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    // A key shape, built at run time from pieces.
    pieces.push(format!("{} ", aws_key_id()));
    // The same key split by an invisible character and by braces: the redactor does not see it, the charset step joins it again.
    let key = aws_key_id();
    pieces.push(format!("{}\u{200b}{} ", &key[..6], &key[6..]));
    pieces.push(format!("{}{{}}{} ", &key[..6], &key[6..]));
    prop::collection::vec(prop::sample::select(pieces), 0..24).prop_map(|v| v.concat())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases(768)))]

    #[test]
    fn seed_is_safe_for_any_generated_input(input in prop_oneof![markdown_ish(), ".{0,400}", any::<String>()]) {
        let seed = seed_name(&input);
        assert_safe_seed("generated", &seed);
        // The seed depends on the first 2,000 characters only.
        let head = prompt_head(&input).0;
        prop_assert_eq!(seed_name(&head), seed);
    }
}

#[test]
fn client_and_server_seeds_are_equal() {
    // The client computes the seed of a run it has just started from `prompt_head(prompt)`; the server computes it from the
    // `promptHead` of the stored search item. Both feed one function the same bytes.
    for e in corpus() {
        let client = seed_name(&prompt_head(&e.prompt).0);
        let item = RunIndexItem::at_start(
            "01M5287J00000000000000000A",
            "2026-10-20T12:00:00Z",
            &e.prompt,
            None,
            &RunTarget::S3Only,
        );
        let server = seed_name(&item.prompt_head);
        assert_eq!(client, server, "{}", e.id);
        // And the whole prompt gives the same seed as its head.
        assert_eq!(seed_name(&e.prompt), client, "{}", e.id);
    }
    // A prompt past the head.
    let long = format!(
        "Add retry logic to the uploader. {}",
        "More background text. ".repeat(500)
    );
    let item = RunIndexItem::at_start(
        "01M5287J00000000000000000B",
        "2026-10-20T12:00:00Z",
        &long,
        None,
        &RunTarget::S3Only,
    );
    assert!(item.prompt_truncated);
    assert_eq!(seed_name(&item.prompt_head), seed_name(&long));
}

#[test]
fn key_straddling_character_2000() {
    let filler = "lorem ipsum dolor sit amet ";
    let pad = |n: usize| filler.repeat(n / filler.len() + 1)[..n].to_string();
    let key = aws_key_id();
    // The key starts at character 1,991: the head of 2,000 characters holds its first nine.
    let prompt = format!("{} {} {}", pad(1990), key, pad(1500));
    let head = prompt_head(&prompt).0;
    assert_eq!(head.chars().count(), 2000);
    assert!(
        head.contains(&key[..9]),
        "the test needs half a key in the head"
    );
    assert!(!head.contains(&key));

    // The model never sees any 8-character piece of the key.
    let text = plan(&prompt).user_text;
    for w in windows(&key, 8) {
        assert!(!text.contains(&w), "piece {w} reached the model");
    }
    // The seed passes the redactor unchanged and holds no piece of it either.
    let seed = seed_name(&head);
    assert_eq!(redact_tokens(&seed), seed);
    for w in windows(&key, 8) {
        assert!(!seed.contains(&w), "piece {w} in the seed {seed:?}");
    }
    // Half a key as the only word is not a name.
    let half = format!("{} {}", &key[..10], "tail");
    let s = seed_name(&half);
    assert_eq!(redact_tokens(&s), s);
}

#[test]
fn a_seed_obeys_the_count_rule_of_the_script_it_ends_in() {
    // The seed counts words (a spaced script) or characters (an unspaced script, at most 20). The rule was chosen from the script of the
    // line before the width cut and the cut of the last words, but those can turn a mostly Latin line into a mostly Japanese one, and a
    // cut at 20 characters can do the opposite. The first input is the case that `seed_is_safe_for_any_generated_input` found at
    // PROPTEST_CASES=100000: the key is cut away by the width loop and what is left is Japanese, 24 characters long.
    let split_key = format!(
        "**bold** ユーザー認証 1. ユーザー認証 ユーザー認証 {}\u{200b}{} ",
        &aws_key_id()[..6],
        &aws_key_id()[6..]
    );
    let long_word = "bold ユーザー認証 1. ユーザー認証 ユーザー認証 abcdefghijklmnopqrstuvwxyz";
    let latin_head = "a b c d e f g h i j k l m n o p q r s t ユーザー認証ユーザー認証ユーザー認証ユーザー認証ユーザー認証ユーザー認証";
    for input in [split_key.as_str(), long_word, latin_head] {
        assert_safe_seed(input, &seed_name(input));
    }
    // A long word that the width loop cuts away leaves 24 characters of Japanese, and the 20-character rule then applies.
    assert_eq!(
        seed_name(long_word),
        "Bold ユーザー認証 1. ユーザー認証 ユー"
    );
}

#[test]
fn a_key_split_by_an_invisible_character_or_a_brace_never_reaches_the_seed() {
    // `redacted_prompt` hides a credential shape only where it can see one. A key with a zero width character or a brace inside is
    // no shape yet, and the charset step of the seed then deletes the extra character and joins the key again. The final shape check
    // of the seed (design 5.4, steps 3 to 5 over the result) is what stops it: the seed is the constant.
    let key = aws_key_id();
    let splitters = [
        "\u{200b}",
        "\u{200c}",
        "\u{200d}",
        "\u{2060}",
        "\u{feff}",
        "\u{3164}",
        "\u{ffa0}",
        "\u{34f}",
        "\u{115f}",
        "\u{e0041}",
        "{}",
        "{",
        "}",
    ];
    let mut checked = 0;
    for splitter in splitters {
        for at in 1..key.len() {
            let prompt = format!(
                "Fix the leak of {}{splitter}{} in the uploader now",
                &key[..at],
                &key[at..]
            );
            let seed = seed_name(&prompt);
            let label = format!("splitter {splitter:?} at {at}");
            assert!(!seed.contains(&key), "{label}: the key is in {seed:?}");
            for w in windows(&key, 8) {
                assert!(!seed.contains(&w), "{label}: piece {w} in {seed:?}");
            }
            assert_eq!(redact_tokens(&seed), seed, "{label}: {seed:?}");
            assert_eq!(seed, UNTITLED, "{label}");
            assert_safe_seed(&label, &seed);
            checked += 1;
        }
    }
    assert_eq!(checked, splitters.len() * (key.len() - 1));
}
