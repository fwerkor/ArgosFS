# Read-side decoded block cache hot path

Borg verify-read benchmarks in [Issue #231](https://github.com/fwerkor/ArgosFS/issues/231#issuecomment-6949031846) identify repeated full-block copies and rehashing as a major source of ArgosFS FUSE worker CPU after #252 removed read-time checkpoint storms.

## Behavior

For normal, unencrypted logical file reads, `decode_inode_range_with_cache_policy_locked` first tries a validated cached subrange. A cache hit checks the immutable cached block's **admission-time** hash against the block's expected BLAKE3/SHA-256 digest and total length. It copies only the requested `[start,end)` range into the caller buffer. The special 1+0 uncompressed direct-range path continues to read and verify physical shard subblocks on every request, even if a prior full decode populated the cache; it is deliberately excluded from this range shortcut. A miss falls back to existing verified raw-shard or erasure decode paths. Invalid block length or content hash evicts the cache entry (and L2 copy) instead of returning stale data.

The L1 cache owns immutable `Arc<[u8]>` values. Once inserted, callers cannot mutate their storage. The existing full-block `get()` interface stays available to other callers, but it now uses the same checked digest metadata. SHA-256 for legacy hashes is calculated at most once per L1 entry. The LRU has per-entry indexed timestamps: hits, replacement, and invalidation avoid scanning every live key. Tick rollover is handled without changing eviction order. In-flight L2 promotions track mutations by cache key (and prefix invalidations), so unrelated block admissions do not cancel valid reads. Tracking entries disappear when their last in-flight read finishes; concurrent replacement or invalidation cannot resurrect a stale disk copy. Admission-time hashing is performed outside the global cache mutex.

L2 cache reads still verify the full on-disk payload before admission. Encrypted blocks never enter the decoded-block fast path. Physical scrub and repair **bypass reconstructed data caches**, still read/validate member shards and may perform parity reconstruction and repair. On a miss, no weakened checksum/repair path is introduced.

## Reproducing tests

```bash
cargo test --lib data::cache::tests -- --test-threads=1
cargo test --lib read_cache_range_tests -- --test-threads=1
cargo test --all-targets -- --test-threads=1
cargo clippy --all-targets --all-features -- -D warnings
```

An opt-in, non-CI timing comparison uses the original full-block `clone` + content hash and the new subrange API, each for 1,024 reads of 128 KiB from an immutable 1 MiB block:

```bash
cargo test --lib cache_subrange_microbench -- --ignored --nocapture
```

This microbenchmark isolates **only the decoded-block cache hotspot**. It is not a prediction of end-to-end Borg throughput; a reliable field comparison needs the same nine-member pool, benchmark repository, cold/warm cache state, and release binary, with daemon CPU, physical reads, and checkpoint counts captured. Other remaining costs include per-request metadata locking/snapshot copies and full metadata journal rollovers.
