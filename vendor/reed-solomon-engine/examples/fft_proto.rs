//! PROTOTYPE, NOT SHIPPING CODE. This is evidence, and it is frozen.
//!
//! Scalar Lin-Chung-Han additive FFT over GF(2^8) with a Cantor basis: the
//! "leopard8" construction, encode and decode. Built to answer open question 2
//! in `docs/DESIGN.md` — does an FFT engine dominate the O(k*m) matrix engine
//! across the whole range up to 256 shards? The answer is no, and section 8b
//! of that document is the write-up. **This file is the only thing in the
//! repository that can re-derive the numbers in it.**
//!
//! Every run re-checks itself before reporting anything: an independent O(n^2)
//! transform oracle, the truncated transforms against the untruncated ones,
//! exhaustive erasure round-trips (18,564 patterns at 12+6 on the encode side,
//! 2,509 at 6+6 on the decode side) plus sampled patterns at eleven larger
//! splits, and the analytic Cauchy generator compared against what the encoder
//! actually computes. That is what makes section 8b checkable rather than
//! merely asserted, and it is the reason this file is in the repository.
//!
//! This is the *corrected* implementation. An earlier one omitted two
//! optimizations that `klauspost/reedsolomon`'s `leopard8.go` has, the
//! truncated IFFT (`mtrunc = m + k`, not `n`) and `errorBitfield` pruning, and
//! so overstated the FFT's decode cost by 22% to 29%. Every figure in section
//! 8b comes from this version. If a decode ratio ever reads 4.44 at 8+4 at the
//! correction limit rather than 3.91, the pruning has been lost.
//!
//! Nothing here touches the shipping engine. It borrows only `gf` (the field),
//! and it is not compiled into the library.
//!
//! **Freeze policy.** Change this only to keep it building, or to answer a new
//! question with a new verified result. Do not refactor it for style: its
//! value is that it is checked, and every edit spends some of that. If it ever
//! stops building against the shipping crate, fix the build rather than
//! deleting the file, or section 8b becomes an assertion with nothing behind
//! it.
//!
//! Run with:
//!
//! ```text
//! cargo run --release --example fft_proto            # encode analysis
//! cargo run --release --example fft_proto decode     # decode op counts
//! cargo run --release --example fft_proto decode2    # decode, cost-weighted
//! ```
//!
//! ---------------------------------------------------------------------------
//! The mathematics, stated so the code below can be checked against it
//! ---------------------------------------------------------------------------
//!
//! Cantor basis: v_0 = 1 and v_i^2 + v_i = v_{i-1}. Define the normalized
//! subspace vanishing polynomials by s_0(x) = x and s_i(x) = s_{i-1}(x)^2 +
//! s_{i-1}(x). Then:
//!
//!   * s_i is F2-linear (it is an i-fold composition of x -> x^2 + x),
//!   * s_i(v_j) = v_{j-i} for j >= i, so s_i(v_i) = v_0 = 1,
//!   * ker(s_i) = V_i = span(v_0 .. v_{i-1}).
//!
//! Write omega_i = XOR of v_b over the set bits b of i. omega is F2-linear in
//! the *index*: omega_a ^ omega_b = omega_{a^b}. Hence s_i(omega_r) =
//! omega_{r >> i} whenever every set bit of r is >= i.
//!
//! Novel polynomial basis: X_j(x) = product of s_b(x) over the set bits b of j.
//! The size-n transform at offset beta is
//!
//!   FFT_beta(f)[i] = sum_j f_j * X_j(omega_i + beta),   i, j < n = 2^l.
//!
//! Splitting f = f0 + s_{l-1} * f1 and using s_{l-1}(omega_i + beta) =
//! s_{l-1}(beta) on the first half and s_{l-1}(beta) + 1 on the second gives
//! the butterfly on *coefficients*
//!
//!   a' = a + t*b,  b' = a' + b,      t = s_{l-1}(beta),
//!
//! after which the first half recurses at offset beta and the second at offset
//! beta + v_{l-1}. Flattened, a node covering output range [r, r+2^{i+1}) of a
//! transform based at domain position p has skew t = s_i(omega_{p+r}) =
//! omega_{(p+r) >> i}. That is exactly the leopard skew table, indexed by
//! q = p + r + 2^i - 1.
//!
//! Encoder (leopard8): M = ceilPow2(parity). Parity occupies domain positions
//! 0..M-1; data block c occupies positions (c+1)*M .. (c+2)*M-1. Parity is
//! sum_c FFT_0(IFFT_{beta_c}(data block c)). The map FFT_0 . IFFT_{beta_c}
//! is Lagrange interpolation on one coset followed by evaluation on another,
//! which works out to the Cauchy-like closed form
//!
//!   G[i][(c,j)] = omega_{c+1} / omega_{(c+1)*M + (i ^ j)}
//!
//! (derived in the comments on `analytic_generator`). Every square submatrix of
//! a Cauchy matrix on distinct nodes is non-singular, so the code is MDS.
//!
//! The closed form is *not* used by the encoder; it is an independent oracle.

// The loops below index by domain position because the mathematics does:
// `omega_i`, `omega_{m + d}` and `X_j(omega_i + beta)` are statements about
// positions, and an `enumerate()` rewrite would put the reader one renaming
// away from the derivation above. The correspondence is worth more here than
// the lint, and rewriting verified code to satisfy style spends verification
// for nothing. See the freeze policy above.
#![allow(clippy::needless_range_loop)]

use reed_solomon_engine::gf;

// ===========================================================================
// Field-level helpers
// ===========================================================================

/// `s_i(x)`, the i-fold composition of `x -> x^2 + x`.
fn s_map(i: usize, mut x: u8) -> u8 {
    for _ in 0..i {
        x = gf::mul(x, x) ^ x;
    }
    x
}

/// `X_j(x)` in the novel polynomial basis.
fn novel(j: usize, x: u8) -> u8 {
    let mut acc = 1u8;
    for b in 0..8 {
        if (j >> b) & 1 == 1 {
            acc = gf::mul(acc, s_map(b, x));
        }
    }
    acc
}

struct Field {
    /// Cantor basis v_0 .. v_7.
    v: [u8; 8],
    /// omega[i] = XOR of v_b over set bits b of i.
    omega: [u8; 256],
    /// skew[q] = s_i(omega_r) where 2^i is the low bit of q+1 and r = q+1-2^i.
    skew: [u8; 255],
}

impl Field {
    fn new() -> Field {
        // v_0 = 1; v_i is a root of x^2 + x = v_{i-1}. Both roots (b and b+1)
        // are equally valid: the induction only uses v_i^2 + v_i = v_{i-1}.
        let mut v = [0u8; 8];
        v[0] = 1;
        for i in 1..8 {
            let target = v[i - 1];
            let mut found = None;
            for x in 0u16..256 {
                let x = x as u8;
                if gf::mul(x, x) ^ x == target {
                    found = Some(x);
                    break;
                }
            }
            v[i] = found.unwrap_or_else(|| {
                panic!("no Cantor basis: x^2+x = {target:#04x} has no root in GF(2^8)")
            });
        }

        // Independence follows from s_i(v_i) = 1, which we check outright.
        for (i, &vi) in v.iter().enumerate() {
            assert_eq!(s_map(i, vi), 1, "s_{i}(v_{i}) != 1; basis is degenerate");
        }

        let mut omega = [0u8; 256];
        for (i, slot) in omega.iter_mut().enumerate() {
            let mut acc = 0u8;
            for (b, &vb) in v.iter().enumerate() {
                if (i >> b) & 1 == 1 {
                    acc ^= vb;
                }
            }
            *slot = acc;
        }
        // omega must be a bijection 0..256 -> GF(2^8) (basis independence).
        let mut seen = [false; 256];
        for &w in omega.iter() {
            assert!(!seen[w as usize], "Cantor basis is linearly dependent");
            seen[w as usize] = true;
        }

        let mut skew = [0u8; 255];
        for (q, slot) in skew.iter_mut().enumerate() {
            let vv = q + 1;
            let low = vv & vv.wrapping_neg();
            let i = low.trailing_zeros() as usize;
            let r = vv - low;
            // Computed the slow way, cross-checked against the shift identity.
            *slot = s_map(i, omega[r]);
            assert_eq!(
                *slot,
                omega[r >> i],
                "skew identity s_i(omega_r) = omega_{{r>>i}}"
            );
        }

        Field { v, omega, skew }
    }
}

// ===========================================================================
// Direct O(n^2) reference transform  (written from the definition, not from
// the butterfly recursion, so it is an independent check of the fast one)
// ===========================================================================

/// `M[i][j] = X_j(omega_i + beta)`; `FFT_beta(f) = M f`.
fn direct_fft_matrix(n: usize, beta: u8, f: &Field) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| (0..n).map(|j| novel(j, f.omega[i] ^ beta)).collect())
        .collect()
}

fn mat_vec(m: &[Vec<u8>], x: &[u8]) -> Vec<u8> {
    m.iter()
        .map(|row| {
            row.iter()
                .zip(x)
                .fold(0u8, |acc, (&a, &b)| acc ^ gf::mul(a, b))
        })
        .collect()
}

/// Gauss-Jordan inverse over GF(2^8). `None` if singular.
fn invert(a: &[Vec<u8>]) -> Option<Vec<Vec<u8>>> {
    let n = a.len();
    let mut m: Vec<Vec<u8>> = a
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let mut r = row.clone();
            r.extend((0..n).map(|j| u8::from(i == j)));
            r
        })
        .collect();

    for col in 0..n {
        let pivot = (col..n).find(|&r| m[r][col] != 0)?;
        m.swap(col, pivot);
        let inv = gf::inv(m[col][col]);
        for x in m[col].iter_mut() {
            *x = gf::mul(*x, inv);
        }
        for r in 0..n {
            if r != col && m[r][col] != 0 {
                let f = m[r][col];
                let pivot_row = m[col].clone();
                for (cell, &p) in m[r].iter_mut().zip(pivot_row.iter()) {
                    *cell ^= gf::mul(f, p);
                }
            }
        }
    }
    Some(m.into_iter().map(|row| row[n..].to_vec()).collect())
}

fn is_singular(a: &[Vec<u8>]) -> bool {
    invert(a).is_none()
}

// ===========================================================================
// Fast transform, instrumented
// ===========================================================================

#[derive(Default, Clone, Copy, Debug)]
struct Ops {
    /// Passes over a shard doing `dst ^= c * src`.
    mul_add: u64,
    /// Passes over a shard doing `dst ^= src`.
    xor: u64,
    /// Passes over a shard doing a copy or a clear.
    mem: u64,
}

impl Ops {
    fn total(&self) -> u64 {
        self.mul_add + self.xor + self.mem
    }
}

/// `dst ^= src`
fn xor_into(w: &mut [Vec<u8>], dst: usize, src: usize) {
    debug_assert_ne!(dst, src);
    let (lo, hi) = if dst < src {
        let (a, b) = w.split_at_mut(src);
        (&mut a[dst], &b[0])
    } else {
        let (a, b) = w.split_at_mut(dst);
        (&mut b[0], &a[src])
    };
    for (d, s) in lo.iter_mut().zip(hi.iter()) {
        *d ^= *s;
    }
}

/// `dst ^= c * src`
fn mul_add_into(w: &mut [Vec<u8>], dst: usize, src: usize, c: u8) {
    debug_assert_ne!(dst, src);
    let (lo, hi) = if dst < src {
        let (a, b) = w.split_at_mut(src);
        (&mut a[dst], &b[0])
    } else {
        let (a, b) = w.split_at_mut(dst);
        (&mut b[0], &a[src])
    };
    for (d, s) in lo.iter_mut().zip(hi.iter()) {
        *d ^= gf::mul(c, *s);
    }
}

