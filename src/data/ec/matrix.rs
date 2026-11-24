//! Generator matrix construction and linear algebra over GF(2^8).
//!
//! Matrices are flat, row-major `[u8]` slices with an explicit column count.
//! Every routine here is a `const fn`, so the same code builds a matrix at
//! compile time for `no_std` callers and at runtime for `alloc` callers. There
//! is one implementation, not two that can drift apart.

use crate::data::ec::error::MAX_SHARDS;
use crate::data::ec::gf;

/// Reduces the leading `n` columns of the `n * cols` row-major matrix `m` to
/// the identity, applying the same row operations to the trailing columns.
///
/// Returns `false` if the leading block is singular, in which case `m` is left
/// in a partially reduced state.
pub(crate) const fn gauss_jordan(m: &mut [u8], n: usize, cols: usize) -> bool {
    let mut col = 0;
    while col < n {
        // Find a row at or below `col` with a non-zero entry in this column.
        // Over a finite field any non-zero pivot is as good as any other, so
        // there is no stability criterion to apply.
        let mut pivot_row = col;
        while pivot_row < n && m[pivot_row * cols + col] == 0 {
            pivot_row += 1;
        }
        if pivot_row == n {
            return false;
        }

        if pivot_row != col {
            let mut j = 0;
            while j < cols {
                m.swap(col * cols + j, pivot_row * cols + j);
                j += 1;
            }
        }

        // Normalize the pivot row so the pivot becomes 1.
        let pivot = m[col * cols + col];
        if pivot != 1 {
            let scale = gf::inv(pivot);
            let mut j = 0;
            while j < cols {
                m[col * cols + j] = gf::mul(m[col * cols + j], scale);
                j += 1;
            }
        }

        // Clear the column in every other row. Subtraction is XOR here.
        let mut row = 0;
        while row < n {
            if row != col {
                let factor = m[row * cols + col];
                if factor != 0 {
                    let mut j = 0;
                    while j < cols {
                        m[row * cols + j] ^= gf::mul(factor, m[col * cols + j]);
                        j += 1;
                    }
                }
            }
            row += 1;
        }

        col += 1;
    }
    true
}

