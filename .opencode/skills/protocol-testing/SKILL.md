---
name: protocol-testing
description: Use when writing tests for parsers, peer protocol, trackers, storage, or end-to-end transfers.
---

## Layers
1. Unit tests from spec examples (round-trips, boundary values, malformed input).
2. Property tests (`proptest`) for encode/decode invariants.
3. Fuzz targets (`cargo-fuzz`): bencode, metainfo, peer messages, tracker replies.
   Seed corpus is small and committed; crashes are not.
4. Integration: localhost seeder + leecher in the same test process,
   using temp dirs and a small generated torrent.

## Rules
- Test data is generated random bytes. Never commit or download real media.
- CI never contacts real trackers, DHT bootstrap nodes, or public swarms.
  Use local fake trackers and loopback DHT nodes.
- Tests must be deterministic: seed RNGs, no sleeps for sync (use channels
  or timeouts with clear failure messages).
- A bug fix needs a regression test first.