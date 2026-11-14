//! Versioned command and event types shared by the engine and all front-ends.
//!
//! This crate intentionally contains only serializable data definitions. It has
//! no engine logic and does not depend on the engine or any front-end crate.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Version of the command/event API.
///
/// Bump this only for incompatible wire-format changes. New optional fields are
/// backward compatible and do not require a version bump.
pub const API_VERSION: u32 = 1;

/// Opaque identifier for a torrent managed by the engine.
///
/// Clients must treat this as an opaque string. The engine may change how it is
/// derived, so do not parse it or rely on its contents.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TorrentId(String);

impl TorrentId {
    /// Creates an identifier from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Returns the identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TorrentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for TorrentId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for TorrentId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// Where a torrent comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Source {
    /// A magnet link.
    Magnet(String),
    /// A path to a `.torrent` file.
    File(PathBuf),
    /// Raw `.torrent` bytes.
    Bytes(Vec<u8>),
}

/// Options for adding a torrent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddOptions {
    /// The torrent source.
    pub source: Source,
    /// Optional download directory. `None` uses the engine default.
    #[serde(default)]
    pub download_dir: Option<PathBuf>,
    /// Whether the torrent starts paused.
    #[serde(default)]
    pub paused: bool,
}

impl AddOptions {
    /// Creates options for `source` with engine defaults.
    pub fn new(source: Source) -> Self {
        Self {
            source,
            download_dir: None,
            paused: false,
        }
    }
}

/// Priority for a file within a torrent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Do not download this file.
    Skip,
    /// Low priority.
    Low,
    /// Normal priority.
    #[default]
    Normal,
    /// High priority.
    High,
}

/// Global transfer limits.
///
/// `None` means unlimited. `Some(0)` stops the corresponding transfer direction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// Download limit in bytes per second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_bps: Option<u64>,
    /// Upload limit in bytes per second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_bps: Option<u64>,
}

/// Lifecycle state of a torrent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TorrentState {
    /// Waiting for a download slot.
    #[default]
    Queued,
    /// Verifying existing data.
    Checking,
    /// Downloading data.
    Downloading,
    /// Complete and uploading.
    Seeding,
    /// Paused by the user.
    Paused,
    /// Stopped because of an error.
    Error,
}

/// Full state of one torrent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TorrentSummary {
    /// Opaque torrent identifier.
    pub id: TorrentId,
    /// Display name.
    pub name: String,
    /// Current lifecycle state.
    pub state: TorrentState,
    /// Completion fraction in the range `0.0..=1.0`.
    pub progress: f32,
    /// Total size in bytes.
    pub total_bytes: u64,
    /// Bytes downloaded in this session.
    pub downloaded_bytes: u64,
    /// Bytes uploaded in this session.
    pub uploaded_bytes: u64,
    /// Current download speed in bytes per second.
    pub download_bps: u64,
    /// Current upload speed in bytes per second.
    pub upload_bps: u64,
    /// Number of connected peers.
    pub peers_connected: u32,
    /// Estimated seconds remaining, or `None` when unknown.
    pub eta_secs: Option<u64>,
}

/// Update to a torrent's ETA.
///
/// This distinguishes "no change" from "ETA is now unknown", which a plain
/// `Option<u64>` cannot represent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EtaUpdate {
    /// Leave the current ETA unchanged.
    #[default]
    Unchanged,
    /// Clear the ETA because it is now unknown.
    Clear,
    /// Set the ETA to this many seconds.
    Set(u64),
}

impl EtaUpdate {
    /// Returns `true` when this update carries no change.
    pub fn is_unchanged(&self) -> bool {
        matches!(self, Self::Unchanged)
    }
}

/// Only-changed-fields update for one torrent.
///
/// Every `Option` field is `None` when the value did not change. Use
/// [`EtaUpdate`] for the nullable ETA field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TorrentDelta {
    /// Opaque torrent identifier.
    pub id: TorrentId,
    /// New lifecycle state, if changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<TorrentState>,
    /// New completion fraction, if changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<f32>,
    /// New downloaded-byte count, if changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downloaded_bytes: Option<u64>,
    /// New uploaded-byte count, if changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uploaded_bytes: Option<u64>,
    /// New download speed, if changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_bps: Option<u64>,
    /// New upload speed, if changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_bps: Option<u64>,
    /// New connected-peer count, if changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peers_connected: Option<u32>,
    /// ETA change, or [`EtaUpdate::Unchanged`].
    #[serde(default, skip_serializing_if = "EtaUpdate::is_unchanged")]
    pub eta: EtaUpdate,
}

