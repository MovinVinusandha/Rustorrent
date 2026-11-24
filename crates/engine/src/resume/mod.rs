//! Versioned resume data.
//!
//! Resume documents are canonical bencode. They record verified piece bits and
//! the observed size and modification time of every mapped file. The engine
//! takes an explicit resume directory; choosing the OS data directory belongs
//! to the daemon.

mod format;

pub use format::{
    DecodeError, EncodeError, MAX_RESUME_BYTES, RESUME_VERSION, ResumeDocument, ResumeFile,
    decode_document, encode_document, layout_fingerprint,
};
