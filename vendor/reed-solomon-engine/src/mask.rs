//! Shard presence bitmap.

use crate::error::MAX_SHARDS;
use core::fmt;

const WORDS: usize = MAX_SHARDS / 64;

/// A bitmap marking which shards are present.
///
/// Fixed at [`MAX_SHARDS`] bits, so it is `Copy`, allocation-free and usable in
/// `no_std`. Bit `i` corresponds to shard `i` in the caller's shard slice: data
/// shards first, then parity shards.
///
/// ```
/// # use reed_solomon_engine::ShardMask;
/// let mut mask = ShardMask::all_present(6);
/// mask.remove(1);
/// mask.remove(4);
/// assert_eq!(mask.count_present(), 4);
/// assert!(!mask.is_present(1));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ShardMask {
    words: [u64; WORDS],
}

impl ShardMask {
    /// A mask with no shards present.
    pub const EMPTY: ShardMask = ShardMask { words: [0; WORDS] };

    /// Creates an empty mask.
    #[inline]
    pub const fn new() -> Self {
        Self::EMPTY
    }

    /// Creates a mask with shards `0..count` present.
    ///
    /// # Panics
    ///
    /// Panics if `count` exceeds [`MAX_SHARDS`].
    pub const fn all_present(count: usize) -> Self {
        assert!(count <= MAX_SHARDS, "count exceeds MAX_SHARDS");
        let mut mask = Self::EMPTY;
        let mut i = 0;
        while i < count {
            mask.words[i / 64] |= 1u64 << (i % 64);
            i += 1;
        }
        mask
    }

    /// Marks shard `index` as present.
    ///
    /// Indices at or beyond [`MAX_SHARDS`] are ignored.
    #[inline]
    pub const fn insert(&mut self, index: usize) {
        if index < MAX_SHARDS {
            self.words[index / 64] |= 1u64 << (index % 64);
        }
    }

    /// Marks shard `index` as missing.
    ///
    /// Indices at or beyond [`MAX_SHARDS`] are ignored.
    #[inline]
    pub const fn remove(&mut self, index: usize) {
        if index < MAX_SHARDS {
            self.words[index / 64] &= !(1u64 << (index % 64));
        }
    }

    /// Sets the presence of shard `index`.
    #[inline]
    pub const fn set(&mut self, index: usize, present: bool) {
        if present {
            self.insert(index);
        } else {
            self.remove(index);
        }
    }

    /// Returns whether shard `index` is present.
    ///
    /// Indices at or beyond [`MAX_SHARDS`] are never present.
    #[inline]
    pub const fn is_present(&self, index: usize) -> bool {
        index < MAX_SHARDS && self.words[index / 64] & (1u64 << (index % 64)) != 0
    }

    /// Returns the number of shards marked present.
    #[inline]
    pub const fn count_present(&self) -> usize {
        let mut total = 0;
        let mut i = 0;
        while i < WORDS {
            total += self.words[i].count_ones() as usize;
            i += 1;
        }
        total
    }

    /// Returns the number of shards in `0..limit` marked present.
    #[inline]
    pub const fn count_present_within(&self, limit: usize) -> usize {
        let mut total = 0;
        let mut i = 0;
        while i < limit && i < MAX_SHARDS {
            if self.words[i / 64] & (1u64 << (i % 64)) != 0 {
                total += 1;
            }
            i += 1;
        }
        total
    }
}

impl fmt::Display for ShardMask {
    /// Writes `MAX_SHARDS` characters, `#` for present and `.` for missing,
    /// lowest index first.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for i in 0..MAX_SHARDS {
            f.write_str(if self.is_present(i) { "#" } else { "." })?;
        }
        Ok(())
    }
}

impl FromIterator<usize> for ShardMask {
    /// Builds a mask from the indices of the present shards.
    fn from_iter<I: IntoIterator<Item = usize>>(iter: I) -> Self {
        let mut mask = Self::EMPTY;
        for index in iter {
            mask.insert(index);
        }
        mask
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_query_every_index() {
        let mut mask = ShardMask::new();
        for i in 0..MAX_SHARDS {
            assert!(!mask.is_present(i));
            mask.insert(i);
            assert!(mask.is_present(i));
            assert_eq!(mask.count_present(), 1);
            mask.remove(i);
        }
        assert_eq!(mask, ShardMask::EMPTY);
    }

    #[test]
    fn out_of_range_indices_are_inert() {
        let mut mask = ShardMask::new();
        mask.insert(MAX_SHARDS);
        mask.insert(usize::MAX);
        assert_eq!(mask, ShardMask::EMPTY);
        assert!(!mask.is_present(MAX_SHARDS));
    }

    #[test]
    fn counting_within_a_limit() {
        let mask: ShardMask = [0usize, 3, 64, 200].into_iter().collect();
        assert_eq!(mask.count_present(), 4);
        assert_eq!(mask.count_present_within(0), 0);
        assert_eq!(mask.count_present_within(1), 1);
        assert_eq!(mask.count_present_within(64), 2);
        assert_eq!(mask.count_present_within(65), 3);
        assert_eq!(mask.count_present_within(MAX_SHARDS), 4);
    }

    #[test]
    fn all_present_covers_exactly_the_prefix() {
        for n in 0..=MAX_SHARDS {
            let mask = ShardMask::all_present(n);
            assert_eq!(mask.count_present(), n);
            for i in 0..MAX_SHARDS {
                assert_eq!(mask.is_present(i), i < n);
            }
        }
    }
}
