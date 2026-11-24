//! Dedicated storage worker thread and its cloneable async handle.
//!
//! One worker owns one [`Storage`] until shutdown. Every blocking file
//! operation runs on that thread, never on a runtime worker. Callers use
//! [`StorageHandle`] from async tasks, and the bounded request queue turns a
//! slow disk into backpressure instead of growing memory.
//!
//! The bound covers queued payload only: at most
//! `QUEUE_CAPACITY * MAX_RANGE_LEN` bytes of write data. A caller blocked in
//! `reserve` still holds its own request buffer (up to 1 MiB), and each read
//! reply adds up to 1 MiB, so callers that issue many concurrent requests must
//! bound their own in-flight work.
//!
//! Requests are positioned byte ranges. Piece assembly, hashing, and recheck
//! live above this module and never run on the storage thread.

use std::path::PathBuf;
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use super::backend::{Storage, StorageConfig, StorageError};
use super::layout::FileMap;

/// Maximum number of requests waiting in the worker queue.
pub const QUEUE_CAPACITY: usize = 64;

/// Maximum number of bytes in one [`StorageHandle::read_range`] or
/// [`StorageHandle::write_range`] request.
///
/// Piece writes are split into chunks of this size, so the queue holds at most
/// `QUEUE_CAPACITY * MAX_RANGE_LEN` bytes of write payload.
pub const MAX_RANGE_LEN: usize = 1024 * 1024;

const THREAD_NAME: &str = "bt-storage";
const THREAD_STACK_SIZE: usize = 512 * 1024;

/// Errors returned by [`StorageHandle`].
#[derive(Debug, Error)]
pub enum StorageHandleError {
    /// The storage backend returned an error.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// The worker is shutting down and no longer accepts requests.
    #[error("storage worker is shutting down")]
    ShuttingDown,
    /// The worker exited, so the request was not completed.
    ///
    /// This covers a worker panic in unwind builds, a dropped request
    /// receiver, and a request lost while a shutdown was racing with it.
    #[error("storage worker stopped unexpectedly")]
    WorkerStopped,
    /// A range is larger than [`MAX_RANGE_LEN`].
    #[error("range length {len} exceeds the {max}-byte limit")]
    RangeTooLarge {
        /// Requested length.
        len: usize,
        /// Maximum accepted length.
        max: usize,
    },
    /// The worker thread could not be spawned.
    #[error("could not spawn the storage thread")]
    Spawn(#[source] std::io::Error),
}

type Reply<T> = oneshot::Sender<Result<T, StorageHandleError>>;

/// Cloneable, `Send + Sync` handle to one storage worker.
///
/// All methods are async and never block a runtime worker. A request is
/// enqueued when a queue slot becomes available; a caller that is cancelled
/// while waiting for a reply may still have its request executed. Sequential
/// requests from one handle are processed in order; requests from different
/// clones are interleaved.
///
/// Dropping the last handle closes the queue. The worker then performs a
/// best-effort close and exits without reporting errors. Call
/// [`StorageHandle::shutdown`] when the final sync result matters.
#[derive(Clone)]
pub struct StorageHandle {
    tx: mpsc::Sender<Command>,
    state: Arc<State>,
}

struct State {
    shutting_down: AtomicBool,
    #[cfg(test)]
    exit: Mutex<Option<oneshot::Receiver<()>>>,
}

impl StorageHandle {
    /// Spawns a worker and opens the storage on it.
    ///
    /// The root, map, and configuration are moved into the worker, so every
    /// blocking check, directory creation, and file open happens off the
    /// async runtime. A failed open ends the worker and is returned here.
    pub async fn open(
        root: PathBuf,
        map: Arc<FileMap>,
        config: StorageConfig,
    ) -> Result<Self, StorageHandleError> {
        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        let (open_tx, open_rx) = oneshot::channel();

        #[cfg(test)]
        let (exit_tx, exit_rx) = oneshot::channel();
        #[cfg(test)]
        let exit = Some(exit_tx);
        #[cfg(not(test))]
        let exit = None;

        // The thread is detached: nothing joins it, so a panic in the worker is
        // observable only as `WorkerStopped` on a later request (unwind
        // builds; release builds abort on panic).
        let _join = thread::Builder::new()
            .name(THREAD_NAME.to_owned())
            .stack_size(THREAD_STACK_SIZE)
            .spawn(move || run_worker(root, map, config, rx, open_tx, exit))
            .map_err(StorageHandleError::Spawn)?;

        match open_rx.await {
            Ok(Ok(())) => Ok(Self {
                tx,
                state: Arc::new(State {
                    shutting_down: AtomicBool::new(false),
                    #[cfg(test)]
                    exit: Mutex::new(Some(exit_rx)),
                }),
            }),
            Ok(Err(error)) => Err(StorageHandleError::Storage(error)),
            Err(_) => Err(StorageHandleError::WorkerStopped),
        }
    }

