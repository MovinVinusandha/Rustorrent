//! Storage backend: validates torrent paths, creates files, and moves bytes.
//!
//! The backend first resolves every torrent-derived path below the root and
//! rejects symlinks, reparse points, wrong file types, unsanitized components,
//! and duplicate inodes. It then creates directories, eager zero-length files,
//! and preallocated files according to [`Preallocation`].
//!
//! Range reads and writes are positioned, so one call never depends on a file
//! cursor. Handles live in a strict LRU cache; evicted dirty files are flushed
//! by [`Storage::sync`] and [`Storage::close`].

use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bt_platform::file::{File, create_new, open_read, open_rw, open_write};
use thiserror::Error;

use super::layout::{FileMap, MappedFile, sanitize_component};

/// Default maximum number of open file handles.
pub const DEFAULT_MAX_OPEN_HANDLES: usize = 64;

/// Maximum number of files observed by one storage worker command.
///
/// Resume saves observe every file. Chunking keeps a large torrent from
/// occupying the worker queue for the whole pass, so reads and writes for
/// other torrents can interleave.
pub const OBSERVE_CHUNK_FILES: usize = 256;

/// Maximum number of items processed by one removal worker command.
pub const REMOVE_CHUNK_ITEMS: usize = 256;

/// Maximum number of paths kept in one [`RemovalReport`] list.
///
/// The matching `*_count` field is always exact, even when this cap truncates
/// the list and sets [`RemovalReport::truncated`].
pub const MAX_REPORT_PATHS: usize = 1000;

/// Outcome of removing one torrent from disk.
///
/// Every path is relative to the download root and sanitized before it is
/// reported. Paths and file names are user data: callers must not log them
/// without the user's consent.
#[derive(Debug, Default)]
pub struct RemovalReport {
    /// Files and directories that were removed, capped at
    /// [`MAX_REPORT_PATHS`].
    pub removed: Vec<PathBuf>,
    /// Paths that were already absent, capped at [`MAX_REPORT_PATHS`].
    pub already_missing: Vec<PathBuf>,
    /// Directories left in place because they still contain files. This is a
    /// success, not a failure.
    pub not_empty: Vec<PathBuf>,
    /// Items that could not be removed, capped at [`MAX_REPORT_PATHS`].
    pub failed: Vec<RemovalFailure>,
    /// Exact number of removed items.
    pub removed_count: usize,
    /// Exact number of already-missing items.
    pub already_missing_count: usize,
    /// Exact number of non-empty directories that were kept.
    pub not_empty_count: usize,
    /// Exact number of failed items.
    pub failed_count: usize,
    /// `true` when at least one list was capped.
    pub truncated: bool,
}

impl RemovalReport {
    /// Returns `true` when at least one item failed.
    pub fn has_failures(&self) -> bool {
        self.failed_count != 0
    }

    pub(crate) fn record(&mut self, item: RemovalItemOutcome) {
        match item {
            RemovalItemOutcome::Removed(path) => {
                self.removed_count = self.removed_count.saturating_add(1);
                push_path(&mut self.removed, path, &mut self.truncated);
            }
            RemovalItemOutcome::AlreadyMissing(path) => {
                self.already_missing_count = self.already_missing_count.saturating_add(1);
                push_path(&mut self.already_missing, path, &mut self.truncated);
            }
            RemovalItemOutcome::NotEmpty(path) => {
                self.not_empty_count = self.not_empty_count.saturating_add(1);
                push_path(&mut self.not_empty, path, &mut self.truncated);
            }
            RemovalItemOutcome::Failed { path, error } => {
                self.failed_count = self.failed_count.saturating_add(1);
                if self.failed.len() < MAX_REPORT_PATHS {
                    self.failed.push(RemovalFailure { path, error });
                } else {
                    self.truncated = true;
                }
            }
        }
    }
}

/// One failed removal, with a path relative to the download root.
#[derive(Debug)]
pub struct RemovalFailure {
    /// Relative path that could not be removed.
    pub path: PathBuf,
    /// Why the removal failed.
    pub error: RemovalError,
}

/// Typed reason a single removal item failed.
#[derive(Debug, Error)]
pub enum RemovalError {
    /// A path failed a safety check.
    #[error("unsafe path: {0}")]
    Unsafe(UnsafeReason),
    /// Permission was denied.
    #[error("permission denied: {0}")]
    PermissionDenied(#[source] io::Error),
    /// The file is locked or in use by another process.
    #[error("file is in use: {0}")]
    FileInUse(#[source] io::Error),
    /// Another I/O operation failed.
    #[error("I/O error: {0}")]
    Io(#[source] io::Error),
    /// An internal invariant was violated.
    #[error("internal removal invariant: {reason}")]
    Internal {
        /// Which invariant.
        reason: &'static str,
    },
}

pub(crate) enum RemovalItemOutcome {
    Removed(PathBuf),
    AlreadyMissing(PathBuf),
    NotEmpty(PathBuf),
    Failed { path: PathBuf, error: RemovalError },
}

impl RemovalItemOutcome {
    fn is_failure(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

pub(crate) struct RemovalChunk {
    pub items: Vec<RemovalItemOutcome>,
    pub done: bool,
}

fn push_path(list: &mut Vec<PathBuf>, path: PathBuf, truncated: &mut bool) {
    if list.len() < MAX_REPORT_PATHS {
        list.push(path);
    } else {
        *truncated = true;
    }
}

/// Metadata observed for one mapped file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservedFile {
    /// The file exists and is a regular file.
    Present {
        /// Current file size in bytes.
        size: u64,
        /// Modification time in nanoseconds since the Unix epoch.
        ///
        /// `None` means the platform could not report it or the value does not
        /// fit in `i64`; callers must treat the file as unverifiable.
        mtime_ns: Option<i64>,
    },
    /// The file or one of its parent directories does not exist.
    Missing,
}

/// Preallocation policy for newly created files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Preallocation {
    /// Do not extend files ahead of writes.
    #[default]
    Off,
    /// Extend each file to its expected length when it is created.
    SetLen,
}

/// Storage configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageConfig {
    /// Maximum number of open file handles.
    pub max_open_handles: usize,
    /// Preallocation policy.
    pub preallocation: Preallocation,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            max_open_handles: DEFAULT_MAX_OPEN_HANDLES,
            preallocation: Preallocation::Off,
        }
    }
}

/// A validated torrent storage layout rooted at an absolute directory.
///
/// Dirty data is flushed by [`Storage::sync`] and [`Storage::close`]. Dropping
/// a `Storage` performs no sync and loses any pending flush, so a caller that
/// needs durability must call `sync` or `close` explicitly.
#[derive(Debug)]
pub struct Storage {
    root: PathBuf,
    map: Arc<FileMap>,
    config: StorageConfig,
    files: Vec<PreparedFile>,
    seen_inodes: BTreeSet<FileId>,
    handles: HandleCache,
    created_dirs: CreatedDirs,
    removed: bool,
    removal_failed: bool,
    removal_plan: Option<Vec<Vec<String>>>,
}

