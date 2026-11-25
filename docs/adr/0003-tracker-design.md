# ADR 0003: Tracker announce design

- Status: Proposed
- Date: 2026-11-25
- Deciders: project owner
- Supersedes: none
- Related: `docs/ROADMAP.md` item 4, ADR 0001, `docs/BEP_STATUS.md`,
  `docs/beps/bep_0015.md`, `bep_0012.md`, `bep_0023.md`, `bep_0007.md`,
  `bep_0020.md`, `bep_0027.md`, `deny.toml`, `fuzz/Cargo.toml`

## Decisions summary

Read this section first; the rest of the ADR holds the measurements, rules and
rationale. It is kept short so a session can load the decision without the
full document.

- Tracker: implement in-tree in `crates/engine/src/tracker`, not rbit. The
  full in-tree tracker is about 1.46 MB versus 2.10 MB for rbit, and rbit
  lacks hostname UDP, BEP 12 tiers, caps and a DNS policy.
- Size: UDP +54 KB, HTTP +71 KB, both +90 KB over a 398 KB tokio baseline.
  TLS dominates: ring adds about 968 KB, aws-lc about 1.72 MB. ring is chosen;
  it is smaller and needs no C toolchain.
- Build: `https` is a cargo feature, enabled by default in `bt-daemon` and
  `bt-tui`. `--no-default-features` gives a roughly 490 KB HTTP+UDP build
  that rejects `https` URLs; there is no runtime toggle.
- TLS: rustls 0.23 + tokio-rustls 0.26 + ring, TLS 1.2 and 1.3, ALPN
  `http/1.1`, no insecure override, no revocation checking.
- Roots: `webpki-roots` 1.0, compiled in. It needs `CDLA-Permissive-2.0` in
  `deny.toml`; that is a deny.toml-only prerequisite PR. Fallback is
  `rustls-native-certs` 0.8, which follows the OS store.
- Identity: peer ID `-BA0100-` plus 12 OS-random bytes. `BA` is a placeholder
  checked against BEP 20's known and unidentified tables, not another
  client's code. User agent `bt-app/0.1` sits in one constant. We do not
  impersonate another client; a private whitelist may refuse `BA`.
- HTTP: hand-written HTTP/1.1 `GET` with `Connection: close` and
  `Accept-Encoding: identity`. Limits: 1 MiB body, 64 KiB head, 100 header
  lines, 8 KiB per line. Chunked, Content-Length, close-delimited and
  HTTP/1.0 bodies are accepted; conflicting length/transfer-coding is
  rejected. Up to 5 redirects: absolute paths stay on the same host, absolute
  URLs are re-checked; 30 s per hop, 60 s total.
- Query: `info_hash` and `peer_id` are percent-encoded byte by byte; an
  existing query is extended with `&`, never a second `?`; our values replace
  duplicates already in the URL.
- UDP: BEP 15 connect then announce, 15/30/60 s sends with a 105 s cap. The
  cap is our decision, not the full `15 * 2^n` schedule. 200 peers, 6-byte
  IPv4 and 18-byte IPv6 strides.
- Peers: compact and list formats plus `peers6`; 200-peer cap; invalid
  addresses dropped; tracker-supplied hostnames dropped (SSRF).
- SSRF: public targets only by default; `allow_private_targets` is for tests
  and self-hosting. DNS results are checked and the connection goes to the
  checked IP; SNI and certificate verification use the hostname. Every
  redirect is re-checked.
- Redaction: `Debug`/`Display` show only `scheme://host[:port]/…`; path,
  query and fragment stay hidden. Redirect `Location`, `tracker id`,
  `failure reason` and `warning message` are sanitized (controls stripped,
  256-byte cap) before any log or UI. Logs keep a length, not the text.
- Tiers: BEP 12 for public torrents (shuffle once, try all trackers in a tier,
  move a success to the front; 32 URLs, 8 tiers, 4 per tier). Private
  torrents use one active tracker and switch only on failure (BEP 27).
- State: `started`, `completed`, `stopped`; interval clamped to 60..86400 s;
  exponential backoff with jitter; one in-flight announce per torrent.
- Boundary: engine-internal `TrackerTransport` with boxed futures; the client
  sees only a `TrackerStatus` snapshot.
- Tests: unit vectors, local HTTP/UDP/TLS trackers, paused-time UDP tests,
  log-capture tests, and fuzz targets `tracker_url`,
  `tracker_http_response`, `tracker_udp_response`.
- Limitations: no proxy (roadmap v0.3), no scrape, no dual-family announce,
  no IPv6 peer connections, and private whitelists may refuse the client.
- Prerequisites: the `deny.toml` license PR, dependency approval, and the
  tokio feature additions listed in section 14.

## Context

Roadmap item 4 is tracker announce. The engine already parses v1 metainfo
(`InfoHash`, `announce_list`, `private`), bencode, storage and resume. It has
no tracker, peer wire, or session code. The tracker is the first component
that talks to an untrusted network endpoint, follows redirects, resolves
names, and terminates TLS, so its design carries the project's first SSRF and
response-parsing decisions.

Constraints from `AGENTS.md` and the skills:

- small release size and low idle resource use, on Linux, Windows and macOS;
- no `unsafe`, no `unwrap`/`expect` outside tests, typed `thiserror` errors;
- no blocking I/O or hashing on a runtime worker;
- every network/file parser handles malformed input without panicking and has
  a fuzz target;
- TUI and GUI see only `crates/client`, never engine internals;
- new dependencies need explicit owner approval and a size justification;
- one concern per commit, small diffs.

ADR 0001 deferred the rbit decision to item 4. This ADR covers the tracker
part of that decision; DHT remains a separate item 5 decision.

Current release sizes for reference (Linux x86_64, workspace release profile):
`bt-daemon` 415,576 bytes and `bt-tui` 772,456 bytes.

## Decision

1. Implement the tracker in-tree in `crates/engine/src/tracker`. Do not adopt
   rbit for tracker announce.
2. v0.1 ships UDP, HTTP and HTTPS announces. HTTPS is implemented last, behind
   a cargo feature `https` that is enabled by default in the app build;
   `--no-default-features` gives a roughly 490 KB HTTP+UDP build that rejects
   `https` URLs.
