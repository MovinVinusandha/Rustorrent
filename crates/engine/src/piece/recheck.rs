//! Streaming recheck of on-disk piece data.
//!
//! A recheck reads existing bytes through [`StorageHandle::read_range`] and
//! hashes them against the metainfo hashes. It never holds a whole piece or a
//! whole file in memory: data is read in fixed [`RECHECK_CHUNK_LEN`] chunks,
//! and up to [`RECHECK_CHUNKS_PER_JOB`] chunks are hashed by one blocking job
//! with an incremental SHA-1 state.
//!
//! The result is a full [`Bitfield`] returned only after the run finishes.
//! Selected pieces start clear, so a damaged piece loses any bit it had in the
//! base bitfield; unselected pieces keep their base bits. A missing or short
//! read means the piece is absent, not an error. Other storage errors stop the
//! run and are returned as typed [`RecheckError::Storage`] values.
//!
//! Only one recheck can run at a time for one [`Rechecker`]. Recheck is
//! exclusive with piece writes for the same torrent: the caller must not run
//! [`store_verified`](crate::piece::store_verified) concurrently. Other
//! torrents continue to use the shared storage worker and hash pool.

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use sha1::{Digest, Sha1};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::task::JoinError;

use super::{HashPool, PieceSpec, PieceSpecError};
use crate::bitfield::{Bitfield, BitfieldError};
use crate::metainfo::{MetaInfo, PIECE_HASH_LENGTH};
use crate::storage::layout::FileMap;
use crate::storage::thread::{MAX_RANGE_LEN, StorageHandle, StorageHandleError};

/// Bytes read from storage in one recheck read.
///
/// One quarter of [`MAX_RANGE_LEN`], so a read reply is small and four chunks
/// fit in one hash job without exceeding the storage worker's range limit.
pub const RECHECK_CHUNK_LEN: usize = 256 * 1024;

/// Number of chunks handed to one blocking hash job.
pub const RECHECK_CHUNKS_PER_JOB: usize = 4;

/// Maximum bytes in one recheck hash batch.
///
/// This is [`RECHECK_CHUNK_LEN`] times [`RECHECK_CHUNKS_PER_JOB`] and stays at
/// or below [`MAX_RANGE_LEN`], so a recheck never allocates a piece-sized
/// buffer.
pub const RECHECK_BATCH_LEN: usize = RECHECK_CHUNK_LEN * RECHECK_CHUNKS_PER_JOB;

const _: () = assert!(RECHECK_CHUNK_LEN <= MAX_RANGE_LEN);
const _: () = assert!(RECHECK_BATCH_LEN <= MAX_RANGE_LEN);

/// Cooperative cancellation flag for a recheck.
///
/// Clones share one flag. Cancellation is checked before each chunk read and
/// before each hash job, so a run that observes it stops within one batch and
/// returns [`RecheckError::Cancelled`] without publishing a bitfield.
#[derive(Debug, Clone, Default)]
pub struct RecheckCancel {
    cancelled: Arc<AtomicBool>,
}

impl RecheckCancel {
    /// Creates a flag that is not cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation. Calling this more than once is harmless.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Returns `true` once cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Pieces selected for one recheck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecheckSelection<'a> {
    /// Check every piece in the torrent.
    All,
    /// Check only these pieces.
    ///
    /// Indices must be strictly increasing and inside the torrent. An empty
    /// slice returns a clone of the base bitfield without reading anything.
    Only(&'a [u32]),
}

impl<'a> RecheckSelection<'a> {
    /// Number of selected pieces.
    fn total(self, piece_count: u32) -> Result<u32, RecheckError> {
        match self {
            Self::All => Ok(piece_count),
            Self::Only(pieces) => u32::try_from(pieces.len())
                .map_err(|_| RecheckError::SelectionTooLong { len: pieces.len() }),
        }
    }

    /// Iterates the selected piece indices.
    fn iter(self, piece_count: u32) -> SelectionIter<'a> {
        match self {
            Self::All => SelectionIter::All(0..piece_count),
            Self::Only(pieces) => SelectionIter::Only(pieces.iter()),
        }
    }
}