/// Directories this storage instance created below the download root.
///
/// Removal only offers these for deletion. The set is empty after a process
/// restart, where ownership is unknown and removal falls back to the
/// structural layout instead.
#[derive(Debug, Default)]
struct CreatedDirs {
    dirs: BTreeSet<Vec<String>>,
}

impl CreatedDirs {
    fn record(&mut self, components: &[String]) {
        self.dirs.insert(components.to_vec());
    }

    fn is_empty(&self) -> bool {
        self.dirs.is_empty()
    }

    fn names(&self) -> &BTreeSet<Vec<String>> {
        &self.dirs
    }
}

impl Storage {
    /// Validates the root and every existing torrent-derived path.
    ///
    /// Missing paths are accepted; they are created later. The root itself may
    /// be a symlink, but every component below it is checked.
    pub fn open(
        root: &Path,
        map: Arc<FileMap>,
        config: StorageConfig,
    ) -> Result<Self, StorageError> {
        if !root.is_absolute() {
            return Err(StorageError::InvalidConfig {
                reason: "download root must be an absolute path",
            });
        }
        if config.max_open_handles == 0 {
            return Err(StorageError::InvalidConfig {
                reason: "max_open_handles must be at least 1",
            });
        }

        let root_metadata = fs::metadata(root).map_err(|source| classify(root, source))?;
        if !root_metadata.is_dir() {
            return Err(StorageError::UnsafePath {
                path: root.to_path_buf(),
                reason: UnsafeReason::NotDirectory,
            });
        }

        let mut files = Vec::with_capacity(map.files().len());
        let mut seen_inodes = BTreeSet::new();
        let mut created_dirs = CreatedDirs::default();

        for file in map.files() {
            let path = resolve_file_path(root, file)?;
            let (existed, id) = match fs::symlink_metadata(&path) {
                Ok(metadata) => {
                    check_file(&path, &metadata)?;
                    record_inode(&path, &metadata, &mut seen_inodes)?;
                    (true, file_id(&metadata))
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => (false, None),
                Err(source) => return Err(classify(&path, source)),
            };

            files.push(PreparedFile { path, existed, id });
        }

        prepare_files(
            root,
            map.as_ref(),
            &config,
            &mut files,
            &mut seen_inodes,
            &mut created_dirs,
        )?;
        let handles = HandleCache::new(config.max_open_handles)?;

        Ok(Self {
            root: root.to_path_buf(),
            map,
            config,
            files,
            seen_inodes,
            handles,
            created_dirs,
            removed: false,
            removal_failed: false,
            removal_plan: None,
        })
    }

    /// Download root used by this storage.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// File map used by this storage.
    pub fn map(&self) -> &FileMap {
        self.map.as_ref()
    }

    /// Storage configuration.
    pub fn config(&self) -> &StorageConfig {
        &self.config
    }

    /// Number of files in the torrent.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Absolute path of one file, if the index is valid.
    pub fn file_path(&self, index: usize) -> Option<&Path> {
        self.files.get(index).map(|file| file.path.as_path())
    }

    /// Reads up to `buf.len()` bytes starting at `offset`.
    ///
    /// Returns the number of leading bytes that are available. Reading stops
    /// at the first missing or short file, so a gap is never skipped. A file
    /// longer than its mapped length contributes only the mapped length.
    /// Returns zero when `offset` is at or past the end of the torrent.
    ///
    /// A storage marked removed rejects every data operation, so a stale
    /// handle cannot read through paths that no longer exist.
    pub fn read_range(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize, StorageError> {
        if self.removed {
            return Err(StorageError::Removed);
        }
        if buf.is_empty() {
            return Ok(0);
        }

        let map = Arc::clone(&self.map);
        let total_length = map.total_length();
        if offset >= total_length {
            return Ok(0);
        }

        let available = total_length - offset;
        let requested = (buf.len() as u64).min(available);
        let end = offset
            .checked_add(requested)
            .ok_or(StorageError::OutOfRange {
                offset,
                len: requested,
                total_length,
            })?;

        let mut filled = 0usize;
        for segment in map.segments(offset..end) {
            let index = segment.file_index;
            let path = self
                .files
                .get(index)
                .map(|file| file.path.clone())
                .ok_or_else(|| internal("file index is outside the file map"))?;
            let mapped = map
                .files()
                .get(index)
                .ok_or_else(|| internal("file index is outside the file map"))?;

            if !check_ancestors(&self.root, &mapped.disk_components)? {
                break;
            }

            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Err(source) => return Err(classify(&path, source)),
            };
            check_file(&path, &metadata)?;

            let file_len = metadata.len();
            let expected = file_id(&metadata);
            if segment.offset_in_file >= file_len {
                break;
            }

            let available_in_file = file_len - segment.offset_in_file;
            let remaining =
                requested
                    .checked_sub(filled as u64)
                    .ok_or(StorageError::OutOfRange {
                        offset,
                        len: requested,
                        total_length,
                    })?;
            let to_read = segment.len.min(available_in_file).min(remaining);
            let to_read = usize::try_from(to_read).map_err(|_| StorageError::OutOfRange {
                offset,
                len: requested,
                total_length,
            })?;
            if to_read == 0 {
                break;
            }
            let read_end = filled
                .checked_add(to_read)
                .ok_or_else(|| internal("read cursor overflowed the buffer"))?;
            let target = buf
                .get_mut(filled..read_end)
                .ok_or_else(|| internal("read range is outside the buffer"))?;

            // Reads never require write access. A pending sync upgrades the
            // cached handle to read-write when the flush actually runs. A file
            // removed after the metadata check is treated like any other
            // missing file: stop at this boundary so the caller sees a short
            // read and the covered pieces are absent.
            let file = match self
                .handles
                .get_or_open(index, &path, open_read, false, false, expected)
            {
                Ok(file) => file,
                Err(StorageError::Io { source, .. })
                    if source.kind() == io::ErrorKind::NotFound =>
                {
                    break;
                }
                Err(error) => return Err(error),
            };

            let read = read_up_to(
                |chunk, position| file.read_at(chunk, position),
                target,
                segment.offset_in_file,
            )
            .map_err(|source| classify(&path, source))?;
            filled = filled
                .checked_add(read)
                .ok_or_else(|| internal("read count is outside the buffer"))?;

            if read < to_read || (to_read as u64) < segment.len {
                break;
            }
        }

        Ok(filled)
    }

    /// Writes all bytes starting at `offset`.
    ///
    /// The range must end at or before the torrent length; ranges that overflow
    /// `u64` or exceed the torrent are rejected. Missing files and parents are
    /// created lazily when [`Preallocation::Off`] is selected.
    ///
    /// If this returns an error, part of the range may already be on disk.
    /// Callers must set verified state only after a complete successful call.
    ///
    /// A storage marked removed rejects writes, so a stale handle cannot
    /// recreate files or directories that were just deleted.
    pub fn write_range(&mut self, offset: u64, buf: &[u8]) -> Result<(), StorageError> {
        if self.removed {
            return Err(StorageError::Removed);
        }
        if buf.is_empty() {
            return Ok(());
        }

        let map = Arc::clone(&self.map);
        let total_length = map.total_length();
        let len = buf.len() as u64;
        let end = offset.checked_add(len).ok_or(StorageError::OutOfRange {
            offset,
            len,
            total_length,
        })?;
        if end > total_length {
            return Err(StorageError::OutOfRange {
                offset,
                len,
                total_length,
            });
        }

        let mut written = 0usize;
        for segment in map.segments(offset..end) {
            let index = segment.file_index;
            let expected = self.ensure_writable_file(index)?;
            let path = self
                .files
                .get(index)
                .map(|file| file.path.clone())
                .ok_or_else(|| internal("file index is outside the file map"))?;

            let remaining = buf
                .len()
                .checked_sub(written)
                .ok_or(StorageError::OutOfRange {
                    offset,
                    len,
                    total_length,
                })?;
            let to_write = segment.len.min(u64::try_from(remaining).map_err(|_| {
                StorageError::OutOfRange {
                    offset,
                    len,
                    total_length,
                }
            })?);
            let to_write = usize::try_from(to_write).map_err(|_| StorageError::OutOfRange {
                offset,
                len,
                total_length,
            })?;
            let write_end = written
                .checked_add(to_write)
                .ok_or_else(|| internal("write cursor overflowed the buffer"))?;
            let source = buf
                .get(written..write_end)
                .ok_or_else(|| internal("write range is outside the buffer"))?;

            let file = self
                .handles
                .get_or_open(index, &path, open_write, true, true, expected)?;
            write_all_at(
                |chunk, position| file.write_at(chunk, position),
                source,
                segment.offset_in_file,
            )
            .map_err(|source| classify(&path, source))?;
            written = write_end;
        }

        Ok(())
    }

    /// Flushes dirty data for cached and evicted files.
    ///
    /// Evicted dirty files are reopened read-write after their ancestors are
    /// re-checked and synced. A file that was removed after eviction is a typed
    /// error.
    ///
    /// A removed storage has no files left to flush, so this returns `Ok(())`
    /// and lets shutdown complete.
    pub fn sync(&mut self) -> Result<(), StorageError> {
        if self.removed {
            return Ok(());
        }
        self.handles
            .sync(&self.root, self.map.as_ref(), &self.files)
    }

    /// Observes size and modification time for a slice of the file map.
    ///
    /// `start` is clamped to the map length and `count` bounds the number of
    /// files, so a chunk can never read outside the map. Missing files are
    /// reported as [`ObservedFile::Missing`], not as errors. Ancestors and the
    /// final path are checked with the same symlink rules as every other
    /// storage operation; a symlinked file is a typed error.
    ///
    /// A storage marked removed rejects observes instead of reporting every
    /// file as missing.
    pub fn observe_files(
        &self,
        start: usize,
        count: usize,
    ) -> Result<Vec<ObservedFile>, StorageError> {
        if self.removed {
            return Err(StorageError::Removed);
        }
        let files = self.map.files();
        let begin = start.min(files.len());
        let end = begin.saturating_add(count).min(files.len());
        let mut observed = Vec::with_capacity(end - begin);

        for file in &files[begin..end] {
            observed.push(observe_file(&self.root, file)?);
        }

        Ok(observed)
    }

    /// Starts a removal pass.
    ///
    /// Clears the previous pass's failure mark and the cached directory plan,
    /// so a retry after a partial failure looks at the current filesystem.
    pub(crate) fn begin_removal(&mut self) {
        self.removal_failed = false;
        self.removal_plan = None;
    }

    /// Ends a removal pass and marks the storage removed when it was clean.
    ///
    /// Returns `true` when the storage is now marked removed. A pass with any
    /// failed item leaves it usable so the caller can retry.
    pub(crate) fn finish_removal(&mut self) -> bool {
        if !self.removal_failed {
            self.removed = true;
        }
        self.removed
    }

    /// Removes one chunk of mapped files.
    ///
    /// `start` is clamped to the map length and `count` bounds the number of
    /// files. Every item produces exactly one outcome; a failure never stops
    /// the rest of the chunk. Missing files are reported as already missing.
    pub(crate) fn remove_files(
        &mut self,
        start: usize,
        count: usize,
    ) -> Result<RemovalChunk, StorageError> {
        let map = Arc::clone(&self.map);
        let total = map.files().len();
        let begin = start.min(total);
        let end = begin.saturating_add(count).min(total);
        let mut items = Vec::with_capacity(end.saturating_sub(begin));

        for index in begin..end {
            let item = self.remove_file_item(&map, index);
            if item.is_failure() {
                self.removal_failed = true;
            }
            items.push(item);
        }

        Ok(RemovalChunk {
            items,
            done: end == total,
        })
    }

    /// Removes one chunk of directories left by a torrent.
    ///
    /// Only directories this storage instance created are offered, always a
    /// child before its parent. After a restart, when ownership is unknown, the
    /// torrent's own top-level folder and its subfolders are offered instead. A
    /// non-empty directory is kept and reported as a success, so files this
    /// client did not create are never touched.
    ///
    /// Tracked directories are iterated from the creation set in reverse
    /// lexicographic order, which puts every child before its parent without
    /// materializing a second copy of the plan. The structural fallback is the
    /// only case that needs a collected plan.
    pub(crate) fn remove_directories(
        &mut self,
        start: usize,
        count: usize,
    ) -> Result<RemovalChunk, StorageError> {
        if self.created_dirs.is_empty() && self.removal_plan.is_none() {
            self.removal_plan = Some(self.build_structural_directory_plan());
        }
        let total = if self.created_dirs.is_empty() {
            self.removal_plan.as_ref().map_or(0, Vec::len)
        } else {
            self.created_dirs.names().len()
        };
        let begin = start.min(total);
        let end = begin.saturating_add(count).min(total);
        let mut items = Vec::with_capacity(end.saturating_sub(begin));
        let mut failed = false;

        if self.created_dirs.is_empty() {
            if let Some(plan) = self.removal_plan.as_ref() {
                for components in &plan[begin..end] {
                    let item = self.remove_directory_item(components);
                    failed |= item.is_failure();
                    items.push(item);
                }
            }
        } else {
            for components in self
                .created_dirs
                .names()
                .iter()
                .rev()
                .skip(begin)
                .take(end.saturating_sub(begin))
            {
                let item = self.remove_directory_item(components);
                failed |= item.is_failure();
                items.push(item);
            }
        }
        if failed {
            self.removal_failed = true;
        }

        Ok(RemovalChunk {
            items,
            done: end == total,
        })
    }

    /// Lists the structural directories removal may try, children first.
    ///
    /// Used after a restart, when no directory was tracked. Reverse
    /// lexicographic order places every descendant before its ancestors.
    fn build_structural_directory_plan(&self) -> Vec<Vec<String>> {
        let mut dirs = BTreeSet::new();

        for file in self.map.files() {
            for len in 1..file.disk_components.len() {
                dirs.insert(file.disk_components[..len].to_vec());
            }
        }

        dirs.into_iter().rev().collect()
    }

    /// Removes one directory after re-validating its ancestors and target.
    fn remove_directory_item(&self, components: &[String]) -> RemovalItemOutcome {
        let report_path = relative_path(components);

        if components.is_empty() {
            return RemovalItemOutcome::Failed {
                path: report_path,
                error: RemovalError::Internal {
                    reason: "removal plan contains the download root",
                },
            };
        }
        for component in components {
            if sanitize_component(component) != *component {
                return RemovalItemOutcome::Failed {
                    path: report_path,
                    error: RemovalError::Unsafe(UnsafeReason::InvalidComponent),
                };
            }
        }

        let mut path = self.root.to_path_buf();
        for component in components {
            path.push(component);
        }

        let before = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return RemovalItemOutcome::AlreadyMissing(report_path);
            }
            Err(source) => {
                return RemovalItemOutcome::Failed {
                    path: report_path,
                    error: removal_error(classify(&path, source)),
                };
            }
        };
        if let Err(error) = check_directory(&path, &before) {
            return RemovalItemOutcome::Failed {
                path: report_path,
                error: removal_error(error),
            };
        }
        let before_id = file_id(&before);

