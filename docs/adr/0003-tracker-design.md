# ADR 0003: Tracker announce design

- Status: Accepted
- Date: 2026-11-25
- Deciders: project owner
- Supersedes: none
- Related: `docs/ROADMAP.md` item 4, ADR 0001, `docs/BEP_STATUS.md`,
  `docs/beps/bep_0015.md`, `bep_0003_tracker.md`, `bep_0012.md`,
  `bep_0023.md`, `bep_0007.md`, `bep_0020.md`, `bep_0027.md`, `deny.toml`,
  `fuzz/Cargo.toml`

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
  rejected. Up to 5 redirects (301, 302, 303, 307, 308): public torrents may
  cross hosts, private torrents use `RedirectPolicy::SameHostOnly`; every
  target is re-checked; 30 s per hop, 60 s total.
- Query: `info_hash` and `peer_id` are percent-encoded byte by byte; an
  existing query is extended with `&`, never a second `?`; our values replace
  duplicates already in the URL.
- UDP: BEP 15 connect then announce; one 105 s deadline from the first
  datagram, sends due at 0/15/45 s with waits 15/30/60; every datagram counts,
  and a connect is sent before any send when the ID is at least 50 s old. The
  cap is our decision, not the full `15 * 2^n` schedule. 200 peers, 6-byte
  IPv4 and 18-byte IPv6 strides.
- Peers: compact and list formats plus `peers6`; 200-peer cap; invalid
  addresses dropped; tracker-supplied hostnames dropped (SSRF).
- SSRF: public targets only by default; `allow_private_targets` is for tests
  and self-hosting. DNS results are checked and the connection goes to the
  checked IP; SNI is sent only for DNS hostnames, and certificate verification
  uses the hostname or the IP SAN. Every redirect is re-checked.
- Redaction: `Debug`/`Display` show only `scheme://host[:port]/…`; path,
  query and fragment stay hidden. Logs and errors never contain
  tracker-supplied text: they keep only a length and a classification. The UI
  may show `failure reason` and `warning message` after best-effort
  sanitization (controls stripped, 256-byte cap, URL tokens of at least 4
  bytes replaced); `tracker id` is suppressed.
- Tiers: BEP 12 for public torrents (shuffle once, try all trackers in a tier,
  move a success to the front; 32 URLs, 8 tiers, 4 per tier; every round
  starts at tier 0). `announce-list` is used when it has at least one valid
  URL, otherwise `announce`; this is a documented deviation from BEP 12.
  Private torrents use one active tracker and switch only on failure (BEP 27).
- State: `started`, `completed`, `stopped`; interval clamped to 60..86400 s;
  saturating exponential backoff with a capped exponent and jitter; one
  in-flight announce per torrent.
- Boundary: engine-internal `TrackerTransport` with boxed futures; `bt-client`
  defines its own `TrackerStatus` and maps from the engine snapshot. A
  versioned `bt-proto` type is planned for v0.2.
- Tests: unit vectors, local HTTP/UDP/TLS trackers, paused-time UDP tests
  (including the 51 s reconnect and the 105 s deadline), log-capture tests, and
  fuzz targets `tracker_url`, `tracker_http_response`,
  `tracker_udp_response`.
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
part of that decision; DHT remains a separate item 9 decision.

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
5. Public torrents use the BEP 12 tier rules, with `announce` as a fallback
   when `announce-list` has no URL that passes validation (a documented
   deviation from BEP 12). Private torrents use one active tracker and switch
   only on failure, per BEP 27.
6. Announce responses are decoded from both compact and list peer formats
   (BEP 23) and from `peers6` (BEP 7). UDP is compact by construction.
7. The tracker exposes a small engine-internal state machine and an
   object-safe `TrackerTransport` trait with boxed futures, so tests can
   replace the network with a scripted fake.
8. Identity uses placeholder constants: peer ID `-BA0100-` plus 12 random
   bytes, and user agent `bt-app/0.1`. `BA` appears in neither BEP 20's
   known-client table nor its unidentified list, and is not another client's
   code. We do not impersonate another client; a private tracker that
   whitelists client IDs may refuse this one.

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

- The tracker logic without TLS is cheap: +54 KB for UDP, +71 KB for HTTP,
  +90 KB for both. This is the part we would write either way.
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
globally: DHT at item 9 may still evaluate rbit, and that decision should use
its own measurements.

## 2. Scope per release

### v0.1

- HTTP/1.1 announce over TCP, HTTPS through rustls in the default app build,
  UDP announce per BEP 15. A `--no-default-features` build keeps HTTP and UDP
  and rejects `https` URLs.
