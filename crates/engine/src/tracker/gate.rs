//! Per-host gate for UDP announce attempts.
//!
//! The session holds one [`HostGate`]. Before a UDP attempt starts it acquires
//! the gate for the tracker's normalized host and effective port; the permit
//! is held until the attempt ends. This keeps a stalled attempt from stacking
//! sockets, timers and retransmissions for the same host.
//!
//! The gate covers DNS resolution too, because the attempt begins there. If
//! the task is cancelled while the blocking resolver is still running, the
//! permit is released immediately and the resolver continues in the
//! background under the resolver's own eight-permit semaphore. The gate never
//! waits for the resolver to finish.
//!
//! The lock is a [`std::sync::Mutex`] held only for a set insert or remove,
//! never across an `.await`. Waiters park on [`tokio::sync::Notify`].

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::Notify;

use super::{Host, TrackerUrl};

/// Default wait for a busy host gate.
///
/// A second attempt for the same host and port waits this long, then gives up
/// so the tracker task can report a timeout instead of holding the attempt
/// forever.
pub const GATE_WAIT: Duration = Duration::from_secs(30);

/// Normalized host and effective port that share one gate.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HostKey {
    host: Host,
    port: u16,
}

impl HostKey {
    /// Builds the key for a tracker URL.
    pub fn new(url: &TrackerUrl) -> Self {
        Self {
            host: url.host().clone(),
            port: url.port(),
        }
    }
}

/// At most one announce attempt per host and effective port.
#[derive(Debug, Default)]
pub struct HostGate {
    busy: Mutex<HashSet<HostKey>>,
    released: Notify,
}

impl HostGate {
    /// Creates an empty gate.
    pub fn new() -> Self {
        Self::default()
    }

    /// Waits until the key is free and returns a permit that releases it on
    /// drop.
    ///
    /// The future is cancel-safe: dropping it while it waits leaves no state.
    pub async fn acquire(self: &Arc<Self>, key: HostKey) -> GatePermit {
        loop {
            // Register before the check so a release between the check and the
            // await cannot be lost.
            let notified = self.released.notified();
            {
                let mut busy = self.lock();
                if busy.insert(key.clone()) {
                    return GatePermit {
                        gate: Arc::clone(self),
                        key,
                    };
                }
            }
            notified.await;
        }
    }

    /// Waits up to `wait` for the key.
    ///
    /// Returns `None` when the wait expires; the caller decides whether that
    /// is a tracker timeout or a retryable scheduling delay.
    pub async fn acquire_for(self: &Arc<Self>, key: HostKey, wait: Duration) -> Option<GatePermit> {
        tokio::time::timeout(wait, self.acquire(key)).await.ok()
    }

    fn lock(&self) -> MutexGuard<'_, HashSet<HostKey>> {
        match self.busy.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Releases its key when dropped.
#[derive(Debug)]
pub struct GatePermit {
    gate: Arc<HostGate>,
    key: HostKey,
}

impl Drop for GatePermit {
    fn drop(&mut self) {
        {
            let mut busy = self.gate.lock();
            busy.remove(&self.key);
        }
        self.gate.released.notify_waiters();
    }
}

#[cfg(test)]
mod tests;
