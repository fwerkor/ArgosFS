# Metadata V3: durable snapshots and shared data

**Normative design target for the final V3 format; not implemented yet.**
The experimental V3 format is not frozen. The existing JSON
`argosfs snapshot ROOT NAME` is a metadata export, not a restorable file
snapshot. It must not be advertised or silently imported as one.

> Implementation note: the experimental branch now has native typed
> metadata snapshot edges and an extent-pin compatibility prototype.
> See [metadata-v3-development.md](metadata-v3-development.md) for verified
> capabilities and limitations. It deliberately uses temporary required
> bit 63 and private domain 0xff00, not the final bit/domain reservations.
> The logical object/placement and retirement design below remains the
> release contract; prototype behavior must not be mistaken for completion.

## 1. Scope and contract

The first production implementation supports atomic, pool-wide read-only
namespace snapshots, named listing and deletion, read-only access and explicit
whole-namespace rollback. A snapshot captures file bytes, holes, names,
hard-link identity, inode attributes, symlinks, ACLs and xattrs at one durable
transaction boundary. It does not capture running processes, open handles,
pending uncommitted writes, pool hardware, free space, topology or audit
history. Application-level consistency still requires application quiescence.

The format also supports independent writable namespace roots (clones), shared
extents and incremental replication without another incompatible format change.
Clone and replication CLI availability may follow the initial snapshot
implementation; their on-disk ownership semantics must be qualified before V3
is frozen. Arbitrary directory snapshots are not a v1 promise: directory trees
can have cross-boundary hard links and need an explicit dataset boundary
contract. The initial root is the existing entire filesystem.

Creating a snapshot or a clone must share existing tree roots and data, never
walk every inode or copy user data. Space consumption grows with divergence and
retained history. Deletion unpublishes a name atomically and reclaims space in
bounded restartable batches; no promise of immediate space recovery.

## 2. Split the certified catalog into views and pool state

Extend the existing authenticated compound-key domain catalog rather than
store one complete historical Metadata image per snapshot:

- A **namespace view descriptor** references the roots of Inode, Dirent, Xattr,
  Block, Parent, FileIndex, Quota, QuotaMembership and Statistics, plus a small
  view header (root inode, namespace schema and any view-local attributes).
  A root set is immutable once published. Missing domains have a canonical
  empty root. Captured quota policy and usage describe that view, not current
  physical pool usage.
- A **pool catalog** contains the active view, named snapshots, writable clone
  roots, a retirement/GC root set and global control domains: pool Header,
  Disk, DataAllocator, DataFree, MemberReference, Event, object placement,
  object/node ownership and lifecycle state. Split view-dependent fields out
  of the current serialized Header; transaction generation, topology epoch,
  allocation counters, credentials/key lifecycle and policy belong to the
  pool. No view may point back to the pool catalog or to another snapshot
  registry. This prevents recursively retaining all past catalogs.
- Use a bounded-fanout authenticated index for view descriptors; the existing
  one-page domain catalog limit is not a limit on snapshot count. Root-set
  descriptors are small and fixed by schema, not proportional to file count.
  Stable monotonic namespace/snapshot IDs are never reused in a pool. Inode
  IDs are scoped by view; new IDs and object IDs are allocated by monotonic
  pool state that rollback cannot rewind. Names are a secondary binary
  index, uniquely mapped to immutable IDs, not identities.

A view reference is an actual typed metadata graph edge, not a JSON record
containing a page number. Extend the V3 node/value codec with typed child
references (including leaf values and root-set descriptors). Every encoder,
decoder, node-size check, child enumerator, planner, graph validator, repair,
scan and compactor must understand these edges. Existing
`Node::children()` handles branches, catalogs and blobs only; opaque pointer
bytes would let the allocator free historical pages. Logical hashes authenticate
edge role, ordering and child digest, never a member-local physical address.
Each member stores/resolves its own physical pointer under the same logical
identity. Validate allowed edge types, domain, levels, bounds, acyclicity and
key ordering. No dangling edges, cycles, self-references or cross-pool roots.