    /// Reads up to `len` bytes starting at `offset`.
    ///
    /// The result may be shorter than `len` at the end of the torrent or at
    /// the first missing or short file, matching [`Storage::read_range`].
    pub async fn read_range(&self, offset: u64, len: usize) -> Result<Vec<u8>, StorageHandleError> {
        if len > MAX_RANGE_LEN {
            return Err(StorageHandleError::RangeTooLarge {
                len,
                max: MAX_RANGE_LEN,
            });
        }
        self.send(|reply| Command::ReadRange { offset, len, reply })
            .await
    }

    /// Writes all bytes starting at `offset`.
    ///
    /// If this returns an error, part of the range may already be on disk.
    /// Callers must set verified state only after a complete success.
    pub async fn write_range(&self, offset: u64, bytes: Vec<u8>) -> Result<(), StorageHandleError> {
        if bytes.len() > MAX_RANGE_LEN {
            return Err(StorageHandleError::RangeTooLarge {
                len: bytes.len(),
                max: MAX_RANGE_LEN,
            });
        }
        self.send(|reply| Command::WriteRange {
            offset,
            bytes,
            reply,
        })
        .await
    }

    /// Flushes dirty data.
    ///
    /// Evicted dirty files are reopened and synced. A failed sync keeps the
    /// pending files so the caller can retry.
    pub async fn sync(&self) -> Result<(), StorageHandleError> {
        self.send(|reply| Command::Sync { reply }).await
    }

    /// Returns `true` once shutdown has begun or the worker has exited.
    pub fn is_closed(&self) -> bool {
        self.state.shutting_down.load(Ordering::Acquire) || self.tx.is_closed()
    }

    /// Drains queued requests, syncs, closes every file, then exits the worker.
    ///
    /// A queue slot is reserved before the shutdown flag is set, so cancelling
    /// this future while the queue is full leaves the handle usable. Once the
    /// flag is set, new requests fail with [`StorageHandleError::ShuttingDown`].
    /// Requests queued before `Shutdown` still finish first; a request that
    /// ends up behind it never executes and fails with
    /// [`StorageHandleError::ShuttingDown`], or with
    /// [`StorageHandleError::WorkerStopped`] when it arrives after the worker
    /// closed the queue and its reply is dropped.
    ///
    /// Cancelling this future after the flag is set does not undo the shutdown:
    /// the worker still closes and exits, and a retry fails with
    /// [`StorageHandleError::ShuttingDown`].
    ///
    /// The worker drops the [`Storage`] before it replies, so all file handles
    /// are closed when this returns. The return value is the final sync result:
    /// a failure is reported and the worker still exits. Call [`sync`](Self::sync)
    /// first when the caller needs to retry a failed flush.
    pub async fn shutdown(&self) -> Result<(), StorageHandleError> {
        if self.state.shutting_down.load(Ordering::Acquire) {
            return Err(StorageHandleError::ShuttingDown);
        }

        // Reserve first: dropping this future while it waits for a slot must
        // not set the flag or consume a slot.
        let permit = self
            .tx
            .reserve()
            .await
            .map_err(|_| StorageHandleError::WorkerStopped)?;

        if self
            .state
            .shutting_down
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(StorageHandleError::ShuttingDown);
        }

        let (reply, response) = oneshot::channel();
        permit.send(Command::Shutdown { reply });

        response
            .await
            .map_err(|_| StorageHandleError::WorkerStopped)?
    }

