# Sparse physical pages in a protected checkpoint rollover

A raw journal rollover may update only one metadata slot per voting member
when the other slot has been independently verified on a quorum. That retained
slot is the crash-recovery base. This change takes advantage of the
**unprotected target slot's existing bytes** rather than rewriting the entire
new checkpoint body.

## Write and failure protocol

1. Find and verify a matching previous checkpoint on a quorum using existing
   protected-rollover validation. If no safe old quorum exists, keep the
   conservative dual-slot path or refuse an ambiguous rollover.
2. Prepare the complete new checkpoint's existing 4 KiB page digests, index
   and full-payload checksum. The on-disk format does not change.
3. For each body/index write region of the *unprotected* target slot, read up
   to 1 MiB of existing physical bytes. Compare bytes, not the old index or
   header: a target slot could be damaged or left torn by an interrupted write.
   Physically identical pages are reused. Changed adjacent pages become
   bounded contiguous writes. If at least half the pages differ, rewrite the
   whole batch to cap syscall overhead. If the read fails, rewrite all bytes
   for that batch rather than treating unknown storage as valid.
4. Flush all target-body writes, publish the new full integrity header, and
   flush again. Only then may the journal be reset. Never touch the verified
   old quorum checkpoint during this attempt.

A torn target-page write may invalidate its old header but cannot invalidate
the protected older slot. A successful new header authenticates all reused
and written bytes. Recovery still chooses a valid quorum checkpoint as before.
On the next rollover, the now-newer verified slot is protected and the
older target may be updated. No runtime global cache or on-disk migration is
required.

## Field measurement

Prometheus counters exported for mounted-volume operations:

- `argosfs_checkpoint_cow_bytes_total{kind="target_bytes_read"}` measures
  successful physical target-page reads before comparison.
- `argosfs_checkpoint_cow_bytes_total{kind="reused_bytes"}` measures bytes
  actually found identical and left unwritten.
- `argosfs_raw_metadata_write_bytes_total{kind="checkpoint"}` measures
  actual checkpoint payload writes (including headers).
- `argosfs_checkpoint_rollover_member_strategy_total` distinguishes
  verified single-slot updates from the conservative dual-slot fallback.

An unchanged 2 MiB checkpoint body requires only the new 4 KiB slot header.
A 2 MiB body with two changed pages and one corrupted otherwise-unchanged
page required fewer than 1/16 of the complete checkpoint bytes in synthetic
loop-device tests. This is a byte-saving test, **not** a claim of 16x physical
USB throughput or end-to-end Borg speedup. Every skipped byte incurs a
comparison read, and compressed/shifted metadata can share fewer pages.

Run Vincecate's matching 294k-inode write/verify fixture before claiming
latency or endurance gains. Observe CPU, physical reads, physical writes,
rollover count, and the proportion of reused bytes; don't assume a speedup
when nearly all pages differ. Persisted cross-generation content-addressed
metadata trees and shard catalogs remain separate architectural work.
