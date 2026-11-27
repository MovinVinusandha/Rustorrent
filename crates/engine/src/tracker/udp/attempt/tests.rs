use std::time::Duration;

use tokio::time::Instant;

use crate::tracker::udp::codec::{ConnectionId, Family};
use crate::tracker::{AnnounceResponse, PeerWarnings, TrackerError, TrackerMessage};

use super::{
    ATTEMPT_DEADLINE, AttemptOutcome, MAX_SENDS, RECONNECT_AGE, STOPPED_DEADLINE, Step,
    TransactionIds, UdpAttempt,
};

const FAMILY: Family = Family::V4;
const CONNECTION_ID: ConnectionId = ConnectionId::new(0x0102_0304_0506_0708);
const OTHER_CONNECTION_ID: ConnectionId = ConnectionId::new(0x1112_1314_1516_1718);

struct CountingIds(u32);

impl CountingIds {
    fn new() -> Box<Self> {
        Box::new(Self(0x1000))
    }
}

impl TransactionIds for CountingIds {
    fn next(&mut self) -> Option<u32> {
        self.0 = self.0.wrapping_add(1);
        Some(self.0)
    }
}

fn attempt(now: Instant, stopped: bool) -> UdpAttempt {
    UdpAttempt::new(now, stopped, CountingIds::new())
}

fn connect_response(transaction_id: u32, connection_id: ConnectionId) -> Vec<u8> {
    let mut packet = Vec::new();
    packet.extend_from_slice(&0u32.to_be_bytes());
    packet.extend_from_slice(&transaction_id.to_be_bytes());
    packet.extend_from_slice(&connection_id.get().to_be_bytes());
    packet
}

fn announce_response(transaction_id: u32) -> Vec<u8> {
    let mut packet = Vec::new();
    packet.extend_from_slice(&1u32.to_be_bytes());
    packet.extend_from_slice(&transaction_id.to_be_bytes());
    packet.extend_from_slice(&1800u32.to_be_bytes());
    packet.extend_from_slice(&0u32.to_be_bytes());
    packet.extend_from_slice(&0u32.to_be_bytes());
    packet
}

fn error_response(transaction_id: u32, text: &[u8]) -> Vec<u8> {
    let mut packet = Vec::new();
    packet.extend_from_slice(&3u32.to_be_bytes());
    packet.extend_from_slice(&transaction_id.to_be_bytes());
    packet.extend_from_slice(text);
    packet
}

fn expected_response() -> AnnounceResponse {
    AnnounceResponse {
        interval: Duration::from_secs(1800),
        min_interval: None,
        complete: Some(0),
        incomplete: Some(0),
        peers: Vec::new(),
        tracker_id: None,
        warning: None,
        peer_warnings: PeerWarnings::default(),
    }
}

async fn advance(duration: Duration) -> Instant {
    tokio::time::advance(duration).await;
    Instant::now()
}

#[tokio::test(start_paused = true)]
async fn the_schedule_sends_at_zero_fifteen_and_forty_five() {
    let start = Instant::now();
    let mut attempt = attempt(start, false);

    let Step::Connect { transaction_id } = attempt.step(start) else {
        panic!("the first send must be a connect");
    };
    assert_eq!(attempt.deadline(), start + ATTEMPT_DEADLINE);

    let now = advance(Duration::from_secs(14)).await;
    assert_eq!(
        attempt.step(now),
        Step::Wait {
            until: start + Duration::from_secs(15)
        }
    );

    let now = advance(Duration::from_secs(1)).await;
    assert_eq!(
        attempt.step(now),
        Step::Connect { transaction_id },
        "the second send retransmits the connect"
    );

    let now = advance(Duration::from_secs(30)).await;
    assert_eq!(
        attempt.step(now),
        Step::Connect { transaction_id },
        "the third send retransmits the connect"
    );

    let now = advance(Duration::from_secs(59)).await;
    assert_eq!(
        attempt.step(now),
        Step::Wait {
            until: start + ATTEMPT_DEADLINE
        }
    );

    let now = advance(Duration::from_secs(1)).await;
    assert_eq!(attempt.step(now), Step::Done(AttemptOutcome::Timeout));
}

