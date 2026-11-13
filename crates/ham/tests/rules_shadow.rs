// COMPAT-CAH: this file names the folder `cah` of the earlier command name on purpose.
//! The rules, the skills and the engine file of the user are found through the same folder rule as `config.toml`, as a whole: an empty
//! folder or an empty `rules/` in `~/.config/ham` never hides the files in `~/.config/cah`, and a `~/.config/ham` that holds files wins as a
//! whole, never file by file.
#![allow(clippy::unwrap_used)]

mod home;
mod support;

const ENDPOINT: &str = "https://ham-rules-shadow.invalid/direct";

fn earlier_home(label: &str) -> home::Home {
    let h = home::home_with(ENDPOINT, label, "ham-test");
    std::fs::rename(h.config.join("ham"), h.config.join("cah")).unwrap();
    std::fs::create_dir_all(h.config.join("cah/rules")).unwrap();
    std::fs::write(
        h.config.join("cah/rules/00-my-rules.md"),
        "rule text from the earlier folder\n",
    )
    .unwrap();
    std::fs::write(
        h.config.join("cah/continue.md"),
        "continue text from the earlier folder\n",
    )
    .unwrap();
    h
}

#[tokio::test]
async fn rules_in_the_earlier_folder_are_shown_and_sent_while_the_new_folder_has_none() {
    let h = earlier_home("shadow-read");
    let o = home::ham(&h, &["rules", "show"]).await;
    let out = home::text(&o.stdout);
    assert_eq!(o.status.code(), Some(0), "{}", home::text(&o.stderr));
    assert!(out.contains("rule text from the earlier folder"), "{out}");
    assert!(
        out.contains("continue text from the earlier folder"),
        "{out}"
    );
}

#[tokio::test]
async fn an_empty_rules_folder_in_the_new_folder_does_not_shadow_the_earlier_rules() {
    let h = earlier_home("shadow-empty");
    std::fs::create_dir_all(h.config.join("ham/rules")).unwrap();
    std::fs::create_dir_all(h.config.join("ham/skills")).unwrap();
    let o = home::ham(&h, &["rules", "show"]).await;
    let out = home::text(&o.stdout);
    assert!(out.contains("rule text from the earlier folder"), "{out}");
}

#[tokio::test]
async fn a_new_folder_with_files_wins_as_a_whole_and_the_earlier_rules_are_not_read() {
    let h = earlier_home("shadow-whole");
    std::fs::create_dir_all(h.config.join("ham/rules")).unwrap();
    std::fs::write(
        h.config.join("ham/rules/00-my-rules.md"),
        "rule text from the new folder\n",
    )
    .unwrap();
    let o = home::ham(&h, &["rules", "show"]).await;
    let out = home::text(&o.stdout);
    assert!(out.contains("rule text from the new folder"), "{out}");
    assert!(!out.contains("earlier folder"), "{out}");
}

#[tokio::test]
async fn rules_init_with_only_the_earlier_folder_is_refused_and_writes_nothing() {
    let h = earlier_home("shadow-init");
    let o = home::ham(&h, &["rules", "init"]).await;
    assert_eq!(o.status.code(), Some(2), "{}", home::text(&o.stderr));
    assert!(home::text(&o.stderr).contains("ham config migrate"));
    assert!(!h.config.join("ham").exists());
}
