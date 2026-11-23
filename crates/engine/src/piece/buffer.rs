//! Block assembly for one piece.
//!
//! A piece is divided into 16 KiB blocks. The last block of a piece may be
//! shorter, because the piece length is the actual byte range from the file
//! map. If a torrent's declared piece length is not a multiple of 16 KiB, the
//! final block of every full piece is short; no padding is ever added.
//!
//! Blocks are written once. The first valid block for an index wins; a later
//! block for the same index is rejected with [`BlockError::Duplicate`] and
//! never overwrites the first peer's bytes. Partial-block writes are not
//! supported: a block that starts inside another block is
//! [`BlockError::Misaligned`], and a payload spanning more than one block is
//! [`BlockError::WrongLength`]. There is deliberately no separate overlap
//! error.
//!
//! Every block records an engine-assigned [`PeerKey`]. On a hash failure the
//! engine re-requests the full piece; the contributor list is for later
//! statistical blame, not an immediate ban.

use std::collections::BTreeSet;

use thiserror::Error;

use super::{BLOCK_LEN, PeerKey, PieceReservation, PieceSpec};

/// A buffer that assembles one piece from blocks.
pub struct PieceBuffer {
    spec: PieceSpec,
    data: Box<[u8]>,
    peers: Box<[Option<PeerKey>]>,
    blocks: u32,
    received: u32,
    reservation: PieceReservation,
}

impl PieceBuffer {
    /// Allocates a buffer for `spec`.
    ///
    /// The reservation must cover exactly the actual piece length. Any
    /// allocation failure returns a typed error and drops the reservation, so
    /// no budget is leaked.
    pub fn new(spec: PieceSpec, reservation: PieceReservation) -> Result<Self, PieceBufferError> {
        if reservation.bytes() != spec.length() {
            return Err(PieceBufferError::ReservationMismatch {
                reserved: reservation.bytes(),
                needed: spec.length(),
            });
        }

        let blocks = spec.length().div_ceil(BLOCK_LEN);
        let block_count =
            u32::try_from(blocks).map_err(|_| PieceBufferError::TooManyBlocks { blocks })?;
        let data = allocate_zeroed(spec.length())?;
        let peers = allocate_peers(block_count)?;

        Ok(Self {
            spec,
            data,
            peers,
            blocks: block_count,
            received: 0,
            reservation,
        })
    }

    /// Piece metadata.
    pub fn spec(&self) -> &PieceSpec {
        &self.spec
    }

    /// Piece index within the torrent.
    pub fn piece(&self) -> u32 {
        self.spec.index()
    }

    /// Actual piece length in bytes.
    pub fn piece_len(&self) -> usize {
        self.spec.length()
    }

    /// Number of 16 KiB blocks, including a short final block.
    pub fn block_count(&self) -> u32 {
        self.blocks
    }

    /// Expected length of `block`, or `None` when it is outside the piece.
    pub fn block_len(&self, block: u32) -> Option<usize> {
        if block >= self.block_count() {
            return None;
        }
        let block_index = usize::try_from(block).ok()?;
        let start = block_index.checked_mul(BLOCK_LEN)?;
        Some(self.spec.length().saturating_sub(start).min(BLOCK_LEN))
    }

    /// Number of blocks received so far.
    pub fn received_blocks(&self) -> u32 {
        self.received
    }

    /// Whether every block has been supplied exactly once.
    pub fn is_complete(&self) -> bool {
        self.received == self.block_count()
    }

    /// Returns the piece bytes only when the piece is complete.
    pub fn complete_bytes(&self) -> Option<&[u8]> {
        self.is_complete().then_some(self.data.as_ref())
    }

    /// Peer that supplied `block`, or `None` if it is missing or out of range.
    pub fn block_contributor(&self, block: u32) -> Option<PeerKey> {
        let index = usize::try_from(block).ok()?;
        self.peers.get(index).copied().flatten()
    }

    /// Sorted, deduplicated peers that supplied at least one block.
    ///
    /// Used for diagnostics after a hash failure. No banning happens here.
    pub fn contributors(&self) -> Vec<PeerKey> {
        let mut peers = BTreeSet::new();
        for peer in self.peers.iter().flatten() {
            peers.insert(*peer);
        }
        peers.into_iter().collect()
    }

    /// Bytes reserved by this buffer.
    pub fn reserved_bytes(&self) -> usize {
        self.reservation.bytes()
    }

