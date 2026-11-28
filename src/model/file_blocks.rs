//! Persistent file-block sequence and exact offset index. The JSON wire
//! representation remains an ordered array. Unrestricted legacy mutations
//! invalidate the derived index; normal range edits preserve shared nodes.
use super::types::FileBlock;
use imbl::{OrdMap, Vector};
use serde::{Deserialize, Serialize};
use std::ops::{Deref, DerefMut};
use std::sync::OnceLock;

#[derive(Clone, Debug)]
struct OffsetIndex {
    blocks: OrdMap<u64, FileBlock>,
    nonoverlapping: bool,
    stripes: Option<OrdMap<String, u64>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FileBlocks {
    blocks: Vector<FileBlock>,
    #[serde(skip)]
    index: OnceLock<Option<OffsetIndex>>,
}

impl FileBlocks {
    fn index(&self) -> Option<&OffsetIndex> {
        self.index
            .get_or_init(|| {
                let mut nonoverlapping = self.blocks.iter().all(|b| {
                    b.raw_size > 0 && b.raw_offset.checked_add(b.raw_size as u64).is_some()
                });
                for (a, b) in self.blocks.iter().zip(self.blocks.iter().skip(1)) {
                    if a.raw_offset >= b.raw_offset {
                        return None;
                    }
                    nonoverlapping &=
                        a.raw_offset.saturating_add(a.raw_size as u64) <= b.raw_offset;
                }
                let mut stripes = OrdMap::new();
                let unique = self.blocks.iter().all(|block| {
                    stripes
                        .insert(block.stripe_id.clone(), block.raw_offset)
                        .is_none()
                });
                Some(OffsetIndex {
                    blocks: self
                        .blocks
                        .iter()
                        .map(|b| (b.raw_offset, b.clone()))
                        .collect(),
                    nonoverlapping,
                    stripes: unique.then_some(stripes),
                })
            })
            .as_ref()
    }

    pub(crate) fn offset_index(&self) -> Option<&OrdMap<u64, FileBlock>> {
        self.index().map(|index| &index.blocks)
    }

    pub(crate) fn unique_stripe_index(&self) -> Option<&OrdMap<u64, FileBlock>> {
        self.index()
            .filter(|index| index.stripes.is_some())
            .map(|index| &index.blocks)
    }

