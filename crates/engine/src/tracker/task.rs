//! The tracker task: one state machine, one transport, one attempt at a time.
//!
//! [`spawn`] owns a [`Tracker`] and drives it until it is terminal or its
//! handle is dropped. The task is the only place that awaits a transport, so
//! the state machine stays pure and time-dependent tests stay deterministic.
//!
//! # Attempts
//!
//! The task builds one request when the state machine is due, waits for the
//! per-host UDP gate when the URL is `udp`, and runs the transport future
//! under a watchdog of [`TrackerTransport::max_duration`] plus
//! [`WATCHDOG_MARGIN`]. A watchdog expiry and a gate expiry are reported as
//! [`TrackerError::Timeout`]; the transport's own deadline is unchanged.
//! Only one attempt per tracker is in flight.
//!
//! A transport panic is contained by a [`std::panic::catch_unwind`] wrapper
//! and reported as [`TrackerError::Panicked`]. This only works in unwind
//! builds: release builds use `panic = "abort"`, so a transport panic ends
//! the process. Tests for the panic path are compiled only with
//! `#[cfg(panic = "unwind")]`.
//!
//! # Cancellation
//!
//! Dropping the in-flight future is not a tracker failure. A `Complete`
//! command cancels the attempt, requeues the event through
//! [`Tracker::handle_cancel`], and retries immediately. A `Stop` command
//! cancels the attempt and starts the stop fan-out. `UpdateStats` never
//! cancels an attempt; it updates the counters used by the next request.
//!
//! # Stop fan-out
//!
//! On `Stop`, or when the handle is dropped, the task sends `stopped`
//! directly to every tracker that has answered at least one announce, in
//! parallel and under one [`STOP_BUDGET`] deadline. Each request carries that
//! tracker's own tracker ID. Trackers that only ever failed are not notified.
//! A stopped attempt is never retried: a failure or a budget expiry is
//! reported with `retry_at: None`, and the tracker becomes terminal. The
//! state machine is marked terminal only after the fan-out.
//!
//! # Events and backpressure
//!
//! Events go to a bounded channel. The task never blocks on a full channel:
//! data events (status, peers, warnings) are coalesced, and peer batches are
//! merged up to [`MAX_QUEUED_PEERS`], dropping the oldest batches first.
//! [`TrackerEvent::TrackerSwitched`] and [`TrackerEvent::NoUsableTracker`]
//! are control events: they are never dropped or coalesced. One channel slot
//! stays reserved for control events, and the task waits for capacity rather
//! than discarding them. On shutdown, queued data may be dropped to deliver
//! a control event; control events are still never dropped.

#[cfg(test)]
mod tests;

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinHandle, JoinSet};
use tokio::time::Instant;

use super::TrackerError;
use super::backoff::effective_interval;
use super::gate::{GATE_WAIT, GatePermit, HostGate, HostKey};
use super::peers::MAX_PEERS_PER_RESPONSE;
use super::state::{NotifiedTracker, Tracker, TrackerEvent, TrackerStatus};
use super::transport::{
    AnnounceEvent, AnnounceRequest, AnnounceStats, RedirectPolicy, TrackerTransport,
};
use super::url::{Scheme, TrackerUrl};

/// Capacity of the command channel.
pub const COMMAND_CAPACITY: usize = 8;

/// Capacity of the event channel.
pub const EVENT_CAPACITY: usize = 32;

/// Extra time the watchdog allows over the transport's own maximum.
pub const WATCHDOG_MARGIN: Duration = Duration::from_secs(5);

/// Total budget for the stop fan-out, across every notified tracker.
pub const STOP_BUDGET: Duration = Duration::from_secs(3);

/// Maximum peers retained while coalescing event batches.
///
/// This equals [`MAX_PEERS_PER_RESPONSE`]; a single batch is never truncated.
pub const MAX_QUEUED_PEERS: usize = MAX_PEERS_PER_RESPONSE;

/// Commands accepted by the tracker task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackerCommand {
    /// Queue `completed` and announce it as soon as possible.
    Complete,
    /// Send `stopped` to every notified tracker and finish.
    Stop,
    /// Replace the transfer counters used by the next announce.
    UpdateStats(AnnounceStats),
}

/// A running tracker task.
pub struct TrackerHandle {
    commands: mpsc::Sender<TrackerCommand>,
    status: Arc<std::sync::RwLock<TrackerStatus>>,
    events: mpsc::Receiver<TrackerEvent>,
    join: JoinHandle<()>,
}

