//! Pure announce state machine.
//!
//! The state machine owns no task, socket or clock. The session supplies
//! `Instant` values, builds a request when [`Tracker::build_request`] returns
//! one, and reports the transport result through [`Tracker::handle_response`],
//! [`Tracker::handle_failure`] or [`Tracker::handle_cancel`]. This keeps every
//! timer deterministic under a paused clock.
//!
//! Event coalescing follows the session rules: `stopped` wins over `completed`,
//! and `completed` wins over `started`. `started` and `completed` are sent at
//! most once; a failed `started` or `completed` stays pending for the next
//! attempt. `stopped` is terminal and is never retried after a failure.
//!
//! [`TrackerStatus`] is published as a small snapshot behind a
//! [`std::sync::RwLock`]. The lock is held only while copying the snapshot and
//! is never held across an `.await`; the state machine itself is synchronous.

use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::time::Instant;

use crate::metainfo::InfoHash;

use super::backoff::{Jitter, JitterSeed, checked_deadline, effective_interval};
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
    /// Whether the torrent is private (BEP 27).
    pub private: bool,
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
        /// Next attempt time, or `None` when the state is terminal.
        retry_at: Option<Instant>,
    },
    /// Peers to add to the peer set.
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
    /// Consecutive failed rounds.
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

/// One torrent's announce state.
pub struct Tracker {
    candidates: TrackerCandidates,
    config: TrackerConfig,
    tier: usize,
    index: usize,
    tracker_id: Option<TrackerId>,
    next_announce: Option<Instant>,
    pending_event: Option<AnnounceEvent>,
    in_flight: bool,
    in_flight_event: Option<AnnounceEvent>,
    started_sent: bool,
    completed_sent: bool,
    terminal: bool,
    failure_count: u32,
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

        let seed = JitterSeed::generate(config.info_hash);
        let completed_sent = config.completed_sent_at_start;
        let tracker = Self {
            candidates,
            config,
            tier: 0,
            index: 0,
            tracker_id: None,
            next_announce: Some(now),
            pending_event: Some(AnnounceEvent::Started),
            in_flight: false,
            in_flight_event: None,
            started_sent: false,
            completed_sent,
            terminal: false,
            failure_count: 0,
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

        let url = self.active_url()?.clone();
        let event = self.pending_event.take().unwrap_or(AnnounceEvent::None);
        let request = AnnounceRequest::new(
            url,
            self.config.info_hash,
            self.config.peer_id,
            self.config.port,
            stats,
            event,
            self.config.key,
            self.tracker_id.clone(),
            if self.config.private {
                RedirectPolicy::SameHostOnly
            } else {
                RedirectPolicy::Public
            },
        );

        self.in_flight = true;
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

        let event = self.in_flight_event.take();
        self.in_flight = false;
        self.failure_count = 0;
        self.last_success = Some(now);
        let interval = effective_interval(response.interval, response.min_interval);
        self.last_interval = Some(interval);
        self.last_warning = response.warning.is_some();
        self.peer_count = response.peers.len();
        if response.tracker_id.is_some() {
            self.tracker_id = response.tracker_id;
        }

        match event {
            Some(AnnounceEvent::Started) => self.started_sent = true,
            Some(AnnounceEvent::Completed) => self.completed_sent = true,
            Some(AnnounceEvent::Stopped) => {
                self.terminal = true;
                self.pending_event = None;
                self.next_announce = None;
            }
            Some(AnnounceEvent::None) | None => {}
        }

        if !self.terminal {
            self.next_announce = if self.pending_event.is_some() {
                Some(now)
            } else {
                checked_deadline(now, interval)
            };
        }
        self.publish();

        let mut events = vec![TrackerEvent::AnnounceSucceeded { interval }];
        if !response.peers.is_empty() {
            events.push(TrackerEvent::PeersReceived(response.peers));
        }
        if let Some(warning) = response.warning {
            events.push(TrackerEvent::Warning(warning));
        }
        events
    }

    /// Reports a failed attempt.
    pub fn handle_failure(&mut self, now: Instant, error: TrackerError) -> Vec<TrackerEvent> {
        if !self.in_flight {
            return Vec::new();
        }

        let event = self.in_flight_event.take();
        self.in_flight = false;

        if event == Some(AnnounceEvent::Stopped) {
            self.terminal = true;
            self.pending_event = None;
            self.next_announce = None;
            self.publish();
            return vec![TrackerEvent::AnnounceFailed {
                error,
                retry_at: None,
            }];
        }

        self.failure_count = self.failure_count.saturating_add(1);
        match event {
            Some(AnnounceEvent::Started) if !self.started_sent => {
                self.queue_pending(AnnounceEvent::Started);
            }
            Some(AnnounceEvent::Completed) if !self.completed_sent => {
                self.queue_pending(AnnounceEvent::Completed);
            }
            _ => {}
        }

        let retry_at = if self.pending_event == Some(AnnounceEvent::Stopped) {
            Some(now)
        } else {
            checked_deadline(now, self.jitter.delay(self.failure_count))
        };
        self.next_announce = retry_at;
        self.publish();
        vec![TrackerEvent::AnnounceFailed { error, retry_at }]
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

        let event = self.in_flight_event.take();
        self.in_flight = false;
        if event == Some(AnnounceEvent::Stopped) {
            self.terminal = true;
            self.pending_event = None;
            self.next_announce = None;
        } else {
            if let Some(event) = event {
                self.queue_pending(event);
            }
            self.next_announce = Some(now);
        }
        self.publish();
        Vec::new()
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

    fn active_url(&self) -> Option<&TrackerUrl> {
        self.candidates.tiers().get(self.tier)?.get(self.index)
    }

    fn publish(&self) {
        let snapshot = TrackerStatus {
            active_url: self.active_url().cloned(),
            last_success: self.last_success,
            next_announce: self.next_announce,
            last_interval: self.last_interval,
            consecutive_failures: self.failure_count,
            peer_count: self.peer_count,
            last_warning: self.last_warning,
            tracker_count: self.candidates.len(),
            private: self.config.private,
            terminal: self.terminal,
        };
        match self.status.write() {
            Ok(mut guard) => *guard = snapshot,
            Err(poisoned) => *poisoned.into_inner() = snapshot,
        }
    }
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
