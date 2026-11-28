use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

use crate::metainfo::InfoHash;
use crate::tracker::udp::codec::{ConnectionId, Family, PROTOCOL_ID};
use crate::tracker::{
    AnnounceEvent, AnnounceRequest, AnnounceResponse, AnnounceStats, RedirectPolicy,
    SystemResolver, TargetPolicy, TrackerError, TrackerTransport, TrackerUrl,
};

use super::{UdpTransport, bind_address, map_recv_error, map_send_error};

struct FakeTracker {
    socket: UdpSocket,
    address: SocketAddr,
}

impl FakeTracker {
    async fn bind() -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind fake tracker");
        let address = socket.local_addr().expect("fake tracker address");
        Self { socket, address }
    }

    async fn recv(&self, buffer: &mut [u8]) -> (Vec<u8>, SocketAddr) {
        let (length, source) = self
            .socket
            .recv_from(buffer)
            .await
            .expect("receive datagram");
        (buffer[..length].to_vec(), source)
    }

    fn try_recv(&self, buffer: &mut [u8]) -> Option<(usize, SocketAddr)> {
        self.socket.try_recv_from(buffer).ok()
    }

    async fn send(&self, packet: &[u8], target: SocketAddr) {
        self.socket
            .send_to(packet, target)
            .await
            .expect("send datagram");
    }
}

fn transport() -> UdpTransport {
    UdpTransport::new(Arc::new(SystemResolver::new()), TargetPolicy::new(true))
}

fn url(address: SocketAddr) -> TrackerUrl {
    TrackerUrl::parse(&format!("udp://{address}/announce")).expect("valid tracker URL")
}

fn request_with(url: TrackerUrl, event: AnnounceEvent) -> AnnounceRequest {
    AnnounceRequest::new(
        url,
        InfoHash::from_bytes([0x11; 20]),
        *b"-BA0100-123456789012",
        51413,
        AnnounceStats::default(),
        event,
        0x1234_5678,
        None,
        RedirectPolicy::Public,
    )
}

fn request(url: TrackerUrl) -> AnnounceRequest {
    request_with(url, AnnounceEvent::Started)
}

fn spawn(request: AnnounceRequest) -> JoinHandle<Result<AnnounceResponse, TrackerError>> {
    let transport = transport();
    tokio::spawn(async move { transport.announce(request).await })
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
    packet.extend_from_slice(&2u32.to_be_bytes());
    packet.extend_from_slice(&3u32.to_be_bytes());
    packet
}

fn error_response(transaction_id: u32, text: &[u8]) -> Vec<u8> {
    let mut packet = Vec::new();
    packet.extend_from_slice(&3u32.to_be_bytes());
    packet.extend_from_slice(&transaction_id.to_be_bytes());
    packet.extend_from_slice(text);
    packet
}

fn transaction_id(packet: &[u8]) -> u32 {
    u32::from_be_bytes(
        packet
            .get(12..16)
            .expect("transaction ID field")
            .try_into()
            .expect("four bytes"),
    )
}

