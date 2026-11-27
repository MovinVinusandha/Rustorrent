//! BEP 15 attempt schedule.
//!
//! The attempt is a pure state machine: it owns no socket, no task and no
//! clock. The caller passes `tokio::time::Instant` and feeds received
//! datagrams, so every deadline and retransmission is deterministic under a
//! paused clock. `std::time::Instant` is not used here.
//!
//! Schedule: sends are due at 0, 15 and 45 seconds, and the attempt fails at
//! the absolute 105-second deadline. A `stopped` announce uses a 2-second
//! deadline and no retransmission. Every datagram, connect or announce, counts
//! as one of the three sends. A retransmission reuses the transaction ID of
//! the request it repeats; a new request (the first connect, a connect after
//! the 50-second guard, or an announce after a connect response) gets a fresh
//! ID.
//!
//! The 50-second guard is defensive: at the nominal 0/15/45 schedule a
//! connection ID is at most 45 seconds old at a send. It protects a delayed
//! send, such as a stalled task or a suspended host.

use std::time::Duration;

use tokio::time::Instant;

use super::codec::{ConnectionId, Decoded, Family, UdpAnnounceResponse, decode};
use crate::tracker::{TrackerError, TrackerMessage};

/// Deadline of a normal attempt, measured from the first datagram.
pub const ATTEMPT_DEADLINE: Duration = Duration::from_secs(105);

/// Deadline of a `stopped` announce.
pub const STOPPED_DEADLINE: Duration = Duration::from_secs(2);

/// Maximum number of datagrams sent in one attempt.
pub const MAX_SENDS: u8 = 3;

/// Age at which a connection ID is replaced before a send.
pub const RECONNECT_AGE: Duration = Duration::from_secs(50);

const FIRST_WAIT: Duration = Duration::from_secs(15);
const SECOND_WAIT: Duration = Duration::from_secs(30);

/// Source of transaction IDs.
pub trait TransactionIds: Send {
    /// Returns a fresh 32-bit transaction ID, or `None` when the OS source
    /// failed.
    fn next(&mut self) -> Option<u32>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    Connect { transaction_id: u32 },
    Announce { transaction_id: u32 },
}

impl Pending {
    fn transaction_id(self) -> u32 {
        match self {
            Self::Connect { transaction_id } | Self::Announce { transaction_id } => transaction_id,
        }
    }
}

/// The next thing the transport must do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Send a connect request now.
    Connect {
        /// Transaction ID to put in the request.
        transaction_id: u32,
    },
    /// Send an announce request now.
    Announce {
        /// Transaction ID to put in the request.
        transaction_id: u32,
        /// Connection ID from the most recent connect response.
        connection_id: ConnectionId,
    },
    /// Wait until this instant, then call [`UdpAttempt::step`] again.
    Wait {
        /// Absolute time of the next send or the deadline.
        until: Instant,
    },
    /// The attempt is over.
    Done(AttemptOutcome),
}

/// Final result of one attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// The tracker answered the announce.
    Announce(UdpAnnounceResponse),
    /// The tracker sent an error packet.
    Rejected(TrackerMessage),
    /// The deadline passed with no usable response.
    Timeout,
    /// The attempt could not continue, for example because the OS
    /// randomness source failed.
    Failed(TrackerError),
}

/// One UDP announce attempt.
pub struct UdpAttempt {
    deadline: Instant,
    next_send: Option<Instant>,
    sends: u8,
    pending: Option<Pending>,
    connection_id: Option<ConnectionId>,
    id_received: Option<Instant>,
    outcome: Option<AttemptOutcome>,
    ids: Box<dyn TransactionIds>,
}

impl UdpAttempt {
    /// Starts an attempt at `now`.
    pub fn new(now: Instant, stopped: bool, ids: Box<dyn TransactionIds>) -> Self {
        let deadline = now
            + if stopped {
                STOPPED_DEADLINE
            } else {
                ATTEMPT_DEADLINE
            };

        Self {
            deadline,
            next_send: Some(now),
            sends: 0,
            pending: None,
            connection_id: None,
            id_received: None,
            outcome: None,
            ids,
        }
    }

