//! Compatibility bridge for exercising real volume operations against the V3
//! engine before the runtime switches to disk-resident inode access. Mount
//! deliberately materializes a complete image. Never claim fast V3 mount
//! based on this adapter, and do not enable it as the production default.
use super::records::{decode_image, Record};
use super::{encode, Arena, CommitIo, CommitReport, V3_INTEGRITY_SCHEME};
use crate::backend::StorageBackend;
use crate::error::{ArgosError, Result};
use crate::metadata::store::InodeCorePage;
use crate::storage::raw::paged_metadata::{
    typed::{decode_record, encode_record, import_image_with_events, Domain},
    TypedChanges, TypedKey, TypedMetadata,
};
use crate::types::*;
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;
use uuid::Uuid;
mod delta;
use delta::Accounting;

fn corrupt(message: &str) -> ArgosError {
    ArgosError::CorruptedMetadata(message.into())
}

fn number(bytes: &[u8]) -> Result<u64> {
    Ok(u64::from_be_bytes(
        bytes
            .try_into()
            .map_err(|_| corrupt("invalid V3 numeric key"))?,
    ))
}

fn split_number(bytes: &[u8]) -> Result<(u64, &[u8])> {
    let (head, tail) = bytes
        .split_at_checked(8)
        .ok_or_else(|| corrupt("truncated V3 inode key"))?;
    Ok((number(head)?, tail))
}

fn split_disk(bytes: &[u8]) -> Result<(String, &[u8])> {
    let (len, bytes) = bytes
        .split_at_checked(2)
        .ok_or_else(|| corrupt("truncated V3 disk key"))?;
    let len = u16::from_be_bytes(len.try_into().unwrap()) as usize;
    let (id, tail) = bytes
        .split_at_checked(len)
        .ok_or_else(|| corrupt("truncated V3 disk identity"))?;
    let id = std::str::from_utf8(id)
        .map_err(|_| corrupt("invalid V3 disk identity"))?
        .to_owned();
    Ok((id, tail))
}

