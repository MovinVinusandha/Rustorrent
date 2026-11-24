# ADR 0002: Storage design

- Status: Accepted
- Date: 2026-11-23
- Deciders: project owner
- Supersedes: none
- Related: `docs/ROADMAP.md` item 3, `.opencode/skills/storage-io/SKILL.md`,
  `docs/beps/bep_0003.md`

## Context

Metainfo parsing (roadmap item 2) is done: the engine can turn a `.torrent`
into a `MetaInfo` with a verified info-hash, validated-at-parse `name` and
`path` components, and an exact piece count. Storage is the next step and is
split into four deliverables:

- 3a: path sanitization and file layout mapping (pure, no I/O).
- 3b: storage backend (safe file creation, positioned I/O via `crates/platform`,
  open-handle cap, preallocation policy).
- 3c: piece verification (in-memory piece assembly, hashing on the blocking
  pool, write only verified pieces, full recheck).
- 3d: resume data (bencode, versioned, atomic write, validation, fallback to
  recheck).

This ADR records the decisions for all four so their interfaces line up. The
design is approved; implementation proceeds in roadmap order (3a, then 3b–3d).

Constraints from `AGENTS.md` and the `storage-io` skill:

- every on-disk path derives from sanitized components and stays inside the
  download root;
- no blocking I/O or hashing on an async worker;
- typed errors, no panics on untrusted input, no `unwrap`/`expect`;
- no `unsafe` without a comment and a test;
- cross-platform (Linux, Windows, macOS), small resource footprint.

## Decision

### 1. Sanitize on every platform

Apply one sanitization algorithm on all platforms, not only on Windows.

Rationale:

- the layout is deterministic and identical on every host, so tests can cover
  Windows cases on Linux;
- a download directory moved between operating systems keeps working;
- resume data does not depend on the host that wrote it;
- there is one code path to audit, not two.

Sanitization happens in two layers:

1. **Metainfo parsing (already done)** rejects empty components, `.`, `..`,
   `/`, and ASCII control characters, and rejects invalid UTF-8.
2. **Layout mapping (3a)** re-validates defensively and repairs everything else.
   `MetaInfo` fields are public, so the mapper must not assume the parser ran.

Repair rules, applied per component, in order:

| Input | Result |
|---|---|
| empty | `_` |
| `.` or `..` | `_` |
| `/` or `\` | `_` |
| ASCII control character, NUL, DEL | `_` |
| `<` `>` `:` `"` `|` `?` `*` | `_` |
| Windows reserved device name (`CON`, `PRN`, `AUX`, `NUL`, `CONIN$`, `CONOUT$`, `CLOCK$`, `COM1`-`COM9`, `LPT1`-`LPT9`, `COM¹`-`COM³`, `LPT¹`-`LPT³`, case-insensitive, with or without extension) | prefix `_` |
| trailing dots or spaces | replaced with `_` |
| component longer than 255 bytes | truncate at a UTF-8 boundary to 255 bytes minus suffix, append `~` plus 8 hex chars of SHA-1 of the original component |

- Non-ASCII text is preserved as-is. No Unicode normalization is applied to
  the on-disk name or to collision keys in v0.1; the NFC/NFD gap is documented
  in section 2.
- Sanitization is total, deterministic, and idempotent
  (`sanitize(sanitize(x)) == sanitize(x)`).