/// FFT butterfly: `x ^= t*y; y ^= x`.
fn fft2(w: &mut [Vec<u8>], x: usize, y: usize, t: u8, ops: &mut Ops) {
    if t != 0 {
        mul_add_into(w, x, y, t);
        ops.mul_add += 1;
    }
    xor_into(w, y, x);
    ops.xor += 1;
}

/// IFFT butterfly, the exact inverse of `fft2`: `y ^= x; x ^= t*y`.
fn ifft2(w: &mut [Vec<u8>], x: usize, y: usize, t: u8, ops: &mut Ops) {
    xor_into(w, y, x);
    ops.xor += 1;
    if t != 0 {
        mul_add_into(w, x, y, t);
        ops.mul_add += 1;
    }
}

/// Forward transform of `w[0..n]`, based at domain position `p` (so the offset
/// is `beta = omega_p`). Only outputs `0..out_trunc` are guaranteed correct;
/// sub-trees entirely outside that range are skipped, which is exactly the
/// "skip regions known to be zero" saving on the encode path.
fn fft(w: &mut [Vec<u8>], n: usize, p: usize, out_trunc: usize, f: &Field, ops: &mut Ops) {
    let l = n.trailing_zeros() as usize;
    for i in (0..l).rev() {
        let dist = 1usize << i;
        let mut r = 0;
        while r < n {
            if r < out_trunc {
                let t = f.skew[p + r + dist - 1];
                for j in r..r + dist {
                    fft2(w, j, j + dist, t, ops);
                }
            }
            r += dist * 2;
        }
    }
}

/// Forward transform pruned to the outputs actually read back, which is what
/// leopard's `errorBitfield` does (see `errorBitfield8.fftDIT8`: it skips a
/// butterfly group whenever `isNeeded(mipLevel, r)` is false). A block
/// `[r, r+2*dist)` at layer `dist` feeds only outputs inside that block, so it
/// can be skipped outright when no output in it is wanted. Leopard prunes at
/// DIT4 granularity; this prunes at single-layer granularity, so it is a
/// slightly *optimistic* stand-in for leopard, never a pessimistic one.
fn fft_pruned(
    w: &mut [Vec<u8>],
    n: usize,
    p: usize,
    out_trunc: usize,
    needed: &[bool],
    f: &Field,
    ops: &mut Ops,
) {
    let l = n.trailing_zeros() as usize;
    for i in (0..l).rev() {
        let dist = 1usize << i;
        let mut r = 0;
        while r < n {
            let hi = core::cmp::min(r + dist * 2, n);
            let wanted = (r..hi).any(|q| q < out_trunc && needed[q]);
            if r < out_trunc && wanted {
                let t = f.skew[p + r + dist - 1];
                for j in r..r + dist {
                    fft2(w, j, j + dist, t, ops);
                }
            }
            r += dist * 2;
        }
    }
}

/// Inverse transform of `w[0..n]` based at domain position `p`. Inputs
/// `in_trunc..n` are assumed zero, and sub-trees whose whole input range is
/// zero are skipped.
fn ifft(w: &mut [Vec<u8>], n: usize, p: usize, in_trunc: usize, f: &Field, ops: &mut Ops) {
    let l = n.trailing_zeros() as usize;
    for i in 0..l {
        let dist = 1usize << i;
        let mut r = 0;
        while r < n {
            if r < in_trunc {
                let t = f.skew[p + r + dist - 1];
                for j in r..r + dist {
                    ifft2(w, j, j + dist, t, ops);
                }
            }
            r += dist * 2;
        }
    }
}

// ---------------------------------------------------------------------------
// The same two transforms written the way leopard actually writes them: two
// layers at a time. The butterfly count is identical, but the truncation is
// coarser, because a skipped unit is now four layers-worth of work at two
// strides rather than one block at one stride. That difference is the only
// reason both variants exist here.
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn fft_dit4(w: &mut [Vec<u8>], i: usize, dist: usize, m01: u8, m23: u8, m02: u8, ops: &mut Ops) {
    fft2(w, i, i + dist * 2, m02, ops);
    fft2(w, i + dist, i + dist * 3, m02, ops);
    fft2(w, i, i + dist, m01, ops);
    fft2(w, i + dist * 2, i + dist * 3, m23, ops);
}

#[allow(clippy::too_many_arguments)]
fn ifft_dit4(w: &mut [Vec<u8>], i: usize, dist: usize, m01: u8, m23: u8, m02: u8, ops: &mut Ops) {
    ifft2(w, i, i + dist, m01, ops);
    ifft2(w, i + dist * 2, i + dist * 3, m23, ops);
    ifft2(w, i, i + dist * 2, m02, ops);
    ifft2(w, i + dist, i + dist * 3, m02, ops);
}

fn fft_leo(w: &mut [Vec<u8>], n: usize, p: usize, out_trunc: usize, f: &Field, ops: &mut Ops) {
    let mut dist4 = n;
    let mut dist = n >> 2;
    while dist != 0 {
        let mut r = 0;
        while r < out_trunc {
            let iend = r + dist;
            let m01 = f.skew[p + iend - 1];
            let m02 = f.skew[p + iend + dist - 1];
            let m23 = f.skew[p + iend + dist * 2 - 1];
            for i in r..iend {
                fft_dit4(w, i, dist, m01, m23, m02, ops);
            }
            r += dist4;
        }
        dist4 = dist;
        dist >>= 2;
    }
    if dist4 == 2 {
        let mut r = 0;
        while r < out_trunc {
            let t = f.skew[p + r];
            fft2(w, r, r + 1, t, ops);
            r += 2;
        }
    }
}

fn ifft_leo(w: &mut [Vec<u8>], n: usize, p: usize, in_trunc: usize, f: &Field, ops: &mut Ops) {
    let mut dist = 1usize;
    let mut dist4 = 4usize;
    while dist4 <= n {
        let mut r = 0;
        while r < in_trunc {
            let iend = r + dist;
            let m01 = f.skew[p + iend - 1];
            let m02 = f.skew[p + iend + dist - 1];
            let m23 = f.skew[p + iend + dist * 2 - 1];
            for i in r..iend {
                ifft_dit4(w, i, dist, m01, m23, m02, ops);
            }
            r += dist4;
        }
        dist = dist4;
        dist4 <<= 2;
    }
    if dist < n {
        // Leopard does not truncate this last layer; dist == n / 2 here.
        let t = f.skew[p + dist - 1];
        for i in 0..dist {
            ifft2(w, i, i + dist, t, ops);
        }
    }
}

// ===========================================================================
// The leopard8 encoder
// ===========================================================================

fn ceil_pow2(x: usize) -> usize {
    let mut m = 1;
    while m < x {
        m *= 2;
    }
    m
}

#[derive(Clone, Copy, Debug)]
struct Layout {
    /// Transform size actually used.
    m: usize,
    /// Number of size-`m` data blocks.
    blocks: usize,
}