/// Decode the primary records using the existing volume semantic checks.
/// Compare every derived index against the re-encoded authoritative image.
/// Unknown domains are retained on disk and not overwritten by this bridge.
fn materialize(
    records: &BTreeMap<TypedKey, Vec<u8>>,
    pool: Uuid,
    root: [u8; 32],
) -> Result<(Metadata, Vec<PagedAuditEvent>)> {
    let mut numeric = BTreeMap::new();
    let mut events = Vec::new();
    let mut snapshots = RawSnapshotState::default();
    let mut next = 1u64;
    for (key, value) in records {
        let suffix = &key.as_bytes()[2..];
        let record = match key.domain() {
            d if d == Domain::Header as u16 => {
                if !suffix.is_empty() {
                    return Err(corrupt("invalid V3 header key"));
                }
                Record::Header(Box::new(decode_record(value)?))
            }
            d if d == Domain::Inode as u16 => {
                let core: InodeCorePage = decode_record(value)?;
                if core.id != number(suffix)? {
                    return Err(corrupt("V3 inode key/value mismatch"));
                }
                Record::Inode(Box::new(core))
            }
            d if d == Domain::Dirent as u16 => {
                let (ino, name) = split_number(suffix)?;
                let name = crate::volume::entry_name_from_os(std::ffi::OsStr::from_bytes(name))?;
                Record::Dirent(ino, name, decode_record(value)?)
            }
            d if d == Domain::Xattr as u16 => {
                let (ino, name) = split_number(suffix)?;
                let name =
                    std::str::from_utf8(name).map_err(|_| corrupt("invalid V3 xattr name"))?;
                Record::Xattr(ino, name.to_owned(), decode_record(value)?)
            }
            d if d == Domain::Block as u16 => {
                let (ino, offset) = split_number(suffix)?;
                let block: FileBlock = decode_record(value)?;
                if block.raw_offset != number(offset)? {
                    return Err(corrupt("V3 block key/value mismatch"));
                }
                Record::Block(ino, Box::new(block))
            }
            d if d == Domain::Disk as u16 => {
                let (id, tail) = split_disk(suffix)?;
                let disk: Disk = decode_record(value)?;
                if disk.id != id || !tail.is_empty() {
                    return Err(corrupt("V3 disk key/value mismatch"));
                }
                Record::Disk(Box::new(disk))
            }
            d if d == Domain::DataAllocator as u16 => {
                let (id, tail) = split_disk(suffix)?;
                if !tail.is_empty() {
                    return Err(corrupt("invalid V3 allocator key"));
                }
                Record::Allocator(id, decode_record(value)?)
            }
            d if d == Domain::DataFree as u16 => {
                let (id, tail) = split_disk(suffix)?;
                let extent: RawFreeExtent = decode_record(value)?;
                if extent.offset != number(tail)? {
                    return Err(corrupt("V3 free extent key/value mismatch"));
                }
                Record::Free(id, extent)
            }
            d if d == Domain::Event as u16 => {
                let event: PagedAuditEvent = decode_record(value)?;
                if event.sequence != number(suffix)? {
                    return Err(corrupt("V3 event key/value mismatch"));
                }
                events.push(event.clone());
                Record::Event(Box::new(event))
            }
            d if d == Domain::Snapshot as u16 => {
                let name = std::str::from_utf8(suffix)
                    .map_err(|_| corrupt("invalid snapshot name"))?
                    .to_owned();
                super::super::snapshot_key(&name)?;
                super::super::SnapshotInfo::decode(value)?;
                snapshots.names.insert(name);
                continue;
            }
            d if d == Domain::SnapshotPin as u16 => {
                let (id, tail) = split_disk(suffix)?;
                let pin: RawSnapshotPin = decode_record(value)?;
                if pin.extent.disk_id != id
                    || pin.extent.offset != number(tail)?
                    || pin.references == 0
                {
                    return Err(corrupt("invalid V3 snapshot pin"));
                }
                let mut disk = snapshots.pins.get(&id).cloned().unwrap_or_default();
                disk.insert(pin.extent.offset, pin);
                snapshots.pins.insert(id, disk);
                continue;
            }
            _ => continue,
        };
        let id = if key.domain() == Domain::Header as u16 {
            0
        } else {
            let id = next;
            next = next
                .checked_add(1)
                .ok_or_else(|| corrupt("V3 materialization ID exhausted"))?;
            id
        };
        numeric.insert(id, encode(&record)?);
    }
    let (mut meta, _) = decode_image(numeric, pool, root)?;
    if meta.integrity.hash_scheme != Some(V3_INTEGRITY_SCHEME)
        || !meta.integrity.meta_hash.is_empty()
        || meta.integrity.generation != meta.txid
    {
        return Err(corrupt("invalid V3 volume integrity header"));
    }
    if meta.raw_pool.snapshot_state.is_some() {
        return Err(corrupt("V3 header contains embedded snapshot state"));
    }
    if snapshots.names.is_empty() {
        if !snapshots.pins.is_empty() {
            return Err(corrupt("orphan V3 snapshot pins"));
        }
    } else {
        let count = snapshots.names.len() as u64;
        if snapshots
            .pins
            .values()
            .flat_map(|pins| pins.values())
            .any(|pin| pin.references > count)
        {
            return Err(corrupt("V3 snapshot pin count exceeds named snapshots"));
        }
        meta.raw_pool.snapshot_state = Some(snapshots);
        validate_snapshot_allocations(&meta)?;
    }
    let expected = import_image_with_events(&meta, &events)?;
    for (key, value) in &expected {
        // Primary records were already authenticated and decoded. Do not
        // require byte-identical reserialization of floating-point fields.
        let derived = matches!(
            key.domain(),
            6 | 10 | 11 | 12 | 15 | 16 | super::super::SNAPSHOT_PIN_DOMAIN
        );
        if !records.contains_key(key) || (derived && records.get(key) != value.as_ref()) {
            return Err(ArgosError::CorruptedMetadata(format!(
                "V3 derived index or primary record mismatch: domain {}",
                key.domain()
            )));
        }
    }
    for key in records.keys() {
        if (17..=23).contains(&key.domain()) {
            return Err(ArgosError::IncompatibleFormat(
                "final snapshot object domains are not supported by extent-pin prototype".into(),
            ));
        }
        // All known primary and derived domains except the native snapshot
        // descriptors are represented by this materialized view.
        if ((1..=16).contains(&key.domain()) || key.domain() == super::super::SNAPSHOT_PIN_DOMAIN)
            && key.domain() != Domain::Snapshot as u16
            && !expected.contains_key(key)
        {
            return Err(corrupt("unexpected record in V3 volume domain"));
        }
    }
    meta.integrity.meta_hash = hex::encode(root);
    Ok((meta, events))
}

