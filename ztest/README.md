# argosfs-ztest

`argosfs-ztest` is a ztest-style crash and fault-injection stress harness for
ArgosFS. It runs a concurrent namespace/data workload, kills the workload
process with `SIGKILL` at random points, recovers the pool, verifies every
durably committed file against a host-side oracle, and repairs/verifies parity.

It is intentionally a separate, workspace-excluded package. Nothing outside the
small `#[doc(hidden)]` backend-open hook in ArgosFS needs to know it exists, so
the harness can move to its own repository or distribution package later.

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

Fault decisions are deterministic from `--seed`. Injection is disabled while
the pool is opened and only armed after mount, so recovery itself is clean; the
crash is what exercises recovery. By default, durable read/write bit rot is
restricted to the data region, and metadata/journal write faults run at a lower
rate so one injected metadata error does not immediately fence the whole pool.

## What it verifies

- every crash reopens the pool and runs `scrub()`;
- `unrecoverable_files` must be zero and `scrub()` must report no errors;
- every file in the host-side oracle must exist and match its BLAKE3 digest;
- after the random cycles, a deterministic parity drill damages one committed
  data shard in a backing image and requires scrub to report a repaired file
  (`damaged_files >= 1`, `repaired_files >= 1`) without losing the file.

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
one. The JSON summary prints the seed and fault counts. Artifacts (images,
per-cycle worker logs, fault event log, oracle) are retained on failure and when
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
