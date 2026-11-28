# BEP status

Legend: planned | in-progress | done | skipped. The "rbit" column records a
module rbit could provide if adopted; ADR 0001 is deferred, so every entry
stays unverified.

| BEP | Name | Target | Status | rbit | Module | Tests | Notes |
|---|---|---|---|---|---|---|---|
| 3 | Core protocol | v0.1 | in-progress | verify | bencode, metainfo, bitfield, storage, piece, resume | unit + fuzz | bencode, v1 metainfo, layout, verification/write, streaming recheck, and version 1 resume save/restore done; tracker announce is in progress (HTTP is 4d) and the peer wire is not implemented. Fuzz targets compile in CI and run weekly. Gaps: `.utf-8` keys and legacy text encodings (v0.2). Tracker HTTP fields are described in `docs/beps/bep_0003_tracker.md` |
| 7 | IPv6 tracker | v0.1 | in-progress | verify | tracker::peers | unit + fuzz | `key` announce parameter and `peers6` decoding done; no dual-family or multi-homed announce |
| 23 | Compact peers | v0.1 | done | verify | tracker::peers | unit + fuzz | compact and list peer formats decoded, invalid records dropped, 200-peer cap |
| 15 | UDP tracker | v0.1 | in-progress | verify | tracker::udp, tracker::dns | unit + fuzz | codec, bounded resolver, target policy, 105 s deadline and UDP transport done; the announce state machine wires it. Session wiring lands with item 10 |
| 12 | Multi-tracker | v0.1 | in-progress | verify | tracker::candidates, tracker::state | unit | `announce-list` tiers, caps, shuffle, `announce` fallback and the public round policy done; session wiring lands with item 10 |
| 9 | Magnet metadata | v0.1 | planned | verify | | | |
| 10 | Extension protocol | v0.1 | planned | verify | | | |
| 5 | DHT | v0.1 | planned | verify | | | |
| 11 | PEX | v0.2 | planned | verify | | | |
| 14 | LSD | v0.2 | planned | verify | | | |
| 6 | Fast extension | v0.2 | planned | verify | | | |
| 27 | Private torrents | v0.2 | in-progress | n/a | tracker::state | unit | `private` flag, single active tracker, failure-only switch with peer drop and same-host redirect policy done; DHT/PEX/LSD/web-seed enforcement lands with those components |
| 19 | Web seeds | v0.2 | planned | verify | | | |
| 20 | Peer ID conventions | v0.2 | planned | n/a | | | |
| 42 | DHT security ext. | v0.3 | planned | verify | | | |
| 32 | DHT IPv6 | v0.3 | planned | verify | | | |
| 29 | uTP | v0.6 | planned | no | | | |
| 52 | v2 torrents | v0.6 | planned | verify | | | |
| 16 | Super-seeding | v1.0 | planned | no | | | |
| 55 | Holepunch | v1.0 | planned | verify | | | |

Non-BEP: MSE/PE encryption (v0.3).