type ExtentIndex = BTreeMap<(String, u64), PhysicalExtent>;

fn extent_index(meta: &Metadata) -> Result<ExtentIndex> {
    let mut out = ExtentIndex::new();
    for shard in meta
        .inodes
        .values()
        .flat_map(|inode| inode.blocks.iter())
        .flat_map(|block| block.shards.iter())
    {
        let Some(ShardLocation::RawExtent(extent)) = &shard.location else {
            return Err(corrupt("snapshot data shard has no raw extent"));
        };
        if extent.disk_id != shard.disk_id || extent.length == 0 {
            return Err(corrupt("invalid snapshot extent identity"));
        }
        let key = (extent.disk_id.clone(), extent.offset);
        if let Some(old) = out.insert(key, extent.clone()) {
            if old != *extent {
                return Err(corrupt("conflicting snapshot extent generation"));
            }
        }
    }
    Ok(out)
}

fn pin_extent(state: &mut RawSnapshotState, extent: PhysicalExtent) -> Result<()> {
    let mut pins = state.pins.get(&extent.disk_id).cloned().unwrap_or_default();
    if let Some(pin) = pins.get_mut(&extent.offset) {
        if pin.extent != extent {
            return Err(corrupt("snapshot pin physical generation mismatch"));
        }
        pin.references = pin
            .references
            .checked_add(1)
            .ok_or_else(|| corrupt("snapshot reference count overflow"))?;
    } else {
        pins.insert(
            extent.offset,
            RawSnapshotPin {
                extent: extent.clone(),
                references: 1,
            },
        );
    }
    state.pins.insert(extent.disk_id.clone(), pins);
    Ok(())
}

fn validate_snapshot_allocations(meta: &Metadata) -> Result<()> {
    let mut extents = extent_index(meta)?;
    if let Some(state) = &meta.raw_pool.snapshot_state {
        for pin in state.pins.values().flat_map(|pins| pins.values()) {
            let extent = &pin.extent;
            if let Some(old) =
                extents.insert((extent.disk_id.clone(), extent.offset), extent.clone())
            {
                if old != *extent {
                    return Err(corrupt("snapshot and live extents disagree"));
                }
            }
        }
    }
    let mut by_disk = BTreeMap::<String, Vec<PhysicalExtent>>::new();
    for (_, extent) in extents {
        let alloc = meta
            .raw_pool
            .allocators
            .get(&extent.disk_id)
            .ok_or_else(|| corrupt("snapshot extent has no allocator"))?;
        if extent
            .offset
            .checked_add(extent.length)
            .is_none_or(|end| end > alloc.next_offset)
        {
            return Err(corrupt("snapshot extent above allocation high-water mark"));
        }
        by_disk
            .entry(extent.disk_id.clone())
            .or_default()
            .push(extent);
    }
    for (id, extents) in by_disk {
        crate::storage::raw::allocator::validate_allocations(
            &meta.raw_pool.allocators[&id],
            extents,
        )?;
    }
    Ok(())
}

fn bounded_events(events: &[PagedAuditEvent], txid: u64) -> Result<Vec<PagedAuditEvent>> {
    let mut out = events.to_vec();
    out.sort_by_key(|event| event.sequence);
    if out
        .iter()
        .any(|event| event.sequence == 0 || event.txid > txid)
        || out
            .windows(2)
            .any(|pair| pair[0].sequence == pair[1].sequence)
    {
        return Err(corrupt("invalid V3 event history"));
    }
    let mut total = 0usize;
    for event in &out {
        total += serde_json::to_vec(event)?.len();
    }
    while out.len() > 512 || total > 256 * 1024 {
        total -= serde_json::to_vec(&out.remove(0))?.len();
    }
    Ok(out)
}

pub(super) struct V3ImageStore {
    tree: TypedMetadata,
    pool: Uuid,
    base: Metadata,
    events: Vec<PagedAuditEvent>,
    accounting: Accounting,
}