- The complete relative path (torrent `name` plus `path` components) is capped
  at 1024 bytes, because the platform `PATH_MAX` includes the download root and
  3a does not know the root. Exceeding the cap after per-component truncation
  is a typed `LayoutError::PathTooLong`. Rust's standard library handles
  Windows long paths internally, so no `\\?\` prefix appears in our code.

### 2. Collision handling and mapping

`FileMap` is built once from `MetaInfo`, is immutable, and is shared as an
`Arc` by the backend, verification, and resume code.

`MappedFile` stores:

- the original components from metainfo (diagnostics and resume validation);
- the sanitized relative components as `Vec<String>`, including the torrent
  `name`, with a `to_path()` helper that joins them; no `PathBuf` is stored in
  the layout;
- the file length;
- the byte offset of the file in the torrent stream;
- the first and last piece index touched by the file.

`FileMap` exposes `segments(Range<u64>)`, yielding `(file_index,
offset_in_file, len)` items, and `files_for_piece(piece)` for the files a
piece touches. This is how 3b turns a piece range into positioned reads and
writes without re-deriving offsets.

`FileMap::build` validates that `piece_length` is non-zero, that `pieces.len()`
matches `ceil(total_length / piece_length)`, and that the total length does not
overflow. Violations are typed `LayoutError`s (`ZeroPieceLength`,
`PieceCountMismatch`, `TotalLengthOverflow`), never panics.

Collision key: the joined sanitized relative path, lowercased with
`str::to_lowercase`. This catches three kinds of collision in one check:

- exact duplicates (already rejected by metainfo, checked again defensively);
- case-insensitive duplicates (`A.txt` vs `a.txt`);
- post-sanitization duplicates (`a:b` and `a?b` both become `a_b`).

Case folding via `str::to_lowercase` is accepted as sufficient for v0.1; full
Unicode case folding is not implemented.

Unicode normalization is not applied in v0.1. `unicode-normalization` measured
+121.4 KiB in the release binary, above the 50 KiB budget, so NFC and NFD
spellings are not unified. On macOS an NFC/NFD pair can therefore alias the
same on-disk file; 3b detects that after open (device + inode) and returns a
typed error instead of writing both files to one inode.

On collision, derive the next candidate from the last component. If the
component contains a dot at an index other than 0 and the text after the last
dot is at most 10 bytes, insert `~N` before that dot; otherwise append `~N`.
Re-apply the 255-byte component truncation afterwards, then re-check the key.
`~1`, `~2`, ... are tried in order. Because the suffix goes before the
extension, a colliding `a~1.txt` becomes `a~1~1.txt`, not `a~1.txt`, so it
cannot steal the name of a real `a~1.txt` that appears later in the torrent.
Resolution walks files in torrent order, so the result is deterministic.
A path that is a prefix of another file's directory path (`a` as a file and
`a/b` as a directory) is also a conflict; it is detected and disambiguated the
same way.

Disambiguation is chosen over rejection because real torrents contain
`A.txt`/`a.txt` and `a:b`/`a?b`; rejecting makes them unusable, while a
deterministic rename keeps both files.

The original-to-disk mapping lives only in `crates/engine/src/storage/layout.rs`.
No other module re-derives paths from `MetaInfo`.

### 3. Root and symlink safety

- The download root itself may be a symlink. The session accepts it as given
  and opens it once; only torrent-derived components below it are checked.
- All storage paths are relative to that root; absolute paths are never built
  from torrent data.
- `canonicalize()` is not used: it races and fails for files that do not exist
  yet.
- 3b re-checks every ancestor component with `symlink_metadata` on every open:
  reads, writes, lazy creation, and the reopen that `sync` performs. Symlinks
  and Windows reparse points are rejected, so a parent swapped after `open`
  cannot redirect the join. On Unix the opened handle's `(dev, ino)` from
  `fstat` must match the metadata that was just checked, which narrows the
  check-then-open window. Nothing compares inode identity on Windows.
- v0.1 deliberately does not use `openat`, `windows-sys`, or `unsafe` for
  handle-relative traversal. `symlink_metadata` plus the Unix `fstat` check is
  still check-then-use, so an attacker who can write to the download tree could
  swap a component between the check and the open (TOCTOU). This is out of
  scope for v0.1: the download root is user-controlled, the check fails closed
  in every non-racing case, and the backend repeats it before every open.
  Handle-relative traversal is a later hardening item, not a v0.1 blocker.
- A component that is a symlink/reparse point, or the wrong kind (file where a
  directory is expected, or vice versa), is a typed `Error::UnsafePath`.
- Before creation, the joined relative path is checked: not absolute, no `..`,
  no empty components, no drive or UNC prefix.
- Deletion only removes files recorded in `FileMap`, never directories outside
  the root, and never follows links.
- A test creates a symlink inside the root pointing outside it and asserts a
  typed error with no file created outside.

### 4. Positioned I/O and the storage thread

`crates/platform` exposes a small file wrapper with:

```text
open_read(path) / open_write(path) / create_new(path)
read_at(buf, offset) -> usize
write_at(buf, offset) -> usize
len() / set_len(len)
sync_data() / sync_all()
```

The wrapper exposes `read_at`/`write_at`, which may transfer fewer bytes than
requested. 3b builds `read_exact_at`/`write_all_at` loops on top that retry at
the advanced offset and treat a zero-byte read before the buffer is full as an
unexpected EOF.

Unix uses `std::os::unix::fs::FileExt::{read_at, write_at}`, Windows uses
`std::os::windows::fs::FileExt::{seek_read, seek_write}`. Both are safe APIs,
so no `unsafe` is needed. On Windows `seek_read`/`seek_write` also move the file
cursor, so handles are never shared across threads: the single storage thread
owns every handle. The wrapper is the only place that knows about platform file
APIs; the engine sees one type.

Thread model:

- one dedicated storage thread per session, a plain `std::thread`, shared by
  all torrents in that session. This serializes disk I/O across torrents; it is
  a known v0.1 limitation, and per-torrent threads can be added later if
  measurements justify it;
- the thread owns the handle cache and all file handles. Each `FileMap` is
  shared as an `Arc`;
- requests arrive on a bounded `tokio::sync::mpsc::channel(64)`; the async side
  uses `send().await` for backpressure, the storage thread uses
  `blocking_recv()` and is never a runtime worker;
- each request carries a `tokio::sync::oneshot` reply channel;
- commands: `Open`, `WritePiece`, `ReadPiece`, `Recheck`, `Sync`, `Shutdown`;
- piece hashing does not run on the storage thread. 3c hashes on
  `spawn_blocking` and sends only verified bytes to storage.

A bounded queue plus a byte budget (below) means a slow disk slows the torrent
down instead of growing memory.

### 5. Handles, preallocation, fsync, disk full

- **Open handles:** LRU cache of at most 64 handles per session, configurable.
  The cache tracks a dirty set: files written since the last sync. Eviction
  never fsyncs; the index is recorded and `sync` reopens it read-write later.
  Reads open files read-only, and a cached handle is upgraded only when a write
  or a pending sync needs one. Pieces are processed one segment at a time, so
  the cache never holds more than the cap; a transient handle for a sync reopen
  or a lazy create puts peak usage at `capacity + 1`.
- **Preallocation:** optional policy, off by default. v0.1 does not rely on
  sparse files: on Windows, NTFS zero-fills a write far beyond EOF, so `set_len`
  does not guarantee sparse allocation. When enabled, 3b may `set_len` on Unix
  or use the platform equivalent, but correctness never depends on it. Missing
  or short files mean those pieces are absent and are rechecked. Fragmentation
  is acceptable.
- **fsync:** no per-piece fsync. `sync_data` runs on pause, stop, and graceful
  shutdown, and dropping a `Storage` performs no sync. 3d must call
  `Storage::sync()` before every resume write. `sync` attempts every pending
  file, clears only the ones that succeeded so a failure can be retried, and
  returns the first error; `close` drops the cache only after a successful
  sync. Every resume write therefore starts from a complete flush, reopening
  any file that was evicted. Resume files use temp file + `sync_all` + rename +
  parent-directory sync.
- **Disk full:** `ENOSPC` / `ERROR_DISK_FULL` / `ERROR_HANDLE_DISK_FULL` map to
  `Error::DiskFull`. The torrent stops writing, keeps the verified bitmap for
  pieces already written, emits an event, and leaves resume intact. Permission
  denied and file-in-use get their own typed errors. Recovery never deletes or
  truncates user files.
- A missing or short file means the pieces it covers are absent; recheck clears
  them. This is not a fatal error.
- **Zero-length files:** created eagerly by the backend, created if missing,
  and an existing file is never truncated.

### 6. Piece buffering budget

Piece assembly belongs to 3c; storage never allocates piece-sized buffers.

- Default global budget: 128 MiB, configurable.
- Per-torrent share: `min(2 * piece_length, 64 MiB)`; at least one piece must
  fit, and metainfo caps `piece_length` at 64 MiB.
- Enforcement: a byte-counting semaphore. A piece is requested only after its
  actual piece length is reserved. In-flight pieces per torrent:
  `max(1, share / piece_length)`.
- When the budget is exhausted, new piece requests wait; blocks already in
  flight are never dropped.
- 3c releases the reservation of a stalled partial piece after a timeout, so a
  peer that stops sending cannot pin the budget forever. The partial data is
  discarded and the piece is requested again.
- Completed pieces are hashed on the blocking pool before being handed to
  storage, so unverified data never consumes disk write bandwidth.

### 7. Resume data

- Format: canonical bencode via `engine::bencode`, version 1, encoded and
  decoded by `engine::resume`. Canonical means sorted dictionary keys with no
  duplicates, so equal documents have equal bytes.
- Location: the engine takes an explicit resume directory. The daemon will
  pass `directories::BaseDirs::data_local_dir()/resume` when item 10 wires the
  session; the engine never chooses an OS path and never stores an absolute
  path in the file. A resume file is never inside the download directory.
- File names: `<info-hash-hex>.resume` for the document and
  `<info-hash-hex>.resume.tmp` for the one temporary file, both in the resume
  directory.
- Version 1 fields:

```text
d
  8:bitfield <ceil(piece_count/8) bytes>
  5:files l d 6:length i<n>e
               8:mtime_ns i<n>e
               13:observed_size i<n>e e ... e
  9:info_hash 20:<bytes>
  11:layout_hash 20:<bytes>
  11:piece_count i<n>e
  12:piece_length i<n>e
  12:total_length i<n>e
  10:updated_at i<unix seconds>e
  7:version i1e
