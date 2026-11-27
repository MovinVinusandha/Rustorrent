use std::time::Duration;

use tokio::time::Instant;

use super::{GATE_WAIT, HostGate, HostKey};
use crate::tracker::TrackerUrl;

fn key(text: &str) -> HostKey {
    HostKey::new(&TrackerUrl::parse(text).expect("valid tracker URL"))
}

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn the_gate_is_shareable_between_tasks() {
    assert_send_sync::<HostGate>();
}

#[tokio::test(start_paused = true)]
async fn a_host_and_port_has_at_most_one_attempt() {
    let gate = HostGate::new();
    let first = gate.acquire(key("udp://a.example:6969")).await;

    assert!(
        gate.acquire_for(key("udp://a.example:6969"), Duration::ZERO)
            .await
            .is_none()
    );

    drop(first);
    assert!(
        gate.acquire_for(key("udp://a.example:6969"), Duration::ZERO)
            .await
            .is_some()
    );
}

#[tokio::test(start_paused = true)]
async fn different_hosts_and_ports_do_not_share_a_gate() {
    let gate = HostGate::new();
    let _held = gate.acquire(key("udp://a.example:6969")).await;

    assert!(
        gate.acquire_for(key("udp://a.example:6970"), Duration::ZERO)
            .await
            .is_some()
    );
    assert!(
        gate.acquire_for(key("udp://b.example:6969"), Duration::ZERO)
            .await
            .is_some()
    );
}

#[tokio::test(start_paused = true)]
async fn a_busy_gate_times_out_after_the_wait_budget() {
    let gate = HostGate::new();
    let _held = gate.acquire(key("udp://a.example:6969")).await;
    let start = Instant::now();

    let permit = gate
        .acquire_for(key("udp://a.example:6969"), GATE_WAIT)
        .await;

    assert!(permit.is_none());
    assert!(start.elapsed() >= GATE_WAIT);
}

#[tokio::test(start_paused = true)]
async fn a_waiter_registered_before_the_release_is_woken() {
    let gate = HostGate::new();
    let first = gate.acquire(key("udp://a.example:6969")).await;
    let mut waiter = Box::pin(gate.acquire(key("udp://a.example:6969")));

    // Poll once so the waiter registers before the release, then leave it
    // suspended. This is the race that `Notify::notified` must close.
    tokio::select! {
        biased;
        _ = &mut waiter => panic!("acquired while the key was held"),
        _ = tokio::task::yield_now() => {}
    }

    drop(first);
    let permit = waiter.await;
    drop(permit);
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_waiter_leaves_the_gate_free() {
    let gate = HostGate::new();
    let first = gate.acquire(key("udp://a.example:6969")).await;
    let mut waiter = Box::pin(gate.acquire(key("udp://a.example:6969")));

    tokio::select! {
        biased;
        _ = &mut waiter => panic!("acquired while the key was held"),
        _ = tokio::task::yield_now() => {}
    }

    drop(waiter);
    drop(first);
    assert!(
        gate.acquire_for(key("udp://a.example:6969"), Duration::ZERO)
            .await
            .is_some()
    );
}

#[tokio::test(start_paused = true)]
async fn normalized_hosts_share_the_key() {
    let gate = HostGate::new();
    let _held = gate.acquire(key("udp://A.Example.:6969")).await;

    assert!(
        gate.acquire_for(key("udp://a.example:6969"), Duration::ZERO)
            .await
            .is_none()
    );
}

#[tokio::test(start_paused = true)]
async fn a_released_key_can_be_acquired_again() {
    let gate = HostGate::new();
    for _ in 0..3 {
        let permit = gate.acquire(key("udp://a.example:6969")).await;
        drop(permit);
    }
}
