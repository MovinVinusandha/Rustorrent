//! BEP 15 UDP transport.
//!
//! One connected UDP socket per announce attempt, bound to the address family
//! of the checked tracker address. The socket connects to the exact `IpAddr`
//! returned by the resolver; the host is never resolved twice. The kernel
//! drops datagrams from any other source.
//!
//! Dropping the announce future drops the socket and stops every send: there
//! is no background task. An ICMP port-unreachable arrives as
//! `ConnectionRefused` on Unix and as `ConnectionReset` on Windows; either one
//! ends the attempt immediately with [`TrackerError::Connect`], because
//! waiting for the deadline would only delay failover to the next tracker.
//!
//! On Windows an oversized datagram can fail `recv` with `WSAEMSGSIZE`
//! instead of being truncated; that is mapped to [`TrackerError::TooLarge`].

#![allow(dead_code)] // The session wires the transport in item 10; tests drive it today.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use tokio::net::UdpSocket;
use tokio::time::{Instant, sleep_until};

use super::attempt::{AttemptOutcome, Step, TransactionIds, UdpAttempt};
use super::codec::{
    Family, MAX_DATAGRAM_LEN, UdpAnnounce, UdpAnnounceResponse, encode_announce, encode_connect,
};
use crate::tracker::{Resolver, Scheme, TargetPolicy, TrackerError, TrackerUrl};

/// Windows `WSAEMSGSIZE`: the datagram was larger than the receive buffer.
const WSAEMSGSIZE: i32 = 10040;

/// Sends UDP announces for one tracker URL.
pub(crate) struct UdpTransport {
    resolver: Arc<dyn Resolver>,
    policy: TargetPolicy,
}

impl UdpTransport {
    /// Builds a transport with the given resolver and target policy.
    pub(crate) fn new(resolver: Arc<dyn Resolver>, policy: TargetPolicy) -> Self {
        Self { resolver, policy }
    }

    /// Runs one announce attempt and returns the decoded response.
    ///
    /// A `stopped` announce uses the short deadline and is not retried. The
    /// returned future is the whole attempt: dropping it closes the socket.
    pub(crate) async fn announce(
        &self,
        url: &TrackerUrl,
        request: &UdpAnnounce,
        stopped: bool,
    ) -> Result<UdpAnnounceResponse, TrackerError> {
        if url.scheme() != Scheme::Udp {
            return Err(TrackerError::UnsupportedScheme);
        }

        let address = self
            .resolver
            .resolve(url.host(), url.port(), self.policy)
            .await?;
        let family = match address {
            SocketAddr::V4(_) => Family::V4,
            SocketAddr::V6(_) => Family::V6,
        };

        let socket = UdpSocket::bind(bind_address(family))
            .await
            .map_err(map_send_error)?;
        socket.connect(address).await.map_err(map_send_error)?;

        let mut attempt = UdpAttempt::new(Instant::now(), stopped, Box::new(OsTransactionIds));
        let mut buffer = [0u8; MAX_DATAGRAM_LEN];

        loop {
            match attempt.step(Instant::now()) {
                Step::Connect { transaction_id } => {
                    let packet = encode_connect(transaction_id);
                    socket.send(&packet).await.map_err(map_send_error)?;
                }
                Step::Announce {
                    transaction_id,
                    connection_id,
                } => {
                    let mut request = request.clone();
                    request.connection_id = connection_id;
                    let packet = encode_announce(&request, transaction_id);
                    socket.send(&packet).await.map_err(map_send_error)?;
                }
                Step::Wait { until } => {
                    tokio::select! {
                        result = socket.recv(&mut buffer) => {
                            let length = result.map_err(map_recv_error)?;
                            let datagram = buffer.get(..length).unwrap_or_default();
                            attempt.on_datagram(Instant::now(), datagram, family);
                        }
                        () = sleep_until(until) => {}
                    }
                }
                Step::Done(outcome) => {
                    return match outcome {
                        AttemptOutcome::Announce(response) => Ok(response),
                        AttemptOutcome::Rejected(message) => {
                            Err(TrackerError::Rejected { message })
                        }
                        AttemptOutcome::Timeout => Err(TrackerError::Timeout),
                        AttemptOutcome::Failed(error) => Err(error),
                    };
                }
            }
        }
    }
}

fn bind_address(family: Family) -> SocketAddr {
    match family {
        Family::V4 => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        Family::V6 => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    }
}

/// Transaction IDs from the OS randomness source.
struct OsTransactionIds;

impl TransactionIds for OsTransactionIds {
    fn next(&mut self) -> Option<u32> {
        let mut bytes = [0u8; 4];
        getrandom::fill(&mut bytes).ok()?;
        Some(u32::from_be_bytes(bytes))
    }
}

fn map_send_error(error: io::Error) -> TrackerError {
    match error.kind() {
        io::ErrorKind::ConnectionRefused | io::ErrorKind::ConnectionReset => TrackerError::Connect,
        kind => TrackerError::Io { kind },
    }
}

fn map_recv_error(error: io::Error) -> TrackerError {
    if error.raw_os_error() == Some(WSAEMSGSIZE) {
        return TrackerError::TooLarge;
    }
    map_send_error(error)
}

#[cfg(test)]
mod tests;