impl TorrentDelta {
    /// Creates an empty delta for `id`.
    pub fn new(id: TorrentId) -> Self {
        Self {
            id,
            state: None,
            progress: None,
            downloaded_bytes: None,
            uploaded_bytes: None,
            download_bps: None,
            upload_bps: None,
            peers_connected: None,
            eta: EtaUpdate::Unchanged,
        }
    }
}

/// Global engine statistics.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalStats {
    /// Current download speed in bytes per second.
    pub download_bps: u64,
    /// Current upload speed in bytes per second.
    pub upload_bps: u64,
    /// Total number of torrents.
    pub torrents_total: u32,
    /// Number of torrents actively transferring or checking.
    pub torrents_active: u32,
    /// Number of connected peers across all torrents.
    pub peers_connected: u32,
    /// Total bytes downloaded in this session.
    pub downloaded_bytes: u64,
    /// Total bytes uploaded in this session.
    pub uploaded_bytes: u64,
}

/// Typed engine error with a stable machine-readable code.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[error("{message} (code: {code})")]
pub struct EngineError {
    /// Stable error code. See [`error_code`].
    pub code: String,
    /// Human-readable message. Do not parse this.
    pub message: String,
}

impl EngineError {
    /// Creates an error from a stable code and a human-readable message.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Stable error codes used in [`EngineError::code`].
pub mod error_code {
    /// The engine is not running or its command channel is closed.
    pub const ENGINE_UNAVAILABLE: &str = "engine_unavailable";
    /// The requested torrent identifier is unknown.
    pub const TORRENT_NOT_FOUND: &str = "torrent_not_found";
    /// The command is recognized but not implemented yet.
    pub const NOT_IMPLEMENTED: &str = "not_implemented";
}

/// Successful reply to a [`Command`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    /// The command completed without a new identifier.
    Ok,
    /// The command added a torrent with this identifier.
    Added {
        /// Identifier assigned to the new torrent.
        id: TorrentId,
    },
}

/// Full engine state for a new or reconnecting client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// API version of the engine that produced this snapshot.
    pub api_version: u32,
    /// Current torrents.
    pub torrents: Vec<TorrentSummary>,
    /// Global statistics.
    pub stats: GlobalStats,
    /// Current global limits.
    pub limits: Limits,
}

impl Snapshot {
    /// Creates an empty snapshot for `api_version`.
    pub fn empty(api_version: u32) -> Self {
        Self {
            api_version,
            torrents: Vec::new(),
            stats: GlobalStats::default(),
            limits: Limits::default(),
        }
    }
}

/// Command sent to the engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", content = "args", rename_all = "snake_case")]
pub enum Command {
    /// Add a torrent.
    AddTorrent(AddOptions),
    /// Pause a torrent.
    Pause {
        /// Torrent to pause.
        id: TorrentId,
    },
    /// Resume a torrent.
    Resume {
        /// Torrent to resume.
        id: TorrentId,
    },
    /// Remove a torrent.
    Remove {
        /// Torrent to remove.
        id: TorrentId,
        /// Whether to delete the downloaded files.
        delete_files: bool,
    },
    /// Set the priority of one file in a torrent.
    SetFilePriority {
        /// Torrent that owns the file.
        id: TorrentId,
        /// Zero-based file index within the torrent.
        file_index: u32,
        /// New priority.
        priority: Priority,
    },
    /// Replace the global transfer limits.
    SetLimits {
        /// New limits.
        limits: Limits,
    },
}

