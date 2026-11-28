//! Typed bridge between the authenticated page store and ArgosFS Metadata.
//! The inode table is never serialized as a single JSON image. Header fields
//! and each individual inode are separately compressed records.
use super::{Arena, Changes, CommitIo, CommitReport, PagedMetadata};
use crate::backend::StorageBackend;
use crate::error::{ArgosError, Result};
use crate::types::{Metadata, PagedAuditEvent};
use serde::{de::DeserializeOwned, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use uuid::Uuid;

pub const PAGED_INTEGRITY_SCHEME: u32 = 3;
pub const V3_INTEGRITY_SCHEME: u32 = 4;
const MAX_EXPANDED_RECORD: usize = 128 * 1024 * 1024;
const JSON: u8 = 0;
const LZ4: u8 = 1;

fn encode(value: &impl Serialize) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_EXPANDED_RECORD {
        return Err(ArgosError::FileTooLarge(
            "paged metadata image record".into(),
        ));
    }
    let compressed = lz4_flex::compress_prepend_size(&bytes);
    let (tag, payload) = if compressed.len() < bytes.len() {
        (LZ4, compressed)
    } else {
        (JSON, bytes)
    };
    let mut out = Vec::with_capacity(payload.len() + 1);
    out.push(tag);
    out.extend(payload);
    Ok(out)
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let (tag, payload) = bytes
        .split_first()
        .ok_or_else(|| super::wire::corrupt("empty metadata image record"))?;
    match *tag {
        JSON if payload.len() <= MAX_EXPANDED_RECORD => Ok(serde_json::from_slice(payload)?),
        LZ4 => {
            let size = payload
                .get(..4)
                .map(|prefix| u32::from_le_bytes(prefix.try_into().unwrap()) as usize)
                .filter(|&len| len <= MAX_EXPANDED_RECORD)
                .ok_or_else(|| super::wire::corrupt("invalid metadata image expanded size"))?;
            let expanded = lz4_flex::decompress_size_prepended(payload).map_err(|err| {
                super::wire::corrupt(format!("invalid compressed metadata image: {err}"))
            })?;
            if expanded.len() != size {
                return Err(super::wire::corrupt("metadata image size mismatch"));
            }
            Ok(serde_json::from_slice(&expanded)?)
        }
        _ => Err(super::wire::corrupt(
            "unknown or oversized metadata image record",
        )),
    }
}

mod records;
mod v3;
use records::{
    append_event, decode_image, import_events, initial_records, plan_records, RecordIndex,
};
use v3::V3ImageStore;

/// Typed metadata image on dedicated authenticated page arenas. Each
/// directory entry, block mapping and free extent has an independent record.
struct NumericImageStore {
    tree: PagedMetadata,
    pool: Uuid,
    base: Metadata,
    index: RecordIndex,
}

impl NumericImageStore {
    pub fn format(
        backend: Arc<dyn StorageBackend>,
        arenas: Vec<Arena>,
        metadata: &mut Metadata,
    ) -> Result<Self> {
        Self::format_with_events(backend, arenas, metadata, &[], None)
    }

    pub(crate) fn format_with_events(
        backend: Arc<dyn StorageBackend>,
        arenas: Vec<Arena>,
        metadata: &mut Metadata,
        events: &[PagedAuditEvent],
        selected: Option<&BTreeSet<String>>,
    ) -> Result<Self> {
        let pool = Uuid::parse_str(&metadata.uuid)
            .map_err(|err| ArgosError::Invalid(format!("invalid metadata pool UUID: {err}")))?;
        // Validate and serialize all records before initializing any region.
        let (records, index) = initial_records(metadata)?;
        let (records, index) = import_events(records, index, events)?;
        let mut tree = PagedMetadata::format_selected(backend, *pool.as_bytes(), arenas, selected)?;
        let report = tree.commit(&records)?;
        if metadata.integrity.hash_scheme == Some(PAGED_INTEGRITY_SCHEME) {
            metadata.integrity.meta_hash = hex::encode(report.root_hash);
        }
        metadata.inodes.mark_committed();
        Ok(Self {
            tree,
            pool,
            base: metadata.clone(),
            index,
        })
    }

    pub fn open(backend: Arc<dyn StorageBackend>, pool: Uuid, arenas: Vec<Arena>) -> Result<Self> {
        let tree = PagedMetadata::open(backend, *pool.as_bytes(), arenas)?;
        let (base, index) = decode_image(tree.records()?, pool, tree.root_hash())?;
        Ok(Self {
            tree,
            pool,
            base,
            index,
        })
    }

