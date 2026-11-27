use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use crate::bencode::{self, Value};
use crate::tracker::{MAX_PEERS_PER_RESPONSE, TrackerError, TrackerId, TrackerUrl};

use super::{
    AnnounceResponse, DEFAULT_INTERVAL_SECS, MAX_INTERVAL_SECS, MAX_RESPONSE_BYTES,
    MIN_INTERVAL_SECS, decode_response,
};

fn body(entries: Vec<(&[u8], Value<'_>)>) -> Vec<u8> {
    bencode::encode(&Value::dict(entries).expect("dictionary")).expect("bencode")
}

fn decode(entries: Vec<(&[u8], Value<'_>)>) -> Result<AnnounceResponse, TrackerError> {
    decode_response(&body(entries))
}

fn compact_v4(peers: &[(Ipv4Addr, u16)]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(peers.len() * 6);
    for (address, port) in peers {
        bytes.extend(address.octets());
        bytes.extend(port.to_be_bytes());
    }
    bytes
}

fn v4(a: u8, b: u8, c: u8, d: u8, port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(a, b, c, d)), port)
}

#[test]
fn decodes_a_full_response() {
    let peers = compact_v4(&[(Ipv4Addr::new(1, 2, 3, 4), 6881)]);
    let response = decode(vec![
        (b"interval".as_slice(), Value::int(1800)),
        (b"min interval".as_slice(), Value::int(900)),
        (b"complete".as_slice(), Value::int(10)),
        (b"incomplete".as_slice(), Value::int(20)),
        (b"peers".as_slice(), Value::bytes(&peers)),
        (b"tracker id".as_slice(), Value::bytes(b"tracker-1")),
        (b"warning message".as_slice(), Value::bytes(b"careful")),
    ])
    .expect("valid response");

    assert_eq!(response.interval, Duration::from_secs(1800));
    assert_eq!(response.min_interval, Some(Duration::from_secs(900)));
    assert_eq!(response.complete, Some(10));
    assert_eq!(response.incomplete, Some(20));
    assert_eq!(response.peers, vec![v4(1, 2, 3, 4, 6881)]);
    assert_eq!(
        response.tracker_id.as_ref().map(TrackerId::as_bytes),
        Some(&b"tracker-1"[..])
    );
    assert!(response.warning.is_some());
    assert!(response.peer_warnings.is_empty());
}

#[test]
fn a_missing_interval_defaults_to_1800() {
    let response = decode(vec![(b"peers".as_slice(), Value::bytes(&[]))]).expect("valid response");

    assert_eq!(
        response.interval,
        Duration::from_secs(DEFAULT_INTERVAL_SECS)
    );
    assert_eq!(response.min_interval, None);
    assert!(response.peers.is_empty());
    assert!(response.peer_warnings.is_empty());
}

#[test]
fn interval_is_clamped_to_the_documented_range() {
    for (value, expected) in [
        (None, DEFAULT_INTERVAL_SECS),
        (Some(0), MIN_INTERVAL_SECS),
        (Some(-5), MIN_INTERVAL_SECS),
        (Some(59), MIN_INTERVAL_SECS),
        (Some(60), MIN_INTERVAL_SECS),
        (Some(1800), 1800),
        (Some(86400), MAX_INTERVAL_SECS),
        (Some(86401), MAX_INTERVAL_SECS),
        (Some(i64::MAX), MAX_INTERVAL_SECS),
    ] {
        let entries = match value {
            Some(seconds) => vec![(b"interval".as_slice(), Value::int(seconds))],
            None => vec![],
        };
        let response = decode(entries).expect("valid interval");
        assert_eq!(
            response.interval,
            Duration::from_secs(expected),
            "interval {value:?}"
        );
        assert_eq!(response.min_interval, None);
    }
}

