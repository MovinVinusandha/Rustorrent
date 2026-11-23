//! In-memory piece assembly and verification primitives.
//!
//! The first stage (3c-2a) is pure in-memory: the memory budget, piece specs,
//! piece buffers, and the assembler that expires stalled pieces. Hashing and
//! writing verified pieces arrive in 3c-2b and depend on the storage worker.

pub mod budget;
pub mod spec;

pub use budget::{
    BudgetError, DEFAULT_GLOBAL_BUDGET, MAX_TORRENT_SHARE, MemoryBudget, PieceReservation,
    TorrentBudget,
};
pub use spec::{PieceSpec, PieceSpecError};

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