impl TrackerHandle {
    /// Queues `completed`.
    ///
    /// Returns `false` when the command queue is full or the task already
    /// finished; the caller may retry.
    pub fn complete(&self) -> bool {
        self.commands.try_send(TrackerCommand::Complete).is_ok()
    }

    /// Queues `stopped`.
    ///
    /// Returns `false` when the command queue is full or the task already
    /// finished; the caller may retry.
    pub fn stop(&self) -> bool {
        self.commands.try_send(TrackerCommand::Stop).is_ok()
    }

    /// Replaces the transfer counters used by the next announce.
    ///
    /// Returns `false` when the command queue is full or the task already
    /// finished; the caller may retry.
    pub fn update_stats(&self, stats: AnnounceStats) -> bool {
        self.commands
            .try_send(TrackerCommand::UpdateStats(stats))
            .is_ok()
    }

    /// Returns a cloned status snapshot.
    pub fn status(&self) -> TrackerStatus {
        match self.status.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Returns the shared status snapshot handle.
    pub fn status_handle(&self) -> Arc<std::sync::RwLock<TrackerStatus>> {
        Arc::clone(&self.status)
    }

    /// Waits for the next event.
    ///
    /// Returns `None` once the task finished and every queued event was read.
    pub async fn next_event(&mut self) -> Option<TrackerEvent> {
        self.events.recv().await
    }

    /// Returns the next queued event without waiting.
    pub fn try_next_event(&mut self) -> Option<TrackerEvent> {
        self.events.try_recv().ok()
    }

    /// Waits for the task to finish.
    pub async fn join(&mut self) -> Result<(), JoinError> {
        (&mut self.join).await
    }

    /// Returns `true` once the task finished.
    pub fn is_finished(&self) -> bool {
        self.join.is_finished()
    }
}

/// Spawns the task that drives `tracker` until it is terminal.
///
/// `transport` performs one announce; `gate` serializes UDP attempts per
/// host and port; `stats` are the counters for the first request. The task
/// must be spawned inside a tokio runtime.
pub fn spawn(
    tracker: Tracker,
    transport: Arc<dyn TrackerTransport>,
    gate: Arc<HostGate>,
    stats: AnnounceStats,
) -> TrackerHandle {
    let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (event_tx, event_rx) = mpsc::channel(EVENT_CAPACITY);
    let status = tracker.status_handle();
    let join = tokio::spawn(run(tracker, transport, gate, stats, command_rx, event_tx));
    TrackerHandle {
        commands: command_tx,
        status,
        events: event_rx,
        join,
    }
}

async fn run(
    mut tracker: Tracker,
    transport: Arc<dyn TrackerTransport>,
    gate: Arc<HostGate>,
    mut stats: AnnounceStats,
    mut commands: mpsc::Receiver<TrackerCommand>,
    events: mpsc::Sender<TrackerEvent>,
) {
    let mut outbox = Outbox::new(events);

    loop {
        if tracker.is_terminal() {
            break;
        }
        if outbox.is_closed() {
            stop(&mut tracker, &transport, &gate, stats, &mut outbox).await;
            break;
        }

        let now = Instant::now();
        if let Some(request) = tracker.build_request(now, stats) {
            let outcome = attempt(&transport, &gate, request, &mut commands, &mut stats).await;
            let now = Instant::now();
            match outcome {
                AttemptOutcome::Done(result) => {
                    let events = match result {
                        Ok(response) => tracker.handle_response(now, response),
                        Err(error) => tracker.handle_failure(now, error),
                    };
                    outbox.push_all(events);
                }
                AttemptOutcome::Watchdog | AttemptOutcome::GateTimeout => {
                    outbox.push_all(tracker.handle_failure(now, TrackerError::Timeout));
                }
                AttemptOutcome::Command(command) => {
                    outbox.push_all(tracker.handle_cancel(now));
                    match command {
                        TrackerCommand::Complete => tracker.complete(now),
                        TrackerCommand::Stop => {
                            stop(&mut tracker, &transport, &gate, stats, &mut outbox).await;
                            break;
                        }
                        TrackerCommand::UpdateStats(new) => stats = new,
                    }
                }
                AttemptOutcome::Closed => {
                    outbox.push_all(tracker.handle_cancel(now));
                    stop(&mut tracker, &transport, &gate, stats, &mut outbox).await;
                    break;
                }
            }
            outbox.flush();
            continue;
        }

        let deadline = tracker.next_deadline();
        let has_pending = outbox.has_pending();
        tokio::select! {
            biased;
            command = commands.recv() => match command {
                Some(TrackerCommand::UpdateStats(new)) => stats = new,
                Some(TrackerCommand::Complete) => tracker.complete(Instant::now()),
                Some(TrackerCommand::Stop) | None => {
                    stop(&mut tracker, &transport, &gate, stats, &mut outbox).await;
                    break;
                }
            },
            _ = sleep_until_opt(deadline) => {}
            _ = outbox.send_next(), if has_pending => {}
        }
        outbox.flush();
    }

    outbox.flush_control().await;
}

/// One attempt's result.
enum AttemptOutcome {
    /// The transport returned.
    Done(Result<super::AnnounceResponse, TrackerError>),
    /// The transport exceeded its watchdog.
    Watchdog,
    /// The per-host gate was busy for the whole wait budget.
    GateTimeout,
    /// A command interrupted the attempt.
    Command(TrackerCommand),
    /// Every command sender was dropped.
    Closed,
}

/// One gate wait's result.
enum GateOutcome {
    /// The permit was acquired; it is released on drop.
    Acquired(GatePermit),
    /// The gate stayed busy for [`GATE_WAIT`].
    TimedOut,
    /// A command interrupted the wait.
    Command(TrackerCommand),
    /// Every command sender was dropped.
    Closed,
}

async fn attempt(
    transport: &Arc<dyn TrackerTransport>,
    gate: &Arc<HostGate>,
    request: AnnounceRequest,
    commands: &mut mpsc::Receiver<TrackerCommand>,
    stats: &mut AnnounceStats,
) -> AttemptOutcome {
    let event = request.event;
    let url = request.url.clone();
    let _permit = if url.scheme() == Scheme::Udp {
        match wait_for_gate(gate, HostKey::new(&url), commands, stats).await {
            GateOutcome::Acquired(permit) => Some(permit),
            GateOutcome::TimedOut => return AttemptOutcome::GateTimeout,
            GateOutcome::Command(command) => return AttemptOutcome::Command(command),
            GateOutcome::Closed => return AttemptOutcome::Closed,
        }
    } else {
        None
    };
    run_transport(transport, request, event, commands, stats).await
}

async fn wait_for_gate(
    gate: &Arc<HostGate>,
    key: HostKey,
    commands: &mut mpsc::Receiver<TrackerCommand>,
    stats: &mut AnnounceStats,
) -> GateOutcome {
    let wait = gate.acquire_for(key, GATE_WAIT);
    tokio::pin!(wait);
    loop {
        tokio::select! {
            biased;
            command = commands.recv() => match command {
                Some(TrackerCommand::UpdateStats(new)) => *stats = new,
                Some(command) => return GateOutcome::Command(command),
                None => return GateOutcome::Closed,
            },
            permit = &mut wait => return match permit {
                Some(permit) => GateOutcome::Acquired(permit),
                None => GateOutcome::TimedOut,
            },
        }
    }
}

async fn run_transport(
    transport: &Arc<dyn TrackerTransport>,
    request: AnnounceRequest,
    event: AnnounceEvent,
    commands: &mut mpsc::Receiver<TrackerCommand>,
    stats: &mut AnnounceStats,
) -> AttemptOutcome {
    let watchdog = watchdog_deadline(Instant::now(), transport.max_duration(event));
    let future = Guarded::new(transport.announce(request));
    tokio::pin!(future);
    loop {
        tokio::select! {
            biased;
            result = &mut future => return AttemptOutcome::Done(result),
            command = commands.recv() => match command {
                Some(TrackerCommand::UpdateStats(new)) => *stats = new,
                Some(command) => return AttemptOutcome::Command(command),
                None => return AttemptOutcome::Closed,
            },
            _ = sleep_until_opt(watchdog) => return AttemptOutcome::Watchdog,
        }
    }
}

/// Sends `stopped` to every notified tracker and marks the state terminal.
async fn stop(
    tracker: &mut Tracker,
    transport: &Arc<dyn TrackerTransport>,
    gate: &Arc<HostGate>,
    stats: AnnounceStats,
    outbox: &mut Outbox,
) {
    let notified = tracker.notified_trackers();
    let config = tracker.config().clone();
    let redirect = if tracker.is_private() {
        RedirectPolicy::SameHostOnly
    } else {
        RedirectPolicy::Public
    };

    let now = Instant::now();
    let deadline = now.checked_add(STOP_BUDGET).unwrap_or(now);
    let mut pending: HashSet<TrackerUrl> = notified.iter().map(|entry| entry.url.clone()).collect();
    let mut tasks = JoinSet::new();

    for NotifiedTracker { url, tracker_id } in notified {
        let request = AnnounceRequest::new(
            url.clone(),
            config.info_hash,
            config.peer_id,
            config.port,
            stats,
            AnnounceEvent::Stopped,
            config.key,
            tracker_id,
            redirect,
        );
        let transport = Arc::clone(transport);
        let gate = Arc::clone(gate);
        tasks.spawn(async move {
            let result = stopped_attempt(&transport, &gate, request).await;
            (url, result)
        });
    }

    let fan_out = async {
        while let Some(result) = tasks.join_next().await {
            if let Ok((url, result)) = result {
                pending.remove(&url);
                outbox.push_all(stopped_events(result));
            }
        }
    };

    // Dropping the future at the deadline aborts the remaining tasks. A task
    // panic leaves its URL in `pending`; it is reported as a timeout.
    let _expired = tokio::time::timeout_at(deadline, fan_out).await.is_err();
    for _ in 0..pending.len() {
        outbox.push(TrackerEvent::AnnounceFailed {
            error: TrackerError::Timeout,
            retry_at: None,
        });
    }

    tracker.mark_terminal();
    outbox.flush();
}

async fn stopped_attempt(
    transport: &Arc<dyn TrackerTransport>,
    gate: &Arc<HostGate>,
    request: AnnounceRequest,
) -> Result<super::AnnounceResponse, TrackerError> {
    let _permit = if request.url.scheme() == Scheme::Udp {
        match gate
            .acquire_for(HostKey::new(&request.url), GATE_WAIT)
            .await
        {
            Some(permit) => Some(permit),
            None => return Err(TrackerError::Timeout),
        }
    } else {
        None
    };

    let event = request.event;
    let watchdog = watchdog_deadline(Instant::now(), transport.max_duration(event));
    let future = Guarded::new(transport.announce(request));
    tokio::pin!(future);
    tokio::select! {
        biased;
        result = &mut future => result,
        _ = sleep_until_opt(watchdog) => Err(TrackerError::Timeout),
    }
}

fn stopped_events(result: Result<super::AnnounceResponse, TrackerError>) -> Vec<TrackerEvent> {
    match result {
        Ok(response) => vec![TrackerEvent::AnnounceSucceeded {
            interval: effective_interval(response.interval, response.min_interval),
        }],
        Err(error) => vec![TrackerEvent::AnnounceFailed {
            error,
            retry_at: None,
        }],
    }
}

fn watchdog_deadline(now: Instant, maximum: Duration) -> Option<Instant> {
    // If either sum is unrepresentable there is no watchdog; the transport's
    // own deadline remains the only bound. This needs an extreme clock.
    let total = maximum.checked_add(WATCHDOG_MARGIN)?;
    now.checked_add(total)
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending::<()>().await,
    }
}

