//! BEP 15 packet encoding and decoding.
//!
//! Every function is pure and bounds-checked: no socket, no clock, and no
//! allocation beyond the decoded peer vector and its deduplication set, both
//! capped at [`MAX_PEERS_PER_RESPONSE`]. The transport receives at most
//! [`MAX_DATAGRAM_LEN`] bytes; a full 200-peer IPv6 response is
//! `20 + 200 * 18 = 3620` bytes, so it fits in that buffer. A datagram that
//! is still too large for the buffer is truncated by the kernel, and the
//! trailing partial peer record is ignored.
//!
//! A packet is accepted only when its action and transaction ID match the
//! request. Anything else returns `None` and the caller keeps waiting until
//! its absolute deadline; a bad datagram never extends a wait.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use crate::tracker::peers::usable_socket;
use crate::tracker::{
    AnnounceRequest, AnnounceResponse, MAX_PEERS_PER_RESPONSE, PeerWarnings, TrackerMessage,
};

/// Protocol magic sent by a connect request.
pub const PROTOCOL_ID: u64 = 0x0000_0417_2710_1980;

/// Length of a connect request in bytes.
pub const CONNECT_REQUEST_LEN: usize = 16;

/// Minimum length of a connect response in bytes.
pub const CONNECT_RESPONSE_LEN: usize = 16;

/// Length of an IPv4 announce request in bytes.
pub const ANNOUNCE_REQUEST_LEN: usize = 98;

/// Minimum length of an announce response in bytes.
pub const ANNOUNCE_RESPONSE_MIN_LEN: usize = 20;

/// Minimum length of an error packet in bytes.
pub const ERROR_MIN_LEN: usize = 8;

/// UDP receive buffer size in bytes.
pub const MAX_DATAGRAM_LEN: usize = 4096;

/// Peer record stride in an IPv4 announce response.
pub const IPV4_PEER_STRIDE: usize = 6;

/// Peer record stride in an IPv6 announce response.
pub const IPV6_PEER_STRIDE: usize = 18;

const ACTION_CONNECT: u32 = 0;
const ACTION_ANNOUNCE: u32 = 1;
const ACTION_ERROR: u32 = 3;

/// Address family of the socket that received the datagram.
///
/// The family selects the peer record stride; it is never inferred from the
/// packet contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    /// IPv4 socket, 6-byte peer records.
    V4,
    /// IPv6 socket, 18-byte peer records.
    V6,
}

impl Family {
    /// Returns the peer record stride for this family.
    pub const fn peer_stride(self) -> usize {
        match self {
            Self::V4 => IPV4_PEER_STRIDE,
            Self::V6 => IPV6_PEER_STRIDE,
        }
    }
}

/// An opaque connection ID issued by a tracker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectionId(u64);

impl ConnectionId {
    /// Wraps a raw connection ID.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the raw connection ID.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A datagram accepted for the current request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decoded {
    /// A connect response.
    Connect(ConnectionId),
    /// An announce response, in the shared response shape.
    Announce(AnnounceResponse),
    /// An error packet; the text is untrusted and length-only in diagnostics.
    Error(TrackerMessage),
}

/// Encodes a connect request.
pub fn encode_connect(transaction_id: u32) -> [u8; CONNECT_REQUEST_LEN] {
    let mut packet = [0u8; CONNECT_REQUEST_LEN];
    packet[0..8].copy_from_slice(&PROTOCOL_ID.to_be_bytes());
    packet[8..12].copy_from_slice(&ACTION_CONNECT.to_be_bytes());
    packet[12..16].copy_from_slice(&transaction_id.to_be_bytes());
    packet
}

/// Encodes an announce request as exactly 98 bytes.
pub(crate) fn encode_announce(
    request: &AnnounceRequest,
    connection_id: ConnectionId,
    transaction_id: u32,
) -> [u8; ANNOUNCE_REQUEST_LEN] {
    let mut packet = [0u8; ANNOUNCE_REQUEST_LEN];
    packet[0..8].copy_from_slice(&connection_id.get().to_be_bytes());
    packet[8..12].copy_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
    packet[12..16].copy_from_slice(&transaction_id.to_be_bytes());
    packet[16..36].copy_from_slice(request.info_hash.as_bytes());
    packet[36..56].copy_from_slice(&request.peer_id);
    packet[56..64].copy_from_slice(&request.stats.downloaded.to_be_bytes());
    packet[64..72].copy_from_slice(&request.stats.left.to_be_bytes());
    packet[72..80].copy_from_slice(&request.stats.uploaded.to_be_bytes());
    packet[80..84].copy_from_slice(&request.event.as_udp().to_be_bytes());
    // Bytes 84..88 stay zero: the IP address field.
    packet[88..92].copy_from_slice(&request.key.to_be_bytes());
    packet[92..96].copy_from_slice(&request.num_want.to_be_bytes());
    packet[96..98].copy_from_slice(&request.port.to_be_bytes());
    packet
}

