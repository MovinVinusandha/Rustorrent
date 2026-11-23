//! On-disk storage.
//!
//! `layout` is pure path and range mapping. `backend` validates torrent paths
//! under the download root and owns file I/O.

pub mod backend;
pub mod layout;

pub use backend::{Preallocation, Storage, StorageConfig, StorageError, UnsafeReason};
