use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::tracker::{Host, TargetPolicy, TrackerError};

use super::{Lookup, MAX_RESOLVED_ADDRESSES, Resolver, SystemResolver};

fn name() -> Host {
    Host::Name("tracker.example".to_string())
}

fn ip(text: &str) -> Host {
    Host::Ip(text.parse::<IpAddr>().expect("valid IP literal"))
}

fn socket(text: &str, port: u16) -> SocketAddr {
    SocketAddr::new(text.parse::<IpAddr>().expect("valid IP literal"), port)
}

fn policy() -> TargetPolicy {
    TargetPolicy::new(false)
}

struct StaticLookup {
    calls: AtomicUsize,
    addresses: Vec<SocketAddr>,
}

impl StaticLookup {
    fn new(addresses: Vec<SocketAddr>) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            addresses,
        })
    }
}

impl Lookup for StaticLookup {
    fn lookup(&self, _host: &str, _port: u16) -> io::Result<Vec<SocketAddr>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.addresses.clone())
    }
}

/// Lookup that blocks until the test releases it.
struct GateLookup {
    release: Arc<(Mutex<bool>, Condvar)>,
}

impl GateLookup {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            release: Arc::new((Mutex::new(false), Condvar::new())),
        })
    }

    fn release(&self) {
        let (lock, cvar) = &*self.release;
        *lock.lock().expect("gate lock") = true;
        cvar.notify_all();
    }
}

impl Lookup for GateLookup {
    fn lookup(&self, _host: &str, _port: u16) -> io::Result<Vec<SocketAddr>> {
        let (lock, cvar) = &*self.release;
        let mut released = lock.lock().expect("gate lock");
        while !*released {
            released = cvar.wait(released).expect("gate wait");
        }
        drop(released);

        Ok(vec![socket("93.184.216.34", 6969)])
    }
}

fn wait_for_permits(resolver: &SystemResolver, expected: usize) {
    for _ in 0..1_000 {
        if resolver.permits.available_permits() == expected {
            return;
        }
        std::thread::yield_now();
    }
    panic!(
        "expected {expected} free permits, found {}",
        resolver.permits.available_permits()
    );
}

fn resolver_with(
    lookup: Arc<dyn Lookup>,
    permits: usize,
    queue: Duration,
    lookup_timeout: Duration,
) -> Arc<SystemResolver> {
    Arc::new(SystemResolver::with_lookup(
        permits,
        queue,
        lookup_timeout,
        lookup,
    ))
}

