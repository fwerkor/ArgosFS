//! x86 and x86-64 kernels, using the `pshufb` family of byte shuffles.
//!
//! `pshufb` looks up 16 bytes in a 16-entry table in one instruction, which is
//! exactly the shape of the split-nibble tables in [`super::tables`]. Two
//! shuffles and an XOR replace 16 field multiplications, and the AVX2 encoding
//! does 32 at a time.
//!
//! Two kernels live here, in one file because they are the same algorithm at
//! two widths. [`avx2`] is a line-for-line analogue of [`ssse3`]: same
//! structure, same block factor, same tail, with `__m128i` widened to
//! `__m256i`. The one thing that is not a mechanical translation is the table
//! broadcast, which is called out where it happens.
//!
//! Neither ISA is part of any x86 baseline this crate can assume, so both are
//! gated on detection. [`ssse3_supported`] and [`avx2_supported`] are that
//! gate, and [`super::Kernel::is_supported`] is the only thing that calls
//! them.
//!
//! # Why there is no blocked `mul_rows` here
//!
//! `src/kernel/neon.rs` computes several output rows in one pass, holding their
//! accumulators in registers so that each source byte's nibble split is paid
//! once for all of them. On aarch64 that turns `10G` SIMD operations per 32
//! bytes per source shard into `4 + 6G`, and it is worth 1.4x to 1.6x measured.
//!
//! Neither half of that accounting carries over to x86, so the `mul_rows`
//! implementations below are thin loops over [`ssse3::mul_slice`] and
//! [`ssse3::mul_add_slice`] (and their AVX2 counterparts), byte-identical by
//! construction to the row-major loop the codec used to run itself.
//!
//! - **Registers.** SSE and AVX2 have 16 vector registers, not 32. Eight
//!   accumulator pairs plus the source and table vectors do not fit, so the
//!   group width that pays for itself is smaller and has to be found, not
//!   assumed.
//! - **No three-input XOR.** The aarch64 figure of six operations per row per
//!   32 bytes counts `eor3`, which folds an accumulate into the XOR of the two
//!   shuffle results. Below AVX-512 (`vpternlogd`) x86 has no equivalent, so
//!   each row costs an extra XOR and the ratio moves.
//!
//! A blocked x86 path may well be worth writing. It is not worth writing
//! *blind*: it needs its own measurement on x86 hardware, including the group
//! width, before it replaces a loop that is already correct.

// The one place in the crate where `unsafe` is permitted, and only for
// intrinsics. `forbid(unsafe_op_in_unsafe_fn)` is still in force from the
// crate root, so every operation below carries its own `unsafe` block and its
// own justification.
#![allow(unsafe_code)]
// Which `core::arch` intrinsics are `unsafe fn` has changed across compiler
// versions: the ones that touch no memory are safe on a recent toolchain. The
// blocks below are written for the declared MSRV, where they are all required;
// a newer compiler sees some of them as redundant. The alternative is code
// that fails to build on the MSRV, which is worse.
#![allow(unused_unsafe)]

// The 32-bit and 64-bit intrinsic modules are separate but carry the same
// names for everything used here, so one alias covers both and the kernels
// below are written once.
#[cfg(target_arch = "x86")]
use core::arch::x86 as arch;
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64 as arch;

// The crate root is `no_std`, so `std` is not in scope anywhere by default and
// has to be named where it is used. It is linked only under the `std` feature,
// which is exactly the configuration that has runtime CPU detection.
#[cfg(feature = "std")]
extern crate std;

/// Whether the current CPU can run the [`ssse3`] kernel.
///
/// See [`avx2_supported`] for the two-path detection this shares.
#[inline]
pub(super) fn ssse3_supported() -> bool {
    #[cfg(feature = "std")]
    {
        std::arch::is_x86_feature_detected!("ssse3")
    }
    #[cfg(not(feature = "std"))]
    {
        cfg!(target_feature = "ssse3")
    }
}

