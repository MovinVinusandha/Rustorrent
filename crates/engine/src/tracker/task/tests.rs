use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use super::{STOP_BUDGET, TrackerHandle, WATCHDOG_MARGIN};
use crate::metainfo::{FileInfo, InfoHash, MetaInfo, Mode};
use crate::tracker::{
    AnnounceEvent, AnnounceRequest, AnnounceResponse, AnnounceStats, GATE_WAIT, HostGate, HostKey,
    PeerWarnings, Shuffle, Tracker, TrackerCandidates, TrackerConfig, TrackerError, TrackerEvent,
    TrackerId, TrackerTransport, TrackerUrl,
};

struct KeepOrder;

impl Shuffle for KeepOrder {
    fn shuffle(&mut self, _tier: &mut Vec<TrackerUrl>) {}
}

fn candidates(tiers: &[&[&str]], private: bool) -> TrackerCandidates {
    let meta = MetaInfo {
        info_hash: InfoHash::from_bytes([0x22; 20]),
        name: "test".to_owned(),
        mode: Mode::Single,
        files: vec![FileInfo {
            path: vec!["test".to_owned()],
            length: 1,
        }],
        total_length: 1,
        piece_length: 16 * 1024,
        piece_hashes: vec![[0u8; 20]],
        announce: None,
        announce_list: tiers
            .iter()
            .map(|tier| tier.iter().map(|url| (*url).to_owned()).collect())
            .collect(),
        private,
    };
    TrackerCandidates::from_metainfo(&meta, &mut KeepOrder).expect("candidates")
}

fn config() -> TrackerConfig {
    TrackerConfig {
        info_hash: InfoHash::from_bytes([0x22; 20]),
        peer_id: *b"-BT0001-000000000001",
        port: 6881,
        key: 0x1234_5678,
        completed_sent_at_start: false,
    }
}

fn tracker(tiers: &[&[&str]], private: bool, now: Instant) -> Tracker {
    Tracker::new(candidates(tiers, private), config(), now).expect("tracker")
}

fn url(text: &str) -> TrackerUrl {
    TrackerUrl::parse(text).expect("valid tracker URL")
}

fn peer(index: u8) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::new(127, 0, 0, index), 6881))
}

fn response(interval: u64, peers: Vec<SocketAddr>, tracker_id: Option<&[u8]>) -> AnnounceResponse {
    AnnounceResponse {
        interval: Duration::from_secs(interval),
        min_interval: None,
        complete: None,
        incomplete: None,
        peers,
        tracker_id: tracker_id.and_then(TrackerId::new),
        warning: None,
        peer_warnings: PeerWarnings::default(),
    }
}

#[derive(Clone)]
enum Script {
    Success(AnnounceResponse),
    Failure(TrackerError),
    Hang,
    Panic,
}

struct FakeTransport {
    requests: Mutex<Vec<AnnounceRequest>>,
    scripts: Mutex<std::collections::VecDeque<Script>>,
    stopped: Script,
    maximum: Duration,
}

impl FakeTransport {
    fn new(scripts: Vec<Script>, stopped: Script, maximum: Duration) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            scripts: Mutex::new(scripts.into()),
            stopped,
            maximum,
        }
    }

    fn requests(&self) -> Vec<AnnounceRequest> {
        self.requests.lock().expect("requests").clone()
    }

    fn next_script(&self) -> Script {
        self.scripts
            .lock()
            .expect("scripts")
            .pop_front()
            .unwrap_or(Script::Failure(TrackerError::Timeout))
    }
}

impl TrackerTransport for FakeTransport {
    fn announce<'a>(
        &'a self,
        request: AnnounceRequest,
    ) -> Pin<Box<dyn Future<Output = Result<AnnounceResponse, TrackerError>> + Send + 'a>> {
        let script = if request.event == AnnounceEvent::Stopped {
            self.stopped.clone()
        } else {
            self.next_script()
        };
        self.requests.lock().expect("requests").push(request);
        Box::pin(async move {
            match script {
                Script::Success(response) => Ok(response),
                Script::Failure(error) => Err(error),
                Script::Hang => {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    Err(TrackerError::Timeout)
                }
                Script::Panic => panic!("transport panic"),
            }
        })
    }

    fn max_duration(&self, _event: AnnounceEvent) -> Duration {
        self.maximum
    }
}

