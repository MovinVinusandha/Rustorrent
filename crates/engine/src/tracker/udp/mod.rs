//! BEP 15 UDP tracker protocol.
//!
//! The wire format lives in [`codec`]: pure, bounds-checked functions over
//! byte slices with no socket and no clock. The `attempt` module is the pure
//! schedule; `transport` owns the connected socket.
//!
//! Time in this module uses `tokio::time::Instant` only, never
//! `std::time::Instant`, so a paused clock controls every deadline in tests.

#[cfg(feature = "fuzzing")]
pub mod attempt;
#[cfg(not(feature = "fuzzing"))]
pub(crate) mod attempt;
pub mod codec;
pub(crate) mod transport;
