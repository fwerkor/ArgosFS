# Stripe-Level Updates

Range writes and truncation now preserve unaffected stripe records and rewrite
only the affected stripe window. This reduces write amplification for random
writes, appends, and truncates while keeping Reed-Solomon shard validation
unchanged.

Range writes decode only clean bytes that must survive within the affected
window. A fully overwritten stripe does not need its previous shards.

FUSE mounts use a bounded per-inode writeback buffer. Adjacent or overlapping
writes coalesce; disjoint writes whose intervening gap lies within a logical
stripe may also coalesce. The buffer tracks clean gaps explicitly: reads overlay
only acknowledged bytes and commit reconstructs clean bytes from the current
durable inode under the metadata writer lock. Each combined stripe window is
encoded once. Unrelated distant writes use the existing ordered fallback.

Normal reads and attributes observe buffered writes without forcing a flush.
Explicit fsync/release, conflicting mutations, write pressure and daemon
shutdown preserve their existing durability and ordering behavior. The
ordinary buffer limit is 4 MiB, with bounded expansion for larger sequential
stripes and a 64 MiB mount-wide payload budget. Direct and synchronous writes
retain their original semantics. Failed commits keep the acknowledged bytes
available for retry.