3. TLS uses rustls 0.23 with the `ring` provider, TLS 1.2 and 1.3, ALPN
   `http/1.1`, and compiled-in `webpki-roots` 1.0 if the root license is
   approved; otherwise the OS root store through `rustls-native-certs`.
4. The default target policy allows public addresses only. Loopback, private
   and link-local targets require `TrackerConfig::allow_private_targets`.
5. Public torrents use the BEP 12 tier rules; private torrents use one active
   tracker and switch only on failure, per BEP 27.
6. Announce responses are decoded from both compact and list peer formats
   (BEP 23) and from `peers6` (BEP 7). UDP is compact by construction.
7. The tracker exposes a small engine-internal state machine and an
   object-safe `TrackerTransport` trait with boxed futures, so tests can
   replace the network with a scripted fake.
8. Identity uses placeholder constants: peer ID `-BA0100-` plus 12 random
   bytes, and user agent `bt-app/0.1`. `BA` is not a registered client code.
   We do not impersonate another client; a private tracker that whitelists
   client IDs may refuse this one.

## 1. In-tree implementation versus rbit

### Method

The comparison builds were made in a scratch workspace outside this
repository, with the workspace release profile (`opt-level = "z"`, LTO,
`codegen-units = 1`, `panic = "abort"`, `strip = true`), on Linux x86_64 with
rustc 1.99.0. Each feature set was rebuilt in isolation so Cargo feature
unification between rbit and the in-tree code could not skew a result. All
numbers are single binaries; the baseline is a tokio binary with the same
runtime features and no tracker code. The in-tree HTTP build is a prototype
with a hand-written HTTP/1.1 client and URL parser, not production code, so
treat the deltas as indicative, not as a CI gate.

| Build | Bytes | Delta vs baseline |
|---|---:|---:|
| tokio baseline | 397,904 | - |
| baseline + getrandom | 400,368 | +2,464 |
| in-tree UDP | 451,968 | +54,064 |
| in-tree HTTP | 469,096 | +71,192 |
| in-tree HTTP + UDP | 488,328 | +90,424 |
| in-tree HTTP + ring | 1,435,088 | +1,037,184 |
| in-tree HTTP + UDP + ring | 1,456,488 | +1,058,584 |
| in-tree HTTP + ring, no roots | 1,372,816 | +974,912 |
| in-tree HTTP + ring, TLS 1.3 only | 1,390,344 | +992,440 |
| in-tree HTTP + ring, native roots | 1,403,160 | +1,005,256 |
| in-tree HTTP + aws-lc | 2,188,816 | +1,790,912 |
| in-tree HTTP + UDP + aws-lc | 2,208,416 | +1,810,512 |
| in-tree HTTP + aws-lc, no roots | 2,124,432 | +1,726,528 |
| rbit announce facade (HTTP + UDP + TLS) | 2,104,728 | +1,706,824 |
| rbit HTTP only (with TLS) | 2,063,528 | +1,665,624 |
| rbit UDP only | 500,344 | +102,440 |

Dependency graph: rbit pulls 166 crates; the in-tree tracker needs tokio
(already present), `getrandom`, and for HTTPS `rustls`, `tokio-rustls`,
`ring` and a root source.

In the table, a `ring` or `aws-lc` row is the HTTPS build (HTTP plus TLS);
UDP is a separate transport. The no-roots rows are measurement bounds only; a
build without a root store cannot verify certificates and is not a supported
configuration.

### Reading the numbers

- The tracker logic without TLS is cheap: +54 KiB for UDP, +71 KiB for HTTP,
  +90 KiB for both. This is the part we would write either way.
- TLS dominates. Adding ring to the HTTP + UDP build costs 968,160 bytes;
  aws-lc costs 1,720,088. The full in-tree tracker is 1,456,488 bytes, about
  3.5 times the current daemon, and the delta over the current daemon is
  roughly 1.04 MB because tokio is already linked there.
- rbit's full facade is 648,240 bytes larger than the in-tree ring build
  (about 31% smaller in-tree) and 1.62 MB larger than the in-tree build
  without TLS. Its UDP-only build is 48,376 bytes larger than our UDP build,
  but it still needs a TLS stack for HTTPS.
- The cheap TLS levers are native roots (-31,928) and TLS 1.3 only (-44,744).
  Neither changes the order of magnitude; system TLS was not measured and is
  the only plausible large reduction. It is rejected below for portability
  and testability reasons.

### Functional gaps in rbit 0.2.2

These were confirmed against the rbit 0.2.2 source in a scratch probe:

- UDP rejects hostnames (`InvalidUrl("localhost:16969")`); a UDP tracker URL
  with a name, which is the normal case, cannot be used.
- An HTTP announce URL that already has a query is appended with `?` instead
  of `&`, producing `?passkey=abc?info_hash=...`.
- Invalid peers such as `0.0.0.0:0` and `::` are kept.
- No BEP 12 tier scheduler: no per-tier ordering, shuffle, or move-to-front.
- No HTTP `key`, `numwant` or `trackerid`.
- A fixed 30-second HTTP timeout and three UDP attempts, with no configurable
  budget.
- No response size cap, no peer cap, and no DNS/IP target policy.

Wrapping rbit would therefore still require a policy layer, a URL fix, a peer
filter, a tier scheduler and a timeout override, on top of a 166-crate graph.
The in-tree code is smaller and gives us the exact policies this ADR
specifies.

### Recommendation

Implement in-tree. Reject rbit for tracker announce. Do not close ADR 0001
globally: DHT at item 5 may still evaluate rbit, and that decision should use
its own measurements.

## 2. Scope per release

### v0.1

- HTTP/1.1 announce over TCP, HTTPS through rustls in the default app build,
  UDP announce per BEP 15. A `--no-default-features` build keeps HTTP and UDP
  and rejects `https` URLs.
- Public torrents with BEP 12 tiers and private torrents with BEP 27 rules.
- BEP 23 compact and list peers, BEP 7 `peers6` and `key`.
- Events `started`, `completed`, `stopped`; periodic announces; exponential
  backoff after a failed round.
- Response fields: `interval`, `min interval`, `complete`, `incomplete`,
  `peers`, `peers6`, `tracker id`, `failure reason`, `warning message`.