#[tokio::test]
async fn an_ip_literal_skips_the_lookup_and_is_policy_checked() {
    let lookup = StaticLookup::new(vec![socket("93.184.216.34", 80)]);
    let resolver = resolver_with(
        lookup.clone(),
        8,
        Duration::from_secs(2),
        Duration::from_secs(5),
    );

    let address = resolver
        .resolve(&ip("1.1.1.1"), 6969, policy())
        .await
        .expect("public literal");
    assert_eq!(address, socket("1.1.1.1", 6969));
    assert_eq!(lookup.calls.load(Ordering::SeqCst), 0);

    assert_eq!(
        resolver
            .resolve(&ip("::ffff:127.0.0.1"), 6969, policy())
            .await,
        Err(TrackerError::BlockedTarget)
    );
    assert_eq!(
        resolver.resolve(&ip("::1"), 6969, policy()).await,
        Err(TrackerError::BlockedTarget)
    );
    assert_eq!(lookup.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn blocked_results_are_skipped_and_the_first_allowed_one_wins() {
    let lookup = StaticLookup::new(vec![
        socket("127.0.0.1", 80),
        socket("93.184.216.34", 80),
        socket("8.8.8.8", 80),
    ]);
    let resolver = resolver_with(lookup, 8, Duration::from_secs(2), Duration::from_secs(5));

    let address = resolver
        .resolve(&name(), 6969, policy())
        .await
        .expect("one allowed result");
    assert_eq!(address, socket("93.184.216.34", 6969));
}

#[tokio::test]
async fn an_all_blocked_result_is_blocked_target() {
    let lookup = StaticLookup::new(vec![socket("127.0.0.1", 80), socket("10.0.0.1", 80)]);
    let resolver = resolver_with(lookup, 8, Duration::from_secs(2), Duration::from_secs(5));

    assert_eq!(
        resolver.resolve(&name(), 6969, policy()).await,
        Err(TrackerError::BlockedTarget)
    );
}

#[tokio::test]
async fn a_resolved_ipv4_mapped_address_is_blocked() {
    let lookup = StaticLookup::new(vec![socket("::ffff:127.0.0.1", 80)]);
    let resolver = resolver_with(lookup, 8, Duration::from_secs(2), Duration::from_secs(5));

    assert_eq!(
        resolver.resolve(&name(), 6969, policy()).await,
        Err(TrackerError::BlockedTarget)
    );
}

#[tokio::test]
async fn only_the_first_eight_results_are_considered() {
    let mut addresses = vec![socket("127.0.0.1", 80); MAX_RESOLVED_ADDRESSES];
    addresses.push(socket("93.184.216.34", 80));
    let lookup = StaticLookup::new(addresses);
    let resolver = resolver_with(lookup, 8, Duration::from_secs(2), Duration::from_secs(5));

    assert_eq!(
        resolver.resolve(&name(), 6969, policy()).await,
        Err(TrackerError::BlockedTarget)
    );
}

#[tokio::test]
async fn eight_concurrent_lookups_hold_the_permits_and_the_ninth_times_out() {
    let lookup = GateLookup::new();
    let resolver = resolver_with(
        lookup.clone(),
        8,
        Duration::from_millis(100),
        Duration::from_secs(5),
    );

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let resolver = Arc::clone(&resolver);
        tasks.push(tokio::spawn(async move {
            resolver.resolve(&name(), 6969, policy()).await
        }));
    }
    for _ in 0..1_000 {
        if resolver.permits.available_permits() == 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        resolver.permits.available_permits(),
        0,
        "eight lookups hold every permit"
    );

    let ninth = {
        let resolver = Arc::clone(&resolver);
        tokio::spawn(async move { resolver.resolve(&name(), 6969, policy()).await })
    };
    assert_eq!(ninth.await.expect("task"), Err(TrackerError::Dns));

    lookup.release();
    for task in tasks {
        assert_eq!(task.await.expect("task"), Ok(socket("93.184.216.34", 6969)));
    }
    assert_eq!(resolver.permits.available_permits(), 8);
}

#[tokio::test]
async fn dropping_the_future_keeps_the_permit_until_the_lookup_returns() {
    let lookup = GateLookup::new();
    let resolver = resolver_with(
        lookup.clone(),
        8,
        Duration::from_secs(2),
        Duration::from_secs(5),
    );

    let host = name();
    let mut future = resolver.resolve(&host, 6969, policy());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut future)
            .await
            .is_err()
    );
    assert_eq!(resolver.permits.available_permits(), 7);

    drop(future);
    assert_eq!(resolver.permits.available_permits(), 7);

    lookup.release();
    wait_for_permits(&resolver, 8);
}

#[tokio::test]
async fn a_lookup_past_the_timeout_fails_with_dns_and_still_releases_the_permit() {
    let lookup = GateLookup::new();
    let resolver = resolver_with(
        lookup.clone(),
        8,
        Duration::from_secs(2),
        Duration::from_millis(50),
    );

    assert_eq!(
        resolver.resolve(&name(), 6969, policy()).await,
        Err(TrackerError::Dns)
    );
    assert_eq!(resolver.permits.available_permits(), 7);

    lookup.release();
    wait_for_permits(&resolver, 8);
}

#[tokio::test]
async fn a_lookup_error_is_dns() {
    struct FailingLookup;

    impl Lookup for FailingLookup {
        fn lookup(&self, _host: &str, _port: u16) -> io::Result<Vec<SocketAddr>> {
            Err(io::Error::other("resolver failed"))
        }
    }

    let resolver = resolver_with(
        Arc::new(FailingLookup),
        8,
        Duration::from_secs(2),
        Duration::from_secs(5),
    );

    assert_eq!(
        resolver.resolve(&name(), 6969, policy()).await,
        Err(TrackerError::Dns)
    );
}

#[test]
fn the_documented_limits_are_the_contract() {
    assert_eq!(MAX_RESOLVED_ADDRESSES, 8);
    assert_eq!(super::MAX_CONCURRENT_LOOKUPS, 8);
    assert_eq!(super::DNS_QUEUE_TIMEOUT, Duration::from_secs(2));
    assert_eq!(super::DNS_LOOKUP_TIMEOUT, Duration::from_secs(5));
}