#[tokio::test]
async fn an_announce_round_trip() {
    let server = FakeTracker::bind().await;
    let mut buffer = [0u8; 2048];
    let handle = spawn(request(url(server.address)));

    let (connect, source) = server.recv(&mut buffer).await;
    assert_eq!(connect.len(), 16);
    assert_eq!(&connect[..8], &PROTOCOL_ID.to_be_bytes());
    let connect_transaction = transaction_id(&connect);

    server
        .send(
            &connect_response(connect_transaction, ConnectionId::new(7)),
            source,
        )
        .await;

    let (announce, _) = server.recv(&mut buffer).await;
    assert_eq!(announce.len(), 98);
    assert_eq!(&announce[..8], &7u64.to_be_bytes());
    assert_eq!(&announce[96..98], &51413u16.to_be_bytes());
    let announce_transaction = transaction_id(&announce);
    assert_ne!(announce_transaction, connect_transaction);

    server
        .send(&announce_response(announce_transaction), source)
        .await;

    let response = handle
        .await
        .expect("announce task")
        .expect("announce response");
    assert_eq!(response.interval, Duration::from_secs(1800));
    assert_eq!(response.incomplete, Some(2));
    assert_eq!(response.complete, Some(3));
    assert!(response.peers.is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_attempt_sends_three_connects_then_times_out() {
    let server = FakeTracker::bind().await;
    let mut buffer = [0u8; 2048];
    let handle = spawn(request(url(server.address)));

    let (first, _) = server.recv(&mut buffer).await;

    tokio::time::advance(Duration::from_secs(15)).await;
    let (second, _) = server.recv(&mut buffer).await;
    assert_eq!(second, first);

    tokio::time::advance(Duration::from_secs(30)).await;
    let (third, _) = server.recv(&mut buffer).await;
    assert_eq!(third, first);

    tokio::time::advance(Duration::from_secs(60)).await;
    assert_eq!(
        handle.await.expect("announce task"),
        Err(TrackerError::Timeout)
    );
}

#[tokio::test(start_paused = true)]
async fn a_stopped_announce_uses_the_short_deadline_and_does_not_retry() {
    let server = FakeTracker::bind().await;
    let mut buffer = [0u8; 2048];
    let handle = spawn(request_with(url(server.address), AnnounceEvent::Stopped));

    let (connect, _) = server.recv(&mut buffer).await;
    assert_eq!(connect.len(), 16);

    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert!(
        server.try_recv(&mut buffer).is_none(),
        "a stopped announce must not retransmit"
    );

    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(
        handle.await.expect("announce task"),
        Err(TrackerError::Timeout)
    );
}

#[tokio::test(start_paused = true)]
async fn a_wrong_transaction_id_is_ignored_and_the_connect_is_retransmitted() {
    let server = FakeTracker::bind().await;
    let mut buffer = [0u8; 2048];
    let handle = spawn(request(url(server.address)));

    let (connect, source) = server.recv(&mut buffer).await;
    let connect_transaction = transaction_id(&connect);

    server
        .send(
            &connect_response(connect_transaction ^ 1, ConnectionId::new(9)),
            source,
        )
        .await;
    tokio::task::yield_now().await;
    assert!(
        server.try_recv(&mut buffer).is_none(),
        "a wrong transaction ID must not trigger an announce"
    );

    tokio::time::advance(Duration::from_secs(15)).await;
    let (retry, _) = server.recv(&mut buffer).await;
    assert_eq!(retry, connect, "the retransmission reuses the ID");

    server
        .send(
            &connect_response(connect_transaction, ConnectionId::new(9)),
            source,
        )
        .await;
    let (announce, _) = server.recv(&mut buffer).await;
    assert_eq!(announce.len(), 98);
    assert_eq!(&announce[..8], &9u64.to_be_bytes());

    handle.abort();
}

#[tokio::test]
async fn a_datagram_from_another_source_is_dropped() {
    let server = FakeTracker::bind().await;
    let mut buffer = [0u8; 2048];
    let handle = spawn(request(url(server.address)));

    let (connect, source) = server.recv(&mut buffer).await;
    let connect_transaction = transaction_id(&connect);

    let forger = UdpSocket::bind("127.0.0.1:0").await.expect("bind forger");
    let forged = connect_response(connect_transaction, ConnectionId::new(99));
    forger
        .send_to(&forged, source)
        .await
        .expect("send forged datagram");

    tokio::task::yield_now().await;
    assert!(
        server.try_recv(&mut buffer).is_none(),
        "the connected socket drops datagrams from another source"
    );

    server
        .send(
            &connect_response(connect_transaction, ConnectionId::new(7)),
            source,
        )
        .await;
    let (announce, _) = server.recv(&mut buffer).await;
    assert_eq!(&announce[..8], &7u64.to_be_bytes());

    handle.abort();
}

#[tokio::test]
async fn an_error_packet_ends_the_attempt() {
    let server = FakeTracker::bind().await;
    let mut buffer = [0u8; 2048];
    let tracker_url = url(server.address);
    let handle = spawn(request(tracker_url.clone()));

    let (connect, source) = server.recv(&mut buffer).await;
    let connect_transaction = transaction_id(&connect);

    server
        .send(&error_response(connect_transaction, b"go away"), source)
        .await;

    match handle.await.expect("announce task") {
        Err(TrackerError::Rejected { message }) => {
            assert_eq!(message.sanitized_text(&tracker_url), "go away");
            assert!(!format!("{message:?}").contains("go away"));
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
}

#[tokio::test]
async fn a_non_udp_url_is_rejected() {
    let transport = transport();

    for text in [
        "http://127.0.0.1:6969/announce",
        "https://127.0.0.1:6969/announce",
    ] {
        let url = TrackerUrl::parse(text).expect("valid tracker URL");
        assert_eq!(
            transport.announce(request(url)).await,
            Err(TrackerError::UnsupportedScheme)
        );
    }
}

#[tokio::test(start_paused = true)]
async fn dropping_the_future_closes_the_socket() {
    let server = FakeTracker::bind().await;
    let mut buffer = [0u8; 2048];
    let transport = transport();

    let mut future = Box::pin(transport.announce(request(url(server.address))));
    let mut receive = Box::pin(server.recv(&mut buffer));
    tokio::select! {
        _ = &mut receive => {}
        result = &mut future => panic!("attempt ended before the connect was sent: {result:?}"),
    }
    drop(receive);

    drop(future);
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    assert!(
        server.try_recv(&mut buffer).is_none(),
        "a dropped attempt must not send again"
    );
}

#[test]
fn max_duration_covers_dns_and_the_attempt_deadline() {
    let transport = transport();

    for event in [
        AnnounceEvent::None,
        AnnounceEvent::Started,
        AnnounceEvent::Completed,
    ] {
        assert_eq!(
            transport.max_duration(event),
            Duration::from_secs(112),
            "{event:?} must include the bounded DNS phase"
        );
    }
    assert_eq!(
        transport.max_duration(AnnounceEvent::Stopped),
        Duration::from_secs(9)
    );
}

#[test]
fn the_bind_address_matches_the_family() {
    assert_eq!(bind_address(Family::V4).to_string(), "0.0.0.0:0");
    assert_eq!(bind_address(Family::V6).to_string(), "[::]:0");
}

#[test]
fn socket_errors_map_to_typed_errors() {
    use std::io::{Error, ErrorKind};

    assert_eq!(
        map_send_error(Error::from(ErrorKind::ConnectionRefused)),
        TrackerError::Connect
    );
    assert_eq!(
        map_send_error(Error::from(ErrorKind::ConnectionReset)),
        TrackerError::Connect
    );
    assert_eq!(
        map_recv_error(Error::from_raw_os_error(10040)),
        TrackerError::TooLarge
    );
    assert_eq!(
        map_recv_error(Error::from(ErrorKind::WouldBlock)),
        TrackerError::Io {
            kind: ErrorKind::WouldBlock
        }
    );
}

#[test]
fn the_announce_future_is_send() {
    fn assert_send<T: Send>(value: T) -> T {
        value
    }

    let transport = transport();
    let url = TrackerUrl::parse("udp://127.0.0.1:1/announce").expect("valid tracker URL");
    drop(assert_send(transport.announce(request(url))));
}