- One in-flight announce per torrent; one connection per HTTP announce.

Known limitations of v0.1:

- No proxy. A user who can reach trackers only through a proxy cannot
  announce; proxy support is roadmap item v0.3.
- Many private trackers whitelist client IDs. The placeholder `BA` code is
  not on those lists, so such a tracker may refuse the client. We do not
  impersonate a listed client to work around it.
- No scrape, no dual-family announce, no IPv6 peer connections.

### v0.2 and later

- Scrape (BEP 48 / BEP 15 action 2).
- Dual-family and multi-homed announces (BEP 7).
- IPv6 peer connections; v0.1 decodes and stores IPv6 peers only.
- Per-tracker health statistics and tracker URL auto-update.
- Custom CA / extra root certificates, if self-hosted trackers need it.
- HTTP proxy support is v0.3 (roadmap), not v0.2.

### Explicitly out of v0.1

Scrape, BEP 41 extensions, HTTP/2, gzip, cookies, HTTP authentication,
proxies, web seeds, DHT, PEX, LSD, tracker URL lists from magnet links, and
any user-supplied TLS override.

## 3. TLS and roots

### Build feature

TLS is a cargo feature named `https`, enabled by default in the app build:

- `bt-engine` gets an `https` feature that pulls `rustls`, `tokio-rustls`,
  `ring` and `webpki-roots`.
- `bt-daemon` and `bt-tui` set `default = ["https"]` and forward it to
  `bt-engine`.
- `cargo build --no-default-features` produces the minimal build: HTTP and UDP
  only, about 490 KB (measured 488,328 bytes), versus about 1.46 MB with TLS.
- A build without the feature rejects an `https` URL with
  `TrackerError::UnsupportedScheme` at parse time. There is no plaintext
  fallback and no runtime toggle: the feature is compile-time only.
- `deny.toml` sets `all-features = true`, so `cargo deny` checks the TLS
  dependency graph even when the default build does not enable it.

### Crates and features

| Crate | Version | Features | License |
|---|---|---|---|
| rustls | 0.23 | `default-features = false`, `std`, `tls12`, `ring` | Apache-2.0 OR ISC OR MIT |
| tokio-rustls | 0.26 | `default-features = false`, `tls12`, `ring` | MIT OR Apache-2.0 |
| ring | 0.17 (via rustls) | - | Apache-2.0 AND ISC |
| webpki-roots | 1.0 | default | CDLA-Permissive-2.0 |
| rustls-native-certs | 0.8 (fallback) | default | Apache-2.0 OR ISC OR MIT |

All MSRVs are below the workspace MSRV of 1.88.

### Why ring

ring costs 968,160 bytes over the no-TLS build; aws-lc-rs costs 1,720,088,
751,928 more than ring. aws-lc-rs also needs a C toolchain (CMake, and NASM on
Windows), which complicates cross-platform release builds. ring is the smaller
and simpler choice, and rustls treats both providers as supported. The
trade-off is that ring is not FIPS-oriented; this client does not need FIPS.

### Protocol policy

- TLS 1.2 and TLS 1.3 are both enabled. TLS 1.3 only would save 44,744 bytes
  but breaks older tracker endpoints. The saving is not worth the
  compatibility loss.
- ALPN offers `http/1.1` only. HTTP/2 is out of scope and would add a framing
  stack for no tracker benefit.
- Certificate and hostname verification are always on. There is no
  `danger_accept_invalid_certs`, no custom verifier, and no plaintext fallback
  for an `https` URL.
- SNI is the URL host. For an IP-literal URL, `ServerName::IpAddress` is used
  and the certificate must contain a matching IP SAN.
- No client certificates, no custom session resumption, no early data, and
  no revocation checking (no OCSP or CRL). Revocation is documented as out of
  scope; the root store and certificate lifetime are the control.
- The TLS handshake has a 10-second timeout inside the 30-second per-hop
  budget. A handshake failure is `TrackerError::Tls`; the announce round
  fails and the state machine backs off.

### Root store

Preferred: `webpki-roots` 1.0, compiled into the binary. It gives the same
roots on every OS, deterministic tests, and no dependency on the host store.
Its license is CDLA-Permissive-2.0, which `deny.toml` does not allow today.

**Prerequisite PR (deny.toml only):** before the TLS branch, add
`CDLA-Permissive-2.0` to the `deny.toml` allow list for `webpki-roots`. `ring`
0.17 is `Apache-2.0 AND ISC`, and both are already allowed, so it should pass
unchanged; run `cargo deny check licenses` and add a `[[licenses.clarify]]`
entry for ring only if the file scanner disagrees. This ADR does not change
`deny.toml`.

The root list is baked in at build time, so a major root change needs a
dependency update. The weekly `cargo deny` and `cargo update` pass is the
reminder; there is no runtime root refresh.

Fallback: `rustls-native-certs` 0.8, which loads the OS store. It is 31,928
bytes smaller, is license-clean under the current policy, and follows
enterprise or user-installed roots. The cost is OS-dependent behavior and
tests that cannot assume a fixed root set. If the license approval is denied,
the fallback is the decision, not a blocker.

## 4. HTTP behaviour

### Request

- One `GET` per announce, HTTP/1.1, `Connection: close`, no request body, no
  keep-alive, no pipelining.
- Headers: `Host` (host plus non-default port, IPv6 bracketed),
  `User-Agent: bt-app/0.1` (a placeholder constant), `Accept: */*`,
  `Accept-Encoding: identity`, `Connection: close`. Any content coding other
  than `identity` is rejected, so a gzip response is a protocol error rather
  than something to decode.
- The request target is the URL path and query, or `/` when empty.
- Query parameters, in this order: `info_hash`, `peer_id`, `port`,
  `uploaded`, `downloaded`, `left`, `compact=1`, `event` when not `none`,
  `numwant`, `key`, `trackerid` when the tracker returned one.
- `info_hash` and `peer_id` are the raw 20 bytes, percent-encoded byte by
  byte with uppercase hex, never via a UTF-8 string. Every byte outside the
  unreserved set `A-Z a-z 0-9 - . _ ~` is escaped, including ASCII letters
  that happen to appear in a hash: `0x41` becomes `%41`, not `A`. `port`,
  counters, `numwant` and `key` are decimal.