- Public torrents with BEP 12 tiers and private torrents with BEP 27 rules.
- The private flag and its tracker rules (one active tracker, same-host
  redirect, peer drop on switch). DHT (item 9), PEX, LSD and web seeds must
  enforce the flag when they land.
- BEP 23 compact and list peers, BEP 7 `peers6` and `key`.
- Events `started`, `completed`, `stopped`; periodic announces; exponential
  backoff after a failed round.
- Response fields: `interval`, `min interval`, `complete`, `incomplete`,
  `peers`, `peers6`, `tracker id`, `failure reason`, `warning message`; the
  HTTP fields are defined in `docs/beps/bep_0003_tracker.md`.
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
- SNI is sent only for DNS hostnames, using the hostname with any trailing
  dot stripped. For an IP-literal URL no SNI is sent; `ServerName::IpAddress`
  is used for verification and the certificate must contain a matching IP SAN.
- No client certificates, no custom session resumption, no early data, and
  no revocation checking (no OCSP or CRL). Revocation is documented as out of
  scope; the root store and certificate lifetime are the control.
- The TLS handshake has a 10-second timeout inside the 30-second per-hop
  budget. A handshake failure is `TrackerError::Tls`; the attempt fails and
  §8 applies the tier and backoff rules.

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
dependency update. The weekly `cargo deny` pass and a periodic `cargo update`
serve as the reminder; there is no runtime root refresh.

Fallback: `rustls-native-certs` 0.8, which loads the OS store. It is 31,928
bytes smaller, is license-clean under the current policy, and follows
enterprise or user-installed roots. The cost is OS-dependent behavior and
tests that cannot assume a fixed root set. If the license approval is denied,
the fallback is the decision, not a blocker.

Tests use a `pub(crate)` constructor on the TLS connector that takes extra
`CertificateDer` roots. It is compiled only for tests (or behind the existing
dev-only `test-util` feature) and is not reachable from a release build.
Production constructs the connector from `webpki-roots` alone; this is how the
`rcgen` test CA is trusted without weakening production verification.

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
  unreserved set `A-Z a-z 0-9 - . _ ~` is escaped, so `0x41` stays `A` and
  `0x7e` stays `~`; `0x00` becomes `%00`. A space is `%20`, never `+`; a
  literal `+` is `%2B` and a literal `%` is `%25`. `port`, counters,
  `numwant` and `key` are decimal.
- An existing query in the tracker URL is preserved and extended with `&`,
  never a second `?`: `?passkey=abc` becomes `?passkey=abc&info_hash=...`.
  A URL ending in `?` gets the first pair directly after that `?`. Existing
  `info_hash`, `peer_id`, `port`, `uploaded`, `downloaded`, `left`, `compact`,
  `event`, `numwant`, `key`, `trackerid`, `ip`, `ipv4` and `ipv6` parameters
  are removed first, so our values always win.
  Names are matched after percent-decoding and case-insensitively, so
  `%69nfo_hash` and `info%5Fhash` are removed too. Only names are decoded for
  matching; values and the remaining query are kept as raw bytes and are not
  re-encoded.
- `numwant` is 50 for a normal announce and 0 for `stopped`.
- `event` is omitted for a periodic announce, and is `started`, `completed`
  or `stopped` otherwise.
- The URL is rebuilt from parsed components, not string-concatenated.

### Response

- Status 200 is the only success. 301, 302, 303, 307 and 308 are redirects;
  every other 3xx (including 300, 304, 305 and 306) is a terminal
  `TrackerError::HttpStatus`. Any other status is also `HttpStatus`, with no
  body retained. Only the numeric status is kept; the reason phrase is
  discarded.
- The status line must be HTTP/1.0 or HTTP/1.1; any other version is a
  malformed response. HTTP/2 is not negotiated.
- The response head is capped at 64 KiB, 100 header lines, and 8 KiB per
  line. Header names are ASCII and case-insensitive; obs-fold, NUL, CR and LF
  inside a value are rejected. Header values are not retained except
  `Location`; they never reach a log, error or UI.
- `Content-Length` must parse as a decimal `usize`; duplicate values must be
  identical. `Transfer-Encoding` must be exactly `chunked`. A response with
  both `Content-Length` and `Transfer-Encoding` is rejected, as is a
  non-chunked transfer coding.
- No length and no chunking means read to EOF, capped at 1 MiB. A chunked body
  is decoded incrementally with the same cap; the chunk-size line is capped at
  1 KiB and trailers at 8 KiB. Chunk extensions are ignored up to the 1 KiB
  line cap; an invalid extension is a malformed response.
- The body is capped at 1 MiB before bencode decoding. The existing bounded
  bencode decoder is used with its depth limit; no length prefix can allocate
  beyond the body.