e
```

- File entries are in `FileMap` order and carry no path components. Path
  components are compared indirectly through `layout_hash`, a SHA-1 over the
  file count, every sanitized disk component (length-prefixed), and every
  nominal file length, all in map order. The download root is not hashed: the
  same torrent in another directory still matches. A rename, a split component,
  an added or removed file, or a changed nominal length invalidates the whole
  document and triggers a full recheck.
- Integers are limited to the bencode `i64` range. A file entry stores the
  nominal `length`, the exact `observed_size`, and `mtime_ns` in nanoseconds
  since the Unix epoch. `MTIME_UNAVAILABLE` (the smallest `i64`) means the
  platform could not report a time; such an entry never matches an observed
  time and its pieces are rechecked. The exact size and mtime comparison has no
  tolerance and no "newer than" guard.
- Size limit: one document is at most `MAX_RESUME_BYTES` (16 MiB). The limit is
  checked from the file metadata before any read, and the read is capped in
  case the file grows, so a hostile file cannot force an unbounded allocation.
- Validation before use:
  - known version;
  - `info_hash`, `piece_length`, `total_length`, `piece_count`, and
    `layout_hash` match the loaded metainfo and current `FileMap`;
  - the file entry count matches the map, and the bitfield length is exact
    with zero trailing bits;
  - structural mismatch means the document is ignored and every readable
    piece is rechecked. Resume files are never deleted, so a bad document does
    not destroy evidence and a later build can read it.
- Per-file changes: a file whose observed size or mtime differs, or whose entry
  is `MTIME_UNAVAILABLE`, clears the pieces it covers. Those pieces are
  rechecked with `RecheckSelection::Only`; verified pieces in unchanged files
  keep their bits. Missing or short files are marked absent without a read,
  including when no usable document exists, and only the remaining readable
  pieces are put in the `Only` list.
- Spot check: after per-file validation, up to `SPOT_CHECK_PIECES` (5)
  verified pieces are sampled evenly across the verified range, always
  including the first and last. A torrent with five or fewer verified pieces
  checks all of them. The sampled pieces are cleared and added to the `Only`
  list, so an unchanged torrent reads only the sample. If any sample fails,
  every readable piece is rechecked with `RecheckSelection::All`; a failure
  proves metadata no longer describes the bytes, so the remaining saved bits
  cannot be trusted.
- External-modification window: the size/mtime check catches ordinary edits,
  truncation, and replacement, but a change that preserves both size and mtime
  is invisible. The five-piece sample makes silent corruption unlikely to
  survive, but it does not prove unsampled pieces. A future, optional lighter
  mode could trust size/mtime only and skip the sample, trading that detection
  for a faster start on very large torrents; it is not in v0.1.
- Crash-recheck cost: a document describes the last successful save, so an
  unclean shutdown loses up to one save interval of verified bits. Pieces
  downloaded after that save are cleared and downloaded again. A file whose
  size grew during the interval is stale and is rechecked, which may recover
  some of them, but correctness never depends on it. The five-piece sample
  bounds the normal start cost; a failed sample or structural mismatch costs
  one full recheck of the readable pieces.
- Save ordering: take the bitfield snapshot by value, call
  `Storage::sync()`, observe every mapped file, encode, and write atomically.
  Syncing before observing guarantees every recorded bit was written before
  the flush began. The snapshot is consumed so a later bitfield change cannot
  leak into an earlier document. A per-torrent save lease rejects a second
  concurrent save with `AlreadySaving`. The blocking write task owns the
  lease, so dropping the save future cannot admit a second writer; the lease
  is released when the write finishes or fails.
- Atomic write: create the resume directory in the blocking task, write the
  temporary file in the same directory, `sync_all`, rename over the target,
  then sync the parent directory on Unix. On Windows, the rename replaces an
  existing target and retries sharing or lock violations. A failed save
  leaves the previous document untouched.
- Blocking I/O: the load, directory creation, and write all run on
  `spawn_blocking`; no file operation runs on a runtime worker. A missing file
  is `Ok(None)`, and a corrupt or oversized file is a typed decode error; both
  fall back in `restore` and never block the torrent. The caller receives a
  typed `RestoreSource` describing which path was taken, for the session to
  log once logging exists.
- Save triggers are not wired yet. Item 10 owns them: every 30 seconds while
  active, and on pause, stop, and graceful shutdown. Saves are not per piece.

### 8. Dependencies

- `unicode-normalization` is not added. Measured on a stripped
  `opt-level = "z"`, LTO, `panic = "abort"` binary: +124,352 bytes (121.4 KiB),
  from 293,976 bytes baseline to 418,328 bytes with NFC. That is above the
  50 KiB budget, so v0.1 uses `str::to_lowercase` only and documents the
  NFC/NFD gap.
- `proptest = "1"` is approved as a dev-dependency only; it never enters the
  release binary. Property tests use 64 cases per property to keep CI quick.
- No other dependency is added for 3a; the layout uses `std` only.

## Threat model

In scope for v0.1:

- a malicious `.torrent`: path traversal, absolute paths, drive and UNC syntax,
  Windows device and reserved names, case and post-sanitization collisions,
  oversized components;
- a malicious peer or tracker: arbitrary piece payloads, malformed messages,
  oversized, short, or zero-length ranges;
- corrupt or truncated resume data: falls back to a recheck and never blocks the
  torrent;
- an existing on-disk tree: a symlink or reparse point anywhere below the
  download root is rejected before any read or write through it, including a
  component swapped after `open`.

Out of scope for v0.1:

- an attacker who can already write to the download directory. Such an attacker
  can replace a component between the final check and the open (TOCTOU), or
  plant a hard link in advance. Closing that window needs handle-relative
  traversal (`openat`/`openat2` with `RESOLVE_BENEATH` on Linux, reparse-point
  handles on Windows), which v0.1 does not use.
- a compromised local account or a hostile kernel.

## Known limitations

- **Hard links are accepted.** The backend does not reject `nlink > 1`:
  hard-linked media libraries are a common, legitimate setup. The device +
  inode check only rejects two mapped paths that resolve to the same inode, so
  a hard link inside the root that points at a file outside it is read and
  written in place. On Linux `fs.protected_hardlinks=1` (the default on most
  distributions) stops an unprivileged process from linking to a file it does
  not own. Windows has no stable file-index accessor in `std`, so duplicate
  hard links there are caught only by the case-folded layout keys.
- **Peak handle usage is `capacity + 1`.** The cache itself never exceeds
  `max_open_handles`, but a sync reopen and a lazy create each hold one
  transient handle outside it.
- **No implicit sync.** Dropping a `Storage` closes its handles without
  flushing. Callers must call `sync` or `close`; 3d calls `sync` before every
  resume write.
- **`set_len` is not shrink-proof against a race.** Preallocation re-reads the
  length from the opened handle and only extends a short file, but another
  process can grow the file between that `fstat` and `set_len`.
- **Unicode normalization is not applied.** An NFC/NFD pair can alias on macOS;
  on Unix the inode check reports it as a typed error, and case folding is
  `str::to_lowercase`.

## Consequences

Positive:

- one deterministic layout on every platform;
- path traversal and symlink escapes fail closed with typed errors;
- memory and handle use are bounded;
- resume corruption degrades to a recheck, never to a stuck torrent;
- only hash-verified bytes reach the user's files.

Negative:

- some files are renamed (`a:b` becomes `a_b`, collisions get `~N`);
- no per-piece durability, so a crash loses recent pieces and forces a recheck;
- preallocation is off by default; files may fragment, and a Windows far write
  is not sparse;
- case folding via `str::to_lowercase` is an approximation of full Unicode case
  folding;
- NFC and NFD spellings are not unified. On macOS an alias is caught by the
  3b device + inode check and reported as a typed error;
- the `symlink_metadata` check leaves a TOCTOU window for a local attacker who
  can already write to the download tree;
- one storage thread serializes I/O across all torrents in a session.

## Alternatives considered

- **Windows-only sanitization.** Rejected: layout and resume become
  host-dependent, and Windows cases cannot be tested on Linux CI.
- **Reject colliding paths.** Rejected: common torrents would fail to load.
- **One async task per file using `tokio::fs`.** Rejected: unbounded handles
  and blocking work on the runtime.
- **`canonicalize()` for containment checks.** Rejected: races with symlink
  changes and fails for not-yet-created files.
- **`openat`/`O_NOFOLLOW` and Windows reparse-point handles.** Rejected for
  v0.1: platform-specific code, a larger dependency surface, and `unsafe` in
  the Windows path. `symlink_metadata` checks are simpler and fail closed in
  all non-racing cases; the TOCTOU window is documented above.
- **Write pieces before verification.** Rejected: corrupt data on disk and no
  clean recheck semantics.
- **Per-piece fsync.** Rejected: destroys throughput; resume plus recheck is the
  chosen durability model.
- **NFC normalization via `unicode-normalization`.** Rejected for v0.1: it adds
  121.4 KiB to the release binary, above the 50 KiB budget. The macOS alias is
  caught by the 3b inode check instead.

## Open questions

- LRU handle cap 64: measure real usage with many-file torrents before fixing.
- Revisit handle-relative traversal (`openat` on Unix, reparse-point handles on
  Windows) only if the threat model changes; see "Threat model" above.

## Implementation notes

Roadmap 3b was implemented after this ADR was accepted. These notes record
where the code differs from the design above.

- **Storage thread deferred to 3c.** 3b is a synchronous, single-owner
  `Storage`: callers pass a byte range and get the result on the same thread.
  The handle cache and every file handle live inside `Storage`, and a
  compile-time assertion keeps `Storage: Send` so 3c can move it to the
  dedicated storage thread without changing the type. The bounded queue and
  the `Open`/`WritePiece`/`ReadPiece`/`Sync`/`Shutdown` messages are not built
  yet.
- **Windows duplicate-inode check skipped.** `std` exposes no stable
  volume/file-index pair on Windows. The backend relies on the case-folded
  `FileMap` keys and on `create_new` returning `AlreadyExists`; the Unix
  `(dev, ino)` check is implemented. The macOS NFC/NFD alias is therefore
  caught on Unix but not on Windows.
- **The handle cap is strict.** The design allowed a piece write spanning
  several files to overshoot the cap by that many handles. 3b instead opens,
  uses, and releases one segment at a time, so the cache never holds more than
  `max_open_handles`. Transient handles are the exception: a sync reopen and a
  lazy create each hold one outside the cache, so peak usage is `capacity + 1`.
- **Eviction defers fsync.** Evicting a dirty handle never calls `sync_data`.
  The file index is recorded in a `needs_sync` set; `sync()` and `close()`
  flush cached dirty handles and reopen evicted ones read-write with `open_rw`
  before `sync_data`. Reopening a file the user deleted is a typed
  `StorageError`, not a silent recreation. `sync` attempts every pending file,
  removes only successes, and returns the first error; `close` drops the cache
  only after a successful sync, so a failed flush can be retried.
- **Ancestors are re-checked on every open.** `read_range`, the lazy create in
  `write_range`, and the `sync` reopen walk every parent component with
  `symlink_metadata` before opening, so a component swapped after `open` is
  rejected. On Unix the opened handle's `(dev, ino)` is compared with the
  metadata that was just checked; a mismatch is
  `UnsafePath { reason: Changed }`.
- **Reads are read-only.** `read_range` opens with `open_read`; a cached handle
  is reopened read-write only when a write or a pending sync needs one, so a
  read works on a file or mount without write permission.
- **Preallocation re-reads the length.** `extend_if_short` takes the length
  from the opened handle instead of the `lstat` that triggered it, so a stale
  length cannot shrink a file.
- **Preallocation `Off` is lazy.** Zero-length files and their parents are
  created during `open`; non-zero files and their parents are created on the
  first write. `SetLen` creates every file during `open` and extends only
  short files, never truncating.
- **Range semantics.** `read_range` returns only the leading contiguous bytes
  and stops at the first missing or short file; it never skips a gap and never
  reads a file past its mapped length. `write_range` rejects ranges that
  overflow `u64` or exceed the total length, and a failed write may leave a
  partial range on disk, so verified state is set only after a complete
  success.
- **3c-1 worker scope is temporary.** There is one `Storage` and one
  `bt-storage` thread per torrent for now, so the 64-handle cap is per torrent.
  Ten torrents can hold 640 descriptors while macOS defaults to a soft limit of
  256. Item 10 adds a session pool with a global handle budget; the
  `StorageHandle` methods stay the same.
- **Commands are range-based.** The worker accepts `ReadRange` (at most 1 MiB,
  replying with a `Vec<u8>`), `WriteRange` (at most 1 MiB), `Sync`, and
  `Shutdown`. `Open` is not a command: root, map, and config move into the
  worker closure and `Storage::open` runs there. There is no `Recheck`
  command; 3c-3 streams `ReadRange` chunks and hashes above the worker, so
  hashing never runs on the storage thread. Piece writes are split into 1 MiB
  chunks, and a verified bit is set only after every chunk succeeds.
- **The queue is bounded by count and payload.** Capacity is 64 requests and
  every range is capped at 1 MiB, so queued write payload is at most 64 MiB.
  The bound does not cover callers blocked in `reserve`, which each hold their
  request buffer, nor read replies, which add up to 1 MiB each; 3c-2 must bound
  its own concurrent callers. A full queue leaves `send().await` pending; no
  runtime worker blocks. A cancelled caller's enqueued write may still execute,
  and sequential requests from one handle keep FIFO order, so retrying the same
  verified bytes at the same offset is safe.
- **Shutdown reserves before it commits.** `shutdown()` reserves a queue slot,
  sets an atomic flag with `compare_exchange`, then sends `Shutdown` through
  the permit. Dropping the future while it waits for a slot leaves the handle
  usable; dropping it after the flag is set still shuts the worker down, and a
  retry gets `ShuttingDown`. After the flag is set, new requests fail with
  `ShuttingDown`.
- **Requests behind `Shutdown` never execute.** The worker breaks out of the
  receive loop, calls `Receiver::close()` (tokio still lets an outstanding
  `Permit` append a message), then drains `try_recv()` and answers each
  leftover with `ShuttingDown`. A request appended after the drain is dropped
  with the queue and its caller gets `WorkerStopped`.
- **Shutdown drops the storage before replying.** The worker drains queued
  requests, calls `Storage::close()`, drops the `Storage` so every file handle
  is closed, and only then replies. A failed final sync is returned and the
  worker still exits; a caller that needs a retry calls `sync()` first.
  Dropping the last handle is a best-effort close with no error reporting;
  `shutdown()` is the reportable path.
- **Worker panic policy.** In unwind builds a panic drops the receiver, and
  pending and future requests get `WorkerStopped`. The panic unwinds past
  `Storage::close()`, so dirty data is not flushed; release builds use
  `panic = "abort"` and abort the process instead. There is no `catch_unwind`;
  this is accepted for v0.1.
- **No request timeouts.** A request waits for its reply indefinitely, and
  `shutdown()` waits behind other requests like any caller. A syscall that
  never returns (a hung mount, a removed USB disk) blocks the storage thread
  and with it every request for that torrent. The item-10 session pool is the
  mitigation for the session; per-request timeouts are not part of 3c-1.
- **Thread and features.** The thread is named `bt-storage` and uses a 512 KiB
  stack. `bt-engine` enables only the tokio `sync` and `rt` features; `macros`
  and `time` are dev-dependencies. The stripped release `bt-tui` binary stayed
  at 770,856 bytes.
- **Piece hashing runs above the storage thread.** 3c-2b adds `HashPool`,
  which computes SHA-1 on `spawn_blocking` with a configurable cap (default 2,
  `HashPool::with_max_jobs`). The semaphore permit and the whole `PieceBuffer`
  move into the blocking closure, so cancelling the outer future cannot leak
  the piece reservation or a job slot; the blocking job drops both when it
  finishes. An incomplete buffer is rejected before a permit is taken. A hash
  mismatch returns the sorted, deduplicated `PeerKey` contributors for later
  statistical blame, drops the bytes, and makes the engine re-request the full
  piece; no peer is banned at this layer.
- **`VerifiedPiece` and `StoredPiece` have no public constructors.** Only a
  successful hash comparison produces a `VerifiedPiece`, and only a complete
  chunk run produces a `StoredPiece`. The verified piece keeps the piece
  reservation until the caller drops it, including while it is being written.
- **Verified writes borrow the piece.** `store_verified(&StorageHandle,
  &VerifiedPiece)` writes 1 MiB chunks (`MAX_RANGE_LEN`) and returns a typed
  `WriteError` that names the failed chunk index, absolute offset, and length.
  A failure leaves the piece unverified but keeps the bytes with the caller, so
  a disk-full error can be retried after space is freed without re-downloading.
  Chunk writes are positioned, so storing the same piece twice is harmless. An
  offset beyond the torrent surfaces as the storage layer's
  `StorageError::OutOfRange` wrapped in the chunk error, never as a panic.
  There is still no per-piece fsync.
- **A cancelled store can leave a prefix on disk.** Dropping `store_verified`
  midway leaves the chunks already written, and a chunk already enqueued on the
  worker may still execute (3c-1 contract). Because the caller still owns the
  piece and the writes are idempotent, a fresh call is safe and rewrites the
  whole piece.
- **The 1 MiB chunk copy and the attribution array are outside the budget.**
  The piece data remains covered by its `PieceReservation`; each chunk adds a
  transient copy of at most 1 MiB, and the per-block peer array costs
  `size_of::<Option<PeerKey>>()` per 16 KiB block, at most 64 KiB for a 64 MiB
  piece. Both are bounded and never sized by peer input, so they are not part
  of the 128 MiB piece budget (the M1 finding).
- **Verified bits have one setter for new pieces.** `Bitfield::set` is private
  to the bitfield module. `mark_stored(&StoredPiece)` is the only way to set a
  bit for a newly stored piece and is synchronous, so no `&mut Bitfield` is
  held across an await; `from_bytes` is the resume exception, and `clear_piece`
  / `clear` only invalidate. A test drives the pipeline and asserts the bit
  stays clear after a hash mismatch and after a failed chunk, then becomes set
  only after a successful retry.
- **Streaming recheck (3c-3) runs above the storage worker.** `Rechecker` owns
  the `StorageHandle`, the shared `FileMap` and `MetaInfo`, and a shared
  `HashPool`. `run(selection, base, cancel, progress)` reads through
  `StorageHandle::read_range` in 256 KiB chunks, hashes 1 MiB batches with an
  incremental SHA-1 state, and reads the next batch while the current batch
  hashes. At most two batches (2 MiB) are in memory, and one pool permit is
  held at a time, so the FIFO semaphore still makes live verification wait at
  most one recheck batch. There is still no `Recheck` storage command.
- **Recheck publishes only a complete result.** Selected bits are cleared in a
  clone of the base bitfield before reading and set again only after a full
  hash match; unselected bits are preserved and the caller's bitfield is never
  mutated. `Bitfield::mark_rechecked_present` is crate-private, and
  `Bitfield::set` stays private. A short read, missing file, or hash mismatch
  leaves the bit clear and counts as absent; the piece is abandoned on the
  first short read, including in the overlapped batch. A file removed between
  the metadata check and the open is reported as a short read by the backend,
  not as an I/O error. Other storage errors stop the run as typed
  `RecheckError::Storage`; `UnsafePath` and permission errors can never be
  reported as absent. Nothing is returned on error.
- **Recheck cancellation and exclusivity.** `RecheckCancel` is a shared atomic
  flag checked before each chunk read, before each hash job, and after the final
  hash; a cancelled run returns `RecheckError::Cancelled` and publishes
  nothing. Dropping the future releases the lease at its next await. An atomic
  flag plus a drop guard allows one recheck per `Rechecker` and returns
  `AlreadyRunning` otherwise; the session owns one `Rechecker` per torrent, so
  the lease is not shared across torrents. Recheck stays exclusive with
  same-torrent piece writes by caller contract; item 10 needs a `Checking`
  state that excludes `Downloading` while a recheck or resume validation runs.
- **Recheck progress is advisory.** One `RecheckProgress` snapshot is sent with
  `try_send` after each selected piece. A full channel drops updates, including
  the final one, so the returned `RecheckOutcome` is authoritative. The `Only`
  selection must be strictly increasing and in range; an empty selection
  returns the base bitfield without reading. `RecheckSelection` is validated
  before any read, so an invalid list cannot clear a bit.
- **Test hooks are compiled out of release builds.** The hash gate, the
  injected `DiskFull` write failure, the injected `PermissionDenied` read
  failure, and the read-request counter exist only under `#[cfg(test)]`; the
  release `bt-engine` library and `bt-tui` binary were checked for their
  strings. The read counter is what proves the next recheck batch is read while
  the current batch is held in the hash gate.