- An existing query in the tracker URL is preserved and extended with `&`,
  never a second `?`: `?passkey=abc` becomes `?passkey=abc&info_hash=...`.
  A URL ending in `?` gets the first pair directly after that `?`. Existing
  `info_hash`, `peer_id`, `port`, `key`, `compact`, `numwant`, `event`, `ip`,
  `ipv4` and `ipv6` parameters are removed first, so our values always win.
  The raw query is kept as bytes; it is not decoded and re-encoded.
- `numwant` is 50 for a normal announce and 0 for `stopped`.
- `event` is omitted for a periodic announce, and is `started`, `completed`
  or `stopped` otherwise.
- The URL is rebuilt from parsed components, not string-concatenated.

### Response

- Status 200 is the only success. 3xx is handled as a redirect. Any other
  status is `TrackerError::HttpStatus`, with no body retained.
- The status line must be HTTP/1.0 or HTTP/1.1; any other version is a
  malformed response. HTTP/2 is not negotiated.
- The response head is capped at 64 KiB, 100 header lines, and 8 KiB per
  line. Header names are ASCII and case-insensitive; obs-fold, NUL, CR and LF
  inside a value are rejected.
- `Content-Length` must parse as a decimal `usize`; duplicate values must be
  identical. `Transfer-Encoding` must be exactly `chunked`. A response with
  both `Content-Length` and `Transfer-Encoding` is rejected, as is a
  non-chunked transfer coding.
- No length and no chunking means read to EOF, capped at 1 MiB. A chunked body
  is decoded incrementally with the same cap; the chunk-size line is capped at
  1 KiB and trailers at 8 KiB.
- The body is capped at 1 MiB before bencode decoding. The existing bounded
  bencode decoder is used with its depth limit; no length prefix can allocate
  beyond the body.
- The body must be a bencode dictionary. `failure reason` is checked first and
  produces `TrackerError::Rejected`. `warning message` is stored as a
  sanitized message. `interval` and `min interval` are integers in seconds;
  missing `interval` means 1800. The effective interval is
  `clamp(max(interval, min_interval), 60, 86400)`. `complete` and `incomplete`
  are optional non-negative counters. `tracker id` is an optional byte string
  capped at 256 bytes and kept per tracker for HTTP announces. Both tracker
  strings are sanitized before they can reach a log or the UI: UTF-8 lossy,
  control characters stripped, 256-byte cap, and passkey-like substrings of
  the tracker URL replaced with `<redacted>` (section 9). The tracker ID is
  opaque: it is echoed only to the tracker that issued it, is never logged,
  and its `Debug` form prints only its length.
- Unknown keys are ignored. `peers` and `peers6` are decoded per the BEP 23
  and BEP 7 notes.
- The `Content-Type` header is ignored; trackers frequently send
  `text/plain`. Bencode validity is the only test. A `Content-Encoding` other
  than `identity` is a malformed response, because we request `identity`.

### Redirects

- 301, 302, 303, 307 and 308 are followed, up to 5 hops.
- `Location` may be an absolute `http`/`https` URL or an absolute path on the
  same host. Other relative forms are rejected. The raw value is capped at
  2048 bytes.
- Every `Location` value is resolved against the current URL, then parsed into
  a `TrackerUrl` before use, so it is redacted exactly like the original URL
  in `Debug`, `Display`, errors and logs. A malformed `Location` produces the
  fixed `<invalid tracker url>` placeholder, never the raw header value.
- An `https` to `http` downgrade is rejected. `http` to `https` is allowed.
  Any other scheme is rejected.
- Every hop is parsed, resolved, policy-checked and connected from scratch.
  No connection, DNS result or TLS session is reused across hops.
- Userinfo in a redirect target is rejected, not silently stripped.
- The total announce budget is 60 seconds across all hops; each hop has 30
  seconds for DNS, connect, TLS, request and response.

### Timeouts

| Phase | Budget |
|---|---|
| DNS resolve | 5 seconds |
| Connect | 10 seconds, inside the hop budget |
| TLS handshake | 10 seconds, inside the hop budget |
| One hop (DNS + connect + TLS + request + response) | 30 seconds |
| Whole announce including redirects | 60 seconds |
| Stopped announce | 2 seconds total, no retries |

## 5. UDP behaviour

The wire format and retransmission rules are in `docs/beps/bep_0015.md`.
Summary of the decisions:

- One connected UDP socket per announce attempt, bound to the address family
  of the chosen tracker address. The kernel drops datagrams from other
  sources.
- Connect request first; the connection ID is reused for the announce and for
  a reconnect after an expired-ID error. No cross-announce connection-ID
  cache in v0.1.
- Three sends per attempt, with waits of 15, 30 and 60 seconds, for a
  105-second budget. This is a deliberate cap on the BEP 15 `15 * 2^n`
  schedule; the state machine's backoff covers longer outages.
- Receive buffer 4096 bytes; 200 peers per response; 6-byte stride on IPv4
  and 18-byte stride on IPv6, selected by the socket family.
- Transaction IDs are 32 bits from OS randomness, fresh for each connect and
  announce. A mismatched action or transaction ID is ignored and the receive
  wait continues.
- Error packets are checked for transaction ID, truncated to 256 bytes,
  control-stripped, and never logged raw. The attempt fails with
  `TrackerRejected`.
- No scrape, no BEP 41, no multi-homed or dual-family announce.

## 6. Target policy and SSRF

`TrackerConfig::allow_private_targets` defaults to `false`. The policy is a
pure function of the candidate `IpAddr` and the flag, applied to every address
the resolver returns and to every redirect target.

### Always blocked

| Range | Reason |
|---|---|
| `0.0.0.0/8` | "this network" |
| `224.0.0.0/4`, `ff00::/8` | multicast |
| `255.255.255.255` | broadcast |
| `::/128` | unspecified |
| `::ffff:0:0/96` | IPv4-mapped IPv6, to avoid check bypass |
| `2002::/16`, `64:ff9b::/96` | IPv4-embedded transition forms, to avoid check bypass |
| `192.0.0.0/24` | IETF protocol assignments |
| `192.0.2.0/24`, `198.51.100.0/24`, `203.0.113.0/24`, `2001:db8::/32` | documentation |
| `198.18.0.0/15` | benchmarking |
| `240.0.0.0/4` | reserved |
| `100::/64` | discard-only |

