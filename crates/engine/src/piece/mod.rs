//! In-memory piece assembly and verification primitives.
//!
//! The first stage (3c-2a) is pure in-memory: the memory budget, piece specs,
//! piece buffers, and the assembler that expires stalled pieces. Hashing and
//! writing verified pieces arrive in 3c-2b and depend on the storage worker.

pub mod assembler;
pub mod budget;
pub mod buffer;
pub mod spec;

pub use assembler::{DEFAULT_STALL_TIMEOUT, InsertResult, PieceAssembler, PieceError};
pub use budget::{
    BudgetError, DEFAULT_GLOBAL_BUDGET, MAX_TORRENT_SHARE, MemoryBudget, PieceReservation,
    TorrentBudget,
};
pub use buffer::{BlockError, InsertOutcome, PieceBuffer, PieceBufferError};
pub use spec::{PieceSpec, PieceSpecError};

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
