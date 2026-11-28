# Metadata V3 development: disk-resident authenticated engine

**Development-only wire format. Do not use this engine to format or upgrade a
production pool.** The raw/loop CLI still uses the existing paged format.
No new superblock feature is enabled by this work. Do not freeze the V3 wire
layout until the volume integration and the qualification gates below pass.

## Implemented core

- The existing COW B+ tree, quorum body/root/certificate barriers, readback,
  stale-member repair, and fencing are shared by the numeric and compound-key
  codecs. Numeric pages and root encoding remain byte-compatible.
- V3 uses distinct page/root/certificate magic, length-delimited ordered
  compound keys (two-byte domain, big-endian numeric components, binary name
  suffixes), and versioned record envelopes with bounded LZ4 expansion.
  Direct point, predecessor and bounded range queries do not require a
  logical-key/record-ID table. The V3 root is now an authenticated catalog of independent domain
  trees. Only changed domain roots are replaced and the catalog is published
  atomically. The current one-page catalog accommodates 96 nonempty domains;
  extensible catalog storage remains a release gate if more are needed.
- An authenticated member-local reference-count radix map tracks metadata
  pages. Each leaf holds 512 u32 counts; branches hold 64 authenticated
  child references and free counts. Entirely free subtrees are implicit.
  The deterministic allocator tail occupies about 0.6% of the logical arena
  (plus rounding). Three slots per node allow COW edits while preserving
  both retained root maps, without recursive allocator-of-allocator
  allocations. This is a fixed-region implementation; segmented growth is
  still required.
- The root/certificate binds the logical root, physical allocator root,
  retained alternate root identity and feature flags. Physical allocation
  is local; the voted logical identity excludes device-local page addresses
  and includes the feature masks. Unknown incompatible flags fail closed,
  unknown read-only-compatible flags prevent writable open, and compatible
  flags are preserved.
- On a normal certified open, verify the member/root/certificate headers,
  logical tree heads and allocator heads. Verify descendants on access.
  Reads can retry another member of the same certified generation. Never
  fall back to an older certified generation on a damaged path.
- An uncertified prepared quorum triggers exhaustive graph validation.
  A mismatched alternate root (e.g. interrupted root publication) requires
  rebuilding the actual reachable union before allocation. An unreadable
  allocator head prevents that member from writing. Corruption of the
  allocator must not become evidence that a page is free.
- During a transaction, allocate only zero-reference pages from the
  currently valid union; add staged outgoing edges/new root references
  before decrementing the overwritten root. Descend an old page only on a
  zero-reference transition. Flush and read back both body types before
  root publication; certify only after a confirmed root quorum. Full
  allocator audits compare persisted counts against both retained graphs.
- Typed record access supports inode cores, binary directory lookup and
  paging, predecessor extent lookup, xattrs, member/allocator cores,
  generation-bound transactions and atomic directory/parent-index edits
  with precondition checks. The bounded record cache is tied to a borrowed
  immutable generation; range scans bypass the hot point cache.
- An offline image-to-record converter covers the existing inode cores,
  dirents, blocks, xattrs, disks, data-allocator cores/free extents, and
  parent/file indexes. It also imports quota usage and hard-link membership
  counts, member-to-block references, global statistics and supplied audit
  events. The quota accounting reuses the supported format's semantics.
- Generation-bound dirty-overlay point reads and file-core accounting
  update logical usage and all existing nested quotas atomically. Block
  edits include member-reference changes. Namespace quota membership changes,
  data allocator mutations and full live-volume integration are still required.
- Resumable migration staging into a separate V3 arena commits each bounded
  batch together with its source digest and cursor. Reopen resolves ambiguous
  acknowledgements; verification compares every record and audits all
  configured member allocators. The legacy source is still materialized in
  memory and no production superblock publication is implemented.

## Safety invariants

1. The full configured membership is bound to every root; missing members
   cannot reduce the majority.
2. No live metadata body or allocator slot reachable from either retained
   root can be overwritten by a prepared transaction.
3. A writable allocator must describe the exact union of the current and
   actual alternate root; if that proof is absent, full validation is
   required before rebuilding it.
4. No acknowledgement precedes durable quorum certification. An ambiguous
   publication fences the writer. Certified corruption never justifies
   rollback.
5. Read handles borrow/pin an immutable generation. A caller must exclude
   other processes writing the supplied arena for the handle lifetime.
   Long-lived user snapshots across writes are not implemented yet.
6. Unknown required formats or features must not be mistaken for older
   recoverable generations.

## Qualification and remaining gates

The unit tests exercise sparse billion-page allocator geometry, COW
retention, exact randomized reference counts, reopen/reclaim, torn
body/root/certificate phases, returned-member repair, damage refusal,
variable-length keys, non-UTF8 name conversion and bounded scans.

The synthetic 50,000-record, three-member fixture uses 24 read requests on
normal writable open and writes ten pages per member for a single
record edit. This measures the **standalone metadata engine**, not FUSE
mount, whole-volume startup, physical disk latency or Borg performance.

Before exposing any final-format mkfs/migration:

1. Finish runtime namespace/quota/index mutation paths and data-extent
   accounting. Extend the root catalog if required and finalize record schema
   evolution rules. Preserve unknown compatible data.
2. Integrate disk-resident transactions into the live volume model and
   remove whole-image dependencies from namespace, data allocation,
   quotas, verification, FUSE, CLI, health and Autopilot. Bound dirty
   memory, page caches, snapshot readers and maintenance windows.
3. Support dynamic metadata segment growth/compaction, durable snapshot
   and clone references (including data extents), and local metadata
   repair. Generalize the physical map retention rules for the final root
   catalog; add a safe allocator rebuild protocol when its own pages are
   damaged.
4. Integrate topology epochs and joint-quorum transitions; design and
   connect authenticated source recovery to the implemented resumable
   staging/verification, then implement crash-safe superblock publication
   from both legacy formats. No production conversion without explicit approval.
5. Run mixed workload and power-cut/fault-injection qualification, compare
   old/new namespaces and physical extents, verify compatibility with old
   software, and measure real cold mounts at 294k, million and larger
   inode populations. Only then finalize feature flags and enable the
   format by default.

The existing format remains the supported deployment path while these
gates are open.