/// Iterator over a recheck selection.
enum SelectionIter<'a> {
    All(Range<u32>),
    Only(std::slice::Iter<'a, u32>),
}

impl Iterator for SelectionIter<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::All(pieces) => pieces.next(),
            Self::Only(pieces) => pieces.next().copied(),
        }
    }
}

/// Result of a completed recheck.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecheckOutcome {
    bits: Bitfield,
    checked: u32,
    present: u32,
    absent: u32,
}

impl RecheckOutcome {
    /// Bitfield with every selected piece's result applied.
    ///
    /// Unselected pieces keep the bits they had in the base bitfield.
    pub fn bits(&self) -> &Bitfield {
        &self.bits
    }

    /// Consumes the outcome and returns its bitfield.
    pub fn into_bits(self) -> Bitfield {
        self.bits
    }

    /// Number of selected pieces that were checked.
    pub fn checked(&self) -> u32 {
        self.checked
    }

    /// Number of checked pieces whose SHA-1 matched the metainfo.
    pub fn present(&self) -> u32 {
        self.present
    }

    /// Number of checked pieces that were absent or damaged.
    pub fn absent(&self) -> u32 {
        self.absent
    }
}

/// Advisory snapshot sent after each selected piece finishes.
///
/// Updates are sent with `try_send`, so a full channel drops an update,
/// including the final one. The returned [`RecheckOutcome`] is the
/// authoritative result; progress is only a hint for a front-end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecheckProgress {
    piece: u32,
    checked: u32,
    present: u32,
    absent: u32,
    total: u32,
}

impl RecheckProgress {
    /// Index of the piece that just finished.
    pub fn piece(&self) -> u32 {
        self.piece
    }

    /// Number of selected pieces finished so far.
    pub fn checked(&self) -> u32 {
        self.checked
    }

    /// Number of finished pieces that matched the metainfo hash.
    pub fn present(&self) -> u32 {
        self.present
    }

    /// Number of finished pieces that were absent or damaged.
    pub fn absent(&self) -> u32 {
        self.absent
    }

    /// Number of selected pieces in this run.
    pub fn total(&self) -> u32 {
        self.total
    }
}

/// Per-torrent streaming recheck.
///
/// A `Rechecker` owns the torrent's storage handle, file map, metainfo, and a
/// shared hash pool. Calling [`Rechecker::run`] while another run is active on
/// the same value returns [`RecheckError::AlreadyRunning`].
pub struct Rechecker {
    storage: StorageHandle,
    map: Arc<FileMap>,
    meta: Arc<MetaInfo>,
    pool: Arc<HashPool>,
    running: AtomicBool,
}

impl Rechecker {
    /// Creates a rechecker for one torrent.
    ///
    /// The map and metainfo must agree on piece length, total length, and hash
    /// count.
    pub fn new(
        storage: StorageHandle,
        map: Arc<FileMap>,
        meta: Arc<MetaInfo>,
        pool: Arc<HashPool>,
    ) -> Result<Self, RecheckError> {
        validate_torrent(map.as_ref(), meta.as_ref())?;
        Ok(Self {
            storage,
            map,
            meta,
            pool,
            running: AtomicBool::new(false),
        })
    }

