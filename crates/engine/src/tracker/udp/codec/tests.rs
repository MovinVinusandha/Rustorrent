use std::fmt::Write;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use crate::metainfo::InfoHash;
use crate::tracker::{
    AnnounceEvent, AnnounceRequest, AnnounceResponse, AnnounceStats, PeerWarnings, RedirectPolicy,
    TrackerError, TrackerUrl,
};

use super::{
    ANNOUNCE_REQUEST_LEN, ANNOUNCE_RESPONSE_MIN_LEN, CONNECT_REQUEST_LEN, CONNECT_RESPONSE_LEN,
    ConnectionId, Decoded, ERROR_MIN_LEN, Family, IPV4_PEER_STRIDE, IPV6_PEER_STRIDE,
    MAX_DATAGRAM_LEN, PROTOCOL_ID, decode, encode_announce, encode_connect,
};

const TRANSACTION_ID: u32 = 0x1234_5678;
const CONNECTION_ID: u64 = 0x0123_4567_89ab_cdef;

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut text, "{byte:02x}").expect("write to string");
    }
    text
}

fn request() -> AnnounceRequest {
    let mut info_hash = [0u8; 20];
    for (index, byte) in info_hash.iter_mut().enumerate() {
        *byte = index as u8;
    }

    AnnounceRequest::new(
        TrackerUrl::parse("udp://127.0.0.1:6969/announce").expect("valid tracker URL"),
        InfoHash::from_bytes(info_hash),
        *b"-BA0100-123456789012",
        51413,
        AnnounceStats {
            uploaded: 768,
            downloaded: 256,
            left: 512,
        },
        AnnounceEvent::Started,
        0xdead_beef,
        None,
        RedirectPolicy::Public,
    )
}

fn announce_response(interval: u32, leechers: u32, seeders: u32, peers: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(ANNOUNCE_RESPONSE_MIN_LEN + peers.len());
    packet.extend_from_slice(&1u32.to_be_bytes());
    packet.extend_from_slice(&TRANSACTION_ID.to_be_bytes());
    packet.extend_from_slice(&interval.to_be_bytes());
    packet.extend_from_slice(&leechers.to_be_bytes());
    packet.extend_from_slice(&seeders.to_be_bytes());
    packet.extend_from_slice(peers);
    packet
}

fn v4_peer(octets: [u8; 4], port: u16) -> [u8; 6] {
    let mut record = [0u8; 6];
    record[..4].copy_from_slice(&octets);
    record[4..].copy_from_slice(&port.to_be_bytes());
    record
}

fn v6_peer(octets: [u8; 16], port: u16) -> [u8; 18] {
    let mut record = [0u8; 18];
    record[..16].copy_from_slice(&octets);
    record[16..].copy_from_slice(&port.to_be_bytes());
    record
}

fn decode_announce(packet: &[u8], family: Family) -> AnnounceResponse {
    match decode(packet, TRANSACTION_ID, family) {
        Some(Decoded::Announce(response)) => response,
        other => panic!("expected an announce response, got {other:?}"),
    }
}

#[test]
fn the_connect_request_matches_the_bep_vector() {
    assert_eq!(
        hex(&encode_connect(TRANSACTION_ID)),
        "00000417271019800000000012345678"
    );
    assert_eq!(PROTOCOL_ID, 0x0000_0417_2710_1980);
    assert_eq!(encode_connect(TRANSACTION_ID).len(), CONNECT_REQUEST_LEN);
}

#[test]
fn the_announce_request_is_exactly_98_bytes() {
    let packet = encode_announce(&request(), ConnectionId::new(CONNECTION_ID), TRANSACTION_ID);

    assert_eq!(packet.len(), ANNOUNCE_REQUEST_LEN);
    assert_eq!(
        hex(&packet),
        concat!(
            "0123456789abcdef",
            "00000001",
            "12345678",
            "000102030405060708090a0b0c0d0e0f10111213",
            "2d4241303130302d313233343536373839303132",
            "0000000000000100",
            "0000000000000200",
            "0000000000000300",
            "00000002",
            "00000000",
            "deadbeef",
            "00000032",
            "c8d5",
        )
    );
}

