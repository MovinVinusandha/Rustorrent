# Roadmap

Status key: [ ] todo, [~] in progress, [x] done.

## v0.1: engine + API + TUI (first release)
BEPs: 3, 23, 15, 12, 9, 10, 5. See BEP_STATUS.md.

- [ ] 1. proto types and EngineClient trait (bootstrap)
- [x] 2. Bencode + metainfo parsing, fuzz targets
- [ ] 3. Storage: file mapping, piece verification, resume data.
  - [x] 3a. Path sanitization and file layout mapping (pure, no I/O).
    Sanitize torrent paths per operating system before creating any file.
    Test at least `..\..\x`, `C:\x`, and `C:x`. Detect case-insensitive and
    post-sanitization path collisions.
  - [x] 3b. Storage backend: safe file creation, positioned reads/writes via
    `crates/platform`, open-handle cap, preallocation policy.
  - [x] 3c. Piece verification, split into separately merged steps.
    - [x] 3c-1. Storage thread: one owned `Storage` on a dedicated OS thread,
      bounded request queue and a cloneable async handle (`Open`, `ReadRange`,
      `WriteRange`, `Sync`, `Shutdown`); typed errors when the worker is gone,
      and a drain-then-sync-then-exit shutdown.
    - [x] 3c-2a. Piece buffer: bounds-checked bitfield, memory budget with RAII
      reservations, piece specs and peer keys, block assembly with peer
      attribution, stall expiry, and block-insertion fuzzing. Pure in-memory:
      it does not use `StorageHandle` and can merge before 3c-1.
    - [x] 3c-2b. Piece verification and write path: SHA-1 on the blocking pool
      with a configurable job cap (default 2), and writing verified pieces
      through `StorageHandle::write_range` in chunks of at most 1 MiB.
      `store_verified(storage, &VerifiedPiece) -> Result<StoredPiece, WriteError>`
      borrows the piece, so a failed chunk is retried without re-downloading.
      `StoredPiece` has no public constructor, and
      `Bitfield::mark_stored(&StoredPiece)` is the only way to set a bit for a
      newly stored piece; `Bitfield::from_bytes` is the resume exception. It is
      synchronous, so no `&mut Bitfield` is held across an await.
      The bit stays false after a failed chunk and becomes true only after a
      complete write. The 1 MiB chunk copy is outside the piece budget
      (bounded, not peer-sized). The bitfield's generic setter is private;
      invalidation uses `clear_piece`. See ADR 0002 for the implementation
      notes.
    - [x] 3c-3. Streaming recheck: `Rechecker::run(selection, base, cancel,
      progress)` reads existing data through `StorageHandle::read_range` in
      256 KiB chunks, hashes 1 MiB batches on the shared `HashPool` with an
      incremental SHA-1 state, and reads the next batch while the current one
      hashes (at most two batches in memory). `RecheckSelection` is `All` or a
      strictly increasing `Only(&[u32])`; an empty selection returns the base.
      Selected bits are cleared before checking and set again only after a hash
      match, so damage, missing files, short reads, cancellation, and storage
      errors never publish a bit. Missing or short data is absent and abandoned
      on the first short read; other storage errors stop the run as typed
      errors. Cancellation is a shared `RecheckCancel` checked before each chunk
      and hash job; dropping the future releases the run lease. Progress is an
      advisory `try_send` snapshot per piece. One recheck runs per `Rechecker`
      (the session owns one per torrent), exclusive with same-torrent writes.
      See ADR 0002 for the implementation notes.
  - [x] 3d. Resume data: bencode-based, versioned, atomic write, validation
    against files, fallback to recheck. Persist verified bits only after
    `Storage::sync()` succeeds or a recheck confirms the data. When a file
    changed, 3d builds the recheck `Only` list from `FileMap` file piece ranges
    (`MappedFile` first and last piece), so only those pieces are re-hashed.
    The version 1 document is canonical bencode with a 16 MiB cap, a layout
    fingerprint instead of stored paths, and exact observed size/mtime per
    file. Save takes the bitfield by value, flushes storage, observes files,
    and writes through a per-torrent lease and the atomic file writer. Restore
    clears stale and unreadable pieces without reading them, checks a spread
    sample of up to five verified pieces (all of them for small torrents), and
    falls back to a full recheck on a failed sample or an invalid document.
    Missing, corrupt, and mismatched documents never block a torrent, and
    resume files are not deleted. A future optional mode may trust size/mtime
    only and skip the sample, trading detection for a faster start; save
    triggers belong to item 10. See ADR 0002.
  - [ ] Torrent removal: implement `Remove { delete_files }`. Delete only files
    recorded in the `FileMap`, never follow links, and never remove directories
    outside the root. Route deletion through the storage worker as a command so
    it runs on the storage thread and cannot race open handles. The API stub
    currently rejects `delete_files: true` with `NOT_IMPLEMENTED`.
