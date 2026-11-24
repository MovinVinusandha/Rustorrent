//! Bounds-checked bitfield of piece indices.
//!
//! The bitfield stores one bit per piece, least significant bit first, in
//! exactly `ceil(piece_count / 8)` bytes. Bits after the last piece are always
//! zero, so the same bytes can be serialized into resume data (3d) and used by
//! a streaming recheck (3c-3).
//!
//! Every index access is checked and returns a typed error; malformed byte
//! slices are rejected rather than normalized.
//!
//! Bits are set only through [`Bitfield::mark_stored`], which takes a
//! [`StoredPiece`] produced by a complete write after a hash match. The
//! generic setter is private. [`Bitfield::from_bytes`] restores resume data,
//! and [`Bitfield::clear_piece`] / [`Bitfield::clear`] invalidate pieces; an
//! invalidation can never create false trust.

use std::fmt;

use thiserror::Error;

use crate::bencode::MAX_STRING_BYTES;
use crate::metainfo::PIECE_HASH_LENGTH;
use crate::piece::StoredPiece;

/// Highest piece count accepted by [`Bitfield`].
///
/// Metainfo stores all piece hashes in one bencode byte string. The parser caps
/// that string at [`MAX_STRING_BYTES`] and every hash occupies
/// [`PIECE_HASH_LENGTH`] bytes, so no valid metainfo can declare more pieces
/// than this.
pub const MAX_PIECE_COUNT: u32 = (MAX_STRING_BYTES / PIECE_HASH_LENGTH) as u32;

// `MetaInfo::parse` enforces `MAX_STRING_BYTES` on every bencode string, so the
// derived count always fits in `u32`. Pinned here so raising the string limit
// cannot silently truncate it.
const _: () = assert!(MAX_STRING_BYTES / PIECE_HASH_LENGTH <= u32::MAX as usize);

/// Errors returned by [`Bitfield`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BitfieldError {
    /// The requested piece count exceeds [`MAX_PIECE_COUNT`].
    #[error("piece count {count} exceeds the maximum {max}")]
    TooManyPieces {
        /// Requested piece count.
        count: u32,
        /// Maximum accepted piece count.
        max: u32,
    },
    /// A piece index is outside the bitfield.
    #[error("piece index {piece} is outside a {piece_count}-piece bitfield")]
    OutOfRange {
        /// Requested piece index.
        piece: u32,
        /// Number of pieces in this bitfield.
        piece_count: u32,
    },
    /// The byte slice does not have the exact length required by the piece
    /// count.
    #[error(
        "bitfield length {found} does not match the expected {expected} bytes for {piece_count} pieces"
    )]
    WrongLength {
        /// Required byte length.
        expected: usize,
        /// Actual byte length.
        found: usize,
        /// Requested piece count.
        piece_count: u32,
    },
    /// Bits after the last piece are set.
    #[error("trailing bits after piece {piece_count} are not zero")]
    TrailingBitsSet {
        /// Number of pieces in the bitfield.
        piece_count: u32,
    },
    /// The backing allocation failed.
    #[error("could not allocate a {bytes}-byte bitfield")]
    Allocation {
        /// Requested byte length.
        bytes: usize,
    },
}

/// Fixed-size set of piece indices.
#[derive(Clone, PartialEq, Eq)]
pub struct Bitfield {
    bytes: Box<[u8]>,
    piece_count: u32,
}

impl Bitfield {
    /// Creates a bitfield with every bit clear.
    ///
    /// Returns [`BitfieldError::TooManyPieces`] above [`MAX_PIECE_COUNT`].
    pub fn new(piece_count: u32) -> Result<Self, BitfieldError> {
        let byte_len = checked_byte_len(piece_count)?;
        let bytes = allocate_zeroed(byte_len)?;
        Ok(Self { bytes, piece_count })
    }

    /// Builds a bitfield from serialized bytes.
    ///
    /// The slice must be exactly `ceil(piece_count / 8)` bytes long, and every
    /// bit after the last piece must be zero.
    pub fn from_bytes(bytes: &[u8], piece_count: u32) -> Result<Self, BitfieldError> {
        let expected = checked_byte_len(piece_count)?;
        if bytes.len() != expected {
            return Err(BitfieldError::WrongLength {
                expected,
                found: bytes.len(),
                piece_count,
            });
        }
        if !trailing_bits_are_zero(bytes, piece_count) {
            return Err(BitfieldError::TrailingBitsSet { piece_count });
        }

        let mut owned = Vec::new();
        owned
            .try_reserve_exact(expected)
            .map_err(|_| BitfieldError::Allocation { bytes: expected })?;
        owned.extend_from_slice(bytes);
        Ok(Self {
            bytes: owned.into_boxed_slice(),
            piece_count,
        })
    }