#[test]
fn every_event_has_its_wire_value() {
    assert_eq!(AnnounceEvent::None.as_udp(), 0);
    assert_eq!(AnnounceEvent::Completed.as_udp(), 1);
    assert_eq!(AnnounceEvent::Started.as_udp(), 2);
    assert_eq!(AnnounceEvent::Stopped.as_udp(), 3);
}

#[test]
fn a_connect_response_yields_the_connection_id() {
    let mut packet = Vec::new();
    packet.extend_from_slice(&0u32.to_be_bytes());
    packet.extend_from_slice(&TRANSACTION_ID.to_be_bytes());
    packet.extend_from_slice(&CONNECTION_ID.to_be_bytes());

    assert_eq!(
        decode(&packet, TRANSACTION_ID, Family::V4),
        Some(Decoded::Connect(ConnectionId::new(CONNECTION_ID)))
    );

    // Bytes beyond the documented 16 are ignored.
    packet.extend_from_slice(&[0xff; 9]);
    assert_eq!(
        decode(&packet, TRANSACTION_ID, Family::V4),
        Some(Decoded::Connect(ConnectionId::new(CONNECTION_ID)))
    );
}

#[test]
fn a_short_connect_response_is_ignored() {
    let mut packet = Vec::new();
    packet.extend_from_slice(&0u32.to_be_bytes());
    packet.extend_from_slice(&TRANSACTION_ID.to_be_bytes());
    packet.extend_from_slice(&CONNECTION_ID.to_be_bytes());
    packet.truncate(CONNECT_RESPONSE_LEN - 1);

    assert_eq!(decode(&packet, TRANSACTION_ID, Family::V4), None);
}

#[test]
fn a_wrong_transaction_id_or_action_is_ignored() {
    let mut connect = Vec::new();
    connect.extend_from_slice(&0u32.to_be_bytes());
    connect.extend_from_slice(&TRANSACTION_ID.to_be_bytes());
    connect.extend_from_slice(&CONNECTION_ID.to_be_bytes());
    assert_eq!(decode(&connect, TRANSACTION_ID ^ 1, Family::V4), None);

    let mut wrong_action = connect.clone();
    wrong_action[..4].copy_from_slice(&2u32.to_be_bytes());
    assert_eq!(decode(&wrong_action, TRANSACTION_ID, Family::V4), None);

    let announce = announce_response(1800, 3, 4, &[]);
    assert_eq!(decode(&announce, TRANSACTION_ID ^ 1, Family::V4), None);
}

#[test]
fn the_announce_vector_keeps_one_peer_and_drops_the_invalid_one() {
    let mut peers = Vec::new();
    peers.extend_from_slice(&v4_peer([127, 0, 0, 1], 51413));
    peers.extend_from_slice(&v4_peer([0, 0, 0, 0], 0));
    let packet = announce_response(1800, 3, 4, &peers);

    assert_eq!(
        hex(&packet),
        "00000001123456780000070800000003000000047f000001c8d5000000000000"
    );

    let response = decode_announce(&packet, Family::V4);
    assert_eq!(response.interval, Duration::from_secs(1800));
    assert_eq!(response.incomplete, Some(3));
    assert_eq!(response.complete, Some(4));
    assert_eq!(response.peers.len(), 1);
    assert_eq!(response.peers[0].to_string(), "127.0.0.1:51413");
    assert_eq!(
        response.peer_warnings,
        PeerWarnings {
            dropped_peer_records: 1,
            ..PeerWarnings::default()
        }
    );
}

#[test]
fn duplicate_peers_are_deduplicated_keeping_the_first() {
    let mut peers = Vec::new();
    peers.extend_from_slice(&v4_peer([93, 184, 216, 34], 6881));
    peers.extend_from_slice(&v4_peer([93, 184, 216, 34], 6881));
    peers.extend_from_slice(&v4_peer([1, 1, 1, 1], 51413));
    let packet = announce_response(1800, 1, 2, &peers);

    let response = decode_announce(&packet, Family::V4);
    assert_eq!(response.peers.len(), 2);
    assert_eq!(response.peers[0].to_string(), "93.184.216.34:6881");
    assert_eq!(response.peers[1].to_string(), "1.1.1.1:51413");
    assert!(
        response.peer_warnings.is_empty(),
        "a duplicate is not a warning or a truncated peer"
    );
}

