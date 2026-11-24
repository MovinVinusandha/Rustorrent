//! Versioned resume data.
//!
//! Resume documents are canonical bencode. They record verified piece bits and
//! the observed size and modification time of every mapped file. The engine
//! takes an explicit resume directory; choosing the OS data directory belongs
//! to the daemon.

mod format;
mod restore;
mod save;

pub use format::{
    DecodeError, EncodeError, MAX_RESUME_BYTES, MTIME_UNAVAILABLE, RESUME_VERSION, ResumeDocument,
    ResumeFile, decode_document, encode_document, layout_fingerprint,
};
pub use restore::{
    RestoreError, RestorePlan, SPOT_CHECK_PIECES, ValidationError, plan_restore, validate_document,
};
pub use save::{ResumeStore, SaveError};
