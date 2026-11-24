//! AVX-512BW and GFNI kernels. GFNI's affine transform implements the
//! linear map for multiplication by a fixed scalar in our 0x11D field.
//! This deliberately does not use GF2P8MULB (the AES 0x11B field).
#![allow(unsafe_code)]
#![allow(unused_unsafe)]

use super::{scalar, tables};
use crate::data::ec::{gf, shards::ShardSource};
use core::arch::x86_64::*;

/// GFNI stores the row for output bit i in byte 7-i of each matrix word.
const fn affine_tables() -> [u64; 256] {
    let mut matrices = [0; 256];
    let mut c = 0;
    while c < 256 {
        let mut input_bit = 0;
        while input_bit < 8 {
            let product = gf::mul(c as u8, 1 << input_bit);
            let mut output_bit = 0;
            while output_bit < 8 {
                if product & (1 << output_bit) != 0 {
                    matrices[c] |= 1u64 << ((7 - output_bit) * 8 + input_bit);
                }
                output_bit += 1;
            }
            input_bit += 1;
        }
        c += 1;
    }
    matrices
}

static AFFINE: [u64; 256] = affine_tables();

pub(super) fn avx512_supported() -> bool {
    std::arch::is_x86_feature_detected!("avx512f")
        && std::arch::is_x86_feature_detected!("avx512bw")
}

pub(super) fn gfni256_supported() -> bool {
    std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("gfni")
}

pub(super) fn gfni512_supported() -> bool {
    avx512_supported() && std::arch::is_x86_feature_detected!("gfni")
}

// The vector types and instructions are isolated behind runtime-checked
// target-feature boundaries. All vector loads/stores stay inside the slice;
// the scalar reference handles the final partial vector.
macro_rules! kernel {
    ($name:ident, $features:literal, $width:expr, $vector:ty,
     $load:ident, $store:ident, $xor:ident, $zero:ident, $prepare:ident, $product:ident) => {
        pub(super) mod $name {
            use super::*;

            #[target_feature(enable = $features)]
            unsafe fn apply<const ADD: bool>(c: u8, src: &[u8], dst: &mut [u8]) {
                debug_assert_eq!(src.len(), dst.len());
                match c {
                    0 => {
                        if !ADD {
                            dst.fill(0);
                        }
                        return;
                    }
                    1 if !ADD => return dst.copy_from_slice(src),
                    _ => {}
                }
                // SAFETY: the caller checked the features required by this module.
                let table = unsafe { $prepare(c) };
                let mut i = 0;
                let len = src.len();
                while i + 4 * $width <= len {
                    for j in 0..4 {
                        let off = i + j * $width;
                        // SAFETY: each full vector is in bounds; loads and stores are unaligned.
                        unsafe {
                            let value: $vector = $load(src.as_ptr().add(off).cast());
                            let product = if c == 1 {
                                value
                            } else {
                                $product(value, table)
                            };
                            let product = if ADD {
                                $xor(product, $load(dst.as_ptr().add(off).cast()))
                            } else {
                                product
                            };
                            $store(dst.as_mut_ptr().add(off).cast(), product);
                        }
                    }
                    i += 4 * $width;
                }
                while i + $width <= len {
                    // SAFETY: i + width <= len, and the ISA is enabled.
                    unsafe {
                        let value: $vector = $load(src.as_ptr().add(i).cast());
                        let product = if c == 1 {
                            value
                        } else {
                            $product(value, table)
                        };
                        let product = if ADD {
                            $xor(product, $load(dst.as_ptr().add(i).cast()))
                        } else {
                            product
                        };
                        $store(dst.as_mut_ptr().add(i).cast(), product);
                    }
                    i += $width;
                }
                if ADD {
                    scalar::mul_add_slice(c, &src[i..], &mut dst[i..]);
                } else {
                    scalar::mul_slice(c, &src[i..], &mut dst[i..]);
                }
            }

            #[target_feature(enable = $features)]
            pub(in crate::data::ec::kernel) unsafe fn mul_slice(c: u8, src: &[u8], dst: &mut [u8]) {
                // SAFETY: caller guarantees the ISA.
                unsafe { apply::<false>(c, src, dst) }
            }

            #[target_feature(enable = $features)]
            pub(in crate::data::ec::kernel) unsafe fn mul_add_slice(
                c: u8,
                src: &[u8],
                dst: &mut [u8],
            ) {
                // SAFETY: caller guarantees the ISA.
                unsafe { apply::<true>(c, src, dst) }
            }

            // Compute G rows in registers, writing each output once. All
            // source/output windows are bounds checked before vector access.
            #[target_feature(enable = $features)]
            unsafe fn fused<const G: usize, S: ShardSource + ?Sized>(
                matrix: &[u8],
                k: usize,
                srcs: &S,
                mut outs: [&mut [u8]; G],
                start: usize,
                end: usize,
            ) {
                let rows: [&[u8]; G] = core::array::from_fn(|r| &matrix[r * k..(r + 1) * k]);
                let mut i = start;
                let zero = $zero();
                while i + 2 * $width <= end {
                    let mut a = [zero; G];
                    let mut b = [zero; G];
                    for j in 0..k {
                        let src = &srcs.shard(j)[i..i + 2 * $width];
                        // SAFETY: two vectors fit inside the checked window.
                        unsafe {
                            let x = $load(src.as_ptr().cast());
                            let y = $load(src.as_ptr().add($width).cast());
                            for r in 0..G {
                                let t = $prepare(rows[r][j]);
                                a[r] = $xor(a[r], $product(x, t));
                                b[r] = $xor(b[r], $product(y, t));
                            }
                        }
                    }
                    for r in 0..G {
                        let out = &mut outs[r][i..i + 2 * $width];
                        // SAFETY: two vectors fit inside the checked window.
                        unsafe {
                            $store(out.as_mut_ptr().cast(), a[r]);
                            $store(out.as_mut_ptr().add($width).cast(), b[r]);
                        }
                    }
                    i += 2 * $width;
                }
                for (r, out) in outs.iter_mut().enumerate() {
                    let out = &mut out[i..end];
                    // SAFETY: the same ISA is enabled here.
                    unsafe {
                        apply::<false>(rows[r][0], &srcs.shard(0)[i..end], out);
                        for (j, &c) in rows[r].iter().enumerate().skip(1) {
                            apply::<true>(c, &srcs.shard(j)[i..end], out);
                        }
                    }
                }
            }

            #[target_feature(enable = $features)]
            pub(in crate::data::ec::kernel) unsafe fn mul_rows<
                S: ShardSource + ?Sized,
                O: AsMut<[u8]>,
            >(
                matrix: &[u8],
                k: usize,
                srcs: &S,
                outs: &mut [O],
            ) {
                let len = srcs.shard(0).len();
                let tile = super::super::encode_tile_size(k, outs.len());
                for start in (0..len).step_by(tile) {
                    let end = (start + tile).min(len);
                    let mut groups = outs.chunks_exact_mut(4);
                    let mut row = 0;
                    if k >= 4 {
                        for group in groups.by_ref() {
                            let mut iter = group.iter_mut();
                            let group = core::array::from_fn(|_| iter.next().unwrap().as_mut());
                            // SAFETY: the same ISA is enabled here.
                            unsafe {
                                fused::<4, S>(&matrix[row * k..], k, srcs, group, start, end)
                            };
                            row += 4;
                        }
                    }
                    for (row, out) in outs.iter_mut().enumerate().skip(row) {
                        let coeffs = &matrix[row * k..(row + 1) * k];
                        let out = &mut out.as_mut()[start..end];
                        // SAFETY: this function is enabled for the same ISA as apply.
                        unsafe {
                            apply::<false>(coeffs[0], &srcs.shard(0)[start..end], out);
                            for (j, &c) in coeffs.iter().enumerate().skip(1) {
                                apply::<true>(c, &srcs.shard(j)[start..end], out);
                            }
                        }
                    }
                }
            }
        }
    };
}

