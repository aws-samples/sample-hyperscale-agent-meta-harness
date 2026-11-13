//! No production code asks for the label of an outcome without naming the engine of the run. The one-argument forms (`label`, `label_for`) are the Cline
//! forms; a writer or reader in a crate's `src/` that used one would print a Cline sentence for a run of another engine. The scan reads every `src/` file of
//! every crate. Two places are allowed, each with its reason.

#![allow(clippy::unwrap_used)]

/// The files that may use the Cline form, and why.
const ALLOWED: &[(&str, &str)] = &[
    (
        "harness-protocol/src/direct.rs",
        "defines the Cline forms and the engine-aware ones",
    ),
    (
        "ham-core/src/state.rs",
        "the fallback for a run.completed event that carries no label (events written before labels existed): every such run was a Cline run",
    ),
    (
        "ham-e2e/src/",
        "the live test drivers compare the Cline sentences and run Cline only; they become engine-aware when they gain an engine option",
    ),
];

#[test]
fn b788_the_production_code_never_asks_for_the_label_without_an_engine() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut hits = Vec::new();
    for krate in std::fs::read_dir(&root).unwrap().flatten() {
        let src = krate.path().join("src");
        if src.is_dir() {
            scan(&src, &mut hits);
        }
    }
    assert!(
        hits.is_empty(),
        "label without an engine in production code:\n{}",
        hits.join("\n")
    );
}

fn allowed(path: &std::path::Path) -> bool {
    let text = path.to_string_lossy().replace('\\', "/");
    ALLOWED.iter().any(|(part, _)| text.contains(part))
}

fn scan(dir: &std::path::Path, hits: &mut Vec<String>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            scan(&p, hits);
        } else if p.extension().is_some_and(|x| x == "rs") && !allowed(&p) {
            let text = std::fs::read_to_string(&p).unwrap();
            for (i, line) in text.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                let one_argument = line.contains(".label_for(");
                let sentence = line.contains("outcome.label()")
                    || line.contains("DirectOutcome::label")
                    || line.contains("Outcome::StoppedSessionLost.label()");
                if one_argument || sentence {
                    hits.push(format!("{}:{}: {}", p.display(), i + 1, line.trim()));
                }
            }
        }
    }
}

#[test]
fn b788_every_allowed_place_names_its_reason() {
    assert!(ALLOWED.iter().all(|(_, why)| why.len() > 20));
    assert_eq!(ALLOWED.len(), 3);
}
