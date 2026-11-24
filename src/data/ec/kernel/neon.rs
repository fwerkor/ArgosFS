//! aarch64 NEON kernel, using the `vqtbl1q_u8` byte shuffle.
//!
//! `vqtbl1q_u8` looks up 16 bytes in a 16-entry table in one instruction,
//! which is exactly the shape of the split-nibble tables in
//! [`super::tables`]. Two shuffles and an XOR replace 16 field
//! multiplications.
//!
//! The dispatcher checks NEON support at runtime before entering this module.
//!
//! [`mul_rows`] is the same shuffle, blocked over several output shards at
//! once. Section "Why `mul_rows` is blocked" below explains why that is worth a
//! second loop shape.
//!
//! [`Kernel::is_supported`]: super::Kernel::is_supported

// The one place in the crate where `unsafe` is permitted, and only for
// intrinsics. `forbid(unsafe_op_in_unsafe_fn)` is still in force from the
// crate root, so every operation below carries its own `unsafe` block and its
// own justification.
#![allow(unsafe_code)]
// Which `core::arch::aarch64` intrinsics are `unsafe fn` has changed across
// compiler versions. The blocks below are written for the declared MSRV, where
// they are all required; a newer compiler sees some of them as redundant. The
// alternative is code that fails to build on the MSRV, which is worse.
#![allow(unused_unsafe)]

use core::arch::aarch64::{
    uint8x16_t, vandq_u8, vdupq_n_u8, veorq_u8, vld1q_u8, vqtbl1q_u8, vshrq_n_u8, vst1q_u8,
};

use super::{scalar, tables};
use crate::data::ec::shards::ShardSource;

/// Bytes per vector register.
const LANES: usize = 16;
/// Bytes per iteration of the main loop. Four independent vectors give the
/// out-of-order engine enough to work with to hide the shuffle latency.
const BLOCK: usize = 4 * LANES;

/// The two shuffle tables for one scalar, plus the nibble mask.
#[derive(Clone, Copy)]
struct Shuffles {
    low: uint8x16_t,
    high: uint8x16_t,
    mask: uint8x16_t,
}

/// Loads the split tables for `c` into registers.
#[inline]
#[target_feature(enable = "neon")]
unsafe fn shuffles(c: u8) -> Shuffles {
    let table = tables::SPLIT.get(c);
    Shuffles {
        // SAFETY: `table` is a `&[u8; 32]`, so 16 bytes are readable at offset
        // `LOW` (0) and 16 more at offset `HIGH` (16). `vld1q_u8` has no
        // alignment requirement.
        low: unsafe { vld1q_u8(table.as_ptr().add(tables::LOW)) },
        // SAFETY: as above.
        high: unsafe { vld1q_u8(table.as_ptr().add(tables::HIGH)) },
        // SAFETY: `vdupq_n_u8` reads no memory.
        mask: unsafe { vdupq_n_u8(0x0F) },
    }
}

/// Multiplies 16 bytes by the scalar the tables were built for.
#[inline]
#[target_feature(enable = "neon")]
unsafe fn multiply(s: uint8x16_t, t: Shuffles) -> uint8x16_t {
    // SAFETY: all of these are register-to-register operations that read no
    // memory. The shuffle indices are nibbles, so every one is below 16 and
    // within the table.
    unsafe {
        let low = vqtbl1q_u8(t.low, vandq_u8(s, t.mask));
        let high = vqtbl1q_u8(t.high, vshrq_n_u8(s, 4));
        veorq_u8(low, high)
    }
}

/// `dst = c * src`, where `dst` and `src` have equal length.
#[inline]
#[target_feature(enable = "neon")]
pub(super) unsafe fn mul_slice(c: u8, src: &[u8], dst: &mut [u8]) {
    debug_assert_eq!(src.len(), dst.len());
    // `mul(0, x) == 0` and `mul(1, x) == x`. The table path handles both
    // correctly; these are shortcuts, and they exist only to mirror the scalar
    // reference exactly.
    match c {
        0 => return dst.fill(0),
        1 => return dst.copy_from_slice(src),
        _ => {}
    }

    // SAFETY: this function requires `neon`.
    let t = unsafe { shuffles(c) };
    let len = src.len();
    let mut i = 0;

    while i + BLOCK <= len {
        for lane in 0..4 {
            let offset = i + lane * LANES;
            // SAFETY: `offset + LANES <= i + BLOCK <= len`, and `src` and
            // `dst` both have `len` bytes.
            unsafe {
                let s = vld1q_u8(src.as_ptr().add(offset));
                vst1q_u8(dst.as_mut_ptr().add(offset), multiply(s, t));
            }
        }
        i += BLOCK;
    }

    while i + LANES <= len {
        // SAFETY: `i + LANES <= len`, and both slices have `len` bytes.
        unsafe {
            let s = vld1q_u8(src.as_ptr().add(i));
            vst1q_u8(dst.as_mut_ptr().add(i), multiply(s, t));
        }
        i += LANES;
    }

    scalar::mul_slice(c, &src[i..], &mut dst[i..]);
}

