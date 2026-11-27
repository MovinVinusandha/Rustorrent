use std::time::Duration;

use tokio::time::Instant;

use crate::metainfo::InfoHash;

use super::{
    BASE_BACKOFF_SECS, JITTER_PERCENT, Jitter, JitterSeed, JitterSeedSource, MAX_BACKOFF_SECS,
    checked_deadline, effective_interval,
};

fn base_delay(failures: u32) -> u64 {
    let exponent = failures.saturating_sub(1).min(6);
    BASE_BACKOFF_SECS
        .saturating_mul(1u64 << exponent)
        .min(MAX_BACKOFF_SECS)
}

fn within_jitter(actual: Duration, base: u64) -> bool {
    let low = base * (100 - JITTER_PERCENT) / 100;
    let high = base * (100 + JITTER_PERCENT) / 100;
    (low..=high).contains(&actual.as_secs())
}

#[test]
fn the_backoff_schedule_runs_from_thirty_to_eighteen_hundred_seconds() {
    let mut jitter = Jitter::new(0x1234_5678_9abc_def0);

    for failures in 1..=12 {
        let delay = jitter.delay(failures);
        assert!(
            within_jitter(delay, base_delay(failures)),
            "failure {failures} produced {delay:?}, base {}",
            base_delay(failures)
        );
    }
}

#[test]
fn the_backoff_caps_at_u32_max_failures() {
    let mut jitter = Jitter::new(7);
    let delay = jitter.delay(u32::MAX);

    assert!(within_jitter(delay, MAX_BACKOFF_SECS));
    assert!(delay <= Duration::from_secs(MAX_BACKOFF_SECS * 120 / 100));
}

#[test]
fn the_jitter_sequence_is_reproducible_for_one_seed() {
    let mut first = Jitter::new(42);
    let mut second = Jitter::new(42);

    for _ in 0..100 {
        assert_eq!(first.next_percent(), second.next_percent());
    }
}

#[test]
fn the_info_hash_fallback_is_deterministic_and_distinct() {
    let first = JitterSeed::info_hash_fallback(InfoHash::from_bytes([1; 20]));
    let same = JitterSeed::info_hash_fallback(InfoHash::from_bytes([1; 20]));
    let other = JitterSeed::info_hash_fallback(InfoHash::from_bytes([2; 20]));

    assert_eq!(first, same);
    assert_ne!(first.value(), other.value());
    assert_eq!(first.source(), JitterSeedSource::InfoHash);
    assert_eq!(other.source(), JitterSeedSource::InfoHash);
}

#[test]
fn a_generated_seed_uses_the_os_when_it_is_available() {
    let seed = JitterSeed::generate(InfoHash::from_bytes([0; 20]));

    assert_eq!(seed.source(), JitterSeedSource::Os);
}

#[test]
fn the_effective_interval_is_clamped_and_zero_is_missing() {
    assert_eq!(
        effective_interval(Duration::ZERO, None),
        Duration::from_secs(1800)
    );
    assert_eq!(
        effective_interval(Duration::from_secs(10), None),
        Duration::from_secs(60)
    );
    assert_eq!(
        effective_interval(Duration::from_secs(100_000), None),
        Duration::from_secs(86_400)
    );
}

#[test]
fn min_interval_can_only_raise_the_effective_interval() {
    assert_eq!(
        effective_interval(Duration::from_secs(1800), Some(Duration::from_secs(3600))),
        Duration::from_secs(3600)
    );
    assert_eq!(
        effective_interval(Duration::from_secs(3600), Some(Duration::from_secs(60))),
        Duration::from_secs(3600)
    );
    assert_eq!(
        effective_interval(Duration::from_secs(1800), Some(Duration::ZERO)),
        Duration::from_secs(1800)
    );
}

#[test]
fn checked_deadline_returns_none_when_unrepresentable() {
    assert!(checked_deadline(Instant::now(), Duration::MAX).is_none());
}

#[tokio::test(start_paused = true)]
async fn a_large_clock_advance_still_produces_a_checked_deadline() {
    tokio::time::advance(Duration::from_secs(10 * 365 * 24 * 60 * 60)).await;

    let now = Instant::now();
    let deadline = checked_deadline(now, Duration::from_secs(1800)).expect("representable");

    assert_eq!(
        deadline.checked_duration_since(now),
        Some(Duration::from_secs(1800))
    );
}
