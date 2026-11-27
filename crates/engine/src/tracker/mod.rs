//! Tracker URL parsing and, later, announce decoding.
//!
//! The module is pure: parsing and decoding perform no network I/O. URL
//! parsing lives in [`url`]; transports and response decoding are added in
//! later steps.

use thiserror::Error;

pub mod url;

pub use url::{
    Host, MAX_HOST_LEN, MAX_LABEL_LEN, MAX_REDACTED_URL_LEN, MAX_URL_LEN, Scheme, TrackerUrl,
};

/// Errors produced by tracker parsing and announce transports.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum TrackerError {
    /// The input is not a valid tracker URL.
    #[error("<invalid tracker url>")]
    InvalidUrl,
    /// The URL scheme is not `http`, `https` or `udp`.
    #[error("unsupported tracker URL scheme")]
    UnsupportedScheme,
}