    /// Returns the absolute deadline of this attempt.
    #[cfg(test)]
    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Advances the state machine to `now`.
    pub fn step(&mut self, now: Instant) -> Step {
        if let Some(outcome) = self.outcome.take() {
            return Step::Done(outcome);
        }
        if now >= self.deadline {
            return Step::Done(AttemptOutcome::Timeout);
        }

        match self.next_send {
            Some(at) if now >= at => self.send(now),
            Some(at) => Step::Wait {
                until: at.min(self.deadline),
            },
            None => Step::Wait {
                until: self.deadline,
            },
        }
    }

    /// Feeds one received datagram to the attempt.
    ///
    /// The datagram is decoded against the pending transaction ID, so a wrong
    /// action or transaction ID is ignored and changes nothing: the deadline
    /// and the next send stay where they were.
    pub fn on_datagram(&mut self, now: Instant, datagram: &[u8], family: Family) {
        let Some(pending) = self.pending else {
            return;
        };
        let Some(decoded) = decode(datagram, pending.transaction_id(), family) else {
            return;
        };

        match decoded {
            Decoded::Connect(connection_id) => {
                if matches!(pending, Pending::Connect { .. }) && self.sends < MAX_SENDS {
                    let Some(transaction_id) = self.ids.next() else {
                        self.outcome = Some(AttemptOutcome::Failed(TrackerError::Random));
                        return;
                    };
                    self.connection_id = Some(connection_id);
                    self.id_received = Some(now);
                    self.pending = Some(Pending::Announce { transaction_id });
                    self.next_send = Some(now);
                }
            }
            Decoded::Announce(response) => {
                if matches!(pending, Pending::Announce { .. }) {
                    self.outcome = Some(AttemptOutcome::Announce(response));
                }
            }
            Decoded::Error(message) => {
                self.outcome = Some(AttemptOutcome::Rejected(message));
            }
        }
    }

    fn send(&mut self, now: Instant) -> Step {
        if self.sends >= MAX_SENDS {
            self.next_send = None;
            return Step::Wait {
                until: self.deadline,
            };
        }

        self.sends += 1;
        self.next_send = match self.sends {
            1 => Some(now + FIRST_WAIT),
            2 => Some(now + SECOND_WAIT),
            _ => None,
        };

        // A pending connect is retransmitted with the same transaction ID.
        if let Some(Pending::Connect { transaction_id }) = self.pending {
            return Step::Connect { transaction_id };
        }

        // A pending announce is retransmitted with the same transaction ID
        // unless the connection ID is old enough to need a new connect.
        if let Some(Pending::Announce { transaction_id }) = self.pending
            && let Some(connection_id) = self.connection_id
            && !self.reconnect_due(now)
        {
            return Step::Announce {
                transaction_id,
                connection_id,
            };
        }

        let Some(transaction_id) = self.ids.next() else {
            return Step::Done(AttemptOutcome::Failed(TrackerError::Random));
        };
        self.pending = Some(Pending::Connect { transaction_id });
        Step::Connect { transaction_id }
    }

    fn reconnect_due(&self, now: Instant) -> bool {
        self.id_received
            .and_then(|received| now.checked_duration_since(received))
            .is_none_or(|age| age >= RECONNECT_AGE)
    }

    /// Builds an attempt that already holds a connection ID.
    #[cfg(test)]
    pub(crate) fn with_connection(
        now: Instant,
        received_at: Instant,
        connection_id: ConnectionId,
        ids: Box<dyn TransactionIds>,
    ) -> Self {
        let mut attempt = Self::new(now, false, ids);
        attempt.connection_id = Some(connection_id);
        attempt.id_received = Some(received_at);
        attempt.pending = Some(Pending::Announce {
            transaction_id: attempt.ids.next().expect("test transaction source"),
        });
        attempt
    }
}

#[cfg(test)]
mod tests;
