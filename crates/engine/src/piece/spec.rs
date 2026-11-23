//! Piece identity and hash metadata.
//!
//! A [`PieceSpec`] ties one piece index to the exact byte range it covers and
//! the SHA-1 hash it must match. The last piece may be shorter than the
//! torrent's declared piece length, so the length is always taken from the
//! actual range and converted with checked arithmetic.

use thiserror::Error;

use crate::metainfo::{MAX_PIECE_LENGTH, MetaInfo, PIECE_HASH_LENGTH};
use crate::storage::layout::FileMap;

/// One piece's index, byte range, and expected SHA-1 hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PieceSpec {
    index: u32,
    offset: u64,
    end: u64,
    length: usize,
    hash: [u8; PIECE_HASH_LENGTH],
}

impl PieceSpec {
    /// Creates a piece spec after validating its range and length.
    ///
    /// `length` is the actual piece length, not the torrent's declared piece
    /// length. A zero length is rejected because no piece can be empty, and a
    /// length above [`MAX_PIECE_LENGTH`] is rejected even when the metainfo
    /// fields were forged.
    pub fn new(
        index: u32,
        offset: u64,
        length: usize,
        hash: [u8; PIECE_HASH_LENGTH],
    ) -> Result<Self, PieceSpecError> {
        if length == 0 {
            return Err(PieceSpecError::ZeroLength);
        }
        let length_u64 =
            u64::try_from(length).map_err(|_| PieceSpecError::LengthNotRepresentable { length })?;
        if length_u64 > MAX_PIECE_LENGTH {
            return Err(PieceSpecError::PieceTooLarge {
                length,
                max: MAX_PIECE_LENGTH,
            });
        }
        let end = offset
            .checked_add(length_u64)
            .ok_or(PieceSpecError::LengthOverflow {
                offset,
                length: length_u64,
            })?;

        Ok(Self {
            index,
            offset,
            end,
            length,
            hash,
        })
    }

    /// Derives a spec from a parsed torrent and its file map.
    ///
    /// The map and metainfo are re-validated because both expose public fields:
    /// piece length, total length, and hash count must agree. The last piece's
    /// length comes from the map's actual byte range, never from
    /// `piece_length`.
    pub fn from_metainfo(
        map: &FileMap,
        meta: &MetaInfo,
        index: u32,
    ) -> Result<Self, PieceSpecError> {
        if meta.piece_length == 0 {
            return Err(PieceSpecError::ZeroPieceLength);
        }
        if meta.piece_length > MAX_PIECE_LENGTH {
            return Err(PieceSpecError::PieceLengthTooLarge {
                piece_length: meta.piece_length,
                max: MAX_PIECE_LENGTH,
            });
        }
        if meta.piece_length != map.piece_length() {
            return Err(PieceSpecError::PieceLengthMismatch {
                map: map.piece_length(),
                metainfo: meta.piece_length,
            });
        }
        if meta.total_length != map.total_length() {
            return Err(PieceSpecError::TotalLengthMismatch {
                map: map.total_length(),
                metainfo: meta.total_length,
            });
        }

        let expected =
            usize::try_from(map.piece_count()).map_err(|_| PieceSpecError::CountOverflow {
                value: u64::from(map.piece_count()),
            })?;
        if meta.piece_hashes.len() != expected {
            return Err(PieceSpecError::HashCountMismatch {
                expected: map.piece_count(),
                found: meta.piece_hashes.len(),
            });
        }

        let range = map
            .piece_byte_range(index)
            .ok_or(PieceSpecError::PieceOutOfRange {
                piece: index,
                piece_count: map.piece_count(),
            })?;
        let length_u64 =
            range
                .end
                .checked_sub(range.start)
                .ok_or(PieceSpecError::RangeUnderflow {
                    start: range.start,
                    end: range.end,
                })?;
        let length = usize::try_from(length_u64)
            .map_err(|_| PieceSpecError::LengthTooLarge { length: length_u64 })?;
        let hash_index = usize::try_from(index).map_err(|_| PieceSpecError::CountOverflow {
            value: u64::from(index),
        })?;
        let hash = meta
            .piece_hashes
            .get(hash_index)
            .copied()
            .ok_or(PieceSpecError::HashMissing { piece: index })?;

        Self::new(index, range.start, length, hash)
    }

