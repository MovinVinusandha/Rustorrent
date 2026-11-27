//! Tracker URL parsing, response decoding and announce transports.
//!
//! URL parsing and response decoding are pure. DNS resolution and the UDP
//! transport perform network I/O under their own deadlines; the announce
//! state machine and the HTTP transport land in 4c and 4d.

use std::io;

use thiserror::Error;

pub mod candidates;
pub mod dns;
pub mod id;
pub mod message;
pub mod peers;
pub mod policy;
pub mod response;
pub mod transport;
pub mod udp;
pub mod url;

pub use candidates::{
    CandidateWarnings, MAX_TIERS, MAX_TRACKER_URLS, MAX_URLS_PER_TIER, OsShuffle, Shuffle,
    TrackerCandidates,
};
pub use dns::{
    DNS_LOOKUP_TIMEOUT, DNS_QUEUE_TIMEOUT, MAX_CONCURRENT_LOOKUPS, MAX_RESOLVED_ADDRESSES,
    Resolver, SystemResolver,
};
pub use id::{MAX_TRACKER_ID_LEN, TrackerId};
pub use message::{MAX_MESSAGE_LEN, TrackerMessage};
pub use peers::{MAX_PEERS_PER_RESPONSE, PeerWarnings, decode_peers};
pub use policy::TargetPolicy;
pub use response::{
    AnnounceResponse, DEFAULT_INTERVAL_SECS, MAX_INTERVAL_SECS, MAX_RESPONSE_BYTES,
    MIN_INTERVAL_SECS, decode_response,
};
pub use transport::{
    AnnounceEvent, AnnounceRequest, AnnounceStats, RedirectPolicy, TrackerTransport,
};
pub use url::{
    Host, MAX_HOST_LEN, MAX_LABEL_LEN, MAX_REDACTED_URL_LEN, MAX_URL_LEN, Scheme, TrackerUrl,
};

/// Errors produced by tracker parsing and announce transports.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum TrackerError {
    /// The input is not a valid tracker URL.
    #[error("<invalid tracker url>")]
    InvalidUrl,
    /// A URL has no port and its scheme has no default.
    #[error("invalid tracker port")]
    InvalidPort,
    /// The URL scheme is not `http`, `https` or `udp`.
    #[error("unsupported tracker URL scheme")]
    UnsupportedScheme,
    /// The tracker response has the wrong shape.
    #[error("malformed tracker response")]
    MalformedResponse,
    /// The tracker rejected the announce.
    #[error("tracker rejected the announce")]
    Rejected {
        /// Tracker-supplied failure text; length-only in diagnostics.
        message: TrackerMessage,
    },
    /// The tracker response body is larger than the cap.
    #[error("tracker response is too large")]
    TooLarge,
    /// Every candidate address is blocked by the target policy.
    #[error("blocked tracker target")]
    BlockedTarget,
    /// No tracker candidate survived URL validation.
    #[error("no usable tracker")]
    NoUsableTracker,
    /// Host resolution failed, timed out or produced no usable address.
    #[error("tracker DNS lookup failed")]
    Dns,
    /// The tracker address refused the connection or reset it.
    ///
    /// This ends the attempt immediately: an ICMP port-unreachable means
    /// nothing is listening, and waiting for the deadline would only delay
    /// failover to the next tracker.
    #[error("tracker is not reachable")]
    Connect,
    /// The attempt deadline passed with no usable response.
    #[error("tracker announce timed out")]
    Timeout,
    /// A socket operation failed; only the error kind is kept.
    #[error("tracker network error")]
    Io {
        /// Kind of the failed operation.
        kind: io::ErrorKind,
    },
    /// The OS randomness source failed.
    #[error("failed to generate a transaction ID")]
    Random,
    /// A transport panicked; the panic was contained at the task boundary.
    ///
    /// Only reachable in unwind builds. Release builds use `panic = "abort"`,
    /// so a transport panic ends the process instead.
    #[error("tracker transport panicked")]
    Panicked,
}
