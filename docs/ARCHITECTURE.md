# Architecture

## Goals
Small, low-resource, cross-platform torrent client. Engine as a library;
all front-ends use one API.

## Crates and dependency direction
proto <- engine <- client <- { tui, daemon, gui }
platform is used by engine and daemon for OS glue.

## Engine (crates/engine)
Modules:
- `bencode`: parser and constructors for canonical bencode, bounded and
  fuzzed.
- `metainfo`: v1 `.torrent` parsing, info-hash, file list, piece hashes.
- `bitfield`: fixed-size piece bits with exact trailing-bit checks.
- `storage`: pure path sanitization and `FileMap` (`layout`), the safe file
  backend (`backend`), and the dedicated storage worker and its async handle
  (`thread`).
- `piece`: piece specs and blocks, memory budget, assembly with peer
  attribution, verification and `HashPool`, piece storage, and streaming
  recheck.
- `resume`: version 1 codec and layout fingerprint (`format`), bounded load
  (`load`), pure restore planning (`restore`), save against a `StorageHandle`
  (`save`), and restore against a `Rechecker` (`restore_run`).
- Planned: `tracker`, `peer`, `picker`, `choker`, `ratelimit`, `magnet`,
  `dht`, `session`, `settings`.

Threads and tasks: one storage OS thread owns the open-file cache and all
positioned I/O; async code talks to it through a bounded request queue and
`StorageHandle`. Piece hashing runs on the blocking pool through a shared
`HashPool`, never on a runtime worker. A recheck reads 256 KiB chunks and
hashes 1 MiB batches with an incremental SHA-1 state, overlapping one read
with one hash. Resume load and save use `spawn_blocking`. The per-torrent
session tasks, tracker, and peer tasks arrive with items 4-10; the session
will own one `Rechecker`, one `PieceAssembler`, and one budget handle per
torrent, plus one shared `HashPool`.

## API (proto + client)
Command in / Event out. InProcess client (channels) and Remote client
(local socket JSON-RPC). Versioned messages. See skill engine-api-design.

## Data on disk
- Config directory: OS-specific path chosen by the daemon/platform later via
  `directories`; the engine does not hardcode paths.
- Download directory: the root of a torrent's `FileMap`; all mapped paths are
  sanitized relative components.
- Resume directory: passed explicitly to `ResumeStore`. The daemon will use
  `<data-local>/resume`; one file per torrent named
  `<info-hash-hex>.resume`. The document is canonical bencode, version 1,
  capped at 16 MiB, records no absolute paths, and is written atomically
  through a temp file. It stores the verified bitfield and exact observed
  size/mtime per file; the format and restore rules are in ADR 0002 section 7.
- Piece data lives only in the download directory. Missing or short files
  mean absent pieces, never a reason to delete or truncate user files.

## Security model
Untrusted: every byte from peers, trackers, DHT, .torrent files.
Local API: socket permissions + token; no network binding by default.

## Decisions
See docs/adr/. Record every significant choice (library, protocol, format).

## Open questions
- (e.g. which rbit parts to adopt vs replace)