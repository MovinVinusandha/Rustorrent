use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::bencode::Value;
use crate::tracker::TrackerError;

use super::{MAX_PEERS_PER_RESPONSE, PeerWarnings, decode_peers};

fn v4_record(address: Ipv4Addr, port: u16) -> [u8; 6] {
    let mut record = [0u8; 6];
    record[..4].copy_from_slice(&address.octets());
    record[4..].copy_from_slice(&port.to_be_bytes());
    record
}

fn v6_record(address: Ipv6Addr, port: u16) -> [u8; 18] {
    let mut record = [0u8; 18];
    record[..16].copy_from_slice(&address.octets());
    record[16..].copy_from_slice(&port.to_be_bytes());
    record
}

fn v4(a: u8, b: u8, c: u8, d: u8, port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(a, b, c, d)), port)
}

fn v6(address: &str, port: u16) -> SocketAddr {
    SocketAddr::new(
        IpAddr::V6(address.parse().expect("valid IPv6 literal")),
        port,
    )
}

fn response_dict<'a>(entries: Vec<(&'a [u8], Value<'a>)>) -> Value<'a> {
    Value::dict(entries).expect("response dictionary")
}

fn peer_entry<'a>(ip: &'a [u8], port: Option<i64>) -> Value<'a> {
    let mut entries = vec![(b"ip".as_slice(), Value::bytes(ip))];
    if let Some(port) = port {
        entries.push((b"port".as_slice(), Value::int(port)));
    }
    Value::dict(entries).expect("peer entry")
}

#[test]
fn compact_peers_are_decoded_and_filtered() {
    let mut bytes = Vec::new();
    bytes.extend(v4_record(Ipv4Addr::new(1, 2, 3, 4), 6881));
    bytes.extend(v4_record(Ipv4Addr::UNSPECIFIED, 0));
    bytes.extend(v4_record(Ipv4Addr::new(5, 6, 7, 8), 51413));

    let response = response_dict(vec![(b"peers".as_slice(), Value::bytes(&bytes))]);
    let (peers, warnings) = decode_peers(&response).expect("valid compact peers");

    assert_eq!(peers, vec![v4(1, 2, 3, 4, 6881), v4(5, 6, 7, 8, 51413)]);
    assert_eq!(warnings.dropped_peer_records, 1);
    assert_eq!(warnings.malformed_peer_records, 0);
    assert_eq!(warnings.truncated_peers, 0);
    assert!(!warnings.missing_peers);
}

#[test]
fn a_partial_compact_record_is_ignored_and_counted() {
    let mut bytes = Vec::new();
    bytes.extend(v4_record(Ipv4Addr::new(1, 2, 3, 4), 6881));
    bytes.extend(v4_record(Ipv4Addr::new(5, 6, 7, 8), 6882));
    bytes.push(0xff);

    let response = response_dict(vec![(b"peers".as_slice(), Value::bytes(&bytes))]);
    let (peers, warnings) = decode_peers(&response).expect("valid compact peers");

    assert_eq!(peers, vec![v4(1, 2, 3, 4, 6881), v4(5, 6, 7, 8, 6882)]);
    assert_eq!(warnings.malformed_peer_records, 1);
    assert_eq!(warnings.dropped_peer_records, 0);
}

#[test]
fn list_peers_filter_each_entry() {
    let entries = vec![
        peer_entry(b"1.2.3.4", Some(6881)),
        peer_entry(b"2001:db8::1", Some(6881)),
        peer_entry(b"tracker.example", Some(6881)),
        peer_entry(b"1.2.3.5", None),
        peer_entry(b"1.2.3.6", Some(0)),
        peer_entry(b"1.2.3.7", Some(65536)),
        Value::int(7),
        Value::dict(vec![
            (b"ip".as_slice(), Value::int(1)),
            (b"port".as_slice(), Value::int(6881)),
        ])
        .expect("wrong-typed peer entry"),
        peer_entry(b"::ffff:127.0.0.1", Some(6881)),
        peer_entry(b"1.2.3.8", Some(-1)),
        peer_entry(b"1.2.3.9", Some(65535)),
    ];

    let response = response_dict(vec![(b"peers".as_slice(), Value::list(entries))]);
    let (peers, warnings) = decode_peers(&response).expect("valid list peers");

    assert_eq!(
        peers,
        vec![
            v4(1, 2, 3, 4, 6881),
            v6("2001:db8::1", 6881),
            v4(1, 2, 3, 9, 65535),
        ]
    );
    assert_eq!(warnings.dropped_peer_records, 8);
    assert_eq!(warnings.malformed_peer_records, 0);
}