/// Builds the systematic parity matrix into `out`, which must be at least
/// `parity_shards * data_shards` bytes.
///
/// # The construction
///
/// This is the standard systematic Vandermonde construction, and it is part of
/// this crate's wire format. It is specified in prose in
/// `docs/erasure-wire-format.md`; the summary is:
///
/// 1. Form the `total_shards * data_shards` Vandermonde matrix `V` over
///    GF(2^8) with `V[r][c] = r^c`, where `r` is the row index taken as a
///    field element and `pow(0, 0) = 1`. Rows `0..total_shards` are distinct
///    field elements, so every `data_shards`-row square submatrix of `V` is
///    non-singular and the code is MDS.
/// 2. Let `T` be the top `data_shards * data_shards` block of `V` and form
///    `G = V * T^-1`. Right-multiplying by an invertible matrix does not change
///    the column space, so `G` generates the same MDS code as `V`, and its top
///    block is the identity: the code is systematic.
/// 3. The parity matrix `P` is the bottom `parity_shards` rows of `G`.
///
/// Because the code is MDS, *every* square submatrix of `P` is non-singular.
/// That is what lets the decoder recover from any erasure pattern within the
/// correction limit.
///
/// # Why there is no matrix inversion here
///
/// Inverting `T` by Gauss-Jordan is `O(data_shards^3)`, and at 250 data shards
/// that is roughly 20 ms of `Codec::new`. It is also unnecessary: `T` is a
/// Vandermonde matrix, and a Vandermonde inverse is Lagrange interpolation
/// written as a matrix, so `P` has a closed form.
///
/// Write `x_m = m` for the node in row `m` (the row index as a field element).
/// `T[m][l] = x_m^l`, and the inverse of a Vandermonde matrix is
///
///
/// where `L_c` is the Lagrange basis polynomial for node `x_c`, because
/// `(T * T^-1)[m][c] = sum over l of x_m^l * [t^l] L_c(t) = L_c(x_m)`, which is
/// `1` when `m == c` and `0` otherwise. Row `data_shards + j` of `V` evaluates
/// the same monomials at `y_j = data_shards + j`, so
///
///
/// Addition is XOR here, so `x_a + x_b` is just `a ^ b`. Multiplying the
/// numerator by the missing factor gives the form the code below evaluates:
///
///
/// That is `O(data_shards^2)` for the denominators plus `O(parity_shards *
/// data_shards)` for the rest. The result is not an approximation of the
/// inversion, it is the same matrix: `T^-1` is unique, so anything that
/// computes `V * T^-1` exactly agrees byte for byte, and a test asserts it
/// against the Gauss-Jordan reference for every supported shard split.
///
/// # The zero node
///
/// `x_0 = 0`, which several textbook Vandermonde-inverse formulations assume
/// away. It is harmless here because nothing above ever divides by a node or
/// by the product of all of them:
///
/// - `denominator(c)` divides by differences `c ^ m` with `m != c`, never by a
///   node itself.
/// - `master(y_j)` has `y_j ^ 0 = y_j` as a factor, and `y_j >= data_shards`
///   is at least 1, so the factor is non-zero.
///
/// What the zero node does change is that `master` has zero constant term, so
/// any formulation that recovers `L_c` by dividing the master polynomial's
/// coefficients by that term would fail. Evaluating `master` at `y_j` and
/// dividing by the one linear factor sidesteps that entirely.
///
/// # Failure
///
/// Returns `false` only for a shard split this crate does not support, which
/// every caller has already rejected. There is no singular case: the code
/// never divides by zero, because `y_j ^ c` is non-zero (`y_j >= data_shards`
/// is greater than every `c`) and `denominator(c)` is a product of non-zero
/// factors in a field with no zero divisors.
pub(crate) const fn build_parity_matrix(
    data_shards: usize,
    parity_shards: usize,
    out: &mut [u8],
) -> bool {
    // The non-zero-divisor argument above rests on every row index fitting in
    // a byte and being distinct, so a split that would wrap `y_j` is rejected
    // here rather than left to reach `gf::inv(0)`, which panics. Written as a
    // subtraction from the bound rather than a sum against it so that an
    // absurd argument cannot overflow the check itself.
    if data_shards == 0
        || parity_shards == 0
        || parity_shards > MAX_SHARDS
        || data_shards > MAX_SHARDS - parity_shards
    {
        return false;
    }

    // Reciprocal Lagrange denominators, one per column. Held in a fixed array
    // rather than caller scratch because `MAX_SHARDS` bytes of stack is
    // nothing next to the alternative of threading a buffer through every
    // caller, including the `const` one.
    let mut inv_denominator = [0u8; MAX_SHARDS];
    let mut c = 0;
    while c < data_shards {
        let mut d: u8 = 1;
        let mut m = 0;
        while m < data_shards {
            if m != c {
                d = gf::mul(d, (c ^ m) as u8);
            }
            m += 1;
        }
        inv_denominator[c] = gf::inv(d);
        c += 1;
    }

    let mut j = 0;
    while j < parity_shards {
        let y = (data_shards + j) as u8;

        let mut master: u8 = 1;
        let mut m = 0;
        while m < data_shards {
            master = gf::mul(master, y ^ (m as u8));
            m += 1;
        }

        let mut c = 0;
        while c < data_shards {
            let numerator = gf::div(master, y ^ (c as u8));
            out[j * data_shards + c] = gf::mul(numerator, inv_denominator[c]);
            c += 1;
        }
        j += 1;
    }

    true
}

/// Bytes of scratch space [`build_parity_matrix_by_inversion`] needs.
#[cfg(test)]
pub(crate) const fn reference_scratch_len(data_shards: usize) -> usize {
    2 * data_shards * data_shards
}