    async fn send<T>(
        &self,
        command: impl FnOnce(Reply<T>) -> Command,
    ) -> Result<T, StorageHandleError> {
        if self.state.shutting_down.load(Ordering::Acquire) {
            return Err(StorageHandleError::ShuttingDown);
        }

        let permit = self
            .tx
            .reserve()
            .await
            .map_err(|_| StorageHandleError::WorkerStopped)?;

        if self.state.shutting_down.load(Ordering::Acquire) {
            return Err(StorageHandleError::ShuttingDown);
        }

        let (reply, response) = oneshot::channel();
        permit.send(command(reply));
        response
            .await
            .map_err(|_| StorageHandleError::WorkerStopped)?
    }
}

#[cfg(test)]
impl StorageHandle {
    /// Arms a one-shot injected failure for the write after `writes`
    /// successful writes.
    ///
    /// The worker answers with a real [`StorageError::DiskFull`], so tests
    /// exercise the same mapping as a full device. This hook is compiled out
    /// of release builds.
    pub(crate) async fn fail_write_after(&self, writes: usize) -> Result<(), StorageHandleError> {
        self.send(|reply| Command::FailWriteAfter { writes, reply })
            .await
    }

    /// Arms a one-shot injected read failure after `reads` successful reads.
    ///
    /// The worker answers with a real [`StorageError::PermissionDenied`], so
    /// tests exercise the same mapping as a permission error. This hook is
    /// compiled out of release builds.
    pub(crate) async fn fail_read_after(&self, reads: usize) -> Result<(), StorageHandleError> {
        self.send(|reply| Command::FailReadAfter { reads, reply })
            .await
    }

    /// Blocks the worker once this command reaches the front of the queue.
    ///
    /// Returns after the worker has entered the hold, with a sender that
    /// releases it. This hook is compiled out of release builds.
    pub(crate) async fn hold(&self) -> Result<oneshot::Sender<()>, StorageHandleError> {
        let (entered, entered_rx) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        let permit = self
            .tx
            .reserve()
            .await
            .map_err(|_| StorageHandleError::WorkerStopped)?;
        permit.send(Command::Hold {
            entered,
            release: release_rx,
        });
        entered_rx
            .await
            .map_err(|_| StorageHandleError::WorkerStopped)?;
        Ok(release)
    }
}

/// One request in the worker queue.
enum Command {
    ReadRange {
        offset: u64,
        len: usize,
        reply: Reply<Vec<u8>>,
    },
    WriteRange {
        offset: u64,
        bytes: Vec<u8>,
        reply: Reply<()>,
    },
    Sync {
        reply: Reply<()>,
    },
    Shutdown {
        reply: Reply<()>,
    },
    #[cfg(test)]
    OpenHandles {
        reply: Reply<usize>,
    },
    #[cfg(test)]
    FailWriteAfter {
        writes: usize,
        reply: Reply<()>,
    },
    #[cfg(test)]
    FailReadAfter {
        reads: usize,
        reply: Reply<()>,
    },
    #[cfg(test)]
    Hold {
        entered: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    },
    #[cfg(test)]
    Panic,
}

/// Answers a request that was queued at or after shutdown without running it.
fn reject(command: Command) {
    match command {
        Command::ReadRange { reply, .. } => {
            let _ = reply.send(Err(StorageHandleError::ShuttingDown));
        }
        Command::WriteRange { reply, .. }
        | Command::Sync { reply }
        | Command::Shutdown { reply } => {
            let _ = reply.send(Err(StorageHandleError::ShuttingDown));
        }
        #[cfg(test)]
        Command::OpenHandles { reply } => {
            let _ = reply.send(Err(StorageHandleError::ShuttingDown));
        }
        #[cfg(test)]
        Command::FailWriteAfter { reply, .. } => {
            let _ = reply.send(Err(StorageHandleError::ShuttingDown));
        }
        #[cfg(test)]
        Command::FailReadAfter { reply, .. } => {
            let _ = reply.send(Err(StorageHandleError::ShuttingDown));
        }
        #[cfg(test)]
        Command::Hold { .. } => {}
        #[cfg(test)]
        Command::Panic => {}
    }
}