#[test]
fn wrong_typed_peer_fields_are_malformed() {
    assert_eq!(
        decode_peers(&Value::int(1)),
        Err(TrackerError::MalformedResponse)
    );

    for value in [
        Value::int(1),
        Value::dict(vec![]).expect("empty dictionary"),
    ] {
        let response = response_dict(vec![(b"peers".as_slice(), value)]);
        assert_eq!(
            decode_peers(&response),
            Err(TrackerError::MalformedResponse)
        );
    }

    for value in [
        Value::int(1),
        Value::list(vec![]),
        Value::dict(vec![]).expect("empty dictionary"),
    ] {
        let response = response_dict(vec![(b"peers6".as_slice(), value)]);
        assert_eq!(
            decode_peers(&response),
            Err(TrackerError::MalformedResponse)
        );
    }
}

#[test]
fn a_missing_peers_field_is_an_empty_swarm_with_a_warning() {
    let response = response_dict(vec![]);
    let (peers, warnings) = decode_peers(&response).expect("empty response");

    assert!(peers.is_empty());
    assert!(warnings.missing_peers);
    assert!(!warnings.is_empty());

    let record = v6_record(Ipv6Addr::LOCALHOST, 6881);
    let response = response_dict(vec![(b"peers6".as_slice(), Value::bytes(&record))]);
    let (peers, warnings) = decode_peers(&response).expect("peers6 only");

    assert_eq!(peers, vec![v6("::1", 6881)]);
    assert!(!warnings.missing_peers);
    assert!(warnings.is_empty());
}

#[test]
fn peers6_is_decoded_with_an_eighteen_byte_stride() {
    let mut bytes = Vec::new();
    bytes.extend(v6_record(Ipv6Addr::LOCALHOST, 51413));
    bytes.extend(v6_record("2001:db8::1".parse().expect("literal"), 6881));

    let response = response_dict(vec![(b"peers6".as_slice(), Value::bytes(&bytes))]);
    let (peers, warnings) = decode_peers(&response).expect("valid peers6");

    assert_eq!(peers, vec![v6("::1", 51413), v6("2001:db8::1", 6881)]);
    assert!(warnings.is_empty());
}

#[test]
fn partial_peers6_records_are_ignored_and_counted() {
    let record = v6_record(Ipv6Addr::LOCALHOST, 51413);

    let mut short = record.to_vec();
    short.pop();
    let response = response_dict(vec![(b"peers6".as_slice(), Value::bytes(&short))]);
    let (peers, warnings) = decode_peers(&response).expect("short peers6");
    assert!(peers.is_empty());
    assert_eq!(warnings.malformed_peer_records, 1);

    let mut long = record.to_vec();
    long.push(0);
    let response = response_dict(vec![(b"peers6".as_slice(), Value::bytes(&long))]);
    let (peers, warnings) = decode_peers(&response).expect("long peers6");
    assert_eq!(peers, vec![v6("::1", 51413)]);
    assert_eq!(warnings.malformed_peer_records, 1);
}

#[test]
fn peers6_filters_unusable_addresses() {
    let mut bytes = Vec::new();
    for (address, port) in [
        ("::", 1),
        ("ff02::1", 1),
        ("::ffff:127.0.0.1", 1),
        ("2001:db8::2", 0),
    ] {
        bytes.extend(v6_record(address.parse().expect("literal"), port));
    }

    let response = response_dict(vec![(b"peers6".as_slice(), Value::bytes(&bytes))]);
    let (peers, warnings) = decode_peers(&response).expect("filtered peers6");

    assert!(peers.is_empty());
    assert_eq!(warnings.dropped_peer_records, 4);
}

#[test]
fn lan_and_loopback_peers_are_kept() {
    let mut bytes = Vec::new();
    for (address, port) in [
        ("127.0.0.1", 1),
        ("10.0.0.1", 1),
        ("192.168.1.1", 1),
        ("169.254.1.1", 1),
        ("224.0.0.1", 1),
        ("255.255.255.255", 1),
        ("0.0.0.0", 1),
    ] {
        let address: Ipv4Addr = address.parse().expect("literal");
        bytes.extend(v4_record(address, port));
    }

    let response = response_dict(vec![(b"peers".as_slice(), Value::bytes(&bytes))]);
    let (peers, warnings) = decode_peers(&response).expect("filtered compact peers");

    assert_eq!(
        peers,
        vec![
            v4(127, 0, 0, 1, 1),
            v4(10, 0, 0, 1, 1),
            v4(192, 168, 1, 1, 1),
            v4(169, 254, 1, 1, 1),
        ]
    );
    assert_eq!(warnings.dropped_peer_records, 3);

    let mut bytes = Vec::new();
    for address in ["::1", "fe80::1", "fd00::1"] {
        bytes.extend(v6_record(address.parse().expect("literal"), 1));
    }
    let response = response_dict(vec![(b"peers6".as_slice(), Value::bytes(&bytes))]);
    let (peers, warnings) = decode_peers(&response).expect("valid LAN peers6");

    assert_eq!(
        peers,
        vec![v6("::1", 1), v6("fe80::1", 1), v6("fd00::1", 1)]
    );
    assert!(warnings.is_empty());
}

