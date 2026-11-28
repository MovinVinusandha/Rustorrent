use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use tokio::time::Instant;

use crate::metainfo::{FileInfo, InfoHash, MetaInfo, Mode};
use crate::tracker::{
    AnnounceEvent, AnnounceResponse, AnnounceStats, PeerWarnings, RedirectPolicy, Shuffle,
    TrackerCandidates, TrackerError, TrackerId, TrackerMessage, TrackerUrl,
};

use super::{NotifiedTracker, Tracker, TrackerConfig, TrackerEvent};

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

fn stats() -> AnnounceStats {
    AnnounceStats::default()
}

fn response(interval: u64) -> AnnounceResponse {
    response_with(interval, None)
}

fn response_with(interval: u64, min_interval: Option<u64>) -> AnnounceResponse {
    AnnounceResponse {
        interval: Duration::from_secs(interval),
        min_interval: min_interval.map(Duration::from_secs),
        complete: None,
        incomplete: None,
        peers: Vec::new(),
        tracker_id: None,
        warning: None,
        peer_warnings: PeerWarnings::default(),
    }
}

fn url(text: &str) -> TrackerUrl {
    TrackerUrl::parse(text).expect("valid tracker URL")
}

fn failure_retry(events: &[TrackerEvent]) -> Instant {
    events
        .iter()
        .find_map(|event| match event {
            TrackerEvent::AnnounceFailed { retry_at, .. } => *retry_at,
            _ => None,
        })
        .expect("failure event with a retry")
}

fn success_interval(events: &[TrackerEvent]) -> Duration {
    events
        .iter()
        .find_map(|event| match event {
            TrackerEvent::AnnounceSucceeded { interval } => Some(*interval),
            _ => None,
        })
        .expect("success event")
}

fn peer(index: u8) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::new(127, 0, 0, index), 6881))
}

fn peer_batches(events: &[TrackerEvent]) -> Vec<Vec<SocketAddr>> {
    events
        .iter()
        .filter_map(|event| match event {
            TrackerEvent::PeersReceived(peers) => Some(peers.clone()),
            _ => None,
        })
        .collect()
}

fn has_switch(events: &[TrackerEvent], drop_peers: bool) -> bool {
    events.iter().any(|event| {
        matches!(
            event,
            TrackerEvent::TrackerSwitched {
                drop_peers: value
            } if *value == drop_peers
        )
    })
}

#[tokio::test(start_paused = true)]
async fn started_is_due_immediately_and_only_one_request_is_in_flight() {
    let now = Instant::now();
    let mut tracker = tracker(&[&["http://a.example/announce"]], false, now);

    assert_eq!(tracker.next_deadline(), Some(now));

    let request = tracker
        .build_request(now, stats())
        .expect("started request");
    assert_eq!(request.event, AnnounceEvent::Started);
    assert_eq!(request.num_want, 50);
    assert_eq!(request.key, config().key);
    assert!(request.tracker_id.is_none());
    assert_eq!(request.redirect, RedirectPolicy::Public);

    assert_eq!(tracker.next_deadline(), None);
    assert!(tracker.build_request(now, stats()).is_none());
}

#[tokio::test(start_paused = true)]
async fn a_successful_announce_schedules_the_interval() {
    let now = Instant::now();
    let mut tracker = tracker(&[&["http://a.example/announce"]], false, now);

    tracker.build_request(now, stats()).expect("started");
    let events = tracker.handle_response(now, response(1800));

    assert_eq!(success_interval(&events), Duration::from_secs(1800));
    assert_eq!(
        tracker.next_deadline(),
        Some(now + Duration::from_secs(1800))
    );

    let status = tracker.status();
    assert_eq!(status.active_url, Some(url("http://a.example/announce")));
    assert_eq!(status.last_success, Some(now));
    assert_eq!(status.next_announce, Some(now + Duration::from_secs(1800)));
    assert_eq!(status.last_interval, Some(Duration::from_secs(1800)));
    assert_eq!(status.consecutive_failures, 0);
    assert!(!status.terminal);
}

#[tokio::test(start_paused = true)]
async fn the_interval_is_clamped_and_min_interval_can_only_raise_it() {
    let now = Instant::now();
    let mut tracker = tracker(&[&["http://a.example/announce"]], false, now);

    tracker.build_request(now, stats()).expect("started");
    let events = tracker.handle_response(now, response(5));
    assert_eq!(success_interval(&events), Duration::from_secs(60));

    let at = now + Duration::from_secs(60);
    tracker.build_request(at, stats()).expect("periodic");
    let events = tracker.handle_response(at, response_with(60, Some(3600)));
    assert_eq!(success_interval(&events), Duration::from_secs(3600));

    let at = at + Duration::from_secs(3600);
    tracker.build_request(at, stats()).expect("periodic");
    let events = tracker.handle_response(at, response(0));
    assert_eq!(success_interval(&events), Duration::from_secs(1800));
}