### Blocked unless `allow_private_targets`

| Range | Reason |
|---|---|
| `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16` | private |
| `100.64.0.0/10` | carrier-grade NAT |
| `127.0.0.0/8`, `::1/128` | loopback |
| `169.254.0.0/16`, `fe80::/10` | link-local |
| `fc00::/7` | unique local |

The setting is for tests and self-hosted trackers. It does not unblock
multicast, broadcast, unspecified, documentation, benchmarking, IETF
assignments, transition forms or IPv4-mapped forms. Transition forms are
blocked rather than translated, so an IPv6-only host with NAT64 can reach
native IPv6 trackers but not IPv4 trackers through the well-known prefix. A
private tracker on a LAN uses a private address and requires the setting.
Enabling it in a daemon that loads untrusted torrents lets a tracker reach
loopback and LAN services, including cloud metadata at `169.254.169.254`;
keep it off outside tests and self-hosting.

### Name resolution

- A hostname is resolved once per hop with a 5-second timeout and at most 8
  results. Every result is policy-checked; blocked results are skipped.
- The first allowed result is used. If a TCP connect fails immediately, the
  next allowed result is tried, up to the 8-result cap. A timeout does not
  fall through to the next result, because the attempt budget is already
  bounded. UDP uses the first allowed result and falls through only when the
  send itself fails.
- The connection is made to the checked `IpAddr`, never by passing the
  hostname to a connector that would resolve again. This closes the DNS
  rebinding window between check and connect.
- For HTTPS the TCP connection still goes to the checked IP, but SNI and
  certificate verification use the original hostname: the TLS `ServerName` is
  the URL host, not the IP. For an IP-literal URL, `ServerName::IpAddress` is
  used and the certificate must contain a matching IP SAN. Connecting to the
  IP without this would verify the wrong name.
- Resolution uses `tokio::net::lookup_host`, which runs the system resolver on
  tokio's blocking pool. The caller wraps it in the 5-second timeout. Dropping
  the future does not cancel the blocking call, so a hung resolver keeps one
  blocking thread until it returns; the one-announce-per-torrent rule and the
  per-hop budget bound how many lookups can be outstanding.
- An IP-literal URL is parsed and checked directly, with no DNS.
- Hostnames are ASCII only: no IDNA, no Unicode. Labels are LDH, at most 63
  bytes, total at most 253 bytes, one trailing dot allowed and stripped.
  IPv6 zone IDs (`%eth0`) are rejected.
- A redirect repeats resolution and checking from scratch. A host that
  resolves to both allowed and blocked addresses can still be used through an
  allowed address; a host whose every result is blocked fails with
  `TrackerError::BlockedTarget`.
- There is no port allowlist. The address policy is the control; a public IP
  on an unusual port is allowed.

## 7. Identity, key and announce parameters

- Peer ID: `-BA0100-` plus 12 bytes from OS randomness, exactly 20 bytes,
  generated once per process and shared by all torrents. Not persisted, not
  configurable. `BA` is a placeholder for the working name `bt-app`, checked
  against BEP 20's known-client and unidentified tables; it is not another
  client's code. See `docs/beps/bep_0020.md`.
- User agent: `bt-app/0.1`, a placeholder held in one named constant, not
  derived from the peer ID.
- Many private trackers whitelist client IDs, so an unknown code such as `BA`
  may be refused. We record that as a product limitation and do not
  impersonate a listed client to get around it.
- `key`: a `u32` from OS randomness, generated once per torrent session. The
  same value is used for HTTP and UDP and for IPv4 and IPv6. It is not the
  peer ID and is not persisted. See `docs/beps/bep_0007.md`.
- `port`: the TCP listen port supplied by the session. Port 0 is rejected
  before a request is built. There is no separate UDP port in v0.1.
- `numwant`: 50 for `started`, `completed` and periodic announces; 0 for
  `stopped`.
- `compact=1` on every HTTP announce. UDP has no such parameter.
- `trackerid` is sent only to the HTTP tracker that returned it, and is
  dropped when the active tracker changes.
- `ip`, `ipv4` and `ipv6` parameters are never sent. The UDP IP field is 0.
- `event` is `started` on the first announce, `completed` once when the
  download finishes, `stopped` on pause, removal or shutdown, and omitted
  otherwise.

## 8. State machine, BEP 12 and BEP 27

### States

`Tracker` is a pure state machine driven by the session's select loop. It
exposes `next_deadline`, `build_request`, `handle_response`,
`handle_failure`, and a `TrackerStatus` snapshot. It does not own a task, a
socket or a clock; the session supplies `Instant` and cancellation. This keeps
time-dependent tests deterministic.

Per torrent, the state is:

- candidate list (public: tiers; private: a flat ordered list);
- active tier and tracker cursor, or active tracker index;
- `next_announce: Instant`;
- `pending_event: Option<AnnounceEvent>`;
- `consecutive_failures: u32`;
- `completed_sent: bool`;
- `tracker_id: Option<Vec<u8>>`;
- `last_interval: Duration`.

### Schedule

- Torrent start: send `started` immediately.
- Download completion: send `completed` immediately, once. If it fails, keep
  it pending and retry it on the next attempt; do not send `started` again.
- Periodic announce: at `next_announce`, with no event.
- Pause, removal or shutdown: send `stopped` once with a 2-second budget and
  no retries, then become terminal. A crash or kill sends no `stopped`; this
  is documented and unavoidable.
- One in-flight announce per torrent. An event that arrives during an
  announce is queued; after the in-flight attempt, `stopped` wins over
  `completed`, and `completed` wins over `started`.
- A successful announce sets `next_announce = now + effective_interval` and
  resets `consecutive_failures`.
- A failed round sets
  `next_announce = now + min(30 * 2^(failures-1), 1800)` seconds, with ±20%
  jitter. The first retry is about 30 seconds, then 60, 120, 240, 480, 960,
  and 1800 at the cap. Jitter avoids a thundering herd after a tracker
  outage.
- The interval is clamped to 60..86400 seconds, and `min interval` can only
  raise it. An interval of 0 or a negative value is treated as missing and
  becomes 1800.

### Public torrents (BEP 12)

