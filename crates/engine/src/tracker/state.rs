//! Pure announce state machine.
//!
//! The state machine owns no task, socket or clock. The session supplies
//! `Instant` values, builds a request when [`Tracker::build_request`] returns
//! one, and reports the transport result through [`Tracker::handle_response`],
//! [`Tracker::handle_failure`] or [`Tracker::handle_cancel`]. This keeps every
//! timer deterministic under a paused clock.
//!
//! # Public torrents (BEP 12)
//!
//! A round starts at tier 0. Within a tier, every tracker whose own backoff
//! has elapsed is attempted in order, even after an earlier success. The round
//! stops after the first tier with a success; later tiers are backups. The
//! first successful tracker in that tier moves to the front, keeping the rest
//! in order. Failed trackers stay in place and are retried at their own
//! backoff. After a successful tier the next timer is the earliest
//! `next_attempt` up to that tier; after a round that exhausts every tier it
//! is the earliest over all slots.
//!
//! # Private torrents (BEP 27)
//!
//! Exactly one tracker is active. A success keeps it active. A failure switches
//! to the next candidate in order (wrapping), emits
//! [`TrackerEvent::TrackerSwitched`] with `drop_peers = true`, and waits for
//! that candidate's own backoff. The failed tracker is not retried before the
//! rest of the list has been tried.
//!
//! # Events and peers
//!
//! `stopped` wins over `completed`, and `completed` wins over `started`.
//! `started` and `completed` are sent at most once after a success; a failure
//! keeps them pending. `stopped` is terminal and is never retried after a
//! failure. An event request interrupts a public round; peers already emitted
//! stay valid and nothing is emitted twice.
//!
//! A public round merges peers from all successes in its tier, removes
//! duplicates, and caps the round at [`MAX_PEERS_PER_RESPONSE`] new peers.
//!
//! [`TrackerStatus`] is published as a small snapshot behind a
//! [`std::sync::RwLock`]. The lock is held only while copying the snapshot and
//! is never held across an `.await`; the state machine itself is synchronous.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::time::Instant;

use crate::metainfo::InfoHash;

use super::backoff::{Jitter, JitterSeed, checked_deadline, effective_interval};
use super::peers::MAX_PEERS_PER_RESPONSE;
use super::{
    AnnounceEvent, AnnounceRequest, AnnounceResponse, AnnounceStats, RedirectPolicy,
    TrackerCandidates, TrackerError, TrackerId, TrackerMessage, TrackerUrl,
};

/// Stable per-torrent inputs for every announce.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackerConfig {
    /// Raw v1 info-hash.
    pub info_hash: InfoHash,
    /// Our peer ID.
    pub peer_id: [u8; 20],
    /// Our TCP listen port.
    pub port: u16,
    /// Client key, reused across announces for one torrent.
    pub key: u32,
    /// Whether `completed` was already sent before this session started.
    pub completed_sent_at_start: bool,
}

/// One event emitted by the state machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrackerEvent {
    /// A tracker answered a non-terminal announce.
    AnnounceSucceeded {
        /// Effective interval for the next periodic announce.
        interval: Duration,
    },
    /// A tracker attempt failed.
    AnnounceFailed {
        /// The failure, with no tracker text in its diagnostics.
        error: TrackerError,
        /// Next attempt time, or `None` when the state is terminal or no
        /// deadline is representable.
        retry_at: Option<Instant>,
    },
    /// New peers for the peer set, deduplicated within a public round.
    PeersReceived(Vec<SocketAddr>),
    /// A tracker response carried a warning.
    Warning(TrackerMessage),
    /// The active tracker changed; private torrents must drop peers.
    TrackerSwitched {
        /// `true` when the session must drop every peer from the old tracker.
        drop_peers: bool,
    },
    /// No tracker candidate can be used.
    NoUsableTracker,
}

