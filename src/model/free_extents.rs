//! Copy-on-write free extent sequence with a cached ordering proof.
use super::types::RawFreeExtent;
use imbl::Vector;
use serde::{Deserialize, Serialize};
use std::ops::{Deref, DerefMut};
use std::sync::OnceLock;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(transparent)]
pub struct FreeExtents {
    extents: Vector<RawFreeExtent>,
    #[serde(skip)]
    normalized: OnceLock<bool>,
    #[serde(skip)]
    length_upper_bound: OnceLock<u64>,
}

impl FreeExtents {
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