#[tokio::test(start_paused = true)]
async fn the_attempt_is_still_alive_at_sixty_seconds() {
    let start = Instant::now();
    let mut attempt = attempt(start, false);

    assert!(matches!(attempt.step(start), Step::Connect { .. }));
    let now = advance(Duration::from_secs(15)).await;
    assert!(matches!(attempt.step(now), Step::Connect { .. }));
    let now = advance(Duration::from_secs(30)).await;
    assert!(matches!(attempt.step(now), Step::Connect { .. }));

    let now = advance(Duration::from_secs(15)).await;
    assert_eq!(
        attempt.step(now),
        Step::Wait {
            until: start + ATTEMPT_DEADLINE
        }
    );
}

#[tokio::test(start_paused = true)]
async fn a_connect_response_schedules_the_announce_immediately() {
    let start = Instant::now();
    let mut attempt = attempt(start, false);

    let Step::Connect {
        transaction_id: connect_transaction,
    } = attempt.step(start)
    else {
        panic!("the first send must be a connect");
    };

    let now = advance(Duration::from_secs(1)).await;
    attempt.on_datagram(
        now,
        &connect_response(connect_transaction, CONNECTION_ID),
        FAMILY,
    );

    let Step::Announce {
        transaction_id,
        connection_id,
    } = attempt.step(now)
    else {
        panic!("the connect response must schedule an announce");
    };
    assert_eq!(connection_id, CONNECTION_ID);
    assert_ne!(transaction_id, connect_transaction);
    assert_eq!(
        attempt.step(now),
        Step::Wait {
            until: now + Duration::from_secs(30)
        }
    );
}

#[tokio::test(start_paused = true)]
async fn a_connect_retransmission_reuses_the_transaction_id() {
    let start = Instant::now();
    let mut attempt = attempt(start, false);

    let Step::Connect { transaction_id } = attempt.step(start) else {
        panic!("the first send must be a connect");
    };

    let now = advance(Duration::from_secs(15)).await;
    assert_eq!(
        attempt.step(now),
        Step::Connect { transaction_id },
        "the retransmission keeps the ID of the first send"
    );

    let now = advance(Duration::from_secs(1)).await;
    attempt.on_datagram(
        now,
        &connect_response(transaction_id, CONNECTION_ID),
        FAMILY,
    );
    assert!(matches!(attempt.step(now), Step::Announce { .. }));
}

#[tokio::test(start_paused = true)]
async fn an_announce_retransmission_reuses_the_transaction_id() {
    let start = Instant::now();
    let mut attempt = attempt(start, false);

    let Step::Connect {
        transaction_id: connect_transaction,
    } = attempt.step(start)
    else {
        panic!("the first send must be a connect");
    };

    let now = advance(Duration::from_secs(1)).await;
    attempt.on_datagram(
        now,
        &connect_response(connect_transaction, CONNECTION_ID),
        FAMILY,
    );
    let Step::Announce {
        transaction_id: announce_transaction,
        ..
    } = attempt.step(now)
    else {
        panic!("the connect response must schedule an announce");
    };

    let now = advance(Duration::from_secs(30)).await;
    assert_eq!(
        attempt.step(now),
        Step::Announce {
            transaction_id: announce_transaction,
            connection_id: CONNECTION_ID
        },
        "the announce retransmission keeps its ID"
    );

    let now = advance(Duration::from_secs(1)).await;
    attempt.on_datagram(now, &announce_response(announce_transaction), FAMILY);
    assert_eq!(
        attempt.step(now),
        Step::Done(AttemptOutcome::Announce(expected_response())),
        "a late reply to the first announce is still accepted"
    );
}

#[tokio::test(start_paused = true)]
async fn the_fifty_second_guard_reconnects_before_the_announce() {
    let received = Instant::now();

    let at_49 = received + Duration::from_secs(49);
    let mut fresh = UdpAttempt::with_connection(at_49, received, CONNECTION_ID, CountingIds::new());
    assert_eq!(
        fresh.step(at_49),
        Step::Announce {
            transaction_id: 0x1001,
            connection_id: CONNECTION_ID
        }
    );

    let at_50 = received + Duration::from_secs(50);
    let mut boundary =
        UdpAttempt::with_connection(at_50, received, CONNECTION_ID, CountingIds::new());
    let Step::Connect {
        transaction_id: reconnect_transaction,
    } = boundary.step(at_50)
    else {
        panic!("a connection ID at the age limit must be replaced");
    };
    assert_eq!(
        reconnect_transaction, 0x1002,
        "the reconnect is a new request with a fresh ID"
    );

    let at_65 = at_50 + Duration::from_secs(15);
    assert_eq!(
        boundary.step(at_65),
        Step::Connect {
            transaction_id: reconnect_transaction
        },
        "the reconnect is retransmitted with the same ID"
    );

    let at_51 = received + Duration::from_secs(51);
    let mut stale = UdpAttempt::with_connection(at_51, received, CONNECTION_ID, CountingIds::new());
    assert!(matches!(stale.step(at_51), Step::Connect { .. }));
    assert_eq!(RECONNECT_AGE, Duration::from_secs(50));
}