type AnnounceFuture<'a> =
    Pin<Box<dyn Future<Output = Result<super::AnnounceResponse, TrackerError>> + Send + 'a>>;

/// Catches a panic raised while polling the transport future.
///
/// In `panic = "abort"` builds the panic aborts the process before the
/// catch can run; the wrapper is still compiled so the two profiles share
/// one code path.
struct Guarded<'a> {
    inner: AnnounceFuture<'a>,
}

impl<'a> Guarded<'a> {
    fn new(inner: AnnounceFuture<'a>) -> Self {
        Self { inner }
    }
}

impl Future for Guarded<'_> {
    type Output = Result<super::AnnounceResponse, TrackerError>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = &mut self.get_mut().inner;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            inner.as_mut().poll(context)
        })) {
            Ok(poll) => poll,
            Err(_) => Poll::Ready(Err(TrackerError::Panicked)),
        }
    }
}

/// Bounded event queue with data coalescing and control-event reservation.
struct Outbox {
    sender: mpsc::Sender<TrackerEvent>,
    pending: VecDeque<TrackerEvent>,
    closed: bool,
}

impl Outbox {
    fn new(sender: mpsc::Sender<TrackerEvent>) -> Self {
        Self {
            sender,
            pending: VecDeque::new(),
            closed: false,
        }
    }

