//! Positioned file I/O.
//!
//! The wrapper hides the platform difference between Unix `read_at`/`write_at`
//! and Windows `seek_read`/`seek_write`. Both are safe APIs, so no `unsafe` is
//! used. Handles are not shared across threads because the Windows calls also
//! move the file cursor.

use std::fs::{File as StdFile, Metadata, OpenOptions};
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Suffix that marks a temporary file created by [`write_atomic`].
const TEMP_SUFFIX: &str = ".tmp";

/// Separates temp files created by the same process in the same nanosecond.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A file handle with positioned reads and writes.
#[derive(Debug)]
pub struct File {
    inner: StdFile,
}

impl File {
    fn new(inner: StdFile) -> Self {
        Self { inner }
    }

    /// Reads into `buf` starting at `offset`.
    ///
    /// May return fewer bytes than `buf.len()`. A return of `0` means EOF.
    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        #[cfg(unix)]
        {
            std::os::unix::fs::FileExt::read_at(&self.inner, buf, offset)
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::FileExt::seek_read(&self.inner, buf, offset)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (buf, offset);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "positioned I/O is not supported on this platform",
            ))
        }
    }

    /// Writes from `buf` starting at `offset`.
    ///
    /// May write fewer bytes than `buf.len()`. A return of `0` means the write
    /// made no progress.
    pub fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        #[cfg(unix)]
        {
            std::os::unix::fs::FileExt::write_at(&self.inner, buf, offset)
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::FileExt::seek_write(&self.inner, buf, offset)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (buf, offset);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "positioned I/O is not supported on this platform",
            ))
        }
    }

    /// Returns metadata for this open file.
    pub fn metadata(&self) -> io::Result<Metadata> {
        self.inner.metadata()
    }

    /// Returns the current file length.
    pub fn len(&self) -> io::Result<u64> {
        self.metadata().map(|metadata| metadata.len())
    }

    /// Returns `true` when the file length is zero.
    pub fn is_empty(&self) -> io::Result<bool> {
        self.len().map(|len| len == 0)
    }

    /// Sets the file length, extending with zeros or truncating.
    pub fn set_len(&self, len: u64) -> io::Result<()> {
        self.inner.set_len(len)
    }

    /// Flushes file data to disk.
    pub fn sync_data(&self) -> io::Result<()> {
        self.inner.sync_data()
    }

    /// Flushes file data and metadata to disk.
    pub fn sync_all(&self) -> io::Result<()> {
        self.inner.sync_all()
    }
}

/// Opens an existing file for reading.
pub fn open_read(path: &Path) -> io::Result<File> {
    StdFile::open(path).map(File::new)
}

/// Opens an existing file for reading and writing.
///
/// Fails with `NotFound` instead of creating the file. Used to reopen a dirty
/// file for syncing without recreating a file the user deleted.
pub fn open_rw(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map(File::new)
}

/// Opens a file for reading and writing, creating it if missing.
///
/// Never truncates an existing file.
pub fn open_write(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map(File::new)
}

/// Creates a new file for reading and writing.
///
/// Fails with `AlreadyExists` if the path exists.
pub fn create_new(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .map(File::new)
}

/// Writes `bytes` to `path` atomically.
///
/// Data is written to a uniquely named temp file in the same directory,
/// flushed with `sync_all`, then renamed over the target. The name is
/// `<file name>.tmp.<pid>.<counter>.<nanos>`, and the temp is opened with
/// `create_new`, so a leftover or planted file is never reused. On Unix the
/// parent directory is synced after the rename. On Windows the rename is
/// retried briefly when the target is temporarily open by another process;
/// that retry blocks the caller for at most a few hundred milliseconds.
///
/// A crash leaves either the old target or the new target, never a mixture.
/// Concurrent writers use separate temp files, so the last rename wins and the
/// target is always one complete document. A failed write removes its own temp
/// file; a process that dies mid-write leaves one behind, which the next call
/// ignores and callers must sweep.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temp = temp_path(path)?;
    let result = write_and_rename(&temp, path, bytes);
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result?;
    sync_parent(path)
}

/// Builds a unique temp path next to `path`.
///
/// The pid separates processes, the process-wide counter separates calls in
/// one process, and the timestamp separates runs that reuse the pid. No
/// randomness is needed because `create_new` refuses an existing name.
fn temp_path(path: &Path) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let mut temp = name.to_os_string();
    temp.push(format!(
        "{TEMP_SUFFIX}.{}.{}.{}",
        std::process::id(),
        counter,
        nanos
    ));
    Ok(path.with_file_name(temp))
}

/// Writes the temp file, flushes it, then renames it over the target.
fn write_and_rename(temp: &Path, target: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    rename_replacing(temp, target)
}

/// Returns the directory that contains `path`, falling back to the current
/// directory for a bare file name.
#[cfg(unix)]
fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Flushes the directory entry after a rename on Unix.
///
/// Some filesystems do not support directory fsync and report `EINVAL` or
/// `ENOTSUP`; that is treated as best effort rather than a failed write.
#[cfg(unix)]
fn sync_parent(path: &Path) -> io::Result<()> {
    let directory = StdFile::open(parent_dir(path))?;
    match directory.sync_all() {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// Windows has no portable directory handle in `std`; the rename is the
/// durability boundary here.
#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Renames a file over an existing target.
#[cfg(not(windows))]
fn rename_replacing(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)
}

/// Renames a file over an existing target, retrying a sharing violation.
#[cfg(windows)]
fn rename_replacing(from: &Path, to: &Path) -> io::Result<()> {
    use std::time::Duration;

    const ATTEMPTS: usize = 20;
    const RETRY_DELAY: Duration = Duration::from_millis(10);

    let mut last_error = None;
    for attempt in 0..ATTEMPTS {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(error) if is_sharing_violation(&error) && attempt + 1 < ATTEMPTS => {
                last_error = Some(error);
                std::thread::sleep(RETRY_DELAY);
            }
            Err(error) => return Err(error),
        }
    }

    match last_error {
        Some(error) => Err(error),
        None => Err(io::Error::other("rename retry exhausted")),
    }
}

/// Returns `true` for Windows errors that mean the target is temporarily open.
#[cfg(windows)]
fn is_sharing_violation(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(32) | Some(33))
}

#[cfg(test)]
mod tests;
