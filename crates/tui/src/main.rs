//! Terminal UI entry point.
//!
//! The TUI talks to the engine only through [`bt_client::EngineClient`]. The
//! event loop multiplexes keyboard input, engine events, and a redraw tick.

mod app;
mod terminal;
mod ui;

use std::time::Duration;

use anyhow::Context;
use bt_client::{EngineClient, InProcessClient};
use crossterm::event::{Event as CrosstermEvent, EventStream};
use tokio::time::MissedTickBehavior;
use tokio_stream::StreamExt;

use crate::app::{Action, AppState, Input, update};
use crate::terminal::TerminalGuard;

/// How often the UI redraws even when no input arrives.
const TICK: Duration = Duration::from_millis(250);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = InProcessClient::spawn().context("start in-process engine")?;
    let snapshot = client.snapshot().await.context("fetch initial snapshot")?;
    let mut state = AppState::from_snapshot(snapshot);

    let mut guard = TerminalGuard::init().context("initialize terminal")?;
    let mut engine_events = client.subscribe();
    let mut input_events = EventStream::new();
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        let step = tokio::select! {
            maybe = input_events.next() => match maybe {
                Some(Ok(CrosstermEvent::Key(key))) => Step::Input(Input::Key(key)),
                Some(Ok(_)) => Step::Redraw,
                Some(Err(error)) => Step::Error(format!("input error: {error}")),
                None => Step::Quit,
            },
            maybe = engine_events.next() => match maybe {
                Some(event) => Step::Input(Input::Engine(event)),
                None => Step::Quit,
            },
            _ = ticker.tick() => Step::Input(Input::Tick),
        };

        match step {
            Step::Input(input) => match update(&mut state, input) {
                Action::None => {}
                Action::Quit => break,
                Action::Command(command) => {
                    if let Err(error) = client.send(command).await {
                        state.status = Some(format!("{}: {}", error.code, error.message));
                    }
                }
                Action::Resync => match client.snapshot().await {
                    Ok(snapshot) => state.apply_snapshot(snapshot),
                    Err(error) => {
                        state.status = Some(format!("{}: {}", error.code, error.message));
                    }
                },
            },
            Step::Redraw => {}
            Step::Error(message) => state.status = Some(message),
            Step::Quit => break,
        }

        guard
            .terminal()
            .draw(|frame| ui::draw(frame, &state))
            .context("draw frame")?;
    }

    Ok(())
}

/// One iteration of the event loop.
enum Step {
    /// Apply an input to the application state.
    Input(Input),
    /// Redraw without changing state, for example after a resize.
    Redraw,
    /// Show an error in the status line.
    Error(String),
    /// Leave the event loop.
    Quit,
}
