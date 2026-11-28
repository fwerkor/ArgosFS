//! Typed bridge between the authenticated page store and ArgosFS Metadata.
//! The inode table is never serialized as a single JSON image. Header fields
//! and each individual inode are separately compressed records.
use super::{Arena, Changes, CommitIo, CommitReport, PagedMetadata};
use crate::backend::StorageBackend;
use crate::error::{ArgosError, Result};
use crate::types::Metadata;
use serde::{de::DeserializeOwned, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use uuid::Uuid;

pub const PAGED_INTEGRITY_SCHEME: u32 = 3;
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
use records::{decode_image, initial_records, plan_records, RecordIndex};

/// Typed metadata image on dedicated authenticated page arenas. Each
/// directory entry, block mapping and free extent has an independent record.
pub struct ImageStore {
    tree: PagedMetadata,
    pool: Uuid,
    base: Metadata,
    index: RecordIndex,
}

impl ImageStore {
    pub fn format(
        backend: Arc<dyn StorageBackend>,
        arenas: Vec<Arena>,
        metadata: &mut Metadata,
    ) -> Result<Self> {
        let pool = Uuid::parse_str(&metadata.uuid)
            .map_err(|err| ArgosError::Invalid(format!("invalid metadata pool UUID: {err}")))?;
        // Validate and serialize all records before initializing any region.
        let (records, index) = initial_records(metadata)?;
        let mut tree = PagedMetadata::format(backend, *pool.as_bytes(), arenas)?;
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
    pub fn commit(&mut self, next: &mut Metadata) -> Result<CommitReport> {
        self.commit_allowed(next, None)
    }

    pub(crate) fn commit_allowed(
        &mut self,
        next: &mut Metadata,
        allowed: Option<&BTreeSet<String>>,
    ) -> Result<CommitReport> {
        if Uuid::parse_str(&next.uuid).ok() != Some(self.pool) || next.txid <= self.base.txid {
            return Err(ArgosError::Invalid(
                "metadata image must advance its pool transaction".into(),
            ));
        }
        let (changes, index) = plan_records(&self.base, next, self.index.clone())?;
        let report = self.tree.commit_allowed(&changes, allowed)?;
        if next.integrity.hash_scheme == Some(PAGED_INTEGRITY_SCHEME) {
            next.integrity.meta_hash = hex::encode(report.root_hash);
        }
        next.inodes.mark_committed();
        self.base = next.clone();
        self.index = index;
        Ok(report)
    }

    pub(crate) fn recover(
        backend: &dyn StorageBackend,
        pool: Uuid,
        arenas: Vec<Arena>,
    ) -> Result<Metadata> {
        let (records, hash) = super::recover_records(backend, *pool.as_bytes(), arenas)?;
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
