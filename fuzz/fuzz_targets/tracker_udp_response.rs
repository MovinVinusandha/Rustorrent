#![no_main]

//! Fuzzes the BEP 15 attempt state machine with arbitrary datagram sequences.
//!
//! The input drives both the schedule (when to advance the clock) and the
//! datagrams fed back. Structured connect, announce and error responses are
//! built from input bytes so the fuzzer reaches the announce path; raw chunks
//! are fed unchanged too.
//!
//! Invariants checked:
//! - at most [`MAX_SENDS`] datagrams are sent in one attempt;
//! - a retransmission keeps its transaction ID, a new request gets a fresh
//!   one, and IDs never go backwards;
//! - a wait never passes the absolute deadline, and a timeout happens only at
//!   or after it;
//! - a response never carries more than [`MAX_PEERS_PER_RESPONSE`] peers;
//! - tracker error text never appears in `Debug` or `Display`.

use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use tokio::time::Instant;

use bt_engine::tracker::udp::attempt::{
    ATTEMPT_DEADLINE, AttemptOutcome, MAX_SENDS, STOPPED_DEADLINE, Step, TransactionIds, UdpAttempt,
};
use bt_engine::tracker::udp::codec::{ConnectionId, Family, MAX_DATAGRAM_LEN, UdpAnnounceResponse};
use bt_engine::tracker::{MAX_PEERS_PER_RESPONSE, TrackerMessage};

/// Marker appended to every structured error packet; diagnostics must hide it.
const SECRET: &str = "SECRET-TRACKER-TEXT-1234";

/// Upper bound on state-machine iterations for one input.
const MAX_STEPS: usize = 32;

struct Input<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Input<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn byte(&mut self) -> u8 {
        let byte = self.data.get(self.position).copied().unwrap_or(0);
        self.position = self.position.saturating_add(1);
        byte
    }

    fn bytes(&mut self, length: usize) -> &'a [u8] {
        let start = self.position.min(self.data.len());
        let end = start.saturating_add(length).min(self.data.len());
        self.position = end;
        &self.data[start..end]
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.position)
    }

    fn array<const N: usize>(&mut self) -> [u8; N] {
        let mut output = [0u8; N];
        let bytes = self.bytes(N);
        output[..bytes.len()].copy_from_slice(bytes);
        output
    }

    fn datagram(&mut self) -> &'a [u8] {
        let length = usize::from(self.byte()).min(MAX_DATAGRAM_LEN);
        self.bytes(length)
    }

    fn family(&mut self) -> Family {
        if self.byte() & 1 == 0 {
            Family::V4
        } else {
            Family::V6
        }
    }

    fn error_text(&mut self) -> Vec<u8> {
        let length = usize::from(self.byte());
        let mut text = self.bytes(length).to_vec();
        text.extend_from_slice(SECRET.as_bytes());
        text
    }
}

struct CountingIds(u32);

impl TransactionIds for CountingIds {
    fn next(&mut self) -> Option<u32> {
        self.0 = self.0.wrapping_add(1);
        Some(self.0)
    }
}

fn connect_response(transaction_id: u32, connection_id: ConnectionId) -> Vec<u8> {
    let mut packet = Vec::with_capacity(16);
    packet.extend_from_slice(&0u32.to_be_bytes());
    packet.extend_from_slice(&transaction_id.to_be_bytes());
    packet.extend_from_slice(&connection_id.get().to_be_bytes());
    packet
}

fn error_response(transaction_id: u32, text: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(8 + text.len());
    packet.extend_from_slice(&3u32.to_be_bytes());
    packet.extend_from_slice(&transaction_id.to_be_bytes());
    packet.extend_from_slice(text);
    packet
}

