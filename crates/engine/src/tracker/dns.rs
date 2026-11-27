//! Bounded tracker host resolution.
//!
//! Hostnames resolve on the blocking pool through a process-wide semaphore of
//! eight permits. A permit is acquired before the blocking call and released
//! only when that call returns, so a hung resolver can hold at most eight
//! threads. Waiting for a permit is capped at two seconds; the lookup itself
//! is capped at five seconds after the permit is acquired.
//!
//! These timeouts use the real clock and are tested against it. The attempt
//! schedule is separate and uses `tokio::time::Instant` under a paused clock.

use std::future::Future;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::task;
use tokio::time::timeout;

use super::{Host, TargetPolicy, TrackerError};

/// Maximum concurrent resolver calls in one process.
pub const MAX_CONCURRENT_LOOKUPS: usize = 8;

/// Maximum addresses considered per lookup.
pub const MAX_RESOLVED_ADDRESSES: usize = 8;

/// Maximum wait for a resolver permit.
pub const DNS_QUEUE_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum lookup time after the permit is acquired.
pub const DNS_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

type ResolveFuture<'a> =
    Pin<Box<dyn Future<Output = Result<SocketAddr, TrackerError>> + Send + 'a>>;

/// Resolves a tracker host to one policy-checked socket address.
pub trait Resolver: Send + Sync {
    /// Resolves `host` on `port` and returns the first allowed address.
    fn resolve<'a>(&'a self, host: &'a Host, port: u16, policy: TargetPolicy) -> ResolveFuture<'a>;
}

/// Blocking lookup seam, replaceable in tests.
trait Lookup: Send + Sync {
    fn lookup(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>>;
}

struct SystemLookup;

impl Lookup for SystemLookup {
    fn lookup(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        (host, port).to_socket_addrs().map(|addresses| {
            addresses
                .take(MAX_RESOLVED_ADDRESSES)
                .collect::<Vec<SocketAddr>>()
        })
    }
}

/// Production resolver backed by the system resolver.
pub struct SystemResolver {
    permits: Arc<Semaphore>,
    lookup: Arc<dyn Lookup>,
    queue_timeout: Duration,
    lookup_timeout: Duration,
}

impl SystemResolver {
    /// Builds a resolver with the process-wide permit pool.
    pub fn new() -> Self {
        Self {
            permits: process_permits(),
            lookup: Arc::new(SystemLookup),
            queue_timeout: DNS_QUEUE_TIMEOUT,
            lookup_timeout: DNS_LOOKUP_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_lookup(
        permits: usize,
        queue_timeout: Duration,
        lookup_timeout: Duration,
        lookup: Arc<dyn Lookup>,
    ) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(permits)),
            lookup,
            queue_timeout,
            lookup_timeout,
        }
    }
}

impl Default for SystemResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl Resolver for SystemResolver {
    fn resolve<'a>(&'a self, host: &'a Host, port: u16, policy: TargetPolicy) -> ResolveFuture<'a> {
        Box::pin(async move {
            if let Some(address) = host.canonical_ip() {
                policy.check(address)?;
                return Ok(SocketAddr::new(address, port));
            }

            let Host::Name(name) = host else {
                return Err(TrackerError::Dns);
            };

            let permit =
                match timeout(self.queue_timeout, self.permits.clone().acquire_owned()).await {
                    Ok(Ok(permit)) => permit,
                    Ok(Err(_)) | Err(_) => return Err(TrackerError::Dns),
                };

            let lookup = Arc::clone(&self.lookup);
            let name = name.clone();
            let job = task::spawn_blocking(move || {
                let result = lookup.lookup(&name, port);
                drop(permit);
                result
            });

            let addresses = match timeout(self.lookup_timeout, job).await {
                Ok(Ok(Ok(addresses))) => addresses,
                Ok(Ok(Err(_))) | Ok(Err(_)) | Err(_) => return Err(TrackerError::Dns),
            };

            for address in addresses.into_iter().take(MAX_RESOLVED_ADDRESSES) {
                let address = SocketAddr::new(address.ip(), port);
                if policy.check(address.ip()).is_ok() {
                    return Ok(address);
                }
            }

            Err(TrackerError::BlockedTarget)
        })
    }
}

fn process_permits() -> Arc<Semaphore> {
    static PERMITS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    PERMITS
        .get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT_LOOKUPS)))
        .clone()
}

#[cfg(test)]
mod tests;
