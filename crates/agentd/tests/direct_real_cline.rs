//! The rules preflight against the real Cline binary: `HAMH_CLINE_BIN` names a
//! linux-x64 build of the pinned fork.
//! `#[ignore]`: run with `--ignored` where the binary exists; the same checks run against the
//! fake engine's listing in `direct_invocation.rs` on every CI host.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::preflight::{check_rules_listing, run_listing};
use agentd::direct::request::sha256_hex;
use agentd::direct::rules_place::place_rules;
use agentd::engine::LaunchSpec;
use agentd::engine::direct::{rules_listing_argv, user_rules_dir};
use common::{git, scratch};
use harness_protocol::api::v1::RuleFile;
use std::path::PathBuf;

fn rule(name: &str, content: &str) -> RuleFile {
    RuleFile {
        path: name.into(),
        sha256: sha256_hex(content.as_bytes()),
        content: content.into(),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs HAMH_CLINE_BIN, a linux-x64 build of the pinned Cline fork"]
async fn the_real_listing_holds_the_placed_rules_by_stem_at_their_own_paths() {
    let bin = PathBuf::from(std::env::var("HAMH_CLINE_BIN").expect("HAMH_CLINE_BIN"));
    assert!(bin.is_file(), "{}", bin.display());
    let dir = scratch("real-cline-rules");
    let (config, home, repo) = (dir.join("config"), dir.join("home"), dir.join("repo"));
    for d in [&config, &home, &repo] {
        std::fs::create_dir_all(d).unwrap();
    }
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("AGENTS.md"), "Repo rule: keep functions short.\n").unwrap();
    std::fs::create_dir_all(repo.join(".clinerules")).unwrap();
    std::fs::write(
        repo.join(".clinerules/10-repo.md"),
        "Repo rule: use tabs.\n",
    )
    .unwrap();
    let placed = place_rules(
        &user_rules_dir(&config),
        &[
            rule(
                "00-my-rules.md",
                "User rule: write the failing test first.\n",
            ),
            rule("20-style.md", "User rule: no globals.\n"),
        ],
    )
    .unwrap();
    let spec = LaunchSpec {
        argv: rules_listing_argv("cline", bin.to_str().unwrap(), &config).unwrap(),
        env: [
            ("HOME", home.display().to_string()),
            ("CLINE_NO_AUTO_UPDATE", "1".to_string()),
            ("CLINE_SESSION_BACKEND_MODE", "local".to_string()),
            ("CLINE_PROVIDER", "bedrock".to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect(),
        cwd: repo.clone(),
        path: vec!["/usr/bin".into(), "/bin".into()],
        stderr_log: dir.join("stderr.log"),
        run_as: None,
        sandbox: None,
    };
    let listing = run_listing(&spec).await.unwrap();
    // NV-1: the listing names a rules-directory file by its stem and carries the path it read.
    let mine: Vec<_> = listing
        .iter()
        .filter(|r| r.path.starts_with(config.to_str().unwrap()))
        .collect();
    assert_eq!(mine.len(), 2, "{listing:?}");
    assert!(
        mine.iter().any(|r| r.name == "00-my-rules"
            && r.instructions == "User rule: write the failing test first."),
        "{listing:?}"
    );
    assert!(
        listing.iter().any(|r| r.path.ends_with("10-repo.md")),
        "repo rules are listed too: {listing:?}"
    );
    check_rules_listing(&placed, &listing).unwrap();

    // A rule the listing does not hold the same way fails the check.
    let mut changed = placed.clone();
    changed[0].text_sha256 = sha256_hex(b"something else");
    assert!(check_rules_listing(&changed, &listing).is_err());
}
