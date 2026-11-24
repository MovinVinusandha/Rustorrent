//! Pure resume validation and restore planning.
//!
//! Nothing in this module performs I/O. The caller decodes a document, observes
//! the files through storage, and then asks this module which pieces can be
//! trusted and which must be rechecked.

use thiserror::Error;

use super::{ResumeDocument, ResumeFile, layout_fingerprint};
use crate::bitfield::{Bitfield, BitfieldError};
use crate::metainfo::InfoHash;
use crate::storage::ObservedFile;
use crate::storage::layout::FileMap;

/// Number of still-verified pieces sampled on every resume.
///
/// The sample is a backstop for silent corruption that leaves both size and
/// modification time unchanged. It is spread across the verified pieces,
/// including the first and the last. A torrent with fewer verified pieces is
/// checked completely.
pub const SPOT_CHECK_PIECES: usize = 5;

/// A resume document that does not describe the torrent it was loaded for.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ValidationError {
    /// The document belongs to a different torrent.
    #[error("resume info-hash does not match the torrent")]
    InfoHash,
    /// The document was written with a different piece length.
    #[error("resume piece length is {found}, expected {expected}")]
    PieceLength {
        /// Torrent piece length.
        expected: u64,
        /// Document piece length.
        found: u64,
    },
    /// The document was written for a different total length.
    #[error("resume total length is {found}, expected {expected}")]
    TotalLength {
        /// Torrent total length.
        expected: u64,
        /// Document total length.
        found: u64,
    },
    /// The document declares a different piece count.
    #[error("resume piece count is {found}, expected {expected}")]
    PieceCount {
        /// Torrent piece count.
        expected: u32,
        /// Document piece count.
        found: u32,
    },
    /// The packed bitfield does not match the torrent.
    #[error("resume bitfield has {found} pieces, expected {expected}")]
    BitfieldCount {
        /// Torrent piece count.
        expected: u32,
        /// Bitfield piece count.
        found: u32,
    },
    /// The document lists a different number of files.
    #[error("resume document has {found} files, expected {expected}")]
    FileCount {
        /// Torrent file count.
        expected: usize,
        /// Document file count.
        found: usize,
    },
    /// One file entry has a different nominal length.
    #[error("resume file {file_index} has length {found}, expected {expected}")]
    FileLength {
        /// Position in file-map order.
        file_index: usize,
        /// Torrent file length.
        expected: u64,
        /// Document file length.
        found: u64,
    },
    /// The disk layout fingerprint does not match the torrent.
    #[error("resume layout fingerprint does not match the torrent")]
    Layout,
}

/// Inputs to restore planning that do not line up.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RestoreError {
    /// Storage returned a different number of observations than mapped files.
    #[error("observed {found} files, expected {expected}")]
    ObservedFileCount {
        /// Number of files in the map.
        expected: usize,
        /// Number of observations.
        found: usize,
    },
    /// The document does not describe this torrent and must not be used.
    #[error("resume document is invalid: {0}")]
    Invalid(#[from] ValidationError),
    /// Clearing a piece failed.
    #[error("could not clear a piece: {0}")]
    Bitfield(#[from] BitfieldError),
}

/// Base bitfield and targeted recheck selection produced from resume data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePlan {
    bits: Bitfield,
    recheck: Vec<u32>,
}

impl RestorePlan {
    /// Bitfield to use as the base for the recheck.
    ///
    /// Every piece that is known absent or needs checking is clear. Unselected
    /// verified pieces are set.
    pub fn bits(&self) -> &Bitfield {
        &self.bits
    }

    /// Pieces to pass to
    /// [`RecheckSelection::Only`](crate::piece::RecheckSelection::Only).
    ///
    /// The indices are strictly increasing. An empty slice means no bytes need
    /// to be read.
    pub fn recheck(&self) -> &[u32] {
        &self.recheck
    }

    /// Consumes the plan and returns its base bitfield and recheck selection.
    pub fn into_parts(self) -> (Bitfield, Vec<u32>) {
        (self.bits, self.recheck)
    }
}

/// Checks that a decoded document describes this torrent.
///
/// The info-hash, piece length, total length, piece count, bitfield piece
/// count, file count, every nominal file length, and the disk layout
/// fingerprint must match. Observed sizes and modification times are checked
/// later against the current files, not here.
pub fn validate_document(
    document: &ResumeDocument,
    info_hash: &InfoHash,
    map: &FileMap,
) -> Result<(), ValidationError> {
    if document.info_hash != *info_hash.as_bytes() {
        return Err(ValidationError::InfoHash);
    }
    if document.piece_length != map.piece_length() {
        return Err(ValidationError::PieceLength {
            expected: map.piece_length(),
            found: document.piece_length,
        });
    }
    if document.total_length != map.total_length() {
        return Err(ValidationError::TotalLength {
            expected: map.total_length(),
            found: document.total_length,
        });
    }
    if document.piece_count != map.piece_count() {
        return Err(ValidationError::PieceCount {
            expected: map.piece_count(),
            found: document.piece_count,
        });
    }
    if document.bitfield.piece_count() != map.piece_count() {
        return Err(ValidationError::BitfieldCount {
            expected: map.piece_count(),
            found: document.bitfield.piece_count(),
        });
    }
    if document.files.len() != map.files().len() {
        return Err(ValidationError::FileCount {
            expected: map.files().len(),
            found: document.files.len(),
        });
    }

    for (file_index, (saved, mapped)) in document.files.iter().zip(map.files()).enumerate() {
        if saved.length != mapped.length {
            return Err(ValidationError::FileLength {
                file_index,
                expected: mapped.length,
                found: saved.length,
            });
        }
    }

    if document.layout_hash != layout_fingerprint(map) {
        return Err(ValidationError::Layout);
    }

    Ok(())
}

