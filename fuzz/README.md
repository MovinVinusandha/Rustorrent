# Fuzzing

The fuzz crate is excluded from the main workspace. It requires nightly Rust
and `cargo-fuzz`; it is not part of the stable CI job.

```text
cargo install cargo-fuzz
cargo fuzz run bencode
cargo fuzz run metainfo
cargo fuzz run resume
```

Corpus seeds live under `fuzz/corpus/`. The `resume` target decodes
arbitrary bytes and, for a document that decodes, checks that re-encoding it
and decoding again returns the same document. Oversized input must be rejected
before parsing.