#[test]
fn compact_peers_are_capped_at_two_hundred() {
    let mut bytes = Vec::new();
    for index in 0..(MAX_PEERS_PER_RESPONSE + 1) as u32 {
        let address = Ipv4Addr::new(10, 0, (index / 256) as u8, (index % 256) as u8);
        bytes.extend(v4_record(address, 6881));
    }

    let response = response_dict(vec![(b"peers".as_slice(), Value::bytes(&bytes))]);
    let (peers, warnings) = decode_peers(&response).expect("capped compact peers");

    assert_eq!(peers.len(), MAX_PEERS_PER_RESPONSE);
    assert_eq!(warnings.truncated_peers, 1);
    assert_eq!(warnings.dropped_peer_records, 0);
}

#[test]
fn list_peers_are_capped_at_two_hundred() {
    let addresses: Vec<String> = (0..(MAX_PEERS_PER_RESPONSE + 1) as u32)
        .map(|index| format!("10.0.{}.{}", index / 256, index % 256))
        .collect();
    let entries = addresses
        .iter()
        .map(|address| peer_entry(address.as_bytes(), Some(6881)))
        .collect();

    let response = response_dict(vec![(b"peers".as_slice(), Value::list(entries))]);
    let (peers, warnings) = decode_peers(&response).expect("capped list peers");

    assert_eq!(peers.len(), MAX_PEERS_PER_RESPONSE);
    assert_eq!(warnings.truncated_peers, 1);
}

#[test]
fn duplicate_records_after_the_cap_are_counted_as_truncated() {
    let mut bytes = Vec::new();
    for index in 0..MAX_PEERS_PER_RESPONSE as u32 {
        let address = Ipv4Addr::new(10, 2, (index / 256) as u8, (index % 256) as u8);
        bytes.extend(v4_record(address, 6881));
    }
    bytes.extend(v4_record(Ipv4Addr::new(10, 2, 0, 0), 6881));

    let response = response_dict(vec![(b"peers".as_slice(), Value::bytes(&bytes))]);
    let (peers, warnings) = decode_peers(&response).expect("capped compact peers");

    assert_eq!(peers.len(), MAX_PEERS_PER_RESPONSE);
    assert_eq!(warnings.truncated_peers, 1);
}

#[test]
fn the_merged_lists_share_one_cap() {
    let mut ipv4 = Vec::new();
    for index in 0..150u32 {
        let address = Ipv4Addr::new(10, 1, (index / 256) as u8, (index % 256) as u8);
        ipv4.extend(v4_record(address, 1));
    }

    let mut ipv6 = Vec::new();
    for index in 0..100u16 {
        let address = Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, index + 1);
        ipv6.extend(v6_record(address, 1));
    }

    let response = response_dict(vec![
        (b"peers".as_slice(), Value::bytes(&ipv4)),
        (b"peers6".as_slice(), Value::bytes(&ipv6)),
    ]);
    let (peers, warnings) = decode_peers(&response).expect("merged peers");

    assert_eq!(peers.len(), MAX_PEERS_PER_RESPONSE);
    assert_eq!(warnings.truncated_peers, 50);
    assert_eq!(peers[0], v4(10, 1, 0, 0, 1));
    assert_eq!(peers[149], v4(10, 1, 0, 149, 1));
    assert_eq!(peers[150], v6("2001:db8::1", 1));
    assert_eq!(peers[199], v6("2001:db8::32", 1));
}

#[test]
fn duplicate_peers_keep_the_first_occurrence() {
    let mut bytes = Vec::new();
    bytes.extend(v4_record(Ipv4Addr::new(1, 2, 3, 4), 80));
    bytes.extend(v4_record(Ipv4Addr::new(1, 2, 3, 4), 80));
    bytes.extend(v4_record(Ipv4Addr::new(1, 2, 3, 4), 81));

    let response = response_dict(vec![(b"peers".as_slice(), Value::bytes(&bytes))]);
    let (peers, warnings) = decode_peers(&response).expect("duplicate compact peers");

    assert_eq!(peers, vec![v4(1, 2, 3, 4, 80), v4(1, 2, 3, 4, 81)]);
    assert!(warnings.is_empty());
}

#[test]
fn empty_peer_values_are_valid() {
    let response = response_dict(vec![
        (b"peers".as_slice(), Value::bytes(&[])),
        (b"peers6".as_slice(), Value::bytes(&[])),
    ]);
    let (peers, warnings) = decode_peers(&response).expect("empty peers");

    assert!(peers.is_empty());
    assert!(warnings.is_empty());
    assert!(!warnings.missing_peers);

    let response = response_dict(vec![(b"peers".as_slice(), Value::list(vec![]))]);
    let (peers, warnings) = decode_peers(&response).expect("empty peer list");
    assert!(peers.is_empty());
    assert!(warnings.is_empty());
}

#[test]
fn peer_warnings_start_empty() {
    let warnings = PeerWarnings::default();
    assert!(warnings.is_empty());
    assert_eq!(
        warnings,
        PeerWarnings {
            malformed_peer_records: 0,
            dropped_peer_records: 0,
            truncated_peers: 0,
            missing_peers: false,
        }
    );
}