fn announce_response(
    transaction_id: u32,
    family: Family,
    count: usize,
    input: &mut Input<'_>,
) -> Vec<u8> {
    let count = match family {
        // More than the peer cap: the decoder must drop the extra records.
        Family::V4 => count,
        // Keep a v6 response inside the 4096-byte receive buffer.
        Family::V6 => count.min(MAX_PEERS_PER_RESPONSE),
    };

    let mut packet = Vec::with_capacity(20 + count * 18);
    packet.extend_from_slice(&1u32.to_be_bytes());
    packet.extend_from_slice(&transaction_id.to_be_bytes());
    packet.extend_from_slice(&1800u32.to_be_bytes());
    packet.extend_from_slice(&2u32.to_be_bytes());
    packet.extend_from_slice(&3u32.to_be_bytes());

    for _ in 0..count {
        match family {
            Family::V4 => {
                packet.extend_from_slice(&[127, 0, 0, 1]);
                packet.extend_from_slice(&input.array::<2>());
            }
            Family::V6 => {
                packet.extend_from_slice(&input.array::<16>());
                packet.extend_from_slice(&input.array::<2>());
            }
        }
    }

    assert!(
        packet.len() <= MAX_DATAGRAM_LEN,
        "response exceeds the receive buffer"
    );
    packet
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SendKind {
    Connect,
    Announce,
}

fn check_send(
    kind: SendKind,
    transaction_id: u32,
    last: &mut Option<(SendKind, u32)>,
    max_id: &mut u32,
    sends: &mut usize,
) {
    *sends += 1;
    assert!(
        *sends <= usize::from(MAX_SENDS),
        "attempt sent more than MAX_SENDS datagrams"
    );

    match *last {
        Some((last_kind, last_id)) if last_kind == kind && last_id == transaction_id => {}
        Some((last_kind, last_id)) if last_kind == kind => {
            panic!("retransmission changed transaction ID: {last_id} -> {transaction_id}");
        }
        _ => {
            assert!(
                transaction_id > *max_id,
                "fresh request reused an ID: {transaction_id} <= {max_id}"
            );
            *max_id = transaction_id;
        }
    }
    *last = Some((kind, transaction_id));
}

fn check_response(response: &UdpAnnounceResponse) {
    assert!(
        response.peers.len() <= MAX_PEERS_PER_RESPONSE,
        "response carried more peers than the cap"
    );
}

fn check_message(message: &TrackerMessage) {
    let debug = format!("{message:?}");
    let display = message.to_string();
    assert!(!debug.contains(SECRET), "Debug leaked tracker error text");
    assert!(
        !display.contains(SECRET),
        "Display leaked tracker error text"
    );
}

fn check_outcome(outcome: AttemptOutcome, deadline: Instant, now: Instant) {
    match outcome {
        AttemptOutcome::Announce(response) => check_response(&response),
        AttemptOutcome::Rejected(message) => check_message(&message),
        AttemptOutcome::Timeout => assert!(now >= deadline, "timeout before the deadline"),
        AttemptOutcome::Failed(error) => {
            panic!("deterministic transaction source failed: {error:?}")
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let mut input = Input::new(data);
    let stopped = input.byte() & 1 == 1;
    let start = Instant::now();
    let deadline = start
        + if stopped {
            STOPPED_DEADLINE
        } else {
            ATTEMPT_DEADLINE
        };
    let mut attempt = UdpAttempt::new(start, stopped, Box::new(CountingIds(0)));
    let mut now = start;
    let mut last = None;
    let mut max_id = 0;
    let mut sends = 0;

    for _ in 0..MAX_STEPS {
        // A raw datagram first: the decoder sees it in whatever state the
        // previous iteration left behind.
        if input.remaining() > 0 {
            let family = input.family();
            let datagram = input.datagram();
            attempt.on_datagram(now, datagram, family);
        }

        match attempt.step(now) {
            Step::Connect { transaction_id } => {
                check_send(
                    SendKind::Connect,
                    transaction_id,
                    &mut last,
                    &mut max_id,
                    &mut sends,
                );
                let family = input.family();
                match input.byte() % 4 {
                    0 => {
                        let connection_id =
                            ConnectionId::new(u64::from_be_bytes(input.array::<8>()));
                        attempt.on_datagram(
                            now,
                            &connect_response(transaction_id, connection_id),
                            family,
                        );
                    }
                    1 => {
                        let text = input.error_text();
                        attempt.on_datagram(now, &error_response(transaction_id, &text), family);
                    }
                    2 => {
                        let datagram = input.datagram();
                        attempt.on_datagram(now, datagram, family);
                    }
                    _ => {}
                }
            }
            Step::Announce { transaction_id, .. } => {
                check_send(
                    SendKind::Announce,
                    transaction_id,
                    &mut last,
                    &mut max_id,
                    &mut sends,
                );
                let family = input.family();
                match input.byte() % 4 {
                    0 => {
                        let count = usize::from(input.byte());
                        let packet = announce_response(transaction_id, family, count, &mut input);
                        attempt.on_datagram(now, &packet, family);
                    }
                    1 => {
                        let text = input.error_text();
                        attempt.on_datagram(now, &error_response(transaction_id, &text), family);
                    }
                    2 => {
                        let datagram = input.datagram();
                        attempt.on_datagram(now, datagram, family);
                    }
                    _ => {}
                }
            }
            Step::Wait { until } => {
                assert!(until <= deadline, "wait scheduled past the deadline");
                let extra = Duration::from_secs(u64::from(input.byte() % 61));
                now = (until + extra).min(deadline);
            }
            Step::Done(outcome) => {
                check_outcome(outcome, deadline, now);
                return;
            }
        }
    }
});
