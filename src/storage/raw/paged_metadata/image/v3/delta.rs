//! Incremental direct-key planning for the materialized compatibility
//! adapter. Reuse the existing persistent-map diff for primary records,
//! including wide directories, file offsets and free extents.
use super::super::records::{plan_primary_changes, Key, RecordSink};
use super::*;
use crate::storage::raw::paged_metadata::typed::{QuotaRecord, VolumeStatistics};
use crate::volume::QuotaRuntime;

fn member_keys(ino: u64, block: &FileBlock) -> Result<BTreeSet<TypedKey>> {
    block
        .shards
        .iter()
        .map(|s| Domain::member_block(&s.disk_id, ino, block.raw_offset))
        .collect()
}

fn set(changes: &mut TypedChanges, key: TypedKey, value: &impl serde::Serialize) -> Result<()> {
    changes.insert(key, Some(encode_record(value)?));
    Ok(())
}

struct Sink<'a> {
    base: &'a Metadata,
    changes: TypedChanges,
}

impl Sink<'_> {
    fn old_block(&self, ino: u64, offset: u64) -> Result<Option<&FileBlock>> {
        match self.base.inodes.get(&ino) {
            Some(inode) => Ok(inode
                .blocks
                .offset_index()
                .ok_or_else(|| corrupt("invalid base block index"))?
                .get(&offset)),
            None => Ok(None),
        }
    }

    fn dirent(&mut self, parent: u64, name: String, child: Option<u64>) -> Result<()> {
        let binary = crate::volume::decode_entry_name_bytes(&name);
        let key = Domain::Dirent.inode_name(parent, &binary)?;
        let old = self
            .base
            .inodes
            .get(&parent)
            .and_then(|i| i.entries.get(&name))
            .copied();
        if let Some(old) = old {
            self.changes
                .insert(Domain::parent(old, parent, &binary)?, None);
        }
        if let Some(child) = child {
            set(
                &mut self.changes,
                Domain::parent(child, parent, &binary)?,
                &(),
            )?;
            set(&mut self.changes, key, &child)?;
        } else {
            self.changes.insert(key, None);
        }
        Ok(())
    }

    fn block(&mut self, ino: u64, offset: u64, next: Option<&FileBlock>) -> Result<()> {
        let old = self
            .old_block(ino, offset)?
            .map(|block| member_keys(ino, block))
            .transpose()?
            .unwrap_or_default();
        let new = next
            .map(|block| member_keys(ino, block))
            .transpose()?
            .unwrap_or_default();
        for key in old.difference(&new) {
            self.changes.insert(key.clone(), None);
        }
        if let Some(block) = next {
            for key in new {
                set(&mut self.changes, key, &block.stripe_id)?;
            }
            set(
                &mut self.changes,
                Domain::Block.inode_offset(ino, offset),
                block,
            )?;
        } else {
            self.changes
                .insert(Domain::Block.inode_offset(ino, offset), None);
        }
        Ok(())
    }
}

impl RecordSink for Sink<'_> {
    fn put(&mut self, record: Record) -> Result<()> {
        match record {
            Record::Header(value) => set(&mut self.changes, Domain::Header.prefix(), &value),
            Record::Inode(value) => {
                let was_file = self
                    .base
                    .inodes
                    .get(&value.id)
                    .is_some_and(|i| i.kind == NodeKind::File);
                if value.kind == NodeKind::File {
                    if !was_file {
                        set(
                            &mut self.changes,
                            Domain::FileIndex.inode_prefix(value.id),
                            &(),
                        )?;
                    }
                } else if was_file {
                    self.changes
                        .insert(Domain::FileIndex.inode_prefix(value.id), None);
                }
                set(
                    &mut self.changes,
                    Domain::Inode.inode_prefix(value.id),
                    &value,
                )
            }
            Record::Dirent(ino, name, child) => self.dirent(ino, name, Some(child)),
            Record::Xattr(ino, name, value) => set(
                &mut self.changes,
                Domain::Xattr.inode_name(ino, name.as_bytes())?,
                &value,
            ),
            Record::Block(ino, block) => self.block(ino, block.raw_offset, Some(&block)),
            Record::Disk(value) => set(
                &mut self.changes,
                Domain::Disk.disk(&value.id, None)?,
                &value,
            ),
            Record::Allocator(id, value) => set(
                &mut self.changes,
                Domain::DataAllocator.disk(&id, None)?,
                &value,
            ),
            Record::Free(id, extent) => set(
                &mut self.changes,
                Domain::DataFree.disk(&id, Some(extent.offset))?,
                &extent,
            ),
            Record::Event(event) => set(
                &mut self.changes,
                Domain::Event.inode_prefix(event.sequence),
                &event,
            ),
        }
    }

    fn remove(&mut self, key: Key) -> Result<()> {
        let key = match key {
            Key::Header => return Err(corrupt("cannot delete V3 volume header")),
            Key::Inode(ino) => {
                if self
                    .base
                    .inodes
                    .get(&ino)
                    .is_some_and(|i| i.kind == NodeKind::File)
                {
                    self.changes
                        .insert(Domain::FileIndex.inode_prefix(ino), None);
                }
                Domain::Inode.inode_prefix(ino)
            }
            Key::Dirent(ino, name) => return self.dirent(ino, name, None),
            Key::Block(ino, offset) => return self.block(ino, offset, None),
            Key::Xattr(ino, name) => Domain::Xattr.inode_name(ino, name.as_bytes())?,
            Key::Disk(id) => Domain::Disk.disk(&id, None)?,
            Key::Allocator(id) => Domain::DataAllocator.disk(&id, None)?,
            Key::Free(id, offset) => Domain::DataFree.disk(&id, Some(offset))?,
            Key::Event(seq) => Domain::Event.inode_prefix(seq),
        };
        self.changes.insert(key, None);
        Ok(())
    }
}

