//! Positioned file I/O.
//!
//! The wrapper hides the platform difference between Unix `read_at`/`write_at`
//! and Windows `seek_read`/`seek_write`. Both are safe APIs, so no `unsafe` is
//! used. Handles are not shared across threads because the Windows calls also
//! move the file cursor.

use std::fs::{File as StdFile, Metadata, OpenOptions};
use std::io;
use std::path::Path;

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

#[cfg(test)]
mod tests;