- The body must be a bencode dictionary; keys may arrive in any order and
  duplicates are rejected. `failure reason` is checked first and
  produces `TrackerError::Rejected { message: TrackerMessage }`. `warning
  message` is stored as a `TrackerMessage`. `interval` and `min interval` are
  integers in seconds; missing `interval` means 1800. The effective interval
  is `clamp(max(interval, min_interval), 60, 86400)`. `complete` and
  `incomplete` are optional non-negative counters. `tracker id` is an optional
  `TrackerId`; a value longer than 256 bytes is ignored, not truncated, and
  the ID is kept per tracker for HTTP announces.
- `TrackerMessage` stores the tracker bytes and exposes
  `sanitized_text(&TrackerUrl)` for the UI. Sanitization is best-effort:
  UTF-8 lossy, control characters stripped, 256-byte cap, and every path
  segment, query name, query value and fragment of the announce URL of at
  least 4 bytes replaced with `<redacted>` (section 9). `Debug` and `Display`
  print only the length; logs and errors record only a length and a
  classification, never the text.
- `TrackerId` is opaque: it is echoed only to the tracker that issued it, is
  never displayed, and its `Debug` and `Display` forms print only its length.
- Unknown keys are ignored. `peers` and `peers6` are decoded per the BEP 23
  and BEP 7 notes.
- The `Content-Type` header is ignored; trackers frequently send
  `text/plain`. Bencode validity is the only test. A `Content-Encoding` other
  than `identity` is a malformed response, because we request `identity`.

### Redirects

- 301, 302, 303, 307 and 308 are followed, up to 5 hops. Any other 3xx is
  terminal (see Response). A response with more than one `Location` header is
  rejected; the first is not chosen silently.
- `Location` may be an absolute `http`/`https` URL or an absolute path on the
  same host. Other relative forms are rejected. The raw value is capped at
  2048 bytes.
- Every `Location` value is resolved against the current URL, then parsed into
  a `TrackerUrl` before use, so it is redacted exactly like the original URL
  in `Debug`, `Display`, errors and logs. A malformed `Location` produces the
  fixed `<invalid tracker url>` placeholder, never the raw header value.
- An `https` to `http` downgrade is rejected. `http` to `https` is allowed.
  Any other scheme is rejected.
- `AnnounceRequest` carries `RedirectPolicy::{Public, SameHostOnly}`.
  `Public` allows a cross-host target after the normal checks. `SameHostOnly`
  requires the same host and effective port (defaults 80 and 443), allows an
  `http` to `https` upgrade only to the default https port, and rejects a
  downgrade. Hosts are compared with a trailing dot normalized. A cross-host
  `SameHostOnly` target fails before a second request and is never added to
  the candidate list.
- Every hop is parsed, resolved, policy-checked and connected from scratch.
  No connection, DNS result or TLS session is reused across hops.
- Userinfo in a redirect target is rejected, not silently stripped.
- The total HTTP announce budget is 60 seconds across all hops; each hop has
  30 seconds for DNS, connect, TLS, request and response.

### Timeouts

| Phase | Budget |
|---|---|
| DNS resolve | 5 seconds |
| Connect | 10 seconds, inside the hop budget |
| TLS handshake | 10 seconds, inside the hop budget |
| One hop (DNS + connect + TLS + request + response) | 30 seconds |
| Whole HTTP announce including redirects | 60 seconds |
| DNS permit wait | 2 seconds, then `Dns` |
| Stopped announce | 2 seconds total, no retries |

These are transport deadlines. The state machine has no outer announce
timeout; each transport owns its own budget, so the 60-second HTTP budget
never cuts a UDP attempt and the 105-second UDP deadline never cuts HTTP.

## 5. UDP behaviour

The wire format and retransmission rules are in `docs/beps/bep_0015.md`.
Summary of the decisions:

- One connected UDP socket per announce attempt, bound to the address family
  of the chosen tracker address. The kernel drops datagrams from other
  sources.
- Connect request first. The connection ID is reused within the attempt. There
  is no cross-announce connection-ID cache in v0.1; the next announce
  reconnects.
- One 105-second deadline per attempt, measured from the first datagram.
  Waits are 15, 30 and 60 seconds, so sends are due at 0, 15 and 45 seconds.
  Every datagram, connect or announce, counts as one of the three sends. A
  reconnect replaces the announce for that slot and does not reset the
  deadline.
- A `stopped` announce is the exception: it uses a 2-second deadline and no
  retries, on UDP as on HTTP, instead of the 105-second normal-attempt
  deadline.
