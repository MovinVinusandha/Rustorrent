//! On-disk storage.
//!
//! `layout` is pure path and range mapping. `backend` validates torrent paths
//! under the download root and owns file I/O. `thread` moves the backend onto a
//! dedicated worker with an async, bounded request queue.

pub mod backend;
pub mod layout;
pub mod thread;

pub use backend::{
    MAX_REPORT_PATHS, OBSERVE_CHUNK_FILES, ObservedFile, Preallocation, REMOVE_CHUNK_ITEMS,
    RemovalError, RemovalFailure, RemovalReport, Storage, StorageConfig, StorageError,
    UnsafeReason,
};
pub use thread::{MAX_RANGE_LEN, QUEUE_CAPACITY, StorageHandle, StorageHandleError};