- [ ] 4. Tracker announce (HTTP, then UDP)
- [ ] 5. Peer connection, handshake, message loop
- [ ] 6. Piece picker, request pipelining, endgame
- [ ] 7. Upload side: choking, rate limiter
- [ ] 8. Magnet + metadata fetch (BEP 9/10)
- [ ] 9. DHT (BEP 5)
- [ ] 10. Real in-process engine behind EngineClient + event stream
  - [ ] Session storage pool with a global file-handle budget. One worker for
    all torrents; the 64-handle cap is per `Storage` today, so N torrents can
    hold N × 64 descriptors while macOS defaults to a 256 soft limit.
    `StorageHandle` methods stay the same.
  - Must call `PieceAssembler::sweep_stalled` on a timer, and before accepting
    blocks when it wants strict expiry: expiry happens only on an explicit
    sweep.
  - Must own one `PieceAssembler` and one `TorrentBudget` handle per torrent,
    dropping both when the torrent is removed. One `HashPool` is shared by the
    whole session and hashes pieces from every torrent.
  - Must introduce a per-torrent state object that owns the `Bitfield`, the
    `TorrentBudget` handle, and the spec factory, and carries the info-hash.
    `mark_stored` checks that hash, so a `StoredPiece` from one torrent cannot
    set a bit in another torrent's bitfield.
  - Must model a `Checking` state that excludes `Downloading`. A recheck reads
    the torrent's files, so the session must keep the torrent out of the
    download scheduler while `Rechecker::run` is active and drop any queued
    piece stores for it. The session owns exactly one `Rechecker` per torrent;
    each one rejects a second concurrent run with `AlreadyRunning`, and
    resume-data validation (3d) uses the same exclusion.
- [ ] 11. TUI: list, add, details, limits
- [ ] 12. Cross-platform CI green, tag v0.1.0

Done when: real torrent and magnet link download and seed on Windows,
Linux and macOS, and survive restart.

## Workspace review triage

Recorded 2026-11-24 from the read-only whole-workspace review of `main`
(`ae62a18`, same tree as `206f2ed`).

Before item 4:
- [x] Inherit the workspace `[lints]` in every crate so `missing_docs` and
  `unsafe_code` are enforced by CI's `-D warnings`.
- [x] Reconcile `AGENTS.md`, `ARCHITECTURE.md`, ADR 0001, ADR 0002, and
  `BEP_STATUS.md` with the code.
- [ ] Harden resume saves: a unique temp file per save (pid + process-wide
  counter + nanosecond timestamp) created with `create_new`, atomic rename,
  and a sweep of stale `*.resume.tmp.*` older than 24 hours. No cross-process
  lock file; keep the in-process lease as an efficiency measure and document
  one daemon per resume directory.
- [ ] CI: build the fuzz crate and add weekly fuzz and `cargo deny` schedules.

Blocking prerequisites of item 10:
- [ ] Non-blocking TUI command dispatch: send commands from a task, feed
  replies back as an `Input` variant, and keep the event loop render-only.
- [ ] Own the storage worker: `JoinHandle` in `State`, join on shutdown, and a
  consuming `close(self)` that returns the final sync result.
- [ ] Session storage pool with a global file-handle budget.
- [ ] Engine integration test of storage -> verify -> store -> recheck ->
  resume through the public API.
- [ ] Client contract test suite parameterized over `EngineClient` impls.

Later:
- [ ] Narrow and de-duplicate the engine public surface to one export style.
- [ ] Make `EngineError` codes a checked type.
- [ ] Windows durability, file-identity, and path-cap tests.

## v0.2
BEP 11, 14, 6, 27, 19, 20. Queue, sequential mode, labels, settings file.
Daemon + remote TUI over local socket.
Metainfo: `.utf-8` keys and legacy text encodings (see docs/BEP_STATUS.md).

## v0.3
Encryption (MSE/PE), BEP 42, 32. IP blocklist, proxy, UPnP/NAT-PMP.

## v0.4
Tauri GUI (tray, window on demand, magnet:/.torrent deep links).

## v0.5
CI matrix builds, installers, code signing, auto-update.

## v0.6
uTP (BEP 29), BEP 52, watch folder, RSS, scheduler, torrent creation.

## v1.0+
Super-seeding (16), BEP 55, remote web access, polish. Stable API.