- Before every send, if `now - id_received >= 50 s`, send a connect instead of
  the announce. 50 s is our decision; BEP 15 allows use until 60 s after
  receipt, and a tracker silently ignores an expired ID. The new ID is used
  once the connect response arrives.
- `num_want` is 50 for a normal announce and 0 for `stopped`, matching the
  HTTP `numwant` rule.
- Receive buffer 4096 bytes; 200 peers per response; 6-byte stride on IPv4
  and 18-byte stride on IPv6, selected by the socket family.
- Transaction IDs are 32 bits from OS randomness, fresh for each connect and
  announce. A mismatched action or transaction ID is ignored and the receive
  wait continues.
- Error packets are checked for transaction ID. The text is untrusted: it is
  passed through the same best-effort sanitizer as an HTTP `failure reason`
  before it may be shown in the UI, and it is never logged. Logs and errors
  record only a length and a classification. The attempt fails with
  `Rejected`.
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
| `2001::/32` | Teredo |
| `64:ff9b:1::/48` | local-use NAT64 |
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
native IPv6 trackers but not IPv4 trackers through the well-known or local-use
prefix. A
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
- For HTTPS the TCP connection still goes to the checked IP, but certificate
  verification uses the original hostname: the TLS `ServerName` is the URL
  host, not the IP. SNI is sent only when the host is a DNS name, using the
  hostname with any trailing dot stripped. For an IP-literal URL no SNI is
  sent; `ServerName::IpAddress` is used and the certificate must contain a
  matching IP SAN. Connecting to the IP without this would verify the wrong
  name.
- Resolution runs the system resolver on tokio's blocking pool through
  `spawn_blocking` with `ToSocketAddrs`, not `lookup_host`, so the semaphore
  permit can move into the blocking closure. A process-wide semaphore of 8
  permits bounds concurrent lookups. The permit is acquired before the call
  and released only when the blocking call returns, even if the announce
  future was dropped; a hung resolver can therefore hold at most 8 blocking
  threads. Waiting for a permit has a 2-second queue timeout, after which the
  announce fails with `Dns`. The 5-second lookup timeout starts after the
  permit is acquired. Dropping the future does not cancel the blocking call.
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
- `trackerid` is stored as a `TrackerId`, sent only to the HTTP tracker that
  returned it, and is dropped when the active tracker changes.
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
- `tracker_id: Option<TrackerId>`;
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
  `next_announce = now + min(30 * 2^min(failures - 1, 6), 1800)` seconds,
  with ±20% jitter. The failure count uses saturating addition and the
  exponent is capped at 6, so `u32::MAX` failures still produce the
  1800-second cap and cannot overflow. The first retry is about 30 seconds,
  then 60, 120, 240, 480, 960, and 1800 at the cap. Jitter avoids a
  thundering herd after a tracker outage.
- The interval is clamped to 60..86400 seconds, and `min interval` can only
  raise it. An interval of 0 or a negative value is treated as missing and
  becomes 1800.

### Public torrents (BEP 12)

BEP 12 defines the `announce-list` tier format and says a client that
understands it ignores `announce` when the list is present. The ordering,
caps, shuffle, merge, tier-0 reset and backoff below are our policy; the
`announce` fallback is a documented deviation.

- `announce-list` is used when it contains at least one URL that passes
  `TrackerUrl` validation, even if other entries are unusable. If it contains
  no usable URL, the single `announce` URL is used as a one-tracker list if
  it passes the same validation; otherwise there is no usable tracker.
  `announce` is never merged into a usable `announce-list`. This deviates from
  BEP 12's "ignore `announce`" rule on purpose: the BEP assumes a well-formed
  list, and the fallback keeps a torrent with a damaged list usable. The BEP 12
  note records the same rule.
- One shuffle at load with OS randomness; the order is stable for the
  session. Tests inject a fixed order.
- Every announce round starts at tier 0.
- Within a tier, try trackers sequentially in order, up to 4 per tier. Every
  tracker in the tier is attempted even after one succeeds; the peer lists of
  all successes in that tier are merged, deduplicated, and capped at 200 new
  peers per round.
- Stop after the first tier with at least one success. Later tiers are
  backups and are not contacted in that round.
- Move the first successful tracker in the tier to the front, keeping the
  relative order of the rest. Failed trackers stay in place and are retried
  next round. A success changes nothing else in the candidate order: the rest
  of the order and the active tracker are unchanged.
- If every tracker in every tier fails, the round fails and the backoff
  applies. The next round starts at tier 0.
- Caps: 32 URLs, 8 tiers, 4 per tier. Duplicate normalized URLs are removed;
  invalid schemes, empty hosts and bad ports are dropped with counters. The
  caps are applied at parse time, before any private flattening.