    fn lower_bound(&self, offset: u64) -> usize {
        let (mut lo, mut hi) = (0, self.blocks.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.blocks[mid].raw_offset < offset {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Insert blocks at their logical positions without sorting an existing
    /// large, ordered file. Malformed legacy lists retain the old sort path.
    pub(crate) fn insert_sorted(&mut self, items: impl IntoIterator<Item = FileBlock>) {
        if self.index().is_none() {
            self.index.take();
            self.blocks.extend(items);
            self.blocks.sort_by(|a, b| a.raw_offset.cmp(&b.raw_offset));
            return;
        }
        let mut index = self.index.take().flatten().unwrap();
        let mut unique = true;
        for block in items {
            index.nonoverlapping &= block.raw_size > 0
                && block
                    .raw_offset
                    .checked_add(block.raw_size as u64)
                    .is_some();
            let mut pos = self.lower_bound(block.raw_offset);
            while self
                .blocks
                .get(pos)
                .is_some_and(|old| old.raw_offset == block.raw_offset)
            {
                pos += 1;
            }
            if let Some(previous) = pos.checked_sub(1).and_then(|i| self.blocks.get(i)) {
                index.nonoverlapping &=
                    previous.raw_offset.saturating_add(previous.raw_size as u64)
                        <= block.raw_offset;
            }
            if let Some(next) = self.blocks.get(pos) {
                index.nonoverlapping &=
                    block.raw_offset.saturating_add(block.raw_size as u64) <= next.raw_offset;
            }
            if let Some(stripes) = &mut index.stripes {
                if stripes
                    .insert(block.stripe_id.clone(), block.raw_offset)
                    .is_some()
                {
                    index.stripes = None;
                }
            }
            unique &= index
                .blocks
                .insert(block.raw_offset, block.clone())
                .is_none();
            self.blocks.insert(pos, block);
        }
        if unique {
            self.index = OnceLock::from(Some(index));
        }
    }

    pub(crate) fn edit_at(&mut self, position: usize, edit: impl FnOnce(&mut FileBlock)) {
        // Most maintenance edits change shard placement, not logical ranges.
        let old = &self.blocks[position];
        let offset = old.raw_offset;
        let size = old.raw_size;
        let stripe = old.stripe_id.clone();
        let mut index = self.index.take().flatten();
        edit(&mut self.blocks[position]);
        let new = &self.blocks[position];
        if offset == new.raw_offset && size == new.raw_size {
            if let Some(mut index) = index.take() {
                if stripe != new.stripe_id {
                    if let Some(stripes) = &mut index.stripes {
                        stripes.remove(&stripe);
                        if stripes.insert(new.stripe_id.clone(), offset).is_some() {
                            index.stripes = None;
                        }
                    }
                }
                index.blocks.insert(offset, new.clone());
                self.index = OnceLock::from(Some(index));
            }
        }
    }

    fn remove_range(&mut self, start: usize, end: usize, removed: &mut Vec<FileBlock>) {
        if start == end {
            return;
        }
        let tail = self.blocks.split_off(end);
        let middle = self.blocks.split_off(start);
        let mut index = self.index.take().flatten();
        for block in middle {
            if let Some(index) = &mut index {
                index.blocks.remove(&block.raw_offset);
                if let Some(stripes) = &mut index.stripes {
                    stripes.remove(&block.stripe_id);
                }
            }
            removed.push(block);
        }
        self.blocks.append(tail);
        self.index = OnceLock::from(index);
    }

    /// Remove just the overlapping range and blocks beyond the new EOF.
    /// The valid non-overlap proof lets us find boundaries logarithmically.
    /// For old malformed/overlapping lists use the exact legacy predicate.
    pub(crate) fn remove_window(&mut self, start: u64, end: u64, new_size: u64) -> Vec<FileBlock> {
        if end < start || !self.index().is_some_and(|index| index.nonoverlapping) {
            self.index.take();
            let mut removed = Vec::new();
            self.blocks.retain(|block| {
                let keep = (block.raw_offset.saturating_add(block.raw_size as u64) <= start
                    || block.raw_offset >= end)
                    && block.raw_offset < new_size;
                if !keep {
                    removed.push(block.clone());
                }
                keep
            });
            return removed;
        }
        let mut first = self.lower_bound(start);
        if first > 0 {
            let previous = &self.blocks[first - 1];
            if previous.raw_offset.saturating_add(previous.raw_size as u64) > start {
                first -= 1;
            }
        }
        let last = self.lower_bound(end).max(first);
        let truncate = self.lower_bound(new_size);
        let mut removed = Vec::new();
        // Preserve ascending order in the reclamation list, as in the old
        // linear implementation. Remove the rightmost range first.
        let mut tail = Vec::new();
        self.remove_range(truncate, self.blocks.len(), &mut tail);
        self.remove_range(first.min(truncate), last.min(truncate), &mut removed);
        removed.extend(tail);
        removed
    }

    pub fn first(&self) -> Option<&FileBlock> {
        self.blocks.front()
    }
    pub fn last(&self) -> Option<&FileBlock> {
        self.blocks.back()
    }
    pub fn push(&mut self, block: FileBlock) {
        self.index.take();
        self.blocks.push_back(block);
    }
}

impl Deref for FileBlocks {
    type Target = Vector<FileBlock>;
    fn deref(&self) -> &Self::Target {
        &self.blocks
    }
}
impl DerefMut for FileBlocks {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.index.take();
        &mut self.blocks
    }
}
impl From<Vec<FileBlock>> for FileBlocks {
    fn from(value: Vec<FileBlock>) -> Self {
        value.into_iter().collect()
    }
}
impl FromIterator<FileBlock> for FileBlocks {
    fn from_iter<T: IntoIterator<Item = FileBlock>>(items: T) -> Self {
        Self {
            blocks: items.into_iter().collect(),
            index: OnceLock::new(),
        }
    }
}
impl IntoIterator for FileBlocks {
    type Item = FileBlock;
    type IntoIter = <Vector<FileBlock> as IntoIterator>::IntoIter;
    fn into_iter(self) -> Self::IntoIter {
        self.blocks.into_iter()
    }
}
impl<'a> IntoIterator for &'a FileBlocks {
    type Item = &'a FileBlock;
    type IntoIter = <&'a Vector<FileBlock> as IntoIterator>::IntoIter;
    fn into_iter(self) -> Self::IntoIter {
        self.blocks.iter()
    }
}
impl<'a> IntoIterator for &'a mut FileBlocks {
    type Item = &'a mut FileBlock;
    type IntoIter = <&'a mut Vector<FileBlock> as IntoIterator>::IntoIter;
    fn into_iter(self) -> Self::IntoIter {
        self.index.take();
        self.blocks.iter_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Compression, StorageTier};

    fn block(offset: u64, size: usize) -> FileBlock {
        FileBlock {
            layout_id: "test".into(),
            stripe_id: format!("stripe-{offset}"),
            raw_offset: offset,
            raw_size: size,
            raw_sha256: String::new(),
            codec: Compression::None,
            encrypted: false,
            nonce_hex: String::new(),
            compressed_size: size,
            shard_size: size,
            shards: Vec::new(),
            storage_class: StorageTier::Warm,
        }
    }

    #[test]
    fn persistent_range_edit_matches_legacy_predicate() {
        let original: FileBlocks = (0..10000).map(|i| block(i * 8192, 4096)).collect();
        original.offset_index().unwrap();
        for (start, end, size) in [
            (4096, 8192, 1 << 30),
            (9000, 9000, 1 << 30),
            (8192, 16384, 8192),
            (1, 2, 1 << 30),
            (0, 0, 0),
            (99999999, 100000000, 1234567),
        ] {
            let mut next = original.clone();
            let removed = next.remove_window(start, end, size);
            let keep = |b: &FileBlock| {
                (b.raw_offset + b.raw_size as u64 <= start || b.raw_offset >= end)
                    && b.raw_offset < size
            };
            assert_eq!(
                next.iter().cloned().collect::<Vec<_>>(),
                original
                    .iter()
                    .filter(|b| keep(b))
                    .cloned()
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                removed,
                original
                    .iter()
                    .filter(|b| !keep(b))
                    .cloned()
                    .collect::<Vec<_>>()
            );
            assert_eq!(next.offset_index().unwrap().len(), next.len());
        }
        let mut next = original.clone();
        next.insert_sorted([block(4096, 4096)]);
        next.edit_at(2, |b| b.raw_sha256 = "changed".into());
        assert_eq!(
            original
                .offset_index()
                .unwrap()
                .diff(next.offset_index().unwrap())
                .count(),
            2
        );
        assert_eq!(original.len(), 10000);
        assert_eq!(next.len(), 10001);
        let mut cold: FileBlocks = vec![block(0, 4096)].into();
        cold.edit_at(0, |b| b.raw_sha256 = "edited".into());
        assert_eq!(cold.offset_index().unwrap()[&0].raw_sha256, "edited");
    }
}