- One shuffle at load with OS randomness; the order is stable for the
  session. Tests inject a fixed order.
- Start at the last tier that succeeded, or tier 0 if none ever did.
- Within a tier, try trackers sequentially in order, up to 4 per tier. Every
  tracker in the tier is attempted even after one succeeds; the peer lists of
  all successes in that tier are merged, deduplicated, and capped at 200 new
  peers per round.
- Stop after the first tier with at least one success. Later tiers are
  backups and are not contacted in that round.
- Move the first successful tracker in the tier to the front, keeping the
  relative order of the rest. Failed trackers stay in place and are retried
  next round.
- If every tracker in every tier fails, the round fails and the backoff
  applies. The next round starts at the last successful tier, or tier 0.
- Caps: 32 URLs, 8 tiers, 4 per tier. Duplicate normalized URLs are removed;
  invalid schemes, empty hosts and bad ports are dropped with counters.
- If no usable URL remains, the torrent has no tracker and the session gets a
  typed error; no request is made.

### Private torrents (BEP 27)

- Flatten the tiers into one ordered list, shuffle once, and keep exactly one
  active tracker.
- The active tracker is used until it fails. On failure, the next candidate
  becomes active immediately; only after every candidate has failed does the
  round back off.
- Before the first announce to a new tracker, drop every peer connection and
  every peer candidate learned from the old tracker. Downloaded pieces and
  verified progress are kept.
- DHT, PEX, LSD and web seeds are disabled at the session level for a private
  torrent, not merely absent by default.
- Incoming connections are still accepted; BEP 27 restricts outgoing peer
  discovery, not listening.
- A redirect is followed only to the same host, with the same port unless the
  scheme upgrades from `http` to `https` (then the default https port is
  allowed). A cross-host redirect fails the announce; the target is never
  added to the candidate list.
- A private torrent with no usable tracker fails without contacting anything.

## 9. URL parsing and redaction

### Parser

A strict hand-written parser covers the tracker URL grammar:
`scheme://host[:port][/path][?query][#fragment]`, with `http`, `https` and
`udp` schemes. It is deliberately smaller than a general URL crate and avoids
IDNA and Unicode tables we do not need. It is a network-input parser and gets
a fuzz target.

Rules:

- scheme and host are lowercased; the default port is removed for `http`
  (80) and `https` (443). A `udp` URL must carry an explicit port.
- userinfo (`user:pass@`) is rejected, not stripped.
- IPv6 literals are bracketed and parsed with `std::net::Ipv6Addr`.
- non-ASCII hosts, spaces, controls, backslashes and empty labels are
  rejected.
- the path defaults to `/`; query and fragment are preserved as raw bytes
  after validation.
- the raw URL is capped at 2048 bytes; longer URLs are invalid.
- a URL with a fragment keeps it in the raw form but never sends it; the
  fragment is not part of the HTTP request target.

### Redaction

`TrackerUrl` stores the raw string and implements `Debug` and `Display` as
the redacted form. The raw form is only reachable through a crate-internal
`raw()` accessor used by the transport, and is never put in a log field.

Redaction keeps `scheme://host[:port]` and replaces the rest:

| Input | Output |
|---|---|
| `https://tracker.example/announce?passkey=SECRET` | `https://tracker.example/…` |
| `http://tracker.example:8080/announce` | `http://tracker.example:8080/…` |
| `udp://tracker.example:6969` | `udp://tracker.example:6969` |
| `http://tracker.example/` | `http://tracker.example/` |
| `http://user:pass@tracker.example/announce` | `<invalid tracker url>` |
| `not a url` | `<invalid tracker url>` |

- The path, query and fragment are never shown, because a passkey can live in
  any of them.
- The default port is omitted; a non-default port is shown.
- IPv6 hosts are bracketed.
- A malformed URL produces the fixed placeholder; the parse error is not
  included, because it can echo the input.
- `TrackerError` variants that name a tracker use the redacted form.
- Redirect `Location` values are parsed into `TrackerUrl` before use, so a
  passkey in a redirect target is redacted the same way. A malformed
  `Location` never reaches a log or error raw; it becomes the fixed
  placeholder.
- Tracker `failure reason` and `warning message` are sanitized separately:
  UTF-8 lossy conversion, control characters stripped, 256-byte cap, and any
  occurrence of the raw URL, its raw path, its query values (any length), and
  its path segments of 8 bytes or more replaced with `<redacted>`. Both go
  through the same helper; neither is special-cased. The sanitized text may
  be shown to the user, but logs record only a length and a fixed "tracker
  message" field, never the text.
- `tracker id` is opaque bytes and is never displayed. It is capped at 256
  bytes, sent back only to the tracker that issued it, and its `Debug` form
  prints only its length. It is percent-encoded byte by byte when echoed.
- Tests cover a passkey in the query, a passkey in the path, userinfo, IPv6,
  default and non-default ports, uppercase scheme and host, a fragment,
  malformed inputs, and a 4096-byte URL. Each test asserts that the secret
  substring is absent from `Debug`, `Display`, and every error string. Further
  tests cover a `Location` with a passkey, a `failure reason` that echoes the
  URL, a `warning message` with control characters, and a `tracker id` that
  contains a passkey: none of the raw bytes may appear in a log line, an
  error, or the status snapshot.

## 10. Trait boundary and types

The tracker is engine-internal; `bt-proto` is not extended and the TUI/GUI
see only a `TrackerStatus` snapshot through `bt-client`.

```rust
pub trait TrackerTransport: Send + Sync {
    fn announce<'a>(
        &'a self,
        request: AnnounceRequest,
    ) -> Pin<Box<dyn Future<Output = Result<AnnounceResponse, TrackerError>> + Send + 'a>>;
}
```

- Boxed futures instead of `async-trait`, to avoid a dependency and keep the
  trait object-safe.
- `Send + Sync` because the session is async and may move the tracker between
  workers.
- Dropping the returned future cancels the announce; the transport closes its
  socket. The state machine also wraps the call in an outer timeout as
  defence in depth.
- The production implementation is `NetworkTransport`, which dispatches on
  the URL scheme to the HTTP and UDP transports. Tests inject a
  `FakeTransport` that records requests and returns scripted responses,
  errors or pending futures.

