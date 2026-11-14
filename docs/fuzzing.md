# Coverage-guided fuzzing

ArgosFS uses `cargo-fuzz`/libFuzzer for in-process coverage-guided testing in
addition to the stateful randomized model, mounted FUSE, block-fault, and QEMU
chaos suites.

The current targets deliberately mix malformed-input exploration with
valid-structure invariants so libFuzzer can reach deep logic quickly:

- `raw_format`: raw superblock/device-label decode plus valid encode/decode invariants.
- `journal_record`: arbitrary and structured journal JSON serialization paths.
- `metadata_delta`: metadata diff/replay and canonical-hash equivalence.
- `paged_metadata`: paged metadata split/reassembly and page-delta replay.
- `journal_recovery`: crash-point injection plus metadata-copy/journal-tail damage and recovery.
- `erasure_codec`: Reed-Solomon encode/reconstruct under bounded shard loss.

Install the nightly toolchain and cargo-fuzz, then run a target:

```sh
rustup toolchain install nightly --profile minimal
cargo install cargo-fuzz --version 0.13.2 --locked
scripts/ci/run-fuzz.sh metadata_delta 300 1
```

Crash artifacts are written under `fuzz/artifacts/<target>/`. The committed
corpus is only a bootstrap. Weekly CI restores a target-and-shard-specific
cached corpus and saves the evolved corpus back under a new cache key, so
coverage accumulates across runs while shards remain independent.

Pull-request CI only performs a short smoke run to catch broken harnesses.
The weekly chaos workflow is the high-intensity gate: every target runs in
three independent 30-minute shards, for nine aggregate libFuzzer hours per
week, in parallel with the existing model/FUSE/fault/QEMU matrices.