/// Decides which pieces can be trusted and which must be rechecked.
///
/// A document is validated with [`validate_document`] first. A mismatch is
/// returned as [`RestoreError::Invalid`] so the caller can log it and plan a
/// full recheck by passing no document. The info-hash and layout fingerprint
/// are never trusted implicitly.
///
/// Pieces whose covering files are missing or shorter than the piece are
/// cleared and never read. With a document, files whose exact size or
/// modification time changed are cleared, and up to [`SPOT_CHECK_PIECES`]
/// still-verified pieces are sampled; only those pieces are rechecked. Without
/// a document, the base bitfield starts clear and every readable piece is
/// rechecked.
pub fn plan_restore(
    map: &FileMap,
    info_hash: &InfoHash,
    document: Option<&ResumeDocument>,
    observed: &[ObservedFile],
) -> Result<RestorePlan, RestoreError> {
    if observed.len() != map.files().len() {
        return Err(RestoreError::ObservedFileCount {
            expected: map.files().len(),
            found: observed.len(),
        });
    }

    let mut bits = match document {
        Some(document) => {
            validate_document(document, info_hash, map)?;
            document.bitfield.clone()
        }
        None => Bitfield::new(map.piece_count())?,
    };

    let readable = clear_unreadable_pieces(map, observed, &mut bits)?;

    let mut recheck = match document {
        Some(document) => {
            let stale = clear_stale_pieces(document, map, observed, &mut bits)?;
            let spot = spot_check_pieces(&bits, SPOT_CHECK_PIECES);
            for &piece in &spot {
                bits.clear_piece(piece)?;
            }

            let mut recheck: Vec<u32> = stale
                .into_iter()
                .filter(|piece| readable.binary_search(piece).is_ok())
                .collect();
            recheck.extend(spot);
            recheck
        }
        None => readable,
    };

    recheck.sort_unstable();
    recheck.dedup();

    Ok(RestorePlan { bits, recheck })
}

/// Clears pieces covered by files whose exact size or modification time
/// changed, and returns those piece indices in increasing order.
fn clear_stale_pieces(
    document: &ResumeDocument,
    map: &FileMap,
    observed: &[ObservedFile],
    bits: &mut Bitfield,
) -> Result<Vec<u32>, RestoreError> {
    if observed.len() != map.files().len() {
        return Err(RestoreError::ObservedFileCount {
            expected: map.files().len(),
            found: observed.len(),
        });
    }

    let mut cleared = Vec::new();
    for (saved, (mapped, observed)) in document.files.iter().zip(map.files().iter().zip(observed)) {
        if file_is_stale(saved, observed) {
            for piece in mapped.pieces.clone() {
                bits.clear_piece(piece)?;
                cleared.push(piece);
            }
        }
    }

    cleared.sort_unstable();
    cleared.dedup();
    Ok(cleared)
}

/// Clears pieces that cannot be read completely from disk and returns the
/// readable piece indices in increasing order.
fn clear_unreadable_pieces(
    map: &FileMap,
    observed: &[ObservedFile],
    bits: &mut Bitfield,
) -> Result<Vec<u32>, RestoreError> {
    if observed.len() != map.files().len() {
        return Err(RestoreError::ObservedFileCount {
            expected: map.files().len(),
            found: observed.len(),
        });
    }

    let mut readable = Vec::new();
    for piece in 0..map.piece_count() {
        if piece_is_readable(map, observed, piece) {
            readable.push(piece);
        } else {
            bits.clear_piece(piece)?;
        }
    }

    Ok(readable)
}

/// Returns whether every byte of a piece is present on disk.
fn piece_is_readable(map: &FileMap, observed: &[ObservedFile], piece: u32) -> bool {
    let Some(range) = map.piece_byte_range(piece) else {
        return false;
    };

    map.segments(range)
        .all(|segment| match observed.get(segment.file_index) {
            Some(ObservedFile::Present { size, .. }) => segment
                .offset_in_file
                .checked_add(segment.len)
                .is_some_and(|end| end <= *size),
            Some(ObservedFile::Missing) | None => false,
        })
}

/// Compares a saved file entry with the current observation.
///
/// The comparison is exact: any size or modification-time difference makes the
/// file stale. A saved or observed modification time that is unavailable never
/// matches, so the file is rechecked.
fn file_is_stale(saved: &ResumeFile, observed: &ObservedFile) -> bool {
    match observed {
        ObservedFile::Missing => true,
        ObservedFile::Present { size, mtime_ns } => {
            *size != saved.observed_size || *mtime_ns != Some(saved.mtime_ns)
        }
    }
}

/// Samples up to `limit` verified pieces, spread across the bitfield.
fn spot_check_pieces(bits: &Bitfield, limit: usize) -> Vec<u32> {
    let verified: Vec<u32> = (0..bits.piece_count())
        .filter(|&piece| matches!(bits.get(piece), Ok(true)))
        .collect();

    if limit == 0 {
        return Vec::new();
    }
    if verified.len() <= limit {
        return verified;
    }
    if limit == 1 {
        return vec![verified[verified.len() / 2]];
    }

    let last = verified.len() as u64 - 1;
    let steps = limit as u64 - 1;
    (0..limit)
        .map(|index| {
            let position = index as u64 * last / steps;
            verified[position as usize]
        })
        .collect()
}

#[cfg(test)]
mod tests;