The current planner rejects multiply referenced staged pages; revise it to
allow validated DAG sharing with exact checked incoming counts, not arbitrary
shared mutable pages. Traversals must distinguish shared nodes from cycles and
count every edge while visiting outgoing edges of an immutable node once.
Normal logical certification must cover all new domains and root types.

## 3. Stable data objects and placement indirection

Today a Block value is a FileBlock with inline ShardLocation records. That
cannot be frozen as the snapshot-aware format: a historical physical address
would pin a disk or become stale after repair. Replace it with a logical block
mapping (inode, offset) -> (object ID, object slice/length and mapping flags).
Holes have no object. Initially a mapping can reference a whole immutable
stripe object; reserve validated slice semantics for shared range clones.
A partial write produces a new object for the affected allocation unit, not a
mutation of the old object's logical bytes.

The pool-wide object table is keyed by a never-reused object ID, not a hash of
plaintext, offset or a device ID. Each object contains immutable content
identity (logical length and digest) and the current authenticated encoding and
placement descriptor: layout, codec, encryption/key ID, nonce, shard hashes,
sizes, device IDs and physical extent generations. Layout/encoding revisions
may change only if the decoded content identity remains equal. Bind encryption
AAD to stable object/encoding identity, not the view or a mutable physical
location. Use fresh nonces as required by the cipher. Keep old key material
while a retained object/encoding requires it; no plaintext fallback.

All views resolve a stable object ID through a **current** certified placement
descriptor, not through the historical pool allocator or disk map. A read
pins the chosen placement version until I/O completes. A snapshot remains
readable after device replacement, tier movement, reshape, compression
re-encoding or key rotation; relocation publishes verified new placement
before retiring the old placement. Never change a snapshot's content hash.
A metadata read may pin a historical namespace root while using a newer
certified pool placement view.

Replace the inode-keyed member reverse index with a complete device -> object
ID / shard-slot / placement-version index, including snapshot-only and
clone-only objects and any still-pinned retired placements. This index is
global; maintenance iterates objects once, not all historical namespaces.
Free extent accounting includes all live and retired encodings until the
reclamation conditions in section 5 are satisfied.

## 4. Two distinct reference graphs

**Member-local metadata pages.** The existing authenticated physical allocator
continues counting physical incoming edges. Its root set is the actual current
and alternate member root slots, including all view and retirement edges
reachable from them. A node's outgoing edges are owned once while reachable,
not multiplied by the number of snapshots reaching the node. Reference counts
are checked, never saturating or wrapping. Zero-transition work that exceeds
the transaction budget transfers ownership to a durable typed GC root before
dropping the original edge; no reachable page is temporarily free. A member
with an uncertain graph cannot allocate from it. The allocator rebuild audits
both actual roots and all retirement edges; the physical map remains
member-local and excluded from the voted logical identity.

**Replicated data-object ownership.** Do not derive object lifetimes from
member-local page allocation/counts, which can differ across replicas.
Maintain a deterministic logical DAG for the Block domain and a certified
pool-wide reference ledger. Assign immutable logical node IDs to newly built
Block tree nodes (transaction generation plus deterministic per-transaction
sequence, domain-separated and never reused), authenticated with their
content. Physical copies of one logical node use the same ID; independently
created equal-content nodes are distinct. A member-independent logical edit
plan must generate these IDs, edges and reference deltas once; the per-member
planner maps them to local physical addresses. Compaction preserves logical IDs.
The ledger is derived from the canonical plan, not from whichever replica
happens to answer a read. A zero count does not permit ID reuse.

The ledger counts incoming edges from published view descriptors and durable
retirement/GC work to Block roots and from each reachable Block branch to its
children. Every reachable Block leaf owns one object reference per mapping
entry (including duplicates); a leaf shared by 100 snapshots still owns its
entries once. New node outgoing edges are added once; removed outgoing edges
are decremented only when that node loses its last incoming edge. Record
splits, merges and clone edits therefore account only for touched nodes.
No full-file or per-snapshot refcount increment on snapshot creation.
Ordinary object references are bounded by actual new/retired leaf entries.
The ledger itself is a pool-control tree, never part of the namespace DAG:
otherwise its own updates would recursively change its ownership.

