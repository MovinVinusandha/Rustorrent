//! In-memory piece assembly, verification, and writing.
//!
//! The memory budget, piece specs, piece buffers, and the assembler that
//! expires stalled pieces are pure in-memory. Verified pieces are hashed on a
//! capped blocking pool and written through the storage worker in 1 MiB
//! chunks.

pub mod assembler;
pub mod budget;
pub mod buffer;
pub mod recheck;
pub mod spec;
pub mod store;
pub mod verify;

pub use assembler::{DEFAULT_STALL_TIMEOUT, InsertResult, PieceAssembler, PieceError};
pub use budget::{
    BudgetError, DEFAULT_GLOBAL_BUDGET, MAX_TORRENT_SHARE, MemoryBudget, PieceReservation,
    TorrentBudget,
};
pub use buffer::{BlockError, InsertOutcome, PieceBuffer, PieceBufferError};
pub use recheck::{
    RECHECK_BATCH_LEN, RECHECK_CHUNK_LEN, RECHECK_CHUNKS_PER_JOB, RecheckCancel, RecheckError,
    RecheckOutcome, RecheckProgress, RecheckSelection, Rechecker,
};
pub use spec::{PieceSpec, PieceSpecError};
pub use store::{StoredPiece, WriteError, store_verified};
pub use verify::{DEFAULT_MAX_HASH_JOBS, HashPool, HashPoolError, VerifiedPiece, VerifyError};

/// Builds a piece spec in unit tests without constructing metainfo.
///
/// Production code must use [`PieceSpec::from_metainfo`], which also checks
/// the spec against the torrent's file map and hash list.
#[cfg(test)]
pub(crate) fn test_spec(
    index: u32,
    offset: u64,
    length: usize,
    hash: [u8; crate::metainfo::PIECE_HASH_LENGTH],
) -> Result<PieceSpec, PieceSpecError> {
    PieceSpec::new(index, offset, length, hash)
}

/// Protocol block size: 16 KiB.
///
/// Every piece is split into blocks of this size. The final block of a piece
/// is shorter when the piece length is not a multiple of it.
pub const BLOCK_LEN: usize = 16 * 1024;

/// Asserts that every byte and slot allocated through `torrent` is released.
#[cfg(test)]
pub(crate) fn assert_budget_idle(budget: &MemoryBudget, torrent: &TorrentBudget) {
    assert_eq!(budget.in_use_bytes(), 0, "global budget leaked bytes");
    assert_eq!(torrent.in_use_bytes(), 0, "torrent budget leaked bytes");
    assert_eq!(torrent.in_flight(), 0, "torrent budget leaked a slot");
}

/// Engine-assigned handle for one peer connection.
///
/// This is deliberately not the 20-byte BitTorrent peer ID from the wire. The
/// engine allocates a key per connection, and the same key may be used for
/// blocks that arrived before the connection closed. The engine must retain a
/// key-to-address mapping after disconnect long enough to attribute a later
/// hash failure, then it may reuse the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PeerKey(u64);

impl PeerKey {
    /// Creates a key from an engine-assigned raw value.
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw engine-assigned value.
    pub const fn raw(self) -> u64 {
        self.0
    }
}
