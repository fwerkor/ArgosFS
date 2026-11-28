# argosfs-ztest

`argosfs-ztest` is a ztest-style crash and fault-injection stress harness for
ArgosFS. It runs a concurrent namespace/data workload, kills the workload
process with `SIGKILL` at random points, recovers the pool, verifies every
durably committed file against a host-side oracle, and repairs/verifies parity.

It is intentionally a separate, workspace-excluded package. Nothing outside the
small `#[doc(hidden)]` backend-open hook in ArgosFS needs to know it exists, so
the harness can move to its own repository or distribution package later.

Fresh test pools use paged metadata by default. Pass `run --legacy-metadata`
to exercise the original checkpoint/journal format; `--paged-metadata`
remains an explicit selection of the default.

## What it injects

The fault backend wraps the normal loop/raw backend:

| fault | behaviour |
|---|---|
| read EIO | returns `EIO` without reading |
| transient read flip | flips bits in the returned buffer only |
| durable bit rot | flips bits and writes the damaged bytes back to the image |
| write EIO | returns `EIO` without writing |
| write EIO after write | writes, then returns `EIO` (indeterminate commit) |
| write bit rot | writes silently corrupted bytes |
| torn write | persists only a sector-aligned prefix |
| lost write | silently discards the write |
| dropped flush | commits the volatile writes but skips the real barrier |
| volatile window | holds writes in a userspace write-back window; `SIGKILL` loses them |
| process kill | supervisor sends `SIGKILL` to the workload child |

Fault streams derive from `--seed` and cycle number; concurrent operation ordering may vary between runs. Injection is disabled while
the pool is opened and only armed after mount, so recovery itself is clean; the
crash is what exercises recovery. By default, durable read/write bit rot is
restricted to the data region, and metadata/journal write faults run at a lower
rate so one injected metadata error does not immediately fence the whole pool.

## What it verifies

- every crash reopens the pool and runs `scrub()`;
- damage to durable oracle files is a hard failure by default; exploratory
  runs with `--allow-expected-loss` may retire candidate over-budget losses;
- damage within the parity budget (`m` or fewer shards) must be repaired; an
  unrecoverable stripe in that case is a hard failure;
- every remaining file in the host-side oracle must exist and match its BLAKE3
  digest;
- after the random cycles, a deterministic parity drill damages one committed
  data shard in a backing image and requires scrub to report a repaired file
  (`damaged_files >= 1`, `repaired_files >= 1`) without losing the file.

### Expected loss

Strict oracle verification is the default. Pass `--allow-expected-loss` only
for exploratory fault-model runs, not durability acceptance. A fault log
records I/O locations, not the lifetime of an extent or proof that a later
write did not repair it, so attribution is heuristic.

A pool with `m` parity shards cannot reconstruct `m + 1` damaged shards in one
stripe. ztest matches scrub's unrecoverable `(inode, stripe)` results against
the cycle fault log and counts distinct shards hit by destructive faults
(`durable_bit_rot`, `write_bit_rot`, `torn_write`, `lost_write`). If more than
`m` were hit, the path is retired from the pool and from the oracle and listed
in the run summary's `expected_loss` array. Transient read faults and
indeterminate EIO-after-write events are not counted, so a recovery failure
within the parity budget still aborts the run.

The oracle is an epoch manifest stored **outside** the pool. An epoch is
published only after `ArgosFs::sync()` succeeds. A crash during the next epoch
therefore leaves the previous oracle intact, and extra uncommitted files are
allowed. Oracle publication happens before old-epoch deletion, so a crash in
the cleanup window can leave extra files but never a missing committed file.

## Run

```sh
cd ztest
cargo run --release -- run \
  --cycles 20 \
  --kill-after-ms-min 500 \
  --kill-after-ms-max 5000 \
  --min-faults 50
```

Fast smoke run:

```sh
cargo run --release -- run --cycles 2 --min-faults 1 --no-parity-drill
```

