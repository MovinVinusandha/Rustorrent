# BEP status

Legend: planned | in-progress | done | skipped. "rbit" column = whether
rbit appears to provide it (verify by reading its code and tests before relying on it).

| BEP | Name | Target | Status | rbit | Module | Tests | Notes |
|---|---|---|---|---|---|---|---|
| 3 | Core protocol | v0.1 | in-progress | verify | bencode, metainfo, piece, storage | unit + fuzz | bencode, v1 metainfo, and the piece verification/write path done; tracker and peer wire pending. Gaps: `.utf-8` keys and legacy text encodings (v0.2) |
| 23 | Compact peers | v0.1 | planned | verify | | | |
| 15 | UDP tracker | v0.1 | planned | verify | | | |
| 12 | Multi-tracker | v0.1 | planned | verify | | | |
| 9 | Magnet metadata | v0.1 | planned | verify | | | |
| 10 | Extension protocol | v0.1 | planned | verify | | | |
| 5 | DHT | v0.1 | planned | verify | | | |
| 11 | PEX | v0.2 | planned | verify | | | |
| 14 | LSD | v0.2 | planned | verify | | | |
| 6 | Fast extension | v0.2 | planned | verify | | | |
| 27 | Private torrents | v0.2 | planned | n/a | | | `private` flag parsed; no DHT/PEX/LSD yet |
| 19 | Web seeds | v0.2 | planned | verify | | | |
| 20 | Peer ID conventions | v0.2 | planned | n/a | | | |
| 42 | DHT security ext. | v0.3 | planned | verify | | | |
| 32 | DHT IPv6 | v0.3 | planned | verify | | | |
| 29 | uTP | v0.6 | planned | no | | | |
| 52 | v2 torrents | v0.6 | planned | verify | | | |
| 16 | Super-seeding | v1.0 | planned | no | | | |
| 55 | Holepunch | v1.0 | planned | verify | | | |

Non-BEP: MSE/PE encryption (v0.3).