    /// Number of pieces represented by this bitfield.
    pub fn piece_count(&self) -> u32 {
        self.piece_count
    }

    /// Exact serialized length in bytes.
    pub fn byte_len(&self) -> usize {
        self.bytes.len()
    }

    /// Returns whether `piece` is set.
    pub fn get(&self, piece: u32) -> Result<bool, BitfieldError> {
        let (index, mask) = self.bit(piece)?;
        let byte = self.bytes.get(index).ok_or(BitfieldError::OutOfRange {
            piece,
            piece_count: self.piece_count,
        })?;
        Ok(byte & mask != 0)
    }

    /// Sets the bit for a piece that was completely written after its SHA-1
    /// matched.
    ///
    /// This is the only way to set a bit. It is synchronous by design, so a
    /// caller never holds a `&mut Bitfield` across an await: call it after
    /// [`store_verified`](crate::piece::store_verified) returns.
    pub fn mark_stored(&mut self, stored: &StoredPiece) -> Result<(), BitfieldError> {
        self.set(stored.piece(), true)
    }

    /// Clears one piece bit, for example when a recheck finds it damaged.
    ///
    /// Invalidating a piece can never create false trust, so this is safe to
    /// expose.
    pub fn clear_piece(&mut self, piece: u32) -> Result<(), BitfieldError> {
        self.set(piece, false)
    }

    /// Sets or clears `piece`; private so `true` stays behind `mark_stored`.
    fn set(&mut self, piece: u32, value: bool) -> Result<(), BitfieldError> {
        let (index, mask) = self.bit(piece)?;
        let byte = self.bytes.get_mut(index).ok_or(BitfieldError::OutOfRange {
            piece,
            piece_count: self.piece_count,
        })?;
        if value {
            *byte |= mask;
        } else {
            *byte &= !mask;
        }
        Ok(())
    }

    /// Returns how many pieces are set.
    pub fn count_verified(&self) -> u32 {
        self.bytes.iter().map(|byte| byte.count_ones()).sum()
    }

    /// Clears every piece and zeroes the trailing bits.
    pub fn clear(&mut self) {
        self.bytes.fill(0);
    }

    /// Returns the exact serialized bytes.
    ///
    /// Bits after the last piece are always zero, so the result is ready for
    /// resume data.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn bit(&self, piece: u32) -> Result<(usize, u8), BitfieldError> {
        if piece >= self.piece_count {
            return Err(BitfieldError::OutOfRange {
                piece,
                piece_count: self.piece_count,
            });
        }
        let index = usize::try_from(piece / 8).map_err(|_| BitfieldError::OutOfRange {
            piece,
            piece_count: self.piece_count,
        })?;
        Ok((index, 1u8 << (piece % 8)))
    }
}

impl fmt::Debug for Bitfield {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Bitfield")
            .field("piece_count", &self.piece_count)
            .field("verified", &self.count_verified())
            .finish()
    }
}

/// Returns the exact byte length for `piece_count`.
fn checked_byte_len(piece_count: u32) -> Result<usize, BitfieldError> {
    if piece_count > MAX_PIECE_COUNT {
        return Err(BitfieldError::TooManyPieces {
            count: piece_count,
            max: MAX_PIECE_COUNT,
        });
    }
    let count = usize::try_from(piece_count).map_err(|_| BitfieldError::TooManyPieces {
        count: piece_count,
        max: MAX_PIECE_COUNT,
    })?;
    let bits = count.checked_add(7).ok_or(BitfieldError::TooManyPieces {
        count: piece_count,
        max: MAX_PIECE_COUNT,
    })?;
    Ok(bits / 8)
}

/// Allocates exactly `len` zero bytes, reporting allocation failure.
fn allocate_zeroed(len: usize) -> Result<Box<[u8]>, BitfieldError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|_| BitfieldError::Allocation { bytes: len })?;
    bytes.resize(len, 0);
    Ok(bytes.into_boxed_slice())
}

/// Returns the mask of valid bits in the final byte.
fn trailing_mask(piece_count: u32) -> u8 {
    let used = piece_count % 8;
    if used == 0 {
        u8::MAX
    } else {
        (1u8 << used) - 1
    }
}

/// Returns whether every bit after the last piece is zero.
fn trailing_bits_are_zero(bytes: &[u8], piece_count: u32) -> bool {
    match bytes.last() {
        Some(last) => last & !trailing_mask(piece_count) == 0,
        None => true,
    }
}

#[cfg(test)]
mod tests;
