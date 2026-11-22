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
  - [ ] 3b. Storage backend: safe file creation, positioned reads/writes via
    `crates/platform`, open-handle cap, preallocation policy.
  - [ ] 3c. Piece verification: in-memory piece assembly, hash on the blocking
    pool, write only verified pieces, full recheck.
  - [ ] 3d. Resume data: bencode-based, versioned, atomic write, validation
    against files, fallback to recheck.
- [ ] 4. Tracker announce (HTTP, then UDP)
- [ ] 5. Peer connection, handshake, message loop
- [ ] 6. Piece picker, request pipelining, endgame
- [ ] 7. Upload side: choking, rate limiter
- [ ] 8. Magnet + metadata fetch (BEP 9/10)
- [ ] 9. DHT (BEP 5)
- [ ] 10. Real in-process engine behind EngineClient + event stream
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