/// Whether the current CPU can run the [`avx2`] kernel.
///
/// With `std`, this is a runtime query: `is_x86_feature_detected!` executes
/// CPUID once and caches the answer, so a binary built for a generic x86
/// target still uses the widest kernel the machine it lands on has.
///
/// Without `std` there is no runtime detection to be had, and this crate will
/// not grow a dependency to get some. The check degrades to what the compiler
/// was told: a `no_std` build gets the AVX2 kernel only when it was compiled
/// with `-C target-feature=+avx2` (or a `target-cpu` that implies it), and the
/// scalar reference otherwise.
///
/// # `std` is not a safety net for `-C target-feature`
///
/// `is_x86_feature_detected!` expands to
/// `cfg!(target_feature = "avx2") || <runtime check>`. Under
/// `-C target-feature=+avx2` the left operand is `true` and CPUID is never
/// consulted, so the `std` path trusts the flag exactly as the `no_std` path
/// does, and this returns `true` on a CPU that has no AVX2.
///
/// That is correct behaviour rather than a hole. The flag is a promise to the
/// compiler about the machine, and LLVM has already acted on it across every
/// crate in the build; a program compiled that way was lost long before it
/// reached this function. It is documented because the wording it replaces
/// suggested `std` protected you from it, and it does not.
#[inline]
pub(super) fn avx2_supported() -> bool {
    #[cfg(feature = "std")]
    {
        std::arch::is_x86_feature_detected!("avx2")
    }
    #[cfg(not(feature = "std"))]
    {
        cfg!(target_feature = "avx2")
    }
}

/// SSSE3, 16 bytes per shuffle.
///
/// SSSE3 shipped in 2006 and is present on essentially every x86 CPU still
/// running, but it is not in the `x86_64` baseline, so it is detected rather
/// than assumed.
pub(super) mod ssse3 {
    use super::arch::{
        __m128i, _mm_and_si128, _mm_loadu_si128, _mm_set1_epi8, _mm_shuffle_epi8, _mm_srli_epi64,
        _mm_storeu_si128, _mm_xor_si128,
    };
    use crate::kernel::{scalar, tables};
    use crate::shards::ShardSource;

    /// Bytes per vector register.
    const LANES: usize = 16;
    /// Bytes per iteration of the main loop. Four independent vectors give the
    /// out-of-order engine enough to work with to hide the shuffle latency.
    const BLOCK: usize = 4 * LANES;

    /// The two shuffle tables for one scalar, plus the nibble mask.
    #[derive(Clone, Copy)]
    struct Shuffles {
        low: __m128i,
        high: __m128i,
        mask: __m128i,
    }

    /// Loads the split tables for `c` into registers.
    #[inline]
    #[target_feature(enable = "ssse3")]
    unsafe fn shuffles(c: u8) -> Shuffles {
        let table = tables::SPLIT.get(c);
        Shuffles {
            // SAFETY: `table` is a `&[u8; 32]`, so 16 bytes are readable at
            // offset `LOW` (0) and 16 more at offset `HIGH` (16).
            // `_mm_loadu_si128` is the unaligned load and imposes no alignment
            // requirement of its own.
            low: unsafe { _mm_loadu_si128(table.as_ptr().add(tables::LOW).cast()) },
            // SAFETY: as above.
            high: unsafe { _mm_loadu_si128(table.as_ptr().add(tables::HIGH).cast()) },
            // SAFETY: `_mm_set1_epi8` reads no memory.
            mask: unsafe { _mm_set1_epi8(0x0F) },
        }
    }

    /// Multiplies 16 bytes by the scalar the tables were built for.
    #[inline]
    #[target_feature(enable = "ssse3")]
    unsafe fn multiply(s: __m128i, t: Shuffles) -> __m128i {
        // SAFETY: all of these are register-to-register operations that read
        // no memory. Both shuffle indices are masked down to four bits, so
        // every one is below 16 and within the table. Keeping bit 7 clear also
        // matters in its own right: `pshufb` zeroes the output byte when the
        // index byte is negative.
        unsafe {
            let low = _mm_shuffle_epi8(t.low, _mm_and_si128(s, t.mask));
            // x86 has no 8-bit shift, so the high nibble comes out of a 64-bit
            // one. That puts each byte's high nibble where it is wanted but
            // also drags the neighbouring byte's low nibble in above it, so
            // the mask here is not a tidy-up: it is what makes the index
            // right.
            let high = _mm_shuffle_epi8(t.high, _mm_and_si128(_mm_srli_epi64(s, 4), t.mask));
            _mm_xor_si128(low, high)
        }
    }