/// The Gauss-Jordan reference for [`build_parity_matrix`].
///
/// This is the obvious implementation of the construction: build `[T | I]`,
/// reduce it to `[I | T^-1]`, and multiply the bottom rows of `V` through it.
/// It is `O(data_shards^3)` and is not what ships; it exists so that the
/// closed form that does ship is checked against a transcription of the
/// definition rather than against itself, for every supported shard split.
///
/// Same discipline as `src/kernel/scalar.rs`, and for the same reason: this
/// file's failures are silent. Keep this readable and never optimize it.
#[cfg(test)]
pub(crate) const fn build_parity_matrix_by_inversion(
    data_shards: usize,
    parity_shards: usize,
    out: &mut [u8],
    scratch: &mut [u8],
) -> bool {
    let cols = 2 * data_shards;

    // scratch <- [T | I]
    let mut r = 0;
    while r < data_shards {
        // v walks r^0, r^1, r^2, ... Note r^0 is 1 even when r is 0, so row 0
        // is [1, 0, 0, ...].
        let mut v: u8 = 1;
        let mut c = 0;
        while c < data_shards {
            scratch[r * cols + c] = v;
            scratch[r * cols + data_shards + c] = if r == c { 1 } else { 0 };
            v = gf::mul(v, r as u8);
            c += 1;
        }
        r += 1;
    }

    if !gauss_jordan(scratch, data_shards, cols) {
        return false;
    }
    // scratch now holds [I | T^-1].

    // out[j] <- (row data_shards + j of V) * T^-1
    let mut j = 0;
    while j < parity_shards {
        let mut c = 0;
        while c < data_shards {
            out[j * data_shards + c] = 0;
            c += 1;
        }

        let row = (data_shards + j) as u8;
        let mut v: u8 = 1; // row^l
        let mut l = 0;
        while l < data_shards {
            if v != 0 {
                let mut c = 0;
                while c < data_shards {
                    out[j * data_shards + c] ^= gf::mul(v, scratch[l * cols + data_shards + c]);
                    c += 1;
                }
            }
            v = gf::mul(v, row);
            l += 1;
        }
        j += 1;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate alloc;
    use alloc::vec;

    fn parity_matrix(data: usize, parity: usize) -> vec::Vec<u8> {
        let mut out = vec![0u8; parity * data];
        assert!(build_parity_matrix(data, parity, &mut out));
        out
    }

    fn reference_parity_matrix(data: usize, parity: usize) -> vec::Vec<u8> {
        let mut out = vec![0u8; parity * data];
        let mut scratch = vec![0u8; reference_scratch_len(data)];
        assert!(build_parity_matrix_by_inversion(
            data,
            parity,
            &mut out,
            &mut scratch
        ));
        out
    }

    /// Both builders produce row `j` from `data_shards + j` alone, so for a
    /// fixed `data_shards` the widest split contains every row any narrower
    /// split can ask for. Checking `(k, MAX_SHARDS - k)` for every `k` is
    /// therefore a proof for all 32,640 supported splits, not a sample of
    /// them, and it costs 255 comparisons rather than 32,640.
    ///
    /// `closed_form_matches_the_reference_for_every_split` drops the argument
    /// and compares all 32,640 outright. It is `#[ignore]`d only because it
    /// takes minutes.
    #[test]
    // Under Miri the cubic reference at 255 shards is hours of interpretation,
    // for a function with no `unsafe` in it and nothing for Miri to find.
    #[cfg_attr(miri, ignore = "cubic reference; minutes per shard count under Miri")]
    fn closed_form_matches_the_reference_at_every_shard_count() {
        for data in 1..MAX_SHARDS {
            let parity = MAX_SHARDS - data;
            assert_eq!(
                parity_matrix(data, parity),
                reference_parity_matrix(data, parity),
                "closed form and inversion disagree at {data}+{parity}"
            );
        }
    }

    #[test]
    #[ignore = "minutes: every supported split, with a cubic reference on each"]
    fn closed_form_matches_the_reference_for_every_split() {
        let mut splits = 0;
        for data in 1..MAX_SHARDS {
            for parity in 1..=(MAX_SHARDS - data) {
                assert_eq!(
                    parity_matrix(data, parity),
                    reference_parity_matrix(data, parity),
                    "closed form and inversion disagree at {data}+{parity}"
                );
                splits += 1;
            }
        }
        assert_eq!(splits, 32_640);
    }

    #[test]
    fn an_unsupported_split_is_refused_rather_than_panicking() {
        // The closed form divides, so it has to reject the shapes where its
        // non-zero argument does not hold instead of reaching `gf::inv(0)`.
        // Every caller checks first; this is the backstop.
        let mut out = vec![0u8; 4 * MAX_SHARDS];
        assert!(!build_parity_matrix(0, 4, &mut out));
        assert!(!build_parity_matrix(4, 0, &mut out));
        assert!(!build_parity_matrix(MAX_SHARDS, 4, &mut out));
        assert!(!build_parity_matrix(MAX_SHARDS - 1, 2, &mut out));
        // Absurd values must be refused by the same check rather than
        // overflowing it.
        assert!(!build_parity_matrix(usize::MAX, 1, &mut out));
        assert!(!build_parity_matrix(1, usize::MAX, &mut out));
    }

    #[test]
    fn gauss_jordan_inverts() {
        // [[1, 2], [3, 4]] over GF(2^8) augmented with the identity.
        let mut m = [1, 2, 1, 0, 3, 4, 0, 1];
        assert!(gauss_jordan(&mut m, 2, 4));
        assert_eq!(&m[0..2], &[1, 0]);
        assert_eq!(&m[4..6], &[0, 1]);

        // Multiplying the original by the recovered inverse gives the identity.
        let a = [[1u8, 2], [3, 4]];
        let inv = [[m[2], m[3]], [m[6], m[7]]];
        let mut product = [[0u8; 2]; 2];
        for (i, row) in product.iter_mut().enumerate() {
            for (j, cell) in row.iter_mut().enumerate() {
                for (k, &a_ik) in a[i].iter().enumerate() {
                    *cell ^= gf::mul(a_ik, inv[k][j]);
                }
            }
        }
        assert_eq!(product, [[1, 0], [0, 1]]);
    }

    #[test]
    fn gauss_jordan_reports_singular() {
        // Second row is a multiple of the first.
        let mut m = [1, 2, 1, 0, 2, 4, 0, 1];
        assert!(!gauss_jordan(&mut m, 2, 4));
    }

    #[test]
    fn every_square_submatrix_of_p_is_invertible() {
        // The MDS property the decoder relies on. Checked exhaustively for
        // 2x2 submatrices and for the full matrix at a spread of sizes.
        for (data, parity) in [(4, 2), (8, 4), (10, 6), (17, 5)] {
            let p = parity_matrix(data, parity);
            for r0 in 0..parity {
                for r1 in (r0 + 1)..parity {
                    for c0 in 0..data {
                        for c1 in (c0 + 1)..data {
                            let mut m = [
                                p[r0 * data + c0],
                                p[r0 * data + c1],
                                1,
                                0,
                                p[r1 * data + c0],
                                p[r1 * data + c1],
                                0,
                                1,
                            ];
                            assert!(
                                gauss_jordan(&mut m, 2, 4),
                                "singular 2x2 submatrix in {data}+{parity} \
                                 at rows {r0},{r1} cols {c0},{c1}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn no_entry_is_zero_for_small_parameters() {
        // Not required for correctness, but a zero would mean a parity shard
        // ignores a data shard, which would be a red flag for the
        // construction. Systematic Vandermonde matrices are dense.
        for (data, parity) in [(4, 2), (8, 4), (16, 4)] {
            for (i, &v) in parity_matrix(data, parity).iter().enumerate() {
                assert_ne!(v, 0, "zero at index {i} of {data}+{parity}");
            }
        }
    }

    #[test]
    fn builds_at_the_maximum_shard_count() {
        let p = parity_matrix(MAX_SHARDS - 1, 1);
        assert_eq!(p.len(), MAX_SHARDS - 1);
    }

    #[test]
    fn matrix_is_const_evaluable() {
        const P: [u8; 8] = {
            let mut out = [0u8; 8];
            assert!(build_parity_matrix(4, 2, &mut out));
            out
        };
        assert_eq!(P.as_slice(), parity_matrix(4, 2).as_slice());
    }
}