impl V3ImageStore {
    pub(super) fn format(
        backend: Arc<dyn StorageBackend>,
        arenas: Vec<Arena>,
        metadata: &mut Metadata,
        events: &[PagedAuditEvent],
        selected: Option<&BTreeSet<String>>,
    ) -> Result<Self> {
        let pool = Uuid::parse_str(&metadata.uuid)
            .map_err(|_| ArgosError::Invalid("invalid V3 pool UUID".into()))?;
        if metadata.raw_pool.snapshot_state.is_some() {
            return Err(ArgosError::Unsupported(
                "V3 snapshot DAG cannot yet be restaged by volume topology".into(),
            ));
        }
        let events = bounded_events(events, metadata.txid)?;
        let accounting = Accounting::from_image(metadata)?;
        let records = import_image_with_events(metadata, &events)?;
        let mut tree = TypedMetadata::format_selected(backend, *pool.as_bytes(), arenas, selected)?;
        let report = tree.commit(&records)?;
        metadata.integrity.meta_hash = hex::encode(report.root_hash);
        metadata.inodes.mark_committed();
        Ok(Self {
            tree,
            pool,
            base: metadata.clone(),
            events,
            accounting,
        })
    }

    pub(super) fn transfer_with_events(
        backend: Arc<dyn StorageBackend>,
        old_arenas: Vec<Arena>,
        new_arenas: Vec<Arena>,
        base: &Metadata,
        next: &mut Metadata,
        events: &[PagedAuditEvent],
        selected: &BTreeSet<String>,
    ) -> Result<Self> {
        let pool = Uuid::parse_str(&base.uuid)
            .map_err(|_| ArgosError::Invalid("invalid V3 pool UUID".into()))?;
        let mut source = Self::open(backend.clone(), pool, old_arenas)?;
        if source.base.txid != base.txid
            || source.base.integrity.meta_hash != base.integrity.meta_hash
            || source.base.raw_pool.snapshot_state != next.raw_pool.snapshot_state
        {
            return Err(ArgosError::Conflict(
                "V3 transfer source or snapshot ownership changed".into(),
            ));
        }
        validate_snapshot_allocations(next)?;
        let mut tree =
            TypedMetadata::format_selected(backend, *pool.as_bytes(), new_arenas, Some(selected))?;
        tree.transfer_from(&mut source.tree)?;
        let events = bounded_events(events, next.txid)?;
        let (changes, accounting) = delta::plan(
            &source.base,
            next,
            &source.accounting,
            &source.events,
            &events,
        )?;
        let report = tree.commit_allowed(&changes, Some(selected))?;
        next.integrity.meta_hash = hex::encode(report.root_hash);
        next.inodes.mark_committed();
        Ok(Self {
            tree,
            pool,
            base: next.clone(),
            events,
            accounting,
        })
    }

    pub(super) fn open(
        backend: Arc<dyn StorageBackend>,
        pool: Uuid,
        arenas: Vec<Arena>,
    ) -> Result<Self> {
        let tree = TypedMetadata::open(backend, *pool.as_bytes(), arenas)?;
        let records = tree.records()?;
        let (base, events) = materialize(&records, pool, tree.root_hash())?;
        if let Some(state) = &base.raw_pool.snapshot_state {
            let mut expected = RawSnapshotState::default();
            for name in &state.names {
                let historical = Self::read_snapshot_image(&tree, pool, name)?;
                for extent in extent_index(&historical)?.into_values() {
                    pin_extent(&mut expected, extent)?;
                }
                expected.names.insert(name.clone());
            }
            if expected != *state {
                return Err(corrupt(
                    "persisted snapshot pins disagree with historical roots",
                ));
            }
        }
        let accounting = Accounting::from_image(&base)?;
        Ok(Self {
            tree,
            pool,
            base,
            events,
            accounting,
        })
    }

    pub(super) fn recover(
        backend: &dyn StorageBackend,
        pool: Uuid,
        arenas: Vec<Arena>,
    ) -> Result<(Metadata, Vec<PagedAuditEvent>)> {
        let (records, root) =
            super::super::recover_records::<TypedKey>(backend, *pool.as_bytes(), arenas)?;
        materialize(&records, pool, root)
    }

    pub(super) fn snapshot(&self) -> Metadata {
        self.base.clone()
    }
    pub(super) fn events(&self) -> Vec<PagedAuditEvent> {
        self.events.clone()
    }
    pub(super) fn root_hash(&self) -> [u8; 32] {
        self.tree.root_hash()
    }
    pub(super) fn confirm_all(&mut self, count: usize) -> Result<()> {
        self.tree.confirm_all(count)
    }
    pub(super) fn repair_stale(
        &mut self,
    ) -> Result<BTreeMap<String, std::result::Result<CommitIo, String>>> {
        self.tree.repair_stale()
    }

