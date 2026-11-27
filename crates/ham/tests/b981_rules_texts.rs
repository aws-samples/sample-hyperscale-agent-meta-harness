//! The two texts about rules files that a person reads with no run at hand name the engine his settings select for his next
//! run: the line of `ham rules init` for the nudge file, and the refusal of a rules file named AGENTS.md. With no `default_engine` in
//! `config.toml` both read as in release 5.34. The real `ham` binary in a temp HOME; no verb here needs the network or AWS.
#![allow(clippy::unwrap_used)]

mod home;
mod support;

use home::{Home, ham_env, text};
use std::process::Output;

/// What `default_engine` of the config file says, and the name the two texts must then carry.
const SETTINGS: [(Option<&str>, &str); 3] = [
    (None, "Cline"),
    (Some("cline"), "Cline"),
    (Some("opencode"), "OpenCode"),
];

fn bare(label: &str) -> Home {
    let root = support::scratch(label);
    let h = Home {
        config: root.join("config"),
        state: root.join("state"),
        cache: root.join("cache"),
        downloads: root.join("downloads"),
        root,
    };
    for d in [&h.config, &h.state, &h.cache, &h.downloads] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::create_dir_all(h.config.join("ham")).unwrap();
    h
}

async fn ham(h: &Home, args: &[&str]) -> Output {
    ham_env(h, args, &[]).await
}

/// A home whose config file selects `engine` the way a person does it, with `ham config set`.
async fn home_on(label: &str, engine: Option<&str>) -> Home {
    let h = bare(&format!("{label}-{}", engine.unwrap_or("unset")));
    if let Some(id) = engine {
        let o = ham(&h, &["config", "set", "default_engine", id]).await;
        assert_eq!(
            o.status.code(),
            Some(0),
            "{}{}",
            text(&o.stdout),
            text(&o.stderr)
        );
    }
    h
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b981_rules_init_says_which_engine_the_nudge_file_is_for() {
    for (engine, name) in SETTINGS {
        for args in [
            &["rules", "init"][..],
            &["rules", "init", "--pack", "none"][..],
        ] {
            let h = home_on(&format!("b981-init-{}", args.len()), engine).await;
            let o = ham(&h, args).await;
            assert_eq!(
                o.status.code(),
                Some(0),
                "{engine:?} {args:?}: {}{}",
                text(&o.stdout),
                text(&o.stderr)
            );
            let said = text(&o.stdout);
            let line = said
                .lines()
                .find(|l| l.starts_with("created ") && l.contains("continue.md"))
                .unwrap_or_else(|| {
                    panic!("{engine:?} {args:?}: no line for the nudge file:\n{said}")
                });
            assert!(
                line.ends_with(&format!(
                    "continue.md: what {name} is told when it stops without saying it is done. Change it with: ham rules edit continue"
                )),
                "{engine:?} {args:?}: {line}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b981_a_rules_file_named_agents_md_is_refused_in_the_words_of_the_engine_the_settings_select()
 {
    for (engine, name) in SETTINGS {
        let h = home_on("b981-reserved", engine).await;
        let o = ham(&h, &["rules", "init", "--pack", "none"]).await;
        assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
        let rules = h.config.join("ham").join("rules");
        assert!(rules.is_dir(), "{engine:?}: {}", rules.display());
        std::fs::write(rules.join("AGENTS.md"), "# my rules\n").unwrap();
        let want = format!(
            "rule \"AGENTS.md\" cannot be called AGENTS.md: {name} would load it as a repository instruction file; rename it"
        );
        // Reading the rules for a person: `ham rules show`.
        let o = ham(&h, &["rules", "show"]).await;
        assert_ne!(o.status.code(), Some(0), "{engine:?}: {}", text(&o.stdout));
        let said = text(&o.stderr);
        assert!(said.contains(&want), "{engine:?} show: {said}");
        // Making such a file: `ham rules edit AGENTS.md` is refused before an editor opens.
        let o = ham(&h, &["rules", "edit", "AGENTS.md"]).await;
        assert_ne!(o.status.code(), Some(0), "{engine:?}: {}", text(&o.stdout));
        let said = text(&o.stderr);
        assert!(said.contains(&want), "{engine:?} edit: {said}");
        // The install of the lifecycle pack reads the rules that are there before it writes.
        let o = ham(&h, &["rules", "init"]).await;
        assert_ne!(o.status.code(), Some(0), "{engine:?}: {}", text(&o.stdout));
        let said = text(&o.stderr);
        assert!(said.contains(&want), "{engine:?} init: {said}");
    }
}

/// A start names the engine of that start, the value the client sends with the new run: `--engine` on the line, else the settings.
#[test]
fn b981_a_start_refuses_a_rules_file_named_agents_md_in_the_words_of_the_engine_of_that_start() {
    for (engine, name) in [
        (None, "Cline"),
        (Some("cline"), "Cline"),
        (Some("opencode"), "OpenCode"),
    ] {
        let root = support::scratch("b981-start");
        let rules = root.join("ham").join("rules");
        std::fs::create_dir_all(&rules).unwrap();
        std::fs::write(rules.join("AGENTS.md"), "# my rules\n").unwrap();
        let config = root.display().to_string();
        let env = move |k: &str| (k == "XDG_CONFIG_HOME").then(|| config.clone());
        // The arguments as `resolve_run_settings` leaves them: they name the engine of the run when it is not the default engine.
        let args = ham_core::command::DirectArgs {
            prompt: "say hello".into(),
            target: ham_core::command::TargetArg::S3Only,
            base: None,
            model: None,
            rules: None,
            no_rules: false,
            continue_file: None,
            input_wait: None,
            skills: None,
            no_skills: true,
            continue_from: None,
            goal: None,
            research: None,
            allow_third_party: false,
            attach: Vec::new(),
            compute: None,
            max_run_hours: None,
            engine: engine.map(str::to_string),
            provider_sharing: None,
        };
        let refusal = match ham::rules_cmd::direct_inputs_for(&env, &args) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("{engine:?}: the rules were read"),
        };
        assert_eq!(
            refusal,
            format!(
                "rule \"AGENTS.md\" cannot be called AGENTS.md: {name} would load it as a repository instruction file; rename it"
            ),
            "{engine:?}"
        );
    }
}
