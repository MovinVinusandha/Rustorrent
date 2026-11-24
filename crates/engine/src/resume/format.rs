//! Version 1 resume document encoding and decoding.
//!
//! A resume document is canonical bencode with a fixed schema:
//!
//! ```text
//! d
//!   8:bitfield      <packed piece bits>
//!   5:files         l d 6:length i..e 8:mtime_ns i..e 13:observed_size i..e e ... e
//!   9:info_hash     <20 bytes>
//!   11:layout_hash  <20 bytes>
//!   11:piece_count  i..e
//!   12:piece_length i..e
//!   12:total_length i..e
//!   10:updated_at   i..e
//!   7:version       i1e
//! e
//! ```
//!
//! File entries are in [`FileMap`] order and carry no path components. A
//! fingerprint of the disk layout is stored instead, so a renamed or resized
//! file invalidates the whole document without leaking absolute paths into the
//! resume directory.
//!
//! Integers are limited to the bencode `i64` range. File lengths and observed
//! sizes are non-negative; modification times may be negative for files older
//! than the Unix epoch. The smallest `i64` is reserved for "modification time
//! unavailable", which [`crate::storage::ObservedFile`] never produces.

use sha1::{Digest, Sha1};
use thiserror::Error;

use crate::bencode::{self, DecodeOptions, KeyOrder, Limits, Value};
use crate::bitfield::{Bitfield, BitfieldError};
use crate::storage::layout::FileMap;

/// Current resume document version.
pub const RESUME_VERSION: i64 = 1;

/// Maximum size of one resume document, in bytes.
///
/// The limit is checked before decoding so a corrupt or hostile file cannot
/// make the engine allocate an unbounded buffer.
pub const MAX_RESUME_BYTES: usize = 16 * 1024 * 1024;

/// Number of bytes in an info-hash or layout fingerprint.
const HASH_BYTES: usize = 20;

/// Modification time stored when the platform cannot report one.
///
/// The smallest signed integer is outside the range a real filesystem time
/// maps to, so an observed time never equals it. Restore treats such an entry
/// as stale and rechecks the pieces it covers.
pub const MTIME_UNAVAILABLE: i64 = i64::MIN;

/// One file entry in a resume document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeFile {
    /// Nominal length from the metainfo, in bytes.
    pub length: u64,
    /// Size observed on disk when the document was written, in bytes.
    pub observed_size: u64,
    /// Modification time observed on disk, in nanoseconds since the epoch.
    ///
    /// [`MTIME_UNAVAILABLE`] means the platform could not report a value that
    /// fits in `i64`. Such an entry never matches an observed time, so every
    /// piece it covers is rechecked.
    pub mtime_ns: i64,
}

impl ResumeFile {
    /// Builds one file entry from explicit values.
    pub fn new(length: u64, observed_size: u64, mtime_ns: i64) -> Self {
        Self {
            length,
            observed_size,
            mtime_ns,
        }
    }
}

/// Versioned snapshot of verified pieces and observed file metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeDocument {
    /// Info-hash of the torrent this document belongs to.
    pub info_hash: [u8; HASH_BYTES],
    /// Piece length from the metainfo, in bytes.
    pub piece_length: u64,
    /// Total torrent length from the metainfo, in bytes.
    pub total_length: u64,
    /// Number of pieces in the torrent.
    pub piece_count: u32,
    /// Fingerprint of the sanitized disk layout.
    pub layout_hash: [u8; HASH_BYTES],
    /// Verified pieces.
    pub bitfield: Bitfield,
    /// File entries in file-map order.
    pub files: Vec<ResumeFile>,
    /// Unix time when the document was written, in seconds.
    pub updated_at: i64,
}

impl ResumeDocument {
    /// Builds a document without validating its fields.
    ///
    /// The piece count is taken from the bitfield. [`encode_document`] rejects
    /// a document whose [`ResumeDocument::piece_count`] field was changed to
    /// disagree with the bitfield, and integers outside the bencode range.
    pub fn new(
        info_hash: [u8; HASH_BYTES],
        piece_length: u64,
        total_length: u64,
        layout_hash: [u8; HASH_BYTES],
        bitfield: Bitfield,
        files: Vec<ResumeFile>,
        updated_at: i64,
    ) -> Self {
        Self {
            info_hash,
            piece_length,
            total_length,
            piece_count: bitfield.piece_count(),
            layout_hash,
            bitfield,
            files,
            updated_at,
        }
    }
}