    /// `dst = c * src`, where `dst` and `src` have equal length.
    #[target_feature(enable = "ssse3")]
    pub(in crate::kernel) unsafe fn mul_slice(c: u8, src: &[u8], dst: &mut [u8]) {
        debug_assert_eq!(src.len(), dst.len());
        // `mul(0, x) == 0` and `mul(1, x) == x`. The table path handles both
        // correctly; these are shortcuts, and they exist only to mirror the
        // scalar reference exactly.
        match c {
            0 => return dst.fill(0),
            1 => return dst.copy_from_slice(src),
            _ => {}
        }

        // SAFETY: this function requires `ssse3`.
        let t = unsafe { shuffles(c) };
        let len = src.len();
        let mut i = 0;

        while i + BLOCK <= len {
            for lane in 0..4 {
                let offset = i + lane * LANES;
                // SAFETY: `offset + LANES <= i + BLOCK <= len`, and `src` and
                // `dst` both have `len` bytes.
                unsafe {
                    let s = _mm_loadu_si128(src.as_ptr().add(offset).cast());
                    _mm_storeu_si128(dst.as_mut_ptr().add(offset).cast(), multiply(s, t));
                }
            }
            i += BLOCK;
        }

        while i + LANES <= len {
            // SAFETY: `i + LANES <= len`, and both slices have `len` bytes.
            unsafe {
                let s = _mm_loadu_si128(src.as_ptr().add(i).cast());
                _mm_storeu_si128(dst.as_mut_ptr().add(i).cast(), multiply(s, t));
            }
            i += LANES;
        }

        scalar::mul_slice(c, &src[i..], &mut dst[i..]);
    }

    /// `dst ^= c * src`, where `dst` and `src` have equal length.
    #[target_feature(enable = "ssse3")]
    pub(in crate::kernel) unsafe fn mul_add_slice(c: u8, src: &[u8], dst: &mut [u8]) {
        debug_assert_eq!(src.len(), dst.len());
        if c == 0 {
            return;
        }

        // SAFETY: this function requires `ssse3`.
        let t = unsafe { shuffles(c) };
        let len = src.len();
        let mut i = 0;

        while i + BLOCK <= len {
            for lane in 0..4 {
                let offset = i + lane * LANES;
                // SAFETY: `offset + LANES <= i + BLOCK <= len`, and `src` and
                // `dst` both have `len` bytes.
                unsafe {
                    let s = _mm_loadu_si128(src.as_ptr().add(offset).cast());
                    let d = _mm_loadu_si128(dst.as_ptr().add(offset).cast());
                    let acc = _mm_xor_si128(d, multiply(s, t));
                    _mm_storeu_si128(dst.as_mut_ptr().add(offset).cast(), acc);
                }
            }
            i += BLOCK;
        }

        while i + LANES <= len {
            // SAFETY: `i + LANES <= len`, and both slices have `len` bytes.
            unsafe {
                let s = _mm_loadu_si128(src.as_ptr().add(i).cast());
                let d = _mm_loadu_si128(dst.as_ptr().add(i).cast());
                let acc = _mm_xor_si128(d, multiply(s, t));
                _mm_storeu_si128(dst.as_mut_ptr().add(i).cast(), acc);
            }
            i += LANES;
        }

        scalar::mul_add_slice(c, &src[i..], &mut dst[i..]);
    }

    /// `outs[t] = sum over j of matrix[t * data_shards + j] * srcs.shard(j)`,
    /// for every `t`.
    ///
    /// A driver over the pair above, in the same row-major order `src/driver.rs`
    /// used to run itself, which makes it byte-identical to the scalar
    /// reference by construction. See the module docs for why this is not the
    /// blocked form `src/kernel/neon.rs` uses.
    #[target_feature(enable = "ssse3")]
    pub(in crate::kernel) unsafe fn mul_rows<S: ShardSource + ?Sized, O: AsMut<[u8]>>(
        matrix: &[u8],
        data_shards: usize,
        srcs: &S,
        outs: &mut [O],
    ) {
        for (row, out) in outs.iter_mut().enumerate() {
            let coeffs = &matrix[row * data_shards..(row + 1) * data_shards];
            let out = out.as_mut();
            // SAFETY: this function requires `ssse3`, which is what
            // `mul_slice` and `mul_add_slice` require.
            unsafe {
                mul_slice(coeffs[0], srcs.shard(0), out);
                for (j, &c) in coeffs.iter().enumerate().skip(1) {
                    mul_add_slice(c, srcs.shard(j), out);
                }
            }
        }
    }
}

