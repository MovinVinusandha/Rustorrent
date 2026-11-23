//! Byte budget for piece buffers.
//!
//! A session owns one [`MemoryBudget`] and each torrent gets a
//! [`TorrentBudget`] derived from its piece length. A caller reserves the
//! actual piece length before allocating a buffer; the returned
//! [`PieceReservation`] releases both the per-torrent slot and the global bytes
//! when it is dropped, on every path: successful write, hash failure,
//! cancellation, stall expiry, or drop.
//!
//! Fairness: a torrent acquires a per-torrent slot before it waits for global
//! bytes, so it can have at most `max_in_flight` waiters on the global
//! semaphore. Tokio's semaphore is FIFO, so a torrent that is already waiting
//! is served before later requests and cannot jump the queue by re-requesting.
//! A reservation acquires all of its bytes atomically and never waits for a
//! second resource while holding bytes. A single piece can therefore never
//! deadlock the budget: the largest accepted piece is `MAX_PIECE_LENGTH`
//! (64 MiB), it fits in the default 128 MiB global budget, and a global budget
//! smaller than one piece is rejected when the torrent budget is created.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use thiserror::Error;
use tokio::sync::{AcquireError, OwnedSemaphorePermit, Semaphore, TryAcquireError};

use crate::metainfo::MAX_PIECE_LENGTH;

/// Default global piece-buffer budget: 128 MiB.
pub const DEFAULT_GLOBAL_BUDGET: u64 = 128 * 1024 * 1024;

/// Maximum per-torrent piece-buffer share: 64 MiB.
pub const MAX_TORRENT_SHARE: u64 = 64 * 1024 * 1024;

/// Errors returned by the piece memory budget.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BudgetError {
    /// The global budget is zero.
    #[error("piece buffer budget must be greater than zero")]
    ZeroCapacity,
    /// The global budget does not fit the platform or the semaphore limit.
    #[error("piece buffer budget {capacity} is too large")]
    CapacityTooLarge {
        /// Requested capacity in bytes.
        capacity: u64,
    },
    /// A torrent declared a zero piece length.
    #[error("piece length must be greater than zero")]
    ZeroPieceLength,
    /// A torrent declared a piece length above the metainfo limit.
    #[error("piece length {piece_length} exceeds the maximum {max}")]
    PieceTooLarge {
        /// Declared piece length in bytes.
        piece_length: u64,
        /// Maximum accepted piece length.
        max: u64,
    },
    /// The global budget cannot hold one piece of this torrent.
    #[error("global budget {global} cannot hold a {piece_length}-byte piece")]
    GlobalTooSmall {
        /// Declared piece length in bytes.
        piece_length: u64,
        /// Global budget in bytes.
        global: u64,
    },
    /// A reservation was requested with zero bytes.
    #[error("piece reservation must be greater than zero bytes")]
    ZeroLength,
    /// A reservation is larger than this torrent's share.
    #[error("reservation of {requested} bytes exceeds the per-torrent share {share}")]
    ExceedsShare {
        /// Requested bytes.
        requested: usize,
        /// Per-torrent share in bytes.
        share: u64,
    },
    /// A reservation is larger than the global budget.
    #[error("reservation of {requested} bytes exceeds the global budget {global}")]
    ExceedsGlobal {
        /// Requested bytes.
        requested: usize,
        /// Global budget in bytes.
        global: u64,
    },
    /// A computed value does not fit the platform representation.
    #[error("piece budget value {value} does not fit this platform")]
    CountOverflow {
        /// Value that overflowed.
        value: u64,
    },
    /// A non-blocking reservation could not acquire permits immediately.
    #[error("piece budget is exhausted")]
    WouldBlock,
    /// The budget's semaphore was closed.
    #[error("piece budget is closed")]
    Closed,
}

/// Session-wide byte budget for piece buffers.
pub struct MemoryBudget {
    capacity: u64,
    bytes: Arc<Semaphore>,
    in_use: Arc<AtomicUsize>,
}