/// The leopard8 domain layout, or `None` if it does not fit in GF(2^8).
///
/// Parity lives in coset 0 of the size-`m` cosets of the 256-point domain and
/// data block `c` in coset `c+1`, so `(blocks + 1) * m <= 256` is required.
fn layout(k: usize, parity: usize) -> Option<Layout> {
    if k == 0 || parity == 0 || k + parity > 256 {
        return None;
    }
    let m = ceil_pow2(parity);
    let blocks = k.div_ceil(m);
    if (blocks + 1) * m > 256 {
        return None;
    }
    Some(Layout { m, blocks })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Variant {
    /// One layer at a time; truncation at the finest granularity available.
    Fine,
    /// Two layers at a time, exactly as leopard writes it.
    Leopard,
}

/// Encode. `data` is `k` shards of equal length; returns `parity` shards.
fn fft_encode_v(
    data: &[Vec<u8>],
    parity: usize,
    f: &Field,
    ops: &mut Ops,
    variant: Variant,
) -> Option<Vec<Vec<u8>>> {
    let k = data.len();
    let lay = layout(k, parity)?;
    let (m, blocks) = (lay.m, lay.blocks);
    let len = data[0].len();

    let mut work = vec![vec![0u8; len]; m];
    let mut scratch = vec![vec![0u8; len]; m];

    for c in 0..blocks {
        let base = c * m;
        let count = core::cmp::min(m, k - base);
        let p = (c + 1) * m;

        let buf = if c == 0 { &mut work } else { &mut scratch };
        for j in 0..m {
            if j < count {
                buf[j].copy_from_slice(&data[base + j]);
            } else {
                buf[j].iter_mut().for_each(|b| *b = 0);
            }
            ops.mem += 1; // one shard-length copy or clear
        }
        match variant {
            Variant::Fine => ifft(buf, m, p, count, f, ops),
            Variant::Leopard => ifft_leo(buf, m, p, count, f, ops),
        }

        if c != 0 {
            for j in 0..m {
                for (d, s) in work[j].iter_mut().zip(scratch[j].iter()) {
                    *d ^= *s;
                }
                ops.xor += 1;
            }
        }
    }

    match variant {
        Variant::Fine => fft(&mut work, m, 0, parity, f, ops),
        Variant::Leopard => fft_leo(&mut work, m, 0, parity, f, ops),
    }

    let mut out = Vec::with_capacity(parity);
    for buf in work.into_iter().take(parity) {
        ops.mem += 1; // copy out
        out.push(buf);
    }
    Some(out)
}

fn fft_encode(data: &[Vec<u8>], parity: usize, f: &Field, ops: &mut Ops) -> Option<Vec<Vec<u8>>> {
    fft_encode_v(data, parity, f, ops, Variant::Fine)
}

// ===========================================================================
// Independent oracle for the generator matrix
// ===========================================================================

/// `G[i][d]` for parity index `i` and data index `d`, from the closed form.
///
/// `A_c = FFT_0 . IFFT_{beta_c}` interpolates on the coset `S_c = V_l + beta_c`
/// and evaluates on `V_l`. With `W` the vanishing polynomial of `V_l` (an
/// F2-linear map, so `W(x + beta) = W(x) + W(beta)`, and with derivative equal
/// to its own linear coefficient `W_1`):
///
///   A_c[i][j] = L_j(omega_i) = (W(omega_i) + W(beta_c)) / ((omega_i + omega_j
///               + beta_c) * W_1) = s_l(beta_c) / (omega_i + omega_j + beta_c)
///
/// because `W(omega_i) = 0` and `W / W_1 = s_l` (the linear coefficient of s_l
/// is 1 at every l). With `beta_c = omega_{(c+1)m}` this is
/// `omega_{c+1} / omega_{(c+1)m + (i ^ j)}`.
fn analytic_generator(k: usize, parity: usize, f: &Field) -> Option<Vec<Vec<u8>>> {
    let lay = layout(k, parity)?;
    let m = lay.m;
    let mut g = vec![vec![0u8; k]; parity];
    for (i, row) in g.iter_mut().enumerate() {
        for (d, cell) in row.iter_mut().enumerate() {
            let c = d / m;
            let j = d % m;
            let p = (c + 1) * m;
            *cell = gf::div(f.omega[c + 1], f.omega[p + (i ^ j)]);
        }
    }
    Some(g)
}

/// The generator matrix as the *encoder* actually computes it: encode unit
/// vectors of length 1.
fn measured_generator(k: usize, parity: usize, f: &Field) -> Option<Vec<Vec<u8>>> {
    let mut g = vec![vec![0u8; k]; parity];
    for d in 0..k {
        let mut data = vec![vec![0u8; 1]; k];
        data[d][0] = 1;
        let mut ops = Ops::default();
        let par = fft_encode(&data, parity, f, &mut ops)?;
        for i in 0..parity {
            g[i][d] = par[i][0];
        }
    }
    Some(g)
}

// ===========================================================================
// Checks
// ===========================================================================

fn check_fft_against_direct(f: &Field) {
    let mut cases = 0usize;
    for l in 0..=7usize {
        let n = 1usize << l;
        // Every domain position a real encode can use, plus 0.
        let mut p = 0;
        while p + n <= 256 {
            let beta = f.omega[p];
            let fwd = direct_fft_matrix(n, beta, f);
            let inv = invert(&fwd).expect("novel-basis evaluation matrix must be invertible");

            for trial in 0..4u32 {
                let coeffs: Vec<u8> = (0..n)
                    .map(|j| lcg(trial as u64 * 1009 + (p * 31 + n * 7 + j) as u64))
                    .collect();

                // Fast FFT vs direct matrix-vector product.
                let mut w: Vec<Vec<u8>> = coeffs.iter().map(|&c| vec![c]).collect();
                let mut ops = Ops::default();
                fft(&mut w, n, p, n, f, &mut ops);
                let fast: Vec<u8> = w.iter().map(|b| b[0]).collect();
                let want = mat_vec(&fwd, &coeffs);
                assert_eq!(fast, want, "FFT != direct at n={n} p={p}");

                // Fast IFFT vs direct inverse matrix.
                let mut w: Vec<Vec<u8>> = fast.iter().map(|&c| vec![c]).collect();
                let mut ops = Ops::default();
                ifft(&mut w, n, p, n, f, &mut ops);
                let back: Vec<u8> = w.iter().map(|b| b[0]).collect();
                assert_eq!(back, coeffs, "IFFT(FFT(x)) != x at n={n} p={p}");
                let want_inv = mat_vec(&inv, &fast);
                assert_eq!(back, want_inv, "IFFT != direct inverse at n={n} p={p}");

                // FFT(IFFT(x)) == x as well.
                let mut w: Vec<Vec<u8>> = coeffs.iter().map(|&c| vec![c]).collect();
                let mut ops = Ops::default();
                ifft(&mut w, n, p, n, f, &mut ops);
                fft(&mut w, n, p, n, f, &mut ops);
                let rt: Vec<u8> = w.iter().map(|b| b[0]).collect();
                assert_eq!(rt, coeffs, "FFT(IFFT(x)) != x at n={n} p={p}");

                // Truncated forward transform agrees on the outputs it keeps.
                for trunc in 1..=n {
                    let mut w: Vec<Vec<u8>> = coeffs.iter().map(|&c| vec![c]).collect();
                    let mut ops = Ops::default();
                    fft(&mut w, n, p, trunc, f, &mut ops);
                    for i in 0..trunc {
                        assert_eq!(w[i][0], want[i], "truncated FFT wrong at n={n} p={p}");
                    }
                }

                // Truncated inverse transform agrees with the untruncated one
                // when the tail of the input really is zero.
                for trunc in 1..=n {
                    let mut zeroed = coeffs.clone();
                    for c in zeroed.iter_mut().skip(trunc) {
                        *c = 0;
                    }
                    let mut a: Vec<Vec<u8>> = zeroed.iter().map(|&c| vec![c]).collect();
                    let mut b: Vec<Vec<u8>> = zeroed.iter().map(|&c| vec![c]).collect();
                    let mut ops = Ops::default();
                    ifft(&mut a, n, p, trunc, f, &mut ops);
                    ifft(&mut b, n, p, n, f, &mut ops);
                    assert_eq!(a, b, "truncated IFFT wrong at n={n} p={p} trunc={trunc}");
                }

                cases += 1;
            }
            p += n;
        }
    }
    println!(
        "  fft vs direct O(n^2): {cases} randomized cases over every (n, coset) pair, all equal"
    );
}

/// The leopard-shaped (two layers at a time) transforms must agree with the
/// one-layer-at-a-time ones on every value they are allowed to produce.
fn check_leopard_variant_agrees(f: &Field) {
    let mut cases = 0usize;
    for l in 0..=7usize {
        let n = 1usize << l;
        let mut p = 0;
        while p + n <= 256 {
            let coeffs: Vec<u8> = (0..n).map(|j| lcg((p * 131 + n * 17 + j) as u64)).collect();
            for trunc in 1..=n {
                let mut a: Vec<Vec<u8>> = coeffs.iter().map(|&c| vec![c]).collect();
                let mut b = a.clone();
                let mut ops = Ops::default();
                fft(&mut a, n, p, trunc, f, &mut ops);
                fft_leo(&mut b, n, p, trunc, f, &mut ops);
                assert_eq!(
                    a[..trunc],
                    b[..trunc],
                    "fft_leo != fft n={n} p={p} trunc={trunc}"
                );

                let mut zeroed = coeffs.clone();
                for c in zeroed.iter_mut().skip(trunc) {
                    *c = 0;
                }
                let mut a: Vec<Vec<u8>> = zeroed.iter().map(|&c| vec![c]).collect();
                let mut b = a.clone();
                ifft(&mut a, n, p, trunc, f, &mut ops);
                ifft_leo(&mut b, n, p, trunc, f, &mut ops);
                assert_eq!(a, b, "ifft_leo != ifft n={n} p={p} trunc={trunc}");
                cases += 1;
            }
            p += n;
        }
    }
    println!("  leopard 2-layer variant == 1-layer variant: {cases} (n, coset, truncation) cases");
}

/// Deterministic pseudo-random bytes; no dev-dependency needed.
fn lcg(seed: u64) -> u8 {
    let mut x = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    (x >> 40) as u8
}

fn check_generator_matches_closed_form(f: &Field) {
    let splits: &[(usize, usize)] = &[
        (1, 1),
        (2, 1),
        (4, 2),
        (5, 3),
        (8, 4),
        (9, 7),
        (16, 8),
        (16, 16),
        (32, 16),
        (32, 32),
        (64, 16),
        (64, 64),
        (128, 128),
        (192, 55),
        (200, 32),
    ];
    for &(k, m) in splits {
        let Some(measured) = measured_generator(k, m, f) else {
            continue;
        };
        let analytic = analytic_generator(k, m, f).unwrap();
        assert_eq!(
            measured, analytic,
            "encoder generator != closed form at {k}+{m}"
        );
    }
    println!(
        "  encoder generator == independent Cauchy closed form at {} splits",
        splits.len()
    );
}

/// Full MDS check: every erasure pattern of exactly `parity` shards out of
/// `k + parity` is solved directly and the recovered data compared.
fn check_mds_by_solving(k: usize, parity: usize, f: &Field) -> usize {
    let n = k + parity;
    let g = measured_generator(k, parity, f).expect("layout must fit");
    // Full generator [I ; G], n rows by k columns.
    let mut full = vec![vec![0u8; k]; n];
    for (i, row) in full.iter_mut().take(k).enumerate() {
        row[i] = 1;
    }
    for (row, gi) in full[k..].iter_mut().zip(g.iter()) {
        row.clone_from(gi);
    }

    let data: Vec<u8> = (0..k).map(|i| lcg(0xabc0 + i as u64) | 1).collect();
    let code: Vec<u8> = full
        .iter()
        .map(|row| {
            row.iter()
                .zip(&data)
                .fold(0u8, |a, (&x, &y)| a ^ gf::mul(x, y))
        })
        .collect();

    let mut patterns = 0usize;
    // Iterate every subset of size `parity` from `n`.
    let mut idx: Vec<usize> = (0..parity).collect();
    loop {
        // Survivors are everything not in idx.
        let mut lost = vec![false; n];
        for &i in &idx {
            lost[i] = true;
        }
        let survivors: Vec<usize> = (0..n).filter(|&i| !lost[i]).collect();
        assert_eq!(survivors.len(), k);
        let sub: Vec<Vec<u8>> = survivors.iter().map(|&r| full[r].clone()).collect();
        let inv = invert(&sub)
            .unwrap_or_else(|| panic!("NOT MDS: {k}+{parity} singular for erasures {idx:?}"));
        let rhs: Vec<u8> = survivors.iter().map(|&r| code[r]).collect();
        let recovered = mat_vec(&inv, &rhs);
        assert_eq!(
            recovered, data,
            "recovery wrong at {k}+{parity} erasures {idx:?}"
        );
        patterns += 1;

        // Next combination.
        let mut i = parity;
        loop {
            if i == 0 {
                return patterns;
            }
            i -= 1;
            if idx[i] != i + n - parity {
                idx[i] += 1;
                for j in i + 1..parity {
                    idx[j] = idx[j - 1] + 1;
                }
                break;
            }
        }
    }
}

/// MDS by the equivalent and much cheaper criterion: every square submatrix of
/// the parity matrix is non-singular.
fn check_mds_by_submatrices(k: usize, parity: usize, f: &Field) -> usize {
    let g = measured_generator(k, parity, f).expect("layout must fit");
    let mut checked = 0usize;
    let tmax = core::cmp::min(k, parity);
    for t in 1..=tmax {
        let mut rows: Vec<usize> = (0..t).collect();
        loop {
            let mut cols: Vec<usize> = (0..t).collect();
            loop {
                let sub: Vec<Vec<u8>> = rows
                    .iter()
                    .map(|&r| cols.iter().map(|&c| g[r][c]).collect())
                    .collect();
                assert!(
                    !is_singular(&sub),
                    "NOT MDS: {k}+{parity}, singular submatrix rows {rows:?} cols {cols:?}"
                );
                checked += 1;
                if !next_combination(&mut cols, k) {
                    break;
                }
            }
            if !next_combination(&mut rows, parity) {
                break;
            }
        }
    }
    checked
}

fn next_combination(idx: &mut [usize], n: usize) -> bool {
    let t = idx.len();
    let mut i = t;
    loop {
        if i == 0 {
            return false;
        }
        i -= 1;
        if idx[i] != i + n - t {
            idx[i] += 1;
            for j in i + 1..t {
                idx[j] = idx[j - 1] + 1;
            }
            return true;
        }
    }
}

/// Sampled MDS check for sizes where exhaustive enumeration is out of reach.
fn check_mds_sampled(k: usize, parity: usize, f: &Field, trials: usize) -> usize {
    let g = measured_generator(k, parity, f).expect("layout must fit");
    let tmax = core::cmp::min(k, parity);
    let mut seed = 0x5eedu64 ^ ((k as u64) << 20) ^ ((parity as u64) << 8);
    let mut rnd = move || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) as usize
    };
    let mut checked = 0;
    for _ in 0..trials {
        let t = 1 + rnd() % tmax;
        let rows = sample_distinct(t, parity, &mut rnd);
        let cols = sample_distinct(t, k, &mut rnd);
        let sub: Vec<Vec<u8>> = rows
            .iter()
            .map(|&r| cols.iter().map(|&c| g[r][c]).collect())
            .collect();
        assert!(
            !is_singular(&sub),
            "NOT MDS (sampled): {k}+{parity} rows {rows:?} cols {cols:?}"
        );
        checked += 1;
    }
    checked
}

fn sample_distinct(t: usize, n: usize, rnd: &mut impl FnMut() -> usize) -> Vec<usize> {
    let mut pool: Vec<usize> = (0..n).collect();
    for i in 0..t {
        let j = i + rnd() % (n - i);
        pool.swap(i, j);
    }
    let mut out = pool[..t].to_vec();
    out.sort_unstable();
    out
}

/// Does the FFT code produce the same parity bytes as the shipping systematic
/// Vandermonde engine? (Design doc section 7 requires it if both engines are
/// ever to serve the same parameters transparently.)
fn check_wire_compatibility(f: &Field) {
    use reed_solomon_engine::Codec;
    let (k, m) = (4usize, 2usize);
    let len = 8;
    let data: Vec<Vec<u8>> = (0..k)
        .map(|i| {
            (0..len)
                .map(|j| lcg(0x1234 + (i * 64 + j) as u64) | 1)
                .collect()
        })
        .collect();

    let mut ops = Ops::default();
    let fft_parity = fft_encode(&data, m, f, &mut ops).unwrap();

    let enc = Codec::new(k, m).unwrap();
    let mut bufs: Vec<Vec<u8>> = data.clone();
    bufs.extend((0..m).map(|_| vec![0u8; len]));
    {
        let (d, p) = bufs.split_at_mut(k);
        let dref: Vec<&[u8]> = d.iter().map(|v| v.as_slice()).collect();
        let mut pref: Vec<&mut [u8]> = p.iter_mut().map(|v| v.as_mut_slice()).collect();
        enc.encode(&dref, &mut pref).unwrap();
    }
    let matrix_parity: Vec<Vec<u8>> = bufs[k..].to_vec();

    if fft_parity == matrix_parity {
        println!("  WIRE: FFT parity is byte-identical to the matrix engine at 4+2");
    } else {
        println!(
            "  WIRE: FFT parity DIFFERS from the matrix engine at 4+2\n         fft    {:02x?}\n         matrix {:02x?}",
            fft_parity[0], matrix_parity[0]
        );
    }
}

