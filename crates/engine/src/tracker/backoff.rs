//! Announce backoff, interval clamping and jitter.
//!
//! A failed announce round waits `min(30 * 2^min(failures - 1, 6), 1800)`
//! seconds, with ±20% jitter. The exponent is capped at 6 and the multiplication
//! saturates, so `u32::MAX` failures still produce the 1800-second cap.
//!
//! Jitter uses one seed per tracker session, never a fresh OS call per failure.
//! If the OS entropy source fails, [`JitterSeed::generate`] falls back to a
//! documented seed derived from the info-hash; this is non-fatal and keeps
//! backoff deterministic instead of failing the torrent.

use std::time::Duration;

use tokio::time::Instant;

use crate::metainfo::InfoHash;

use super::{DEFAULT_INTERVAL_SECS, MAX_INTERVAL_SECS, MIN_INTERVAL_SECS};

/// First backoff delay in seconds.
pub const BASE_BACKOFF_SECS: u64 = 30;

/// Maximum backoff delay before jitter.
pub const MAX_BACKOFF_SECS: u64 = 1800;

/// Maximum exponent applied to the base delay.
pub const MAX_BACKOFF_EXPONENT: u32 = 6;

/// Jitter applied to the backoff, in percent.
pub const JITTER_PERCENT: u64 = 20;

/// Where a [`JitterSeed`] came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JitterSeedSource {
    /// The OS randomness source.
    Os,
    /// The documented info-hash fallback.
    InfoHash,
}

/// One tracker's jitter seed.
///
/// Create it once per tracker session and reuse it for every failure. The
/// source is exposed so a caller can log the non-fatal fallback without
/// logging the seed itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JitterSeed {
    value: u64,
    source: JitterSeedSource,
}

impl JitterSeed {
    /// Generates a seed from the OS, falling back to the info-hash.
    ///
    /// The fallback is used only when `getrandom` fails. It is not an error:
    /// a torrent with a weak jitter seed can still announce.
    pub fn generate(info_hash: InfoHash) -> Self {
        let mut bytes = [0u8; 8];
        if getrandom::fill(&mut bytes).is_ok() {
            return Self {
                value: u64::from_le_bytes(bytes),
                source: JitterSeedSource::Os,
            };
        }
        Self::info_hash_fallback(info_hash)
    }

    /// Returns the documented fallback seed derived from the info-hash.
    ///
    /// This is FNV-1a over the 20 info-hash bytes. It is deterministic and
    /// stable across runs; it is not a security primitive.
    pub fn info_hash_fallback(info_hash: InfoHash) -> Self {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in info_hash.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Self {
            value: hash,
            source: JitterSeedSource::InfoHash,
        }
    }

    /// Returns the raw seed value.
    pub const fn value(self) -> u64 {
        self.value
    }

    /// Returns where the seed came from.
    pub const fn source(self) -> JitterSeedSource {
        self.source
    }

    /// Builds the jitter source for this tracker session.
    pub const fn jitter(self) -> Jitter {
        Jitter::new(self.value)
    }
}

/// Deterministic jitter source for one tracker session.
#[derive(Clone, Debug)]
pub struct Jitter {
    state: u64,
}

impl Jitter {
    /// Creates a jitter source from a seed.
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Returns the backoff delay for `failures` consecutive failures.
    pub fn delay(&mut self, failures: u32) -> Duration {
        let exponent = failures.saturating_sub(1).min(MAX_BACKOFF_EXPONENT);
        let base = BASE_BACKOFF_SECS
            .saturating_mul(1u64 << exponent)
            .min(MAX_BACKOFF_SECS);

        let percent = self.next_percent();
        let delta = base.saturating_mul(percent.unsigned_abs()) / 100;
        let seconds = if percent >= 0 {
            base.saturating_add(delta)
        } else {
            base.saturating_sub(delta)
        };
        Duration::from_secs(seconds)
    }

    fn next_percent(&mut self) -> i64 {
        let value = splitmix64(&mut self.state);
        let range = JITTER_PERCENT * 2 + 1;
        (value % range) as i64 - JITTER_PERCENT as i64
    }
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut value = *state;
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

/// Returns the effective announce interval.
///
/// The interval is clamped to 60..86400 seconds; zero is treated as missing
/// and becomes 1800. `min_interval` can only raise the result, never lower it.
pub fn effective_interval(interval: Duration, min_interval: Option<Duration>) -> Duration {
    let interval = clamp_interval(interval);
    min_interval.map_or(interval, |minimum| interval.max(clamp_minimum(minimum)))
}

fn clamp_interval(interval: Duration) -> Duration {
    if interval.is_zero() {
        return Duration::from_secs(DEFAULT_INTERVAL_SECS);
    }
    interval.clamp(
        Duration::from_secs(MIN_INTERVAL_SECS),
        Duration::from_secs(MAX_INTERVAL_SECS),
    )
}

fn clamp_minimum(minimum: Duration) -> Duration {
    minimum.clamp(
        Duration::from_secs(MIN_INTERVAL_SECS),
        Duration::from_secs(MAX_INTERVAL_SECS),
    )
}

/// Adds a delay to an instant, returning `None` when unrepresentable.
///
/// Callers store `None` as "no timer": a deadline outside the representable
/// range cannot be reached by this process, so scheduling no wake-up is the
/// safe fallback. This cannot happen with a real monotonic clock and a
/// bounded delay; it exists for synthetic clocks and defense in depth.
pub(crate) fn checked_deadline(now: Instant, delay: Duration) -> Option<Instant> {
    now.checked_add(delay)
}

#[cfg(test)]
mod tests;
