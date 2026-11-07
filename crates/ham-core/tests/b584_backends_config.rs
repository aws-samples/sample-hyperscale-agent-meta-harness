//! The `[[backend]]` tables of `config.toml`, the key `default_backend`, and the list of backends the client builds from them.
#![allow(clippy::unwrap_used)]

use ham_core::backends::{DEFAULT_ALIAS, backend_defs, check_backends};
use ham_core::config::{get_key, parse_config, read_config, render_config, set_key, unset_key};
use ham_core::settings::{Key, ROWS, reference_markdown};

const ONE: &str =
    "apiEndpoint = \"https://one.example/direct\"\nregion = \"us-east-1\"\nawsProfile = \"one\"\n";

const TWO: &str = "apiEndpoint = \"https://one.example/direct\"\nregion = \"us-east-1\"\nawsProfile = \"one\"\nconfigVersion = 1\ndefaultBackend = \"work\"\n\n[[backend]]\nalias = \"work\"\napiEndpoint = \"https://work.example/direct\"\nregion = \"us-west-2\"\nawsProfile = \"work\"\ncustomerContent = true\n\n[[backend]]\nalias = \"spare\"\napiEndpoint = \"https://spare.example/direct\"\nawsProfile = \"spare\"\n";

#[test]
fn b584_a_file_with_no_backend_table_has_exactly_the_default_backend_and_renders_as_before() {
    let cfg = parse_config(ONE).unwrap();
    assert!(cfg.backend.is_none());
    let defs = backend_defs(&cfg);
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].alias, DEFAULT_ALIAS);
    assert_eq!(
        defs[0].api_endpoint.as_deref(),
        Some("https://one.example/direct")
    );
    assert_eq!(defs[0].region.as_deref(), Some("us-east-1"));
    assert_eq!(defs[0].aws_profile.as_deref(), Some("one"));
    assert!(!defs[0].customer_content, "the default is: not allowed");
    // Nothing new appears in what the writer renders for a one-backend config.
    let out = render_config(&cfg);
    assert!(!out.contains("backend"), "{out}");
    assert!(!out.contains("configVersion"), "{out}");
}

#[test]
fn b584_backend_tables_are_read_in_file_order_after_the_default() {
    let cfg = parse_config(TWO).unwrap();
    let defs = backend_defs(&cfg);
    let aliases: Vec<&str> = defs.iter().map(|d| d.alias.as_str()).collect();
    assert_eq!(aliases, ["default", "work", "spare"]);
    assert_eq!(
        defs[1].api_endpoint.as_deref(),
        Some("https://work.example/direct")
    );
    assert_eq!(defs[1].region.as_deref(), Some("us-west-2"));
    assert_eq!(defs[1].aws_profile.as_deref(), Some("work"));
    assert!(defs[1].customer_content);
    assert!(!defs[2].customer_content);
    assert_eq!(defs[2].region, None);
    assert_eq!(cfg.default_backend.as_deref(), Some("work"));
    assert!(check_backends(&cfg).is_empty());
}

#[test]
fn b584_tables_alone_make_a_config_without_a_default_backend() {
    let text = "configVersion = 1\n\n[[backend]]\nalias = \"a\"\napiEndpoint = \"https://a.example/direct\"\n";
    let cfg = parse_config(text).unwrap();
    let aliases: Vec<String> = backend_defs(&cfg).into_iter().map(|d| d.alias).collect();
    assert_eq!(aliases, ["a"]);
}

#[test]
fn b584_the_tables_survive_a_write_and_a_round_trip() {
    let cfg = parse_config(TWO).unwrap();
    let again = parse_config(&render_config(&cfg)).unwrap();
    assert_eq!(cfg, again);
}

#[test]
fn b584_a_backend_table_needs_a_valid_unique_alias_and_an_endpoint() {
    let bad = |body: &str| {
        let r = read_config(&format!("configVersion = 1\n\n{body}"));
        assert!(r.has_errors(), "no error for: {body}");
        r.render_errors()
    };
    let m = bad("[[backend]]\napiEndpoint = \"https://a.example/direct\"\n");
    assert!(m.contains("alias"), "{m}");
    let m = bad("[[backend]]\nalias = \"Work\"\napiEndpoint = \"https://a.example/direct\"\n");
    assert!(m.contains("alias"), "{m}");
    let m = bad("[[backend]]\nalias = \"default\"\napiEndpoint = \"https://a.example/direct\"\n");
    assert!(m.contains("default") && m.contains("reserved"), "{m}");
    let m = bad("[[backend]]\nalias = \"a\"\n");
    assert!(m.contains("apiEndpoint"), "{m}");
    let m = bad(
        "[[backend]]\nalias = \"a\"\napiEndpoint = \"https://a.example/direct\"\n[[backend]]\nalias = \"a\"\napiEndpoint = \"https://b.example/direct\"\n",
    );
    assert!(m.contains("twice") || m.contains("repeats"), "{m}");
    let m =
        bad("[[backend]]\nalias = \"a\"\napiEndpoint = \"https://a.example/direct\"\nbogus = 1\n");
    assert!(m.contains("bogus"), "{m}");
    let m = bad(
        "[[backend]]\nalias = \"a\"\napiEndpoint = \"https://a.example/direct\"\ncustomerContent = \"yes\"\n",
    );
    assert!(m.contains("customerContent"), "{m}");
    let m = bad("backend = 3\n");
    assert!(m.contains("backend"), "{m}");
}

