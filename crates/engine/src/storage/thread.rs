//! Dedicated storage worker thread and its cloneable async handle.
//!
//! One worker owns one [`Storage`] for its whole life. Every blocking file
//! operation runs on that thread, never on a runtime worker. Callers use
//! [`StorageHandle`] from async tasks, and the bounded request queue turns a
//! slow disk into backpressure instead of growing memory.
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
/// `QUEUE_CAPACITY * MAX_RANGE_LEN` bytes of request payload.
pub const MAX_RANGE_LEN: usize = 1024 * 1024;

const THREAD_NAME: &str = "bt-storage";
const THREAD_STACK_SIZE: usize = 512 * 1024;

type Reply<T> = oneshot::Sender<Result<T, StorageError>>;

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

/// Cloneable, `Send + Sync` handle to one storage worker.
///
/// All methods are async and never block a runtime worker. A request is
/// enqueued when a queue slot becomes available; a caller that is cancelled
/// while waiting for a reply may still have its request executed. Requests
/// from one handle are processed in the order they were sent (FIFO per
/// sender); requests from different clones are interleaved.
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
    /// flag is set, new requests fail with [`StorageHandleError::ShuttingDown`];
    /// requests already queued finish first.
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

        let result = response
            .await
            .map_err(|_| StorageHandleError::WorkerStopped)?;
        result.map_err(StorageHandleError::Storage)
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
            .map_err(StorageHandleError::Storage)
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
    Hold {
        entered: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    },
    #[cfg(test)]
    Panic,
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
    let mut storage = Some(match Storage::open(&root, map, config) {
        Ok(storage) => {
            let _ = open.send(Ok(()));
            storage
        }
        Err(error) => {
            let _ = open.send(Err(error));
            return;
        }
    });

    while let Some(command) = rx.blocking_recv() {
        match command {
            Command::ReadRange { offset, len, reply } => {
                let Some(storage) = storage.as_mut() else {
                    let _ = reply.send(Err(StorageError::Internal {
                        reason: "storage is already closed",
                    }));
                    continue;
                };
                let len = len.min(MAX_RANGE_LEN);
                let mut buf = vec![0u8; len];
                let result = storage.read_range(offset, &mut buf);
                let reply_result = match result {
                    Ok(filled) => {
                        buf.truncate(filled);
                        Ok(buf)
                    }
                    Err(error) => Err(error),
                };
                let _ = reply.send(reply_result);
            }
            Command::WriteRange {
                offset,
                bytes,
                reply,
            } => {
                let Some(storage) = storage.as_mut() else {
                    let _ = reply.send(Err(StorageError::Internal {
                        reason: "storage is already closed",
                    }));
                    continue;
                };
                let _ = reply.send(storage.write_range(offset, &bytes));
            }
            Command::Sync { reply } => {
                let Some(storage) = storage.as_mut() else {
                    let _ = reply.send(Err(StorageError::Internal {
                        reason: "storage is already closed",
                    }));
                    continue;
                };
                let _ = reply.send(storage.sync());
            }
            Command::Shutdown { reply } => {
                let result = match storage.as_mut() {
                    Some(storage) => storage.close(),
                    None => Ok(()),
                };
                // Drop the storage so every file handle is closed before the
                // caller observes the reply.
                storage = None;
                let _ = reply.send(result);
                break;
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

    if let Some(mut storage) = storage {
        let _ = storage.close();
        drop(storage);
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