Types (engine-internal, `crates/engine/src/tracker`):

- `AnnounceRequest { url: TrackerUrl, info_hash: InfoHash, peer_id: PeerId,
  port: u16, stats: AnnounceStats, event: AnnounceEvent, num_want: i32,
  key: u32, tracker_id: Option<Vec<u8>> }`
- `AnnounceStats { uploaded: u64, downloaded: u64, left: u64 }`
- `AnnounceEvent { None, Started, Completed, Stopped }`
- `AnnounceResponse { interval: Duration, min_interval: Option<Duration>,
  complete: Option<u64>, incomplete: Option<u64>, peers: Vec<SocketAddr>,
  tracker_id: Option<Vec<u8>>, warning: Option<TrackerMessage> }`
- `TrackerError`: `InvalidUrl`, `UnsupportedScheme`, `Dns`, `BlockedTarget`,
  `Connect`, `Tls`, `Timeout`, `HttpStatus(u16)`, `MalformedResponse`,
  `Rejected { message: TrackerMessage }`, `TooLarge`, `Redirect`,
  `Cancelled`, `Io`.
- `TrackerEvent`: `AnnounceSucceeded`, `AnnounceFailed`, `PeersReceived`,
  `Warning`, `TrackerSwitched`, `NoUsableTracker`.
- `TrackerStatus`: redacted active URL, last success, next announce, last
  interval, consecutive failures, peer count, last warning flag.

`thiserror` derives the error type; `Display` never contains a raw URL or raw
tracker message. All parsing returns `Result`, no `unwrap` or `expect` outside
tests.

## 11. Testing and fuzzing

### Unit

- URL parser and redaction tables, including malformed inputs.
- BEP 23 and BEP 7 peer decoding vectors, partial records, invalid
  addresses, dedup and caps.
- HTTP head/body parser vectors: Content-Length, chunked (including a chunk
  split across reads and a trailer section), close-delimited, HTTP/1.0 with
  and without a length, duplicate length, both length and transfer coding,
  a non-chunked transfer coding, an oversized head and body, an oversized
  chunk-size line, invalid UTF-8 in a header value, obs-fold, and a
  `Content-Encoding: gzip` response. The chunked decoder is exercised with a
  body just under and just over the 1 MiB cap.
- HTTP request builder vectors: method and target, `Host` with and without a
  non-default port, IPv6 bracket form, `User-Agent: bt-app/0.1`,
  `Accept-Encoding: identity`, `Connection: close`, and no body. A URL with
  `?passkey=abc` produces `...?passkey=abc&info_hash=...` and exactly one
  `?`; a URL ending in `?` appends the first pair directly. An existing
  `info_hash`, `peer_id`, `port`, `key`, `compact`, `numwant`, `event`, `ip`,
  `ipv4` or `ipv6` parameter is removed. A hash containing `0x00`, `0x20`,
  `0x41`, `0x7e` and `0xff` proves byte-wise percent-encoding: `%00`, `%20`,
  `%41`, `~`, `%FF`.
- Redirect vectors: absolute URL, same-host absolute path, cross-host target,
  `https` to `http` downgrade, a `Location` with a passkey (redacted in every
  diagnostic), a malformed `Location`, and more than 5 hops.
- UDP connect/announce/error vectors, wrong transaction ID and action,
  truncated packets, 6- and 18-byte strides.
- Policy table with the first and last address of every range, plus
  IPv4-mapped and zone-ID cases.
- State machine with a fake clock and fake transport: interval clamp, event
  coalescing, backoff, tier order, move-to-front, private switch, stopped
  budget, no-usable-tracker.

### Integration

- Local HTTP tracker on `127.0.0.1:0`: asserts the request line and every
  query parameter, returns scripted bencode, redirects, chunked bodies,
  Content-Length bodies, close-delimited bodies, HTTP/1.0 responses,
  malformed bodies, conflicting framing headers, oversized bodies and slow
  responses.
- Local UDP tracker on `127.0.0.1:0`: connect/announce, retransmission,
  wrong transaction ID, error packet, timeout. Tests use
  `#[tokio::test(start_paused = true)]` so the 15/30/60-second waits are
  virtual; a fake transport covers the cases a real socket cannot.
- Local TLS tracker with a test CA generated by `rcgen` (dev-dependency only,
  no release size impact): success against a certificate for the hostname
  while the socket is reached through `127.0.0.1` (proves SNI and verification
  use the hostname, not the IP), hostname mismatch, expired certificate, and
  a plaintext server on an `https` URL must all fail closed.
- DNS tests use an injected resolver: a resolve timeout with a paused clock,
  a mix of allowed and blocked results, and a resolver that returns more than
  8 results. `lookup_host` itself is covered by one localhost integration
  test, not by the deterministic suite.
- End-to-end: a metainfo with two tiers, a private torrent, a tracker switch,
  and a session loop that asserts peer churn and event order.
- Log capture: a failure reason, a warning message, a `Location` and a
  `tracker id` containing the passkey must not appear in any formatted log
  line, error, or status snapshot.

### Fuzz

- `tracker_url`: arbitrary bytes to the URL parser and redactor; no panic,
  bounded output.
- `tracker_http_response`: arbitrary bytes to the head, body, chunked and
  bencode decoders, including `peers` and `peers6`; no panic, no allocation
  beyond the 1 MiB cap.
- `tracker_udp_response`: arbitrary bytes to the connect, announce and error
  decoders; no panic.
- Targets are registered in `fuzz/Cargo.toml`, compile in CI, and run weekly,
  matching the existing bencode and metainfo targets.

## 12. Consequences

Positive:

- About 1.46 MB for the full tracker versus 2.10 MB for rbit, with no
  166-crate graph.
- Exact control of SSRF, redirects, timeouts, caps, redaction and private
  torrent behaviour.
- The tracker logic without TLS is only +90 KiB, so UDP and HTTP can land
  before the TLS work and stay testable.
- A small trait boundary keeps the network replaceable in tests.

Negative:

- We own a minimal HTTP/1.1 parser, a URL parser, a TLS connector and the
  UDP retransmission logic. Each needs fuzzing and maintenance.
- HTTPS is the dominant size cost: about +968 KiB over the no-TLS build. The
  daemon grows to roughly 1.46 MB.