#[tokio::test(start_paused = true)]
async fn the_final_send_keeps_an_old_connection_id() {
    let start = Instant::now();
    let mut attempt = attempt(start, false);

    let Step::Connect {
        transaction_id: connect_transaction,
    } = attempt.step(start)
    else {
        panic!("the first send must be a connect");
    };

    let received = advance(Duration::from_secs(1)).await;
    attempt.on_datagram(
        received,
        &connect_response(connect_transaction, CONNECTION_ID),
        FAMILY,
    );
    let Step::Announce {
        transaction_id: announce_transaction,
        ..
    } = attempt.step(received)
    else {
        panic!("the connect response must schedule an announce");
    };

    // The third send is due at 31 s. Delay it to 56 s, so the ID is 55 s old
    // at that final send.
    let now = advance(Duration::from_secs(55)).await;
    assert_eq!(
        attempt.step(now),
        Step::Announce {
            transaction_id: announce_transaction,
            connection_id: CONNECTION_ID
        },
        "the final send must use the existing ID: a reconnect cannot finish"
    );
}

#[tokio::test(start_paused = true)]
async fn a_non_final_delayed_send_still_reconnects() {
    let received = Instant::now();
    let mut attempt =
        UdpAttempt::with_connection(received, received, CONNECTION_ID, CountingIds::new());

    let Step::Announce {
        transaction_id: announce_transaction,
        ..
    } = attempt.step(received)
    else {
        panic!("the first send must be the announce");
    };

    // The second send is due at 15 s. Delay it to 55 s; it is not the final
    // slot, so the guard must replace the old ID with a fresh connect.
    let now = received + Duration::from_secs(55);
    let Step::Connect {
        transaction_id: reconnect_transaction,
    } = attempt.step(now)
    else {
        panic!("a non-final send with an old ID must reconnect");
    };
    assert_ne!(reconnect_transaction, announce_transaction);
    assert_eq!(
        reconnect_transaction, 0x1002,
        "the reconnect is a new request with a fresh ID"
    );
}

#[tokio::test(start_paused = true)]
async fn a_stopped_announce_has_a_two_second_deadline_and_no_retries() {
    let start = Instant::now();
    let mut attempt = attempt(start, true);
    assert_eq!(attempt.deadline(), start + STOPPED_DEADLINE);

    let Step::Connect { transaction_id } = attempt.step(start) else {
        panic!("the first send must be a connect");
    };

    let now = advance(Duration::from_millis(100)).await;
    attempt.on_datagram(
        now,
        &connect_response(transaction_id, CONNECTION_ID),
        FAMILY,
    );
    assert!(matches!(attempt.step(now), Step::Announce { .. }));

    let now = advance(Duration::from_millis(500)).await;
    assert_eq!(
        attempt.step(now),
        Step::Wait {
            until: start + STOPPED_DEADLINE
        },
        "a stopped announce is not retransmitted"
    );

    let now = advance(Duration::from_millis(1400)).await;
    assert_eq!(attempt.step(now), Step::Done(AttemptOutcome::Timeout));
}

#[tokio::test(start_paused = true)]
async fn ten_thousand_bad_datagrams_do_not_move_the_deadline() {
    let start = Instant::now();
    let mut attempt = attempt(start, false);

    let Step::Connect { transaction_id } = attempt.step(start) else {
        panic!("the first send must be a connect");
    };
    let deadline = attempt.deadline();
    let next = start + Duration::from_secs(15);

    let wrong_action = announce_response(transaction_id);
    let wrong_transaction = announce_response(transaction_id ^ 1);
    for _ in 0..10_000 {
        attempt.on_datagram(start, &wrong_action, FAMILY);
        attempt.on_datagram(start, &wrong_transaction, FAMILY);
        attempt.on_datagram(start, b"garbage", FAMILY);
    }

    assert_eq!(attempt.deadline(), deadline);
    assert_eq!(attempt.step(start), Step::Wait { until: next });

    attempt.on_datagram(
        start,
        &connect_response(transaction_id, CONNECTION_ID),
        FAMILY,
    );
    assert!(matches!(attempt.step(start), Step::Announce { .. }));
}