#[test]
fn min_interval_is_clamped_and_lifts_the_effective_interval() {
    for (interval, minimum, effective, stored) in [
        (100, 5000, 5000, 5000),
        (100, 0, 100, 60),
        (100, -1, 100, 60),
        (100, 86401, 86400, 86400),
        (1800, 5000, 5000, 5000),
    ] {
        let response = decode(vec![
            (b"interval".as_slice(), Value::int(interval)),
            (b"min interval".as_slice(), Value::int(minimum)),
        ])
        .expect("valid intervals");

        assert_eq!(
            response.interval,
            Duration::from_secs(effective),
            "interval {interval}, min {minimum}"
        );
        assert_eq!(
            response.min_interval,
            Some(Duration::from_secs(stored)),
            "interval {interval}, min {minimum}"
        );
    }

    let response =
        decode(vec![(b"min interval".as_slice(), Value::int(5000))]).expect("valid min interval");
    assert_eq!(response.interval, Duration::from_secs(5000));
    assert_eq!(response.min_interval, Some(Duration::from_secs(5000)));
}

#[test]
fn non_integer_and_oversized_intervals_are_missing() {
    let response = decode(vec![
        (b"interval".as_slice(), Value::bytes(b"1800")),
        (b"min interval".as_slice(), Value::list(vec![])),
    ])
    .expect("non-integer intervals are ignored");
    assert_eq!(
        response.interval,
        Duration::from_secs(DEFAULT_INTERVAL_SECS)
    );
    assert_eq!(response.min_interval, None);

    let response = decode_response(b"d8:intervali9223372036854775808e5:peers0:e")
        .expect("oversized interval is ignored");
    assert_eq!(
        response.interval,
        Duration::from_secs(DEFAULT_INTERVAL_SECS)
    );
    assert_eq!(response.min_interval, None);

    let response = decode_response(b"d12:min intervali-9223372036854775809e5:peers0:e")
        .expect("oversized min interval is ignored");
    assert_eq!(response.min_interval, None);
}

#[test]
fn counters_are_optional_and_negative_or_wrong_typed_values_are_ignored() {
    let response = decode(vec![
        (b"complete".as_slice(), Value::int(0)),
        (b"incomplete".as_slice(), Value::int(-1)),
    ])
    .expect("valid counters");
    assert_eq!(response.complete, Some(0));
    assert_eq!(response.incomplete, None);

    let response = decode(vec![
        (b"complete".as_slice(), Value::bytes(b"10")),
        (b"incomplete".as_slice(), Value::int(i64::MAX)),
    ])
    .expect("wrong-typed counter is ignored");
    assert_eq!(response.complete, None);
    assert_eq!(response.incomplete, Some(i64::MAX as u64));
}

#[test]
fn tracker_id_is_ignored_when_too_long_or_wrong_typed() {
    let long = vec![b'x'; 257];
    let response = decode(vec![(b"tracker id".as_slice(), Value::bytes(&long))])
        .expect("oversized tracker id is ignored");
    assert!(response.tracker_id.is_none());

    let accepted = vec![b'x'; 256];
    let response = decode(vec![(b"tracker id".as_slice(), Value::bytes(&accepted))])
        .expect("tracker id at the cap");
    assert_eq!(response.tracker_id.as_ref().map(TrackerId::len), Some(256));

    let response = decode(vec![(b"tracker id".as_slice(), Value::int(1))])
        .expect("wrong-typed tracker id is ignored");
    assert!(response.tracker_id.is_none());
}

#[test]
fn warning_messages_are_stored_and_sanitized() {
    let url = TrackerUrl::parse("https://tracker.example/announce?passkey=SECRET")
        .expect("valid tracker URL");
    let response = decode(vec![(
        b"warning message".as_slice(),
        Value::bytes(b"denied SECRET"),
    )])
    .expect("valid warning");

    let warning = response.warning.expect("stored warning");
    assert_eq!(warning.sanitized_text(&url), "denied <redacted>");
    assert!(!format!("{warning:?}").contains("SECRET"));
}

#[test]
fn failure_reason_wins_and_peers_are_ignored() {
    let url = TrackerUrl::parse("https://tracker.example/announce?passkey=SECRET")
        .expect("valid tracker URL");
    let result = decode(vec![
        (b"failure reason".as_slice(), Value::bytes(b"denied SECRET")),
        (b"peers".as_slice(), Value::int(1)),
    ]);

    match result {
        Err(TrackerError::Rejected { message }) => {
            assert_eq!(message.sanitized_text(&url), "denied <redacted>");
            assert!(!format!("{message:?}").contains("SECRET"));
        }
        other => panic!("expected Rejected, got {other:?}"),
    }

    assert_eq!(
        decode(vec![(b"failure reason".as_slice(), Value::int(1))]),
        Err(TrackerError::MalformedResponse)
    );
}

