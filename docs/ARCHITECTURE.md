# Architecture

## Goals
Small, low-resource, cross-platform torrent client. Engine as a library;
all front-ends use one API.

## Crates and dependency direction
proto <- engine <- client <- { tui, daemon, gui }
platform is used by engine and daemon for OS glue.

## Engine (crates/engine)
Modules (fill in as built): metainfo, storage, tracker, peer, picker,
choker, ratelimit, dht, magnet, session, settings.
Threads/tasks: <describe runtime layout, storage thread, per-torrent tasks>.

## API (proto + client)
Command in / Event out. InProcess client (channels) and Remote client
(local socket JSON-RPC). Versioned messages. See skill engine-api-design.

## Data on disk
Config dir, resume data format, download dirs. (Use the `directories` crate.)

## Security model
Untrusted: every byte from peers, trackers, DHT, .torrent files.
Local API: socket permissions + token; no network binding by default.

## Decisions
See docs/adr/. Record every significant choice (library, protocol, format).

## Open questions
- (e.g. which rbit parts to adopt vs replace)