/// `dst ^= c * src`, where `dst` and `src` have equal length.
#[inline]
#[target_feature(enable = "neon")]
pub(super) unsafe fn mul_add_slice(c: u8, src: &[u8], dst: &mut [u8]) {
    debug_assert_eq!(src.len(), dst.len());
    if c == 0 {
        return;
    }

    // SAFETY: this function requires `neon`.
    let t = unsafe { shuffles(c) };
    let len = src.len();
    let mut i = 0;

    while i + BLOCK <= len {
        for lane in 0..4 {
            let offset = i + lane * LANES;
            // SAFETY: `offset + LANES <= i + BLOCK <= len`, and `src` and
            // `dst` both have `len` bytes.
            unsafe {
                let s = vld1q_u8(src.as_ptr().add(offset));
                let d = vld1q_u8(dst.as_ptr().add(offset));
                vst1q_u8(dst.as_mut_ptr().add(offset), veorq_u8(d, multiply(s, t)));
            }
        }
        i += BLOCK;
    }

    while i + LANES <= len {
        // SAFETY: `i + LANES <= len`, and both slices have `len` bytes.
        unsafe {
            let s = vld1q_u8(src.as_ptr().add(i));
            let d = vld1q_u8(dst.as_ptr().add(i));
            vst1q_u8(dst.as_mut_ptr().add(i), veorq_u8(d, multiply(s, t)));
        }
        i += LANES;
    }

    scalar::mul_add_slice(c, &src[i..], &mut dst[i..]);
}

// ---------------------------------------------------------------------------
// Why `mul_rows` is blocked
// ---------------------------------------------------------------------------
//
// Encode runs `parity_shards` output rows over the same `data_shards` sources.
// Driving that as a loop over `mul_add_slice` walks every output shard once per
// source, so it moves `data_shards * parity_shards` shard-lengths of traffic
// through the cache rather than `data_shards + parity_shards`. It also
// re-derives each source byte's two nibble vectors once per output row.
//
// Holding `G` accumulator pairs in registers fixes both. The nibble split
// becomes loop-invariant across rows, and each output is written exactly once.
// Where the compiler has `eor3` (FEAT_SHA3, which Apple silicon has and which
// `aarch64-apple-darwin` enables by default) `mul_add_slice`'s body is five SIMD
// operations per 16 bytes -- and, tbl, ushr, tbl, eor3 -- so `G` rows cost
// `10G` per 32 bytes per source. Blocked, the same work is two `and`s and two
// `ushr`s once, plus four `tbl`s and two `eor3`s per row: `4 + 6G`. Without
// `eor3` the two sides are `12G` and `4 + 8G`, so the shape of the argument
// does not depend on it.
//
// aarch64 has 32 vector registers, which is what makes `G = 8` (16
// accumulators, plus the source and table vectors) fit without spilling. On
// x86, where there are 16, it does not; see the head of `x86.rs`.
//
// Register blocking is used for wide layouts. The small fixed geometries
// have dedicated loops; two-row blocking handles the remaining narrow
// groups. Benchmark results and provenance are in docs/erasure-codec.md.

/// Bytes per iteration of the blocked main loop: two vectors, so one nibble
/// split feeds two shuffles per row.
const PAIR: usize = 2 * LANES;

/// Peels `W` output shards off the front of `rest`, as `W` disjoint slices.
///
/// This is the `src/driver.rs` house style, `split_at_mut` to establish
/// disjointness once at a boundary, generalized from one split-out shard to
/// `W` of them. Holding the group as `[&mut [u8]; W]` rather than indexing back
/// into the outer slice is what lets the store loop below unroll into `W`
/// independent stores.
///
/// Generic over the container element so a caller's `Vec<Vec<u8>>` reaches the
/// blocked path without an adapter slice. This is where that genericity stops:
/// the group it hands to [`group`] is a plain `[&mut [u8]; W]`, so every
/// `unsafe` intrinsic below sees exactly the same type it saw before, and
/// `AsMut` is discharged here in safe code.
fn take_group<'r, const W: usize, O: AsMut<[u8]>>(
    rest: &mut &'r mut [O],
) -> Option<[&'r mut [u8]; W]> {
    if rest.len() < W {
        return None;
    }
    let (head, tail) = core::mem::take(rest).split_at_mut(W);
    *rest = tail;
    let mut shards = head.iter_mut();
    Some(core::array::from_fn(|_| {
        shards
            .next()
            .expect("split_at_mut(W) yields exactly W shards")
            .as_mut()
    }))
}

