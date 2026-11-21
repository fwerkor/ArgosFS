//! The inner kernel: multiply a shard by a field scalar and accumulate.
//!
//! Every encode and every decode in this crate is a matrix-vector product over
//! GF(2^8) whose "elements" are whole shards, and the operations below are the
//! entirety of its inner loop. Essentially all of the runtime is here.
//!
//! There are three of them, and the third is a fusion of the first two rather
//! than anything new:
//!
//! ```text
//! mul_slice(c, src, dst)         dst  = c * src
//! mul_add_slice(c, src, dst)     dst ^= c * src
//! mul_rows(matrix, k, srcs, outs)  outs[t] = sum over j of matrix[t*k + j] * srcs[j]
//! ```
//!
//! `mul_rows` exists because encode drives many output rows over the same
//! sources, and an implementation free to see all of them at once can hold
//! several rows' accumulators in registers and split each source byte's nibbles
//! once for all of them instead of once per row. On aarch64 that is worth 1.19x
//! at 4+4, 1.32x at 8+4 and about 1.45x from 16+8 up. It blocks only in groups
//! of eight and four, and computes the one to three rows that can be left over
//! the old way; `neon.rs` has the accounting and the measurements that set that
//! rule, including the narrower groups it rejected for being slower than what
//! they replaced. Every other implementation of it is a plain loop over the
//! first two operations, which is what the codec used to run itself, so the
//! differential oracle is exact. Decode does *not* use it: at one or two
//! erasures there is nothing to block over.
//!
//! [`scalar`] is the reference implementation: written for obvious
//! correctness, never optimized, never deleted. Every other implementation in
//! this module must produce byte-identical output, which the `tests` module at
//! the bottom of this file enforces over every scalar, every shard length in a
//! range covering all alignment and tail cases, every parity count that
//! reaches a different blocking path, and every kernel the machine running the
//! tests supports.

use crate::shards::ShardSource;

pub(crate) mod scalar;

// The split-nibble tables exist to feed a SIMD byte shuffle and have no other
// reader: the scalar reference multiplies through `gf`. On a target with no
// kernel below they are genuinely dead code, and rustc says so. The condition
// is the union of the kernel conditions that follow, so a new kernel adds its
// architecture here as well as on its own module.
#[cfg(any(target_arch = "aarch64", target_arch = "x86", target_arch = "x86_64"))]
pub(crate) mod tables;

#[cfg(target_arch = "aarch64")]
mod neon;

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
mod x86;

/// Which implementation of the inner kernel to use.
///
/// A `Kernel` can only be obtained from [`Kernel::SCALAR`], [`Kernel::detect`]
/// or [`Kernel::supported`], every one of which returns something the current
/// CPU can actually execute. That is a soundness requirement, not a
/// convenience: the SIMD kernels are `unsafe` and rely on the ISA being
/// present, so the variants are deliberately not public to construct.
///
/// ```
/// use reed_solomon_engine::Kernel;
///
/// // Whatever this machine does best.
/// let best = Kernel::detect();
///
/// // Everything this build can run here, most capable first. Useful for
/// // benchmarking one against another, and for forcing the reference.
/// for kernel in Kernel::supported() {
///     println!("{}", kernel.name());
/// }
/// assert!(Kernel::supported().any(|k| k == Kernel::SCALAR));
/// # let _ = best;
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Kernel(Isa);

/// The private inner enum. Kept private so that a `Kernel` naming an ISA the
/// CPU lacks cannot be constructed from outside this crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Isa {
    Scalar,
    #[cfg(target_arch = "aarch64")]
    Neon,
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    Ssse3,
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    Avx2,
}

/// Every kernel compiled into this build, most capable first. Some may not be
/// runnable on the current CPU; [`Kernel::is_supported`] is the filter.
///
/// The order is the preference order: [`Kernel::detect`] takes the first entry
/// that passes the filter, so AVX2 beats SSSE3 and both beat the reference.
const COMPILED_IN: &[Kernel] = &[
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    Kernel(Isa::Avx2),
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    Kernel(Isa::Ssse3),
    #[cfg(target_arch = "aarch64")]
    Kernel(Isa::Neon),
    Kernel(Isa::Scalar),
];