#[tokio::test(start_paused = true)]
async fn a_failed_started_is_retried_with_backoff() {
    let now = Instant::now();
    let mut tracker = tracker(&[&["http://a.example/announce"]], false, now);

    tracker.build_request(now, stats()).expect("started");
    let events = tracker.handle_failure(now, TrackerError::Timeout);
    let retry_at = failure_retry(&events);
    let delay = retry_at - now;

    assert_eq!(tracker.next_deadline(), Some(retry_at));
    assert!(tracker.status().consecutive_failures == 1);
    assert!(
        (Duration::from_secs(24)..=Duration::from_secs(36)).contains(&delay),
        "backoff {delay:?} outside the ±20% jitter band"
    );
    assert!(tracker.build_request(now, stats()).is_none());

    let request = tracker
        .build_request(retry_at, stats())
        .expect("started retry");
    assert_eq!(request.event, AnnounceEvent::Started);
}

#[tokio::test(start_paused = true)]
async fn completed_is_retried_after_a_failure_and_sent_once() {
    let now = Instant::now();
    let mut tracker = tracker(&[&["http://a.example/announce"]], false, now);

    tracker.build_request(now, stats()).expect("started");
    tracker.handle_response(now, response(1800));

    let at = now + Duration::from_secs(1800);
    tracker.complete(at);
    assert_eq!(tracker.next_deadline(), Some(at));

    let request = tracker.build_request(at, stats()).expect("completed");
    assert_eq!(request.event, AnnounceEvent::Completed);

    let events = tracker.handle_failure(at, TrackerError::Connect);
    let retry_at = failure_retry(&events);
    let request = tracker
        .build_request(retry_at, stats())
        .expect("completed retry");
    assert_eq!(request.event, AnnounceEvent::Completed);

    tracker.handle_response(retry_at, response(1800));
    tracker.complete(retry_at + Duration::from_secs(1));
    assert_eq!(
        tracker.next_deadline(),
        Some(retry_at + Duration::from_secs(1800))
    );

    let request = tracker
        .build_request(retry_at + Duration::from_secs(1800), stats())
        .expect("periodic");
    assert_eq!(request.event, AnnounceEvent::None);
}

#[tokio::test(start_paused = true)]
async fn completed_replaces_a_pending_started() {
    let now = Instant::now();
    let mut tracker = tracker(&[&["http://a.example/announce"]], false, now);

    tracker.complete(now);
    let request = tracker.build_request(now, stats()).expect("completed");
    assert_eq!(request.event, AnnounceEvent::Completed);

    tracker.handle_response(now, response(1800));
    let request = tracker
        .build_request(now + Duration::from_secs(1800), stats())
        .expect("periodic");
    assert_eq!(request.event, AnnounceEvent::None);
}

#[tokio::test(start_paused = true)]
async fn stopped_wins_over_a_pending_completed_and_is_terminal() {
    let now = Instant::now();
    let mut tracker = tracker(&[&["http://a.example/announce"]], false, now);

    tracker.build_request(now, stats()).expect("started");
    tracker.handle_response(now, response(1800));

    tracker.complete(now);
    tracker.stop(now);
    let request = tracker.build_request(now, stats()).expect("stopped");
    assert_eq!(request.event, AnnounceEvent::Stopped);
    assert_eq!(request.num_want, 0);

    tracker.handle_response(now, response(1800));
    assert!(tracker.is_terminal());
    assert_eq!(tracker.next_deadline(), None);
    assert!(tracker.status().terminal);
    assert_eq!(tracker.status().next_announce, None);

    tracker.complete(now);
    tracker.stop(now);
    tracker.queue_event(now, AnnounceEvent::Started);
    assert_eq!(tracker.next_deadline(), None);
    assert!(tracker.build_request(now, stats()).is_none());
}

