//! BitTorrent v1 metainfo (`.torrent`) parsing.
//!
//! The parser is strict about structure and lenient only where real-world
//! torrents are inconsistent:
//!
//! - bencode dictionary keys may be unsorted, but duplicate keys are rejected;
//! - `announce` and `announce-list` problems never fail the parse;
//! - `private` is `true` only for the integer `1`.
//!
//! The info-hash is SHA-1 over the raw bytes of the `info` value as they appear
//! in the input. It is never computed from a re-encoding, because a lenient
//! decode does not preserve dictionary key order.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use sha1::{Digest, Sha1};
use thiserror::Error;

use crate::bencode::{self, DecodeOptions, KeyOrder, Value};

/// Maximum accepted `piece length`: 64 MiB.
pub const MAX_PIECE_LENGTH: u64 = 64 * 1024 * 1024;

/// Length of one SHA-1 piece hash in bytes.
pub const PIECE_HASH_LENGTH: usize = 20;

/// SHA-1 info-hash of a torrent.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InfoHash([u8; PIECE_HASH_LENGTH]);

impl InfoHash {
    /// Creates an info-hash from raw digest bytes.
    pub fn from_bytes(bytes: [u8; PIECE_HASH_LENGTH]) -> Self {
        Self(bytes)
    }

    /// Returns the raw 20 digest bytes.
    pub fn as_bytes(&self) -> &[u8; PIECE_HASH_LENGTH] {
        &self.0
    }
}

impl fmt::Display for InfoHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for InfoHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "InfoHash({self})")
    }
}

/// Torrent layout, taken from the shape of the `info` dictionary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Single-file torrent, described by `info.length`.
    Single,
    /// Multi-file torrent, described by `info.files`.
    Multi,
}

/// One file listed by a torrent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    /// Complete path relative to the download directory.
    ///
    /// The first component is always the torrent `name`; the remaining
    /// components come from the file's `path` list.
    pub path: Vec<String>,
    /// File length in bytes.
    pub length: u64,
}

/// A parsed v1 metainfo file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaInfo {
    /// SHA-1 hash of the raw `info` dictionary bytes.
    pub info_hash: InfoHash,
    /// Suggested name: a file name for single-file torrents, a directory name
    /// for multi-file torrents.
    pub name: String,
    /// Layout kind.
    pub mode: Mode,
    /// Files in torrent order. Paths include `name` as the first component.
    pub files: Vec<FileInfo>,
    /// Total length of all files in bytes.
    pub total_length: u64,
    /// Declared `piece length` in bytes.
    pub piece_length: u64,
    /// SHA-1 hash of each piece, in order.
    pub piece_hashes: Vec<[u8; PIECE_HASH_LENGTH]>,
    /// Primary tracker URL, if it is present and valid UTF-8.
    pub announce: Option<String>,
    /// Tracker tiers from `announce-list`, with invalid entries dropped.
    pub announce_list: Vec<Vec<String>>,
    /// `true` only when `info.private` is the integer `1`.
    pub private: bool,
}

impl MetaInfo {
    /// Parses one metainfo file.
    ///
    /// Dictionary keys may be unsorted, because the info-hash is taken from
    /// the original byte range. Duplicate keys are still rejected.
    pub fn parse(input: &[u8]) -> Result<Self, Error> {
        let options = DecodeOptions {
            key_order: KeyOrder::Lenient,
            ..DecodeOptions::default()
        };
        let root = bencode::decode_with_options(input, options)?;
        if root.as_dict().is_none() {
            return Err(Error::NotADictionary);
        }

        let info = root.dict_get(b"info").ok_or(Error::MissingInfo)?;
        if info.as_dict().is_none() {
            return Err(Error::InfoNotADictionary);
        }

        let info_bytes = input.get(info.span()).ok_or(Error::InfoSpanOutOfRange)?;
        let mut digest = [0u8; PIECE_HASH_LENGTH];
        digest.copy_from_slice(&Sha1::digest(info_bytes));
        let info_hash = InfoHash::from_bytes(digest);

        if info.dict_get(b"pieces").is_none()
            && info.dict_get(b"meta version").and_then(Value::as_int) == Some(2)
        {
            return Err(Error::UnsupportedV2);
        }

        let name = required_utf8(info, b"name", "name")?;
        validate_component(&name, "name")?;
        let piece_length = required_piece_length(info)?;
        let pieces = required_pieces(info)?;

        let (mode, files, total_length) = match (info.dict_get(b"length"), info.dict_get(b"files"))
        {
            (Some(_), Some(_)) => return Err(Error::AmbiguousLayout),
            (None, None) => return Err(Error::MissingLayout),
            (Some(value), None) => {
                let length = non_negative(value, "length")?;
                let files = vec![FileInfo {
                    path: vec![name.clone()],
                    length,
                }];
                (Mode::Single, files, length)
            }
            (None, Some(value)) => {
                let entries = value.as_list().ok_or(Error::WrongType { field: "files" })?;
                if entries.is_empty() {
                    return Err(Error::EmptyFiles);
                }

                let mut files = Vec::with_capacity(entries.len());
                let mut total_length = 0u64;
                for entry in entries {
                    let dict = entry.as_dict().ok_or(Error::WrongType { field: "files" })?;
                    let length = entry_length(dict)?;
                    total_length = total_length
                        .checked_add(length)
                        .ok_or(Error::TotalLengthOverflow)?;
                    files.push(FileInfo {
                        path: entry_path(dict, &name)?,
                        length,
                    });
                }
                (Mode::Multi, files, total_length)
            }
        };

        let mut seen_paths = BTreeSet::new();
        for file in &files {
            let path = file.path.join("/");
            if !seen_paths.insert(path.clone()) {
                return Err(Error::DuplicatePath { path });
            }
        }

        let expected_pieces = total_length.div_ceil(piece_length);
        let found_pieces = (pieces.len() / PIECE_HASH_LENGTH) as u64;
        if expected_pieces != found_pieces {
            return Err(Error::PieceCountMismatch {
                expected: expected_pieces,
                found: found_pieces,
            });
        }

        let (chunks, _) = pieces.as_chunks::<PIECE_HASH_LENGTH>();
        let piece_hashes = chunks.to_vec();

        Ok(Self {
            info_hash,
            name,
            mode,
            files,
            total_length,
            piece_length,
            piece_hashes,
            announce: optional_utf8(&root, b"announce"),
            announce_list: announce_list(&root),
            private: info.dict_get(b"private").and_then(Value::as_int) == Some(1),
        })
    }
}