impl Kernel {
    /// The portable scalar reference.
    ///
    /// Always available, on every target. This is the specification: if a SIMD
    /// kernel ever disagrees with it, the SIMD kernel is wrong.
    pub const SCALAR: Kernel = Kernel(Isa::Scalar);

    /// The fastest kernel this build contains that the current CPU can run.
    ///
    /// On x86 the preference order is AVX2, then SSSE3, then
    /// [`SCALAR`](Self::SCALAR).
    ///
    /// On aarch64 and on `no_std` targets this resolves at compile time and
    /// costs nothing. On x86 with the `std` feature it consults
    /// `is_x86_feature_detected!`, which caches its answer after the first
    /// call. Without `std` there is no runtime CPU detection available, so
    /// selection falls back to whatever `-C target-feature` enabled at compile
    /// time, and building with `-C target-feature=+avx2` or a `-C target-cpu`
    /// that implies it is how a `no_std` caller opts in.
    ///
    /// A `no_std` build with no such flag gets whatever the target's baseline
    /// declares, which is not always [`SCALAR`](Self::SCALAR): the
    /// `x86_64-unknown-linux-gnu` and `*-pc-windows-*` targets stop at SSE2
    /// and do get scalar, but `x86_64-apple-darwin` has SSSE3 in its baseline
    /// and gets the SSSE3 kernel.
    ///
    /// Note that `-C target-feature` overrides detection under `std` too, not
    /// just under `no_std`. `is_x86_feature_detected!` expands to
    /// `cfg!(target_feature = ...) || <runtime check>`, so the flag short
    /// circuits CPUID. That is correct, because the flag is a promise about
    /// the machine that LLVM has already acted on crate-wide, but it means
    /// `std` is not a safety net against building for a CPU you do not have.
    #[inline]
    pub fn detect() -> Self {
        detect_arch().unwrap_or(Kernel::SCALAR)
    }

    /// Every kernel this build contains that the current CPU can run, most
    /// capable first.
    ///
    /// Always yields at least [`SCALAR`](Self::SCALAR).
    #[inline]
    pub fn supported() -> impl Iterator<Item = Kernel> + Clone {
        COMPILED_IN.iter().copied().filter(|k| k.is_supported())
    }

    /// Whether the current CPU can run this kernel.
    #[inline]
    pub fn is_supported(self) -> bool {
        match self.0 {
            Isa::Scalar => true,
            #[cfg(target_arch = "aarch64")]
            Isa::Neon => cfg!(target_feature = "neon"),
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            Isa::Ssse3 => x86::ssse3_supported(),
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            Isa::Avx2 => x86::avx2_supported(),
        }
    }