    /// This is a cheap COW snapshot of the current inode table. Mutate the
    /// returned snapshot and commit it without independently marking its
    /// inode baseline committed. Unrelated snapshots remain supported via
    /// TrackedInodes' exhaustive candidate fallback.
    pub fn snapshot(&self) -> Metadata {
        self.base.clone()
    }

    /// Commit only candidate changed inode records and the metadata header.
    /// The caller's baseline advances only after a certified root commit.
    pub(crate) fn commit_allowed(
        &mut self,
        next: &mut Metadata,
        allowed: Option<&BTreeSet<String>>,
        event: Option<(&str, serde_json::Value)>,
    ) -> Result<CommitReport> {
        if Uuid::parse_str(&next.uuid).ok() != Some(self.pool) || next.txid <= self.base.txid {
            return Err(ArgosError::Invalid(
                "metadata image must advance its pool transaction".into(),
            ));
        }
        let (changes, index) = plan_records(&self.base, next, self.index.clone())?;
        let (changes, index) = if let Some((action, details)) = event {
            append_event(changes, index, next.txid, action, details, false)?
        } else {
            (changes, index)
        };
        let report = self.tree.commit_allowed(&changes, allowed)?;
        if next.integrity.hash_scheme == Some(PAGED_INTEGRITY_SCHEME) {
            next.integrity.meta_hash = hex::encode(report.root_hash);
        }
        next.inodes.mark_committed();
        self.base = next.clone();
        self.index = index;
        Ok(report)
    }

    /// Append an advisory event to the certified tree without committing
    /// any dirty live namespace or advancing the logical metadata txid.
    pub(crate) fn commit_event(
        &mut self,
        allowed: &BTreeSet<String>,
        action: &str,
        details: serde_json::Value,
    ) -> Result<CommitReport> {
        if self.base.integrity.hash_scheme != Some(PAGED_INTEGRITY_SCHEME) {
            return Err(ArgosError::Invalid(
                "paged event requires Merkle integrity".into(),
            ));
        }
        let (changes, index) = append_event(
            Changes::new(),
            self.index.clone(),
            self.base.txid,
            action,
            details,
            true,
        )?;
        let report = self.tree.commit_allowed(&changes, Some(allowed))?;
        self.base.integrity.meta_hash = hex::encode(report.root_hash);
        self.index = index;
        Ok(report)
    }

    pub(crate) fn events(&self) -> Vec<PagedAuditEvent> {
        self.index.events()
    }

    pub(crate) fn recover_with_events(
        backend: &dyn StorageBackend,
        pool: Uuid,
        arenas: Vec<Arena>,
    ) -> Result<(Metadata, Vec<PagedAuditEvent>)> {
        let (records, hash) = super::recover_records::<u64>(backend, *pool.as_bytes(), arenas)?;
        let (meta, index) = decode_image(records, pool, hash)?;
        Ok((meta, index.events()))
    }

    pub(crate) fn recover(
        backend: &dyn StorageBackend,
        pool: Uuid,
        arenas: Vec<Arena>,
    ) -> Result<Metadata> {
        let (records, hash) = super::recover_records::<u64>(backend, *pool.as_bytes(), arenas)?;
        decode_image(records, pool, hash).map(|(meta, _)| meta)
    }

    pub fn root_hash(&self) -> [u8; 32] {
        self.tree.root_hash()
    }

    pub(crate) fn confirm_all(&mut self, count: usize) -> Result<()> {
        self.tree.confirm_all(count)
    }

    pub fn repair_stale(
        &mut self,
    ) -> Result<BTreeMap<String, std::result::Result<CommitIo, String>>> {
        self.tree.repair_stale()
    }
}

/// The materialized volume compatibility adapter. The V3 branch is explicit
/// and experimental; its disk-resident reader is not yet the live volume's
/// authoritative inode API.
pub struct ImageStore {
    inner: ImageStoreKind,
}

enum ImageStoreKind {
    Numeric(NumericImageStore),
    V3(V3ImageStore),
}

