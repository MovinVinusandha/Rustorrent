//! TUI application state and pure update logic.

use bt_client::{
    Command, EtaUpdate, Event, GlobalStats, Limits, Snapshot, TorrentDelta, TorrentState,
    TorrentSummary,
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// All state needed to draw the TUI.
#[derive(Debug, Default)]
pub struct AppState {
    /// Torrents currently known to the engine.
    pub torrents: Vec<TorrentSummary>,
    /// Global engine statistics.
    pub stats: GlobalStats,
    /// Current global limits.
    pub limits: Limits,
    /// Index of the selected torrent.
    pub selected: usize,
    /// Whether the help overlay is visible.
    pub show_help: bool,
    /// Last error or status message.
    pub status: Option<String>,
}

impl AppState {
    /// Builds application state from an engine snapshot.
    pub fn from_snapshot(snapshot: Snapshot) -> Self {
        let mut state = Self::default();
        state.apply_snapshot(snapshot);
        state
    }

    /// Replaces engine-owned fields from a snapshot, keeping UI-only state.
    ///
    /// This is used after [`bt_client::Event::Resync`] so the selection, help
    /// overlay, and status message survive a re-fetch.
    pub fn apply_snapshot(&mut self, snapshot: Snapshot) {
        self.torrents = snapshot.torrents;
        self.stats = snapshot.stats;
        self.limits = snapshot.limits;
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        if self.torrents.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.torrents.len() {
            self.selected = self.torrents.len() - 1;
        }
    }

    fn select_next(&mut self) {
        if !self.torrents.is_empty() {
            self.selected = self.selected.saturating_add(1).min(self.torrents.len() - 1);
        }
    }

    fn select_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn toggle_pause(&self) -> Action {
        let Some(torrent) = self.torrents.get(self.selected) else {
            return Action::None;
        };

        let command = if torrent.state == TorrentState::Paused {
            Command::Resume {
                id: torrent.id.clone(),
            }
        } else {
            Command::Pause {
                id: torrent.id.clone(),
            }
        };

        Action::Command(command)
    }

    fn apply_delta(&mut self, delta: TorrentDelta) {
        let Some(torrent) = self
            .torrents
            .iter_mut()
            .find(|torrent| torrent.id == delta.id)
        else {
            return;
        };

        if let Some(state) = delta.state {
            torrent.state = state;
        }
        if let Some(progress) = delta.progress {
            torrent.progress = progress;
        }
        if let Some(downloaded_bytes) = delta.downloaded_bytes {
            torrent.downloaded_bytes = downloaded_bytes;
        }
        if let Some(uploaded_bytes) = delta.uploaded_bytes {
            torrent.uploaded_bytes = uploaded_bytes;
        }
        if let Some(download_bps) = delta.download_bps {
            torrent.download_bps = download_bps;
        }
        if let Some(upload_bps) = delta.upload_bps {
            torrent.upload_bps = upload_bps;
        }
        if let Some(peers_connected) = delta.peers_connected {
            torrent.peers_connected = peers_connected;
        }

        match delta.eta {
            EtaUpdate::Unchanged => {}
            EtaUpdate::Clear => torrent.eta_secs = None,
            EtaUpdate::Set(secs) => torrent.eta_secs = Some(secs),
        }
    }
}

/// Input to the pure update function.
#[derive(Debug)]
pub enum Input {
    /// Keyboard input.
    Key(KeyEvent),
    /// Engine event.
    Engine(Event),
    /// Periodic tick.
    Tick,
}

/// Side effect requested by the update function.
#[derive(Debug, PartialEq)]
pub enum Action {
    /// Nothing to do.
    None,
    /// Quit the TUI.
    Quit,
    /// Send a command to the engine.
    Command(Command),
    /// Re-fetch the snapshot after a lagged event stream.
    Resync,
}

/// Applies one input to the state and returns the requested side effect.
pub fn update(state: &mut AppState, input: Input) -> Action {
    match input {
        Input::Key(key) => handle_key(state, key),
        Input::Engine(event) => handle_event(state, event),
        Input::Tick => Action::None,
    }
}

fn handle_key(state: &mut AppState, key: KeyEvent) -> Action {
    if key.kind != KeyEventKind::Press {
        return Action::None;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Action::Quit;
    }

    match key.code {
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Char('?') => {
            state.show_help = !state.show_help;
            Action::None
        }
        KeyCode::Esc => {
            state.show_help = false;
            Action::None
        }
        KeyCode::Up | KeyCode::Char('k') => {
            state.select_previous();
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.select_next();
            Action::None
        }
        KeyCode::Char('p') => state.toggle_pause(),
        _ => Action::None,
    }
}

fn handle_event(state: &mut AppState, event: Event) -> Action {
    match event {
        Event::TorrentAdded(summary) => {
            state.torrents.push(summary);
            state.clamp_selection();
            Action::None
        }
        Event::TorrentUpdated(delta) => {
            state.apply_delta(delta);
            Action::None
        }
        Event::TorrentRemoved { id } => {
            state.torrents.retain(|torrent| torrent.id != id);
            state.clamp_selection();
            Action::None
        }
        Event::Stats(stats) => {
            state.stats = stats;
            Action::None
        }
        Event::Error(error) => {
            state.status = Some(format!("{}: {}", error.code, error.message));
            Action::None
        }
        Event::Resync => Action::Resync,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bt_client::{TorrentId, TorrentSummary};
    use crossterm::event::KeyEvent;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn sample_summary(id: &str) -> TorrentSummary {
        TorrentSummary {
            id: TorrentId::new(id),
            name: "Example".to_owned(),
            state: TorrentState::Downloading,
            progress: 0.5,
            total_bytes: 1_000,
            downloaded_bytes: 500,
            uploaded_bytes: 0,
            download_bps: 100,
            upload_bps: 0,
            peers_connected: 2,
            eta_secs: Some(10),
        }
    }

    #[test]
    fn q_quits() {
        let mut state = AppState::default();
        let action = update(&mut state, Input::Key(key(KeyCode::Char('q'))));
        assert_eq!(action, Action::Quit);
    }

    #[test]
    fn help_toggles_and_escape_closes() {
        let mut state = AppState::default();

        let action = update(&mut state, Input::Key(key(KeyCode::Char('?'))));
        assert_eq!(action, Action::None);
        assert!(state.show_help);

        let action = update(&mut state, Input::Key(key(KeyCode::Esc)));
        assert_eq!(action, Action::None);
        assert!(!state.show_help);
    }

    #[test]
    fn pause_sends_command_for_selected_torrent() {
        let mut state = AppState::default();
        state.torrents.push(sample_summary("torrent-1"));

        let action = update(&mut state, Input::Key(key(KeyCode::Char('p'))));

        match action {
            Action::Command(Command::Pause { id }) => {
                assert_eq!(id, TorrentId::new("torrent-1"));
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn delta_updates_torrent() {
        let mut state = AppState::default();
        state.torrents.push(sample_summary("torrent-1"));

        let mut delta = TorrentDelta::new(TorrentId::new("torrent-1"));
        delta.state = Some(TorrentState::Paused);
        delta.progress = Some(0.75);
        delta.eta = EtaUpdate::Clear;

        let action = update(&mut state, Input::Engine(Event::TorrentUpdated(delta)));

        assert_eq!(action, Action::None);
        assert_eq!(state.torrents[0].state, TorrentState::Paused);
        assert_eq!(state.torrents[0].progress, 0.75);
        assert_eq!(state.torrents[0].eta_secs, None);
    }

    #[test]
    fn resync_requests_snapshot() {
        let mut state = AppState::default();
        let action = update(&mut state, Input::Engine(Event::Resync));
        assert_eq!(action, Action::Resync);
    }

    #[test]
    fn remove_clamps_selection() {
        let mut state = AppState::default();
        state.torrents.push(sample_summary("torrent-1"));
        state.torrents.push(sample_summary("torrent-2"));
        state.selected = 1;

        let action = update(
            &mut state,
            Input::Engine(Event::TorrentRemoved {
                id: TorrentId::new("torrent-2"),
            }),
        );

        assert_eq!(action, Action::None);
        assert_eq!(state.torrents.len(), 1);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn snapshot_preserves_selection_and_status() {
        let mut state = AppState::default();
        state.torrents.push(sample_summary("torrent-1"));
        state.torrents.push(sample_summary("torrent-2"));
        state.selected = 1;
        state.show_help = true;
        state.status = Some("kept".to_owned());

        let mut snapshot = Snapshot::empty(bt_client::API_VERSION);
        snapshot.torrents.push(sample_summary("torrent-3"));
        state.apply_snapshot(snapshot);

        assert_eq!(state.torrents.len(), 1);
        assert_eq!(state.selected, 0, "selection clamps to the new length");
        assert!(state.show_help);
        assert_eq!(state.status.as_deref(), Some("kept"));
    }
}
