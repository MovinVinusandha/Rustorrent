//! SHA-1 verification of completed pieces.
//!
//! [`HashPool`] hashes completed piece buffers on Tokio's blocking pool and
//! caps how many hash jobs run at once. The cap is enforced with a semaphore
//! whose permit moves into the blocking closure, so it covers actual hashing,
//! and the [`PieceBuffer`] moves with it. Dropping the outer future therefore
//! cannot leak the piece's memory reservation: the buffer (and its
//! [`PieceReservation`](crate::piece::PieceReservation)) is owned by the
//! blocking job and dropped when that job ends.
//!
//! Only a matching SHA-1 produces a [`VerifiedPiece`], and the type has no
//! public constructor. A mismatch returns the sorted, deduplicated contributor
//! list for later statistical blame; the engine re-requests the whole piece
//! and no peer is banned here.

use std::fmt;
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;

use sha1::{Digest, Sha1};
use thiserror::Error;
use tokio::sync::{AcquireError, Semaphore};
use tokio::task::JoinError;

use super::{PeerKey, PieceBuffer, PieceReservation, PieceSpec};
use crate::metainfo::PIECE_HASH_LENGTH;

/// Default cap on concurrent SHA-1 jobs.
pub const DEFAULT_MAX_HASH_JOBS: usize = 2;

/// Capped pool that verifies completed pieces on the blocking pool.
pub struct HashPool {
    permits: Arc<Semaphore>,
    max_jobs: usize,
    #[cfg(test)]
    gate: Mutex<Option<Arc<HashGate>>>,
}

impl HashPool {
    /// Creates a pool capped at [`DEFAULT_MAX_HASH_JOBS`].
    pub fn new() -> Self {
        Self {
            permits: Arc::new(Semaphore::new(DEFAULT_MAX_HASH_JOBS)),
            max_jobs: DEFAULT_MAX_HASH_JOBS,
            #[cfg(test)]
            gate: Mutex::new(None),
        }
    }

    /// Creates a pool capped at `max_jobs` concurrent SHA-1 jobs.
    ///
    /// A zero cap is rejected. The cap is a hard limit: jobs wait for a permit
    /// before they are handed to the blocking pool.
    pub fn with_max_jobs(max_jobs: usize) -> Result<Self, HashPoolError> {
        if max_jobs == 0 {
            return Err(HashPoolError::ZeroMaxJobs);
        }
        if max_jobs > Semaphore::MAX_PERMITS {
            return Err(HashPoolError::TooManyJobs {
                requested: max_jobs,
                max: Semaphore::MAX_PERMITS,
            });
        }
        Ok(Self {
            permits: Arc::new(Semaphore::new(max_jobs)),
            max_jobs,
            #[cfg(test)]
            gate: Mutex::new(None),
        })
    }

    /// Configured cap on concurrent hash jobs.
    pub fn max_jobs(&self) -> usize {
        self.max_jobs
    }

    /// Verifies one complete piece and returns it only when the SHA-1 matches.
    ///
    /// An incomplete buffer is rejected before a permit is taken, so it never
    /// occupies a hash slot. On every outcome the buffer is consumed and its
    /// memory reservation is released, except on success, where the returned
    /// [`VerifiedPiece`] keeps the reservation until the caller drops it.
    ///
    /// Cancelling this future while it waits for a permit drops the buffer and
    /// releases the reservation. Cancelling it after the blocking job started
    /// detaches the job; the job still owns the buffer and permit and releases
    /// both when it finishes.
    pub async fn verify(&self, buffer: PieceBuffer) -> Result<VerifiedPiece, VerifyError> {
        if !buffer.is_complete() {
            return Err(VerifyError::Incomplete {
                piece: buffer.piece(),
                received: buffer.received_blocks(),
                blocks: buffer.block_count(),
            });
        }

        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_: AcquireError| VerifyError::PoolClosed)?;

        #[cfg(test)]
        let gate = self.gate.lock().expect("hash gate lock").clone();

        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            #[cfg(test)]
            if let Some(gate) = gate {
                gate.enter();
            }
            verify_buffer(buffer)
        })
        .await
        .map_err(VerifyError::TaskFailed)?
    }

    /// Hash jobs that can start immediately.
    ///
    /// This is an admission hint only; a job still has to win the permit.
    #[cfg(test)]
    fn available_jobs(&self) -> usize {
        self.permits.available_permits()
    }

    #[cfg(test)]
    fn set_test_gate(&self, gate: Arc<HashGate>) {
        *self.gate.lock().expect("hash gate lock") = Some(gate);
    }
}

impl Default for HashPool {
    fn default() -> Self {
        Self::new()
    }
}

/// Runs on the blocking pool with the buffer and permit already moved in.
fn verify_buffer(buffer: PieceBuffer) -> Result<VerifiedPiece, VerifyError> {
    let spec = *buffer.spec();
    let Some(bytes) = buffer.complete_bytes() else {
        // `verify` checked completeness before spawning, and the buffer cannot
        // change once it is owned by this closure. Handled instead of asserted
        // because a panic would abort release builds.
        return Err(VerifyError::Incomplete {
            piece: buffer.piece(),
            received: buffer.received_blocks(),
            blocks: buffer.block_count(),
        });
    };

    let digest = Sha1::digest(bytes);
    let mut found = [0u8; PIECE_HASH_LENGTH];
    found.copy_from_slice(&digest);

    if found == spec.hash() {
        let (spec, data, reservation) = buffer.into_parts();
        Ok(VerifiedPiece {
            spec,
            data,
            reservation,
        })
    } else {
        Err(VerifyError::HashMismatch {
            piece: spec.index(),
            expected: spec.hash(),
            found,
            contributors: buffer.contributors(),
        })
    }
}