    /// Rechecks the selected pieces and returns the updated bitfield.
    ///
    /// The base bitfield must have the torrent's piece count. Its selected bits
    /// are cleared before checking and set again only after a hash match;
    /// unselected bits are preserved. A selected piece whose data is missing,
    /// truncated, or does not match the metainfo hash stays clear and counts as
    /// absent. Nothing is published if this returns an error.
    ///
    /// Cancellation is checked before each chunk read and before each hash job.
    /// A cancelled run stops within one batch and returns
    /// [`RecheckError::Cancelled`]; dropping the future releases the run's
    /// lease at its next await. Neither publishes a bitfield.
    ///
    /// When `progress` is set, one update is sent with `try_send` after each
    /// selected piece. Updates are advisory: a full channel drops them,
    /// including the final one, so the returned outcome is the result.
    pub async fn run(
        &self,
        selection: RecheckSelection<'_>,
        base: &Bitfield,
        cancel: &RecheckCancel,
        progress: Option<&mpsc::Sender<RecheckProgress>>,
    ) -> Result<RecheckOutcome, RecheckError> {
        let _lease = self.acquire()?;

        if base.piece_count() != self.map.piece_count() {
            return Err(RecheckError::PieceCountMismatch {
                expected: self.map.piece_count(),
                found: base.piece_count(),
            });
        }

        validate_selection(selection, self.map.piece_count())?;
        let total = selection.total(self.map.piece_count())?;
        let mut bits = base.clone();
        if total == 0 {
            return Ok(RecheckOutcome {
                bits,
                checked: 0,
                present: 0,
                absent: 0,
            });
        }
        if cancel.is_cancelled() {
            return Err(RecheckError::Cancelled);
        }

        let mut checked = 0u32;
        let mut present = 0u32;

        for piece in selection.iter(self.map.piece_count()) {
            if cancel.is_cancelled() {
                return Err(RecheckError::Cancelled);
            }

            let spec = PieceSpec::from_metainfo(self.map.as_ref(), self.meta.as_ref(), piece)?;

            // A selected piece is unknown until the whole piece matches. Clear
            // its base bit now so a mismatch, short read, or cancellation
            // cannot leave a stale verified bit in the result.
            bits.clear_piece(piece)?;

            let mut hasher = Sha1::new();
            let mut position = spec.offset();
            let mut remaining = spec.length();
            let mut complete = true;

            'piece: while remaining > 0 {
                let batch_len = remaining.min(RECHECK_BATCH_LEN);
                let mut batch = Vec::with_capacity(batch_len);

                while batch.len() < batch_len {
                    if cancel.is_cancelled() {
                        return Err(RecheckError::Cancelled);
                    }

                    let want = (batch_len - batch.len()).min(RECHECK_CHUNK_LEN);
                    let read = self
                        .storage
                        .read_range(position, want)
                        .await
                        .map_err(|source| RecheckError::Storage {
                            piece,
                            offset: position,
                            source,
                        })?;

                    if read.len() > want {
                        return Err(RecheckError::ReadOverrun {
                            piece,
                            offset: position,
                            requested: want,
                            got: read.len(),
                        });
                    }
                    if read.len() < want {
                        // A missing or truncated file means the piece is
                        // absent. Abandon it on the first short read: later
                        // chunks cannot make it match, and its cleared bit
                        // must stay clear.
                        complete = false;
                        break 'piece;
                    }

                    batch.extend_from_slice(&read);
                    let read_len =
                        u64::try_from(want).map_err(|_| RecheckError::OffsetOverflow {
                            piece,
                            offset: position,
                        })?;
                    position =
                        position
                            .checked_add(read_len)
                            .ok_or(RecheckError::OffsetOverflow {
                                piece,
                                offset: position,
                            })?;
                    remaining -= want;
                }

                if cancel.is_cancelled() {
                    return Err(RecheckError::Cancelled);
                }

                hasher = self
                    .pool
                    .run_job(move || {
                        hasher.update(&batch);
                        hasher
                    })
                    .await
                    .map_err(RecheckError::HashTask)?;
            }

            checked += 1;

            if complete {
                let digest = hasher.finalize();
                let mut found = [0u8; PIECE_HASH_LENGTH];
                found.copy_from_slice(&digest);

                if found == spec.hash() {
                    bits.mark_rechecked_present(piece)?;
                    present += 1;
                }
            }

            if let Some(progress) = progress {
                let _ = progress.try_send(RecheckProgress {
                    piece,
                    checked,
                    present,
                    absent: checked - present,
                    total,
                });
            }
        }

        Ok(RecheckOutcome {
            bits,
            checked,
            present,
            absent: checked - present,
        })
    }

    fn acquire(&self) -> Result<RunningGuard<'_>, RecheckError> {
        if self
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(RecheckError::AlreadyRunning);
        }
        Ok(RunningGuard {
            running: &self.running,
        })
    }
}

/// Clears the running flag when a recheck exits or its future is dropped.
struct RunningGuard<'a> {
    running: &'a AtomicBool,
}

impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
    }
}