#[test]
fn a_twenty_byte_announce_response_has_no_peers() {
    let packet = announce_response(60, 0, 0, &[]);
    assert_eq!(packet.len(), ANNOUNCE_RESPONSE_MIN_LEN);

    let response = decode_announce(&packet, Family::V4);
    assert!(response.peers.is_empty());
    assert!(response.peer_warnings.is_empty());
}

#[test]
fn a_nineteen_byte_announce_response_is_ignored() {
    let mut packet = announce_response(60, 0, 0, &[]);
    packet.truncate(ANNOUNCE_RESPONSE_MIN_LEN - 1);
    assert_eq!(decode(&packet, TRANSACTION_ID, Family::V4), None);
}

#[test]
fn ipv6_peers_use_an_eighteen_byte_stride() {
    let address = Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1);
    let packet = announce_response(1800, 1, 2, &v6_peer(address.octets(), 6881));

    let response = decode_announce(&packet, Family::V6);
    assert_eq!(response.peers.len(), 1);
    assert_eq!(response.peers[0].to_string(), "[2001:db8::1]:6881");

    // The same bytes read as IPv4 records are three records: two unusable
    // ones and 0.0.0.1:6881. The family, not the packet, selects the stride.
    let as_v4 = decode_announce(&packet, Family::V4);
    assert_eq!(as_v4.peers.len(), 1);
    assert_eq!(as_v4.peers[0].to_string(), "0.0.0.1:6881");
    assert_eq!(as_v4.peer_warnings.malformed_peer_records, 0);
    assert_eq!(as_v4.peer_warnings.dropped_peer_records, 2);
}

#[test]
fn a_full_ipv6_response_is_3620_bytes_and_fits_the_buffer() {
    let mut peers = Vec::with_capacity(200 * IPV6_PEER_STRIDE);
    for index in 0..200u16 {
        let address = Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, index + 1);
        peers.extend_from_slice(&v6_peer(address.octets(), 6881));
    }
    let packet = announce_response(1800, 0, 200, &peers);

    assert_eq!(packet.len(), 3620);
    assert!(packet.len() <= MAX_DATAGRAM_LEN);

    let response = decode_announce(&packet, Family::V6);
    assert_eq!(response.peers.len(), 200);
    assert!(response.peer_warnings.is_empty());
    assert_eq!(response.complete, Some(200));
}

#[test]
fn a_trailing_partial_record_is_ignored() {
    let mut peers = Vec::new();
    peers.extend_from_slice(&v4_peer([127, 0, 0, 1], 6881));
    peers.extend_from_slice(&[1, 2, 3]);
    let packet = announce_response(1800, 1, 0, &peers);

    let response = decode_announce(&packet, Family::V4);
    assert_eq!(response.peers.len(), 1);
    assert_eq!(response.peer_warnings.malformed_peer_records, 1);
    assert_eq!(response.peer_warnings.dropped_peer_records, 0);
}

#[test]
fn invalid_peer_addresses_are_dropped() {
    let mut peers = Vec::new();
    peers.extend_from_slice(&v4_peer([127, 0, 0, 1], 6881));
    peers.extend_from_slice(&v4_peer([10, 0, 0, 1], 6881));
    peers.extend_from_slice(&v4_peer([0, 0, 0, 0], 6881));
    peers.extend_from_slice(&v4_peer([224, 0, 0, 1], 6881));
    peers.extend_from_slice(&v4_peer([255, 255, 255, 255], 6881));
    peers.extend_from_slice(&v4_peer([8, 8, 8, 8], 0));
    let packet = announce_response(1800, 0, 0, &peers);

    let response = decode_announce(&packet, Family::V4);
    assert_eq!(response.peers.len(), 2);
    assert_eq!(response.peer_warnings.dropped_peer_records, 4);
    assert!(!response.peer_warnings.is_empty());
}

