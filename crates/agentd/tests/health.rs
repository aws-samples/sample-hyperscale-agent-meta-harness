//! /ping state.

use agentd::health::Health;
use harness_protocol::runtime::PingStatus;

#[test]
fn idle_is_healthy_and_a_guard_makes_it_busy_until_dropped() {
    let h = Health::new();
    assert_eq!(h.ping().status, PingStatus::Healthy);
    assert!(!h.is_busy());
    let g1 = Health::busy(&h);
    let g2 = Health::busy(&h);
    assert_eq!(h.ping().status, PingStatus::HealthyBusy);
    drop(g1);
    assert_eq!(
        h.ping().status,
        PingStatus::HealthyBusy,
        "one turn still running"
    );
    drop(g2);
    assert_eq!(h.ping().status, PingStatus::Healthy);
}

#[test]
fn time_of_last_update_moves_on_each_transition_and_is_unix_seconds() {
    let h = Health::new();
    let t0 = h.ping().time_of_last_update;
    assert!(t0 > 1_700_000_000, "unix seconds, got {t0}");
    let g = Health::busy(&h);
    let t1 = h.ping().time_of_last_update;
    assert!(t1 >= t0);
    drop(g);
    assert!(h.ping().time_of_last_update >= t1);
}
