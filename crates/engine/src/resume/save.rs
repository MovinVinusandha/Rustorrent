//! Atomic resume saves with one writer per torrent.
//!
//! A save runs in this order:
//!
//! 1. reject a snapshot whose piece count does not match the torrent;
//! 2. take the per-torrent save lease;
//! 3. flush dirty file data through storage;
//! 4. observe size and modification time of every mapped file;
//! 5. encode a document that carries the caller's snapshot;
//! 6. create the resume directory, sweep stale temp files, and write the
//!    document through [`bt_platform::file::write_atomic`].
//!
//! The caller passes the bitfield snapshot by value. Pieces completed after
//! the snapshot was taken are simply absent from this document. Because the
//! flush starts after the snapshot, every recorded bit refers to bytes written
//! before the flush began, so a crash after the rename cannot leave a document
//! that claims a piece is verified while its bytes might still be lost. The
//! reverse order would record bits set while the flush was already running.
//!
//! A file changed after the flush but before it is observed gets the new size
//! and modification time, so the next restore rechecks its pieces. A change
//! that preserves both size and modification time is only caught by the
//! restore spot check. A crash between the data flush and the resume rename
//! leaves the previous document, or no document at all. The next restore then
//! rechecks the pieces that were not already verified. For a single-file
//! torrent that write also changes the file modification time, so the whole
//! torrent is rechecked.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use thiserror::Error;

use super::{
    EncodeError, MTIME_UNAVAILABLE, ResumeDocument, ResumeFile, encode_document, layout_fingerprint,
};
use crate::bitfield::Bitfield;
use crate::metainfo::InfoHash;
use crate::storage::ObservedFile;
use crate::storage::layout::FileMap;
use crate::storage::thread::{StorageHandle, StorageHandleError};

/// Resume files for one directory, with a per-torrent save lease.
///
/// The directory is chosen by the caller. The engine never picks an OS data
/// directory, and documents never contain absolute paths.
#[derive(Debug)]
pub struct ResumeStore {
    dir: PathBuf,
    saving: Arc<Mutex<BTreeSet<InfoHash>>>,
}

impl ResumeStore {
    /// Creates a store that writes into `dir`.
    ///
    /// The directory is created by the first successful save, not here.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            saving: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    /// Returns the directory that holds the resume documents.
    pub fn directory(&self) -> &Path {
        &self.dir
    }

    /// Returns the path of the resume document for `info_hash`.
    ///
    /// The file name is the lowercase hex info-hash plus `.resume`. The
    /// temporary file used by an atomic write adds
    /// `.tmp.<pid>.<counter>.<nanos>`; saves sweep stale temps.
    pub fn document_path(&self, info_hash: &InfoHash) -> PathBuf {
        self.dir.join(self.document_name(info_hash))
    }

    /// Returns the document file name for `info_hash`.
    ///
    /// Every error that mentions a document uses this name, never a path.
    pub(super) fn document_name(&self, info_hash: &InfoHash) -> String {
        format!("{info_hash}.resume")
    }

    /// Flushes storage, observes every file, and atomically writes a document.
    ///
    /// `snapshot` must be taken before this call and must describe the pieces
    /// whose data has already been written. It is consumed so the document
    /// cannot observe later changes to the same bitfield.
    ///
    /// Only one save per info-hash can run on this store at a time. A second
    /// caller gets [`SaveError::AlreadySaving`] without touching storage. The
    /// lease is owned by the blocking write task, so dropping the save future
    /// cannot admit a concurrent writer while that write is still running.
    ///
    /// A failure before the rename leaves the previous document in place. A
    /// failure during the rename leaves either the old or the new document,
    /// never a mixture. Data files are never deleted or modified here.
    pub async fn save(
        &self,
        info_hash: InfoHash,
        map: &FileMap,
        snapshot: Bitfield,
        storage: &StorageHandle,
    ) -> Result<(), SaveError> {
        let found = snapshot.piece_count();
        let expected = map.piece_count();
        if found != expected {
            return Err(SaveError::PieceCount { expected, found });
        }

        let lease = self.acquire(info_hash).ok_or(SaveError::AlreadySaving)?;

        storage.sync().await?;
        let observed = storage.observe_files().await?;
        let files = resume_files(map, &observed)?;
        let document = ResumeDocument::new(
            *info_hash.as_bytes(),
            map.piece_length(),
            map.total_length(),
            layout_fingerprint(map),
            snapshot,
            files,
            now_seconds(),
        );
        let bytes = encode_document(&document)?;

        let dir = self.dir.clone();
        let path = self.document_path(&info_hash);
        let name = self.document_name(&info_hash);
        let blocking = tokio::task::spawn_blocking(move || {
            let result = write_document(&dir, &path, &name, &bytes);
            drop(lease);
            result
        })
        .await
        .map_err(SaveError::Task)?;
        blocking.map_err(|error| match error {
            BlockingError::CreateDir(source) => SaveError::CreateDir(source),
            BlockingError::Write { name, source } => SaveError::Write { name, source },
        })?;

        Ok(())
    }

