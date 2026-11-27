//! BEP 23 compact and list peers, and BEP 7 `peers6` decoding.
//!
//! The decoder is pure and bounded: the response body is already capped
//! before it runs, every record is checked before use, and the merged result
//! is capped at [`MAX_PEERS_PER_RESPONSE`]. Recoverable problems are counted
//! in [`PeerWarnings`]; only a wrong-typed `peers` or `peers6` field fails
//! the whole response.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr, SocketAddrV4, SocketAddrV6};

use crate::bencode::Value;

use super::TrackerError;

/// Maximum peers kept from one response, after deduplication.
pub const MAX_PEERS_PER_RESPONSE: usize = 200;

const COMPACT_V4_STRIDE: usize = 6;
const COMPACT_V6_STRIDE: usize = 18;

/// Counters for recoverable peer-list problems.
///
/// The counters never contain tracker bytes; they are safe to log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeerWarnings {
    /// Compact records discarded because the trailing bytes were not a
    /// complete record.
    pub malformed_peer_records: u32,
    /// Entries or records dropped for an unusable address or port, or for a
    /// missing or wrong-typed field.
    pub dropped_peer_records: u32,
    /// Valid unique peers discarded after the response cap.
    pub truncated_peers: u32,
    /// Neither `peers` nor `peers6` was present.
    pub missing_peers: bool,
}

impl PeerWarnings {
    /// Returns `true` when no problem was recorded.
    pub fn is_empty(&self) -> bool {
        self.malformed_peer_records == 0
            && self.dropped_peer_records == 0
            && self.truncated_peers == 0
            && !self.missing_peers
    }
}

/// Decodes `peers` and `peers6` from a response dictionary.
///
/// `peers` may be a compact string (BEP 23) or a list of dictionaries. A
/// missing `peers` is not an error; a wrong-typed `peers` or `peers6` is
/// [`TrackerError::MalformedResponse`].
pub fn decode_peers(response: &Value<'_>) -> Result<(Vec<SocketAddr>, PeerWarnings), TrackerError> {
    if response.as_dict().is_none() {
        return Err(TrackerError::MalformedResponse);
    }

    let peers = response.dict_get(b"peers");
    let peers6 = response.dict_get(b"peers6");

    let mut warnings = PeerWarnings {
        missing_peers: peers.is_none() && peers6.is_none(),
        ..PeerWarnings::default()
    };

    let mut ipv4 = Vec::new();
    if let Some(value) = peers {
        decode_peers_field(value, &mut ipv4, &mut warnings)?;
    }

    let mut ipv6 = Vec::new();
    if let Some(value) = peers6 {
        let bytes = value.as_bytes().ok_or(TrackerError::MalformedResponse)?;
        decode_compact(
            bytes,
            COMPACT_V6_STRIDE,
            &mut ipv6,
            &mut warnings,
            decode_v6_record,
        );
    }

    let mut merged = Vec::with_capacity(ipv4.len() + ipv6.len());
    let mut seen = HashSet::with_capacity(ipv4.len() + ipv6.len());
    for peer in ipv4.into_iter().chain(ipv6) {
        if !seen.insert(peer) {
            continue;
        }
        if merged.len() >= MAX_PEERS_PER_RESPONSE {
            warnings.truncated_peers = warnings.truncated_peers.saturating_add(1);
            continue;
        }
        merged.push(peer);
    }

    Ok((merged, warnings))
}

fn decode_peers_field(
    value: &Value<'_>,
    peers: &mut Vec<SocketAddr>,
    warnings: &mut PeerWarnings,
) -> Result<(), TrackerError> {
    if let Some(bytes) = value.as_bytes() {
        decode_compact(bytes, COMPACT_V4_STRIDE, peers, warnings, decode_v4_record);
        return Ok(());
    }

    let entries = value.as_list().ok_or(TrackerError::MalformedResponse)?;
    let mut seen = HashSet::new();
    for entry in entries {
        let Some(peer) = decode_list_entry(entry) else {
            warnings.dropped_peer_records = warnings.dropped_peer_records.saturating_add(1);
            continue;
        };
        push_peer(peers, &mut seen, warnings, peer);
    }
    Ok(())
}

fn decode_compact(
    bytes: &[u8],
    stride: usize,
    peers: &mut Vec<SocketAddr>,
    warnings: &mut PeerWarnings,
    decode: fn(&[u8]) -> Option<SocketAddr>,
) {
    if !bytes.len().is_multiple_of(stride) {
        warnings.malformed_peer_records = warnings.malformed_peer_records.saturating_add(1);
    }

    let mut seen = HashSet::new();
    for record in bytes.chunks_exact(stride) {
        match decode(record) {
            Some(peer) => push_peer(peers, &mut seen, warnings, peer),
            None => {
                warnings.dropped_peer_records = warnings.dropped_peer_records.saturating_add(1);
            }
        }
    }
}

fn push_peer(
    peers: &mut Vec<SocketAddr>,
    seen: &mut HashSet<SocketAddr>,
    warnings: &mut PeerWarnings,
    peer: SocketAddr,
) {
    if peers.len() >= MAX_PEERS_PER_RESPONSE {
        warnings.truncated_peers = warnings.truncated_peers.saturating_add(1);
        return;
    }
    if !seen.insert(peer) {
        return;
    }
    peers.push(peer);
}

fn decode_v4_record(record: &[u8]) -> Option<SocketAddr> {
    let address = [record[0], record[1], record[2], record[3]];
    let port = u16::from_be_bytes([record[4], record[5]]);
    usable_socket(IpAddr::from(address), port)
}

fn decode_v6_record(record: &[u8]) -> Option<SocketAddr> {
    let mut octets = [0u8; 16];
    octets.copy_from_slice(&record[..16]);
    let port = u16::from_be_bytes([record[16], record[17]]);
    usable_socket(IpAddr::from(octets), port)
}

fn decode_list_entry(entry: &Value<'_>) -> Option<SocketAddr> {
    let ip = entry.dict_get(b"ip")?.as_bytes()?;
    let port = entry.dict_get(b"port")?.as_int()?;
    let port = u16::try_from(port).ok()?;
    let address = std::str::from_utf8(ip).ok()?.parse::<IpAddr>().ok()?;
    usable_socket(address, port)
}

pub(super) fn usable_socket(address: IpAddr, port: u16) -> Option<SocketAddr> {
    if port == 0 {
        return None;
    }

    match address {
        IpAddr::V4(address) => {
            if address.is_unspecified() || address.is_broadcast() || address.is_multicast() {
                None
            } else {
                Some(SocketAddr::V4(SocketAddrV4::new(address, port)))
            }
        }
        IpAddr::V6(address) => {
            if address.is_unspecified()
                || address.is_multicast()
                || address.to_ipv4_mapped().is_some()
            {
                None
            } else {
                Some(SocketAddr::V6(SocketAddrV6::new(address, port, 0, 0)))
            }
        }
    }
}

#[cfg(test)]
mod tests;