/// Owns the storage and serves requests until the queue closes.
fn run_worker(
    root: PathBuf,
    map: Arc<FileMap>,
    config: StorageConfig,
    mut rx: mpsc::Receiver<Command>,
    open: oneshot::Sender<Result<(), StorageError>>,
    exit: Option<oneshot::Sender<()>>,
) {
    let mut storage = match Storage::open(&root, map, config) {
        Ok(storage) => {
            let _ = open.send(Ok(()));
            storage
        }
        Err(error) => {
            let _ = open.send(Err(error));
            return;
        }
    };

    let mut shutdown = None;
    #[cfg(test)]
    let mut fail_write_after: Option<usize> = None;
    #[cfg(test)]
    let mut fail_read_after: Option<usize> = None;
    while let Some(command) = rx.blocking_recv() {
        match command {
            Command::ReadRange { offset, len, reply } => {
                #[cfg(test)]
                if fail_read_after == Some(0) {
                    fail_read_after = None;
                    let _ = reply.send(Err(StorageHandleError::Storage(
                        StorageError::PermissionDenied {
                            path: PathBuf::from("<injected>"),
                            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
                        },
                    )));
                    continue;
                }
                #[cfg(test)]
                if let Some(remaining) = fail_read_after.as_mut() {
                    *remaining -= 1;
                }
                let len = len.min(MAX_RANGE_LEN);
                let mut buf = vec![0u8; len];
                let result = match storage.read_range(offset, &mut buf) {
                    Ok(filled) => {
                        buf.truncate(filled);
                        Ok(buf)
                    }
                    Err(error) => Err(error),
                };
                let _ = reply.send(result.map_err(StorageHandleError::Storage));
            }
            Command::WriteRange {
                offset,
                bytes,
                reply,
            } => {
                #[cfg(test)]
                if fail_write_after == Some(0) {
                    fail_write_after = None;
                    let _ = reply.send(Err(StorageHandleError::Storage(StorageError::DiskFull {
                        path: PathBuf::from("<injected>"),
                        source: std::io::Error::from(std::io::ErrorKind::StorageFull),
                    })));
                    continue;
                }
                #[cfg(test)]
                if let Some(remaining) = fail_write_after.as_mut() {
                    *remaining -= 1;
                }
                let _ = reply.send(
                    storage
                        .write_range(offset, &bytes)
                        .map_err(StorageHandleError::Storage),
                );
            }
            Command::Sync { reply } => {
                let _ = reply.send(storage.sync().map_err(StorageHandleError::Storage));
            }
            Command::Shutdown { reply } => {
                shutdown = Some(reply);
                break;
            }
            #[cfg(test)]
            Command::OpenHandles { reply } => {
                let _ = reply.send(Ok(storage.open_handle_count()));
            }
            #[cfg(test)]
            Command::FailWriteAfter { writes, reply } => {
                fail_write_after = Some(writes);
                let _ = reply.send(Ok(()));
            }
            #[cfg(test)]
            Command::FailReadAfter { reads, reply } => {
                fail_read_after = Some(reads);
                let _ = reply.send(Ok(()));
            }
            #[cfg(test)]
            Command::Hold { entered, release } => {
                let _ = entered.send(());
                let _ = release.blocking_recv();
            }
            #[cfg(test)]
            Command::Panic => panic!("storage worker test panic"),
        }
    }

    // Stop accepting reservations, then answer everything that was queued
    // behind `Shutdown`. Rejecting instead of running it keeps the "nothing
    // queued after shutdown executes" guarantee for callers that raced with
    // the flag.
    rx.close();
    while let Ok(command) = rx.try_recv() {
        reject(command);
    }

    let result = storage.close().map_err(StorageHandleError::Storage);
    // Drop the storage so every file handle is closed before the caller
    // observes the reply.
    drop(storage);

    if let Some(reply) = shutdown {
        let _ = reply.send(result);
    }

    #[cfg(test)]
    if let Some(exit) = exit {
        let _ = exit.send(());
    }
    #[cfg(not(test))]
    let _ = exit;
}

#[cfg(test)]
mod tests;