        match check_ancestors(&self.root, components) {
            Ok(true) => {}
            Ok(false) => return RemovalItemOutcome::AlreadyMissing(report_path),
            Err(error) => {
                return RemovalItemOutcome::Failed {
                    path: report_path,
                    error: removal_error(error),
                };
            }
        }

        let after = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return RemovalItemOutcome::AlreadyMissing(report_path);
            }
            Err(source) => {
                return RemovalItemOutcome::Failed {
                    path: report_path,
                    error: removal_error(classify(&path, source)),
                };
            }
        };
        if let Err(error) = check_directory(&path, &after) {
            return RemovalItemOutcome::Failed {
                path: report_path,
                error: removal_error(error),
            };
        }
        if !same_target(&before, &after, before_id, file_id(&after)) {
            return RemovalItemOutcome::Failed {
                path: report_path,
                error: RemovalError::Unsafe(UnsafeReason::Changed),
            };
        }

        match fs::remove_dir(&path) {
            Ok(()) => RemovalItemOutcome::Removed(report_path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                RemovalItemOutcome::AlreadyMissing(report_path)
            }
            Err(error) if directory_not_empty(&error) => RemovalItemOutcome::NotEmpty(report_path),
            Err(source) => RemovalItemOutcome::Failed {
                path: report_path,
                error: removal_error(classify(&path, source)),
            },
        }
    }

    /// Removes one mapped file after re-validating its ancestors and target.
    fn remove_file_item(&mut self, map: &FileMap, index: usize) -> RemovalItemOutcome {
        let file = match map.files().get(index) {
            Some(file) => file,
            None => {
                return RemovalItemOutcome::Failed {
                    path: PathBuf::new(),
                    error: RemovalError::Internal {
                        reason: "file index is outside the file map",
                    },
                };
            }
        };
        let report_path = relative_path(&file.disk_components);
        let path = match resolve_file_path(&self.root, file) {
            Ok(path) => path,
            Err(error) => {
                return RemovalItemOutcome::Failed {
                    path: report_path,
                    error: removal_error(error),
                };
            }
        };

        let before = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.forget_file_id(index);
                self.handles.remove_index(index);
                return RemovalItemOutcome::AlreadyMissing(report_path);
            }
            Err(source) => {
                return RemovalItemOutcome::Failed {
                    path: report_path,
                    error: removal_error(classify(&path, source)),
                };
            }
        };
        if let Err(error) = check_file(&path, &before) {
            return RemovalItemOutcome::Failed {
                path: report_path,
                error: removal_error(error),
            };
        }

        let before_id = file_id(&before);
        let pending = self.handles.remove_index(index);

        match check_ancestors(&self.root, &file.disk_components) {
            Ok(true) => {}
            Ok(false) => {
                self.forget_file_id(index);
                return RemovalItemOutcome::AlreadyMissing(report_path);
            }
            Err(error) => {
                self.restore_pending(index, &path, pending);
                return RemovalItemOutcome::Failed {
                    path: report_path,
                    error: removal_error(error),
                };
            }
        }

        let after = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.forget_file_id(index);
                return RemovalItemOutcome::AlreadyMissing(report_path);
            }
            Err(source) => {
                self.restore_pending(index, &path, pending);
                return RemovalItemOutcome::Failed {
                    path: report_path,
                    error: removal_error(classify(&path, source)),
                };
            }
        };
        if let Err(error) = check_file(&path, &after) {
            self.restore_pending(index, &path, pending);
            return RemovalItemOutcome::Failed {
                path: report_path,
                error: removal_error(error),
            };
        }
        if !same_target(&before, &after, before_id, file_id(&after)) {
            self.restore_pending(index, &path, pending);
            return RemovalItemOutcome::Failed {
                path: report_path,
                error: RemovalError::Unsafe(UnsafeReason::Changed),
            };
        }

        match fs::remove_file(&path) {
            Ok(()) => {
                self.forget_file_id(index);
                RemovalItemOutcome::Removed(report_path)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.forget_file_id(index);
                RemovalItemOutcome::AlreadyMissing(report_path)
            }
            Err(source) => {
                self.restore_pending(index, &path, pending);
                RemovalItemOutcome::Failed {
                    path: report_path,
                    error: removal_error(classify(&path, source)),
                }
            }
        }
    }

    /// Restores a pending sync when a failed removal leaves the file in place.
    fn restore_pending(&mut self, index: usize, path: &Path, pending: bool) {
        if !pending {
            return;
        }

        let Ok(metadata) = fs::symlink_metadata(path) else {
            return;
        };
        if metadata.is_file() && link_reason(&metadata).is_none() {
            self.handles.needs_sync.insert(index);
        }
    }

    /// Flushes dirty data and drops every open handle.
    ///
    /// The cache is cleared only when the flush succeeds. On failure the cache
    /// and the pending-sync set are kept so the caller can retry, and the
    /// first error is returned.
    pub fn close(&mut self) -> Result<(), StorageError> {
        self.sync()?;
        self.handles.clear();
        Ok(())
    }

    /// Number of open file handles, so tests can see that eviction happened.
    #[cfg(test)]
    pub(super) fn open_handle_count(&self) -> usize {
        self.handles.entries.len()
    }

    /// Creates one file and its parents on first write, re-checking the path.
    ///
    /// Returns the identity of an existing file so the caller can verify the
    /// handle it opens.
    fn ensure_writable_file(&mut self, index: usize) -> Result<Option<FileId>, StorageError> {
        let map = Arc::clone(&self.map);
        let mapped = map
            .files()
            .get(index)
            .ok_or_else(|| internal("file index is outside the file map"))?;
        let path = self
            .files
            .get(index)
            .map(|file| file.path.clone())
            .ok_or_else(|| internal("file index is outside the file map"))?;
        let parent_count = mapped.disk_components.len().saturating_sub(1);

        if !check_ancestors(&self.root, &mapped.disk_components)? {
            ensure_parent_dirs(
                &self.root,
                &mapped.disk_components[..parent_count],
                &mut self.created_dirs,
            )?;
            let id = ensure_file(
                &path,
                mapped.length,
                false,
                self.config.preallocation,
                &mut self.seen_inodes,
            )?;
            self.record_file_id(index, id);
            return Ok(id);
        }

        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                check_file(&path, &metadata)?;
                let id = file_id(&metadata);
                self.record_file_id(index, id);
                Ok(id)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                ensure_parent_dirs(
                    &self.root,
                    &mapped.disk_components[..parent_count],
                    &mut self.created_dirs,
                )?;
                let id = ensure_file(
                    &path,
                    mapped.length,
                    false,
                    self.config.preallocation,
                    &mut self.seen_inodes,
                )?;
                self.record_file_id(index, id);
                Ok(id)
            }
            Err(source) => Err(classify(&path, source)),
        }
    }

    /// Stores the identity of the file at `index` after an open or create.
    fn record_file_id(&mut self, index: usize, id: Option<FileId>) {
        if let Some(file) = self.files.get_mut(index) {
            file.id = id;
        }
    }

    /// Forgets the identity of a file that no longer exists.
    fn forget_file_id(&mut self, index: usize) {
        if let Some(id) = self.files.get_mut(index).and_then(|file| file.id.take()) {
            self.seen_inodes.remove(&id);
        }
    }
}