/// A small snapshot for the UI and the session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrackerStatus {
    /// Redacted active tracker URL.
    pub active_url: Option<TrackerUrl>,
    /// Last successful announce.
    pub last_success: Option<Instant>,
    /// Next announce deadline, when one is scheduled.
    pub next_announce: Option<Instant>,
    /// Last effective interval.
    pub last_interval: Option<Duration>,
    /// Consecutive failures of the active tracker.
    pub consecutive_failures: u32,
    /// Peers reported by the last successful round.
    pub peer_count: usize,
    /// Whether the last response carried a warning.
    pub last_warning: bool,
    /// Number of usable tracker URLs.
    pub tracker_count: usize,
    /// Whether the torrent is private.
    pub private: bool,
    /// Whether no further announce will be sent.
    pub terminal: bool,
}

/// A tracker that has answered at least one announce.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotifiedTracker {
    /// Redacted tracker URL.
    pub url: TrackerUrl,
    /// Tracker ID returned by this tracker, if any.
    pub tracker_id: Option<TrackerId>,
}

/// One candidate and its own backoff state.
struct Slot {
    url: TrackerUrl,
    failures: u32,
    next_attempt: Option<Instant>,
    tracker_id: Option<TrackerId>,
    notified: bool,
}

/// Cursor and peer accumulator for one public round.
struct Round {
    tier: usize,
    index: usize,
    first_success: Option<usize>,
    peers: HashSet<SocketAddr>,
}

/// One torrent's announce state.
pub struct Tracker {
    slots: Vec<Vec<Slot>>,
    config: TrackerConfig,
    private: bool,
    active: Option<(usize, usize)>,
    round: Option<Round>,
    next_announce: Option<Instant>,
    pending_event: Option<AnnounceEvent>,
    in_flight: bool,
    in_flight_slot: Option<(usize, usize)>,
    in_flight_event: Option<AnnounceEvent>,
    started_sent: bool,
    completed_sent: bool,
    terminal: bool,
    switch_pending: bool,
    last_success: Option<Instant>,
    last_interval: Option<Duration>,
    peer_count: usize,
    last_warning: bool,
    jitter: Jitter,
    status: Arc<RwLock<TrackerStatus>>,
}

impl Tracker {
    /// Creates a tracker that will send `started` at `now`.
    pub fn new(
        candidates: TrackerCandidates,
        config: TrackerConfig,
        now: Instant,
    ) -> Result<Self, TrackerError> {
        if candidates.is_empty() {
            return Err(TrackerError::NoUsableTracker);
        }

        let private = candidates.is_private();
        let slots: Vec<Vec<Slot>> = candidates
            .tiers()
            .iter()
            .map(|tier| {
                tier.iter()
                    .map(|url| Slot {
                        url: url.clone(),
                        failures: 0,
                        next_attempt: Some(now),
                        tracker_id: None,
                        notified: false,
                    })
                    .collect()
            })
            .collect();

        let active = slots
            .iter()
            .position(|tier| !tier.is_empty())
            .map(|tier| (tier, 0));
        if active.is_none() {
            return Err(TrackerError::NoUsableTracker);
        }

        let seed = JitterSeed::generate(config.info_hash);
        let completed_sent = config.completed_sent_at_start;
        let tracker = Self {
            slots,
            config,
            private,
            active,
            round: None,
            next_announce: Some(now),
            pending_event: Some(AnnounceEvent::Started),
            in_flight: false,
            in_flight_slot: None,
            in_flight_event: None,
            started_sent: false,
            completed_sent,
            terminal: false,
            switch_pending: false,
            last_success: None,
            last_interval: None,
            peer_count: 0,
            last_warning: false,
            jitter: seed.jitter(),
            status: Arc::new(RwLock::new(TrackerStatus::default())),
        };
        tracker.publish();
        Ok(tracker)
    }

    /// Returns a handle to the status snapshot.
    ///
    /// Readers take the lock only for a clone. The state machine never holds
    /// the lock across an `.await`, because it has no async code.
    pub fn status_handle(&self) -> Arc<RwLock<TrackerStatus>> {
        Arc::clone(&self.status)
    }

