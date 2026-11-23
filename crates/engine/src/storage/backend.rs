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

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bt_platform::file::{File, create_new, open_read, open_rw, open_write};
use thiserror::Error;

use super::layout::{FileMap, MappedFile, sanitize_component};

/// Default maximum number of open file handles.
pub const DEFAULT_MAX_OPEN_HANDLES: usize = 64;

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
#[derive(Debug)]
pub struct Storage {
    root: PathBuf,
    map: Arc<FileMap>,
    config: StorageConfig,
    files: Vec<PreparedFile>,
    seen_inodes: BTreeSet<FileId>,
    handles: HandleCache,
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

        for file in map.files() {
            let path = resolve_file_path(root, file)?;
            let existed = match fs::symlink_metadata(&path) {
                Ok(metadata) => {
                    check_file(&path, &metadata)?;
                    record_inode(&path, &metadata, &mut seen_inodes)?;
                    true
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                Err(source) => return Err(classify(&path, source)),
            };

            files.push(PreparedFile { path, existed });
        }

        prepare_files(root, map.as_ref(), &config, &files, &mut seen_inodes)?;
        let handles = HandleCache::new(config.max_open_handles)?;

        Ok(Self {
            root: root.to_path_buf(),
            map,
            config,
            files,
            seen_inodes,
            handles,
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
    pub fn read_range(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize, StorageError> {
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
            let path = self.files[index].path.clone();

            if !check_ancestors(&self.root, &map.files()[index].disk_components)? {
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
            let remaining = requested - filled as u64;
            let to_read = segment.len.min(available_in_file).min(remaining);
            let to_read = usize::try_from(to_read).map_err(|_| StorageError::OutOfRange {
                offset,
                len: requested,
                total_length,
            })?;
            if to_read == 0 {
                break;
            }

            // A file with pending synced data is reopened read-write so a
            // later sync works on every platform.
            let needs_sync = self.handles.needs_sync(index);
            let file = self.handles.get_or_open(
                index,
                &path,
                |path| {
                    if needs_sync {
                        open_rw(path)
                    } else {
                        open_read(path)
                    }
                },
                false,
                expected,
            )?;

            let read = read_up_to(
                |chunk, position| file.read_at(chunk, position),
                &mut buf[filled..filled + to_read],
                segment.offset_in_file,
            )
            .map_err(|source| classify(&path, source))?;
            filled += read;

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
    pub fn write_range(&mut self, offset: u64, buf: &[u8]) -> Result<(), StorageError> {
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
            let path = self.files[index].path.clone();

            let remaining = buf.len() - written;
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

            let file = self
                .handles
                .get_or_open(index, &path, open_write, true, expected)?;
            write_all_at(
                |chunk, position| file.write_at(chunk, position),
                &buf[written..written + to_write],
                segment.offset_in_file,
            )
            .map_err(|source| classify(&path, source))?;
            written += to_write;
        }

        Ok(())
    }

    /// Flushes dirty data for cached and evicted files.
    ///
    /// Evicted dirty files are reopened read-write after their ancestors are
    /// re-checked and synced. A file that was removed after eviction is a typed
    /// error.
    pub fn sync(&mut self) -> Result<(), StorageError> {
        self.handles
            .sync(&self.root, self.map.as_ref(), &self.files)
    }

    /// Flushes dirty data and drops every open handle.
    pub fn close(&mut self) -> Result<(), StorageError> {
        let result = self.sync();
        self.handles.clear();
        result
    }

    /// Creates one file and its parents on first write, re-checking the path.
    ///
    /// Returns the identity of an existing file so the caller can verify the
    /// handle it opens.
    fn ensure_writable_file(&mut self, index: usize) -> Result<Option<FileId>, StorageError> {
        let map = Arc::clone(&self.map);
        let mapped = &map.files()[index];
        let path = self.files[index].path.clone();
        let parent_count = mapped.disk_components.len().saturating_sub(1);

        if !check_ancestors(&self.root, &mapped.disk_components)? {
            ensure_parent_dirs(&self.root, &mapped.disk_components[..parent_count])?;
            return ensure_file(
                &path,
                mapped.length,
                false,
                self.config.preallocation,
                &mut self.seen_inodes,
            );
        }

        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                check_file(&path, &metadata)?;
                Ok(file_id(&metadata))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                ensure_parent_dirs(&self.root, &mapped.disk_components[..parent_count])?;
                ensure_file(
                    &path,
                    mapped.length,
                    false,
                    self.config.preallocation,
                    &mut self.seen_inodes,
                )
            }
            Err(source) => Err(classify(&path, source)),
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
#[derive(Debug)]
struct HandleCache {
    capacity: usize,
    /// Least recently used first.
    entries: Vec<CacheEntry>,
    /// Files whose dirty handle was evicted and still need a sync.
    needs_sync: BTreeSet<usize>,
}

/// One open handle and whether it was written since the last sync.
#[derive(Debug)]
struct CacheEntry {
    index: usize,
    file: File,
    dirty: bool,
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
            entries: Vec::new(),
            needs_sync: BTreeSet::new(),
        })
    }

    fn needs_sync(&self, index: usize) -> bool {
        self.needs_sync.contains(&index)
    }

    /// Returns a handle, evicting the least recently used entry when full.
    ///
    /// Eviction never syncs. A dirty entry is recorded in `needs_sync` so a
    /// later [`Storage::sync`] can reopen and flush it.
    fn get_or_open<F>(
        &mut self,
        index: usize,
        path: &Path,
        open: F,
        dirty: bool,
        expected: Option<FileId>,
    ) -> Result<&mut File, StorageError>
    where
        F: FnOnce(&Path) -> io::Result<File>,
    {
        if let Some(position) = self.entries.iter().position(|entry| entry.index == index) {
            if dirty {
                self.entries[position].dirty = true;
            }
            if position + 1 != self.entries.len() {
                let entry = self.entries.remove(position);
                self.entries.push(entry);
            }
            return self.entries.last_mut().map(|entry| &mut entry.file).ok_or(
                StorageError::InvalidConfig {
                    reason: "handle cache lost its most recent entry",
                },
            );
        }

        if self.entries.len() >= self.capacity {
            let evicted = self.entries.remove(0);
            if evicted.dirty {
                self.needs_sync.insert(evicted.index);
            }
        }

        let file = open(path).map_err(|source| classify(path, source))?;
        verify_opened(path, &file, expected)?;
        self.entries.push(CacheEntry { index, file, dirty });
        self.entries
            .last_mut()
            .map(|entry| &mut entry.file)
            .ok_or(StorageError::InvalidConfig {
                reason: "handle cache is empty after insert",
            })
    }

    /// Syncs dirty cached handles and reopens evicted ones.
    fn sync(
        &mut self,
        root: &Path,
        map: &FileMap,
        files: &[PreparedFile],
    ) -> Result<(), StorageError> {
        for entry in &mut self.entries {
            let pending = self.needs_sync.contains(&entry.index);
            if !entry.dirty && !pending {
                continue;
            }

            let path = files
                .get(entry.index)
                .map(|file| file.path.as_path())
                .ok_or(StorageError::InvalidConfig {
                    reason: "handle index is outside the file map",
                })?;
            entry
                .file
                .sync_data()
                .map_err(|source| classify(path, source))?;
            entry.dirty = false;
            self.needs_sync.remove(&entry.index);
        }

        let pending: Vec<usize> = self.needs_sync.iter().copied().collect();
        for index in pending {
            let path = files.get(index).map(|file| file.path.clone()).ok_or(
                StorageError::InvalidConfig {
                    reason: "handle index is outside the file map",
                },
            )?;
            let components = map
                .files()
                .get(index)
                .map(|file| file.disk_components.as_slice())
                .ok_or(StorageError::InvalidConfig {
                    reason: "handle index is outside the file map",
                })?;

            if !check_ancestors(root, components)? {
                return Err(StorageError::Io {
                    path: path.clone(),
                    source: io::Error::from(io::ErrorKind::NotFound),
                });
            }

            let metadata = fs::symlink_metadata(&path).map_err(|source| classify(&path, source))?;
            check_file(&path, &metadata)?;
            let expected = file_id(&metadata);

            let file = open_rw(&path).map_err(|source| classify(&path, source))?;
            verify_opened(&path, &file, expected)?;
            file.sync_data().map_err(|source| classify(&path, source))?;
            self.needs_sync.remove(&index);
        }

        Ok(())
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.needs_sync.clear();
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

/// A resolved file path and whether it existed before creation.
#[derive(Debug)]
struct PreparedFile {
    path: PathBuf,
    existed: bool,
}

/// Creates the paths required by the preallocation policy.
fn prepare_files(
    root: &Path,
    map: &FileMap,
    config: &StorageConfig,
    files: &[PreparedFile],
    seen_inodes: &mut BTreeSet<FileId>,
) -> Result<(), StorageError> {
    for (file, prepared) in map.files().iter().zip(files) {
        let needed = file.length == 0 || config.preallocation == Preallocation::SetLen;
        if !needed {
            continue;
        }

        let parent_count = file.disk_components.len().saturating_sub(1);
        ensure_parent_dirs(root, &file.disk_components[..parent_count])?;
        let _ = ensure_file(
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
fn ensure_parent_dirs(root: &Path, components: &[String]) -> Result<(), StorageError> {
    let mut path = root.to_path_buf();

    for component in components {
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
                Ok(()) => {}
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
                    extend_if_short(path, &metadata, expected_len)?;
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
                    extend_if_short(path, &metadata, expected_len)?;
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
fn extend_if_short(
    path: &Path,
    metadata: &fs::Metadata,
    expected_len: u64,
) -> Result<(), StorageError> {
    if metadata.len() >= expected_len {
        return Ok(());
    }

    let file = open_write(path).map_err(|source| classify(path, source))?;
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
}

#[cfg(test)]
mod tests;
