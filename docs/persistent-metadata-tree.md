# Persistent metadata page engine (opt-in raw format)

ArgosFS supports an explicit paged metadata raw/loop format. The normal
legacy format remains the default. Paged pools use certified COW roots for
immediate transactions, deferred group commits, sync, replay selection, and
clean unmount; legacy checkpoint and journal writers reject the new required
feature. The code remains a draft until topology changes and broader
production testing are complete.

```sh
argosfs mkfs --backend loop --images a.img,b.img,c.img \
  --image-size 512MiB --k 2 --m 1 --paged-metadata
argosfs upgrade-paged-metadata --pool-config pool.json --offline-confirmed
```

The upgrade is one-way and requires all active members, no other mounted
writer, and sufficient never-allocated tail space on every member. Optional
`--arena-size SIZE` controls the reserved size (default: the existing metadata
region size). Back up important data before converting. There is no silent
conversion of a legacy pool.

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

## Metadata records and mounted transaction path

`ImageStore` persists a header (record zero), inode cores, directory entries,
extended attributes, block mappings keyed by logical offset, disks, allocator
cores, and individual free extents. A persistent logical-key to record-ID
index is reconstructed at open. Stable IDs prevent insertions from renumbering
later records. Each record chooses plain JSON or LZ4 with bounded expansion.
The header excludes the root digest itself; scheme 3 reconstructs
`integrity.meta_hash` from the authenticated root. The previous digest and
logical transaction ID are included in the header.

The bridge uses `TrackedInodes::changed_candidates` and advances the shared
baseline exactly once after a successful commit. Directory entries use an
ordered persistent map with structural diff, retaining the existing sorted
JSON representation. File block and free-extent differences are compared by
logical offset; only changed records reach the page tree. An unrelated inode
snapshot takes the exhaustive fallback. Block vectors and a changed allocator
extent sequence still need a local linear comparison; this is not yet strict
O(changed records) CPU work for very large individual files/fragmented disks.

The mounted volume shares one page-store handle across cloned filesystem
handles. Both immediate and deferred transactions pass the existing data
durability checks and data flush barrier, then commit the authenticated root.
The legacy journal is no longer an independent recovery authority. Advisory
event-only records are not persisted in this format; transaction inspection
reports the latest selected root and logical transaction instead of legacy
journal history.

## Offline migration protocol

Fresh pools use the original metadata region as a page arena. Conversion of
an existing pool stages a new arena in the never-allocated tail of each
member's data region; both legacy checkpoint slots and journal remain
untouched. The immutable full membership/geometry manifest is stored in the
previously unused allocator region and independently checksummed. Every
configured member identity and arena boundary is checked against the
superblocks and recovered allocation state.

1. Read the legacy recovery winner without modifying it. Require all active
   members and independently check live physical shard ranges, the allocator
   high-water mark, and free extents before reserving the tail.
2. Stage the new manifest and authenticated root on every member; confirm all
   root certificates. No active format flag has yet been emitted.
3. Write and verify PENDING in every primary and backup superblock, fencing
   older binaries while retaining the legacy recovery data.
4. Only after every copy has been fenced, publish ACTIVE. An ACTIVE copy is
   evidence the new root was staged and all old writers fenced. The new
   metadata reduces each data allocator's boundary to exclude the arena.

An interruption before ACTIVE does not permit a new-format mount. Rerun the
same offline upgrade with all members: it can restage from the untouched legacy
winner. If any primary or backup already has ACTIVE, the retry recovers the
new root instead, preserves any later paged transactions, and finishes flag
publication. Do not use older binaries or run unrelated writers during an
interrupted upgrade. An upgrade can fail for insufficient free tail space
without changing the legacy recovery authority.

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

## Remaining production work

- The current format has fixed configured membership. Add/remove/replace
  operations requiring a membership change fail explicitly before mutating
  the pool. Online membership changes need a durable old/new configuration
  transition with joint quorum evidence, not a change to the existing
  manifest in place. Repair of returning configured members is supported.
- Finish precise mutation tracking for file block vectors and free extent
  sequences, so even the comparison work is local to a changed range.
- Add production-grade event history and detailed paged-member audit
  diagnostics; legacy journal event records are not used for this format.
- Validate arena capacity on large real inode populations, run multi-device
  raw/loop lifecycle and ztest, and benchmark end-to-end Borg performance on
  physical storage. The current 294k test is a fixed-size page-engine
  microbenchmark, not a full physical-pool performance result.
