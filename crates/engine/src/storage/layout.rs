//! Sanitized, collision-free mapping from torrent files to relative paths.
//!
//! This module performs no I/O. Every path it produces is relative and safe to
//! join below a download root.

use std::collections::BTreeSet;
use std::ops::Range;
use std::path::PathBuf;

use sha1::{Digest, Sha1};
use thiserror::Error;

use crate::metainfo::MetaInfo;

/// Maximum size of one sanitized path component, in bytes.
const MAX_COMPONENT_BYTES: usize = 255;

/// Maximum size of the sanitized relative path, in bytes.
const MAX_PATH_BYTES: usize = 1024;

/// Maximum number of collision suffixes tried for one file.
const MAX_COLLISION_ATTEMPTS: u32 = 10_000;

/// Repairs one torrent path component so it is safe on every supported
/// platform.
///
/// The result is non-empty, at most [`MAX_COMPONENT_BYTES`] bytes, free of
/// path separators and Windows-invalid characters, not a reserved device name,
/// and does not end in a dot or space. The function is idempotent.
pub(crate) fn sanitize_component(input: &str) -> String {
    let repaired = repair_component(input);
    if repaired.len() <= MAX_COMPONENT_BYTES {
        return repaired;
    }
    truncate_with_hash(input, &repaired)
}

/// Applies the repair rules before truncation.
fn repair_component(input: &str) -> String {
    if input.is_empty() || input == "." || input == ".." {
        return "_".to_owned();
    }

    let mut output = String::with_capacity(input.len());
    for character in input.chars() {
        let replacement = match character {
            '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            value if value.is_ascii_control() => '_',
            value => value,
        };
        output.push(replacement);
    }

    if is_reserved_name(&output) {
        output.insert(0, '_');
    }

    let trimmed_len = output.trim_end_matches(['.', ' ']).len();
    if trimmed_len < output.len() {
        output.truncate(trimmed_len);
        output.push('_');
    }

    output
}

/// Returns `true` for Windows reserved device names, with or without an
/// extension.
fn is_reserved_name(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or(component);
    let stem = stem.trim_end_matches([' ', '.']);
    let upper = stem.to_ascii_uppercase();

    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
    ) || is_device_name(&upper)
}

/// Returns `true` for `COM`/`LPT` followed by one device number.
///
/// Windows reserves `COM1`-`COM9` and `LPT1`-`LPT9`, plus the superscript forms
/// `COM¹`-`COM³` and `LPT¹`-`LPT³`.
fn is_device_name(upper: &str) -> bool {
    let Some(rest) = upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"))
    else {
        return false;
    };

    let mut characters = rest.chars();
    matches!(
        (characters.next(), characters.next()),
        (Some('1'..='9' | '\u{00b9}' | '\u{00b2}' | '\u{00b3}'), None)
    )
}

/// Truncates a repaired component to the cap and appends a deterministic hash
/// suffix derived from the original input.
fn truncate_with_hash(original: &str, repaired: &str) -> String {
    let digest = Sha1::digest(original.as_bytes());
    let suffix = format!("~{}", hex8(&digest));
    let max_base = MAX_COMPONENT_BYTES.saturating_sub(suffix.len());
    let base = truncate_utf8(repaired, max_base);
    let mut output = String::with_capacity(base.len() + suffix.len());
    output.push_str(base);
    output.push_str(&suffix);
    output
}

/// Truncates `text` to at most `max` bytes without splitting a character.
fn truncate_utf8(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }

    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Formats the first four bytes of a digest as eight lowercase hex characters.