/// Errors produced while parsing metainfo.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum Error {
    /// The input is not valid bencode.
    #[error("invalid bencode: {0}")]
    Bencode(#[from] bencode::Error),
    /// The top-level value is not a dictionary.
    #[error("metainfo must be a dictionary")]
    NotADictionary,
    /// The top-level dictionary has no `info` key.
    #[error("missing `info` dictionary")]
    MissingInfo,
    /// The `info` value is not a dictionary.
    #[error("`info` must be a dictionary")]
    InfoNotADictionary,
    /// A required key is missing.
    #[error("missing `{field}`")]
    MissingField {
        /// Field name used in the message.
        field: &'static str,
    },
    /// A key has the wrong bencode type.
    #[error("`{field}` has the wrong type")]
    WrongType {
        /// Field name used in the message.
        field: &'static str,
    },
    /// A text field is not valid UTF-8.
    #[error("`{field}` is not valid UTF-8")]
    InvalidUtf8 {
        /// Field name used in the message.
        field: &'static str,
    },
    /// A length field is negative.
    #[error("`{field}` is negative: {value}")]
    NegativeLength {
        /// Field name used in the message.
        field: &'static str,
        /// The offending value.
        value: i64,
    },
    /// `info` has neither `length` nor `files`.
    #[error("`info` has neither `length` nor `files`")]
    MissingLayout,
    /// `info` has both `length` and `files`.
    #[error("`info` has both `length` and `files`")]
    AmbiguousLayout,
    /// The `files` list is empty.
    #[error("`files` must not be empty")]
    EmptyFiles,
    /// A `path` list is empty.
    #[error("`path` must not be empty")]
    EmptyPath,
    /// A `name` or `path` component is empty.
    #[error("`{field}` contains an empty component")]
    EmptyPathComponent {
        /// Field name used in the message.
        field: &'static str,
    },
    /// A `name` or `path` component is not a single safe path element.
    #[error("`{field}` contains an unsafe component: {component:?}")]
    UnsafePathComponent {
        /// Field name used in the message.
        field: &'static str,
        /// The offending component.
        component: String,
    },
    /// Two files resolve to the same relative path.
    #[error("duplicate file path: {path:?}")]
    DuplicatePath {
        /// The repeated path, joined with `/`.
        path: String,
    },
    /// `piece length` is zero or negative.
    #[error("`piece length` must be greater than zero")]
    ZeroPieceLength,
    /// `piece length` exceeds [`MAX_PIECE_LENGTH`].
    #[error("`piece length` {len} exceeds {max}")]
    PieceLengthTooLarge {
        /// Declared piece length.
        len: u64,
        /// Configured maximum.
        max: u64,
    },
    /// The `pieces` string is not a whole number of 20-byte hashes.
    #[error("`pieces` length {len} is not a multiple of {PIECE_HASH_LENGTH}")]
    InvalidPiecesLength {
        /// Declared string length.
        len: usize,
    },
    /// The number of piece hashes does not match the total length.
    #[error("expected {expected} piece hashes, found {found}")]
    PieceCountMismatch {
        /// Number of hashes implied by the total length and piece length.
        expected: u64,
        /// Number of hashes present in `pieces`.
        found: u64,
    },
    /// The sum of file lengths does not fit in `u64`.
    #[error("total length overflows u64")]
    TotalLengthOverflow,
    /// The `info` dictionary describes a BitTorrent v2 torrent.
    #[error("BitTorrent v2 metainfo is not supported")]
    UnsupportedV2,
    /// Internal invariant violation: the decoded `info` span is outside the
    /// input.
    #[error("decoded `info` span is outside the input")]
    InfoSpanOutOfRange,
}

