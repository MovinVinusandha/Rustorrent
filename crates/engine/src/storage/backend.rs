//! Storage backend: validates torrent paths and creates files under a root.
//!
//! The backend first resolves every torrent-derived path below the root and
//! rejects symlinks, reparse points, wrong file types, unsanitized components,
//! and duplicate inodes. It then creates directories, eager zero-length files,
//! and preallocated files according to [`Preallocation`].

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bt_platform::file::{create_new, open_write};
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

        Ok(Self {
            root: root.to_path_buf(),
            map,
            config,
            files,
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
}

// Compile-time guarantee for the storage thread added in 3c.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<Storage>();
};

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
        ensure_file(
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
                path: path.join(component),
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
fn ensure_file(
    path: &Path,
    expected_len: u64,
    existed: bool,
    preallocation: Preallocation,
    seen_inodes: &mut BTreeSet<FileId>,
) -> Result<(), StorageError> {
    if existed {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                check_file(path, &metadata)?;
                if preallocation == Preallocation::SetLen {
                    extend_if_short(path, &metadata, expected_len)?;
                }
                return Ok(());
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
                return Ok(());
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
                    return Ok(());
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
                path: path.join(component),
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

    if kind == io::ErrorKind::StorageFull || kind == io::ErrorKind::QuotaExceeded {
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
    /// The configuration is invalid.
    #[error("invalid storage config: {reason}")]
    InvalidConfig {
        /// What is wrong.
        reason: &'static str,
    },
}

#[cfg(test)]
mod tests;
