# ADR 0001: Use rbit as the protocol building-block library

- Status: Proposed (dependency not added yet)
- Date: 2026-11-22
- Deciders: project owner
- Supersedes: none
- Related: `docs/RBIT_REPORT.md`, `docs/BEP_STATUS.md`

## Context

`AGENTS.md` states that the engine uses `rbit` for building blocks. The engine
must speak BitTorrent (BEP 3, 23, 15, 12, 9, 10, 5) for v0.1, and writing
bencode, metainfo, peer wire, trackers, and DHT from scratch is weeks of work
with a high chance of subtle protocol bugs.

A registry probe of `rbit` 0.2.2 was done on 2026-11-22. The findings and every
unverified claim are recorded in `docs/RBIT_REPORT.md`. Nothing from rbit has
been vendored into this repository, and `rbit` is not in any `Cargo.toml` yet.

## Decision

Adopt `rbit` as an **engine-internal** dependency for v0.1 protocol building
blocks, under the constraints below. Adding the dependency to `Cargo.toml`
requires explicit owner approval, and only after the "to verify" items in
`docs/RBIT_REPORT.md` have been checked against rbit's source and tests.

### Constraints

1. **Crate boundary.** Only `crates/engine` may depend on `rbit`. No rbit type
   may appear in a public signature of `bt-proto`, `bt-client`, `bt-tui`, or
   `bt-daemon`.
2. **Wrap, do not leak.** The engine converts rbit values into our own types and
   maps rbit errors into `EngineError` with stable codes from `error_code`.
3. **Untrusted input.** Every rbit parser we call is exercised at the engine
   boundary with malformed input, and gets a fuzz target where the input is
   network- or file-derived (bencode, metainfo, peer messages, tracker
   responses, DHT messages).
4. **Per-module adoption.** A module is adopted only when its BEP row in
   `docs/BEP_STATUS.md` moves from `verify` to `done` with a test. Presence of a
   module in rbit is not evidence that it is correct or complete.
5. **No all-or-nothing risk.** If a module turns out to be incomplete or
   unsuitable, implement that module in-tree and keep using the rest. The
   engine must not depend on rbit internals that make this impossible.
6. **Size and resource budget.** Measure dependency count and release binary
   size before tagging v0.1. Revisit this decision if rbit's dependency tree
   dominates the binary.
7. **MSRV.** rbit requires Rust 1.85; the workspace MSRV is 1.88, so no change
   is needed.

## Consequences

Positive:

- Protocol work for v0.1 is reduced to integration, validation, and tests.
- One dependency covers metainfo, peer wire, trackers, DHT, PEX, LSD, storage,
  cache, bandwidth, UPnP, and web seeds, which keeps the engine small.
- Licence (MIT OR Apache-2.0) matches the workspace.

Negative:

- Single maintainer and roughly 76% documented, so upstream support is a risk.
- No feature flags: we cannot compile out modules we do not use.
- Pulls `reqwest` with rustls and `tokio` with the `full` feature, which is a
  large tree for a client that wants a small binary.
- No uTP or MSE/PE module was found. Both are out of scope for v0.1, so this
  does not block v0.1, but v0.3 encryption and v0.6 uTP will need other work.
- The workspace currently depends on a deliberately small, approved set of
  crates; rbit expands that set considerably.

## Alternatives considered

- **Implement everything in-tree.** Maximum control and no extra dependency, but
  it delays v0.1 by weeks and adds protocol risk that tests alone will not
  remove quickly. Rejected for v0.1, kept as the fallback for individual
  modules under constraint 5.
- **Use another existing library** (for example the crates behind `librqbit`).
  Not evaluated; would need the same probe and report before it can be
  considered.
- **Fork or vendor rbit.** Only if we must patch behaviour and upstream is
  unresponsive. Not planned.

## Verification before adding the dependency

- [ ] Read rbit's bencode and metainfo parsers; confirm they reject malformed
      input without panicking.
- [ ] Confirm BEP 12 announce-list tier handling (order, fallback, `failure
      reason`).
- [ ] Confirm the tracker module covers both HTTP and UDP (BEP 15).
- [ ] Confirm peer wire support for BEP 10 extension messages and BEP 9
      metadata exchange.
- [ ] Confirm DHT (BEP 5) can be disabled per torrent for private torrents
      (BEP 27).
- [ ] Record the exact dependency tree and release binary size impact.
- [ ] Check rbit's MSRV and licence in `cargo deny` terms.

Until this checklist is complete, `docs/BEP_STATUS.md` keeps `verify` in the
rbit column.