pub(super) struct Accounting {
    stats: VolumeStatistics,
    quotas: QuotaRuntime,
}

fn adjust(stats: &mut VolumeStatistics, inode: &Inode, add: bool) -> Result<()> {
    let count = |old: u64| {
        if add {
            old.checked_add(1)
        } else {
            old.checked_sub(1)
        }
    };
    stats.inodes =
        count(stats.inodes).ok_or_else(|| corrupt("V3 inode count overflow or underflow"))?;
    if inode.kind == NodeKind::File {
        stats.files =
            count(stats.files).ok_or_else(|| corrupt("V3 file count overflow or underflow"))?;
        stats.logical_bytes = if add {
            stats.logical_bytes.checked_add(u128::from(inode.size))
        } else {
            stats.logical_bytes.checked_sub(u128::from(inode.size))
        }
        .ok_or_else(|| corrupt("V3 logical byte count overflow or underflow"))?;
    }
    Ok(())
}

impl Accounting {
    pub(super) fn from_image(meta: &Metadata) -> Result<Self> {
        let mut stats = VolumeStatistics::default();
        for inode in meta.inodes.values() {
            adjust(&mut stats, inode, true)?;
        }
        Ok(Self {
            stats,
            quotas: QuotaRuntime::from_metadata(meta)?,
        })
    }
}

fn quota_records(meta: &Metadata, quotas: &QuotaRuntime) -> Result<TypedChanges> {
    let mut out = TypedChanges::new();
    for (&ino, &used) in &quotas.usage_bytes {
        let limit = meta
            .inodes
            .get(&ino)
            .and_then(|i| i.quota_bytes)
            .ok_or_else(|| corrupt("missing V3 quota limit"))?;
        set(
            &mut out,
            Domain::Quota.inode_prefix(ino),
            &QuotaRecord { limit, used },
        )?;
    }
    for (&ino, scopes) in &quotas.memberships {
        for (&quota, &count) in scopes {
            set(
                &mut out,
                Domain::QuotaMembership.inode_offset(ino, quota),
                &count,
            )?;
        }
    }
    Ok(out)
}

pub(super) fn event_changes(
    old: &[PagedAuditEvent],
    new: &[PagedAuditEvent],
) -> Result<TypedChanges> {
    let old: BTreeMap<_, _> = old.iter().map(|e| (e.sequence, e)).collect();
    let new: BTreeMap<_, _> = new.iter().map(|e| (e.sequence, e)).collect();
    let mut changes = TypedChanges::new();
    for (&seq, event) in &new {
        if !old.contains_key(&seq) {
            set(&mut changes, Domain::Event.inode_prefix(seq), event)?;
        }
    }
    for seq in old.keys() {
        if !new.contains_key(seq) {
            changes.insert(Domain::Event.inode_prefix(*seq), None);
        }
    }
    Ok(changes)
}

pub(super) fn plan(
    base: &Metadata,
    next: &Metadata,
    accounting: &Accounting,
    old_events: &[PagedAuditEvent],
    new_events: &[PagedAuditEvent],
) -> Result<(TypedChanges, Accounting)> {
    let mut sink = Sink {
        base,
        changes: TypedChanges::new(),
    };
    plan_primary_changes(base, next, &mut sink)?;
    let candidates = base.inodes.changed_candidates(&next.inodes);
    let mut stats = accounting.stats.clone();
    let mut quota_changed = false;
    for ino in &candidates {
        let old = base.inodes.get(ino);
        let new = next.inodes.get(ino);
        if let Some(old) = old {
            adjust(&mut stats, old, false)?;
        }
        if let Some(new) = new {
            adjust(&mut stats, new, true)?;
        }
        quota_changed |= old.and_then(|i| i.quota_bytes) != new.and_then(|i| i.quota_bytes);
        if !accounting.quotas.usage_bytes.is_empty() {
            // Quota scopes are still produced by the materialized runtime.
            // Only topology, file size and quota-limit changes need rebuild.
            quota_changed |= match (old, new) {
                (Some(a), Some(b)) => {
                    a.kind != b.kind || a.size != b.size || !a.entries.ptr_eq(&b.entries)
                }
                _ => true,
            };
        }
    }
    if stats != accounting.stats {
        set(&mut sink.changes, Domain::Statistics.prefix(), &stats)?;
    }
    let quotas = if quota_changed {
        let new = QuotaRuntime::from_metadata(next)?;
        let old_records = quota_records(base, &accounting.quotas)?;
        let new_records = quota_records(next, &new)?;
        for (key, value) in &new_records {
            if old_records.get(key) != Some(value) {
                sink.changes.insert(key.clone(), value.clone());
            }
        }
        for key in old_records.keys() {
            if !new_records.contains_key(key) {
                sink.changes.insert(key.clone(), None);
            }
        }
        new
    } else {
        accounting.quotas.clone()
    };
    sink.changes.extend(event_changes(old_events, new_events)?);
    Ok((sink.changes, Accounting { stats, quotas }))
}
