//! The catalog age limit of `ham doctor --catalog-max-age <hours>` reaches the catalog check through
//! `doctor::checks_at`, which is `doctor::checks` with the limit and the clock given. The clock is given so that the test does not depend on
//! how old the compiled-in model list is: `now` is the year 2100, so every record is old, and the only thing that changes the line is the
//! limit. The rule of the check itself (`catalog_check_limited`) is `tests/doctor_catalog.rs`; the flag through the binary is `tests/cli.rs`.
#![allow(clippy::unwrap_used)]

mod support;

use ham::api::{Api, Retry};
use ham::doctor::{self, Check};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use std::sync::Arc;
use std::time::Duration;
use support::{Fake, REGION, creds};

/// 2100-01-01T00:00:00Z.
const NOW_2100: u64 = 4_102_444_800;

fn api(fake: &Fake) -> Api<HttpsPlane> {
    let plane = HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap();
    Api::new(Arc::new(plane)).with_retry(Retry {
        attempts: 2,
        base: Duration::from_millis(10),
    })
}

fn catalog(d: &doctor::Doctor) -> &Check {
    d.checks.iter().find(|c| c.name == "catalog").unwrap()
}

#[tokio::test]
async fn the_limit_reaches_the_catalog_check_and_nothing_else_changes() {
    let fake = Fake::start().await;
    let api = api(&fake);

    let none = doctor::checks_at(&api, &fake.endpoint, "p", None, NOW_2100).await;
    let c = catalog(&none);
    assert!(c.pass && !c.warn, "no limit, no warning: {c:?}");
    assert!(!c.detail.contains("refresh them"), "{c:?}");

    let limited = doctor::checks_at(&api, &fake.endpoint, "p", Some(24), NOW_2100).await;
    let c = catalog(&limited);
    assert!(
        c.pass && c.warn,
        "a limit that the age exceeds warns: {c:?}"
    );
    assert!(c.detail.contains(" hours old (refresh them)"), "{c:?}");
    assert!(
        c.detail.starts_with("11 selectable rows, newest smoke "),
        "{c:?}"
    );

    // The hours become seconds with a saturating product, so a limit whose product does not fit in a u64 means "no record is old
    // enough". It must not overflow (a debug build panics) and must not wrap to a small limit (every record would then warn).
    // `1 << 63` hours wrap to 0 seconds, and `u64::MAX / 3600 + 1`, the first value that overflows, wraps to 3584 seconds.
    // `u64::MAX / 3600` is the largest value that still fits.
    for hours in [u64::MAX, 1u64 << 63, u64::MAX / 3600 + 1, u64::MAX / 3600] {
        let huge = doctor::checks_at(&api, &fake.endpoint, "p", Some(hours), NOW_2100).await;
        let c = catalog(&huge);
        assert!(c.pass && !c.warn, "{hours} hours must not warn: {c:?}");
    }

    // The checks, their order and every line but the catalog one are the same with and without a limit.
    let names = |d: &doctor::Doctor| d.checks.iter().map(|c| c.name).collect::<Vec<_>>();
    assert_eq!(
        names(&none),
        [
            "identity",
            "api",
            "versions",
            "catalog",
            "git-host",
            "allowlist"
        ]
    );
    assert_eq!(names(&limited), names(&none));
    for (a, b) in none.checks.iter().zip(&limited.checks) {
        if a.name != "catalog" {
            assert_eq!(a, b, "{}", a.name);
        }
    }

    // `checks` is the same as no limit and the clock.
    let plain = doctor::checks(&api, &fake.endpoint, "p").await;
    assert!(
        !catalog(&plain).warn,
        "the default never warns: {:?}",
        catalog(&plain)
    );
}