fn start(tracker: Tracker, transport: FakeTransport) -> (TrackerHandle, Arc<FakeTransport>) {
    let transport = Arc::new(transport);
    let dynamic: Arc<dyn TrackerTransport> = transport.clone();
    let handle = super::spawn(
        tracker,
        dynamic,
        Arc::new(HostGate::new()),
        AnnounceStats::default(),
    );
    (handle, transport)
}

fn start_with_gate(
    tracker: Tracker,
    transport: Arc<FakeTransport>,
    gate: Arc<HostGate>,
) -> TrackerHandle {
    let dynamic: Arc<dyn TrackerTransport> = transport;
    super::spawn(tracker, dynamic, gate, AnnounceStats::default())
}

async fn settle() {
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
}

async fn next(handle: &mut TrackerHandle) -> TrackerEvent {
    tokio::time::timeout(Duration::from_secs(1), handle.next_event())
        .await
        .expect("event within one second")
        .expect("event channel open")
}

async fn next_success(handle: &mut TrackerHandle) -> TrackerEvent {
    loop {
        let event = next(handle).await;
        if matches!(event, TrackerEvent::AnnounceSucceeded { .. }) {
            return event;
        }
    }
}

async fn wait_for_requests(transport: &FakeTransport, count: usize) {
    for _ in 0..16 {
        if transport.requests().len() >= count {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!(
        "expected {count} requests, got {}",
        transport.requests().len()
    );
}

async fn wait_for_stopped(transport: &FakeTransport, count: usize) {
    for _ in 0..16 {
        if transport
            .requests()
            .iter()
            .filter(|request| request.event == AnnounceEvent::Stopped)
            .count()
            >= count
        {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("expected {count} stopped requests");
}

#[tokio::test(start_paused = true)]
async fn started_is_announced_and_the_status_is_published() {
    let now = Instant::now();
    let tracker = tracker(&[&["http://a.example/announce"]], false, now);
    let (mut handle, transport) = start(
        tracker,
        FakeTransport::new(
            vec![Script::Success(response(1800, Vec::new(), None))],
            Script::Success(response(1800, Vec::new(), None)),
            Duration::from_secs(1),
        ),
    );

    let event = next(&mut handle).await;
    assert!(matches!(
        event,
        TrackerEvent::AnnounceSucceeded { interval } if interval == Duration::from_secs(1800)
    ));

    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].event, AnnounceEvent::Started);
    assert_eq!(requests[0].url, url("http://a.example/announce"));

    let status = handle.status();
    assert_eq!(status.active_url, Some(url("http://a.example/announce")));
    assert_eq!(status.last_success, Some(now));
    assert!(!status.terminal);
}

#[tokio::test(start_paused = true)]
async fn periodic_announces_follow_the_interval() {
    let now = Instant::now();
    let tracker = tracker(&[&["http://a.example/announce"]], false, now);
    let (mut handle, transport) = start(
        tracker,
        FakeTransport::new(
            vec![
                Script::Success(response(60, Vec::new(), None)),
                Script::Success(response(60, Vec::new(), None)),
            ],
            Script::Success(response(60, Vec::new(), None)),
            Duration::from_secs(1),
        ),
    );

    assert!(matches!(
        next(&mut handle).await,
        TrackerEvent::AnnounceSucceeded { .. }
    ));

    tokio::time::advance(Duration::from_secs(60)).await;
    assert!(matches!(
        next(&mut handle).await,
        TrackerEvent::AnnounceSucceeded { .. }
    ));

    let requests = transport.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].event, AnnounceEvent::None);
}

#[tokio::test(start_paused = true)]
async fn the_watchdog_reports_a_timeout() {
    let now = Instant::now();
    let tracker = tracker(&[&["http://a.example/announce"]], false, now);
    let (mut handle, transport) = start(
        tracker,
        FakeTransport::new(
            vec![Script::Hang],
            Script::Success(response(60, Vec::new(), None)),
            Duration::from_secs(1),
        ),
    );

    wait_for_requests(&transport, 1).await;
    tokio::time::advance(WATCHDOG_MARGIN + Duration::from_secs(1)).await;

    let event = next(&mut handle).await;
    assert!(matches!(
        event,
        TrackerEvent::AnnounceFailed {
            error: TrackerError::Timeout,
            retry_at: Some(_),
        }
    ));
}