// Compile-time guarantee for the storage thread added in 3c.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<Storage>();
};

/// LRU cache of positioned file handles.
///
/// The cache never holds more than `capacity` handles and processes one
/// segment at a time, so a piece spanning several files cannot exceed the cap.
/// Transient handles are the exception: [`HandleCache::sync`] opens one
/// read-write handle per pending file outside the cache, and lazy creation
/// opens a file before it is cached, so peak usage is `capacity + 1`.
#[derive(Debug)]
struct HandleCache {
    capacity: usize,
    /// Least recently used first.
    entries: VecDeque<CacheEntry>,
    /// Files whose dirty handle was evicted and still need a sync.
    needs_sync: BTreeSet<usize>,
}

/// One open handle, whether it can write, and whether it is dirty.
#[derive(Debug)]
struct CacheEntry {
    index: usize,
    file: File,
    dirty: bool,
    writable: bool,
}

impl HandleCache {
    fn new(capacity: usize) -> Result<Self, StorageError> {
        if capacity == 0 {
            return Err(StorageError::InvalidConfig {
                reason: "max_open_handles must be at least 1",
            });
        }

        Ok(Self {
            capacity,
            entries: VecDeque::new(),
            needs_sync: BTreeSet::new(),
        })
    }

    /// Returns a handle, evicting the least recently used entry when full.
    ///
    /// Eviction never syncs. A dirty entry is recorded in `needs_sync` so a
    /// later [`Storage::sync`] can reopen and flush it. A cached read-only
    /// handle is reopened for writing when `writable` is requested.
    fn get_or_open<F>(
        &mut self,
        index: usize,
        path: &Path,
        open: F,
        dirty: bool,
        writable: bool,
        expected: Option<FileId>,
    ) -> Result<&mut File, StorageError>
    where
        F: FnOnce(&Path) -> io::Result<File>,
    {
        if let Some(position) = self.entries.iter().position(|entry| entry.index == index) {
            {
                let entry = self
                    .entries
                    .get_mut(position)
                    .ok_or_else(|| internal("handle cache entry vanished"))?;
                if dirty {
                    entry.dirty = true;
                }
                if writable && !entry.writable {
                    let file = open(path).map_err(|source| classify(path, source))?;
                    verify_opened(path, &file, expected)?;
                    entry.file = file;
                    entry.writable = true;
                }
            }
            if position + 1 != self.entries.len() {
                let entry = self
                    .entries
                    .remove(position)
                    .ok_or_else(|| internal("handle cache entry vanished"))?;
                self.entries.push_back(entry);
            }
            return self
                .entries
                .back_mut()
                .map(|entry| &mut entry.file)
                .ok_or_else(|| internal("handle cache lost its most recent entry"));
        }

        if self.entries.len() >= self.capacity {
            let evicted = self
                .entries
                .pop_front()
                .ok_or_else(|| internal("handle cache is full but has no entry"))?;
            if evicted.dirty {
                self.needs_sync.insert(evicted.index);
            }
        }

        let file = open(path).map_err(|source| classify(path, source))?;
        verify_opened(path, &file, expected)?;
        self.entries.push_back(CacheEntry {
            index,
            file,
            dirty,
            writable,
        });
        self.entries
            .back_mut()
            .map(|entry| &mut entry.file)
            .ok_or_else(|| internal("handle cache is empty after insert"))
    }