/// Computes `G` consecutive output rows, starting at row `first`, in one pass
/// over the sources.
///
/// Source lengths and coefficient rows are validated once before the vector
/// loops; every output window is bounds checked before each store.
#[target_feature(enable = "neon")]
unsafe fn group<const G: usize, S: ShardSource + ?Sized>(
    matrix: &[u8],
    data_shards: usize,
    first: usize,
    srcs: &S,
    outs: [&mut [u8]; G],
    len: usize,
) {
    // Cut to `len` once, here, rather than per row in the tail below. Every
    // caller already passes rows of exactly `len` bytes and `Kernel::mul_rows`
    // debug-asserts it, but deriving the bound instead of trusting it costs one
    // check per row per group and removes the only place in this function where
    // an output's own length could disagree with the length the loops use.
    //
    // Doing it here rather than in the tail is not a stylistic choice. Slicing
    // the tail as `out[i..len]` keeps `len` live across both vector loops and
    // measures 16% slower at 8+4 and 10% at 4+2, while being free at 16+8;
    // hoisting it out costs nothing at any width.
    let mut outs: [&mut [u8]; G] = outs.map(|out| &mut out[..len]);

    // Sliced once per group, so the inner loop indexes a `&[u8]` of exactly
    // `data_shards` bytes with a loop counter that is already known to be
    // below `data_shards`.
    let coeffs: [&[u8]; G] =
        core::array::from_fn(|t| &matrix[(first + t) * data_shards..(first + t + 1) * data_shards]);

    // Validate every source once, then keep its address in a fixed-size
    // stack array. The codec limits geometry to MAX_SHARDS.
    let mut sources = [core::ptr::null(); crate::data::ec::MAX_SHARDS];
    for (j, ptr) in sources[..data_shards].iter_mut().enumerate() {
        *ptr = srcs.shard(j)[..len].as_ptr();
    }
    let sources = &sources[..data_shards];

    // SAFETY: neither reads memory, and this function requires `neon`.
    let (mask, zero) = unsafe { (vdupq_n_u8(0x0F), vdupq_n_u8(0)) };
    let mut i = 0;

    while i + PAIR <= len {
        let mut low_half = [zero; G];
        let mut high_half = [zero; G];
        for (j, &source) in sources.iter().enumerate() {
            // SAFETY: each source was checked to contain len bytes;
            // i + PAIR <= len. The rest are register-to-register.
            let (l0, h0, l1, h1) = unsafe {
                let s0 = vld1q_u8(source.add(i));
                let s1 = vld1q_u8(source.add(i + LANES));
                (
                    vandq_u8(s0, mask),
                    vshrq_n_u8(s0, 4),
                    vandq_u8(s1, mask),
                    vshrq_n_u8(s1, 4),
                )
            };
            for (t, row) in coeffs.iter().enumerate() {
                let table = tables::SPLIT.get(row[j]);
                // SAFETY: `table` is a `&[u8; 32]`, so 16 bytes are readable at
                // offset `LOW` (0) and 16 more at offset `HIGH` (16). The
                // shuffle indices are nibbles, so every one is below 16 and
                // within the table.
                unsafe {
                    let lo = vld1q_u8(table.as_ptr().add(tables::LOW));
                    let hi = vld1q_u8(table.as_ptr().add(tables::HIGH));
                    let p0 = veorq_u8(vqtbl1q_u8(lo, l0), vqtbl1q_u8(hi, h0));
                    let p1 = veorq_u8(vqtbl1q_u8(lo, l1), vqtbl1q_u8(hi, h1));
                    low_half[t] = veorq_u8(low_half[t], p0);
                    high_half[t] = veorq_u8(high_half[t], p1);
                }
            }
        }
        for t in 0..G {
            let out = &mut outs[t][i..i + PAIR];
            // SAFETY: `out` is `PAIR` == 32 bytes long, so 16 bytes are
            // writable at offset 0 and 16 more at offset `LANES`.
            unsafe {
                vst1q_u8(out.as_mut_ptr(), low_half[t]);
                vst1q_u8(out.as_mut_ptr().add(LANES), high_half[t]);
            }
        }
        i += PAIR;
    }

    while i + LANES <= len {
        let mut acc = [zero; G];
        for (j, &source) in sources.iter().enumerate() {
            // SAFETY: source length was checked and i + LANES <= len.
            let (low, high) = unsafe {
                let s = vld1q_u8(source.add(i));
                (vandq_u8(s, mask), vshrq_n_u8(s, 4))
            };
            for (t, row) in coeffs.iter().enumerate() {
                let table = tables::SPLIT.get(row[j]);
                // SAFETY: as in the loop above.
                unsafe {
                    let lo = vld1q_u8(table.as_ptr().add(tables::LOW));
                    let hi = vld1q_u8(table.as_ptr().add(tables::HIGH));
                    acc[t] = veorq_u8(acc[t], veorq_u8(vqtbl1q_u8(lo, low), vqtbl1q_u8(hi, high)));
                }
            }
        }
        for t in 0..G {
            let out = &mut outs[t][i..i + LANES];
            // SAFETY: `out` is `LANES` == 16 bytes long.
            unsafe { vst1q_u8(out.as_mut_ptr(), acc[t]) }
        }
        i += LANES;
    }

    // The residue is under 16 bytes and is handed to the scalar reference
    // itself, one row at a time, exactly as `mul_slice` and `mul_add_slice`
    // hand it over today. Nothing about the tail is new code, so nothing about
    // it can disagree with the specification.
    //
    // `out` is already exactly `len` bytes, so `out[i..]` is the residue and
    // nothing here has to trust that. The sources are not cut the same way:
    // `srcs.shard(j)[i..len]` measures 16% slower at 8+4 and 10% at 4+2,
    // because it keeps `len` live through both vector loops above. They are
    // instead required to be `len` bytes by `Kernel::mul_rows`, which
    // debug-asserts it, and every codec path checks it before calling. A source
    // longer than `len` would trip the length assertion inside the scalar
    // reference rather than corrupt anything.
    for (t, out) in outs.iter_mut().enumerate() {
        let tail = &mut out[i..];
        scalar::mul_slice(coeffs[t][0], &srcs.shard(0)[i..], tail);
        for (j, &c) in coeffs[t].iter().enumerate().skip(1) {
            scalar::mul_add_slice(c, &srcs.shard(j)[i..], tail);
        }
    }
}