#[tokio::test(start_paused = true)]
async fn a_busy_udp_gate_times_out() {
    let now = Instant::now();
    let tracker = tracker(&[&["udp://a.example:6969"]], false, now);
    let gate = Arc::new(HostGate::new());
    let key = HostKey::new(&url("udp://a.example:6969"));
    let _held = gate.acquire(key).await;
    let transport = Arc::new(FakeTransport::new(
        Vec::new(),
        Script::Success(response(60, Vec::new(), None)),
        Duration::from_secs(1),
    ));
    let mut handle = start_with_gate(tracker, Arc::clone(&transport), Arc::clone(&gate));

    settle().await;
    tokio::time::advance(GATE_WAIT).await;

    let event = next(&mut handle).await;
    assert!(matches!(
        event,
        TrackerEvent::AnnounceFailed {
            error: TrackerError::Timeout,
            ..
        }
    ));
    assert!(transport.requests().is_empty());
}

#[tokio::test(start_paused = true)]
async fn stop_fans_out_to_every_notified_tracker() {
    let now = Instant::now();
    let tracker = tracker(
        &[&["http://a.example/announce", "http://b.example/announce"]],
        false,
        now,
    );
    let (mut handle, transport) = start(
        tracker,
        FakeTransport::new(
            vec![
                Script::Success(response(60, Vec::new(), Some(b"aaa"))),
                Script::Success(response(60, Vec::new(), Some(b"aaa"))),
                Script::Success(response(60, Vec::new(), Some(b"bbb"))),
            ],
            Script::Success(response(60, Vec::new(), None)),
            Duration::from_secs(1),
        ),
    );

    assert!(matches!(
        next(&mut handle).await,
        TrackerEvent::AnnounceSucceeded { .. }
    ));
    tokio::time::advance(Duration::from_secs(60)).await;
    assert!(matches!(
        next_success(&mut handle).await,
        TrackerEvent::AnnounceSucceeded { .. }
    ));
    assert!(matches!(
        next_success(&mut handle).await,
        TrackerEvent::AnnounceSucceeded { .. }
    ));

    assert!(handle.stop());
    handle.join().await.expect("tracker task");

    let requests = transport.requests();
    let stopped: Vec<_> = requests
        .iter()
        .filter(|request| request.event == AnnounceEvent::Stopped)
        .collect();
    assert_eq!(stopped.len(), 2);
    assert!(
        stopped
            .iter()
            .any(|request| request.url == url("http://a.example/announce"))
    );
    assert!(
        stopped
            .iter()
            .any(|request| request.url == url("http://b.example/announce"))
    );
    assert!(handle.status().terminal);
}

#[tokio::test(start_paused = true)]
async fn stop_uses_each_trackers_own_id() {
    let now = Instant::now();
    let tracker = tracker(
        &[
            &["http://a.example/announce"],
            &["http://b.example/announce"],
        ],
        true,
        now,
    );
    let b_id = TrackerId::new(b"bbb").expect("id");
    let (mut handle, transport) = start(
        tracker,
        FakeTransport::new(
            vec![
                Script::Success(response(60, Vec::new(), Some(b"aaa"))),
                Script::Failure(TrackerError::Timeout),
                Script::Success(response(60, Vec::new(), Some(b"bbb"))),
            ],
            Script::Success(response(60, Vec::new(), None)),
            Duration::from_secs(1),
        ),
    );

    assert!(matches!(
        next(&mut handle).await,
        TrackerEvent::AnnounceSucceeded { .. }
    ));
    tokio::time::advance(Duration::from_secs(60)).await;
    assert!(matches!(
        next_success(&mut handle).await,
        TrackerEvent::AnnounceSucceeded { .. }
    ));

    assert!(handle.stop());
    handle.join().await.expect("tracker task");

    let requests = transport.requests();
    let stopped: Vec<_> = requests
        .iter()
        .filter(|request| request.event == AnnounceEvent::Stopped)
        .collect();
    assert_eq!(stopped.len(), 2);
    let stopped_a = stopped
        .iter()
        .find(|request| request.url == url("http://a.example/announce"))
        .expect("a stopped request");
    let stopped_b = stopped
        .iter()
        .find(|request| request.url == url("http://b.example/announce"))
        .expect("b stopped request");
    // Switching away from A cleared A's tracker ID; B is active and keeps its
    // own ID. The fan-out passes each slot's ID, not the active one for all.
    assert!(stopped_a.tracker_id.is_none());
    assert_eq!(stopped_b.tracker_id.as_ref(), Some(&b_id));
    assert!(handle.status().terminal);
}

