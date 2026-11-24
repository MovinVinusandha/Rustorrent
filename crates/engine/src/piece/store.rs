//! Writing hash-verified pieces through the storage worker.
//!
//! [`store_verified`] borrows a [`VerifiedPiece`] instead of consuming it, so
//! a disk-full or permission error halfway through a piece does not throw away
//! bytes that were already downloaded and verified. The caller keeps the piece,
//! retries after the error clears, and drops it on success or when giving up.
//! The piece's memory reservation stays held for as long as the caller keeps
//! it, and storing the same piece twice is harmless: every write is positioned
//! and idempotent.
//!
//! A piece is written in chunks of at most [`MAX_RANGE_LEN`] (1 MiB) because
//! that is the storage worker's range limit. Each chunk is copied into a
//! transient `Vec`; the copy is at most 1 MiB and is outside the piece-buffer
//! budget, which still only accounts for the piece data itself.
//!
//! Cancelling the returned future may leave the chunks written so far on disk,
//! and a chunk that was already enqueued on the storage worker may still run,
//! matching the 3c-1 contract. Since the caller still owns the piece and the
//! writes are idempotent, calling [`store_verified`] again is safe.
//!
//! The returned [`StoredPiece`] has no public constructor: only a complete
//! chunk run produces one, and [`Bitfield::mark_stored`] is the only way to
//! set a verified bit from it.
//!
//! [`Bitfield::mark_stored`]: crate::bitfield::Bitfield::mark_stored

use thiserror::Error;

use super::{PieceSpec, VerifiedPiece};
use crate::storage::backend::StorageError;
use crate::storage::thread::{MAX_RANGE_LEN, StorageHandle, StorageHandleError};

/// Writes every chunk of a verified piece.
///
/// Writes are sequential and each one is at most 1 MiB. On the first error the
/// remaining chunks are skipped and a [`WriteError`] names the failed chunk.
/// Earlier chunks may already be on disk; the piece stays unverified and a
/// retry rewrites the whole piece at the same offsets.
pub async fn store_verified(
    storage: &StorageHandle,
    verified: &VerifiedPiece,
) -> Result<StoredPiece, WriteError> {
    let spec = *verified.spec();
    for (chunk_index, chunk) in verified.data().chunks(MAX_RANGE_LEN).enumerate() {
        let offset = chunk_offset(spec, chunk_index)?;
        storage
            .write_range(offset, chunk.to_vec())
            .await
            .map_err(|source| WriteError::Chunk {
                piece: spec.index(),
                chunk_index,
                offset,
                len: chunk.len(),
                source,
            })?;
    }

    Ok(StoredPiece { spec })
}

/// Absolute torrent offset of one write chunk.
fn chunk_offset(spec: PieceSpec, chunk_index: usize) -> Result<u64, WriteError> {
    let overflow = || WriteError::OffsetOverflow {
        piece: spec.index(),
        chunk_index,
    };
    let index = u64::try_from(chunk_index).map_err(|_| overflow())?;
    let distance = index
        .checked_mul(MAX_RANGE_LEN as u64)
        .ok_or_else(overflow)?;
    spec.offset().checked_add(distance).ok_or_else(overflow)
}

/// Evidence that every chunk of one piece was written completely.
///
/// No public constructor exists: only a successful [`store_verified`] call
/// produces one, so a bit can only be set after both a hash match and a
/// complete write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredPiece {
    spec: PieceSpec,
}

impl StoredPiece {
    /// Piece metadata, including the expected hash.
    pub fn spec(&self) -> &PieceSpec {
        &self.spec
    }

    /// Piece index within the torrent.
    pub fn piece(&self) -> u32 {
        self.spec.index()
    }

    /// Byte offset in the torrent stream.
    pub fn offset(&self) -> u64 {
        self.spec.offset()
    }

    /// Actual piece length in bytes.
    pub fn length(&self) -> usize {
        self.spec.length()
    }
}

/// Errors returned by [`store_verified`].
#[derive(Debug, Error)]
pub enum WriteError {
    /// One chunk failed; the error names the chunk and its offset.
    #[error(
        "piece {piece} chunk {chunk_index} at offset {offset} ({len} bytes) could not be written"
    )]
    Chunk {
        /// Piece index.
        piece: u32,
        /// Zero-based chunk index within the piece.
        chunk_index: usize,
        /// Absolute torrent offset of the chunk.
        offset: u64,
        /// Chunk length in bytes.
        len: usize,
        /// Storage worker error for this chunk.
        #[source]
        source: StorageHandleError,
    },
    /// A chunk offset does not fit `u64`; defensive, a valid spec cannot hit it.
    #[error("piece {piece} chunk {chunk_index} offset overflows u64")]
    OffsetOverflow {
        /// Piece index.
        piece: u32,
        /// Zero-based chunk index within the piece.
        chunk_index: usize,
    },
}

impl WriteError {
    /// Whether the failed chunk hit a full disk.
    ///
    /// The caller keeps the verified piece, so pausing the torrent and
    /// retrying after space is freed does not need a re-download.
    pub fn is_disk_full(&self) -> bool {
        matches!(
            self,
            Self::Chunk {
                source: StorageHandleError::Storage(StorageError::DiskFull { .. }),
                ..
            }
        )
    }
}

#[cfg(test)]
mod tests;