#[tokio::test(start_paused = true)]
async fn a_failed_stopped_is_terminal_and_never_retried() {
    let now = Instant::now();
    let mut tracker = tracker(&[&["http://a.example/announce"]], false, now);

    tracker.build_request(now, stats()).expect("started");
    tracker.handle_response(now, response(1800));
    tracker.stop(now);
    tracker.build_request(now, stats()).expect("stopped");

    let events = tracker.handle_failure(now, TrackerError::Timeout);
    assert_eq!(
        events,
        vec![TrackerEvent::AnnounceFailed {
            error: TrackerError::Timeout,
            retry_at: None,
        }]
    );
    assert!(tracker.is_terminal());
    assert_eq!(tracker.next_deadline(), None);
    assert!(tracker.build_request(now, stats()).is_none());
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_stopped_is_terminal() {
    let now = Instant::now();
    let mut tracker = tracker(&[&["http://a.example/announce"]], false, now);

    tracker.build_request(now, stats()).expect("started");
    tracker.handle_response(now, response(1800));
    tracker.stop(now);
    tracker.build_request(now, stats()).expect("stopped");

    assert!(
        tracker
            .handle_cancel(now + Duration::from_secs(1))
            .is_empty()
    );
    assert!(tracker.is_terminal());
    assert_eq!(tracker.next_deadline(), None);
}

#[tokio::test(start_paused = true)]
async fn cancellation_requeues_the_event_without_counting_a_failure() {
    let now = Instant::now();
    let mut tracker = tracker(&[&["http://a.example/announce"]], false, now);

    tracker.build_request(now, stats()).expect("started");
    assert!(
        tracker
            .handle_cancel(now + Duration::from_secs(5))
            .is_empty()
    );

    assert_eq!(tracker.status().consecutive_failures, 0);
    assert_eq!(tracker.next_deadline(), Some(now + Duration::from_secs(5)));

    let request = tracker
        .build_request(now + Duration::from_secs(5), stats())
        .expect("started retry");
    assert_eq!(request.event, AnnounceEvent::Started);
    tracker.handle_response(now + Duration::from_secs(5), response(1800));

    let periodic = now + Duration::from_secs(1805);
    tracker.build_request(periodic, stats()).expect("periodic");
    tracker.handle_cancel(periodic + Duration::from_secs(1));

    assert_eq!(tracker.status().consecutive_failures, 0);
    assert_eq!(
        tracker.next_deadline(),
        Some(periodic + Duration::from_secs(1))
    );
    let request = tracker
        .build_request(periodic + Duration::from_secs(1), stats())
        .expect("periodic retry");
    assert_eq!(request.event, AnnounceEvent::None);
}

#[tokio::test(start_paused = true)]
async fn a_periodic_failure_does_not_requeue_started() {
    let now = Instant::now();
    let mut tracker = tracker(&[&["http://a.example/announce"]], false, now);

    tracker.build_request(now, stats()).expect("started");
    tracker.handle_response(now, response(1800));

    let at = now + Duration::from_secs(1800);
    tracker.build_request(at, stats()).expect("periodic");
    let retry_at = failure_retry(&tracker.handle_failure(at, TrackerError::Timeout));

    let request = tracker
        .build_request(retry_at, stats())
        .expect("periodic retry");
    assert_eq!(request.event, AnnounceEvent::None);
}

#[tokio::test(start_paused = true)]
async fn events_queued_during_flight_are_sent_next() {
    let now = Instant::now();
    let mut tracker = tracker(&[&["http://a.example/announce"]], false, now);

    tracker.build_request(now, stats()).expect("started");
    tracker.complete(now);
    tracker.stop(now);
    assert_eq!(tracker.next_deadline(), None);

    tracker.handle_response(now, response(1800));
    assert_eq!(tracker.next_deadline(), Some(now));

    let request = tracker.build_request(now, stats()).expect("stopped");
    assert_eq!(request.event, AnnounceEvent::Stopped);
}

#[tokio::test(start_paused = true)]
async fn completed_sent_at_start_is_not_sent_again() {
    let now = Instant::now();
    let mut config = config();
    config.completed_sent_at_start = true;
    let mut tracker = Tracker::new(
        candidates(&[&["http://a.example/announce"]], false),
        config,
        now,
    )
    .expect("tracker");

    tracker.complete(now);
    let request = tracker.build_request(now, stats()).expect("started");
    assert_eq!(request.event, AnnounceEvent::Started);

    tracker.handle_response(now, response(1800));
    tracker.complete(now);
    assert_eq!(
        tracker.next_deadline(),
        Some(now + Duration::from_secs(1800))
    );
}

#[tokio::test(start_paused = true)]
async fn a_duplicate_started_during_flight_is_not_queued_again() {
    let now = Instant::now();
    let mut tracker = tracker(&[&["http://a.example/announce"]], false, now);

    tracker.build_request(now, stats()).expect("started");
    tracker.queue_event(now, AnnounceEvent::Started);
    tracker.handle_response(now, response(1800));

    assert_eq!(
        tracker.next_deadline(),
        Some(now + Duration::from_secs(1800))
    );
    let request = tracker
        .build_request(now + Duration::from_secs(1800), stats())
        .expect("periodic");
    assert_eq!(request.event, AnnounceEvent::None);
}

#[tokio::test(start_paused = true)]
async fn a_duplicate_completed_during_flight_is_not_queued_again() {
    let now = Instant::now();
    let mut tracker = tracker(&[&["http://a.example/announce"]], false, now);

    tracker.build_request(now, stats()).expect("started");
    tracker.handle_response(now, response(1800));

    let at = now + Duration::from_secs(1800);
    tracker.complete(at);
    tracker.build_request(at, stats()).expect("completed");
    tracker.complete(at);
    tracker.handle_response(at, response(1800));

    assert_eq!(
        tracker.next_deadline(),
        Some(at + Duration::from_secs(1800))
    );
    let request = tracker
        .build_request(at + Duration::from_secs(1800), stats())
        .expect("periodic");
    assert_eq!(request.event, AnnounceEvent::None);
}

#[tokio::test(start_paused = true)]
async fn started_is_not_queued_behind_an_in_flight_completed() {
    let now = Instant::now();
    let mut tracker = tracker(&[&["http://a.example/announce"]], false, now);

    tracker.complete(now);
    tracker.build_request(now, stats()).expect("completed");
    tracker.queue_event(now, AnnounceEvent::Started);
    tracker.handle_response(now, response(1800));

    assert_eq!(
        tracker.next_deadline(),
        Some(now + Duration::from_secs(1800))
    );
    let request = tracker
        .build_request(now + Duration::from_secs(1800), stats())
        .expect("periodic");
    assert_eq!(request.event, AnnounceEvent::None);
}

#[tokio::test(start_paused = true)]
async fn private_torrents_only_follow_same_host_redirects() {
    let now = Instant::now();
    let mut private = tracker(&[&["http://a.example/announce"]], true, now);
    let request = private.build_request(now, stats()).expect("private");
    assert_eq!(request.redirect, RedirectPolicy::SameHostOnly);

    let mut public = tracker(&[&["http://a.example/announce"]], false, now);
    let request = public.build_request(now, stats()).expect("public");
    assert_eq!(request.redirect, RedirectPolicy::Public);
}

#[tokio::test(start_paused = true)]
async fn a_tracker_id_is_reused_for_the_same_tracker() {
    let now = Instant::now();
    let mut tracker = tracker(&[&["http://a.example/announce"]], false, now);

    tracker.build_request(now, stats()).expect("started");
    let mut first = response(1800);
    first.tracker_id = Some(TrackerId::new(b"first").expect("tracker id"));
    tracker.handle_response(now, first);

    let at = now + Duration::from_secs(1800);
    let request = tracker.build_request(at, stats()).expect("periodic");
    assert_eq!(request.tracker_id, TrackerId::new(b"first"));

    tracker.handle_response(at, response(1800));
    let at = at + Duration::from_secs(1800);
    let request = tracker.build_request(at, stats()).expect("periodic");
    assert_eq!(request.tracker_id, TrackerId::new(b"first"));

    let mut second = response(1800);
    second.tracker_id = Some(TrackerId::new(b"second").expect("tracker id"));
    tracker.handle_response(at, second);
    let at = at + Duration::from_secs(1800);
    let request = tracker.build_request(at, stats()).expect("periodic");
    assert_eq!(request.tracker_id, TrackerId::new(b"second"));
}

#[tokio::test(start_paused = true)]
async fn peers_and_warnings_are_emitted_with_the_success() {
    let now = Instant::now();
    let mut tracker = tracker(&[&["http://a.example/announce"]], false, now);
    let peer = SocketAddr::from((Ipv4Addr::new(127, 0, 0, 1), 6881));
    let warning = TrackerMessage::from_bytes(b"slow down");

    tracker.build_request(now, stats()).expect("started");
    let mut first = response(1800);
    first.peers = vec![peer];
    first.warning = Some(warning.clone());
    let events = tracker.handle_response(now, first);

    assert_eq!(
        events,
        vec![
            TrackerEvent::AnnounceSucceeded {
                interval: Duration::from_secs(1800),
            },
            TrackerEvent::PeersReceived(vec![peer]),
            TrackerEvent::Warning(warning),
        ]
    );
    assert_eq!(tracker.status().peer_count, 1);
    assert!(tracker.status().last_warning);

    let at = now + Duration::from_secs(1800);
    tracker.build_request(at, stats()).expect("periodic");
    let events = tracker.handle_response(at, response(1800));
    assert_eq!(
        events,
        vec![TrackerEvent::AnnounceSucceeded {
            interval: Duration::from_secs(1800),
        }]
    );
    assert_eq!(tracker.status().peer_count, 0);
    assert!(!tracker.status().last_warning);
}

#[tokio::test(start_paused = true)]
async fn status_is_a_redacted_snapshot() {
    let now = Instant::now();
    let mut tracker = tracker(
        &[&["http://tracker.example/announce/SECRET?passkey=TOPSECRET"]],
        false,
        now,
    );

    let handle = tracker.status_handle();
    let snapshot = match handle.read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    assert_eq!(snapshot.tracker_count, 1);
    assert!(!snapshot.private);
    assert_eq!(snapshot.next_announce, Some(now));

    let text = format!("{snapshot:?}");
    assert!(text.contains("TrackerUrl(http://tracker.example/…)"));
    assert!(!text.contains("SECRET"));
    assert!(!text.contains("TOPSECRET"));

    tracker.build_request(now, stats()).expect("started");
    assert_eq!(tracker.status().next_announce, None);
}

#[tokio::test(start_paused = true)]
async fn calls_without_an_in_flight_attempt_are_ignored() {
    let now = Instant::now();
    let mut tracker = tracker(&[&["http://a.example/announce"]], false, now);

    assert!(tracker.handle_response(now, response(1800)).is_empty());
    assert!(
        tracker
            .handle_failure(now, TrackerError::Timeout)
            .is_empty()
    );
    assert!(tracker.handle_cancel(now).is_empty());
    assert_eq!(tracker.next_deadline(), Some(now));
    assert_eq!(tracker.status().consecutive_failures, 0);
}

#[tokio::test(start_paused = true)]
async fn a_public_round_stops_after_the_first_successful_tier() {
    let now = Instant::now();
    let mut tracker = tracker(
        &[
            &["http://a.example/announce", "http://b.example/announce"],
            &["http://c.example/announce"],
        ],
        false,
        now,
    );
    let mut requested = Vec::new();

    let request = tracker.build_request(now, stats()).expect("started on a");
    requested.push(request.url.clone());
    assert_eq!(request.url, url("http://a.example/announce"));
    assert_eq!(request.event, AnnounceEvent::Started);

    let events = tracker.handle_failure(now, TrackerError::Timeout);
    let retry_at = failure_retry(&events);
    assert_eq!(retry_at, now);
    assert!(has_switch(&events, false));

    let request = tracker.build_request(now, stats()).expect("started on b");
    requested.push(request.url.clone());
    assert_eq!(request.url, url("http://b.example/announce"));
    assert_eq!(request.event, AnnounceEvent::Started);
    tracker.handle_response(now, response(1800));

    let a_retry = tracker.next_deadline().expect("a retry");
    assert!(a_retry > now && a_retry <= now + Duration::from_secs(36));

    let request = tracker.build_request(a_retry, stats()).expect("a retry");
    requested.push(request.url.clone());
    assert_eq!(request.url, url("http://a.example/announce"));
    assert_eq!(request.event, AnnounceEvent::None);
    tracker.handle_response(a_retry, response(1800));

    let b_retry = tracker.next_deadline().expect("b retry");
    assert_eq!(b_retry, now + Duration::from_secs(1800));
    let request = tracker.build_request(b_retry, stats()).expect("b retry");
    requested.push(request.url.clone());
    assert_eq!(request.url, url("http://b.example/announce"));
    tracker.handle_response(b_retry, response(1800));

    assert!(!requested.contains(&url("http://c.example/announce")));
    assert!(requested.iter().all(|requested| {
        *requested == url("http://a.example/announce")
            || *requested == url("http://b.example/announce")
    }));
}

#[tokio::test(start_paused = true)]
async fn peers_from_successes_in_one_tier_are_merged_and_deduped() {
    let now = Instant::now();
    let mut tracker = tracker(
        &[
            &["http://a.example/announce", "http://b.example/announce"],
            &["http://c.example/announce"],
        ],
        false,
        now,
    );
    let p1 = peer(1);
    let p2 = peer(2);
    let p3 = peer(3);

    let request = tracker.build_request(now, stats()).expect("started");
    assert_eq!(request.url, url("http://a.example/announce"));
    tracker.handle_response(now, response(60));

    let at = now + Duration::from_secs(60);
    let request = tracker.build_request(at, stats()).expect("a");
    assert_eq!(request.url, url("http://a.example/announce"));
    let mut a = response(60);
    a.peers = vec![p1, p2];
    let a_events = tracker.handle_response(at, a);
    assert_eq!(peer_batches(&a_events), vec![vec![p1, p2]]);

    let request = tracker.build_request(at, stats()).expect("b");
    assert_eq!(request.url, url("http://b.example/announce"));
    assert_eq!(request.event, AnnounceEvent::None);
    let mut b = response(60);
    b.peers = vec![p2, p3];
    let b_events = tracker.handle_response(at, b);
    assert_eq!(peer_batches(&b_events), vec![vec![p3]]);
    assert_eq!(tracker.status().peer_count, 3);
    assert!(tracker.build_request(at, stats()).is_none());
}

#[tokio::test(start_paused = true)]
async fn the_first_successful_tracker_in_a_tier_moves_to_the_front() {
    let now = Instant::now();
    let mut tracker = tracker(
        &[&[
            "http://a.example/announce",
            "http://b.example/announce",
            "http://c.example/announce",
        ]],
        false,
        now,
    );

    let request = tracker.build_request(now, stats()).expect("started");
    assert_eq!(request.url, url("http://a.example/announce"));
    tracker.handle_response(now, response(60));

    let at = now + Duration::from_secs(60);
    let request = tracker.build_request(at, stats()).expect("a");
    assert_eq!(request.url, url("http://a.example/announce"));
    tracker.handle_failure(at, TrackerError::Timeout);

    let request = tracker.build_request(at, stats()).expect("b");
    assert_eq!(request.url, url("http://b.example/announce"));
    let mut b = response(60);
    b.peers = vec![peer(1)];
    tracker.handle_response(at, b);

    let request = tracker.build_request(at, stats()).expect("c");
    assert_eq!(request.url, url("http://c.example/announce"));
    let mut c = response(60);
    c.peers = vec![peer(2)];
    let events = tracker.handle_response(at, c);

    assert!(has_switch(&events, false));
    assert_eq!(
        tracker.status().active_url,
        Some(url("http://b.example/announce"))
    );
    assert_eq!(
        tracker.slots[0]
            .iter()
            .map(|slot| slot.url.clone())
            .collect::<Vec<_>>(),
        vec![
            url("http://b.example/announce"),
            url("http://a.example/announce"),
            url("http://c.example/announce"),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_tracker_keeps_its_own_backoff() {
    let now = Instant::now();
    let mut tracker = tracker(
        &[&["http://a.example/announce", "http://b.example/announce"]],
        false,
        now,
    );

    let request = tracker.build_request(now, stats()).expect("started");
    assert_eq!(request.url, url("http://a.example/announce"));
    tracker.handle_response(now, response(60));

    let at = now + Duration::from_secs(60);
    let request = tracker.build_request(at, stats()).expect("a");
    assert_eq!(request.url, url("http://a.example/announce"));
    tracker.handle_response(at, response(60));

    let request = tracker.build_request(at, stats()).expect("b");
    assert_eq!(request.url, url("http://b.example/announce"));
    tracker.handle_failure(at, TrackerError::Timeout);
    assert_eq!(tracker.slots[0][1].failures, 1);
    assert_eq!(tracker.status().consecutive_failures, 0);

    let b_retry = tracker.next_deadline().expect("b retry");
    assert!(b_retry > at && b_retry < at + Duration::from_secs(60));
    let request = tracker.build_request(b_retry, stats()).expect("b retry");
    assert_eq!(request.url, url("http://b.example/announce"));
    tracker.handle_failure(b_retry, TrackerError::Timeout);
    assert_eq!(tracker.status().consecutive_failures, 2);

    let a_retry = tracker.next_deadline().expect("a retry");
    assert_eq!(a_retry, at + Duration::from_secs(60));
    let request = tracker.build_request(a_retry, stats()).expect("a retry");
    assert_eq!(request.url, url("http://a.example/announce"));
}

#[tokio::test(start_paused = true)]
async fn a_private_failure_switches_to_the_next_candidate_and_drops_peers() {
    let now = Instant::now();
    let mut tracker = tracker(
        &[&["http://a.example/announce", "http://b.example/announce"]],
        true,
        now,
    );

    let request = tracker.build_request(now, stats()).expect("started");
    assert_eq!(request.url, url("http://a.example/announce"));
    let events = tracker.handle_failure(now, TrackerError::Timeout);
    assert!(has_switch(&events, true));
    assert_eq!(
        tracker.status().active_url,
        Some(url("http://b.example/announce"))
    );
    assert_eq!(failure_retry(&events), now);

    let request = tracker.build_request(now, stats()).expect("started on b");
    assert_eq!(request.url, url("http://b.example/announce"));
    assert_eq!(request.event, AnnounceEvent::Started);
    let events = tracker.handle_response(now, response(60));
    assert!(!has_switch(&events, true));
    assert_eq!(
        tracker.notified_trackers(),
        vec![NotifiedTracker {
            url: url("http://b.example/announce"),
            tracker_id: None,
        }]
    );
    assert_eq!(
        tracker.status().active_url,
        Some(url("http://b.example/announce"))
    );
}

#[tokio::test(start_paused = true)]
async fn a_private_torrent_cycles_through_every_candidate_before_retrying() {
    let now = Instant::now();
    let mut tracker = tracker(
        &[&[
            "http://a.example/announce",
            "http://b.example/announce",
            "http://c.example/announce",
        ]],
        true,
        now,
    );
    let mut requested = Vec::new();
    let mut at = now;

    for expected in [
        "http://a.example/announce",
        "http://b.example/announce",
        "http://c.example/announce",
    ] {
        let request = tracker.build_request(at, stats()).expect("started");
        requested.push(request.url.clone());
        assert_eq!(request.url, url(expected));
        assert_eq!(request.event, AnnounceEvent::Started);
        let events = tracker.handle_failure(at, TrackerError::Timeout);
        assert!(has_switch(&events, true));
        at = failure_retry(&events);
        assert_eq!(Some(at), tracker.next_deadline());
    }

    let request = tracker.build_request(at, stats()).expect("a retry");
    requested.push(request.url.clone());
    assert_eq!(request.url, url("http://a.example/announce"));
    assert_eq!(
        requested,
        vec![
            url("http://a.example/announce"),
            url("http://b.example/announce"),
            url("http://c.example/announce"),
            url("http://a.example/announce"),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_private_single_candidate_retries_without_a_switch() {
    let now = Instant::now();
    let mut tracker = tracker(&[&["http://a.example/announce"]], true, now);

    tracker.build_request(now, stats()).expect("started");
    let events = tracker.handle_failure(now, TrackerError::Timeout);
    assert!(!has_switch(&events, true));
    let retry = failure_retry(&events);
    assert!(retry > now);

    let request = tracker.build_request(retry, stats()).expect("retry");
    assert_eq!(request.url, url("http://a.example/announce"));
    assert_eq!(request.event, AnnounceEvent::Started);
}

#[tokio::test(start_paused = true)]
async fn a_private_success_keeps_the_active_tracker() {
    let now = Instant::now();
    let mut tracker = tracker(
        &[&["http://a.example/announce", "http://b.example/announce"]],
        true,
        now,
    );

    tracker.build_request(now, stats()).expect("started");
    let events = tracker.handle_response(now, response(60));
    assert!(!has_switch(&events, true));

    let at = now + Duration::from_secs(60);
    let request = tracker.build_request(at, stats()).expect("periodic");
    assert_eq!(request.url, url("http://a.example/announce"));
    assert_eq!(request.event, AnnounceEvent::None);
    let events = tracker.handle_response(at, response(60));
    assert!(!has_switch(&events, true));
    assert_eq!(
        tracker.status().active_url,
        Some(url("http://a.example/announce"))
    );
    assert!(
        !tracker
            .notified_trackers()
            .iter()
            .any(|tracker| tracker.url == url("http://b.example/announce"))
    );
}

#[tokio::test(start_paused = true)]
async fn notified_trackers_excludes_trackers_that_only_failed() {
    let now = Instant::now();
    let mut tracker = tracker(
        &[&[
            "http://a.example/announce",
            "http://b.example/announce",
            "http://c.example/announce",
        ]],
        true,
        now,
    );

    tracker.build_request(now, stats()).expect("started on a");
    tracker.handle_failure(now, TrackerError::Timeout);
    tracker.build_request(now, stats()).expect("started on b");
    tracker.handle_response(now, response(60));

    assert_eq!(
        tracker.notified_trackers(),
        vec![NotifiedTracker {
            url: url("http://b.example/announce"),
            tracker_id: None,
        }]
    );
}

#[tokio::test(start_paused = true)]
async fn stopped_ignores_backoff_and_is_terminal() {
    let now = Instant::now();
    let mut tracker = tracker(&[&["http://a.example/announce"]], false, now);

    tracker.build_request(now, stats()).expect("started");
    let events = tracker.handle_failure(now, TrackerError::Timeout);
    assert!(failure_retry(&events) > now);

    tracker.stop(now);
    assert_eq!(tracker.next_deadline(), Some(now));
    let request = tracker.build_request(now, stats()).expect("stopped");
    assert_eq!(request.event, AnnounceEvent::Stopped);
    tracker.handle_response(now, response(0));
    assert!(tracker.is_terminal());
    assert_eq!(tracker.next_deadline(), None);
}

#[tokio::test(start_paused = true)]
async fn cancellation_between_tier_successes_keeps_emitted_peers_and_does_not_repeat_them() {
    let now = Instant::now();
    let mut tracker = tracker(
        &[&["http://a.example/announce", "http://b.example/announce"]],
        false,
        now,
    );
    let p1 = peer(1);
    let p2 = peer(2);

    tracker.build_request(now, stats()).expect("started");
    tracker.handle_response(now, response(60));

    let at = now + Duration::from_secs(60);
    tracker.build_request(at, stats()).expect("a");
    let mut a = response(60);
    a.peers = vec![p1];
    let a_events = tracker.handle_response(at, a);
    assert_eq!(peer_batches(&a_events), vec![vec![p1]]);

    let request = tracker.build_request(at, stats()).expect("b");
    assert_eq!(request.url, url("http://b.example/announce"));
    tracker.handle_cancel(at);
    assert_eq!(tracker.status().consecutive_failures, 0);

    let request = tracker.build_request(at, stats()).expect("b retry");
    assert_eq!(request.url, url("http://b.example/announce"));
    let mut b = response(60);
    b.peers = vec![p1, p2];
    let b_events = tracker.handle_response(at, b);
    assert_eq!(peer_batches(&b_events), vec![vec![p2]]);
    assert_eq!(tracker.status().peer_count, 2);
    assert_eq!(
        tracker.status().active_url,
        Some(url("http://a.example/announce"))
    );
}

#[tokio::test(start_paused = true)]
async fn a_scripted_public_session_matches_the_expected_trace() {
    let now = Instant::now();
    let mut tracker = tracker(
        &[
            &["http://a.example/announce", "http://b.example/announce"],
            &["http://c.example/announce"],
        ],
        false,
        now,
    );
    let mut trace = Vec::new();

    // The started event fails on A and succeeds on B.
    let request = tracker.build_request(now, stats()).expect("started");
    trace.push((request.url.clone(), request.event));
    tracker.handle_failure(now, TrackerError::Timeout);

    let request = tracker.build_request(now, stats()).expect("started");
    trace.push((request.url.clone(), request.event));
    tracker.handle_response(now, response(60));

    // A's own backoff expires first. Its success moves it to the front and
    // ends the round, so C is never contacted.
    let a_retry = tracker.next_deadline().expect("a retry");
    assert!(a_retry > now && a_retry < now + Duration::from_secs(60));
    let request = tracker.build_request(a_retry, stats()).expect("a retry");
    trace.push((request.url.clone(), request.event));
    tracker.handle_response(a_retry, response(60));

    // B is next at its interval.
    let b_due = tracker.next_deadline().expect("b due");
    assert_eq!(b_due, now + Duration::from_secs(60));
    let request = tracker.build_request(b_due, stats()).expect("b");
    trace.push((request.url.clone(), request.event));
    let events = tracker.handle_response(b_due, response(60));
    assert!(has_switch(&events, false));

    // The remaining timer is A's interval.
    let a_due = tracker.next_deadline().expect("a due");
    assert_eq!(a_due, a_retry + Duration::from_secs(60));
    let request = tracker.build_request(a_due, stats()).expect("a");
    trace.push((request.url.clone(), request.event));
    tracker.handle_response(a_due, response(60));

    assert_eq!(
        trace,
        vec![
            (url("http://a.example/announce"), AnnounceEvent::Started),
            (url("http://b.example/announce"), AnnounceEvent::Started),
            (url("http://a.example/announce"), AnnounceEvent::None),
            (url("http://b.example/announce"), AnnounceEvent::None),
            (url("http://a.example/announce"), AnnounceEvent::None),
        ]
    );
    assert_eq!(
        tracker.status().active_url,
        Some(url("http://a.example/announce"))
    );
}

#[tokio::test(start_paused = true)]
async fn an_unrepresentable_deadline_schedules_no_timer() {
    let base = Instant::now();
    let mut good = Duration::ZERO;
    let mut bad = Duration::from_secs(u64::MAX);
    if base.checked_add(bad).is_some() {
        // This platform can represent every offset; the fallback cannot be
        // reached with a real monotonic clock, so there is nothing to check.
        return;
    }
    while bad - good > Duration::from_secs(1) {
        let mid = good + (bad - good) / 2;
        if base.checked_add(mid).is_some() {
            good = mid;
        } else {
            bad = mid;
        }
    }

    // Stop one second short of the largest representable instant, so adding
    // any real announce interval overflows.
    let delta = good.saturating_sub(Duration::from_secs(1));
    tokio::time::advance(delta).await;
    let now = Instant::now();

    let mut tracker = tracker(&[&["http://a.example/announce"]], false, now);
    tracker.build_request(now, stats()).expect("started");
    tracker.handle_response(now, response(60));

    assert_eq!(tracker.status().next_announce, None);
    assert_eq!(tracker.next_deadline(), None);
    assert!(!tracker.is_terminal());
}

#[tokio::test(start_paused = true)]
async fn a_failed_event_does_not_bypass_backoff_when_an_earlier_tier_is_due() {
    let now = Instant::now();
    let mut tracker = tracker(
        &[
            &["http://a.example/announce"],
            &["http://b.example/announce"],
        ],
        false,
        now,
    );

    tracker.build_request(now, stats()).expect("started on a");
    tracker.handle_response(now, response(60));

    let at = now + Duration::from_secs(60);
    let request = tracker.build_request(at, stats()).expect("periodic a");
    assert_eq!(request.url, url("http://a.example/announce"));
    tracker.handle_failure(at, TrackerError::Timeout);

    let request = tracker.build_request(at, stats()).expect("periodic b");
    assert_eq!(request.url, url("http://b.example/announce"));
    tracker.handle_response(at, response(60));

    // A's backoff expires before B's interval, so A is due while B is active.
    let a_retry = tracker.next_deadline().expect("a retry");
    tracker.complete(a_retry);
    let request = tracker
        .build_request(a_retry, stats())
        .expect("completed on b");
    assert_eq!(request.url, url("http://b.example/announce"));
    tracker.handle_failure(a_retry, TrackerError::Timeout);

    // B must not be retried immediately with its backoff overwritten; the
    // earlier tier is due, so the pending event moves there.
    let request = tracker
        .build_request(a_retry, stats())
        .expect("completed on a");
    assert_eq!(request.url, url("http://a.example/announce"));
    assert_eq!(request.event, AnnounceEvent::Completed);
    tracker.handle_response(a_retry, response(60));

    // `completed` was sent once; a duplicate trigger is ignored.
    tracker.complete(a_retry + Duration::from_secs(1));
    assert_eq!(
        tracker.next_deadline(),
        Some(a_retry + Duration::from_secs(60))
    );
}
