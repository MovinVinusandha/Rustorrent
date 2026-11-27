//! Announce transport boundary.
//!
//! The state machine builds an [`AnnounceRequest`] and a transport performs
//! one announce. The trait is object-safe and uses boxed futures instead of
//! `async-trait`; dropping the returned future cancels the attempt.
//!
//! [`TrackerTransport::max_duration`] is the transport's own upper bound for
//! one attempt. The tracker task adds a small margin and treats a future that
//! outlives it as a transport bug. The UDP implementation returns the 105 s
//! attempt deadline, or 2 s for `stopped`; HTTP will return its own deadline.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::metainfo::InfoHash;

use super::{AnnounceResponse, TrackerError, TrackerId, TrackerUrl};

/// One announce transport.
pub trait TrackerTransport: Send + Sync {
    /// Performs one announce.
    ///
    /// Dropping the returned future cancels the attempt and releases its
    /// resources.
    fn announce<'a>(
        &'a self,
        request: AnnounceRequest,
    ) -> Pin<Box<dyn Future<Output = Result<AnnounceResponse, TrackerError>> + Send + 'a>>;

    /// Returns the transport's own maximum duration for this event.
    ///
    /// The tracker task uses this value plus a small margin as a watchdog.
    /// An implementation must return a bound that is at least as long as its
    /// internal deadline; returning a shorter value would cut a valid attempt.
    fn max_duration(&self, event: AnnounceEvent) -> Duration;
}

/// One announce request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnnounceRequest {
    /// Tracker to contact.
    pub url: TrackerUrl,
    /// Raw v1 info-hash.
    pub info_hash: InfoHash,
    /// Our peer ID.
    pub peer_id: [u8; 20],
    /// Our TCP listen port.
    pub port: u16,
    /// Transfer counters for this announce.
    pub stats: AnnounceStats,
    /// Announce event.
    pub event: AnnounceEvent,
    /// Peers wanted; always 0 for `stopped`.
    pub num_want: i32,
    /// Client key, reused across announces for one torrent.
    pub key: u32,
    /// Opaque identifier returned by this tracker, if any.
    pub tracker_id: Option<TrackerId>,
    /// Redirect policy enforced by the transport.
    pub redirect: RedirectPolicy,
}

impl AnnounceRequest {
    /// Builds a request and derives `num_want` from the event.
    ///
    /// `num_want` is 50 for normal announces and 0 for `stopped`; callers
    /// cannot accidentally request peers while stopping.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        url: TrackerUrl,
        info_hash: InfoHash,
        peer_id: [u8; 20],
        port: u16,
        stats: AnnounceStats,
        event: AnnounceEvent,
        key: u32,
        tracker_id: Option<TrackerId>,
        redirect: RedirectPolicy,
    ) -> Self {
        Self {
            url,
            info_hash,
            peer_id,
            port,
            stats,
            event,
            num_want: event.num_want(),
            key,
            tracker_id,
            redirect,
        }
    }
}

/// Transfer counters sent with an announce.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AnnounceStats {
    /// Bytes uploaded so far.
    pub uploaded: u64,
    /// Bytes downloaded so far.
    pub downloaded: u64,
    /// Bytes still needed.
    pub left: u64,
}

/// Event carried by an announce.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AnnounceEvent {
    /// A periodic announce.
    #[default]
    None,
    /// The first announce for this torrent.
    Started,
    /// The download completed.
    Completed,
    /// The torrent is stopping.
    Stopped,
}

impl AnnounceEvent {
    /// Returns the number of peers to request.
    pub const fn num_want(self) -> i32 {
        match self {
            Self::Stopped => 0,
            Self::None | Self::Started | Self::Completed => 50,
        }
    }

    /// Returns `true` for the terminal event.
    pub const fn is_stopped(self) -> bool {
        matches!(self, Self::Stopped)
    }

    /// Returns the BEP 15 wire value.
    pub(crate) const fn as_udp(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Completed => 1,
            Self::Started => 2,
            Self::Stopped => 3,
        }
    }
}

/// Redirect policy carried by every request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RedirectPolicy {
    /// Public torrents may follow a redirect to another host.
    #[default]
    Public,
    /// Private torrents may follow only a same-host redirect.
    SameHostOnly,
}

#[cfg(test)]
mod tests;