- No scrape, proxy, dual-family announce, or IPv6 peer connections in v0.1.
- No proxy at all in v0.1: a user who can reach trackers only through a proxy
  cannot announce. Proxy support is roadmap v0.3.
- Private trackers that whitelist client IDs may refuse the placeholder `BA`
  code. This is a product limitation, not something we work around by
  impersonating a listed client.
- The webpki-roots license needs an explicit `deny.toml` change; otherwise
  the root store is OS-dependent.

Risks and mitigations:

- HTTP parser bugs: restrict to the subset trackers use, cap every buffer,
  fuzz, and reject ambiguous framing (both length and transfer coding).
- DNS rebinding: check the resolved address and connect to that address, not
  the name; re-check on every redirect.
- Tracker outage: exponential backoff with jitter, and tier backups for
  public torrents.
- Log leaks: redacted `Debug`/`Display`, sanitized tracker messages, and a
  log-capture test.
- Root store drift: prefer compiled-in roots; document the fallback.

## 13. Alternatives considered

- **rbit tracker.** Rejected on size (2.10 MB), 166 crates, and the confirmed
  functional gaps in section 1.
- **hyper or reqwest.** Not measured. Both bring an HTTP stack larger than the
  subset we need, and still need a TLS choice and the same policy layer.
  Revisit only if the in-tree parser becomes a maintenance burden.
- **aws-lc-rs.** Rejected: 751,928 bytes more than ring and a C toolchain on
  every release platform.
- **System TLS (native-tls, Schannel, Security Framework).** Not measured.
  Rejected for v0.1 because behaviour and root handling differ per OS, tests
  need three code paths, and the dependency set is larger. This is the lever
  to revisit if 1.46 MB is unacceptable.
- **TLS 1.3 only.** Rejected: 44,744-byte saving does not justify dropping
  TLS 1.2 trackers.
- **Build-time `https` feature.** Adopted, not rejected: TLS is compiled in
  for the default app build and left out with `--no-default-features`, which
  keeps a roughly 490 KB HTTP+UDP binary. The cost is a feature matrix; the
  benefit is that the minimal build does not pay about 1 MB for TLS it cannot
  use. The feature is compile-time only, with no runtime toggle.
- **`url` crate.** Rejected for v0.1: IDNA and Unicode tables for a grammar
  that rejects non-ASCII hosts. The parser is fuzzed and the grammar is
  small. Revisit if the parser needs to grow.

## 14. Prerequisites and open questions

1. A `deny.toml`-only prerequisite PR adds `CDLA-Permissive-2.0` for
   webpki-roots 1.0 and verifies that ring's `Apache-2.0 AND ISC` passes
   unchanged, adding a ring license clarification only if the scanner needs
   one. If the license is denied, use `rustls-native-certs`.
2. Owner approval for the new dependencies: `getrandom`, `rustls`,
   `tokio-rustls`, `webpki-roots` (or `rustls-native-certs`), and the
   dev-dependency `rcgen` for TLS tests. `ring` arrives through rustls.
   The tracker also needs tokio features `net`, `time` and `io-util`, plus
   the dev-only `test-util` feature for paused-time tests; tokio itself is
   already a dependency, so these are feature additions, not new crates.
3. Owner decision on whether `allow_private_targets` is exposed in the v0.1
   daemon config. The default is `false`; tests and self-hosted setups set it
   through the engine API.
4. The ROADMAP line says "HTTP, then UDP". This ADR implements UDP first
   because it is smaller and its codec is easier to test in isolation. The
   roadmap line is updated when the implementation lands, not in this
   proposed ADR.
5. BEP_STATUS rows for 7, 12, 15, 20, 23 and 27 stay `planned` until code
   lands; the notes and this ADR do not change implementation status.

## 15. Implementation order

The order follows the measured risk and size: pure decoders first, UDP, the
state machine, then HTTP and HTTPS. Each step is independently testable and
leaves the workspace green.

| Step | Branch | Content | New dependencies |
|---|---|---|---|
| 1 | `feat/tracker-url-decode` | URL parser, `TrackerUrl`, redaction, BEP 23/7 peer decoding, response envelope, fuzz targets | none |
| 2 | `feat/tracker-udp` | BEP 15 codec, UDP transport, retransmission, local fake tracker | `getrandom`; tokio `net`, `time`, `test-util` (dev) |
| 3 | `feat/tracker-state` | state machine, BEP 12 tiers, BEP 27 private policy, backoff, fake transport | none |
| 4 | `feat/tracker-http` | HTTP/1.1 client, redirects, target policy, TLS connector, HTTPS tests | `rustls`, `tokio-rustls`, `ring`, roots, `rcgen` (dev), tokio `io-util` |
| 5 | `docs/tracker-status` | BEP_STATUS, ROADMAP, ARCHITECTURE updates | none |

The first branch is pure and adds no dependencies: it can be reviewed and
merged before any network code exists. Its commits are:

1. `feat(tracker): add tracker URL parser` - grammar, normalization,
   `TrackerUrl`, unit tests, `tracker_url` fuzz target.
2. `feat(tracker): redact tracker URLs in diagnostics` - `Debug`/`Display`,
   `TrackerError` redaction, log-capture tests.
3. `feat(tracker): decode compact and list peers` - BEP 23, BEP 7 `peers6`,
   caps, dedup, address filtering, unit tests.
4. `feat(tracker): decode tracker response envelope` - `failure reason`,
   interval, counters, `tracker id`, warning sanitization, bounded bencode,
   `tracker_http_response` fuzz target.

Exit criteria for branch 1: `cargo fmt --all --check`,
`cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace`, fuzz targets build, and no new dependency in any
`Cargo.toml`. Branch 2 starts only after branch 1 is merged.

## 16. References

- `docs/beps/bep_0015.md`, `bep_0012.md`, `bep_0023.md`, `bep_0007.md`,
  `bep_0020.md`, `bep_0027.md`
- `docs/adr/0001-use-rbit.md`, `docs/RBIT_REPORT.md`
- `docs/adr/0002-storage-design.md` for the error and worker conventions
- `docs/ROADMAP.md` item 4, `docs/BEP_STATUS.md`
- `deny.toml`, `fuzz/Cargo.toml`