/// Two data shards and one parity are common on small pools. Keep both
/// coefficient tables in registers and assign each output vector once.
#[target_feature(enable = "neon")]
unsafe fn two_sources(c0: u8, c1: u8, x: &[u8], y: &[u8], out: &mut [u8]) {
    assert_eq!(x.len(), y.len());
    assert_eq!(x.len(), out.len());
    let len = x.len();
    let t0 = unsafe { shuffles(c0) };
    let t1 = unsafe { shuffles(c1) };
    let mut i = 0;
    while i + BLOCK <= len {
        for lane in 0..4 {
            let off = i + lane * LANES;
            // SAFETY: both sources and destination have a full vector here.
            unsafe {
                let a = vld1q_u8(x.as_ptr().add(off));
                let b = vld1q_u8(y.as_ptr().add(off));
                vst1q_u8(
                    out.as_mut_ptr().add(off),
                    veorq_u8(multiply(a, t0), multiply(b, t1)),
                );
            }
        }
        i += BLOCK;
    }
    while i + LANES <= len {
        // SAFETY: the full vector lies in all three equal-length slices.
        unsafe {
            let a = vld1q_u8(x.as_ptr().add(i));
            let b = vld1q_u8(y.as_ptr().add(i));
            vst1q_u8(
                out.as_mut_ptr().add(i),
                veorq_u8(multiply(a, t0), multiply(b, t1)),
            );
        }
        i += LANES;
    }
    scalar::mul_slice(c0, &x[i..], &mut out[i..]);
    scalar::mul_add_slice(c1, &y[i..], &mut out[i..]);
}