Store object counts, Block-node counts/identity and retirement progress in
certified global records; references and immutable child identity must be
auditable from the owning typed node (including spilled values). Apply new
edges before retiring old edges, coalesce deltas, and reject underflow,
overflow, absent objects or inconsistent IDs before certification.
An exact audit traverses all logical view and retirement roots, counts each
incoming edge, expands each logical node once, then compares against the
ledger, object table, placement index and raw allocator. Neither corruption
nor an incomplete audit is evidence of free space. A safe rebuild writes a
separate verified ledger and publishes it atomically under the normal quorum.

## 5. Transactions, retirement and crash consistency

Snapshot creation uses the live writer, not an independent CLI open of a
mounted pool. Drain writes ordered before a snapshot barrier, including dirty
FUSE/writeback overlays, complete the data durability contract, then insert
the name, descriptor and root references in one certified transaction.
Writers after the barrier belong to a subsequent view. Return a snapshot ID
only after durable quorum certification. A retried operation can identify the
same committed result using a durable request ID or explicit ID/name
precondition; never overwrite an existing name.

Ordinary writes allocate fresh data extents, persist and verify data according
to the existing redundancy policy, update object/placement and Block roots,
ledger and allocator atomically, then certify the new root. Existing data
reachable from any retained view is immutable. A live block mapping being
deleted is not proof that its object is unused. In-place overwrite shortcuts
are disallowed unless exclusive ownership, all recovery roots and all
reader fences are proven; initial implementation has no such shortcut.

A removed view/root is transferred atomically to a persistent retirement
record. Keep displaced logical roots (or equivalent certified retention
edges) until no selectable recovery generation or in-flight reader can use
them. The logical retirement ledger is replicated; the local allocator still
protects the actual alternate root on each member. No quorum recovery may
select a generation whose data have been reclaimed. Stale/offline members
must be fenced and resynchronized from the accepted graph before allocating
or serving reads; their obsolete roots cannot resurrect retired objects.
If eligibility of a prepared/alternate generation is ambiguous, fence and
retain, not free. Account for all roots that recovery can select, not merely
the latest acknowledged root.

Deletion commits namespace removal plus retirement ownership; GC later
walks zero-transition nodes with a durable cursor and bounded I/O, time and
dirty-memory budgets. Each batch replaces its ownership edges and progress
atomically. When an object loses its last logical reference, retire its
placements rather than immediately publishing free extents. A free-space
transaction may release a placement only after:

1. no published or retirement view can reach its object (or it is an obsolete
   encoding of a still-live object);
2. no eligible recovery root can depend on that placement; the transaction
   removing its last eligible reference is certified and the required
   retention/slot-rotation barrier is durable;
3. no reader, write, scrub, rebuild, relocation or other in-flight I/O pins
   that version, and relevant writers/members are fenced;
4. its physical extent generation and reverse-index ownership match the
   retirement record. Removal from the index and insertion into DataFree
   are one certified transaction.

New allocation must use the last committed safe-free state; never reuse
extents released in a merely prepared transaction. Pending reclamation remains
charged to physical capacity. Durable reservations must allow bounded GC,
deletion and recovery at ENOSPC (including metadata exhaustion); if unavailable,
fail safely rather than reclaiming unproven space. Crashes before publication
leave the old view; after certification they leave the new view. Ambiguous
acknowledgements fence until resolution. A mount must resume pending work
without scanning the entire namespace; a full graph walk is an explicit audit
or necessary recovery fallback, not normal startup.

## 6. Readers, rollback and clones

Expose snapshots through an explicit read-only mount/view selection; a
read-only virtual snapshot directory is optional and must not reserve an
existing user path silently. Cache and FUSE inode/handle identity include the
view ID and relevant generation. Keep POSIX lookup/open references, hard-link
identity and ACL checks scoped to that view. Existing readers keep a pin;
deletion hides the name but does not invalidate a previously opened view.
Pin a durable retirement root until the last handle is released; process
death releases ephemeral pins only after writer/session fencing. Cross-process
readers require an explicit lease/fencing protocol or exclusive writer
coordination; unsupported independent concurrent opens fail closed.
Do not pin every historical pool generation for the lifetime of a snapshot:
namespace roots are long-lived, placement pins are bounded per I/O.