impl MemoryBudget {
    /// Creates a budget with `capacity` global bytes.
    pub fn new(capacity: u64) -> Result<Self, BudgetError> {
        if capacity == 0 {
            return Err(BudgetError::ZeroCapacity);
        }
        let capacity_usize =
            usize::try_from(capacity).map_err(|_| BudgetError::CapacityTooLarge { capacity })?;
        if capacity_usize > Semaphore::MAX_PERMITS {
            return Err(BudgetError::CapacityTooLarge { capacity });
        }

        Ok(Self {
            capacity,
            bytes: Arc::new(Semaphore::new(capacity_usize)),
            in_use: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Global capacity in bytes.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Bytes currently reserved across all torrents.
    pub fn in_use_bytes(&self) -> u64 {
        self.in_use.load(Ordering::Relaxed) as u64
    }

    /// Bytes still available across all torrents.
    pub fn available_bytes(&self) -> u64 {
        self.capacity.saturating_sub(self.in_use_bytes())
    }

    /// Derives the per-torrent budget for `piece_length`.
    ///
    /// The share is `min(2 * piece_length, 64 MiB)` and the number of
    /// concurrent buffers is `max(1, share / piece_length)`. A piece length
    /// larger than the global budget is rejected here, before any request can
    /// wait forever.
    pub fn per_torrent(&self, piece_length: u64) -> Result<TorrentBudget, BudgetError> {
        if piece_length == 0 {
            return Err(BudgetError::ZeroPieceLength);
        }
        if piece_length > MAX_PIECE_LENGTH {
            return Err(BudgetError::PieceTooLarge {
                piece_length,
                max: MAX_PIECE_LENGTH,
            });
        }
        if piece_length > self.capacity {
            return Err(BudgetError::GlobalTooSmall {
                piece_length,
                global: self.capacity,
            });
        }

        let share = piece_length.saturating_mul(2).min(MAX_TORRENT_SHARE);
        let slots = (share / piece_length).max(1);
        let slot_count =
            usize::try_from(slots).map_err(|_| BudgetError::CountOverflow { value: slots })?;

        Ok(TorrentBudget {
            global: Arc::clone(&self.bytes),
            global_capacity: self.capacity,
            global_in_use: Arc::clone(&self.in_use),
            torrent_in_use: Arc::new(AtomicUsize::new(0)),
            slots: Arc::new(Semaphore::new(slot_count)),
            slot_count,
            share,
            piece_length,
        })
    }
}

/// Per-torrent cap on piece buffers, sharing one [`MemoryBudget`].
#[derive(Clone)]
pub struct TorrentBudget {
    global: Arc<Semaphore>,
    global_capacity: u64,
    global_in_use: Arc<AtomicUsize>,
    torrent_in_use: Arc<AtomicUsize>,
    slots: Arc<Semaphore>,
    slot_count: usize,
    share: u64,
    piece_length: u64,
}

impl TorrentBudget {
    /// Waits for a torrent slot and then for global bytes.
    ///
    /// The reservation releases both when dropped. Cancelling this future
    /// releases a slot if it was already acquired and no bytes otherwise.
    pub async fn reserve(&self, bytes: usize) -> Result<PieceReservation, BudgetError> {
        let permits = self.check_reservation(bytes)?;

        let slot = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_: AcquireError| BudgetError::Closed)?;
        let global = self
            .global
            .clone()
            .acquire_many_owned(permits)
            .await
            .map_err(|_: AcquireError| BudgetError::Closed)?;

        Ok(self.finish_reservation(bytes, slot, global))
    }

    /// Tries to reserve without waiting.
    ///
    /// Returns [`BudgetError::WouldBlock`] when either the per-torrent slot or
    /// the global bytes are unavailable. A failed attempt releases any slot it
    /// acquired first.
    pub fn try_reserve(&self, bytes: usize) -> Result<PieceReservation, BudgetError> {
        let permits = self.check_reservation(bytes)?;

        let slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(map_try_error)?;
        let global = self
            .global
            .clone()
            .try_acquire_many_owned(permits)
            .map_err(map_try_error)?;

        Ok(self.finish_reservation(bytes, slot, global))
    }

    /// Per-torrent share in bytes.
    pub fn share(&self) -> u64 {
        self.share
    }

    /// Number of piece buffers this torrent may reserve at once.
    pub fn max_in_flight(&self) -> usize {
        self.slot_count
    }

    /// Piece buffers currently held by this torrent.
    pub fn in_flight(&self) -> usize {
        self.slot_count
            .saturating_sub(self.slots.available_permits())
    }

    /// Bytes currently reserved by this torrent.
    pub fn in_use_bytes(&self) -> u64 {
        self.torrent_in_use.load(Ordering::Relaxed) as u64
    }

    /// Declared piece length for this torrent.
    pub fn piece_length(&self) -> u64 {
        self.piece_length
    }

    fn check_reservation(&self, bytes: usize) -> Result<u32, BudgetError> {
        if bytes == 0 {
            return Err(BudgetError::ZeroLength);
        }
        let requested =
            u64::try_from(bytes).map_err(|_| BudgetError::CountOverflow { value: u64::MAX })?;
        if requested > self.share {
            return Err(BudgetError::ExceedsShare {
                requested: bytes,
                share: self.share,
            });
        }
        if requested > self.global_capacity {
            return Err(BudgetError::ExceedsGlobal {
                requested: bytes,
                global: self.global_capacity,
            });
        }
        u32::try_from(bytes).map_err(|_| BudgetError::CountOverflow { value: requested })
    }

    fn finish_reservation(
        &self,
        bytes: usize,
        slot: OwnedSemaphorePermit,
        global: OwnedSemaphorePermit,
    ) -> PieceReservation {
        self.global_in_use.fetch_add(bytes, Ordering::Relaxed);
        self.torrent_in_use.fetch_add(bytes, Ordering::Relaxed);
        PieceReservation {
            bytes,
            global_in_use: Arc::clone(&self.global_in_use),
            torrent_in_use: Arc::clone(&self.torrent_in_use),
            _global: global,
            _slot: slot,
        }
    }
}

/// RAII reservation of piece-buffer bytes and one per-torrent slot.
///
/// Dropping it releases the reservation. The type is deliberately not
/// cloneable and has no public constructor.
pub struct PieceReservation {
    bytes: usize,
    global_in_use: Arc<AtomicUsize>,
    torrent_in_use: Arc<AtomicUsize>,
    _global: OwnedSemaphorePermit,
    _slot: OwnedSemaphorePermit,
}

impl PieceReservation {
    /// Number of bytes reserved.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Drop for PieceReservation {
    fn drop(&mut self) {
        self.global_in_use.fetch_sub(self.bytes, Ordering::Relaxed);
        self.torrent_in_use.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

fn map_try_error(error: TryAcquireError) -> BudgetError {
    match error {
        TryAcquireError::NoPermits => BudgetError::WouldBlock,
        TryAcquireError::Closed => BudgetError::Closed,
    }
}

#[cfg(test)]
mod tests;