#[test]
fn malformed_bodies_are_rejected() {
    for body in [
        b"i1e".as_slice(),
        b"le".as_slice(),
        b"3:abc".as_slice(),
        b"deX".as_slice(),
        b"".as_slice(),
    ] {
        assert_eq!(
            decode_response(body),
            Err(TrackerError::MalformedResponse),
            "body {body:?}"
        );
    }

    let mut deep = vec![b'l'; 65];
    deep.extend(vec![b'e'; 65]);
    assert_eq!(decode_response(&deep), Err(TrackerError::MalformedResponse));
}

#[test]
fn bodies_over_one_mib_are_too_large() {
    let body = vec![0u8; MAX_RESPONSE_BYTES + 1];
    assert_eq!(decode_response(&body), Err(TrackerError::TooLarge));
}

#[test]
fn a_body_at_the_cap_is_accepted() {
    let payload = MAX_RESPONSE_BYTES - 17;
    let records = payload / 6;
    let mut body = format!("d5:peers{payload}:").into_bytes();
    for index in 0..records {
        let address = [10, 0, (index / 256) as u8, (index % 200 + 1) as u8];
        body.extend(address);
        body.extend(6881u16.to_be_bytes());
    }
    body.extend(vec![0u8; payload - records * 6]);
    body.push(b'e');
    assert_eq!(body.len(), MAX_RESPONSE_BYTES);

    let response = decode_response(&body).expect("body at the cap");
    assert_eq!(response.peers.len(), MAX_PEERS_PER_RESPONSE);
    assert_eq!(response.peer_warnings.malformed_peer_records, 1);
    assert!(response.peer_warnings.truncated_peers > 0);
}

#[test]
fn wrong_typed_peer_fields_are_malformed() {
    assert_eq!(
        decode(vec![(b"peers".as_slice(), Value::int(1))]),
        Err(TrackerError::MalformedResponse)
    );
    assert_eq!(
        decode(vec![(b"peers6".as_slice(), Value::int(1))]),
        Err(TrackerError::MalformedResponse)
    );
}

#[test]
fn unsorted_keys_are_accepted_and_duplicates_are_rejected() {
    let response = decode_response(b"d5:peers0:8:intervali1800ee").expect("unsorted response keys");
    assert_eq!(response.interval, Duration::from_secs(1800));

    assert_eq!(
        decode_response(b"d8:intervali1800e8:intervali1800e5:peers0:e"),
        Err(TrackerError::MalformedResponse)
    );
}

#[test]
fn unknown_keys_are_ignored() {
    let response = decode(vec![
        (b"another".as_slice(), Value::list(vec![])),
        (b"unknown".as_slice(), Value::int(1)),
    ])
    .expect("unknown keys are ignored");

    assert_eq!(
        response.interval,
        Duration::from_secs(DEFAULT_INTERVAL_SECS)
    );
    assert!(response.peers.is_empty());
    assert!(response.peer_warnings.missing_peers);
}

#[test]
fn an_oversized_integer_in_an_unknown_key_is_ignored() {
    let response = decode_response(b"d5:peers0:7:unknowni9223372036854775808ee")
        .expect("unknown oversized integer is ignored");
    assert!(response.peers.is_empty());
}

#[test]
fn response_debug_never_contains_tracker_text() {
    let response = decode(vec![
        (b"tracker id".as_slice(), Value::bytes(b"SECRET-ID")),
        (
            b"warning message".as_slice(),
            Value::bytes(b"SECRET-WARNING"),
        ),
    ])
    .expect("valid response");

    let debug = format!("{response:?}");
    assert!(!debug.contains("SECRET"));
    assert!(debug.contains("TrackerId("));
    assert!(debug.contains("TrackerMessage("));
}