fn hex8(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(8);
    for byte in bytes.iter().take(4) {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

/// Builds the case-insensitive key used to detect collisions.
///
/// Components are sanitized first, so this only needs to fold case and join
/// with `/`. Unicode normalization is deliberately not applied in v0.1.
fn collision_key(components: &[String]) -> String {
    components.join("/").to_lowercase()
}

/// Adds a deterministic `~N` suffix to one component.
///
/// If the component has a dot at an index other than zero and an extension of
/// at most 10 bytes, the suffix goes before the last dot. Otherwise it is
/// appended. The result is truncated to [`MAX_COMPONENT_BYTES`] while keeping
/// the suffix.
fn suffix_component(component: &str, attempt: u32) -> String {
    let suffix = format!("~{attempt}");

    if let Some((stem, extension)) = split_extension(component) {
        let fixed = suffix.len() + 1 + extension.len();
        if fixed < MAX_COMPONENT_BYTES {
            let max_stem = MAX_COMPONENT_BYTES - fixed;
            let stem = truncate_utf8(stem, max_stem);
            let mut output = String::with_capacity(stem.len() + fixed);
            output.push_str(stem);
            output.push_str(&suffix);
            output.push('.');
            output.push_str(extension);
            return output;
        }
    }

    let max_base = MAX_COMPONENT_BYTES.saturating_sub(suffix.len());
    let base = truncate_utf8(component, max_base);
    let mut output = String::with_capacity(base.len() + suffix.len());
    output.push_str(base);
    output.push_str(&suffix);
    output
}

/// Splits a component into stem and extension when the extension rule applies.
fn split_extension(component: &str) -> Option<(&str, &str)> {
    let dot = component.rfind('.')?;
    if dot == 0 {
        return None;
    }

    let extension = &component[dot + 1..];
    if extension.is_empty() || extension.len() > 10 {
        return None;
    }

    Some((&component[..dot], extension))
}

/// A file in the torrent stream and its sanitized relative path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedFile {
    /// Original path components from metainfo, including the torrent name.
    pub original_path: Vec<String>,
    /// Sanitized relative components, including the torrent name.
    pub disk_components: Vec<String>,
    /// File length in bytes.
    pub length: u64,
    /// Byte offset of the file in the torrent stream.
    pub offset: u64,
    /// Half-open range of piece indices touched by the file.
    ///
    /// Empty for zero-length files.
    pub pieces: Range<u32>,
}

impl MappedFile {
    /// Joins the sanitized components into a relative path.
    pub fn to_path(&self) -> PathBuf {
        let mut path = PathBuf::new();
        for component in &self.disk_components {
            path.push(component);
        }
        path
    }
}

/// One contiguous part of a byte range that falls inside a single file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// Index into [`FileMap::files`].
    pub file_index: usize,
    /// Offset from the start of that file.
    pub offset_in_file: u64,
    /// Number of bytes in this segment.
    pub len: u64,
}

/// Immutable, sanitized mapping from torrent files to byte and piece ranges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMap {
    files: Vec<MappedFile>,
    total_length: u64,
    piece_length: u64,
    piece_count: u32,
}

impl FileMap {
    /// Builds the mapping from parsed metainfo.
    ///
    /// This performs no I/O. It re-validates the public `MetaInfo` fields so a
    /// caller cannot bypass the parser's invariants.
    pub fn build(meta: &MetaInfo) -> Result<Self, LayoutError> {
        if meta.files.is_empty() {
            return Err(LayoutError::EmptyFiles);
        }
        if meta.piece_length == 0 {
            return Err(LayoutError::ZeroPieceLength);
        }

        let mut computed_total = 0u64;
        for file in &meta.files {
            computed_total = computed_total
                .checked_add(file.length)
                .ok_or(LayoutError::TotalLengthOverflow)?;
        }
        if computed_total != meta.total_length {
            return Err(LayoutError::LengthMismatch {
                declared: meta.total_length,
                computed: computed_total,
            });
        }

        let piece_count_u64 = meta.total_length.div_ceil(meta.piece_length);
        let found = meta.piece_hashes.len() as u64;
        if piece_count_u64 != found {
            return Err(LayoutError::PieceCountMismatch {
                expected: piece_count_u64,
                found,
            });
        }
        let piece_count =
            u32::try_from(piece_count_u64).map_err(|_| LayoutError::TooManyPieces {
                count: piece_count_u64,
            })?;

        let mut files = Vec::with_capacity(meta.files.len());
        let mut file_keys = BTreeSet::new();
        let mut dir_keys = BTreeSet::new();
        let mut offset = 0u64;

        for (file_index, file) in meta.files.iter().enumerate() {
            if file.path.is_empty() {
                return Err(LayoutError::EmptyPath { file_index });
            }

            let sanitized: Vec<String> = file
                .path
                .iter()
                .map(|component| sanitize_component(component))
                .collect();
            check_path_len(&sanitized)?;
            let (disk_components, key) = resolve_path(&sanitized, &file_keys, &dir_keys)?;
            check_path_len(&disk_components)?;

            file_keys.insert(key);
            for prefix_len in 1..disk_components.len() {
                dir_keys.insert(collision_key(&disk_components[..prefix_len]));
            }

            let first_piece = u32::try_from(offset / meta.piece_length).map_err(|_| {
                LayoutError::TooManyPieces {
                    count: piece_count_u64,
                }
            })?;
            let end_offset = offset
                .checked_add(file.length)
                .ok_or(LayoutError::TotalLengthOverflow)?;
            let pieces = if file.length == 0 {
                first_piece..first_piece
            } else {
                let last_piece =
                    u32::try_from((end_offset - 1) / meta.piece_length).map_err(|_| {
                        LayoutError::TooManyPieces {
                            count: piece_count_u64,
                        }
                    })?;
                let end_piece = last_piece
                    .checked_add(1)
                    .ok_or(LayoutError::TooManyPieces {
                        count: piece_count_u64,
                    })?;
                first_piece..end_piece
            };

            files.push(MappedFile {
                original_path: file.path.clone(),
                disk_components,
                length: file.length,
                offset,
                pieces,
            });
            offset = end_offset;
        }

        Ok(Self {
            files,
            total_length: meta.total_length,
            piece_length: meta.piece_length,
            piece_count,
        })
    }