    /// Deletes the resume document and this torrent's leftover temp files.
    ///
    /// The save lease is taken first, so a concurrent save yields
    /// [`RemoveError::AlreadySaving`] instead of waiting. A missing document
    /// or resume directory is a success. The document is removed first; a
    /// real I/O error is returned and temp files are left alone. A temp file
    /// that is not a regular file is never touched.
    ///
    /// Errors name the document file, never a full path. Callers must treat
    /// that name as user data and not log it without consent.
    pub async fn remove(&self, info_hash: InfoHash) -> Result<ResumeRemoval, RemoveError> {
        let lease = self.acquire(info_hash).ok_or(RemoveError::AlreadySaving)?;
        let dir = self.dir.clone();
        let name = self.document_name(&info_hash);
        tokio::task::spawn_blocking(move || {
            let result = remove_document(&dir, &name);
            drop(lease);
            result
        })
        .await
        .map_err(RemoveError::Task)?
    }

    /// Reserves the save slot for one torrent.
    fn acquire(&self, info_hash: InfoHash) -> Option<SaveLease> {
        let mut saving = self.saving.lock().unwrap_or_else(PoisonError::into_inner);
        if !saving.insert(info_hash) {
            return None;
        }
        drop(saving);

        Some(SaveLease {
            saving: Arc::clone(&self.saving),
            info_hash,
        })
    }

    /// Holds the save slot for one torrent. Test-only.
    #[cfg(test)]
    pub(crate) fn hold_save_lease(&self, info_hash: InfoHash) -> SaveLease {
        self.acquire(info_hash).expect("test save lease")
    }
}

/// Holds the save slot until the blocking write finishes or is dropped.
pub(crate) struct SaveLease {
    saving: Arc<Mutex<BTreeSet<InfoHash>>>,
    info_hash: InfoHash,
}

impl Drop for SaveLease {
    fn drop(&mut self) {
        let mut saving = self.saving.lock().unwrap_or_else(PoisonError::into_inner);
        saving.remove(&self.info_hash);
    }
}