/// End-to-end: encode real multi-byte shards at large splits, drop `parity`
/// random shards, solve the linear system, and compare.
fn check_large_roundtrip(f: &Field) {
    for &(k, parity) in &[
        (32usize, 32usize),
        (64, 64),
        (128, 128),
        (192, 55),
        (200, 32),
    ] {
        let len = 16;
        let data: Vec<Vec<u8>> = (0..k)
            .map(|i| (0..len).map(|j| lcg(0x77 + (i * 256 + j) as u64)).collect())
            .collect();
        let mut ops = Ops::default();
        let par = fft_encode(&data, parity, f, &mut ops).expect("layout must fit");
        let par_leo = fft_encode_v(&data, parity, f, &mut ops, Variant::Leopard).unwrap();
        assert_eq!(
            par, par_leo,
            "leopard variant encodes differently at {k}+{parity}"
        );

        let g = measured_generator(k, parity, f).unwrap();
        let n = k + parity;
        let mut full = vec![vec![0u8; k]; n];
        for (i, row) in full.iter_mut().take(k).enumerate() {
            row[i] = 1;
        }
        for (row, gi) in full[k..].iter_mut().zip(g.iter()) {
            row.clone_from(gi);
        }

        let mut seed = 0x9e37u64 ^ (k as u64) << 8;
        let mut rnd = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        for _ in 0..8 {
            let lost = sample_distinct(parity, n, &mut rnd);
            let mut is_lost = vec![false; n];
            for &i in &lost {
                is_lost[i] = true;
            }
            let survivors: Vec<usize> = (0..n).filter(|&i| !is_lost[i]).collect();
            let sub: Vec<Vec<u8>> = survivors.iter().map(|&r| full[r].clone()).collect();
            let inv = invert(&sub).expect("MDS: every k-subset must be invertible");
            for byte in 0..len {
                let rhs: Vec<u8> = survivors
                    .iter()
                    .map(|&r| {
                        if r < k {
                            data[r][byte]
                        } else {
                            par[r - k][byte]
                        }
                    })
                    .collect();
                let rec = mat_vec(&inv, &rhs);
                let want: Vec<u8> = (0..k).map(|i| data[i][byte]).collect();
                assert_eq!(rec, want, "roundtrip failed at {k}+{parity}");
            }
        }
    }
    println!("  roundtrip: encode + recover from `parity` random erasures at 32+32, 64+64,");
    println!("             128+128, 192+55, 200+32 (8 patterns each, 16-byte shards)");
}

// ===========================================================================
// Measurement
// ===========================================================================

/// Raw op counts weigh a mul-add the same as an XOR, which is wrong on a
/// machine with a real kernel. Measure the actual per-op cost of the three
/// kinds of shard pass with the live kernel so the counts can be weighted.
/// `mul_add_slice` is `pub(crate)`, so the mul-add pass is timed through the
/// public `Encoder` at `k+1`, whose encode is exactly `k` shard passes (one
/// `mul_slice` and `k-1` `mul_add_slice`). All three loops sweep the same
/// number of distinct source buffers so the cache behaviour is comparable.
fn measure_op_weights(shard_len: usize) -> (f64, f64, f64) {
    use core::hint::black_box;
    use reed_solomon_engine::{Codec, Kernel};
    use std::time::Instant;

    const FAN: usize = 64;
    let srcs: Vec<Vec<u8>> = (0..FAN)
        .map(|i| {
            (0..shard_len)
                .map(|j| lcg((i * 4099 + j) as u64) | 1)
                .collect()
        })
        .collect();
    let mut dst: Vec<u8> = vec![0u8; shard_len];

    let enc = Codec::new(FAN, 1).unwrap();
    let sref: Vec<&[u8]> = srcs.iter().map(|v| v.as_slice()).collect();

    let target = core::time::Duration::from_millis(120);
    let time_it = |mut f: Box<dyn FnMut()>| -> f64 {
        for _ in 0..50 {
            f();
        }
        let mut reps = 0usize;
        let t0 = Instant::now();
        while t0.elapsed() < target {
            f();
            reps += 1;
        }
        t0.elapsed().as_secs_f64() / (reps * FAN) as f64
    };

    let mul_add = {
        let mut out = vec![0u8; shard_len];
        let sref = sref.clone();
        time_it(Box::new(move || {
            let mut p: [&mut [u8]; 1] = [&mut out];
            enc.encode(black_box(&sref), &mut p).unwrap();
        }))
    };
    let xor = {
        let srcs = srcs.clone();
        let mut d = core::mem::take(&mut dst);
        time_it(Box::new(move || {
            for s in black_box(&srcs) {
                for (a, b) in d.iter_mut().zip(s.iter()) {
                    *a ^= *b;
                }
            }
            black_box(&d);
        }))
    };
    let mem = {
        let srcs = srcs.clone();
        let mut d = vec![0u8; shard_len];
        time_it(Box::new(move || {
            for s in black_box(&srcs) {
                d.copy_from_slice(s);
            }
            black_box(&d);
        }))
    };

    println!(
        "  kernel {:?}, {shard_len:>5}-byte shards: mul_add {:>7.1} ns, xor {:>7.1} ns, copy {:>7.1} ns   (xor = {:.2} mul_adds, copy = {:.2})",
        Kernel::detect(),
        mul_add * 1e9,
        xor * 1e9,
        mem * 1e9,
        xor / mul_add,
        mem / mul_add
    );
    (mul_add, xor, mem)
}

/// End-to-end wall clock, scalar against scalar. Both sides do their mul-adds
/// with `gf::mul` a byte at a time, so this is a like-for-like check that the
/// op counts predict something real. It is *not* a prediction of the SIMD
/// world: the prototype's XOR and copy passes are plain byte loops that LLVM
/// auto-vectorizes while its mul-adds are not, which flatters the FFT here in
/// the same direction the measured NEON weights do.
fn time_scalar_encode(k: usize, parity: usize, shard_len: usize, f: &Field) -> Option<(f64, f64)> {
    use core::hint::black_box;
    use reed_solomon_engine::{Codec, Kernel};
    use std::time::{Duration, Instant};

    layout(k, parity)?;
    let data: Vec<Vec<u8>> = (0..k)
        .map(|i| {
            (0..shard_len)
                .map(|j| lcg((i * 7919 + j) as u64) | 1)
                .collect()
        })
        .collect();

    let target = Duration::from_millis(150);

    let mut ops = Ops::default();
    let mut reps = 0usize;
    let t0 = Instant::now();
    while t0.elapsed() < target {
        black_box(fft_encode(black_box(&data), parity, f, &mut ops));
        reps += 1;
    }
    let fft_ns = t0.elapsed().as_secs_f64() / reps as f64;

    let enc = Codec::with_kernel(Kernel::SCALAR, k, parity).ok()?;
    let dref: Vec<&[u8]> = data.iter().map(|v| v.as_slice()).collect();
    let mut pbuf: Vec<Vec<u8>> = (0..parity).map(|_| vec![0u8; shard_len]).collect();
    let mut reps = 0usize;
    let t0 = Instant::now();
    while t0.elapsed() < target {
        let mut pref: Vec<&mut [u8]> = pbuf.iter_mut().map(|v| v.as_mut_slice()).collect();
        enc.encode(black_box(&dref), &mut pref).unwrap();
        reps += 1;
    }
    let mat_ns = t0.elapsed().as_secs_f64() / reps as f64;

    Some((fft_ns, mat_ns))
}

fn measure_v(k: usize, parity: usize, f: &Field, variant: Variant) -> Option<(Layout, Ops)> {
    let lay = layout(k, parity)?;
    let data: Vec<Vec<u8>> = (0..k).map(|_| Vec::new()).collect();
    let mut ops = Ops::default();
    fft_encode_v(&data, parity, f, &mut ops, variant)?;
    Some((lay, ops))
}

fn measure(k: usize, parity: usize, f: &Field) -> Option<(Layout, Ops)> {
    measure_v(k, parity, f, Variant::Fine)
}

// ===========================================================================
// Leopard-style decode
// ===========================================================================
//
// The decode is *not* the encode run backwards. It is a full-domain erasure
// decode, and its cost does not depend on how many shards were actually lost.
// The derivation, so the code below can be checked against it:
//
// Domain positions. The encoder puts parity value `i` at domain position `i`
// (i < M) and data value `d` at position `M + d`. That is exactly what the
// layout in `fft_encode_v` does: data index d = c*M + j sits in coset c+1 at
// position (c+1)*M + j = M + d.
//
// The generator, from `analytic_generator`, is
//
//     G[i][d] = omega_{c+1} / omega_{(c+1)M + (i^j)}
//             = a_d / (omega_i + omega_{M+d}),        a_d = omega_{1 + d/M}
//
// because omega is F2-linear in the index. So with x_i = omega_i (parity
// nodes) and y_d = omega_{M+d} (data nodes), the code is Cauchy:
//
//     parity_i = sum_d a_d * data_d / (x_i + y_d).
//
// Put Psi(x) = prod_{d<k} (x + y_d) and
//
//     N(x) = sum_d a_d * data_d * prod_{d' != d} (x + y_{d'}),   deg N < k.
//
// Then N(y_d) = a_d * Psi'(y_d) * data_d and N(x_i) = Psi(x_i) * parity_i. So
// every surviving shard gives one evaluation of a single polynomial of degree
// < k on the size-n domain, after one scalar multiply. Those scalars depend
// only on (k, parity), not on the erasure pattern, so they are precomputed
// into `to_n` / `from_n` and folded into the multiply the decode already does.
//
// Erasure decoding of an evaluation code is then the standard three-transform
// recipe: multiply by the error locator Pi, inverse-transform, formal
// derivative, forward-transform, divide by Pi'. Every one of those steps is
// sized by n = ceilPow2(M + k), never by the number of erasures.
//
// The erasure set contains, besides the shards actually lost: positions
// [parity, M) (parity values the encoder truncated away and never sent) and
// positions [M + k, n) (padding). deg(N*Pi) <= (k-1) + (n - k - parity + e)
// = n - 1 - parity + e, which is < n exactly when e <= parity: the MDS bound
// falls out of the degree budget.

/// Field log/exp tables plus the Walsh transform of the log table.
///
/// `gf` does not export its own, and the log-domain Walsh trick needs them.
/// `log[0] = 0` is a deliberate convention, not a bug: see `error_locator`.
struct LogTab {
    exp: [u8; 255],
    log_walsh: [i32; 256],
}

/// Walsh-Hadamard transform over Z/255, in place, over the whole 256-point
/// index space. Applying it twice scales by 256 == 1 (mod 255), which is why
/// no normalization step appears anywhere below.
fn fwht256(d: &mut [i32; 256]) {
    let mut dist = 1usize;
    while dist < 256 {
        let mut i = 0;
        while i < 256 {
            for j in i..i + dist {
                let a = d[j];
                let b = d[j + dist];
                d[j] = (a + b) % 255;
                d[j + dist] = (a + 255 - b) % 255;
            }
            i += dist * 2;
        }
        dist *= 2;
    }
}

