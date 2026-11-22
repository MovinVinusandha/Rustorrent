//! On-disk storage layout.
//!
//! Roadmap 3a is pure: it sanitizes torrent paths and maps files to byte and
//! piece ranges without touching the filesystem. The storage backend is added
//! in 3b.

pub mod layout;