- If no usable URL remains, the torrent has no tracker and the session gets
  `TrackerError::NoUsableTracker`; no request is made.

### Private torrents (BEP 27)

BEP 27 requires the `private=1` flag, that only the private tracker is used,
that one tracker is active at a time and switched only on failure, and that
peer connections are dropped on a switch. It also says DHT, PEX and LSD would
subvert that access control. The ordering, redirect rule, web-seed rule and
backoff below are our policy.

- Flatten the tiers into one ordered list, shuffle once, and keep exactly one
  active tracker. The 32-URL, 8-tier and 4-per-tier caps are applied at parse
  time before flattening, so the flattened list holds at most 32 URLs.
- The active tracker is used until it fails. On failure, the next candidate
  becomes active immediately; only after every candidate has failed does the
  round back off.
- Before the first announce to a new tracker, drop every peer connection and
  every peer candidate learned from the old tracker. Downloaded pieces and
  verified progress are kept.
- v0.1 delivers the private flag and the tracker rules above. DHT (item 9),
  PEX, LSD and web seeds are separate components; when each lands, it must
  consult the session-level guard and refuse to run for a private torrent. The
  guard is enforced at the session level, not only by not starting the
  components, so a future component cannot opt in by accident.
- Incoming connections are still accepted; BEP 27 restricts outgoing peer
  discovery, not listening.
- A redirect is followed only to the same host and the same effective port
  (defaults 80 and 443), with a trailing dot normalized; an `http` to `https`
  upgrade is allowed only to the default https port, and a downgrade is
  rejected. `RedirectPolicy::SameHostOnly` is carried in `AnnounceRequest` and
  enforced by the transport. A cross-host redirect fails before a second
  request; the target is never added to the candidate list.
- A private torrent with no usable tracker fails with
  `TrackerError::NoUsableTracker` without contacting anything.

## 9. URL parsing and redaction

### Parser

A strict hand-written parser covers the tracker URL grammar:
`scheme://host[:port][/path][?query][#fragment]`, with `http`, `https` and
`udp` schemes. It is deliberately smaller than a general URL crate and avoids
IDNA and Unicode tables we do not need. It is a network-input parser and gets
a fuzz target.

Rules:

- The URL is ASCII only. Non-ASCII bytes, spaces, ASCII controls and
  backslashes are rejected anywhere, not only in the host.
- scheme and host are lowercased; the default port is omitted from the
  normalized URL for `http` (80) and `https` (443). A `udp` URL must carry an
  explicit port. The parsed form keeps the effective port; a port equal to the
  scheme default is omitted when the URL is rendered, so redaction shows only a
  non-default port.
- userinfo (`user:pass@`) is rejected, not stripped.
- A host is one of three things: a canonical IPv4 literal, a bracketed IPv6
  literal, or a validated DNS name. Every numeric-looking host is classified
  before any resolver sees it; it is never passed to DNS as a name.
- IPv4 literals are exactly four decimal parts, 1-3 digits each, no leading
  zero unless the part is `0`, each at most 255. A host made only of digits
  and dots is accepted only in that canonical form. Any host that matches the
  `inet_aton` numeric grammar (decimal, octal with a leading `0`, hex with
  `0x`, or a 1-3 part shorthand) but is not canonical dotted-quad is
  rejected. This closes the `127.1`, `2130706433`, `0177.0.0.1`,
  `0x7f.0.0.1` and `127.0.0.1.` bypasses.
- IPv6 literals are bracketed and parsed with `std::net::Ipv6Addr`; zone IDs
  (`%eth0`) are rejected. Unbracketed IPv6, bracketed IPv4 and empty brackets
  are rejected.
- IPv4-mapped IPv6 (`::ffff:a.b.c.d`) and the transition forms in section 6
  parse as IPv6. `Host::canonical_ip()` returns the embedded IPv4 for a
  mapped address and the IPv6 address otherwise, so the section 6 policy has
  one place to check; the policy still blocks the mapped range itself.
- DNS names are ASCII LDH: labels of 1-63 bytes, 253 bytes total, no empty
  labels, no leading or trailing hyphen, no underscore. One trailing dot is
  allowed and stripped; a trailing dot on an IPv4 literal is rejected.
- `%` in the host is rejected. In the path, query and fragment every `%` must
  be followed by two hex digits; a bare or invalid escape is rejected.
- the path defaults to `/`; query and fragment are preserved byte for byte
  after validation. `+` is a literal plus, `%20` is a space, and mixed-case
  hex in an existing escape is not rewritten.
- the URL is capped at 2048 bytes; longer URLs are invalid.
- a URL with a fragment keeps it in the parsed form but never sends it; the
  fragment is not part of the HTTP request target.