/// A completed piece whose bytes matched the metainfo SHA-1.
///
/// No public constructor exists: only [`HashPool::verify`] success builds one.
/// The piece keeps its memory reservation until it is dropped, including while
/// [`store_verified`](crate::piece::store_verified) writes it.
pub struct VerifiedPiece {
    spec: PieceSpec,
    data: Box<[u8]>,
    reservation: PieceReservation,
}

impl VerifiedPiece {
    /// Piece metadata, including the expected hash.
    pub fn spec(&self) -> &PieceSpec {
        &self.spec
    }

    /// Piece index within the torrent.
    pub fn piece(&self) -> u32 {
        self.spec.index()
    }

    /// Byte offset in the torrent stream.
    pub fn offset(&self) -> u64 {
        self.spec.offset()
    }

    /// Actual piece length in bytes; the buffer is exactly this long.
    pub fn length(&self) -> usize {
        self.spec.length()
    }

    /// Bytes currently reserved by this piece.
    pub fn reserved_bytes(&self) -> usize {
        self.reservation.bytes()
    }

    /// Verified piece bytes, exactly [`VerifiedPiece::length`] long.
    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

impl fmt::Debug for VerifiedPiece {
    /// Shows the piece shape, never the buffered bytes.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VerifiedPiece")
            .field("index", &self.spec.index())
            .field("offset", &self.spec.offset())
            .field("length", &self.spec.length())
            .finish_non_exhaustive()
    }
}

/// Errors returned while creating a [`HashPool`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum HashPoolError {
    /// The requested cap is zero.
    #[error("hash pool needs at least one job slot")]
    ZeroMaxJobs,
    /// The requested cap exceeds the semaphore limit.
    #[error("hash job cap {requested} exceeds the maximum {max}")]
    TooManyJobs {
        /// Requested cap.
        requested: usize,
        /// Maximum accepted cap.
        max: usize,
    },
}

/// Errors returned by [`HashPool::verify`].
#[derive(Debug, Error)]
pub enum VerifyError {
    /// The piece is missing at least one block.
    #[error("piece {piece} is incomplete: {received} of {blocks} blocks")]
    Incomplete {
        /// Piece index.
        piece: u32,
        /// Blocks received so far.
        received: u32,
        /// Blocks in the piece.
        blocks: u32,
    },
    /// The SHA-1 of the assembled bytes does not match the metainfo hash.
    #[error("piece {piece} SHA-1 does not match the metainfo hash")]
    HashMismatch {
        /// Piece index.
        piece: u32,
        /// Hash from metainfo.
        expected: [u8; PIECE_HASH_LENGTH],
        /// Hash of the assembled bytes.
        found: [u8; PIECE_HASH_LENGTH],
        /// Sorted, deduplicated peers that supplied blocks.
        contributors: Vec<PeerKey>,
    },
    /// The pool's semaphore was closed; this only happens during shutdown.
    #[error("hash pool is closed")]
    PoolClosed,
    /// The blocking hash task panicked or was cancelled by the runtime.
    #[error("piece hashing task failed")]
    TaskFailed(#[source] JoinError),
}

/// Test-only gate that holds hash jobs so concurrency is observable without
/// sleeps. It is compiled out of release builds.
#[cfg(test)]
pub(crate) struct HashGate {
    entered: tokio::sync::mpsc::UnboundedSender<()>,
    release: Mutex<std::sync::mpsc::Sender<()>>,
    waiters: Mutex<std::sync::mpsc::Receiver<()>>,
    state: Mutex<GateState>,
    panic: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
#[derive(Default)]
struct GateState {
    active: usize,
    peak: usize,
    open: bool,
}

#[cfg(test)]
impl HashGate {
    /// Creates a closed gate and the receiver that reports each entry.
    pub(crate) fn new() -> (Arc<Self>, tokio::sync::mpsc::UnboundedReceiver<()>) {
        let (entered, received) = tokio::sync::mpsc::unbounded_channel();
        let (release, waiters) = std::sync::mpsc::channel();
        (
            Arc::new(Self {
                entered,
                release: Mutex::new(release),
                waiters: Mutex::new(waiters),
                state: Mutex::new(GateState::default()),
                panic: std::sync::atomic::AtomicBool::new(false),
            }),
            received,
        )
    }

    /// Signals entry and blocks until the gate is opened or a token arrives.
    fn enter(&self) {
        {
            let mut state = self.state.lock().expect("hash gate state");
            state.active = state.active.saturating_add(1);
            state.peak = state.peak.max(state.active);
        }
        let _ = self.entered.send(());
        if self.panic.load(std::sync::atomic::Ordering::Acquire) {
            panic!("injected hash task panic");
        }
        let open = self.state.lock().expect("hash gate state").open;
        if !open {
            let waiters = self.waiters.lock().expect("hash gate waiters");
            let _ = waiters.recv();
        }
        let mut state = self.state.lock().expect("hash gate state");
        state.active = state.active.saturating_sub(1);
    }

    /// Highest number of jobs that entered the gate at once.
    pub(crate) fn peak(&self) -> usize {
        self.state.lock().expect("hash gate state").peak
    }

    /// Releases every current and future entry.
    pub(crate) fn open(&self) {
        self.state.lock().expect("hash gate state").open = true;
        let release = self.release.lock().expect("hash gate release");
        for _ in 0..1024 {
            let _ = release.send(());
        }
    }

    /// Makes the next entry panic after reporting itself.
    pub(crate) fn panic_on_enter(&self) {
        self.panic.store(true, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
mod tests;