/// AVX2, 32 bytes per shuffle.
///
/// The same kernel as [`ssse3`] at twice the width. The only substantive
/// difference is in `shuffles` below: `vpshufb` is two independent 128-bit
/// shuffles wearing one opcode, so the table has to be present in both halves
/// of the register.
pub(super) mod avx2 {
    use super::arch::{
        __m256i, _mm_loadu_si128, _mm256_and_si256, _mm256_broadcastsi128_si256,
        _mm256_loadu_si256, _mm256_set1_epi8, _mm256_shuffle_epi8, _mm256_srli_epi64,
        _mm256_storeu_si256, _mm256_xor_si256,
    };
    use crate::kernel::{scalar, tables};
    use crate::shards::ShardSource;

    /// Bytes per vector register.
    const LANES: usize = 32;
    /// Bytes per iteration of the main loop. Four independent vectors give the
    /// out-of-order engine enough to work with to hide the shuffle latency.
    const BLOCK: usize = 4 * LANES;

    /// The two shuffle tables for one scalar, plus the nibble mask.
    #[derive(Clone, Copy)]
    struct Shuffles {
        low: __m256i,
        high: __m256i,
        mask: __m256i,
    }

    /// Loads the 16-byte table at `offset` in `table` into *both* 128-bit
    /// lanes of a 256-bit register.
    ///
    /// This is the one place the AVX2 kernel is not a widening of the SSSE3
    /// one, and it is the classic way to get this wrong. `vpshufb` does not
    /// shuffle across a 256-bit register: it is two 128-bit shuffles side by
    /// side, and each one indexes only the 16 table bytes sitting in its own
    /// lane. A table loaded into the low lane alone would leave the high
    /// lane's table zero, and the upper 16 bytes of every vector would come
    /// out zero with it, silently, for exactly half the output. Broadcasting
    /// makes the two lanes identical, which makes `vpshufb` behave as the
    /// 16-byte shuffle done twice.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn broadcast_table(table: &[u8; 32], offset: usize) -> __m256i {
        // SAFETY: `offset` is `LOW` (0) or `HIGH` (16) and `table` is 32 bytes
        // long, so 16 bytes are readable from `offset`. `_mm_loadu_si128` is
        // the unaligned load and imposes no alignment requirement of its own.
        // The broadcast reads no memory.
        unsafe { _mm256_broadcastsi128_si256(_mm_loadu_si128(table.as_ptr().add(offset).cast())) }
    }

    /// Loads the split tables for `c` into registers.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn shuffles(c: u8) -> Shuffles {
        let table = tables::SPLIT.get(c);
        Shuffles {
            // SAFETY: `broadcast_table` requires `avx2`, which this function
            // has, and both offsets are in bounds of the 32-byte entry.
            low: unsafe { broadcast_table(table, tables::LOW) },
            // SAFETY: as above.
            high: unsafe { broadcast_table(table, tables::HIGH) },
            // SAFETY: `_mm256_set1_epi8` reads no memory.
            mask: unsafe { _mm256_set1_epi8(0x0F) },
        }
    }

    /// Multiplies 32 bytes by the scalar the tables were built for.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn multiply(s: __m256i, t: Shuffles) -> __m256i {
        // SAFETY: all of these are register-to-register operations that read
        // no memory. Both shuffle indices are masked down to four bits, so
        // every one is below 16 and within the table. Keeping bit 7 clear also
        // matters in its own right: `vpshufb` zeroes the output byte when the
        // index byte is negative.
        unsafe {
            let low = _mm256_shuffle_epi8(t.low, _mm256_and_si256(s, t.mask));
            // x86 has no 8-bit shift, so the high nibble comes out of a 64-bit
            // one. That puts each byte's high nibble where it is wanted but
            // also drags the neighbouring byte's low nibble in above it, so
            // the mask here is not a tidy-up: it is what makes the index
            // right.
            let shifted = _mm256_and_si256(_mm256_srli_epi64(s, 4), t.mask);
            let high = _mm256_shuffle_epi8(t.high, shifted);
            _mm256_xor_si256(low, high)
        }
    }

    /// `dst = c * src`, where `dst` and `src` have equal length.
    #[target_feature(enable = "avx2")]
    pub(in crate::kernel) unsafe fn mul_slice(c: u8, src: &[u8], dst: &mut [u8]) {
        debug_assert_eq!(src.len(), dst.len());
        // `mul(0, x) == 0` and `mul(1, x) == x`. The table path handles both
        // correctly; these are shortcuts, and they exist only to mirror the
        // scalar reference exactly.
        match c {
            0 => return dst.fill(0),
            1 => return dst.copy_from_slice(src),
            _ => {}
        }

        // SAFETY: this function requires `avx2`.
        let t = unsafe { shuffles(c) };
        let len = src.len();
        let mut i = 0;

        while i + BLOCK <= len {
            for lane in 0..4 {
                let offset = i + lane * LANES;
                // SAFETY: `offset + LANES <= i + BLOCK <= len`, and `src` and
                // `dst` both have `len` bytes.
                unsafe {
                    let s = _mm256_loadu_si256(src.as_ptr().add(offset).cast());
                    _mm256_storeu_si256(dst.as_mut_ptr().add(offset).cast(), multiply(s, t));
                }
            }
            i += BLOCK;
        }

        while i + LANES <= len {
            // SAFETY: `i + LANES <= len`, and both slices have `len` bytes.
            unsafe {
                let s = _mm256_loadu_si256(src.as_ptr().add(i).cast());
                _mm256_storeu_si256(dst.as_mut_ptr().add(i).cast(), multiply(s, t));
            }
            i += LANES;
        }

        scalar::mul_slice(c, &src[i..], &mut dst[i..]);
    }

    /// `dst ^= c * src`, where `dst` and `src` have equal length.
    #[target_feature(enable = "avx2")]
    pub(in crate::kernel) unsafe fn mul_add_slice(c: u8, src: &[u8], dst: &mut [u8]) {
        debug_assert_eq!(src.len(), dst.len());
        if c == 0 {
            return;
        }

        // SAFETY: this function requires `avx2`.
        let t = unsafe { shuffles(c) };
        let len = src.len();
        let mut i = 0;

        while i + BLOCK <= len {
            for lane in 0..4 {
                let offset = i + lane * LANES;
                // SAFETY: `offset + LANES <= i + BLOCK <= len`, and `src` and
                // `dst` both have `len` bytes.
                unsafe {
                    let s = _mm256_loadu_si256(src.as_ptr().add(offset).cast());
                    let d = _mm256_loadu_si256(dst.as_ptr().add(offset).cast());
                    let acc = _mm256_xor_si256(d, multiply(s, t));
                    _mm256_storeu_si256(dst.as_mut_ptr().add(offset).cast(), acc);
                }
            }
            i += BLOCK;
        }

        while i + LANES <= len {
            // SAFETY: `i + LANES <= len`, and both slices have `len` bytes.
            unsafe {
                let s = _mm256_loadu_si256(src.as_ptr().add(i).cast());
                let d = _mm256_loadu_si256(dst.as_ptr().add(i).cast());
                let acc = _mm256_xor_si256(d, multiply(s, t));
                _mm256_storeu_si256(dst.as_mut_ptr().add(i).cast(), acc);
            }
            i += LANES;
        }

        scalar::mul_add_slice(c, &src[i..], &mut dst[i..]);
    }

    /// `outs[t] = sum over j of matrix[t * data_shards + j] * srcs.shard(j)`,
    /// for every `t`.
    ///
    /// As in [`super::ssse3::mul_rows`]: a driver over the pair above, in the
    /// row-major order the codec used to run itself. See the module docs for
    /// why this is not the blocked form `src/kernel/neon.rs` uses.
    #[target_feature(enable = "avx2")]
    pub(in crate::kernel) unsafe fn mul_rows<S: ShardSource + ?Sized, O: AsMut<[u8]>>(
        matrix: &[u8],
        data_shards: usize,
        srcs: &S,
        outs: &mut [O],
    ) {
        for (row, out) in outs.iter_mut().enumerate() {
            let coeffs = &matrix[row * data_shards..(row + 1) * data_shards];
            let out = out.as_mut();
            // SAFETY: this function requires `avx2`, which is what `mul_slice`
            // and `mul_add_slice` require.
            unsafe {
                mul_slice(coeffs[0], srcs.shard(0), out);
                for (j, &c) in coeffs.iter().enumerate().skip(1) {
                    mul_add_slice(c, srcs.shard(j), out);
                }
            }
        }
    }
}