/// Decodes one datagram for the given request.
///
/// Returns `None` when the datagram is shorter than its minimum, when the
/// action is unknown, or when the transaction ID does not match. A connect
/// response with extra bytes is accepted; an announce response with a
/// trailing partial peer record keeps the complete records and records a
/// warning.
pub fn decode(datagram: &[u8], transaction_id: u32, family: Family) -> Option<Decoded> {
    if datagram.len() < ERROR_MIN_LEN {
        return None;
    }

    let action = u32::from_be_bytes(datagram.get(0..4)?.try_into().ok()?);
    let received = u32::from_be_bytes(datagram.get(4..8)?.try_into().ok()?);
    if received != transaction_id {
        return None;
    }

    match action {
        ACTION_CONNECT if datagram.len() >= CONNECT_RESPONSE_LEN => {
            let connection_id = u64::from_be_bytes(datagram.get(8..16)?.try_into().ok()?);
            Some(Decoded::Connect(ConnectionId::new(connection_id)))
        }
        ACTION_ANNOUNCE if datagram.len() >= ANNOUNCE_RESPONSE_MIN_LEN => {
            Some(Decoded::Announce(decode_announce_body(datagram, family)?))
        }
        ACTION_ERROR => Some(Decoded::Error(TrackerMessage::from_bytes(
            datagram.get(ERROR_MIN_LEN..).unwrap_or_default(),
        ))),
        _ => None,
    }
}

fn decode_announce_body(datagram: &[u8], family: Family) -> Option<AnnounceResponse> {
    let interval = u32::from_be_bytes(datagram.get(8..12)?.try_into().ok()?);
    let leechers = u32::from_be_bytes(datagram.get(12..16)?.try_into().ok()?);
    let seeders = u32::from_be_bytes(datagram.get(16..20)?.try_into().ok()?);

    let body = datagram
        .get(ANNOUNCE_RESPONSE_MIN_LEN..)
        .unwrap_or_default();
    let stride = family.peer_stride();
    let mut warnings = PeerWarnings::default();
    if !body.len().is_multiple_of(stride) {
        warnings.malformed_peer_records = 1;
    }

    let mut peers = Vec::new();
    let mut seen = HashSet::with_capacity(MAX_PEERS_PER_RESPONSE);
    for record in body.chunks_exact(stride) {
        let Some(peer) = decode_record(record, family) else {
            warnings.dropped_peer_records = warnings.dropped_peer_records.saturating_add(1);
            continue;
        };
        if seen.contains(&peer) {
            continue;
        }
        if peers.len() >= MAX_PEERS_PER_RESPONSE {
            warnings.truncated_peers = warnings.truncated_peers.saturating_add(1);
            continue;
        }
        seen.insert(peer);
        peers.push(peer);
    }

    Some(AnnounceResponse {
        interval: Duration::from_secs(u64::from(interval)),
        min_interval: None,
        complete: Some(u64::from(seeders)),
        incomplete: Some(u64::from(leechers)),
        peers,
        tracker_id: None,
        warning: None,
        peer_warnings: warnings,
    })
}

fn decode_record(record: &[u8], family: Family) -> Option<SocketAddr> {
    match family {
        Family::V4 => {
            let octets: [u8; 4] = record.get(0..4)?.try_into().ok()?;
            let port = u16::from_be_bytes(record.get(4..6)?.try_into().ok()?);
            usable_socket(IpAddr::V4(Ipv4Addr::from(octets)), port)
        }
        Family::V6 => {
            let octets: [u8; 16] = record.get(0..16)?.try_into().ok()?;
            let port = u16::from_be_bytes(record.get(16..18)?.try_into().ok()?);
            usable_socket(IpAddr::V6(Ipv6Addr::from(octets)), port)
        }
    }
}

#[cfg(test)]
mod tests;
