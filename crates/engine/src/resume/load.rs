//! Loading resume documents from disk.
//!
//! A missing resume file never prevents a torrent from starting: absence is
//! not an error, and a corrupt file is reported as a typed decode error so the
//! caller can fall back to a full recheck. The size limit is checked from the
//! file metadata before any read, and the read itself is capped in case the
//! file grows.

use std::io::Read;
use std::path::Path;

use thiserror::Error;

use super::{DecodeError, MAX_RESUME_BYTES, ResumeDocument, ResumeStore, decode_document};
use crate::metainfo::InfoHash;

/// Errors returned while loading a resume document.
#[derive(Debug, Error)]
pub enum LoadError {
    /// The document could not be read.
    #[error("could not read resume document {name}: {source}")]
    Io {
        /// Document file name, never a full path.
        name: String,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The document is oversized, malformed, or not a supported version.
    #[error(transparent)]
    Decode(#[from] DecodeError),
    /// The blocking load task was cancelled or panicked.
    #[error("resume load task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

impl ResumeStore {
    /// Loads and decodes the document for `info_hash`.
    ///
    /// Returns `Ok(None)` when the file does not exist. The document is not
    /// checked against a torrent here; [`plan_restore`](super::plan_restore)
    /// does that before any bit is trusted.
    pub async fn load(&self, info_hash: &InfoHash) -> Result<Option<ResumeDocument>, LoadError> {
        let path = self.document_path(info_hash);
        let name = self.document_name(info_hash);
        tokio::task::spawn_blocking(move || read_document(&path, &name))
            .await
            .map_err(LoadError::Task)?
    }
}

/// Reads a document on a blocking thread and decodes it.
fn read_document(path: &Path, name: &str) -> Result<Option<ResumeDocument>, LoadError> {
    let io_error = |source| LoadError::Io {
        name: name.to_owned(),
        source,
    };
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io_error(source)),
    };

    let max = u64::try_from(MAX_RESUME_BYTES).unwrap_or(u64::MAX);
    let metadata = file.metadata().map_err(io_error)?;
    if metadata.len() > max {
        return Err(LoadError::Decode(DecodeError::TooLarge {
            len: usize::try_from(metadata.len()).unwrap_or(usize::MAX),
            max: MAX_RESUME_BYTES,
        }));
    }

    let mut bytes = Vec::new();
    file.take(max.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() > MAX_RESUME_BYTES {
        return Err(LoadError::Decode(DecodeError::TooLarge {
            len: bytes.len(),
            max: MAX_RESUME_BYTES,
        }));
    }

    Ok(Some(decode_document(&bytes)?))
}

#[cfg(test)]
mod tests;
