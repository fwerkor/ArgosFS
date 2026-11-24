# Native erasure engine migration (temporary)

## Completed (2026-11-24)

- [x] Inspect codec, license, format and call sites; retain upstream MIT/Apache notices and wire specification.
- [x] Integrate the GF(256)/0x11D arithmetic, small-erasure solver and existing scalar/SSSE3/AVX2/NEON kernels; remove the standalone crate API, vendor source and production dependency.
- [x] Add runtime-checked AVX-512BW, GFNI-256 and GFNI-512. GFNI uses the linear affine matrix for each 0x11D scalar, not AES-field multiplication.
- [x] Tile x86 parity computation and recovery to improve cache residency; reuse write geometry, move owned shard buffers, and avoid allocating missing parity for data-only reads.
- [x] Cache the last reduced recovery system by geometry and presence mask, per worker; allocate only for the actual erasure count.
- [x] Local build, Clippy with warnings denied, legacy differential tests (12 passed), kernel/matrix/field tests (29 passed, 1 exhaustive slow test ignored before final tiling), cache-plan regression and tile boundary tests.
- [x] Hardware validation on Intel Xeon Gold 6138 (AVX-512BW) and AMD Ryzen AI Max+ 395 (GFNI-256/GFNI-512), including 14 kernel tests on each after tiling.
- [x] Isolated baseline comparison and reproducible benchmark script; verify package file list includes native implementation and legal notices.
- [x] Update README, release workflow, Rust CI, contributing and permanent codec documentation. Rust 1.89 minimum also matches the locked UUID dependency.

## Measured single-thread encode throughput (GiB/s)

Baseline: vendored reed-solomon-engine 0.2.0 AVX2 from
`2a344164b4bb3df79e9851aa50a140cf4c1e07c9`.
Optimized builds: rustc 1.94.0, opt-level 3, three 45 ms samples, median,
same core and preallocated buffers, encode throughput based on data bytes.

| CPU | Layout | Shard | Baseline | New | Backend |
| --- | --- | --- | ---: | ---: | --- |
| Ryzen AI Max+ 395 | 2+1 | 4 KiB | 50.340 | 110.877 | GFNI-512 |
| Ryzen AI Max+ 395 | 4+2 | 4 KiB | 38.372 | 55.169 | GFNI-512 |
| Ryzen AI Max+ 395 | 8+4 | 1 MiB | 14.438 | 23.662 | GFNI-512 |
| Ryzen AI Max+ 395 | 16+8 | 1 MiB | 7.307 | 11.608 | GFNI-512 |
| Ryzen AI Max+ 395 | 16+8 | 1 MiB | 7.307 | 13.472 | AVX-512BW |
| Xeon Gold 6138 | 16+8 | 1 MiB | 1.213 | 3.187 | AVX-512BW |

These are kernel microbenchmarks, not disk or filesystem throughput.
On memory-bound shapes a wider ISA is not always fastest; the initial
dispatch is conservative feature-based selection, not per-layout tuning.

## Remaining optimization and acceptance

- [ ] Native ARM64 CI including NEON, native x86 CI, full library/integration,
  packaged-source parity oracle, raw pool reopen/recovery and fuzz gates.
- [ ] Verify minimum supported Rust and cross-architecture build/release
  packaging. Preserve the 0x11D format and codec IDs 0/1 without migration.
- [ ] Benchmark representative small/large stripes, cold and hot cache, and
  real file write, degraded read, repair, and mixed concurrency.
- [ ] Profile x86 GFNI/AVX-512BW/AVX2 and ARM NEON across geometries and sizes;
  tune tile size, backend selection, and evaluate fused multi-output kernels.
  Require reproducible gains and no material regressions before enabling.
- [ ] Evaluate safe incremental parity updates only with proven unchanged
  shard geometry/transformation and transactional recovery semantics.
- [ ] Review whether multi-entry recovery plans, parallel large-stripe
  execution, ARM SVE2 and more specialized paths justify their complexity.
- [ ] Remove this temporary progress document once the above gates and
  optimization work are complete; keep the permanent architecture, format,
  benchmarking and provenance documentation.

No disk format changes or unmeasured end-to-end throughput claims.