#[test]
fn b584_default_backend_is_a_registered_text_setting_and_must_name_a_backend() {
    let key = Key::lookup("default_backend").expect("registered");
    assert_eq!(key.row().file, ("", "defaultBackend"));
    assert!(ROWS.iter().any(|r| r.key == key));
    assert!(reference_markdown().contains("`defaultBackend`"));
    let cfg = parse_config(TWO).unwrap();
    assert_eq!(
        get_key(&cfg, "default_backend").unwrap().as_deref(),
        Some("work")
    );
    let cfg = set_key(&cfg, "default_backend", "spare").unwrap();
    assert_eq!(cfg.default_backend.as_deref(), Some("spare"));
    let e = set_key(&cfg, "default_backend", "nonesuch")
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("nonesuch") && e.contains("work") && e.contains("spare"),
        "{e}"
    );
    // `default` is always a valid choice when the top-level keys name a stage.
    assert!(set_key(&cfg, "default_backend", "default").is_ok());
    let cfg = unset_key(&cfg, "default_backend").unwrap();
    assert_eq!(cfg.default_backend, None);
}

#[test]
fn b584_a_default_backend_that_names_no_backend_is_a_finding_of_the_reader() {
    let r = read_config(
        "configVersion = 1\ndefaultBackend = \"nonesuch\"\n\n[[backend]]\nalias = \"a\"\napiEndpoint = \"https://a.example/direct\"\n",
    );
    assert!(r.has_errors());
    assert!(
        r.render_errors().contains("nonesuch"),
        "{}",
        r.render_errors()
    );
}

#[test]
fn b584_tables_and_the_key_ask_for_a_config_version() {
    let cfg = parse_config(ONE).unwrap();
    let cfg = set_key(&cfg, "default_backend", "default").unwrap();
    assert!(render_config(&cfg).contains("configVersion = 1"));
    let cfg = parse_config("configVersion = 1\n\n[[backend]]\nalias = \"a\"\napiEndpoint = \"https://a.example/direct\"\n").unwrap();
    assert_eq!(cfg.backend.as_ref().map(Vec::len), Some(1));
}

#[test]
fn b584_two_backends_with_one_address_are_a_problem_of_the_list() {
    let text = "apiEndpoint = \"https://one.example/direct\"\nconfigVersion = 1\n\n[[backend]]\nalias = \"again\"\napiEndpoint = \"HTTPS://one.example/direct/\"\n";
    let cfg = parse_config(text).unwrap();
    let problems = check_backends(&cfg);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains("default")
            && problems[0].contains("again")
            && problems[0].contains("apiEndpoint"),
        "{problems:?}"
    );
    assert!(check_backends(&parse_config(TWO).unwrap()).is_empty());
}

#[test]
fn b584_more_faults_of_a_backend_table_are_named() {
    let bad = |body: &str| {
        let r = read_config(&format!("configVersion = 1\n\n{body}"));
        assert!(r.has_errors(), "no error for: {body}");
        r.render_errors()
    };
    let m = bad("backend = [1]\n");
    assert!(m.contains("list of tables"), "{m}");
    let m = bad("[[backend]]\nalias = 5\napiEndpoint = \"https://a.example/direct\"\n");
    assert!(m.contains("alias") && m.contains("text in quotes"), "{m}");
    let m = bad("[[backend]]\nalias = \"a\"\napiEndpoint = 7\n");
    assert!(
        m.contains("apiEndpoint") && m.contains("text in quotes"),
        "{m}"
    );
    let m = bad("[[backend]]\nalias = \"a\"\napiEndpont = \"https://a.example/direct\"\n");
    assert!(m.contains("did you mean `apiEndpoint`"), "{m}");
    let m = bad("[[backend]]\nalias = \"a\"\napiEndpoint = \"\"\n");
    assert!(m.contains("apiEndpoint"), "{m}");
    let long = "a".repeat(33);
    let m = bad(&format!(
        "[[backend]]\nalias = \"{long}\"\napiEndpoint = \"https://a.example/direct\"\n"
    ));
    assert!(m.contains("alias"), "{m}");
}

#[test]
fn b584_default_backend_default_needs_the_top_level_stage() {
    let cfg = parse_config("configVersion = 1\ndefaultBackend = \"default\"\n\n[[backend]]\nalias = \"a\"\napiEndpoint = \"https://a.example/direct\"\n").unwrap();
    let problems = check_backends(&cfg);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains("apiEndpoint is not set") && problems[0].contains("a"),
        "{problems:?}"
    );
    let cfg = parse_config("configVersion = 1\ndefaultBackend = \"default\"\napiEndpoint = \"https://a.example/direct\"\n").unwrap();
    assert!(check_backends(&cfg).is_empty());
}

#[test]
fn b584_customer_content_at_the_top_level_declares_the_default_backend() {
    let cfg = parse_config(
        "configVersion = 1\ncustomerContent = true\napiEndpoint = \"https://a.example/direct\"\n",
    )
    .unwrap();
    assert!(backend_defs(&cfg)[0].customer_content);
    let cfg = set_key(&parse_config(ONE).unwrap(), "customer_content", "on").unwrap();
    assert_eq!(
        get_key(&cfg, "customer_content").unwrap().as_deref(),
        Some("true")
    );
    assert!(render_config(&cfg).contains("customerContent = true"));
}