/// Errors returned while saving a resume document.
#[derive(Debug, Error)]
pub enum SaveError {
    /// Another save is already running for this torrent on this store.
    #[error("a resume save is already running for this torrent")]
    AlreadySaving,
    /// The snapshot does not describe this torrent.
    #[error("snapshot has {found} pieces, expected {expected}")]
    PieceCount {
        /// Torrent piece count.
        expected: u32,
        /// Snapshot piece count.
        found: u32,
    },
    /// Storage returned a different number of observations than mapped files.
    #[error("observed {found} files, expected {expected}")]
    ObservedFileCount {
        /// Number of files in the map.
        expected: usize,
        /// Number of observations.
        found: usize,
    },
    /// The document exceeds the encoding limits.
    #[error("could not encode resume data: {0}")]
    Encode(#[from] EncodeError),
    /// Storage failed or is shutting down, so nothing was written.
    #[error("storage failed: {0}")]
    Storage(#[from] StorageHandleError),
    /// The resume directory could not be created.
    #[error("could not create resume directory: {0}")]
    CreateDir(#[source] io::Error),
    /// The document could not be written or renamed into place.
    #[error("could not write resume document {name}: {source}")]
    Write {
        /// Document file name, never a full path.
        name: String,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// The blocking write task was cancelled or panicked.
    #[error("resume save task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

/// Outcome of deleting one torrent's resume data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeRemoval {
    /// `true` when the document itself existed and was deleted.
    pub document_removed: bool,
    /// Number of leftover temp files deleted for this torrent.
    pub temps_removed: usize,
}

/// Errors returned while removing resume data.
#[derive(Debug, Error)]
pub enum RemoveError {
    /// Another save or removal is already running for this torrent.
    #[error("a resume save is already running for this torrent")]
    AlreadySaving,
    /// The document could not be deleted.
    #[error("could not remove resume document {name}: {source}")]
    Remove {
        /// Document file name, never a full path.
        name: String,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// The blocking removal task was cancelled or panicked.
    #[error("resume removal task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

/// One file entry per mapped file, in map order.
fn resume_files(map: &FileMap, observed: &[ObservedFile]) -> Result<Vec<ResumeFile>, SaveError> {
    let found = observed.len();
    let expected = map.files().len();
    if found != expected {
        return Err(SaveError::ObservedFileCount { expected, found });
    }

    Ok(map
        .files()
        .iter()
        .zip(observed)
        .map(|(mapped, observed)| match observed {
            ObservedFile::Present { size, mtime_ns } => {
                ResumeFile::new(mapped.length, *size, mtime_ns.unwrap_or(MTIME_UNAVAILABLE))
            }
            ObservedFile::Missing => ResumeFile::new(mapped.length, 0, MTIME_UNAVAILABLE),
        })
        .collect())
}

/// Returns the current Unix time in seconds, or `0` when the clock is before
/// the epoch or does not fit in `i64`.
fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or(0)
}

/// Error from the blocking part of a save.
enum BlockingError {
    /// The resume directory could not be created.
    CreateDir(io::Error),
    /// The document could not be written atomically.
    Write {
        /// Document file name, never a full path.
        name: String,
        /// Underlying I/O error.
        source: io::Error,
    },
}

/// Age after which a temp file from an interrupted save is swept.
const STALE_TEMP_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Creates the resume directory, sweeps stale temps, and writes the document.
fn write_document(dir: &Path, path: &Path, name: &str, bytes: &[u8]) -> Result<(), BlockingError> {
    std::fs::create_dir_all(dir).map_err(BlockingError::CreateDir)?;
    sweep_stale_temps(dir);
    bt_platform::file::write_atomic(path, bytes).map_err(|source| BlockingError::Write {
        name: name.to_owned(),
        source,
    })
}

/// Deletes the document, then this torrent's temp files.
fn remove_document(dir: &Path, name: &str) -> Result<ResumeRemoval, RemoveError> {
    let document_removed = match std::fs::remove_file(dir.join(name)) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(source) => {
            return Err(RemoveError::Remove {
                name: name.to_owned(),
                source,
            });
        }
    };

    Ok(ResumeRemoval {
        document_removed,
        temps_removed: remove_temps(dir, name),
    })
}

/// Deletes regular temp files that belong to exactly this document.
///
/// Best-effort: removal errors and unreadable entries are ignored, matching
/// the stale-temp sweep. A symlink or directory with a temp name is skipped.
fn remove_temps(dir: &Path, document: &str) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;

    for entry in entries.flatten() {
        if !is_temp_for(&entry.file_name(), document) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        if std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }

    removed
}

/// Removes temp files left behind by an interrupted atomic write.
///
/// Only regular files named `<document>.resume.tmp.<pid>.<counter>.<nanos>`
/// and last modified more than 24 hours ago are removed. Removal errors and
/// unreadable entries are ignored: a leftover temp never blocks a save, and
/// the directory may hold the documents and files of other torrents.
fn sweep_stale_temps(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        if !is_resume_temp(&entry.file_name()) {
            continue;
        }
        // `DirEntry::metadata` does not follow a symlink at the entry.
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        if age >= STALE_TEMP_AGE {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Returns `true` for `<document>.resume.tmp.<pid>.<counter>.<nanos>`.
fn is_resume_temp(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some((document, suffix)) = name.rsplit_once(".resume.tmp.") else {
        return false;
    };
    !document.is_empty() && is_temp_suffix(suffix)
}

/// Returns `true` when `name` is a temp file of exactly `document`.
fn is_temp_for(name: &OsStr, document: &str) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(suffix) = name
        .strip_prefix(document)
        .and_then(|rest| rest.strip_prefix(".tmp."))
    else {
        return false;
    };
    is_temp_suffix(suffix)
}

/// Returns `true` for the `<pid>.<counter>.<nanos>` part of a temp name.
fn is_temp_suffix(suffix: &str) -> bool {
    let mut parts = suffix.split('.');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(pid), Some(counter), Some(nanos), None)
            if is_digits(pid) && is_digits(counter) && is_digits(nanos)
    )
}

/// Returns `true` for a non-empty run of ASCII digits.
fn is_digits(part: &str) -> bool {
    !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests;
