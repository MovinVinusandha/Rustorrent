//! Opaque tracker identifiers.
//!
//! A `tracker id` is echoed only to the tracker that issued it. It is never
//! displayed: `Debug` and `Display` print only the length, so a value that
//! contains a passkey cannot leak through a log line.

use std::fmt;

/// Maximum accepted `tracker id` length in bytes.
pub const MAX_TRACKER_ID_LEN: usize = 256;

/// An opaque tracker identifier.
#[derive(Clone, PartialEq, Eq)]
pub struct TrackerId(Vec<u8>);

impl TrackerId {
    /// Returns an identifier, or `None` when the value is too long.
    ///
    /// A value longer than [`MAX_TRACKER_ID_LEN`] is ignored, not truncated.
    pub fn new(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_TRACKER_ID_LEN {
            return None;
        }
        Some(Self(bytes.to_vec()))
    }

    /// Returns the identifier bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Returns the identifier length in bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns `true` when the identifier is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for TrackerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "TrackerId({} bytes)", self.0.len())
    }
}

impl fmt::Display for TrackerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "TrackerId({} bytes)", self.0.len())
    }
}

#[cfg(test)]
mod tests;
