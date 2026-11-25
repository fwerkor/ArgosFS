//! Split-nibble multiplication tables.
//!
//! Multiplication by a fixed scalar `c` is F2-linear, so it distributes over
//! the two nibbles of a byte:
//!
//! ```text
//!   mul(c, b) == mul(c, b & 0x0F) ^ mul(c, b & 0xF0)
//! ```
//!
//! Tabulating each half over its 16 possible values gives two 16-entry byte
//! tables per scalar, and a 16-entry byte table is exactly what a SIMD byte
//! shuffle indexes: `pshufb` on x86, `vqtbl1q_u8` on aarch64,
//! `i8x16.swizzle` on WASM. That turns 16, 32 or 64 field multiplications into
//! two shuffles and an XOR.
//!
//! All 256 scalars are tabulated up front. The whole thing is 8 KiB of
//! `const fn`-evaluated `.rodata`, which is why there is nothing to initialize
//! at runtime and no lazy-init check on the hot path.

use crate::data::ec::gf;

/// Low-nibble table for scalar `c`, spanning bytes `0..16` of its entry.
pub(crate) const LOW: usize = 0;
/// High-nibble table for scalar `c`, spanning bytes `16..32` of its entry.
pub(crate) const HIGH: usize = 16;

/// Both tables for every scalar, one 32-byte entry per scalar.
///
/// Aligned to 64 bytes so each entry sits inside a single cache line and every
/// 16-byte half is naturally aligned for a vector load.
#[repr(align(64))]
pub(crate) struct SplitTables([[u8; 32]; 256]);

impl SplitTables {
    /// The 32-byte entry for scalar `c`: low-nibble table then high-nibble
    /// table.
    #[inline]
    pub(crate) fn get(&self, c: u8) -> &[u8; 32] {
        &self.0[c as usize]
    }
}

/// `SPLIT.get(c)[LOW + i] == mul(c, i)` and
/// `SPLIT.get(c)[HIGH + i] == mul(c, i << 4)` for `i` in `0..16`.
///
/// A `static` rather than a `const` so the tables have one stable address to
/// load from, instead of being materialized afresh at each use site.
pub(crate) static SPLIT: SplitTables = SplitTables(TABLES);

/// The tables as a `const`, which is what forces them to be evaluated by the
/// compiler rather than assembled at startup.
const TABLES: [[u8; 32]; 256] = build();

const fn build() -> [[u8; 32]; 256] {
    let mut tables = [[0u8; 32]; 256];
    let mut c = 0;
    while c < 256 {
        let mut i = 0;
        while i < 16 {
            tables[c][LOW + i] = gf::mul(c as u8, i as u8);
            tables[c][HIGH + i] = gf::mul(c as u8, (i as u8) << 4);
            i += 1;
        }
        c += 1;
    }
    tables
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_split_reproduces_full_multiplication() {
        // The identity every SIMD kernel in this crate rests on, checked
        // exhaustively over all 65536 (scalar, byte) pairs.
        for c in 0..=255u8 {
            let table = SPLIT.get(c);
            for b in 0..=255u8 {
                let split = table[LOW + (b & 0x0F) as usize] ^ table[HIGH + (b >> 4) as usize];
                assert_eq!(split, gf::mul(c, b), "c = {c}, b = {b}");
            }
        }
    }

    #[test]
    fn the_identity_and_zero_scalars_are_what_they_should_be() {
        let zero = SPLIT.get(0);
        assert_eq!(zero, &[0u8; 32]);

        let one = SPLIT.get(1);
        for i in 0..16u8 {
            assert_eq!(one[LOW + i as usize], i);
            assert_eq!(one[HIGH + i as usize], i << 4);
        }
    }

    #[test]
    fn the_tables_are_const_evaluated() {
        // If these were computed at runtime they could not appear in a const.
        const ENTRY: [u8; 32] = TABLES[0x53];
        assert_eq!(&ENTRY, SPLIT.get(0x53));
        assert_eq!(ENTRY[LOW + 7], gf::mul(0x53, 7));
    }
}
