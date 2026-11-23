# Verified single-slot raw checkpoint rollover

Raw journal rollover must leave a durable checkpoint quorum before resetting
journals. Previously every rollover wrote the complete compressed or
uncompressed metadata image into **both** slots on every member. For the
294k-inode physical workload, this creates approximately two full copies per
member per rollover even though the previous checkpoint already exists.

## Write protocol

A quorum group commit can use a single-slot rollover when there is a common,
physically validated older checkpoint on at least the required number of
distinct voting members:

1. Read both slot headers, find the latest identical complete checkpoint
   header present on a quorum, then verify each candidate member's page-index
   checksum, every page checksum and the full stored payload checksum. Read
   validation is parallel across members with a 1 MiB data buffer per member.
   The existing on-disk tree format is unchanged. Unsupported older layouts
   and checkpoints larger than 256 MiB *in stored/compressed form* use the
   existing conservative two-slot path.
2. On verified members write only the **other** slot: physical metadata body
   followed by the page index, flush, slot header, then flush again. The previous checkpoint slot
   is never modified during this rollover. A member without a verified copy
   receives the historical two-slot write, and cannot reduce the verified
   old-checkpoint quorum.
3. Only after a member's new slot is fully published may its journal be reset
   and flushed. A quorum of completed member operations is required to finish
   the transaction; the existing journal certificate and indeterminate-commit
   fencing remain unchanged.

A crash before the new checkpoint reaches quorum leaves a quorum of old
verified checkpoint slots intact. A crash after the new checkpoint reaches
quorum allows recovery to select the newer version. On the next rollover, the
protected slot changes to the now-newer verified checkpoint; the very old slot
is overwritten. This alternating selection must not use just txid parity:
successive rollovers need not have alternating txid parity.

The conservative full two-slot write remains in place for initial formatting,
explicit full checkpoints, replay checkpoints, unsupported old layouts, and
failure to preflight a physically valid prior quorum.

## Measurement and scope

The new Prometheus metric
`argosfs_checkpoint_rollover_member_strategy_total{strategy="verified_single_slot"}`
counts attempted single-slot member writes; the
`conservative_dual_slot` label counts fallbacks. These are *attempts*, not
acknowledged commits. Compare them with
`argosfs_raw_metadata_write_bytes_total{kind="checkpoint"}` and the
`journal_rollover` checkpoint reason. Mount-time recovery checkpoints are
not included in these mounted-volume counters.

The prior two-slot strategy writes roughly twice the checkpoint payload
bytes on fully healthy members. This change halves checkpoint write bytes
**on successful rollover preflight**, but incurs validated old-checkpoint
reads and does not eliminate the remaining full-image write, inode scan, or
journal rollover frequency. A nine-member physical retest is required to
measure elapsed time, actual NAND/USB I/O and Borg amplification.

Local regression coverage includes consecutive rollovers, old-checkpoint
corruption, partial new-body writes, less-than-quorum failures and compressed
metadata recovery. Do not bypass the old-checkpoint quorum validation to
reduce reads: doing so can destroy the only replay base while a journal is
being truncated.