    /// Builds a map directly for tests that need unsanitized components.
    #[cfg(test)]
    pub(crate) fn from_mapped_files(
        files: Vec<MappedFile>,
        total_length: u64,
        piece_length: u64,
        piece_count: u32,
    ) -> Self {
        Self {
            files,
            total_length,
            piece_length,
            piece_count,
        }
    }

    /// Files in torrent order.
    pub fn files(&self) -> &[MappedFile] {
        &self.files
    }

    /// Total torrent length in bytes.
    pub fn total_length(&self) -> u64 {
        self.total_length
    }

    /// Declared piece length in bytes.
    pub fn piece_length(&self) -> u64 {
        self.piece_length
    }

    /// Number of pieces in the torrent.
    pub fn piece_count(&self) -> u32 {
        self.piece_count
    }

    /// Byte range covered by one piece.
    ///
    /// Returns `None` when `piece` is outside the torrent.
    pub fn piece_byte_range(&self, piece: u32) -> Option<Range<u64>> {
        if piece >= self.piece_count {
            return None;
        }

        let start = u64::from(piece) * self.piece_length;
        let end = start
            .saturating_add(self.piece_length)
            .min(self.total_length);
        Some(start..end)
    }

    /// Splits a torrent byte range into per-file segments.
    ///
    /// The range is clamped to the torrent length. Zero-length files and
    /// out-of-range portions are skipped.
    pub fn segments(&self, range: Range<u64>) -> impl Iterator<Item = Segment> + '_ {
        let start = range.start;
        let end = range.end.min(self.total_length);
        let files: &[MappedFile] = if start < end { &self.files } else { &[] };

        files
            .iter()
            .enumerate()
            .filter_map(move |(file_index, file)| {
                let file_end = file.offset + file.length;
                let segment_start = start.max(file.offset);
                let segment_end = end.min(file_end);
                (segment_start < segment_end).then(|| Segment {
                    file_index,
                    offset_in_file: segment_start - file.offset,
                    len: segment_end - segment_start,
                })
            })
    }

    /// Indices of the files touched by one piece.
    pub fn files_for_piece(&self, piece: u32) -> impl Iterator<Item = usize> + '_ {
        self.piece_byte_range(piece)
            .into_iter()
            .flat_map(move |range| self.segments(range))
            .map(|segment| segment.file_index)
    }

    /// File that contains `offset`, if any.
    pub fn file_for_offset(&self, offset: u64) -> Option<&MappedFile> {
        if offset >= self.total_length {
            return None;
        }

        let index = self
            .files
            .partition_point(|file| file.offset + file.length <= offset);
        let file = self.files.get(index)?;
        (offset >= file.offset && offset < file.offset + file.length).then_some(file)
    }
}