    /// Flushes every dirty or pending file.
    ///
    /// Every file is attempted; only the ones that succeed leave
    /// `needs_sync`, so a caller can retry after a failure. The first error is
    /// returned.
    fn sync(
        &mut self,
        root: &Path,
        map: &FileMap,
        files: &[PreparedFile],
    ) -> Result<(), StorageError> {
        let mut first_error = None;

        for entry in &mut self.entries {
            let pending = self.needs_sync.contains(&entry.index);
            if !entry.dirty && !pending {
                continue;
            }

            let path = match files.get(entry.index) {
                Some(file) => file.path.as_path(),
                None => {
                    record_first(
                        &mut first_error,
                        internal("handle index is outside the file map"),
                    );
                    continue;
                }
            };

            if !entry.writable {
                match open_rw(path) {
                    Ok(file) => {
                        entry.file = file;
                        entry.writable = true;
                    }
                    Err(source) => {
                        record_first(&mut first_error, classify(path, source));
                        continue;
                    }
                }
            }

            match entry.file.sync_data() {
                Ok(()) => {
                    entry.dirty = false;
                    self.needs_sync.remove(&entry.index);
                }
                Err(source) => record_first(&mut first_error, classify(path, source)),
            }
        }

        let pending: Vec<usize> = self.needs_sync.iter().copied().collect();
        for index in pending {
            match sync_pending(root, map, files, index) {
                Ok(()) => {
                    self.needs_sync.remove(&index);
                }
                Err(error) => record_first(&mut first_error, error),
            }
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Drops the cached handle for `index` before its path is removed.
    ///
    /// Returns `true` when the entry was dirty or already waiting for a sync,
    /// so a failed removal can put the pending flag back.
    fn remove_index(&mut self, index: usize) -> bool {
        let pending = self.needs_sync.remove(&index);
        let mut dirty = false;
        self.entries.retain(|entry| {
            if entry.index == index {
                dirty = entry.dirty;
                false
            } else {
                true
            }
        });
        dirty || pending
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.needs_sync.clear();
    }
}

/// Builds a relative display path from mapped components.
///
/// Components are sanitized before joining, so even a hostile hand-built map
/// cannot put an absolute path or `..` into a report.
fn relative_path(components: &[String]) -> PathBuf {
    let mut path = PathBuf::new();
    for component in components {
        path.push(sanitize_component(component));
    }
    path
}

/// Returns `true` when two metadata checks describe the same target.
fn same_target(
    before: &fs::Metadata,
    after: &fs::Metadata,
    before_id: Option<FileId>,
    after_id: Option<FileId>,
) -> bool {
    before.file_type().is_file() == after.file_type().is_file()
        && before.file_type().is_dir() == after.file_type().is_dir()
        && before_id == after_id
}

/// Converts a storage error into a reportable removal error.
fn removal_error(error: StorageError) -> RemovalError {
    match error {
        StorageError::UnsafePath {
            reason: UnsafeReason::NotFile | UnsafeReason::NotDirectory,
            ..
        } => RemovalError::Unsafe(UnsafeReason::WrongKind),
        StorageError::UnsafePath { reason, .. } => RemovalError::Unsafe(reason),
        StorageError::PermissionDenied { source, .. } => RemovalError::PermissionDenied(source),
        StorageError::FileInUse { source, .. } => RemovalError::FileInUse(source),
        StorageError::Io { source, .. } | StorageError::DiskFull { source, .. } => {
            RemovalError::Io(source)
        }
        _ => RemovalError::Internal {
            reason: "unexpected storage error during removal",
        },
    }
}

/// Reopens one evicted file after re-checking its ancestors and syncs it.
fn sync_pending(
    root: &Path,
    map: &FileMap,
    files: &[PreparedFile],
    index: usize,
) -> Result<(), StorageError> {
    let path = files
        .get(index)
        .map(|file| file.path.clone())
        .ok_or_else(|| internal("handle index is outside the file map"))?;
    let components = map
        .files()
        .get(index)
        .map(|file| file.disk_components.as_slice())
        .ok_or_else(|| internal("handle index is outside the file map"))?;

    if !check_ancestors(root, components)? {
        return Err(StorageError::Io {
            path,
            source: io::Error::from(io::ErrorKind::NotFound),
        });
    }

    let metadata = fs::symlink_metadata(&path).map_err(|source| classify(&path, source))?;
    check_file(&path, &metadata)?;
    let expected = file_id(&metadata);

    let file = open_rw(&path).map_err(|source| classify(&path, source))?;
    verify_opened(&path, &file, expected)?;
    file.sync_data().map_err(|source| classify(&path, source))
}

/// Keeps the first error of a multi-file sync and ignores later ones.
fn record_first(slot: &mut Option<StorageError>, error: StorageError) {
    if slot.is_none() {
        *slot = Some(error);
    }
}

/// Reads until `buf` is full, EOF, or an error.
///
/// Retries [`io::ErrorKind::Interrupted`] at the same position and returns the
/// number of bytes read before EOF. Generic over the read call so tests can
/// inject short counts.
fn read_up_to<F>(mut read_at: F, buf: &mut [u8], offset: u64) -> io::Result<usize>
where
    F: FnMut(&mut [u8], u64) -> io::Result<usize>,
{
    let mut filled = 0usize;

    while filled < buf.len() {
        let position = offset
            .checked_add(filled as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "read offset overflow"))?;

        match read_at(&mut buf[filled..], position) {
            Ok(0) => break,
            Ok(read) if read <= buf.len() - filled => filled += read,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "read_at returned more bytes than the buffer holds",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }

    Ok(filled)
}

/// Writes all of `buf`, retrying short counts and [`io::ErrorKind::Interrupted`].
///
/// A zero-byte write is [`io::ErrorKind::WriteZero`]. Generic over the write
/// call so tests can inject short counts.
fn write_all_at<F>(mut write_at: F, buf: &[u8], offset: u64) -> io::Result<()>
where
    F: FnMut(&[u8], u64) -> io::Result<usize>,
{
    let mut written = 0usize;

    while written < buf.len() {
        let position = offset
            .checked_add(written as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "write offset overflow"))?;

        match write_at(&buf[written..], position) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
            Ok(count) if count <= buf.len() - written => written += count,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "write_at returned more bytes than the buffer holds",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }

    Ok(())
}

/// A resolved file path, whether it existed before creation, and its inode.
#[derive(Debug)]
struct PreparedFile {
    path: PathBuf,
    existed: bool,
    id: Option<FileId>,
}

/// Creates the paths required by the preallocation policy.
fn prepare_files(
    root: &Path,
    map: &FileMap,
    config: &StorageConfig,
    files: &mut [PreparedFile],
    seen_inodes: &mut BTreeSet<FileId>,
    created_dirs: &mut CreatedDirs,
) -> Result<(), StorageError> {
    for (file, prepared) in map.files().iter().zip(files.iter_mut()) {
        let needed = file.length == 0 || config.preallocation == Preallocation::SetLen;
        if !needed {
            continue;
        }

        let parent_count = file.disk_components.len().saturating_sub(1);
        ensure_parent_dirs(root, &file.disk_components[..parent_count], created_dirs)?;
        prepared.id = ensure_file(
            &prepared.path,
            file.length,
            prepared.existed,
            config.preallocation,
            seen_inodes,
        )?;
    }

    Ok(())
}

/// Creates missing parent directories, re-checking every existing component.
///
/// A directory is recorded as created by this storage only when the `mkdir`
/// succeeds here. A concurrent creator that wins the race is not recorded.
fn ensure_parent_dirs(
    root: &Path,
    components: &[String],
    created: &mut CreatedDirs,
) -> Result<(), StorageError> {
    let mut path = root.to_path_buf();

    for (index, component) in components.iter().enumerate() {
        if sanitize_component(component) != *component {
            return Err(StorageError::UnsafePath {
                path: path.clone(),
                reason: UnsafeReason::InvalidComponent,
            });
        }

        path.push(component);

        match fs::symlink_metadata(&path) {
            Ok(metadata) => check_directory(&path, &metadata)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => match fs::create_dir(&path) {
                Ok(()) => created.record(&components[..=index]),
                Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                    let metadata =
                        fs::symlink_metadata(&path).map_err(|source| classify(&path, source))?;
                    check_directory(&path, &metadata)?;
                }
                Err(source) => return Err(classify(&path, source)),
            },
            Err(source) => return Err(classify(&path, source)),
        }
    }

