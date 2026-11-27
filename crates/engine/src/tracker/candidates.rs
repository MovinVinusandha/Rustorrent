//! BEP 12 tracker candidate construction.
//!
//! The metainfo parser keeps `announce` and `announce-list` as strings. This
//! module validates them with [`TrackerUrl`], applies the session caps, removes
//! duplicates after normalization, and shuffles each tier once at load.
//!
//! `announce-list` is used when it contains at least one valid URL; otherwise
//! the single `announce` URL is used as a one-tracker list. A valid list is
//! never merged with `announce`, even when the list has only one URL. This is
//! the documented deviation from BEP 12's "ignore `announce`" rule: a damaged
//! list falls back, a usable list wins.
//!
//! For a private torrent the capped tiers are flattened into one ordered list
//! and shuffled once; the state machine then keeps exactly one active tracker.

use std::collections::HashSet;

use crate::metainfo::MetaInfo;

use super::{TrackerError, TrackerUrl};

/// Maximum tracker URLs kept for one torrent.
pub const MAX_TRACKER_URLS: usize = 32;

/// Maximum tiers kept for one torrent.
pub const MAX_TIERS: usize = 8;

/// Maximum URLs kept in one tier.
pub const MAX_URLS_PER_TIER: usize = 4;

/// Counters for candidate entries that were not kept.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CandidateWarnings {
    /// Entries that failed URL validation or used an unsupported scheme.
    pub invalid_urls: u32,
    /// Normalized duplicates removed, keeping the first occurrence.
    pub duplicate_urls: u32,
    /// Valid entries dropped by the 32-URL or 4-per-tier cap.
    pub dropped_urls: u32,
    /// Tiers removed because they were empty or beyond the 8-tier cap.
    pub dropped_tiers: u32,
}

/// Reorders one tier before the session starts.
///
/// Production uses [`OsShuffle`]. Tests inject a fixed order so the expected
/// candidate order can be asserted exactly.
pub trait Shuffle {
    /// Reorders `tier` in place.
    fn shuffle(&mut self, tier: &mut Vec<TrackerUrl>);
}

/// Shuffles a tier with OS randomness.
///
/// If the OS entropy source fails, the tier keeps its document order. That is
/// non-fatal: a stable order is valid BEP 12 behaviour, just less fair.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsShuffle;

impl Shuffle for OsShuffle {
    fn shuffle(&mut self, tier: &mut Vec<TrackerUrl>) {
        if tier.len() < 2 {
            return;
        }

        let mut seed = [0u8; 8];
        if getrandom::fill(&mut seed).is_err() {
            return;
        }
        let mut state = u64::from_le_bytes(seed);

        for index in (1..tier.len()).rev() {
            let value = splitmix64(&mut state);
            let swap = (value % (index as u64 + 1)) as usize;
            tier.swap(index, swap);
        }
    }
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut value = *state;
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

/// Validated tracker candidates in BEP 12 tier order.
///
/// A public torrent keeps its tiers. A private torrent has exactly one tier
/// holding the flattened, shuffled candidate list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackerCandidates {
    tiers: Vec<Vec<TrackerUrl>>,
    private: bool,
    warnings: CandidateWarnings,
}

impl TrackerCandidates {
    /// Builds candidates from one torrent's metadata.
    ///
    /// Returns [`TrackerError::NoUsableTracker`] when neither a valid
    /// `announce-list` entry nor a valid `announce` URL remains.
    pub fn from_metainfo(meta: &MetaInfo, shuffle: &mut dyn Shuffle) -> Result<Self, TrackerError> {
        let mut warnings = CandidateWarnings::default();
        let mut tiers = Vec::new();
        let mut seen = HashSet::new();
        let mut total = 0usize;
        let mut list_has_valid = false;

        for raw_tier in &meta.announce_list {
            let mut tier = Vec::new();
            for text in raw_tier {
                let url = match TrackerUrl::parse(text) {
                    Ok(url) => url,
                    Err(_) => {
                        warnings.invalid_urls = warnings.invalid_urls.saturating_add(1);
                        continue;
                    }
                };
                list_has_valid = true;

                if tier.len() >= MAX_URLS_PER_TIER || total + tier.len() >= MAX_TRACKER_URLS {
                    warnings.dropped_urls = warnings.dropped_urls.saturating_add(1);
                    continue;
                }
                if !seen.insert(url.clone()) {
                    warnings.duplicate_urls = warnings.duplicate_urls.saturating_add(1);
                    continue;
                }
                tier.push(url);
            }

            if tier.is_empty() {
                warnings.dropped_tiers = warnings.dropped_tiers.saturating_add(1);
                continue;
            }
            if tiers.len() >= MAX_TIERS {
                warnings.dropped_tiers = warnings.dropped_tiers.saturating_add(1);
                warnings.dropped_urls = warnings
                    .dropped_urls
                    .saturating_add(u32::try_from(tier.len()).unwrap_or(u32::MAX));
                continue;
            }

            total += tier.len();
            tiers.push(tier);
        }

        if !list_has_valid && let Some(text) = meta.announce.as_deref() {
            match TrackerUrl::parse(text) {
                Ok(url) => tiers.push(vec![url]),
                Err(_) => {
                    warnings.invalid_urls = warnings.invalid_urls.saturating_add(1);
                }
            }
        }

        if tiers.is_empty() {
            return Err(TrackerError::NoUsableTracker);
        }

        if meta.private {
            let mut flattened: Vec<TrackerUrl> = tiers.into_iter().flatten().collect();
            shuffle.shuffle(&mut flattened);
            tiers = vec![flattened];
        } else {
            for tier in &mut tiers {
                shuffle.shuffle(tier);
            }
        }

        Ok(Self {
            tiers,
            private: meta.private,
            warnings,
        })
    }

    /// Returns the tiers in try order.
    pub fn tiers(&self) -> &[Vec<TrackerUrl>] {
        &self.tiers
    }

    /// Returns `true` when the torrent is private.
    pub const fn is_private(&self) -> bool {
        self.private
    }

    /// Returns the counters for entries that were not kept.
    pub const fn warnings(&self) -> CandidateWarnings {
        self.warnings
    }

    /// Returns the number of kept URLs.
    pub fn len(&self) -> usize {
        self.tiers.iter().map(Vec::len).sum()
    }

    /// Returns `true` when no URL was kept.
    ///
    /// [`Self::from_metainfo`] never returns an empty value; this exists for
    /// callers that construct or filter candidates themselves.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Iterates over every kept URL in tier order.
    pub fn iter(&self) -> impl Iterator<Item = &TrackerUrl> {
        self.tiers.iter().flatten()
    }
}

#[cfg(test)]
mod tests;
