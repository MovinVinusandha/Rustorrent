//! In-process stub implementation of [`crate::EngineClient`].

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bt_proto::{
    API_VERSION, Command, EngineError, Event, Reply, Snapshot, Source, TorrentDelta, TorrentId,
    TorrentState, TorrentSummary, error_code,
};
use tokio::runtime::Handle;
use tokio::sync::{RwLock, broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::{EngineClient, EventStream};

/// Bounded command queue capacity.
const COMMAND_CAPACITY: usize = 64;
/// Bounded event broadcast capacity.
const EVENT_CAPACITY: usize = 256;

/// A command plus the channel used to send its reply back to the caller.
type CommandEnvelope = (Command, oneshot::Sender<Result<Reply, EngineError>>);

/// Result of processing one command: the reply and an optional event.
type CommandResult = Result<(Reply, Option<Event>), EngineError>;

/// Owns the stub task and aborts it when the last client clone is dropped.
struct StubTask {
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl StubTask {
    fn new(handle: JoinHandle<()>) -> Self {
        Self {
            handle: Mutex::new(Some(handle)),
        }
    }
}

impl Drop for StubTask {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.handle.lock()
            && let Some(handle) = guard.take()
        {
            handle.abort();
        }
    }
}

/// In-process engine client backed by bounded Tokio channels.
///
/// This is a stub: it does not download anything and never touches disk. It
/// exists so front-ends can be developed against the real [`EngineClient`] API.
/// Dropping the last clone aborts the stub task.
#[derive(Clone)]
pub struct InProcessClient {
    commands: mpsc::Sender<CommandEnvelope>,
    events: broadcast::Sender<Event>,
    snapshot: Arc<RwLock<Snapshot>>,
    _task: Arc<StubTask>,
}

impl InProcessClient {
    /// Starts the in-process stub.
    ///
    /// This must be called inside a Tokio runtime. It returns
    /// [`error_code::ENGINE_UNAVAILABLE`] otherwise.
    pub fn spawn() -> Result<Self, EngineError> {
        Self::spawn_with_api_version(API_VERSION)
    }

    /// Starts the in-process stub reporting `api_version` in its snapshots.
    ///
    /// Production code passes [`API_VERSION`]; tests use another value to
    /// exercise the snapshot version check.
    fn spawn_with_api_version(api_version: u32) -> Result<Self, EngineError> {
        let handle = Handle::try_current().map_err(|_| {
            EngineError::new(
                error_code::ENGINE_UNAVAILABLE,
                "InProcessClient::spawn requires a Tokio runtime",
            )
        })?;

        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let snapshot = Arc::new(RwLock::new(Snapshot::empty(api_version)));

        let task = handle.spawn(run_stub(command_rx, events.clone(), Arc::clone(&snapshot)));

        Ok(Self {
            commands,
            events,
            snapshot,
            _task: Arc::new(StubTask::new(task)),
        })
    }
}

#[async_trait]
impl EngineClient for InProcessClient {
    async fn send(&self, command: Command) -> Result<Reply, EngineError> {
        let (reply_tx, reply_rx) = oneshot::channel();

        self.commands.send((command, reply_tx)).await.map_err(|_| {
            EngineError::new(
                error_code::ENGINE_UNAVAILABLE,
                "engine command channel is closed",
            )
        })?;

        reply_rx.await.map_err(|_| {
            EngineError::new(
                error_code::ENGINE_UNAVAILABLE,
                "engine dropped the command reply",
            )
        })?
    }

    async fn snapshot(&self) -> Result<Snapshot, EngineError> {
        let snapshot = self.snapshot.read().await.clone();
        snapshot.check_api_version().map_err(|error| {
            EngineError::new(error_code::API_VERSION_MISMATCH, error.to_string())
        })?;
        Ok(snapshot)
    }

    fn subscribe(&self) -> EventStream {
        let stream = BroadcastStream::new(self.events.subscribe()).map(|item| match item {
            Ok(event) => event,
            Err(_) => Event::Resync,
        });

        Box::pin(stream)
    }
}

async fn run_stub(
    mut commands: mpsc::Receiver<CommandEnvelope>,
    events: broadcast::Sender<Event>,
    snapshot: Arc<RwLock<Snapshot>>,
) {
    let mut next_id = 1_u64;

    while let Some((command, reply_tx)) = commands.recv().await {
        let result = handle_command(command, &mut next_id, snapshot.as_ref()).await;

        let (reply, event) = match result {
            Ok((reply, event)) => (Ok(reply), event),
            Err(error) => (Err(error), None),
        };

        let _ = reply_tx.send(reply);

        if let Some(event) = event {
            let _ = events.send(event);
        }
    }
}

async fn handle_command(
    command: Command,
    next_id: &mut u64,
    snapshot: &RwLock<Snapshot>,
) -> CommandResult {
    match command {
        Command::AddTorrent(options) => {
            let id = TorrentId::new(format!("stub-{next_id}"));
            *next_id = (*next_id).saturating_add(1);

            let summary = TorrentSummary {
                id: id.clone(),
                name: stub_name(&options.source),
                state: if options.paused {
                    TorrentState::Paused
                } else {
                    TorrentState::Downloading
                },
                progress: 0.0,
                total_bytes: 0,
                downloaded_bytes: 0,
                uploaded_bytes: 0,
                download_bps: 0,
                upload_bps: 0,
                peers_connected: 0,
                eta_secs: None,
            };

            let mut state = snapshot.write().await;
            state.torrents.push(summary.clone());
            refresh_stats(&mut state);
            drop(state);

            Ok((Reply::Added { id }, Some(Event::TorrentAdded(summary))))
        }
        Command::Pause { id } => set_state(snapshot, id, TorrentState::Paused).await,
        Command::Resume { id } => set_state(snapshot, id, TorrentState::Downloading).await,
        Command::Remove { id, delete_files } => {
            if delete_files {
                return Err(EngineError::new(
                    error_code::NOT_IMPLEMENTED,
                    "deleting torrent files is not implemented",
                ));
            }

            let mut state = snapshot.write().await;
            let before = state.torrents.len();
            state.torrents.retain(|torrent| torrent.id != id);

            if state.torrents.len() == before {
                return Err(EngineError::new(
                    error_code::TORRENT_NOT_FOUND,
                    format!("unknown torrent: {id}"),
                ));
            }

            refresh_stats(&mut state);
            drop(state);

            Ok((Reply::Ok, Some(Event::TorrentRemoved { id })))
        }
        Command::SetFilePriority { .. } => Err(EngineError::new(
            error_code::NOT_IMPLEMENTED,
            "file priorities are not implemented by the in-process stub",
        )),
        Command::SetLimits { limits } => {
            let mut state = snapshot.write().await;
            state.limits = limits;
            let stats = state.stats.clone();
            drop(state);

            Ok((Reply::Ok, Some(Event::Stats(stats))))
        }
    }
}

async fn set_state(
    snapshot: &RwLock<Snapshot>,
    id: TorrentId,
    new_state: TorrentState,
) -> CommandResult {
    let mut state = snapshot.write().await;

    let Some(summary) = state.torrents.iter_mut().find(|torrent| torrent.id == id) else {
        return Err(EngineError::new(
            error_code::TORRENT_NOT_FOUND,
            format!("unknown torrent: {id}"),
        ));
    };

    summary.state = new_state;
    if matches!(new_state, TorrentState::Paused) {
        summary.download_bps = 0;
        summary.upload_bps = 0;
    }

    let download_bps = summary.download_bps;
    let upload_bps = summary.upload_bps;

    refresh_stats(&mut state);
    drop(state);

    let delta = TorrentDelta {
        state: Some(new_state),
        download_bps: Some(download_bps),
        upload_bps: Some(upload_bps),
        ..TorrentDelta::new(id)
    };

    Ok((Reply::Ok, Some(Event::TorrentUpdated(delta))))
}

fn refresh_stats(snapshot: &mut Snapshot) {
    let total = u32::try_from(snapshot.torrents.len()).unwrap_or(u32::MAX);
    let active = u32::try_from(
        snapshot
            .torrents
            .iter()
            .filter(|torrent| {
                matches!(
                    torrent.state,
                    TorrentState::Checking | TorrentState::Downloading | TorrentState::Seeding
                )
            })
            .count(),
    )
    .unwrap_or(u32::MAX);

    snapshot.stats.torrents_total = total;
    snapshot.stats.torrents_active = active;
}

fn stub_name(source: &Source) -> String {
    match source {
        Source::Magnet(_) => "magnet (stub)".to_owned(),
        Source::File(path) => path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "torrent (stub)".to_owned()),
        Source::Bytes(_) => "torrent bytes (stub)".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bt_proto::{API_VERSION, AddOptions, Priority};
    use tokio::time::{Duration, timeout};
    use tokio_stream::StreamExt;

    use crate::EngineClient;

    #[test]
    fn spawn_without_runtime_fails() {
        let result = InProcessClient::spawn();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn commands_and_events_flow() {
        let client = InProcessClient::spawn().expect("runtime");
        let mut events = client.subscribe();

        let reply = client
            .send(Command::AddTorrent(AddOptions::new(Source::Magnet(
                "magnet:?xt=urn:btih:abc".to_owned(),
            ))))
            .await
            .expect("add torrent");

        let id = match reply {
            Reply::Added { id } => id,
            other => panic!("unexpected reply: {other:?}"),
        };

        let event = timeout(Duration::from_secs(1), events.next())
            .await
            .expect("event timeout")
            .expect("event stream");

        match event {
            Event::TorrentAdded(summary) => assert_eq!(summary.id, id),
            other => panic!("unexpected event: {other:?}"),
        }

        let snapshot = client.snapshot().await.expect("snapshot");
        assert_eq!(snapshot.torrents.len(), 1);
        assert_eq!(snapshot.stats.torrents_total, 1);
        assert_eq!(snapshot.stats.torrents_active, 1);

        client
            .send(Command::Pause { id: id.clone() })
            .await
            .expect("pause");

        let event = timeout(Duration::from_secs(1), events.next())
            .await
            .expect("event timeout")
            .expect("event stream");

        match event {
            Event::TorrentUpdated(delta) => {
                assert_eq!(delta.id, id);
                assert_eq!(delta.state, Some(TorrentState::Paused));
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let snapshot = client.snapshot().await.expect("snapshot");
        assert_eq!(snapshot.torrents[0].state, TorrentState::Paused);
        assert_eq!(snapshot.stats.torrents_active, 0);
    }

    #[tokio::test]
    async fn unknown_torrent_returns_error() {
        let client = InProcessClient::spawn().expect("runtime");

        let error = client
            .send(Command::Pause {
                id: TorrentId::new("missing"),
            })
            .await
            .expect_err("unknown torrent must fail");

        assert_eq!(error.code, error_code::TORRENT_NOT_FOUND);
    }

    #[tokio::test]
    async fn unimplemented_command_returns_error() {
        let client = InProcessClient::spawn().expect("runtime");

        let error = client
            .send(Command::SetFilePriority {
                id: TorrentId::new("missing"),
                file_index: 0,
                priority: Priority::Normal,
            })
            .await
            .expect_err("stub must report not implemented");

        assert_eq!(error.code, error_code::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn mismatched_snapshot_version_is_rejected() {
        let client = InProcessClient::spawn_with_api_version(API_VERSION + 1).expect("runtime");

        let error = client
            .snapshot()
            .await
            .expect_err("snapshot version must be checked");

        assert_eq!(error.code, error_code::API_VERSION_MISMATCH);
        assert!(error.message.contains(&(API_VERSION + 1).to_string()));
    }

    #[tokio::test]
    async fn delete_files_removal_is_rejected() {
        let client = InProcessClient::spawn().expect("runtime");
        let reply = client
            .send(Command::AddTorrent(AddOptions::new(Source::Magnet(
                "magnet:?xt=urn:btih:abc".to_owned(),
            ))))
            .await
            .expect("add torrent");
        let id = match reply {
            Reply::Added { id } => id,
            other => panic!("unexpected reply: {other:?}"),
        };

        let error = client
            .send(Command::Remove {
                id: id.clone(),
                delete_files: true,
            })
            .await
            .expect_err("deleting files must be rejected");
        assert_eq!(error.code, error_code::NOT_IMPLEMENTED);
        assert_eq!(client.snapshot().await.expect("snapshot").torrents.len(), 1);

        client
            .send(Command::Remove {
                id,
                delete_files: false,
            })
            .await
            .expect("remove without deleting files");
        assert!(
            client
                .snapshot()
                .await
                .expect("snapshot")
                .torrents
                .is_empty()
        );
    }

    #[tokio::test]
    async fn slow_subscriber_gets_resync() {
        let client = InProcessClient::spawn().expect("runtime");
        let mut events = client.subscribe();

        for index in 0..=EVENT_CAPACITY {
            client
                .send(Command::AddTorrent(AddOptions::new(Source::Magnet(
                    format!("magnet:?xt=urn:btih:{index}"),
                ))))
                .await
                .expect("add torrent");
        }

        let event = timeout(Duration::from_secs(5), events.next())
            .await
            .expect("event timeout")
            .expect("event stream");

        assert_eq!(event, Event::Resync);
    }
}
