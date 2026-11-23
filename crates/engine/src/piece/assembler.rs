//! In-flight piece assembly and stalled-piece expiry.
//!
//! [`PieceAssembler`] owns every piece buffer a torrent is currently filling.
//! Starting a piece transfers a [`PieceReservation`] into its buffer; the
//! buffer keeps the reservation until it completes or is swept, so dropping a
//! stalled piece releases its budget immediately.
//!
//! Stall expiry is explicit. The engine decides when to call
//! [`PieceAssembler::sweep_stalled`] with its current [`Instant`]; there is no
//! background timer. A piece expires when it has gone `stall_timeout` without
//! a *valid* block. Duplicate, misaligned, out-of-range, and wrong-length
//! blocks do not refresh the deadline, so a peer cannot keep a dead piece
//! alive by resending junk.
//!
//! Completing a piece removes its buffer from the assembler and returns it to
//! the caller. A block that arrives after completion (or after stall expiry)
//! is [`PieceError::NotStarted`], which is benign endgame/latency noise rather
//! than a protocol violation.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use thiserror::Error;

use super::{
    BlockError, InsertOutcome, PeerKey, PieceBuffer, PieceBufferError, PieceReservation, PieceSpec,
};

/// Default stall timeout for an in-flight piece: 30 seconds.
pub const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Owns the piece buffers currently being filled for one torrent.
pub struct PieceAssembler {
    stall_timeout: Duration,
    pieces: BTreeMap<u32, InFlightPiece>,
}

struct InFlightPiece {
    buffer: PieceBuffer,
    last_progress: Instant,
}

/// Result of a block insertion.
pub enum InsertResult {
    /// The block was accepted and the piece is still incomplete.
    Incomplete(InsertOutcome),
    /// The block completed the piece; the assembler no longer owns the buffer.
    Complete(PieceBuffer),
}

impl PieceAssembler {
    /// Creates an assembler whose pieces expire after `stall_timeout`.
    pub fn new(stall_timeout: Duration) -> Result<Self, PieceError> {
        if stall_timeout.is_zero() {
            return Err(PieceError::ZeroStallTimeout);
        }
        Ok(Self {
            stall_timeout,
            pieces: BTreeMap::new(),
        })
    }

    /// Configures the stall timeout used by this assembler.
    pub fn stall_timeout(&self) -> Duration {
        self.stall_timeout
    }

    /// Starts filling `spec` with the bytes reserved by `reservation`.
    ///
    /// Returns [`PieceError::AlreadyInFlight`] if the piece index is already
    /// being assembled. On allocation or reservation errors the reservation is
    /// dropped, releasing its budget.
    pub fn start(
        &mut self,
        spec: PieceSpec,
        reservation: PieceReservation,
    ) -> Result<(), PieceError> {
        self.start_at(spec, reservation, Instant::now())
    }

    /// Inserts one block into an in-flight piece.
    ///
    /// Only a valid block refreshes the stall deadline. Errors are returned
    /// unchanged as [`PieceError::Block`], except that a piece which is not
    /// in flight yields [`PieceError::NotStarted`].
    pub fn insert(
        &mut self,
        piece: u32,
        peer: PeerKey,
        offset: u32,
        bytes: &[u8],
    ) -> Result<InsertResult, PieceError> {
        self.insert_at(piece, peer, offset, bytes, Instant::now())
    }

    /// Removes and returns every piece that has not made valid progress for
    /// `stall_timeout`, in ascending piece order.
    ///
    /// A piece expires when `now - last_progress >= stall_timeout`. The
    /// returned indices have already been dropped, so their budgets are free.
    pub fn sweep_stalled(&mut self, now: Instant) -> Vec<u32> {
        let timeout = self.stall_timeout;
        let mut stalled = Vec::new();
        self.pieces.retain(|piece, entry| {
            let expired = now.saturating_duration_since(entry.last_progress) >= timeout;
            if expired {
                stalled.push(*piece);
            }
            !expired
        });
        stalled
    }

    /// Whether `piece` is currently being assembled.
    pub fn is_in_flight(&self, piece: u32) -> bool {
        self.pieces.contains_key(&piece)
    }

    /// Number of pieces currently being assembled.
    pub fn in_flight(&self) -> usize {
        self.pieces.len()
    }

    /// Starts filling `spec` as of `now`; tests inject a clock here.
    fn start_at(
        &mut self,
        spec: PieceSpec,
        reservation: PieceReservation,
        now: Instant,
    ) -> Result<(), PieceError> {
        let piece = spec.index();
        if self.pieces.contains_key(&piece) {
            return Err(PieceError::AlreadyInFlight { piece });
        }
        let buffer = PieceBuffer::new(spec, reservation)?;
        self.pieces.insert(
            piece,
            InFlightPiece {
                buffer,
                last_progress: now,
            },
        );
        Ok(())
    }

    /// Inserts one block as of `now`; tests inject a clock here.
    fn insert_at(
        &mut self,
        piece: u32,
        peer: PeerKey,
        offset: u32,
        bytes: &[u8],
        now: Instant,
    ) -> Result<InsertResult, PieceError> {
        let outcome = {
            let entry = self
                .pieces
                .get_mut(&piece)
                .ok_or(PieceError::NotStarted { piece })?;
            let outcome = entry.buffer.insert(peer, offset, bytes)?;
            entry.last_progress = now;
            outcome
        };

        if !outcome.complete {
            return Ok(InsertResult::Incomplete(outcome));
        }

        match self.pieces.remove(&piece) {
            Some(entry) => Ok(InsertResult::Complete(entry.buffer)),
            None => Err(PieceError::NotStarted { piece }),
        }
    }

    #[cfg(test)]
    fn last_progress(&self, piece: u32) -> Option<Instant> {
        self.pieces.get(&piece).map(|entry| entry.last_progress)
    }
}

/// Errors returned by [`PieceAssembler`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PieceError {
    /// No buffer is in flight for this piece.
    ///
    /// This is expected when a block arrives after the piece completed, was
    /// swept as stalled, or was never started. It is not a protocol violation.
    #[error("piece {piece} is not in flight")]
    NotStarted {
        /// Piece index.
        piece: u32,
    },
    /// The piece is already being assembled.
    #[error("piece {piece} is already in flight")]
    AlreadyInFlight {
        /// Piece index.
        piece: u32,
    },
    /// A zero stall timeout would expire every piece on the next sweep.
    #[error("stall timeout must be greater than zero")]
    ZeroStallTimeout,
    /// Allocating or checking the piece buffer failed.
    #[error(transparent)]
    Buffer(#[from] PieceBufferError),
    /// The block was rejected by the piece buffer.
    #[error(transparent)]
    Block(#[from] BlockError),
}

impl PieceError {
    /// Whether this error is a protocol violation by a peer.
    ///
    /// Only the wrapped [`BlockError`] can be a violation, and even then
    /// [`BlockError::Duplicate`] stays benign. [`PieceError::NotStarted`],
    /// [`PieceError::AlreadyInFlight`], [`PieceError::ZeroStallTimeout`], and
    /// buffer allocation errors are local engine conditions. No banning
    /// decision is made here.
    pub fn is_protocol_violation(&self) -> bool {
        match self {
            Self::Block(error) => error.is_protocol_violation(),
            Self::NotStarted { .. }
            | Self::AlreadyInFlight { .. }
            | Self::ZeroStallTimeout
            | Self::Buffer(_) => false,
        }
    }
}

#[cfg(test)]
mod tests;