Rollback is an explicit exclusive namespace operation, initially requiring
unmount/quiescence and no live handles. Switch the active root set to the
chosen immutable view and increment the live generation; later writes COW.
Retire the replaced root or retain it under an explicitly named safety
snapshot. Never rewind global transaction IDs, allocators, topology, keys,
event history or object ID counters. Revalidate live policy/quotas and
invalidate all live caches. Refuse rollback if root/schema/key compatibility
cannot be proven. Do not restore legacy metadata JSON into a running pool.

Clones publish an independent writable root set sharing the same captured
roots; the first write forks only affected paths. Snapshots/clones survive
deletion of their origin because ownership is by graph edges, not a
parent-name dependency. Cross-view inode operations and hard links are
rejected; range clone/reflink uses object sharing and checked COW accounting.
Each writable view has its own logical quota/usage; physical usage is global
and shared bytes are not multiplied by the number of views. Initial reporting
distinguishes active logical bytes, retained physical bytes, pending reclaim
and actual free bytes; exact per-snapshot exclusive space attribution is not
required on the hot path.

Incremental send/receive can compare immutable authenticated roots and skip
identical subtrees, stream changed namespace records and content objects,
and use a base snapshot identity/content digest. Pool-local object IDs and
member-local pointers are not portable wire addresses. Receive stages and
verifies data and dependencies before atomically publishing a view. Reserve
format support for view identity and lineage; a separate versioned transport
protocol can be implemented later, without changing pool ownership semantics.

## 7. Maintenance and compatibility

Scrub verifies all reachable objects, including snapshot-only objects, and
repairs the current encoding once. fsck validates namespace views, object
ownership, physical extent non-overlap/generation, reverse indexes and both
metadata allocator roots. Orphan repair must consider retirement and staged
ownership; it must not reconstruct free space from only the active namespace.
Disk drain/removal, replacement, redundancy changes and Autopilot must walk
the global object/placement index, wait for pins and verify all retained
objects remain recoverable. Failed maintenance must not discard history to
make room. Retire superseded encodings using section 5.

Metadata member repair, segmented growth and topology joint-quorum transfer
must copy/validate the entire reachable DAG (including snapshot, clone and GC
roots and pool-control trees), preserving shared logical identity and the
reference ledger while translating member-local addresses. A live-only
tree export/reimport is insufficient. Fenced old arenas become reclaimable
only after the certified manifest transition and reader/recovery barriers.
Restore/copy verification compares all retained namespace/content identities,
not merely the current image. No topology or migration path may bypass the
snapshot-aware reference accounting.

Reserve the following logical schema in final V3 (numbers are design
allocations, not yet accepted by the experimental decoder):

| Domain | Key suffix | Record purpose |
| --- | --- | --- |
| 14 Snapshot (existing reservation) | snapshot ID; separately tagged binary name | immutable view descriptor, name mapping, creation tx/time, optional lineage |
| 17 View | view ID | writable namespace root set and view-local configuration |
| 18 Object | object ID | immutable content identity, current encoding/placement |
| 19 ObjectRef | object ID | checked logical reference count |
| 20 BlockNodeRef | logical node ID | checked incoming count and authenticated node identity |
| 21 Retirement | typed retirement ID / cursor | root edges, old placement generations, GC ownership and progress |
| 22 KeyRegistry | key ID | encoding/key lifecycle metadata, never unprotected secret material |
| 23 Lifecycle | tagged control key | monotonic ID counters, request receipts, retention/GC state |

