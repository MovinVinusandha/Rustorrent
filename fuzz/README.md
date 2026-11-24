# Fuzzing

The fuzz crate is excluded from the main workspace and requires nightly Rust
and `cargo-fuzz`.

Every push and pull request checks that the targets compile with
`cargo check --manifest-path fuzz/Cargo.toml --bins` on nightly. A scheduled
workflow fuzzes each target for five minutes every Monday, and on demand, and
uploads crash inputs as `fuzz-artifacts-<target>-<run_attempt>`.

```text
cargo install cargo-fuzz
cargo fuzz run bencode
cargo fuzz run metainfo
cargo fuzz run piece_buffer
cargo fuzz run resume
```

Corpus seeds live under `fuzz/corpus/`. The `resume` target decodes
arbitrary bytes and, for a document that decodes, checks that re-encoding it
and decoding again returns the same document. Oversized input must be rejected
before parsing.