A typical nightly job can use a fixed seed for replay, or omit it to generate
one. The seed and artifact directory are printed at startup, and the JSON summary prints fault counts. An explicit `--work-dir` must not already exist. Artifacts (images,
per-cycle worker and fault event logs, oracle) are retained on failure and when
`--keep-work-dir` is set.

Useful fault-rate options are percentages: `--read-error-pct`,
`--read-flip-pct`, `--bit-rot-pct`, `--write-error-pct`,
`--write-eio-after-pct`, `--write-bit-rot-pct`, `--torn-write-pct`,
`--lost-write-pct`, `--drop-flush-pct`. `--bit-rot-metadata` extends durable bit
rot to metadata/journal regions; `--discard-pending-on-drop-flush` makes a
dropped flush also discard volatile writes, modelling a cache that lies about
what it persisted.

The default layout is a 3+2 pool on five 64 MiB loop images. `--devices`, `--k`,
`--m`, `--image-size`, and `--chunk-size` are configurable.

## Why the core hook exists

`StorageBackend` is already the natural seam for disk faults, but
`ArgosFs::open_loop` constructs its own `FileBlockBackend`. The harness uses
`ArgosFs::open_loop_with_backend` (and the raw equivalent), a `#[doc(hidden)]`
test seam that installs a wrapper before journal/checkpoint recovery. Normal
callers still use `open_loop`/`open_raw`; the wrapper cannot change on-disk
format, metadata, or the public CLI.

## Limits

- The workload uses the Rust volume API rather than a mounted FUSE session. It
  exercises the volume, journal, allocator, erasure codec, and repair paths, but
  not kernel FUSE request scheduling.
- `SIGKILL` loses userspace buffers and the simulated volatile window, not the
  host page cache. A dropped flush models a missing barrier, not storage that
  silently lies about its cache contents. Use
  `--discard-pending-on-drop-flush` when testing that stronger (and generally
  unrecoverable) failure model.
- The oracle verifies durable committed files. Scratch mutations under injected
  torn/lost writes are expected to fail or expose stale data; only the durable
  oracle is a correctness assertion.

## Strict model traces and recovery boundaries

The separate model workload checks the exact file contents and directory
entries after each operation, without treating arbitrary failures as legal.
It exercises overlapping/disjoint writes to one inode, competing renames,
hard links, symlinks, range writes, truncation, deletion and persisted
transaction ordering. The optional topology drill opens a degraded pool,
adds a larger replacement, removes the missing member and rebalances; every
modeled file must still match. Atomic rename/unlink drills kill a subprocess
at selected mutating I/O boundaries and accept only whole old/new states
before sync or the new state after sync, including hard-link identity.

```sh
cargo run --release -- model --work-dir /tmp/model-356 --seed 356 --operations 256 --topology
cargo run --release -- model --work-dir /tmp/model-replay --replay /tmp/model-356/trace.json --minimize
cargo run --release -- model --work-dir /tmp/model-legacy --legacy-metadata --seed 356 --topology
```

The model saves a versioned operation trace before execution; failed runs
retain their pool and logs. With `--minimize`, a bounded delta debugger
removes chunks of operations and accepts a reduction only when two fresh
pools reproduce the same failure category and root error. Seeds and traces
reproduce operation inputs and the selected crash boundary, not the exact
OS thread schedule; races remain nondeterministic.

`run --recovery-cuts 2 --recovery-eios 1 --reorder-pending` additionally
interrupts recovery itself, injects a one-shot EIO during recovery, and
allows independent buffered writes to reach media out of issue order.
Overlapping writes retain their order, and successful flushes drain the
pending window. Recovery faults are logged separately and an unreached
boundary cannot count as an injected fault. Repeated recovery culminates
in a clean reopen with a strict durable-data oracle, independent namespace
and allocator interval checks, and non-repairing fsck. The scratch model
also treats wrong data/read errors as fatal when no new fault has been
observed since the last verified state; ambiguous scratch mutations remain
exploratory and are not durable-data guarantees.

Both metadata formats are exercised by pull-request smoke tests and the
weekly randomized CI matrix. The model does not replace mounted FUSE tests.
