# rbit evaluation

Date: 2026-11-22
Subject: `rbit` 0.2.2 (published 2026-10-26)
Related: `docs/adr/0001-use-rbit.md`

## Scope

Answer two questions before the engine starts:

1. Does rbit cover the protocol work v0.1 needs (BEP 3, 23, 15, 12, 9, 10, 5)?
2. What does adopting it cost in dependencies, size, and maintenance risk?

## Method and limits

This is a **registry probe only**. The published crate was downloaded to a
temporary directory, and its metadata, `Cargo.toml`, module layout, and docs
coverage were inspected. The probe directory has since been removed.

What was **not** done:

- No rbit source was read line by line.
- No rbit tests were run.
- rbit was not compiled into this workspace.
- No malformed-input or fuzz testing of rbit parsers.

Anything that depends on behaviour rather than module presence is marked
**to verify** below and in the ADR checklist. No rbit code is in this
repository, and no `Cargo.toml` mentions rbit.

## Package facts (observed from the published crate)

| Field | Value |
|---|---|
| Version | 0.2.2 |
| Published | 2026-10-26 |
| Licence | MIT OR Apache-2.0 |
| MSRV | 1.85 |
| Feature flags | none |
| Documentation coverage | about 76% (docs.rs) |
| Maintainers | one |
| Notable dependencies | `tokio` with the `full` feature, `reqwest` with rustls |

Workspace MSRV is 1.88, so rbit's 1.85 requirement does not force a change.
The licence matches the workspace and should pass `deny.toml` as written.

## Modules observed

`bencode`, `metainfo`, `peer`, `tracker`, `dht`, `pex`, `lsd`, `storage`,
`cache`, `bandwidth`, `upnp`, `webseed`.

No module for uTP (BEP 29) or MSE/PE encryption was found.

## Coverage against v0.1

Module presence is not proof of correctness. "Observed" means a module with a
matching name exists; "to verify" means the behaviour still needs a code read
and a test.

| BEP | Need | rbit module | Status |
|---|---|---|---|
| 3 | Core peer wire | `peer` | observed, to verify |
| 23 | Compact peer lists | `tracker`, `peer` | to verify |
| 15 | UDP tracker | `tracker` | to verify (HTTP vs UDP split unknown) |
| 12 | Multi-tracker tiers | `metainfo`, `tracker` | announce-list mentioned; tier behaviour to verify |
| 9 | Magnet metadata exchange | `peer`, `metainfo` | to verify |
| 10 | Extension protocol | `peer` | to verify |
| 5 | DHT | `dht` | observed, to verify |
| 11 | PEX | `pex` | observed (v0.2) |
| 14 | LSD | `lsd` | observed (v0.2) |
| 19 | Web seeds | `webseed` | observed (v0.2) |
| 27 | Private torrents | `dht`, `pex`, `lsd` | to verify that they can be disabled |
| 29 | uTP | none | not provided (v0.6) |
| MSE/PE | Encryption | none | not provided (v0.3) |

v0.1 does not need BEP 11, 14, 19, 27, 29, or MSE/PE. The gaps that matter for
v0.1 are behavioural: tracker protocol coverage, extension messages, and DHT
disablement for private torrents.

## Risks

1. **Single maintainer.** Bus factor of one. A fork or in-tree replacement must
   stay possible (ADR constraint 5).
2. **No feature flags.** We cannot compile out DHT, UPnP, web seeds, or the
   cache. Everything rbit links is in our binary even if unused.
3. **Dependency weight.** `tokio` with `full` overrides the workspace's
   selective feature set, and `reqwest` with rustls is a large tree. This
   conflicts with the "small, low-resource" goal and must be measured before
   the v0.1 tag.
4. **Documentation gaps.** About 24% of public items are undocumented, so
   integration will need source reading.
5. **API churn.** 0.2.x is pre-1.0; minor releases may break us. Pin the exact
   version and upgrade deliberately.
6. **Unverified behaviour.** Nothing in the "to verify" column above should be
   trusted until read and tested.

## To verify before adoption

1. Bencode and metainfo parsers reject malformed input without panicking, and
   are safe to fuzz from our side.
2. BEP 12 tier handling: ordering, fallback on failure, and `failure reason`
   handling.
3. Tracker module supports HTTP announce and BEP 15 UDP announce.
4. Peer module supports BEP 10 extended handshake and BEP 9 `ut_metadata`.
5. DHT, PEX, and LSD can be disabled for private torrents (BEP 27).
6. Exact dependency tree, compile time, and release binary size delta.
7. Which rbit modules the engine can avoid at runtime even though they are
   compiled in.

## Recommendation

Adopt rbit behind the engine boundary, per
`docs/adr/0001-use-rbit.md`, but do not add the dependency yet:

- finish the verification list above first;
- adopt module by module, each with a test that moves its `BEP_STATUS.md` row
  from `verify` to `done`;
- keep the option to implement a single module in-tree if rbit's version is
  incomplete or unsuitable.

If the dependency weight in item 6 turns out to be unacceptable, fall back to
in-tree bencode, metainfo, and peer wire for v0.1, and defer the rest.
