---
name: rust-conventions
description: Use for any Rust code in this repo. Error handling, async rules, crate boundaries, size and performance settings.
---

## Errors
- Libraries: `thiserror` enums per crate. Binaries: `anyhow` at the top level only.
- No `unwrap()`/`expect()` outside tests. Never `panic!` on external input.

## Async
- Tokio runtime. Never do blocking file I/O or hashing directly on async tasks:
  use `spawn_blocking` or a dedicated storage thread.
- Bounded channels only (`mpsc::channel(N)`). No unbounded queues.
- Every spawned task must be cancellable and owned by a handle or JoinSet.

## Structure
- Dependency direction: proto <- engine <- client <- tui/daemon/gui.
- `pub` only what other crates need. Document public items.
- OS-specific code lives only in crates/platform behind `cfg`.

## Size and speed (release profile)
opt-level = "z" (try "s" if hashing is too slow), lto = true,
codegen-units = 1, panic = "abort", strip = true.
Before adding a dependency: check it is maintained, license-compatible
(cargo-deny), and what it adds to binary size.

## Done checklist
cargo fmt --all; cargo clippy --workspace --all-targets -- -D warnings;
cargo test --workspace.