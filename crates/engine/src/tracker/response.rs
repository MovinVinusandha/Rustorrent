//! HTTP tracker response envelope decoding.
//!
//! The body is bounded before decoding, the top level must be a dictionary,
//! and `failure reason` is checked before any other field. Peer lists are
//! decoded by [`super::peers`]. Diagnostics never contain tracker text.

use std::net::SocketAddr;
use std::time::Duration;

use crate::bencode::{self, DecodeOptions, Value};

use super::{PeerWarnings, TrackerError, TrackerId, TrackerMessage, decode_peers};

/// Maximum accepted response body length in bytes.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Interval used when the response does not carry a usable one.
pub const DEFAULT_INTERVAL_SECS: u64 = 1800;

/// Minimum effective announce interval in seconds.
pub const MIN_INTERVAL_SECS: u64 = 60;

/// Maximum effective announce interval in seconds.
pub const MAX_INTERVAL_SECS: u64 = 86400;

/// A decoded announce response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnnounceResponse {
    /// Effective interval between regular announces.
    pub interval: Duration,
    /// Clamped `min interval`, when the tracker sent one.
    pub min_interval: Option<Duration>,
    /// Number of seeders, when present and non-negative.
    pub complete: Option<u64>,
    /// Number of leechers, when present and non-negative.
    pub incomplete: Option<u64>,
    /// Decoded and filtered peers.
    pub peers: Vec<SocketAddr>,
    /// Opaque tracker identifier for the next announce.
    pub tracker_id: Option<TrackerId>,
    /// Advisory tracker text, length-only in diagnostics.
    pub warning: Option<TrackerMessage>,
    /// Counters for recoverable peer-list problems.
    pub peer_warnings: PeerWarnings,
}

/// Decodes an HTTP announce response body.
///
/// A `failure reason` produces [`TrackerError::Rejected`] before any other
/// field is inspected. Every other malformed shape produces
/// [`TrackerError::MalformedResponse`]; an oversized body produces
/// [`TrackerError::TooLarge`].
pub fn decode_response(body: &[u8]) -> Result<AnnounceResponse, TrackerError> {
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(TrackerError::TooLarge);
    }

    let options = DecodeOptions {
        allow_integer_overflow: true,
        ..DecodeOptions::default()
    };
    let value =
        bencode::decode_with_options(body, options).map_err(|_| TrackerError::MalformedResponse)?;

    if value.as_dict().is_none() {
        return Err(TrackerError::MalformedResponse);
    }

    if let Some(reason) = value.dict_get(b"failure reason") {
        let bytes = reason.as_bytes().ok_or(TrackerError::MalformedResponse)?;
        return Err(TrackerError::Rejected {
            message: TrackerMessage::from_bytes(bytes),
        });
    }

    let interval = clamp_interval(value.dict_get(b"interval").and_then(Value::as_int));
    let min_interval = value
        .dict_get(b"min interval")
        .and_then(Value::as_int)
        .map(|seconds| clamp_interval(Some(seconds)));
    let effective = min_interval.map_or(interval, |minimum| interval.max(minimum));

    let (peers, peer_warnings) = decode_peers(&value)?;

    Ok(AnnounceResponse {
        interval: Duration::from_secs(effective),
        min_interval: min_interval.map(Duration::from_secs),
        complete: non_negative(value.dict_get(b"complete")),
        incomplete: non_negative(value.dict_get(b"incomplete")),
        peers,
        tracker_id: value
            .dict_get(b"tracker id")
            .and_then(Value::as_bytes)
            .and_then(TrackerId::new),
        warning: value
            .dict_get(b"warning message")
            .and_then(Value::as_bytes)
            .map(TrackerMessage::from_bytes),
        peer_warnings,
    })
}

fn clamp_interval(seconds: Option<i64>) -> u64 {
    let seconds = seconds.unwrap_or(DEFAULT_INTERVAL_SECS as i64);
    u64::try_from(seconds.clamp(MIN_INTERVAL_SECS as i64, MAX_INTERVAL_SECS as i64))
        .unwrap_or(MIN_INTERVAL_SECS)
}

fn non_negative(value: Option<&Value<'_>>) -> Option<u64> {
    u64::try_from(value?.as_int()?).ok()
}

#[cfg(test)]
mod tests;