/// Rejects selections that are out of range or not strictly increasing.
fn validate_selection(
    selection: RecheckSelection<'_>,
    piece_count: u32,
) -> Result<(), RecheckError> {
    let RecheckSelection::Only(pieces) = selection else {
        return Ok(());
    };

    let mut previous = None;
    for &piece in pieces {
        if piece >= piece_count {
            return Err(RecheckError::SelectionOutOfRange { piece, piece_count });
        }
        if let Some(previous) = previous
            && piece <= previous
        {
            return Err(RecheckError::SelectionNotSorted {
                previous,
                current: piece,
            });
        }
        previous = Some(piece);
    }
    Ok(())
}

/// Checks that the map and metainfo describe the same torrent.
fn validate_torrent(map: &FileMap, meta: &MetaInfo) -> Result<(), RecheckError> {
    if meta.piece_length != map.piece_length() {
        return Err(PieceSpecError::PieceLengthMismatch {
            map: map.piece_length(),
            metainfo: meta.piece_length,
        }
        .into());
    }
    if meta.total_length != map.total_length() {
        return Err(PieceSpecError::TotalLengthMismatch {
            map: map.total_length(),
            metainfo: meta.total_length,
        }
        .into());
    }

    let expected =
        usize::try_from(map.piece_count()).map_err(|_| PieceSpecError::CountOverflow {
            value: u64::from(map.piece_count()),
        })?;
    if meta.piece_hashes.len() != expected {
        return Err(PieceSpecError::HashCountMismatch {
            expected: map.piece_count(),
            found: meta.piece_hashes.len(),
        }
        .into());
    }
    Ok(())
}

/// Errors returned by a recheck.
#[derive(Debug, Error)]
pub enum RecheckError {
    /// Another recheck is already running on this `Rechecker`.
    #[error("a recheck is already running for this torrent")]
    AlreadyRunning,
    /// The run observed a cancelled [`RecheckCancel`] and stopped.
    #[error("recheck was cancelled")]
    Cancelled,
    /// The base bitfield does not match the torrent's piece count.
    #[error("bitfield has {found} pieces, torrent has {expected}")]
    PieceCountMismatch {
        /// Torrent piece count.
        expected: u32,
        /// Bitfield piece count.
        found: u32,
    },
    /// A selected piece index is outside the torrent.
    #[error("piece {piece} is outside a {piece_count}-piece torrent")]
    SelectionOutOfRange {
        /// Requested piece index.
        piece: u32,
        /// Number of pieces in the torrent.
        piece_count: u32,
    },
    /// Selected piece indices are not strictly increasing.
    #[error("selection is not strictly increasing: {previous} then {current}")]
    SelectionNotSorted {
        /// Previous index.
        previous: u32,
        /// Current index.
        current: u32,
    },
    /// The selection slice is too long to represent as a piece count.
    #[error("selection contains {len} indices, more than u32::MAX")]
    SelectionTooLong {
        /// Slice length.
        len: usize,
    },
    /// A piece spec could not be built from the map and metainfo.
    #[error(transparent)]
    Spec(#[from] PieceSpecError),
    /// Storage returned an error other than a short read.
    #[error("piece {piece} read at offset {offset} failed")]
    Storage {
        /// Piece being checked.
        piece: u32,
        /// Absolute torrent offset of the failed read.
        offset: u64,
        /// Storage error.
        #[source]
        source: StorageHandleError,
    },
    /// Storage returned more bytes than requested.
    #[error(
        "piece {piece} read at offset {offset} returned {got} bytes, expected at most {requested}"
    )]
    ReadOverrun {
        /// Piece being checked.
        piece: u32,
        /// Absolute torrent offset of the read.
        offset: u64,
        /// Bytes requested.
        requested: usize,
        /// Bytes returned.
        got: usize,
    },
    /// A read offset overflowed `u64`.
    #[error("piece {piece} read offset overflowed at {offset}")]
    OffsetOverflow {
        /// Piece being checked.
        piece: u32,
        /// Offset that overflowed.
        offset: u64,
    },
    /// The blocking hash task failed.
    #[error("piece hashing task failed")]
    HashTask(#[source] JoinError),
    /// A bitfield operation failed.
    #[error(transparent)]
    Bitfield(#[from] BitfieldError),
}

#[cfg(test)]
mod tests;