    /// Inserts one block supplied by `peer`.
    ///
    /// Validation order is fixed: alignment, piece range, exact block length,
    /// then duplicate. Every check runs before the buffer is modified, so a
    /// rejected block cannot corrupt it. A valid block sets `complete` when it
    /// was the last missing block.
    pub fn insert(
        &mut self,
        peer: PeerKey,
        offset: u32,
        bytes: &[u8],
    ) -> Result<InsertOutcome, BlockError> {
        let offset_usize = usize::try_from(offset).map_err(|_| BlockError::OutOfRange {
            offset,
            piece_len: self.spec.length(),
        })?;
        if offset_usize % BLOCK_LEN != 0 {
            return Err(BlockError::Misaligned {
                offset,
                block_len: BLOCK_LEN,
            });
        }

        let block_index = offset_usize / BLOCK_LEN;
        let block = u32::try_from(block_index).map_err(|_| BlockError::OutOfRange {
            offset,
            piece_len: self.spec.length(),
        })?;
        let expected = self.block_len(block).ok_or(BlockError::OutOfRange {
            offset,
            piece_len: self.spec.length(),
        })?;
        if bytes.len() != expected {
            return Err(BlockError::WrongLength {
                block,
                expected,
                found: bytes.len(),
            });
        }

        if self
            .peers
            .get(block_index)
            .is_some_and(|peer| peer.is_some())
        {
            return Err(BlockError::Duplicate { block });
        }

        let end = offset_usize
            .checked_add(expected)
            .ok_or(BlockError::OutOfRange {
                offset,
                piece_len: self.spec.length(),
            })?;
        let target = self
            .data
            .get_mut(offset_usize..end)
            .ok_or(BlockError::OutOfRange {
                offset,
                piece_len: self.spec.length(),
            })?;
        target.copy_from_slice(bytes);

        let peer_slot = self
            .peers
            .get_mut(block_index)
            .ok_or(BlockError::OutOfRange {
                offset,
                piece_len: self.spec.length(),
            })?;
        *peer_slot = Some(peer);
        self.received = self.received.saturating_add(1);

        Ok(InsertOutcome {
            block,
            complete: self.is_complete(),
        })
    }
}

/// Result of a successful block insertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InsertOutcome {
    /// Block index that was filled.
    pub block: u32,
    /// Whether the piece is complete after this insertion.
    pub complete: bool,
}

/// Errors returned by [`PieceBuffer::insert`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BlockError {
    /// The block start is not a multiple of the block size.
    #[error("block offset {offset} is not aligned to {block_len} bytes")]
    Misaligned {
        /// Supplied offset within the piece.
        offset: u32,
        /// Required block alignment.
        block_len: usize,
    },
    /// The block starts outside the piece.
    #[error("block offset {offset} is outside the {piece_len}-byte piece")]
    OutOfRange {
        /// Supplied offset within the piece.
        offset: u32,
        /// Actual piece length.
        piece_len: usize,
    },
    /// The payload is not exactly one block long.
    #[error("block {block} has {found} bytes, expected {expected}")]
    WrongLength {
        /// Block index.
        block: u32,
        /// Expected block length.
        expected: usize,
        /// Supplied length.
        found: usize,
    },
    /// The block index was already supplied.
    #[error("block {block} was already supplied")]
    Duplicate {
        /// Block index.
        block: u32,
    },
}

impl BlockError {
    /// Whether this error is a protocol violation by the peer.
    ///
    /// A duplicate is benign: it happens in endgame and when a late block
    /// arrives after the piece completed. Misaligned, out-of-range, and
    /// wrong-length blocks are protocol violations. No banning decision is
    /// made here.
    pub fn is_protocol_violation(&self) -> bool {
        match self {
            Self::Duplicate { .. } => false,
            Self::Misaligned { .. } | Self::OutOfRange { .. } | Self::WrongLength { .. } => true,
        }
    }
}

/// Errors returned while allocating a [`PieceBuffer`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PieceBufferError {
    /// The reservation does not match the actual piece length.
    #[error("reserved {reserved} bytes for a {needed}-byte piece")]
    ReservationMismatch {
        /// Reserved bytes.
        reserved: usize,
        /// Required bytes.
        needed: usize,
    },
    /// The block count does not fit `u32`.
    #[error("piece has {blocks} blocks, which does not fit u32")]
    TooManyBlocks {
        /// Computed block count.
        blocks: usize,
    },
    /// The data allocation failed.
    #[error("could not allocate {bytes} bytes for a piece buffer")]
    Allocation {
        /// Requested bytes.
        bytes: usize,
    },
}

fn allocate_zeroed(len: usize) -> Result<Box<[u8]>, PieceBufferError> {
    let mut data = Vec::new();
    data.try_reserve_exact(len)
        .map_err(|_| PieceBufferError::Allocation { bytes: len })?;
    data.resize(len, 0);
    Ok(data.into_boxed_slice())
}

fn allocate_peers(block_count: u32) -> Result<Box<[Option<PeerKey>]>, PieceBufferError> {
    let count = usize::try_from(block_count)
        .map_err(|_| PieceBufferError::TooManyBlocks { blocks: usize::MAX })?;
    let bytes = count.saturating_mul(std::mem::size_of::<Option<PeerKey>>());
    let mut peers = Vec::new();
    peers
        .try_reserve_exact(count)
        .map_err(|_| PieceBufferError::Allocation { bytes })?;
    peers.resize(count, None);
    Ok(peers.into_boxed_slice())
}

#[cfg(test)]
mod tests;
