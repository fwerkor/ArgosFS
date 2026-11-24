//! The inner kernel: multiply a shard by a field scalar and accumulate.
//!
//! Every encode and every decode in this crate is a matrix-vector product over
//! GF(2^8) whose "elements" are whole shards, and the two functions below are
//! the entirety of its inner loop. Essentially all of the runtime is here.
//!
//! This is the scalar reference implementation. It is written for obvious
//! correctness and is never optimized: it is the specification that every
//! future SIMD path must reproduce byte for byte.

use crate::data::ec::gf;
use crate::data::ec::shards::ShardSource;

/// `dst = c * src`
///
/// `dst` and `src` must have the same length.
#[inline]
pub(crate) fn mul_slice(c: u8, src: &[u8], dst: &mut [u8]) {
    debug_assert_eq!(src.len(), dst.len());
    // `mul(0, x) == 0` and `mul(1, x) == x`, so these branches are pure
    // shorthand for the general loop, not approximations of it.
    match c {
        0 => dst.fill(0),
        1 => dst.copy_from_slice(src),
        _ => {
            for (d, &s) in dst.iter_mut().zip(src) {
                *d = gf::mul(c, s);
            }
        }
    }
}

/// `dst ^= c * src`
///
/// `dst` and `src` must have the same length.
#[inline]
pub(crate) fn mul_add_slice(c: u8, src: &[u8], dst: &mut [u8]) {
    debug_assert_eq!(src.len(), dst.len());
    match c {
        0 => {}
        1 => {
            for (d, &s) in dst.iter_mut().zip(src) {
                *d ^= s;
            }
        }
        _ => {
            for (d, &s) in dst.iter_mut().zip(src) {
                *d ^= gf::mul(c, s);
            }
        }
    }
}

/// `outs[t] = sum over j of matrix[t * data_shards + j] * srcs.shard(j)`, for
/// every `t`.
///
/// This is a driver over [`mul_slice`] and [`mul_add_slice`], not a new
/// algorithm: one `mul_slice` for the first term of each output row, then
/// `data_shards - 1` `mul_add_slice` calls. That is exactly the row-major loop
/// `src/driver.rs` used to run itself, so this stays the specification the
/// vector implementations of the same operation are differentially tested
/// against, and the two functions above stay untouched.
pub(crate) fn mul_rows<S: ShardSource + ?Sized, O: AsMut<[u8]>>(
    matrix: &[u8],
    data_shards: usize,
    srcs: &S,
    outs: &mut [O],
) {
    for (row, out) in outs.iter_mut().enumerate() {
        let coeffs = &matrix[row * data_shards..(row + 1) * data_shards];
        let out = out.as_mut();
        // The first term is assigned rather than accumulated, which saves
        // zeroing the output.
        mul_slice(coeffs[0], srcs.shard(0), out);
        for (j, &c) in coeffs.iter().enumerate().skip(1) {
            mul_add_slice(c, srcs.shard(j), out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcuts_match_the_general_loop() {
        let src: [u8; 64] = core::array::from_fn(|i| (i * 7 + 3) as u8);
        for c in 0..=255u8 {
            let mut dst = [0u8; 64];
            mul_slice(c, &src, &mut dst);
            for (i, &s) in src.iter().enumerate() {
                assert_eq!(dst[i], gf::mul(c, s), "c = {c}, i = {i}");
            }
        }
    }

    #[test]
    fn accumulation_is_xor() {
        let src: [u8; 33] = core::array::from_fn(|i| (i * 11 + 1) as u8);
        for c in 0..=255u8 {
            let mut dst: [u8; 33] = core::array::from_fn(|i| (i * 5) as u8);
            let before = dst;
            mul_add_slice(c, &src, &mut dst);
            for i in 0..src.len() {
                assert_eq!(dst[i], before[i] ^ gf::mul(c, src[i]), "c = {c}, i = {i}");
            }
        }
    }

    #[test]
    fn adding_a_value_twice_cancels() {
        let src: [u8; 16] = core::array::from_fn(|i| (i * 17) as u8);
        let mut dst = [0xAAu8; 16];
        mul_add_slice(37, &src, &mut dst);
        mul_add_slice(37, &src, &mut dst);
        assert_eq!(dst, [0xAAu8; 16]);
    }
}