### Redaction

`TrackerUrl` stores the parsed components only: scheme, host, effective port,
path, query and fragment. It does not store the original string. `Debug` and
`Display` render the redacted form; the path and query are available to the
transport as the same bytes that were parsed, and the request target is
rebuilt from those components. `PartialEq` and `Hash` use the same normalized
tuple `(scheme, host, port, path, query, fragment)`, never the original
spelling, so equal URLs hash equally.

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
- Tracker `failure reason` and `warning message` are stored as
  `TrackerMessage`. Sanitization is best-effort: UTF-8 lossy conversion,
  control characters stripped, 256-byte cap, and the announce URL tokenized
  into path segments, query names, query values and fragment. Every token of
  at least 4 bytes is replaced with `<redacted>`, case-insensitively and after
  percent-decoding. Tokens shorter than 4 bytes are not replaced; this is a
  documented limit. The sanitized text may be shown in the UI through
  `sanitized_text()`; logs and errors record only a length and a
  classification.
- Sanitization is bounded: the URL is at most 2048 bytes and the message at
  most 256 bytes, so the token scan is O(URL tokens x 256) and no input can
  make it unbounded. The implementation keeps that bound; a test uses the
  longest accepted URL and a 256-byte message.
- `TrackerId` is a newtype over the opaque bytes. It is never displayed, is
  ignored if longer than 256 bytes, is sent back only to the tracker that
  issued it, and its `Debug` and `Display` forms print only its length. It is
  percent-encoded byte by byte when echoed.
- `TrackerMessage` `Debug` and `Display` also print only the length; the UI
  must call `sanitized_text()` explicitly. This keeps a stray `?error` or
  `?response` in a log from printing tracker text.
- A log or error "classification" is a fixed label such as `rejected`,
  `warning` or `malformed`; it is never derived from tracker text.
- Tests cover a passkey in the query, a passkey in the path, userinfo, IPv6,
  default and non-default ports, uppercase scheme and host, a fragment,
  malformed inputs, and a 4096-byte URL. Each test asserts that the secret
  substring is absent from `Debug`, `Display`, and every error string. Further
  tests cover a `Location` with a passkey, a `failure reason` that echoes the
  URL, a `warning message` with control characters, a `tracker id` that
  contains a passkey, a path segment of 3 and 4 bytes, a fragment passkey, a
  percent-encoded echo, a passkey in the HTTP reason phrase, and a UDP error
  packet that echoes the URL: none of the raw bytes may appear in a log line,
  an error, or the status snapshot. The `Debug` and `Display` of
  `AnnounceRequest`, `AnnounceResponse`, `TrackerId` and `TrackerMessage` are
  formatted in each test.

## 10. Trait boundary and types

The tracker is engine-internal; `bt-proto` is not extended in v0.1. `bt-client`
defines its own `TrackerStatus` and maps from the engine snapshot, so the TUI
and GUI do not depend on engine types. A versioned `bt-proto` type is planned
for v0.2.

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
  socket. There is no outer announce timeout: each transport owns its own
  deadlines, so a UDP attempt is not cut at 60 seconds and a `stopped`
  announce uses its 2-second budget.
- The production implementation is `NetworkTransport`, which dispatches on
  the URL scheme to the HTTP and UDP transports. Tests inject a
  `FakeTransport` that records requests and returns scripted responses,
  errors or pending futures.

Types (engine-internal, `crates/engine/src/tracker`):

- `AnnounceRequest { url: TrackerUrl, info_hash: InfoHash, peer_id: PeerId,
  port: u16, stats: AnnounceStats, event: AnnounceEvent, num_want: i32,
  key: u32, tracker_id: Option<TrackerId>, redirect: RedirectPolicy }`
- `AnnounceStats { uploaded: u64, downloaded: u64, left: u64 }`
- `AnnounceEvent { None, Started, Completed, Stopped }`
- `RedirectPolicy { Public, SameHostOnly }`
- `AnnounceResponse { interval: Duration, min_interval: Option<Duration>,
  complete: Option<u64>, incomplete: Option<u64>, peers: Vec<SocketAddr>,
  tracker_id: Option<TrackerId>, warning: Option<TrackerMessage>,
  peer_warnings: PeerWarnings }`
- `PeerWarnings { malformed_peer_records: u32, dropped_peer_records: u32,
  truncated_peers: u32, missing_peers: bool }`: counters for recoverable
  peer-list problems. They contain no tracker bytes and are safe to log.
- `TrackerId`: opaque bytes, ignored if longer than 256; `Debug` and `Display`
  print only the length.