    /// A short stable name, for benchmark output and error messages.
    #[inline]
    pub const fn name(self) -> &'static str {
        match self.0 {
            Isa::Scalar => "scalar",
            #[cfg(target_arch = "aarch64")]
            Isa::Neon => "neon",
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            Isa::Ssse3 => "ssse3",
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            Isa::Avx2 => "avx2",
        }
    }

    /// `dst = c * src`. `dst` and `src` must have the same length.
    // The crate is `deny(unsafe_code)`; this and `mul_add_slice` are the only
    // two places outside a kernel module that name `unsafe`, and only to enter
    // one. The safety argument is on each arm.
    #[allow(unsafe_code)]
    #[inline]
    pub(crate) fn mul_slice(self, c: u8, src: &[u8], dst: &mut [u8]) {
        debug_assert_eq!(src.len(), dst.len());
        match self.0 {
            Isa::Scalar => scalar::mul_slice(c, src, dst),
            #[cfg(target_arch = "aarch64")]
            // SAFETY: `Isa::Neon` is unconstructible outside this module, and
            // the only paths that produce it (`detect` and `supported`) gate
            // on `is_supported`, which requires `target_feature = "neon"`.
            Isa::Neon => unsafe { neon::mul_slice(c, src, dst) },
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            // SAFETY: `Isa::Ssse3` is unconstructible outside this module, and
            // the only paths that produce it (`detect` and `supported`) gate
            // on `is_supported`, which returns true only when
            // `x86::ssse3_supported` has confirmed the CPU has SSSE3.
            Isa::Ssse3 => unsafe { x86::ssse3::mul_slice(c, src, dst) },
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            // SAFETY: as above, via `x86::avx2_supported`.
            Isa::Avx2 => unsafe { x86::avx2::mul_slice(c, src, dst) },
        }
    }

    /// `dst ^= c * src`. `dst` and `src` must have the same length.
    #[allow(unsafe_code)]
    #[inline]
    pub(crate) fn mul_add_slice(self, c: u8, src: &[u8], dst: &mut [u8]) {
        debug_assert_eq!(src.len(), dst.len());
        match self.0 {
            Isa::Scalar => scalar::mul_add_slice(c, src, dst),
            #[cfg(target_arch = "aarch64")]
            // SAFETY: as above.
            Isa::Neon => unsafe { neon::mul_add_slice(c, src, dst) },
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            // SAFETY: as above.
            Isa::Ssse3 => unsafe { x86::ssse3::mul_add_slice(c, src, dst) },
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            // SAFETY: as above.
            Isa::Avx2 => unsafe { x86::avx2::mul_add_slice(c, src, dst) },
        }
    }

    /// `outs[t] = sum over j of matrix[t * data_shards + j] * srcs.shard(j)`,
    /// for every `t`.
    ///
    /// `matrix` holds exactly `outs.len() * data_shards` bytes, row-major.
    /// `srcs` holds exactly `data_shards` shards, and every source shard and
    /// every output has the same length.
    ///
    /// This **assigns**: whatever was in `outs` is ignored, not accumulated
    /// into. That mirrors what the row-major loop does by assigning its first
    /// term rather than zeroing the output first.
    #[allow(unsafe_code)]
    #[inline]
    pub(crate) fn mul_rows<S: ShardSource + ?Sized, O: AsRef<[u8]> + AsMut<[u8]>>(
        self,
        matrix: &[u8],
        data_shards: usize,
        srcs: &S,
        outs: &mut [O],
    ) {
        debug_assert_ne!(data_shards, 0);
        debug_assert_eq!(matrix.len(), outs.len() * data_shards);
        debug_assert_eq!(srcs.count(), data_shards);
        debug_assert!(
            (0..data_shards).all(|j| srcs.shard(j).len() == srcs.shard(0).len()),
            "every source shard has the same length"
        );
        debug_assert!(
            outs.iter().all(|o| o.as_ref().len() == srcs.shard(0).len()),
            "every output has the same length as the sources"
        );

        match self.0 {
            Isa::Scalar => scalar::mul_rows(matrix, data_shards, srcs, outs),
            #[cfg(target_arch = "aarch64")]
            // SAFETY: as above.
            Isa::Neon => unsafe { neon::mul_rows(matrix, data_shards, srcs, outs) },
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            // SAFETY: as above.
            Isa::Ssse3 => unsafe { x86::ssse3::mul_rows(matrix, data_shards, srcs, outs) },
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            // SAFETY: as above.
            Isa::Avx2 => unsafe { x86::avx2::mul_rows(matrix, data_shards, srcs, outs) },
        }
    }
}

impl Default for Kernel {
    /// [`Kernel::detect`], so that `Kernel::default()` is the fast path rather
    /// than the reference one.
    #[inline]
    fn default() -> Self {
        Self::detect()
    }
}

impl core::fmt::Display for Kernel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name())
    }
}

/// The best kernel for this architecture, or `None` for the reference.
///
/// The arms below are mutually exclusive and between them cover every target,
/// so exactly one is compiled whatever the build.
///
/// NEON is mandatory on aarch64, so there is nothing to ask and nothing to
/// branch on.
#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
#[inline]
fn detect_arch() -> Option<Kernel> {
    Some(Kernel(Isa::Neon))
}

