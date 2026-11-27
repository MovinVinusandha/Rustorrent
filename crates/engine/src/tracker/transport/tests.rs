use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use super::{AnnounceEvent, AnnounceRequest, AnnounceStats, RedirectPolicy, TrackerTransport};
use crate::metainfo::InfoHash;
use crate::tracker::{AnnounceResponse, PeerWarnings, TrackerError, TrackerId, TrackerUrl};

struct FakeTransport;

impl TrackerTransport for FakeTransport {
    fn announce<'a>(
        &'a self,
        _request: AnnounceRequest,
    ) -> Pin<Box<dyn Future<Output = Result<AnnounceResponse, TrackerError>> + Send + 'a>> {
        Box::pin(async {
            Ok(AnnounceResponse {
                interval: Duration::from_secs(1800),
                min_interval: None,
                complete: None,
                incomplete: None,
                peers: Vec::new(),
                tracker_id: None,
                warning: None,
                peer_warnings: PeerWarnings::default(),
            })
        })
    }

    fn max_duration(&self, event: AnnounceEvent) -> Duration {
        if event.is_stopped() {
            Duration::from_secs(2)
        } else {
            Duration::from_secs(105)
        }
    }
}

fn info_hash() -> InfoHash {
    InfoHash::from_bytes([0x11; 20])
}

fn url() -> TrackerUrl {
    TrackerUrl::parse("http://tracker.example:8080/announce/SECRET?passkey=TOPSECRET#fragment")
        .expect("valid tracker URL")
}

fn request(event: AnnounceEvent) -> AnnounceRequest {
    AnnounceRequest::new(
        url(),
        info_hash(),
        *b"-BA0100-123456789012",
        51413,
        AnnounceStats {
            uploaded: 1,
            downloaded: 2,
            left: 3,
        },
        event,
        0x1234_5678,
        Some(TrackerId::new(b"tracker-secret").expect("short tracker id")),
        RedirectPolicy::SameHostOnly,
    )
}

fn assert_send<T: Send>(value: T) -> T {
    value
}

#[test]
fn the_transport_trait_is_object_safe_and_its_future_is_send() {
    let transport: Box<dyn TrackerTransport> = Box::new(FakeTransport);
    let future = transport.announce(request(AnnounceEvent::Started));
    drop(assert_send(future));
}

#[test]
fn numwant_is_derived_from_the_event() {
    assert_eq!(AnnounceEvent::None.num_want(), 50);
    assert_eq!(AnnounceEvent::Started.num_want(), 50);
    assert_eq!(AnnounceEvent::Completed.num_want(), 50);
    assert_eq!(AnnounceEvent::Stopped.num_want(), 0);
}

#[test]
fn the_request_constructor_derives_numwant_from_the_event() {
    assert_eq!(request(AnnounceEvent::Started).num_want, 50);
    assert_eq!(request(AnnounceEvent::Completed).num_want, 50);
    assert_eq!(request(AnnounceEvent::Stopped).num_want, 0);
}

#[test]
fn request_debug_redacts_the_url_path_query_and_tracker_id() {
    let text = format!("{:?}", request(AnnounceEvent::Started));

    assert!(text.contains("http://tracker.example:8080/…"));
    assert!(!text.contains("SECRET"));
    assert!(!text.contains("TOPSECRET"));
    assert!(!text.contains("fragment"));
    assert!(text.contains("TrackerId(14 bytes)"));
}

#[test]
fn the_fake_transport_reports_the_documented_maximums() {
    let transport = FakeTransport;
    assert_eq!(
        transport.max_duration(AnnounceEvent::None),
        Duration::from_secs(105)
    );
    assert_eq!(
        transport.max_duration(AnnounceEvent::Stopped),
        Duration::from_secs(2)
    );
}

#[test]
fn the_new_error_variants_do_not_leak_input() {
    assert_eq!(
        TrackerError::NoUsableTracker.to_string(),
        "no usable tracker"
    );
    assert_eq!(
        TrackerError::InvalidPort.to_string(),
        "invalid tracker port"
    );
    assert_eq!(
        TrackerError::Panicked.to_string(),
        "tracker transport panicked"
    );
}