#[target_feature(enable = "avx512f,avx512bw")]
unsafe fn prepare_shuffle(c: u8) -> (__m512i, __m512i, __m512i) {
    let table = tables::SPLIT.get(c);
    // SAFETY: both 16-byte halves lie inside this table.
    unsafe {
        (
            _mm512_broadcast_i32x4(_mm_loadu_si128(table.as_ptr().cast())),
            _mm512_broadcast_i32x4(_mm_loadu_si128(table.as_ptr().add(16).cast())),
            _mm512_set1_epi8(15),
        )
    }
}
#[inline]
#[target_feature(enable = "avx512f,avx512bw")]
unsafe fn product_shuffle(value: __m512i, (lo, hi, mask): (__m512i, __m512i, __m512i)) -> __m512i {
    unsafe {
        _mm512_xor_si512(
            _mm512_shuffle_epi8(lo, _mm512_and_si512(value, mask)),
            _mm512_shuffle_epi8(hi, _mm512_and_si512(_mm512_srli_epi64::<4>(value), mask)),
        )
    }
}
#[inline]
#[target_feature(enable = "avx2,gfni")]
unsafe fn prepare_gfni256(c: u8) -> __m256i {
    unsafe { _mm256_set1_epi64x(AFFINE[c as usize] as i64) }
}
#[inline]
#[target_feature(enable = "avx2,gfni")]
unsafe fn product_gfni256(value: __m256i, matrix: __m256i) -> __m256i {
    unsafe { _mm256_gf2p8affine_epi64_epi8::<0>(value, matrix) }
}
#[inline]
#[target_feature(enable = "avx512f,avx512bw,gfni")]
unsafe fn prepare_gfni512(c: u8) -> __m512i {
    unsafe { _mm512_set1_epi64(AFFINE[c as usize] as i64) }
}
#[inline]
#[target_feature(enable = "avx512f,avx512bw,gfni")]
unsafe fn product_gfni512(value: __m512i, matrix: __m512i) -> __m512i {
    unsafe { _mm512_gf2p8affine_epi64_epi8::<0>(value, matrix) }
}

kernel!(
    avx512,
    "avx512f,avx512bw",
    64,
    __m512i,
    _mm512_loadu_si512,
    _mm512_storeu_si512,
    _mm512_xor_si512,
    _mm512_setzero_si512,
    prepare_shuffle,
    product_shuffle
);
kernel!(
    gfni256,
    "avx2,gfni",
    32,
    __m256i,
    _mm256_loadu_si256,
    _mm256_storeu_si256,
    _mm256_xor_si256,
    _mm256_setzero_si256,
    prepare_gfni256,
    product_gfni256
);
kernel!(
    gfni512,
    "avx512f,avx512bw,gfni",
    64,
    __m512i,
    _mm512_loadu_si512,
    _mm512_storeu_si512,
    _mm512_xor_si512,
    _mm512_setzero_si512,
    prepare_gfni512,
    product_gfni512
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affine_matrices_match_legacy_field_for_all_inputs() {
        for c in 0..=255u8 {
            let matrix = AFFINE[c as usize];
            for b in 0..=255u8 {
                let mut result = 0u8;
                for i in 0..8 {
                    let row = (matrix >> (8 * (7 - i))) as u8;
                    result |= (((row & b).count_ones() & 1) as u8) << i;
                }
                assert_eq!(result, gf::mul(c, b));
            }
        }
    }
}
