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
//! The row driver tiles source and parity buffers to avoid repeatedly
//! streaming whole shards from memory. AVX-512 and GFNI are in wide_x86.

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

#[inline]
pub(super) fn ssse3_supported() -> bool {
    std::arch::is_x86_feature_detected!("ssse3")
}

#[inline]
pub(super) fn avx2_supported() -> bool {
    std::arch::is_x86_feature_detected!("avx2")
}

/// SSSE3, 16 bytes per shuffle.
///
/// SSSE3 shipped in 2006 and is present on essentially every x86 CPU still
/// running, but it is not in the `x86_64` baseline, so it is detected rather
/// than assumed.
pub(super) mod ssse3 {
    use super::arch::{
        __m128i, _mm_and_si128, _mm_loadu_si128, _mm_set1_epi8, _mm_setzero_si128,
        _mm_shuffle_epi8, _mm_srli_epi64, _mm_storeu_si128, _mm_xor_si128,
    };
    use crate::data::ec::kernel::{scalar, tables};
    use crate::data::ec::shards::ShardSource;

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
    pub(in crate::data::ec::kernel) unsafe fn mul_slice(c: u8, src: &[u8], dst: &mut [u8]) {
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
    pub(in crate::data::ec::kernel) unsafe fn mul_add_slice(c: u8, src: &[u8], dst: &mut [u8]) {
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

    // Two independent output rows share each loaded source and its nibble
    // indices. Four accumulators fit in the narrow x86 register file.
    #[target_feature(enable = "ssse3")]
    unsafe fn fused_pair<S: ShardSource + ?Sized>(
        rows: [&[u8]; 2],
        srcs: &S,
        mut outs: [&mut [u8]; 2],
        start: usize,
        end: usize,
    ) {
        let mut i = start;
        let zero = _mm_setzero_si128();
        while i + 2 * LANES <= end {
            let mut a = [zero; 2];
            let mut b = [zero; 2];
            for (j, &c0) in rows[0].iter().enumerate() {
                let src = &srcs.shard(j)[i..i + 2 * LANES];
                // SAFETY: the checked source window contains two full vectors.
                unsafe {
                    let x = _mm_loadu_si128(src.as_ptr().cast());
                    let y = _mm_loadu_si128(src.as_ptr().add(LANES).cast());
                    let mask = _mm_set1_epi8(15);
                    let lx = _mm_and_si128(x, mask);
                    let hx = _mm_and_si128(_mm_srli_epi64(x, 4), mask);
                    let ly = _mm_and_si128(y, mask);
                    let hy = _mm_and_si128(_mm_srli_epi64(y, 4), mask);
                    for r in 0..2 {
                        let t = shuffles(if r == 0 { c0 } else { rows[1][j] });
                        a[r] = _mm_xor_si128(
                            a[r],
                            _mm_xor_si128(
                                _mm_shuffle_epi8(t.low, lx),
                                _mm_shuffle_epi8(t.high, hx),
                            ),
                        );
                        b[r] = _mm_xor_si128(
                            b[r],
                            _mm_xor_si128(
                                _mm_shuffle_epi8(t.low, ly),
                                _mm_shuffle_epi8(t.high, hy),
                            ),
                        );
                    }
                }
            }
            for r in 0..2 {
                let out = &mut outs[r][i..i + 2 * LANES];
                // SAFETY: the checked output window contains two full vectors.
                unsafe {
                    _mm_storeu_si128(out.as_mut_ptr().cast(), a[r]);
                    _mm_storeu_si128(out.as_mut_ptr().add(LANES).cast(), b[r]);
                }
            }
            i += 2 * LANES;
        }
        if i < end {
            for (r, out) in outs.iter_mut().enumerate() {
                let out = &mut out[i..end];
                // SAFETY: the same ISA is enabled in this function.
                unsafe {
                    mul_slice(rows[r][0], &srcs.shard(0)[i..end], out);
                    for (j, &c) in rows[r].iter().enumerate().skip(1) {
                        mul_add_slice(c, &srcs.shard(j)[i..end], out);
                    }
                }
            }
        }
    }

    /// `outs[t] = sum over j of matrix[t * data_shards + j] * srcs.shard(j)`,
    /// for every `t`.
    ///
    /// Encode a cache-sized tile across all parity rows before advancing.
    #[target_feature(enable = "ssse3")]
    pub(in crate::data::ec::kernel) unsafe fn mul_rows<S: ShardSource + ?Sized, O: AsMut<[u8]>>(
        matrix: &[u8],
        data_shards: usize,
        srcs: &S,
        outs: &mut [O],
    ) {
        let len = srcs.shard(0).len();
        let tile = crate::data::ec::kernel::encode_tile_size(data_shards, outs.len());
        for start in (0..len).step_by(tile) {
            let end = (start + tile).min(len);
            let mut row = 0;
            if data_shards >= 4 && end - start >= 2 * LANES {
                for pair in outs.chunks_exact_mut(2) {
                    let (left, right) = pair.split_at_mut(1);
                    let coeffs = &matrix[row * data_shards..(row + 2) * data_shards];
                    let (r0, r1) = coeffs.split_at(data_shards);
                    // SAFETY: same ISA as the fused helper.
                    unsafe {
                        fused_pair(
                            [r0, r1],
                            srcs,
                            [left[0].as_mut(), right[0].as_mut()],
                            start,
                            end,
                        )
                    };
                    row += 2;
                }
            }
            for (row, out) in outs.iter_mut().enumerate().skip(row) {
                let coeffs = &matrix[row * data_shards..(row + 1) * data_shards];
                let out = &mut out.as_mut()[start..end];
                // SAFETY: this function requires `ssse3`, which is what
                // `mul_slice` and `mul_add_slice` require.
                unsafe {
                    mul_slice(coeffs[0], &srcs.shard(0)[start..end], out);
                    for (j, &c) in coeffs.iter().enumerate().skip(1) {
                        mul_add_slice(c, &srcs.shard(j)[start..end], out);
                    }
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
        __m256i, _mm256_and_si256, _mm256_broadcastsi128_si256, _mm256_loadu_si256,
        _mm256_set1_epi8, _mm256_setzero_si256, _mm256_shuffle_epi8, _mm256_srli_epi64,
        _mm256_storeu_si256, _mm256_xor_si256, _mm_loadu_si128,
    };
    use crate::data::ec::kernel::{scalar, tables};
    use crate::data::ec::shards::ShardSource;

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
    pub(in crate::data::ec::kernel) unsafe fn mul_slice(c: u8, src: &[u8], dst: &mut [u8]) {
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
    pub(in crate::data::ec::kernel) unsafe fn mul_add_slice(c: u8, src: &[u8], dst: &mut [u8]) {
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

    // Two independent output rows share each loaded source and its nibble
    // indices. Four accumulators fit in the narrow x86 register file.
    #[target_feature(enable = "avx2")]
    unsafe fn fused_pair<S: ShardSource + ?Sized>(
        rows: [&[u8]; 2],
        srcs: &S,
        mut outs: [&mut [u8]; 2],
        start: usize,
        end: usize,
    ) {
        let mut i = start;
        let zero = _mm256_setzero_si256();
        while i + 2 * LANES <= end {
            let mut a = [zero; 2];
            let mut b = [zero; 2];
            for (j, &c0) in rows[0].iter().enumerate() {
                let src = &srcs.shard(j)[i..i + 2 * LANES];
                // SAFETY: the checked source window contains two full vectors.
                unsafe {
                    let x = _mm256_loadu_si256(src.as_ptr().cast());
                    let y = _mm256_loadu_si256(src.as_ptr().add(LANES).cast());
                    let mask = _mm256_set1_epi8(15);
                    let lx = _mm256_and_si256(x, mask);
                    let hx = _mm256_and_si256(_mm256_srli_epi64(x, 4), mask);
                    let ly = _mm256_and_si256(y, mask);
                    let hy = _mm256_and_si256(_mm256_srli_epi64(y, 4), mask);
                    for r in 0..2 {
                        let t = shuffles(if r == 0 { c0 } else { rows[1][j] });
                        a[r] = _mm256_xor_si256(
                            a[r],
                            _mm256_xor_si256(
                                _mm256_shuffle_epi8(t.low, lx),
                                _mm256_shuffle_epi8(t.high, hx),
                            ),
                        );
                        b[r] = _mm256_xor_si256(
                            b[r],
                            _mm256_xor_si256(
                                _mm256_shuffle_epi8(t.low, ly),
                                _mm256_shuffle_epi8(t.high, hy),
                            ),
                        );
                    }
                }
            }
            for r in 0..2 {
                let out = &mut outs[r][i..i + 2 * LANES];
                // SAFETY: the checked output window contains two full vectors.
                unsafe {
                    _mm256_storeu_si256(out.as_mut_ptr().cast(), a[r]);
                    _mm256_storeu_si256(out.as_mut_ptr().add(LANES).cast(), b[r]);
                }
            }
            i += 2 * LANES;
        }
        if i < end {
            for (r, out) in outs.iter_mut().enumerate() {
                let out = &mut out[i..end];
                // SAFETY: the same ISA is enabled in this function.
                unsafe {
                    mul_slice(rows[r][0], &srcs.shard(0)[i..end], out);
                    for (j, &c) in rows[r].iter().enumerate().skip(1) {
                        mul_add_slice(c, &srcs.shard(j)[i..end], out);
                    }
                }
            }
        }
    }

    /// `outs[t] = sum over j of matrix[t * data_shards + j] * srcs.shard(j)`,
    /// for every `t`.
    ///
    /// Encode a cache-sized tile across all parity rows before advancing.
    #[target_feature(enable = "avx2")]
    pub(in crate::data::ec::kernel) unsafe fn mul_rows<S: ShardSource + ?Sized, O: AsMut<[u8]>>(
        matrix: &[u8],
        data_shards: usize,
        srcs: &S,
        outs: &mut [O],
    ) {
        let len = srcs.shard(0).len();
        let tile = crate::data::ec::kernel::encode_tile_size(data_shards, outs.len());
        for start in (0..len).step_by(tile) {
            let end = (start + tile).min(len);
            let mut row = 0;
            if data_shards >= 4 && end - start >= 2 * LANES {
                for pair in outs.chunks_exact_mut(2) {
                    let (left, right) = pair.split_at_mut(1);
                    let coeffs = &matrix[row * data_shards..(row + 2) * data_shards];
                    let (r0, r1) = coeffs.split_at(data_shards);
                    // SAFETY: same ISA as the fused helper.
                    unsafe {
                        fused_pair(
                            [r0, r1],
                            srcs,
                            [left[0].as_mut(), right[0].as_mut()],
                            start,
                            end,
                        )
                    };
                    row += 2;
                }
            }
            for (row, out) in outs.iter_mut().enumerate().skip(row) {
                let coeffs = &matrix[row * data_shards..(row + 1) * data_shards];
                let out = &mut out.as_mut()[start..end];
                // SAFETY: this function requires `avx2`, which is what `mul_slice`
                // and `mul_add_slice` require.
                unsafe {
                    mul_slice(coeffs[0], &srcs.shard(0)[start..end], out);
                    for (j, &c) in coeffs.iter().enumerate().skip(1) {
                        mul_add_slice(c, &srcs.shard(j)[start..end], out);
                    }
                }
            }
        }
    }
}
