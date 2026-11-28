# Persistent metadata page engine (experimental)

The raw backend currently stores a whole metadata snapshot in each checkpoint
slot. This document specifies the separate page engine in
`src/storage/raw/paged_metadata/`. It is implemented and tested on explicitly
reserved loop-file regions, but **is not yet an ArgosFS pool format**.
Normal `mkfs`, mount, fsync, recovery, and topology operations are unchanged.
Do not point this engine at a legacy checkpoint, allocator, journal, or live
data region.

## Page format and tree

Each arena has four reserved 4 KiB pages: two alternating root slots and two
matching quorum certificate slots. The remaining pages are independently
allocated from a device-local free set.

A page contains a fixed header, ordered records or child pointers, zero
padding, and SHA-256 of its physical bytes. Tree pages are a packed B+ tree
over ordered 64-bit record keys:

- Leaf values up to 768 bytes are inline; larger values occupy linked,
  authenticated overflow pages. Values are bounded at 1 GiB.
- Internal child records contain the subtree's maximum key and an authenticated
  pointer. At most 84 pointers fit on a page. Children must be one level lower,
  ordered and non-overlapping.
- Insertion splits overflowing nodes. Deletion removes empty nodes, merges or
  redistributes underfull neighboring pages, and collapses single-child roots.
- A logical page digest covers the canonical page encoding with physical page
  numbers zeroed. The child digest still binds the exact expected content.
  A separate physical checksum detects damage to local pointers. Logical root
  identities remain equal across replicas with different page allocations.

Root slots contain the arena size, transaction number, root pointer, and
member identity. The member identity binds the pool UUID, sorted full
configured membership, each region's geometry, and the local device ID.
Omitting an offline member cannot silently lower the majority requirement;
copying the bytes of one member to another does not create a second vote.
Callers must still supply distinct physical devices and exclusive arena access.

## Transaction protocol

1. Prepare changed paths against the selected root on each eligible member.
   Reserve only pages unreachable from either retained root. Check all staged
   child references before issuing writes. The initial empty tree has a
   bottom-up packed build; subsequent updates use localized COW.
2. Write new pages, coalescing neighboring allocations, then flush devices.
   Require a majority of durable bodies before publishing any new root.
3. Publish a new checksummed root into the slot not holding the selected
   recovery base, then flush. Require a majority of confirmed roots.
4. Write and flush independent certificates referencing the new root only
   after the previous barrier has succeeded. Acknowledge only after a
   majority of certificates is durable.

A failed body phase cannot change a root. A failed root or certificate phase
may have exposed a new state and therefore returns an indeterminate commit;
the handle is fenced until reopened. A stale or ambiguous member is not used
as the next mutation base. A recovered prepared quorum is certified before
any older root can be overwritten by a later transaction.

A durable root makes its predecessor's retired pages reclaimable only when
neither retained root references them. Increment new references before
releasing the overwritten root. Shared untouched descendants need no
traversal. On opening, reconstruct the free set and reference counts from
both verified root graphs. This mount-time scan is deliberate: no on-disk
free list can incorrectly mark a live page free after a torn write.

## Recovery and repair

Read and verify root/certificate headers and every reachable page. Verify
tree height, key ranges, overflow lengths, checksums, and absence of cycles
or duplicate pages in each graph. Count votes once per configured member.

Select the highest root supported by an independently verified majority or
by a certificate written after a durable root majority. An uncertified
minority root is not a commit. A certificate remains evidence even if its
associated root body is damaged: if no intact copy of the latest certified
root remains, refuse recovery instead of silently rolling back.

When enough devices remain readable but a selected certified root is on too
few members to make new writes, `repair_stale` copies its exact authenticated
tree shape to stale accessible members, using their own allocators. It
preserves logical digests and certifies repaired copies; normal commits
resume only after the selected base is again on a certified majority.
Repairing missing devices requires reopening with the same full configured
membership after the devices return. Changing that membership is not yet
supported by this engine.

## Typed bridge

`ImageStore` persists a metadata header (key zero) and separately encoded
inode records. Each record chooses plain JSON or LZ4, with bounded expansion.
The bridge uses `TrackedInodes::changed_candidates` and advances the shared
baseline only after a successful commit. Unrelated snapshots take the
exhaustive candidate fallback. On recovery, reconstruct the authoritative
inode map and check every inode's key against its embedded ID.

The current bridge deliberately preserves the existing Metadata model and
caller-supplied transaction/integrity fields. It does not yet replace the
raw journal's transaction semantics. Directory entries, file block vectors,
and the raw allocator header can still be large records; updating one of
those rewrites that record's overflow pages.

## Complexity and measurements

For a fixed-size changed record, steady state touches the affected leaf and
its ancestors, plus the two publication pages. The cost is proportional
to changed paths and overflow pages, not the total namespace. Reopening,
initial import, and stale-member full repair are necessarily larger
operations. Recovery reconstructs the allocator in memory rather than
persisting a separate mutable bitmap.

Local loop-file regression using 128-byte records, one member:

| Records | Initial pages written | Reads for one edit | Writes for one edit |
| --- | ---: | ---: | ---: |
| 50,000 | 1,811 | 4 | 5 |
| 294,000 | 10,630 | 5 | 6 |

The write counts include the new root and certificate. The 294k test is
opt-in for routine CI; run it with:

```sh
cargo test --lib two_hundred_ninety_four_thousand_records_stay_path_bounded -- --ignored --nocapture
```

These are page-engine microbenchmarks, not a Borg backup benchmark or a
measurement of an existing nine-member physical ArgosFS pool.

## Production format integration checklist

Before advertising a new mountable raw format:

- Persist and authenticate a complete membership manifest that can be
  discovered from available superblocks, including absent-member geometry.
  Implement joint old/new quorum transitions for adding, draining, and
  removing members. Keep the existing data durability and topology checks.
- Allocate a dedicated page arena, reserve its space in data allocation,
  and fence old binaries with a required superblock feature. Migration must
  retain the existing checkpoint and acknowledged journal through the new
  root/certificate barrier, including interrupted upgrades and failed members.
- Route every transaction path (immediate, deferred, fsync, recovery,
  allocator repair, member repair, clean unmount, and offline tools) through
  the same mounted page-store handle. Reconcile the existing journal and
  integrity semantics, rather than introducing a second authority.
- Split large directory maps, block vectors, and allocator extent structures
  into independently keyed records with precise dirty tracking, so one
  dirent or block edit does not rewrite a huge inode/header value.
- Validate arena capacity at 294k and larger real inode populations, test
  raw and loop pool lifecycle/topology, crash injection and ztest, and
  benchmark end-to-end Borg performance on actual multi-device storage.

The implementation here is the storage-engine foundation for that work.
It does not silently enable or migrate an existing pool.