impl LogTab {
    fn new(f: &Field) -> LogTab {
        let mut exp = [0u8; 255];
        let mut log = [0u8; 256];
        let mut x = 1u8;
        for (i, slot) in exp.iter_mut().enumerate() {
            *slot = x;
            log[x as usize] = i as u8;
            x = gf::mul(x, 2);
        }
        assert_eq!(x, 1, "0x02 must generate the multiplicative group");
        log[0] = 0; // convention; makes the locator produce Pi' at erased points

        let mut lw = [0i32; 256];
        for (q, slot) in lw.iter_mut().enumerate() {
            *slot = log[f.omega[q] as usize] as i32;
        }
        fwht256(&mut lw);
        LogTab { exp, log_walsh: lw }
    }
}

/// `exp( sum over e in E of log(omega_q + omega_e) )` for every `q < n`.
///
/// For `q` not in `E` that is `Pi(omega_q)`; for `q` in `E` the `log(0) = 0`
/// convention drops the vanishing factor and leaves `Pi'(omega_q)`, which is
/// exactly the other value the decode needs. Both come out of the same pass.
///
/// Cost: two 256-point Walsh transforms plus 256 modular multiplies. That is
/// scalar work, independent of shard length, and it is counted separately from
/// the shard-sized ops.
fn error_locator(erased: &[bool], n: usize, t: &LogTab) -> Vec<u8> {
    let mut e = [0i32; 256];
    for (q, slot) in e.iter_mut().take(n).enumerate() {
        *slot = i32::from(erased[q]);
    }
    fwht256(&mut e);
    for (i, slot) in e.iter_mut().enumerate() {
        *slot = (*slot * t.log_walsh[i]) % 255;
    }
    fwht256(&mut e);
    (0..n).map(|q| t.exp[e[q] as usize]).collect()
}

/// Everything about a decode that depends only on `(k, parity)`.
struct DecodeConst {
    k: usize,
    parity: usize,
    m: usize,
    n: usize,
    /// Scalar taking a surviving shard at domain position `q` to `N(omega_q)`.
    to_n: Vec<u8>,
    /// Scalar taking `N(omega_{m+d})` back to `data_d`.
    from_n: Vec<u8>,
}

fn decode_const(k: usize, parity: usize, f: &Field) -> Option<DecodeConst> {
    let lay = layout(k, parity)?;
    let m = lay.m;
    let n = ceil_pow2(m + k);
    if n > 256 {
        return None;
    }

    let mut to_n = vec![0u8; m + k];
    // Parity position i: N(omega_i) = Psi(omega_i) * parity_i.
    for i in 0..m {
        let mut acc = 1u8;
        for d in 0..k {
            acc = gf::mul(acc, f.omega[i] ^ f.omega[m + d]);
        }
        to_n[i] = acc;
    }
    // Data position m+d: N(omega_{m+d}) = a_d * Psi'(y_d) * data_d.
    let mut from_n = vec![0u8; k];
    for d in 0..k {
        let mut acc = f.omega[d / m + 1];
        for d2 in 0..k {
            if d2 != d {
                acc = gf::mul(acc, f.omega[m + d] ^ f.omega[m + d2]);
            }
        }
        assert_ne!(acc, 0);
        to_n[m + d] = acc;
        from_n[d] = gf::inv(acc);
    }
    Some(DecodeConst {
        k,
        parity,
        m,
        n,
        to_n,
        from_n,
    })
}

/// Leopard's in-place formal derivative in the novel polynomial basis.
///
/// `X_j' = sum over set bits b of j of X_{j - 2^b}`, so the true derivative is
/// `d_j = sum over clear bits b of c_{j + 2^b}`. This loop computes
/// `d_j + c_j`, i.e. `f' + f`, which is what leopard's `FormalDerivative`
/// computes too. The extra term is harmless *here specifically*: the only
/// places the result is read are erased positions, and `N*Pi` vanishes at
/// every erased position by construction.
///
/// Op count is `(n/2) * log2(n)` XOR passes either way.
fn formal_derivative(w: &mut [Vec<u8>], n: usize, ops: &mut Ops) {
    for i in 1..n {
        let width = ((i ^ (i - 1)) + 1) >> 1;
        for j in (i - width)..i {
            xor_into(w, j, j + width);
            ops.xor += 1;
        }
    }
}

