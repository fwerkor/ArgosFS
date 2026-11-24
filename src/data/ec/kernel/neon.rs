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
// Measured end to end through `Codec::encode_all` on an Apple M1 Pro at
// 1200-byte shards, against the same benchmark run on the previous row-major
// driver: 1.31x at 8+4, 1.44x at 16+8, 1.44x at 32+16 and 1.46x at 64+16. The
// `tbl` count is identical either way -- blocking removes the surrounding work,
// not the shuffles -- so the win comes from the traffic and the nibble split,
// and it grows with `data_shards * parity_shards`. At 4+2, where the whole
// working set is a few kilobytes and the shuffles are the bottleneck, this is
// *slower* than the row-major loop: 6% at 64 bytes, 8% at 256, 6% at 1200, 8%
// at 4 KB, 5% at 16 KB and 1% at 64 KB. That is the one configuration
// where it does not pay, and it is left alone rather than special-cased: a
// narrow-group fallback to the row-major path helps 4+2 by about 7% but would
// cost far more at, say, 64+2, where the traffic argument is at its strongest
// and the group is still only two rows wide.
//
// Small-split figures here move by several percent with machine state, and the
// narrow peels move most. Any re-measurement has to A/B against the previous
// driver in the same session on an otherwise idle machine; comparing a fresh
// run against a published table will overstate the gain.
//
// The same blocking is a *loss* on the scalar path at every configuration,
// which is why `scalar::mul_rows` stays a plain driver over the reference pair.

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
/// Every index below is bounds checked. That is not a concession: a 32-byte
/// window costs one range check per source per block, amortized over `G` rows,
/// and slicing the `G` coefficient rows once per group rather than indexing
/// `matrix` per element keeps the checks out of the inner loop entirely. The
/// only `unsafe` here is the intrinsics.
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

    // SAFETY: neither reads memory, and this function requires `neon`.
    let (mask, zero) = unsafe { (vdupq_n_u8(0x0F), vdupq_n_u8(0)) };
    let mut i = 0;

    while i + PAIR <= len {
        let mut low_half = [zero; G];
        let mut high_half = [zero; G];
        for j in 0..data_shards {
            let window = &srcs.shard(j)[i..i + PAIR];
            // SAFETY: `window` is `PAIR` == 32 bytes long, so 16 bytes are
            // readable at offset 0 and 16 more at offset `LANES`. `vld1q_u8`
            // has no alignment requirement. The rest are register-to-register.
            let (l0, h0, l1, h1) = unsafe {
                let s0 = vld1q_u8(window.as_ptr());
                let s1 = vld1q_u8(window.as_ptr().add(LANES));
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
        for j in 0..data_shards {
            let window = &srcs.shard(j)[i..i + LANES];
            // SAFETY: `window` is `LANES` == 16 bytes long. `vld1q_u8` has no
            // alignment requirement; the rest are register-to-register.
            let (low, high) = unsafe {
                let s = vld1q_u8(window.as_ptr());
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

/// `outs[t] = sum over j of matrix[t * data_shards + j] * srcs.shard(j)`, for
/// every `t`.
///
/// Output rows are peeled in groups of 8, then 4; the one to three rows that
/// can be left over are computed row-major, by the same two operations the
/// codec used before this one existed. Eight is where the register file runs
/// out. Four is where blocking starts to pay, and the reason the peel stops
/// there is measured rather than assumed.
///
/// The arithmetic predicts a group of `G` rows issues `4 + 6G` SIMD ops per 32
/// bytes per source against row-major's `10G`, so `G = 1` should be a wash and
/// `G = 2` a 1.25x win. Neither holds. Against the row-major predecessor, on
/// 1024-byte shards on an Apple M1 Pro:
///
/// | split | `G` used | ratio |
/// |------:|---------:|------:|
/// | 8+1   | 1        | 0.67x |
/// | 4+2   | 2        | 0.92x |
/// | 8+2   | 2        | 1.04x |
/// | 4+3   | 2 then 1 | 0.80x |
/// | 4+4   | 4        | 1.19x |
/// | 8+4   | 4        | 1.32x |
/// | 16+8  | 8        | 1.44x |
/// | 64+16 | 8        | 1.47x |
///
/// A group's fixed cost - forming the row array, hoisting the output bounds,
/// re-slicing the coefficient rows, and the tail - does not shrink with `G`,
/// so at `G = 1` it is paid in full against no saving at all and costs half
/// again the row-major time. `G = 2` is worse than a wash at four data shards
/// and barely better at eight, because the saving scales with the source count
/// it is amortized over while the fixed cost does not. Only from `G = 4` is the
/// win large enough to clear the overhead at every source count in range.
///
/// Peeling narrower groups therefore made the operation *slower* at exactly the
/// splits with the least headroom: any parity count with a low bit set pulled
/// in a `G = 1` or `G = 2` group, so 8+1 lost 50% and 4+3 lost 25%. Stopping at
/// four costs the 4% that 8+2 gave up and leaves no configuration slower than
/// the row-major path it replaced.
///
/// The obvious objection is that a narrow group is only unprofitable when there
/// are also few sources, and that at many sources and few parity rows the
/// blocked form should win by a lot. It does not. The saving and the row-major
/// cost both scale with the source count, so their ratio is fixed at `10G` to
/// `4 + 6G` and only the per-group overhead varies. Measured at 1024 bytes,
/// blocked against row-major: 64+1 0.68x, 128+1 0.68x, 32+2 1.04x, 64+2 1.06x,
/// 128+2 1.01x, 250+2 0.99x, 128+3 0.89x. Many sources do not rescue a narrow
/// group; `G = 1` stays a 47% loss at any width, and the most a `G = 2` group
/// ever returns is 6%.
///
/// Recovering that 6% would take a threshold on `data_shards` as well as
/// `parity_shards`, applying to `parity_shards == 2` only and not to 1 or 3.
/// That is two interacting thresholds and a third code path for at most 6% at
/// one split, and no consumer of this crate is known to run it. Peeling at 8
/// and 4 is the whole rule.
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

    // One to three rows left. Row-major, which is what the driver ran for every
    // row before `mul_rows` existed, so this path is the predecessor exactly.
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