    Ok(())
}

/// Creates or reuses one file and applies the preallocation policy.
///
/// Returns the identity of the resulting file, when the platform exposes one,
/// so the caller can verify the handle it opens.
fn ensure_file(
    path: &Path,
    expected_len: u64,
    existed: bool,
    preallocation: Preallocation,
    seen_inodes: &mut BTreeSet<FileId>,
) -> Result<Option<FileId>, StorageError> {
    if existed {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                check_file(path, &metadata)?;
                if preallocation == Preallocation::SetLen {
                    extend_if_short(path, expected_len)?;
                }
                return Ok(file_id(&metadata));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(classify(path, source)),
        }
    }

    loop {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                check_file(path, &metadata)?;
                record_inode(path, &metadata, seen_inodes)?;
                if preallocation == Preallocation::SetLen {
                    extend_if_short(path, expected_len)?;
                }
                return Ok(file_id(&metadata));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => match create_new(path) {
                Ok(file) => {
                    let metadata = file.metadata().map_err(|source| classify(path, source))?;
                    check_file(path, &metadata)?;
                    record_inode(path, &metadata, seen_inodes)?;
                    if preallocation == Preallocation::SetLen && expected_len > 0 {
                        file.set_len(expected_len)
                            .map_err(|source| classify(path, source))?;
                    }
                    return Ok(file_id(&metadata));
                }
                Err(source) if source.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(source) => return Err(classify(path, source)),
            },
            Err(source) => return Err(classify(path, source)),
        }
    }
}