    fn push(&mut self, event: TrackerEvent) {
        if !self.closed {
            self.pending.push_back(event);
        }
    }

    fn push_all(&mut self, events: Vec<TrackerEvent>) {
        for event in events {
            self.push(event);
        }
    }

    fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    fn is_closed(&self) -> bool {
        self.closed || self.sender.is_closed()
    }

    /// Sends as much as possible without waiting.
    ///
    /// Data events stop one slot short of full so a control event can always
    /// use the last slot. When the channel is full, the remaining data is
    /// coalesced before the caller waits for capacity.
    fn flush(&mut self) {
        if self.closed {
            self.pending.clear();
            return;
        }
        while let Some(event) = self.pending.front() {
            if !is_control(event) && self.sender.capacity() <= 1 {
                self.coalesce();
                break;
            }
            let Some(event) = self.pending.pop_front() else {
                break;
            };
            match self.sender.try_send(event) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(event)) => {
                    self.pending.push_front(event);
                    self.coalesce();
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    self.closed = true;
                    self.pending.clear();
                    break;
                }
            }
        }
    }

    /// Waits for capacity and sends the event at the front.
    ///
    /// Data waits for two slots, keeping one reserved for a control event.
    /// The future is cancel-safe: it pops the event only after the capacity
    /// is reserved.
    async fn send_next(&mut self) -> bool {
        if self.closed {
            return false;
        }
        let Some(event) = self.pending.front() else {
            return true;
        };
        if is_control(event) {
            match self.sender.reserve().await {
                Ok(permit) => {
                    if let Some(event) = self.pending.pop_front() {
                        permit.send(event);
                    }
                    true
                }
                Err(_) => {
                    self.closed = true;
                    self.pending.clear();
                    false
                }
            }
        } else {
            match self.sender.reserve_many(2).await {
                Ok(mut permits) => {
                    let permit = permits.next();
                    let event = self.pending.pop_front();
                    if let (Some(permit), Some(event)) = (permit, event) {
                        permit.send(event);
                    }
                    drop(permits);
                    true
                }
                Err(_) => {
                    self.closed = true;
                    self.pending.clear();
                    false
                }
            }
        }
    }

    /// Delivers every pending control event before the task exits.
    ///
    /// Data ahead of a control event is dropped: the tracker is terminal and
    /// the control event must not be lost. A consumer that never drains keeps
    /// the task alive here.
    async fn flush_control(&mut self) {
        if self.closed {
            return;
        }
        while let Some(index) = self.pending.iter().position(is_control) {
            self.pending.drain(..index);
            let Some(event) = self.pending.pop_front() else {
                break;
            };
            match self.sender.reserve().await {
                Ok(permit) => permit.send(event),
                Err(_) => {
                    self.closed = true;
                    self.pending.clear();
                    break;
                }
            }
        }
        self.flush();
    }

    /// Coalesces data between control events.
    ///
    /// Within a segment, only the latest status event is kept, peer batches
    /// are merged with deduplication up to [`MAX_QUEUED_PEERS`] (oldest
    /// batches first), and warnings keep their order. Control events split
    /// segments and are never moved, merged or dropped.
    fn coalesce(&mut self) {
        let mut result = VecDeque::new();
        let mut segment = Segment::default();
        for event in self.pending.drain(..) {
            match event {
                TrackerEvent::TrackerSwitched { .. } | TrackerEvent::NoUsableTracker => {
                    segment.flush(&mut result);
                    segment = Segment::default();
                    result.push_back(event);
                }
                TrackerEvent::AnnounceSucceeded { .. } | TrackerEvent::AnnounceFailed { .. } => {
                    segment.status = Some(event);
                }
                TrackerEvent::PeersReceived(peers) => segment.push_peers(peers),
                TrackerEvent::Warning(_) => segment.warnings.push(event),
            }
        }
        segment.flush(&mut result);
        self.pending = result;
    }
}

