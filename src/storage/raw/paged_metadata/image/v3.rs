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
    let expected = import_image_with_events(&meta, &events)?;
    for (key, value) in &expected {
        // Primary records were already authenticated and decoded. Do not
        // require byte-identical reserialization of floating-point fields.
        let derived = matches!(key.domain(), 6 | 10 | 11 | 12 | 15 | 16);
        if !records.contains_key(key) || (derived && records.get(key) != value.as_ref()) {
            return Err(ArgosError::CorruptedMetadata(format!(
                "V3 derived index or primary record mismatch: domain {}",
                key.domain()
            )));
        }
    }
    for key in records.keys() {
        // All current schema domains except snapshots (not supported yet)
        // are owned by this materialized view.
        if (1..=16).contains(&key.domain()) && !expected.contains_key(key) {
            return Err(corrupt("unexpected record in V3 volume domain"));
        }
    }
    meta.integrity.meta_hash = hex::encode(root);
    Ok((meta, events))
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

    pub(super) fn open(
        backend: Arc<dyn StorageBackend>,
        pool: Uuid,
        arenas: Vec<Arena>,
    ) -> Result<Self> {
        let tree = TypedMetadata::open(backend, *pool.as_bytes(), arenas)?;
        let records = tree.records()?;
        let (base, events) = materialize(&records, pool, tree.root_hash())?;
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