- `TrackerMessage`: tracker-supplied bytes plus `sanitized_text(&TrackerUrl)`
  for the UI; `Debug` and `Display` print only the length.
- `TrackerError`: `InvalidUrl`, `UnsupportedScheme`, `Dns`, `BlockedTarget`,
  `Connect`, `Tls`, `Timeout`, `HttpStatus(u16)`, `MalformedResponse`,
  `Rejected { message: TrackerMessage }`, `TooLarge`, `Redirect`,
  `Cancelled`, `NoUsableTracker`, `Io`.
- `TrackerEvent`: `AnnounceSucceeded`, `AnnounceFailed`, `PeersReceived`,
  `Warning`, `TrackerSwitched`, `NoUsableTracker`.
- `TrackerStatus`: redacted active URL, last success, next announce, last
  interval, consecutive failures, peer count, last warning flag.

`thiserror` derives the error type; `Display` never contains a raw URL or
tracker-supplied text. All parsing returns `Result`, no `unwrap` or `expect`
outside tests.

## 11. Testing and fuzzing

### Unit

- URL parser and redaction tables, including malformed inputs. Tests format
  `Debug` and `Display` of `TrackerUrl`, `AnnounceRequest`, `AnnounceResponse`,
  `TrackerId` and `TrackerMessage`, and cover a path segment of 3 and 4 bytes,
  a fragment passkey, and a percent-encoded echo.
- URL IP-literal vectors: canonical dotted-quad accepted; `127.1`,
  `2130706433`, `0177.0.0.1`, `0x7f.0.0.1`, `0x7f000001`, `1.2.3.4.5`,
  `1.2.3.256` and `127.0.0.1.` rejected. `[::1]:80` and `[::1]` are accepted
  with the default port; `[::1]:0`, `[::1]:65536`, `[::1`, `::1]` and
  `[::1]x` are rejected. `[::ffff:127.0.0.1]` maps to the embedded IPv4
  through `Host::canonical_ip()`.
- URL round-trip vectors: `+`, `%20` and mixed-case hex in the path and query
  survive unchanged; equal URLs with different original spellings compare
  equal and hash equally. The sanitizer test uses the longest accepted URL
  and a 256-byte message.
- BEP 23 and BEP 7 peer decoding vectors, partial records, invalid
  addresses, dedup and caps.
- HTTP head/body parser vectors: Content-Length, chunked (including a chunk
  split across reads and a trailer section), close-delimited, HTTP/1.0 with
  and without a length, duplicate length, both length and transfer coding,
  a non-chunked transfer coding, an oversized head and body, an oversized
  chunk-size line, invalid UTF-8 in a header value, obs-fold, and a
  `Content-Encoding: gzip` response. The chunked decoder is exercised with a
  body just under and just over the 1 MiB cap.
- HTTP status vectors: 300, 304, 305 and 306 are terminal errors; duplicate
  `Location` is rejected; chunk extensions are ignored up to the 1 KiB line
  cap; a non-UTF-8 header value is discarded except for `Location`.
- HTTP request builder vectors: method and target, `Host` with and without a
  non-default port, IPv6 bracket form, `User-Agent: bt-app/0.1`,
  `Accept-Encoding: identity`, `Connection: close`, and no body. A URL with
  `?passkey=abc` produces `...?passkey=abc&info_hash=...` and exactly one
  `?`; a URL ending in `?` appends the first pair directly. An existing
  `info_hash`, `peer_id`, `port`, `uploaded`, `downloaded`, `left`, `compact`,
  `event`, `numwant`, `key`, `trackerid`, `ip`, `ipv4` or `ipv6` parameter is
  removed. Names are matched after percent-decoding, so `%69nfo_hash` and
  `info%5Fhash` are removed too; a URL with `?left=0&trackerid=abc` loses
  both. A hash containing `0x00`, `0x20`, `0x25`, `0x2B`, `0x41`, `0x7E` and
  `0xFF` proves byte-wise percent-encoding: `%00`, `%20`, `%25`, `%2B`, `A`,
  `~`, `%FF`.
- Redirect vectors: absolute URL, same-host absolute path, cross-host target,
  `https` to `http` downgrade, a `Location` with a passkey (redacted in every
  diagnostic), a malformed `Location`, and more than 5 hops.
- Redirect policy vectors: `Public` accepts a cross-host target;
  `SameHostOnly` rejects it before a second request, compares effective ports
  (80 and 443 defaults), normalizes a trailing dot, and rejects a downgrade.
- UDP connect/announce/error vectors, wrong transaction ID and action,
  truncated packets, 6- and 18-byte strides.