    fn next_events(
        &self,
        txid: u64,
        action: &str,
        mut details: serde_json::Value,
        advisory: bool,
    ) -> Result<Vec<PagedAuditEvent>> {
        use sha2::{Digest, Sha256};
        if action.len() > 1024 {
            return Err(ArgosError::Invalid("V3 event action too long".into()));
        }
        let bytes = serde_json::to_vec(&details)?;
        if bytes.len() > 16 * 1024 {
            details = serde_json::json!({"omitted_bytes":bytes.len(),"sha256":hex::encode(Sha256::digest(&bytes))});
        }
        let mut events = self.events.clone();
        let sequence = events
            .last()
            .map_or(Some(1), |e| e.sequence.checked_add(1))
            .ok_or_else(|| corrupt("V3 event sequence exhausted"))?;
        events.push(PagedAuditEvent {
            sequence,
            txid,
            time: crate::util::now_f64(),
            action: action.into(),
            details,
            advisory,
        });
        bounded_events(&events, txid)
    }

    fn read_snapshot_image(tree: &TypedMetadata, pool: Uuid, name: &str) -> Result<Metadata> {
        use super::super::{ScanOrder, ScanWindow};
        use std::ops::Bound;
        let info = tree
            .snapshot_info(name)?
            .ok_or_else(|| ArgosError::NotFound(name.into()))?;
        let mut records = BTreeMap::new();
        let mut window = ScanWindow {
            start: Bound::Unbounded,
            end: Bound::Unbounded,
            max_records: 256,
            max_bytes: super::super::wire::MAX_VALUE + 4096,
            order: ScanOrder::Ascending,
        };
        loop {
            let page = tree.scan_snapshot(name, &window)?;
            if page.more {
                window.start = Bound::Excluded(
                    page.records
                        .last()
                        .ok_or_else(|| corrupt("snapshot scan made no progress"))?
                        .0
                        .clone(),
                );
            }
            records.extend(page.records);
            if !page.more {
                break;
            }
        }
        let (meta, _) = materialize(&records, pool, info.root_hash)?;
        if meta.raw_pool.snapshot_state.is_some() {
            return Err(corrupt("historical snapshot contains live pin index"));
        }
        Ok(meta)
    }

    pub(super) fn snapshot_image(&self, name: &str) -> Result<Metadata> {
        Self::read_snapshot_image(&self.tree, self.pool, name)
    }