#[tokio::test(start_paused = true)]
async fn the_stop_fan_out_is_bounded_by_three_seconds() {
    let now = Instant::now();
    let tracker = tracker(
        &[&["http://a.example/announce", "http://b.example/announce"]],
        false,
        now,
    );
    let (mut handle, transport) = start(
        tracker,
        FakeTransport::new(
            vec![
                Script::Success(response(60, Vec::new(), None)),
                Script::Success(response(60, Vec::new(), None)),
                Script::Success(response(60, Vec::new(), None)),
            ],
            Script::Hang,
            Duration::from_secs(1),
        ),
    );

    assert!(matches!(
        next(&mut handle).await,
        TrackerEvent::AnnounceSucceeded { .. }
    ));
    tokio::time::advance(Duration::from_secs(60)).await;
    assert!(matches!(
        next_success(&mut handle).await,
        TrackerEvent::AnnounceSucceeded { .. }
    ));
    assert!(matches!(
        next_success(&mut handle).await,
        TrackerEvent::AnnounceSucceeded { .. }
    ));

    assert!(handle.stop());
    wait_for_stopped(&transport, 2).await;
    tokio::time::advance(STOP_BUDGET).await;

    let mut timeouts = 0;
    for _ in 0..8 {
        if let TrackerEvent::AnnounceFailed {
            error: TrackerError::Timeout,
            retry_at: None,
        } = next(&mut handle).await
        {
            timeouts += 1;
            if timeouts == 2 {
                break;
            }
        }
    }
    assert_eq!(timeouts, 2);

    handle.join().await.expect("tracker task");
    assert!(handle.status().terminal);
}

#[cfg(panic = "unwind")]
#[tokio::test(start_paused = true)]
async fn a_transport_panic_is_reported_as_a_failure() {
    let now = Instant::now();
    let tracker = tracker(&[&["http://a.example/announce"]], false, now);
    let (mut handle, _transport) = start(
        tracker,
        FakeTransport::new(
            vec![Script::Panic],
            Script::Success(response(60, Vec::new(), None)),
            Duration::from_secs(1),
        ),
    );

    let event = next(&mut handle).await;
    assert!(matches!(
        event,
        TrackerEvent::AnnounceFailed {
            error: TrackerError::Panicked,
            retry_at: Some(_),
        }
    ));
}

#[tokio::test(start_paused = true)]
async fn a_full_event_channel_still_delivers_a_switch() {
    let now = Instant::now();
    let tracker = tracker(
        &[
            &["http://a.example/announce"],
            &["http://b.example/announce"],
        ],
        true,
        now,
    );
    let mut scripts = Vec::new();
    for _ in 0..16 {
        scripts.push(Script::Success(response(60, vec![peer(1)], None)));
    }
    scripts.push(Script::Failure(TrackerError::Timeout));
    scripts.push(Script::Success(response(60, vec![peer(2)], None)));
    let (mut handle, _transport) = start(
        tracker,
        FakeTransport::new(
            scripts,
            Script::Success(response(60, Vec::new(), None)),
            Duration::from_secs(1),
        ),
    );

    assert!(matches!(
        next(&mut handle).await,
        TrackerEvent::AnnounceSucceeded { .. }
    ));

    for _ in 0..15 {
        tokio::time::advance(Duration::from_secs(60)).await;
        settle().await;
    }
    assert!(
        handle.events.capacity() <= 1,
        "the event channel is full before the switch"
    );

    tokio::time::advance(Duration::from_secs(60)).await;
    settle().await;

    let mut saw_switch = false;
    for _ in 0..64 {
        match tokio::time::timeout(Duration::from_secs(1), handle.next_event()).await {
            Ok(Some(TrackerEvent::TrackerSwitched { drop_peers })) => {
                assert!(drop_peers);
                saw_switch = true;
                break;
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    assert!(saw_switch, "the switch was not delivered");
}
