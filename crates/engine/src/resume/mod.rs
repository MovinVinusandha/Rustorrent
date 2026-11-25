//! Versioned resume data.
//!
//! Resume documents are canonical bencode. They record verified piece bits and
//! the observed size and modification time of every mapped file. The engine
//! takes an explicit resume directory; choosing the OS data directory belongs
//! to the daemon.

mod format;
mod load;
mod restore;
mod restore_run;
mod save;

pub use format::{
    DecodeError, EncodeError, MAX_RESUME_BYTES, MTIME_UNAVAILABLE, RESUME_VERSION, ResumeDocument,
    ResumeFile, decode_document, encode_document, layout_fingerprint,
};
pub use load::LoadError;
pub use restore::{
    RestoreError, RestorePlan, SPOT_CHECK_PIECES, ValidationError, plan_restore, validate_document,
};
pub use restore_run::{RestoreOutcome, RestoreRunError, RestoreSource};
pub use save::{RemoveError, ResumeRemoval, ResumeStore, SaveError};