/// Leopard-style decode. Returns all `k` data shards, or `None` if the erasure
/// pattern is beyond the correction limit.
///
/// `parity_in[i]` is parity shard `i`, `data_in[d]` is data shard `d`; `None`
/// means lost.
fn fft_decode(
    dc: &DecodeConst,
    parity_in: &[Option<Vec<u8>>],
    data_in: &[Option<Vec<u8>>],
    len: usize,
    f: &Field,
    t: &LogTab,
    ops: &mut Ops,
) -> Option<Vec<Vec<u8>>> {
    let (k, m, n, parity) = (dc.k, dc.m, dc.n, dc.parity);
    assert_eq!(parity_in.len(), parity);
    assert_eq!(data_in.len(), k);

    let mut erased = vec![true; n];
    for (q, s) in parity_in.iter().enumerate() {
        erased[q] = s.is_none();
    }
    for (d, s) in data_in.iter().enumerate() {
        erased[m + d] = s.is_none();
    }
    // Positions [parity, m) and [m + k, n) stay erased: nothing carries them.
    let lost = parity_in.iter().filter(|s| s.is_none()).count()
        + data_in.iter().filter(|s| s.is_none()).count();
    if lost > parity {
        return None;
    }

    let el = error_locator(&erased, n, t);

    let mut work = vec![vec![0u8; len]; n];
    for q in 0..n {
        let src = if q < parity {
            parity_in[q].as_ref()
        } else if (m..m + k).contains(&q) {
            data_in[q - m].as_ref()
        } else {
            None
        };
        match src {
            Some(s) => {
                let c = gf::mul(el[q], dc.to_n[q]);
                for (o, &b) in work[q].iter_mut().zip(s.iter()) {
                    *o = gf::mul(c, b);
                }
                ops.mul_add += 1;
            }
            None => {
                work[q].iter_mut().for_each(|b| *b = 0);
                ops.mem += 1;
            }
        }
    }

    // leopard's ifftDITDecoder is called with mtrunc = m + dataShards, not n.
    // Positions [m+k, n) are zero here, so the same truncation is legal. The
    // first version of this prototype passed n, which is correct but does more
    // work and overstated the FFT's cost; `check_published_op_counts` pins the
    // difference so it cannot be lost again silently.
    ifft(&mut work, n, 0, m + k, f, ops);
    formal_derivative(&mut work, n, ops);
    // Only positions m..m+k are ever read back, so the tail of the domain can
    // be skipped. Leopard truncates at exactly the same place.
    // leopard prunes this transform to the outputs it actually reads back
    // (errorBitfield). Only lost data positions are read.
    let mut needed = vec![false; n];
    for (d, s) in data_in.iter().enumerate() {
        if s.is_none() {
            needed[m + d] = true;
        }
    }
    fft_pruned(&mut work, n, 0, m + k, &needed, f, ops);

    let mut out = Vec::with_capacity(k);
    for (d, s) in data_in.iter().enumerate() {
        match s {
            Some(s) => out.push(s.clone()),
            None => {
                let c = gf::mul(dc.from_n[d], gf::inv(el[m + d]));
                let mut o = vec![0u8; len];
                for (x, &b) in o.iter_mut().zip(work[m + d].iter()) {
                    *x = gf::mul(c, b);
                }
                ops.mul_add += 1;
                out.push(o);
            }
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Decode checks
// ---------------------------------------------------------------------------

/// The Walsh-trick locator must agree with the definition, computed directly.
fn check_error_locator(f: &Field, t: &LogTab) {
    let mut cases = 0usize;
    for &n in &[4usize, 8, 16, 32, 64, 128, 256] {
        for trial in 0..8u64 {
            let mut erased = vec![false; n];
            let mut count = 0;
            for (q, e) in erased.iter_mut().enumerate() {
                if lcg(trial * 7919 + (n * 31 + q) as u64) & 1 == 1 {
                    *e = true;
                    count += 1;
                }
            }
            if count == 0 {
                continue;
            }
            let fast = error_locator(&erased, n, t);
            for q in 0..n {
                let mut want = 1u8;
                for (x, &e) in erased.iter().enumerate().take(n) {
                    if e && x != q {
                        want = gf::mul(want, f.omega[q] ^ f.omega[x]);
                    }
                }
                assert_eq!(fast[q], want, "error locator wrong at n={n} q={q}");
            }
            cases += 1;
        }
    }
    println!("  error locator (2 Walsh transforms) == direct product: {cases} random cases");
}

/// The Cauchy form the decode is derived from must be the encoder's generator.
fn check_cauchy_form(f: &Field) {
    let splits: &[(usize, usize)] = &[(4, 2), (8, 4), (16, 8), (32, 16), (64, 16), (64, 64)];
    for &(k, parity) in splits {
        let g = analytic_generator(k, parity, f).unwrap();
        let m = layout(k, parity).unwrap().m;
        for (i, row) in g.iter().enumerate() {
            for (d, &cell) in row.iter().enumerate() {
                let a = f.omega[d / m + 1];
                let want = gf::div(a, f.omega[i] ^ f.omega[m + d]);
                assert_eq!(cell, want, "Cauchy form wrong at {k}+{parity} [{i}][{d}]");
            }
        }
    }
    println!(
        "  generator == a_d / (omega_i + omega_(m+d)) at {} splits",
        splits.len()
    );
}

fn shard_bytes(seed: u64, len: usize) -> Vec<u8> {
    (0..len)
        .map(|j| lcg(seed.wrapping_mul(1_000_003).wrapping_add(j as u64)))
        .collect()
}

/// Encode, drop a set of shards, decode, compare. Returns false on mismatch.
fn decode_roundtrip(
    k: usize,
    parity: usize,
    lost: &[usize],
    len: usize,
    f: &Field,
    t: &LogTab,
    dc: &DecodeConst,
) -> Ops {
    let data: Vec<Vec<u8>> = (0..k)
        .map(|i| shard_bytes(0xd0d0 + i as u64, len))
        .collect();
    let mut ops = Ops::default();
    let par = fft_encode(&data, parity, f, &mut ops).expect("layout must fit");

    // `lost` indexes the concatenation data(0..k) then parity(k..k+parity).
    let mut data_in: Vec<Option<Vec<u8>>> = data.iter().cloned().map(Some).collect();
    let mut parity_in: Vec<Option<Vec<u8>>> = par.iter().cloned().map(Some).collect();
    for &i in lost {
        if i < k {
            data_in[i] = None;
        } else {
            parity_in[i - k] = None;
        }
    }

    let mut ops = Ops::default();
    let got = fft_decode(dc, &parity_in, &data_in, len, f, t, &mut ops)
        .unwrap_or_else(|| panic!("{k}+{parity} refused erasures {lost:?}"));
    assert_eq!(got, data, "decode wrong at {k}+{parity}, erasures {lost:?}");
    ops
}

/// Every erasure pattern within the correction limit, exhaustively.
fn check_decode_exhaustive(k: usize, parity: usize, len: usize, f: &Field, t: &LogTab) -> usize {
    let dc = decode_const(k, parity, f).expect("layout must fit");
    let n = k + parity;
    let mut patterns = 0usize;
    for e in 1..=parity {
        let mut idx: Vec<usize> = (0..e).collect();
        loop {
            decode_roundtrip(k, parity, &idx, len, f, t, &dc);
            patterns += 1;
            if !next_combination(&mut idx, n) {
                break;
            }
        }
    }
    patterns
}

fn check_decode_sampled(
    k: usize,
    parity: usize,
    len: usize,
    trials: usize,
    f: &Field,
    t: &LogTab,
) -> usize {
    let dc = decode_const(k, parity, f).expect("layout must fit");
    let n = k + parity;
    let mut seed = 0xbeef_u64 ^ ((k as u64) << 24) ^ ((parity as u64) << 8);
    let mut rnd = move || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) as usize
    };
    let mut done = 0;
    for i in 0..trials {
        // Sweep e deterministically over 1..=parity as well as randomly, so
        // both the realistic end and the correction limit are always covered.
        let e = if i < parity {
            i + 1
        } else {
            1 + rnd() % parity
        };
        let lost = sample_distinct(e, n, &mut rnd);
        decode_roundtrip(k, parity, &lost, len, f, t, &dc);
        done += 1;
    }
    done
}

// ---------------------------------------------------------------------------
// Decode measurement
// ---------------------------------------------------------------------------

/// Shard-sized ops the shipping matrix decode performs, derived from
/// `reconstruct_impl` in `src/codec.rs`.
///
/// With `e` missing data shards, it forms an `e x (e + k)` augmented system,
/// Gauss-Jordans it (scalar field ops only, no shard passes), and then calls
/// `combine` once per missing data shard over `input_indices`, whose length is
/// exactly `data_shards`. `combine` issues one `mul_slice` plus `len - 1`
/// `mul_add_slice`, i.e. `k` shard passes. So `reconstruct_data` is `e * k`
/// shard passes, and `reconstruct` adds `k` more per missing parity shard.
fn matrix_decode_shard_ops(k: usize, missing_data: usize, missing_parity_regen: usize) -> usize {
    (missing_data + missing_parity_regen) * k
}

/// Scalar (non-shard-sized) field ops the Gauss-Jordan in `reconstruct_impl`
/// performs on the `e x (e + k)` system, counted from the loop structure in
/// `matrix::gauss_jordan`: for each of `e` pivot columns, one normalize pass of
/// `cols` and `e - 1` elimination passes of `cols`.
fn matrix_decode_scalar_ops(k: usize, e: usize) -> usize {
    let cols = e + k;
    e * cols + e * (e.saturating_sub(1)) * cols
}

fn fft_decode_ops(dc: &DecodeConst, e_data: usize, e_parity: usize, f: &Field, t: &LogTab) -> Ops {
    let (k, parity) = (dc.k, dc.parity);
    let mut data_in: Vec<Option<Vec<u8>>> = (0..k).map(|_| Some(Vec::new())).collect();
    let mut parity_in: Vec<Option<Vec<u8>>> = (0..parity).map(|_| Some(Vec::new())).collect();
    // Spread the losses so no pattern is accidentally degenerate.
    for i in 0..e_data {
        data_in[(i * 7 + 1) % k] = None;
    }
    for i in 0..e_parity {
        parity_in[(i * 5 + 1) % parity] = None;
    }
    let mut ops = Ops::default();
    fft_decode(dc, &parity_in, &data_in, 0, f, t, &mut ops).expect("within the correction limit");
    ops
}

/// Wall clock of the shipping decode, at the detected kernel.
fn time_matrix_decode(k: usize, parity: usize, e: usize, len: usize) -> f64 {
    use core::hint::black_box;
    use reed_solomon_engine::{Codec, ShardMask};
    use std::time::{Duration, Instant};

    let n = k + parity;
    let mut bufs: Vec<Vec<u8>> = (0..n)
        .map(|i| shard_bytes(0x5150 + i as u64, len))
        .collect();
    for b in bufs.iter_mut().skip(k) {
        b.fill(0);
    }
    let enc = Codec::new(k, parity).unwrap();
    {
        let mut refs: Vec<&mut [u8]> = bufs.iter_mut().map(|v| v.as_mut_slice()).collect();
        enc.encode_all(&mut refs).unwrap();
    }
    let pristine = bufs.clone();

    let mut present = ShardMask::all_present(n);
    for i in 0..e {
        present.remove((i * 7 + 1) % k);
    }
    let dec = Codec::new(k, parity).unwrap();
    let mut workspace = dec.workspace();

    let target = Duration::from_millis(150);
    let mut reps = 0usize;
    let t0 = Instant::now();
    while t0.elapsed() < target {
        for (b, p) in bufs.iter_mut().zip(pristine.iter()) {
            b.copy_from_slice(p);
        }
        dec.reconstruct_data(black_box(&mut bufs), &present, &mut workspace)
            .unwrap();
        reps += 1;
    }
    let total = t0.elapsed().as_secs_f64() / reps as f64;

    // Subtract the restore cost, which is not part of the decode.
    let mut reps2 = 0usize;
    let t1 = Instant::now();
    while t1.elapsed() < Duration::from_millis(60) {
        for (b, p) in bufs.iter_mut().zip(pristine.iter()) {
            b.copy_from_slice(p);
        }
        black_box(&bufs);
        reps2 += 1;
    }
    let restore = t1.elapsed().as_secs_f64() / reps2 as f64;
    (total - restore).max(0.0)
}

/// Wall clock of the error-locator computation alone: two 256-point Walsh
/// transforms plus 256 modular multiplies, with no shard-sized work at all.
fn time_error_locator(t: &LogTab) -> f64 {
    use core::hint::black_box;
    use std::time::{Duration, Instant};
    let mut erased = vec![false; 256];
    for (i, e) in erased.iter_mut().enumerate() {
        *e = i % 5 == 0;
    }
    let mut reps = 0usize;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(150) {
        black_box(error_locator(black_box(&erased), 256, t));
        reps += 1;
    }
    t0.elapsed().as_secs_f64() / reps as f64
}

/// Pins the shard-op counts that `docs/DESIGN.md` section 8b quotes.
///
/// The correctness checks cannot catch a lost optimization. Dropping the
/// truncated IFFT or the `errorBitfield` pruning leaves the decode perfectly
/// correct and merely more expensive, so every round-trip still passes while
/// the numbers the section is built on quietly change: the correction-limit
/// ratio at 8+4 goes from 3.91 back to the 4.44 of the strawman this replaced.
///
/// These counts are exact integers, not timings, so they do not drift. If one
/// of these assertions fires, either an optimization was lost or the model
/// changed on purpose; in the second case update section 8b in the same commit.
fn check_published_op_counts(f: &Field, t: &LogTab) {
    // (k, parity, e, expected fft shard ops, expected matrix shard ops).
    // The four splits section 8b quotes at the correction limit.
    let pinned: [(usize, usize, usize, u64, usize); 4] = [
        (8, 4, 4, 125, 32),
        (16, 8, 8, 313, 128),
        (32, 16, 16, 729, 512),
        (64, 16, 16, 1573, 1024),
    ];
    for &(k, parity, e, want_fft, want_matrix) in &pinned {
        let dc = decode_const(k, parity, f).expect("layout must fit");
        let got_fft = fft_decode_ops(&dc, e, 0, f, t).total();
        let got_matrix = matrix_decode_shard_ops(k, e, 0);
        assert_eq!(
            got_fft, want_fft,
            "{k}+{parity} at e={e}: fft shard ops moved from {want_fft} to {got_fft}. \
             Section 8b quotes a ratio derived from this; see the note on this function."
        );
        assert_eq!(
            got_matrix, want_matrix,
            "{k}+{parity} at e={e}: matrix shard ops moved from {want_matrix} to {got_matrix}. \
             The shipping decode changed shape; re-derive section 8b."
        );
    }
    println!("  published op counts at the correction limit unchanged (4 splits)");
}

fn decode_main(f: &Field) {
    let t = LogTab::new(f);

    println!("Correctness of the decode");
    check_error_locator(f, &t);
    check_cauchy_form(f);
    check_published_op_counts(f, &t);

    for &(k, parity) in &[(4usize, 2usize), (5, 3), (8, 4), (6, 6)] {
        let p = check_decode_exhaustive(k, parity, 13, f, &t);
        println!("  {k}+{parity}: all {p} erasure patterns within the limit decoded correctly");
    }
    for &(k, parity) in &[
        (16usize, 8usize),
        (32, 16),
        (64, 16),
        (64, 64),
        (128, 128),
        (16, 16),
        (100, 20),
    ] {
        let c = check_decode_sampled(k, parity, 37, 200, f, &t);
        println!("  {k}+{parity}: {c} sampled erasure patterns decoded correctly");
    }

    println!();
    println!("Shard-sized op counts, decode (one op = one pass over a shard-length buffer)");
    println!("  matrix = e*k, derived from reconstruct_impl; fft = instrumented");
    println!(
        "{:>10} {:>4} {:>5} {:>4} | {:>7} | {:>7} {:>6} {:>6} {:>7} | {:>7}",
        "split", "M", "n", "e", "matrix", "mul_add", "xor", "mem", "fft", "ratio"
    );
    let grid: &[(usize, usize)] = &[(8, 4), (16, 8), (32, 16), (64, 16), (64, 64), (128, 128)];
    for &(k, parity) in grid {
        let Some(dc) = decode_const(k, parity, f) else {
            println!(
                "{:>10}  layout does not fit in GF(2^8)",
                format!("{k}+{parity}")
            );
            continue;
        };
        let mut es: Vec<usize> = vec![1, 2, 3, parity / 4, parity / 2, parity];
        es.retain(|&e| e >= 1 && e <= parity && e <= k);
        es.sort_unstable();
        es.dedup();
        for e in es {
            let ops = fft_decode_ops(&dc, e, 0, f, &t);
            let matrix = matrix_decode_shard_ops(k, e, 0);
            println!(
                "{:>10} {:>4} {:>5} {:>4} | {:>7} | {:>7} {:>6} {:>6} {:>7} | {:>7.2}",
                format!("{k}+{parity}"),
                dc.m,
                dc.n,
                e,
                matrix,
                ops.mul_add,
                ops.xor,
                ops.mem,
                ops.total(),
                ops.total() as f64 / matrix as f64
            );
        }
    }

    println!();
    println!("Crossover in e: smallest erasure count at which the FFT decode is cheaper");
    println!("  (unweighted shard ops; 'never' = matrix cheaper up to the correction limit)");
    println!(
        "{:>10} {:>6} {:>6} {:>10} {:>12} {:>14}",
        "split", "M", "n", "fft ops", "e*", "e* / m"
    );
    for &(k, parity) in grid {
        let Some(dc) = decode_const(k, parity, f) else {
            continue;
        };
        let ops = fft_decode_ops(&dc, 1, 0, f, &t).total();
        let mut cross = None;
        for e in 1..=parity.min(k) {
            let o = fft_decode_ops(&dc, e, 0, f, &t).total();
            if (o as f64) < matrix_decode_shard_ops(k, e, 0) as f64 {
                cross = Some(e);
                break;
            }
        }
        println!(
            "{:>10} {:>6} {:>6} {:>10} {:>12} {:>14}",
            format!("{k}+{parity}"),
            dc.m,
            dc.n,
            ops,
            match cross {
                Some(e) => e.to_string(),
                None => "never".to_string(),
            },
            match cross {
                Some(e) => format!("{:.2}", e as f64 / parity as f64),
                None => "-".to_string(),
            }
        );
    }

    println!();
    println!("Crossover across the whole expressible range (unweighted shard ops)");
    println!(
        "{:>10} {:>8} {:>10} {:>10}",
        "split", "e*", "e*/m", "verdict"
    );
    let wide: &[(usize, usize)] = &[
        (4, 2),
        (8, 4),
        (16, 4),
        (16, 8),
        (16, 16),
        (32, 8),
        (32, 16),
        (32, 32),
        (64, 8),
        (64, 16),
        (64, 32),
        (64, 64),
        (100, 20),
        (128, 32),
        (128, 64),
        (128, 128),
        (192, 55),
        (200, 32),
    ];
    for &(k, parity) in wide {
        let Some(dc) = decode_const(k, parity, f) else {
            println!(
                "{:>10} {:>8} {:>10} {:>10}",
                format!("{k}+{parity}"),
                "-",
                "-",
                "no layout"
            );
            continue;
        };
        let mut cross = None;
        for e in 1..=parity.min(k) {
            let o = fft_decode_ops(&dc, e, 0, f, &t).total();
            if (o as f64) < matrix_decode_shard_ops(k, e, 0) as f64 {
                cross = Some(e);
                break;
            }
        }
        println!(
            "{:>10} {:>8} {:>10} {:>10}",
            format!("{k}+{parity}"),
            match cross {
                Some(e) => e.to_string(),
                None => "never".to_string(),
            },
            match cross {
                Some(e) => format!("{:.2}", e as f64 / parity as f64),
                None => "-".to_string(),
            },
            if cross.is_some() {
                "fft can win"
            } else {
                "matrix wins to the limit"
            }
        );
    }

    println!();
    println!("How much is an op worth on this machine? (live kernel)");
    let weights: Vec<(usize, (f64, f64, f64))> = [64usize, 1200, 16384]
        .iter()
        .map(|&l| (l, measure_op_weights(l)))
        .collect();

    let loc = time_error_locator(&t);
    println!();
    println!("Non-shard-sized costs, measured");
    println!(
        "  error locator (2x 256-pt Walsh + 256 mults): {:.3} us",
        loc * 1e6
    );
    for (l, (wa, _, _)) in &weights {
        println!(
            "    = {:.1} shard mul-add passes at {l}-byte shards",
            loc / wa
        );
    }

    println!();
    println!("Ratio with ops weighted by their measured cost (>1 means FFT decode is WORSE)");
    print!("{:>10} {:>4} {:>10}", "split", "e", "unweighted");
    for (l, _) in &weights {
        print!("{:>14}", format!("{l}B shards"));
    }
    println!();
    for &(k, parity) in grid {
        let Some(dc) = decode_const(k, parity, f) else {
            continue;
        };
        let mut es: Vec<usize> = vec![1, 2, 3, parity / 4, parity / 2, parity];
        es.retain(|&e| e >= 1 && e <= parity && e <= k);
        es.sort_unstable();
        es.dedup();
        for e in es {
            let ops = fft_decode_ops(&dc, e, 0, f, &t);
            let matrix = matrix_decode_shard_ops(k, e, 0) as f64;
            print!(
                "{:>10} {:>4} {:>10.2}",
                format!("{k}+{parity}"),
                e,
                ops.total() as f64 / matrix
            );
            for (_, (wa, wx, wm)) in &weights {
                let mat = matrix * wa;
                let fft = ops.mul_add as f64 * wa + ops.xor as f64 * wx + ops.mem as f64 * wm;
                print!("{:>14.2}", (fft + loc) / mat);
            }
            println!();
        }
    }

    println!();
    println!("Shipping decode, wall clock, detected kernel, 1200-byte shards");
    println!("  reconstruct_data only (missing parity is not regenerated)");
    println!(
        "{:>10} {:>4} {:>12} {:>14} {:>12} {:>12}",
        "split", "e", "measured us", "e*k*w us", "overhead us", "GJ scalar"
    );
    let w1200 = weights
        .iter()
        .find(|(l, _)| *l == 1200)
        .map(|(_, w)| w.0)
        .unwrap();
    for &(k, parity) in grid {
        let mut es: Vec<usize> = vec![1, 2, 3, parity / 4, parity / 2, parity];
        es.retain(|&e| e >= 1 && e <= parity && e <= k);
        es.sort_unstable();
        es.dedup();
        for e in es {
            let measured = time_matrix_decode(k, parity, e, 1200);
            let predicted = matrix_decode_shard_ops(k, e, 0) as f64 * w1200;
            println!(
                "{:>10} {:>4} {:>12.2} {:>14.2} {:>12.2} {:>12}",
                format!("{k}+{parity}"),
                e,
                measured * 1e6,
                predicted * 1e6,
                (measured - predicted) * 1e6,
                matrix_decode_scalar_ops(k, e)
            );
        }
    }
}

/// End-to-end decode wall clock, scalar against scalar, as a reality check on
/// the op weighting. Both sides multiply a byte at a time through `gf::mul`.
/// It is *not* a prediction of the SIMD world; it only tests whether the op
/// counts predict something real.
fn time_scalar_decode(
    k: usize,
    parity: usize,
    e: usize,
    len: usize,
    f: &Field,
    t: &LogTab,
) -> Option<(f64, f64)> {
    use core::hint::black_box;
    use reed_solomon_engine::{Codec, Kernel, ShardMask};
    use std::time::{Duration, Instant};

    let dc = decode_const(k, parity, f)?;
    let n = k + parity;
    let data: Vec<Vec<u8>> = (0..k)
        .map(|i| shard_bytes(0x2020 + i as u64, len))
        .collect();
    let mut ops = Ops::default();
    let par = fft_encode(&data, parity, f, &mut ops)?;

    let missing: Vec<usize> = (0..e).map(|i| (i * 7 + 1) % k).collect();
    let mut data_in: Vec<Option<Vec<u8>>> = data.iter().cloned().map(Some).collect();
    let parity_in: Vec<Option<Vec<u8>>> = par.iter().cloned().map(Some).collect();
    for &i in &missing {
        data_in[i] = None;
    }

    let target = Duration::from_millis(150);
    let mut reps = 0usize;
    let t0 = Instant::now();
    while t0.elapsed() < target {
        let mut ops = Ops::default();
        black_box(fft_decode(&dc, &parity_in, &data_in, len, f, t, &mut ops));
        reps += 1;
    }
    let fft_s = t0.elapsed().as_secs_f64() / reps as f64;

    // Matrix side, scalar kernel.
    let mut bufs: Vec<Vec<u8>> = (0..n)
        .map(|i| shard_bytes(0x5150 + i as u64, len))
        .collect();
    for b in bufs.iter_mut().skip(k) {
        b.fill(0);
    }
    let enc = Codec::with_kernel(Kernel::SCALAR, k, parity).ok()?;
    {
        let mut refs: Vec<&mut [u8]> = bufs.iter_mut().map(|v| v.as_mut_slice()).collect();
        enc.encode_all(&mut refs).unwrap();
    }
    let pristine = bufs.clone();
    let mut present = ShardMask::all_present(n);
    for &i in &missing {
        present.remove(i);
    }
    let dec = Codec::with_kernel(Kernel::SCALAR, k, parity).ok()?;
    let mut workspace = dec.workspace();
    let mut reps = 0usize;
    let t1 = Instant::now();
    while t1.elapsed() < target {
        for (b, p) in bufs.iter_mut().zip(pristine.iter()) {
            b.copy_from_slice(p);
        }
        dec.reconstruct_data(black_box(&mut bufs), &present, &mut workspace)
            .unwrap();
        reps += 1;
    }
    let mat_total = t1.elapsed().as_secs_f64() / reps as f64;
    let mut reps2 = 0usize;
    let t2 = Instant::now();
    while t2.elapsed() < Duration::from_millis(60) {
        for (b, p) in bufs.iter_mut().zip(pristine.iter()) {
            b.copy_from_slice(p);
        }
        black_box(&bufs);
        reps2 += 1;
    }
    let restore = t2.elapsed().as_secs_f64() / reps2 as f64;
    Some((fft_s, (mat_total - restore).max(0.0)))
}

/// The comparison that actually decides the question: measured wall clock of
/// the shipping decode against a projection of the FFT decode built from its
/// instrumented op counts and this machine's measured per-op costs.
fn decode_head_to_head(f: &Field, t: &LogTab, weights: &[(usize, (f64, f64, f64))], loc: f64) {
    let grid: &[(usize, usize)] = &[(8, 4), (16, 8), (32, 16), (64, 16), (64, 64), (128, 128)];
    for &(shard_len, (wa, wx, wm)) in weights.iter().filter(|(l, _)| *l == 1200 || *l == 16384) {
        println!();
        println!(
            "Head to head at {shard_len}-byte shards: matrix MEASURED vs FFT PROJECTED from op counts"
        );
        println!("  matrix us includes its Gauss-Jordan; fft us includes the error locator");
        println!(
            "{:>10} {:>4} {:>13} {:>14} {:>10} {:>10}",
            "split", "e", "matrix us", "fft us (proj)", "fft/matrix", "winner"
        );
        for &(k, parity) in grid {
            let Some(dc) = decode_const(k, parity, f) else {
                continue;
            };
            let mut es: Vec<usize> = vec![1, 2, 3, parity / 4, parity / 2, parity];
            es.retain(|&e| e >= 1 && e <= parity && e <= k);
            es.sort_unstable();
            es.dedup();
            for e in es {
                let ops = fft_decode_ops(&dc, e, 0, f, t);
                let fft = ops.mul_add as f64 * wa + ops.xor as f64 * wx + ops.mem as f64 * wm + loc;
                let mat = time_matrix_decode(k, parity, e, shard_len);
                println!(
                    "{:>10} {:>4} {:>13.2} {:>14.2} {:>10.2} {:>10}",
                    format!("{k}+{parity}"),
                    e,
                    mat * 1e6,
                    fft * 1e6,
                    fft / mat,
                    if fft < mat { "fft" } else { "matrix" }
                );
            }
        }
        // Crossover in e under this weighting, against the measured matrix side.
        println!("  crossover in e under this weighting:");
        for &(k, parity) in grid {
            let Some(dc) = decode_const(k, parity, f) else {
                continue;
            };
            let mut cross = None;
            for e in 1..=parity.min(k) {
                let ops = fft_decode_ops(&dc, e, 0, f, t);
                let fft = ops.mul_add as f64 * wa + ops.xor as f64 * wx + ops.mem as f64 * wm + loc;
                if fft < time_matrix_decode(k, parity, e, shard_len) {
                    cross = Some(e);
                    break;
                }
            }
            match cross {
                Some(e) => println!(
                    "    {k}+{parity}: fft wins from e = {e} ({:.0}% of the correction limit)",
                    100.0 * e as f64 / parity as f64
                ),
                None => println!("    {k}+{parity}: matrix wins at every e up to the limit"),
            }
        }
    }
}

fn main() {
    let f = Field::new();
    if std::env::args().nth(1).as_deref() == Some("decode") {
        decode_main(&f);
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("decode2") {
        let t = LogTab::new(&f);
        println!("How much is an op worth on this machine? (live kernel)");
        let weights: Vec<(usize, (f64, f64, f64))> = [1200usize, 16384]
            .iter()
            .map(|&l| (l, measure_op_weights(l)))
            .collect();
        let loc = time_error_locator(&t);
        println!("  error locator: {:.3} us", loc * 1e6);
        decode_head_to_head(&f, &t, &weights, loc);

        println!();
        println!("Reality check: end-to-end wall clock, scalar prototype vs SCALAR matrix engine");
        println!("  1200-byte shards; both sides multiply a byte at a time through gf::mul");
        println!(
            "{:>10} {:>4} {:>12} {:>12} {:>10} {:>14}",
            "split", "e", "fft us", "matrix us", "fft/matrix", "predicted"
        );
        for &(k, parity) in &[(16usize, 8usize), (32, 16), (64, 16), (64, 64), (128, 128)] {
            let Some(dc) = decode_const(k, parity, &f) else {
                continue;
            };
            for e in [1usize, parity / 2, parity] {
                if e == 0 || e > k {
                    continue;
                }
                if let Some((fft_s, mat_s)) = time_scalar_decode(k, parity, e, 1200, &f, &t) {
                    let ops = fft_decode_ops(&dc, e, 0, &f, &t);
                    println!(
                        "{:>10} {:>4} {:>12.2} {:>12.2} {:>10.2} {:>14.2}",
                        format!("{k}+{parity}"),
                        e,
                        fft_s * 1e6,
                        mat_s * 1e6,
                        fft_s / mat_s,
                        ops.total() as f64 / matrix_decode_shard_ops(k, e, 0) as f64
                    );
                }
            }
        }
        return;
    }

    println!("Cantor basis v_0..v_7 = {:02x?}", f.v);
    println!();

    println!("Correctness");
    check_fft_against_direct(&f);
    check_leopard_variant_agrees(&f);
    check_generator_matches_closed_form(&f);

    let mut total = 0usize;
    for &(k, m) in &[
        (1usize, 1usize),
        (2, 2),
        (4, 2),
        (4, 4),
        (6, 3),
        (8, 4),
        (8, 8),
        (12, 6),
    ] {
        let p = check_mds_by_solving(k, m, &f);
        total += p;
        println!("  MDS {k}+{m}: all {p} erasure patterns of size {m} solved and verified");
    }
    let _ = total;
    for &(k, m) in &[(16usize, 8usize), (12, 12), (16, 4)] {
        let c = check_mds_by_submatrices(k, m, &f);
        println!("  MDS {k}+{m}: all {c} square submatrices of the parity matrix non-singular");
    }
    for &(k, m) in &[(32usize, 32usize), (64, 64), (128, 128), (192, 55)] {
        let c = check_mds_sampled(k, m, &f, 20_000);
        println!("  MDS {k}+{m}: {c} random square submatrices, all non-singular (sampled)");
    }
    check_wire_compatibility(&f);
    check_large_roundtrip(&f);
    println!();

    println!("Operation counts (one op = one pass over a shard-length buffer)");
    println!("  'fine' truncates one layer at a time; 'leo' is leopard's 2-layers-at-a-time form");
    println!(
        "{:>9} {:>4} {:>4} {:>9} | {:>7} {:>6} {:>6} {:>7} {:>6} | {:>7} {:>6}",
        "split", "n", "blk", "matrix", "mul_add", "xor", "mem", "fine", "ratio", "leo", "ratio"
    );
    let splits: &[(usize, usize)] = &[
        (4, 2),
        (8, 4),
        (8, 8),
        (12, 6),
        (16, 4),
        (16, 8),
        (16, 16),
        (32, 16),
        (32, 32),
        (64, 16),
        (64, 32),
        (64, 64),
        (100, 20),
        (128, 128),
        (192, 55),
        (200, 32),
        (200, 55),
        (250, 5),
    ];
    for &(k, m) in splits {
        let matrix = k * m;
        match (measure(k, m, &f), measure_v(k, m, &f, Variant::Leopard)) {
            (Some((lay, ops)), Some((_, leo))) => println!(
                "{:>9} {:>4} {:>4} {:>9} | {:>7} {:>6} {:>6} {:>7} {:>6.2} | {:>7} {:>6.2}",
                format!("{k}+{m}"),
                lay.m,
                lay.blocks,
                matrix,
                ops.mul_add,
                ops.xor,
                ops.mem,
                ops.total(),
                matrix as f64 / ops.total() as f64,
                leo.total(),
                matrix as f64 / leo.total() as f64
            ),
            _ => println!(
                "{:>9} {:>4} {:>4} {:>9} | {:>7} {:>6} {:>6} {:>7} {:>6} | {:>7} {:>6}",
                format!("{k}+{m}"),
                ceil_pow2(m),
                k.div_ceil(ceil_pow2(m)),
                matrix,
                "-",
                "-",
                "-",
                "-",
                "N/A",
                "-",
                "N/A"
            ),
        }
    }

    println!();
    println!("Crossover sweep (ratio = matrix ops / fft ops, fine variant; >1 means FFT cheaper)");
    for total_shards in [6usize, 8, 12, 16, 20, 24, 32, 48, 64] {
        let mut line = format!("{total_shards:>3} total: ");
        for m in 1..total_shards {
            let k = total_shards - m;
            if let Some((_, ops)) = measure(k, m, &f) {
                let r = (k * m) as f64 / ops.total() as f64;
                line.push_str(&format!("{k}+{m}={r:.2} "));
            }
        }
        println!("{line}");
    }

    println!();
    println!("Ratio as a function of parity count alone (fine / leopard), k as shown");
    println!(
        "{:>7} | {:>15} {:>15} {:>15} {:>15}",
        "parity", "k=16", "k=32", "k=64", "k=128"
    );
    for m in [
        1usize, 2, 3, 4, 5, 6, 7, 8, 9, 12, 16, 17, 24, 32, 33, 48, 64,
    ] {
        let mut line = format!("{m:>7} |");
        for k in [16usize, 32, 64, 128] {
            match (measure(k, m, &f), measure_v(k, m, &f, Variant::Leopard)) {
                (Some((_, a)), Some((_, b))) => line.push_str(&format!(
                    "{:>15}",
                    format!(
                        "{:.2}/{:.2}",
                        (k * m) as f64 / a.total() as f64,
                        (k * m) as f64 / b.total() as f64
                    )
                )),
                _ => line.push_str(&format!("{:>15}", "N/A")),
            }
        }
        println!("{line}");
    }

    println!();
    println!("Exact crossover: smallest k at which the FFT is cheaper, per parity count");
    println!("  (fine variant; 'never' = matrix wins at every expressible k)");
    println!(
        "{:>7} {:>12} {:>28}",
        "parity", "min k", "ratio at that k / at max k"
    );
    for m in 1..=64usize {
        let mut first = None;
        let mut last = None;
        for k in 1..=255usize {
            if let Some((_, ops)) = measure(k, m, &f) {
                let r = (k * m) as f64 / ops.total() as f64;
                if r > 1.0 {
                    if first.is_none() {
                        first = Some((k, r));
                    }
                    last = Some((k, r));
                } else if first.is_some() {
                    // Ratio is monotone in k in practice; note it if not.
                    first = first.filter(|_| true);
                }
            }
        }
        match (first, last) {
            (Some((k0, r0)), Some((k1, r1))) => {
                println!(
                    "{m:>7} {k0:>12} {:>28}",
                    format!("{r0:.2} (k={k0}) / {r1:.2} (k={k1})")
                )
            }
            _ => println!("{m:>7} {:>12} {:>28}", "never", "-"),
        }
    }

    println!();
    println!("The losing side, in full: best ratio the FFT ever reaches for small parity");
    println!(
        "{:>7} {:>4} {:>10} {:>12} {:>26}",
        "parity", "M", "best k", "best ratio", "asymptote parity/(log2 M + 2)"
    );
    for m in 1..=8usize {
        let mm = ceil_pow2(m);
        let mut best = (0usize, 0.0f64);
        let mut monotone = true;
        let mut prev = 0.0f64;
        for k in 1..=255usize {
            if let Some((_, ops)) = measure(k, m, &f) {
                let r = (k * m) as f64 / ops.total() as f64;
                if r > best.1 {
                    best = (k, r);
                }
                if r + 1e-12 < prev {
                    monotone = false;
                }
                prev = r;
            }
        }
        println!(
            "{:>7} {:>4} {:>10} {:>12.4} {:>26.4}{}",
            m,
            mm,
            best.0,
            best.1,
            m as f64 / (mm.trailing_zeros() as f64 + 2.0),
            if monotone {
                ""
            } else {
                "  (non-monotone in k)"
            }
        );
    }

    println!();
    println!("Where the FFT engine cannot express the split at all");
    println!("  requirement: ceil(k/M)*M + M <= 256 with M = ceilPow2(parity)");
    println!("{:>8} {:>6} {:>10}", "parity", "M", "max k");
    for e in 0..8u32 {
        let m_lo = if e == 0 { 1 } else { (1 << e) / 2 + 1 };
        let cap = 1usize << e;
        let max_k = (0..=256usize)
            .rev()
            .find(|&k| layout(k, m_lo).is_some())
            .unwrap_or(0);
        println!(
            "{:>8} {:>6} {:>10}",
            if cap == 1 {
                "1".to_string()
            } else {
                format!("{m_lo}..{cap}")
            },
            cap,
            max_k
        );
    }
    let mut feasible = 0usize;
    let mut infeasible = 0usize;
    let mut fft_wins = 0usize;
    for k in 1..256usize {
        for m in 1..=(256 - k) {
            match measure(k, m, &f) {
                None => infeasible += 1,
                Some((_, ops)) => {
                    feasible += 1;
                    if (k * m) as f64 / ops.total() as f64 > 1.0 {
                        fft_wins += 1;
                    }
                }
            }
        }
    }
    let all = feasible + infeasible;
    println!(
        "  of {all} splits with k+m <= 256: {feasible} expressible ({:.0}%), {infeasible} not",
        100.0 * feasible as f64 / all as f64
    );
    println!(
        "  of the {feasible} expressible: FFT cheaper in {fft_wins} ({:.0}%), matrix cheaper in {}",
        100.0 * fft_wins as f64 / feasible as f64,
        feasible - fft_wins
    );

    println!();
    println!("How much is an op worth? (measured on this machine, live kernel)");
    let weights: Vec<(usize, (f64, f64, f64))> = [64usize, 1200, 16384]
        .iter()
        .map(|&l| (l, measure_op_weights(l)))
        .collect();

    println!();
    println!("Ratio with ops weighted by their measured cost");
    print!("{:>9} {:>10}", "split", "unweighted");
    for (l, _) in &weights {
        print!("{:>14}", format!("{l}B shards"));
    }
    println!();
    for &(k, m) in splits {
        let Some((_, ops)) = measure(k, m, &f) else {
            continue;
        };
        print!(
            "{:>9} {:>10.2}",
            format!("{k}+{m}"),
            (k * m) as f64 / ops.total() as f64
        );
        for (_, (wa, wx, wm)) in &weights {
            // The matrix engine is k*m mul-add-class passes and nothing else.
            let matrix = (k * m) as f64 * wa;
            let fft = ops.mul_add as f64 * wa + ops.xor as f64 * wx + ops.mem as f64 * wm;
            print!("{:>14.2}", matrix / fft);
        }
        println!();
    }

    println!();
    println!("Crossover in parity count, under each weighting (smallest parity that wins at k=64)");
    for (l, (wa, wx, wm)) in &weights {
        let mut first = None;
        for m in 1..=64usize {
            if let Some((_, ops)) = measure(64, m, &f) {
                let matrix = (64 * m) as f64 * wa;
                let fft = ops.mul_add as f64 * wa + ops.xor as f64 * wx + ops.mem as f64 * wm;
                if matrix / fft > 1.0 {
                    first = Some(m);
                    break;
                }
            }
        }
        match first {
            Some(m) => println!("  {l:>6}-byte shards: FFT wins from parity {m} up"),
            None => println!("  {l:>6}-byte shards: matrix wins at every parity"),
        }
    }
    let mut first = None;
    for m in 1..=64usize {
        if let Some((_, ops)) = measure(64, m, &f) {
            if (64 * m) as f64 / ops.total() as f64 > 1.0 {
                first = Some(m);
                break;
            }
        }
    }
    println!(
        "      unweighted: FFT wins from parity {} up",
        first.unwrap()
    );

    println!();
    println!("End-to-end wall clock, scalar prototype vs scalar matrix engine, 1200-byte shards");
    println!(
        "  (the prototype allocates its work buffers per call, which the matrix engine does not)"
    );
    println!(
        "{:>9} {:>12} {:>12} {:>10} {:>12}",
        "split", "fft us", "matrix us", "speedup", "op ratio"
    );
    for &(k, m) in splits {
        if let Some((fft_s, mat_s)) = time_scalar_encode(k, m, 1200, &f) {
            let (_, ops) = measure(k, m, &f).unwrap();
            println!(
                "{:>9} {:>12.2} {:>12.2} {:>10.2} {:>12.2}",
                format!("{k}+{m}"),
                fft_s * 1e6,
                mat_s * 1e6,
                mat_s / fft_s,
                (k * m) as f64 / ops.total() as f64
            );
        }
    }

    println!();
    println!("Same table counting only mul-adds on the FFT side (memory ops free)");
    for &(k, m) in splits {
        if let Some((_, ops)) = measure(k, m, &f) {
            let denom = ops.mul_add.max(1);
            println!(
                "  {:>9}: matrix {:>6} mul-add   fft {:>5} mul-add + {:>5} xor + {:>4} mem   ratio(mul-add only) {:>6.1}",
                format!("{k}+{m}"),
                k * m,
                ops.mul_add,
                ops.xor,
                ops.mem,
                (k * m) as f64 / denom as f64
            );
        }
    }
}