Use existing versioned record framing for ordinary values; graph edges need
explicit wire-level types. Numeric key components remain big-endian, binary
names length-bounded and collision-free. The reserved domain IDs are not
compatible with the current materializing adapter merely because it ignores
some unknown domains. Reserve incompatible feature bit 3 for snapshot-aware
view/object/edge semantics (bits 0..2 are already required); final V3 must
require it even with zero snapshots, because data ownership rules differ.
Set the corresponding required format negotiation at superblock and certified
root before exposing any writer. Old experimental readers must reject the new
format, not ignore snapshot records or fall back to an older root. Do not
enable the bit until all readers/writers/recovery paths implement it.
Do not freeze page tags, record byte layouts or bit assignments without codec
tests; these are explicit allocations for the current design, not a shipped
format. No production pool has been promised compatibility with experimental V3.

Legacy numeric/raw migration imports the authenticated live image into a
single active view, gives every source block an object identity, constructs
ownership and reverse indexes, checks physical extent identity/overlap, and
verifies all content mappings before superblock publication. Legacy JSON
snapshots remain diagnostic exports; their historical data cannot be inferred
or imported safely. Never overwrite the source before verified staging and
crash-safe activation. The experimental V3 materializing adapter must be
replaced or explicitly refuse snapshot-aware pools; it cannot silently drop
extra roots when re-encoding a live image.

## 8. Implementation order and release gates

1. Finalize the root-set/typed-edge codec and logical Block-node identity
   plan; implement DAG-aware physical allocator, audit, repair and transfer.
   Add format negotiation and rejection tests before permitting new records.
2. Implement object mapping/placement indirection and certified logical
   reference ledger, including migration and all data-plane mutation paths.
   Replace live-only member reference indexes and allocator reclamation.
3. Implement durable retirement queues, recovery/reader fencing, bounded GC,
   space reservations and exact cross-layer audit/rebuild. Integrate with
   disk-resident runtime, dynamic segments and joint-quorum topology.
4. Implement snapshot create/list/open/delete, explicit rollback and
   independent shared-root clone lifecycle; provide CLI/FUSE/admin routing
   through the actual writer. Reserve transport features without claiming
   send/receive is already shipped.
5. Qualify end-to-end before enabling snapshot-aware V3 by default.

Required tests (both single-member and redundant pools, including degraded
configurations and real raw/loop volume integration):

- Snapshot before overwrite/truncate/hole punch/unlink/rename/hard-link/
  xattr/ACL changes; compare historical and active namespace/content byte
  for byte. Multiple snapshots and clone divergence, delete origins and
  rollback without rewinding pool state. Dirty writeback barrier tests.
- Differential randomized DAG/ledger checks against a full reference model
  after every operation; shared staged nodes, split/merge, multiple identical
  logical contents, stale identity, underflow/overflow and cycles; exact
  metadata physical counts on each member, and exact raw allocation audit.
- Crash/fault at data write, metadata body, allocator, root, certificate,
  name removal, retirement ownership transfer, GC batch and free-space
  publication; reopen old/new certified state only. Lost acknowledgements,
  ENOSPC, corrupt queues, concurrent reader/deletion and stale-member
  rejoin. Verify no prematurely reused extent and no unbounded leaks after
  successful GC and quiescence.
- Snapshot-only object corruption and repair, disk loss/replacement/drain,
  reshape, encryption rotation, segmented growth and repeated topology
  transitions. Verify all retained views after every operation. Test obsolete
  root refusal, old-binary refusal and resumable migration cut points.
- Measure 294k, 1M and larger inode fixtures with many snapshots. Creation
  and clone should be proportional to the fixed root set and catalog/ledger
  tree height, independent of total namespace size; zero data copies and no
  full traversal. Unchanged snapshot count must not multiply ordinary write
  work. Measure physical metadata and data amplification, mount I/O, bounded
  dirty memory, GC batch latency and eventual free-space convergence.
  Include randomized mixed ztest with history byte oracles and topology
  changes; standalone engine measurements are not end-to-end qualification.

The final format is gated on correctness of the ownership model and these
tests, not just availability of a snapshot command. All above mechanisms are design requirements. The prototype implements
typed snapshot edges and physical pinning only; final-format object
ownership, retirement and maintenance remain release gates.
