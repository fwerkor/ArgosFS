//! Arithmetic in the Galois field GF(2^8).
//!
//! The field is `GF(2)[x] / p(x)` with the reduction polynomial
//! [`POLYNOMIAL`] `= 0x11D`, and `0x02` as the primitive element used to
//! generate the logarithm tables. This is the field the standard GF(2^8)
//! erasure-coding format uses, which is what makes this crate's parity bytes
//! interchangeable with other implementations of it.
//!
//! Every table here is built by a `const fn` at compile time, so the field
//! costs nothing to initialize at runtime and the optimizer can see through it.

/// Reduction polynomial of the field, `x^8 + x^4 + x^3 + x^2 + 1`.
///
/// Note this is *not* the AES polynomial (`0x11B`).
pub const POLYNOMIAL: u16 = 0x11D;

/// The multiplicative group of the field has this many elements.
const ORDER: usize = 255;

/// `(exp, log)` where `exp[i]` is `g^i` for the primitive element `g = 0x02`,
/// and `log[exp[i]] == i`. Multiplying by `0x02` is a left shift followed by a
/// conditional reduction, which is why the loop below looks like it does.
///
/// `exp` is doubled to 512 entries so that `log[a] + log[b]` (at most 508) can
/// index it without a modular reduction.
const fn build_tables() -> ([u8; 512], [u8; 256]) {
    let mut exp = [0u8; 512];
    let mut log = [0u8; 256];

    let mut x: u16 = 1;
    let mut i = 0;
    while i < ORDER {
        exp[i] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= POLYNOMIAL;
        }
        i += 1;
    }

    // Wrap-around copy: exp[i] == exp[i - 255] for i >= 255.
    let mut i = ORDER;
    while i < exp.len() {
        exp[i] = exp[i - ORDER];
        i += 1;
    }

    (exp, log)
}

const TABLES: ([u8; 512], [u8; 256]) = build_tables();
const EXP: [u8; 512] = TABLES.0;
const LOG: [u8; 256] = TABLES.1;

/// Multiplies two field elements.
#[inline]
pub const fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    EXP[LOG[a as usize] as usize + LOG[b as usize] as usize]
}

/// Divides `a` by `b`.
///
/// # Panics
///
/// Panics if `b` is zero.
#[inline]
pub const fn div(a: u8, b: u8) -> u8 {
    assert!(b != 0, "division by zero in GF(2^8)");
    if a == 0 {
        return 0;
    }
    // Bias by ORDER so the subtraction cannot underflow.
    EXP[LOG[a as usize] as usize + ORDER - LOG[b as usize] as usize]
}

/// Returns the multiplicative inverse of `a`.
///
/// # Panics
///
/// Panics if `a` is zero.
#[inline]
pub const fn inv(a: u8) -> u8 {
    assert!(a != 0, "zero has no multiplicative inverse in GF(2^8)");
    if a == 1 {
        return 1;
    }
    EXP[ORDER - LOG[a as usize] as usize]
}

/// Raises `a` to the power `n`, with `pow(a, 0) == 1` for every `a` including
/// zero.
///
/// The `pow(0, 0) == 1` convention matters: it is what makes row 0 of the
/// Vandermonde matrix `[1, 0, 0, ...]`, and it is what the reference
/// implementations this crate is wire-compatible with do.
#[inline]
#[cfg(test)]
pub const fn pow(a: u8, n: u32) -> u8 {
    if n == 0 {
        return 1;
    }
    if a == 0 {
        return 0;
    }
    let mut acc = (LOG[a as usize] as u64) * (n as u64);
    acc %= ORDER as u64;
    EXP[acc as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_exp_are_inverses() {
        for a in 1..=255u8 {
            assert_eq!(EXP[LOG[a as usize] as usize], a, "a = {a}");
        }
    }

    #[test]
    fn multiplication_matches_carryless_reference() {
        // Independent reference: schoolbook carry-less multiply followed by
        // polynomial reduction. Deliberately not table-driven.
        fn reference(a: u8, b: u8) -> u8 {
            let mut product: u16 = 0;
            for bit in 0..8 {
                if b & (1 << bit) != 0 {
                    product ^= (a as u16) << bit;
                }
            }
            for bit in (8..16).rev() {
                if product & (1 << bit) != 0 {
                    product ^= POLYNOMIAL << (bit - 8);
                }
            }
            product as u8
        }

        for a in 0..=255u8 {
            for b in 0..=255u8 {
                assert_eq!(mul(a, b), reference(a, b), "{a} * {b}");
            }
        }
    }

    #[test]
    fn field_axioms() {
        for a in 0..=255u8 {
            assert_eq!(mul(a, 0), 0);
            assert_eq!(mul(a, 1), a);
            for b in 0..=255u8 {
                assert_eq!(mul(a, b), mul(b, a));
            }
        }
        for a in 1..=255u8 {
            assert_eq!(mul(a, inv(a)), 1, "a = {a}");
            for b in 1..=255u8 {
                assert_eq!(mul(div(a, b), b), a, "{a} / {b}");
            }
        }
    }

    #[test]
    fn distributivity() {
        for a in 0..=255u8 {
            for b in 0..=255u8 {
                for c in [0u8, 1, 2, 7, 100, 255] {
                    assert_eq!(mul(c, a ^ b), mul(c, a) ^ mul(c, b));
                }
            }
        }
    }

    #[test]
    fn pow_matches_repeated_multiplication() {
        for a in 0..=255u8 {
            let mut expected = 1u8;
            for n in 0..300u32 {
                assert_eq!(pow(a, n), expected, "{a}^{n}");
                expected = mul(expected, a);
            }
        }
    }

    #[test]
    fn tables_are_const_evaluated() {
        const CHECK: u8 = mul(0x53, 0xCA);
        assert_eq!(CHECK, mul(0x53, 0xCA));
    }
}