/// A fixed 4+2 geometry keeps all eight split tables in registers.
#[target_feature(enable = "neon")]
unsafe fn four_two<S: ShardSource + ?Sized, O: AsMut<[u8]>>(
    matrix: &[u8],
    srcs: &S,
    outs: &mut [O],
    len: usize,
) {
    let src: [&[u8]; 4] = core::array::from_fn(|j| &srcs.shard(j)[..len]);
    let (left, right) = outs.split_at_mut(1);
    let out0 = &mut left[0].as_mut()[..len];
    let out1 = &mut right[0].as_mut()[..len];
    // SAFETY: this function requires NEON.
    let t0: [Shuffles; 4] = core::array::from_fn(|j| unsafe { shuffles(matrix[j]) });
    let t1: [Shuffles; 4] = core::array::from_fn(|j| unsafe { shuffles(matrix[4 + j]) });
    let mask = unsafe { vdupq_n_u8(15) };
    let zero = unsafe { vdupq_n_u8(0) };
    let mut i = 0;
    while i + PAIR <= len {
        let mut a0 = zero;
        let mut a1 = zero;
        let mut b0 = zero;
        let mut b1 = zero;
        // SAFETY: all source/output slices were checked to len; each
        // vector is within the full pair. Tables have 16 entries each.
        unsafe {
            macro_rules! accumulate {
                ($j:literal) => {{
                    let x = vld1q_u8(src[$j].as_ptr().add(i));
                    let y = vld1q_u8(src[$j].as_ptr().add(i + LANES));
                    let lx = vandq_u8(x, mask);
                    let hx = vshrq_n_u8(x, 4);
                    let ly = vandq_u8(y, mask);
                    let hy = vshrq_n_u8(y, 4);
                    a0 = veorq_u8(
                        a0,
                        veorq_u8(vqtbl1q_u8(t0[$j].low, lx), vqtbl1q_u8(t0[$j].high, hx)),
                    );
                    a1 = veorq_u8(
                        a1,
                        veorq_u8(vqtbl1q_u8(t0[$j].low, ly), vqtbl1q_u8(t0[$j].high, hy)),
                    );
                    b0 = veorq_u8(
                        b0,
                        veorq_u8(vqtbl1q_u8(t1[$j].low, lx), vqtbl1q_u8(t1[$j].high, hx)),
                    );
                    b1 = veorq_u8(
                        b1,
                        veorq_u8(vqtbl1q_u8(t1[$j].low, ly), vqtbl1q_u8(t1[$j].high, hy)),
                    );
                }};
            }
            accumulate!(0);
            accumulate!(1);
            accumulate!(2);
            accumulate!(3);
            vst1q_u8(out0.as_mut_ptr().add(i), a0);
            vst1q_u8(out0.as_mut_ptr().add(i + LANES), a1);
            vst1q_u8(out1.as_mut_ptr().add(i), b0);
            vst1q_u8(out1.as_mut_ptr().add(i + LANES), b1);
        }
        i += PAIR;
    }
    for (r, out) in [out0, out1].into_iter().enumerate() {
        scalar::mul_slice(matrix[r * 4], &src[0][i..], &mut out[i..]);
        for j in 1..4 {
            scalar::mul_add_slice(matrix[r * 4 + j], &src[j][i..], &mut out[i..]);
        }
    }
}

/// Compute parity rows with fixed-geometry kernels or register groups.
#[target_feature(enable = "neon")]
pub(super) unsafe fn mul_rows<S: ShardSource + ?Sized, O: AsRef<[u8]> + AsMut<[u8]>>(
    matrix: &[u8],
    data_shards: usize,
    srcs: &S,
    outs: &mut [O],
) {
    let Some(first_out) = outs.first() else {
        return;
    };
    let len = first_out.as_ref().len();
    if data_shards == 2 && outs.len() == 1 {
        // SAFETY: same target feature; the helper checks slice lengths.
        unsafe {
            two_sources(
                matrix[0],
                matrix[1],
                srcs.shard(0),
                srcs.shard(1),
                outs[0].as_mut(),
            )
        };
        return;
    }

    if data_shards == 4 && outs.len() == 2 {
        // SAFETY: same target feature; all accesses are checked.
        unsafe { four_two(matrix, srcs, outs, len) };
        return;
    }
    let mut rest: &mut [O] = outs;
    let mut first = 0;

    macro_rules! peel {
        ($($width:literal),+) => {$(
            while let Some(g) = take_group::<$width, O>(&mut rest) {
                // SAFETY: this function requires `neon`.
                unsafe { group::<$width, S>(matrix, data_shards, first, srcs, g, len) };
                first += $width;
            }
        )+};
    }
    peel!(8, 4);
    if data_shards >= 4 {
        peel!(2);
    }

    // Remaining rows use the single-output slice operations.
    for (t, out) in rest.iter_mut().enumerate() {
        let row = &matrix[(first + t) * data_shards..][..data_shards];
        let out = out.as_mut();
        // SAFETY: this function requires `neon`.
        unsafe { mul_slice(row[0], &srcs.shard(0)[..len], &mut out[..len]) };
        for (j, &c) in row.iter().enumerate().skip(1) {
            // SAFETY: this function requires `neon`.
            unsafe { mul_add_slice(c, &srcs.shard(j)[..len], &mut out[..len]) };
        }
    }
}
