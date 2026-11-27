//! BEP 15 UDP tracker protocol.
//!
//! The wire format lives in [`codec`]: pure, bounds-checked functions over
//! byte slices with no socket and no clock. The attempt schedule and the
//! connected socket are added in later steps.
//!
//! Time in this module uses `tokio::time::Instant` only, never
//! `std::time::Instant`, so a paused clock controls every deadline in tests.

pub mod codec;

#[cfg(test)]
pub(crate) mod attempt;