/// Errors produced while building a [`FileMap`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum LayoutError {
    /// Metainfo contains no files.
    #[error("torrent has no files")]
    EmptyFiles,
    /// A file entry has an empty path.
    #[error("file {file_index} has an empty path")]
    EmptyPath {
        /// Index of the offending file.
        file_index: usize,
    },
    /// `piece length` is zero.
    #[error("piece length must be greater than zero")]
    ZeroPieceLength,
    /// The sum of file lengths does not fit in `u64`.
    #[error("total length overflows u64")]
    TotalLengthOverflow,
    /// The declared total length differs from the sum of file lengths.
    #[error("declared total length {declared} does not match file lengths {computed}")]
    LengthMismatch {
        /// `MetaInfo::total_length`.
        declared: u64,
        /// Sum of `FileInfo::length`.
        computed: u64,
    },
    /// The number of piece hashes differs from the piece count.
    #[error("expected {expected} piece hashes, found {found}")]
    PieceCountMismatch {
        /// Piece count implied by the total length and piece length.
        expected: u64,
        /// Number of hashes present in metainfo.
        found: u64,
    },
    /// The piece count does not fit in `u32`.
    #[error("piece count {count} exceeds u32::MAX")]
    TooManyPieces {
        /// Piece count implied by the total length and piece length.
        count: u64,
    },
    /// A sanitized relative path exceeds the cap.
    #[error("relative path is {len} bytes, maximum is {max}: {path:?}")]
    PathTooLong {
        /// Joined sanitized path.
        path: String,
        /// Actual length in bytes.
        len: usize,
        /// Maximum allowed length.
        max: usize,
    },
    /// Collision resolution did not converge.
    #[error("could not resolve a collision for {path:?} after {attempts} attempts")]
    CollisionLimit {
        /// Original sanitized path.
        path: String,
        /// Number of suffixes tried.
        attempts: u32,
    },
}

/// Resolves collisions for one sanitized path.
fn resolve_path(
    sanitized: &[String],
    file_keys: &BTreeSet<String>,
    dir_keys: &BTreeSet<String>,
) -> Result<(Vec<String>, String), LayoutError> {
    let mut attempts = vec![0u32; sanitized.len()];
    let mut total_attempts = 0u32;
    let original_path = sanitized.join("/");

    loop {
        let candidate: Vec<String> = sanitized
            .iter()
            .enumerate()
            .map(|(index, component)| {
                if attempts[index] == 0 {
                    component.clone()
                } else {
                    suffix_component(component, attempts[index])
                }
            })
            .collect();
        let key = collision_key(&candidate);

        let conflict = if file_keys.contains(&key) {
            Some(candidate.len() - 1)
        } else if let Some(index) = first_file_ancestor(&candidate, file_keys) {
            Some(index)
        } else if dir_keys.contains(&key) {
            Some(candidate.len() - 1)
        } else {
            None
        };

        let Some(index) = conflict else {
            return Ok((candidate, key));
        };

        total_attempts =
            total_attempts
                .checked_add(1)
                .ok_or_else(|| LayoutError::CollisionLimit {
                    path: original_path.clone(),
                    attempts: total_attempts,
                })?;
        if total_attempts > MAX_COLLISION_ATTEMPTS {
            return Err(LayoutError::CollisionLimit {
                path: original_path,
                attempts: total_attempts,
            });
        }
        attempts[index] = attempts[index]
            .checked_add(1)
            .ok_or(LayoutError::CollisionLimit {
                path: original_path.clone(),
                attempts: total_attempts,
            })?;
    }
}

/// Returns the component index of the first ancestor that is an existing file.
fn first_file_ancestor(candidate: &[String], file_keys: &BTreeSet<String>) -> Option<usize> {
    for prefix_len in 1..candidate.len() {
        if file_keys.contains(&collision_key(&candidate[..prefix_len])) {
            return Some(prefix_len - 1);
        }
    }
    None
}

/// Enforces the relative path cap.
fn check_path_len(components: &[String]) -> Result<(), LayoutError> {
    let len = components
        .iter()
        .fold(0usize, |acc, component| acc.saturating_add(component.len()))
        .saturating_add(components.len().saturating_sub(1));

    if len > MAX_PATH_BYTES {
        return Err(LayoutError::PathTooLong {
            path: components.join("/"),
            len,
            max: MAX_PATH_BYTES,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
