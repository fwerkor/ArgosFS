//! Copy-on-write free extent sequence with a cached ordering proof.
use super::types::RawFreeExtent;
use imbl::{OrdMap, OrdSet, Vector};
use serde::{Deserialize, Serialize};
use std::ops::{Deref, DerefMut};
use std::sync::OnceLock;

/// Persistent length -> offsets index for normalized free lists. Exact
/// length buckets retain first-fit placement, not best-fit placement.
#[derive(Clone, Debug, Default)]
pub(crate) struct FitIndex {
    by_length: OrdMap<u64, OrdSet<u64>>,
}

impl FitIndex {
    pub(crate) fn insert(&mut self, extent: &RawFreeExtent) {
        self.by_length
            .entry(extent.length)
            .or_default()
            .insert(extent.offset);
    }

    pub(crate) fn remove(&mut self, extent: &RawFreeExtent) {
        if let Some(offsets) = self.by_length.get_mut(&extent.length) {
            offsets.remove(&extent.offset);
            if offsets.is_empty() {
                self.by_length.remove(&extent.length);
            }
        }
    }

    fn first_fit(&self, length: u64, alignment: u64) -> Option<u64> {
        self.by_length
            .range(length..)
            .filter_map(|(_, offsets)| {
                offsets
                    .iter()
                    .find(|offset| **offset % alignment == 0)
                    .copied()
            })
            .min()
    }

    pub(crate) fn max_length(&self) -> u64 {
        self.by_length
            .iter()
            .next_back()
            .map_or(0, |(length, _)| *length)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(transparent)]
pub struct FreeExtents {
    extents: Vector<RawFreeExtent>,
    #[serde(skip)]
    normalized: OnceLock<bool>,
    #[serde(skip)]
    length_upper_bound: OnceLock<u64>,
    #[serde(skip)]
    fit_index: OnceLock<FitIndex>,
}

impl FreeExtents {
    /// Some(None) means an indexed search proved that no extent fits;
    /// None means use the original linear search (small/legacy lists).
    pub(crate) fn indexed_fit(
        &self,
        length: u64,
        alignment: u64,
    ) -> Option<Option<(usize, RawFreeExtent)>> {
        let index = self.fit_index()?;
        let Some(offset) = index.first_fit(length, alignment) else {
            return Some(None);
        };
        let (mut lo, mut hi) = (0, self.extents.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let extent = &self.extents[mid];
            match extent.offset.cmp(&offset) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(Some((mid, extent.clone()))),
            }
        }
        // Defensive fallback; should be impossible with trusted mutations.
        None
    }

    fn fit_index(&self) -> Option<&FitIndex> {
        if self.fit_index.get().is_none() && (self.extents.len() < 1024 || !self.is_normalized()) {
            return None;
        }
        Some(self.fit_index.get_or_init(|| {
            let mut index = FitIndex::default();
            for extent in &self.extents {
                index.insert(extent);
            }
            index
        }))
    }

    /// Exact aligned capacity from the length buckets already maintained by
    /// allocator operations. Avoid revisiting every fragmented extent on
    /// each statfs publication. Invalid/unordered legacy lists use the old
    /// scan and therefore retain its accounting semantics.
    pub(crate) fn aligned_free_bytes(&self, block: u64) -> u64 {
        let block = block.max(1);
        if let Some(index) = self.fit_index() {
            index.by_length.iter().fold(0u64, |sum, (length, offsets)| {
                sum.saturating_add((length / block * block).saturating_mul(offsets.len() as u64))
            })
        } else {
            self.extents.iter().fold(0u64, |sum, extent| {
                sum.saturating_add(extent.length / block * block)
            })
        }
    }

    pub(crate) fn take_fit_index(&mut self) -> Option<FitIndex> {
        self.fit_index.take()
    }

    pub(crate) fn restore_fit_index(&mut self, index: FitIndex) {
        self.mark_length_upper_bound(index.max_length());
        self.fit_index = OnceLock::from(index);
    }

    pub(crate) fn is_normalized(&self) -> bool {
        *self.normalized.get_or_init(|| {
            self.extents
                .iter()
                .zip(self.extents.iter().skip(1))
                .all(|(left, right)| left.offset.saturating_add(left.length) < right.offset)
        })
    }

    pub(crate) fn known_length_upper_bound(&self) -> Option<u64> {
        self.length_upper_bound.get().copied()
    }

    /// A conservative upper bound is enough to rule out a first-fit search.
    /// Only a full unsuccessful scan or allocator-owned mutations may set it.
    pub(crate) fn mark_length_upper_bound(&mut self, bound: u64) {
        self.length_upper_bound = OnceLock::from(bound);
    }

    pub(crate) fn known_normalized(&self) -> bool {
        self.normalized.get().copied() == Some(true)
    }

    /// Only allocator operations that preserve ordering and separation may
    /// establish this proof without re-scanning the sequence.
    pub(crate) fn mark_normalized(&mut self) {
        self.normalized = OnceLock::from(true);
    }
}

impl Deref for FreeExtents {
    type Target = Vector<RawFreeExtent>;
    fn deref(&self) -> &Self::Target {
        &self.extents
    }
}

impl DerefMut for FreeExtents {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // All external mutation paths (including IndexMut, get_mut, and
        // whole-vector replacement) invalidate the ordering proof first.
        self.normalized.take();
        self.length_upper_bound.take();
        self.fit_index.take();
        &mut self.extents
    }
}

impl PartialEq for FreeExtents {
    fn eq(&self, other: &Self) -> bool {
        self.extents == other.extents
    }
}
impl Eq for FreeExtents {}

impl From<Vec<RawFreeExtent>> for FreeExtents {
    fn from(extents: Vec<RawFreeExtent>) -> Self {
        extents.into_iter().collect()
    }
}

impl FromIterator<RawFreeExtent> for FreeExtents {
    fn from_iter<T: IntoIterator<Item = RawFreeExtent>>(items: T) -> Self {
        Self {
            extents: items.into_iter().collect(),
            normalized: OnceLock::new(),
            length_upper_bound: OnceLock::new(),
            fit_index: OnceLock::new(),
        }
    }
}

impl IntoIterator for FreeExtents {
    type Item = RawFreeExtent;
    type IntoIter = <Vector<RawFreeExtent> as IntoIterator>::IntoIter;
    fn into_iter(self) -> Self::IntoIter {
        self.extents.into_iter()
    }
}
