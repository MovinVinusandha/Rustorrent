//! Engine client API.
//!
//! Front-ends depend on this crate and never import engine internals. The
//! [`EngineClient`] trait is the only way to send commands and receive events.

use std::pin::Pin;

use async_trait::async_trait;
use tokio_stream::Stream;

mod in_process;

pub use bt_proto::{
    API_VERSION, AddOptions, Command, EngineError, EtaUpdate, Event, GlobalStats, Limits, Priority,
    Reply, Snapshot, Source, TorrentDelta, TorrentId, TorrentState, TorrentSummary, error_code,
};
pub use in_process::InProcessClient;

/// A boxed, sendable stream of engine events.
pub type EventStream = Pin<Box<dyn Stream<Item = Event> + Send + 'static>>;

/// Async client for a torrent engine.
#[async_trait]
pub trait EngineClient: Send + Sync {
    /// Sends a command and waits for the engine's reply.
    async fn send(&self, command: Command) -> Result<Reply, EngineError>;

    /// Returns a full snapshot of current engine state.
    async fn snapshot(&self) -> Result<Snapshot, EngineError>;

    /// Subscribes to engine events.
    ///
    /// If the subscriber falls behind, the stream emits [`Event::Resync`] and
    /// the consumer must call [`EngineClient::snapshot`] again.
    fn subscribe(&self) -> EventStream;
}
