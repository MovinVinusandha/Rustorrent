---
name: tui-ratatui
description: Use when writing or changing the ratatui terminal UI. Event loop, state, drawing, and terminal safety.
---

## Rules
- The TUI uses only crates/client (`EngineClient`). Never import engine internals.
- Keep three parts separate: `AppState` (data), `update(state, input|event)`
  (logic), `draw(frame, &state)` (pure rendering, no side effects).
- One event loop with `tokio::select!` over: keyboard input (crossterm
  EventStream), engine events, and a ~250 ms tick.
- Install a panic hook and a drop guard that restore the terminal
  (disable raw mode, leave alternate screen) on every exit path.
- Handle resize events. Test on narrow terminals (80x24).
- Never block the draw loop; send commands to the engine asynchronously.

## Screens (v0.1)
List (name, progress, speed, ETA, peers), details (files, peers, trackers),
add dialog (magnet or path), limits dialog, help overlay.

## Keys
Arrows/jk move, Enter details, a add, p pause/resume, d remove, l limits,
? help, q quit. Keep keys listed in the help overlay.

## Tests
Unit-test `update()`. Use ratatui's `TestBackend` for draw snapshots.