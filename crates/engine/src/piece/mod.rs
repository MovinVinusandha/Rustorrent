//! In-memory piece assembly and verification primitives.
//!
//! The first stage (3c-2a) is pure in-memory: the memory budget, piece
//! buffers, and the assembler that expires stalled pieces. Hashing and writing
//! verified pieces arrive in 3c-2b and depend on the storage worker.

pub mod budget;

pub use budget::{
    BudgetError, DEFAULT_GLOBAL_BUDGET, MAX_TORRENT_SHARE, MemoryBudget, PieceReservation,
    TorrentBudget,
};