impl ImageStore {
    pub fn format(
        backend: Arc<dyn StorageBackend>,
        arenas: Vec<Arena>,
        metadata: &mut Metadata,
    ) -> Result<Self> {
        NumericImageStore::format(backend, arenas, metadata).map(|store| Self {
            inner: ImageStoreKind::Numeric(store),
        })
    }
    pub(crate) fn format_with_events(
        backend: Arc<dyn StorageBackend>,
        arenas: Vec<Arena>,
        metadata: &mut Metadata,
        events: &[PagedAuditEvent],
        selected: Option<&BTreeSet<String>>,
    ) -> Result<Self> {
        if metadata.integrity.hash_scheme == Some(V3_INTEGRITY_SCHEME) {
            V3ImageStore::format(backend, arenas, metadata, events, selected).map(|store| Self {
                inner: ImageStoreKind::V3(store),
            })
        } else {
            NumericImageStore::format_with_events(backend, arenas, metadata, events, selected).map(
                |store| Self {
                    inner: ImageStoreKind::Numeric(store),
                },
            )
        }
    }
    pub(crate) fn format_v3(
        backend: Arc<dyn StorageBackend>,
        arenas: Vec<Arena>,
        metadata: &mut Metadata,
    ) -> Result<Self> {
        V3ImageStore::format(backend, arenas, metadata, &[], None).map(|store| Self {
            inner: ImageStoreKind::V3(store),
        })
    }
    pub fn open(backend: Arc<dyn StorageBackend>, pool: Uuid, arenas: Vec<Arena>) -> Result<Self> {
        NumericImageStore::open(backend, pool, arenas).map(|store| Self {
            inner: ImageStoreKind::Numeric(store),
        })
    }
    pub(crate) fn open_v3(
        backend: Arc<dyn StorageBackend>,
        pool: Uuid,
        arenas: Vec<Arena>,
    ) -> Result<Self> {
        V3ImageStore::open(backend, pool, arenas).map(|store| Self {
            inner: ImageStoreKind::V3(store),
        })
    }
    pub(crate) fn recover_with_events(
        backend: &dyn StorageBackend,
        pool: Uuid,
        arenas: Vec<Arena>,
    ) -> Result<(Metadata, Vec<PagedAuditEvent>)> {
        NumericImageStore::recover_with_events(backend, pool, arenas)
    }
    pub(crate) fn recover_v3(
        backend: &dyn StorageBackend,
        pool: Uuid,
        arenas: Vec<Arena>,
    ) -> Result<(Metadata, Vec<PagedAuditEvent>)> {
        V3ImageStore::recover(backend, pool, arenas)
    }
    pub(crate) fn recover(
        backend: &dyn StorageBackend,
        pool: Uuid,
        arenas: Vec<Arena>,
    ) -> Result<Metadata> {
        NumericImageStore::recover(backend, pool, arenas)
    }
    pub fn snapshot(&self) -> Metadata {
        match &self.inner {
            ImageStoreKind::Numeric(store) => store.snapshot(),
            ImageStoreKind::V3(store) => store.snapshot(),
        }
    }
    pub fn commit(&mut self, next: &mut Metadata) -> Result<CommitReport> {
        self.commit_allowed(next, None, None)
    }
    pub(crate) fn commit_allowed(
        &mut self,
        next: &mut Metadata,
        allowed: Option<&BTreeSet<String>>,
        event: Option<(&str, serde_json::Value)>,
    ) -> Result<CommitReport> {
        match &mut self.inner {
            ImageStoreKind::Numeric(store) => store.commit_allowed(next, allowed, event),
            ImageStoreKind::V3(store) => store.commit_allowed(next, allowed, event),
        }
    }
    pub(crate) fn commit_event(
        &mut self,
        allowed: &BTreeSet<String>,
        action: &str,
        details: serde_json::Value,
    ) -> Result<CommitReport> {
        match &mut self.inner {
            ImageStoreKind::Numeric(store) => store.commit_event(allowed, action, details),
            ImageStoreKind::V3(store) => store.commit_event(allowed, action, details),
        }
    }
    pub(crate) fn events(&self) -> Vec<PagedAuditEvent> {
        match &self.inner {
            ImageStoreKind::Numeric(store) => store.events(),
            ImageStoreKind::V3(store) => store.events(),
        }
    }
    pub fn root_hash(&self) -> [u8; 32] {
        match &self.inner {
            ImageStoreKind::Numeric(store) => store.root_hash(),
            ImageStoreKind::V3(store) => store.root_hash(),
        }
    }
    pub(crate) fn confirm_all(&mut self, count: usize) -> Result<()> {
        match &mut self.inner {
            ImageStoreKind::Numeric(store) => store.confirm_all(count),
            ImageStoreKind::V3(store) => store.confirm_all(count),
        }
    }
    pub fn repair_stale(
        &mut self,
    ) -> Result<BTreeMap<String, std::result::Result<CommitIo, String>>> {
        match &mut self.inner {
            ImageStoreKind::Numeric(store) => store.repair_stale(),
            ImageStoreKind::V3(store) => store.repair_stale(),
        }
    }
}
