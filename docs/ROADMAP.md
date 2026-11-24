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
  - [ ] 3c. Piece verification, split into separately merged steps.
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
      `Bitfield::mark_stored(&StoredPiece)` is the only way to set a verified
      bit; it is synchronous, so no `&mut Bitfield` is held across an await.
      The bit stays false after a failed chunk and becomes true only after a
      complete write. The 1 MiB chunk copy is outside the piece budget
      (bounded, not peer-sized). The bitfield's generic setter is private;
      invalidation uses `clear_piece`. See ADR 0002 for the implementation
      notes.
    - [ ] 3c-3. Streaming recheck: hash existing data in bounded chunks, rebuild
      the verified bitmap, treat missing or short data as absent.
  - [ ] 3d. Resume data: bencode-based, versioned, atomic write, validation
    against files, fallback to recheck.
  - [ ] Torrent removal: implement `Remove { delete_files }`. Delete only files
    recorded in the `FileMap`, never follow links, and never remove directories
    outside the root. Route deletion through the storage worker as a command so
    it runs on the storage thread and cannot race open handles. The API stub
    currently ignores `delete_files`.
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
    dropping both when the torrent is removed.
- [ ] 11. TUI: list, add, details, limits
- [ ] 12. Cross-platform CI green, tag v0.1.0

Done when: real torrent and magnet link download and seed on Windows,
Linux and macOS, and survive restart.

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