#[tokio::test(start_paused = true)]
async fn an_error_packet_ends_the_attempt() {
    let start = Instant::now();
    let mut attempt = attempt(start, false);

    let Step::Connect { transaction_id } = attempt.step(start) else {
        panic!("the first send must be a connect");
    };
    attempt.on_datagram(start, &error_response(transaction_id, b"go away"), FAMILY);

    assert_eq!(
        attempt.step(start),
        Step::Done(AttemptOutcome::Rejected(TrackerMessage::from_bytes(
            b"go away"
        )))
    );
}

#[tokio::test(start_paused = true)]
async fn a_connect_response_after_the_announce_is_ignored() {
    let start = Instant::now();
    let mut attempt = attempt(start, false);

    let Step::Connect { transaction_id } = attempt.step(start) else {
        panic!("the first send must be a connect");
    };
    let now = advance(Duration::from_secs(1)).await;
    attempt.on_datagram(
        now,
        &connect_response(transaction_id, CONNECTION_ID),
        FAMILY,
    );
    let Step::Announce {
        transaction_id: announce_transaction,
        ..
    } = attempt.step(now)
    else {
        panic!("the connect response must schedule an announce");
    };

    // A connect response that matches the pending announce transaction is a
    // wrong action for this state and must not replace the connection ID.
    attempt.on_datagram(
        now,
        &connect_response(announce_transaction, OTHER_CONNECTION_ID),
        FAMILY,
    );
    let now = advance(Duration::from_secs(30)).await;
    assert_eq!(
        attempt.step(now),
        Step::Announce {
            transaction_id: announce_transaction,
            connection_id: CONNECTION_ID
        }
    );
}

#[tokio::test(start_paused = true)]
async fn no_announce_is_scheduled_after_the_third_send() {
    let start = Instant::now();
    let mut attempt = attempt(start, false);

    let Step::Connect { transaction_id } = attempt.step(start) else {
        panic!("the first send must be a connect");
    };
    let now = advance(Duration::from_secs(15)).await;
    assert!(matches!(attempt.step(now), Step::Connect { .. }));
    let now = advance(Duration::from_secs(30)).await;
    assert!(matches!(attempt.step(now), Step::Connect { .. }));

    let now = advance(Duration::from_secs(1)).await;
    attempt.on_datagram(
        now,
        &connect_response(transaction_id, CONNECTION_ID),
        FAMILY,
    );
    assert_eq!(
        attempt.step(now),
        Step::Wait {
            until: start + ATTEMPT_DEADLINE
        },
        "all three sends are spent"
    );

    let now = advance(Duration::from_secs(59)).await;
    assert_eq!(attempt.step(now), Step::Done(AttemptOutcome::Timeout));
}

#[tokio::test(start_paused = true)]
async fn a_datagram_before_the_first_send_is_ignored() {
    let start = Instant::now();
    let mut attempt = attempt(start, false);

    attempt.on_datagram(start, &connect_response(1, CONNECTION_ID), FAMILY);
    assert_eq!(
        attempt.step(start),
        Step::Connect {
            transaction_id: 0x1001
        }
    );
}

struct FailingIds;

impl TransactionIds for FailingIds {
    fn next(&mut self) -> Option<u32> {
        None
    }
}

#[tokio::test(start_paused = true)]
async fn a_transaction_id_failure_ends_the_attempt() {
    let start = Instant::now();
    let mut attempt = UdpAttempt::new(start, false, Box::new(FailingIds));

    assert_eq!(
        attempt.step(start),
        Step::Done(AttemptOutcome::Failed(TrackerError::Random))
    );
}

#[test]
fn the_schedule_constants_are_the_contract() {
    assert_eq!(ATTEMPT_DEADLINE, Duration::from_secs(105));
    assert_eq!(STOPPED_DEADLINE, Duration::from_secs(2));
    assert_eq!(MAX_SENDS, 3);
    assert_eq!(RECONNECT_AGE, Duration::from_secs(50));
}