/// Event emitted by the engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum Event {
    /// A torrent was added.
    TorrentAdded(TorrentSummary),
    /// A torrent changed.
    TorrentUpdated(TorrentDelta),
    /// A torrent was removed.
    TorrentRemoved {
        /// Identifier of the removed torrent.
        id: TorrentId,
    },
    /// Global statistics changed.
    Stats(GlobalStats),
    /// The engine reported an error.
    Error(EngineError),
    /// The subscriber lagged and missed events. Re-fetch [`Snapshot`].
    Resync,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::DeserializeOwned;

    fn roundtrip<T>(value: T)
    where
        T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(&value).expect("serialize");
        let decoded: T = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(value, decoded);
    }

    fn sample_summary() -> TorrentSummary {
        TorrentSummary {
            id: TorrentId::new("torrent-1"),
            name: "Example".to_owned(),
            state: TorrentState::Downloading,
            progress: 0.5,
            total_bytes: 1_000,
            downloaded_bytes: 500,
            uploaded_bytes: 25,
            download_bps: 100,
            upload_bps: 10,
            peers_connected: 3,
            eta_secs: Some(5),
        }
    }

    fn sample_delta() -> TorrentDelta {
        let mut delta = TorrentDelta::new(TorrentId::new("torrent-1"));
        delta.state = Some(TorrentState::Seeding);
        delta.progress = Some(1.0);
        delta.eta = EtaUpdate::Clear;
        delta
    }

    #[test]
    fn torrent_id_roundtrip() {
        roundtrip(TorrentId::new("abc-123"));
    }

    #[test]
    fn source_roundtrip() {
        roundtrip(Source::Magnet("magnet:?xt=urn:btih:abc".to_owned()));
        roundtrip(Source::File(PathBuf::from("downloads/example.torrent")));
        roundtrip(Source::Bytes(vec![1, 2, 3, 4]));
    }

    #[test]
    fn add_options_roundtrip() {
        roundtrip(AddOptions::new(Source::Bytes(vec![1, 2, 3])));
        roundtrip(AddOptions {
            source: Source::File(PathBuf::from("example.torrent")),
            download_dir: Some(PathBuf::from("downloads")),
            paused: true,
        });
    }

    #[test]
    fn priority_roundtrip() {
        roundtrip(Priority::Skip);
        roundtrip(Priority::Low);
        roundtrip(Priority::Normal);
        roundtrip(Priority::High);
    }

    #[test]
    fn limits_roundtrip() {
        roundtrip(Limits::default());
        roundtrip(Limits {
            download_bps: Some(1_000_000),
            upload_bps: Some(0),
        });
    }

    #[test]
    fn torrent_state_roundtrip() {
        roundtrip(TorrentState::Queued);
        roundtrip(TorrentState::Checking);
        roundtrip(TorrentState::Downloading);
        roundtrip(TorrentState::Seeding);
        roundtrip(TorrentState::Paused);
        roundtrip(TorrentState::Error);
    }

    #[test]
    fn torrent_summary_roundtrip() {
        roundtrip(sample_summary());
    }

    #[test]
    fn eta_update_roundtrip() {
        roundtrip(EtaUpdate::Unchanged);
        roundtrip(EtaUpdate::Clear);
        roundtrip(EtaUpdate::Set(42));
    }

    #[test]
    fn torrent_delta_roundtrip() {
        roundtrip(TorrentDelta::new(TorrentId::new("torrent-1")));
        roundtrip(sample_delta());
    }

    #[test]
    fn global_stats_roundtrip() {
        roundtrip(GlobalStats {
            download_bps: 100,
            upload_bps: 50,
            torrents_total: 2,
            torrents_active: 1,
            peers_connected: 4,
            downloaded_bytes: 1_000,
            uploaded_bytes: 500,
        });
    }

    #[test]
    fn engine_error_roundtrip() {
        roundtrip(EngineError::new(
            error_code::TORRENT_NOT_FOUND,
            "no such torrent",
        ));
    }

    #[test]
    fn reply_roundtrip() {
        roundtrip(Reply::Ok);
        roundtrip(Reply::Added {
            id: TorrentId::new("torrent-1"),
        });
    }

    #[test]
    fn snapshot_roundtrip() {
        roundtrip(Snapshot {
            api_version: API_VERSION,
            torrents: vec![sample_summary()],
            stats: GlobalStats::default(),
            limits: Limits::default(),
        });
    }

    #[test]
    fn command_roundtrip() {
        let id = TorrentId::new("torrent-1");
        roundtrip(Command::AddTorrent(AddOptions::new(Source::Magnet(
            "magnet:?xt=urn:btih:abc".to_owned(),
        ))));
        roundtrip(Command::Pause { id: id.clone() });
        roundtrip(Command::Resume { id: id.clone() });
        roundtrip(Command::Remove {
            id: id.clone(),
            delete_files: true,
        });
        roundtrip(Command::SetFilePriority {
            id,
            file_index: 2,
            priority: Priority::High,
        });
        roundtrip(Command::SetLimits {
            limits: Limits {
                download_bps: Some(1_000),
                upload_bps: None,
            },
        });
    }

    #[test]
    fn event_roundtrip() {
        let id = TorrentId::new("torrent-1");
        roundtrip(Event::TorrentAdded(sample_summary()));
        roundtrip(Event::TorrentUpdated(sample_delta()));
        roundtrip(Event::TorrentRemoved { id });
        roundtrip(Event::Stats(GlobalStats::default()));
        roundtrip(Event::Error(EngineError::new(
            error_code::NOT_IMPLEMENTED,
            "not implemented",
        )));
        roundtrip(Event::Resync);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_path_serialization_errors_on_unix() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let path = PathBuf::from(OsString::from_vec(vec![0xff, 0xfe]));
        let result = serde_json::to_string(&Source::File(path));
        assert!(result.is_err(), "non-UTF-8 path must not serialize");
    }

    #[cfg(windows)]
    #[test]
    fn non_utf8_path_serialization_errors_on_windows() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;

        let path = PathBuf::from(OsString::from_wide(&[0xd800]));
        let result = serde_json::to_string(&Source::File(path));
        assert!(result.is_err(), "non-UTF-8 path must not serialize");
    }
}