/// Neither SSSE3 nor AVX2 is in any x86 baseline this crate can assume, so
/// both are asked about, widest first.
///
/// Every answer comes from [`Kernel::is_supported`], which is what makes the
/// result sound: this function cannot name a kernel that same check would
/// reject.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[inline]
fn detect_arch() -> Option<Kernel> {
    if Kernel(Isa::Avx2).is_supported() {
        Some(Kernel(Isa::Avx2))
    } else if Kernel(Isa::Ssse3).is_supported() {
        Some(Kernel(Isa::Ssse3))
    } else {
        None
    }
}

/// Every other target has no SIMD kernel here yet.
#[cfg(not(any(
    all(target_arch = "aarch64", target_feature = "neon"),
    target_arch = "x86",
    target_arch = "x86_64"
)))]
#[inline]
fn detect_arch() -> Option<Kernel> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gf;

    /// Shard lengths that cover every vector width, every tail remainder and
    /// the degenerate ends.
    const LENGTHS: &[usize] = &[
        1, 2, 3, 7, 8, 15, 16, 17, 31, 32, 33, 47, 48, 49, 63, 64, 65, 95, 96, 127, 128, 129, 255,
        256, 257, 1200,
    ];

    /// Parity counts that reach every group width a blocked kernel peels, and
    /// the boundaries between them: 8, 4, 2 and 1, each exactly, each one over
    /// and each one under.
    const PARITY_COUNTS: &[usize] = &[1, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17];

    /// Source counts swept alongside [`PARITY_COUNTS`]. One is the degenerate
    /// case where the first term is the only term.
    ///
    /// These are *rotated* through the parity and length sweeps rather than
    /// crossed with them; [`data_count_for`] says why.
    const DATA_COUNTS: &[usize] = &[1, 2, 5];

    /// The source count to use at a given point in the parity-by-length sweep.
    ///
    /// Crossing all three source counts with all eleven parity counts and all
    /// twenty-six lengths triples the work and reaches no path the rotation
    /// misses: `LENGTHS` has 26 entries, so for every parity count the rotation
    /// still hits all three source counts, and every source count is still seen
    /// at every length. What it does not do is run the vector loops three times
    /// over, which matters because CI interprets these two tests under Miri on
    /// a macOS runner, where the full cross costs about 37 minutes against 12.
    fn data_count_for(parity_index: usize, length_index: usize) -> usize {
        DATA_COUNTS[(parity_index + length_index) % DATA_COUNTS.len()]
    }

    fn pattern(len: usize, seed: u8) -> std::vec::Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    /// A pseudo-random coefficient matrix, `rows * cols` bytes, row-major.
    ///
    /// Deliberately *not* the real parity matrix: these tests are about the
    /// kernel reproducing the reference for arbitrary coefficients, and pulling
    /// in `matrix.rs` would make a kernel bug and a matrix bug able to hide
    /// each other.
    ///
    /// Zero and one are salted in rather than left to the generator. They take
    /// shortcut paths in the scalar reference and no shortcut at all in the
    /// blocked NEON path, so they are exactly where the two can disagree, and a
    /// random byte lands on them twice in 256.
    fn coefficients(rows: usize, cols: usize, seed: u64) -> std::vec::Vec<u8> {
        let mut state = seed | 1;
        (0..rows * cols)
            .map(|i| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                match i % 11 {
                    0 => 0,
                    5 => 1,
                    _ => (state >> 24) as u8,
                }
            })
            .collect()
    }

    /// `data_shards` distinct source shards of `len` bytes.
    fn sources(data_shards: usize, len: usize) -> std::vec::Vec<std::vec::Vec<u8>> {
        (0..data_shards).map(|j| pattern(len, (j as u8).wrapping_mul(53).wrapping_add(3))).collect()
    }

    #[test]
    fn every_supported_kernel_matches_the_scalar_reference() {
        for kernel in Kernel::supported() {
            for &len in LENGTHS {
                let src = pattern(len, 7);
                let initial = pattern(len, 199);

                for c in 0..=255u8 {
                    let mut expected = initial.clone();
                    let mut actual = initial.clone();

                    scalar::mul_slice(c, &src, &mut expected);
                    kernel.mul_slice(c, &src, &mut actual);
                    assert_eq!(actual, expected, "mul_slice: {kernel} c={c} len={len}");

                    let mut expected = initial.clone();
                    let mut actual = initial.clone();
                    scalar::mul_add_slice(c, &src, &mut expected);
                    kernel.mul_add_slice(c, &src, &mut actual);
                    assert_eq!(actual, expected, "mul_add_slice: {kernel} c={c} len={len}");
                }
            }
        }
    }

    #[test]
    fn every_supported_kernel_computes_the_field_product() {
        // Independent of the scalar reference: compare against `gf::mul`
        // directly, so a kernel and the reference cannot be wrong together.
        for kernel in Kernel::supported() {
            let src: std::vec::Vec<u8> = (0..=255u8).collect();
            for c in 0..=255u8 {
                let mut dst = std::vec![0u8; src.len()];
                kernel.mul_slice(c, &src, &mut dst);
                for (i, &s) in src.iter().enumerate() {
                    assert_eq!(dst[i], gf::mul(c, s), "{kernel} c={c} src={s}");
                }
            }
        }
    }

    #[test]
    fn kernels_do_not_write_past_the_end() {
        // A vectorized kernel that rounds the length up would corrupt the
        // bytes after `dst`. Give it a canary and check it survives. Both
        // operations are checked: they have separate loops, so an overrun in
        // one says nothing about the other. So are the scalars that take the
        // shortcut paths, since those write `dst` without going near the
        // vector loops at all.
        for kernel in Kernel::supported() {
            for &len in LENGTHS {
                let src = pattern(len, 11);
                for c in [0u8, 1, 0x9D] {
                    let mut buffer = std::vec![0xEDu8; len + 64];
                    let (dst, canary) = buffer.split_at_mut(len);
                    dst.fill(0);
                    kernel.mul_slice(c, &src, dst);
                    assert!(
                        canary.iter().all(|&b| b == 0xED),
                        "{kernel} mul_slice overran at len={len} c={c}"
                    );

                    let mut buffer = std::vec![0xEDu8; len + 64];
                    let (dst, canary) = buffer.split_at_mut(len);
                    dst.fill(0);
                    kernel.mul_add_slice(c, &src, dst);
                    assert!(
                        canary.iter().all(|&b| b == 0xED),
                        "{kernel} mul_add_slice overran at len={len} c={c}"
                    );
                }
            }
        }
    }

    #[test]
    fn every_supported_kernel_matches_the_scalar_reference_for_multiple_rows() {
        // `mul_rows` is a second loop shape, not a second spelling of the first
        // one: on aarch64 it blocks several output rows together and peels
        // groups of 8, 4, 2 and 1. Every one of those paths, at every tail
        // remainder, has to reproduce the scalar driver byte for byte.
        for kernel in Kernel::supported() {
            for (parity_index, &parity_shards) in PARITY_COUNTS.iter().enumerate() {
                for (length_index, &len) in LENGTHS.iter().enumerate() {
                    let data_shards = data_count_for(parity_index, length_index);
                    let matrix = coefficients(parity_shards, data_shards, 0x9E3779B97F4A7C15);
                    let owned = sources(data_shards, len);
                    let srcs: std::vec::Vec<&[u8]> =
                        owned.iter().map(std::vec::Vec::as_slice).collect();

                    // Pre-filled with something other than zero, because
                    // `mul_rows` assigns rather than accumulates and a path that
                    // forgot to overwrite a byte would be invisible against a
                    // zeroed buffer.
                    let mut expected: std::vec::Vec<std::vec::Vec<u8>> =
                        (0..parity_shards).map(|t| pattern(len, t as u8)).collect();
                    let mut actual = expected.clone();

                    let mut outs: std::vec::Vec<&mut [u8]> =
                        expected.iter_mut().map(|o| o.as_mut_slice()).collect();
                    scalar::mul_rows(&matrix, data_shards, &srcs[..], &mut outs);

                    let mut outs: std::vec::Vec<&mut [u8]> =
                        actual.iter_mut().map(|o| o.as_mut_slice()).collect();
                    kernel.mul_rows(&matrix, data_shards, &srcs[..], &mut outs);

                    assert_eq!(
                        actual, expected,
                        "mul_rows: {kernel} {data_shards}+{parity_shards} len={len}"
                    );
                }
            }
        }
    }

    #[test]
    fn multi_row_kernels_do_not_write_past_the_end() {
        // The single-output canary above says nothing about this shape. A
        // kernel that computes eight rows at once has eight chances to round a
        // length up, and seven of them are in rows the narrower test never
        // creates. Every output gets its own canary, and the group widths and
        // tail remainders swept are the same ones the differential test uses.
        const CANARY: usize = 64;

        for kernel in Kernel::supported() {
            for (parity_index, &parity_shards) in PARITY_COUNTS.iter().enumerate() {
                for (length_index, &len) in LENGTHS.iter().enumerate() {
                    let data_shards = data_count_for(parity_index, length_index);
                    let matrix = coefficients(parity_shards, data_shards, 0xD1B54A32D192ED03);
                    let owned = sources(data_shards, len);
                    let srcs: std::vec::Vec<&[u8]> =
                        owned.iter().map(std::vec::Vec::as_slice).collect();

                    let mut buffers: std::vec::Vec<std::vec::Vec<u8>> =
                        (0..parity_shards).map(|_| std::vec![0xEDu8; len + CANARY]).collect();
                    {
                        let mut outs: std::vec::Vec<&mut [u8]> =
                            buffers.iter_mut().map(|b| &mut b[..len]).collect();
                        kernel.mul_rows(&matrix, data_shards, &srcs[..], &mut outs);
                    }
                    for (t, buffer) in buffers.iter().enumerate() {
                        assert!(
                            buffer[len..].iter().all(|&b| b == 0xED),
                            "{kernel} mul_rows overran output {t} of {parity_shards} \
                             at len={len} data_shards={data_shards}"
                        );
                    }
                }
            }
        }
    }

    /// The kernels the environment insists this machine can run, as a
    /// comma-separated list of [`Kernel::name`] values.
    ///
    /// Every test above sweeps [`Kernel::supported`], so a machine that does
    /// not have an ISA simply does not test it, quietly. That is right for a
    /// developer's laptop and wrong for CI: a runner that loses AVX2, or a
    /// detection bug that stops reporting it, would drop a kernel from the
    /// sweep and every test would go on passing over what remained. Setting
    /// this variable turns that silent loss of coverage into a failure.
    const REQUIRED_KERNELS: &str = "RS_REQUIRE_KERNELS";

    #[test]
    fn the_kernels_the_environment_requires_are_all_supported() {
        // Unset, as on any ordinary machine, this test asserts nothing.
        let Ok(required) = std::env::var(REQUIRED_KERNELS) else { return };

        let available: std::vec::Vec<&str> = Kernel::supported().map(|k| k.name()).collect();
        for name in required.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            assert!(
                available.contains(&name),
                "{REQUIRED_KERNELS} requires {name}, but this machine supports only {available:?}"
            );
        }
    }

    #[test]
    fn detect_is_supported_and_scalar_is_always_available() {
        assert!(Kernel::detect().is_supported());
        assert!(Kernel::SCALAR.is_supported());
        assert!(Kernel::supported().any(|k| k == Kernel::SCALAR));
        assert!(Kernel::supported().all(|k| k.is_supported()));

        // `COMPILED_IN` and `detect_arch` are two hand-maintained lists of the
        // same kernels, and every other way of adding a kernel wrong is a
        // compile error: a new `Isa` variant alone fails four exhaustive
        // matches, and a duplicate `detect_arch` arm is a duplicate
        // definition. Forgetting `COMPILED_IN` is the one silent mistake, and
        // it is a bad one, because `supported()` is what the three sweeps
        // above iterate. A kernel missing from it becomes the default kernel
        // while being covered by no differential test at all.
        assert!(
            Kernel::supported().any(|k| k == Kernel::detect()),
            "detect() chose {}, which COMPILED_IN does not list, so the \
             default kernel is not swept by any cross-kernel test",
            Kernel::detect()
        );
    }
}
