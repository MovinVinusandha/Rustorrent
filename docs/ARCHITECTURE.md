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
  (`thread`). Removal of mapped files and owned directories runs through the
  backend and worker.
- `piece`: piece specs and blocks, memory budget, assembly with peer
  attribution, verification and `HashPool`, piece storage, and streaming
  recheck.
- `resume`: version 1 codec and layout fingerprint (`format`), bounded load
  (`load`), pure restore planning (`restore`), save and removal against a
  `StorageHandle` (`save`), and restore against a `Rechecker` (`restore_run`).
- `removal`: orchestrates one torrent removal, deleting resume data first and
  torrent data through the storage worker when requested.
- `tracker`: strict URL parsing and redaction (`url`), BEP 23/7 peer
  decoding (`peers`), response envelope (`response`), target policy
  (`policy`), bounded DNS (`dns`), BEP 15 UDP (`udp`), BEP 12 candidates
  (`candidates`), backoff and jitter (`backoff`), the per-host gate (`gate`),
  the pure announce state machine (`state`), and the tracker task (`task`).
- Planned: `peer`, `picker`, `choker`, `ratelimit`, `magnet`, `dht`,
  `session`, `settings`.

Threads and tasks: one storage OS thread per `StorageHandle` owns the
open-file cache and all positioned I/O; async code talks to it through a
bounded request queue. That is one worker per torrent today; roadmap item 10
replaces it with one session worker and a global handle budget. Piece hashing
runs on the blocking pool through a shared `HashPool`, never on a runtime
worker. A recheck reads 256 KiB chunks and hashes 1 MiB batches with an
incremental SHA-1 state, overlapping one read with one hash. Resume load,
save, and removal use `spawn_blocking` (bounded, non-cancellable jobs; see ADR
0002). Data removal is a worker pass of 256-item unlink and `remove_dir`
chunks, never a runtime-worker operation. The tracker task (`tracker::task`) is
one tokio task per torrent and awaits at most one transport attempt at a time;
the item-10 session will spawn it and own the shared `HostGate`. Tracker DNS
runs on the blocking pool behind an eight-permit semaphore. The per-torrent
session tasks and peer tasks arrive with items 5-10; the session will own one
`Rechecker`, one `PieceAssembler`, and one budget handle per torrent, plus one
shared `HashPool`.

## API (proto + client)
Command in / Event out. `InProcessClient` (channels) exists today; a Remote
client over a local socket is planned for v0.2. Versioned messages: a snapshot
whose `API_VERSION` differs is rejected. See skill engine-api-design.

## Data on disk
- Config directory: OS-specific path chosen by the daemon/platform later via
  `directories`; the engine does not hardcode paths.
- Download directory: the root of a torrent's `FileMap`; all mapped paths are
  sanitized relative components.
- Resume directory: passed explicitly to `ResumeStore`. The daemon will use
  `<data-local>/resume`; one file per torrent named
  `<info-hash-hex>.resume`. The document is canonical bencode, version 1,
  capped at 16 MiB, records no absolute paths, and is written atomically
  through a uniquely named temp in the same directory; saves sweep temps older
  than 24 hours. Run one daemon per resume directory: the temp names are unique
  but not a cross-process lock. It stores the verified bitfield and exact
  observed size/mtime per file; the format and restore rules are in ADR 0002
  section 7. Removal deletes that torrent's document and leftover temps; a
  missing document is success.
- Piece data lives only in the download directory. Missing or short files
  mean absent pieces, never a reason to delete or truncate user files.
  Removal deletes only mapped file paths and the torrent's own empty
  directories, deepest first; the download root and non-empty directories are
  always kept. A clean pass marks the storage removed, and a fresh instance is
  needed to download the torrent again.

## Security model
Untrusted: every byte from peers, trackers, DHT, .torrent files.
Local API (planned, v0.2): socket permissions + token; no network binding by
default.

## Decisions
See docs/adr/. Record every significant choice (library, protocol, format).

## Open questions
- rbit adoption is deferred to item 4 (ADR 0001); no other questions are
  recorded.