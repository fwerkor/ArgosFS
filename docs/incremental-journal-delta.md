# Issue #212: bounded-memory journal delta construction

The previous group-commit path serialized the entire previous and current
metadata into two serde_json::Value trees on every delta commit.

Delta construction now compares metadata at three levels:

1. The non-namespace header is processed independently.
2. Disk records are compared one at a time.
3. Inode records are compared one at a time, generating the same JSON pointer
   operations as the former recursive full-tree diff.

The on-disk journal format and replay semantics are unchanged. In particular,
the changed-record comparison still scans all B-tree keys and serializes each
inode to compare it. **This is not yet O(changed inodes) CPU time.** The goal
of this stage is to remove the two large transient JSON trees. O(changed)
journal construction requires exhaustive mutation tracking across metadata
maintenance, rename, xattrs, allocator and recovery paths.

Replay and canonical metadata hashing still materialize additional metadata
copies, so measured peak RSS may remain above the steady-state table.