- Paused-clock UDP tests: a connection ID 51 seconds old triggers a connect
  before the announce; every datagram counts as a send; a reconnect does not
  reset the 105-second deadline; an attempt is not cut at 60 seconds.
- Policy table with the first and last address of every range, plus
  IPv4-mapped and zone-ID cases.
- State machine with a fake clock and fake transport: interval clamp, event
  coalescing, saturating backoff including `u32::MAX` failures, tier order,
  move-to-front, every round starting at tier 0 (including a round after a
  failed round that follows an earlier success in a later tier), the
  `announce` fallback when `announce-list` has no valid URL, private switch,
  stopped budget, no-usable-tracker.

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
  no release size impact), trusted through the test-only extra-roots
  constructor: success against a certificate for the hostname while the socket
  is reached through `127.0.0.1` (proves SNI and verification use the
  hostname, not the IP), hostname mismatch, expired certificate, and a
  plaintext server on an `https` URL must all fail closed. Further vectors:
  an IP-literal URL with a matching IP SAN succeeds, the same URL without an
  IP SAN fails, a trailing-dot hostname verifies, and a CN-only certificate
  fails.
- DNS tests use an injected resolver: a resolve timeout with a paused clock,
  a mix of allowed and blocked results, a resolver that returns more than 8
  results, and a resolver that never returns, proving that the 8-permit
  semaphore bounds lookups and that the queue timeout fails with `Dns`. The
  real resolver path is covered by one localhost integration test, not by the
  deterministic suite.
- End-to-end: a metainfo with two tiers, a private torrent, a tracker switch,
  and a session loop that asserts peer churn and event order.
- Log capture: a failure reason, a warning message, a `Location`, a
  `tracker id`, a passkey in the HTTP reason phrase, and a UDP error packet
  that echoes the URL must not appear in any formatted log line, error, or
  status snapshot. Logs record only a length and a classification, never the
  tracker text.

### Fuzz

- `tracker_url`: arbitrary bytes to the URL parser and redactor; no panic,
  bounded output.
- `tracker_http_response`: arbitrary bytes to the head, body, chunked and
  bencode decoders, including `peers` and `peers6`; no panic, no allocation
  beyond the 1 MiB cap.
- `tracker_udp_response`: arbitrary bytes to the connect, announce and error
  decoders; no panic.
- The response targets also call `sanitized_text()` on every `failure reason`
  and `warning message`, so the sanitizer is fuzzed with the same input.
- Targets are registered in `fuzz/Cargo.toml`, compile in CI, and run weekly,
  matching the existing bencode and metainfo targets.

## 12. Consequences

Positive:

- About 1.46 MB for the full tracker versus 2.10 MB for rbit, with no
  166-crate graph.
- Exact control of SSRF, redirects, timeouts, caps, redaction and private
  torrent behaviour.
- The tracker logic without TLS is only +90 KB, so UDP and HTTP can land
  before the TLS work and stay testable.
- A small trait boundary keeps the network replaceable in tests.

Negative:

- We own a minimal HTTP/1.1 parser, a URL parser, a TLS connector and the
  UDP retransmission logic. Each needs fuzzing and maintenance.
- HTTPS is the dominant size cost: about +968 KB (968,160 bytes) over the
  no-TLS build. The daemon grows to roughly 1.46 MB.
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
- Log leaks: length/classification-only logs and errors, sanitized UI text,
  and a log-capture test.
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
4. BEP_STATUS rows for 7, 12, 15, 20, 23 and 27 stay `planned` until code
   lands; the notes and this ADR do not change implementation status.

## 15. Implementation order

The order follows the measured risk and size: pure decoders first, UDP, the
state machine, then HTTP and HTTPS. Each step is independently testable and
leaves the workspace green.

| Step | Branch | Content | New dependencies |
|---|---|---|---|
| 1 | `feat/tracker-url-decode` | URL parser, `TrackerUrl`, redaction, BEP 23/7 peer decoding, response envelope, fuzz targets | none |
| 2 | `feat/tracker-udp` | BEP 15 codec, UDP transport, target policy, 105 s deadline, reconnect, local fake tracker | `getrandom`; tokio `net`, `time`, `test-util` (dev) |
| 3 | `feat/tracker-state` | state machine, BEP 12 tiers, BEP 27 private policy, backoff, fake transport | none |
| 4 | `feat/tracker-http` | HTTP/1.1 client, redirects, `RedirectPolicy`, TLS connector and test roots, HTTPS tests | `rustls`, `tokio-rustls`, `ring`, roots, `rcgen` (dev), tokio `io-util` |
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
   interval, counters, `TrackerId`, `TrackerMessage`, warning sanitization,
   length-only diagnostics, bounded bencode, `tracker_http_response` fuzz
   target.

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