/// Extends an existing file when preallocation is enabled. Never truncates.
///
/// The current length is read from the opened handle, so a stale `lstat` made
/// before the open cannot shrink a file that another process already grew. A
/// narrow race remains between that `fstat` and `set_len`; v0.1 accepts it
/// because preallocation runs before this torrent writes anything and no
/// other component of the client grows these files.
fn extend_if_short(path: &Path, expected_len: u64) -> Result<(), StorageError> {
    let file = open_write(path).map_err(|source| classify(path, source))?;
    let current = file
        .metadata()
        .map_err(|source| classify(path, source))?
        .len();
    if current >= expected_len {
        return Ok(());
    }

    file.set_len(expected_len)
        .map_err(|source| classify(path, source))
}

/// Re-checks every existing ancestor of one file below the root.
///
/// Returns `Ok(false)` when an ancestor is missing, which means the file cannot
/// exist yet. Called before every open so a component swapped after
/// [`Storage::open`] cannot redirect the join.
fn check_ancestors(root: &Path, components: &[String]) -> Result<bool, StorageError> {
    let parent_count = components.len().saturating_sub(1);
    let mut path = root.to_path_buf();

    for component in &components[..parent_count] {
        if sanitize_component(component) != *component {
            return Err(StorageError::UnsafePath {
                path: path.clone(),
                reason: UnsafeReason::InvalidComponent,
            });
        }

        path.push(component);

        match fs::symlink_metadata(&path) {
            Ok(metadata) => check_directory(&path, &metadata)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(source) => return Err(classify(&path, source)),
        }
    }

    Ok(true)
}

/// Observes one mapped file after validating its ancestors.
fn observe_file(root: &Path, file: &MappedFile) -> Result<ObservedFile, StorageError> {
    let path = resolve_file_path(root, file)?;

    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            check_file(&path, &metadata)?;
            Ok(ObservedFile::Present {
                size: metadata.len(),
                mtime_ns: metadata.modified().ok().and_then(system_time_ns),
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(ObservedFile::Missing),
        Err(source) => Err(classify(&path, source)),
    }
}

/// Converts a `SystemTime` to nanoseconds since the Unix epoch.
///
/// Returns `None` when the value is outside the `i64` nanosecond range, which
/// is roughly 1677-2262. Callers treat that as an unavailable mtime instead of
/// panicking or saturating to a value that could compare equal by accident.
fn system_time_ns(time: SystemTime) -> Option<i64> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_nanos()).ok(),
        Err(error) => {
            let nanos = i64::try_from(error.duration().as_nanos()).ok()?;
            Some(-nanos)
        }
    }
}

/// Resolves one mapped file and validates every existing ancestor.
fn resolve_file_path(root: &Path, file: &MappedFile) -> Result<PathBuf, StorageError> {
    if file.disk_components.is_empty() {
        return Err(StorageError::UnsafePath {
            path: root.to_path_buf(),
            reason: UnsafeReason::InvalidComponent,
        });
    }

    let mut path = root.to_path_buf();
    let last = file.disk_components.len() - 1;
    let mut missing_parent = false;

    for (index, component) in file.disk_components.iter().enumerate() {
        if sanitize_component(component) != *component {
            return Err(StorageError::UnsafePath {
                path: path.clone(),
                reason: UnsafeReason::InvalidComponent,
            });
        }

        path.push(component);

        if index == last || missing_parent {
            continue;
        }

        match fs::symlink_metadata(&path) {
            Ok(metadata) => check_directory(&path, &metadata)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => missing_parent = true,
            Err(source) => return Err(classify(&path, source)),
        }
    }

    Ok(path)
}

/// Returns `true` when `remove_dir` failed because the directory is not empty.
///
/// POSIX permits `rmdir` to report a non-empty directory as either `ENOTEMPTY`
/// or `EEXIST`; Rust maps the latter to `AlreadyExists`.
fn directory_not_empty(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::AlreadyExists
    )
}

/// Rejects links and non-directories.
fn check_directory(path: &Path, metadata: &fs::Metadata) -> Result<(), StorageError> {
    if let Some(reason) = link_reason(metadata) {
        return Err(StorageError::UnsafePath {
            path: path.to_path_buf(),
            reason,
        });
    }
    if !metadata.is_dir() {
        return Err(StorageError::UnsafePath {
            path: path.to_path_buf(),
            reason: UnsafeReason::NotDirectory,
        });
    }
    Ok(())
}

/// Rejects links and non-regular files.
fn check_file(path: &Path, metadata: &fs::Metadata) -> Result<(), StorageError> {
    if let Some(reason) = link_reason(metadata) {
        return Err(StorageError::UnsafePath {
            path: path.to_path_buf(),
            reason,
        });
    }
    if !metadata.is_file() {
        return Err(StorageError::UnsafePath {
            path: path.to_path_buf(),
            reason: UnsafeReason::NotFile,
        });
    }
    Ok(())
}

