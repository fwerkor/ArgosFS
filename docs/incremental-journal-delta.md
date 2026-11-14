# Issue #212: bounded-memory journal delta construction

The previous group-commit path serialized the entire previous and current
metadata into two serde_json::Value trees on every delta commit.

Delta construction now compares metadata at three levels:

1. The non-namespace header is processed independently.
2. Disk records are compared one at a time.
3. Inode records are compared one at a time, generating the same JSON pointer
   operations as the former recursive full-tree diff.

The initial #212 stage retained the v1 on-disk journal format and replay
semantics; it removed two large transient JSON trees. Since then,
`TrackedInodes` supplies a copy-on-write, dirty-key baseline for normal group
commits. Independent or recovered metadata snapshots still fall back to a
full inode-key scan for correctness. Allocator free-extents and the bounded
header/disk records are handled separately and may need a full scan when they
change.

Legacy canonical metadata hashing still materializes additional metadata
copies, so its peak RSS can remain above the steady-state table. The opt-in
v2 raw integrity scheme below avoids this legacy full-tree hash per normal
deferred group commit.

## Raw delta-integrity v2 (Issue #230; explicit upgrade)

The raw/loop metadata journal can authenticate each deferred transaction using
`SHA-256("ArgosFS/raw-delta-integrity/v2\\0" || previous_digest ||
volume_uuid || txid || generation || length || ordered_delta)`. The delta
contains the entire changed header/disk/inode information; its self-referential
`/integrity` fields are excluded from the digest, then checked after replay.
This binds a transaction to its verified predecessor without serializing the
unchanged inode table at each group commit. A checkpoint's physical per-page
SHA-256 tree and a quorum of copies provide the trusted starting state.

This protocol is **not silently activated on existing pools**. It requires the
`raw-delta-integrity-v2` feature bit (`required_feature_flags` bit 0) in the
primary and backup superblocks of **every active** member. Older binaries
reject the required bit and therefore cannot continue the chain using the
legacy hash algorithm. Journal replay checks v2 digests and rejects a v2
candidate without a quorum of upgraded members. Other, unknown hash schemes
fail closed.

### Offline, one-way upgrade

1. Stop all mounts and writers to the pool, and back up all important data.
   Verify the pool's member list and run `argosfs fsck` beforehand.
2. Use the **new** binary and provide **all active** devices/images, e.g.:

   ```sh
   argosfs upgrade-integrity --pool-config /etc/argosfs/root-pool.json --offline-confirmed
   ```

   The command first syncs and checkpoints the last legacy state. It then
   writes and flushes each member's **backup** superblock and **primary**
   superblock with the mandatory feature, verifying each write by rereading.
   No v2 transaction is issued until all members are confirmed upgraded.
3. Verify `argosfs inspect-pool` and `argosfs verify-journal` using the new
   binary. Resume mounts only after the command succeeds. **Do not downgrade
   the binary** or clear required-feature bits as a rollback strategy.

If power is lost or a member fails mid-upgrade, no v2 transaction has yet been
issued. Keep the new binary, restore all members, and **rerun the same upgrade
command**; the feature writes are idempotent. A backup and the original member
set are necessary to recover from a physical loss that prevents quorum. The
`--offline-confirmed` flag is an operator assertion, not a mechanism to stop an
unrelated process that already has a device open.

The v2 chain can alternate with legacy canonical full checkpoints: physical
checkpoint recovery never relies solely on a delta chain, and mount-time
metadata normalization forces a one-time full checkpoint before continuing
v2 transactions. Normal deferred group commits then hash only changed records.
Strict synchronous writes and explicit full snapshots retain their original
canonical-hash behavior. V2 does not eliminate the cost of physical checkpoint
serialization or copies of large raw allocator free lists.