#[test]
fn a_mapped_ipv6_peer_is_dropped() {
    let mapped = Ipv4Addr::new(127, 0, 0, 1).to_ipv6_mapped();
    let packet = announce_response(1800, 0, 0, &v6_peer(mapped.octets(), 6881));

    let response = decode_announce(&packet, Family::V6);
    assert!(response.peers.is_empty());
    assert_eq!(response.peer_warnings.dropped_peer_records, 1);
}

#[test]
fn the_peer_cap_is_two_hundred() {
    let mut peers = Vec::new();
    for index in 0..201u32 {
        let octets = Ipv4Addr::new(10, 0, 0, (index % 250) as u8 + 1).octets();
        peers.extend_from_slice(&v4_peer(octets, 6881));
    }
    let packet = announce_response(1800, 0, 0, &peers);

    let response = decode_announce(&packet, Family::V4);
    assert_eq!(response.peers.len(), 200);
    assert_eq!(response.peer_warnings.truncated_peers, 1);
}

#[test]
fn the_error_packet_is_classified_and_its_text_stays_out_of_diagnostics() {
    let mut packet = Vec::new();
    packet.extend_from_slice(&3u32.to_be_bytes());
    packet.extend_from_slice(&TRANSACTION_ID.to_be_bytes());
    packet.extend_from_slice(b"failure text");

    let decoded = decode(&packet, TRANSACTION_ID, Family::V4).expect("error packet");
    let Decoded::Error(message) = &decoded else {
        panic!("expected an error packet, got {decoded:?}");
    };
    assert_eq!(message.len(), b"failure text".len());

    let url = TrackerUrl::parse("http://tracker.example/announce").expect("valid tracker URL");
    assert_eq!(message.sanitized_text(&url), "failure text");

    let debug = format!("{decoded:?}");
    assert!(!debug.contains("failure text"));
    let error = TrackerError::Rejected {
        message: message.clone(),
    };
    assert!(!format!("{error:?}").contains("failure text"));
    assert!(!error.to_string().contains("failure text"));
}

#[test]
fn a_short_or_mismatched_error_packet_is_ignored() {
    let mut packet = Vec::new();
    packet.extend_from_slice(&3u32.to_be_bytes());
    packet.extend_from_slice(&TRANSACTION_ID.to_be_bytes());
    packet.extend_from_slice(b"failure text");
    packet.truncate(ERROR_MIN_LEN - 1);
    assert_eq!(decode(&packet, TRANSACTION_ID, Family::V4), None);

    let mut mismatched = Vec::new();
    mismatched.extend_from_slice(&3u32.to_be_bytes());
    mismatched.extend_from_slice(&(TRANSACTION_ID ^ 1).to_be_bytes());
    mismatched.extend_from_slice(b"failure text");
    assert_eq!(decode(&mismatched, TRANSACTION_ID, Family::V4), None);
}

#[test]
fn every_prefix_of_a_response_is_safe_to_decode() {
    let mut peers = Vec::new();
    for index in 0..200u16 {
        let address = Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, index + 1);
        peers.extend_from_slice(&v6_peer(address.octets(), 6881));
    }
    let packet = announce_response(1800, 0, 200, &peers);

    for length in 0..=packet.len() {
        let _ = decode(&packet[..length], TRANSACTION_ID, Family::V6);
    }
}

#[test]
fn the_codec_limits_are_the_contract() {
    assert_eq!(CONNECT_REQUEST_LEN, 16);
    assert_eq!(CONNECT_RESPONSE_LEN, 16);
    assert_eq!(ANNOUNCE_REQUEST_LEN, 98);
    assert_eq!(ANNOUNCE_RESPONSE_MIN_LEN, 20);
    assert_eq!(ERROR_MIN_LEN, 8);
    assert_eq!(MAX_DATAGRAM_LEN, 4096);
    assert_eq!(IPV4_PEER_STRIDE, 6);
    assert_eq!(IPV6_PEER_STRIDE, 18);
    assert_eq!(Family::V4.peer_stride(), IPV4_PEER_STRIDE);
    assert_eq!(Family::V6.peer_stride(), IPV6_PEER_STRIDE);
    assert_eq!(ANNOUNCE_RESPONSE_MIN_LEN + 200 * IPV6_PEER_STRIDE, 3620);
}