/// Returns the reason a metadata entry is a link, if it is one.
fn link_reason(metadata: &fs::Metadata) -> Option<UnsafeReason> {
    if metadata.file_type().is_symlink() {
        Some(UnsafeReason::Symlink)
    } else if is_reparse_point(metadata) {
        Some(UnsafeReason::ReparsePoint)
    } else {
        None
    }
}

/// Detects Windows reparse points that are not reported as symlinks.
#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

/// Rejects two paths that resolve to the same inode.
#[cfg(unix)]
fn record_inode(
    path: &Path,
    metadata: &fs::Metadata,
    seen: &mut BTreeSet<FileId>,
) -> Result<(), StorageError> {
    use std::os::unix::fs::MetadataExt;

    let id = FileId {
        dev: metadata.dev(),
        ino: metadata.ino(),
    };
    if !seen.insert(id) {
        return Err(StorageError::UnsafePath {
            path: path.to_path_buf(),
            reason: UnsafeReason::DuplicateFile,
        });
    }
    Ok(())
}

/// Windows has no stable volume/file-index accessor in `std`, so the layout
/// and `create_new` collisions are the duplicate check there.
#[cfg(not(unix))]
fn record_inode(
    _path: &Path,
    _metadata: &fs::Metadata,
    _seen: &mut BTreeSet<FileId>,
) -> Result<(), StorageError> {
    Ok(())
}

/// Returns the identity used to verify that an opened handle is the file that
/// was checked. Always `None` off Unix.
#[cfg(unix)]
fn file_id(metadata: &fs::Metadata) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;

    Some(FileId {
        dev: metadata.dev(),
        ino: metadata.ino(),
    })
}

#[cfg(not(unix))]
fn file_id(_metadata: &fs::Metadata) -> Option<FileId> {
    None
}

/// Verifies on Unix that the opened handle matches the checked metadata.
///
/// This narrows the window between `symlink_metadata` and the open: a component
/// swapped in between yields a different device or inode.
#[cfg(unix)]
fn verify_opened(path: &Path, file: &File, expected: Option<FileId>) -> Result<(), StorageError> {
    use std::os::unix::fs::MetadataExt;

    let Some(expected) = expected else {
        return Ok(());
    };

    let metadata = file.metadata().map_err(|source| classify(path, source))?;
    if metadata.dev() != expected.dev || metadata.ino() != expected.ino {
        return Err(StorageError::UnsafePath {
            path: path.to_path_buf(),
            reason: UnsafeReason::Changed,
        });
    }

    Ok(())
}

#[cfg(not(unix))]
fn verify_opened(
    _path: &Path,
    _file: &File,
    _expected: Option<FileId>,
) -> Result<(), StorageError> {
    Ok(())
}

/// Unix device and inode pair.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct FileId {
    dev: u64,
    ino: u64,
}

/// Placeholder so the inode set has one type on every platform.
#[cfg(not(unix))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct FileId;

/// Builds a typed error for a broken internal invariant.
fn internal(reason: &'static str) -> StorageError {
    StorageError::Internal { reason }
}

/// Maps I/O failures to typed storage errors.
fn classify(path: &Path, source: io::Error) -> StorageError {
    let kind = source.kind();

    if kind == io::ErrorKind::NotADirectory {
        return StorageError::UnsafePath {
            path: path.to_path_buf(),
            reason: UnsafeReason::NotDirectory,
        };
    }
    if kind == io::ErrorKind::StorageFull
        || kind == io::ErrorKind::QuotaExceeded
        || is_disk_full(&source)
    {
        return StorageError::DiskFull {
            path: path.to_path_buf(),
            source,
        };
    }
    if is_file_in_use(&source) {
        return StorageError::FileInUse {
            path: path.to_path_buf(),
            source,
        };
    }
    if kind == io::ErrorKind::PermissionDenied {
        return StorageError::PermissionDenied {
            path: path.to_path_buf(),
            source,
        };
    }

    StorageError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Windows disk-full codes, including the handle variant.
#[cfg(windows)]
fn is_disk_full(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(39) | Some(112))
}

#[cfg(not(windows))]
fn is_disk_full(_error: &io::Error) -> bool {
    false
}

/// Windows sharing and lock violations.
#[cfg(windows)]
fn is_file_in_use(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(32) | Some(33))
}

#[cfg(not(windows))]
fn is_file_in_use(_error: &io::Error) -> bool {
    false
}

/// Reasons a torrent-derived path is unsafe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsafeReason {
    /// The component is a symlink.
    Symlink,
    /// The component is a Windows reparse point.
    ReparsePoint,
    /// A directory was expected.
    NotDirectory,
    /// A regular file was expected.
    NotFile,
    /// A mapped file or directory was replaced by the opposite kind.
    WrongKind,
    /// Two paths resolve to the same file.
    DuplicateFile,
    /// A component changed between the check and the open.
    Changed,
    /// The component is not in sanitized form.
    InvalidComponent,
}

impl fmt::Display for UnsafeReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Symlink => "component is a symlink",
            Self::ReparsePoint => "component is a reparse point",
            Self::NotDirectory => "component is not a directory",
            Self::NotFile => "component is not a regular file",
            Self::WrongKind => "path has the wrong kind",
            Self::DuplicateFile => "path aliases another file",
            Self::Changed => "component changed while opening",
            Self::InvalidComponent => "component is not sanitized",
        };
        formatter.write_str(text)
    }
}

/// Errors produced by the storage backend.
#[derive(Debug, Error)]
pub enum StorageError {
    /// An I/O operation failed.
    #[error("I/O error at {path}: {source}")]
    Io {
        /// Path that failed.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// The device or quota is full.
    #[error("disk full at {path}: {source}")]
    DiskFull {
        /// Path that failed.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// Permission was denied.
    #[error("permission denied at {path}: {source}")]
    PermissionDenied {
        /// Path that failed.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// The file is locked or in use.
    #[error("file is in use at {path}: {source}")]
    FileInUse {
        /// Path that failed.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// A torrent-derived path failed a safety check.
    #[error("unsafe path {path}: {reason}")]
    UnsafePath {
        /// Path that failed.
        path: PathBuf,
        /// Why it failed.
        reason: UnsafeReason,
    },
    /// A byte range is outside the torrent or overflows `u64`.
    #[error("range offset {offset} length {len} is outside total length {total_length}")]
    OutOfRange {
        /// Start of the range.
        offset: u64,
        /// Requested length.
        len: u64,
        /// Total torrent length.
        total_length: u64,
    },
    /// The configuration is invalid.
    #[error("invalid storage config: {reason}")]
    InvalidConfig {
        /// What is wrong.
        reason: &'static str,
    },
    /// The storage completed a removal pass and no longer accepts data
    /// operations.
    #[error("storage was removed")]
    Removed,
    /// An internal invariant was violated.
    #[error("internal storage invariant: {reason}")]
    Internal {
        /// Which invariant.
        reason: &'static str,
    },
}

#[cfg(test)]
mod tests;