/// Data accumulated between two control events.
#[derive(Default)]
struct Segment {
    status: Option<TrackerEvent>,
    peers: VecDeque<Vec<SocketAddr>>,
    peer_count: usize,
    seen: HashSet<SocketAddr>,
    warnings: Vec<TrackerEvent>,
}

impl Segment {
    fn push_peers(&mut self, peers: Vec<SocketAddr>) {
        let mut unique = Vec::new();
        for peer in peers {
            if self.seen.insert(peer) {
                unique.push(peer);
            }
        }
        if unique.is_empty() {
            return;
        }
        self.peer_count = self.peer_count.saturating_add(unique.len());
        self.peers.push_back(unique);
        while self.peer_count > MAX_QUEUED_PEERS && self.peers.len() > 1 {
            if let Some(old) = self.peers.pop_front() {
                self.peer_count = self.peer_count.saturating_sub(old.len());
            }
        }
    }

    fn flush(self, out: &mut VecDeque<TrackerEvent>) {
        if let Some(status) = self.status {
            out.push_back(status);
        }
        if !self.peers.is_empty() {
            let peers: Vec<SocketAddr> = self.peers.into_iter().flatten().collect();
            out.push_back(TrackerEvent::PeersReceived(peers));
        }
        out.extend(self.warnings);
    }
}

fn is_control(event: &TrackerEvent) -> bool {
    matches!(
        event,
        TrackerEvent::TrackerSwitched { .. } | TrackerEvent::NoUsableTracker
    )
}
