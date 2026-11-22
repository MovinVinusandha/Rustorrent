---
name: storage-io
description: Use when writing code that creates, reads, writes, or deletes files on the user's disk (storage, resume data, piece verification).
---

## Safety
- Every on-disk path is derived from sanitized components and verified to
  stay inside the download root after joining. Never join raw torrent strings.
- Never follow a symlink out of the root. Check parents before creating files.
- Deleting torrent data deletes only files this torrent created, never
  directories outside the root.

## Concurrency
- All blocking file I/O and hashing runs on a storage thread or spawn_blocking
  behind a bounded queue. Never on async tasks.
- Cap open file handles (LRU).

## Durability
- Resume files and settings are written to a temp file, flushed, then
  renamed. Never write them in place.
- Corrupt or missing resume data falls back to a recheck, never an error
  that blocks the torrent.

## Data integrity
- Only hash-verified pieces are written to the user's files.
- Handle disk-full, permission-denied, and file-in-use as typed errors.

## Tests
- Use temp directories only. Test odd paths, long names, many files, last
  piece, pieces spanning files, crash mid-write (truncated resume file).
- Run OS-specific cases under cfg and let CI cover all three platforms.