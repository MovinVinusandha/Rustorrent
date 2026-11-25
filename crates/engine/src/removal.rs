//! Torrent removal: resume data first, then torrent data.
//!
//! The caller must stop downloads, rechecks, and resume saves before asking
//! for removal. The resume document is deleted first; a real I/O failure there
//! does not stop data removal, because the files are the user-visible payload
//! and a leftover document is validated against the layout and ignored when it
//! no longer matches. A save already in progress does stop the data phase: the
//! document is still being written, so removal must not race it.
//!
//! No path is logged here. Reports and errors carry names that callers must
//! treat as user data.

use crate::metainfo::InfoHash;
use crate::resume::{RemoveError, ResumeRemoval, ResumeStore};
use crate::storage::RemovalReport;
use crate::storage::thread::{StorageHandle, StorageHandleError};

/// Result of one torrent removal.
#[derive(Debug)]
pub struct RemoveOutcome {
    /// Resume document removal. A failure here does not stop data removal.
    pub resume: Result<ResumeRemoval, RemoveError>,
    /// Data removal report, or `None` when it was not requested or a save was
    /// still running.
    pub data: Option<Result<RemovalReport, StorageHandleError>>,
}

impl RemoveOutcome {
    /// Returns `true` when resume removal succeeded and every data item that
    /// was attempted succeeded. A skipped data phase counts as success only
    /// when it was not requested.
    pub fn is_success(&self) -> bool {
        if self.resume.is_err() {
            return false;
        }
        match &self.data {
            None => true,
            Some(Ok(report)) => !report.has_failures(),
            Some(Err(_)) => false,
        }
    }
}

/// Deletes resume data and, when requested, every mapped file and owned
/// directory for one torrent.
///
/// The resume document and its temps are removed first. A real I/O error
/// there still attempts the data phase when `delete_files` is `true`;
/// [`RemoveError::AlreadySaving`] does not, because a save is still writing
/// the document. Both outcomes are returned even when one fails.
pub async fn remove_torrent(
    resume: &ResumeStore,
    storage: &StorageHandle,
    info_hash: InfoHash,
    delete_files: bool,
) -> RemoveOutcome {
    let resume = resume.remove(info_hash).await;

    let data = if delete_files && !matches!(resume, Err(RemoveError::AlreadySaving)) {
        Some(storage.remove_data().await)
    } else {
        None
    };

    RemoveOutcome { resume, data }
}

#[cfg(test)]
mod tests;