    pub(super) fn commit_snapshot(
        &mut self,
        next: &mut Metadata,
        allowed: &BTreeSet<String>,
        name: &str,
        create: bool,
    ) -> Result<CommitReport> {
        use super::super::{snapshot_key, SnapshotEdit};
        let key = snapshot_key(name)?;
        if Uuid::parse_str(&next.uuid).ok() != Some(self.pool)
            || next.txid <= self.base.txid
            || next.integrity.hash_scheme != Some(V3_INTEGRITY_SCHEME)
        {
            return Err(ArgosError::Invalid(
                "snapshot transaction must advance its pool generation".into(),
            ));
        }
        let mut state = self
            .base
            .raw_pool
            .snapshot_state
            .clone()
            .unwrap_or_default();
        if next.raw_pool.snapshot_state != self.base.raw_pool.snapshot_state {
            return Err(ArgosError::Conflict(
                "snapshot state changed outside snapshot transaction".into(),
            ));
        }
        let edit = if create {
            if state.names.contains(name) || self.tree.snapshot_info(name)?.is_some() {
                return Err(ArgosError::AlreadyExists(name.into()));
            }
            // The snapshot captures the certified base, not uncommitted
            // namespace writes. The caller flushed the dirty live state.
            if extent_index(next)? != extent_index(&self.base)? {
                return Err(ArgosError::Conflict(
                    "snapshot creation requires a committed data mapping".into(),
                ));
            }
            for extent in extent_index(&self.base)?.into_values() {
                pin_extent(&mut state, extent)?;
            }
            state.names.insert(name.into());
            SnapshotEdit::Create(key)
        } else {
            if !state.names.contains(name) {
                return Err(ArgosError::NotFound(name.into()));
            }
            let old = self.snapshot_image(name)?;
            let live = extent_index(next)?;
            for extent in extent_index(&old)?.into_values() {
                let mut pins = state
                    .pins
                    .get(&extent.disk_id)
                    .cloned()
                    .ok_or_else(|| corrupt("missing snapshot pin disk"))?;
                let pin = pins
                    .get_mut(&extent.offset)
                    .ok_or_else(|| corrupt("missing snapshot extent pin"))?;
                if pin.extent != extent || pin.references == 0 {
                    return Err(corrupt("snapshot pin identity changed"));
                }
                pin.references -= 1;
                if pin.references == 0 {
                    pins.remove(&extent.offset);
                    match live.get(&(extent.disk_id.clone(), extent.offset)) {
                        Some(active) if *active != extent => {
                            return Err(corrupt("live allocation reused pinned extent"))
                        }
                        Some(_) => {}
                        None => {
                            let allocator = next
                                .raw_pool
                                .allocators
                                .get_mut(&extent.disk_id)
                                .ok_or_else(|| corrupt("missing snapshot allocator"))?;
                            crate::storage::raw::allocator::free(allocator, &extent)?;
                            let disk = next
                                .disks
                                .get_mut(&extent.disk_id)
                                .ok_or_else(|| corrupt("missing snapshot disk"))?;
                            disk.used_bytes = disk.used_bytes.saturating_sub(extent.length);
                        }
                    }
                }
                if pins.is_empty() {
                    state.pins.remove(&extent.disk_id);
                } else {
                    state.pins.insert(extent.disk_id.clone(), pins);
                }
            }
            state.names.remove(name);
            SnapshotEdit::Delete(key)
        };
        if state.names.is_empty() {
            if !state.pins.is_empty() {
                return Err(corrupt("pins remain after last snapshot deletion"));
            }
            next.raw_pool.snapshot_state = None;
        } else {
            next.raw_pool.snapshot_state = Some(state);
        }
        validate_snapshot_allocations(next)?;
        let events = self.next_events(
            next.txid,
            if create {
                "snapshot-create"
            } else {
                "snapshot-delete"
            },
            serde_json::json!({"name":name}),
            false,
        )?;
        let (changes, accounting) =
            delta::plan(&self.base, next, &self.accounting, &self.events, &events)?;
        let report = self
            .tree
            .commit_with_snapshot(&changes, Some(allowed), Some(&edit))?;
        next.integrity.meta_hash = hex::encode(report.root_hash);
        next.inodes.mark_committed();
        self.base = next.clone();
        self.accounting = accounting;
        self.events = events;
        Ok(report)
    }

    pub(super) fn commit_allowed(
        &mut self,
        next: &mut Metadata,
        allowed: Option<&BTreeSet<String>>,
        event: Option<(&str, serde_json::Value)>,
    ) -> Result<CommitReport> {
        if Uuid::parse_str(&next.uuid).ok() != Some(self.pool)
            || next.txid <= self.base.txid
            || next.integrity.hash_scheme != Some(V3_INTEGRITY_SCHEME)
        {
            return Err(ArgosError::Invalid(
                "V3 image must advance its pool transaction".into(),
            ));
        }
        if next.raw_pool.snapshot_state != self.base.raw_pool.snapshot_state {
            return Err(ArgosError::Conflict(
                "snapshot state changed outside snapshot transaction".into(),
            ));
        }
        let events = match event {
            Some((action, details)) => self.next_events(next.txid, action, details, false)?,
            None => self.events.clone(),
        };
        let (changes, accounting) =
            delta::plan(&self.base, next, &self.accounting, &self.events, &events)?;
        let report = self.tree.commit_allowed(&changes, allowed)?;
        next.integrity.meta_hash = hex::encode(report.root_hash);
        next.inodes.mark_committed();
        self.base = next.clone();
        self.accounting = accounting;
        self.events = events;
        Ok(report)
    }

    pub(super) fn commit_event(
        &mut self,
        allowed: &BTreeSet<String>,
        action: &str,
        details: serde_json::Value,
    ) -> Result<CommitReport> {
        let events = self.next_events(self.base.txid, action, details, true)?;
        let changes = delta::event_changes(&self.events, &events)?;
        let report = self.tree.commit_allowed(&changes, Some(allowed))?;
        self.events = events;
        self.base.integrity.meta_hash = hex::encode(report.root_hash);
        Ok(report)
    }
}
