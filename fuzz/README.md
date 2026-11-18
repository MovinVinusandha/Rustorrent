# Fuzzing

The fuzz crate is excluded from the main workspace. It requires nightly Rust
and `cargo-fuzz`; it is not part of the stable CI job.

```text
cargo install cargo-fuzz
cargo fuzz run bencode
```

Corpus seeds live under `fuzz/corpus/`.