    /// Piece index within the torrent.
    pub fn index(&self) -> u32 {
        self.index
    }

    /// Byte offset in the torrent stream.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Absolute end offset, exclusive.
    pub fn end_offset(&self) -> u64 {
        self.end
    }

    /// Actual piece length in bytes.
    pub fn length(&self) -> usize {
        self.length
    }

    /// Expected SHA-1 hash.
    pub fn hash(&self) -> [u8; PIECE_HASH_LENGTH] {
        self.hash
    }
}

/// Errors returned while creating a [`PieceSpec`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PieceSpecError {
    /// A piece cannot have a zero length.
    #[error("piece length must be greater than zero")]
    ZeroLength,
    /// A piece length exceeds the metainfo limit.
    #[error("piece length {length} exceeds the maximum {max}")]
    PieceTooLarge {
        /// Actual piece length.
        length: usize,
        /// Maximum accepted length.
        max: u64,
    },
    /// The declared piece length in metainfo exceeds the limit.
    #[error("declared piece length {piece_length} exceeds the maximum {max}")]
    PieceLengthTooLarge {
        /// Declared piece length.
        piece_length: u64,
        /// Maximum accepted length.
        max: u64,
    },
    /// The declared piece length is zero.
    #[error("declared piece length must be greater than zero")]
    ZeroPieceLength,
    /// Offset plus length overflows `u64`.
    #[error("piece offset {offset} plus length {length} overflows u64")]
    LengthOverflow {
        /// Piece offset.
        offset: u64,
        /// Piece length.
        length: u64,
    },
    /// A length does not fit `usize`.
    #[error("piece length {length} does not fit usize")]
    LengthTooLarge {
        /// Piece length in bytes.
        length: u64,
    },
    /// A `usize` length does not fit `u64`.
    #[error("piece length {length} does not fit u64")]
    LengthNotRepresentable {
        /// Piece length in bytes.
        length: usize,
    },
    /// The file map and metainfo disagree about the piece length.
    #[error("map piece length {map} does not match metainfo piece length {metainfo}")]
    PieceLengthMismatch {
        /// Piece length from the map.
        map: u64,
        /// Piece length from metainfo.
        metainfo: u64,
    },
    /// The file map and metainfo disagree about the total length.
    #[error("map total length {map} does not match metainfo total length {metainfo}")]
    TotalLengthMismatch {
        /// Total length from the map.
        map: u64,
        /// Total length from metainfo.
        metainfo: u64,
    },
    /// The hash count does not match the piece count.
    #[error("metainfo has {found} hashes, expected {expected}")]
    HashCountMismatch {
        /// Expected hash count.
        expected: u32,
        /// Actual hash count.
        found: usize,
    },
    /// A piece index is outside the torrent.
    #[error("piece {piece} is outside a {piece_count}-piece torrent")]
    PieceOutOfRange {
        /// Requested piece index.
        piece: u32,
        /// Number of pieces in the torrent.
        piece_count: u32,
    },
    /// A byte range has an end before its start.
    #[error("piece range {start}..{end} is invalid")]
    RangeUnderflow {
        /// Range start.
        start: u64,
        /// Range end.
        end: u64,
    },
    /// A hash is missing for a piece.
    #[error("metainfo has no hash for piece {piece}")]
    HashMissing {
        /// Piece index.
        piece: u32,
    },
    /// A count does not fit the platform representation.
    #[error("value {value} does not fit usize")]
    CountOverflow {
        /// Value that overflowed.
        value: u64,
    },
}

#[cfg(test)]
mod tests;