    /// Returns a cloned status snapshot.
    pub fn status(&self) -> TrackerStatus {
        match self.status.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Returns the next timer, or `None` while an attempt is in flight.
    pub fn next_deadline(&self) -> Option<Instant> {
        if self.terminal || self.in_flight {
            return None;
        }
        self.next_announce
    }

    /// Returns `true` once no further announce will be sent.
    pub const fn is_terminal(&self) -> bool {
        self.terminal
    }

    /// Returns the trackers that have answered at least one announce.
    ///
    /// The tracker task uses this list to send `stopped` to every notified
    /// tracker; trackers that only ever failed are excluded.
    pub(crate) fn notified_trackers(&self) -> Vec<NotifiedTracker> {
        self.slots
            .iter()
            .flatten()
            .filter(|slot| slot.notified)
            .map(|slot| NotifiedTracker {
                url: slot.url.clone(),
                tracker_id: slot.tracker_id.clone(),
            })
            .collect()
    }

    /// Returns the stable per-torrent announce inputs.
    pub(crate) const fn config(&self) -> &TrackerConfig {
        &self.config
    }

    /// Returns `true` for a private torrent.
    pub(crate) const fn is_private(&self) -> bool {
        self.private
    }

    /// Marks the state terminal after the task's stop fan-out.
    pub(crate) fn mark_terminal(&mut self) {
        self.terminal = true;
        self.pending_event = None;
        self.round = None;
        self.next_announce = None;
        self.in_flight = false;
        self.in_flight_slot = None;
        self.in_flight_event = None;
        self.publish();
    }

    /// Builds the request that is due at `now`, if any.
    ///
    /// The caller must report the result through `handle_response`,
    /// `handle_failure` or `handle_cancel`; until then no second request is
    /// returned.
    pub fn build_request(&mut self, now: Instant, stats: AnnounceStats) -> Option<AnnounceRequest> {
        if self.terminal || self.in_flight {
            return None;
        }
        let due = self.next_announce?;
        if now < due {
            return None;
        }

        let event = self.pending_event.take().unwrap_or(AnnounceEvent::None);
        let slot = if event == AnnounceEvent::None {
            self.select_periodic_slot(now)
        } else {
            self.select_event_slot(now)
        };

        let Some(slot) = slot else {
            if event != AnnounceEvent::None {
                self.queue_pending(event);
            }
            self.publish();
            return None;
        };
        let Some(tracker) = self.slot(slot) else {
            if event != AnnounceEvent::None {
                self.queue_pending(event);
            }
            self.publish();
            return None;
        };
        let url = tracker.url.clone();
        let tracker_id = tracker.tracker_id.clone();

        let request = AnnounceRequest::new(
            url,
            self.config.info_hash,
            self.config.peer_id,
            self.config.port,
            stats,
            event,
            self.config.key,
            tracker_id,
            if self.private {
                RedirectPolicy::SameHostOnly
            } else {
                RedirectPolicy::Public
            },
        );

        self.in_flight = true;
        self.in_flight_slot = Some(slot);
        self.in_flight_event = Some(event);
        self.next_announce = None;
        self.publish();
        Some(request)
    }

    /// Reports a successful announce.
    pub fn handle_response(
        &mut self,
        now: Instant,
        response: AnnounceResponse,
    ) -> Vec<TrackerEvent> {
        if !self.in_flight {
            return Vec::new();
        }
        let Some(slot) = self.in_flight_slot.take() else {
            return Vec::new();
        };
        let event = self.in_flight_event.take().unwrap_or(AnnounceEvent::None);
        self.in_flight = false;

        let interval = effective_interval(response.interval, response.min_interval);
        let deadline = checked_deadline(now, interval);
        let tracker_id = response.tracker_id.clone();
        if let Some(tracker) = self.slot_mut(slot) {
            tracker.failures = 0;
            tracker.next_attempt = deadline;
            tracker.notified = true;
            if let Some(id) = tracker_id {
                tracker.tracker_id = Some(id);
            }
        }
        self.last_success = Some(now);
        self.last_interval = Some(interval);
        self.last_warning = response.warning.is_some();

        let peers = if event == AnnounceEvent::None && !self.private {
            self.merge_round_peers(&response.peers)
        } else {
            let peers = dedupe_peers(&response.peers);
            self.peer_count = peers.len();
            peers
        };

        let mut events = vec![TrackerEvent::AnnounceSucceeded { interval }];
        if !peers.is_empty() {
            events.push(TrackerEvent::PeersReceived(peers));
        }
        if let Some(warning) = response.warning {
            events.push(TrackerEvent::Warning(warning));
        }
        if let Some(drop_peers) = self.take_pending_switch() {
            events.push(TrackerEvent::TrackerSwitched { drop_peers });
        }
        self.set_active(slot, self.private, &mut events);

        match event {
            AnnounceEvent::Started => self.started_sent = true,
            AnnounceEvent::Completed => self.completed_sent = true,
            AnnounceEvent::Stopped => {
                self.terminal = true;
                self.pending_event = None;
                self.round = None;
                self.next_announce = None;
                self.publish();
                return events;
            }
            AnnounceEvent::None => {}
        }

        if event == AnnounceEvent::None {
            if self.private {
                self.next_announce = self.recompute_private();
            } else {
                self.advance_public_after_attempt(now, slot, true, &mut events);
            }
        } else {
            self.round = None;
            self.next_announce = if self.private {
                self.recompute_private()
            } else {
                let tier = self.active.map_or(0, |slot| slot.0);
                self.recompute_public_through(tier)
            };
        }

        if self.pending_event.is_some() {
            self.force_pending_event(now);
            self.next_announce = Some(now);
        }
        self.publish();
        events
    }

    /// Reports a failed attempt.
    pub fn handle_failure(&mut self, now: Instant, error: TrackerError) -> Vec<TrackerEvent> {
        if !self.in_flight {
            return Vec::new();
        }
        let Some(slot) = self.in_flight_slot.take() else {
            return Vec::new();
        };
        let event = self.in_flight_event.take().unwrap_or(AnnounceEvent::None);
        self.in_flight = false;

        if event == AnnounceEvent::Stopped {
            self.terminal = true;
            self.pending_event = None;
            self.round = None;
            self.next_announce = None;
            let mut events = vec![TrackerEvent::AnnounceFailed {
                error,
                retry_at: None,
            }];
            if let Some(drop_peers) = self.take_pending_switch() {
                events.push(TrackerEvent::TrackerSwitched { drop_peers });
            }
            self.publish();
            return events;
        }

        let failures = self
            .slot(slot)
            .map_or(1, |tracker| tracker.failures.saturating_add(1));
        let delay = self.jitter.delay(failures);
        if let Some(tracker) = self.slot_mut(slot) {
            tracker.failures = failures;
            tracker.next_attempt = checked_deadline(now, delay);
        }

        match event {
            AnnounceEvent::Started if !self.started_sent => {
                self.queue_pending(AnnounceEvent::Started);
            }
            AnnounceEvent::Completed if !self.completed_sent => {
                self.queue_pending(AnnounceEvent::Completed);
            }
            _ => {}
        }

        let mut events = Vec::new();
        if let Some(drop_peers) = self.take_pending_switch() {
            events.push(TrackerEvent::TrackerSwitched { drop_peers });
        }
        if self.private {
            self.round = None;
            if let Some(next) = self.next_private_slot(slot) {
                self.set_active(next, true, &mut events);
            }
            self.next_announce = self.recompute_private();
        } else if event == AnnounceEvent::None {
            self.advance_public_after_attempt(now, slot, false, &mut events);
        } else {
            self.round = None;
            if let Some(next) = self.find_next_due_after(slot, now) {
                self.set_active(next, false, &mut events);
                self.next_announce = Some(now);
            } else {
                self.next_announce = self.recompute_public();
            }
        }

        if self.pending_event == Some(AnnounceEvent::Stopped) {
            self.force_pending_event(now);
            self.next_announce = Some(now);
        }

        let retry_at = self.next_announce;
        events.insert(0, TrackerEvent::AnnounceFailed { error, retry_at });
        self.publish();
        events
    }

    /// Reports that the caller dropped the in-flight attempt.
    ///
    /// Cancellation is not a tracker failure: the in-flight event is queued
    /// again and retried at `now`, without increasing the failure count. A
    /// cancelled `stopped` attempt is terminal.
    pub fn handle_cancel(&mut self, now: Instant) -> Vec<TrackerEvent> {
        if !self.in_flight {
            return Vec::new();
        }
        let Some(slot) = self.in_flight_slot.take() else {
            return Vec::new();
        };
        let event = self.in_flight_event.take().unwrap_or(AnnounceEvent::None);
        self.in_flight = false;

        if event == AnnounceEvent::Stopped {
            self.terminal = true;
            self.pending_event = None;
            self.round = None;
            self.next_announce = None;
        } else {
            if event != AnnounceEvent::None {
                self.queue_pending(event);
            }
            if let Some(tracker) = self.slot_mut(slot) {
                tracker.next_attempt = Some(now);
            }
            self.next_announce = Some(now);
        }
        self.publish();
        self.take_pending_switch()
            .map(|drop_peers| vec![TrackerEvent::TrackerSwitched { drop_peers }])
            .unwrap_or_default()
    }

    /// Queues `completed` for the next attempt unless it was already sent.
    pub fn complete(&mut self, now: Instant) {
        self.queue_event(now, AnnounceEvent::Completed);
    }

    /// Queues `stopped`; the state becomes terminal after the attempt.
    pub fn stop(&mut self, now: Instant) {
        self.queue_event(now, AnnounceEvent::Stopped);
    }

    /// Queues an event, coalescing it with any event already waiting.
    pub fn queue_event(&mut self, now: Instant, event: AnnounceEvent) {
        if self.terminal || event == AnnounceEvent::None {
            return;
        }
        if event == AnnounceEvent::Started && self.started_sent {
            return;
        }
        if event == AnnounceEvent::Completed && self.completed_sent {
            return;
        }
        // A duplicate trigger while the same event is already in flight must
        // not queue a second copy: `started` and `completed` are sent at most
        // once, and `stopped` is terminal either way.
        if self.in_flight_event == Some(event) {
            return;
        }
        // `started` is lower priority than an in-flight `completed`; queuing
        // it now would send it after `completed` if that attempt succeeds.
        if event == AnnounceEvent::Started && self.in_flight_event == Some(AnnounceEvent::Completed)
        {
            return;
        }

        self.queue_pending(event);
        if !self.in_flight {
            self.round = None;
            self.force_pending_event(now);
            self.next_announce = Some(now);
        }
        self.publish();
    }

    fn queue_pending(&mut self, event: AnnounceEvent) {
        let current = self.pending_event.unwrap_or(AnnounceEvent::None);
        if event_priority(event) > event_priority(current) {
            self.pending_event = Some(event);
        }
    }

    fn select_event_slot(&mut self, now: Instant) -> Option<(usize, usize)> {
        let slot = if self.private {
            self.active.or_else(|| self.first_slot())
        } else {
            self.round = None;
            self.active
                .or_else(|| self.find_due_any(now))
                .or_else(|| self.first_slot())
        }?;

        if let Some(tracker) = self.slot_mut(slot) {
            tracker.next_attempt = Some(now);
        }
        self.mark_active(slot);
        Some(slot)
    }

    fn select_periodic_slot(&mut self, now: Instant) -> Option<(usize, usize)> {
        if self.private {
            let slot = self.active?;
            let due = self.slot(slot)?.next_attempt.is_some_and(|at| at <= now);
            return due.then_some(slot);
        }

        if self.round.is_none() {
            self.round = Some(Round {
                tier: 0,
                index: 0,
                first_success: None,
                peers: HashSet::new(),
            });
        }

        loop {
            let (tier, index) = self.round.as_ref().map(|round| (round.tier, round.index))?;
            if tier >= self.slots.len() {
                self.round = None;
                self.next_announce = self.recompute_public();
                return None;
            }

            if let Some(found) = self.find_due_in_tier(tier, index, now) {
                if let Some(round) = self.round.as_mut() {
                    round.index = found;
                }
                let slot = (tier, found);
                self.mark_active(slot);
                return Some(slot);
            }

            if let Some(round) = self.round.as_mut() {
                round.tier = tier.saturating_add(1);
                round.index = 0;
                round.first_success = None;
            }
        }
    }

    fn force_pending_event(&mut self, now: Instant) {
        let slot = if self.private {
            self.active.or_else(|| self.first_slot())
        } else {
            self.active
                .or_else(|| self.find_due_any(now))
                .or_else(|| self.first_slot())
        };
        if let Some(slot) = slot {
            if let Some(tracker) = self.slot_mut(slot) {
                tracker.next_attempt = Some(now);
            }
            self.mark_active(slot);
        }
    }

    fn advance_public_after_attempt(
        &mut self,
        now: Instant,
        slot: (usize, usize),
        success: bool,
        events: &mut Vec<TrackerEvent>,
    ) {
        let Some((tier, _, first_success)) = self
            .round
            .as_ref()
            .map(|round| (round.tier, round.index, round.first_success))
        else {
            self.next_announce = self.recompute_public();
            return;
        };
        if tier != slot.0 {
            self.next_announce = self.recompute_public();
            return;
        }

        let first_success = if success && first_success.is_none() {
            Some(slot.1)
        } else {
            first_success
        };
        if let Some(round) = self.round.as_mut() {
            round.index = slot.1.saturating_add(1);
            round.first_success = first_success;
        }
        self.advance_public_cursor(now, events);
    }

    fn advance_public_cursor(&mut self, now: Instant, events: &mut Vec<TrackerEvent>) {
        loop {
            let Some((tier, index)) = self.round.as_ref().map(|round| (round.tier, round.index))
            else {
                self.next_announce = self.recompute_public();
                return;
            };

            if tier >= self.slots.len() {
                self.round = None;
                self.next_announce = self.recompute_public();
                return;
            }

            let tier_len = self.slots.get(tier).map_or(0, Vec::len);
            if index < tier_len {
                if let Some(found) = self.find_due_in_tier(tier, index, now) {
                    if let Some(round) = self.round.as_mut() {
                        round.index = found;
                    }
                    self.set_active((tier, found), false, events);
                    self.next_announce = Some(now);
                    return;
                }

                let first_success = self.round.as_ref().and_then(|round| round.first_success);
                if let Some(first) = first_success {
                    self.finish_successful_tier(tier, first, events);
                    return;
                }

                if let Some(round) = self.round.as_mut() {
                    round.tier = tier.saturating_add(1);
                    round.index = 0;
                    round.first_success = None;
                }
                continue;
            }

            let first_success = self.round.as_ref().and_then(|round| round.first_success);
            if let Some(first) = first_success {
                self.finish_successful_tier(tier, first, events);
                return;
            }

            if let Some(round) = self.round.as_mut() {
                round.tier = tier.saturating_add(1);
                round.index = 0;
                round.first_success = None;
            }
        }
    }

    fn finish_successful_tier(
        &mut self,
        tier: usize,
        first: usize,
        events: &mut Vec<TrackerEvent>,
    ) {
        let old_url = self
            .active
            .and_then(|slot| self.slot(slot))
            .map(|slot| slot.url.clone());
        let moved_url = self
            .slots
            .get(tier)
            .and_then(|row| row.get(first))
            .map(|slot| slot.url.clone());
        let active_changes = old_url.is_some() && old_url != moved_url;
        if active_changes
            && let Some(old) = self.active
            && let Some(tracker) = self.slot_mut(old)
        {
            tracker.tracker_id = None;
        }

        let moved = if let Some(row) = self.slots.get_mut(tier) {
            if first < row.len() {
                let slot = row.remove(first);
                row.insert(0, slot);
                true
            } else {
                false
            }
        } else {
            false
        };

        if moved {
            self.active = Some((tier, 0));
            if active_changes {
                events.push(TrackerEvent::TrackerSwitched { drop_peers: false });
            }
        }

        self.round = None;
        self.next_announce = self.recompute_public_through(tier);
    }

    fn merge_round_peers(&mut self, peers: &[SocketAddr]) -> Vec<SocketAddr> {
        let mut batch = Vec::new();
        let total = {
            let Some(round) = self.round.as_mut() else {
                let peers = dedupe_peers(peers);
                self.peer_count = peers.len();
                return peers;
            };
            for peer in peers {
                if round.peers.len() >= MAX_PEERS_PER_RESPONSE {
                    break;
                }
                if round.peers.insert(*peer) {
                    batch.push(*peer);
                }
            }
            round.peers.len()
        };
        self.peer_count = total;
        batch
    }

    /// Makes `next` active and reports whether the URL actually changed.
    ///
    /// A switch clears the old tracker's ID: the next announce to it is a
    /// fresh registration. A same-URL move keeps the ID.
    fn activate(&mut self, next: (usize, usize)) -> bool {
        if self.active == Some(next) {
            return false;
        }
        let old_url = self
            .active
            .and_then(|slot| self.slot(slot))
            .map(|slot| slot.url.clone());
        let new_url = self.slot(next).map(|slot| slot.url.clone());
        let changed = old_url.is_some() && old_url != new_url;
        if changed
            && let Some(old) = self.active
            && let Some(tracker) = self.slot_mut(old)
        {
            tracker.tracker_id = None;
        }
        self.active = Some(next);
        changed
    }

    /// Changes the active slot and defers the switch event to the attempt
    /// result, which keeps event ordering stable.
    fn mark_active(&mut self, next: (usize, usize)) {
        if self.activate(next) {
            self.switch_pending = true;
        }
    }

    fn take_pending_switch(&mut self) -> Option<bool> {
        if self.switch_pending {
            self.switch_pending = false;
            Some(self.private)
        } else {
            None
        }
    }

    fn set_active(
        &mut self,
        next: (usize, usize),
        drop_peers: bool,
        events: &mut Vec<TrackerEvent>,
    ) {
        if self.activate(next) {
            events.push(TrackerEvent::TrackerSwitched { drop_peers });
        }
    }

    fn recompute_public(&self) -> Option<Instant> {
        self.slots
            .iter()
            .flatten()
            .filter_map(|slot| slot.next_attempt)
            .min()
    }

    /// Next deadline after a tier succeeded.
    ///
    /// Only tiers up to the successful one are considered. Later tiers have
    /// never been attempted and still hold their initial `now`, so including
    /// them would schedule an immediate backup round and defeat the rule that
    /// a round stops after the first successful tier.
    fn recompute_public_through(&self, tier: usize) -> Option<Instant> {
        self.slots
            .iter()
            .take(tier.saturating_add(1))
            .flatten()
            .filter_map(|slot| slot.next_attempt)
            .min()
    }

    fn recompute_private(&self) -> Option<Instant> {
        self.active
            .and_then(|slot| self.slot(slot))
            .and_then(|slot| slot.next_attempt)
    }

    fn find_due_in_tier(&self, tier: usize, start: usize, now: Instant) -> Option<usize> {
        self.slots
            .get(tier)?
            .iter()
            .enumerate()
            .skip(start)
            .find_map(|(index, slot)| {
                slot.next_attempt
                    .is_some_and(|at| at <= now)
                    .then_some(index)
            })
    }

    fn find_due_any(&self, now: Instant) -> Option<(usize, usize)> {
        self.slots.iter().enumerate().find_map(|(tier, row)| {
            row.iter().enumerate().find_map(|(index, slot)| {
                slot.next_attempt
                    .is_some_and(|at| at <= now)
                    .then_some((tier, index))
            })
        })
    }

    fn find_next_due_after(&self, current: (usize, usize), now: Instant) -> Option<(usize, usize)> {
        let due = |slot: &Slot| slot.next_attempt.is_some_and(|at| at <= now);

        let mut after = false;
        for (tier, row) in self.slots.iter().enumerate() {
            for (index, slot) in row.iter().enumerate() {
                if (tier, index) == current {
                    after = true;
                    continue;
                }
                if after && due(slot) {
                    return Some((tier, index));
                }
            }
        }

        // Wrap to the slots before `current`: a failure in a later tier must
        // not bypass an earlier tier whose backoff has already expired.
        for (tier, row) in self.slots.iter().enumerate() {
            for (index, slot) in row.iter().enumerate() {
                if (tier, index) == current {
                    return None;
                }
                if due(slot) {
                    return Some((tier, index));
                }
            }
        }
        None
    }

    fn next_private_slot(&self, current: (usize, usize)) -> Option<(usize, usize)> {
        let total: usize = self.slots.iter().map(Vec::len).sum();
        if total == 0 {
            return None;
        }
        let current = self.flat_index(current)?;
        self.slot_at_flat((current + 1) % total)
    }

    fn flat_index(&self, slot: (usize, usize)) -> Option<usize> {
        let row = self.slots.get(slot.0)?;
        if slot.1 >= row.len() {
            return None;
        }
        Some(self.slots.iter().take(slot.0).map(Vec::len).sum::<usize>() + slot.1)
    }

    fn slot_at_flat(&self, mut index: usize) -> Option<(usize, usize)> {
        for (tier, row) in self.slots.iter().enumerate() {
            if index < row.len() {
                return Some((tier, index));
            }
            index -= row.len();
        }
        None
    }

    fn first_slot(&self) -> Option<(usize, usize)> {
        self.slots
            .iter()
            .position(|row| !row.is_empty())
            .map(|tier| (tier, 0))
    }

    fn slot(&self, slot: (usize, usize)) -> Option<&Slot> {
        self.slots.get(slot.0)?.get(slot.1)
    }

    fn slot_mut(&mut self, slot: (usize, usize)) -> Option<&mut Slot> {
        self.slots.get_mut(slot.0)?.get_mut(slot.1)
    }

    fn publish(&self) {
        let active_url = self
            .active
            .and_then(|slot| self.slot(slot))
            .map(|slot| slot.url.clone());
        let consecutive_failures = self
            .active
            .and_then(|slot| self.slot(slot))
            .map_or(0, |slot| slot.failures);
        let snapshot = TrackerStatus {
            active_url,
            last_success: self.last_success,
            next_announce: self.next_announce,
            last_interval: self.last_interval,
            consecutive_failures,
            peer_count: self.peer_count,
            last_warning: self.last_warning,
            tracker_count: self.slots.iter().map(Vec::len).sum(),
            private: self.private,
            terminal: self.terminal,
        };
        match self.status.write() {
            Ok(mut guard) => *guard = snapshot,
            Err(poisoned) => *poisoned.into_inner() = snapshot,
        }
    }
}

fn dedupe_peers(peers: &[SocketAddr]) -> Vec<SocketAddr> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for peer in peers {
        if result.len() >= MAX_PEERS_PER_RESPONSE {
            break;
        }
        if seen.insert(*peer) {
            result.push(*peer);
        }
    }
    result
}

const fn event_priority(event: AnnounceEvent) -> u8 {
    match event {
        AnnounceEvent::None => 0,
        AnnounceEvent::Started => 1,
        AnnounceEvent::Completed => 2,
        AnnounceEvent::Stopped => 3,
    }
}

#[cfg(test)]
mod tests;
