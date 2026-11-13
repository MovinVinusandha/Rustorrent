---
name: engine-api-design
description: Use when adding or changing Command/Event messages, the EngineClient trait, or the daemon socket API.
---

## Principles
- crates/proto holds only serde types. No logic, no engine types leaked.
- Commands go in, Events come out. Clients never read engine internals.
- Every message type is versioned (`api_version` in handshake). Add fields as
  optional; never repurpose a field.
- IDs are opaque newtypes (TorrentId), not indexes.

## Events
- Send deltas (changed fields only), not full torrent state.
- Global stats at most 1 Hz; per-torrent progress batched to 1-4 Hz.
- A `snapshot()` call returns full state for new/reconnecting clients.
- Slow subscribers must not stall the engine: drop or coalesce old events.

## Remote API
- Local socket only (Unix socket / Windows named pipe via `interprocess`).
- Token or OS-permission auth. Never bind a network port by default.
- JSON-RPC style framing with length limits. Reject oversized messages.

## Errors
- Return typed, serializable errors with a stable `code` string.

## Required tests
- Round-trip serde tests for every message.
- Test that InProcess and Remote clients behave the same (shared test suite).