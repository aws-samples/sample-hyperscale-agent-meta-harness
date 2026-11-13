//! The compiled `ham` when `api_endpoint` names a stage whose host does not resolve: `ham runs` prints one line on standard error that says the
//! stage address does not resolve, that the stage may have been removed or replaced, and the two ways forward, and exits 1. The line carries the
//! host, not the request.
#![allow(clippy::unwrap_used)]

mod home;
mod support;

#[tokio::test]
async fn ham_runs_on_a_stage_that_is_gone_names_both_ways_forward() {
    let h = home::home_with(
        "https://ham-stage-gone.invalid/direct",
        "stage-gone",
        "ham-test",
    );
    let o = home::ham(&h, &["runs"]).await;
    let err = home::text(&o.stderr);
    assert_eq!(o.status.code(), Some(1), "{err}");
    assert!(home::text(&o.stdout).is_empty());
    assert_eq!(
        err.trim_end(),
        "ham: the stage address does not resolve (ham-stage-gone.invalid). The stage may have been removed or replaced. Run `./setup.sh --status` to see the current endpoint, or `ham config set api_endpoint <url>` to set it."
    );
    assert!(!err.contains("network:"), "{err}");
    home::assert_no_secrets(&o);
}