fn required_utf8(dict: &Value<'_>, key: &[u8], field: &'static str) -> Result<String, Error> {
    let value = dict.dict_get(key).ok_or(Error::MissingField { field })?;
    let bytes = value.as_bytes().ok_or(Error::WrongType { field })?;
    let text = std::str::from_utf8(bytes).map_err(|_| Error::InvalidUtf8 { field })?;
    Ok(text.to_owned())
}

fn non_negative(value: &Value<'_>, field: &'static str) -> Result<u64, Error> {
    let number = value.as_int().ok_or(Error::WrongType { field })?;
    u64::try_from(number).map_err(|_| Error::NegativeLength {
        field,
        value: number,
    })
}

fn validate_component(component: &str, field: &'static str) -> Result<(), Error> {
    if component.is_empty() {
        return Err(Error::EmptyPathComponent { field });
    }

    // Windows-specific problems (backslashes, drive prefixes, reserved names,
    // trailing dots/spaces) are left to the storage layer.
    let unsafe_text = component == "."
        || component == ".."
        || component.contains('/')
        || component.chars().any(|c| c.is_ascii_control());
    if unsafe_text {
        return Err(Error::UnsafePathComponent {
            field,
            component: component.to_owned(),
        });
    }

    Ok(())
}

fn entry_length(dict: &BTreeMap<&[u8], Value<'_>>) -> Result<u64, Error> {
    let value = dict
        .get(b"length".as_slice())
        .ok_or(Error::MissingField { field: "length" })?;
    non_negative(value, "length")
}

fn entry_path(dict: &BTreeMap<&[u8], Value<'_>>, name: &str) -> Result<Vec<String>, Error> {
    let value = dict
        .get(b"path".as_slice())
        .ok_or(Error::MissingField { field: "path" })?;
    let components = value.as_list().ok_or(Error::WrongType { field: "path" })?;
    if components.is_empty() {
        return Err(Error::EmptyPath);
    }

    let mut path = Vec::with_capacity(components.len() + 1);
    path.push(name.to_owned());
    for component in components {
        let bytes = component
            .as_bytes()
            .ok_or(Error::WrongType { field: "path" })?;
        let text = std::str::from_utf8(bytes).map_err(|_| Error::InvalidUtf8 { field: "path" })?;
        validate_component(text, "path")?;
        path.push(text.to_owned());
    }
    Ok(path)
}

fn required_piece_length(info: &Value<'_>) -> Result<u64, Error> {
    let value = info.dict_get(b"piece length").ok_or(Error::MissingField {
        field: "piece length",
    })?;
    let number = value.as_int().ok_or(Error::WrongType {
        field: "piece length",
    })?;
    let length = u64::try_from(number).map_err(|_| Error::ZeroPieceLength)?;
    if length == 0 {
        return Err(Error::ZeroPieceLength);
    }
    if length > MAX_PIECE_LENGTH {
        return Err(Error::PieceLengthTooLarge {
            len: length,
            max: MAX_PIECE_LENGTH,
        });
    }
    Ok(length)
}

fn required_pieces<'a>(info: &'a Value<'a>) -> Result<&'a [u8], Error> {
    let value = info
        .dict_get(b"pieces")
        .ok_or(Error::MissingField { field: "pieces" })?;
    let pieces = value
        .as_bytes()
        .ok_or(Error::WrongType { field: "pieces" })?;
    if pieces.len() % PIECE_HASH_LENGTH != 0 {
        return Err(Error::InvalidPiecesLength { len: pieces.len() });
    }
    Ok(pieces)
}

fn optional_utf8(dict: &Value<'_>, key: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(dict.dict_get(key)?.as_bytes()?).ok()?;
    if text.is_empty() {
        return None;
    }
    Some(text.to_owned())
}

fn announce_list(root: &Value<'_>) -> Vec<Vec<String>> {
    let Some(tiers) = root.dict_get(b"announce-list").and_then(Value::as_list) else {
        return Vec::new();
    };

    let mut result = Vec::new();
    for tier in tiers {
        let Some(entries) = tier.as_list() else {
            continue;
        };
        let urls: Vec<String> = entries
            .iter()
            .filter_map(|entry| std::str::from_utf8(entry.as_bytes()?).ok())
            .filter(|url| !url.is_empty())
            .map(str::to_owned)
            .collect();
        if !urls.is_empty() {
            result.push(urls);
        }
    }
    result
}

#[cfg(test)]
mod tests;