/// Errors returned while encoding a resume document.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum EncodeError {
    /// A field does not fit in the signed bencode integer range.
    #[error("{field} does not fit in a bencode integer")]
    IntegerOutOfRange {
        /// Offending field name.
        field: &'static str,
    },
    /// The bitfield and the declared piece count disagree.
    #[error("bitfield has {found} pieces, expected {expected}")]
    BitfieldCount {
        /// Declared piece count.
        expected: u32,
        /// Bitfield piece count.
        found: u32,
    },
    /// The canonical encoding exceeds [`MAX_RESUME_BYTES`].
    #[error("encoded resume data is {len} bytes, limit is {max}")]
    TooLarge {
        /// Encoded size.
        len: usize,
        /// Configured maximum.
        max: usize,
    },
    /// Bencode encoding failed.
    #[error("could not encode resume data: {0}")]
    Bencode(#[from] bencode::Error),
}

/// Errors returned while decoding a resume document.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DecodeError {
    /// The input exceeds [`MAX_RESUME_BYTES`].
    #[error("resume data is {len} bytes, limit is {max}")]
    TooLarge {
        /// Input size.
        len: usize,
        /// Configured maximum.
        max: usize,
    },
    /// The document version is not supported by this build.
    #[error("unsupported resume version {version}")]
    UnsupportedVersion {
        /// Version found in the document.
        version: i64,
    },
    /// A fixed-length field has the wrong size.
    #[error("{field} is {found} bytes, expected {expected}")]
    WrongLength {
        /// Offending field name.
        field: &'static str,
        /// Required length.
        expected: usize,
        /// Actual length.
        found: usize,
    },
    /// The piece length is zero.
    #[error("piece length is zero")]
    ZeroPieceLength,
    /// An integer field is outside its allowed range.
    #[error("{field} is out of range")]
    OutOfRange {
        /// Offending field name.
        field: &'static str,
    },
    /// The packed bitfield is invalid for the declared piece count.
    #[error("invalid bitfield: {0}")]
    Bitfield(#[from] BitfieldError),
    /// The top-level value is not a dictionary.
    #[error("resume data is not a dictionary")]
    NotADictionary,
    /// A required field is missing.
    #[error("resume data is missing the {field} field")]
    MissingField {
        /// Missing field name.
        field: &'static str,
    },
    /// A field has the wrong bencode type.
    #[error("{field} has the wrong type")]
    WrongType {
        /// Offending field name.
        field: &'static str,
    },
    /// Bencode decoding failed.
    #[error("could not decode resume data: {0}")]
    Bencode(#[from] bencode::Error),
}

/// Encodes a document in canonical bencode.
pub fn encode_document(document: &ResumeDocument) -> Result<Vec<u8>, EncodeError> {
    let found = document.bitfield.piece_count();
    if found != document.piece_count {
        return Err(EncodeError::BitfieldCount {
            expected: document.piece_count,
            found,
        });
    }

    let piece_length = encode_integer(document.piece_length, "piece_length")?;
    let total_length = encode_integer(document.total_length, "total_length")?;
    let mut files = Vec::with_capacity(document.files.len());

    for file in &document.files {
        files.push(Value::dict(vec![
            (
                b"length",
                Value::int(encode_integer(file.length, "length")?),
            ),
            (b"mtime_ns", Value::int(file.mtime_ns)),
            (
                b"observed_size",
                Value::int(encode_integer(file.observed_size, "observed_size")?),
            ),
        ])?);
    }

    let value = Value::dict(vec![
        (b"bitfield", Value::bytes(document.bitfield.as_bytes())),
        (b"files", Value::list(files)),
        (b"info_hash", Value::bytes(&document.info_hash)),
        (b"layout_hash", Value::bytes(&document.layout_hash)),
        (b"piece_count", Value::int(i64::from(document.piece_count))),
        (b"piece_length", Value::int(piece_length)),
        (b"total_length", Value::int(total_length)),
        (b"updated_at", Value::int(document.updated_at)),
        (b"version", Value::int(RESUME_VERSION)),
    ])?;

    let encoded = bencode::encode(&value)?;
    if encoded.len() > MAX_RESUME_BYTES {
        return Err(EncodeError::TooLarge {
            len: encoded.len(),
            max: MAX_RESUME_BYTES,
        });
    }

    Ok(encoded)
}

/// Decodes and validates a version 1 resume document.
///
/// The size limit is enforced before parsing. Unknown versions, missing
/// fields, wrong types, out-of-range integers, malformed bitfields, and
/// non-canonical bencode are all typed errors. Structural validation against
/// the torrent's file map happens in the caller.
pub fn decode_document(bytes: &[u8]) -> Result<ResumeDocument, DecodeError> {
    if bytes.len() > MAX_RESUME_BYTES {
        return Err(DecodeError::TooLarge {
            len: bytes.len(),
            max: MAX_RESUME_BYTES,
        });
    }

    let document = bencode::decode_with_options(
        bytes,
        DecodeOptions {
            limits: Limits::default(),
            key_order: KeyOrder::Strict,
        },
    )?;

    if document.as_dict().is_none() {
        return Err(DecodeError::NotADictionary);
    }

    let version = integer_field(&document, "version")?;
    if version != RESUME_VERSION {
        return Err(DecodeError::UnsupportedVersion { version });
    }

    let info_hash = hash_field(&document, "info_hash")?;
    let layout_hash = hash_field(&document, "layout_hash")?;

    let piece_length = unsigned_field(&document, "piece_length")?;
    if piece_length == 0 {
        return Err(DecodeError::ZeroPieceLength);
    }

    let total_length = unsigned_field(&document, "total_length")?;
    let piece_count = u32::try_from(integer_field(&document, "piece_count")?).map_err(|_| {
        DecodeError::OutOfRange {
            field: "piece_count",
        }
    })?;

    let bitfield = Bitfield::from_bytes(bytes_field(&document, "bitfield")?, piece_count)?;

    let files = document
        .dict_get(b"files")
        .ok_or(DecodeError::MissingField { field: "files" })?
        .as_list()
        .ok_or(DecodeError::WrongType { field: "files" })?
        .iter()
        .map(decode_file)
        .collect::<Result<Vec<_>, _>>()?;

    let updated_at = integer_field(&document, "updated_at")?;

    Ok(ResumeDocument {
        info_hash,
        piece_length,
        total_length,
        piece_count,
        layout_hash,
        bitfield,
        files,
        updated_at,
    })
}

/// Fingerprints the sanitized disk layout of a torrent.
///
/// The digest covers, in file-map order, the file count, every disk path
/// component with its length, and every nominal file length. Length prefixes
/// make the encoding unambiguous, so a split or renamed component always
/// changes the result. The absolute root is not included: the same torrent in
/// a different download directory still matches.
pub fn layout_fingerprint(map: &FileMap) -> [u8; HASH_BYTES] {
    let mut hasher = Sha1::new();
    hasher.update((map.files().len() as u64).to_be_bytes());

    for file in map.files() {
        hasher.update((file.disk_components.len() as u32).to_be_bytes());
        for component in &file.disk_components {
            hasher.update((component.len() as u32).to_be_bytes());
            hasher.update(component.as_bytes());
        }
        hasher.update(file.length.to_be_bytes());
    }

    let mut fingerprint = [0u8; HASH_BYTES];
    fingerprint.copy_from_slice(&hasher.finalize());
    fingerprint
}

/// Decodes one file entry.
fn decode_file(value: &Value<'_>) -> Result<ResumeFile, DecodeError> {
    if value.as_dict().is_none() {
        return Err(DecodeError::WrongType { field: "file" });
    }

    Ok(ResumeFile {
        length: unsigned_field(value, "length")?,
        observed_size: unsigned_field(value, "observed_size")?,
        mtime_ns: integer_field(value, "mtime_ns")?,
    })
}

/// Reads a required integer field.
fn integer_field(value: &Value<'_>, field: &'static str) -> Result<i64, DecodeError> {
    value
        .dict_get(field.as_bytes())
        .ok_or(DecodeError::MissingField { field })?
        .as_int()
        .ok_or(DecodeError::WrongType { field })
}

/// Reads a required non-negative integer field.
fn unsigned_field(value: &Value<'_>, field: &'static str) -> Result<u64, DecodeError> {
    u64::try_from(integer_field(value, field)?).map_err(|_| DecodeError::OutOfRange { field })
}

/// Reads a required 20-byte field.
fn hash_field(value: &Value<'_>, field: &'static str) -> Result<[u8; HASH_BYTES], DecodeError> {
    let bytes = bytes_field(value, field)?;
    bytes.try_into().map_err(|_| DecodeError::WrongLength {
        field,
        expected: HASH_BYTES,
        found: bytes.len(),
    })
}

/// Reads a required byte-string field.
fn bytes_field<'a>(value: &Value<'a>, field: &'static str) -> Result<&'a [u8], DecodeError> {
    value
        .dict_get(field.as_bytes())
        .ok_or(DecodeError::MissingField { field })?
        .as_bytes()
        .ok_or(DecodeError::WrongType { field })
}

/// Converts an unsigned field to the signed bencode range.
fn encode_integer(value: u64, field: &'static str) -> Result<i64, EncodeError> {
    i64::try_from(value).map_err(|_| EncodeError::IntegerOutOfRange { field })
}

#[cfg(test)]
mod tests;
