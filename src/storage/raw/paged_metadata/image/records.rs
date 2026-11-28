//! Stable logical record IDs: inserting one directory entry never renumbers
//! following entries, and allocation addresses are independent of inode IDs.
use super::{decode, encode, Changes, PAGED_INTEGRITY_SCHEME};
use crate::error::{ArgosError, Result};
use crate::metadata::store::InodeCorePage;
use crate::types::*;
use imbl::OrdMap;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

fn corrupt(message: &str) -> ArgosError {
    ArgosError::CorruptedMetadata(message.into())
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Key {
    Header,
    Inode(u64),
    Dirent(u64, String),
    Xattr(u64, String),
    Block(u64, u64),
    Disk(String),
    Allocator(String),
    Free(String, u64),
}

#[derive(Deserialize, Serialize)]
enum Record {
    Header(Box<Metadata>),
    Inode(Box<InodeCorePage>),
    Dirent(u64, String, u64),
    Xattr(u64, String, String),
    Block(u64, Box<FileBlock>),
    Disk(Box<Disk>),
    Allocator(String, RawAllocatorState),
    Free(String, RawFreeExtent),
}

impl Record {
    fn key(&self) -> Key {
        match self {
            Self::Header(_) => Key::Header,
            Self::Inode(core) => Key::Inode(core.id),
            Self::Dirent(ino, name, _) => Key::Dirent(*ino, name.clone()),
            Self::Xattr(ino, name, _) => Key::Xattr(*ino, name.clone()),
            Self::Block(ino, block) => Key::Block(*ino, block.raw_offset),
            Self::Disk(disk) => Key::Disk(disk.id.clone()),
            Self::Allocator(disk, _) => Key::Allocator(disk.clone()),
            Self::Free(disk, extent) => Key::Free(disk.clone(), extent.offset),
        }
    }
}

#[derive(Clone, Default)]
pub(super) struct RecordIndex {
    ids: OrdMap<Key, u64>,
    next: u64,
}

struct Plan {
    changes: Changes,
    index: RecordIndex,
}

impl Plan {
    fn new(index: RecordIndex) -> Self {
        Self {
            changes: Changes::new(),
            index,
        }
    }

    fn put(&mut self, record: Record) -> Result<()> {
        let key = record.key();
        let id = if let Some(&id) = self.index.ids.get(&key) {
            id
        } else if key == Key::Header {
            self.index.ids.insert(key, 0);
            0
        } else {
            let id = self.index.next.max(1);
            self.index.next = id
                .checked_add(1)
                .ok_or_else(|| ArgosError::Invalid("metadata record ID exhausted".into()))?;
            self.index.ids.insert(key, id);
            id
        };
        self.changes.insert(id, Some(encode(&record)?));
        Ok(())
    }

    fn remove(&mut self, key: Key) -> Result<()> {
        let id = self
            .index
            .ids
            .remove(&key)
            .ok_or_else(|| corrupt("missing metadata record during deletion"))?;
        self.changes.insert(id, None);
        Ok(())
    }

    fn replace(&mut self, old: Option<Record>, new: Option<Record>) -> Result<()> {
        match (old, new) {
            (Some(old), Some(new)) => {
                if old.key() != new.key() {
                    return Err(corrupt("metadata record identity changed"));
                }
                if encode(&old)? != encode(&new)? {
                    self.put(new)?;
                }
            }
            (Some(old), None) => self.remove(old.key())?,
            (None, Some(new)) => self.put(new)?,
            (None, None) => {}
        }
        Ok(())
    }

    fn finish(self) -> (Changes, RecordIndex) {
        (self.changes, self.index)
    }
}

fn header(meta: &Metadata) -> Record {
    let mut header = meta.clone();
    header.inodes = Default::default();
    header.disks.clear();
    header.raw_pool.allocators.clear();
    header.checkpoint_member_catalog = None;
    if header.integrity.hash_scheme == Some(PAGED_INTEGRITY_SCHEME) {
        header.integrity.meta_hash.clear();
    }
    Record::Header(Box::new(header))
}

fn allocator_core(id: &str, alloc: &RawAllocatorState) -> Record {
    let mut core = alloc.clone();
    core.free_extents = Default::default();
    Record::Allocator(id.into(), core)
}

// Only changed values are serialized. Input order is checked to prevent a
// malformed legacy list from silently aliasing two persistent records.
fn diff<'a, K: Ord, V: PartialEq + 'a>(
    old: impl Iterator<Item = (K, &'a V)>,
    new: impl Iterator<Item = (K, &'a V)>,
    mut emit: impl FnMut(K, Option<&'a V>) -> Result<()>,
) -> Result<()> {
    let mut old = old.peekable();
    let mut new = new.peekable();
    loop {
        match (old.peek(), new.peek()) {
            (None, None) => return Ok(()),
            (Some((a, _)), Some((b, _))) if a == b => {
                let (_, previous) = old.next().unwrap();
                let (key, next) = new.next().unwrap();
                if previous != next {
                    emit(key, Some(next))?;
                }
            }
            (Some((a, _)), Some((b, _))) if a < b => {
                emit(old.next().unwrap().0, None)?;
            }
            (_, Some(_)) => {
                let (key, next) = new.next().unwrap();
                emit(key, Some(next))?;
            }
            (Some(_), None) => emit(old.next().unwrap().0, None)?,
        }
    }
}

fn inode_records(
    plan: &mut Plan,
    key: u64,
    old: Option<&Inode>,
    new: Option<&Inode>,
) -> Result<()> {
    if key == 0 || old.is_some_and(|i| i.id != key) || new.is_some_and(|i| i.id != key) {
        return Err(corrupt("invalid inode record identity"));
    }
    let core = |inode: &Inode| Record::Inode(Box::new(InodeCorePage::from(inode)));
    plan.replace(old.map(core), new.map(core))?;
    // Persistent ordered maps skip pointer-identical subtrees, so changing
    // one name in a large directory does not scan the whole directory.
    let empty = imbl::OrdMap::new();
    let old_entries = old.map_or(&empty, |i| &i.entries);
    let new_entries = new.map_or(&empty, |i| &i.entries);
    use imbl::ordmap::DiffItem;
    for change in old_entries.diff(new_entries) {
        match change {
            DiffItem::Add(name, ino)
            | DiffItem::Update {
                new: (name, ino), ..
            } => plan.put(Record::Dirent(key, name.clone(), *ino))?,
            DiffItem::Remove(name, _) => plan.remove(Key::Dirent(key, name.clone()))?,
        }
    }
    diff(
        old.into_iter().flat_map(|i| i.xattrs.iter()),
        new.into_iter().flat_map(|i| i.xattrs.iter()),
        |name, value| match value {
            Some(value) => plan.put(Record::Xattr(key, name.clone(), value.clone())),
            None => plan.remove(Key::Xattr(key, name.clone())),
        },
    )?;
    for inode in old.into_iter().chain(new) {
        if !inode
            .blocks
            .windows(2)
            .all(|pair| pair[0].raw_offset < pair[1].raw_offset)
        {
            return Err(corrupt(
                "file block mappings must be strictly ordered by offset",
            ));
        }
    }
    diff(
        old.into_iter()
            .flat_map(|i| i.blocks.iter())
            .map(|b| (b.raw_offset, b)),
        new.into_iter()
            .flat_map(|i| i.blocks.iter())
            .map(|b| (b.raw_offset, b)),
        |offset, value| match value {
            Some(block) => plan.put(Record::Block(key, Box::new(block.clone()))),
            None => plan.remove(Key::Block(key, offset)),
        },
    )
}

fn allocator_records(
    plan: &mut Plan,
    disk: &str,
    old: Option<&RawAllocatorState>,
    new: Option<&RawAllocatorState>,
) -> Result<()> {
    plan.replace(
        old.map(|a| allocator_core(disk, a)),
        new.map(|a| allocator_core(disk, a)),
    )?;
    if old
        .zip(new)
        .is_some_and(|(a, b)| a.free_extents.ptr_eq(&b.free_extents))
    {
        return Ok(());
    }
    for alloc in old.into_iter().chain(new) {
        if !alloc
            .free_extents
            .iter()
            .zip(alloc.free_extents.iter().skip(1))
            .all(|(a, b)| a.offset < b.offset)
        {
            return Err(corrupt(
                "allocator free extents must be strictly ordered by offset",
            ));
        }
    }
    diff(
        old.into_iter()
            .flat_map(|a| a.free_extents.iter())
            .map(|e| (e.offset, e)),
        new.into_iter()
            .flat_map(|a| a.free_extents.iter())
            .map(|e| (e.offset, e)),
        |offset, value| match value {
            Some(extent) => plan.put(Record::Free(disk.into(), extent.clone())),
            None => plan.remove(Key::Free(disk.into(), offset)),
        },
    )
}

pub(super) fn initial_records(meta: &Metadata) -> Result<(Changes, RecordIndex)> {
    let mut plan = Plan::new(RecordIndex::default());
    plan.put(header(meta))?;
    for (id, disk) in &meta.disks {
        if *id != disk.id {
            return Err(corrupt("disk ID mismatch"));
        }
        plan.put(Record::Disk(Box::new(disk.clone())))?;
    }
    for (id, alloc) in &meta.raw_pool.allocators {
        allocator_records(&mut plan, id, None, Some(alloc))?;
    }
    let mut keys = meta.inodes.keys().copied().collect::<Vec<_>>();
    keys.sort_unstable();
    for key in keys {
        inode_records(&mut plan, key, None, meta.inodes.get(&key))?;
    }
    Ok(plan.finish())
}

pub(super) fn plan_records(
    base: &Metadata,
    next: &Metadata,
    index: RecordIndex,
) -> Result<(Changes, RecordIndex)> {
    let mut plan = Plan::new(index);
    plan.put(header(next))?;
    for id in base
        .disks
        .keys()
        .chain(next.disks.keys())
        .collect::<BTreeSet<_>>()
    {
        let disk = |d: &Disk| Record::Disk(Box::new(d.clone()));
        if next.disks.get(id).is_some_and(|d| d.id != *id) {
            return Err(corrupt("disk ID mismatch"));
        }
        plan.replace(base.disks.get(id).map(disk), next.disks.get(id).map(disk))?;
    }
    for id in base
        .raw_pool
        .allocators
        .keys()
        .chain(next.raw_pool.allocators.keys())
        .collect::<BTreeSet<_>>()
    {
        allocator_records(
            &mut plan,
            id,
            base.raw_pool.allocators.get(id),
            next.raw_pool.allocators.get(id),
        )?;
    }
    for key in base.inodes.changed_candidates(&next.inodes) {
        inode_records(&mut plan, key, base.inodes.get(&key), next.inodes.get(&key))?;
    }
    Ok(plan.finish())
}

pub(super) fn decode_image(
    records: BTreeMap<u64, Vec<u8>>,
    pool: Uuid,
    root_hash: [u8; 32],
) -> Result<(Metadata, RecordIndex)> {
    let mut index = RecordIndex::default();
    let mut header = None;
    let mut cores = BTreeMap::new();
    let mut entries = BTreeMap::<u64, BTreeMap<String, u64>>::new();
    let mut xattrs = BTreeMap::<u64, BTreeMap<String, String>>::new();
    let mut blocks = BTreeMap::<u64, BTreeMap<u64, FileBlock>>::new();
    let mut disks = BTreeMap::new();
    let mut allocators = BTreeMap::new();
    let mut free = BTreeMap::<String, BTreeMap<u64, RawFreeExtent>>::new();
    for (id, bytes) in records {
        let record: Record = decode(&bytes)?;
        let key = record.key();
        if (id == 0) != (key == Key::Header) || index.ids.insert(key, id).is_some() {
            return Err(corrupt("duplicate or invalid metadata record identity"));
        }
        index.next = index.next.max(
            id.checked_add(1)
                .ok_or_else(|| corrupt("metadata record ID exhausted"))?,
        );
        match record {
            Record::Header(value) => header = Some(*value),
            Record::Inode(core) => {
                cores.insert(core.id, core);
            }
            Record::Dirent(ino, name, child) => {
                entries.entry(ino).or_default().insert(name, child);
            }
            Record::Xattr(ino, name, value) => {
                xattrs.entry(ino).or_default().insert(name, value);
            }
            Record::Block(ino, block) => {
                blocks
                    .entry(ino)
                    .or_default()
                    .insert(block.raw_offset, *block);
            }
            Record::Disk(disk) => {
                disks.insert(disk.id.clone(), *disk);
            }
            Record::Allocator(id, core) => {
                if !core.free_extents.is_empty() {
                    return Err(corrupt("allocator core contains embedded extents"));
                }
                allocators.insert(id, core);
            }
            Record::Free(id, extent) => {
                free.entry(id).or_default().insert(extent.offset, extent);
            }
        }
    }
    let mut meta = header.ok_or_else(|| corrupt("missing metadata image header"))?;
    if Uuid::parse_str(&meta.uuid).ok() != Some(pool)
        || !meta.inodes.is_empty()
        || !meta.disks.is_empty()
        || !meta.raw_pool.allocators.is_empty()
    {
        return Err(corrupt("invalid metadata image header"));
    }
    for (ino, core) in cores {
        if ino == 0 {
            return Err(corrupt("invalid inode zero"));
        }
        meta.inodes.insert(
            ino,
            core.into_inode(
                entries.remove(&ino).unwrap_or_default(),
                xattrs.remove(&ino).unwrap_or_default(),
                blocks
                    .remove(&ino)
                    .unwrap_or_default()
                    .into_values()
                    .collect(),
            ),
        );
    }
    if !entries.is_empty() || !xattrs.is_empty() || !blocks.is_empty() {
        return Err(corrupt("orphaned inode record"));
    }
    for (disk, alloc) in &mut allocators {
        alloc.free_extents = free
            .remove(disk)
            .unwrap_or_default()
            .into_values()
            .collect();
    }
    if !free.is_empty() {
        return Err(corrupt("orphaned allocator extent record"));
    }
    meta.disks = disks;
    meta.raw_pool.allocators = allocators;
    if meta.integrity.hash_scheme == Some(PAGED_INTEGRITY_SCHEME) {
        if !meta.integrity.meta_hash.is_empty() || meta.integrity.generation != meta.txid {
            return Err(corrupt("invalid paged integrity header"));
        }
        meta.integrity.meta_hash = hex::encode(root_hash);
    }
    meta.inodes.mark_committed();
    Ok((meta, index))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_directory_and_allocator_change_only_independent_records() {
        let dir = tempfile::tempdir().unwrap();
        let fs = crate::ArgosFs::create(
            dir.path(),
            VolumeConfig {
                k: 1,
                m: 0,
                ..Default::default()
            },
            1,
            false,
        )
        .unwrap();
        fs.write_file("/file", b"data", 0o600).unwrap();
        let mut base = fs.metadata_snapshot();
        let child = base.inodes[&1].entries["file"];
        for n in 0..50_000 {
            base.inodes
                .get_mut(&1)
                .unwrap()
                .entries
                .insert(format!("name-{n:05}"), child);
        }
        let mut alloc = RawAllocatorState {
            data_start: 4096,
            data_end: 1 << 30,
            block_size: 4096,
            next_offset: 1 << 29,
            ..Default::default()
        };
        alloc.free_extents = (0..10_000)
            .map(|n| RawFreeExtent {
                offset: 4096 + n * 8192,
                length: 4096,
            })
            .collect();
        base.raw_pool.allocators.insert("disk-0000".into(), alloc);
        let (_, index) = initial_records(&base).unwrap();
        base.inodes.mark_committed();
        let mut next = base.clone();
        next.txid += 1;
        next.inodes
            .get_mut(&1)
            .unwrap()
            .entries
            .remove("name-00000");
        next.inodes
            .get_mut(&1)
            .unwrap()
            .entries
            .insert("new-name".into(), child);
        next.raw_pool
            .allocators
            .get_mut("disk-0000")
            .unwrap()
            .free_extents
            .remove(0);
        let (changes, _) = plan_records(&base, &next, index).unwrap();
        assert_eq!(
            changes.len(),
            4,
            "header, removed dirent, inserted dirent, removed free extent"
        );
        assert_eq!(changes.values().filter(|value| value.is_none()).count(), 2);
        assert_eq!(base.inodes[&1].entries.len(), 50_001);
        assert_eq!(next.inodes[&1].entries.len(), 50_001);
    }
}
